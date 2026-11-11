// Adapted from zeron crates/ui/src/popover.rs (popover_card, menu_row) (MIT).
//! "Port forwards…" on a tab: the forwards running on that session, each with Stop, and a line
//! to add another (`-L 8080:db:5432`, `-R …`, `-D 1080`) while connected.

use gpui::actions;
use gpui::{
    AnyElement, AppContext, Context, Entity, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Pixels, Point, SharedString, StatefulInteractiveElement, Styled,
    Window, anchored, deferred, div, px,
};
use tern_ssh::Forward;

use super::broadcast_ui::TabId;
use super::{Shell, ToastKind};
use crate::forward_spec;
use crate::keymap::{self, Keymap, ShortcutId};
use crate::session::Status;
use crate::session::{Session, SessionNote};
use crate::text_input::{InputColors, TextInput};

actions!(tern, [FillPassword, ClearTerminal]);

/// The popovers and side panels a tab can have open.
#[derive(Default)]
pub(super) struct Panels {
    pub forwards: Option<ForwardsPanel>,
    pub sftp: Option<super::sftp_ui::SftpPanel>,
}

pub(super) struct ForwardsPanel {
    tab: TabId,
    position: Point<Pixels>,
    input: Entity<TextInput>,
    error: Option<String>,
}

/// What a running forward is shown as: its spec, and the port it really got when that was
/// asked to be chosen (`-R 0:…`).
fn describe(forward: &Forward, bound_port: u16) -> String {
    let asked = match forward {
        Forward::Local { bind_port, .. }
        | Forward::Remote { bind_port, .. }
        | Forward::Dynamic { bind_port, .. } => *bind_port,
    };
    if asked == bound_port {
        forward.to_string()
    } else {
        format!("{forward}  (port {bound_port})")
    }
}

impl Shell {
    pub(crate) fn open_forwards_panel(
        &mut self,
        ix: usize,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(ix).map(|t| t.id) else {
            return;
        };
        let t = self.theme;
        let colors = InputColors {
            text: t.text,
            placeholder: t.faint,
            cursor: t.accent,
            selection: t.accent.opacity(0.35),
        };
        let input = cx.new(|cx| {
            TextInput::new(
                "-L 8080:db:5432  (-R, -D too)".to_owned(),
                false,
                colors,
                cx,
            )
        });
        // The panel re-renders as the person types so a bad spec's message can clear.
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        window.focus(&input.focus_handle(cx), cx);
        self.panels.forwards = Some(ForwardsPanel {
            tab,
            position,
            input,
            error: None,
        });
        cx.notify();
    }

