// Adapted from zeron crates/ui/src/files/tree.rs (render_tree_row) (MIT) for the row shape.
//! The Files tab's window: two panels (source, path bar, column titles, rows, footer), the transfer
//! list, and the dialogs. The state and what the buttons do live in `files_tab.rs`.

use std::time::SystemTime;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, ClickEvent, Context, ExternalPaths, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px, uniform_list,
};

use crate::empty_state;
use crate::files_model::SortKey;
use crate::files_tab::{Dialog, Dragged, FilesView, Panel, State, Target};
use crate::icons::{self, icon};
use crate::panel_login::Step;
use crate::sftp_paths;
use crate::theme::Theme;
use crate::transfers;

const ROW_HEIGHT: f32 = 26.0;

/// What follows the pointer while items are dragged between panels.
impl Render for Dragged {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let label = match self.entries.as_slice() {
            [one] => one.name.clone(),
            many => format!("{} items", many.len()),
        };
        div()
            .px(px(10.))
            .py(px(4.))
            .rounded(px(8.))
            .bg(gpui::hsla(0., 0., 0.15, 0.92))
            .text_color(gpui::white())
            .text_size(px(12.))
            .child(SharedString::from(label))
    }
}

/// A small text button; unavailable ones are dimmed and ignore clicks.
fn action(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    primary: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(26.))
        .px(px(10.))
        .flex()
        .items_center()
        .rounded(px(7.))
        .text_size(px(12.))
        .cursor_pointer()
        .when(primary, |el| el.bg(t.accent).text_color(t.on_accent))
        .when(!primary, |el| {
            el.text_color(t.text).hover(|s| s.bg(t.row_hover))
        })
        .child(label.into())
}

fn icon_button(id: String, glyph: &'static str, t: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .flex_none()
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .cursor_pointer()
        .hover(|s| s.bg(t.row_active))
        .child(icon(glyph).size(px(15.)).text_color(t.muted))
}

impl FilesView {
    fn typing_in_a_path(&self, window: &Window, cx: &Context<Self>) -> Option<usize> {
        (0..2).find(|&side| self.panels[side].path.focus_handle(cx).is_focused(window))
    }

