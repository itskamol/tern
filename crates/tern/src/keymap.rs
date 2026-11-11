// Adapted from zeron crates/ui/src/settings.rs (combo_from_keystroke_on, badge_combo_on) and
// crates/ui/src/settings/shortcuts.rs (record_key, RecordOutcome, refusal) (MIT).
//! Every rebindable shortcut, the user's overrides in `keymap.json`, and the pure helpers the
//! Shortcuts page records with. ⌘ chords belong to tern; everything else reaches the remote
//! shell, so a recorded shortcut must use ⌘, ⌃ or ⌥ (or be a function key).
//!
//! Windows and Linux. ⌘ is the Super key there, which window managers keep, so the defaults
//! are a second table (`other_combo`) built on the rule terminals on those systems follow: plain
//! Ctrl-<letter> belongs to the shell (^C, ^D, ^W, ^K, ^R, ^B, ^N ...), so an app shortcut is
//! Ctrl+Shift-<key>, or Ctrl+Alt-<key> where Ctrl+Shift is already taken (the terminal's own
//! copy, paste and find use Ctrl+Shift+C/V/F). Keys that send nothing to a shell (Ctrl+comma,
//! Ctrl+=, Ctrl+-, Ctrl+0) stay plain Ctrl. The file stores combos in the same `cmd-`/`ctrl-`
//! spelling everywhere; a combo recorded on one system binds as written on the other.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use gpui::{Action, App, Global, KeyBinding};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "keymap.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShortcutId {
    HostPicker,
    SearchHosts,
    Snippets,
    NewConnection,
    SplitRight,
    SplitDown,
    PaneLeft,
    PaneRight,
    PaneUp,
    PaneDown,
    Settings,
    ToggleSidebar,
    BrowseFiles,
    CloseTab,
    NextTab,
    PrevTab,
    FontBigger,
    FontSmaller,
    FontReset,
    FillPassword,
    ClearTerminal,
}

