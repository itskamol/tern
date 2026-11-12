// Adapted from zeron crates/ui/src/popover.rs (search_input_frame) (MIT), see sidebar.rs.
//! The sidebar's moving parts that are not drawing: the search box, the recent hosts, which
//! group folders are folded, and the order the host picker lists hosts in.
//!
//! Hosts connected to lately, newest first, in `recent.json`. It is per device (when you
//! last used a host here), so it is not synced.

use gpui::{AppContext, Context, Entity, Focusable, Subscription, Window, actions};

use super::Shell;
use crate::connections::{self, Connection};
use crate::picker;
use crate::sidebar;
use crate::text_input::TextInput;
use crate::theme::Theme;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

actions!(tern, [FocusHostSearch, UndoHosts, RedoHosts]);

pub(super) struct HostList {
    pub search: Entity<TextInput>,
    pub collapsed_groups: Vec<String>,
    pub recent: Recent,
    pub history: crate::host_history::HostHistory,
    /// The row ↑ and ↓ have reached while the search box has focus, as a position in
    /// [`visible_order`]; a new query starts again at the top.
    pub cursor: usize,
    query: String,
    _repaint: Subscription,
}

impl HostList {
    pub fn new(t: &Theme, recent: Recent, cx: &mut Context<Shell>) -> Self {
        let colors = t.input_colors();
        let search = cx.new(|cx| TextInput::new("Search hosts", false, colors, cx));
        // The list under the box re-filters on every keystroke.
        let repaint = cx.observe(&search, |shell: &mut Shell, search, cx| {
            let query = search.read(cx).text();
            if shell.hostlist.query != query {
                shell.hostlist.query = query.to_owned();
                shell.hostlist.cursor = 0;
            }
            cx.notify();
        });
        Self {
            search,
            collapsed_groups: Vec::new(),
            recent,
            history: crate::host_history::HostHistory::default(),
            cursor: 0,
            query: String::new(),
            _repaint: repaint,
        }
    }
}

impl Shell {
    pub(super) fn search_query(&self, cx: &gpui::App) -> String {
        self.hostlist.search.read(cx).text().to_owned()
    }

