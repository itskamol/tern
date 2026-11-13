//! The Files panel's changes to the folder shown: rename a row in place, delete rows after they
//! ask, and upload what is dropped from Finder. Each change runs on the pane's own connection
//! and lists the folder again; what waits for the person is the model's (`Edit`).

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext as _, Context, Focusable as _, Window};
use tern_ssh::{Call, CopyJob, FileEntry, FileSource, LocalFs};

use crate::files_panel::FilesPanel;
use crate::files_panel_model::Edit;
use crate::files_tab_copy::{copy_label, local_entry};
use crate::files_tab_edit::plain_name;
use crate::text_input::TextInput;
use crate::transfers::Finished;

impl FilesPanel {
    /// F2 or the Rename action: the one picked row's name turns into a field, all selected.
    pub fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.model.start_rename() else {
            return;
        };
        let colors = self.theme.input_colors();
        let input = cx.new(|cx| {
            let mut input = TextInput::new("Name", false, colors, cx);
            input.set_text(tern_ssh::file_name(&path).to_owned(), cx);
            input.select_everything(cx);
            input
        });
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.rename_input = Some(input);
        cx.notify();
    }

    /// ⌘⌫ or the Delete action: the picked rows ask in place before anything goes.
    pub fn start_delete(&mut self, cx: &mut Context<Self>) {
        if self.model.start_delete() {
            cx.notify();
        }
    }

    /// Escape or Cancel: nothing changes on the host.
    pub fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.cancel_edit();
        self.rename_input = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// Enter or the confirm button: the rename or the delete runs, then the folder is listed
    /// again.
    pub fn confirm_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.rename_input.take();
        window.focus(&self.focus, cx);
        match self.model.take_edit() {
            Some(Edit::Rename(from)) => {
                let typed = input
                    .map(|i| i.read(cx).text().trim().to_owned())
                    .unwrap_or_default();
                match plain_name(&typed) {
                    Some(name) if name != tern_ssh::file_name(&from) => {
                        let to = tern_ssh::join(&tern_ssh::parent(&from), name);
                        self.change("rename", cx, move |s| {
                            Box::pin(async move { s.rename(&from, &to).await })
                        });
                    }
                    Some(_) => {}
                    None => self.alert(&format!("{typed:?} is not a name"), cx),
                }
            }
            Some(Edit::Delete(paths)) => {
                self.change("delete", cx, move |s| {
                    Box::pin(async move {
                        for path in &paths {
                            s.delete(path).await?;
                        }
                        Ok(())
                    })
                });
            }
            None => {}
        }
        cx.notify();
    }

    /// Files dropped from Finder go into the folder shown, through the shell's transfer queue,
    /// which asks about a name that is taken.
    pub fn upload(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let entries: Vec<FileEntry> = paths.iter().filter_map(|p| local_entry(p)).collect();
        let dir = self.listing.as_ref().map(|l| l.cwd.clone());
        let (Some(to), Some(dir), false) = (self.source(), dir, entries.is_empty()) else {
            return;
        };
        let label = copy_label(&entries, &self.name());
        let jobs = upload_jobs(entries, &to, &dir);
        self.transfers.update(cx, |t, cx| t.submit(jobs, label, cx));
    }

    /// A copy into the folder shown lists it again, so an upload's files appear.
    pub(crate) fn copy_finished(&mut self, done: &Finished, cx: &mut Context<Self>) {
        let here = self.source().is_some_and(|s| s.id() == done.summary.dest)
            && self
                .listing
                .as_ref()
                .is_some_and(|l| l.cwd == done.summary.dir);
        if here {
            self.refresh(cx);
        }
    }

    /// Runs one change on the pane in view's source, then lists the folder again; a failure
    /// is an Alert.
    fn change(
        &mut self,
        what: &'static str,
        cx: &mut Context<Self>,
        work: impl FnOnce(Arc<dyn FileSource>) -> Call<'static, ()> + Send + 'static,
    ) {
        let Some(source) = self.source() else {
            return;
        };
        let rt = self.rt.clone();
        cx.spawn(async move |this, cx| {
            let done = rt.spawn(work(source)).await;
            let _ = this.update(cx, |this, cx| {
                match done {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, what, "files_panel_change_failed");
                        this.alert(&format!("Could not {what}: {e}"), cx);
                    }
                    Err(_) => this.alert(&format!("Could not {what}"), cx),
                }
                this.model.clear_picked();
                this.refresh(cx);
            });
        })
        .detach();
    }
}

/// The copies an upload hands to the queue: each dropped item from this Mac into `dir` on the
/// pane's host. A name already there is asked about (`rule: None`), not replaced.
pub fn upload_jobs(items: Vec<FileEntry>, to: &Arc<dyn FileSource>, dir: &str) -> Vec<CopyJob> {
    let mac: Arc<dyn FileSource> = Arc::new(LocalFs::new());
    items
        .into_iter()
        .map(|item| CopyJob {
            from: mac.clone(),
            item,
            to: to.clone(),
            dir: dir.to_owned(),
            rule: None,
        })
        .collect()
}

/// What a key does to the panel's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKey {
    Rename,
    Delete,
    Confirm,
    Cancel,
}

/// F2 renames; ⌘⌫ (Delete off macOS, as in Explorer and Nautilus) asks to delete; Enter and
/// Escape answer an edit that waits. Anything else is not the panel's.
pub fn edit_key(ks: &gpui::Keystroke, editing: bool) -> Option<EditKey> {
    let m = &ks.modifiers;
    let bare = !m.platform && !m.control && !m.alt && !m.shift;
    let delete = if cfg!(target_os = "macos") {
        ks.key == "backspace" && m.platform && !m.control && !m.alt && !m.shift
    } else {
        ks.key == "delete" && bare
    };
    match ks.key.as_str() {
        "f2" if bare => Some(EditKey::Rename),
        _ if delete => Some(EditKey::Delete),
        "enter" if bare && editing => Some(EditKey::Confirm),
        "escape" if bare && editing => Some(EditKey::Cancel),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn key(s: &str, editing: bool) -> Option<EditKey> {
        edit_key(&gpui::Keystroke::parse(s).unwrap(), editing)
    }

    #[test]
    fn f2_renames_and_the_platform_delete_key_asks_to_delete() {
        assert_eq!(key("f2", false), Some(EditKey::Rename));
        let delete = if cfg!(target_os = "macos") {
            "cmd-backspace"
        } else {
            "delete"
        };
        assert_eq!(key(delete, false), Some(EditKey::Delete));
        assert_eq!(
            key("backspace", false),
            None,
            "plain backspace is not delete"
        );
        assert_eq!(key("shift-f2", false), None);
    }

    #[test]
    fn enter_and_escape_answer_only_an_edit_that_waits() {
        assert_eq!(key("enter", true), Some(EditKey::Confirm));
        assert_eq!(key("escape", true), Some(EditKey::Cancel));
        assert_eq!(key("enter", false), None);
        assert_eq!(key("escape", false), None);
    }

    #[test]
    fn an_upload_asks_about_taken_names_and_lands_in_the_folder_shown() {
        let to: Arc<dyn FileSource> = Arc::new(LocalFs::new());
        let item = FileEntry {
            name: "a.txt".into(),
            path: "/tmp/a.txt".into(),
            is_dir: false,
            is_symlink: false,
            size: 1,
            modified: None,
        };
        let jobs = upload_jobs(vec![item], &to, "/srv/app");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].dir, "/srv/app");
        assert_eq!(jobs[0].rule, None);
        assert_eq!(jobs[0].item.name, "a.txt");
    }
}
