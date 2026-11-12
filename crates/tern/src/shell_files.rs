//! The shell's side of the Files tab: opening it, telling it how to reach a host, and what has to
//! be answered wherever the person is, because copies outlive the tab: a name that is taken, a
//! batch that ended, and quitting while copies run.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, Context, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, anchored, deferred, div, point, px,
};
use tern_ssh::{Conflict, ConnectSpec};

use super::{Content, Pane, Shell, Toast, ToastKind};
use crate::files_tab::{FilesView, HostAccess};
use crate::shell::connections_ui::button;
use crate::statusline;
use crate::transfers::{self, Finished};

/// The first live connection among `sessions` (label, connection) whose label is `label`: a
/// second tab to the host still serves the panel when the first one is closed.
fn connected_to<T>(
    label: &str,
    sessions: impl IntoIterator<Item = (String, Option<T>)>,
) -> Option<T> {
    sessions
        .into_iter()
        .find_map(|(l, handle)| handle.filter(|_| l == label))
}

/// The Files tab's name in the strip.
const FILES_ALIAS: &str = "Files";

impl Shell {
    /// ⌘⇧O: the Files tab, which opens with the Mac on the left and the host of tab `from` (the
    /// tab in front when none is named) on the right. One is enough: a second request brings
    /// the first to the front.
    pub(crate) fn open_files_tab(
        &mut self,
        from: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.tabs.iter().position(|t| !t.is_session()) {
            return self.show_tab(ix, window, cx);
        }
        let from = from.unwrap_or(self.tabs.active());
        let right = self
            .tabs
            .get(from)
            .filter(|t| t.is_session())
            .map(|t| t.alias.clone())
            .and_then(|alias| {
                let access = self.host_access(&alias, cx)?;
                Some((alias, access))
            });
        let (hosts, transfers, theme) = (self.hosts.clone(), self.transfers.clone(), self.theme);
        let shell = cx.weak_entity();
        let view = cx.new(|cx| FilesView::new(hosts, right, transfers, shell, theme, cx));
        let pane = Pane {
            id: self.tabs.new_pane_id(),
            host: FILES_ALIAS.into(),
            content: Content::Files(view),
        };
        self.tabs.open(FILES_ALIAS.into(), pane);
        self.error = None;
        self.settings_page = None;
        self.front_tab_changed(window, cx);
    }

    /// What a host panel needs to open a connection to `alias`: how to dial it and log in, and
    /// the connection of an open session to the same host when there is one.
    pub(crate) fn host_access(&self, alias: &str, cx: &App) -> Option<HostAccess> {
        let spec = match self.hosts.iter().find(|h| h.alias == alias) {
            Some(host) => ConnectSpec::from_host_entry(host),
            None => ConnectSpec::parse(alias),
        }
        .ok()?;
        let label = statusline::host_label(&spec.user, &spec.host, spec.port);
        let session = connected_to(
            &label,
            self.tabs
                .iter()
                .filter_map(|t| t.session())
                .map(|s| (s.read(cx).label(), s.read(cx).ssh_handle())),
        );
        Some(HostAccess {
            auth: self.auth_for(alias),
            spec,
            session,
            label,
        })
    }

    /// The Files tabs follow the theme.
    pub(super) fn retheme_files(&self, cx: &mut Context<Self>) {
        let theme = self.theme;
        for pane in self.tabs.iter().flat_map(|t| t.panes()) {
            if let Content::Files(view) = &pane.content {
                view.update(cx, |v, cx| v.set_theme(theme, cx));
            }
        }
    }

    /// A batch ended, in view or not.
    pub(super) fn on_transfer_finished(&mut self, done: &Finished, cx: &mut Context<Self>) {
        let (ok, text) = transfers::outcome(&done.label, &done.summary);
        tracing::info!(
            copied = done.summary.copied,
            failed = done.summary.failed.len(),
            cancelled = done.summary.cancelled,
            "transfer_finished"
        );
        if ok {
            self.activity(text, cx);
        } else {
            self.alert(Toast::new(ToastKind::Critical, text), cx);
        }
    }

    /// Copies are running or waiting.
    pub(super) fn copies_running(&self, cx: &App) -> bool {
        self.transfers.read(cx).busy()
    }

