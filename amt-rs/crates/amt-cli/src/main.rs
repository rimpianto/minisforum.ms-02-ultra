mod config;
mod gui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::load;
use amt_wsman::{self, PowerState, AmtConfig as WsmanConfig};

/// ver. 0.1.<commit count>, filled in by build.rs so the number always moves.
pub const VERSION: &str = env!("AMT_VERSION");
pub const GIT_HASH: &str = env!("AMT_GIT_HASH");
pub const VERSION_LONG: &str = concat!(env!("AMT_VERSION"), " (", env!("AMT_GIT_HASH"), ")");

#[derive(Parser)]
#[command(name = "amt-cli", version = VERSION, long_version = VERSION_LONG, about = "Intel AMT command-line client")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Change or query the power state
    Power {
        /// Desired state: "on", "off", or "reset"
        #[arg(long)]
        state: Option<String>,
    },
    /// Open a KVM (remote console) session
    Kvm {
        /// How long to stay connected, in seconds
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Save the first full frame to this PNG file instead of opening a window
        #[arg(long)]
        screenshot: Option<String>,
        /// Key to send before capturing: a name (space, enter, tab, escape,
        /// shift, delete, f1..f12), a single character, or a chord like
        /// "ctrl+alt+f2". Re-sent every 1.5s, which also wakes a blank console.
        #[arg(long)]
        key: Option<String>,
        /// Send the pointer to this pixel position ("x,y") before capturing
        #[arg(long)]
        pointer: Option<String>,
        /// Click the left button after moving the pointer
        #[arg(long)]
        click: bool,
    },
    /// Rebuild a PNG from tiles captured with AMT_DUMP (offline decoder test)
    Replay {
        /// Directory written by AMT_DUMP
        dir: String,
        /// Output PNG
        #[arg(long)]
        out: String,
        /// Screen width
        #[arg(long, default_value_t = 1920)]
        width: usize,
        /// Screen height
        #[arg(long, default_value_t = 1080)]
        height: usize,
    },
    /// Open a Serial-over-LAN session
    Sol {
        /// How long to stay connected, in seconds (ignored with --interactive)
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Send what you type on stdin to the console, and print what comes back
        #[arg(long)]
        interactive: bool,
    },
}

