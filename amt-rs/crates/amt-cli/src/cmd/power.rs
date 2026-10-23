use anyhow::{anyhow, Result};
use amt_wsman::{self, PowerState, get_power_state};

pub fn run(state: Option<String>) -> Result<()> {
    // Load the user configuration (IP, port, credentials)
    let cfg = crate::config::load()?;

    // If no state is supplied we query the current power state via WS‑Man.
    let requested_state = match state.as_deref() {
        None => {
            // Asynchronously fetch the state.
            let (ps, rps) = tokio::runtime::Handle::current()
                .block_on(amt_wsman::get_power_state(&cfg.amt))?;
            // Map numeric values to friendly names.
            fn map_state(v: u8) -> &'static str {
                match v {
                    1 => "Full Power",
                    2 => "On",
                    3 => "Sleep-Light",
                    4 => "Sleep-Deep",
                    6 => "Off-Hard",
                    8 => "Off-Soft",
                    10 => "Power Cycle Graceful",
                    11 => "Reset",
                    _ => "Unknown",
                }
            }
            println!(
                "PowerState={} ({}) RequestedPowerState={} ({})",
                ps,
                map_state(ps),
                rps,
                map_state(rps)
            );
            return Ok(());
        }
        Some(s) => s,
    };

    // Convert the textual argument into the PowerState enum used by the WS‑Man
    // library.  The enum maps directly to the numeric values required by the
    // AMT SOAP request.
    let power_state = PowerState::from_str(requested_state).ok_or_else(|| {
        anyhow!(format!(
            "Invalid power state '{}'. Use 'on', 'off', or 'reset'",
            requested_state
        ))
    })?;

    // Call the async WS‑Man client. Because the CLI is synchronous we block
    // on the future using `tokio::runtime::Handle::current().block_on`.
    // This keeps the public API of `run` simple (Result<()>).
    tokio::runtime::Handle::current().block_on(amt_wsman::set_power(&cfg.amt, power_state))?;

    println!("Power state change request sent (state = {})", requested_state);
    Ok(())
}

