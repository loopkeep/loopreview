//! Configurable key bindings.
//!
//! Every command the review UI understands is an [`Action`]. A [`Keymap`] maps a
//! pressed key to an action; each action interprets itself in the active context
//! (for example [`Action::MoveDown`] moves the diff cursor, the selected thread,
//! or the sidebar selection). The bindings come from built-in defaults, which the
//! user can override per action in the config's `[keys]` table
//! (`cursor_down = "j"`). Fixed alternates — the arrow, page, and home/end keys —
//! always work and are not remappable.
//!
//! Structural keys (quit, view switch, and the modal keys such as Ctrl-S / Esc in
//! the composer) are handled directly by the UI, not through this map.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyModifiers};

/// A remappable command, interpreted by whichever context is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    MoveDown,
    MoveUp,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    Top,
    Bottom,
    NextFile,
    PrevFile,
    NextHunk,
    PrevHunk,
    NavIn,
    NavOut,
    ScrollLeft,
    ScrollRight,
    ToggleLayout,
    Comment,
    Reply,
    Resolve,
    Fold,
    Select,
    CloseReview,
    ToggleSidebar,
    FileFinder,
    Refresh,
    Submit,
}

/// Each action's config name and its default (remappable) key. The Page keys are
/// not here — they are the fixed `PageUp`/`PageDown` keys (plus `space`).
const DEFAULTS: &[(Action, &str, &str)] = &[
    (Action::MoveDown, "cursor_down", "j"),
    (Action::MoveUp, "cursor_up", "k"),
    (Action::HalfPageDown, "half_page_down", "ctrl+d"),
    (Action::HalfPageUp, "half_page_up", "ctrl+u"),
    (Action::Top, "top", "g"),
    (Action::Bottom, "bottom", "G"),
    (Action::NextFile, "next_file", "n"),
    (Action::PrevFile, "prev_file", "p"),
    (Action::NextHunk, "next_hunk", "]"),
    (Action::PrevHunk, "prev_hunk", "["),
    (Action::NavIn, "nav_in", "l"),
    (Action::NavOut, "nav_out", "h"),
    (Action::ScrollLeft, "scroll_left", "<"),
    (Action::ScrollRight, "scroll_right", ">"),
    (Action::ToggleLayout, "layout_toggle", "v"),
    (Action::Comment, "comment", "c"),
    (Action::Reply, "reply", "r"),
    (Action::Resolve, "resolve", "x"),
    (Action::Fold, "fold", "o"),
    (Action::Select, "select", "V"),
    (Action::CloseReview, "close_review", "X"),
    (Action::ToggleSidebar, "sidebar", "b"),
    (Action::FileFinder, "file_finder", "ctrl+p"),
    (Action::Refresh, "refresh", "ctrl+r"),
    (Action::Submit, "submit", "ctrl+s"),
];

/// Fixed alternate keys, always mapped regardless of the config.
const FIXED: &[(&str, Action)] = &[
    ("down", Action::MoveDown),
    ("up", Action::MoveUp),
    ("pagedown", Action::PageDown),
    ("pageup", Action::PageUp),
    ("space", Action::PageDown),
    ("home", Action::Top),
    ("end", Action::Bottom),
    ("right", Action::NavIn),
    ("enter", Action::NavIn),
    ("left", Action::NavOut),
    ("}", Action::NextHunk),
    ("{", Action::PrevHunk),
];

/// A resolved key-to-action map.
#[derive(Debug, Clone)]
pub struct Keymap {
    map: HashMap<(KeyCode, KeyModifiers), Action>,
}

impl Keymap {
    /// Build the keymap from the defaults, applying per-action `overrides` (from
    /// the config's `[keys]` table). Returns a list of human-readable errors for
    /// any unknown action name or unparseable key, so startup can report exactly
    /// which line is wrong.
    pub fn from_overrides(overrides: &HashMap<String, String>) -> Result<Keymap, Vec<String>> {
        let mut map = HashMap::new();
        let mut errors = Vec::new();

        // Fixed alternates first (a config override can shadow them if it collides).
        for (key, action) in FIXED {
            if let Ok(binding) = parse_key(key) {
                map.insert(binding, *action);
            }
        }
        // Each action's primary key: the override if present, else the default.
        for (action, name, default) in DEFAULTS {
            let key = overrides.get(*name).map(String::as_str).unwrap_or(default);
            match parse_key(key) {
                Ok(binding) => {
                    map.insert(binding, *action);
                }
                Err(reason) => errors.push(format!("keys.{name} = \"{key}\": {reason}")),
            }
        }
        // Flag override names that match no action.
        for name in overrides.keys() {
            if !DEFAULTS.iter().any(|(_, n, _)| n == name) {
                errors.push(format!("keys.{name}: unknown action"));
            }
        }

        if errors.is_empty() {
            Ok(Keymap { map })
        } else {
            Err(errors)
        }
    }

