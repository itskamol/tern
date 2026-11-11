// Adapted from zeron crates/ui/src/files/tree.rs (render_tree_row: row height, hover and
// selection washes, folder/file glyphs) (MIT).
//! The Files tab: two panels side by side, each the Mac's own files or one host's, for copying
//! between them. This file holds the state and what the panels do; `files_tab_view.rs` draws it.
//!
//! Every panel talks to a [`FileSource`], so a panel neither knows nor cares whether it shows the
//! Mac or a host. The calls run on the SSH runtime (the Mac's adapter needs a tokio reactor), and
//! the panel is told when they are done. Copies go to the shell's transfer queue, which outlives
//! this tab.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext, Context, Entity, FocusHandle, Focusable, Subscription, WeakEntity, Window};
use tern_ssh::{
    Call, ConnectSpec, CopyJob, FileEntry, FileError, FileSource, HostEntry, LocalFs,
    SessionHandle, SftpFiles,
};

use crate::files_model::{Selection, Sort, SortKey, order};
use crate::panel_login::{LoginEvent, PanelLogin};
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

/// Where a panel stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Opening the connection, or answering its login questions.
    Connecting,
    Loading,
    Ready,
    /// Could not connect or list; Retry tries again.
    Failed(String),
    /// The connection was up and went away; Reconnect starts over.
    Dropped(String),
}

pub struct Panel {
    pub target: Target,
    pub state: State,
    pub cwd: String,
    pub entries: Vec<FileEntry>,
    pub sort: Sort,
    pub selection: Selection,
    pub path: Entity<TextInput>,
    pub source: Option<Arc<dyn FileSource>>,
    pub login: Option<Entity<PanelLogin>>,
    pub notice: Option<String>,
    /// Answers to older listings are dropped when a newer one was asked for.
    generation: u64,
    _login: Option<Subscription>,
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
                selection: Selection::default(),
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

