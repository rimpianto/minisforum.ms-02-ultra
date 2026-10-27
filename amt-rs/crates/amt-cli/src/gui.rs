//! KVM window: winit 0.29 + softbuffer 0.3, fed by the RFB session running in
//! a separate tokio thread. Frames arrive through a channel, keyboard and mouse
//! go back through another one.

use anyhow::Result;
use std::num::NonZeroU32;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use winit::event::{ElementState, Event, MouseButton, StartCause, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::window::{Window, WindowBuilder};

/// A decoded frame handed from the network thread to the UI.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>, // RGBA
}

/// Input events the UI sends back to the RFB thread.
pub enum Input {
    /// A whole keystroke: the loop sends down, a short hold, then up.
    Key { keysym: u32 },
    /// A modifier goes down and stays down until it is released, otherwise
    /// ctrl+c and ctrl+alt+f2 are impossible.
    Modifier { keysym: u32, down: bool },
    Pointer { x: u16, y: u16, buttons: u8 },
}

/// Modifiers we hold down for the host while the user keeps them pressed.
fn modifier_keysym(key: &winit::keyboard::Key) -> Option<u32> {
    use winit::keyboard::{Key, NamedKey};
    match key {
        Key::Named(NamedKey::Shift) => Some(0xFFE1),
        Key::Named(NamedKey::Control) => Some(0xFFE3),
        Key::Named(NamedKey::Alt) => Some(0xFFE9),
        Key::Named(NamedKey::Super) => Some(0xFFEB),
        _ => None,
    }
}

/// Map a winit key to an RFB keysym: printable ASCII plus what a console needs.
pub fn key_to_keysym(key: &winit::keyboard::Key) -> Option<u32> {
    use winit::keyboard::{Key, NamedKey};
    match key {
        Key::Named(NamedKey::Enter) => Some(0xFF0D),
        Key::Named(NamedKey::Tab) => Some(0xFF09),
        Key::Named(NamedKey::Escape) => Some(0xFF1B),
        Key::Named(NamedKey::Backspace) => Some(0xFF08),
        Key::Named(NamedKey::Space) => Some(0x20),
        Key::Named(NamedKey::ArrowUp) => Some(0xFF52),
        Key::Named(NamedKey::ArrowDown) => Some(0xFF54),
        Key::Named(NamedKey::ArrowLeft) => Some(0xFF51),
        Key::Named(NamedKey::ArrowRight) => Some(0xFF53),
        Key::Named(NamedKey::Shift) => Some(0xFFE1),
        Key::Named(NamedKey::Control) => Some(0xFFE3),
        Key::Named(NamedKey::Alt) => Some(0xFFE9),
        Key::Named(NamedKey::Delete) => Some(0xFFFF),
        Key::Named(NamedKey::Home) => Some(0xFF50),
        Key::Named(NamedKey::End) => Some(0xFF57),
        Key::Named(NamedKey::PageUp) => Some(0xFF55),
        Key::Named(NamedKey::PageDown) => Some(0xFF56),
        Key::Named(NamedKey::Insert) => Some(0xFF63),
        Key::Named(NamedKey::F1) => Some(0xFFBE),
        Key::Named(NamedKey::F2) => Some(0xFFBF),
        Key::Named(NamedKey::F3) => Some(0xFFC0),
        Key::Named(NamedKey::F4) => Some(0xFFC1),
        Key::Named(NamedKey::F5) => Some(0xFFC2),
        Key::Named(NamedKey::F6) => Some(0xFFC3),
        Key::Named(NamedKey::F7) => Some(0xFFC4),
        Key::Named(NamedKey::F8) => Some(0xFFC5),
        Key::Named(NamedKey::F9) => Some(0xFFC6),
        Key::Named(NamedKey::F10) => Some(0xFFC7),
        Key::Named(NamedKey::F11) => Some(0xFFC8),
        Key::Named(NamedKey::F12) => Some(0xFFC9),
        Key::Character(s) => s.chars().next().map(|c| c as u32),
        _ => None,
    }
}

pub struct Gui {
    pub frames: Receiver<Frame>,
    pub input: Sender<Input>,
    pub state: Arc<Mutex<(usize, usize)>>,
}

