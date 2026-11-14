// Adapted from zeron crates/ui/src/files/tree.rs (render_tree_row) (MIT) for the row shape.
//! The Files panel's window: header, how the folder was chosen, breadcrumb, column titles, rows
//! and the footer. What is shown and what the buttons do live in `files_panel.rs` and
//! `files_panel_model.rs`.

use std::time::SystemTime;

use gpui::AppContext as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, ClickEvent, Context, ExternalPaths, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px, uniform_list,
};

use crate::empty_state;
use crate::files_panel::{FilesPanel, Load};
use crate::files_panel_edit::{EditKey, edit_key};
use crate::files_panel_model::{Edit, Mode, SNIPPET, Shown, crumbs};
use crate::files_panel_send::send_button;
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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(name_title(self.model.picked().len()))),
            )
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
            // The count is in the Name column's title: the actions need this row's room.
            .child(div().flex_1().min_w_0())
            .child(
                action("files-panel-rename", "Rename", picked == 1, &t).when(picked == 1, |el| {
                    el.on_click(cx.listener(|this, _, w, cx| this.start_rename(w, cx)))
                }),
            )
            .child(
                action("files-panel-delete", "Delete", picked > 0, &t).when(picked > 0, |el| {
                    el.on_click(cx.listener(|this, _, _, cx| this.start_delete(cx)))
                }),
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
        let edit = self.model.edit().cloned();
        let deleting = match &edit {
            Some(Edit::Delete(paths)) => paths.clone(),
            _ => Vec::new(),
        };
        // The question goes in the first row it is about, in the order shown.
        let asks_on = (0..listing.visible.len())
            .find(|ix| listing.row(*ix).is_some_and(|e| deleting.contains(&e.path)));
        range
            .filter_map(|ix| listing.row(ix).map(|e| (ix, e)))
            .map(|(ix, e)| {
                let selected = self.model.is_picked(&e.path);
                let renaming = matches!(&edit, Some(Edit::Rename(p)) if *p == e.path)
                    .then(|| self.rename_input.clone())
                    .flatten();
                let renaming_none = renaming.is_none();
                let dragged = renaming_none.then(|| self.dragged_rows(ix)).flatten();
                let doomed = deleting.contains(&e.path);
                let asks = (asks_on == Some(ix)).then_some(deleting.len());
                let modified = e
                    .modified
                    .map(|m| files_text::modified(m, now))
                    .unwrap_or_default();
                let group: SharedString = format!("files-panel-row-{ix}").into();
                div()
                    .id(("files-panel-row", ix))
                    .group(group.clone())
                    .relative()
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
                    // A row being renamed keeps its clicks for the field.
                    .when(renaming.is_none(), |el| {
                        el.on_click(cx.listener(move |this, ev: &ClickEvent, w, cx| {
                            w.focus(&this.focus, cx);
                            if ev.click_count() >= 2 {
                                this.open_row(ix, cx);
                            } else {
                                let m = ev.modifiers();
                                this.click_row(ix, m.platform || m.control, m.shift, cx);
                            }
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, ev: &MouseDownEvent, w, cx| {
                                w.focus(&this.focus, cx);
                                this.open_send_menu(ix, ev.position, cx);
                            }),
                        )
                    })
                    .when_some(dragged, |el, rows| {
                        el.on_drag(rows, |rows, _, window, cx| {
                            // The panes lay their drop layers out only while a drag is on.
                            window.refresh();
                            cx.new(|_| rows.clone())
                        })
                    })
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
                    .child(match renaming {
                        // The field in the name's place, on the Files tab's input frame.
                        Some(input) => div()
                            .flex_1()
                            .min_w_0()
                            .h(px(ROW_HEIGHT - 6.))
                            .px(px(6.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .border_1()
                            .border_color(t.accent)
                            .bg(t.row_hover)
                            .overflow_hidden()
                            .child(input)
                            .into_any_element(),
                        None => div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(doomed, |el| el.text_color(t.danger))
                            .child(SharedString::from(e.name.clone()))
                            .into_any_element(),
                    })
                    .when_some(asks, |el, n| el.child(self.delete_question(n, cx)))
                    // Send to…, at the row's end while it is hovered or picked.
                    .when(asks.is_none() && renaming_none, |el| {
                        el.child(send_button(ix, selected, &t, cx).when(!selected, |b| {
                            b.invisible().group_hover(group.clone(), |s| s.visible())
                        }))
                    })
                    .when(asks.is_none(), |el| {
                        el.child(
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
                    })
                    .into_any_element()
            })
            .collect()
    }
}

impl FilesPanel {
    /// The delete's question, in the row of the first item it would remove: what goes, and the
    /// two answers. Nothing is deleted until Delete is pressed.
    fn delete_question(&self, n: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = self.theme;
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(t.danger)
                    .child(SharedString::from(match n {
                        1 => "Delete?".to_owned(),
                        n => format!("Delete {n} items?"),
                    })),
            )
            .child(
                action("files-panel-delete-yes", "Delete", true, &t)
                    .on_click(cx.listener(|this, _, w, cx| this.confirm_edit(w, cx))),
            )
            .child(
                action("files-panel-delete-no", "Cancel", false, &t)
                    .on_click(cx.listener(|this, _, w, cx| this.cancel_edit(w, cx))),
            )
            .into_any_element()
    }
}

impl Render for FilesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.settle_rename(window, cx);
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
                let clip = self.render_clip_banner(cx);
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
                    .children(clip)
                    .child(columns)
                    .child(div().flex_1().min_h_0().flex().flex_col().child(rows))
                    .child(footer)
                    .into_any_element()
            }
        };
        div()
            .id("files-panel")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .text_color(t.text)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, w, cx| {
                if this.send_menu.is_some() && e.keystroke.key == "escape" {
                    this.close_send_menu(cx);
                    return cx.stop_propagation();
                }
                let Some(key) = edit_key(&e.keystroke, this.model.edit().is_some()) else {
                    return;
                };
                match key {
                    EditKey::Rename => this.start_rename(w, cx),
                    EditKey::Delete => this.start_delete(cx),
                    EditKey::Confirm => this.confirm_edit(w, cx),
                    EditKey::Cancel => this.cancel_edit(w, cx),
                    EditKey::Copy => this.copy_rows(cx),
                    EditKey::Paste => this.paste(cx),
                }
                cx.stop_propagation();
            }))
            // Files dropped from Finder upload into the folder shown.
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(t.accent.opacity(0.08)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.upload(paths.paths(), cx);
            }))
            .child(header)
            .child(body)
            .children(crate::transfer_rows::transfer_list(&self.transfers, t, cx))
            .children(self.render_send_menu(cx))
    }
}

/// The Name column's title: how many rows are picked while any are, where there is room to say
/// so in full.
fn name_title(picked: usize) -> String {
    match picked {
        0 => "Name".to_owned(),
        1 => "1 selected".to_owned(),
        n => format!("{n} selected"),
    }
}

#[cfg(test)]
mod tests {
    use super::name_title;

    #[test]
    fn the_name_title_counts_the_picked_rows_and_is_name_with_none() {
        assert_eq!(name_title(0), "Name");
        assert_eq!(name_title(1), "1 selected");
        assert_eq!(name_title(12), "12 selected");
    }
}