    /// The default keymap (no overrides). Cannot fail — the defaults are valid.
    pub fn defaults() -> Keymap {
        Keymap::from_overrides(&HashMap::new()).expect("built-in bindings are valid")
    }

    /// The action bound to a pressed key, if any. Shift is normalised away (the
    /// character already encodes case), so `V` matches whether or not the
    /// terminal also reports Shift.
    pub fn action(&self, code: KeyCode, mods: KeyModifiers) -> Option<Action> {
        self.map
            .get(&(code, mods.difference(KeyModifiers::SHIFT)))
            .copied()
    }
}

/// Parse a key string such as `j`, `V`, `ctrl+d`, `enter`, `space`, `pageup`
/// into a `(KeyCode, KeyModifiers)`. Shift is folded into the character's case,
/// so the result never carries `SHIFT`.
pub fn parse_key(spec: &str) -> Result<(KeyCode, KeyModifiers), String> {
    if spec.is_empty() {
        return Err("empty key".to_string());
    }
    let mut mods = KeyModifiers::NONE;
    let mut shift = false;
    let parts: Vec<&str> = spec.split('+').collect();
    let (name, modifiers) = parts.split_last().unwrap();
    for m in modifiers {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" => mods |= KeyModifiers::ALT,
            "shift" => shift = true,
            other => return Err(format!("unknown modifier `{other}`")),
        }
    }

    let code = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "backspace" => KeyCode::Backspace,
        _ => {
            let mut chars = name.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else {
                return Err(format!("unknown key `{name}`"));
            };
            // Shift on a letter means its uppercase form (how terminals deliver it).
            let ch = if shift { ch.to_ascii_uppercase() } else { ch };
            KeyCode::Char(ch)
        }
    };
    Ok((code, mods))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_modified_keys() {
        assert_eq!(
            parse_key("j").unwrap(),
            (KeyCode::Char('j'), KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key("ctrl+d").unwrap(),
            (KeyCode::Char('d'), KeyModifiers::CONTROL)
        );
        // Shift folds into the character's case, not a modifier.
        assert_eq!(
            parse_key("shift+v").unwrap(),
            (KeyCode::Char('V'), KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key("V").unwrap(),
            (KeyCode::Char('V'), KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key("enter").unwrap(),
            (KeyCode::Enter, KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key("space").unwrap(),
            (KeyCode::Char(' '), KeyModifiers::NONE)
        );
        assert_eq!(
            parse_key("pageup").unwrap(),
            (KeyCode::PageUp, KeyModifiers::NONE)
        );
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(parse_key("").is_err());
        assert!(parse_key("hyper+x").is_err()); // unknown modifier
        assert!(parse_key("nope").is_err()); // unknown named key
    }

    #[test]
    fn default_keymap_resolves_actions() {
        let km = Keymap::defaults();
        assert_eq!(
            km.action(KeyCode::Char('j'), KeyModifiers::NONE),
            Some(Action::MoveDown)
        );
        assert_eq!(
            km.action(KeyCode::Down, KeyModifiers::NONE),
            Some(Action::MoveDown)
        );
        assert_eq!(
            km.action(KeyCode::Char('p'), KeyModifiers::CONTROL),
            Some(Action::FileFinder)
        );
        // Shift is normalised: V resolves whether or not SHIFT is also reported.
        assert_eq!(
            km.action(KeyCode::Char('V'), KeyModifiers::SHIFT),
            Some(Action::Select)
        );
        assert_eq!(km.action(KeyCode::Char('z'), KeyModifiers::NONE), None);
    }

    #[test]
    fn overrides_remap_and_report_errors() {
        let mut over = HashMap::new();
        over.insert("comment".to_string(), "m".to_string());
        let km = Keymap::from_overrides(&over).unwrap();
        assert_eq!(
            km.action(KeyCode::Char('m'), KeyModifiers::NONE),
            Some(Action::Comment)
        );
        // The default `c` is gone (replaced).
        assert_eq!(km.action(KeyCode::Char('c'), KeyModifiers::NONE), None);

        let mut bad = HashMap::new();
        bad.insert("comment".to_string(), "ctrl+".to_string()); // trailing +
        bad.insert("nonexistent".to_string(), "z".to_string());
        let errors = Keymap::from_overrides(&bad).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("comment")));
        assert!(errors.iter().any(|e| e.contains("nonexistent")));
    }
}
