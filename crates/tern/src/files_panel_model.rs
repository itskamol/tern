//! What the Files panel shows for each pane, apart from the window: which folder, whether it
//! follows the shell or the person is browsing, what is picked, and whether the pane can be
//! browsed at all. The panel shows one pane at a time and keeps every pane's place, so moving
//! focus back and forth loses nothing.
//!
//! Listings are not kept here: they are slow to get and large, so the window holds the rows of
//! the pane in view only. The model answers "what should be listed" ([`Shown`]); the window
//! lists it.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use tern_term::ShellFolder;

use crate::lifecycle::Status;
use crate::split::PaneId;

/// One line for the remote's `~/.bashrc` that makes the shell report its folder (OSC 7) at every
/// prompt, keeping any `PROMPT_COMMAND` already there.
pub const SNIPPET: &str = r#"PROMPT_COMMAND='printf "\e]7;file://%s%s\a" "$HOSTNAME" "$PWD"'"${PROMPT_COMMAND:+;$PROMPT_COMMAND}""#;

/// Where a pane's connection stands, as the panel sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// Dialling or logging in.
    Waiting,
    Connected,
    /// Was connected, or failed to be; Reconnect dials again.
    Dropped,
    /// A restored tab that waits for Enter.
    Idle,
}

impl Link {
    pub fn of(status: &Status) -> Self {
        match status {
            Status::Connecting => Self::Waiting,
            Status::Connected => Self::Connected,
            Status::Closed => Self::Dropped,
            Status::Idle => Self::Idle,
        }
    }
}

/// A folder to list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Path(String),
    /// Wherever the host says home is; only the connection knows.
    Home,
}

/// How the shown folder was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The shell's folder, and it moves when the shell does.
    Following,
    /// A folder the person opened; the shell's moves are not followed.
    Browsing,
    /// The shell reports no folder, so home is all there is to follow.
    AtHome,
}

/// What the panel shows for one pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shown {
    /// The pane is still logging in.
    Waiting,
    /// Nothing to browse until the pane connects; `idle` is a restored tab never dialled.
    Offline { idle: bool },
    Folder {
        place: Place,
        mode: Mode,
        /// The shell has told tern a folder on this connection. When not, the panel says so
        /// and offers [`SNIPPET`].
        shell_reports: bool,
        hidden: bool,
    },
}

impl Shown {
    /// Browsing, with a Shell folder to go back to.
    pub fn can_return(&self) -> bool {
        matches!(
            self,
            Self::Folder {
                mode: Mode::Browsing,
                shell_reports: true,
                ..
            }
        )
    }
}

/// A change to the pane's rows waiting for the person: the row whose name is being edited, or
/// the rows a delete would remove, asked in the rows themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    Rename(String),
    Delete(Vec<String>),
}

/// The rows picked, by path: a path stays picked through a new listing and a trip to another
/// pane, where a row number would point at something else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Picked {
    paths: BTreeSet<String>,
    anchor: Option<String>,
}

impl Picked {
    fn click(&mut self, rows: &[String], ix: usize, command: bool, shift: bool) {
        let Some(row) = rows.get(ix) else { return };
        let anchor = self
            .anchor
            .as_ref()
            .and_then(|a| rows.iter().position(|r| r == a));
        match (command, shift, anchor) {
            (_, true, Some(from)) => {
                if !command {
                    self.paths.clear();
                }
                let (lo, hi) = (from.min(ix), from.max(ix));
                self.paths.extend(rows[lo..=hi].iter().cloned());
            }
            (true, _, _) => {
                if !self.paths.remove(row) {
                    self.paths.insert(row.clone());
                }
                self.anchor = Some(row.clone());
            }
            _ => {
                self.paths.clear();
                self.paths.insert(row.clone());
                self.anchor = Some(row.clone());
            }
        }
    }

    fn clear(&mut self) {
        self.paths.clear();
        self.anchor = None;
    }
}

#[derive(Debug)]
struct PaneState {
    /// The address the pane dials, to tell its own shell's folders from a nested `ssh`'s.
    dialled: String,
    /// The host name the shell reports for itself; known from its first folder.
    own_host: Option<String>,
    link: Link,
    /// Counts connections: a new one means the listing's source is gone.
    epoch: u64,
    shell: Option<String>,
    browsing: Option<String>,
    hidden: bool,
    picked: Picked,
    /// Ends with what was picked: it is about those rows.
    edit: Option<Edit>,
}

impl PaneState {
    fn new(dialled: &str, link: Link) -> Self {
        Self {
            dialled: dialled.to_owned(),
            own_host: None,
            link,
            epoch: u64::from(link == Link::Connected),
            shell: None,
            browsing: None,
            hidden: false,
            picked: Picked::default(),
            edit: None,
        }
    }

