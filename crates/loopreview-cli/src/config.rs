//! User settings, read from `<config-dir>/loopreview/config.json`.
//!
//! Settings are optional — a missing or unreadable file falls back to defaults —
//! so the tool always runs. This is separate from the review store (comment
//! data); this holds preferences.

use std::path::PathBuf;

use serde::Deserialize;

/// Loaded user preferences.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Minimum body width (columns) at which `auto` layout chooses side-by-side.
    pub split_min_width: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            split_min_width: 160,
        }
    }
}

impl Config {
    /// Load settings, falling back to defaults when the file is absent or
    /// malformed.
    pub fn load() -> Config {
        config_dir()
            .map(|dir| dir.join("loopreview").join("config.json"))
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

/// The user's config directory: `$XDG_CONFIG_HOME` or `~/.config` on Unix,
/// `%APPDATA%` on Windows. Shared by the config and the review store.
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }
}
