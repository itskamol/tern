//! The app's one vault: where it lives and, once unlocked, the open [`Vault`] every tab reads.
//! Unlocking and saving run scrypt (about a second), so both happen off the UI thread.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Duration;

use gpui::{App, AppContext, Global, Task};
use tern_ssh::SecretString;
use tern_vault::{Key, Vault, VaultError};

pub use crate::keeper_gate::{Ask, Opening, Pending, Tried, Typed};
use crate::keeper_gate::{
    Gate, Need, Read, Waiter, adopt, open_typed, open_with, store_due, tried,
};
use crate::vault_pin;

pub struct Keeper {
    path: Option<PathBuf>,
    open: Option<Vault>,
    /// The Keychain opt-in (Settings -> Vault) is on.
    keychain: bool,
    /// The run's one Keychain attempt: whether it ran, is running, and who waits for it.
    gate: Gate<Waiter>,
    /// The Keychain item already holds the current passphrase, so `put` need not write it.
    keychain_current: bool,
    /// Numbers each write in the order it was asked for.
    ticket: u64,
    /// A PIN is set: the Keychain holds the passphrase sealed to it (see `vault_pin`).
    pin: bool,
    /// How many times a lock was asked for, open vault or not: an open that began before one
    /// is stale and must not reopen the vault.
    locks: u64,
}

impl Global for Keeper {}

/// The newest write that reached the disk. Writes run on background threads and could finish
/// out of order; an older one must never overwrite a newer one.
static WRITTEN: Mutex<u64> = Mutex::new(0);

/// Runs `write` only if no newer write (higher `ticket`) has already run; `None` when skipped.
fn in_order<T>(ticket: u64, write: impl FnOnce() -> T) -> Option<T> {
    ordered(&WRITTEN, ticket, write)
}

/// [`in_order`] against any counter, so a test can use its own.
fn ordered<T>(written: &Mutex<u64>, ticket: u64, write: impl FnOnce() -> T) -> Option<T> {
    let mut last = written.lock().unwrap_or_else(|e| e.into_inner());
    if ticket < *last {
        return None;
    }
    *last = ticket;
    Some(write())
}

impl Keeper {
    /// A keeper over the vault file at `path`, locked, with the Keychain and PIN off: what a
    /// session scenario test starts from.
    #[cfg(test)]
    pub(crate) fn install_at(path: PathBuf, cx: &mut App) {
        cx.set_global(Self {
            path: Some(path),
            open: None,
            keychain: false,
            gate: Gate::default(),
            keychain_current: false,
            ticket: 0,
            pin: false,
            locks: 0,
        });
    }

    pub fn install(keychain: bool, cx: &mut App) {
        let path = crate::settings::dir().map(|d| d.join("vault.age"));
        cx.set_global(Self {
            path,
            open: None,
            keychain,
            gate: Gate::default(),
            keychain_current: false,
            ticket: 0,
            // From the local mark, not the Keychain item: reading that asks macOS first.
            pin: crate::settings::dir().is_some_and(|d| crate::vault_pin::marked(&d)),
            locks: 0,
        });
    }