    /// A new connection is a new shell: what the last one said is not this one's.
    fn connected(&mut self) {
        self.epoch += 1;
        self.own_host = None;
        self.shell = None;
        self.browsing = None;
        self.unpick();
    }

    /// Nothing is picked any more, so no edit of those rows waits either.
    fn unpick(&mut self) {
        self.picked.clear();
        self.edit = None;
    }

    fn set_link(&mut self, link: Link) {
        if link == Link::Connected && self.link != Link::Connected {
            self.connected();
        }
        self.link = link;
    }

    fn arrive(&mut self, folder: &ShellFolder) {
        if !self.is_ours(&folder.host) {
            return;
        }
        if !folder.host.is_empty() && self.own_host.is_none() {
            self.own_host = Some(folder.host.clone());
        }
        if self.shell.as_deref() != Some(&folder.path) {
            if self.browsing.is_none() {
                self.unpick();
            }
            self.shell = Some(folder.path.clone());
        }
    }

    /// The pane's own shell reports itself by its host name, which is not always the address
    /// tern dials (an IP, an alias). So a folder is ours when it names the dialled host, or the
    /// name this connection's shell used first; a nested `ssh` names another host.
    fn is_ours(&self, reported: &str) -> bool {
        reported.is_empty()
            || names_match(reported, &self.dialled)
            || self
                .own_host
                .as_deref()
                .is_none_or(|own| own.eq_ignore_ascii_case(reported))
    }

    fn forget_shell(&mut self) {
        self.shell = None;
        self.own_host = None;
    }

    fn shown(&self) -> Shown {
        match self.link {
            Link::Waiting => Shown::Waiting,
            Link::Dropped => Shown::Offline { idle: false },
            Link::Idle => Shown::Offline { idle: true },
            Link::Connected => {
                let (place, mode) = match (&self.browsing, &self.shell) {
                    (Some(path), _) => (Place::Path(path.clone()), Mode::Browsing),
                    (None, Some(path)) => (Place::Path(path.clone()), Mode::Following),
                    (None, None) => (Place::Home, Mode::AtHome),
                };
                Shown::Folder {
                    place,
                    mode,
                    shell_reports: self.shell.is_some(),
                    hidden: self.hidden,
                }
            }
        }
    }
}

/// `web` and `web.example.com` are one host; two IP addresses are the same only when equal.
fn names_match(a: &str, b: &str) -> bool {
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    if a.parse::<IpAddr>().is_ok() || b.parse::<IpAddr>().is_ok() {
        return false;
    }
    let short = |s: &str| s.split('.').next().unwrap_or(s).to_ascii_lowercase();
    short(a) == short(b)
}

/// One part of a path in the breadcrumb, with the folder a click on it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crumb {
    pub label: String,
    pub path: String,
}

/// `/home/me/logs` as `/`, `home`, `me`, `logs`.
pub fn crumbs(path: &str) -> Vec<Crumb> {
    let mut out = vec![Crumb {
        label: "/".into(),
        path: "/".into(),
    }];
    let mut at = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        at.push('/');
        at.push_str(part);
        out.push(Crumb {
            label: part.into(),
            path: at.clone(),
        });
    }
    out
}

/// The folder a download lands in: `Downloads` under `home`.
pub fn downloads_in(home: &std::path::Path) -> std::path::PathBuf {
    home.join("Downloads")
}

/// Every pane's place in the panel, and which pane is in view.
#[derive(Debug, Default)]
pub struct FilesPanelModel {
    panes: BTreeMap<PaneId, PaneState>,
    focused: Option<PaneId>,
}

impl FilesPanelModel {
    /// Tells the model what pane `id` is now: the address it dials, its connection and the last
    /// folder its shell reported (`None` when it reports none, or forgot it on reconnect).
    pub fn pane(&mut self, id: PaneId, dialled: &str, link: Link, folder: Option<&ShellFolder>) {
        let state = self
            .panes
            .entry(id)
            .or_insert_with(|| PaneState::new(dialled, link));
        state.dialled = dialled.to_owned();
        state.set_link(link);
        match folder {
            Some(folder) => state.arrive(folder),
            None => state.forget_shell(),
        }
    }

    /// Forgets panes that are not in `live`: a closed pane's place is not kept.
    pub fn keep_only(&mut self, live: &[PaneId]) {
        self.panes.retain(|id, _| live.contains(id));
    }

    /// The pane the panel shows; `None` when no pane is in front.
    pub fn focus(&mut self, id: Option<PaneId>) {
        self.focused = id.filter(|id| self.panes.contains_key(id));
    }

