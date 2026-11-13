//! The Files tab's changes to a panel's folder: new folder, rename and delete, each asked in a
//! dialog and run on the panel's source.

use std::sync::Arc;

use gpui::{AppContext, Context, Entity, Focusable, Window};
use tern_ssh::{Call, FileSource};

use crate::files_tab::{Dialog, FilesView, State};
use crate::text_input::TextInput;

impl FilesView {
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
}

/// A name for something in a folder: no empty names, no paths.
pub(crate) fn plain_name(name: &str) -> Option<&str> {
    (!matches!(name, "" | "." | "..") && !name.contains(['/', '\0'])).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_name_is_one_plain_name() {
        assert_eq!(plain_name("docs"), Some("docs"));
        for bad in ["", ".", "..", "a/b", "../x", "a\0"] {
            assert_eq!(plain_name(bad), None, "{bad:?}");
        }
    }
}
