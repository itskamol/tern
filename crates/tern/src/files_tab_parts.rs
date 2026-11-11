//! Pieces of the Files tab's window that stand apart from the panels: the buttons, what follows
//! the pointer in a drag, the transfer list and the dialogs.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::files_tab::{Dialog, Dragged, FilesView, Target};
use crate::files_text;
use crate::icons::icon;
use crate::theme::Theme;
use crate::transfers;

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
pub(crate) fn action(
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

pub(crate) fn icon_button(id: String, glyph: &'static str, t: &Theme) -> gpui::Stateful<gpui::Div> {
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
    /// The copies in progress, under the panels.
    pub(crate) fn transfer_list(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                .map_or(0, |p| files_text::percent(p.done, p.total));
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

    pub(crate) fn dialog_view(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
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
