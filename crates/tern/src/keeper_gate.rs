//! How the vault opens: the run's one Keychain attempt (who may start it, who waits for it, what
//! happens to the vault it opens), then the PIN or the passphrase the person types. No gpui
//! logic here (only the `Waiter` type), so the rules are tested with fake stores; the real
//! Keychain is never reached from a test.

use std::path::Path;

use tern_vault::{SecretString, Vault, VaultError};

use gpui::App;

use crate::keeper::keychain;
use crate::vault_pin::{self, Outcome};

/// Which secret the person is asked for to open the vault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    Pin,
    Passphrase,
}

/// What opening the vault needs now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opening {
    /// It is open already.
    Open,
    /// The Keychain is being read; the need runs again when it answers.
    Waiting,
    /// Ask the person.
    Ask(Ask),
}

/// A secret the person typed to open the vault.
pub enum Typed {
    Pin(String),
    Passphrase(SecretString),
}

/// What a typed secret came to, once the keeper took it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tried {
    Opened,
    /// Not opened; ask for `ask` next, after saying `message` (a wrong PIN, or the PIN is off).
    Again {
        ask: Ask,
        message: String,
    },
    /// Not opened, and nothing else to ask for: a wrong passphrase, an unreadable vault.
    Failed(String),
}

/// Opens the vault at `path` with what was typed; scrypt runs, so call it off the UI thread.
/// `dir` holds the PIN's attempt count, `pins` its sealed passphrase.
pub(crate) fn open_typed(
    typed: Typed,
    path: &Path,
    dir: Option<&Path>,
    pins: &impl vault_pin::Store,
) -> Outcome {
    match typed {
        Typed::Passphrase(passphrase) => match Vault::unlock(path, passphrase) {
            Ok(vault) => Outcome::Opened(vault),
            Err(VaultError::WrongPassphrase) => Outcome::Failed("Wrong vault passphrase.".into()),
            Err(e) => Outcome::Failed(format!("Vault unavailable: {e}")),
        },
        Typed::Pin(pin) => match dir {
            Some(dir) => vault_pin::unlock(dir, path, &pin, pins),
            None => Outcome::Gone,
        },
    }
}

/// What the person sees after `outcome`, and the vault to keep, if any. `stale` is true when the
/// vault was locked after the attempt began: the vault opened is dropped, which wipes it.
pub(crate) fn tried(outcome: Outcome, stale: bool) -> (Tried, Option<Vault>) {
    let message = vault_pin::message(&outcome).unwrap_or_default();
    match outcome {
        Outcome::Opened(_) if stale => (
            Tried::Failed("The vault was locked meanwhile; try again.".into()),
            None,
        ),
        Outcome::Opened(vault) => (Tried::Opened, Some(vault)),
        Outcome::Wrong { .. } => (
            Tried::Again {
                ask: Ask::Pin,
                message,
            },
            None,
        ),
        Outcome::Wiped | Outcome::Gone => (
            Tried::Again {
                ask: Ask::Passphrase,
                message,
            },
            None,
        ),
        Outcome::Failed(_) => (Tried::Failed(message), None),
    }
}

/// What the Keychain answered when asked for the passphrase.
pub(crate) enum Load {
    Found(SecretString),
    /// There is no item.
    Missing,
    /// The store refused or failed, as when the person denies the macOS dialog.
    Refused,
}

/// Where the Keychain passphrase comes from: the platform store, or a fake in tests.
pub(crate) trait Passphrases {
    fn load(&self) -> Load;
}

impl Passphrases for keychain::Passphrase {
    fn load(&self) -> Load {
        keychain::read()
    }
}

/// What one Keychain attempt came to.
pub(crate) struct Read {
    pub(crate) vault: Option<Vault>,
    pub(crate) refused: bool,
}

