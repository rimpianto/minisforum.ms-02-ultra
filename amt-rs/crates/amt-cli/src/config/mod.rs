use serde::Deserialize;
use std::fs;
use anyhow::{Result, Context};

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

pub fn load() -> Result<Config> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let path = format!("{}/.amt/config.yaml", home);
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config file {}", path))?;
    let cfg: Config = serde_yaml::from_str(&contents)
        .with_context(|| "Failed to parse YAML config")?;
    Ok(cfg)
}
