//! The app's one vault: where it lives and, once unlocked, the open [`Vault`] every tab reads.
//! Unlocking and saving run scrypt (about a second), so both happen off the UI thread.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, AppContext, Global, Task};
use tern_ssh::SecretString;
use tern_vault::{Key, Vault, VaultError};

pub use crate::keeper_gate::{Ask, Opening, Pending, Tried, Typed};
use crate::keeper_gate::{
    Gate, Need, Read, Waiter, adopt, open_typed, open_with, store_due, tried,
};
pub(crate) use crate::keeper_store::{CredentialStore, OsCredentialStore, keychain};
use crate::vault_pin;

/// Lifecycle transition emitted by [`Keeper`] whenever the vault opens or locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultTransition {
    Unlocked,
    Locked,
}

type TransitionListener = Box<dyn FnMut(VaultTransition, &mut App) + 'static>;

pub struct Keeper {
    path: Option<PathBuf>,
    open: Option<Vault>,
    /// The Keychain opt-in (Settings -> Vault) is on.
    keychain: bool,
    /// Coalesces reads and remembers whether the automatic attempt ran.
    gate: Gate<Waiter>,
    /// The Keychain item already holds the current passphrase, so `put` need not write it.
    keychain_current: bool,
    /// Numbers each write in the order it was asked for.
    ticket: u64,
    /// The newest write that reached the disk for this keeper instance.
    written: Arc<Mutex<u64>>,
    /// Injected platform or test credential store.
    store: Arc<dyn CredentialStore>,
    /// Subscribers notified on every `Locked <-> Unlocked` transition.
    listeners: Vec<TransitionListener>,
    /// A PIN is set: the Keychain holds the passphrase sealed to it (see `vault_pin`).
    pin: bool,
    /// How many times a lock was asked for, open vault or not: an open that began before one
    /// is stale and must not reopen the vault.
    locks: u64,
}

impl Global for Keeper {}

