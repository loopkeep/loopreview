//! User settings, read from `<config-dir>/loopreview/config.json`.
//!
//! Settings are optional — a missing or unreadable file falls back to defaults —
//! so the tool always runs. This is separate from the review store (comment
//! data); this holds preferences.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// When the file-explorer sidebar is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarMode {
    /// Shown when the terminal is wide enough (the default).
    Auto,
    /// Preferred shown (still hidden if the terminal is too narrow to fit it).
    Open,
    /// Hidden until toggled with `b`.
    Closed,
}

/// Loaded user preferences.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Minimum body width (columns) at which `auto` layout chooses side-by-side.
    pub split_min_width: usize,
    /// A diff with more changed files than this opens with every file collapsed.
    pub auto_collapse_files: usize,
    /// A diff with more changed lines than this opens with every file collapsed.
    pub auto_collapse_lines: usize,
    /// Whether the file-explorer sidebar is shown by default.
    pub sidebar: SidebarMode,
    /// Minimum diff width (columns) kept beside the sidebar; below this the
    /// sidebar auto-hides so the diff stays usable.
    pub sidebar_min_content: usize,
    /// A fixed sidebar width (columns), clamped to the sensible bounds. When
    /// unset, the width auto-fits the longest file row.
    pub sidebar_width: Option<usize>,
    /// Per-action key overrides (the `[keys]` table); action name → key string.
    pub keys: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            split_min_width: 160,
            auto_collapse_files: 50,
            auto_collapse_lines: 20_000,
            sidebar: SidebarMode::Auto,
            sidebar_min_content: 44,
            sidebar_width: None,
            keys: HashMap::new(),
        }
    }
}

impl Config {
    /// Load settings from `config.toml`, falling back to the deprecated
    /// `config.json` (with a warning) and then to built-in defaults. The second
    /// return value is a one-line notice to surface (a migration hint, or a
    /// parse warning), or `None`.
    pub fn load() -> (Config, Option<String>) {
        let Some(dir) = config_dir().map(|d| d.join("loopreview")) else {
            return (Config::default(), None);
        };

        // Preferred: config.toml.
        if let Ok(text) = std::fs::read_to_string(dir.join("config.toml")) {
            return match toml::from_str(&text) {
                Ok(config) => (config, None),
                Err(e) => (
                    Config::default(),
                    Some(format!("config.toml is invalid ({e}); using defaults")),
                ),
            };
        }

        // Deprecated: config.json (still read, but nudge toward TOML).
        if let Ok(text) = std::fs::read_to_string(dir.join("config.json"))
            && let Ok(config) = serde_json::from_str(&text)
        {
            return (
                config,
                Some("config.json is deprecated — move your settings to config.toml".to_string()),
            );
        }

        (Config::default(), None)
    }
}

/// The directory holding one JSON record per live review session, under the
/// config directory. Used by the control plane's registry.
pub fn sessions_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("loopreview").join("sessions"))
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
