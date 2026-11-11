//! A whole sync of tern's settings folder with one remote: what travels (the synced files and
//! the vault), the per-item decision against the last sync's record, the manifest sealed
//! inside each push and checked on each pull, and the record written after. The vault crosses
//! as [`SyncVault`], so this crate stays free of how it is encrypted; the remote as [`Remote`]
//! (a git repository, a gist, or an in-memory one in tests).

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};

use crate::{Files, Gist, GitRepo, ItemPlan, Plan, RemoteBundle};

/// Why a vault did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    WrongPassphrase,
    Other(String),
}

/// The vault as a sync needs it: opened from the remote's copy, saved, and used to seal and
/// open the rest of the bundle. Errors are messages for the person.
pub trait SyncVault: Sized {
    /// Decrypted bundle data; a real vault zeroes it when dropped.
    type Plain: AsRef<[u8]>;

    /// The vault file `bytes`, opened with this vault's passphrase.
    ///
    /// # Errors
    /// The file is under another passphrase, or unreadable.
    fn reopen(&self, bytes: &[u8]) -> Result<Self, OpenError>;

    /// The vault file `bytes`, opened with `passphrase`.
    ///
    /// # Errors
    /// The passphrase is wrong, or the file unreadable.
    fn unlock_bytes(bytes: &[u8], passphrase: &str) -> Result<Self, OpenError>;

    /// # Errors
    /// The file cannot be written.
    fn save(&self, path: &Path) -> Result<(), String>;

    fn is_empty(&self) -> bool;

    /// # Errors
    /// Encryption failed.
    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String>;

    /// # Errors
    /// `sealed` was not sealed by this vault's passphrase, or is damaged.
    fn open(&self, sealed: &[u8]) -> Result<Self::Plain, String>;
}

/// Where the bundle lives.
pub trait Remote {
    /// The remote's bundle; `None` when it has none yet.
    ///
    /// # Errors
    /// The remote cannot be reached or read.
    fn fetch(&self) -> Result<Option<RemoteBundle>, String>;

    /// Replaces the remote's bundle.
    ///
    /// # Errors
    /// The remote cannot be reached or refuses the write.
    fn push(
        &self,
        hash: &str,
        seq: u64,
        device: &str,
        vault: &[u8],
        sealed: &[u8],
    ) -> Result<(), String>;
}

impl Remote for GitRepo {
    fn fetch(&self) -> Result<Option<RemoteBundle>, String> {
        GitRepo::fetch(self).map_err(|e| e.to_string())
    }

    fn push(
        &self,
        hash: &str,
        seq: u64,
        device: &str,
        vault: &[u8],
        sealed: &[u8],
    ) -> Result<(), String> {
        GitRepo::push(self, hash, seq, device, vault, sealed).map_err(|e| e.to_string())
    }
}

impl Remote for Gist {
    fn fetch(&self) -> Result<Option<RemoteBundle>, String> {
        Gist::fetch(self).map_err(|e| e.to_string())
    }

    fn push(
        &self,
        hash: &str,
        seq: u64,
        device: &str,
        vault: &[u8],
        sealed: &[u8],
    ) -> Result<(), String> {
        Gist::push(self, hash, seq, device, vault, sealed).map_err(|e| e.to_string())
    }
}

/// The remote for this settings folder: the person's own git repository when one is set
/// (cloned under `dir`), else their gist.
///
/// # Errors
/// No GitHub token is saved for the gist.
pub fn remote_for(dir: &Path, remote_url: Option<&str>) -> Result<Box<dyn Remote>, String> {
    Ok(match remote_url {
        Some(url) => Box::new(GitRepo::new(url.to_owned(), dir.join("sync-repo"))),
        None => {
            let (token, _) = crate::token().map_err(|e| e.to_string())?;
            Box::new(Gist::new(token))
        }
    })
}

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
pub enum Side {
    Mac,
    Remote,
}

/// The person's answers to per-item questions, by file name.
pub type Resolutions = BTreeMap<String, Side>;

/// A cheap look at the synced files, taken off the UI thread.
#[derive(Debug)]
pub struct Snapshot {
    pub local: String,
    pub last: Option<String>,
}

/// What a sync did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    Pushed,
    Pulled,
    Merged,
    Conflict { whole: bool, items: Vec<String> },
}

/// Syncs the settings folder `dir` with `remote`: decides from both sides and the last sync's
/// record whether to push, pull, merge item by item or ask, does it, and records the result.
/// `vault` is this Mac's open vault; `passphrase`, when typed, opens the remote's vault
/// instead of this one's passphrase. `force` answers a whole-bundle question, `resolve` the
/// per-item ones. Returns what happened, and the remote's vault when it was taken.
///
/// # Errors
/// A message for the person: the remote unreachable or refusing, the remote's vault under
/// another passphrase, a remote copy altered or rolled back, a file that cannot be written.
pub fn sync<V: SyncVault>(
    dir: &Path,
    remote: &dyn Remote,
    vault: &V,
    passphrase: Option<&str>,
    force: Option<Plan>,
    resolve: &Resolutions,
) -> Result<(Outcome, Option<V>), String> {
    // A Mac with no vault file yet writes the one it was given, so it travels. Should the
    // sync then fail (the remote's vault under another passphrase, say), that file goes
    // again: a vault under a mistyped passphrase must not stay behind and lock the next try
    // out.
    let vault_path = dir.join(VAULT_FILE);
    let started = !vault_path.exists();
    if started {
        vault.save(&vault_path)?;
    }
    let result = sync_saved(dir, remote, vault, passphrase, force, resolve);
    if started && result.is_err() {
        let _ = std::fs::remove_file(&vault_path);
    }
    result
}

