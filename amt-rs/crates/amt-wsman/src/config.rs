/// Minimal configuration used by the WS‑Man client.
/// This type lives in the `amt-wsman` crate to avoid a circular dependency
/// on the CLI crate.
#[derive(Debug, Clone)]
pub struct AmtConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}
