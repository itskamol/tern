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
        }
    }

    /// A new connection is a new shell: what the last one said is not this one's.
    fn connected(&mut self) {
        self.epoch += 1;
        self.own_host = None;
        self.shell = None;
        self.browsing = None;
        self.picked.clear();
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
                self.picked.clear();
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
            p.picked.clear();
        }
        p.browsing = next;
    }

    /// The way back from browsing: the Shell folder again, followed from now on.
    pub fn back_to_shell(&mut self) {
        if let Some(p) = self.view()
            && p.browsing.take().is_some()
        {
            p.picked.clear();
        }
    }

    pub fn toggle_hidden(&mut self) {
        if let Some(p) = self.view() {
            p.hidden = !p.hidden;
            p.picked.clear();
        }
    }

    /// A click on row `ix` of `rows` (the paths shown, top to bottom) in the pane in view.
    pub fn click_row(&mut self, rows: &[String], ix: usize, command: bool, shift: bool) {
        if let Some(p) = self.view() {
            p.picked.click(rows, ix, command, shift);
        }
    }

    /// The listing changed under the pane in view: what was picked may be gone.
    pub fn clear_picked(&mut self) {
        if let Some(p) = self.view() {
            p.picked.clear();
        }
    }

    pub fn is_picked(&self, path: &str) -> bool {
        self.focused
            .and_then(|id| self.panes.get(&id))
            .is_some_and(|p| p.picked.paths.contains(path))
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
mod tests {
    use super::*;

    fn folder(host: &str, path: &str) -> ShellFolder {
        ShellFolder {
            host: host.into(),
            path: path.into(),
        }
    }

    /// A panel with pane 1 on `web`, connected, in view.
    fn one_pane() -> FilesPanelModel {
        let mut m = FilesPanelModel::default();
        m.pane(1, "web", Link::Connected, None);
        m.focus(Some(1));
        m
    }

    fn following(path: &str) -> Shown {
        Shown::Folder {
            place: Place::Path(path.into()),
            mode: Mode::Following,
            shell_reports: true,
            hidden: false,
        }
    }

    fn browsing(path: &str) -> Shown {
        Shown::Folder {
            place: Place::Path(path.into()),
            mode: Mode::Browsing,
            shell_reports: true,
            hidden: false,
        }
    }

    #[test]
    fn a_shell_that_reports_no_folder_opens_at_home_and_says_so() {
        let m = one_pane();
        assert_eq!(
            m.shown(),
            Some(Shown::Folder {
                place: Place::Home,
                mode: Mode::AtHome,
                shell_reports: false,
                hidden: false,
            })
        );
        assert!(!m.shown().is_some_and(|s| s.can_return()));
        assert!(SNIPPET.contains(r"\e]7;file://") && !SNIPPET.contains('\n'));
    }

    #[test]
    fn the_panel_starts_at_the_shell_folder_and_follows_it() {
        let mut m = one_pane();
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv/app")));
        assert_eq!(m.shown(), Some(following("/srv/app")));
        m.pane(
            1,
            "web",
            Link::Connected,
            Some(&folder("web", "/srv/app/logs")),
        );
        assert_eq!(m.shown(), Some(following("/srv/app/logs")));
    }

    #[test]
    fn opening_a_folder_by_hand_stops_following_until_the_way_back_is_taken() {
        let mut m = one_pane();
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv/app")));
        m.open_folder("/etc");
        assert_eq!(m.shown(), Some(browsing("/etc")));
        assert!(m.shown().is_some_and(|s| s.can_return()));
        // The shell moves on; the panel stays where the person put it.
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/tmp")));
        assert_eq!(m.shown(), Some(browsing("/etc")));
        m.back_to_shell();
        assert_eq!(m.shown(), Some(following("/tmp")));
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/var")));
        assert_eq!(m.shown(), Some(following("/var")));
    }

    #[test]
    fn opening_the_shell_folder_by_hand_is_following_it_again() {
        let mut m = one_pane();
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
        m.open_folder("/etc");
        m.open_folder("/srv");
        assert_eq!(m.shown(), Some(following("/srv")));
    }

    #[test]
    fn browsing_without_a_reporting_shell_has_no_way_back_but_still_says_so() {
        let mut m = one_pane();
        m.open_folder("/etc");
        let shown = m.shown();
        assert_eq!(
            shown,
            Some(Shown::Folder {
                place: Place::Path("/etc".into()),
                mode: Mode::Browsing,
                shell_reports: false,
                hidden: false,
            })
        );
        assert!(!shown.is_some_and(|s| s.can_return()));
        m.back_to_shell();
        assert!(matches!(
            m.shown(),
            Some(Shown::Folder {
                place: Place::Home,
                mode: Mode::AtHome,
                ..
            })
        ));
    }

    #[test]
    fn each_pane_keeps_its_own_folder_selection_and_browsing_when_focus_moves() {
        let mut m = FilesPanelModel::default();
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
        m.pane(2, "db", Link::Connected, Some(&folder("db", "/var/lib")));
        m.focus(Some(1));
        m.open_folder("/etc");
        let rows = vec!["/etc/a".to_owned(), "/etc/b".to_owned()];
        m.click_row(&rows, 1, false, false);
        m.focus(Some(2));
        assert_eq!(m.shown(), Some(following("/var/lib")));
        assert!(m.picked().is_empty());
        m.focus(Some(1));
        assert_eq!(m.shown(), Some(browsing("/etc")));
        assert_eq!(m.picked(), ["/etc/b"]);
    }

    #[test]
    fn a_pane_out_of_view_still_follows_its_shell() {
        let mut m = FilesPanelModel::default();
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
        m.pane(2, "db", Link::Connected, Some(&folder("db", "/var")));
        m.focus(Some(1));
        m.pane(2, "db", Link::Connected, Some(&folder("db", "/var/log")));
        m.focus(Some(2));
        assert_eq!(m.shown(), Some(following("/var/log")));
    }

    #[test]
    fn a_pane_still_logging_in_is_waiting_and_a_dropped_one_offers_reconnect() {
        let mut m = FilesPanelModel::default();
        m.pane(1, "web", Link::Waiting, None);
        m.focus(Some(1));
        assert_eq!(m.shown(), Some(Shown::Waiting));
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
        m.pane(1, "web", Link::Dropped, Some(&folder("web", "/srv")));
        assert_eq!(m.shown(), Some(Shown::Offline { idle: false }));
        m.pane(1, "web", Link::Idle, None);
        assert_eq!(m.shown(), Some(Shown::Offline { idle: true }));
    }

    #[test]
    fn a_reconnect_starts_over_with_nothing_of_the_old_shell() {
        let mut m = one_pane();
        let before = m.epoch(1);
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
        m.open_folder("/etc");
        let rows = vec!["/etc/a".to_owned()];
        m.click_row(&rows, 0, false, false);
        m.pane(1, "web", Link::Dropped, Some(&folder("web", "/srv")));
        m.pane(1, "web", Link::Waiting, None);
        assert_eq!(m.shown(), Some(Shown::Waiting));
        assert_eq!(m.epoch(1), before, "only a new connection is a new epoch");
        m.pane(1, "web", Link::Connected, None);
        assert_eq!(m.epoch(1), before + 1);
        assert_eq!(
            m.shown(),
            Some(Shown::Folder {
                place: Place::Home,
                mode: Mode::AtHome,
                shell_reports: false,
                hidden: false,
            })
        );
        assert!(m.picked().is_empty());
        assert_ne!(
            m.epoch(1),
            before,
            "the listing must reopen on the new connection"
        );
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/home/me")));
        assert_eq!(m.shown(), Some(following("/home/me")));
    }

    #[test]
    fn a_folder_reported_by_a_nested_ssh_is_ignored() {
        let mut m = one_pane();
        // The shell names itself by its host name, not the address tern dialled.
        m.pane(
            1,
            "10.0.0.5",
            Link::Connected,
            Some(&folder("web01", "/srv")),
        );
        assert_eq!(m.shown(), Some(following("/srv")));
        m.pane(
            1,
            "10.0.0.5",
            Link::Connected,
            Some(&folder("db01", "/var/lib")),
        );
        assert_eq!(m.shown(), Some(following("/srv")));
        // Back in the outer shell.
        m.pane(
            1,
            "10.0.0.5",
            Link::Connected,
            Some(&folder("WEB01", "/home")),
        );
        assert_eq!(m.shown(), Some(following("/home")));
        // A folder with no host does not make the next nested one ours.
        m.pane(1, "10.0.0.5", Link::Connected, Some(&folder("", "/opt")));
        m.pane(1, "10.0.0.5", Link::Connected, Some(&folder("db01", "/x")));
        assert_eq!(m.shown(), Some(following("/opt")));
    }

    #[test]
    fn a_folder_with_no_host_or_the_dialled_host_is_the_panes_own() {
        let mut m = one_pane();
        m.pane(
            1,
            "web.example.com",
            Link::Connected,
            Some(&folder("", "/a")),
        );
        assert_eq!(m.shown(), Some(following("/a")));
        m.pane(
            1,
            "web.example.com",
            Link::Connected,
            Some(&folder("web", "/b")),
        );
        assert_eq!(m.shown(), Some(following("/b")));
        m.pane(
            1,
            "web.example.com",
            Link::Connected,
            Some(&folder("db", "/c")),
        );
        assert_eq!(m.shown(), Some(following("/b")));
    }

    #[test]
    fn the_dialled_name_stays_ours_even_when_the_shell_calls_itself_something_else() {
        let mut m = one_pane();
        m.pane(1, "web", Link::Connected, Some(&folder("web01", "/a")));
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/b")));
        assert_eq!(m.shown(), Some(following("/b")));
    }

    #[test]
    fn a_shell_that_stops_reporting_goes_back_to_home() {
        let mut m = one_pane();
        m.pane(1, "web", Link::Connected, Some(&folder("web", "/srv")));
        m.pane(1, "web", Link::Connected, None);
        assert!(matches!(
            m.shown(),
            Some(Shown::Folder {
                mode: Mode::AtHome,
                shell_reports: false,
                ..
            })
        ));
        m.pane(1, "web", Link::Connected, Some(&folder("other", "/z")));
        assert_eq!(m.shown(), Some(following("/z")));
    }

    #[test]
    fn a_folder_without_a_host_does_not_use_up_the_shells_first_name() {
        let mut m = one_pane();
        m.pane(1, "10.0.0.5", Link::Connected, Some(&folder("", "/a")));
        m.pane(1, "10.0.0.5", Link::Connected, Some(&folder("web01", "/b")));
        assert_eq!(m.shown(), Some(following("/b")));
    }

    #[test]
    fn two_addresses_are_the_same_host_only_when_equal() {
        assert!(!names_match("10.0.0.5", "10"));
        assert!(names_match("web", "WEB.example.com"));
        assert!(names_match("10.0.0.5", "10.0.0.5"));
        assert!(!names_match("10.0.0.5", "10.9.9.9"));
        assert!(!names_match("web", "db"));
    }

    #[test]
    fn a_closed_pane_is_forgotten_and_cannot_be_in_view() {
        let mut m = one_pane();
        assert_eq!(m.focused(), Some(1));
        m.keep_only(&[]);
        m.focus(Some(1));
        assert_eq!(m.shown(), None);
        assert_eq!(m.focused(), None);
    }

    #[test]
    fn picking_follows_the_click_rules_and_goes_with_the_folder() {
        let mut m = one_pane();
        let rows: Vec<String> = ["a", "b", "c", "d"].map(|n| format!("/x/{n}")).to_vec();
        m.click_row(&rows, 1, false, false);
        m.click_row(&rows, 3, false, true);
        assert_eq!(m.picked(), ["/x/b", "/x/c", "/x/d"]);
        m.click_row(&rows, 2, true, false);
        assert!(!m.is_picked("/x/c") && m.is_picked("/x/b"));
        // ⌘⇧ extends from the anchor and keeps what was picked.
        m.click_row(&rows, 0, true, true);
        assert_eq!(m.picked(), ["/x/a", "/x/b", "/x/c", "/x/d"]);
        m.clear_picked();
        assert!(m.picked().is_empty(), "a new listing drops the picks");
        m.click_row(&rows, 2, false, false);
        m.click_row(&rows, 1, true, false);
        m.click_row(&rows, 0, false, false);
        assert_eq!(m.picked(), ["/x/a"]);
        m.open_folder("/y");
        assert!(m.picked().is_empty());
        m.click_row(&rows, 9, false, false);
        assert!(m.picked().is_empty(), "a row past the end picks nothing");
    }

    #[test]
    fn showing_dotfiles_is_kept_per_pane_and_drops_what_was_picked() {
        let mut m = FilesPanelModel::default();
        m.pane(1, "web", Link::Connected, None);
        m.pane(2, "db", Link::Connected, None);
        m.focus(Some(1));
        let rows = vec!["/a".to_owned()];
        m.click_row(&rows, 0, false, false);
        m.toggle_hidden();
        assert!(m.picked().is_empty());
        assert!(matches!(
            m.shown(),
            Some(Shown::Folder { hidden: true, .. })
        ));
        m.focus(Some(2));
        assert!(matches!(
            m.shown(),
            Some(Shown::Folder { hidden: false, .. })
        ));
    }

    #[test]
    fn the_breadcrumb_names_every_folder_up_to_the_root() {
        let labels = |p: &str| {
            crumbs(p)
                .into_iter()
                .map(|c| (c.label, c.path))
                .collect::<Vec<_>>()
        };
        assert_eq!(labels("/"), [("/".into(), "/".into())]);
        assert_eq!(
            labels("/home/me//logs/"),
            [
                ("/".into(), "/".into()),
                ("home".into(), "/home".into()),
                ("me".into(), "/home/me".into()),
                ("logs".into(), "/home/me/logs".into()),
            ]
        );
    }

    #[test]
    fn downloads_go_to_the_downloads_folder_of_the_mac_home() {
        let home = std::path::Path::new("/Users/me");
        assert_eq!(
            downloads_in(home),
            std::path::Path::new("/Users/me/Downloads")
        );
    }
}