fn sync_saved<V: SyncVault>(
    dir: &Path,
    remote: &dyn Remote,
    vault: &V,
    passphrase: Option<&str>,
    force: Option<Plan>,
    resolve: &Resolutions,
) -> Result<(Outcome, Option<V>), String> {
    let backend = remote;
    let vault_path = dir.join(VAULT_FILE);
    let local = read_local(dir)?;
    let local_hash = crate::hash(&local);
    let state = load_state(dir);
    let remote = backend.fetch()?;
    // A Mac that never synced and holds nothing of its own (no connections, an empty vault)
    // just takes GitHub's copy instead of asking which side wins.
    let fresh = state.last_hash.is_none() && !local.contains_key("hosts.json") && vault.is_empty();
    let plan = force.unwrap_or_else(|| match (&remote, fresh) {
        (Some(_), true) => Plan::Pull,
        _ => crate::decide(
            &local_hash,
            remote.as_ref().map(|r| r.hash.as_str()),
            state.last_hash.as_deref(),
        ),
    });
    // The remote's vault may use another passphrase than this Mac's: a typed one wins.
    let open_remote = |remote: &crate::RemoteBundle| {
        match passphrase {
            Some(p) => V::unlock_bytes(&remote.vault, p),
            None => vault.reopen(&remote.vault),
        }
        .map_err(|e| match e {
            OpenError::WrongPassphrase => {
                "The remote's vault uses another passphrase: enter it above and sync again."
                    .to_owned()
            }
            OpenError::Other(other) => other,
        })
    };
    // Numbered above anything this Mac or the remote has claimed, so no Mac reads it as old.
    let next_seq = state.seq.max(remote.as_ref().map_or(0, |r| r.seq)) + 1;
    let push = |files: &Files, hash: &str, with: &V| -> Result<u64, String> {
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
            let merged_hash = crate::hash(&merged);
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
        crate::item_hashes(local, &ITEMS),
        crate::item_hashes(remote, &ITEMS),
    );
    let (mut picks, mut asks) = (BTreeMap::new(), Vec::new());
    for name in ITEMS {
        let last = state.items.get(name).map(String::as_str);
        let pick = match crate::decide_item(&lh[name], &rh[name], last) {
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
                        match crate::merge_hosts(base, &text(local), &text(remote))? {
                            crate::HostsMerge::Merged(t) => Some(Pick::Merged(t.into_bytes())),
                            crate::HostsMerge::Conflict(_) => None,
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
fn seal_bundle<V: SyncVault>(
    files: &Files,
    hash: &str,
    seq: u64,
    with: &V,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut rest = files.clone();
    let vault_bytes = rest.remove(VAULT_FILE).unwrap_or_default();
    let manifest = Manifest {
        hash: hash.to_owned(),
        items: crate::item_hashes(files, &ITEMS),
        seq,
    };
    rest.insert(
        MANIFEST.to_owned(),
        serde_json::to_vec(&manifest).map_err(|e| e.to_string())?,
    );
    let sealed = with.seal(&encode(&rest)?)?;
    Ok((vault_bytes, sealed))
}

/// The remote's files, vault included, and its bundle number, once they are shown to be what
/// one tern pushed together: the sealed manifest names every item's hash and the meta's
/// bundle hash, and its number is not below one this Mac already had. A bundle from a tern
/// before the manifest is checked against the meta's hash alone and cannot be dated.
fn open_bundle<V: SyncVault>(
    remote: &crate::RemoteBundle,
    theirs: &V,
    known_seq: u64,
) -> Result<(Files, u64), String> {
    const ALTERED: &str = "The remote copy was changed outside tern (its files do not match \
                           each other), so nothing was taken from it. Sync from a Mac that \
                           has the right copy to replace it.";
    let plain = theirs.open(&remote.sealed)?;
    let mut files = decode(plain.as_ref())?;
    files.insert(VAULT_FILE.to_owned(), remote.vault.clone());
    let manifest = serde_json::from_slice::<BTreeMap<String, String>>(plain.as_ref())
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
        if crate::hash(&files) != remote.hash {
            return Err(ALTERED.into());
        }
        tracing::warn!("sync_bundle_unnumbered");
        return Ok((files, known_seq));
    };
    let ours = crate::item_hashes(&files, &ITEMS);
    // An item the pusher did not know (an older build) must be absent here too.
    let items_match = ITEMS.iter().all(|name| {
        manifest
            .items
            .get(*name)
            .map_or(crate::ABSENT, String::as_str)
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
        items: crate::item_hashes(files, &ITEMS),
        hosts_base: files
            .get(HOSTS_FILE)
            .map(|b| String::from_utf8_lossy(b).into_owned()),
        seq,
    })
    .map_err(|e| e.to_string())?;
    write_atomic(&dir.join(STATE_FILE), &json)
}

/// A look at the synced files for the change watcher; `None` when they cannot be read.
pub fn snapshot(dir: &Path) -> Option<Snapshot> {
    let local = crate::hash(&read_local(dir).ok()?);
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
#[path = "bundle_tests.rs"]
mod tests;