    /// Moves the caret into the sidebar's search box, opening the sidebar and leaving
    /// Settings first when needed.
    pub(super) fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_page = None;
        self.picker = None;
        if self.settings.sidebar_collapsed {
            self.toggle_sidebar(cx);
        }
        let focus = self.hostlist.search.focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.hostlist
            .search
            .update(cx, |input, cx| input.set_text("", cx));
    }

    /// The rows the sidebar shows, top to bottom.
    fn visible_hosts(&self, cx: &gpui::App) -> Vec<usize> {
        visible_order(
            &self.search_query(cx),
            &self.connections,
            &self.hostlist.collapsed_groups,
        )
    }

    /// The host ↑/↓ point at, drawn selected while the search box has focus.
    pub(super) fn sidebar_selection(&self, window: &Window, cx: &gpui::App) -> Option<usize> {
        let focused = self.hostlist.search.focus_handle(cx).is_focused(window);
        focused
            .then(|| self.visible_hosts(cx).get(self.hostlist.cursor).copied())
            .flatten()
    }

    /// Escape clears the query and gives the terminal its keys back; ↑ and ↓ move through the
    /// rows shown (stopping at the ends, as a macOS list does) and Enter connects to that one.
    pub(crate) fn on_search_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => {
                self.clear_search(cx);
                self.restore_focus(window, cx);
            }
            "down" | "up" => {
                let last = self.visible_hosts(cx).len().saturating_sub(1);
                let at = &mut self.hostlist.cursor;
                *at = match event.keystroke.key.as_str() {
                    "down" => (*at + 1).min(last),
                    _ => at.saturating_sub(1),
                };
                cx.notify();
            }
            "enter" => {
                let at = self.visible_hosts(cx).get(self.hostlist.cursor).copied();
                if let Some(host) = at.and_then(|ix| self.hosts.get(ix).cloned()) {
                    self.clear_search(cx);
                    self.connect_host(host, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    pub(crate) fn toggle_group(&mut self, name: &str, cx: &mut Context<Self>) {
        let folded = &mut self.hostlist.collapsed_groups;
        match folded.iter().position(|g| g == name) {
            Some(at) => {
                folded.remove(at);
            }
            None => folded.push(name.to_owned()),
        }
        cx.notify();
    }

    /// Notes the connection time and writes `recent.json`; a failed write is only logged.
    pub(super) fn record_recent(&mut self, alias: &str) {
        self.hostlist.recent.touch(alias, now());
        if let Some(dir) = crate::settings::dir()
            && let Err(e) = self.hostlist.recent.save(&dir)
        {
            tracing::warn!(error = %e, "recent_save_failed");
        }
    }

    /// Hosts the host picker lists for its current query, in order: the ranked matches, or on
    /// an empty query the recents first.
    pub fn picker_matches(&self) -> Vec<usize> {
        let labels: Vec<String> = self.connections.iter().map(sidebar::search_label).collect();
        let aliases: Vec<&str> = self.hosts.iter().map(|h| h.alias.as_str()).collect();
        let recent: Vec<&str> = self.hostlist.recent.aliases().collect();
        picker::order(&self.picker_query(), &labels, &aliases, &recent)
    }

    pub fn recent_aliases(&self) -> Vec<String> {
        self.hostlist.recent.aliases().map(str::to_owned).collect()
    }
}

impl Shell {
    /// Adds or replaces a connection and writes `hosts.json`.
    pub(super) fn store_connection(
        &mut self,
        editing: Option<usize>,
        connection: Connection,
    ) -> Result<(), String> {
        // A command typed in this form is approved by saving it.
        crate::password_command::approve_saved(&connection);
        let mut next = self.connections.clone();
        let what = match editing.and_then(|ix| next.get_mut(ix)) {
            Some(slot) => {
                *slot = connection;
                format!("Edit {}", slot.name)
            }
            None => {
                let what = format!("Add {}", connection.name);
                next.push(connection);
                what
            }
        };
        if let Some(dir) = crate::settings::dir() {
            connections::save(&dir, &next).map_err(|e| e.to_string())?;
        }
        self.commit_hosts(next, what);
        Ok(())
    }

    /// The list after an edit made here, already written: shown, and kept for ⌘Z.
    pub(super) fn commit_hosts(&mut self, next: Vec<Connection>, what: String) {
        let before = std::mem::replace(&mut self.connections, next);
        self.hostlist
            .history
            .record(before, self.connections.clone(), what);
        self.refresh_hosts();
    }

    pub(super) fn undo_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let step = self.hostlist.history.undo(&self.connections);
        self.restore_hosts(step, "Undo", window, cx);
    }

    pub(super) fn redo_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let step = self.hostlist.history.redo(&self.connections);
        self.restore_hosts(step, "Redo", window, cx);
    }

    /// Writes back a list from the history and names what changed ("Undo Remove web-01").
    /// Nothing to undo, or a list changed by sync since, only beeps.
    fn restore_hosts(
        &mut self,
        step: Option<(Vec<Connection>, String)>,
        verb: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((list, what)) = step else {
            return window.play_system_bell();
        };
        if let Some(dir) = crate::settings::dir()
            && let Err(e) = connections::save(&dir, &list)
        {
            self.hostlist.history.clear();
            return self.notify_toast(
                super::ToastKind::Critical,
                format!("Could not {}: {e}", verb.to_lowercase()),
                cx,
            );
        }
        self.connections = list;
        self.refresh_hosts();
        self.notify_toast(super::ToastKind::Default, format!("{verb} {what}"), cx);
        cx.notify();
    }

    /// A host dragged in the sidebar: moved (into the group it landed in) and written to
    /// `hosts.json`. A failed write leaves the list as it was, and says so.
    pub(crate) fn move_host(
        &mut self,
        from: usize,
        to: &connections::Spot,
        cx: &mut Context<Self>,
    ) {
        let mut next = self.connections.clone();
        if !connections::move_to(&mut next, from, to) {
            return;
        }
        if let Some(dir) = crate::settings::dir()
            && let Err(e) = connections::save(&dir, &next)
        {
            tracing::warn!(error = %e, "hosts_move_save_failed");
            return self.notify_toast(
                super::ToastKind::Critical,
                format!("Could not move the host: {e}"),
                cx,
            );
        }
        let moved = self
            .connections
            .get(from)
            .map_or("host", |c| c.name.as_str());
        let what = format!("Move {moved}");
        self.commit_hosts(next, what);
        cx.notify();
    }
}

