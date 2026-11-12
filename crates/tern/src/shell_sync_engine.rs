//! The app's side of a sync, off the UI thread: opening the vault it syncs and handing it to
//! `tern_sync::bundle`, which does the sync itself. No UI in here.

use std::path::Path;
use std::time::{Duration, Instant};

use tern_ssh::SecretString;
use tern_sync::Plan;
use tern_sync::bundle::{self, OpenError, SyncVault};
pub(super) use tern_sync::bundle::{Outcome, Resolutions, Side, Snapshot, snapshot};
use tern_vault::{Vault, VaultError, Zeroizing};

const VAULT_FILE: &str = "vault.age";

/// Lets an event through at most once per `every`; only an allowed event restarts the clock.
/// Used for "check the remote when the window comes to the front" and the 5-minute poll.
pub(super) struct Throttle {
    every: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub(super) fn new(every: Duration) -> Self {
        Self { every, last: None }
    }

    pub(super) fn allow(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < self.every)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// tern's vault as the sync sees it. A newtype, as the trait is `tern-sync`'s and the vault
/// `tern-vault`'s.
pub(super) struct Synced(pub(super) Vault);

fn open_error(e: VaultError) -> OpenError {
    match e {
        VaultError::WrongPassphrase => OpenError::WrongPassphrase,
        other => OpenError::Other(other.to_string()),
    }
}

impl SyncVault for Synced {
    type Plain = Zeroizing<Vec<u8>>;

    fn reopen(&self, bytes: &[u8]) -> Result<Self, OpenError> {
        self.0.reopen(bytes).map(Synced).map_err(open_error)
    }

    fn unlock_bytes(bytes: &[u8], passphrase: &str) -> Result<Self, OpenError> {
        Vault::unlock_bytes(bytes, SecretString::from(passphrase.to_owned()))
            .map(Synced)
            .map_err(open_error)
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        self.0.save(path).map_err(|e| e.to_string())
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
        self.0.seal(plain).map_err(|e| e.to_string())
    }

    fn open(&self, sealed: &[u8]) -> Result<Self::Plain, String> {
        self.0.open(sealed).map_err(|e| e.to_string())
    }
}

/// The whole sync, off the UI thread. Returns the vault to hand back to the keeper (the
/// downloaded one after a pull) and what happened.
pub(super) fn run(
    dir: &Path,
    remote_url: Option<&str>,
    open: Option<Vault>,
    passphrase: Option<&str>,
    force: Option<Plan>,
    resolve: &Resolutions,
) -> (Option<Vault>, Result<Outcome, String>) {
    let vault_path = dir.join(VAULT_FILE);
    let vault = Synced(match (open, passphrase) {
        (Some(v), _) => v,
        (None, Some(p)) if vault_path.exists() => {
            match Vault::unlock(&vault_path, SecretString::from(p.to_owned())) {
                Ok(v) => v,
                Err(VaultError::WrongPassphrase) => {
                    return (None, Err("Wrong vault passphrase.".into()));
                }
                Err(e) => return (None, Err(e.to_string())),
            }
        }
        (None, Some(p)) => Vault::new(SecretString::from(p.to_owned())),
        (None, None) => return (None, Err("The vault is locked.".into())),
    });
    let result = bundle::remote_for(dir, remote_url)
        .and_then(|remote| bundle::sync(dir, remote.as_ref(), &vault, passphrase, force, resolve));
    match result {
        Ok((outcome, Some(pulled))) => (Some(pulled.0), Ok(outcome)),
        Ok((outcome, None)) => (Some(vault.0), Ok(outcome)),
        Err(e) => (Some(vault.0), Err(e)),
    }
}

#[cfg(test)]
#[path = "shell_sync_tests.rs"]
mod tests;
