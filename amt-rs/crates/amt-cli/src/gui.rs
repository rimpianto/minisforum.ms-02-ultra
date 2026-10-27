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

/// Map a winit key to an RFB keysym.
///
/// The physical key is preferred: with Option (Alt) held, macOS reports a
/// different logical key and the function keys stop looking like themselves,
/// which is exactly how ctrl+alt+f2 was lost. Letters and digits fall back to
/// the logical character, and Shift stays a held modifier on the host, so a
/// shifted letter still comes out shifted.
pub fn key_to_keysym(
    physical: &winit::keyboard::PhysicalKey,
    logical: &winit::keyboard::Key,
) -> Option<u32> {
    use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

    if let PhysicalKey::Code(code) = physical {
        let mapped = match code {
            KeyCode::Enter | KeyCode::NumpadEnter => Some(0xFF0D),
            KeyCode::Tab => Some(0xFF09),
            KeyCode::Escape => Some(0xFF1B),
            KeyCode::Backspace => Some(0xFF08),
            KeyCode::Space => Some(0x20),
            KeyCode::ArrowUp => Some(0xFF52),
            KeyCode::ArrowDown => Some(0xFF54),
            KeyCode::ArrowLeft => Some(0xFF51),
            KeyCode::ArrowRight => Some(0xFF53),
            KeyCode::Delete => Some(0xFFFF),
            KeyCode::Home => Some(0xFF50),
            KeyCode::End => Some(0xFF57),
            KeyCode::PageUp => Some(0xFF55),
            KeyCode::PageDown => Some(0xFF56),
            KeyCode::Insert => Some(0xFF63),
            KeyCode::F1 => Some(0xFFBE),
            KeyCode::F2 => Some(0xFFBF),
            KeyCode::F3 => Some(0xFFC0),
            KeyCode::F4 => Some(0xFFC1),
            KeyCode::F5 => Some(0xFFC2),
            KeyCode::F6 => Some(0xFFC3),
            KeyCode::F7 => Some(0xFFC4),
            KeyCode::F8 => Some(0xFFC5),
            KeyCode::F9 => Some(0xFFC6),
            KeyCode::F10 => Some(0xFFC7),
            KeyCode::F11 => Some(0xFFC8),
            KeyCode::F12 => Some(0xFFC9),
            _ => None,
        };
        if mapped.is_some() {
            return mapped;
        }
    }

    match logical {
        Key::Character(s) => s.chars().next().map(|c| c as u32),
        Key::Named(NamedKey::Space) => Some(0x20),
        Key::Named(NamedKey::Enter) => Some(0xFF0D),
        Key::Named(NamedKey::Tab) => Some(0xFF09),
        Key::Named(NamedKey::Escape) => Some(0xFF1B),
        Key::Named(NamedKey::Backspace) => Some(0xFF08),
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
    let mut force_draw = true;
    let mut win_w = width as u32;
    let mut win_h = height as u32;
    // The pointer position stays put between events: a button event that sends
    // (0,0) teleports the host cursor to the top-left corner.
    let mut pos = (0u16, 0u16);
    let mut held_mods: Vec<u32> = Vec::new();
    let mut fb_dims = (0usize, 0usize);
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
                        .with_title(format!(
                            "amt-rs {} ({}) — KVM",
                            crate::VERSION,
                            crate::GIT_HASH
                        ))
                        .with_inner_size(winit::dpi::PhysicalSize::new(
                            width as u32,
                            height as u32,
                        ))
                        .build(elwt)
                        .expect("cannot create window"),
                );

                // The framebuffer is 16:9; a window clamped by a smaller screen
                // keeps the picture letterboxed, so ask for the largest size
                // that fits the monitor at the same ratio.
                if let Some(mon) = win.current_monitor() {
                    let scale = mon.scale_factor();
                    let (mw, mh) = (mon.size().width as f64, mon.size().height as f64);
                    let (avail_w, avail_h) = (mw * 0.92, mh * 0.92);
                    let factor = (avail_w / width as f64)
                        .min(avail_h / height as f64)
                        .min(1.0);
                    if factor < 0.999 {
                        let _ = win.request_inner_size(winit::dpi::PhysicalSize::new(
                            (width as f64 * factor) as u32,
                            (height as f64 * factor) as u32,
                        ));
                    }
                    let _ = scale;
                }
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
                win_w = win.inner_size().width.max(1);
                win_h = win.inner_size().height.max(1);
                // Paint it black straight away: until the first frame arrives
                // the window would show the platform's white background.
                if let Ok(mut buf) = surf.buffer_mut() {
                    for px in buf.iter_mut() {
                        *px = 0;
                    }
                    let _ = buf.present();
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
                        if let Some(k) = key_to_keysym(&event.physical_key, &event.logical_key) {
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
                    let mut dirty = false;
                    while let Ok(frame) = gui.frames.try_recv() {
                        if (frame.width, frame.height) != fb_dims {
                            // POST and the BIOS run at a different resolution:
                            // follow it instead of scaling a small screen up
                            // into a big window.
                            fb_dims = (frame.width, frame.height);
                            if let Some(win) = window.as_ref() {
                                if let Some(mon) = win.current_monitor() {
                                    let (mw, mh) = (mon.size().width as f64, mon.size().height as f64);
                                    // Small framebuffers (POST, BIOS) get
                                    // enlarged up to 2x: chunky but readable.
                                    let factor = (mw * 0.92 / frame.width as f64)
                                        .min(mh * 0.92 / frame.height as f64)
                                        .min(2.0);
                                    let _ = win.request_inner_size(
                                        winit::dpi::PhysicalSize::new(
                                            (frame.width as f64 * factor) as u32,
                                            (frame.height as f64 * factor) as u32,
                                        ),
                                    );
                                }
                                win.request_redraw();
                            }
                        }
                        width = frame.width;
                        height = frame.height;
                        {
                            let mut st = gui.state.lock().unwrap();
                            *st = (frame.width, frame.height);
                        }
                        last = Some(frame);
                        dirty = true;
                    }

                    if !dirty && !force_draw {
                        // Nothing new: do not rescale a full frame 60 times a
                        // second. Exposure and resizes set force_draw.
                        return;
                    }

                    if let (Some(surf), Some(frame)) = (surface.as_mut(), last.as_ref()) {
                        let buf_w = NonZeroU32::new(win_w.max(1)).unwrap_or(NonZeroU32::new(1).unwrap());
                        let buf_h = NonZeroU32::new(win_h.max(1)).unwrap_or(NonZeroU32::new(1).unwrap());
                        if surf.resize(buf_w, buf_h).is_ok() {
                            if let Ok(mut buf) = surf.buffer_mut() {
                                // Scale the framebuffer to the window with the
                                // aspect ratio kept, black bars around it.
                                let (fw, fh) = (frame.width.max(1), frame.height.max(1));
                                let (bw, bh) = (buf_w.get() as usize, buf_h.get() as usize);
                                let scale = (bw as f32 / fw as f32).min(bh as f32 / fh as f32);
                                let dw = ((fw as f32 * scale) as usize).clamp(1, bw);
                                let dh = ((fh as f32 * scale) as usize).clamp(1, bh);
                                let ox = (bw - dw) / 2;
                                let oy = (bh - dh) / 2;

                                let black = 0u32;
                                for px in buf.iter_mut() {
                                    *px = black;
                                }
                                for y in 0..dh {
                                    let sy = y * fh / dh;
                                    let row = sy * fw * 4;
                                    let out_row = (oy + y) * bw + ox;
                                    for x in 0..dw {
                                        let sx = x * fw / dw;
                                        let p = &frame.pixels[row + sx * 4..row + sx * 4 + 4];
                                        buf[out_row + x] = ((p[0] as u32) << 16)
                                            | ((p[1] as u32) << 8)
                                            | p[2] as u32;
                                    }
                                }
                                let _ = buf.present();
                                dirty = false;
                                force_draw = false;
                            }
                        }
                    }

                    if std::env::var("AMT_TRACE").is_ok() {
                        redraws += 1;
                        if redraws % 30 == 0 {
                            eprintln!(
                                "[gui] redraw {redraws}, frame {}x{}, window {win_w}x{win_h}, dirty {dirty}",
                                last.as_ref().map(|f| f.width).unwrap_or(0),
                                last.as_ref().map(|f| f.height).unwrap_or(0)
                            );
                        }
                    }
                    // The next redraw comes from the timer, not from here.
                }
                WindowEvent::Occluded(false) => {
                    force_draw = true;
                    if let Some(win) = window.as_ref() {
                        win.request_redraw();
                    }
                }
                WindowEvent::Resized(size) => {
                    force_draw = true;
                    win_w = size.width.max(1);
                    win_h = size.height.max(1);
                    if let Some(surf) = surface.as_mut() {
                        let _ = surf.resize(
                            NonZeroU32::new(win_w).unwrap_or(NonZeroU32::new(1).unwrap()),
                            NonZeroU32::new(win_h).unwrap_or(NonZeroU32::new(1).unwrap()),
                        );
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
