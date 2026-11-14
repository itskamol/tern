// Adapted from zeron crates/ui/src/files/tree.rs (render_tree_row: row height, hover and
// selection washes, folder/file glyphs) (MIT).
//! The Files tab: two panels side by side, each the Mac's own files or one host's, for copying
//! between them. This file holds the state and what the panels do; `files_tab_view.rs` draws it.
//!
//! Every panel talks to a [`FileSource`], so a panel neither knows nor cares whether it shows the
//! Mac or a host. The calls run on the SSH runtime (the Mac's adapter needs a tokio reactor), and
//! the panel is told when they are done. Copies go to the shell's transfer queue, which outlives
//! this tab.

pub use crate::files_tab_model::{State, Target};
use std::sync::Arc;

use gpui::{AppContext, Context, Entity, FocusHandle, Focusable, Subscription, WeakEntity};
use tern_ssh::{ConnectSpec, FileEntry, FileSource, HostEntry, LocalFs, SessionHandle, SftpFiles};

use crate::files_model::SortKey;
use crate::files_tab_model::{Ask, PanelModel, Reason};
use crate::panel_login::{LoginEvent, PanelLogin, Step};
use crate::password_command::Auth;
use crate::shell::Shell;
use crate::text_input::TextInput;
use crate::theme::Theme;
use crate::transfers::{Finished, Transfers};

/// One panel: its model, and what only the window holds.
pub struct Panel {
    pub model: PanelModel,
    pub path: Entity<TextInput>,
    pub source: Option<Arc<dyn FileSource>>,
    pub login: Option<Entity<PanelLogin>>,
    _login: Option<Subscription>,
}

impl std::ops::Deref for Panel {
    type Target = PanelModel;

    fn deref(&self) -> &PanelModel {
        &self.model
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
                model: PanelModel::new(target),
                path,
                source: None,
                login: None,
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
        self.panels[side].model.switch_to(target);
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
        p.model.start();
        p.login = None;
        p._login = None;
        p.source = None;
        cx.notify();
        let alias = match p.target.clone() {
            Target::Mac => {
                p.source = Some(Arc::new(LocalFs::new()));
                let ask = p.model.files_open();
                return self.run(side, ask, cx);
            }
            Target::Host(alias) => alias,
        };
        let Some(access) = access else {
            self.panels[side].model.unknown_host(&alias);
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
                    this.panels[side].model.dropped(why.clone());
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
        let ticket = self.panels[side].model.opening();
        let rt = self.rt.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let opened = rt.spawn(SftpFiles::open(label, handle)).await;
            let _ = this.update(cx, |this, cx| {
                let opened = match opened {
                    Ok(Ok(files)) => {
                        this.panels[side].source = Some(Arc::new(files));
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "files_sftp_open_failed");
                        Err(Reason::Said(e.to_string()))
                    }
                    Err(_) => Err(Reason::Gone),
                };
                let ask = this.panels[side].model.opened(ticket, opened);
                if let Some(ask) = ask {
                    this.run(side, ask, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Does what the model asked of the panel's source, and tells it the answer.
    fn run(&mut self, side: usize, ask: Ask, cx: &mut Context<Self>) {
        let Some(source) = self.panels[side].source.clone() else {
            return;
        };
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| match ask {
            Ask::Home(ticket) => {
                let home = rt.spawn(async move { source.home().await }).await;
                let _ = this.update(cx, |this, cx| {
                    let next = this.panels[side].model.home(ticket, answer(home));
                    if let Some(next) = next {
                        this.run(side, next, cx);
                    }
                    cx.notify();
                });
            }
            Ask::List(ticket, path) => {
                let asked = path.clone();
                let listed = rt.spawn(async move { source.list(&asked).await }).await;
                let _ = this.update(cx, |this, cx| {
                    let listed = answer(listed);
                    match &listed {
                        Ok(entries) => {
                            tracing::info!(path = %path, count = entries.len(), "files_list");
                        }
                        Err(why) => tracing::warn!(reason = %why.text(), "files_list_failed"),
                    }
                    let p = &mut this.panels[side];
                    if p.model.listed(ticket, &path, listed) {
                        p.path.update(cx, |i, cx| i.set_text(path, cx));
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    pub fn go_home(&mut self, side: usize, cx: &mut Context<Self>) {
        if self.panels[side].source.is_some() {
            let ask = self.panels[side].model.go_home();
            self.run(side, ask, cx);
        }
    }

    /// Lists `path` and makes it the folder shown once the source answers.
    pub fn go(&mut self, side: usize, path: String, cx: &mut Context<Self>) {
        let p = &mut self.panels[side];
        if p.source.is_none() {
            return;
        }
        // The first folder after a login still shows what the login said.
        if !p.cwd.is_empty()
            && let Some(login) = p.login.clone()
        {
            login.update(cx, |l, cx| l.clear_note(cx));
        }
        let ask = self.panels[side].model.open(path);
        cx.notify();
        self.run(side, ask, cx);
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
        self.panels[side].model.sort_by(key);
        cx.notify();
    }

    /// Up and down keys: move the pick by `delta` rows from the last one clicked.
    pub fn step(&mut self, side: usize, delta: isize, cx: &mut Context<Self>) {
        self.panels[side].model.step(delta);
        cx.notify();
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
        self.panels[side].model.click_row(ix, command, shift);
        cx.notify();
    }

    /// The rows picked in panel `side`, as entries.
    pub fn picked(&self, side: usize) -> Vec<FileEntry> {
        self.panels[side].picked()
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

/// What a source answered, or why the call never came back.
fn answer<T>(
    joined: Result<Result<T, tern_ssh::FileError>, tokio::task::JoinError>,
) -> Result<T, Reason> {
    joined
        .map_err(|_| Reason::Gone)?
        .map_err(|e| Reason::from(&e))
}
