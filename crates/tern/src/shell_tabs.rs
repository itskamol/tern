//! Tab editing on the strip: renaming a title in place and dragging a tab to a new slot.

use gpui::{AppContext, Context, Entity, KeyDownEvent, Window};
use tern_ssh::ConnectSpec;

use super::Shell;
use crate::session::Launch;
use crate::tabs_store::{SavedKind, SavedTabs};
use crate::text_input::TextInput;

/// A title being edited: which tab, and the field holding the text.
pub(super) struct Rename {
    pub ix: usize,
    pub input: Entity<TextInput>,
}

impl Shell {
    pub(crate) fn is_renaming(&self, ix: usize) -> bool {
        self.renaming.as_ref().is_some_and(|r| r.ix == ix)
    }

    /// Swaps the tab's title for a field holding it, selected for typing over.
    pub(crate) fn start_rename(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let current = tab.heading().text;
        let t = self.theme;
        let colors = t.input_colors();
        let input = cx.new(|cx| {
            let mut input = TextInput::new(tab.untitled().to_owned(), false, colors, cx);
            input.set_text(current, cx);
            input.select_everything(cx);
            input
        });
        window.focus(&gpui::Focusable::focus_handle(input.read(cx), cx), cx);
        self.renaming = Some(Rename { ix, input });
        cx.notify();
    }

    /// Saves the field. An empty title, or what the tab is called anyway, clears the custom title.
    pub(crate) fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.renaming.take() else {
            return;
        };
        let typed = rename.input.read(cx).text().trim().to_owned();
        if let Some(tab) = self.tabs.get(rename.ix) {
            let title = custom_title(&typed, tab.untitled());
            self.tabs.set_title(rename.ix, title);
        }
        self.restore_focus(window, cx);
    }

    pub(crate) fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            self.restore_focus(window, cx);
        }
    }

    /// Enter saves and Escape drops the edit; true when the key was for the field.
    pub(super) fn on_rename_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.renaming.is_none() {
            return false;
        }
        match event.keystroke.key.as_str() {
            "enter" => self.commit_rename(window, cx),
            "escape" => self.cancel_rename(window, cx),
            _ => return false,
        }
        true
    }

    /// A tab was dropped on slot `to`; the tab in front stays in front.
    pub(crate) fn move_tab(
        &mut self,
        from: usize,
        to: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.renaming = None;
        self.tabs.reorder(from, to);
        self.front_tab_changed(window, cx);
    }
}

impl Shell {
    pub(crate) fn start_logging(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(session) = self.tabs.get(ix).and_then(|t| t.session().cloned()) else {
            return;
        };
        match session.update(cx, |s, cx| {
            cx.notify();
            s.start_logging()
        }) {
            // The file name, not the full path (it ran past the toast), and a way to find it.
            Ok(path) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.toast(
                    super::Toast::new(super::ToastKind::Default, format!("Logging to {name}"))
                        .action(crate::platform::REVEAL_LABEL, move |_, _, cx| {
                            cx.reveal_path(&path)
                        }),
                    cx,
                );
            }
            Err(e) => self.notify_toast(
                super::ToastKind::Critical,
                format!("Could not start the log: {e}"),
                cx,
            ),
        }
    }

    pub(crate) fn stop_logging(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(session) = self.tabs.get(ix).and_then(|t| t.session().cloned()) else {
            return;
        };
        let path = session.update(cx, |s, cx| {
            cx.notify();
            s.stop_logging()
        });
        if let Some(path) = path {
            self.notify_toast(
                super::ToastKind::Default,
                format!("Log saved to {}", path.display()),
                cx,
            );
        }
    }

    /// Reopens the last run's tabs: connections wait for Enter. A host that is gone from the
    /// lists is skipped.
    pub(crate) fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings.reopen_tabs {
            return;
        }
        let Some(saved) = crate::settings::dir().and_then(|d| SavedTabs::load(&d)) else {
            return;
        };
        let mut reopened = Vec::new();
        for saved_tab in &saved.tabs {
            let SavedKind::Ssh { alias } = &saved_tab.kind else {
                continue;
            };
            let spec = match self.hosts.iter().find(|h| &h.alias == alias) {
                Some(host) => ConnectSpec::from_host_entry(host),
                None => ConnectSpec::parse(alias),
            };
            match spec {
                Ok(spec) => {
                    let pane = self.make_pane(Launch::SshIdle(spec), alias, window, cx);
                    reopened.push((alias.clone(), saved_tab.title.clone(), pane));
                }
                Err(e) => tracing::warn!(alias, error = %e, "tab_restore_skipped"),
            }
        }
        self.tabs.restore(reopened, saved.active_slot());
        if !self.tabs.is_empty() {
            self.front_tab_changed(window, cx);
        }
    }
}

/// What a typed title means: nothing, or the name the tab has anyway, is no custom title.
fn custom_title(typed: &str, untitled: &str) -> Option<String> {
    (!typed.is_empty() && typed != untitled).then(|| typed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::custom_title;

    #[test]
    fn empty_or_unchanged_titles_are_not_custom() {
        assert_eq!(custom_title("", "prod"), None);
        assert_eq!(custom_title("prod", "prod"), None);
        assert_eq!(custom_title("prod db", "prod"), Some("prod db".into()));
    }
}
