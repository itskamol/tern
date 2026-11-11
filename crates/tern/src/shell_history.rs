// Adapted from zeron crates/ui/src/shell/command_palette.rs (palette card, overlay) (MIT).
//! The command history window (⌘R): the commands run on the focused pane's host, newest first,
//! fuzzy-searched with the host picker's matcher. ↵ puts the line at the prompt, ⌘↵ also runs
//! it, ⌫ deletes it from the history, ⌘A switches between this host and all hosts, Esc closes.
//! Settings → Command history clears one host's history or all of it.
//!
//! ⌫ deletes, so the query is edited with ⌥⌫ (one character) and ⌃U (all of it). The matches
//! are found when the query, the scope or the history changes, never while drawing.

use crate::hover::HoverFade as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, BorrowAppContext as _, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    Window, actions, anchored, deferred, div, point, px,
};

use super::{Shell, ToastKind};
use crate::command_history::{CommandHistory, Entry, Scope};
use crate::picker;
use crate::settings_widgets as w;
use crate::text_input::SelectAll;

actions!(tern, [ToggleCommandHistory]);

/// Rows drawn; the search still covers everything, so a query reaches older lines.
const ROWS: usize = 200;

#[derive(Default)]
pub(super) struct HistoryUi {
    window: Option<HistoryWindow>,
    /// The clear button waiting for a second press, in Settings.
    confirm: Option<Clear>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Clear {
    Host(String),
    All,
}

struct HistoryWindow {
    /// The focused pane's host.
    alias: String,
    all: bool,
    query: String,
    active: usize,
    /// What the query finds, newest first or best first.
    entries: Vec<Entry>,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

/// What a keystroke does in the open window.
#[derive(Debug, PartialEq, Eq)]
enum Key {
    Move(isize),
    Put { run: bool },
    Delete,
    ToggleAll,
    Close,
    Erase,
    ClearQuery,
    Type(String),
    Ignore,
}

fn classify(event: &KeyDownEvent) -> Key {
    let ks = &event.keystroke;
    let mods = &ks.modifiers;
    match ks.key.as_str() {
        "up" => Key::Move(-1),
        "down" => Key::Move(1),
        "p" if mods.control => Key::Move(-1),
        "n" if mods.control => Key::Move(1),
        "u" if mods.control => Key::ClearQuery,
        "a" if mods.platform => Key::ToggleAll,
        "enter" => Key::Put { run: mods.platform },
        "escape" => Key::Close,
        "backspace" if mods.alt => Key::Erase,
        // A held key would delete a screenful of lines.
        "backspace" if event.is_held => Key::Ignore,
        "backspace" => Key::Delete,
        _ if mods.platform || mods.control => Key::Ignore,
        _ => match &ks.key_char {
            Some(text) if !text.chars().any(char::is_control) => Key::Type(text.clone()),
            _ => Key::Ignore,
        },
    }
}

/// How long ago `at` was, as the row shows it: `now`, `5m`, `3h`, `2d`, `6w`.
fn ago(now: u64, at: u64) -> String {
    let secs = now.saturating_sub(at);
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=1_209_599 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

impl Shell {
    pub(super) fn toggle_command_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.window.is_some() {
            return self.close_command_history(window, cx);
        }
        let alias = match self.tabs.active_tab() {
            Some(tab) if self.settings_page.is_none() => tab.session().read(cx).alias().to_owned(),
            _ => {
                return self.notify_toast(
                    ToastKind::Default,
                    "Open a session to see its command history",
                    cx,
                );
            }
        };
        self.picker = None;
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.history.window = Some(HistoryWindow {
            alias,
            all: false,
            query: String::new(),
            active: 0,
            entries: Vec::new(),
            focus,
            scroll: ScrollHandle::new(),
        });
        self.history_search(cx);
        cx.notify();
    }

    fn close_command_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.window.take().is_some() {
            self.restore_focus(window, cx);
        }
    }

    /// Finds the entries again for the query and scope the window has.
    fn history_search(&mut self, cx: &mut Context<Self>) {
        let Some(win) = self.history.window.as_mut() else {
            return;
        };
        win.entries = if cx.has_global::<CommandHistory>() {
            let scope = if win.all {
                Scope::All
            } else {
                Scope::Host(&win.alias)
            };
            cx.global::<CommandHistory>().search(scope, &win.query)
        } else {
            Vec::new()
        };
        win.active = win.active.min(win.entries.len().saturating_sub(1));
    }

    fn on_history_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = classify(event);
        if key == Key::Ignore {
            return;
        }
        cx.stop_propagation();
        let Some(win) = self.history.window.as_mut() else {
            return;
        };
        let shown = win.entries.len().min(ROWS);
        let active = win_active(win);
        let mut research = true;
        match key {
            Key::Move(delta) => {
                research = false;
                if shown > 0 {
                    win.active = crate::tabs::step(win.active, shown, delta);
                    win.scroll.scroll_to_item(win.active);
                }
            }
            Key::Type(text) => {
                win.query.push_str(&text);
                win.active = 0;
            }
            Key::Erase => {
                win.query.pop();
                win.active = 0;
            }
            Key::ClearQuery => {
                win.query.clear();
                win.active = 0;
            }
            Key::ToggleAll => return self.history_toggle_scope(cx),
            Key::Put { run } => return self.put_history_line(active, run, window, cx),
            Key::Delete => return self.delete_history_line(cx),
            Key::Close => return self.close_command_history(window, cx),
            Key::Ignore => research = false,
        }
        if research {
            self.history_search(cx);
        }
        cx.notify();
    }

