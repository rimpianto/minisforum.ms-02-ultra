use anyhow::{anyhow, Result};
use amt_wsman::{self, PowerState};

pub fn run(state: Option<String>) -> Result<()> {
    // Load the user configuration (IP, port, credentials)
    let cfg = crate::config::load()?;

    // If no state is supplied we only print a notice – querying the current
    // power state is not yet implemented.
    let requested_state = match state.as_deref() {
        None => {
            println!("Query power state – not implemented in this CLI");
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

