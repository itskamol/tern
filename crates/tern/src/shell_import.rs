// Adapted from zeron crates/ui/src/shell/command_palette.rs (palette card and overlay) (MIT).
//! "Import from ~/.ssh/config…": a sheet listing the config's hosts, and a one-time copy of
//! the checked ones into `hosts.json` as ordinary, editable connections. There is no link back:
//! later edits to `~/.ssh/config` do not reach tern, and tern never writes that file.

use crate::hover::HoverFade as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, anchored, deferred, div, point, px,
};
use tern_ssh::HostEntry;

use super::connections_ui::{button, note};
use super::{Shell, ToastKind};
use crate::connections::{self, Connection};
use crate::picker;

pub(super) struct ImportSheet {
    rows: Vec<Row>,
    focus: FocusHandle,
}

struct Row {
    host: HostEntry,
    checked: bool,
    /// A connection with this name is already in tern, so importing would collide.
    imported: bool,
}

/// The sheet's rows: every config host, ticked unless tern already has a connection by that
/// name.
fn rows(config: Vec<HostEntry>, connections: &[Connection]) -> Vec<Row> {
    config
        .into_iter()
        .map(|host| {
            let imported = connections.iter().any(|c| c.name == host.alias);
            Row {
                host,
                checked: !imported,
                imported,
            }
        })
        .collect()
}

/// The connections the checked rows become. A host without a `User` gets the local user, as
/// `ssh` would use it.
fn to_import(rows: &[Row], local_user: &str) -> Vec<Connection> {
    rows.iter()
        .filter(|r| r.checked && !r.imported)
        .map(|r| {
            let mut c = Connection::from_entry(&r.host);
            if c.user.is_empty() {
                c.user = local_user.to_owned();
            }
            c
        })
        .collect()
}