    fn history_toggle_scope(&mut self, cx: &mut Context<Self>) {
        if let Some(win) = self.history.window.as_mut() {
            win.all = !win.all;
            win.active = 0;
            self.history_search(cx);
            cx.notify();
        }
    }

    /// Types entry `ix` at the focused pane's prompt, and presses Enter when `run`.
    fn put_history_line(
        &mut self,
        ix: usize,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line = self
            .history
            .window
            .as_ref()
            .and_then(|w| w.entries.get(ix))
            .map(|e| e.line.clone());
        self.close_command_history(window, cx);
        let (Some(line), Some(tab)) = (line, self.tabs.active_tab()) else {
            return;
        };
        let session = tab.session().clone();
        if !session.update(cx, |s, cx| s.put_at_prompt(&line, run, cx)) {
            self.notify_toast(
                ToastKind::Critical,
                "The session is not connected, so nothing was typed",
                cx,
            );
        }
    }

    fn delete_history_line(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self
            .history
            .window
            .as_ref()
            .and_then(|w| w.entries.get(w.active))
            .cloned()
        else {
            return;
        };
        let result =
            cx.update_global(|h: &mut CommandHistory, _| h.delete(&entry.host, &entry.line));
        if let Err(e) = result {
            tracing::warn!(error = %e, "command_history_delete_failed");
            self.notify_toast(ToastKind::Critical, "Could not delete that line", cx);
        }
        self.history_search(cx);
        cx.notify();
    }

    pub(super) fn render_command_history(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let win = self.history.window.as_ref()?;
        let t = &self.theme;
        let viewport = window.viewport_size();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let active = win_active(win);
        let rows = win.entries.iter().take(ROWS).enumerate().map(|(row, e)| {
            div()
                .id(("history-row", row))
                .mx(px(4.))
                .px(px(12.))
                .py(px(4.))
                .min_h(px(30.))
                .rounded(px(10.))
                .flex()
                .items_center()
                .gap(px(10.))
                .cursor_pointer()
                .hover_fade(
                    format!("history-{row}"),
                    gpui::transparent_black(),
                    t.row_active,
                )
                .when(row == active, |el| el.bg(t.row_active))
                .on_click(cx.listener(move |shell, _, window, cx| {
                    shell.put_history_line(row, false, window, cx)
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(crate::theme::MONO_FONT)
                        .text_size(px(12.5))
                        .child(SharedString::from(e.line.clone())),
                )
                .when(win.all, |el| {
                    el.child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(t.muted)
                            .child(SharedString::from(e.host.clone())),
                    )
                })
                .child(
                    div()
                        .flex_none()
                        .w(px(32.))
                        .flex()
                        .justify_end()
                        .text_xs()
                        .text_color(t.faint)
                        .child(SharedString::from(ago(now, e.at))),
                )
        });
        let query: SharedString = if win.query.is_empty() {
            "Search command history…".into()
        } else {
            win.query.clone().into()
        };
        let scope = if win.all {
            "All hosts".to_owned()
        } else {
            win.alias.clone()
        };
        let card = div()
            .id("command-history")
            .track_focus(&win.focus)
            .w(px(640.0_f32.min(f32::from(viewport.width) - 32.0)))
            .flex()
            .flex_col()
            .rounded(px(16.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .text_color(t.text)
            .on_key_down(cx.listener(Self::on_history_key))
            .on_action(cx.listener(|s, _: &SelectAll, _, cx| s.history_toggle_scope(cx)))
            .on_mouse_down_out(
                cx.listener(|shell, _, window, cx| shell.close_command_history(window, cx)),
            )
            .child(
                div()
                    .min_h(px(44.))
                    .px(px(16.))
                    .py(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .border_b_1()
                    .border_color(t.hairline)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(14.))
                            .when(win.query.is_empty(), |el| el.text_color(t.faint))
                            .child(query),
                    )
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(160.))
                            .truncate()
                            .text_xs()
                            .text_color(t.muted)
                            .child(SharedString::from(scope)),
                    )
                    .child(picker::kbd(
                        SharedString::from(crate::keymap::badge(
                            cx.global::<crate::keymap::Keymap>()
                                .combo(crate::keymap::ShortcutId::CommandHistory),
                        )),
                        t,
                    )),
            )
            .child(
                div()
                    .id("history-results")
                    .py(px(8.))
                    .max_h(px(picker::results_height(viewport)))
                    .overflow_y_scroll()
                    .track_scroll(&win.scroll)
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(rows)
                    .when(win.entries.is_empty(), |el| {
                        let line = match (win.query.trim().is_empty(), win.all) {
                            (true, false) => format!("No commands yet on {}.", win.alias),
                            (true, true) => "No commands yet.".to_owned(),
                            (false, _) => "No commands match.".to_owned(),
                        };
                        el.child(
                            div()
                                .py(px(24.))
                                .flex()
                                .justify_center()
                                .text_sm()
                                .text_color(t.muted)
                                .child(SharedString::from(line)),
                        )
                    }),
            )
            .child(
                div()
                    .px(px(16.))
                    .py(px(7.))
                    .border_t_1()
                    .border_color(t.hairline)
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x(px(12.))
                    .gap_y(px(4.))
                    .child(picker::hint("↑ ↓", "Navigate", t))
                    .child(picker::hint("↵", "Put at prompt", t))
                    .child(picker::hint("⌘↵", "Run", t))
                    .child(picker::hint("⌫", "Delete", t))
                    .child(picker::hint(
                        "⌘A",
                        if win.all { "This host" } else { "All hosts" },
                        t,
                    ))
                    .child(picker::hint("Esc", "Close", t)),
            );
        Some(overlay(card.into_any_element(), viewport, t))
    }