    // ---- choosing and connecting ---------------------------------------------------------

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
            self.panels[side].state = State::Failed(format!("{alias}: not a host tern knows"));
            return;
        };
        match access.session.clone() {
            Some(handle) => self.open_sftp(side, handle, access.label, false, cx),
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
                        this.open_sftp(side, handle, label.clone(), true, cx);
                    }
                }
                LoginEvent::Dropped(why) => {
                    this.panels[side].state = State::Dropped(why.clone());
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

    /// Opens SFTP on `handle`. A connection the panel made itself rides along with its source,
    /// so it closes when the panel and every copy using it are done.
    fn open_sftp(
        &mut self,
        side: usize,
        handle: SessionHandle,
        label: String,
        owned: bool,
        cx: &mut Context<Self>,
    ) {
        let generation = self.panels[side].generation;
        let rt = self.rt.clone();
        self.panels[side].state = State::Loading;
        cx.spawn(async move |this, cx| {
            let opened = {
                let handle = handle.clone();
                rt.spawn(async move { handle.open_sftp().await }).await
            };
            let _ = this.update(cx, |this, cx| {
                let p = &mut this.panels[side];
                if p.generation != generation {
                    return;
                }
                match opened {
                    Ok(Ok(sftp)) => {
                        let mut files = SftpFiles::new(label, sftp);
                        if owned {
                            files = files.keeping(handle);
                        }
                        p.source = Some(Arc::new(files));
                        this.go_home(side, cx);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "files_sftp_open_failed");
                        p.state = State::Failed(e.to_string());
                    }
                    Err(_) => p.state = State::Failed("the connection ended".into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- browsing ------------------------------------------------------------------------

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
                    Err(_) => {
                        this.panels[side].state = State::Failed("the connection ended".into())
                    }
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
                        p.selection.clear();
                        p.state = State::Ready;
                        p.path.update(cx, |i, cx| i.set_text(path, cx));
                    }
                    Ok(Err(e)) => this.listing_failed(side, &e, cx),
                    Err(_) => {
                        this.panels[side].state = State::Failed("the connection ended".into())
                    }
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
        self.panels[side].notice = Some(e.to_string());
        self.panels[side].state = if *e == FileError::Closed {
            State::Dropped("The connection closed.".into())
        } else {
            State::Ready
        };
    }

    fn failed(&mut self, side: usize, e: &FileError, _cx: &mut Context<Self>) {
        self.panels[side].state = match e {
            FileError::Closed => State::Dropped("The connection closed.".into()),
            other => State::Failed(other.to_string()),
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
        let picked: Vec<String> = p
            .selection
            .rows()
            .filter_map(|ix| p.entries.get(ix).map(|e| e.path.clone()))
            .collect();
        p.sort = p.sort.click(key);
        order(&mut p.entries, p.sort);
        // The same items stay picked in their new places.
        p.selection.clear();
        for (ix, e) in p.entries.iter().enumerate() {
            if picked.contains(&e.path) {
                p.selection.click(ix, true, false);
            }
        }
        cx.notify();
    }

    /// Up and down keys: move the pick by `delta` rows from the last one clicked.
    pub fn step(&mut self, side: usize, delta: isize, cx: &mut Context<Self>) {
        let p = &mut self.panels[side];
        if let Some(next) = crate::sftp_paths::step(p.selection.anchor(), delta, p.entries.len()) {
            p.selection.only(next);
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
        self.panels[side].selection.click(ix, command, shift);
        cx.notify();
    }

    /// The rows picked in panel `side`, as entries.
    pub fn picked(&self, side: usize) -> Vec<FileEntry> {
        let p = &self.panels[side];
        p.selection
            .rows()
            .filter_map(|ix| p.entries.get(ix).cloned())
            .collect()
    }

    // ---- copying -------------------------------------------------------------------------

    /// The Copy button: the picked items go to the other panel's folder.
    pub fn copy_picked(&mut self, side: usize, cx: &mut Context<Self>) {
        let entries = self.picked(side);
        self.copy(side, entries, Self::other(side), cx);
    }

    /// Copies `entries` of panel `from` into the folder panel `to` shows.
    pub fn copy(
        &mut self,
        from: usize,
        entries: Vec<FileEntry>,
        to: usize,
        cx: &mut Context<Self>,
    ) {
        if entries.is_empty() {
            return;
        }
        let (Some(src), Some(dst)) = (
            self.panels[from].source.clone(),
            self.panels[to].source.clone(),
        ) else {
            self.panels[to].notice = Some("Both panels need a connection to copy.".into());
            return cx.notify();
        };
        if self.panels[to].state != State::Ready {
            self.panels[to].notice = Some("Open a folder to copy into first.".into());
            return cx.notify();
        }
        let dir = self.panels[to].cwd.clone();
        let label = copy_label(&entries, &self.panels[to].target.label());
        let jobs = entries
            .into_iter()
            .map(|item| CopyJob {
                from: src.clone(),
                item,
                to: dst.clone(),
                dir: dir.clone(),
                rule: None,
            })
            .collect();
        self.transfers.update(cx, |t, cx| t.submit(jobs, label, cx));
    }

    /// Files dropped from Finder upload into the folder panel `side` shows.
    pub fn drop_files(&mut self, side: usize, paths: &[PathBuf], cx: &mut Context<Self>) {
        let entries: Vec<FileEntry> = paths.iter().filter_map(|p| local_entry(p)).collect();
        if entries.is_empty() {
            return;
        }
        let Some(dst) = self.panels[side].source.clone() else {
            return;
        };
        if self.panels[side].state != State::Ready {
            return;
        }
        let dir = self.panels[side].cwd.clone();
        let label = copy_label(&entries, &self.panels[side].target.label());
        let mac: Arc<dyn FileSource> = Arc::new(LocalFs::new());
        let jobs = entries
            .into_iter()
            .map(|item| CopyJob {
                from: mac.clone(),
                item,
                to: dst.clone(),
                dir: dir.clone(),
                rule: None,
            })
            .collect();
        self.transfers.update(cx, |t, cx| t.submit(jobs, label, cx));
    }

    /// A batch ended: panels showing its destination read it again.
    fn transfer_finished(&mut self, done: &Finished, cx: &mut Context<Self>) {
        for side in 0..2 {
            let p = &self.panels[side];
            let same_place = p
                .source
                .as_ref()
                .is_some_and(|s| s.id() == done.summary.dest);
            if same_place && p.cwd == done.summary.dir {
                self.refresh(side, cx);
            }
        }
    }

    // ---- making, renaming, deleting ------------------------------------------------------

    pub fn start_new_folder(&mut self, side: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.panels[side].state != State::Ready {
            return;
        }
        let input = self.dialog_input("New folder", String::new(), cx);
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.dialog = Some(Dialog::NewFolder { side, input });
        cx.notify();
    }

    pub fn start_rename(&mut self, side: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut picked = self.picked(side);
        if picked.len() != 1 {
            return;
        }
        let entry = picked.remove(0);
        let input = self.dialog_input("Name", entry.name.clone(), cx);
        input.update(cx, |i, cx| i.select_everything(cx));
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.dialog = Some(Dialog::Rename { side, entry, input });
        cx.notify();
    }

    pub fn start_delete(&mut self, side: usize, cx: &mut Context<Self>) {
        let entries = self.picked(side);
        if !entries.is_empty() {
            self.dialog = Some(Dialog::Delete { side, entries });
            cx.notify();
        }
    }

    fn dialog_input(
        &mut self,
        placeholder: &'static str,
        text: String,
        cx: &mut Context<Self>,
    ) -> Entity<TextInput> {
        let colors = self.theme.input_colors();
        let input = cx.new(|cx| {
            let mut input = TextInput::new(placeholder, false, colors, cx);
            input.set_text(text, cx);
            input
        });
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        input
    }

    pub fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// Enter or the confirm button.
    pub fn confirm_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        window.focus(&self.focus, cx);
        match dialog {
            Dialog::NewFolder { side, input } => {
                let name = input.read(cx).text().trim().to_owned();
                if let Some(name) = plain_name(&name) {
                    let path = tern_ssh::join(&self.panels[side].cwd, name);
                    self.change(side, "make the folder", cx, move |s| {
                        Box::pin(async move { s.mkdir(&path).await })
                    });
                } else {
                    self.panels[side].notice = Some(format!("{name:?} is not a folder name."));
                }
            }
            Dialog::Rename { side, entry, input } => {
                let name = input.read(cx).text().trim().to_owned();
                match plain_name(&name) {
                    Some(name) if name != entry.name => {
                        let to = tern_ssh::join(&tern_ssh::parent(&entry.path), name);
                        self.change(side, "rename", cx, move |s| {
                            Box::pin(async move { s.rename(&entry.path, &to).await })
                        });
                    }
                    Some(_) => {}
                    None => self.panels[side].notice = Some(format!("{name:?} is not a name.")),
                }
            }
            Dialog::Delete { side, entries } => {
                self.change(side, "delete", cx, move |s| {
                    Box::pin(async move {
                        for e in &entries {
                            s.delete(&e.path).await?;
                        }
                        Ok(())
                    })
                });
            }
        }
        cx.notify();
    }

    /// Runs one change on panel `side`'s source, then lists the folder again.
    fn change(
        &mut self,
        side: usize,
        what: &'static str,
        cx: &mut Context<Self>,
        work: impl FnOnce(Arc<dyn FileSource>) -> Call<'static, ()> + Send + 'static,
    ) {
        let Some(source) = self.panels[side].source.clone() else {
            return;
        };
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| {
            let done = rt.spawn(work(source)).await;
            let _ = this.update(cx, |this, cx| {
                match done {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, what, "files_change_failed");
                        this.panels[side].notice = Some(format!("Could not {what}: {e}"));
                    }
                    Err(_) => this.panels[side].notice = Some(format!("Could not {what}.")),
                }
                this.refresh(side, cx);
            });
        })
        .detach();
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

/// A name for something in a folder: no empty names, no paths.
fn plain_name(name: &str) -> Option<&str> {
    (!matches!(name, "" | "." | "..") && !name.contains(['/', '\0'])).then_some(name)
}

/// What a copy is called in the transfer list and the toast.
pub fn copy_label(entries: &[FileEntry], to: &str) -> String {
    match entries {
        [one] => format!("{} to {to}", one.name),
        many => format!("{} items to {to}", many.len()),
    }
}

/// An item from Finder as a file source entry; `None` when it cannot be read.
fn local_entry(path: &std::path::Path) -> Option<FileEntry> {
    let meta = std::fs::metadata(path).ok()?;
    let name = path.file_name()?.to_str()?.to_owned();
    Some(FileEntry {
        name,
        path: path.to_str()?.to_owned(),
        is_dir: meta.is_dir(),
        is_symlink: false,
        size: meta.len(),
        modified: meta.modified().ok(),
    })
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

    fn entry(name: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: String::new(),
            is_dir: false,
            is_symlink: false,
            size: 0,
            modified: None,
        }
    }

    #[test]
    fn a_copy_is_named_for_one_item_or_the_count() {
        assert_eq!(copy_label(&[entry("a.txt")], "web-01"), "a.txt to web-01");
        assert_eq!(
            copy_label(&[entry("a"), entry("b"), entry("c")], "This Mac"),
            "3 items to This Mac"
        );
    }

    #[test]
    fn a_new_name_is_one_plain_name() {
        assert_eq!(plain_name("docs"), Some("docs"));
        for bad in ["", ".", "..", "a/b", "../x", "a\0"] {
            assert_eq!(plain_name(bad), None, "{bad:?}");
        }
    }
}