/// Async entry point – we need a Tokio runtime because the WS‑Man
/// client is asynchronous.
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    eprintln!("amt-cli {} ({})", VERSION, GIT_HASH);
    if let Commands::Replay { dir, out, width, height } = &cli.command {
        return replay_tiles(dir, out, *width, *height);
    }
    let cfg = load()?; // load once, reuse for all sub‑commands

    match cli.command {
        Commands::Power { state } => {
            // Convert CLI config into the WS‑Man config struct.
            let ws_cfg = WsmanConfig {
                host: cfg.amt.host.clone(),
                port: cfg.amt.port,
                username: cfg.amt.username.clone(),
                password: cfg.amt.password.clone(),
            };

            match state.as_deref() {
                // No --state: read the current state with a WS‑Man Get on
                // CIM_AssociatedPowerManagementService.
                None => {
                    let (ps, rps) = amt_wsman::get_power_state(&ws_cfg).await?;
                    println!(
                        "PowerState={} ({}) RequestedPowerState={} ({})",
                        ps,
                        state_name(ps),
                        rps,
                        state_name(rps)
                    );
                }
                Some(requested) => {
                    let power_state = PowerState::from_str(requested).ok_or_else(|| {
                        anyhow::anyhow!(format!(
                            "Invalid power state '{}'. Use 'on', 'off', or 'reset'",
                            requested
                        ))
                    })?;

                    amt_wsman::set_power(&ws_cfg, power_state).await?;
                    println!("Power state change request sent (state = {})", requested);
                }
            }
        }
        Commands::Replay { .. } => unreachable!("replay gestito prima della config"),
        Commands::Kvm { seconds, screenshot, key, pointer, click } => {
            // With TLS the redirection port is WS-Man + 2 (16993 -> 16995);
            // without TLS it is WS-Man + 2 as well (16992 -> 16994).
            let redir_port = std::env::var("AMT_REDIR_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(cfg.amt.port + 2);

            let pointer_xy = match pointer.as_deref() {
                None => None,
                Some(spec) => {
                    let (x, y) = spec
                        .split_once(',')
                        .ok_or_else(|| anyhow::anyhow!("--pointer wants \"x,y\""))?;
                    Some((x.trim().parse::<u16>()?, y.trim().parse::<u16>()?))
                }
            };

            if screenshot.is_none() {
                // On macOS the winit event loop must own the main thread, so the
                // session gets its own thread and its own tokio runtime.
                let (frame_tx, input_rx, gui) = gui::channels();
                let host = cfg.amt.host.clone();
                let user = cfg.amt.username.clone();
                let pass = cfg.amt.password.clone();
                std::thread::spawn(move || {
                    let rt = match tokio::runtime::Runtime::new() {
                        Ok(rt) => rt,
                        Err(e) => {
                            eprintln!("tokio runtime: {e}");
                            std::process::exit(1);
                        }
                    };
                    rt.block_on(async move {
                        if let Err(e) = run_kvm_session(
                            &host,
                            redir_port,
                            &user,
                            &pass,
                            seconds,
                            key.clone(),
                            pointer_xy,
                            click,
                            Some((frame_tx, input_rx)),
                            None,
                        )
                        .await
                        {
                            eprintln!("kvm: {e}");
                        }
                    });
                    std::process::exit(0);
                });
                gui::run_gui(gui)?;
                return Ok(());
            }

            run_kvm_session(
                &cfg.amt.host,
                redir_port,
                &cfg.amt.username,
                &cfg.amt.password,
                seconds,
                key,
                pointer_xy,
                click,
                None,
                screenshot,
            )
            .await?;
        }
        Commands::Sol { seconds, interactive } => {
            // With TLS the redirection port is WS-Man + 2 (16993 -> 16995);
        // without TLS it is WS-Man + 2 as well (16992 -> 16994).
        let redir_port = std::env::var("AMT_REDIR_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(cfg.amt.port + 2);
            let mut session = amt_redir::sol::open(
                &cfg.amt.host,
                redir_port,
                &cfg.amt.username,
                &cfg.amt.password,
            )
            .await?;

            println!(
                "SOL session established with {}:{}",
                cfg.amt.host, redir_port
            );
            if let Some(settings) = &session.serial_settings {
                println!("serial settings: {:02x?}", settings);
            }

            // A bare CR nudges the remote console into producing output.
            session.sol_send(b"\r").await?;

            if interactive {
                use std::io::Write as _;
                use tokio::io::AsyncBufReadExt as _;

                let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
                let mut line = String::new();
                println!("(interactive: type, Enter sends CR; Ctrl-D quits)");
                loop {
                    tokio::select! {
                        res = session.sol_read() => match res {
                            Ok(Some(data)) => {
                                print!("{}", String::from_utf8_lossy(&data));
                                let _ = std::io::stdout().flush();
                            }
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!("SOL read error: {e}");
                                break;
                            }
                        },
                        read = stdin.read_line(&mut line) => match read {
                            Ok(0) => break,
                            Ok(_) => {
                                let mut out = line.trim_end_matches('\n').as_bytes().to_vec();
                                out.push(b'\r');
                                session.sol_send(&out).await?;
                                line.clear();
                            }
                            Err(e) => {
                                eprintln!("stdin: {e}");
                                break;
                            }
                        }
                    }
                }
            } else {
                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => break,
                        res = session.sol_read() => match res {
                            Ok(Some(data)) => print!("{}", String::from_utf8_lossy(&data)),
                            Ok(None) => {}
                            Err(e) => {
                                eprintln!("SOL read error: {e}");
                                break;
                            }
                        }
                    }
                }
            }
            println!();
        }
    }

    Ok(())
}

/// Human-readable name for a CIM power state value.
fn state_name(v: u8) -> &'static str {
    match v {
        0 => "Unknown",
        1 => "Full Power",
        2 => "On",
        3 => "Sleep-Light",
        4 => "Sleep-Deep",
        5 => "Power Cycle (Hard Off)",
        6 => "Off-Hard",
        7 => "Hibernate",
        8 => "Off-Soft",
        9 => "Off-Hard Graceful",
        10 => "Power Cycle Graceful",
        11 => "Reset",
        _ => "Unrecognised",
    }
}

