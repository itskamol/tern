// Adapted from zeron crates/ui/src/shell/command_palette.rs (palette card and overlay) (MIT).
//! Adding, editing and removing connections: the form, its vault step, and the list changes.

use crate::hover::HoverFade as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, anchored, deferred, div, point, px,
};
use tern_ssh::SecretString;
use tern_vault::{Key, Vault};

use super::{Shell, Toast, ToastKind};
use crate::connections::{self, Connection, DEFAULT_KEEP_ALIVE, Draft};
use crate::keeper::{Keeper, Tried, Typed};
use crate::sidebar;
use crate::text_input::{InputColors, TextInput};
use crate::theme::Theme;

#[path = "shell_connection_advanced.rs"]
mod advanced;

/// Space kept free around the form sheet, per side.
const FORM_MARGIN: f32 = 24.;
const FORWARD_HINT: &str = "-L 8080:db:5432   (-R and -D too)";

pub(super) struct ConnectionForm {
    /// Index into the shell's connections when editing; `None` for a new one.
    editing: Option<usize>,
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    key: Entity<TextInput>,
    group: Entity<TextInput>,
    tags: Entity<TextInput>,
    proxy_jump: Entity<TextInput>,
    keep_alive: Entity<TextInput>,
    forward_agent: bool,
    /// Whether the collapsed "Advanced" section is showing its fields.
    advanced_open: bool,
    /// Kept from an import; the form has no field for these.
    proxy_command: Option<String>,
    vault_key: Entity<TextInput>,
    password_command: Entity<TextInput>,
    sudo_password_command: Entity<TextInput>,
    agent_socket: Option<String>,
    /// One input per port forward, as `-L`/`-R`/`-D` text; blank rows are ignored on save.
    forwards: Vec<Entity<TextInput>>,
    password: Entity<TextInput>,
    vault_pass: Entity<TextInput>,
    vault_repeat: Entity<TextInput>,
    error: Option<String>,
    busy: bool,
    _repaint: Vec<Subscription>,
}

/// What the vault needs before a typed password can be stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VaultStep {
    None,
    Unlock,
    Create,
}

impl ConnectionForm {
    fn fields(&self) -> Vec<&Entity<TextInput>> {
        let mut all = vec![
            &self.name,
            &self.host,
            &self.port,
            &self.user,
            &self.key,
            &self.group,
            &self.tags,
            &self.proxy_jump,
            &self.keep_alive,
            &self.vault_key,
            &self.password_command,
            &self.sudo_password_command,
            &self.password,
            &self.vault_pass,
            &self.vault_repeat,
        ];
        all.extend(&self.forwards);
        all
    }
}

fn read(input: &Entity<TextInput>, cx: &gpui::App) -> String {
    input.read(cx).text().to_owned()
}

