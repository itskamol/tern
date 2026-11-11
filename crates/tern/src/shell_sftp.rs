// Adapted from zeron crates/ui/src/files/tree.rs (render_tree_row: row height, hover and
// selection washes, folder/file glyphs) (MIT).
//! "Browse files": an SFTP panel on the right of the terminal for the tab's connection. It lists
//! the remote home, folders first, and moves files: download with a double click or the button
//! (into ~/Downloads, never over a file already there), upload with the button or by dropping
//! files on the panel. The wire work is tern-ssh's `Sftp`; this is the window around it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gpui::{AppContext, Context, Entity, FocusHandle, Focusable, Window, actions};
use tern_ssh::{Progress, Sftp, SftpEntry, SftpError};

use super::broadcast_ui::TabId;
use super::{Shell, Toast, ToastKind};
use crate::sftp_paths;
use crate::text_input::TextInput;

actions!(tern, [ToggleSftp]);

pub(super) enum State {
    /// Opening the SFTP channel and reading the login directory.
    Opening,
    Loading,
    Ready,
    /// The panel cannot work on this connection (no SFTP service, session gone).
    Failed(String),
}

pub(super) struct SftpPanel {
    pub(super) tab: TabId,
    sftp: Option<Sftp>,
    pub(super) cwd: String,
    pub(super) entries: Vec<SftpEntry>,
    pub(super) state: State,
    pub(super) selected: Option<usize>,
    pub(super) path: Entity<TextInput>,
    /// The list has the keyboard when this does: arrows pick, Enter opens, Backspace goes up.
    pub(super) focus: FocusHandle,
    /// The last thing that went wrong in a listing or transfer, shown under the list.
    pub(super) notice: Option<String>,
    /// A transfer is running; one at a time keeps the progress line honest.
    pub(super) busy: bool,
    /// Answers to older listings are dropped when a newer one was asked for.
    generation: u64,
}

fn describe(e: &SftpError) -> String {
    e.to_string()
}