    // ---- Settings → Command history ------------------------------------------------------

    pub(super) fn history_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let hosts = if cx.has_global::<CommandHistory>() {
            cx.global::<CommandHistory>().hosts()
        } else {
            Vec::new()
        };
        let combo = crate::keymap::badge(
            cx.global::<crate::keymap::Keymap>()
                .combo(crate::keymap::ShortcutId::CommandHistory),
        );
        let mut list = w::card(&t);
        for (ix, host) in hosts.iter().enumerate() {
            let which = Clear::Host(host.clone());
            list = list.child(w::row(
                &t,
                ix == 0,
                host.clone(),
                None,
                self.clear_button(("history-clear", ix), &which, cx),
            ));
        }
        w::page_column()
            .child(w::page_header(&t, "Command history"))
            .child(w::page_subtitle(
                &t,
                format!(
                    "{combo} opens it over a session. The commands you run on each host are kept \
                     on this Mac only and never synced. Lines that look like secrets, and lines \
                     that start with a space, are left out."
                ),
            ))
            .child(if hosts.is_empty() {
                div()
                    .mt(px(32.))
                    .px(px(8.))
                    .text_size(px(13.))
                    .text_color(t.muted)
                    .child("No commands kept yet.")
                    .into_any_element()
            } else {
                div()
                    .child(w::section(&t, "Hosts", list))
                    .child(w::section(
                        &t,
                        "Everything",
                        w::card(&t).child(w::row(
                            &t,
                            true,
                            "All hosts",
                            Some("Forgets every command on this Mac.".into()),
                            self.clear_button("history-clear-all", &Clear::All, cx),
                        )),
                    ))
                    .into_any_element()
            })
    }

    /// A button that clears on its second press.
    fn clear_button(
        &self,
        id: impl Into<gpui::ElementId>,
        which: &Clear,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let t = self.theme;
        let confirming = self.history.confirm.as_ref() == Some(which);
        let which = which.clone();
        w::button(&t, id, if confirming { "Clear?" } else { "Clear" })
            .when(confirming, |el| el.text_color(t.danger))
            .on_click(cx.listener(move |s, _, _, cx| s.clear_history(&which, cx)))
    }

    fn clear_history(&mut self, which: &Clear, cx: &mut Context<Self>) {
        if self.history.confirm.as_ref() != Some(which) {
            self.history.confirm = Some(which.clone());
            return cx.notify();
        }
        self.history.confirm = None;
        let result = cx.update_global(|h: &mut CommandHistory, _| match which {
            Clear::Host(host) => h.clear(Scope::Host(host)),
            Clear::All => h.clear(Scope::All),
        });
        if let Err(e) = result {
            tracing::warn!(error = %e, "command_history_clear_failed");
            self.notify_toast(ToastKind::Critical, "Could not clear the history", cx);
        }
        cx.notify();
    }
}

/// The highlighted row, kept inside what is drawn.
fn win_active(win: &HistoryWindow) -> usize {
    win.active
        .min(win.entries.len().min(ROWS).saturating_sub(1))
}

fn overlay(
    card: AnyElement,
    viewport: gpui::Size<gpui::Pixels>,
    t: &crate::theme::Theme,
) -> AnyElement {
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
    .priority(2)
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::ago;

    #[test]
    fn ago_names_the_biggest_whole_unit() {
        assert_eq!(ago(1000, 1000), "now");
        assert_eq!(ago(1000, 1000 - 59), "now");
        assert_eq!(ago(1000, 1000 - 60), "1m");
        assert_eq!(ago(10_000, 10_000 - 3600), "1h");
        assert_eq!(ago(1_000_000, 1_000_000 - 2 * 86_400), "2d");
        assert_eq!(ago(10_000_000, 10_000_000 - 2 * 604_800), "2w");
        // A clock that went backwards is not the future.
        assert_eq!(ago(10, 500), "now");
    }
}
