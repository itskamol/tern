//! The Files panel: the pane in view's folder, listed over that pane's own connection.
//!
//! [`FilesPanelModel`] decides what is shown; this file does what that needs from the world:
//! opens SFTP on the pane's connection, lists the folder, and hands downloads to the shell's
//! transfer queue. Only the pane in view has rows in memory, and nothing runs while nothing
//! changes: the shell hands the panel the panes' state when it renders, and the panel asks for a
//! listing only when what it shows is not what it listed.
//!
//! A pane that is not connected is never dialled here. The panel says it is waiting, or offers
//! Reconnect, which is the session's own.

use std::collections::BTreeMap;
use std::sync::Arc;

use gpui::{ClipboardItem, Context, Entity, WeakEntity};
use tern_ssh::{CopyJob, FileEntry, FileError, FileSource, LocalFs, SftpFiles};

use crate::files_model::{Sort, order};
use crate::files_panel_model::{FilesPanelModel, Link, Place, SNIPPET, Shown, downloads_in};
use crate::session::Session;
use crate::shell::Shell;
use crate::split::PaneId;
use crate::theme::Theme;
use crate::transfers::Transfers;

/// One pane of the tab group, as the shell tells the panel about it.
pub struct PaneInfo {
    pub id: PaneId,
    /// What the header calls the host: the tab's name for it.
    pub name: String,
    pub session: Entity<Session>,
}

struct Pane {
    session: WeakEntity<Session>,
    name: String,
}

/// How far the listing of the pane in view has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Load {
    Loading,
    Ready,
    Failed(String),
}

/// The rows of the pane in view.
pub struct Listing {
    pane: PaneId,
    /// The connection and folder last asked for, so the same ask is not made twice.
    asked: Option<(u64, Place)>,
    /// Answers to an older ask are dropped.
    generation: u64,
    pub(crate) cwd: String,
    pub(crate) entries: Vec<FileEntry>,
    /// Indexes into `entries` of the rows shown: dotfiles may be hidden.
    pub(crate) visible: Vec<usize>,
    hidden_shown: bool,
    pub(crate) load: Load,
}

impl Listing {
    fn new(pane: PaneId) -> Self {
        Self {
            pane,
            asked: None,
            generation: 0,
            cwd: String::new(),
            entries: Vec::new(),
            visible: Vec::new(),
            hidden_shown: false,
            load: Load::Loading,
        }
    }

    fn show_hidden(&mut self, on: bool) {
        self.hidden_shown = on;
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| on || !e.name.starts_with('.'))
            .map(|(ix, _)| ix)
            .collect();
    }

    /// The entry on row `ix` of what is shown.
    pub(crate) fn row(&self, ix: usize) -> Option<&FileEntry> {
        self.entries.get(*self.visible.get(ix)?)
    }

    fn paths(&self) -> Vec<String> {
        self.visible
            .iter()
            .filter_map(|&ix| self.entries.get(ix))
            .map(|e| e.path.clone())
            .collect()
    }
}

pub struct FilesPanel {
    pub(crate) model: FilesPanelModel,
    panes: BTreeMap<PaneId, Pane>,
    /// The SFTP channel of each pane's connection, tagged with the connection it belongs to.
    sources: BTreeMap<PaneId, (u64, Arc<dyn FileSource>)>,
    opening: Option<(PaneId, u64)>,
    pub(crate) listing: Option<Listing>,
    pub(crate) theme: Theme,
    /// The panel's width, which decides how many columns fit.
    pub(crate) width: f32,
    transfers: Entity<Transfers>,
    shell: WeakEntity<Shell>,
    rt: tokio::runtime::Handle,
}