fn be16(b: &[u8]) -> usize {
    u16::from_be_bytes([b[0], b[1]]) as usize
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// RFB (VNC) handshake inside the redirection session. Returns the desktop
/// size and the bytes per pixel the device announced.
async fn rfb_handshake(session: &mut amt_redir::Session) -> Result<(usize, usize, usize)> {
    let version = session.read_raw(12).await?;
    eprintln!("[kvm] server RFB version: {}", String::from_utf8_lossy(&version).trim());
    let reply: Vec<u8> = if version.starts_with(b"RFB 003.") {
        version
    } else {
        b"RFB 003.008\n".to_vec()
    };
    session.write_raw(&reply).await?;

    let count = session.read_raw(1).await?[0] as usize;
    if count == 0 {
        let len = be32(&session.read_raw(4).await?) as usize;
        let reason = session.read_raw(len).await?;
        anyhow::bail!(
            "RFB refused the session: {}",
            String::from_utf8_lossy(&reason)
        );
    }
    let _types = session.read_raw(count).await?;

    // Security type 1 = None: authentication already happened in the
    // redirection handshake.
    session.write_raw(&[1]).await?;
    let result = session.read_raw(4).await?;
    if be32(&result) != 0 {
        anyhow::bail!("RFB security handshake failed");
    }

    // Shared desktop flag.
    session.write_raw(&[1]).await?;

    let init = session.read_raw(24).await?;
    let width = be16(&init[0..2]);
    let height = be16(&init[2..4]);
    let bits_per_pixel = init[4] as usize;
    let name_len = be32(&init[20..24]) as usize;
    let _name = session.read_raw(name_len).await?;

    let bpp = if bits_per_pixel <= 8 { 1 } else { 2 };
    Ok((width, height, bpp))
}

async fn request_update(
    session: &mut amt_redir::Session,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Result<()> {
    let mut msg = vec![3u8, 0];
    msg.extend_from_slice(&(x as u16).to_be_bytes());
    msg.extend_from_slice(&(y as u16).to_be_bytes());
    msg.extend_from_slice(&(w as u16).to_be_bytes());
    msg.extend_from_slice(&(h as u16).to_be_bytes());
    session.write_raw(&msg).await
}


fn print_center(fb: &amt_kvm::Framebuffer) {
    if fb.width == 0 || fb.height == 0 {
        return;
    }
    let (cx, cy) = (fb.width / 2, fb.height / 2);
    let i = (cy * fb.width + cx) * 4;
    println!(
        "center pixel: R={} G={} B={}",
        fb.pixels[i], fb.pixels[i + 1], fb.pixels[i + 2]
    );
    let mut lit = 0usize;
    let mut sampled = 0usize;
    for px in fb.pixels.chunks_exact(4).step_by(97) {
        sampled += 1;
        if px[0] > 8 || px[1] > 8 || px[2] > 8 {
            lit += 1;
        }
    }
    println!("non-black pixels: {lit}/{sampled} (sampled)");
}

/// X11 keysyms for the keys we care about (AMT takes RFB KeyEvent frames).
fn keysym_for(name: &str) -> Result<u32> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "space" => 0x20,
        "enter" | "return" => 0xFF0D,
        "tab" => 0xFF09,
        "escape" | "esc" => 0xFF1B,
        "shift" => 0xFFE1,
        "ctrl" | "control" => 0xFFE3,
        "alt" => 0xFFE9,
        "delete" | "del" => 0xFFFF,
        "home" => 0xFF50,
        "end" => 0xFF57,
        "pageup" | "pgup" => 0xFF55,
        "pagedown" | "pgdn" => 0xFF56,
        "insert" | "ins" => 0xFF63,
        other if other.len() == 1 => other.as_bytes()[0] as u32,
        // F1..F12 are 0xFFBE..0xFFC9, consecutive.
        other if other.len() >= 2
            && other.starts_with('f')
            && other[1..].parse::<u32>().map(|n| (1..=12).contains(&n)).unwrap_or(false) =>
        {
            0xFFBE + other[1..].parse::<u32>()? - 1
        }
        other => anyhow::bail!("unknown key '{other}'"),
    })
}

async fn send_pointer(
    session: &mut amt_redir::Session,
    x: u16,
    y: u16,
    buttons: u8,
) -> Result<()> {
    let mut msg = vec![5u8, buttons];
    msg.extend_from_slice(&x.to_be_bytes());
    msg.extend_from_slice(&y.to_be_bytes());
    session.write_raw(&msg).await?;
    Ok(())
}

async fn send_key(session: &mut amt_redir::Session, keysym: u32) -> Result<()> {
    let mut msg = vec![4u8, 1, 0, 0];
    msg.extend_from_slice(&keysym.to_be_bytes());
    session.write_raw(&msg).await?;
    // Short but non-zero: zero is dropped by some firmware, and anything near
    // the host's key-repeat delay turns one press into a burst.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let mut msg = vec![4u8, 0, 0, 0];
    msg.extend_from_slice(&keysym.to_be_bytes());
    session.write_raw(&msg).await?;
    Ok(())
}