/// The connections the sidebar lists, in screen order: the ranked matches for a query, else
/// the ungrouped hosts then each group by name, leaving out the folded ones.
fn visible_order(query: &str, connections: &[Connection], folded: &[String]) -> Vec<usize> {
    if !query.trim().is_empty() {
        return sidebar::search(query, connections);
    }
    let layout = sidebar::layout(connections);
    let open = layout
        .groups
        .into_iter()
        .filter(|(name, _)| !folded.contains(name))
        .flat_map(|(_, rows)| rows);
    layout.ungrouped.into_iter().chain(open).collect()
}

const FILE_NAME: &str = "recent.json";
/// What is kept on disk.
pub const CAP: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub alias: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    entries: Vec<Entry>,
}

impl Recent {
    /// Missing or unreadable file is an empty history; losing it costs nothing.
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(FILE_NAME))
            .ok()
            .and_then(|text| serde_json::from_str::<Self>(&text).ok())
            .map(|mut r| {
                r.entries.sort_by_key(|e| std::cmp::Reverse(e.at));
                r.entries.truncate(CAP);
                r
            })
            .unwrap_or_default()
    }

    /// Temp file + rename.
    ///
    /// # Errors
    /// When the directory or file cannot be written.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    /// Connecting again moves the alias to the front instead of listing it twice.
    pub fn touch(&mut self, alias: &str, at: u64) {
        self.entries.retain(|e| e.alias != alias);
        self.entries.insert(
            0,
            Entry {
                alias: alias.to_owned(),
                at,
            },
        );
        self.entries.truncate(CAP);
    }

    /// Aliases, newest first.
    pub fn aliases(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.alias.as_str())
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn conn(name: &str, group: Option<&str>) -> Connection {
        Connection {
            name: name.into(),
            host: format!("{name}.example.com"),
            group: group.map(Into::into),
            ..Connection::default()
        }
    }

    #[test]
    fn arrows_walk_the_rows_in_screen_order_skipping_folded_groups() {
        let all = [
            conn("web", Some("prod")),
            conn("laptop", None),
            conn("db", Some("prod")),
            conn("ci", Some("build")),
        ];
        assert_eq!(visible_order("", &all, &[]), [1, 3, 0, 2]);
        assert_eq!(visible_order("", &all, &["prod".into()]), [1, 3]);
        assert_eq!(visible_order("  ", &all, &["build".into()]), [1, 0, 2]);
    }

    #[test]
    fn with_a_query_the_arrows_walk_the_matches_even_in_folded_groups() {
        let all = [conn("web", Some("prod")), conn("laptop", None)];
        assert_eq!(visible_order("web", &all, &["prod".into()]), [0]);
    }

    fn aliases(r: &Recent) -> Vec<&str> {
        r.aliases().collect()
    }

    #[test]
    fn newest_first_and_a_repeat_moves_to_the_front() {
        let mut r = Recent::default();
        r.touch("web", 1);
        r.touch("db", 2);
        r.touch("cache", 3);
        assert_eq!(aliases(&r), ["cache", "db", "web"]);
        r.touch("web", 4);
        assert_eq!(aliases(&r), ["web", "cache", "db"]);
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn the_list_is_capped_and_drops_the_oldest() {
        let mut r = Recent::default();
        for i in 0..(CAP + 5) {
            r.touch(&format!("h{i}"), i as u64);
        }
        assert_eq!(r.len(), CAP);
        assert_eq!(r.aliases().next(), Some("h24"));
        assert_eq!(r.aliases().last(), Some("h5"));
    }

    #[test]
    fn it_round_trips_and_a_bad_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Recent::load(dir.path()).len(), 0);
        let mut r = Recent::default();
        r.touch("a", 10);
        r.touch("b", 20);
        r.save(dir.path()).unwrap();
        assert_eq!(Recent::load(dir.path()), r);
        std::fs::write(dir.path().join(FILE_NAME), "{ nope").unwrap();
        assert_eq!(Recent::load(dir.path()).len(), 0);
    }

    #[test]
    fn a_hand_edited_file_is_sorted_and_capped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let entries: Vec<String> = (0..30)
            .map(|i| format!(r#"{{"alias":"h{i}","at":{i}}}"#))
            .collect();
        let json = format!(r#"{{"entries":[{}]}}"#, entries.join(","));
        std::fs::write(dir.path().join(FILE_NAME), json).unwrap();
        let r = Recent::load(dir.path());
        assert_eq!(r.len(), CAP);
        assert_eq!(r.aliases().next(), Some("h29"));
    }
}