/// Opens the vault at `path` with the stored passphrase; scrypt runs, so call it off the UI
/// thread. No vault for no item, a refusal or a wrong passphrase.
pub(crate) fn open_with(store: &impl Passphrases, path: &Path) -> Read {
    match store.load() {
        Load::Found(passphrase) => Read {
            vault: Vault::unlock(path, passphrase).ok(),
            refused: false,
        },
        Load::Missing => Read {
            vault: None,
            refused: false,
        },
        Load::Refused => Read {
            vault: None,
            refused: true,
        },
    }
}

/// Keeps a vault the Keychain opened only if nothing else opened one meanwhile: the open vault
/// may hold edits the Keychain's copy lacks. The unused copy is dropped, which wipes it.
pub(crate) fn adopt(open: &mut Option<Vault>, found: Option<Vault>) -> bool {
    match (open.is_none(), found) {
        (true, Some(vault)) => {
            *open = Some(vault);
            true
        }
        _ => false,
    }
}

/// Whether `put` writes the passphrase to the Keychain: not over an item the store just refused,
/// and not while a read is still in flight, either of which would raise a second dialog.
pub(crate) fn store_due(keychain: bool, current: bool, refused: bool, in_flight: bool) -> bool {
    keychain && !current && !refused && !in_flight
}

pub(crate) type Waiter = Box<dyn FnOnce(&mut App)>;

/// What a need for the vault does about the Keychain.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// Ask the Keychain now; the need waits for it.
    Start,
    /// An attempt is running; the need waits for it.
    Wait,
    /// The Keychain is not to be asked.
    Skip,
}

/// The once-per-run Keychain attempt and the needs waiting on it.
pub(crate) struct Gate<W> {
    tried: bool,
    in_flight: bool,
    pub(crate) refused: bool,
    waiting: Vec<W>,
}

impl<W> Default for Gate<W> {
    fn default() -> Self {
        Self {
            tried: false,
            in_flight: false,
            refused: false,
            waiting: Vec::new(),
        }
    }
}

impl<W> Gate<W> {
    /// A need for the locked vault arrives; `waiter` is kept unless the answer is `Skip`.
    /// A Keychain read is running now.
    pub(crate) fn busy(&self) -> bool {
        self.in_flight
    }

    pub(crate) fn need(&mut self, enabled: bool, locked: bool, waiter: W) -> Need {
        if self.in_flight {
            self.waiting.push(waiter);
            Need::Wait
        } else if enabled && !self.tried && locked {
            self.tried = true;
            self.in_flight = true;
            self.waiting.push(waiter);
            Need::Start
        } else {
            Need::Skip
        }
    }

