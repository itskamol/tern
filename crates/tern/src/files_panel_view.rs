// Adapted from zeron crates/ui/src/files/tree.rs (render_tree_row) (MIT) for the row shape.
//! The Files panel's window: header, how the folder was chosen, breadcrumb, column titles, rows
//! and the footer. What is shown and what the buttons do live in `files_panel.rs` and
//! `files_panel_model.rs`.

use std::time::SystemTime;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px, uniform_list,
};

use crate::empty_state;
use crate::files_panel::{FilesPanel, Load};
use crate::files_panel_model::{Mode, SNIPPET, Shown, crumbs};
use crate::files_tab_parts::{action, icon_button};
use crate::files_text;
use crate::icons::{self, icon};
use crate::theme::MONO_FONT;

/// Row height, header and column widths. The Files tab's row and header sizes, with Size and
/// Modified narrowed to fit the sidebar's range (docs/ui.md, Files panel).
const ROW_HEIGHT: f32 = 26.0;
const HEADER_HEIGHT: f32 = 34.0;
const ICON_WIDTH: f32 = 15.0;
const SIZE_WIDTH: f32 = 52.0;
const MODIFIED_WIDTH: f32 = 72.0;
/// Below this panel width the Modified column is left out: the name would have under 70 px.
pub const MODIFIED_MIN_PANEL: f32 = 256.0;

impl FilesPanel {
    fn note(&self, text: impl Into<SharedString>) -> gpui::Div {
        div()
            .px(px(10.))
            .py(px(8.))
            .text_size(px(12.))
            .text_color(self.theme.muted)
            .child(text.into())
    }

    /// How the folder shown was chosen, and the way to change that.
    fn mode_line(&self, shown: &Shown, cx: &mut Context<Self>) -> Option<AnyElement> {
        let t = self.theme;
        let Shown::Folder { mode, .. } = shown else {
            return None;
        };
        let line = div()
            .flex_none()
            .px(px(10.))
            .py(px(6.))
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(11.))
            .text_color(t.faint);
        Some(match mode {
            Mode::Following => line
                .child("Following the shell's folder")
                .into_any_element(),
            Mode::Browsing => line
                .child(div().flex_1().min_w_0().truncate().child("Browsing"))
                .when(shown.can_return(), |el| {
                    el.child(
                        action("files-panel-back", "Back to shell folder", false, &t)
                            .h(px(22.))
                            .text_size(px(11.))
                            .text_color(t.accent)
                            .on_click(cx.listener(|this, _, _, cx| this.back_to_shell(cx))),
                    )
                })
                .into_any_element(),
            Mode::AtHome => return None,
        })
    }

    /// The shell reports no folder: say so, and offer the line that makes it.
    fn silent_notice(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        div()
            .flex_none()
            .px(px(10.))
            .py(px(8.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .border_b_1()
            .border_color(t.hairline)
            .text_size(px(11.))
            .text_color(t.muted)
            .child("This shell does not report its folder, so the panel stays where you open it. Add this line to its rc file to follow it:")
            .child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .rounded(px(7.))
                    .bg(t.row_hover)
                    .font_family(MONO_FONT)
                    .text_size(px(10.))
                    .text_color(t.text)
                    .child(SharedString::from(SNIPPET)),
            )
            .child(
                div().flex().child(
                    action("files-panel-copy-snippet", "Copy line", false, &t)
                        .h(px(22.))
                        .text_size(px(11.))
                        .text_color(t.accent)
                        .on_click(cx.listener(|this, _, _, cx| this.copy_snippet(cx))),
                ),
            )
            .into_any_element()
    }

    fn path_bar(&self, hidden: bool, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let cwd = self
            .listing
            .as_ref()
            .map(|l| l.cwd.clone())
            .unwrap_or_default();
        let parts = if cwd.is_empty() {
            Vec::new()
        } else {
            crumbs(&cwd)
        };
        let last = parts.len().saturating_sub(1);
        let trail = parts.into_iter().enumerate().map(|(ix, c)| {
            let path = c.path.clone();
            div()
                .id(("files-panel-crumb", ix))
                .flex_none()
                .h(px(22.))
                .px(px(4.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if ix == last { t.text } else { t.muted })
                .hover(|s| s.bg(t.row_hover))
                .child(SharedString::from(c.label))
                .on_click(cx.listener(move |this, _, _, cx| this.open_path(&path, cx)))
        });
        div()
            .flex_none()
            .px(px(6.))
            .py(px(4.))
            .flex()
            .items_center()
            .gap(px(2.))
            .child(
                icon_button("files-panel-up".into(), icons::ARROW_UP, &t)
                    .on_click(cx.listener(|this, _, _, cx| this.up(cx))),
            )
            // The tail of a long path is what matters, so the front is what gets clipped.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .justify_end()
                    .overflow_hidden()
                    .children(trail),
            )
            .child(
                icon_button("files-panel-refresh".into(), icons::RESTART, &t)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                div()
                    .id("files-panel-dotfiles")
                    .flex_none()
                    .h(px(22.))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(if hidden { t.accent } else { t.muted })
                    .when(hidden, |el| el.bg(t.row_active))
                    .hover(|s| s.bg(t.row_hover))
                    .child("Dotfiles")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_hidden(cx))),
            )
            .into_any_element()
    }

    fn columns(&self) -> AnyElement {
        let t = self.theme;
        div()
            .flex_none()
            .h(px(22.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .border_b_1()
            .border_color(t.hairline)
            .text_size(px(11.))
            .text_color(t.faint)
            .child(div().w(px(ICON_WIDTH)).flex_none())
            .child(div().flex_1().min_w_0().child("Name"))
            .child(
                div()
                    .w(px(SIZE_WIDTH))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .child("Size"),
            )
            .when(self.width >= MODIFIED_MIN_PANEL, |el| {
                el.child(div().w(px(MODIFIED_WIDTH)).flex_none().child("Modified"))
            })
            .into_any_element()
    }

    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let picked = self.model.picked().len();
        div()
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
                    .text_color(t.faint)
                    .child(SharedString::from(match picked {
                        0 => "Pick files to download".to_owned(),
                        1 => "1 selected".to_owned(),
                        n => format!("{n} selected"),
                    })),
            )
            .child(
                action("files-panel-download", "Download", picked > 0, &t).when(picked > 0, |el| {
                    el.on_click(cx.listener(|this, _, _, cx| this.download(cx)))
                }),
            )
            .into_any_element()
    }

    fn body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        let (load, count) = match &self.listing {
            Some(l) => (l.load.clone(), l.visible.len()),
            None => (Load::Loading, 0),
        };
        match load {
            Load::Failed(why) => div()
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
                    action("files-panel-retry", "Retry", true, &t)
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
                .into_any_element(),
            Load::Loading if count == 0 => self.note("Loading…").into_any_element(),
            _ if count == 0 => div()
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
                "files-panel-rows",
                count,
                cx.processor(|this, range, _, cx| this.rows(range, cx)),
            )
            .flex_1()
            .min_h_0()
            .into_any_element(),
        }
    }

    fn rows(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = self.theme;
        let now = SystemTime::now();
        let with_modified = self.width >= MODIFIED_MIN_PANEL;
        let Some(listing) = self.listing.as_ref() else {
            return Vec::new();
        };
        range
            .filter_map(|ix| listing.row(ix).map(|e| (ix, e)))
            .map(|(ix, e)| {
                let selected = self.model.is_picked(&e.path);
                let modified = e
                    .modified
                    .map(|m| files_text::modified(m, now))
                    .unwrap_or_default();
                div()
                    .id(("files-panel-row", ix))
                    // A uniform_list lays each row out on its own; without a width it shrinks to
                    // its name and Size and Modified drift off their column titles.
                    .w_full()
                    .h(px(ROW_HEIGHT))
                    .flex_none()
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .when(selected, |el| el.bg(t.row_active))
                    .when(!selected, |el| el.hover(|s| s.bg(t.row_hover)))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        if ev.click_count() >= 2 {
                            this.open_row(ix, cx);
                        } else {
                            let m = ev.modifiers();
                            this.click_row(ix, m.platform || m.control, m.shift, cx);
                        }
                    }))
                    .child(
                        icon(if e.is_dir {
                            icons::FOLDER
                        } else {
                            icons::DOCUMENT
                        })
                        .size(px(ICON_WIDTH))
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
                            .w(px(SIZE_WIDTH))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .text_size(px(11.))
                            .text_color(t.muted)
                            .child(SharedString::from(if e.is_dir {
                                String::new()
                            } else {
                                files_text::human_size(e.size)
                            })),
                    )
                    .when(with_modified, |el| {
                        el.child(
                            div()
                                .w(px(MODIFIED_WIDTH))
                                .flex_none()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(t.muted)
                                .child(SharedString::from(modified)),
                        )
                    })
                    .into_any_element()
            })
            .collect()
    }
}

