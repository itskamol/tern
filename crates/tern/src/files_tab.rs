// Adapted from zeron crates/ui/src/files/tree.rs (render_tree_row: row height, hover and
// selection washes, folder/file glyphs) (MIT).
//! The Files tab: two panels side by side, each the Mac's own files or one host's, for copying
//! between them. This file holds the state and what the panels do; `files_tab_view.rs` draws it.
//!
//! Every panel talks to a [`FileSource`], so a panel neither knows nor cares whether it shows the
//! Mac or a host. The calls run on the SSH runtime (the Mac's adapter needs a tokio reactor), and
//! the panel is told when they are done. Copies go to the shell's transfer queue, which outlives
//! this tab.

use std::sync::Arc;

use gpui::{AppContext, Context, Entity, FocusHandle, Focusable, Subscription, WeakEntity};
use tern_ssh::{
    ConnectSpec, FileEntry, FileError, FileSource, HostEntry, LocalFs, SessionHandle, SftpFiles,
};

use crate::files_model::{Sort, SortKey, order};
use crate::files_pick::Picked;
use crate::panel_login::{LoginEvent, PanelLogin, Step};
use crate::password_command::Auth;
use crate::shell::Shell;
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::transfers::{Finished, Transfers};

/// What a panel shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Mac,
    /// A host by its name in tern (or the `user@host` it was opened by).
    Host(String),
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Self::Mac => "This Mac".to_owned(),
            Self::Host(alias) => alias.clone(),
        }
    }
}

/// Why a panel could not show a host's files, worded in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The host stopped answering while the panel was open.
    Gone,
    /// The panel was asked for a name tern has no host for.
    Unknown(String),
    /// The host went away after the login; what it said, if anything.
    Lost(String),
    /// A host's or the login's own message, as it came.
    Said(String),
}

impl Reason {
    pub fn text(&self) -> String {
        match self {
            Self::Gone => "The host stopped answering.".to_owned(),
            Self::Unknown(alias) => format!("{alias}: not a host tern knows"),
            Self::Lost(why) if why.is_empty() => "The host went away.".to_owned(),
            Self::Lost(why) => format!("The host went away: {why}"),
            Self::Said(text) => text.clone(),
        }
    }
}

impl From<&FileError> for Reason {
    fn from(e: &FileError) -> Self {
        match e {
            FileError::Closed => Self::Gone,
            other => Self::Said(other.to_string()),
        }
    }
}

/// Where a panel stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Opening the connection, or answering its login questions.
    Connecting,
    Loading,
    Ready,
    /// Could not connect or list; Retry tries again.
    Failed(Reason),
    /// The host was there and went away; Reconnect starts over.
    Dropped(Reason),
}

pub struct Panel {
    pub target: Target,
    pub state: State,
    pub cwd: String,
    pub entries: Vec<FileEntry>,
    pub sort: Sort,
    pub selection: Picked,
    pub path: Entity<TextInput>,
    pub source: Option<Arc<dyn FileSource>>,
    pub login: Option<Entity<PanelLogin>>,
    pub notice: Option<String>,
    /// Answers to older listings are dropped when a newer one was asked for.
    pub(crate) generation: u64,
    _login: Option<Subscription>,
}

impl Panel {
    /// The paths of the rows shown, top to bottom.
    pub fn paths(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.path.clone()).collect()
    }

    /// The row last plainly clicked, if it is still listed.
    pub fn anchor_row(&self) -> Option<usize> {
        let anchor = self.selection.anchor()?;
        self.entries.iter().position(|e| e.path == anchor)
    }
}

/// What a host panel needs to open a connection, from the shell: the host's address and login
/// details, and an open session's connection if there is one.
pub struct HostAccess {
    pub spec: ConnectSpec,
    pub auth: Auth,
    pub session: Option<SessionHandle>,
    /// `user@host:port`: what names the host in the queue, whichever connection serves it.
    pub label: String,
}

/// A dialog over the panels.
pub enum Dialog {
    NewFolder {
        side: usize,
        input: Entity<TextInput>,
    },
    Rename {
        side: usize,
        entry: FileEntry,
        input: Entity<TextInput>,
    },
    Delete {
        side: usize,
        entries: Vec<FileEntry>,
    },
}

/// Items being dragged from one panel to the other.
#[derive(Clone)]
pub struct Dragged {
    pub from: usize,
    pub entries: Vec<FileEntry>,
}