impl Shell {
    /// Opens the form: empty for a new connection, filled for an edit or a duplicate.
    pub(crate) fn open_form(
        &mut self,
        editing: Option<usize>,
        draft: Option<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let t = self.theme;
        let colors = InputColors {
            text: t.text,
            placeholder: t.faint,
            cursor: t.accent,
            selection: t.accent.opacity(0.35),
        };
        let mut field = |placeholder: &str, masked: bool, value: String| {
            cx.new(|cx| {
                let mut input = TextInput::new(placeholder.to_owned(), masked, colors, cx);
                if !value.is_empty() {
                    input.set_text(value, cx);
                }
                input
            })
        };
        let d = draft.unwrap_or(Connection {
            port: 22,
            user: std::env::var("USER").unwrap_or_default(),
            ..Connection::default()
        });
        let password_hint = if editing.is_some() {
            "Unchanged"
        } else {
            "Optional, saved to the vault"
        };
        let advanced_open = advanced::in_use(&d);
        let form = ConnectionForm {
            editing,
            advanced_open,
            name: field("production-web", false, d.name),
            host: field("10.0.0.5 or example.com", false, d.host),
            // A default port stays blank so the field looks like its neighbours; blank saves as 22.
            port: field(
                "22",
                false,
                if d.port == 22 {
                    String::new()
                } else {
                    d.port.to_string()
                },
            ),
            user: field("deploy", false, d.user),
            key: field(
                "~/.ssh/id_ed25519 (optional)",
                false,
                d.identity_file.unwrap_or_default(),
            ),
            group: field("No group", false, d.group.unwrap_or_default()),
            tags: field("comma separated, e.g. eu, db", false, d.tags.join(", ")),
            proxy_jump: field("bastion, or user@host:port (optional)", false, d.proxy_jump),
            keep_alive: field(
                "30",
                false,
                d.keep_alive.unwrap_or(DEFAULT_KEEP_ALIVE).to_string(),
            ),
            forward_agent: d.forward_agent,
            proxy_command: d.proxy_command,
            vault_key: field(
                "Name of a key in the vault (optional)",
                false,
                d.vault_key.unwrap_or_default(),
            ),
            password_command: field(
                "Prints the password, e.g. gopass show -o …",
                false,
                d.password_command.unwrap_or_default(),
            ),
            sudo_password_command: field(
                "e.g. gopass show -o …",
                false,
                d.sudo_password_command.unwrap_or_default(),
            ),
            agent_socket: d.agent_socket,
            forwards: d
                .forwards
                .iter()
                .map(|f| field(FORWARD_HINT, false, f.clone()))
                .collect(),
            password: field(password_hint, true, String::new()),
            vault_pass: field("Vault passphrase", true, String::new()),
            vault_repeat: field("Repeat vault passphrase", true, String::new()),
            error: None,
            busy: false,
            _repaint: Vec::new(),
        };
        // Typing changes which vault fields show, so the shell re-renders on every edit.
        let mut repaint: Vec<_> = form
            .fields()
            .into_iter()
            .map(|f| cx.observe(f, |_, _, cx| cx.notify()))
            .collect();
        // The vault is first needed when a password is typed; the Keychain gets its one try then.
        repaint.push(cx.observe(&form.password, |shell, password, cx| {
            if wants_keychain(password.read(cx).text()) {
                shell.unlock_from_keychain(cx);
            }
        }));
        let first = form.name.focus_handle(cx);
        self.form = Some(ConnectionForm {
            _repaint: repaint,
            ..form
        });
        self.picker = None;
        window.focus(&first, cx);
        cx.notify();
    }