impl Shell {
    pub(crate) fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.import = Some(ImportSheet {
            rows: rows(tern_ssh::load_ssh_config_hosts(), &self.connections),
            focus,
        });
        cx.notify();
    }

    fn close_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.import.take().is_some() {
            self.restore_focus(window, cx);
        }
    }

    fn submit_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = self.import.as_ref() else {
            return;
        };
        let fresh = to_import(&sheet.rows, &std::env::var("USER").unwrap_or_default());
        if fresh.is_empty() {
            return self.close_import(window, cx);
        }
        if let Some(e) = self.store_error.clone() {
            return self.notify_toast(ToastKind::Critical, e, cx);
        }
        let mut next = self.connections.clone();
        let count = fresh.len();
        // Copied from this Mac's own ~/.ssh/config, which runs these already.
        for connection in &fresh {
            crate::password_command::approve_saved(connection);
        }
        next.extend(fresh);
        if let Some(dir) = crate::settings::dir()
            && let Err(e) = connections::save(&dir, &next)
        {
            return self.notify_toast(ToastKind::Critical, e.to_string(), cx);
        }
        self.connections = next;
        self.refresh_hosts();
        self.close_import(window, cx);
        let plural = if count == 1 { "" } else { "s" };
        self.notify_toast(
            ToastKind::Positive,
            format!("Imported {count} host{plural}"),
            cx,
        );
    }

    fn on_import_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.close_import(window, cx),
            "enter" => self.submit_import(window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    pub(super) fn render_import(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let sheet = self.import.as_ref()?;
        let t = &self.theme;
        let viewport = window.viewport_size();
        let chosen = sheet
            .rows
            .iter()
            .filter(|r| r.checked && !r.imported)
            .count();
        let mut list = div()
            .id("import-list")
            .max_h(px(picker::results_height(viewport)))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(8.))
            .py(px(8.));
        if sheet.rows.is_empty() {
            list = list.child(
                div()
                    .py(px(24.))
                    .flex()
                    .justify_center()
                    .text_sm()
                    .text_color(t.muted)
                    .child("No hosts found in ~/.ssh/config"),
            );
        }
        for (ix, r) in sheet.rows.iter().enumerate() {
            let off = r.imported;
            let on = r.checked && !off;
            list = list.child(
                div()
                    .id(("import-row", ix))
                    .px(px(8.))
                    .py(px(4.))
                    .min_h(px(30.))
                    .rounded(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .when(!off, |el| {
                        el.cursor_pointer()
                            .hover_fade(
                                format!("import-row-{ix}"),
                                gpui::transparent_black(),
                                t.row_hover,
                            )
                            .on_click(cx.listener(move |shell, _, _, cx| {
                                if let Some(row) = shell
                                    .import
                                    .as_mut()
                                    .and_then(|sheet| sheet.rows.get_mut(ix))
                                {
                                    row.checked = !row.checked;
                                }
                                cx.notify();
                            }))
                    })
                    .child(
                        div()
                            .flex_none()
                            .size(px(14.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(if on { t.accent } else { t.border })
                            .when(on, |el| el.bg(t.accent))
                            .when(off, |el| el.opacity(0.4)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .when(off, |el| el.text_color(t.faint))
                            .child(SharedString::from(r.host.alias.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(t.muted)
                            .child(SharedString::from(if off {
                                "Already in tern".to_owned()
                            } else {
                                crate::sidebar::address(&r.host)
                            })),
                    ),
            );
        }
        let label = match chosen {
            0 => "Import".to_owned(),
            n => format!("Import {n}"),
        };
        let card = div()
            .id("import-sheet")
            .track_focus(&sheet.focus)
            .w(px(520.0_f32.min(f32::from(viewport.width) - 32.0)))
            .flex()
            .flex_col()
            .rounded(px(16.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .text_color(t.text)
            .on_key_down(cx.listener(Self::on_import_key))
            .child(
                div()
                    .px(px(16.))
                    .py(px(12.))
                    .border_b_1()
                    .border_color(t.hairline)
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_size(px(14.)).child("Import from ~/.ssh/config"))
                    .child(note(
                        "Copies the checked hosts into tern once. The file is never read again or changed.",
                        t,
                    )),
            )
            .child(list)
            .child(
                div()
                    .px(px(16.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(t.hairline)
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        button("import-cancel", "Cancel", false, t).on_click(cx.listener(
                            |shell, _, window, cx| shell.close_import(window, cx),
                        )),
                    )
                    .child(
                        button("import-go", label, chosen > 0, t).on_click(cx.listener(
                            |shell, _, window, cx| shell.submit_import(window, cx),
                        )),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(alias: &str, user: Option<&str>) -> HostEntry {
        HostEntry {
            alias: alias.into(),
            host_name: format!("{alias}.example.com"),
            port: 2200,
            user: user.map(Into::into),
            identity_files: vec![PathBuf::from("/k/id")],
            proxy_command: Some("ssh -W %h:%p bastion".into()),
            ..Default::default()
        }
    }

    fn existing(name: &str) -> Connection {
        Connection::from_entry(&entry(name, Some("root")))
    }

    #[test]
    fn hosts_already_in_tern_are_disabled_and_the_rest_ticked() {
        let r = rows(
            vec![entry("web", None), entry("db", None)],
            &[existing("db")],
        );
        assert!(r[0].checked && !r[0].imported);
        assert!(!r[1].checked && r[1].imported);
    }

    #[test]
    fn import_copies_the_fields_and_skips_unchecked_and_existing() {
        let mut r = rows(
            vec![
                entry("web", Some("deploy")),
                entry("db", None),
                entry("old", None),
            ],
            &[existing("old")],
        );
        r[1].checked = false;
        // "old" is ticked by hand but already imported: still not copied.
        r[2].checked = true;
        let out = to_import(&r, "me");
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!(
            (c.name.as_str(), c.user.as_str(), c.port),
            ("web", "deploy", 2200)
        );
        assert_eq!(c.host, "web.example.com");
        assert_eq!(c.identity_file.as_deref(), Some("/k/id"));
        assert_eq!(c.proxy_command.as_deref(), Some("ssh -W %h:%p bastion"));
        // A host with no User takes the local user.
        r[1].checked = true;
        let out = to_import(&r, "me");
        assert_eq!(out[1].user, "me");
    }
}
