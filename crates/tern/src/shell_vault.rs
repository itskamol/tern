//! Settings → Vault: what the vault holds (never the values), deleting entries with Undo,
//! keys (generate, import, public half), changing the passphrase, the idle lock and the
//! Keychain opt-in. The vault itself lives in `keeper.rs`; every scrypt run happens off the
//! UI thread.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{AppContext, ClipboardItem, Context, Entity, Subscription};
use tern_ssh::{HostEntry, SecretString};
use tern_vault::keys::{self, KeyInfo};
use tern_vault::{ExposeSecret, Key, Vault, VaultError};

use super::{Shell, Toast, ToastKind};
use crate::keeper::{self, Ask, Keeper, Tried, Typed};
use crate::lock_on_sleep::{self, Cause};
use crate::text_input::TextInput;

/// How often the idle lock looks at the clock.
const IDLE_CHECK: Duration = Duration::from_secs(10);
/// A private key file larger than this is not one.
const MAX_KEY_FILE: u64 = 64 * 1024;

pub(super) struct VaultUi {
    pub(super) unlock: Entity<TextInput>,
    pub(super) current: Entity<TextInput>,
    pub(super) new: Entity<TextInput>,
    pub(super) confirm: Entity<TextInput>,
    /// The vault PIN, entered to unlock, and entered twice to set one.
    pub(super) pin: Entity<TextInput>,
    pub(super) pin_new: Entity<TextInput>,
    pub(super) pin_repeat: Entity<TextInput>,
    /// "Use passphrase instead": the unlock row asks for the passphrase although a PIN is set.
    pub(super) use_passphrase: bool,
    pub(super) key_name: Entity<TextInput>,
    pub(super) key_path: Entity<TextInput>,
    pub(super) busy: bool,
    /// The last outcome of an action on this page: ok?, text.
    pub(super) message: Option<(bool, String)>,
    /// The key just made or imported: its name and public half.
    pub(super) shown_key: Option<(String, KeyInfo)>,
    _repaint: Vec<Subscription>,
}

/// How an entry reads in the list: a title and a line under it, never the secret. A password
/// shows the alias of the host it belongs to when a connection or `~/.ssh/config` host has
/// that address and port.
pub(super) fn describe_entry(key: &Key, hosts: &[HostEntry]) -> (String, String) {
    match key {
        Key::Password { user, host, port } => {
            let alias = hosts
                .iter()
                .find(|h| h.host_name == *host && h.port == *port)
                .map_or_else(
                    || {
                        if *port == 22 {
                            host.clone()
                        } else {
                            format!("{host}:{port}")
                        }
                    },
                    |h| h.alias.clone(),
                );
            (alias, format!("{user} · Password"))
        }
        Key::KeyPassphrase { path } => (path.clone(), "Key passphrase".into()),
        Key::SshKey { name } => (name.clone(), "SSH key".into()),
    }
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Why a new passphrase is refused, in words for the form; `None` when it is acceptable.
fn passphrase_problem(current: &str, new: &str, confirm: &str) -> Option<&'static str> {
    if current.is_empty() {
        Some("Enter the current passphrase.")
    } else if new.is_empty() {
        Some("Enter a new passphrase.")
    } else if new != confirm {
        Some("The new passphrase and its confirmation differ.")
    } else if new == current {
        Some("The new passphrase is the same as the current one.")
    } else {
        None
    }
}