/// "ctrl+l" means: hold ctrl, press l, release ctrl.
async fn send_chord(session: &mut amt_redir::Session, spec: &str) -> Result<()> {
    if std::env::var("AMT_TRACE").is_ok() {
        eprintln!("[kvm] sending key '{spec}'");
    }
    let parts: Vec<&str> = spec.split('+').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return send_key(session, keysym_for(spec)?).await;
    }
    let mods: Vec<u32> = parts[..parts.len() - 1]
        .iter()
        .map(|m| keysym_for(m))
        .collect::<Result<_>>()?;
    let last = keysym_for(parts[parts.len() - 1])?;

    for m in &mods {
        let mut msg = vec![4u8, 1, 0, 0];
        msg.extend_from_slice(&m.to_be_bytes());
        session.write_raw(&msg).await?;
    }
    send_key(session, last).await?;
    for m in mods.iter().rev() {
        let mut msg = vec![4u8, 0, 0, 0];
        msg.extend_from_slice(&m.to_be_bytes());
        session.write_raw(&msg).await?;
    }
    Ok(())
}

fn replay_tiles(dir: &str, out: &str, width: usize, height: usize) -> Result<()> {

    let mut fb = amt_kvm::Framebuffer::new(width, height);
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| anyhow::anyhow!("{dir}: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "raw").unwrap_or(false))
        .collect();
    files.sort();
    let (mut ok, mut bad) = (0usize, 0usize);
    let mut last_err: Option<String> = None;
    for f in files {
        let stem = f
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() < 3 {
            continue;
        }
        let (x, y) = match (
            parts[parts.len() - 2].parse::<usize>(),
            parts[parts.len() - 1].parse::<usize>(),
        ) {
            (Ok(x), Ok(y)) => (x, y),
            _ => continue,
        };
        let data = std::fs::read(&f)?;
        match amt_kvm::decoder::draw_rle_tiles(&data, 2, x, y, 64, 64, &mut fb) {
            Ok(()) => ok += 1,
            Err(e) => {
                bad += 1;
                last_err = Some(format!("{}: {e}", f.display()));
            }
        }
    }
    fb.save_png(&out).map_err(|e| anyhow::anyhow!(e))?;
    println!("{ok} tile disegnati, {bad} errori -> {out}");
    if let Some(e) = last_err {
        println!("ultimo errore: {e}");
    }
    Ok(())
}


async fn run_kvm_session(
    host: &str,
    redir_port: u16,
    user: &str,
    pass: &str,
    seconds: u64,
    key: Option<String>,
    pointer: Option<(u16, u16)>,
    click: bool,
    window: Option<(std::sync::mpsc::Sender<gui::Frame>, std::sync::mpsc::Receiver<gui::Input>)>,
    screenshot: Option<String>,
) -> Result<()> {
    eprintln!("[kvm] connecting to {host}:{redir_port}");
    // Plain TCP probe first: if this fails the trouble is not TLS.
    match std::net::TcpStream::connect_timeout(
        &format!("{host}:{redir_port}").parse()?,
        std::time::Duration::from_secs(5),
    ) {
        Ok(_) => eprintln!("[kvm] tcp ok"),
        Err(e) => eprintln!("[kvm] tcp failed: {e}"),
    }
    let mut session = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        amt_redir::Session::connect(host, redir_port, amt_redir::Protocol::Kvm),
    )
    .await
    .map_err(|_| anyhow::anyhow!("TLS connect timed out after 10s"))??;
    eprintln!("[kvm] TLS up, authenticating");
    session.handshake(user, pass).await?;
    eprintln!("[kvm] KVM session established with {host}:{redir_port}");

    let (w, h, bpp) = rfb_handshake(&mut session).await?;
    eprintln!("[kvm] desktop {w}x{h} ({bpp} byte per pixel)");

    let fb = amt_kvm::Framebuffer::new(w, h);
    kvm_capture(&mut session, bpp, fb, seconds, screenshot.as_deref(), key, pointer, click, window).await
}

