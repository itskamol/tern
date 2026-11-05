//! The sync itself, off the UI thread: reading the synced files, deciding per item, pushing and
//! pulling through the backend. No UI in here.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use tern_ssh::SecretString;
use tern_sync::{Files, Gist, ItemPlan, Plan};
use tern_vault::{Vault, VaultError};

/// The files that travel besides the vault, by their name in the settings directory.
/// `snippets.json` belongs to the snippets store; an older build's `decode` drops the name.
const SYNCED: [&str; 4] = [
    "hosts.json",
    "settings.json",
    "keymap.json",
    "snippets.json",
];
const VAULT_FILE: &str = "vault.age";
const HOSTS_FILE: &str = "hosts.json";
const STATE_FILE: &str = "sync.json";
/// The manifest inside the sealed data. An older build's `decode` drops the name.
const MANIFEST: &str = "tern-bundle";
/// Every file that travels, each merged on its own.
const ITEMS: [&str; 5] = [
    "hosts.json",
    "settings.json",
    "keymap.json",
    "snippets.json",
    VAULT_FILE,
];

#[derive(Default, Serialize, Deserialize)]
struct SyncState {
    /// The bundle hash both sides had after the last sync.
    last_hash: Option<String>,
    /// Each item's hash at the last sync. Empty on a record from before per-item merging:
    /// "unknown", so the first conflict falls back to the whole-bundle question.
    #[serde(default)]
    items: BTreeMap<String, String>,
    /// `hosts.json` as both sides had it, the base for merging connections by name.
    #[serde(default)]
    hosts_base: Option<String>,
    /// The highest bundle number this Mac pushed or checked; a remote below it was rolled back.
    #[serde(default)]
    seq: u64,
}

/// What the pushing Mac sealed beside the files: each item's hash, the vault's included, and
/// the bundle's number. `tern-meta.json` and `vault.age` sit in the remote unsealed, so anyone
/// who can write the gist or repository could edit the meta or swap in an older vault; this
/// is what a pull checks them against.
#[derive(Serialize, Deserialize)]
struct Manifest {
    hash: String,
    items: BTreeMap<String, String>,
    seq: u64,
}