    /// Where Send to… can put files: each connected pane at its Shell folder, else home. A host
    /// and folder is listed once, and the folder in view (`here`) not at all.
    pub fn send_targets(&self, here: Option<&str>) -> Vec<(PaneId, Place)> {
        let in_view = self.focused.and_then(|id| self.panes.get(&id));
        let mut out: Vec<(PaneId, Place)> = Vec::new();
        let mut seen: Vec<(&str, Place)> = Vec::new();
        for (id, p) in &self.panes {
            if p.link != Link::Connected {
                continue;
            }
            let place = p.shell.clone().map_or(Place::Home, Place::Path);
            let same_host = |a: &str| a.eq_ignore_ascii_case(&p.dialled);
            // The pane in view shows its own target unless it is browsing elsewhere.
            let is_here = (Some(*id) == self.focused && p.browsing.is_none())
                || (in_view.is_some_and(|v| same_host(&v.dialled))
                    && matches!((&place, here), (Place::Path(a), Some(h)) if a == h));
            if is_here || seen.iter().any(|(h, at)| same_host(h) && *at == place) {
                continue;
            }
            seen.push((&p.dialled, place.clone()));
            out.push((*id, place));
        }
        out
    }

    pub fn focused(&self) -> Option<PaneId> {
        self.focused
    }

    /// What the pane in view shows.
    pub fn shown(&self) -> Option<Shown> {
        self.shown_for(self.focused?)
    }

    pub fn shown_for(&self, id: PaneId) -> Option<Shown> {
        self.panes.get(&id).map(PaneState::shown)
    }

    /// How many connections the pane has had; a change means a new source to list through.
    pub fn epoch(&self, id: PaneId) -> u64 {
        self.panes.get(&id).map_or(0, |p| p.epoch)
    }

    fn view(&mut self) -> Option<&mut PaneState> {
        let id = self.focused?;
        self.panes.get_mut(&id)
    }

    /// The person opens `path` in the pane in view. Opening the Shell folder itself is
    /// following it again.
    pub fn open_folder(&mut self, path: &str) {
        let Some(p) = self.view() else { return };
        let next = (p.shell.as_deref() != Some(path)).then(|| path.to_owned());
        if p.browsing != next {
            p.unpick();
        }
        p.browsing = next;
    }

    /// The way back from browsing: the Shell folder again, followed from now on.
    pub fn back_to_shell(&mut self) {
        if let Some(p) = self.view()
            && p.browsing.take().is_some()
        {
            p.unpick();
        }
    }

    pub fn toggle_hidden(&mut self) {
        if let Some(p) = self.view() {
            p.hidden = !p.hidden;
            p.unpick();
        }
    }

    /// A click on row `ix` of `rows` (the paths shown, top to bottom) in the pane in view.
    pub fn click_row(&mut self, rows: &[String], ix: usize, command: bool, shift: bool) {
        if let Some(p) = self.view() {
            p.picked.click(rows, ix, command, shift);
            p.edit = None;
        }
    }

    /// The listing changed under the pane in view: what was picked may be gone.
    pub fn clear_picked(&mut self) {
        if let Some(p) = self.view() {
            p.unpick();
        }
    }

    pub fn is_picked(&self, path: &str) -> bool {
        self.focused
            .and_then(|id| self.panes.get(&id))
            .is_some_and(|p| p.picked.paths.contains(path))
    }

    /// F2 or the row's Rename: the one picked row's name becomes editable. `None` unless exactly
    /// one row is picked.
    pub fn start_rename(&mut self) -> Option<String> {
        let p = self.view()?;
        let mut picked = p.picked.paths.iter();
        let (Some(one), None) = (picked.next(), picked.next()) else {
            return None;
        };
        let one = one.clone();
        p.edit = Some(Edit::Rename(one.clone()));
        Some(one)
    }

    /// ⌘⌫ or the row's Delete: the picked rows ask before anything goes. False when none is.
    pub fn start_delete(&mut self) -> bool {
        let Some(p) = self.view().filter(|p| !p.picked.paths.is_empty()) else {
            return false;
        };
        p.edit = Some(Edit::Delete(p.picked.paths.iter().cloned().collect()));
        true
    }

    /// The edit waiting in the pane in view.
    pub fn edit(&self) -> Option<&Edit> {
        self.focused
            .and_then(|id| self.panes.get(&id))
            .and_then(|p| p.edit.as_ref())
    }

    pub fn cancel_edit(&mut self) {
        if let Some(p) = self.view() {
            p.edit = None;
        }
    }

    /// Enter or the row's confirm: the edit, to carry out, and ended.
    pub fn take_edit(&mut self) -> Option<Edit> {
        self.view()?.edit.take()
    }

    /// The paths picked in the pane in view.
    pub fn picked(&self) -> Vec<String> {
        self.focused
            .and_then(|id| self.panes.get(&id))
            .map(|p| p.picked.paths.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[path = "files_panel_model_tests.rs"]
mod tests;