    fn on_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = e.keystroke.key.as_str();
        let plain = !e.keystroke.modifiers.platform && !e.keystroke.modifiers.control;
        if self.dialog.is_some() {
            match key {
                "enter" => self.confirm_dialog(window, cx),
                "escape" => self.close_dialog(window, cx),
                _ => return,
            }
            return cx.stop_propagation();
        }
        // A login question in a panel has the keyboard while its field does.
        for side in 0..2 {
            let Some(login) = self.panels[side].login.clone() else {
                continue;
            };
            let asking = match login.read(cx).step() {
                Step::Asking(q) => Some(q.input.as_ref().map(|i| i.focus_handle(cx))),
                _ => None,
            };
            if let Some(Some(field)) = asking
                && field.is_focused(window)
            {
                match key {
                    "enter" => login.update(cx, |l, cx| l.submit(cx)),
                    "escape" => login.update(cx, |l, cx| l.cancel(cx)),
                    _ => return,
                }
                window.focus(&self.focus, cx);
                return cx.stop_propagation();
            }
        }
        if let Some(side) = self.typing_in_a_path(window, cx) {
            match key {
                "enter" => self.go_typed(side, cx),
                "escape" => {}
                _ => return,
            }
            window.focus(&self.focus, cx);
            return cx.stop_propagation();
        }
        let side = self.active;
        match key {
            "down" if plain => self.step(side, 1, cx),
            "up" if plain => self.step(side, -1, cx),
            "enter" if plain => {
                if let Some(ix) = self.panels[side].selection.anchor() {
                    self.open_row(side, ix, cx);
                }
            }
            "backspace" if e.keystroke.modifiers.platform => self.start_delete(side, cx),
            "backspace" if plain => self.up(side, cx),
            "tab" if plain => {
                self.active = Self::other(side);
                cx.notify();
            }
            "f2" => self.start_rename(side, window, cx),
            "escape" => {
                if self.menu.take().is_none() {
                    self.panels[side].selection.clear();
                }
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn panel(&mut self, side: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let body = self.body(side, window, cx);
        let p = &self.panels[side];
        let active = self.active == side;
        let ready = p.state == State::Ready;
        let header = div()
            .flex_none()
            .px(px(10.))
            .h(px(34.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.hairline)
            .child(
                div()
                    .id(("files-source", side))
                    .min_w_0()
                    .h(px(24.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .rounded(px(7.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .hover(|s| s.bg(t.row_active))
                    .child(
                        icon(match p.target {
                            Target::Mac => icons::HOME,
                            Target::Host(_) => icons::SERVER,
                        })
                        .size(px(14.))
                        .text_color(t.muted),
                    )
                    .child(div().truncate().child(SharedString::from(p.target.label())))
                    .child(icon(icons::CHEVRON_RIGHT).size(px(12.)).text_color(t.faint))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_menu(side, cx))),
            );
        let path_bar = div()
            .flex_none()
            .px(px(8.))
            .py(px(6.))
            .flex()
            .items_center()
            .gap(px(4.))
            .child(
                icon_button(format!("files-home-{side}"), icons::HOME, &t)
                    .on_click(cx.listener(move |this, _, _, cx| this.go_home(side, cx))),
            )
            .child(
                icon_button(format!("files-up-{side}"), icons::ARROW_UP, &t)
                    .on_click(cx.listener(move |this, _, _, cx| this.up(side, cx))),
            )
            .child(
                div()
                    .id(("files-path", side))
                    .flex_1()
                    .min_w_0()
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(t.border)
                    .bg(t.row_hover)
                    .overflow_hidden()
                    .text_size(px(12.))
                    .child(p.path.clone()),
            )
            .child(
                icon_button(format!("files-refresh-{side}"), icons::RESTART, &t)
                    .on_click(cx.listener(move |this, _, _, cx| this.refresh(side, cx))),
            )
            .child(
                icon_button(format!("files-mkdir-{side}"), icons::PLUS, &t).on_click(
                    cx.listener(move |this, _, w, cx| this.start_new_folder(side, w, cx)),
                ),
            );
        let title = |label: &'static str, key: SortKey| {
            let arrow = if p.sort.key == key {
                if p.sort.ascending { " ↑" } else { " ↓" }
            } else {
                ""
            };
            div()
                .id(SharedString::from(format!("files-sort-{side}-{label}")))
                .cursor_pointer()
                .child(SharedString::from(format!("{label}{arrow}")))
                .on_click(cx.listener(move |this, _, _, cx| this.sort_by(side, key, cx)))
        };
        let columns = div()
            .flex_none()
            .h(px(22.))
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.hairline)
            .text_size(px(11.))
            .text_color(t.faint)
            .child(div().w(px(16.)))
            .child(div().flex_1().child(title("Name", SortKey::Name)))
            .child(
                div()
                    .w(px(64.))
                    .flex()
                    .justify_end()
                    .child(title("Size", SortKey::Size)),
            )
            .child(div().w(px(92.)).child(title("Modified", SortKey::Modified)));
        let picked = p.selection.len();
        let footer = div()
            .flex_none()
            .px(px(10.))
            .py(px(7.))
            .border_t_1()
            .border_color(t.hairline)
            .flex()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(if p.notice.is_some() {
                        t.danger
                    } else {
                        t.faint
                    })
                    .child(SharedString::from(p.notice.clone().unwrap_or_else(
                        || match picked {
                            0 => "Drag files here, or from Finder".to_owned(),
                            1 => "1 selected".to_owned(),
                            n => format!("{n} selected"),
                        },
                    ))),
            )
            .child(
                action(("files-rename", side), "Rename", picked == 1, &t).when(picked == 1, |el| {
                    el.on_click(cx.listener(move |this, _, w, cx| this.start_rename(side, w, cx)))
                }),
            )
            .child(
                action(("files-delete", side), "Delete", picked > 0, &t).when(picked > 0, |el| {
                    el.on_click(cx.listener(move |this, _, _, cx| this.start_delete(side, cx)))
                }),
            )
            .child(
                action(
                    ("files-copy", side),
                    if side == 0 { "Copy →" } else { "← Copy" },
                    picked > 0 && ready,
                    &t,
                )
                .when(picked > 0 && ready, |el| {
                    el.on_click(cx.listener(move |this, _, _, cx| this.copy_picked(side, cx)))
                }),
            );
        let menu = (self.menu == Some(side)).then(|| self.source_menu(side, cx));
        let offer = p.login.clone().and_then(|l| {
            let what = l.read(cx).save_offer()?;
            Some((l, what))
        });
        let offer = offer.map(|(login, what)| {
            let (yes, no) = (login.clone(), login);
            div()
                .flex_none()
                .px(px(10.))
                .py(px(6.))
                .border_t_1()
                .border_color(t.hairline)
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(11.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(format!("Save {what} in the vault?"))),
                )
                .child(
                    action(("files-save", side), "Save", true, &t)
                        .on_click(move |_, _, cx| yes.update(cx, |l, cx| l.answer_offer(true, cx))),
                )
                .child(
                    action(("files-nosave", side), "Not now", false, &t)
                        .on_click(move |_, _, cx| no.update(cx, |l, cx| l.answer_offer(false, cx))),
                )
        });
        div()
            .id(("files-panel", side))
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .border_1()
            .border_color(if active {
                t.accent.opacity(0.5)
            } else {
                t.hairline
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.active != side {
                        this.active = side;
                        cx.notify();
                    }
                }),
            )
            .drag_over::<Dragged>(move |s, _, _, _| s.border_color(t.accent))
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.border_color(t.accent))
            .on_drop(cx.listener(move |this, d: &Dragged, _, cx| {
                if d.from != side {
                    this.copy(d.from, d.entries.clone(), side, cx);
                }
            }))
            .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                this.drop_files(side, paths.paths(), cx);
            }))
            .child(header)
            .child(path_bar)
            .child(columns)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .children(offer)
            .child(footer)
            .children(menu)
            .into_any_element()
    }

    /// The panel's list of places: the Mac, then every host.
    fn source_menu(&self, side: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let mut targets = vec![Target::Mac];
        targets.extend(self.hosts.iter().map(|h| Target::Host(h.alias.clone())));
        let item = |ix: usize, target: Target| {
            let picked = self.panels[side].target == target;
            div()
                .id(("files-source-item", side * 1000 + ix))
                .h(px(26.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .text_size(px(12.))
                .hover(|s| s.bg(t.row_hover))
                .when(picked, |el| el.bg(t.row_active))
                .child(
                    icon(match target {
                        Target::Mac => icons::HOME,
                        Target::Host(_) => icons::SERVER,
                    })
                    .size(px(14.))
                    .text_color(t.muted),
                )
                .child(div().truncate().child(SharedString::from(target.label())))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_target(side, target.clone(), cx);
                }))
        };
        div()
            .id(("files-source-menu", side))
            .absolute()
            .top(px(34.))
            .left(px(8.))
            .w(px(240.))
            .max_h(px(320.))
            .overflow_y_scroll()
            .occlude()
            .py(px(4.))
            .rounded(px(10.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .shadow_lg()
            .children(targets.into_iter().enumerate().map(|(ix, tg)| item(ix, tg)))
            .into_any_element()
    }

    fn body(&mut self, side: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let (state, empty, count) = {
            let p = &self.panels[side];
            (p.state.clone(), p.entries.is_empty(), p.entries.len())
        };
        let note = |text: SharedString, color: gpui::Hsla| {
            div()
                .p(px(12.))
                .text_size(px(12.))
                .text_color(color)
                .child(text)
        };
        match &state {
            State::Connecting => self.connecting(side, window, cx),
            State::Loading if empty => note("Loading…".into(), t.muted).into_any_element(),
            State::Failed(why) => self.failure(side, why.clone(), "Retry", cx),
            State::Dropped(why) => self.failure(
                side,
                format!("The connection dropped. {why}"),
                "Reconnect",
                cx,
            ),
            _ if empty => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(empty_state::view(
                    &t,
                    empty_state::FOLDER,
                    "This folder is empty.",
                ))
                .into_any_element(),
            _ => uniform_list(
                ("files-rows", side),
                count,
                cx.processor(move |this, range, _, cx| this.rows(side, range, cx)),
            )
            .flex_1()
            .min_h_0()
            .into_any_element(),
        }
    }

    /// Why a panel has no folder to show, and the button that tries again: a login that failed
    /// asks its own connection again; anything else connects from the start.
    fn failure(
        &self,
        side: usize,
        why: String,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        div()
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .items_start()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(t.danger)
                    .child(SharedString::from(why)),
            )
            .child(
                action(("files-retry", side), label, true, &t).on_click(cx.listener(
                    move |this, _, _, cx| {
                        let login = this.panels[side].login.clone();
                        let failed = login
                            .as_ref()
                            .is_some_and(|l| matches!(l.read(cx).step(), Step::Failed(_)));
                        match login {
                            Some(login) if failed => login.update(cx, |l, cx| l.retry(cx)),
                            _ => this.connect(side, cx),
                        }
                        cx.notify();
                    },
                )),
            )
            .into_any_element()
    }

    /// A host panel that is still connecting: what the connection is asking, or that it waits.
    fn connecting(
        &mut self,
        side: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let Some(login) = self.panels[side].login.clone() else {
            return div()
                .p(px(12.))
                .text_size(px(12.))
                .text_color(t.muted)
                .child("Connecting…")
                .into_any_element();
        };
        let failed = match login.read(cx).step() {
            Step::Failed(why) => Some(why.clone()),
            _ => None,
        };
        if let Some(why) = failed {
            return self.failure(side, why, "Retry", cx);
        }
        let (lines, input, yes_no, skippable, note) = {
            let l = login.read(cx);
            let Step::Asking(q) = l.step() else {
                let text = l.note().unwrap_or("Connecting…").to_owned();
                return div()
                    .p(px(12.))
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child(SharedString::from(text))
                    .into_any_element();
            };
            (
                q.lines.clone(),
                q.input.clone(),
                q.yes_no,
                q.skippable,
                l.note().map(str::to_owned),
            )
        };
        if let Some(input) = &input {
            // A new question's field takes the keyboard once, so typing reaches it.
            let id = input.entity_id();
            if self.asked != Some(id) {
                self.asked = Some(id);
                window.focus(&input.focus_handle(cx), cx);
            }
        }
        let mut card = div()
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .text_size(px(12.));
        for line in lines {
            card = card.child(div().text_color(t.text).child(SharedString::from(line)));
        }
        if let Some(input) = input {
            card = card.child(
                div()
                    .h(px(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(t.border)
                    .bg(t.row_hover)
                    .child(input),
            );
        }
        if let Some(note) = note {
            card = card.child(div().text_color(t.muted).child(SharedString::from(note)));
        }
        let buttons = if yes_no {
            div()
                .flex()
                .gap(px(8.))
                .child(
                    action(("files-yes", side), "Yes", true, &t).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(l) = this.panels[side].login.clone() {
                                l.update(cx, |l, cx| l.answer_yes(true, cx));
                            }
                        },
                    )),
                )
                .child(
                    action(("files-no", side), "No", false, &t).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(l) = this.panels[side].login.clone() {
                                l.update(cx, |l, cx| l.answer_yes(false, cx));
                            }
                        },
                    )),
                )
        } else {
            div()
                .flex()
                .gap(px(8.))
                .child(
                    action(("files-continue", side), "Continue", true, &t).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(l) = this.panels[side].login.clone() {
                                l.update(cx, |l, cx| l.submit(cx));
                            }
                        },
                    )),
                )
                .child(
                    action(
                        ("files-skip", side),
                        if skippable { "Skip" } else { "Cancel" },
                        false,
                        &t,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(l) = this.panels[side].login.clone() {
                            l.update(cx, |l, cx| l.cancel(cx));
                        }
                    })),
                )
        };
        card.child(buttons).into_any_element()
    }

    /// The rows of panel `side` in `range`.
    fn rows(
        &mut self,
        side: usize,
        range: std::ops::Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = self.theme;
        let now = SystemTime::now();
        let picked = self.picked(side);
        let p: &Panel = &self.panels[side];
        range
            .filter_map(|ix| p.entries.get(ix).map(|e| (ix, e.clone())))
            .map(|(ix, e)| {
                let selected = p.selection.contains(ix);
                let modified = e
                    .modified
                    .map(|m| sftp_paths::modified(m, now))
                    .unwrap_or_default();
                let dragged = Dragged {
                    from: side,
                    entries: if selected {
                        picked.clone()
                    } else {
                        vec![e.clone()]
                    },
                };
                div()
                    .id(("files-row", side * 1_000_000 + ix))
                    .h(px(ROW_HEIGHT))
                    .flex_none()
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .when(selected, |el| el.bg(t.row_active))
                    .when(!selected, |el| el.hover(|s| s.bg(t.row_hover)))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        window.focus(&this.focus, cx);
                        if ev.click_count() >= 2 {
                            this.open_row(side, ix, cx);
                        } else {
                            let m = ev.modifiers();
                            this.click_row(side, ix, m.platform || m.control, m.shift, cx);
                        }
                    }))
                    .on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
                    .child(
                        icon(if e.is_dir {
                            icons::FOLDER
                        } else {
                            icons::DOCUMENT
                        })
                        .size(px(15.))
                        .text_color(if e.is_dir {
                            t.accent
                        } else {
                            t.muted
                        }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(e.name.clone())),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .text_color(t.muted)
                            .child(SharedString::from(if e.is_dir {
                                String::new()
                            } else {
                                sftp_paths::human_size(e.size)
                            })),
                    )
                    .child(
                        div()
                            .w(px(92.))
                            .flex_none()
                            .truncate()
                            .text_color(t.muted)
                            .child(SharedString::from(modified)),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// The copies in progress, under the panels.
    fn transfer_list(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = self.theme;
        let transfers = self.transfers.read(cx);
        let rows = transfers.rows();
        if rows.is_empty() {
            return None;
        }
        let mut list = div()
            .flex_none()
            .max_h(px(180.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(t.hairline)
            .child(
                div()
                    .px(px(12.))
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(px(11.))
                    .text_color(t.faint)
                    .child("Copying")
                    .child(
                        div()
                            .id("files-cancel-all")
                            .cursor_pointer()
                            .text_color(t.muted)
                            .hover(|s| s.text_color(t.text))
                            .child("Cancel all")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.transfers.update(cx, |t, _| t.cancel_all());
                            })),
                    ),
            );
        for row in rows {
            let batch = row.batch;
            let pct = row
                .progress
                .as_ref()
                .map_or(0, |p| sftp_paths::percent(p.done, p.total));
            let detail = row
                .progress
                .as_ref()
                .map_or_else(|| "Waiting…".to_owned(), transfers::detail);
            list = list.child(
                div()
                    .px(px(12.))
                    .py(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(
                                div()
                                    .truncate()
                                    .child(SharedString::from(row.label.clone())),
                            )
                            .child(
                                div().h(px(3.)).rounded(px(2.)).bg(t.row_active).child(
                                    div()
                                        .h_full()
                                        .rounded(px(2.))
                                        .bg(t.accent)
                                        .w(gpui::relative(pct as f32 / 100.0)),
                                ),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(t.faint)
                                    .child(SharedString::from(detail)),
                            ),
                    )
                    .child(
                        action(("files-cancel", batch as usize), "Cancel", false, &t).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.transfers.update(cx, |t, _| t.cancel(batch));
                            }),
                        ),
                    ),
            );
        }
        Some(list.into_any_element())
    }

    fn dialog_view(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = self.theme;
        let dialog = self.dialog.as_ref()?;
        let (title, detail, field, go) = match dialog {
            Dialog::NewFolder { input, .. } => ("New folder", None, Some(input.clone()), "Create"),
            Dialog::Rename { entry, input, .. } => (
                "Rename",
                Some(entry.name.clone()),
                Some(input.clone()),
                "Rename",
            ),
            Dialog::Delete { side, entries } => {
                let host = matches!(self.panels[*side].target, Target::Host(_));
                let what = match entries.as_slice() {
                    [one] => format!("\u{201c}{}\u{201d}", one.name),
                    many => format!("{} items", many.len()),
                };
                (
                    if host { "Delete" } else { "Move to the Trash" },
                    Some(if host {
                        format!(
                            "Delete {what} from {}? This cannot be undone.",
                            self.panels[*side].target.label()
                        )
                    } else {
                        format!("Move {what} to the Trash?")
                    }),
                    None,
                    if host { "Delete" } else { "Move to Trash" },
                )
            }
        };
        let card = div()
            .id("files-dialog")
            .w(px(380.))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded(px(14.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .text_color(t.text)
            .child(div().text_size(px(14.)).child(title))
            .children(detail.map(|d| {
                div()
                    .text_size(px(12.))
                    .text_color(t.muted)
                    .child(SharedString::from(d))
            }))
            .children(field.map(|input| {
                div()
                    .h(px(28.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(t.border)
                    .bg(t.row_hover)
                    .text_size(px(12.))
                    .child(input)
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        action("files-dialog-cancel", "Cancel", false, &t)
                            .on_click(cx.listener(|this, _, w, cx| this.close_dialog(w, cx))),
                    )
                    .child(
                        action("files-dialog-ok", go, true, &t)
                            .on_click(cx.listener(|this, _, w, cx| this.confirm_dialog(w, cx))),
                    ),
            );
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(t.scrim(0.35))
                .flex()
                .items_center()
                .justify_center()
                .child(card)
                .into_any_element(),
        )
    }
}

impl Render for FilesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let left = self.panel(0, window, cx);
        let right = self.panel(1, window, cx);
        let transfers = self.transfer_list(cx);
        let dialog = self.dialog_view(cx);
        div()
            .id("files-tab")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .text_color(t.text)
            .on_key_down(cx.listener(Self::on_key))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap(px(1.))
                    .child(left)
                    .child(right),
            )
            .children(transfers)
            .children(dialog)
    }
}
