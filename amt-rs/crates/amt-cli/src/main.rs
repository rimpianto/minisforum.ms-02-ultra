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
    /// Open a KVM (remote console) session and report the frames it sends
    Kvm {
        /// How long to stay connected, in seconds
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Open a Serial-over-LAN session and print what the console sends
    Sol {
        /// How long to stay connected, in seconds
        #[arg(long, default_value_t = 5)]
        seconds: u64,
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
        Commands::Kvm { seconds } => {
            let redir_port = cfg.amt.port + 1;
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

            let mut frames = 0usize;
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    res = session.read_frame() => match res {
                        Ok((ty, p)) => {
                            frames += 1;
                            println!("frame 0x{:02x} ({} byte di payload)", ty, p.len());
                        }
                        Err(e) => {
                            eprintln!("KVM read: {e}");
                            break;
                        }
                    }
                }
            }
            println!("{frames} frame ricevuti in {seconds}s");
        }
        Commands::Sol { seconds } => {
            // Redirection listens on the port after WS-Man (16993 -> 16994).
            let redir_port = cfg.amt.port + 1;
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