fn load_state(dir: &Path) -> SyncState {
    std::fs::read_to_string(dir.join(STATE_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Which copy of an item wins.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Side {
    Mac,
    Remote,
}

pub(super) type Resolutions = BTreeMap<String, Side>;

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

/// A cheap look at the synced files, taken off the UI thread.
pub(super) struct Snapshot {
    pub(super) local: String,
    pub(super) last: Option<String>,
}

pub(super) enum Outcome {
    UpToDate,
    Pushed,
    Pulled,
    Merged,
    Conflict { whole: bool, items: Vec<String> },
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
    let vault = match (open, passphrase) {
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
    };
    let result = sync(dir, remote_url, &vault, passphrase, force, resolve);
    match result {
        Ok((outcome, Some(pulled))) => (Some(pulled), Ok(outcome)),
        Ok((outcome, None)) => (Some(vault), Ok(outcome)),
        Err(e) => (Some(vault), Err(e)),
    }
}

/// Where the bundle lives: the user's own git repository when one is set, else a gist.
enum Backend {
    Git(tern_sync::GitRepo),
    Gist(Gist),
}

impl Backend {
    fn open(dir: &Path, remote_url: Option<&str>) -> Result<Self, String> {
        Ok(match remote_url {
            Some(url) => Backend::Git(tern_sync::GitRepo::new(
                url.to_owned(),
                dir.join("sync-repo"),
            )),
            None => {
                let (token, _) = tern_sync::token().map_err(|e| e.to_string())?;
                Backend::Gist(Gist::new(token))
            }
        })
    }

    fn fetch(&self) -> Result<Option<tern_sync::RemoteBundle>, String> {
        match self {
            Backend::Git(g) => g.fetch(),
            Backend::Gist(g) => g.fetch(),
        }
        .map_err(|e| e.to_string())
    }

    fn push(
        &self,
        hash: &str,
        seq: u64,
        device: &str,
        vault: &[u8],
        sealed: &[u8],
    ) -> Result<(), String> {
        match self {
            Backend::Git(g) => g.push(hash, seq, device, vault, sealed),
            Backend::Gist(g) => g.push(hash, seq, device, vault, sealed),
        }
        .map_err(|e| e.to_string())
    }
}

/// Returns the outcome, and the downloaded vault after a pull.
fn sync(
    dir: &Path,
    remote_url: Option<&str>,
    vault: &Vault,
    passphrase: Option<&str>,
    force: Option<Plan>,
    resolve: &Resolutions,
) -> Result<(Outcome, Option<Vault>), String> {
    let vault_path = dir.join(VAULT_FILE);
    if !vault_path.exists() {
        vault.save(&vault_path).map_err(|e| e.to_string())?;
    }
    let local = read_local(dir)?;
    let local_hash = tern_sync::hash(&local);
    let state = load_state(dir);
    let backend = Backend::open(dir, remote_url)?;
    let remote = backend.fetch()?;
    // A Mac that never synced and holds nothing of its own (no connections, an empty vault)
    // just takes GitHub's copy instead of asking which side wins.
    let fresh = state.last_hash.is_none() && !local.contains_key("hosts.json") && vault.is_empty();
    let plan = force.unwrap_or_else(|| match (&remote, fresh) {
        (Some(_), true) => Plan::Pull,
        _ => tern_sync::decide(
            &local_hash,
            remote.as_ref().map(|r| r.hash.as_str()),
            state.last_hash.as_deref(),
        ),
    });
    // The remote's vault may use another passphrase than this Mac's: a typed one wins.
    let open_remote = |remote: &tern_sync::RemoteBundle| {
        match passphrase {
            Some(p) => Vault::unlock_bytes(&remote.vault, SecretString::from(p.to_owned())),
            None => vault.reopen(&remote.vault),
        }
        .map_err(|e| match e {
            VaultError::WrongPassphrase => {
                "The remote's vault uses another passphrase: enter it above and sync again."
                    .to_owned()
            }
            other => other.to_string(),
        })
    };
    // Numbered above anything this Mac or the remote has claimed, so no Mac reads it as old.
    let next_seq = state.seq.max(remote.as_ref().map_or(0, |r| r.seq)) + 1;
    let push = |files: &Files, hash: &str, with: &Vault| -> Result<u64, String> {
        let (vault_bytes, sealed) = seal_bundle(files, hash, next_seq, with)?;
        let device = std::env::var("USER").unwrap_or_else(|_| "mac".into());
        backend.push(hash, next_seq, &device, &vault_bytes, &sealed)?;
        Ok(next_seq)
    };
    match plan {
        Plan::UpToDate => {
            write_state(dir, &local_hash, &local, state.seq)?;
            Ok((Outcome::UpToDate, None))
        }
        Plan::Conflict => {
            let Some(remote) = remote else {
                return Err("The remote has no bundle yet.".into());
            };
            // No per-item record (an older sync.json, or never synced): the whole-bundle
            // question, once. The answer writes the record, so the next conflict is per item.
            if state.items.is_empty() {
                return Ok((
                    Outcome::Conflict {
                        whole: true,
                        items: Vec::new(),
                    },
                    None,
                ));
            }
            let theirs = open_remote(&remote)?;
            let (remote_files, remote_seq) = open_bundle(&remote, &theirs, state.seq)?;
            let picks = match decide_items(&state, &local, &remote_files, resolve)? {
                Ok(picks) => picks,
                Err(asks) => {
                    return Ok((
                        Outcome::Conflict {
                            whole: false,
                            items: asks,
                        },
                        None,
                    ));
                }
            };
            let mut merged = local.clone();
            for (name, pick) in &picks {
                match pick {
                    Pick::Mac => {}
                    Pick::Remote => match remote_files.get(*name) {
                        Some(b) => {
                            merged.insert((*name).to_owned(), b.clone());
                        }
                        None => {
                            merged.remove(*name);
                        }
                    },
                    Pick::Merged(b) => {
                        merged.insert((*name).to_owned(), b.clone());
                    }
                }
            }
            let merged_hash = tern_sync::hash(&merged);
            let vault_from_remote = matches!(picks.get(VAULT_FILE), Some(Pick::Remote));
            let seq = if merged_hash == remote.hash {
                remote_seq
            } else {
                push(
                    &merged,
                    &merged_hash,
                    if vault_from_remote { &theirs } else { vault },
                )?
            };
            for (name, pick) in &picks {
                if matches!(pick, Pick::Mac) {
                    continue;
                }
                match merged.get(*name) {
                    Some(b) => write_atomic(&dir.join(name), b)?,
                    None => {
                        let _ = std::fs::remove_file(dir.join(name));
                    }
                }
            }
            write_state(dir, &merged_hash, &merged, seq)?;
            Ok((Outcome::Merged, vault_from_remote.then_some(theirs)))
        }
        Plan::Push => {
            let seq = push(&local, &local_hash, vault)?;
            write_state(dir, &local_hash, &local, seq)?;
            Ok((Outcome::Pushed, None))
        }
        Plan::Pull => {
            let Some(remote) = remote else {
                return Err("The remote has no bundle yet.".into());
            };
            let theirs = open_remote(&remote)?;
            let (files, seq) = open_bundle(&remote, &theirs, state.seq)?;
            for (name, bytes) in &files {
                if name != VAULT_FILE {
                    write_atomic(&dir.join(name), bytes)?;
                }
            }
            for name in SYNCED {
                if !files.contains_key(name) {
                    let _ = std::fs::remove_file(dir.join(name));
                }
            }
            write_atomic(&vault_path, &remote.vault)?;
            write_state(dir, &remote.hash, &files, seq)?;
            Ok((Outcome::Pulled, Some(theirs)))
        }
    }
}

/// What a diverged item becomes.
#[derive(Debug, PartialEq, Eq)]
enum Pick {
    Mac,
    Remote,
    Merged(Vec<u8>),
}

/// The item-level decision for a conflict. `Ok(Ok(picks))` settles every item; `Ok(Err(items))`
/// lists those only the user can decide (changed on both sides, no merge, no answer yet).
fn decide_items(
    state: &SyncState,
    local: &Files,
    remote: &Files,
    resolve: &Resolutions,
) -> Result<Result<BTreeMap<&'static str, Pick>, Vec<String>>, String> {
    let (lh, rh) = (
        tern_sync::item_hashes(local, &ITEMS),
        tern_sync::item_hashes(remote, &ITEMS),
    );
    let (mut picks, mut asks) = (BTreeMap::new(), Vec::new());
    for name in ITEMS {
        let last = state.items.get(name).map(String::as_str);
        let pick = match tern_sync::decide_item(&lh[name], &rh[name], last) {
            ItemPlan::Same | ItemPlan::KeepLocal => Pick::Mac,
            ItemPlan::TakeRemote => Pick::Remote,
            ItemPlan::Both => {
                // Connections merge by name when the base is known; only an edit both sides
                // made differently to one connection is left to the user.
                let text = |f: &Files| {
                    f.get(name)
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .unwrap_or_default()
                };
                let merged = match (name, &state.hosts_base) {
                    (HOSTS_FILE, Some(base)) => {
                        match tern_sync::merge_hosts(base, &text(local), &text(remote))? {
                            tern_sync::HostsMerge::Merged(t) => Some(Pick::Merged(t.into_bytes())),
                            tern_sync::HostsMerge::Conflict(_) => None,
                        }
                    }
                    _ => None,
                };
                match (merged, resolve.get(name)) {
                    (Some(p), _) => p,
                    (None, Some(Side::Mac)) => Pick::Mac,
                    (None, Some(Side::Remote)) => Pick::Remote,
                    (None, None) => {
                        asks.push(name.to_owned());
                        continue;
                    }
                }
            }
        };
        picks.insert(name, pick);
    }
    Ok(if asks.is_empty() {
        Ok(picks)
    } else {
        Err(asks)
    })
}

fn read_local(dir: &Path) -> Result<Files, String> {
    let mut files = Files::new();
    for name in SYNCED.into_iter().chain([VAULT_FILE]) {
        match std::fs::read(dir.join(name)) {
            Ok(bytes) => {
                files.insert(name.to_owned(), bytes);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{name}: {e}")),
        }
    }
    Ok(files)
}

fn encode(files: &Files) -> Result<Vec<u8>, String> {
    let map: BTreeMap<&str, String> = files
        .iter()
        .map(|(n, b)| (n.as_str(), B64.encode(b)))
        .collect();
    serde_json::to_vec(&map).map_err(|e| e.to_string())
}

/// The inverse of [`encode`]; names outside [`SYNCED`] are dropped, so a tampered bundle
/// cannot write elsewhere.
fn decode(bytes: &[u8]) -> Result<Files, String> {
    let map: BTreeMap<String, String> =
        serde_json::from_slice(bytes).map_err(|e| format!("sync data: {e}"))?;
    map.into_iter()
        .filter(|(n, _)| SYNCED.contains(&n.as_str()))
        .map(|(n, b)| {
            B64.decode(b)
                .map(|bytes| (n.clone(), bytes))
                .map_err(|e| format!("{n}: {e}"))
        })
        .collect()
}

/// What a push uploads for `files` (vault included): the vault as it is, and the other files
/// sealed with the [`Manifest`] that binds them to it.
fn seal_bundle(
    files: &Files,
    hash: &str,
    seq: u64,
    with: &Vault,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut rest = files.clone();
    let vault_bytes = rest.remove(VAULT_FILE).unwrap_or_default();
    let manifest = Manifest {
        hash: hash.to_owned(),
        items: tern_sync::item_hashes(files, &ITEMS),
        seq,
    };
    rest.insert(
        MANIFEST.to_owned(),
        serde_json::to_vec(&manifest).map_err(|e| e.to_string())?,
    );
    let sealed = with.seal(&encode(&rest)?).map_err(|e| e.to_string())?;
    Ok((vault_bytes, sealed))
}

/// The remote's files, vault included, and its bundle number, once they are shown to be what
/// one tern pushed together: the sealed manifest names every item's hash and the meta's
/// bundle hash, and its number is not below one this Mac already had. A bundle from a tern
/// before the manifest is checked against the meta's hash alone and cannot be dated.
fn open_bundle(
    remote: &tern_sync::RemoteBundle,
    theirs: &Vault,
    known_seq: u64,
) -> Result<(Files, u64), String> {
    const ALTERED: &str = "The remote copy was changed outside tern (its files do not match \
                           each other), so nothing was taken from it. Sync from a Mac that \
                           has the right copy to replace it.";
    let plain = theirs.open(&remote.sealed).map_err(|e| e.to_string())?;
    let mut files = decode(&plain)?;
    files.insert(VAULT_FILE.to_owned(), remote.vault.clone());
    let manifest = serde_json::from_slice::<BTreeMap<String, String>>(&plain)
        .ok()
        .and_then(|mut map| map.remove(MANIFEST))
        .map(|b| {
            B64.decode(b)
                .ok()
                .and_then(|json| serde_json::from_slice::<Manifest>(&json).ok())
                .ok_or_else(|| ALTERED.to_owned())
        })
        .transpose()?;
    let Some(manifest) = manifest else {
        if tern_sync::hash(&files) != remote.hash {
            return Err(ALTERED.into());
        }
        tracing::warn!("sync_bundle_unnumbered");
        return Ok((files, known_seq));
    };
    let ours = tern_sync::item_hashes(&files, &ITEMS);
    // An item the pusher did not know (an older build) must be absent here too.
    let items_match = ITEMS.iter().all(|name| {
        manifest
            .items
            .get(*name)
            .map_or(tern_sync::ABSENT, String::as_str)
            == ours[*name]
    });
    if !items_match || manifest.hash != remote.hash {
        return Err(ALTERED.into());
    }
    if manifest.seq < known_seq {
        return Err(format!(
            "The remote copy is older than one this Mac already synced (number {} after {known_seq}): \
             it may have been rolled back, so nothing was taken from it. Sync from a Mac that \
             has the newest copy to replace it.",
            manifest.seq
        ));
    }
    Ok((files, manifest.seq))
}

/// Records what both sides now hold: the bundle hash and number, each item's hash, and
/// `hosts.json` itself as the base for the next by-name merge. `files` is the agreed bundle,
/// vault included.
fn write_state(dir: &Path, hash: &str, files: &Files, seq: u64) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(&SyncState {
        last_hash: Some(hash.to_owned()),
        items: tern_sync::item_hashes(files, &ITEMS),
        hosts_base: files
            .get(HOSTS_FILE)
            .map(|b| String::from_utf8_lossy(b).into_owned()),
        seq,
    })
    .map_err(|e| e.to_string())?;
    write_atomic(&dir.join(STATE_FILE), &json)
}

/// A look at the synced files for the change watcher; `None` when they cannot be read.
pub(super) fn snapshot(dir: &Path) -> Option<Snapshot> {
    let local = tern_sync::hash(&read_local(dir).ok()?);
    Some(Snapshot {
        local,
        last: load_state(dir).last_hash,
    })
}
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("sync.tmp");
    std::fs::write(&tmp, bytes)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
#[path = "shell_sync_tests.rs"]
mod tests;
