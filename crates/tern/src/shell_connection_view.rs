// Adapted from zeron crates/ui/src/shell/command_palette.rs (palette card and overlay) (MIT).
//! Connection form overlay rendering and shared form widgets (`row`, `note`, `button`).

use crate::hover::HoverFade as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, anchored, deferred, div, point, px,
};

use super::{FORM_MARGIN, Shell, VaultStep};
use crate::sidebar;
use crate::text_input::TextInput;
use crate::theme::Theme;

impl Shell {
    pub(in super::super) fn render_form(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let form = self.form.as_ref()?;
        let t = &self.theme;
        let step = self.vault_step(cx);
        let viewport = window.viewport_size();
        let title = if form.editing.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
        // Existing groups, one click to reuse a spelling instead of typing it again.
        let groups = div().flex().flex_wrap().gap(px(6.)).children(
            sidebar::group_names(&self.connections)
                .into_iter()
                .enumerate()
                .map(|(n, name)| {
                    let pick = name.clone();
                    div()
                        .id(("group-chip", n))
                        .px(px(8.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .text_xs()
                        .text_color(t.muted)
                        .hover_fade(format!("group-chip-{n}"), t.row_hover, t.row_active)
                        .cursor_pointer()
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            if let Some(form) = shell.form.as_ref() {
                                let group = form.group.clone();
                                group.update(cx, |i, cx| i.set_text(pick.clone(), cx));
                            }
                        }))
                        .child(SharedString::from(name))
                }),
        );
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(16.))
            .py(px(12.))
            .child(row("Name", &form.name, t))
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(div().flex_1().child(row("Host", &form.host, t)))
                    .child(div().w(px(96.)).child(row("Port", &form.port, t))),
            )
            .child(row("User", &form.user, t))
            .child(row("Key file", &form.key, t))
            .child(row("Group", &form.group, t))
            .child(groups)
            .child(row("Tags", &form.tags, t))
            .child(row("Password", &form.password, t))
            .child(self.advanced_section(form, cx));
        match step {
            VaultStep::None => {}
            VaultStep::Unlock => body = body.child(row("Vault passphrase", &form.vault_pass, t)),
            VaultStep::Create => {
                body = body
                    .child(note(
                        "No vault yet: this creates one, locked with this passphrase.",
                        t,
                    ))
                    .child(row("New vault passphrase", &form.vault_pass, t))
                    .child(row("Repeat passphrase", &form.vault_repeat, t));
            }
        }
        let busy = form.busy;
        let card =
            div()
                .id("connection-form")
                .w(px(
                    460.0_f32.min(f32::from(viewport.width) - 2. * FORM_MARGIN)
                ))
                // The sheet never outgrows the window: the body scrolls between a header and
                // a footer that stay put, so Save is always reachable.
                .max_h(viewport.height - px(2. * FORM_MARGIN))
                .overflow_hidden()
                .flex()
                .flex_col()
                .rounded(px(16.))
                .border_1()
                .border_color(t.border)
                .bg(t.popup)
                .text_color(t.text)
                .on_key_down(cx.listener(Self::on_form_key))
                .child(
                    div()
                        .flex_none()
                        .px(px(16.))
                        .py(px(12.))
                        .border_b_1()
                        .border_color(t.hairline)
                        .text_size(px(14.))
                        .child(title),
                )
                .child(
                    div()
                        .id("connection-form-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(body),
                )
                .child(
                    div()
                        .flex_none()
                        .px(px(16.))
                        .py(px(10.))
                        .border_t_1()
                        .border_color(t.hairline)
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(t.danger)
                                .children(form.error.clone().map(SharedString::from)),
                        )
                        .child(button("form-cancel", "Cancel", false, t).on_click(
                            cx.listener(|shell, _, window, cx| shell.close_form(window, cx)),
                        ))
                        .child(
                            button("form-save", if busy { "Saving…" } else { "Save" }, true, t)
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.submit_form(window, cx)
                                })),
                        ),
                );
        Some(
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
            .into_any_element(),
        )
    }
}

pub(in super::super) fn row(
    label: &'static str,
    input: &Entity<TextInput>,
    t: &Theme,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(div().text_xs().text_color(t.muted).child(label))
        .child(
            div()
                .h(px(30.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(8.))
                .border_1()
                .border_color(t.border)
                .bg(t.row_hover)
                .overflow_hidden()
                .text_sm()
                .child(input.clone()),
        )
}

pub(in super::super) fn note(text: &'static str, t: &Theme) -> impl IntoElement + use<> {
    div().text_xs().text_color(t.muted).child(text)
}

pub(crate) fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    primary: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(28.))
        .px(px(12.))
        .flex()
        .items_center()
        .rounded(px(8.))
        .text_sm()
        .cursor_pointer()
        .when(primary, |el| el.bg(t.accent).text_color(t.on_accent))
        .when(!primary, |el| {
            el.hover_fade(format!("form-button-{id}"), t.row_hover, t.row_active)
                .text_color(t.text)
        })
        .child(label.into())
}
