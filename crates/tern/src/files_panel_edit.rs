//! The Files panel's changes to the folder shown: rename a row in place, delete rows after they
//! ask, and upload what is dropped from Finder. Each change runs on the pane's own connection
//! and lists the folder again; what waits for the person is the model's (`Edit`).

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{AppContext as _, Context, Focusable as _, Window};
use tern_ssh::{Call, FileEntry, FileSource, LocalFs};

use crate::files_change::change_then;
use crate::files_panel::FilesPanel;
use crate::files_panel_model::Edit;
use crate::files_panel_send::SendTo;
use crate::files_tab_copy::local_entry;
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

    /// The rename field goes once no rename waits (it ended by a click elsewhere, a new folder,
    /// another pane, a reconnect), and the keyboard comes back to the panel with it.
    pub(crate) fn settle_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let renaming = matches!(self.model.edit(), Some(Edit::Rename(_)));
        if !renaming && self.rename_input.take().is_some() {
            window.focus(&self.focus, cx);
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
        // Into the folder shown, which waits until it is listed: before that there is none.
        let mac: Arc<dyn FileSource> = Arc::new(LocalFs::new());
        self.send_entries(mac, entries, SendTo::Here, false, cx);
    }

    /// A copy into the folder shown, or a move out of it, lists it again, so an upload's
    /// files appear and a moved one goes.
    /// Not while a rename or delete waits: listing again would end it mid-answer, and its own
    /// confirm lists the folder anyway.
    pub(crate) fn copy_finished(&mut self, done: &Finished, cx: &mut Context<Self>) {
        let here = self.model.edit().is_none()
            && self.source().is_some_and(|s| {
                self.listing
                    .as_ref()
                    .is_some_and(|l| done.touched(s.id(), &l.cwd))
            });
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
            change_then(&rt, source, what, work, |failure| {
                let _ = this.update(cx, |this, cx| {
                    if let Some(text) = failure {
                        this.alert(&text, cx);
                    }
                    this.model.clear_picked();
                    this.refresh(cx);
                });
            })
            .await;
        })
        .detach();
    }
}

/// What a key does to the panel's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKey {
    Rename,
    Delete,
    Confirm,
    Cancel,
    /// ⌘C: the picked rows go on tern's clipboard.
    Copy,
    /// ⌘V: tern's clipboard is pasted into the folder shown.
    Paste,
}

/// F2 renames; ⌘⌫ (Delete off macOS, as in Explorer and Nautilus) asks to delete; Enter and
/// Escape answer an edit that waits; ⌘C and ⌘V (Ctrl off macOS) copy and paste rows, but not
/// while an edit waits, when they are the rename field's. Anything else is not the panel's.
pub fn edit_key(ks: &gpui::Keystroke, editing: bool) -> Option<EditKey> {
    let m = &ks.modifiers;
    let bare = !m.platform && !m.control && !m.alt && !m.shift;
    let delete = if cfg!(target_os = "macos") {
        ks.key == "backspace" && m.platform && !m.control && !m.alt && !m.shift
    } else {
        ks.key == "delete" && bare
    };
    // The platform's own modifier alone: ⌘ on a Mac, Ctrl elsewhere.
    let command = if cfg!(target_os = "macos") {
        m.platform && !m.control
    } else {
        m.control && !m.platform
    } && !m.alt
        && !m.shift;
    match ks.key.as_str() {
        "c" if command && !editing => Some(EditKey::Copy),
        "v" if command && !editing => Some(EditKey::Paste),
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
    fn the_platform_copy_and_paste_keys_carry_rows_unless_a_rename_has_them() {
        let (copy, paste) = if cfg!(target_os = "macos") {
            ("cmd-c", "cmd-v")
        } else {
            ("ctrl-c", "ctrl-v")
        };
        assert_eq!(key(copy, false), Some(EditKey::Copy));
        assert_eq!(key(paste, false), Some(EditKey::Paste));
        assert_eq!(key(copy, true), None, "the rename field copies its text");
        assert_eq!(key(paste, true), None);
        assert_eq!(key("c", false), None);
        assert_eq!(key("shift-cmd-c", false), None);
        assert_eq!(key("alt-cmd-v", false), None);
        assert_eq!(
            key("ctrl-cmd-c", false),
            None,
            "both ⌘ and Ctrl is another chord"
        );
    }

    #[test]
    fn enter_and_escape_answer_only_an_edit_that_waits() {
        assert_eq!(key("enter", true), Some(EditKey::Confirm));
        assert_eq!(key("escape", true), Some(EditKey::Cancel));
        assert_eq!(key("enter", false), None);
        assert_eq!(key("escape", false), None);
    }
}
