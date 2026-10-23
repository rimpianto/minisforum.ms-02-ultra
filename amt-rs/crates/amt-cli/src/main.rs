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
            // If the user omitted the state we only show a notice.
            let requested = match state.as_deref() {
                None => {
                    println!("Query power state – not implemented in this CLI");
                    return Ok(());
                }
                Some(s) => s,
            };

            let power_state = PowerState::from_str(requested).ok_or_else(|| {
                anyhow::anyhow!(format!(
                    "Invalid power state '{}'. Use 'on', 'off', or 'reset'",
                    requested
                ))
            })?;

            // Convert CLI config into the WS‑Man config struct.
            let ws_cfg = WsmanConfig {
                host: cfg.amt.host.clone(),
                port: cfg.amt.port,
                username: cfg.amt.username.clone(),
                password: cfg.amt.password.clone(),
            };
            amt_wsman::set_power(&ws_cfg, power_state).await?;
            println!("Power state change request sent (state = {})", requested);
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