    /// The vault is needed for `work`. With the Keychain opt-in on, the first need in this run
    /// reads the passphrase off the UI thread and opens the vault; `work` is held meanwhile and
    /// handed to `then` when the read answers, open or not (`Waiting`). A need while that read
    /// runs waits for it too. Otherwise `work` comes straight back with what to do: go on (the
    /// vault is open) or ask for the PIN when one is set, else the passphrase.
    pub fn need_vault<T: 'static>(
        cx: &mut App,
        work: T,
        then: impl FnOnce(T, &mut App) + 'static,
    ) -> Pending<T> {
        if cx.global::<Self>().open.is_some() {
            return Pending::Now(Opening::Open, work);
        }
        let slot = Rc::new(RefCell::new(Some(work)));
        let held = slot.clone();
        let waiting = Self::keychain_then(cx, move |cx| {
            if let Some(work) = held.borrow_mut().take() {
                then(work, cx);
            }
        });
        // Not waiting, the closure (and `held`) was dropped: the work is only in `slot`.
        let work = slot.borrow_mut().take();
        match work {
            Some(work) if !waiting => Pending::Now(
                Opening::Ask(if Self::pin_set(cx) {
                    Ask::Pin
                } else {
                    Ask::Passphrase
                }),
                work,
            ),
            Some(work) => {
                *slot.borrow_mut() = Some(work);
                Pending::Waiting
            }
            None => Pending::Waiting,
        }
    }

    /// Tries a typed PIN or passphrase off the UI thread and takes the result in. The vault it
    /// opens is kept, unless one is open already (it may hold edits) or a lock came since. A PIN
    /// that was wiped or is gone is forgotten, so the passphrase is asked next.
    pub fn try_unlock(typed: Typed, cx: &mut App) -> Task<Tried> {
        let k = cx.global::<Self>();
        let (since, path, dir) = (k.locks, k.path.clone(), crate::settings::dir());
        let work = cx.background_spawn(async move {
            match path {
                Some(path) => open_typed(typed, &path, dir.as_deref(), &vault_pin::KeychainStore),
                None => vault_pin::Outcome::Failed("There is no settings folder.".into()),
            }
        });
        cx.spawn(async move |cx| {
            let outcome = work.await;
            cx.update(|cx| Self::unlock_done(outcome, since, cx))
        })
    }

    fn unlock_done(outcome: vault_pin::Outcome, since: u64, cx: &mut App) -> Tried {
        if matches!(
            outcome,
            vault_pin::Outcome::Wiped | vault_pin::Outcome::Gone
        ) {
            cx.global_mut::<Self>().pin = false;
        }
        let (tried, vault) = tried(outcome, cx.global::<Self>().locks != since);
        if let Some(vault) = vault {
            Self::adopt(vault, cx);
        }
        tried
    }

    /// The first need for a locked vault in this run: with the Keychain opt-in on, reads the
    /// passphrase and opens the vault off the UI thread, then runs `then`. Returns `true` when
    /// `then` will run once the attempt resolves, whether this call started it or joined one in
    /// flight; the vault is in the keeper by then if it opened. Returns `false` when the Keychain
    /// is not to be asked (off, already asked this run, nothing to open) and `then` is dropped:
    /// the caller prompts as usual. A missing item, a refusal or a stale passphrase say nothing.
    fn keychain_then(cx: &mut App, then: impl FnOnce(&mut App) + 'static) -> bool {
        let locked = Self::locked(cx);
        let k = cx.global_mut::<Self>();
        let Some(path) = k.path.clone() else {
            return false;
        };
        match k.gate.need(k.keychain, locked, Box::new(then)) {
            Need::Skip => false,
            Need::Wait => true,
            Need::Start => {
                let since = k.locks;
                let work =
                    cx.background_spawn(async move { open_with(&keychain::Passphrase, &path) });
                cx.spawn(async move |cx| {
                    let read = work.await;
                    cx.update(|cx| Self::keychain_done(read, since, cx));
                })
                .detach();
                true
            }
        }
    }

    /// `since` is the lock count when the read began; a lock after it (the Mac slept while the
    /// Keychain dialog was up) keeps the vault shut, and the copy read is dropped, which wipes it.
    fn keychain_done(read: Read, since: u64, cx: &mut App) {
        let k = cx.global_mut::<Self>();
        let found = read.vault.filter(|_| k.locks == since);
        if adopt(&mut k.open, found) {
            k.keychain_current = true;
        }
        for waiter in k.gate.finish(read.refused) {
            waiter(cx);
        }
    }

    pub fn path(cx: &App) -> Option<PathBuf> {
        cx.global::<Self>().path.clone()
    }

    /// A vault file exists but has not been unlocked in this run.
    pub fn locked(cx: &App) -> bool {
        let k = cx.global::<Self>();
        k.open.is_none() && k.path.as_ref().is_some_and(|p| p.exists())
    }

    pub fn exists(cx: &App) -> bool {
        let k = cx.global::<Self>();
        k.open.is_some() || k.path.as_ref().is_some_and(|p| p.exists())
    }

    pub fn lookup(key: &Key, cx: &App) -> Option<SecretString> {
        cx.global::<Self>().open.as_ref()?.get(key).cloned()
    }

    /// Number of saved secrets, when the vault is open.
    pub fn len(cx: &App) -> Option<usize> {
        cx.global::<Self>().open.as_ref().map(Vault::len)
    }

    /// The saved entries, when the vault is open. Keys only, never values.
    pub fn keys(cx: &App) -> Option<Vec<Key>> {
        let v = cx.global::<Self>().open.as_ref()?;
        Some(v.keys().cloned().collect())
    }

    /// Forgets the unlocked vault, zeroing its secrets; the next use asks for the passphrase
    /// again. Open sessions are untouched: they hold no vault data. An open still running
    /// (a Keychain read) does not reopen it. Returns whether a vault was open.
    pub fn lock(cx: &mut App) -> bool {
        let k = cx.global_mut::<Self>();
        k.locks += 1;
        k.open.take().map(|mut vault| vault.wipe()).is_some()
    }

    /// Hands an unlocked vault back. With the Keychain opt-in on, the passphrase is stored
    /// the first time (and again after it changes).
    pub fn put(vault: Vault, cx: &mut App) {
        let k = cx.global_mut::<Self>();
        if store_due(
            k.keychain,
            k.keychain_current,
            k.gate.refused,
            k.gate.busy(),
        ) {
            match keychain::store(vault.passphrase()) {
                Ok(()) => k.keychain_current = true,
                Err(e) => tracing::warn!(error = %e, "vault_keychain_store_failed"),
            }
        }
        k.open = Some(vault);
    }

    /// Keeps a vault opened or created away from the keeper (a typed passphrase, a new vault),
    /// unless one is open already: that one may hold edits the other lacks, so it stays and
    /// the other is dropped, which wipes it.
    pub fn adopt(vault: Vault, cx: &mut App) {
        if cx.global::<Self>().open.is_none() {
            Self::put(vault, cx);
        }
    }

    /// Stores `secret` under `key` in the open vault and writes it, in order with every other
    /// write. The vault stays in the keeper throughout, so other tabs keep reading it instead
    /// of finding it locked while scrypt runs. `None` when the vault is locked.
    pub fn store(key: Key, secret: SecretString, cx: &mut App) -> Option<Task<Result<(), String>>> {
        Self::edit(cx, |v| v.set(key, secret))?;
        Self::persist(cx)
    }

    /// A copy of the open vault for work off the UI thread that may replace it (sync).
    pub fn snapshot(cx: &App) -> Option<Vault> {
        cx.global::<Self>().open.as_ref().map(Vault::snapshot)
    }

    /// How many writes were asked for so far: compare before and after to tell whether the
    /// open vault changed meanwhile.
    pub fn writes(cx: &App) -> u64 {
        cx.global::<Self>().ticket
    }

    /// Takes back the vault a sync ends with. `opened` is true when the sync unlocked the vault
    /// itself (it was locked when the sync began), `downloaded` when the vault is the remote's
    /// and has replaced the file, and `since` is [`Keeper::writes`] when the sync began.
    pub fn sync_done(vault: Vault, opened: bool, downloaded: bool, since: u64, cx: &mut App) {
        let k = cx.global::<Self>();
        if k.open.is_none() {
            // Locked meanwhile (idle lock, Lock now): the sync's own copy does not reopen it.
            if opened {
                Self::put(vault, cx);
            }
            return;
        }
        if !downloaded {
            return;
        }
        if k.ticket == since {
            Self::put(vault, cx);
            return;
        }
        // Written here while the sync ran: the download replaced the file under that write,
        // so the local vault is written again and the next sync carries it up.
        tracing::warn!("vault_changed_during_sync");
        if let Some(task) = Self::persist(cx) {
            task.detach();
        }
    }

    /// Changes the open vault in place; the caller then calls [`Keeper::persist`].
    pub fn edit<R>(cx: &mut App, change: impl FnOnce(&mut Vault) -> R) -> Option<R> {
        cx.global_mut::<Self>().open.as_mut().map(change)
    }

    /// The Keychain opt-in: stores the open vault's passphrase now, or removes the item.
    ///
    /// # Errors
    /// A message for the person when the vault is not open or the Keychain refuses.
    pub fn set_keychain(on: bool, cx: &mut App) -> Result<(), String> {
        let k = cx.global_mut::<Self>();
        if on {
            let Some(vault) = k.open.as_ref() else {
                return Err("Unlock the vault first: its passphrase is what gets stored.".into());
            };
            keychain::store(vault.passphrase())?;
            k.keychain = true;
            k.keychain_current = true;
            // The person just wrote the item themselves: an earlier refusal no longer holds.
            k.gate.refused = false;
        } else {
            keychain::delete()?;
            k.keychain = false;
            k.keychain_current = false;
        }
        Ok(())
    }

    /// The vault was re-sealed under a new passphrase: the Keychain item, if any, is stale,
    /// and so is the PIN's sealed copy, which is deleted: the PIN must be set again.
    pub fn passphrase_changed(cx: &mut App) {
        let k = cx.global_mut::<Self>();
        k.keychain_current = false;
        if k.pin {
            k.pin = false;
            if let Some(dir) = crate::settings::dir()
                && let Err(e) = vault_pin::clear(&dir, &vault_pin::KeychainStore)
            {
                tracing::warn!(error = %e, "vault_pin_clear_failed");
            }
        }
    }

    /// Unlocking asks for the PIN first: one is set and there is a vault to open.
    pub fn pin_set(cx: &App) -> bool {
        cx.global::<Self>().pin && Self::exists(cx)
    }

    /// Seals the open vault's passphrase to `pin` off the UI thread. Call
    /// [`Keeper::pin_stored`] when the task succeeds.
    ///
    /// # Errors
    /// A message for the person when the vault is locked or the PIN is not 4 to 8 digits.
    pub fn set_pin(pin: String, cx: &mut App) -> Result<Task<Result<(), String>>, String> {
        if let Some(problem) = tern_vault::pin::problem(&pin) {
            return Err(problem.into());
        }
        let Some(vault) = cx.global::<Self>().open.as_ref() else {
            return Err("Unlock the vault first: its passphrase is what the PIN seals.".into());
        };
        let passphrase = vault.passphrase().clone();
        let dir = crate::settings::dir().ok_or("There is no settings folder.")?;
        Ok(cx.background_spawn(async move {
            vault_pin::set(&dir, &passphrase, &pin, &vault_pin::KeychainStore)
        }))
    }

    /// The sealed passphrase is in the Keychain.
    pub fn pin_stored(cx: &mut App) {
        cx.global_mut::<Self>().pin = true;
    }

    /// Turns the PIN off and deletes its Keychain item.
    ///
    /// # Errors
    /// A message when the Keychain refuses.
    pub fn clear_pin(cx: &mut App) -> Result<(), String> {
        let dir = crate::settings::dir().ok_or("There is no settings folder.")?;
        vault_pin::clear(&dir, &vault_pin::KeychainStore)?;
        cx.global_mut::<Self>().pin = false;
        Ok(())
    }

    /// Writes a copy of the open vault off the UI thread. Writes land in the order asked.
    pub fn persist(cx: &mut App) -> Option<Task<Result<(), String>>> {
        let k = cx.global_mut::<Self>();
        let path = k.path.clone()?;
        let snapshot = k.open.as_ref()?.snapshot();
        k.ticket += 1;
        let ticket = k.ticket;
        Some(cx.background_spawn(async move {
            in_order(ticket, || snapshot.save(&path))
                .unwrap_or(Ok(()))
                .map_err(|e| e.to_string())
        }))
    }

    /// Re-seals the vault file under a new passphrase off the UI thread, in order with other
    /// writes. Returns the vault, now open under the new passphrase.
    pub fn rekey(
        current: SecretString,
        new: SecretString,
        cx: &mut App,
    ) -> Option<Task<Result<Vault, VaultError>>> {
        let k = cx.global_mut::<Self>();
        let path = k.path.clone()?;
        k.ticket += 1;
        let ticket = k.ticket;
        Some(cx.background_spawn(async move {
            in_order(ticket, || Vault::change_passphrase(&path, current, new))
                .unwrap_or(Err(VaultError::WrongPassphrase))
        }))
    }
}