fn secret(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

impl Shell {
    pub(super) fn ensure_vault_ui(&mut self, cx: &mut Context<Self>) {
        if self.vault_ui.is_some() {
            return;
        }
        let t = self.theme;
        let colors = t.input_colors();
        let mut input = |placeholder: &'static str, masked: bool| {
            cx.new(|cx| TextInput::new(placeholder, masked, colors, cx))
        };
        let ui = VaultUi {
            unlock: input("Vault passphrase", true),
            current: input("Current passphrase", true),
            new: input("New passphrase", true),
            confirm: input("Repeat new passphrase", true),
            pin: input("Vault PIN", true),
            pin_new: input("New PIN, 4 to 8 digits", true),
            pin_repeat: input("Repeat PIN", true),
            use_passphrase: false,
            key_name: input("Key name, e.g. laptop", false),
            key_path: input("~/.ssh/id_ed25519", false),
            busy: false,
            message: None,
            shown_key: None,
            _repaint: Vec::new(),
        };
        let repaint = [
            &ui.unlock,
            &ui.current,
            &ui.new,
            &ui.confirm,
            &ui.pin,
            &ui.pin_new,
            &ui.pin_repeat,
            &ui.key_name,
            &ui.key_path,
        ]
        .into_iter()
        .map(|i| cx.observe(i, |_, _, cx| cx.notify()))
        .collect();
        self.vault_ui = Some(VaultUi {
            _repaint: repaint,
            ..ui
        });
    }

    /// The first need for the vault outside a session (the connection form, the Vault page): the
    /// Keychain's one try this run. The fields show as before when it cannot open the vault.
    pub(super) fn unlock_from_keychain(&mut self, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let _ = Keeper::need_vault(cx, (), move |(), cx| {
            let _ = this.update(cx, |_, cx| cx.notify());
        });
    }

    fn vault_message(&mut self, ok: bool, text: impl Into<String>, cx: &mut Context<Self>) {
        if let Some(ui) = self.vault_ui.as_mut() {
            ui.message = Some((ok, text.into()));
        }
        cx.notify();
    }

    fn clear_inputs(&self, which: &[&Entity<TextInput>], cx: &mut Context<Self>) {
        for input in which {
            input.update(cx, |i, cx| i.set_text("", cx));
        }
    }

    // ---- unlock / create / lock -------------------------------------------------------

    /// Settings' Unlock row: the PIN while one is set (unless the person chose the passphrase),
    /// else the passphrase, tried off the UI thread.
    pub(super) fn unlock_vault(&mut self, cx: &mut Context<Self>) {
        let pin_set = Keeper::pin_set(cx);
        let Some(ui) = self.vault_ui.as_mut() else {
            return;
        };
        if ui.busy {
            return;
        }
        let (input, typed) = if pin_set && !ui.use_passphrase {
            let pin = ui.pin.read(cx).text().to_owned();
            if pin.is_empty() {
                return self.vault_message(false, "Enter the vault PIN.", cx);
            }
            (ui.pin.clone(), Typed::Pin(pin))
        } else {
            let typed = ui.unlock.read(cx).text().to_owned();
            if typed.is_empty() {
                return self.vault_message(false, "Enter the vault passphrase.", cx);
            }
            (ui.unlock.clone(), Typed::Passphrase(secret(&typed)))
        };
        ui.busy = true;
        ui.message = None;
        let work = Keeper::try_unlock(typed, cx);
        cx.spawn(async move |this, cx| {
            let tried = work.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(ui) = s.vault_ui.as_mut() {
                    ui.busy = false;
                    // A PIN that is off leaves only the passphrase to ask for.
                    ui.use_passphrase |= matches!(
                        tried,
                        Tried::Again {
                            ask: Ask::Passphrase,
                            ..
                        }
                    );
                }
                match tried {
                    Tried::Opened => {
                        s.clear_inputs(&[&input], cx);
                        s.activity("Vault unlocked", cx);
                    }
                    Tried::Again { message, .. } | Tried::Failed(message) => {
                        s.vault_message(false, message, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn toggle_use_passphrase(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.vault_ui.as_mut() {
            ui.use_passphrase = !ui.use_passphrase;
            ui.message = None;
        }
        cx.notify();
    }

    pub(super) fn create_vault(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(ui)) = (Keeper::path(cx), self.vault_ui.as_mut()) else {
            return;
        };
        if ui.busy {
            return;
        }
        let new = ui.new.read(cx).text().to_owned();
        let confirm = ui.confirm.read(cx).text().to_owned();
        if new.is_empty() {
            return self.vault_message(false, "Enter a passphrase for the new vault.", cx);
        }
        if new != confirm {
            return self.vault_message(false, "The passphrase and its confirmation differ.", cx);
        }
        ui.busy = true;
        ui.message = None;
        let work = cx.background_spawn(async move {
            let vault = Vault::new(secret(&new));
            let saved = vault.save(&path);
            (vault, saved)
        });
        cx.spawn(async move |this, cx| {
            let (vault, saved) = work.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(ui) = s.vault_ui.as_mut() {
                    ui.busy = false;
                }
                match saved {
                    Ok(()) => {
                        Keeper::put(vault, cx);
                        if let Some(ui) = &s.vault_ui {
                            let (a, b) = (ui.new.clone(), ui.confirm.clone());
                            s.clear_inputs(&[&a, &b], cx);
                        }
                        s.activity("Vault created", cx);
                    }
                    Err(e) => s.vault_message(false, format!("Could not create it: {e}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Settings -> Vault -> "Unlock with a PIN": set it (the vault must be open: its passphrase
    /// is what gets sealed).
    pub(super) fn set_vault_pin(&mut self, cx: &mut Context<Self>) {
        let Some(ui) = self.vault_ui.as_mut() else {
            return;
        };
        if ui.busy {
            return;
        }
        let pin = ui.pin_new.read(cx).text().to_owned();
        let again = ui.pin_repeat.read(cx).text().to_owned();
        if pin != again {
            return self.vault_message(false, "The PIN and its repeat differ.", cx);
        }
        let task = match Keeper::set_pin(pin, cx) {
            Ok(task) => task,
            Err(e) => return self.vault_message(false, e, cx),
        };
        if let Some(ui) = self.vault_ui.as_mut() {
            ui.busy = true;
            ui.message = None;
        }
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(ui) = s.vault_ui.as_mut() {
                    ui.busy = false;
                }
                match result {
                    Ok(()) => {
                        Keeper::pin_stored(cx);
                        if let Some(ui) = s.vault_ui.as_mut() {
                            ui.use_passphrase = false;
                            let (a, b) = (ui.pin_new.clone(), ui.pin_repeat.clone());
                            s.clear_inputs(&[&a, &b], cx);
                        }
                        s.activity("Vault PIN set", cx);
                    }
                    Err(e) => s.vault_message(false, e, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn clear_vault_pin(&mut self, cx: &mut Context<Self>) {
        match Keeper::clear_pin(cx) {
            Ok(()) => self.activity("Vault PIN removed", cx),
            Err(e) => self.alert_failed(e, cx),
        }
        cx.notify();
    }

    pub(super) fn lock_vault(&mut self, cx: &mut Context<Self>) {
        if Keeper::len(cx).is_some() {
            Keeper::lock(cx);
            if let Some(ui) = self.vault_ui.as_mut() {
                ui.shown_key = None;
            }
            self.activity("Vault locked", cx);
        }
    }

    /// Locks the vault because the Mac slept, the screen locked or the session changed hands
    /// (Settings -> Vault), the same lock as Lock now: open sessions keep running.
    pub(crate) fn lock_vault_for(&mut self, cause: Cause, cx: &mut Context<Self>) {
        if lock_on_sleep::lock(self.settings.vault_lock_on_sleep, cx) {
            if let Some(ui) = self.vault_ui.as_mut() {
                ui.shown_key = None;
            }
            self.activity(cause.message(), cx);
        }
    }

    // ---- idle lock ---------------------------------------------------------------------

    /// Locks the vault after the configured idle time. Idle means no key or mouse input to
    /// the window (`last_input`); sessions already open keep running.
    pub(super) fn watch_idle(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(IDLE_CHECK).await;
                let alive = this.update(cx, |s, cx| {
                    let minutes = s.settings.vault_lock_minutes;
                    if Keeper::len(cx).is_some()
                        && keeper::idle_expired(s.last_input.elapsed(), minutes)
                    {
                        Keeper::lock(cx);
                        s.activity(format!("Vault locked after {minutes} minutes idle"), cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    // ---- entries ------------------------------------------------------------------------

    /// Writes the open vault; a failure is shown, since the change would be lost at quit.
    fn persist_vault(&mut self, cx: &mut Context<Self>) {
        let Some(task) = Keeper::persist(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Err(e) = task.await {
                let _ = this.update(cx, |s, cx| {
                    s.alert_failed(format!("Could not save the vault: {e}"), cx)
                });
            }
        })
        .detach();
    }

    pub(super) fn delete_secret(&mut self, key: &Key, cx: &mut Context<Self>) {
        let Some(value) = Keeper::lookup(key, cx) else {
            return;
        };
        Keeper::edit(cx, |v| v.remove(key));
        self.persist_vault(cx);
        let (title, _) = describe_entry(key, &self.hosts);
        let undo_key = key.clone();
        self.alert(
            Toast::new(
                ToastKind::Default,
                format!("Removed {title} from the vault"),
            )
            .action("Undo", move |s, _, cx| {
                s.restore_secret(undo_key.clone(), value.clone(), cx)
            }),
            cx,
        );
        cx.notify();
    }

    fn restore_secret(&mut self, key: Key, value: SecretString, cx: &mut Context<Self>) {
        if Keeper::edit(cx, |v| v.set(key, value)).is_none() {
            return self.alert_failed(
                "The vault is locked: unlock it, then save the entry again",
                cx,
            );
        }
        self.persist_vault(cx);
        cx.notify();
    }

    // ---- keys ---------------------------------------------------------------------------

    fn key_name_input(&self, cx: &Context<Self>) -> String {
        self.vault_ui
            .as_ref()
            .map(|ui| ui.key_name.read(cx).text().trim().to_owned())
            .unwrap_or_default()
    }

    /// Adds a key to the open vault under `name`, or says why not.
    fn store_key(
        &mut self,
        name: &str,
        private: SecretString,
        info: KeyInfo,
        cx: &mut Context<Self>,
    ) {
        let key = Key::SshKey {
            name: name.to_owned(),
        };
        if Keeper::keys(cx).is_some_and(|keys| keys.contains(&key)) {
            return self.vault_message(
                false,
                format!("A key named \"{name}\" already exists."),
                cx,
            );
        }
        if Keeper::edit(cx, |v| v.set(key, private)).is_none() {
            return self.vault_message(false, "Unlock the vault first.", cx);
        }
        self.persist_vault(cx);
        if let Some(ui) = self.vault_ui.as_mut() {
            ui.shown_key = Some((name.to_owned(), info));
            ui.message = None;
            let (n, p) = (ui.key_name.clone(), ui.key_path.clone());
            self.clear_inputs(&[&n, &p], cx);
        }
        self.activity(format!("Key {name} saved"), cx);
        cx.notify();
    }

    pub(super) fn generate_key(&mut self, cx: &mut Context<Self>) {
        let name = self.key_name_input(cx);
        if name.is_empty() {
            return self.vault_message(false, "Give the key a name first.", cx);
        }
        if Keeper::len(cx).is_none() {
            return self.vault_message(false, "Unlock the vault first.", cx);
        }
        match keys::generate_ed25519(&format!("tern:{name}")) {
            Ok(g) => self.store_key(&name, g.private, g.info, cx),
            Err(e) => self.vault_message(false, e.to_string(), cx),
        }
    }

    pub(super) fn import_key(&mut self, cx: &mut Context<Self>) {
        let Some(ui) = &self.vault_ui else {
            return;
        };
        let path = ui.key_path.read(cx).text().trim().to_owned();
        if path.is_empty() {
            return self.vault_message(false, "Enter the path of a private key file.", cx);
        }
        if Keeper::len(cx).is_none() {
            return self.vault_message(false, "Unlock the vault first.", cx);
        }
        let file = expand_home(&path);
        let text = match std::fs::metadata(&file) {
            Ok(m) if m.len() > MAX_KEY_FILE => {
                return self.vault_message(false, "That file is too large to be a key.", cx);
            }
            Ok(_) => match std::fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) => return self.vault_message(false, format!("{path}: {e}"), cx),
            },
            Err(e) => return self.vault_message(false, format!("{path}: {e}"), cx),
        };
        let info = match keys::inspect(&text) {
            Ok(i) => i,
            Err(e) => return self.vault_message(false, e.to_string(), cx),
        };
        let typed = self.key_name_input(cx);
        let name = if typed.is_empty() {
            file.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "imported".into())
        } else {
            typed
        };
        self.store_key(&name, SecretString::from(text), info, cx);
    }

    pub(super) fn copy_public_key(&mut self, key: &Key, cx: &mut Context<Self>) {
        let Some(private) = Keeper::lookup(key, cx) else {
            return;
        };
        match keys::inspect(private.expose_secret()) {
            Ok(info) => {
                cx.write_to_clipboard(ClipboardItem::new_string(info.public));
                self.activity("Public key copied", cx);
            }
            Err(e) => self.alert_failed(e.to_string(), cx),
        }
    }

    /// The vault keys the connection named `alias` offers at login. The session looks the keys
    /// up when it dials, asking for the vault passphrase first if the vault is locked.
    pub(super) fn vault_keys_for(&self, alias: &str) -> Vec<String> {
        self.connections
            .iter()
            .find(|c| c.name == alias)
            .and_then(|c| c.vault_key.clone())
            .into_iter()
            .collect()
    }

    /// What the session for `alias` may log in with besides answering the server's prompts.
    pub(super) fn auth_for(&self, alias: &str) -> crate::password_command::Auth {
        crate::password_command::Auth {
            vault_keys: self.vault_keys_for(alias),
            password_command: self
                .connections
                .iter()
                .find(|c| c.name == alias)
                .and_then(|c| c.password_command.clone()),
            sudo_command: self
                .connections
                .iter()
                .find(|c| c.name == alias)
                .and_then(|c| c.sudo_password_command.clone()),
            proxy_command: self
                .connections
                .iter()
                .find(|c| c.name == alias)
                .and_then(|c| c.proxy_command.clone()),
        }
    }

    // ---- passphrase, Keychain -------------------------------------------------------------

    pub(super) fn change_passphrase(&mut self, cx: &mut Context<Self>) {
        let Some(ui) = self.vault_ui.as_mut() else {
            return;
        };
        if ui.busy {
            return;
        }
        let current = ui.current.read(cx).text().to_owned();
        let new = ui.new.read(cx).text().to_owned();
        let confirm = ui.confirm.read(cx).text().to_owned();
        if let Some(problem) = passphrase_problem(&current, &new, &confirm) {
            return self.vault_message(false, problem, cx);
        }
        let Some(task) = Keeper::rekey(secret(&current), secret(&new), cx) else {
            return self.vault_message(false, "There is no vault yet.", cx);
        };
        if let Some(ui) = self.vault_ui.as_mut() {
            ui.busy = true;
            ui.message = None;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(ui) = s.vault_ui.as_mut() {
                    ui.busy = false;
                }
                match result {
                    Ok(vault) => {
                        let had_pin = Keeper::pin_set(cx);
                        Keeper::passphrase_changed(cx);
                        Keeper::put(vault, cx);
                        if let Some(ui) = &s.vault_ui {
                            let all = [ui.current.clone(), ui.new.clone(), ui.confirm.clone()];
                            s.clear_inputs(&[&all[0], &all[1], &all[2]], cx);
                        }
                        let text = if had_pin {
                            "Vault passphrase changed. The PIN was removed: set it again"
                        } else {
                            "Vault passphrase changed"
                        };
                        s.activity(text, cx);
                    }
                    Err(VaultError::WrongPassphrase) => {
                        s.vault_message(false, "The current passphrase is wrong.", cx)
                    }
                    Err(e) => s.vault_message(false, format!("Could not change it: {e}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn toggle_keychain(&mut self, cx: &mut Context<Self>) {
        let on = !self.settings.vault_keychain;
        match Keeper::set_keychain(on, cx) {
            Ok(()) => {
                self.update_settings(|s| s.vault_keychain = on, cx);
                self.activity(
                    if on {
                        "The vault will open the first time tern needs it"
                    } else {
                        "Removed from the Keychain"
                    },
                    cx,
                );
            }
            Err(e) => self.alert_failed(e, cx),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn host(alias: &str, address: &str, port: u16) -> HostEntry {
        HostEntry {
            alias: alias.into(),
            host_name: address.into(),
            port,
            user: None,
            identity_files: Vec::new(),
            proxy_command: None,
            ..Default::default()
        }
    }

    fn password(user: &str, address: &str, port: u16) -> Key {
        Key::Password {
            user: user.into(),
            host: address.into(),
            port,
        }
    }

    #[test]
    fn a_password_shows_the_alias_of_the_host_with_that_address_and_port() {
        let hosts = [
            host("grape-ssh", "10.0.0.5", 2222),
            host("grape", "10.0.0.5", 22),
        ];
        assert_eq!(
            describe_entry(&password("deploy", "10.0.0.5", 22), &hosts),
            ("grape".to_owned(), "deploy · Password".to_owned())
        );
        assert_eq!(
            describe_entry(&password("deploy", "10.0.0.5", 2222), &hosts).0,
            "grape-ssh"
        );
    }

    #[test]
    fn an_unknown_address_is_shown_as_it_is_with_its_port_when_not_22() {
        assert_eq!(
            describe_entry(&password("root", "example.com", 22), &[]).0,
            "example.com"
        );
        assert_eq!(
            describe_entry(&password("root", "example.com", 2200), &[]).0,
            "example.com:2200"
        );
    }

    #[test]
    fn keys_and_passphrases_are_described_by_name_and_path_never_by_value() {
        assert_eq!(
            describe_entry(
                &Key::SshKey {
                    name: "laptop".into()
                },
                &[]
            ),
            ("laptop".to_owned(), "SSH key".to_owned())
        );
        assert_eq!(
            describe_entry(
                &Key::KeyPassphrase {
                    path: "~/.ssh/id".into()
                },
                &[]
            ),
            ("~/.ssh/id".to_owned(), "Key passphrase".to_owned())
        );
    }

    #[test]
    fn each_passphrase_mistake_gets_its_own_message() {
        assert_eq!(
            passphrase_problem("", "n", "n"),
            Some("Enter the current passphrase.")
        );
        assert_eq!(
            passphrase_problem("c", "", ""),
            Some("Enter a new passphrase.")
        );
        assert!(
            passphrase_problem("c", "n", "m")
                .unwrap()
                .contains("differ")
        );
        assert!(passphrase_problem("c", "c", "c").unwrap().contains("same"));
        assert_eq!(passphrase_problem("c", "n", "n"), None);
    }

    #[test]
    fn home_is_expanded_only_as_a_leading_tilde_slash() {
        assert_eq!(expand_home("/abs/key"), PathBuf::from("/abs/key"));
        assert_eq!(expand_home("rel/~/key"), PathBuf::from("rel/~/key"));
        assert_ne!(expand_home("~/key"), PathBuf::from("~/key"));
    }
}
