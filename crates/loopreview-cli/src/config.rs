//! User settings, read from `<config-dir>/loopreview/config.toml` (a legacy
//! `config.json` is still read, with a migration hint).
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

/// What the Enter key does in the comment composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComposerEnter {
    /// Enter inserts a newline; Ctrl-S saves (the default — reliable in every
    /// terminal, and multi-line comments and suggestions need dependable
    /// newlines).
    Newline,
    /// Enter saves; a modifier (Shift+Enter, or Alt+Enter) inserts a newline.
    /// Opt-in, for terminals where the Kitty keyboard protocol reliably reports
    /// the modifier through to the app.
    Save,
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
    /// What Enter does in the comment composer: insert a newline (the default,
    /// with Ctrl-S to save) or save (with Shift/Alt+Enter for a newline).
    pub composer_enter: ComposerEnter,
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
            composer_enter: ComposerEnter::Newline,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_enter_defaults_to_newline_and_parses_the_opt_in() {
        // Absent: the reliable default (Enter is a newline, Ctrl-S saves).
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.composer_enter, ComposerEnter::Newline);
        // The opt-in for terminals where the Kitty protocol reaches the app.
        let cfg: Config = toml::from_str("composer_enter = \"save\"").unwrap();
        assert_eq!(cfg.composer_enter, ComposerEnter::Save);
        // And the default named explicitly.
        let cfg: Config = toml::from_str("composer_enter = \"newline\"").unwrap();
        assert_eq!(cfg.composer_enter, ComposerEnter::Newline);
    }
}
