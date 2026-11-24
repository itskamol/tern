//! Adding, editing and removing connections: the form, its vault step, and the list changes.

use gpui::{
    AppContext, Context, Entity, FocusHandle, Focusable, KeyDownEvent, Subscription, Window,
};
use tern_ssh::SecretString;
use tern_vault::{Key, Vault};

use super::{Alert, Shell};
use crate::connections::{self, Connection, DEFAULT_KEEP_ALIVE, Draft};
use crate::keeper::{Keeper, Tried, Typed};
use crate::session::Session;
use crate::text_input::TextInput;

#[path = "shell_connection_advanced.rs"]
mod advanced;
#[path = "shell_connection_view.rs"]
mod view;

pub(crate) use view::button;
pub(super) use view::{note, row};

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
        let colors = t.input_colors();
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
                clipboard_always: editing
                    .and_then(|ix| self.connections.get(ix))
                    .is_some_and(|c| c.clipboard_always),
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
            self.activity(format!("Saved {}", draft.name.trim()), cx);
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
                    shell.activity("Password saved in the vault", cx);
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
                    // The same step as ⌘Z, so the Alert and Edit → Undo never disagree.
                    self.alert(
                        Alert::new(format!("Removed {name}"))
                            .action("Undo", |s, w, cx| s.undo_hosts(w, cx)),
                        cx,
                    );
                }
            }
            Err(e) => self.alert_failed(e, cx),
        }
        cx.notify();
    }

    /// "Always for this host": every open session of the host stops asking and its waiting
    /// write goes through. A saved connection keeps it across relaunches; a `~/.ssh/config`
    /// host has none, so there it lasts while those sessions do.
    pub(super) fn keep_clipboard_always(
        &mut self,
        session: &Entity<Session>,
        cx: &mut Context<Self>,
    ) {
        let alias = session.read(cx).alias().to_owned();
        if let Some(ix) = self.connections.iter().position(|c| c.name == alias) {
            let mut next = self.connections.clone();
            if let Some(c) = next.get_mut(ix) {
                c.clipboard_always = true;
            }
            if let Some(dir) = crate::settings::dir()
                && let Err(e) = connections::save(&dir, &next)
            {
                self.alert_failed(format!("Could not save {alias}: {e}"), cx);
            } else {
                self.connections = next;
            }
        }
        let others: Vec<Entity<Session>> = self
            .tabs
            .iter()
            .flat_map(|t| t.panes())
            .filter_map(|p| p.session())
            .filter(|s| *s != session && s.read(cx).alias() == alias)
            .cloned()
            .collect();
        for other in others {
            other.update(cx, |s, cx| {
                s.allow_clipboard_always();
                s.answer_clipboard(crate::clipboard::Answer::Once, cx);
            });
        }
        self.forget_decisions_of(&alias, cx);
        cx.notify();
    }
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