    /// A blank port-forward row, focused.
    fn add_forward_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let t = self.theme;
        let colors = InputColors {
            text: t.text,
            placeholder: t.faint,
            cursor: t.accent,
            selection: t.accent.opacity(0.35),
        };
        let input = cx.new(|cx| TextInput::new(FORWARD_HINT.to_owned(), false, colors, cx));
        window.focus(&input.focus_handle(cx), cx);
        if let Some(form) = self.form.as_mut() {
            form._repaint
                .push(cx.observe(&input, |_, _, cx| cx.notify()));
            form.forwards.push(input);
        }
        cx.notify();
    }

    fn remove_forward_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(form) = self.form.as_mut()
            && ix < form.forwards.len()
        {
            form.forwards.remove(ix);
        }
        cx.notify();
    }

    pub(crate) fn connection(&self, ix: usize) -> Option<Connection> {
        self.connections.get(ix).cloned()
    }

    pub(super) fn close_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.form.take().is_some() {
            self.restore_focus(window, cx);
        }
    }

    fn vault_step(&self, cx: &gpui::App) -> VaultStep {
        let Some(form) = &self.form else {
            return VaultStep::None;
        };
        if read(&form.password, cx).is_empty() {
            VaultStep::None
        } else if Keeper::locked(cx) {
            VaultStep::Unlock
        } else if Keeper::exists(cx) {
            VaultStep::None
        } else {
            VaultStep::Create
        }
    }

    fn visible_fields(&self, cx: &gpui::App) -> Vec<FocusHandle> {
        let Some(form) = &self.form else {
            return Vec::new();
        };
        let step = self.vault_step(cx);
        let mut fields = vec![
            &form.name,
            &form.host,
            &form.port,
            &form.user,
            &form.key,
            &form.group,
            &form.tags,
            &form.password,
        ];
        // A collapsed section is skipped; its fields are not on screen to land on.
        if form.advanced_open {
            fields.extend([&form.proxy_jump, &form.keep_alive]);
            fields.extend(&form.forwards);
            fields.extend([
                &form.vault_key,
                &form.password_command,
                &form.sudo_password_command,
            ]);
        }
        if step != VaultStep::None {
            fields.push(&form.vault_pass);
        }
        if step == VaultStep::Create {
            fields.push(&form.vault_repeat);
        }
        fields.into_iter().map(|f| f.focus_handle(cx)).collect()
    }

    fn on_form_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.close_form(window, cx),
            "enter" => self.submit_form(window, cx),
            "tab" => {
                let fields = self.visible_fields(cx);
                let at = fields
                    .iter()
                    .position(|f| f.is_focused(window))
                    .unwrap_or(0);
                let delta = if event.keystroke.modifiers.shift {
                    -1
                } else {
                    1
                };
                let next = crate::tabs::step(at, fields.len(), delta);
                if let Some(f) = fields.get(next) {
                    window.focus(f, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    fn submit_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let step = self.vault_step(cx);
        let Some(form) = self.form.as_mut() else {
            return;
        };
        if form.busy {
            return;
        }
        if let Some(e) = &self.store_error {
            form.error = Some(e.clone());
            return cx.notify();
        }
        let draft = Draft {
            name: read(&form.name, cx),
            host: read(&form.host, cx),
            port: read(&form.port, cx),
            user: read(&form.user, cx),
            identity_file: read(&form.key, cx),
            group: read(&form.group, cx),
            tags: read(&form.tags, cx),
            vault_key: read(&form.vault_key, cx),
            password_command: read(&form.password_command, cx),
            sudo_password_command: read(&form.sudo_password_command, cx),
            proxy_jump: read(&form.proxy_jump, cx),
            forward_agent: form.forward_agent,
            keep_alive: read(&form.keep_alive, cx),
            forwards: form.forwards.iter().map(|f| read(f, cx)).collect(),
        };
        let editing = form.editing;
        let others: Vec<&str> = self
            .connections
            .iter()
            .enumerate()
            .filter(|(ix, _)| Some(*ix) != editing)
            .map(|(_, c)| c.name.as_str())
            .collect();
        let connection = match connections::validate(&draft, &others, &self.connections) {
            Ok(c) => Connection {
                proxy_command: form.proxy_command.clone(),
                agent_socket: form.agent_socket.clone(),
                ..c
            },
            Err(e) => {
                form.error = Some(e);
                return cx.notify();
            }
        };
        let password = read(&form.password, cx);
        let vault_pass = read(&form.vault_pass, cx);
        if step != VaultStep::None && vault_pass.is_empty() {
            form.error = Some("Enter the vault passphrase to save the password.".into());
            return cx.notify();
        }
        if step == VaultStep::Create && vault_pass != read(&form.vault_repeat, cx) {
            form.error = Some("The vault passphrases differ.".into());
            return cx.notify();
        }
        let key = Key::Password {
            user: connection.user.clone(),
            host: connection.host.clone(),
            port: connection.port,
        };
        if let Err(e) = self.store_connection(editing, connection) {
            if let Some(form) = self.form.as_mut() {
                form.error = Some(e);
            }
            return cx.notify();
        }
        if password.is_empty() {
            self.notify_toast(
                ToastKind::Positive,
                format!("Saved {}", draft.name.trim()),
                cx,
            );
            return self.close_form(window, cx);
        }
        if let Some(form) = self.form.as_mut() {
            form.busy = true;
            // The connection itself is saved; from here on the form stays open only for the
            // vault, and can be re-submitted after a wrong passphrase.
            form.editing = self
                .connections
                .iter()
                .position(|c| c.name == read(&form.name, cx).trim());
        }
        cx.notify();
        self.save_password(
            key,
            SecretString::from(password),
            SecretString::from(vault_pass),
            cx,
        );
    }

    /// Unlocks or creates the vault as needed, stores the password and saves, the scrypt work
    /// off the UI thread (about a second each). An open vault never leaves the keeper, so other
    /// tabs keep reading it while this writes, and the write is ordered with every other one.
    fn save_password(
        &mut self,
        key: Key,
        password: SecretString,
        vault_pass: SecretString,
        cx: &mut Context<Self>,
    ) {
        // A locked vault opens with the passphrase typed in the form; with no vault yet, that
        // passphrase starts one.
        let unlocking = if Keeper::locked(cx) {
            Some(Keeper::try_unlock(Typed::Passphrase(vault_pass), cx))
        } else {
            if !Keeper::exists(cx) {
                Keeper::adopt(Vault::new(vault_pass), cx);
            }
            None
        };
        cx.spawn(async move |this, cx| {
            let opened = match unlocking {
                Some(task) => match task.await {
                    Tried::Opened => Ok(()),
                    Tried::Again { message, .. } | Tried::Failed(message) => Err(message),
                },
                None => Ok(()),
            };
            let result = match opened {
                Ok(()) => match this.update(cx, |_, cx| Keeper::store(key, password, cx)) {
                    Ok(Some(task)) => task.await,
                    Ok(None) => Err("Vault: it was locked before the password was saved.".into()),
                    Err(_) => return,
                },
                Err(message) => Err(format!(
                    "{message} The connection is saved; the password is not."
                )),
            };
            let _ = this.update_in(cx, |shell, window, cx| match result {
                Ok(()) => {
                    shell.notify_toast(ToastKind::Positive, "Password saved in the vault", cx);
                    shell.close_form(window, cx);
                }
                Err(message) => {
                    if let Some(form) = shell.form.as_mut() {
                        form.busy = false;
                        form.error = Some(message);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// First click asks, second click removes. Saved passwords stay in the vault.
    pub(crate) fn delete_connection(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.confirm_delete != Some(ix) {
            self.confirm_delete = Some(ix);
            return cx.notify();
        }
        self.confirm_delete = None;
        let mut next = self.connections.clone();
        if ix < next.len() {
            next.remove(ix);
        }
        let saved = match crate::settings::dir() {
            Some(dir) => connections::save(&dir, &next).map_err(|e| e.to_string()),
            None => Ok(()),
        };
        match saved {
            Ok(()) => {
                let name = self.connections.get(ix).map(|c| c.name.clone());
                self.commit_hosts(
                    next,
                    format!("Remove {}", name.as_deref().unwrap_or("host")),
                );
                if let Some(name) = name {
                    // The same step as ⌘Z, so the toast and Edit → Undo never disagree.
                    self.toast(
                        Toast::new(ToastKind::Default, format!("Removed {name}"))
                            .action("Undo", |s, w, cx| s.undo_hosts(w, cx)),
                        cx,
                    );
                }
            }
            Err(e) => self.notify_toast(ToastKind::Critical, e, cx),
        }
        cx.notify();
    }

    pub(super) fn render_form(
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

pub(super) fn row(
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

pub(super) fn note(text: &'static str, t: &Theme) -> impl IntoElement + use<> {
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

/// A typed password is what makes the form need the vault; an empty field never asks the Keychain.
fn wants_keychain(password: &str) -> bool {
    !password.is_empty()
}

#[cfg(test)]
mod keychain_tests {
    use super::wants_keychain;

    #[test]
    fn only_a_typed_password_makes_the_form_ask_the_keychain() {
        assert!(!wants_keychain(""));
        assert!(wants_keychain("x"));
    }
}