/// Runs `write` only if no newer write (higher `ticket`) has already run; `None` when skipped.
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
        Self::install_at_with_keychain(path, false, cx);
    }

    /// Like [`Self::install_at`], allowing a test to turn the Keychain opt-in flag on without
    /// touching the real macOS Keychain unless code mistakenly invokes `need_vault`.
    #[cfg(test)]
    pub(crate) fn install_at_with_keychain(path: PathBuf, keychain: bool, cx: &mut App) {
        Self::install_at_with_store(path, keychain, Arc::new(OsCredentialStore), cx);
    }

    /// Installs a keeper backed by an injected [`CredentialStore`] (e.g. an in-memory fake).
    #[cfg(test)]
    pub(crate) fn install_at_with_store(
        path: PathBuf,
        keychain: bool,
        store: Arc<dyn CredentialStore>,
        cx: &mut App,
    ) {
        cx.set_global(Self {
            path: Some(path),
            open: None,
            keychain,
            gate: Gate::default(),
            keychain_current: false,
            ticket: 0,
            written: Arc::new(Mutex::new(0)),
            store,
            listeners: Vec::new(),
            pin: false,
            locks: 0,
        });
    }

    /// Registers a listener invoked on every [`VaultTransition`] (`Unlocked` or `Locked`).
    ///
    /// Delivery is deferred via [`App::defer`] so transitions triggered inside an entity update
    /// (such as `Shell` locking the vault on idle or sleep) never re-enter that entity while it
    /// is still leased.
    pub fn subscribe(cx: &mut App, listener: impl FnMut(VaultTransition, &mut App) + 'static) {
        cx.global_mut::<Self>().listeners.push(Box::new(listener));
    }

    fn emit(transition: VaultTransition, cx: &mut App) {
        cx.defer(move |cx| {
            if !cx.has_global::<Self>() {
                return;
            }
            let mut listeners = std::mem::take(&mut cx.global_mut::<Self>().listeners);
            for listener in &mut listeners {
                listener(transition, cx);
            }
            let mut rest = std::mem::take(&mut cx.global_mut::<Self>().listeners);
            listeners.append(&mut rest);
            cx.global_mut::<Self>().listeners = listeners;
        });
    }

    #[cfg(test)]
    pub(crate) fn keychain_attempted(cx: &App) -> bool {
        cx.global::<Self>().gate.tried()
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
            written: Arc::new(Mutex::new(0)),
            store: Arc::new(OsCredentialStore),
            listeners: Vec::new(),
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
        let waiting = Self::keychain_then(cx, move |cx, _| {
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
        let (since, path, dir, store) = (
            k.locks,
            k.path.clone(),
            crate::settings::dir(),
            Arc::clone(&k.store),
        );
        let work = cx.background_spawn(async move {
            match path {
                Some(path) => open_typed(typed, &path, dir.as_deref(), &store),
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

    pub fn keychain_enabled(cx: &App) -> bool {
        cx.global::<Self>().keychain
    }

    /// Called only from a person's unlock action. Automatic needs retain their one-shot policy.
    pub fn unlock_with_keychain(cx: &mut App, then: impl FnOnce(&mut App, bool) + 'static) -> bool {
        if !Self::keychain_enabled(cx) || !Self::locked(cx) {
            return false;
        }
        cx.global_mut::<Self>().gate.allow_explicit_retry();
        Self::keychain_then(cx, then)
    }

    /// The first need for a locked vault in this run: with the Keychain opt-in on, reads the
    /// passphrase and opens the vault off the UI thread, then runs `then`. Returns `true` when
    /// `then` will run once the attempt resolves, whether this call started it or joined one in
    /// flight; the vault is in the keeper by then if it opened. Returns `false` when the Keychain
    /// is not to be asked (off, already asked this run, nothing to open) and `then` is dropped:
    /// the caller prompts as usual. A missing item, a refusal or a stale passphrase say nothing.
    fn keychain_then(cx: &mut App, then: impl FnOnce(&mut App, bool) + 'static) -> bool {
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
                let store = Arc::clone(&k.store);
                let work = cx.background_spawn(async move { open_with(&store, &path) });
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
        let opened = adopt(&mut k.open, found);
        if opened {
            k.keychain_current = true;
        }
        let waiters = k.gate.finish(read.refused);
        if opened {
            Self::emit(VaultTransition::Unlocked, cx);
        }
        for waiter in waiters {
            waiter(cx, read.refused);
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
        let was_open = k.open.take().map(|mut vault| vault.wipe()).is_some();
        if was_open {
            Self::emit(VaultTransition::Locked, cx);
        }
        was_open
    }

    /// Hands an unlocked vault back. With the Keychain opt-in on, the passphrase is stored
    /// the first time (and again after it changes).
    pub fn put(vault: Vault, cx: &mut App) {
        let k = cx.global_mut::<Self>();
        let was_closed = k.open.is_none();
        if store_due(
            k.keychain,
            k.keychain_current,
            k.gate.refused,
            k.gate.busy(),
        ) {
            match k.store.store_passphrase(vault.passphrase()) {
                Ok(()) => k.keychain_current = true,
                Err(e) => tracing::warn!(error = %e, "vault_keychain_store_failed"),
            }
        }
        k.open = Some(vault);
        if was_closed {
            Self::emit(VaultTransition::Unlocked, cx);
        }
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
            k.store.store_passphrase(vault.passphrase())?;
            k.keychain = true;
            k.keychain_current = true;
            // The person just wrote the item themselves: an earlier refusal no longer holds.
            k.gate.refused = false;
        } else {
            k.store.delete_passphrase()?;
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
            let store = Arc::clone(&k.store);
            if let Some(dir) = crate::settings::dir()
                && let Err(e) = vault_pin::clear(&dir, &store)
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
        let k = cx.global::<Self>();
        let Some(vault) = k.open.as_ref() else {
            return Err("Unlock the vault first: its passphrase is what the PIN seals.".into());
        };
        let passphrase = vault.passphrase().clone();
        let store = Arc::clone(&k.store);
        let dir = crate::settings::dir().ok_or("There is no settings folder.")?;
        Ok(cx.background_spawn(async move { vault_pin::set(&dir, &passphrase, &pin, &store) }))
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
        let store = Arc::clone(&cx.global::<Self>().store);
        vault_pin::clear(&dir, &store)?;
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
        let written = Arc::clone(&k.written);
        Some(cx.background_spawn(async move {
            ordered(&written, ticket, || snapshot.save(&path))
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
        let written = Arc::clone(&k.written);
        Some(cx.background_spawn(async move {
            ordered(&written, ticket, || {
                Vault::change_passphrase(&path, current, new)
            })
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::keeper_gate::Load;

    #[derive(Default)]
    struct MemoryStore {
        passphrase: Mutex<Option<SecretString>>,
        refuse_read: Mutex<bool>,
        pin_blob: Mutex<Option<Vec<u8>>>,
    }

    impl CredentialStore for MemoryStore {
        fn read_passphrase(&self) -> Load {
            if *self.refuse_read.lock().unwrap() {
                return Load::Refused;
            }
            match self.passphrase.lock().unwrap().clone() {
                Some(p) => Load::Found(p),
                None => Load::Missing,
            }
        }

        fn store_passphrase(&self, passphrase: &SecretString) -> Result<(), String> {
            *self.passphrase.lock().unwrap() = Some(passphrase.clone());
            Ok(())
        }

        fn delete_passphrase(&self) -> Result<(), String> {
            *self.passphrase.lock().unwrap() = None;
            Ok(())
        }

        fn store_pin_blob(&self, blob: &[u8]) -> Result<(), String> {
            *self.pin_blob.lock().unwrap() = Some(blob.to_vec());
            Ok(())
        }

        fn load_pin_blob(&self) -> Option<Vec<u8>> {
            self.pin_blob.lock().unwrap().clone()
        }

        fn delete_pin_blob(&self) -> Result<(), String> {
            *self.pin_blob.lock().unwrap() = None;
            Ok(())
        }
    }

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
                written: Arc::new(Mutex::new(0)),
                store: Arc::new(MemoryStore::default()),
                listeners: Vec::new(),
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

    #[test]
    fn emits_unlocked_and_locked_transitions_and_supports_fake_credential_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.age");
        let vault = vault_with("prod");
        vault.save(&path).unwrap();

        let store = Arc::new(MemoryStore::default());
        store
            .store_passphrase(&SecretString::from("p".to_owned()))
            .unwrap();

        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&events);

        let mut app = gpui::TestApp::new();
        app.update(|cx| {
            Keeper::install_at_with_store(path.clone(), true, store, cx);
            Keeper::subscribe(cx, move |transition, _| {
                seen.lock().unwrap().push(transition);
            });
            assert!(Keeper::unlock_with_keychain(cx, |_, _| {}));
        });
        app.run_until_parked();
        assert!(has(&mut app, "prod"));
        assert_eq!(*events.lock().unwrap(), vec![VaultTransition::Unlocked]);

        app.update(|cx| {
            assert!(Keeper::lock(cx));
            assert!(!Keeper::lock(cx));
        });
        assert_eq!(
            *events.lock().unwrap(),
            vec![VaultTransition::Unlocked, VaultTransition::Locked]
        );
    }
}

#[cfg(test)]
#[path = "keeper_lock_tests.rs"]
mod lock_tests;