async fn kvm_capture(
    session: &mut amt_redir::Session,
    bpp: usize,
    mut fb: amt_kvm::Framebuffer,
    seconds: u64,
    screenshot: Option<&str>,
    key: Option<String>,
    pointer: Option<(u16, u16)>,
    click: bool,
    window: Option<(std::sync::mpsc::Sender<gui::Frame>, std::sync::mpsc::Receiver<gui::Input>)>,
) -> Result<()> {
    // RGB565 is the device default; the reference only sends SetPixelFormat
    // when it wants 8 bpp. Sending it unconditionally is a byte the firmware
    // does not expect, so keep the wire identical to the reference.
    if bpp != 2 {
        let mut pf = vec![0u8, 0, 0, 0];
        pf.extend_from_slice(&[8, 8, 0, 1]);
        pf.extend_from_slice(&7u16.to_be_bytes());
        pf.extend_from_slice(&7u16.to_be_bytes());
        pf.extend_from_slice(&3u16.to_be_bytes());
        pf.extend_from_slice(&[5, 2, 0, 0, 0, 0]);
        session.write_raw(&pf).await?;
    }

    // RAW is mandatory for Intel AMT, RLE is what it actually sends, 1092 and
    // DesktopSize are what the reference always advertises.
    let encodings: [i32; 4] = [16, 0, 1092, -223];
    let mut msg = vec![2u8, 0];
    msg.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for encoding in encodings {
        msg.extend_from_slice(&encoding.to_be_bytes());
    }
    session.write_raw(&msg).await?;

    if let Some((px, py)) = pointer {
        // Pixel coordinates to RFB's 0..65535 absolute space.
        let ax = ((px as f64 / fb.width.max(1) as f64) * 65535.0).clamp(0.0, 65535.0) as u16;
        let ay = ((py as f64 / fb.height.max(1) as f64) * 65535.0).clamp(0.0, 65535.0) as u16;
        send_pointer(session, ax, ay, 0).await?;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if click {
            send_pointer(session, ax, ay, 1).await?;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            send_pointer(session, ax, ay, 0).await?;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    if std::env::var("AMT_BOGUS").is_ok() {
        // Does the device parse our stream at all? A bogus type should either
        // kill the session or be ignored, and that tells us a lot.
        session.write_raw(&[0x99, 0, 0, 0, 0]).await?;
        eprintln!("[kvm] sent a bogus message type 0x99");
    }

    request_update(session, 0, 0, fb.width, fb.height).await?;
    let mut last_key = tokio::time::Instant::now();
    let mut last_request = tokio::time::Instant::now();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut drawn = false;
    let mut updates = 0usize;
    let quiet = window.is_some();
    while tokio::time::Instant::now() < deadline {
        // Input first: a static screen sends no framebuffer updates at all, so
        // draining input only when a frame arrives makes the keyboard and mouse
        // dead exactly when the console is idle.
        if let Some((_, input_rx)) = window.as_ref() {
            while let Ok(ev) = input_rx.try_recv() {
                match ev {
                    // A keystroke is sent whole the moment the key goes down.
                    // Waiting for the release to send the key-up means the
                    // host holds the key while we decode a frame, and the
                    // console repeats it (spaces everywhere, doubled letters).
                    gui::Input::Key { keysym } => {
                        send_key(session, keysym).await?;
                    }
                    gui::Input::Modifier { keysym, down } => {
                        let mut msg = vec![4u8, down as u8, 0, 0];
                        msg.extend_from_slice(&keysym.to_be_bytes());
                        session.write_raw(&msg).await?;
                    }
                    gui::Input::Pointer { x, y, buttons } => {
                        send_pointer(session, x, y, buttons).await?;
                    }
                }
            }
        }
        // A blanked console sends nothing but a DesktopSize rect; the key has
        // to be repeated because the first ones land while the host wakes up.
        if let Some(spec) = key.as_deref() {
            if last_key.elapsed() > std::time::Duration::from_millis(1500) {
                send_chord(session, spec).await?;
                last_key = tokio::time::Instant::now();
            }
        }
        let msg_type = match tokio::time::timeout(
            std::time::Duration::from_millis(30),
            session.read_raw(1),
        )
        .await
        {
            Ok(Ok(buf)) => buf[0],
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                // Nothing to draw, but keep the session alive and the input
                // flowing; ask for a frame about once a second.
                if last_request.elapsed() > std::time::Duration::from_millis(800) {
                    request_update(session, 0, 0, fb.width, fb.height).await?;
                    last_request = tokio::time::Instant::now();
                }
                continue;
            }
        };
        match msg_type {
            0 => {
                let _pad = session.read_raw(1).await?;
                let nrects = be16(&session.read_raw(2).await?);
                if std::env::var("AMT_TRACE").is_ok() {
                    eprintln!("update with {nrects} rect(s)");
                }
                for _ in 0..nrects {
                    let hdr = session.read_raw(12).await?;
                    let (rx, ry) = (be16(&hdr[0..2]), be16(&hdr[2..4]));
                    let (rw, rh) = (be16(&hdr[4..6]), be16(&hdr[6..8]));
                    let encoding = be32(&hdr[8..12]) as i32;
                    if std::env::var("AMT_TRACE").ok().as_deref() == Some("2") {
                        eprintln!("rect x={rx} y={ry} w={rw} h={rh} enc={encoding}");
                    }
                    match encoding {
                        0 => {
                            let payload = session.read_raw(rw * rh * bpp).await?;
                            amt_kvm::decoder::draw_raw(&payload, bpp, rx, ry, rw, rh, &mut fb)
                                .map_err(|e| anyhow::anyhow!(e))?;
                            drawn = true;
                        }
                        16 => {
                            let len = be32(&session.read_raw(4).await?) as usize;
                            if len > 32 * 1024 * 1024 {
                                anyhow::bail!("RLE block too large: {len} bytes");
                            }
                            let mut block = Vec::with_capacity(4 + len);
                            block.extend_from_slice(&(len as u32).to_le_bytes());
                            block.extend_from_slice(&session.read_raw(len).await?);
                            if let Ok(dir) = std::env::var("AMT_DUMP") {
                                // Keep the raw block and the inflated tile stream
                                // so the decoder can be debugged offline.
                                let n = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_micros())
                                    .unwrap_or(0);
                                let _ = std::fs::create_dir_all(&dir);
                                let _ = std::fs::write(format!("{dir}/{n}-{rx}-{ry}.rle"), &block);
                                if let Ok(s) = amt_kvm::decoder::rle_stream(&block) {
                                    let _ = std::fs::write(format!("{dir}/{n}-{rx}-{ry}.raw"), &s);
                                }
                            }
                            match amt_kvm::decoder::rle_stream(&block)
                                .and_then(|stream| {
                                    amt_kvm::decoder::draw_rle_tiles(
                                        &stream, bpp, rx, ry, rw, rh, &mut fb,
                                    )
                                }) {
                                Ok(()) => drawn = true,
                                Err(e) => eprintln!(
                                    "rect {rx},{ry} {rw}x{rh}: {e} (tile skipped, frame kept)"
                                ),
                            }
                        }
                        -223 => fb.resize(rw, rh),
                        other => anyhow::bail!("unsupported encoding {other}"),
                    }
                }

                if drawn {
                    updates += 1;
                    let mut lit = 0usize;
                    let mut sampled = 0usize;
                    for px in fb.pixels.chunks_exact(4).step_by(97) {
                        sampled += 1;
                        if px[0] > 8 || px[1] > 8 || px[2] > 8 {
                            lit += 1;
                        }
                    }
                    if !quiet {
                        println!("update {updates}: {lit}/{sampled} non-black (sampled)");
                    }
                    if let Some((frame_tx, _)) = window.as_ref() {
                        let _ = frame_tx.send(gui::Frame {
                            width: fb.width,
                            height: fb.height,
                            pixels: fb.pixels.clone(),
                        });
                    }
                }
                // Ask again after every update, drawn or not: the first answer
                // is often just a DesktopSize rect, and a client that only
                // re-requests after painting stays black forever.
                request_update(session, 0, 0, fb.width, fb.height).await?;
            }
            1 => {
                // SetColourMapEntries: pad, first, count, then 6 bytes per entry
                let head = session.read_raw(5).await?;
                let count = be16(&head[3..5]);
                let _ = session.read_raw(6 * count).await?;
            }
            2 => {}
            3 => {
                let _ = session.read_raw(3).await?;
                let len = be32(&session.read_raw(4).await?) as usize;
                let _ = session.read_raw(len).await?;
            }
            other => anyhow::bail!("unexpected RFB message type {other}"),
        }
    }

    print_center(&fb);
    if let Some(path) = screenshot {
        fb.save_png(path).map_err(|e| anyhow::anyhow!(e))?;
        println!("screenshot written to {} ({}x{})", path, fb.width, fb.height);
    } else if window.is_none() {
        println!("no frame drawn within {seconds}s");
    }
    Ok(())
}
