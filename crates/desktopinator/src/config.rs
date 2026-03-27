use serde::Deserialize;
use std::path::PathBuf;

/// Top-level configuration loaded from `~/.config/desktopinator/config.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub background: Option<String>,
    pub gap: Option<i32>,
    pub layout: Option<String>,

    #[serde(default)]
    pub vnc: VncConfig,

    #[serde(default)]
    pub rdp: RdpConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct VncConfig {
    pub port: u16,
}

impl Default for VncConfig {
    fn default() -> Self {
        Self { port: 5900 }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct RdpConfig {
    pub port: u16,
}

impl Default for RdpConfig {
    fn default() -> Self {
        Self { port: 3389 }
    }
}

/// Load config from `~/.config/desktopinator/config.toml`.
/// Returns default config if the file doesn't exist.
pub fn load_config() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => match toml::from_str(&contents) {
            Ok(config) => {
                tracing::info!(path = %path.display(), "loaded config");
                config
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to parse config, using defaults");
                Config::default()
            }
        },
        Err(_) => {
            tracing::debug!(path = %path.display(), "no config file found, using defaults");
            Config::default()
        }
    }
}

fn config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("desktopinator/config.toml")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/desktopinator/config.toml")
    } else {
        PathBuf::from("/etc/desktopinator/config.toml")
    }
}