    /// Quit (⌘Q): asks first when copies would be cut off.
    pub(crate) fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self.copies_running(cx) {
            self.quit_ask = true;
            cx.notify();
        } else {
            cx.quit();
        }
    }

    /// The window's close button: the same question, and the window stays until it is answered.
    pub(crate) fn close_allowed(&mut self, cx: &mut Context<Self>) -> bool {
        if self.copies_running(cx) {
            self.quit_ask = true;
            cx.notify();
            return false;
        }
        true
    }

    /// Cancels the copies and quits once they have taken their `.part` files away (a second or
    /// two at most), so quitting leaves no half-written file behind.
    fn quit_after_stopping(&mut self, cx: &mut Context<Self>) {
        self.transfers.update(cx, |t, _| t.cancel_all());
        cx.spawn(async move |this, cx| {
            for _ in 0..60 {
                let busy = this
                    .update(cx, |s, cx| s.copies_running(cx))
                    .unwrap_or(false);
                if !busy {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    fn sheet(&self, window: &Window, card: impl IntoElement) -> AnyElement {
        let t = self.theme;
        let viewport = window.viewport_size();
        deferred(
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    .bg(t.scrim(0.35))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(card),
            ),
        )
        .priority(3)
        .into_any_element()
    }

    /// A name that is taken, asked wherever the person is.
    pub(super) fn render_conflict_sheet(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let t = self.theme;
        let transfers = self.transfers.read(cx);
        let (question, label) = transfers.question()?;
        let what = if question.is_dir {
            "A folder"
        } else {
            "A file"
        };
        let name = question.name.clone();
        let label = label.to_owned();
        let all = self.conflict_all;
        let choose = |id: &'static str, text: &'static str, choice: Conflict, primary: bool| {
            button(id, text, primary, &t).on_click(cx.listener(move |s, _, _, cx| {
                let all = s.conflict_all;
                s.conflict_all = false;
                s.transfers.update(cx, |t, cx| t.answer(choice, all, cx));
                cx.notify();
            }))
        };
        let skip = choose("conflict-skip", "Skip", Conflict::Skip, false);
        let both = choose("conflict-both", "Keep both", Conflict::KeepBoth, false);
        let replace = choose("conflict-replace", "Replace", Conflict::Replace, true);
        let card = div()
            .id("conflict-sheet")
            .w(px(420.))
            .p(px(18.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .rounded(px(14.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .text_color(t.text)
            .child(div().text_size(px(14.)).child(SharedString::from(format!(
                "{what} named \u{201c}{name}\u{201d} is already there"
            ))))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child(SharedString::from(format!("Copying {label}"))),
            )
            .child(
                div()
                    .id("conflict-all")
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .on_click(cx.listener(|s, _, _, cx| {
                        s.conflict_all = !s.conflict_all;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(14.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(if all { t.accent } else { t.border })
                            .when(all, |el| el.bg(t.accent)),
                    )
                    .child("Apply to all the rest of this copy"),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(skip)
                    .child(both)
                    .child(replace),
            );
        Some(self.sheet(window, card))
    }

    /// Quitting while copies run.
    pub(super) fn render_quit_sheet(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.quit_ask {
            return None;
        }
        let t = self.theme;
        let card = div()
            .id("quit-sheet")
            .w(px(420.))
            .p(px(18.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .rounded(px(14.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .text_color(t.text)
            .child(div().text_size(px(14.)).child("Files are still being copied"))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child("Quitting now stops the copies. Files copied so far stay where they are; the one in progress is removed."),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(button("quit-stay", "Keep copying", true, &t).on_click(
                        cx.listener(|s, _, _, cx| {
                            s.quit_ask = false;
                            cx.notify();
                        }),
                    ))
                    .child(button("quit-go", "Quit", false, &t).on_click(
                        cx.listener(|s, _, _, cx| s.quit_after_stopping(cx)),
                    )),
            );
        Some(self.sheet(window, card))
    }
}

#[cfg(test)]
mod tests {
    use super::connected_to;

    #[test]
    fn a_panel_uses_the_connection_of_any_open_session_to_its_host() {
        let tabs = [
            ("me@web:22".to_owned(), None),
            ("me@db:22".to_owned(), Some(1)),
            ("me@web:22".to_owned(), Some(2)),
        ];
        assert_eq!(connected_to("me@web:22", tabs.clone()), Some(2));
        assert_eq!(connected_to("me@cache:22", tabs), None);
    }
}