pub struct FilesView {
    pub(crate) focus: FocusHandle,
    pub(crate) theme: Theme,
    pub(crate) transfers: Entity<Transfers>,
    shell: WeakEntity<Shell>,
    pub(crate) hosts: Vec<HostEntry>,
    pub(crate) panels: [Panel; 2],
    /// The panel the keyboard goes to.
    pub(crate) active: usize,
    pub(crate) dialog: Option<Dialog>,
    /// The panel whose source list is open.
    pub(crate) menu: Option<usize>,
    /// The login field that was last given the keyboard, so a new question gets it once.
    pub(crate) asked: Option<gpui::EntityId>,
    pub(crate) rt: tokio::runtime::Handle,
    _subs: Vec<Subscription>,
}

impl Focusable for FilesView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FilesView {
    /// The Mac on the left, `right` (the active tab's host, with how to reach it) on the right.
    pub fn new(
        hosts: Vec<HostEntry>,
        right: Option<(String, HostAccess)>,
        transfers: Entity<Transfers>,
        shell: WeakEntity<Shell>,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Self {
        let rt = crate::platform::SshRuntime::handle(cx);
        let colors = theme.input_colors();
        let panel = |target: Target, cx: &mut Context<Self>| {
            let path = cx.new(|cx| TextInput::new("/", false, colors, cx));
            cx.observe(&path, |_, _, cx| cx.notify()).detach();
            Panel {
                target,
                state: State::Connecting,
                cwd: String::new(),
                entries: Vec::new(),
                sort: Sort::default(),
                selection: Picked::default(),
                path,
                source: None,
                login: None,
                notice: None,
                generation: 0,
                _login: None,
            }
        };
        let left = panel(Target::Mac, cx);
        let (right_target, access) = match right {
            Some((alias, access)) => (Target::Host(alias), Some(access)),
            None => (Target::Mac, None),
        };
        let right = panel(right_target, cx);
        let refresh = cx.subscribe(&transfers, |this, _, done: &Finished, cx| {
            this.transfer_finished(done, cx);
        });
        let watch = cx.observe(&transfers, |_, _, cx| cx.notify());
        let mut this = Self {
            focus: cx.focus_handle(),
            theme,
            transfers,
            shell,
            hosts,
            panels: [left, right],
            active: 0,
            dialog: None,
            menu: None,
            asked: None,
            rt,
            _subs: vec![refresh, watch],
        };
        // The shell is being updated while it makes this view, so it cannot be asked for the
        // host here: what the right panel needs came with the call.
        this.start(0, None, cx);
        this.start(1, access, cx);
        this
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    /// The other panel.
    pub(crate) fn other(side: usize) -> usize {
        1 - side
    }

    /// Shows `target` in panel `side`, starting from its home folder.
    pub fn set_target(&mut self, side: usize, target: Target, cx: &mut Context<Self>) {
        self.menu = None;
        let p = &mut self.panels[side];
        p.target = target;
        self.connect(side, cx);
    }

    /// Opens the panel's source: the Mac at once, a host over an open session's connection or
    /// its own, whose login questions the panel asks.
    pub fn connect(&mut self, side: usize, cx: &mut Context<Self>) {
        let access = match self.panels[side].target.clone() {
            Target::Mac => None,
            Target::Host(alias) => self
                .shell
                .read_with(cx, |s, cx| s.host_access(&alias, cx))
                .ok()
                .flatten(),
        };
        self.start(side, access, cx);
    }

    /// Retry: a session opened since the panel failed serves it; otherwise a failed login of
    /// its own asks again, and anything else starts over.
    pub fn retry(&mut self, side: usize, cx: &mut Context<Self>) {
        let login = self.panels[side].login.clone();
        let failed = login
            .as_ref()
            .is_some_and(|l| matches!(l.read(cx).step(), Step::Failed(_)));
        let session = match self.panels[side].target.clone() {
            Target::Host(alias) => self
                .shell
                .read_with(cx, |s, cx| s.host_access(&alias, cx))
                .ok()
                .flatten()
                .is_some_and(|a| a.session.is_some()),
            Target::Mac => false,
        };
        match login {
            Some(login) if failed && !session => login.update(cx, |l, cx| l.retry(cx)),
            _ => self.connect(side, cx),
        }
    }

    fn start(&mut self, side: usize, access: Option<HostAccess>, cx: &mut Context<Self>) {
        let p = &mut self.panels[side];
        p.generation += 1;
        p.login = None;
        p._login = None;
        p.source = None;
        p.entries.clear();
        p.cwd.clear();
        p.selection.clear();
        p.notice = None;
        p.state = State::Connecting;
        cx.notify();
        let alias = match p.target.clone() {
            Target::Mac => {
                p.source = Some(Arc::new(LocalFs::new()));
                return self.go_home(side, cx);
            }
            Target::Host(alias) => alias,
        };
        let Some(access) = access else {
            self.panels[side].state = State::Failed(Reason::Unknown(alias));
            return;
        };
        match access.session.clone() {
            Some(handle) => self.open_sftp(side, handle, access.label, cx),
            None => self.login(side, access, cx),
        }
    }

    /// No open session to the host: the panel connects on its own.
    fn login(&mut self, side: usize, access: HostAccess, cx: &mut Context<Self>) {
        let colors = self.theme.input_colors();
        let spec = dev_spec(access.spec);
        let label = access.label;
        let login = cx.new(|cx| PanelLogin::start(spec, access.auth, colors, cx));
        let sub = cx.subscribe(&login, move |this, login, event: &LoginEvent, cx| {
            match event {
                LoginEvent::Connected => {
                    if let Some(handle) = login.read(cx).handle() {
                        this.open_sftp(side, handle, label.clone(), cx);
                    }
                }
                LoginEvent::Dropped(why) => {
                    this.panels[side].state = State::Dropped(Reason::Lost(why.clone()));
                }
            }
            cx.notify();
        });
        cx.observe(&login, |_, _, cx| cx.notify()).detach();
        let p = &mut self.panels[side];
        p.login = Some(login);
        p._login = Some(sub);
        cx.notify();
    }

    /// Opens SFTP on `handle`. The connection rides along with its source, whoever opened it, so
    /// a copy in progress keeps it open after the panel or the terminal tab is closed.
    fn open_sftp(
        &mut self,
        side: usize,
        handle: SessionHandle,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let generation = self.panels[side].generation;
        let rt = self.rt.clone();
        self.panels[side].state = State::Loading;
        cx.spawn(async move |this, cx| {
            let opened = rt.spawn(SftpFiles::open(label, handle)).await;
            let _ = this.update(cx, |this, cx| {
                let p = &mut this.panels[side];
                if p.generation != generation {
                    return;
                }
                match opened {
                    Ok(Ok(files)) => {
                        p.source = Some(Arc::new(files));
                        this.go_home(side, cx);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "files_sftp_open_failed");
                        p.state = State::Failed(Reason::Said(e.to_string()));
                    }
                    Err(_) => p.state = State::Failed(Reason::Gone),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn go_home(&mut self, side: usize, cx: &mut Context<Self>) {
        let Some(source) = self.panels[side].source.clone() else {
            return;
        };
        let generation = self.panels[side].generation;
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| {
            let home = rt.spawn(async move { source.home().await }).await;
            let _ = this.update(cx, |this, cx| {
                if this.panels[side].generation != generation {
                    return;
                }
                match home {
                    Ok(Ok(home)) => this.go(side, home, cx),
                    Ok(Err(e)) => this.failed(side, &e, cx),
                    Err(_) => this.panels[side].state = State::Failed(Reason::Gone),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Lists `path` and makes it the folder shown once the source answers; the old rows stay on
    /// screen until then, so a slow host does not flash an empty list.
    pub fn go(&mut self, side: usize, path: String, cx: &mut Context<Self>) {
        let p = &mut self.panels[side];
        let Some(source) = p.source.clone() else {
            return;
        };
        p.generation += 1;
        let generation = p.generation;
        if p.entries.is_empty() {
            p.state = State::Loading;
        }
        p.notice = None;
        // The first folder after a login still shows what the login said.
        if !p.cwd.is_empty()
            && let Some(login) = p.login.clone()
        {
            login.update(cx, |l, cx| l.clear_note(cx));
        }
        cx.notify();
        let rt = self.rt.clone();
        let asked = path.clone();
        cx.spawn(async move |this, cx| {
            let listed = rt.spawn(async move { source.list(&asked).await }).await;
            let _ = this.update(cx, |this, cx| {
                if this.panels[side].generation != generation {
                    return;
                }
                match listed {
                    Ok(Ok(mut entries)) => {
                        tracing::info!(path = %path, count = entries.len(), "files_list");
                        let p = &mut this.panels[side];
                        order(&mut entries, p.sort);
                        p.cwd = path.clone();
                        p.entries = entries;
                        p.selection.keep_only(&p.paths());
                        p.state = State::Ready;
                        p.path.update(cx, |i, cx| i.set_text(path, cx));
                    }
                    Ok(Err(e)) => this.listing_failed(side, &e, cx),
                    Err(_) => this.panels[side].state = State::Failed(Reason::Gone),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A listing failed: a gone connection is its own state; anything else stays a notice when
    /// there is a folder to stay in.
    fn listing_failed(&mut self, side: usize, e: &FileError, cx: &mut Context<Self>) {
        tracing::warn!(error = %e, "files_list_failed");
        if self.panels[side].cwd.is_empty() {
            return self.failed(side, e, cx);
        }
        self.panels[side].notice = Some(Reason::from(e).text());
        self.panels[side].state = if *e == FileError::Closed {
            State::Dropped(Reason::Gone)
        } else {
            State::Ready
        };
    }

    fn failed(&mut self, side: usize, e: &FileError, _cx: &mut Context<Self>) {
        self.panels[side].state = match e {
            FileError::Closed => State::Dropped(Reason::Gone),
            other => State::Failed(Reason::from(other)),
        };
    }

    pub fn up(&mut self, side: usize, cx: &mut Context<Self>) {
        let up = tern_ssh::parent(&self.panels[side].cwd);
        self.go(side, up, cx);
    }

    pub fn refresh(&mut self, side: usize, cx: &mut Context<Self>) {
        let cwd = self.panels[side].cwd.clone();
        if !cwd.is_empty() {
            self.go(side, cwd, cx);
        }
    }

    /// Enter in the path field.
    pub fn go_typed(&mut self, side: usize, cx: &mut Context<Self>) {
        let typed = self.panels[side].path.read(cx).text().trim().to_owned();
        if !typed.is_empty() {
            self.go(side, typed, cx);
        }
    }

    /// Double click or Enter: a folder is entered; a file is left alone (copy it instead).
    pub fn open_row(&mut self, side: usize, ix: usize, cx: &mut Context<Self>) {
        if let Some(e) = self.panels[side].entries.get(ix).cloned()
            && e.is_dir
        {
            self.go(side, e.path, cx);
        }
    }

    pub fn sort_by(&mut self, side: usize, key: SortKey, cx: &mut Context<Self>) {
        let p = &mut self.panels[side];
        p.sort = p.sort.click(key);
        order(&mut p.entries, p.sort);
        cx.notify();
    }

    /// Up and down keys: move the pick by `delta` rows from the last one clicked.
    pub fn step(&mut self, side: usize, delta: isize, cx: &mut Context<Self>) {
        let p = &mut self.panels[side];
        if let Some(next) = crate::files_text::step(p.anchor_row(), delta, p.entries.len()) {
            p.selection.only(&p.paths(), next);
            cx.notify();
        }
    }

    pub fn click_row(
        &mut self,
        side: usize,
        ix: usize,
        command: bool,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        self.active = side;
        let p = &mut self.panels[side];
        p.selection.click(&p.paths(), ix, command, shift);
        cx.notify();
    }

    /// The rows picked in panel `side`, as entries.
    pub fn picked(&self, side: usize) -> Vec<FileEntry> {
        let p = &self.panels[side];
        p.entries
            .iter()
            .filter(|e| p.selection.contains(&e.path))
            .cloned()
            .collect()
    }

    pub fn toggle_menu(&mut self, side: usize, cx: &mut Context<Self>) {
        self.menu = if self.menu == Some(side) {
            None
        } else {
            Some(side)
        };
        if self.menu.is_some()
            && let Ok(hosts) = self.shell.read_with(cx, |s, _| s.hosts().to_vec())
        {
            self.hosts = hosts;
        }
        cx.notify();
    }
}

/// Debug builds keep scripted logins out of the real known_hosts, as a session's tab does.
#[cfg(debug_assertions)]
fn dev_spec(spec: ConnectSpec) -> ConnectSpec {
    crate::shell::with_dev_known_hosts(spec)
}

#[cfg(not(debug_assertions))]
fn dev_spec(spec: ConnectSpec) -> ConnectSpec {
    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reason_is_worded_in_the_hosts_terms() {
        assert_eq!(Reason::Gone.text(), "The host stopped answering.");
        assert_eq!(Reason::from(&FileError::Closed).text(), Reason::Gone.text());
        assert_eq!(
            Reason::Unknown("web".into()).text(),
            "web: not a host tern knows"
        );
        assert_eq!(
            Reason::Lost("exit status 1".into()).text(),
            "The host went away: exit status 1"
        );
        assert_eq!(Reason::Lost(String::new()).text(), "The host went away.");
    }
}