impl ShortcutId {
    pub const ALL: [ShortcutId; 21] = [
        ShortcutId::HostPicker,
        ShortcutId::SearchHosts,
        ShortcutId::Snippets,
        ShortcutId::NewConnection,
        ShortcutId::SplitRight,
        ShortcutId::SplitDown,
        ShortcutId::PaneLeft,
        ShortcutId::PaneRight,
        ShortcutId::PaneUp,
        ShortcutId::PaneDown,
        ShortcutId::Settings,
        ShortcutId::ToggleSidebar,
        ShortcutId::BrowseFiles,
        ShortcutId::CloseTab,
        ShortcutId::NextTab,
        ShortcutId::PrevTab,
        ShortcutId::FontBigger,
        ShortcutId::FontSmaller,
        ShortcutId::FontReset,
        ShortcutId::FillPassword,
        ShortcutId::ClearTerminal,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ShortcutId::HostPicker => "New tab (pick a host)",
            ShortcutId::SearchHosts => "Search hosts",
            ShortcutId::Snippets => "Snippets",
            ShortcutId::NewConnection => "New connection",
            ShortcutId::SplitRight => "Split right",
            ShortcutId::SplitDown => "Split down",
            ShortcutId::PaneLeft => "Focus pane left",
            ShortcutId::PaneRight => "Focus pane right",
            ShortcutId::PaneUp => "Focus pane up",
            ShortcutId::PaneDown => "Focus pane down",
            ShortcutId::Settings => "Open settings",
            ShortcutId::ToggleSidebar => "Toggle sidebar",
            ShortcutId::BrowseFiles => "Files tab",
            ShortcutId::CloseTab => "Close tab",
            ShortcutId::NextTab => "Next tab",
            ShortcutId::PrevTab => "Previous tab",
            ShortcutId::FontBigger => "Bigger text",
            ShortcutId::FontSmaller => "Smaller text",
            ShortcutId::FontReset => "Default text size",
            ShortcutId::FillPassword => "Fill password",
            ShortcutId::ClearTerminal => "Clear scrollback",
        }
    }

    fn mac_combo(self) -> &'static str {
        match self {
            ShortcutId::HostPicker => "cmd-t",
            ShortcutId::SearchHosts => "cmd-shift-f",
            ShortcutId::Snippets => "cmd-shift-s",
            ShortcutId::NewConnection => "cmd-n",
            ShortcutId::SplitRight => "cmd-d",
            ShortcutId::SplitDown => "cmd-shift-d",
            ShortcutId::PaneLeft => "alt-cmd-left",
            ShortcutId::PaneRight => "alt-cmd-right",
            ShortcutId::PaneUp => "alt-cmd-up",
            ShortcutId::PaneDown => "alt-cmd-down",
            ShortcutId::Settings => "cmd-,",
            ShortcutId::ToggleSidebar => "cmd-b",
            ShortcutId::BrowseFiles => "cmd-shift-o",
            ShortcutId::CloseTab => "cmd-w",
            ShortcutId::NextTab => "cmd-shift-]",
            ShortcutId::PrevTab => "cmd-shift-[",
            ShortcutId::FontBigger => "cmd-=",
            ShortcutId::FontSmaller => "cmd--",
            ShortcutId::FontReset => "cmd-0",
            ShortcutId::FillPassword => "cmd-\\",
            ShortcutId::ClearTerminal => "cmd-k",
        }
    }

    fn other_combo(self) -> &'static str {
        match self {
            ShortcutId::HostPicker => "ctrl-shift-t",
            ShortcutId::SearchHosts => "ctrl-alt-f",
            ShortcutId::Snippets => "ctrl-alt-s",
            ShortcutId::NewConnection => "ctrl-shift-n",
            ShortcutId::SplitRight => "ctrl-shift-d",
            ShortcutId::SplitDown => "ctrl-alt-d",
            ShortcutId::PaneLeft => "ctrl-alt-left",
            ShortcutId::PaneRight => "ctrl-alt-right",
            ShortcutId::PaneUp => "ctrl-alt-up",
            ShortcutId::PaneDown => "ctrl-alt-down",
            ShortcutId::Settings => "ctrl-,",
            ShortcutId::ToggleSidebar => "ctrl-shift-b",
            ShortcutId::BrowseFiles => "ctrl-alt-o",
            ShortcutId::CloseTab => "ctrl-shift-w",
            ShortcutId::NextTab => "ctrl-shift-]",
            ShortcutId::PrevTab => "ctrl-shift-[",
            ShortcutId::FontBigger => "ctrl-=",
            ShortcutId::FontSmaller => "ctrl--",
            ShortcutId::FontReset => "ctrl-0",
            // Ctrl+\ alone is SIGQUIT in a terminal.
            ShortcutId::FillPassword => "ctrl-shift-\\",
            ShortcutId::ClearTerminal => "ctrl-shift-k",
        }
    }

    pub fn default_combo(self) -> &'static str {
        if cfg!(target_os = "macos") {
            self.mac_combo()
        } else {
            self.other_combo()
        }
    }

    fn action(self) -> Box<dyn Action> {
        use crate::shell::{
            DecreaseFontSize, FocusPaneDown, FocusPaneLeft, FocusPaneRight, FocusPaneUp,
            IncreaseFontSize, NewConnection, OpenSettings, ResetFontSize, SplitDown, SplitRight,
            ToggleSidebar,
        };
        use crate::tabs::{CloseTab, NextTab, PrevTab};
        match self {
            ShortcutId::HostPicker => Box::new(crate::picker::ToggleHostPicker),
            ShortcutId::SearchHosts => Box::new(crate::shell::FocusHostSearch),
            ShortcutId::Snippets => Box::new(crate::shell::ToggleSnippets),
            ShortcutId::NewConnection => Box::new(NewConnection),
            ShortcutId::SplitRight => Box::new(SplitRight),
            ShortcutId::SplitDown => Box::new(SplitDown),
            ShortcutId::PaneLeft => Box::new(FocusPaneLeft),
            ShortcutId::PaneRight => Box::new(FocusPaneRight),
            ShortcutId::PaneUp => Box::new(FocusPaneUp),
            ShortcutId::PaneDown => Box::new(FocusPaneDown),
            ShortcutId::Settings => Box::new(OpenSettings),
            ShortcutId::ToggleSidebar => Box::new(ToggleSidebar),
            ShortcutId::BrowseFiles => Box::new(crate::shell::OpenFiles),
            ShortcutId::CloseTab => Box::new(CloseTab),
            ShortcutId::NextTab => Box::new(NextTab),
            ShortcutId::PrevTab => Box::new(PrevTab),
            ShortcutId::FontBigger => Box::new(IncreaseFontSize),
            ShortcutId::FontSmaller => Box::new(DecreaseFontSize),
            ShortcutId::FontReset => Box::new(ResetFontSize),
            ShortcutId::FillPassword => Box::new(crate::shell::FillPassword),
            ShortcutId::ClearTerminal => Box::new(crate::shell::ClearTerminal),
        }
    }
}