impl Shell {
    pub(crate) fn toggle_sftp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panels.sftp.is_some() {
            return self.close_sftp(window, cx);
        }
        if self.tabs.active_tab().is_some() && self.settings_page.is_none() {
            self.open_sftp(self.tabs.active(), window, cx);
        }
    }

    pub(crate) fn close_sftp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.panels.sftp.take() {
            if let Some(sftp) = panel.sftp {
                cx.spawn(async move |_, _| sftp.close().await).detach();
            }
            self.restore_focus(window, cx);
        }
    }

    /// Opens the panel for the tab at `ix` and lists the login directory.
    pub(crate) fn open_sftp(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let (tab_id, session) = (tab.id, tab.session().clone());
        let Some(handle) = session.read(cx).ssh_handle() else {
            return self.notify_toast(
                ToastKind::Critical,
                "Not connected: no files to browse.",
                cx,
            );
        };
        if let Some(old) = self.panels.sftp.take().and_then(|p| p.sftp) {
            cx.spawn(async move |_, _| old.close().await).detach();
        }
        let t = self.theme;
        let colors = t.input_colors();
        let path = cx.new(|cx| TextInput::new("/".to_owned(), false, colors, cx));
        cx.observe(&path, |_, _, cx| cx.notify()).detach();
        self.panels.sftp = Some(SftpPanel {
            tab: tab_id,
            sftp: None,
            cwd: String::new(),
            entries: Vec::new(),
            state: State::Opening,
            selected: None,
            path,
            focus: cx.focus_handle(),
            notice: None,
            busy: false,
            generation: 0,
        });
        if let Some(panel) = self.panels.sftp.as_ref() {
            window.focus(&panel.focus, cx);
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let opened = handle.open_sftp().await;
            let home = match &opened {
                Ok(sftp) => sftp.home().await,
                Err(e) => Err(e.clone()),
            };
            let _ = this.update(cx, |s, cx| {
                let Some(panel) = s.panels.sftp.as_mut().filter(|p| p.tab == tab_id) else {
                    return;
                };
                match (opened, home) {
                    (Ok(sftp), Ok(home)) => {
                        tracing::info!(home = %home, "sftp_open");
                        panel.sftp = Some(sftp);
                        s.sftp_go(home, cx);
                    }
                    (_, Err(e)) | (Err(e), _) => {
                        tracing::warn!(error = %e, "sftp_open_failed");
                        panel.state = State::Failed(describe(&e));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Lists `path` and makes it the current directory once the server answers.
    pub(crate) fn sftp_go(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(panel) = self.panels.sftp.as_mut() else {
            return;
        };
        let Some(sftp) = panel.sftp.clone() else {
            return;
        };
        panel.generation += 1;
        let generation = panel.generation;
        if matches!(panel.state, State::Ready) && !panel.entries.is_empty() {
            // Keep the old rows on screen until the new ones arrive; a slow server should
            // not flash an empty list.
        } else {
            panel.state = State::Loading;
        }
        panel.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let listed = sftp.list(&path).await;
            let _ = this.update(cx, |s, cx| {
                let Some(panel) = s
                    .panels
                    .sftp
                    .as_mut()
                    .filter(|p| p.generation == generation)
                else {
                    return;
                };
                match listed {
                    Ok(entries) => {
                        tracing::info!(path = %path, count = entries.len(), "sftp_list");
                        panel.cwd = path.clone();
                        panel.entries = entries;
                        panel.selected = None;
                        panel.state = State::Ready;
                        panel.path.update(cx, |i, cx| i.set_text(path, cx));
                    }
                    Err(e) => {
                        tracing::warn!(path = %path, error = %e, "sftp_list_failed");
                        panel.notice = Some(describe(&e));
                        panel.state = if panel.cwd.is_empty() {
                            State::Failed(describe(&e))
                        } else {
                            State::Ready
                        };
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn sftp_up(&mut self, cx: &mut Context<Self>) {
        if let Some(panel) = self.panels.sftp.as_ref() {
            let up = sftp_paths::parent(&panel.cwd);
            self.sftp_go(up, cx);
        }
    }

    pub(crate) fn sftp_home(&mut self, cx: &mut Context<Self>) {
        self.sftp_go(".".to_owned(), cx);
    }

    /// Enter in the path bar.
    pub(crate) fn sftp_go_typed(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.panels.sftp.as_ref() else {
            return;
        };
        let typed = panel.path.read(cx).text().trim().to_owned();
        if !typed.is_empty() {
            self.sftp_go(typed, cx);
        }
    }

    /// Moves the selection by `delta` rows, starting from the top when nothing is picked.
    pub(crate) fn sftp_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(panel) = self.panels.sftp.as_mut()
            && let Some(next) = sftp_paths::step(panel.selected, delta, panel.entries.len())
        {
            panel.selected = Some(next);
            cx.notify();
        }
    }

    pub(crate) fn sftp_open_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.panels.sftp.as_ref().and_then(|p| p.selected) {
            self.sftp_open_entry(ix, cx);
        }
    }

    pub(crate) fn sftp_select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(panel) = self.panels.sftp.as_mut() {
            panel.selected = Some(ix);
            cx.notify();
        }
    }

    /// Double click: a folder is entered, a file is downloaded.
    pub(crate) fn sftp_open_entry(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self
            .panels
            .sftp
            .as_ref()
            .and_then(|p| p.entries.get(ix).cloned())
        else {
            return;
        };
        if entry.is_dir {
            self.sftp_go(entry.path, cx);
        } else {
            self.sftp_download(entry, cx);
        }
    }

    pub(crate) fn sftp_download_selected(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self
            .panels
            .sftp
            .as_ref()
            .and_then(|p| p.selected.and_then(|ix| p.entries.get(ix).cloned()))
            .filter(|e| !e.is_dir)
        else {
            return;
        };
        self.sftp_download(entry, cx);
    }

    fn sftp_download(&mut self, entry: SftpEntry, cx: &mut Context<Self>) {
        let Some(panel) = self.panels.sftp.as_mut() else {
            return;
        };
        let Some(sftp) = panel.sftp.clone().filter(|_| !panel.busy) else {
            return;
        };
        let Some(dir) = std::env::home_dir().map(|h| h.join("Downloads")) else {
            return self.notify_toast(ToastKind::Critical, "No Downloads folder to save into.", cx);
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return self.notify_toast(ToastKind::Critical, format!("{}: {e}", dir.display()), cx);
        }
        let Some(dest) = sftp_paths::free_name(&dir, &entry.name, |p| p.exists()) else {
            return self.notify_toast(
                ToastKind::Critical,
                format!("Not downloaded: {:?} is not a plain file name.", entry.name),
                cx,
            );
        };
        panel.busy = true;
        let label = entry.name.clone();
        self.run_transfer(
            format!("Downloading {label}"),
            move |progress| {
                let (sftp, remote, dest) = (sftp, entry.path, dest.clone());
                async move {
                    sftp.download(&remote, &dest, progress).await?;
                    Ok(dest)
                }
            },
            move |s, result, cx| match result {
                Ok(path) => s.transfer_done(format!("Downloaded {label}"), Some(path), cx),
                Err(e) => s.transfer_failed(format!("Download failed: {e}"), cx),
            },
            cx,
        );
    }

    pub(crate) fn sftp_pick_upload(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Upload".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |s, cx| s.sftp_upload(paths, cx));
            }
        })
        .detach();
    }

    /// Uploads files (from the button or dropped on the panel) into the current directory, one
    /// after the other. Folders are refused by name rather than half done.
    pub(crate) fn sftp_upload(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(panel) = self.panels.sftp.as_mut() else {
            return;
        };
        let Some(sftp) = panel.sftp.clone() else {
            return;
        };
        if panel.busy {
            panel.notice = Some("A transfer is already running.".into());
            return cx.notify();
        }
        let (files, folders): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| !p.is_dir());
        if let Some(dir) = folders.first() {
            panel.notice = Some(format!(
                "{} is a folder; only files can be uploaded.",
                dir.display()
            ));
        }
        if files.is_empty() {
            return cx.notify();
        }
        panel.busy = true;
        let cwd = panel.cwd.clone();
        let count = files.len();
        self.run_transfer(
            "Uploading".to_owned(),
            move |progress| {
                let progress = Arc::new(std::sync::Mutex::new(progress));
                async move {
                    let mut last = PathBuf::new();
                    for file in files {
                        let name = file
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let remote = sftp_paths::remote_join(&cwd, &name);
                        let progress = progress.clone();
                        sftp.upload(&file, &remote, move |p| {
                            if let Ok(mut f) = progress.lock() {
                                f(p);
                            }
                        })
                        .await?;
                        last = file;
                    }
                    Ok(last)
                }
            },
            move |s, result, cx| match result {
                Ok(_) => {
                    let what = if count == 1 {
                        "Uploaded 1 file".to_owned()
                    } else {
                        format!("Uploaded {count} files")
                    };
                    s.transfer_done(what, None, cx);
                    if let Some(cwd) = s.panels.sftp.as_ref().map(|p| p.cwd.clone()) {
                        s.sftp_go(cwd, cx);
                    }
                }
                Err(e) => s.transfer_failed(format!("Upload failed: {e}"), cx),
            },
            cx,
        );
    }

    /// Runs `work` and keeps one snackbar current with its progress; `done` gets the result on
    /// the UI thread. The progress callback runs on the session's thread, so it only writes two
    /// numbers that a timer here reads.
    fn run_transfer<F, Fut>(
        &mut self,
        label: String,
        work: impl FnOnce(Box<dyn FnMut(Progress) + Send>) -> Fut + 'static,
        done: F,
        cx: &mut Context<Self>,
    ) where
        Fut: std::future::Future<Output = Result<PathBuf, SftpError>> + Send + 'static,
        F: FnOnce(&mut Shell, Result<PathBuf, SftpError>, &mut Context<Shell>) + 'static,
    {
        let (moved, total) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let finished = Arc::new(AtomicBool::new(false));
        let shared = (moved.clone(), total.clone());
        let fut = work(Box::new(move |p| {
            shared.0.store(p.done, Ordering::Relaxed);
            shared.1.store(p.total, Ordering::Relaxed);
        }));
        self.progress_toast(format!("{label}…"), cx);
        let ticker_done = finished.clone();
        cx.spawn(async move |this, cx| {
            while !ticker_done.load(Ordering::Relaxed) {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                let pct = sftp_paths::percent(
                    moved.load(Ordering::Relaxed),
                    total.load(Ordering::Relaxed),
                );
                if this
                    .update(cx, |s, cx| {
                        if !ticker_done.load(Ordering::Relaxed) {
                            s.progress_toast(format!("{label}… {pct}%"), cx)
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(fut).await;
            finished.store(true, Ordering::Relaxed);
            let _ = this.update(cx, |s, cx| {
                if let Some(panel) = s.panels.sftp.as_mut() {
                    panel.busy = false;
                }
                s.end_progress_toast(cx);
                done(s, result, cx);
            });
        })
        .detach();
    }

    fn transfer_done(&mut self, message: String, saved: Option<PathBuf>, cx: &mut Context<Self>) {
        tracing::info!(message = %message, "sftp_transfer_done");
        let mut toast = Toast::new(ToastKind::Positive, message);
        if let Some(path) = saved {
            toast = toast.action(crate::platform::REVEAL_LABEL, move |_, _, cx| {
                cx.reveal_path(&path)
            });
        }
        self.toast(toast, cx);
    }

    fn transfer_failed(&mut self, message: String, cx: &mut Context<Self>) {
        tracing::warn!(message = %message, "sftp_transfer_failed");
        if let Some(panel) = self.panels.sftp.as_mut() {
            panel.notice = Some(message.clone());
        }
        self.notify_toast(ToastKind::Critical, message, cx);
    }

    /// Gives the path bar the keyboard, so typing there reaches it and Enter goes.
    pub(crate) fn focus_sftp_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.panels.sftp.as_ref() {
            window.focus(&panel.path.focus_handle(cx), cx);
        }
    }
}

#[path = "shell_sftp_view.rs"]
mod view;
