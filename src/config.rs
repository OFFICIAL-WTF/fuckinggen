use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::files::{atomic_write, home_dir};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// TUI accent name: blue, cyan, magenta, green, yellow, red, white.
    pub accent: Option<String>,
    /// Default quality for new generations.
    pub quality: Option<String>,
    /// Default save directory.
    pub out_dir: Option<String>,
    /// Generations this install has produced, used to time the coffee nudge.
    pub runs: Option<u32>,
    /// The first-run tour has been seen (or skipped).
    pub intro_seen: Option<bool>,
    /// Whether the coffee popup is allowed to appear at all.
    pub coffee_enabled: Option<bool>,
    /// Whether it already appeared, so it only ever shows once.
    pub coffee_shown: Option<bool>,
    /// Best snake run, because a high score that resets is not a high score.
    pub snake_high: Option<u32>,
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    config_dir().join("fuckinggen").join("config.json")
}

pub fn load() -> Config {
    load_from(&config_path())
}

pub fn load_from(path: &Path) -> Config {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

pub fn save(config: &Config) -> Result<()> {
    save_to(&config_path(), config)
}

pub fn save_to(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(config)?;
    atomic_write(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let config = Config {
            accent: Some("blue".into()),
            quality: Some("high".into()),
            out_dir: Some("/tmp/out".into()),
            runs: Some(9),
            intro_seen: Some(true),
            coffee_enabled: Some(false),
            coffee_shown: Some(true),
            snake_high: Some(7),
        };
        save_to(&path, &config).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.accent.as_deref(), Some("blue"));
        assert_eq!(loaded.quality.as_deref(), Some("high"));
        assert_eq!(loaded.out_dir.as_deref(), Some("/tmp/out"));
        assert_eq!(loaded.runs, Some(9));
        assert_eq!(loaded.intro_seen, Some(true));
        assert_eq!(loaded.coffee_enabled, Some(false));
        assert_eq!(loaded.coffee_shown, Some(true));
        assert_eq!(loaded.snake_high, Some(7));
    }

    #[test]
    fn missing_config_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        let config = load_from(&tmp.path().join("nope.json"));
        assert!(config.accent.is_none());
    }
}
