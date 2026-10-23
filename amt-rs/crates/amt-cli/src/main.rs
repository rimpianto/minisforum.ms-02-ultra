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
    /// Placeholder – will launch the KVM GUI later
    Kvm,
    /// Placeholder – will launch the SOL terminal later
    Sol,
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
        Commands::Kvm => {
            println!("KVM GUI launch – not yet implemented");
        }
        Commands::Sol => {
            println!("SOL terminal launch – not yet implemented");
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