/// Run the window loop on the calling thread; returns when the window closes.
pub fn run_gui(gui: Gui) -> Result<()> {
    let event_loop = EventLoop::new()?;
    let (mut width, mut height) = *gui.state.lock().unwrap();

    // Declared in this order so the surface (dropped first) goes before the
    // context it borrows the display from.
    let mut _context: Option<softbuffer::Context<Arc<Window>>> = None;
    let mut surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>> = None;
    let mut window: Option<Arc<Window>> = None;
    let mut last: Option<Frame> = None;
    let mut redraws = 0u64;
    // The pointer position stays put between events: a button event that sends
    // (0,0) teleports the host cursor to the top-left corner.
    let mut pos = (0u16, 0u16);
    let mut held_mods: Vec<u32> = Vec::new();
    let mut mask = 0u8;

    event_loop.run(move |event, elwt| {
        // Poll the frame channel ~60 times a second. Without this the redraw
        // request below feeds itself and the loop spins at tens of thousands
        // of frames per second, burning a core for nothing.
        elwt.set_control_flow(ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(16),
        ));

        match event {
            Event::Resumed => {
                let win = Arc::new(
                    WindowBuilder::new()
                        .with_title("amt-rs KVM")
                        .with_inner_size(winit::dpi::PhysicalSize::new(
                            width as u32,
                            height as u32,
                        ))
                        .build(elwt)
                        .expect("cannot create window"),
                );
                let ctx = softbuffer::Context::new(win.clone()).expect("softbuffer context");
                let mut surf =
                    softbuffer::Surface::new(&ctx, win.clone()).expect("softbuffer surface");
                let _ = surf.resize(
                    NonZeroU32::new(width as u32).unwrap_or(NonZeroU32::new(1).unwrap()),
                    NonZeroU32::new(height as u32).unwrap_or(NonZeroU32::new(1).unwrap()),
                );
                if std::env::var("AMT_TRACE").is_ok() {
                    eprintln!("[gui] window created {}x{}", width, height);
                }
                win.request_redraw();
                window = Some(win);
                _context = Some(ctx);
                surface = Some(surf);
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                if let Some(win) = window.as_ref() {
                    win.request_redraw();
                }
            }
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => elwt.exit(),
                WindowEvent::KeyboardInput { event, .. } => {
                    if let Some(k) = modifier_keysym(&event.logical_key) {
                        let down = event.state == ElementState::Pressed;
                        if down != held_mods.contains(&k) {
                            if down {
                                held_mods.push(k);
                            } else {
                                held_mods.retain(|m| *m != k);
                            }
                            let _ = gui.input.send(Input::Modifier { keysym: k, down });
                        }
                    } else if event.state == ElementState::Pressed {
                        if let Some(k) = key_to_keysym(&event.logical_key) {
                            let _ = gui.input.send(Input::Key { keysym: k });
                        }
                    }
                }
                WindowEvent::Focused(false) => {
                    // Do not leave ctrl stuck down on the host if the window
                    // loses focus mid-chord.
                    for k in held_mods.drain(..) {
                        let _ = gui.input.send(Input::Modifier { keysym: k, down: false });
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let (fw, fh) = {
                        let st = gui.state.lock().unwrap();
                        (st.0.max(1), st.1.max(1))
                    };
                    pos.0 = (position.x / fw as f64 * 65535.0).clamp(0.0, 65535.0) as u16;
                    pos.1 = (position.y / fh as f64 * 65535.0).clamp(0.0, 65535.0) as u16;
                    let _ = gui.input.send(Input::Pointer {
                        x: pos.0,
                        y: pos.1,
                        buttons: mask,
                    });
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    // RFB has no wheel: buttons 4 and 5 are one click each.
                    let up = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => y > 0.0,
                        winit::event::MouseScrollDelta::PixelDelta(p) => p.y > 0.0,
                    };
                    let bit = if up { 8u8 } else { 16u8 };
                    for buttons in [mask | bit, mask] {
                        let _ = gui.input.send(Input::Pointer {
                            x: pos.0,
                            y: pos.1,
                            buttons,
                        });
                    }
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    let bit = match button {
                        MouseButton::Left => 1u8,
                        MouseButton::Middle => 2,
                        MouseButton::Right => 4,
                        _ => 0,
                    };
                    if state == ElementState::Pressed {
                        mask |= bit;
                    } else {
                        mask &= !bit;
                    }
                    let _ = gui.input.send(Input::Pointer {
                        x: pos.0,
                        y: pos.1,
                        buttons: mask,
                    });
                }
                WindowEvent::RedrawRequested => {
                    while let Ok(frame) = gui.frames.try_recv() {
                        width = frame.width;
                        height = frame.height;
                        {
                            let mut st = gui.state.lock().unwrap();
                            *st = (frame.width, frame.height);
                        }
                        last = Some(frame);
                    }

                    if let (Some(surf), Some(frame)) = (surface.as_mut(), last.as_ref()) {
                        let w = NonZeroU32::new(width as u32).unwrap_or(NonZeroU32::new(1).unwrap());
                        let h =
                            NonZeroU32::new(height as u32).unwrap_or(NonZeroU32::new(1).unwrap());
                        if surf.resize(w, h).is_ok() {
                            if let Ok(mut buf) = surf.buffer_mut() {
                                for (i, px) in frame.pixels.chunks_exact(4).enumerate() {
                                    if i >= buf.len() {
                                        break;
                                    }
                                    buf[i] = ((px[0] as u32) << 16)
                                        | ((px[1] as u32) << 8)
                                        | px[2] as u32;
                                }
                                let _ = buf.present();
                            }
                        }
                    }

                    if std::env::var("AMT_TRACE").is_ok() {
                        redraws += 1;
                        if redraws % 30 == 0 {
                            eprintln!(
                                "[gui] redraw {redraws}, frame {}x{}",
                                last.as_ref().map(|f| f.width).unwrap_or(0),
                                last.as_ref().map(|f| f.height).unwrap_or(0)
                            );
                        }
                    }
                    // The next redraw comes from the timer, not from here.
                }
                _ => {}
            },
            _ => {}
        }
    })?;

    Ok(())
}

/// Build the channels and the GUI state; the caller threads `run_gui`.
pub fn channels() -> (Sender<Frame>, Receiver<Input>, Gui) {
    let (tx_frame, rx_frame) = channel();
    let (tx_input, rx_input) = channel();
    (
        tx_frame,
        rx_input,
        Gui {
            frames: rx_frame,
            input: tx_input,
            state: Arc::new(Mutex::new((1920, 1080))),
        },
    )
}
