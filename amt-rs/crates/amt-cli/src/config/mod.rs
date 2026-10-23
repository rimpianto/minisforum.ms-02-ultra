use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub amt: AmtConfig,
}

#[derive(Debug, Deserialize)]
pub struct AmtConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl Default for AmtConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 16993,
            username: "admin".to_string(),
            password: String::new(),
        }
    }
}

/// Configuration is read from `~/.amt/config.yaml`; every field can be
/// overridden by an environment variable, so no file is needed on a machine
/// where the tool is just being tested (AMT_HOST, AMT_PORT, AMT_USER, AMT_PASS).
pub fn load() -> Result<Config> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let path = format!("{}/.amt/config.yaml", home);

    let mut cfg = match fs::read_to_string(&path) {
        Ok(contents) => serde_yaml::from_str::<Config>(&contents)
            .with_context(|| format!("Failed to parse {}", path))?,
        Err(_) => Config {
            amt: AmtConfig::default(),
        },
    };

    if let Ok(v) = std::env::var("AMT_HOST") {
        cfg.amt.host = v;
    }
    if let Ok(v) = std::env::var("AMT_PORT") {
        cfg.amt.port = v.parse().context("AMT_PORT is not a number")?;
    }
    if let Ok(v) = std::env::var("AMT_USER") {
        cfg.amt.username = v;
    }
    if let Ok(v) = std::env::var("AMT_PASS") {
        cfg.amt.password = v;
    }

    if cfg.amt.host.is_empty() {
        bail!("no AMT host: set AMT_HOST or create {}", path);
    }
    if cfg.amt.password.is_empty() {
        bail!("no AMT password: set AMT_PASS or create {}", path);
    }

    Ok(cfg)
}