/// Chords tern keeps for itself outside the keymap: the menu's (quit, hide, minimise),
/// clipboard, and tab slots.
const RESERVED_MAC: [&str; 10] = [
    "cmd-q",
    "cmd-h",
    "alt-cmd-h",
    "cmd-m",
    "cmd-c",
    "cmd-v",
    "cmd-a",
    "ctrl-cmd-f",
    "cmd-z",
    "shift-cmd-z",
];

/// Quit, and the terminal's copy / paste / find (see `tern-term`'s `view`).
const RESERVED_OTHER: [&str; 4] = [
    "ctrl-shift-q",
    "ctrl-shift-c",
    "ctrl-shift-v",
    "ctrl-shift-f",
];

const RESERVED: &[&str] = if cfg!(target_os = "macos") {
    &RESERVED_MAC
} else {
    &RESERVED_OTHER
};

/// The user's overrides; an empty string unbinds. Anything absent uses its default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keymap(pub BTreeMap<ShortcutId, String>);

impl Global for Keymap {}

impl Keymap {
    pub fn combo(&self, id: ShortcutId) -> &str {
        self.0
            .get(&id)
            .map_or_else(|| id.default_combo(), String::as_str)
    }

    pub fn is_default(&self, id: ShortcutId) -> bool {
        self.combo(id) == id.default_combo()
    }

    /// Sets `id` to `combo`, dropping the override when it equals the default.
    pub fn set(&mut self, id: ShortcutId, combo: &str) {
        if combo == id.default_combo() {
            self.0.remove(&id);
        } else {
            self.0.insert(id, combo.to_owned());
        }
    }

    /// Missing file → defaults; a broken one is logged and ignored.
    pub fn load(dir: &Path) -> Self {
        match std::fs::read_to_string(dir.join(FILE_NAME)) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "keymap_corrupt_using_defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Temp file + rename.
    ///
    /// # Errors
    /// When the directory or file cannot be written.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(self).map_err(io::Error::other)?,
        )?;
        std::fs::rename(&tmp, &path)
    }

    /// Why `combo` cannot go to `id`, or `None` when it can.
    pub fn refusal(&self, id: ShortcutId, combo: &str) -> Option<String> {
        if RESERVED.contains(&combo) {
            return Some(format!(
                "{} is kept for quitting and the clipboard",
                badge(combo)
            ));
        }
        ShortcutId::ALL
            .iter()
            .find(|other| **other != id && self.combo(**other) == combo)
            .map(|other| format!("{} is already “{}”", badge(combo), other.label()))
    }

    /// Key bindings for every bound shortcut.
    pub fn bindings(&self) -> Vec<KeyBinding> {
        ShortcutId::ALL
            .iter()
            .filter(|id| !self.combo(**id).is_empty())
            .map(|id| {
                KeyBinding::load(
                    self.combo(*id),
                    id.action(),
                    None,
                    false,
                    None,
                    &gpui::DummyKeyboardMapper,
                )
            })
            .filter_map(Result::ok)
            .collect()
    }
}

/// Re-installs every binding: the app's fixed ones, then the keymap's.
pub fn apply(cx: &mut App) {
    let keymap = cx.global::<Keymap>().clone();
    cx.clear_key_bindings();
    cx.bind_keys(crate::chrome::menu_bindings());
    cx.bind_keys(crate::tabs::bindings());
    cx.bind_keys(crate::text_input::bindings());
    cx.bind_keys(keymap.bindings());
}

