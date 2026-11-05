//! Settings → Vault, drawn: status, saved secrets, keys, passphrase, security. The actions
//! behind the buttons are in `shell_vault.rs`.

use gpui::prelude::FluentBuilder;
use gpui::{
    ClipboardItem, Context, Entity, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, div, px,
};
use tern_vault::Key;

use super::vault_ui::describe_entry;
use super::{Shell, ToastKind};
use crate::icons::{self, icon};
use crate::keeper::{self, Keeper};
use crate::settings_widgets as w;
use crate::text_input::TextInput;

impl Shell {
    // ---- the page -----------------------------------------------------------------------

    pub(super) fn vault_page(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let Some(ui) = &self.vault_ui else {
            return w::page_column().child(w::page_header(&t, "Vault"));
        };
        let entries = Keeper::keys(cx);
        let exists = Keeper::exists(cx);
        let (status, detail): (&str, String) = match &entries {
            Some(list) => (
                "Unlocked",
                format!(
                    "{} saved secret{}",
                    list.len(),
                    if list.len() == 1 { "" } else { "s" }
                ),
            ),
            None if exists => (
                "Locked",
                "Unlocks the first time a saved login is needed, or here".into(),
            ),
            None => (
                "No vault yet",
                "Created the first time you save a password, or here".into(),
            ),
        };
        // Only an open vault can be locked; a dimmed button that does nothing is noise.
        let lock = div().when(entries.is_some(), |el| {
            el.child(
                w::button(&t, "vault-lock", "Lock now")
                    .on_click(cx.listener(|s, _, _, cx| s.lock_vault(cx))),
            )
        });
        let mut status_card =
            w::card(&t).child(w::row(&t, true, status, Some(detail.into()), lock));
        if entries.is_none() && exists {
            let pin = Keeper::pin_set(cx);
            let by_pin = pin && !ui.use_passphrase;
            let switch = pin.then(|| {
                w::button(
                    &t,
                    "vault-unlock-switch",
                    if by_pin {
                        "Use passphrase instead"
                    } else {
                        "Use PIN instead"
                    },
                )
                .on_click(cx.listener(|s, _, _, cx| s.toggle_use_passphrase(cx)))
            });
            status_card = status_card.child(w::row(
                &t,
                false,
                "Unlock",
                Some(if by_pin {
                    "Enter the vault PIN".into()
                } else {
                    "Enter the vault passphrase".into()
                }),
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(if by_pin {
                        field(&ui.pin, &t)
                    } else {
                        field(&ui.unlock, &t)
                    })
                    .children(switch)
                    .child(
                        w::button(
                            &t,
                            "vault-unlock",
                            if ui.busy { "Unlocking…" } else { "Unlock" },
                        )
                        .on_click(cx.listener(|s, _, _, cx| s.unlock_vault(cx))),
                    ),
            ));
        }
        if !exists {
            status_card = status_card.child(w::row(
                &t,
                false,
                "Create the vault",
                Some("Choose a passphrase. It cannot be recovered if you forget it.".into()),
                // Stacked: side by side, two fields and a button squeezed the title to one
                // letter per line at the 900 px minimum window.
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(6.))
                    .child(field(&ui.new, &t))
                    .child(field(&ui.confirm, &t))
                    .child(
                        w::button(
                            &t,
                            "vault-create",
                            if ui.busy { "Creating…" } else { "Create" },
                        )
                        .on_click(cx.listener(|s, _, _, cx| s.create_vault(cx))),
                    ),
            ));
        }
        let mut page = w::page_column()
            .child(w::page_header(&t, "Vault"))
            .child(w::page_subtitle(
                &t,
                "Passwords, key passphrases and SSH keys, encrypted with your vault passphrase (age, scrypt)",
            ))
            .when_some(ui.message.clone(), |el, (ok, text)| {
                el.child(
                    w::page_subtitle(&t, SharedString::from(text)).text_color(if ok {
                        t.muted
                    } else {
                        t.danger
                    }),
                )
            })
            .child(w::section(&t, "Status", status_card));
        if let Some(list) = entries {
            page = page
                .child(w::section(
                    &t,
                    "Saved secrets",
                    self.entries_card(&list, cx),
                ))
                .child(w::section(&t, "SSH keys", self.keys_card(cx)));
        }
        if exists {
            page = page.child(w::section(
                &t,
                "Change passphrase",
                self.passphrase_card(cx),
            ));
        }
        page.child(w::section(&t, "Security", self.security_card(cx)))
    }

    fn entries_card(&self, list: &[Key], cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let mut card = w::card(&t);
        if list.is_empty() {
            return card.child(w::row(
                &t,
                true,
                "Nothing saved yet",
                Some("Saved passwords and keys appear here".into()),
                div(),
            ));
        }
        for (ix, key) in list.iter().enumerate() {
            let (title, sub) = describe_entry(key, &self.hosts);
            let group: SharedString = format!("vault-entry-{ix}").into();
            let (menu_key, trash_key, copy_key) = (key.clone(), key.clone(), key.clone());
            let is_key = matches!(key, Key::SshKey { .. });
            let actions = div()
                .flex()
                .items_center()
                .gap(px(6.))
                .when(is_key, |el| {
                    el.child(
                        w::button(&t, ("vault-copy", ix), "Copy public key").on_click(
                            cx.listener(move |s, _, _, cx| s.copy_public_key(&copy_key, cx)),
                        ),
                    )
                })
                .child(
                    div()
                        .id(("vault-delete", ix))
                        .size(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(7.))
                        .cursor_pointer()
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .hover(|s| s.bg(t.ink(0.10)))
                        .on_click(cx.listener(move |s, _, _, cx| s.delete_secret(&trash_key, cx)))
                        .child(icon(icons::TRASH).size(px(16.)).text_color(t.danger)),
                );
            card = card.child(
                div()
                    .id(("vault-entry", ix))
                    .group(group)
                    .rounded(px(8.))
                    .hover(|s| s.bg(t.row_hover))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |s, e: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            s.open_vault_menu(menu_key.clone(), e.position, cx);
                        }),
                    )
                    .child(w::row(&t, ix == 0, title, Some(sub.into()), actions)),
            );
        }
        card
    }

    fn keys_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let Some(ui) = &self.vault_ui else {
            return w::card(&t);
        };
        let mut card = w::card(&t)
            .child(w::row(
                &t,
                true,
                "Key name",
                Some("How the key is listed, and how a connection picks it".into()),
                field(&ui.key_name, &t),
            ))
            .child(w::row(
                &t,
                false,
                "Generate",
                Some("A new ed25519 key, stored only in the vault".into()),
                w::button(&t, "vault-generate", "Generate")
                    .on_click(cx.listener(|s, _, _, cx| s.generate_key(cx))),
            ))
            .child(w::row(
                &t,
                false,
                "Import a key file",
                Some("An OpenSSH private key without its own passphrase".into()),
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(field(&ui.key_path, &t))
                    .child(
                        w::button(&t, "vault-import", "Import")
                            .on_click(cx.listener(|s, _, _, cx| s.import_key(cx))),
                    ),
            ));
        if let Some((name, info)) = ui.shown_key.clone() {
            let line = info.public.clone();
            card = card.child(w::row(
                &t,
                false,
                format!("Public key of {name}"),
                Some(format!("{} · {}\n{}", info.algorithm, info.fingerprint, info.public).into()),
                w::button(&t, "vault-copy-new", "Copy").on_click(cx.listener(
                    move |s, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(line.clone()));
                        s.notify_toast(ToastKind::Positive, "Public key copied", cx);
                    },
                )),
            ));
        }
        card
    }

    fn passphrase_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let Some(ui) = &self.vault_ui else {
            return w::card(&t);
        };
        w::card(&t)
            .child(w::row(
                &t,
                true,
                "Current passphrase",
                None,
                field(&ui.current, &t),
            ))
            .child(w::row(
                &t,
                false,
                "New passphrase",
                None,
                field(&ui.new, &t),
            ))
            .child(w::row(
                &t,
                false,
                "Repeat new passphrase",
                Some("The vault is re-encrypted; the old passphrase stops working".into()),
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(field(&ui.confirm, &t))
                    .child(
                        w::button(
                            &t,
                            "vault-change",
                            if ui.busy { "Working…" } else { "Change" },
                        )
                        .on_click(cx.listener(|s, _, _, cx| s.change_passphrase(cx))),
                    ),
            ))
    }

    fn security_card(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme;
        let minutes = self.settings.vault_lock_minutes;
        let label = if minutes == 0 {
            "Off".to_owned()
        } else {
            format!("{minutes} min")
        };
        let (minus, value, plus) = w::stepper(&t, "vault-lock-after", label);
        let keychain = self.settings.vault_keychain;
        let lock_on_sleep = self.settings.vault_lock_on_sleep;
        let card = w::card(&t)
            .child(w::row(
                &t,
                true,
                "Lock vault after idle",
                Some("No key or mouse input to the window. Open sessions keep running.".into()),
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(minus.on_click(cx.listener(|s, _, _, cx| {
                        let next = keeper::step_lock(s.settings.vault_lock_minutes, -1);
                        s.update_settings(|st| st.vault_lock_minutes = next, cx);
                    })))
                    .child(value)
                    .child(plus.on_click(cx.listener(|s, _, _, cx| {
                        let next = keeper::step_lock(s.settings.vault_lock_minutes, 1);
                        s.update_settings(|st| st.vault_lock_minutes = next, cx);
                    }))),
            ))
            .child(w::row(
                &t,
                false,
                "Lock the vault when the Mac sleeps or the screen locks",
                Some("Also when another user takes over the session. Open sessions keep running.".into()),
                div()
                    .id("toggle-vault-lock-on-sleep")
                    .cursor_pointer()
                    .on_click(cx.listener(|s, _, _, cx| {
                        let on = !s.settings.vault_lock_on_sleep;
                        s.update_settings(|st| st.vault_lock_on_sleep = on, cx);
                    }))
                    .child(w::toggle(&t, lock_on_sleep, "vault-lock-on-sleep")),
            ))
            .child(w::row(
                &t,
                false,
                format!("Unlock with the {}", crate::keeper::keychain::STORE_NAME),
                Some(
                    "Stores the vault passphrase in your login Keychain and opens the vault the first time tern needs it"
                        .into(),
                ),
                div()
                    .id("toggle-vault-keychain")
                    .cursor_pointer()
                    .on_click(cx.listener(|s, _, _, cx| s.toggle_keychain(cx)))
                    .child(w::toggle(&t, keychain, "vault-keychain")),
            ));
        // A PIN seals the vault's passphrase, so there is nothing to set until a vault exists.
        card.when(Keeper::exists(cx), |c| c.child(self.pin_row(cx)))
    }

    /// "Unlock with a PIN": set it (twice, masked) or turn it off.
    fn pin_row(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = self.theme;
        let Some(ui) = &self.vault_ui else {
            return div();
        };
        if Keeper::pin_set(cx) {
            return div().child(w::row(
                &t,
                false,
                "Unlock with a PIN",
                Some(
                    "On. Opens the vault after an idle lock; 5 wrong PINs turn it off. Changing the passphrase removes it."
                        .into(),
                ),
                w::button(&t, "vault-pin-off", "Turn off")
                    .on_click(cx.listener(|s, _, _, cx| s.clear_vault_pin(cx))),
            ));
        }
        // Two rows, as the passphrase card does: two fields and a button in one row do not
        // fit the page at the narrowest window width.
        div()
            .child(w::row(
                &t,
                false,
                "Unlock with a PIN",
                Some(
                    "4 to 8 digits, kept in the Keychain on this Mac. The vault must be unlocked to set it."
                        .into(),
                ),
                field(&ui.pin_new, &t),
            ))
            .child(w::row(
                &t,
                false,
                "Repeat PIN",
                None,
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(field(&ui.pin_repeat, &t))
                    .child(
                        w::button(&t, "vault-pin-set", if ui.busy { "Working…" } else { "Set PIN" })
                            .on_click(cx.listener(|s, _, _, cx| s.set_vault_pin(cx))),
                    ),
            ))
    }
}

fn field(input: &Entity<TextInput>, t: &crate::theme::Theme) -> impl IntoElement + use<> {
    div()
        .w(px(220.))
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
        .child(input.clone())
}
