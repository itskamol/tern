// Adapted from zeron crates/ui/src/popover.rs (popover_card, menu_row, menu_separator) (MIT).
//! The right-click menu for host rows and tabs: every action on a host in one place, opened
//! at the pointer. A click outside, Escape or choosing an item closes it.

use crate::hover::HoverFade as _;
use std::rc::Rc;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    SharedString, StatefulInteractiveElement, Styled, Window, anchored, deferred, div, px,
};
use tern_ssh::{ConnectSpec, HostEntry};

use super::{Shell, ThemeTarget};
use crate::icons::{self, icon};

type Run = Rc<dyn Fn(&mut Shell, &mut Window, &mut Context<Shell>)>;

enum Item {
    Action {
        icon: &'static str,
        label: SharedString,
        destructive: bool,
        run: Run,
    },
    Separator,
}

fn action(
    icon: &'static str,
    label: impl Into<SharedString>,
    run: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> Item {
    Item::Action {
        icon,
        label: label.into(),
        destructive: false,
        run: Rc::new(run),
    }
}

fn destructive(
    icon: &'static str,
    label: impl Into<SharedString>,
    run: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> Item {
    Item::Action {
        icon,
        label: label.into(),
        destructive: true,
        run: Rc::new(run),
    }
}

pub(super) struct ContextMenu {
    position: Point<Pixels>,
    items: Vec<Item>,
}

impl Shell {
    /// Right-click on a host row; `editable` is its index among tern's connections.
    pub(crate) fn open_host_menu(
        &mut self,
        host: &HostEntry,
        editable: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let alias = host.alias.clone();
        let connect = host.clone();
        let mut items = vec![action(icons::TERMINAL, "Connect", move |s, w, cx| {
            s.connect_host(connect.clone(), w, cx)
        })];
        let fresh = host.clone();
        items.push(action(icons::PLUS, "Open in new tab", move |s, w, cx| {
            s.open_new_tab(&fresh, w, cx)
        }));
        items.push(Item::Separator);
        items.push(action(icons::PEN, "Edit…", move |s, w, cx| {
            let draft = s.connection(editable);
            s.open_form(Some(editable), draft, w, cx);
        }));
        let theme_alias = alias.clone();
        items.push(action(icons::PALETTE, "Theme…", move |s, w, cx| {
            s.open_theme_picker(ThemeTarget::Host(theme_alias.clone()), w, cx)
        }));
        items.push(Item::Separator);
        // The menu is already a deliberate second step, so remove at once; Undo covers it.
        items.push(destructive(icons::TRASH, "Remove", move |s, _, cx| {
            s.confirm_delete = Some(editable);
            s.delete_connection(editable, cx);
        }));
        self.context_menu = Some(ContextMenu { position, items });
        cx.notify();
    }

    /// Right-click on a tab.
    pub(crate) fn open_tab_menu(
        &mut self,
        ix: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(ix) else {
            return;
        };
        let alias = tab.alias.clone();
        let Some(session) = tab.session() else {
            // A Files tab has nothing of a session's to offer but closing.
            let items = vec![destructive(icons::CLOSE, "Close tab", move |s, w, cx| {
                s.close_tab_at(ix, w, cx)
            })];
            self.context_menu = Some(ContextMenu { position, items });
            return cx.notify();
        };
        let closed = session.read(cx).status().is_dormant();
        let logging = session.read(cx).is_logging();
        let broadcasting = self.is_broadcasting(tab.id);
        let mut items = Vec::new();
        if closed {
            items.push(action(icons::RESTART, "Reconnect", move |s, w, cx| {
                if let Some(session) = s.tabs.get(ix).and_then(|t| t.session().cloned()) {
                    session.update(cx, |s, cx| s.reconnect(cx));
                }
                s.show_tab(ix, w, cx);
            }));
        }
        let again = alias.clone();
        items.push(action(
            icons::PLUS,
            "New tab to this host",
            move |s, w, cx| match s.hosts.iter().find(|h| h.alias == again).cloned() {
                Some(host) => s.open_new_tab(&host, w, cx),
                None => s.connect_target(&again, w, cx),
            },
        ));
        items.push(action(icons::PEN, "Rename…", move |s, w, cx| {
            s.start_rename(ix, w, cx)
        }));
        if broadcasting {
            items.push(action(icons::CLOSE, "Stop broadcasting", |s, _, cx| {
                s.stop_broadcast(cx)
            }));
        }
        items.push(action(
            icons::KEYBOARD,
            "Broadcast input to…",
            move |s, _, cx| s.open_broadcast_picker(ix, position, cx),
        ));
        if logging {
            items.push(action(icons::INFO, "Stop logging", move |s, _, cx| {
                s.stop_logging(ix, cx)
            }));
        } else {
            items.push(action(icons::INFO, "Start logging…", move |s, _, cx| {
                s.start_logging(ix, cx)
            }));
        }
        items.push(action(icons::FOLDER, "Open Files tab", move |s, w, cx| {
            s.open_files_tab(Some(ix), w, cx)
        }));
        items.push(action(icons::GLOBE, "Port forwards…", move |s, w, cx| {
            s.open_forwards_panel(ix, position, w, cx)
        }));
        let theme_alias = alias.clone();
        items.push(action(icons::PALETTE, "Theme…", move |s, w, cx| {
            s.open_theme_picker(ThemeTarget::Host(theme_alias.clone()), w, cx)
        }));
        items.push(Item::Separator);
        items.push(destructive(icons::CLOSE, "Close tab", move |s, w, cx| {
            s.close_tab_at(ix, w, cx)
        }));
        if self.tabs.len() > 1 {
            items.push(destructive(
                icons::CLOSE,
                "Close other tabs",
                move |s, w, cx| s.close_other_tabs(ix, w, cx),
            ));
        }
        self.context_menu = Some(ContextMenu { position, items });
        cx.notify();
    }

    /// The button at the end of an overflowing tab strip: every tab by name, the one in front
    /// checked, so a tab scrolled out of sight is one click away.
    pub(crate) fn open_tabs_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let items = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let name = crate::tabs::middle_ellipsis(tab.title().unwrap_or(&tab.alias), 40);
                let label = if ix == self.tabs.active() {
                    format!("{name}  ✓")
                } else {
                    name
                };
                action(icons::TERMINAL, label, move |s, w, cx| {
                    s.show_tab(ix, w, cx)
                })
            })
            .collect();
        self.context_menu = Some(ContextMenu { position, items });
        cx.notify();
    }

    /// Right-click on an entry in Settings → Vault.
    pub(crate) fn open_vault_menu(
        &mut self,
        key: tern_vault::Key,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let mut items = Vec::new();
        if matches!(key, tern_vault::Key::SshKey { .. }) {
            let copy = key.clone();
            items.push(action(icons::COPY, "Copy public key", move |s, _, cx| {
                s.copy_public_key(&copy, cx)
            }));
            items.push(Item::Separator);
        }
        items.push(destructive(icons::TRASH, "Delete", move |s, _, cx| {
            s.delete_secret(&key, cx)
        }));
        self.context_menu = Some(ContextMenu { position, items });
        cx.notify();
    }

    /// A second session to the same host, even when one is already open.
    fn open_new_tab(&mut self, host: &HostEntry, window: &mut Window, cx: &mut Context<Self>) {
        match ConnectSpec::from_host_entry(host) {
            Ok(spec) => self.open_tab(super::Launch::Ssh(spec), host.alias.clone(), window, cx),
            Err(e) => self.notify_toast(super::ToastKind::Critical, e.to_string(), cx),
        }
    }

    fn close_other_tabs(&mut self, keep: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.close_others(keep).is_empty() {
            return;
        }
        self.sync_broadcast(cx);
        self.front_tab_changed(window, cx);
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.context_menu.as_ref()?;
        let t = self.theme;
        let mut card = div()
            .id("context-menu")
            .occlude()
            .min_w(px(216.))
            .max_w(px(360.))
            .max_h(px(480.))
            .overflow_y_scroll()
            .p(px(4.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(12.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .shadow_lg()
            .text_size(px(13.))
            .text_color(t.text)
            .on_mouse_down_out(cx.listener(|s, _, _, cx| s.close_menu(cx)));
        for (ix, item) in menu.items.iter().enumerate() {
            card = match item {
                Item::Separator => {
                    card.child(div().h(px(1.)).mx(px(-4.)).my(px(2.)).bg(t.hairline))
                }
                Item::Action {
                    icon: glyph,
                    label,
                    destructive,
                    run,
                } => {
                    let run = run.clone();
                    card.child(
                        div()
                            .id(("menu-item", ix))
                            .px(px(8.))
                            .py(px(6.))
                            .rounded(px(7.))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .cursor_pointer()
                            .text_color(t.text.opacity(0.9))
                            .when(*destructive, |el| el.text_color(t.danger))
                            .hover_fade(
                                format!("menu-item-{ix}"),
                                gpui::transparent_black(),
                                t.row_active,
                            )
                            .on_click(cx.listener(move |s, _, w, cx| {
                                s.context_menu = None;
                                run(s, w, cx);
                                cx.notify();
                            }))
                            .child(icon(glyph).size(px(16.)).text_color(if *destructive {
                                t.danger
                            } else {
                                t.muted
                            }))
                            .child(label.clone()),
                    )
                }
            };
        }
        Some(
            deferred(
                anchored()
                    .position(menu.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(card),
            )
            .priority(2)
            .into_any_element(),
        )
    }

    /// The terminal font menu: the monospace families installed on this Mac, by name.
    pub(crate) fn open_font_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let current = self.settings.terminal_font_family.clone();
        let mut names = monospace_families(cx.text_system().all_font_names());
        names.retain(|n| n != crate::theme::MONO_FONT);
        names.insert(0, crate::theme::MONO_FONT.to_owned());
        let items = names
            .into_iter()
            .map(|name| {
                let chosen = current.as_deref().unwrap_or(crate::theme::MONO_FONT) == name;
                let label = if chosen {
                    format!("{name}  ✓")
                } else {
                    name.clone()
                };
                action(icons::TERMINAL, label, move |s, _, cx| {
                    let family = (name != crate::theme::MONO_FONT).then(|| name.clone());
                    s.set_terminal_font(family, cx);
                })
            })
            .collect();
        self.context_menu = Some(ContextMenu { position, items });
        cx.notify();
    }
}

/// Families a terminal can use: fixed-width by name (gpui exposes no monospace flag). Nerd Font
/// "Propo" variants are proportional and left out; capped so the menu fits on screen.
fn monospace_families(all: Vec<String>) -> Vec<String> {
    const KNOWN: [&str; 6] = [
        "Menlo",
        "Monaco",
        "Courier",
        "Courier New",
        "SF Mono",
        "PT Mono",
    ];
    let mut names: Vec<String> = all
        .into_iter()
        .filter(|n| {
            (n.contains("Mono") || n.contains("Code") || KNOWN.contains(&n.as_str()))
                && !n.contains("Propo")
                && !n.starts_with('.')
        })
        .collect();
    names.sort();
    names.dedup();
    names.truncate(30);
    names
}

#[cfg(test)]
mod font_tests {
    use super::monospace_families;

    #[test]
    fn keeps_fixed_width_families_and_drops_proportional_nerd_variants() {
        let all = [
            "JetBrainsMono Nerd Font Mono",
            "JetBrainsMono Nerd Font Propo",
            "Helvetica",
            "Menlo",
            "Fira Code",
            ".SF NS Mono",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            monospace_families(all),
            ["Fira Code", "JetBrainsMono Nerd Font Mono", "Menlo"]
        );
    }
}
