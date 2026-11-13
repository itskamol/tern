//! Pieces of the Files tab's window that stand apart from the panels: the buttons, what follows
//! the pointer in a drag, the transfer list and the dialogs.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::files_tab::{Dialog, Dragged, FilesView, Target};
use crate::icons::icon;
use crate::theme::Theme;

/// What follows the pointer while items are dragged between panels.
impl Render for Dragged {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let label = match self.entries.as_slice() {
            [one] => one.name.clone(),
            many => format!("{} items", many.len()),
        };
        drag_ghost(label)
    }
}

/// The dark pill that follows the pointer in a drag of files, here and from the Files panel.
pub(crate) fn drag_ghost(text: String) -> gpui::Div {
    div()
        .px(px(10.))
        .py(px(4.))
        .rounded(px(8.))
        .bg(gpui::hsla(0., 0., 0.15, 0.92))
        .text_color(gpui::white())
        .text_size(px(12.))
        .child(SharedString::from(text))
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

/// A tick box `size` square, filled with the accent when `on`.
pub(crate) fn tick_box(on: bool, size: f32, t: &Theme) -> gpui::Div {
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(if size > 12.0 { 4. } else { 3. }))
        .border_1()
        .border_color(if on { t.accent } else { t.border })
        .when(on, |el| el.bg(t.accent))
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
