//! The connection form's collapsed "Advanced" section, and the rule for when it opens by itself.

use gpui::prelude::FluentBuilder;
use gpui::{
    Context, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement, Styled,
    Transformation, div, percentage, px,
};

use super::{ConnectionForm, row};
use crate::connections::{Connection, DEFAULT_KEEP_ALIVE};
use crate::hover::HoverFade as _;
use crate::icons::{self, icon};
use crate::settings_widgets::toggle;
use crate::shell::Shell;

/// True when a connection has any advanced value set, so editing it never hides one.
pub(super) fn in_use(c: &Connection) -> bool {
    let set = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
    !c.proxy_jump.trim().is_empty()
        || c.keep_alive.is_some_and(|k| k != DEFAULT_KEEP_ALIVE)
        || c.forward_agent
        || !c.forwards.is_empty()
        || set(&c.vault_key)
        || set(&c.password_command)
        || set(&c.sudo_password_command)
}

impl Shell {
    /// The disclosure row (chevron like the sidebar group headers) and, when open, its fields.
    pub(super) fn advanced_section(
        &self,
        form: &ConnectionForm,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = &self.theme;
        let faint = t.secondary();
        let open = form.advanced_open;
        let header = div()
            .id("form-advanced")
            .h(px(28.))
            .px(px(4.))
            .flex()
            .items_center()
            .gap(px(8.))
            .rounded(px(8.))
            .cursor_pointer()
            .hover_fade("form-advanced", gpui::transparent_black(), t.row_hover)
            .on_click(cx.listener(|shell, _, _, cx| {
                if let Some(form) = shell.form.as_mut() {
                    form.advanced_open = !form.advanced_open;
                }
                cx.notify();
            }))
            .child(
                icon(icons::CHEVRON_RIGHT)
                    .size(px(12.))
                    .text_color(faint)
                    .with_transformation(Transformation::rotate(percentage(if open {
                        0.25
                    } else {
                        0.
                    }))),
            )
            .child(div().text_size(px(12.)).text_color(faint).child("Advanced"));
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(header)
            .when(open, |el| {
                el.child(row("Jump host", &form.proxy_jump, t))
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap(px(10.))
                            .child(div().w(px(160.)).child(row(
                                "Keep-alive (seconds, 0 = off)",
                                &form.keep_alive,
                                t,
                            )))
                            .child(
                                div()
                                    .id("form-forward-agent")
                                    .h(px(30.))
                                    .flex_1()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .gap(px(8.))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|shell, _, _, cx| {
                                        if let Some(form) = shell.form.as_mut() {
                                            form.forward_agent = !form.forward_agent;
                                        }
                                        cx.notify();
                                    }))
                                    .child(div().text_sm().child("Forward SSH agent"))
                                    .child(toggle(t, form.forward_agent, "form-forward-agent")),
                            ),
                    )
                    .child(self.forward_rows(form, cx))
                    .child(row("Vault key", &form.vault_key, t))
                    .child(row("Password command", &form.password_command, t))
                    .child(row("Sudo password command", &form.sudo_password_command, t))
            })
    }

    /// The "Port forwards" list: one row per spec with a remove button, and an add button.
    fn forward_rows(&self, form: &ConnectionForm, cx: &mut Context<Self>) -> impl IntoElement {
        let t = &self.theme;
        let mut list = div().flex().flex_col().gap(px(6.)).child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(div().text_xs().text_color(t.muted).child("Port forwards"))
                .child(
                    div()
                        .id("form-add-forward")
                        .px(px(8.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .text_xs()
                        .text_color(t.accent)
                        .cursor_pointer()
                        .hover(|s| s.bg(t.row_hover))
                        .on_click(
                            cx.listener(|shell, _, window, cx| shell.add_forward_row(window, cx)),
                        )
                        .child("+ Add"),
                ),
        );
        for (ix, input) in form.forwards.iter().enumerate() {
            list =
                list.child(
                    div()
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
                                .text_sm()
                                .child(input.clone()),
                        )
                        .child(
                            div()
                                .id(("form-remove-forward", ix))
                                .flex_none()
                                .size(px(24.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.))
                                .text_color(t.muted)
                                .cursor_pointer()
                                .hover(|s| s.bg(t.row_hover))
                                .on_click(cx.listener(move |shell, _, _, cx| {
                                    shell.remove_forward_row(ix, cx)
                                }))
                                .child("×"),
                        ),
                );
        }
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Connection {
        Connection {
            name: "web".into(),
            host: "10.0.0.5".into(),
            port: 22,
            user: "deploy".into(),
            ..Connection::default()
        }
    }

    #[test]
    fn a_plain_connection_keeps_advanced_closed() {
        assert!(!in_use(&plain()));
        // The default keep-alive spelled out is not a customisation.
        let c = Connection {
            keep_alive: Some(DEFAULT_KEEP_ALIVE),
            ..plain()
        };
        assert!(!in_use(&c));
    }

    #[test]
    fn each_advanced_value_opens_it() {
        let cases: Vec<(&str, Connection)> = vec![
            (
                "proxy_jump",
                Connection {
                    proxy_jump: "bastion".into(),
                    ..plain()
                },
            ),
            (
                "keep_alive off",
                Connection {
                    keep_alive: Some(0),
                    ..plain()
                },
            ),
            (
                "keep_alive 60",
                Connection {
                    keep_alive: Some(60),
                    ..plain()
                },
            ),
            (
                "forward_agent",
                Connection {
                    forward_agent: true,
                    ..plain()
                },
            ),
            (
                "forwards",
                Connection {
                    forwards: vec!["-L 8080:db:5432".into()],
                    ..plain()
                },
            ),
            (
                "vault_key",
                Connection {
                    vault_key: Some("deploy-key".into()),
                    ..plain()
                },
            ),
            (
                "password_command",
                Connection {
                    password_command: Some("gopass show -o x".into()),
                    ..plain()
                },
            ),
            (
                "sudo_password_command",
                Connection {
                    sudo_password_command: Some("gopass show -o y".into()),
                    ..plain()
                },
            ),
        ];
        for (what, c) in cases {
            assert!(in_use(&c), "{what} should open Advanced");
        }
    }

    #[test]
    fn essentials_alone_do_not_open_it() {
        let c = Connection {
            group: Some("prod".into()),
            tags: vec!["eu".into()],
            identity_file: Some("~/.ssh/id_ed25519".into()),
            port: 2222,
            ..plain()
        };
        assert!(!in_use(&c));
    }
}