/// Outcome of one keystroke while recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// Escape: keep the old shortcut.
    Cancelled,
    /// Backspace or Delete with no modifier: unbind.
    Cleared,
    /// A bare modifier, or a key that would reach the shell: keep listening.
    Ignored,
    Set(String),
}

pub fn record(key: &str, ctrl: bool, alt: bool, shift: bool, cmd: bool) -> Record {
    let key = key.trim().to_lowercase();
    let any_mod = ctrl || alt || cmd;
    match key.as_str() {
        "escape" if !any_mod && !shift => return Record::Cancelled,
        "backspace" | "delete" if !any_mod && !shift => return Record::Cleared,
        "" | "control" | "ctrl" | "alt" | "shift" | "cmd" | "platform" | "fn" => {
            return Record::Ignored;
        }
        _ => {}
    }
    let function_key = key.len() > 1 && key.starts_with('f') && key[1..].parse::<u8>().is_ok();
    if !any_mod && !function_key {
        return Record::Ignored;
    }
    let mut parts = Vec::new();
    if ctrl {
        parts.push("ctrl");
    }
    if alt {
        parts.push("alt");
    }
    if shift {
        parts.push("shift");
    }
    if cmd {
        parts.push("cmd");
    }
    parts.push(&key);
    Record::Set(parts.join("-"))
}

/// `cmd-shift-]` → `⇧⌘]` on macOS, in its menu order ⌃⌥⇧⌘; `Ctrl+Shift+]` elsewhere.
pub fn badge(combo: &str) -> String {
    badge_for(combo, cfg!(target_os = "macos"))
}

fn badge_for(combo: &str, mac: bool) -> String {
    if combo.is_empty() {
        return "—".into();
    }
    // A trailing "-" is the minus key, not a separator.
    let (mods, key) = match combo.strip_suffix("--") {
        Some(rest) => (rest, "-"),
        None => combo.rsplit_once('-').unwrap_or(("", combo)),
    };
    let parts: Vec<&str> = mods.split('-').collect();
    let mut key_label = String::new();
    let mut chars = key.chars();
    if let Some(first) = chars.next() {
        key_label.extend(first.to_uppercase());
        key_label.push_str(chars.as_str());
    }
    if mac {
        let mut out = String::new();
        for (name, glyph) in [("ctrl", '⌃'), ("alt", '⌥'), ("shift", '⇧'), ("cmd", '⌘')] {
            if parts.contains(&name) {
                out.push(glyph);
            }
        }
        out.push_str(&key_label);
        return out;
    }
    let mut out = String::new();
    for (name, word) in [
        ("ctrl", "Ctrl"),
        ("alt", "Alt"),
        ("shift", "Shift"),
        ("cmd", "Super"),
    ] {
        if parts.contains(&name) {
            out.push_str(word);
            out.push('+');
        }
    }
    out.push_str(&key_label);
    out
}

