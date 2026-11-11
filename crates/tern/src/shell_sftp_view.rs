// Adapted from zeron crates/ui/src/files/tree.rs (render_tree_row) (MIT) for the row shape, and
// from crates/ui/src/popover.rs (popover_card) (MIT) for the card.
//! The file browser's window: header, path bar, column titles, rows, footer. The state and the
//! transfers live in `shell_sftp.rs`.

use std::time::SystemTime;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, ExternalPaths, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window,
    anchored, deferred, div, point, px,
};

use super::State;
use crate::empty_state::{self, Listing};
use crate::icons::{self, icon};
use crate::sftp_paths;
use crate::shell::Shell;
use crate::shell::connections_ui::button;
use crate::statusline;
use crate::theme::{SPACE_SM, TITLEBAR_HEIGHT};

const WIDTH: f32 = 400.0;
const ROW_HEIGHT: f32 = 26.0;

impl Shell {
    fn on_sftp_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let typing = self
            .panels
            .sftp
            .as_ref()
            .is_some_and(|p| p.path.focus_handle(cx).is_focused(window));
        match (e.keystroke.key.as_str(), typing) {
            ("escape", _) => self.close_sftp(window, cx),
            ("enter", true) => {
                self.sftp_go_typed(cx);
                if let Some(panel) = self.panels.sftp.as_ref() {
                    window.focus(&panel.focus, cx);
                }
            }
            ("enter", false) => self.sftp_open_selected(cx),
            ("down", false) => self.sftp_step(1, cx),
            ("up", false) => self.sftp_step(-1, cx),
            ("backspace", false) => self.sftp_up(cx),
            ("/", false) => self.focus_sftp_path(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    pub(in crate::shell) fn render_sftp_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel = self.panels.sftp.as_ref()?;
        let tab = self.tabs.iter().find(|t| t.id == panel.tab)?;
        let t = self.theme;
        let viewport = window.viewport_size();
        let label = tab.title().map_or_else(|| tab.alias.clone(), str::to_owned);
        let width = WIDTH.min(f32::from(viewport.width) - 2.0 * SPACE_SM);
        let height =
            f32::from(viewport.height) - TITLEBAR_HEIGHT - SPACE_SM - statusline::HEIGHT - 1.0;
        let icon_button = |id: &'static str, glyph: &'static str| {
            div()
                .id(id)
                .flex_none()
                .size(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .cursor_pointer()
                .hover(|s| s.bg(t.row_active))
                .child(icon(glyph).size(px(15.)).text_color(t.muted))
        };
        let header = div()
            .flex_none()
            .px(px(12.))
            .h(px(36.))
            .flex()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(t.hairline)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.))
                    .child(SharedString::from(format!("Files · {label}"))),
            )
            .child(
                icon_button("sftp-close", icons::CLOSE)
                    .on_click(cx.listener(|s, _, w, cx| s.close_sftp(w, cx))),
            );
        let path_bar = div()
            .flex_none()
            .px(px(8.))
            .py(px(6.))
            .flex()
            .items_center()
            .gap(px(4.))
            .child(
                icon_button("sftp-home", icons::HOME)
                    .on_click(cx.listener(|s, _, _, cx| s.sftp_home(cx))),
            )
            .child(
                icon_button("sftp-up", icons::ARROW_UP)
                    .on_click(cx.listener(|s, _, _, cx| s.sftp_up(cx))),
            )
            .child(
                div()
                    .id("sftp-path")
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
                    .on_click(cx.listener(|s, _, w, cx| s.focus_sftp_path(w, cx)))
                    .child(panel.path.clone()),
            )
            .child(
                icon_button("sftp-upload", icons::UPLOAD)
                    .on_click(cx.listener(|s, _, _, cx| s.sftp_pick_upload(cx))),
            );
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
            .child(div().flex_1().child("Name"))
            .child(div().w(px(58.)).flex().justify_end().child("Size"))
            .child(div().w(px(88.)).child("Modified"));
        let now = SystemTime::now();
        let body = match &panel.state {
            State::Opening | State::Loading if panel.entries.is_empty() => {
                note_row(&t, "Loading…").into_any_element()
            }
            State::Failed(why) => div()
                .p(px(12.))
                .text_size(px(12.))
                .text_color(t.danger)
                .child(SharedString::from(why.clone()))
                .into_any_element(),
            _ if empty_state::listing(panel.entries.len(), panel.entries.len())
                == Listing::Empty =>
            {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        empty_state::view(&t, empty_state::FOLDER, "This folder is empty.")
                            .gap(px(12.))
                            .child(
                                button("sftp-empty-upload", "Upload files", true, &t)
                                    .on_click(cx.listener(|s, _, _, cx| s.sftp_pick_upload(cx))),
                            ),
                    )
                    .into_any_element()
            }
            _ => div()
                .id("sftp-rows")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(panel.entries.iter().enumerate().map(|(ix, e)| {
                    let selected = panel.selected == Some(ix);
                    let modified = e
                        .modified
                        .map(|m| sftp_paths::modified(m, now))
                        .unwrap_or_default();
                    div()
                        .id(("sftp-row", ix))
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
                        .on_click(cx.listener(move |s, ev: &ClickEvent, _, cx| {
                            if ev.click_count() >= 2 {
                                s.sftp_open_entry(ix, cx);
                            } else {
                                s.sftp_select(ix, cx);
                            }
                        }))
                        .child(
                            icon(if e.is_dir {
                                icons::FOLDER
                            } else {
                                icons::DOCUMENT
                            })
                            .size(px(15.))
                            .flex_none()
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
                                .w(px(58.))
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
                                .w(px(88.))
                                .flex_none()
                                .truncate()
                                .text_color(t.muted)
                                .child(SharedString::from(modified)),
                        )
                }))
                .into_any_element(),
        };
        let can_download = panel
            .selected
            .and_then(|ix| panel.entries.get(ix))
            .is_some_and(|e| !e.is_dir)
            && !panel.busy;
        let footer = div()
            .flex_none()
            .px(px(12.))
            .py(px(8.))
            .border_t_1()
            .border_color(t.hairline)
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.))
                    .text_color(if panel.notice.is_some() {
                        t.danger
                    } else {
                        t.faint
                    })
                    .child(SharedString::from(
                        panel
                            .notice
                            .clone()
                            .unwrap_or_else(|| "Drop files here to upload".to_owned()),
                    )),
            )
            .child(
                button("sftp-download", "Download", can_download, &t).when(can_download, |el| {
                    el.on_click(cx.listener(|s, _, _, cx| s.sftp_download_selected(cx)))
                }),
            );
        let card = div()
            .id("sftp-panel")
            .track_focus(&panel.focus)
            .occlude()
            .w(px(width))
            .h(px(height))
            .flex()
            .flex_col()
            .rounded(px(12.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .shadow_lg()
            .overflow_hidden()
            .text_color(t.text)
            .on_key_down(cx.listener(Self::on_sftp_key))
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.border_color(t.accent))
            .on_drop(cx.listener(|s, paths: &ExternalPaths, _, cx| {
                s.sftp_upload(paths.paths().to_vec(), cx)
            }))
            .child(header)
            .child(path_bar)
            .child(columns)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .child(footer);
        Some(
            deferred(
                anchored()
                    .position(point(
                        viewport.width - px(width + SPACE_SM),
                        px(TITLEBAR_HEIGHT),
                    ))
                    .child(card),
            )
            .priority(2)
            .into_any_element(),
        )
    }
}

fn note_row(t: &crate::theme::Theme, text: &'static str) -> impl IntoElement + use<> {
    div()
        .px(px(12.))
        .py(px(10.))
        .text_size(px(12.))
        .text_color(t.muted)
        .child(text)
}