impl FilesPanel {
    pub fn new(
        transfers: Entity<Transfers>,
        shell: WeakEntity<Shell>,
        theme: Theme,
        width: f32,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            model: FilesPanelModel::default(),
            panes: BTreeMap::new(),
            sources: BTreeMap::new(),
            opening: None,
            listing: None,
            theme,
            width,
            transfers,
            shell,
            rt: crate::platform::SshRuntime::handle(cx),
        }
    }

    /// What the shell sees now: every pane with a session, and the one in front (`None` when a
    /// Files tab or nothing is in front). Called on every render of the window, so it only
    /// acts on a change.
    pub fn sync(
        &mut self,
        panes: Vec<PaneInfo>,
        focused: Option<PaneId>,
        theme: Theme,
        width: f32,
        cx: &mut Context<Self>,
    ) {
        self.theme = theme;
        self.width = width;
        for p in &panes {
            let s = p.session.read(cx);
            self.model
                .pane(p.id, s.host(), Link::of(&s.status()), s.shell_folder());
        }
        let live: Vec<PaneId> = panes.iter().map(|p| p.id).collect();
        self.model.keep_only(&live);
        self.sources.retain(|id, _| live.contains(id));
        self.panes = panes
            .into_iter()
            .map(|p| {
                (
                    p.id,
                    Pane {
                        session: p.session.downgrade(),
                        name: p.name,
                    },
                )
            })
            .collect();
        self.model.focus(focused);
        self.update_listing(cx);
    }

    /// The host name the header shows.
    pub(crate) fn name(&self) -> String {
        self.model
            .focused()
            .and_then(|id| self.panes.get(&id))
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    /// Lists what the model says is shown, unless that is already what was asked.
    fn update_listing(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.model.focused() else {
            self.listing = None;
            return;
        };
        let Some(Shown::Folder { place, hidden, .. }) = self.model.shown_for(pane) else {
            self.listing = None;
            return;
        };
        if self.listing.as_ref().is_none_or(|l| l.pane != pane) {
            self.listing = Some(Listing::new(pane));
        }
        let epoch = self.model.epoch(pane);
        let Some(listing) = self.listing.as_mut() else {
            return;
        };
        if listing.hidden_shown != hidden {
            listing.show_hidden(hidden);
        }
        if listing.asked == Some((epoch, place.clone())) {
            return;
        }
        match self.sources.get(&pane) {
            Some((e, source)) if *e == epoch => {
                let source = source.clone();
                self.list(pane, epoch, place, source, cx);
            }
            _ => self.open_source(pane, epoch, place, cx),
        }
    }

    /// Opens SFTP on the pane's own connection. The connection rides along with its source, so
    /// a download in progress keeps it open even if the pane is closed.
    fn open_source(&mut self, pane: PaneId, epoch: u64, place: Place, cx: &mut Context<Self>) {
        if self.opening == Some((pane, epoch)) {
            return;
        }
        let session = self.panes.get(&pane).and_then(|p| p.session.upgrade());
        let Some((handle, label)) = session.and_then(|s| {
            let s = s.read(cx);
            Some((s.ssh_handle()?, s.label()))
        }) else {
            return self.fail(pane, epoch, place, "The host stopped answering.".into(), cx);
        };
        self.opening = Some((pane, epoch));
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| {
            let opened = {
                let handle = handle.clone();
                rt.spawn(async move { handle.open_sftp().await }).await
            };
            let _ = this.update(cx, |this, cx| {
                if this.opening == Some((pane, epoch)) {
                    this.opening = None;
                }
                if this.model.epoch(pane) != epoch {
                    return this.update_listing(cx);
                }
                match opened {
                    Ok(Ok(sftp)) => {
                        let files = SftpFiles::new(label, sftp).keeping(handle);
                        this.sources.insert(pane, (epoch, Arc::new(files)));
                        this.update_listing(cx);
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "files_panel_sftp_open_failed");
                        this.fail(pane, epoch, place, e.to_string(), cx);
                    }
                    Err(_) => {
                        this.fail(pane, epoch, place, "The host stopped answering.".into(), cx)
                    }
                }
            });
        })
        .detach();
    }

    /// Lists `place` and shows it once the source answers; the old rows stay until then, so a
    /// slow host does not flash an empty list.
    fn list(
        &mut self,
        pane: PaneId,
        epoch: u64,
        place: Place,
        source: Arc<dyn FileSource>,
        cx: &mut Context<Self>,
    ) {
        let Some(l) = self.listing.as_mut() else {
            return;
        };
        l.generation += 1;
        let generation = l.generation;
        l.asked = Some((epoch, place.clone()));
        if l.entries.is_empty() {
            l.load = Load::Loading;
        }
        cx.notify();
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| {
            let found = rt
                .spawn(async move {
                    let path = match place {
                        Place::Path(path) => path,
                        Place::Home => source.home().await?,
                    };
                    let entries = source.list(&path).await?;
                    Ok::<_, FileError>((path, entries))
                })
                .await;
            let _ = this.update(cx, |this, cx| this.listed(pane, generation, found, cx));
        })
        .detach();
    }

    fn listed(
        &mut self,
        pane: PaneId,
        generation: u64,
        found: Result<Result<(String, Vec<FileEntry>), FileError>, tokio::task::JoinError>,
        cx: &mut Context<Self>,
    ) {
        let Some(l) = self.listing.as_mut() else {
            return;
        };
        if l.pane != pane || l.generation != generation {
            return;
        }
        match found {
            Ok(Ok((path, mut entries))) => {
                tracing::info!(count = entries.len(), "files_panel_list");
                order(&mut entries, Sort::default());
                l.cwd = path;
                l.entries = entries;
                l.load = Load::Ready;
                let hidden = l.hidden_shown;
                l.show_hidden(hidden);
                self.model.clear_picked();
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "files_panel_list_failed");
                l.load = Load::Failed(match e {
                    FileError::Closed => "The host stopped answering.".to_owned(),
                    other => other.to_string(),
                });
            }
            Err(_) => l.load = Load::Failed("The host stopped answering.".into()),
        }
        cx.notify();
    }

    /// The pane could not be listed: say why, and do not ask again until the person retries.
    fn fail(
        &mut self,
        pane: PaneId,
        epoch: u64,
        place: Place,
        why: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(l) = self.listing.as_mut().filter(|l| l.pane == pane) {
            l.asked = Some((epoch, place));
            l.load = Load::Failed(why);
        }
        cx.notify();
    }

    /// Opens a folder by hand: the panel stops following the shell.
    pub fn open_path(&mut self, path: &str, cx: &mut Context<Self>) {
        self.model.open_folder(path);
        self.update_listing(cx);
        cx.notify();
    }

    /// The folder above the one shown.
    pub fn up(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.listing.as_ref().map(|l| l.cwd.clone()) else {
            return;
        };
        if !cwd.is_empty() {
            self.open_path(&tern_ssh::parent(&cwd), cx);
        }
    }

    pub fn back_to_shell(&mut self, cx: &mut Context<Self>) {
        self.model.back_to_shell();
        self.update_listing(cx);
        cx.notify();
    }

    pub fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.model.toggle_hidden();
        self.update_listing(cx);
        cx.notify();
    }

    /// Lists the folder again, over a fresh channel when the last attempt failed to open one.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(pane) = self.model.focused() {
            if matches!(
                self.listing.as_ref().map(|l| &l.load),
                Some(Load::Failed(_))
            ) {
                self.sources.remove(&pane);
            }
            if let Some(l) = self.listing.as_mut() {
                l.asked = None;
            }
        }
        self.update_listing(cx);
        cx.notify();
    }

    pub fn click_row(&mut self, ix: usize, command: bool, shift: bool, cx: &mut Context<Self>) {
        let Some(rows) = self.listing.as_ref().map(Listing::paths) else {
            return;
        };
        self.model.click_row(&rows, ix, command, shift);
        cx.notify();
    }

    /// Double click: a folder opens; a file is left alone (Download it).
    pub fn open_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        let dir = self
            .listing
            .as_ref()
            .and_then(|l| l.row(ix))
            .filter(|e| e.is_dir)
            .map(|e| e.path.clone());
        if let Some(path) = dir {
            self.open_path(&path, cx);
        }
    }

    /// Reconnect on the pane's own session; the panel waits while it logs in again.
    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        let session = self
            .model
            .focused()
            .and_then(|id| self.panes.get(&id))
            .and_then(|p| p.session.upgrade());
        if let Some(session) = session {
            session.update(cx, |s, cx| s.reconnect(cx));
        }
    }

    pub fn copy_snippet(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(SNIPPET.to_owned()));
        let _ = self.shell.update(cx, |s, cx| {
            s.activity("Copied the line for your shell's rc file", cx)
        });
    }

    /// What is picked, as entries.
    pub(crate) fn picked_entries(&self) -> Vec<FileEntry> {
        let picked = self.model.picked();
        self.listing
            .as_ref()
            .map(|l| {
                l.entries
                    .iter()
                    .filter(|e| picked.binary_search(&e.path).is_ok())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Download: the picked items go to `~/Downloads` through the shell's transfer queue, which
    /// shows the progress as Activity and asks about a name that is taken.
    pub fn download(&mut self, cx: &mut Context<Self>) {
        let entries = self.picked_entries();
        let source = self
            .model
            .focused()
            .and_then(|id| self.sources.get(&id))
            .map(|(_, s)| s.clone());
        let (Some(source), false) = (source, entries.is_empty()) else {
            return;
        };
        let Some(dir) = std::env::home_dir().map(|h| downloads_in(&h)) else {
            return self.alert("tern cannot find your home folder", cx);
        };
        let Some(dir_text) = dir.to_str().map(str::to_owned) else {
            return self.alert("The Downloads folder's name is not text", cx);
        };
        let label = crate::files_tab_copy::copy_label(&entries, "~/Downloads");
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| {
            let made = rt
                .spawn_blocking(move || std::fs::create_dir_all(&dir))
                .await;
            let _ = this.update(cx, |this, cx| match made {
                Ok(Ok(())) => {
                    let jobs = download_jobs(&source, entries, &dir_text);
                    this.transfers.update(cx, |t, cx| t.submit(jobs, label, cx));
                }
                Ok(Err(e)) => this.alert(&format!("Could not open ~/Downloads: {e}"), cx),
                Err(_) => this.alert("Could not open ~/Downloads", cx),
            });
        })
        .detach();
    }

    fn alert(&self, text: &str, cx: &mut Context<Self>) {
        let text = text.to_owned();
        let _ = self.shell.update(cx, |s, cx| s.alert_failed(text, cx));
    }
}

/// The copies a Download hands to the queue: each item into `dir` on this Mac. A name already
/// there is asked about (`rule: None`), not replaced.
pub fn download_jobs(from: &Arc<dyn FileSource>, items: Vec<FileEntry>, dir: &str) -> Vec<CopyJob> {
    let mac: Arc<dyn FileSource> = Arc::new(LocalFs::new());
    items
        .into_iter()
        .map(|item| CopyJob {
            from: from.clone(),
            item,
            to: mac.clone(),
            dir: dir.to_owned(),
            rule: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: format!("/srv/{name}"),
            is_dir: dir,
            is_symlink: false,
            size: 1,
            modified: None,
        }
    }

    #[test]
    fn a_download_copies_each_item_from_the_pane_to_the_mac_and_asks_about_clashes() {
        let host: Arc<dyn FileSource> = Arc::new(LocalFs::new());
        let jobs = download_jobs(
            &host,
            vec![entry("a.log", false), entry("logs", true)],
            "/Users/me/Downloads",
        );
        assert_eq!(jobs.len(), 2);
        for job in &jobs {
            assert!(
                Arc::ptr_eq(&job.from, &host),
                "read over the pane's own source"
            );
            assert_eq!(job.to.id(), LocalFs::new().id(), "written on this Mac");
            assert_eq!(job.dir, "/Users/me/Downloads");
            assert!(job.rule.is_none(), "a taken name is asked, not replaced");
        }
        assert_eq!(jobs[1].item.name, "logs");
    }

    #[test]
    fn dotfiles_are_hidden_until_asked_for_and_rows_count_only_what_is_shown() {
        let mut l = Listing::new(1);
        l.entries = vec![
            entry(".env", false),
            entry("app", true),
            entry("a.txt", false),
        ];
        l.show_hidden(false);
        assert_eq!(l.visible, [1, 2]);
        assert_eq!(l.row(0).map(|e| e.name.as_str()), Some("app"));
        assert_eq!(l.paths(), ["/srv/app", "/srv/a.txt"]);
        l.show_hidden(true);
        assert_eq!(l.visible, [0, 1, 2]);
        assert!(l.row(3).is_none());
    }
}
