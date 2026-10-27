use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Where the config is looked for, in order: `AMT_CONFIG`, then the XDG
/// location (`$XDG_CONFIG_HOME/amt/config.yaml`, usually
/// `~/.config/amt/config.yaml`), then the place this tool used first
/// (`~/.amt/config.yaml`), which still works so nobody has to move a file.
pub fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(explicit) = std::env::var("AMT_CONFIG") {
        if !explicit.is_empty() {
            out.push(PathBuf::from(explicit));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            out.push(PathBuf::from(xdg).join("amt/config.yaml"));
        }
    }
    if let Some(home) = home() {
        out.push(home.join(".config/amt/config.yaml"));
        out.push(home.join(".amt/config.yaml"));
    }
    out
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The config file in use, if there is one.
pub fn config_path() -> Option<PathBuf> {
    candidates().into_iter().find(|p| p.is_file())
}

/// Directory that holds the config (or would hold it): history sits next to
/// the file it belongs to.
pub fn config_dir() -> PathBuf {
    if let Some(p) = config_path() {
        if let Some(dir) = p.parent() {
            return dir.to_path_buf();
        }
    }
    candidates()
        .into_iter()
        .next()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where `use <name>` keeps its command history: beside the config file.
pub fn history_path() -> PathBuf {
    config_dir().join("history")
}

/// Human-readable list of the places we look, for error messages.
fn where_we_look() -> String {
    candidates()
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(" or ")
}

/// The config file holds any number of machines plus an optional default. The
/// old single-machine form (`amt: { host: ... }`) still loads, so a config
/// written before this change keeps working.
#[derive(Debug, Deserialize, Default)]
pub struct Config {
    /// Machine used when the command line does not name one.
    #[serde(default)]
    pub default: Option<String>,
    /// name -> machine; a BTreeMap so `amt-cli machines` lists them sorted.
    #[serde(default)]
    pub machines: BTreeMap<String, AmtConfig>,
    /// Legacy single-machine block, used when `machines` is absent.
    #[serde(default)]
    pub amt: Option<AmtConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AmtConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_user")]
    pub username: String,
    pub password: String,
    /// Where the redirection service listens. The firmware puts it at port + 2
    /// (16993 -> 16995), which is what an omitted value means.
    #[serde(default)]
    pub redirection_port: Option<u16>,
}

fn default_port() -> u16 {
    16993
}

fn default_user() -> String {
    "admin".to_string()
}

impl Default for AmtConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: default_port(),
            username: default_user(),
            password: String::new(),
            redirection_port: None,
        }
    }
}

impl AmtConfig {
    pub fn redirection_port(&self) -> u16 {
        self.redirection_port.unwrap_or(self.port + 2)
    }
}

/// A machine together with the name it is configured under.
#[derive(Debug, Clone)]
pub struct Machine {
    pub name: String,
    pub amt: AmtConfig,
}

impl Config {
    /// Every configured machine, legacy block included.
    pub fn list(&self) -> Vec<Machine> {
        let mut out: Vec<Machine> = self
            .machines
            .iter()
            .map(|(name, amt)| Machine {
                name: name.clone(),
                amt: amt.clone(),
            })
            .collect();
        if out.is_empty() {
            if let Some(amt) = self.amt.as_ref() {
                out.push(Machine {
                    name: "default".to_string(),
                    amt: amt.clone(),
                });
            }
        }
        out
    }

    /// Which machine to talk to: an explicit name (--machine or AMT_MACHINE)
    /// wins, then `default:`, then the only machine there is.
    pub fn resolve(&self, requested: Option<&str>) -> Result<Machine> {
        let all = self.list();
        if all.is_empty() {
            // No file at all: the environment alone is enough to work on one
            // machine, which is how the tool is tried out.
            if let Ok(host) = std::env::var("AMT_HOST") {
                return Ok(Machine {
                    name: "env".to_string(),
                    amt: AmtConfig {
                        host,
                        ..AmtConfig::default()
                    },
                });
            }
            bail!(
                "no AMT machine configured: create {} (see config.example.yaml) or set AMT_HOST",
                where_we_look()
            );
        }

        let want = requested
            .map(|s| s.to_string())
            .or_else(|| std::env::var("AMT_MACHINE").ok())
            .or_else(|| self.default.clone());

        match want {
            Some(name) => all.iter().find(|m| m.name == name).cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "no machine called '{}'; configured: {}",
                    name,
                    all.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")
                )
            }),
            None if all.len() == 1 => Ok(all[0].clone()),
            None => bail!(
                "several machines configured and none is the default: use --machine <name> or set default: ({})",
                all.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

/// Read the config file. Environment overrides are applied later, on the
/// machine that was actually chosen.
pub fn load() -> Result<Config> {
    match config_path() {
        Some(path) => {
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            serde_yaml::from_str::<Config>(&contents)
                .with_context(|| format!("Failed to parse {}", path.display()))
        }
        None => Ok(Config::default()),
    }
}

/// Apply AMT_HOST / AMT_PORT / AMT_USER / AMT_PASS / AMT_REDIR_PORT on top of
/// the chosen machine, and refuse to go on with half a configuration.
pub fn with_env_overrides(mut machine: Machine) -> Result<Machine> {
    if let Ok(v) = std::env::var("AMT_HOST") {
        machine.amt.host = v;
    }
    if let Ok(v) = std::env::var("AMT_PORT") {
        machine.amt.port = v.parse().context("AMT_PORT is not a number")?;
    }
    if let Ok(v) = std::env::var("AMT_USER") {
        machine.amt.username = v;
    }
    if let Ok(v) = std::env::var("AMT_PASS") {
        machine.amt.password = v;
    }
    if let Ok(v) = std::env::var("AMT_REDIR_PORT") {
        machine.amt.redirection_port = Some(v.parse().context("AMT_REDIR_PORT is not a number")?);
    }

    if machine.amt.host.is_empty() {
        bail!(
            "no AMT host for '{}': set AMT_HOST or fill it in {}",
            machine.name,
            config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(where_we_look)
        );
    }
    if machine.amt.password.is_empty() {
        bail!(
            "no AMT password for '{}': set AMT_PASS or fill it in {}",
            machine.name,
            config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(where_we_look)
        );
    }
    Ok(machine)
}
