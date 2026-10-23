mod config;

use anyhow::Result;
use clap::{Parser, Subcommand};
use config::load;
use amt_wsman::{self, PowerState, AmtConfig as WsmanConfig};

#[derive(Parser)]
#[command(name = "amt-cli", version = "0.1.0", about = "Intel AMT command‑line client")]
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
        Commands::Kvm { seconds, screenshot } => {
            // With TLS the redirection port is WS-Man + 2 (16993 -> 16995);
        // without TLS it is WS-Man + 2 as well (16992 -> 16994).
        let redir_port = std::env::var("AMT_REDIR_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(cfg.amt.port + 2);
            let mut session = amt_redir::Session::connect(
                &cfg.amt.host,
                redir_port,
                amt_redir::Protocol::Kvm,
            )
            .await?;
            session
                .handshake(&cfg.amt.username, &cfg.amt.password)
                .await?;
            println!(
                "KVM session established with {}:{}",
                cfg.amt.host, redir_port
            );

            let (w, h, bpp) = rfb_handshake(&mut session).await?;
            println!("desktop {}x{} ({} byte per pixel)", w, h, bpp);

            let fb = amt_kvm::Framebuffer::new(w, h);
            kvm_capture(
                &mut session,
                bpp,
                fb,
                seconds,
                screenshot.as_deref(),
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

async fn kvm_capture(
    session: &mut amt_redir::Session,
    bpp: usize,
    mut fb: amt_kvm::Framebuffer,
    seconds: u64,
    screenshot: Option<&str>,
) -> Result<()> {
    // RAW is mandatory for Intel AMT; RLE is what it actually sends;
    // DesktopSize lets it tell us about resolution changes.
    let mut msg = vec![2u8, 0];
    msg.extend_from_slice(&3u16.to_be_bytes());
    for encoding in [0i32, 16, -223] {
        msg.extend_from_slice(&encoding.to_be_bytes());
    }
    session.write_raw(&msg).await?;
    request_update(session, 0, 0, fb.width, fb.height).await?;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut drawn = false;
    while tokio::time::Instant::now() < deadline {
        let msg_type = session.read_raw(1).await?[0];
        match msg_type {
            0 => {
                let _pad = session.read_raw(1).await?;
                let nrects = be16(&session.read_raw(2).await?);
                for _ in 0..nrects {
                    let hdr = session.read_raw(12).await?;
                    let (rx, ry) = (be16(&hdr[0..2]), be16(&hdr[2..4]));
                    let (rw, rh) = (be16(&hdr[4..6]), be16(&hdr[6..8]));
                    let encoding = be32(&hdr[8..12]) as i32;
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
                            let stream =
                                amt_kvm::decoder::rle_stream(&block).map_err(|e| anyhow::anyhow!(e))?;
                            amt_kvm::decoder::draw_rle_tiles(&stream, bpp, rx, ry, rw, rh, &mut fb)
                                .map_err(|e| anyhow::anyhow!(e))?;
                            drawn = true;
                        }
                        -223 => fb.resize(rw, rh),
                        other => anyhow::bail!("unsupported encoding {other}"),
                    }
                }
                if drawn {
                    print_center(&fb);
                    if let Some(path) = screenshot {
                        fb.save_png(path).map_err(|e| anyhow::anyhow!(e))?;
                        println!(
                            "screenshot written to {} ({}x{})",
                            path, fb.width, fb.height
                        );
                        return Ok(());
                    }
                    request_update(session, 0, 0, fb.width, fb.height).await?;
                }
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
    } else {
        println!("no frame drawn within {seconds}s");
    }
    Ok(())
}
