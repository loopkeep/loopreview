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
//!
//! Convention (enforced by `no_two_actions_share_a_default_key`): every action's
//! default key is unique. Unmodified letters are contextual actions; `Ctrl`
//! combos are structural/global (paging, finder, refresh, submit); an uppercase
//! (Shift) letter is the heavier sibling of its lowercase action (`V` select vs
//! `v` layout, `X` close vs `x` resolve, `G`/`g`). When adding an [`Action`],
//! pick a key not already in `DEFAULTS`/`FIXED` and list it in the README
//! `[keys]` table — the test fails on a collision.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyModifiers};

/// A remappable command, interpreted by whichever context is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    Suggest,
    Reply,
    Resolve,
    Fold,
    Select,
    CloseReview,
    Delete,
    Edit,
    ToggleKind,
    ToggleSidebar,
    FileFinder,
    Refresh,
    Submit,
    Palette,
    OpenGithub,
}

impl Action {
    /// Every action, for listing in the command palette.
    pub const ALL: &'static [Action] = &[
        Action::MoveDown,
        Action::MoveUp,
        Action::HalfPageDown,
        Action::HalfPageUp,
        Action::PageDown,
        Action::PageUp,
        Action::Top,
        Action::Bottom,
        Action::NextFile,
        Action::PrevFile,
        Action::NextHunk,
        Action::PrevHunk,
        Action::NavIn,
        Action::NavOut,
        Action::ScrollLeft,
        Action::ScrollRight,
        Action::ToggleLayout,
        Action::Comment,
        Action::Suggest,
        Action::Reply,
        Action::Resolve,
        Action::Fold,
        Action::Select,
        Action::CloseReview,
        Action::Delete,
        Action::Edit,
        Action::ToggleKind,
        Action::ToggleSidebar,
        Action::FileFinder,
        Action::Refresh,
        Action::Submit,
        Action::Palette,
        Action::OpenGithub,
    ];

    /// The config/`[keys]` name — the source of truth for remappable actions is
    /// `DEFAULTS`; the fixed Page keys get a descriptive name.
    pub fn config_name(self) -> &'static str {
        DEFAULTS
            .iter()
            .find(|(a, _, _)| *a == self)
            .map(|(_, name, _)| *name)
            .unwrap_or(match self {
                Action::PageDown => "page_down",
                Action::PageUp => "page_up",
                _ => "?",
            })
    }

    /// A one-line description, for the command palette.
    pub fn describe(self) -> &'static str {
        match self {
            Action::MoveDown => "Move the cursor down",
            Action::MoveUp => "Move the cursor up",
            Action::HalfPageDown => "Scroll half a page down",
            Action::HalfPageUp => "Scroll half a page up",
            Action::PageDown => "Scroll a page down",
            Action::PageUp => "Scroll a page up",
            Action::Top => "Jump to the top",
            Action::Bottom => "Jump to the bottom",
            Action::NextFile => "Next file",
            Action::PrevFile => "Previous file",
            Action::NextHunk => "Next hunk",
            Action::PrevHunk => "Previous hunk",
            Action::NavIn => "Go in: expand, or move into",
            Action::NavOut => "Go out: to the header, then the sidebar",
            Action::ScrollLeft => "Scroll the diff left",
            Action::ScrollRight => "Scroll the diff right",
            Action::ToggleLayout => "Toggle unified / side-by-side",
            Action::Comment => "Comment on the line or selection",
            Action::Suggest => "Suggest a change to the line or selection",
            Action::Reply => "Reply to the thread",
            Action::Resolve => "Resolve or reopen the thread",
            Action::Fold => "Fold / unfold (Enter also toggles a file header)",
            Action::Select => "Start / cancel a line selection",
            Action::CloseReview => "Close (delete) the review",
            Action::Delete => "Withdraw a draft/local comment, or delete your published one",
            Action::Edit => "Edit your own comment",
            Action::ToggleKind => "Toggle the cursor's comment between draft and local note",
            Action::ToggleSidebar => "Toggle the file sidebar",
            Action::FileFinder => "Open the fuzzy file finder",
            Action::Refresh => "Refresh from GitHub",
            Action::Submit => "Open the submit modal",
            Action::Palette => "Open this command palette",
            Action::OpenGithub => "Open the current spot on github.com",
        }
    }
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
    (Action::Suggest, "suggest", "s"),
    (Action::Reply, "reply", "r"),
    (Action::Resolve, "resolve", "x"),
    (Action::Fold, "fold", "o"),
    (Action::Select, "select", "V"),
    (Action::CloseReview, "close_review", "X"),
    (Action::Delete, "delete", "d"),
    (Action::Edit, "edit", "e"),
    (Action::ToggleKind, "toggle_kind", "t"),
    (Action::ToggleSidebar, "sidebar", "b"),
    (Action::FileFinder, "file_finder", "ctrl+p"),
    (Action::Refresh, "refresh", "ctrl+r"),
    (Action::Submit, "submit", "ctrl+s"),
    (Action::Palette, "palette", "?"),
    (Action::OpenGithub, "open_github", "ctrl+o"),
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
    ("left", Action::NavOut),
    ("}", Action::NextHunk),
    ("{", Action::PrevHunk),
];