/// Whether a vault idle for `idle` should lock; `minutes` is the setting, 0 meaning never.
pub fn idle_expired(idle: Duration, minutes: u32) -> bool {
    minutes > 0 && idle >= Duration::from_secs(u64::from(minutes) * 60)
}

/// The idle-lock choice `delta` steps from `current` along the settings choices; stays at the
/// ends.
pub fn step_lock(current: u32, delta: isize) -> u32 {
    let choices = crate::settings::VAULT_LOCK_CHOICES;
    let at = choices
        .iter()
        .position(|&m| m == current)
        .unwrap_or(crate::settings::VAULT_LOCK_DEFAULT_INDEX);
    choices[at.saturating_add_signed(delta).min(choices.len() - 1)]
}

pub(crate) mod keychain {
    //! The vault passphrase in the platform credential store, for the opt-in Keychain unlock
    //! setting: the login Keychain on macOS, Credential Manager on Windows, the Secret Service
    //! (GNOME Keyring, KWallet) on Linux.
    //!
    //! One generic-password item, service `tern vault` (`tern vault (debug)` in debug builds, so a
    //! development run never touches the real item). The passphrase goes nowhere else and is never
    //! logged: errors carry the store's status text only.

    use crate::keeper_gate::Load;
    use tern_vault::{ExposeSecret, SecretString};