    /// The attempt resolved; hands back everything that waited, in order.
    pub(crate) fn finish(&mut self, refused: bool) -> Vec<W> {
        self.in_flight = false;
        self.refused |= refused;
        std::mem::take(&mut self.waiting)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::PathBuf;

    use tern_vault::Key;

    use super::*;

    /// Counts reads; a fake for the Keychain so no test reaches the real one.
    struct Counting {
        answer: fn() -> Load,
        loads: std::cell::Cell<u32>,
    }

    impl Counting {
        fn new(answer: fn() -> Load) -> Self {
            Self {
                answer,
                loads: std::cell::Cell::new(0),
            }
        }
    }

    impl Passphrases for Counting {
        fn load(&self) -> Load {
            self.loads.set(self.loads.get() + 1);
            (self.answer)()
        }
    }

    fn found(p: &str) -> Load {
        Load::Found(SecretString::from(p.to_owned()))
    }
    fn right() -> Load {
        found("right")
    }
    fn stale() -> Load {
        found("changed elsewhere")
    }
    fn missing() -> Load {
        Load::Missing
    }
    fn refused() -> Load {
        Load::Refused
    }

    /// One need as `Keeper::keychain_then` makes it: the gate decides, and only `Start` reads.
    /// Returns what the need did and, when it started the read, what the read came to.
    fn need(gate: &mut Gate<u32>, store: &Counting, path: &Path, id: u32) -> (Need, Option<Read>) {
        let n = gate.need(true, true, id);
        let read = (n == Need::Start).then(|| open_with(store, path));
        (n, read)
    }

    fn vault_file(pass: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.age");
        Vault::new(SecretString::from(pass.to_owned()))
            .save(&path)
            .unwrap();
        (dir, path)
    }

    #[test]
    fn the_first_need_reads_once_and_a_failed_attempt_is_not_repeated() {
        let (_dir, path) = vault_file("right");
        let store = Counting::new(stale);
        let mut gate = Gate::default();
        let (n, read) = need(&mut gate, &store, &path, 1);
        assert_eq!(n, Need::Start);
        assert!(read.unwrap().vault.is_none());
        gate.finish(false);
        for id in 2..5 {
            assert_eq!(need(&mut gate, &store, &path, id).0, Need::Skip);
        }
        assert_eq!(store.loads.get(), 1, "no second dialog this run");
    }

    #[test]
    fn a_refusal_or_missing_item_counts_as_the_attempt() {
        let (_dir, path) = vault_file("right");
        for answer in [missing, refused] {
            let store = Counting::new(answer);
            let mut gate = Gate::default();
            assert_eq!(need(&mut gate, &store, &path, 1).0, Need::Start);
            gate.finish(false);
            assert_eq!(need(&mut gate, &store, &path, 2).0, Need::Skip);
            assert_eq!(store.loads.get(), 1);
        }
    }

    #[test]
    fn a_need_during_the_attempt_waits_for_it_and_is_woken_in_order() {
        let (_dir, path) = vault_file("right");
        let store = Counting::new(right);
        let mut gate = Gate::default();
        let (first, read) = need(&mut gate, &store, &path, 1);
        assert_eq!(first, Need::Start);
        // Still in flight: a second session, the form and the page join instead of prompting.
        assert_eq!(need(&mut gate, &store, &path, 2).0, Need::Wait);
        assert_eq!(need(&mut gate, &store, &path, 3).0, Need::Wait);
        assert!(read.unwrap().vault.is_some());
        assert_eq!(gate.finish(false), vec![1, 2, 3]);
        assert_eq!(store.loads.get(), 1);
        assert_eq!(need(&mut gate, &store, &path, 4).0, Need::Skip);
    }

    #[test]
    fn the_keychain_is_not_asked_when_off_or_when_the_vault_is_already_open() {
        let mut gate = Gate::default();
        assert_eq!(gate.need(false, true, 1), Need::Skip);
        assert_eq!(gate.need(true, false, 2), Need::Skip);
        assert!(
            !gate.tried,
            "a need that did not ask keeps the run's one try"
        );
        assert!(gate.waiting.is_empty());
    }

    #[test]
    fn the_stored_passphrase_opens_the_vault_without_a_prompt() {
        let (_dir, path) = vault_file("right");
        assert!(open_with(&Counting::new(right), &path).vault.is_some());
    }

    #[test]
    fn a_stale_passphrase_leaves_the_vault_locked() {
        let (_dir, path) = vault_file("right");
        let read = open_with(&Counting::new(stale), &path);
        assert!(read.vault.is_none() && !read.refused);
        assert!(
            open_with(&Counting::new(right), &path.with_file_name("none.age"))
                .vault
                .is_none()
        );
    }

    #[test]
    fn a_vault_opened_meanwhile_is_not_replaced_by_the_keychains_copy() {
        let (_dir, path) = vault_file("right");
        let key = Key::SshKey { name: "k".into() };
        let mut open = Some(Vault::unlock(&path, SecretString::from("right".to_owned())).unwrap());
        open.as_mut()
            .unwrap()
            .set(key.clone(), SecretString::from("edited".to_owned()));
        let stale_copy = open_with(&Counting::new(right), &path).vault;
        assert!(!adopt(&mut open, stale_copy));
        assert!(open.unwrap().get(&key).is_some(), "the edit survives");
        let mut locked = None;
        let fresh = open_with(&Counting::new(right), &path).vault;
        assert!(adopt(&mut locked, fresh) && locked.is_some());
    }

    #[test]
    fn a_refused_keychain_is_not_written_over_for_the_rest_of_the_run() {
        let mut gate: Gate<u32> = Gate::default();
        assert!(store_due(true, false, gate.refused, false));
        gate.need(true, true, 1);
        gate.finish(false);
        assert!(
            store_due(true, false, gate.refused, false),
            "no item: store as before"
        );
        let mut gate: Gate<u32> = Gate::default();
        gate.need(true, true, 1);
        gate.finish(true);
        assert!(!store_due(true, false, gate.refused, false));
        assert!(!store_due(false, false, false, false));
        assert!(!store_due(true, true, false, false));
        let (_dir, path) = vault_file("right");
        assert!(open_with(&Counting::new(refused), &path).refused);
    }

    #[test]
    fn nothing_is_stored_while_a_read_is_in_flight() {
        assert!(!store_due(true, false, false, true));
        assert!(store_due(true, false, false, false));
    }

    /// A PIN store with nothing in it, as after a wipe.
    struct NoPin;

    impl vault_pin::Store for NoPin {
        fn load(&self) -> Option<Vec<u8>> {
            None
        }
        fn save(&self, _: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self) -> Result<(), String> {
            Ok(())
        }
    }

    fn typed(pass: &str) -> Typed {
        Typed::Passphrase(SecretString::from(pass.to_owned()))
    }

    #[test]
    fn a_typed_passphrase_opens_the_vault_or_says_why_not() {
        let (dir, path) = vault_file("right");
        let opened = open_typed(typed("right"), &path, Some(dir.path()), &NoPin);
        assert!(matches!(tried(opened, false), (Tried::Opened, Some(_))));
        let wrong = open_typed(typed("wrong"), &path, Some(dir.path()), &NoPin);
        assert_eq!(
            tried(wrong, false).0,
            Tried::Failed("Wrong vault passphrase.".into())
        );
        let gone = dir.path().join("none.age");
        let (missing, vault) = tried(open_typed(typed("right"), &gone, None, &NoPin), false);
        assert!(vault.is_none());
        assert!(matches!(missing, Tried::Failed(m) if m.starts_with("Vault unavailable: ")));
    }

    #[test]
    fn a_pin_with_nothing_sealed_moves_on_to_the_passphrase() {
        let (dir, path) = vault_file("right");
        for d in [Some(dir.path()), None] {
            let outcome = open_typed(Typed::Pin("1234".into()), &path, d, &NoPin);
            assert_eq!(
                tried(outcome, false).0,
                Tried::Again {
                    ask: Ask::Passphrase,
                    message: "PIN unlock is off; use your passphrase.".into()
                }
            );
        }
    }

    #[test]
    fn a_wrong_pin_asks_for_the_pin_again_and_a_wiped_one_for_the_passphrase() {
        let (wrong, vault) = tried(Outcome::Wrong { left: 1 }, false);
        assert!(vault.is_none());
        assert_eq!(
            wrong,
            Tried::Again {
                ask: Ask::Pin,
                message: "Wrong PIN, 1 try left.".into()
            }
        );
        assert_eq!(
            tried(Outcome::Wiped, false).0,
            Tried::Again {
                ask: Ask::Passphrase,
                message: vault_pin::WIPED.into()
            }
        );
        assert_eq!(
            tried(Outcome::Failed("keychain broke".into()), false).0,
            Tried::Failed("keychain broke".into())
        );
    }

    #[test]
    fn a_vault_opened_after_a_lock_is_not_kept() {
        let open = Outcome::Opened(Vault::new(SecretString::from("p".to_owned())));
        let (said, kept) = tried(open, true);
        assert!(kept.is_none());
        assert!(matches!(said, Tried::Failed(_)));
    }
}