/// Keys the review UI intercepts before consulting this map — quit (`q` / `Esc`
/// / `Ctrl-C`), the view switch (`Tab`), and the confirm-modal `y`. They cannot
/// be reassigned to an action (a remap onto one is silently shadowed), so no
/// remappable action's default may sit on one — the `defaults_avoid_structural`
/// test enforces it. (`Enter` is handled directly by the UI too: contextual —
/// it toggles the fold on a file header and otherwise means `NavIn` — so it is
/// not in the map; the finder and composer reserve their own modal keys.)
#[cfg(test)]
const STRUCTURAL: &[&str] = &["q", "esc", "ctrl+c", "tab", "y"];

/// A resolved key-to-action map.
#[derive(Debug, Clone)]
pub struct Keymap {
    map: HashMap<(KeyCode, KeyModifiers), Action>,
    /// Each action's primary key as a display string (the override or default),
    /// for showing "the key that runs this" in the command palette.
    primary: HashMap<Action, String>,
}

impl Keymap {
    /// Build the keymap from the defaults, applying per-action `overrides` (from
    /// the config's `[keys]` table). Returns a list of human-readable errors for
    /// any unknown action name or unparseable key, so startup can report exactly
    /// which line is wrong.
    pub fn from_overrides(overrides: &HashMap<String, String>) -> Result<Keymap, Vec<String>> {
        let mut map = HashMap::new();
        let mut errors = Vec::new();
        // The fixed Page keys are not remappable but still shown in the palette.
        let mut primary: HashMap<Action, String> = HashMap::new();
        primary.insert(Action::PageDown, "PgDn".to_string());
        primary.insert(Action::PageUp, "PgUp".to_string());

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
                    primary.insert(*action, key.to_string());
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
            Ok(Keymap { map, primary })
        } else {
            Err(errors)
        }
    }

    /// The display string of the key that runs `action` (the override or default),
    /// or `None` for an action with no bound key.
    pub fn key_for(&self, action: Action) -> Option<&str> {
        self.primary.get(&action).map(String::as_str)
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

    #[test]
    fn no_two_actions_share_a_default_key() {
        // The permanent rule, machine-checked: every remappable action's default
        // key is unique, so the built-in keymap can never silently shadow one
        // action with another. Adding an `Action` means choosing a free key (and
        // listing it in the README `[keys]` table).
        let mut seen: HashMap<(KeyCode, KeyModifiers), &str> = HashMap::new();
        for (_, name, key) in DEFAULTS {
            let binding = parse_key(key).expect("every default key parses");
            if let Some(prev) = seen.insert(binding, name) {
                panic!("default key `{key}` is bound to both `{prev}` and `{name}`");
            }
        }
    }

    #[test]
    fn defaults_avoid_structural_keys() {
        // A remappable action's default must not sit on a key the UI handles
        // before the keymap (quit / view switch / confirm) — the structural
        // handler would silently shadow it. Users likewise cannot remap onto
        // these (documented as reserved).
        let structural: Vec<(KeyCode, KeyModifiers)> =
            STRUCTURAL.iter().map(|k| parse_key(k).unwrap()).collect();
        for (_, name, key) in DEFAULTS {
            let binding = parse_key(key).unwrap();
            assert!(
                !structural.contains(&binding),
                "default `{name}` = `{key}` sits on a structural key the UI intercepts"
            );
        }
    }

    #[test]
    fn fixed_alternates_never_shadow_a_different_action() {
        // A fixed alternate may intentionally duplicate a default (`right` and
        // `l` both mean NavIn), but must never bind a key that a default already
        // gives to a *different* action.
        let defaults: HashMap<(KeyCode, KeyModifiers), Action> = DEFAULTS
            .iter()
            .map(|(action, _, key)| (parse_key(key).unwrap(), *action))
            .collect();
        for (key, action) in FIXED {
            let binding = parse_key(key).unwrap();
            if let Some(default_action) = defaults.get(&binding) {
                assert_eq!(
                    default_action, action,
                    "fixed key `{key}` collides with a default bound elsewhere"
                );
            }
        }
    }
}