/// A fixed chord as the settings page shows it: ⌘ on macOS, Ctrl elsewhere.
pub fn primary_hint(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("⌘{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn recording_needs_a_modifier_and_orders_it() {
        assert_eq!(
            record("k", false, false, false, true),
            Record::Set("cmd-k".into())
        );
        assert_eq!(
            record("P", true, true, true, true),
            Record::Set("ctrl-alt-shift-cmd-p".into())
        );
        assert_eq!(record("k", false, false, false, false), Record::Ignored);
        assert_eq!(record("shift", false, false, true, false), Record::Ignored);
        assert_eq!(
            record("f5", false, false, false, false),
            Record::Set("f5".into())
        );
        assert_eq!(
            record("escape", false, false, false, false),
            Record::Cancelled
        );
        assert_eq!(
            record("backspace", false, false, false, false),
            Record::Cleared
        );
    }

    #[test]
    fn badges_read_like_macos_menus() {
        assert_eq!(badge_for("cmd-shift-]", true), "⇧⌘]");
        assert_eq!(badge_for("ctrl-shift-]", false), "Ctrl+Shift+]");
        assert_eq!(badge_for("cmd--", true), "⌘-");
        assert_eq!(badge_for("ctrl--", false), "Ctrl+-");
        assert_eq!(badge_for("ctrl-alt-cmd-k", true), "⌃⌥⌘K");
        assert_eq!(badge_for("ctrl-alt-left", false), "Ctrl+Alt+Left");
        assert_eq!(badge(""), "—");
    }

    #[test]
    fn overrides_fall_back_to_defaults_and_conflicts_name_the_owner() {
        let (picker, quit) = if cfg!(target_os = "macos") {
            ("cmd-t", "cmd-q")
        } else {
            ("ctrl-shift-t", "ctrl-shift-q")
        };
        let mut k = Keymap::default();
        assert_eq!(k.combo(ShortcutId::HostPicker), picker);
        k.set(ShortcutId::HostPicker, "cmd-p");
        assert_eq!(k.combo(ShortcutId::HostPicker), "cmd-p");
        assert!(!k.is_default(ShortcutId::HostPicker));
        let refusal = k.refusal(ShortcutId::Settings, "cmd-p").unwrap();
        assert!(refusal.contains("New tab (pick a host)"), "{refusal}");
        assert!(k.refusal(ShortcutId::Settings, picker).is_none());
        // Re-recording a shortcut's own chord is not a conflict with itself.
        assert!(k.refusal(ShortcutId::HostPicker, "cmd-p").is_none());
        assert!(k.refusal(ShortcutId::Settings, quit).is_some());
        k.set(ShortcutId::HostPicker, picker);
        assert!(k.0.is_empty());
    }

    #[test]
    fn no_two_defaults_share_a_chord_or_take_a_reserved_one() {
        let mut seen = std::collections::BTreeSet::new();
        for id in ShortcutId::ALL {
            assert!(seen.insert(id.default_combo()), "{id:?} repeats a chord");
            assert!(
                !RESERVED.contains(&id.default_combo()),
                "{id:?} is reserved"
            );
        }
        let (search, snippets) = if cfg!(target_os = "macos") {
            ("cmd-shift-f", "cmd-shift-s")
        } else {
            ("ctrl-alt-f", "ctrl-alt-s")
        };
        assert_eq!(Keymap::default().combo(ShortcutId::SearchHosts), search);
        assert_eq!(Keymap::default().combo(ShortcutId::Snippets), snippets);
    }

    #[test]
    fn each_platform_table_has_distinct_chords() {
        for table in [ShortcutId::mac_combo, ShortcutId::other_combo] {
            let mut seen = std::collections::BTreeSet::new();
            for id in ShortcutId::ALL {
                assert!(seen.insert(table(id)), "{} is bound twice", table(id));
            }
        }
    }

    #[test]
    fn windows_and_linux_defaults_leave_the_shells_control_keys_alone() {
        for id in ShortcutId::ALL {
            let combo = id.other_combo();
            assert!(
                !combo.contains("cmd"),
                "{combo}: Super belongs to the desktop"
            );
            let parts: Vec<&str> = combo.split('-').collect();
            let plain_ctrl_letter = parts.first() == Some(&"ctrl")
                && parts.len() == 2
                && parts[1].len() == 1
                && parts[1].as_bytes()[0].is_ascii_alphabetic();
            assert!(
                !plain_ctrl_letter,
                "{combo} would steal a control character"
            );
            assert!(!RESERVED_OTHER.contains(&combo), "{combo} is reserved");
        }
    }

    #[test]
    fn every_default_binds() {
        let k = Keymap::default();
        assert_eq!(k.bindings().len(), ShortcutId::ALL.len());
        let mut k2 = Keymap::default();
        k2.set(ShortcutId::CloseTab, "");
        assert_eq!(k2.bindings().len(), ShortcutId::ALL.len() - 1);
    }

    #[test]
    fn keymap_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut k = Keymap::default();
        k.set(ShortcutId::ToggleSidebar, "cmd-\\");
        k.save(dir.path()).unwrap();
        assert_eq!(Keymap::load(dir.path()), k);
    }
}