    const ACCOUNT: &str = "passphrase";

    /// The vault passphrase item, as a source for `crate::keeper_gate::Passphrases`.
    pub struct Passphrase;

    /// The native store, behind three calls. `None` from `get` means "no such item".
    #[cfg(target_os = "macos")]
    mod backend {
        use security_framework::passwords::{
            delete_generic_password, get_generic_password, set_generic_password,
        };

        /// `errSecItemNotFound`.
        const NOT_FOUND: i32 = -25300;

        pub fn set(service: &str, account: &str, secret: &[u8]) -> Result<(), String> {
            set_generic_password(service, account, secret).map_err(|e| e.to_string())
        }

        pub fn get(service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
            match get_generic_password(service, account) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(e) if e.code() == NOT_FOUND => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }

        pub fn delete(service: &str, account: &str) -> Result<(), String> {
            match delete_generic_password(service, account) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == NOT_FOUND => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    mod backend {
        fn entry(service: &str, account: &str) -> Result<keyring::Entry, String> {
            keyring::Entry::new(service, account).map_err(|e| e.to_string())
        }

        pub fn set(service: &str, account: &str, secret: &[u8]) -> Result<(), String> {
            entry(service, account)?
                .set_secret(secret)
                .map_err(|e| e.to_string())
        }

        pub fn get(service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
            match entry(service, account)?.get_secret() {
                Ok(bytes) => Ok(Some(bytes)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }

        pub fn delete(service: &str, account: &str) -> Result<(), String> {
            match entry(service, account)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }

    /// What the store is called on this OS, for messages.
    pub const STORE_NAME: &str = if cfg!(target_os = "macos") {
        "Keychain"
    } else if cfg!(target_os = "windows") {
        "Credential Manager"
    } else {
        "Secret Service"
    };

    fn set(service: &str, secret: &[u8]) -> Result<(), String> {
        backend::set(service, ACCOUNT, secret).map_err(|e| format!("{STORE_NAME}: {e}"))
    }

    /// The item's bytes; `None` when there is none or the store refuses (logged under `event`).
    fn get(service: &str, event: &'static str) -> Option<Vec<u8>> {
        match backend::get(service, ACCOUNT) {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!(error = %e, event);
                None
            }
        }
    }

    /// Removes an item; one that is already gone counts as removed.
    fn remove(service: &str) -> Result<(), String> {
        backend::delete(service, ACCOUNT).map_err(|e| format!("{STORE_NAME}: {e}"))
    }

    pub fn service() -> &'static str {
        if cfg!(debug_assertions) {
            "tern vault (debug)"
        } else {
            "tern vault"
        }
    }

    /// Stores or replaces the passphrase.
    pub fn store(passphrase: &SecretString) -> Result<(), String> {
        set(service(), passphrase.expose_secret().as_bytes())
    }

    /// The stored passphrase, told apart from an item that is not there and a store that refuses.
    pub(crate) fn read() -> Load {
        match backend::get(service(), ACCOUNT) {
            Ok(Some(bytes)) => String::from_utf8(bytes)
                .map(|p| Load::Found(SecretString::from(p)))
                .unwrap_or(Load::Missing),
            Ok(None) => Load::Missing,
            Err(e) => {
                tracing::warn!(error = %e, event = "vault_keychain_read_failed");
                Load::Refused
            }
        }
    }

    /// Removes the item; an item that is already gone counts as removed.
    pub fn delete() -> Result<(), String> {
        remove(service())
    }

    // ---- the vault PIN ------------------------------------------------------------------------

    /// The vault passphrase sealed to the PIN (see `vault_pin`), in its own item: service
    /// `tern vault pin` (`tern vault pin (debug)` in debug builds). The PIN opens nothing without
    /// this item, and the item is useless without the PIN.
    pub fn pin_service() -> &'static str {
        if cfg!(debug_assertions) {
            "tern vault pin (debug)"
        } else {
            "tern vault pin"
        }
    }

    pub fn store_pin_blob(blob: &[u8]) -> Result<(), String> {
        set(pin_service(), blob)
    }

    pub fn load_pin_blob() -> Option<Vec<u8>> {
        get(pin_service(), "vault_pin_keychain_read_failed")
    }

    /// Removes the PIN item; one that is already gone counts as removed.
    pub fn delete_pin_blob() -> Result<(), String> {
        remove(pin_service())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn locks_only_after_the_whole_idle_period_and_never_when_off() {
        let m = |n: u64| Duration::from_secs(n * 60);
        assert!(!idle_expired(m(14), 15));
        assert!(idle_expired(m(15), 15));
        assert!(idle_expired(m(90), 5));
        assert!(!idle_expired(m(10_000), 0));
        assert!(!idle_expired(Duration::ZERO, 5));
    }

    #[test]
    fn the_lock_stepper_walks_the_choices_and_stops_at_the_ends() {
        assert_eq!(step_lock(15, 1), 30);
        assert_eq!(step_lock(15, -1), 5);
        assert_eq!(step_lock(5, -1), 0);
        assert_eq!(step_lock(0, -1), 0);
        assert_eq!(step_lock(60, 1), 60);
        // A value that is not a choice (a hand-edited file) starts from the default.
        assert_eq!(step_lock(7, 1), 30);
    }

    #[test]
    fn an_older_write_never_overwrites_a_newer_one() {
        let counter = Mutex::new(0);
        assert_eq!(ordered(&counter, 2, || "newer"), Some("newer"));
        assert_eq!(ordered(&counter, 1, || "older"), None);
        assert_eq!(ordered(&counter, 2, || "same"), Some("same"));
        assert_eq!(ordered(&counter, 3, || "newest"), Some("newest"));
    }

    pub(super) fn login(host: &str) -> Key {
        Key::Password {
            user: "deploy".into(),
            host: host.into(),
            port: 22,
        }
    }

    /// A keeper over a vault file in a temp dir, with the Keychain and PIN off.
    pub(super) fn keeper(open: Option<Vault>) -> (gpui::TestApp, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut app = gpui::TestApp::new();
        let path = dir.path().join("vault.age");
        app.update(|cx| {
            cx.set_global(Keeper {
                path: Some(path),
                open,
                keychain: false,
                gate: Gate::default(),
                keychain_current: false,
                ticket: 0,
                pin: false,
                locks: 0,
            })
        });
        (app, dir)
    }

    pub(super) fn vault_with(host: &str) -> Vault {
        let mut v = Vault::new(SecretString::from("p".to_owned()));
        v.set(login(host), SecretString::from("pw".to_owned()));
        v
    }

    pub(super) fn has(app: &mut gpui::TestApp, host: &str) -> bool {
        app.update(|cx| Keeper::lookup(&login(host), cx).is_some())
    }

    #[test]
    fn storing_writes_without_taking_the_vault_away_from_other_tabs() {
        // What reaches the disk is `persist`'s, ordered by a counter the tests share, so only
        // the keeper's side is checked here.
        let (mut app, _dir) = keeper(Some(vault_with("a")));
        let task = app.update(|cx| {
            let task = Keeper::store(login("b"), SecretString::from("pw".to_owned()), cx);
            // While it is being written, another tab still finds it open.
            assert!(!Keeper::locked(cx));
            task
        });
        assert!(task.is_some());
        app.run_until_parked();
        assert!(has(&mut app, "a") && has(&mut app, "b"));
        // A locked vault is not written to.
        app.update(Keeper::lock);
        let locked =
            app.update(|cx| Keeper::store(login("c"), SecretString::from("pw".to_owned()), cx));
        assert!(locked.is_none());
    }

    #[test]
    fn a_sync_s_download_replaces_the_vault_only_if_nothing_was_written_meanwhile() {
        let (mut app, _dir) = keeper(Some(vault_with("local")));
        let since = app.update(|cx| Keeper::writes(cx));
        app.update(|cx| Keeper::sync_done(vault_with("remote"), false, true, since, cx));
        assert!(has(&mut app, "remote") && !has(&mut app, "local"));

        let since = app.update(|cx| Keeper::writes(cx));
        app.update(|cx| {
            Keeper::store(login("typed"), SecretString::from("pw".to_owned()), cx);
            Keeper::sync_done(vault_with("newer"), false, true, since, cx);
        });
        app.run_until_parked();
        assert!(has(&mut app, "typed") && !has(&mut app, "newer"));
    }

    #[test]
    fn a_sync_never_reopens_a_vault_locked_while_it_ran_unless_it_opened_it() {
        let (mut app, _dir) = keeper(Some(vault_with("a")));
        app.update(|cx| {
            let since = Keeper::writes(cx);
            Keeper::lock(cx);
            Keeper::sync_done(vault_with("a"), false, false, since, cx);
            assert!(Keeper::lookup(&login("a"), cx).is_none());
            Keeper::sync_done(vault_with("a"), true, false, since, cx);
        });
        assert!(has(&mut app, "a"));
    }
}

#[cfg(test)]
#[path = "keeper_lock_tests.rs"]
mod lock_tests;
