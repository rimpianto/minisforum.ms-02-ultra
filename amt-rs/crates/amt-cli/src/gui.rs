//! KVM window: winit 0.29 + softbuffer 0.3, fed by the RFB session running in
//! a separate tokio thread. Frames arrive through a channel, keyboard and mouse
//! go back through another one.

use anyhow::Result;
use std::num::NonZeroU32;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use winit::event::{ElementState, Event, MouseButton, WindowEvent};
use winit::event_loop::EventLoop;
use winit::window::{Window, WindowBuilder};

/// A decoded frame handed from the network thread to the UI.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>, // RGBA
}

/// Input events the UI sends back to the RFB thread.
pub enum Input {
    Key { keysym: u32, down: bool },
    Pointer { x: u16, y: u16, buttons: u8 },
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

    event_loop.run(move |event, elwt| {

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
                win.request_redraw();
                window = Some(win);
                _context = Some(ctx);
                surface = Some(surf);
            }
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => elwt.exit(),
                WindowEvent::KeyboardInput { event, .. } => {
                    if let Some(k) = key_to_keysym(&event.logical_key) {
                        let _ = gui.input.send(Input::Key {
                            keysym: k,
                            down: event.state == ElementState::Pressed,
                        });
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let (fw, fh) = {
                        let st = gui.state.lock().unwrap();
                        (st.0.max(1), st.1.max(1))
                    };
                    let x = (position.x / fw as f64 * 65535.0).clamp(0.0, 65535.0) as u16;
                    let y = (position.y / fh as f64 * 65535.0).clamp(0.0, 65535.0) as u16;
                    let _ = gui.input.send(Input::Pointer { x, y, buttons: 0 });
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    let bit = match button {
                        MouseButton::Left => 1u8,
                        MouseButton::Middle => 2,
                        MouseButton::Right => 4,
                        _ => 0,
                    };
                    let _ = gui.input.send(Input::Pointer {
                        x: 0,
                        y: 0,
                        buttons: if state == ElementState::Pressed { bit } else { 0 },
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

                    if let Some(win) = window.as_ref() {
                        win.request_redraw();
                    }
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