    pub(super) fn close_forwards_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panels.forwards.take().is_some() {
            self.restore_focus(window, cx);
        }
    }

    fn add_panel_forward(&mut self, cx: &mut Context<Self>) {
        let Some(panel) = self.panels.forwards.as_mut() else {
            return;
        };
        let text = panel.input.read(cx).text().to_owned();
        if text.trim().is_empty() {
            return;
        }
        let forward = match forward_spec::parse(&text) {
            Ok(f) => f,
            Err(e) => {
                panel.error = Some(e);
                return cx.notify();
            }
        };
        let Some(session) = self
            .tabs
            .iter()
            .find(|t| t.id == panel.tab)
            .map(|t| t.session().clone())
        else {
            return;
        };
        if session.read(cx).status() != Status::Connected {
            panel.error = Some("Not connected.".into());
            return cx.notify();
        }
        panel.error = None;
        panel
            .input
            .update(cx, |i, cx| i.set_text(SharedString::default(), cx));
        session.update(cx, |s, cx| s.add_forward(forward, cx));
    }

    fn on_forwards_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match e.keystroke.key.as_str() {
            "escape" => self.close_forwards_panel(window, cx),
            "enter" => self.add_panel_forward(cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    pub(super) fn render_panels(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        [
            self.render_forwards_panel(cx),
            self.render_sftp_panel(window, cx),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    fn render_forwards_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.panels.forwards.as_ref()?;
        let t = self.theme;
        let tab = self.tabs.iter().find(|tab| tab.id == panel.tab)?;
        let label = tab.title.clone().unwrap_or_else(|| tab.alias.clone());
        let session = tab.session().clone();
        let running = session.read(cx).active_forwards();
        let mut card = div()
            .id("forwards-panel")
            .occlude()
            .w(px(340.))
            .p(px(6.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(12.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .shadow_lg()
            .text_size(px(13.))
            .text_color(t.text)
            .on_key_down(cx.listener(Self::on_forwards_key))
            .on_mouse_down_out(cx.listener(|s, _, w, cx| s.close_forwards_panel(w, cx)))
            .child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_size(px(11.))
                    .text_color(t.muted)
                    .child(SharedString::from(format!("Port forwards on {label}"))),
            );
        if running.is_empty() {
            card = card.child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .text_color(t.muted)
                    .child("None running"),
            );
        }
        for (ix, info) in running.iter().enumerate() {
            let id = info.id;
            let session = session.clone();
            card = card.child(
                div()
                    .id(("forward-row", ix))
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(7.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(describe(&info.forward, info.bound_port))),
                    )
                    .child(
                        div()
                            .id(("forward-stop", ix))
                            .flex_none()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .text_color(t.danger)
                            .hover(|s| s.bg(t.row_active))
                            .on_click(move |_, _, cx| {
                                session.update(cx, |s, cx| s.stop_forward(id, cx))
                            })
                            .child("Stop"),
                    ),
            );
        }
        card = card.child(
            div()
                .mt(px(4.))
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(30.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(px(8.))
                        .border_1()
                        .border_color(t.border)
                        .bg(t.row_hover)
                        .overflow_hidden()
                        .child(panel.input.clone()),
                )
                .child(
                    super::connections_ui::button("forward-add", "Add", true, &t)
                        .on_click(cx.listener(|s, _, _, cx| s.add_panel_forward(cx))),
                ),
        );
        if let Some(e) = &panel.error {
            card = card.child(
                div()
                    .px(px(8.))
                    .text_size(px(11.))
                    .text_color(t.danger)
                    .child(SharedString::from(e.clone())),
            );
        }
        Some(
            deferred(
                anchored()
                    .position(panel.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(card),
            )
            .priority(2)
            .into_any_element(),
        )
    }
}

// "Fill password": the shortcut and the toasts sessions raise about it; what it does is in
// `session_fill.rs`.

impl Shell {
    /// ⌘\ (by default): fill the password into the focused pane, if it is at a prompt.
    pub(super) fn fill_password(&mut self, cx: &mut Context<Self>) {
        // A form or the Settings page has the keyboard; the pane behind it is not the target.
        if self.settings_page.is_some() || self.form.is_some() {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let session = tab.session().clone();
        session.update(cx, |s, cx| s.fill_password(cx));
    }

    /// ⌘K: clears the focused pane's scrollback and screen, keeping the prompt line.
    pub(super) fn clear_terminal(&mut self, cx: &mut Context<Self>) {
        if self.settings_page.is_some() || self.form.is_some() {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let terminal = tab.session().read(cx).view.read(cx).terminal().clone();
        terminal.update(cx, |t, cx| {
            t.clear();
            cx.notify();
        });
    }

    /// What a session reports on its own: a forward that could not start, a password prompt
    /// worth a hint, or a line about a fill.
    pub(super) fn on_session_note(
        &mut self,
        session: &Entity<Session>,
        note: &SessionNote,
        cx: &mut Context<Self>,
    ) {
        match note {
            SessionNote::ForwardFailed { forward, error } => self.notify_toast(
                ToastKind::Critical,
                format!("Port forward {forward} failed: {error}"),
                cx,
            ),
            SessionNote::PasswordPrompt => {
                let in_front = self.settings_page.is_none()
                    && self
                        .tabs
                        .get(self.active)
                        .is_some_and(|t| t.session() == session);
                let combo = cx.global::<Keymap>().combo(ShortcutId::FillPassword);
                // Unbound, there is no shortcut to name.
                if in_front && !combo.is_empty() {
                    let text = format!("{} fills the password", keymap::badge(combo));
                    self.notify_toast(ToastKind::Default, text, cx);
                }
            }
            SessionNote::Message(text) => self.notify_toast(ToastKind::Default, text.clone(), cx),
            SessionNote::Problem(text) => self.notify_toast(ToastKind::Critical, text.clone(), cx),
        }
    }
}