impl Render for FilesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme;
        let shown = self.model.shown();
        let header = div()
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.hairline)
            .child(icon(icons::SERVER).size(px(14.)).text_color(t.muted))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.))
                    .child(SharedString::from(self.name())),
            );
        let body: AnyElement = match &shown {
            None => self
                .note("Open a session to browse its files.")
                .into_any_element(),
            Some(Shown::Waiting) => self
                .note("Waiting for the login to finish…")
                .into_any_element(),
            Some(Shown::Offline { idle }) => {
                let idle = *idle;
                div()
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .items_start()
                    .child(div().text_size(px(12.)).text_color(t.muted).child(if idle {
                        "This session is not connected yet."
                    } else {
                        "This session is no longer connected."
                    }))
                    .child(
                        action(
                            "files-panel-reconnect",
                            if idle { "Connect" } else { "Reconnect" },
                            true,
                            &t,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                    )
                    .into_any_element()
            }
            Some(
                folder @ Shown::Folder {
                    shell_reports,
                    hidden,
                    ..
                },
            ) => {
                let (reports, hidden) = (*shell_reports, *hidden);
                let mode = self.mode_line(folder, cx);
                let silent = (!reports).then(|| self.silent_notice(cx));
                let path_bar = self.path_bar(hidden, cx);
                let columns = self.columns();
                let rows = self.body(cx);
                let footer = self.footer(cx);
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .children(silent)
                    .children(mode)
                    .child(path_bar)
                    .child(columns)
                    .child(div().flex_1().min_h_0().flex().flex_col().child(rows))
                    .child(footer)
                    .into_any_element()
            }
        };
        div()
            .id("files-panel")
            .size_full()
            .flex()
            .flex_col()
            .text_color(t.text)
            .child(header)
            .child(body)
    }
}
