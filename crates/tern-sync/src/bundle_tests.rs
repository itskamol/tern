#![allow(clippy::unwrap_used, clippy::disallowed_methods)]
use std::cell::RefCell;

use super::*;

/// A stand-in vault. Its file says who made it and under which passphrase; sealing prefixes
/// the passphrase, so another passphrase cannot open what it sealed, as with age.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fake {
    pass: String,
    empty: bool,
}

fn fake(pass: &str) -> Fake {
    Fake {
        pass: pass.to_owned(),
        empty: true,
    }
}

impl Fake {
    fn file(&self) -> Vec<u8> {
        format!("fake-vault\n{}\n{}", self.pass, self.empty).into_bytes()
    }

    fn parse(bytes: &[u8]) -> Result<(String, bool), OpenError> {
        let text = String::from_utf8_lossy(bytes);
        let mut lines = text.lines();
        match (lines.next(), lines.next(), lines.next()) {
            (Some("fake-vault"), Some(pass), Some(empty)) => Ok((pass.to_owned(), empty == "true")),
            _ => Err(OpenError::Other("not a vault".into())),
        }
    }
}

impl SyncVault for Fake {
    type Plain = Vec<u8>;

    fn reopen(&self, bytes: &[u8]) -> Result<Self, OpenError> {
        Self::unlock_bytes(bytes, &self.pass)
    }

    fn unlock_bytes(bytes: &[u8], passphrase: &str) -> Result<Self, OpenError> {
        let (pass, empty) = Self::parse(bytes)?;
        if pass != passphrase {
            return Err(OpenError::WrongPassphrase);
        }
        Ok(Self { pass, empty })
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, self.file()).map_err(|e| e.to_string())
    }

    fn is_empty(&self) -> bool {
        self.empty
    }

    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
        Ok([format!("sealed:{}\n", self.pass).as_bytes(), plain].concat())
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, String> {
        sealed
            .strip_prefix(format!("sealed:{}\n", self.pass).as_bytes())
            .map(<[u8]>::to_vec)
            .ok_or_else(|| "cannot open the sealed data".to_owned())
    }
}

/// A remote held in memory, as a gist or repository would hold the bundle.
#[derive(Default)]
struct Mem {
    bundle: RefCell<Option<RemoteBundle>>,
    down: bool,
}

impl Remote for Mem {
    fn fetch(&self) -> Result<Option<RemoteBundle>, String> {
        if self.down {
            return Err("unreachable".into());
        }
        Ok(self.bundle.borrow().clone())
    }

    fn push(
        &self,
        hash: &str,
        seq: u64,
        device: &str,
        vault: &[u8],
        sealed: &[u8],
    ) -> Result<(), String> {
        *self.bundle.borrow_mut() = Some(RemoteBundle {
            hash: hash.to_owned(),
            updated_at: 0,
            device: device.to_owned(),
            seq,
            vault: vault.to_vec(),
            sealed: sealed.to_vec(),
        });
        Ok(())
    }
}

#[test]
fn two_macs_converge_through_any_remote_and_a_wrong_passphrase_takes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let remote = Mem::default();
    let none = Resolutions::new();
    let va = fake("pw");
    std::fs::write(a.join(HOSTS_FILE), hosts(&[("web", "1.1.1.1")])).unwrap();
    let (out, _) = sync(&a, &remote, &va, None, None, &none).unwrap();
    assert!(matches!(out, Outcome::Pushed));
    // B's vault is under another passphrase: the remote's vault cannot be opened with it.
    let err = sync(&b, &remote, &fake("other"), None, None, &none)
        .map(|_| ())
        .unwrap_err();
    assert!(err.contains("another passphrase"), "{err}");
    assert!(!b.join(HOSTS_FILE).exists(), "nothing was taken");
    assert!(
        !b.join(VAULT_FILE).exists(),
        "the vault this sync started under the wrong passphrase is not left behind"
    );
    // Typed, the right passphrase opens it.
    let (out, theirs) = sync(&b, &remote, &fake("other"), Some("pw"), None, &none).unwrap();
    assert!(matches!(out, Outcome::Pulled));
    assert_eq!(theirs, Some(va.clone()));
    assert_eq!(
        std::fs::read(b.join(HOSTS_FILE)).unwrap(),
        hosts(&[("web", "1.1.1.1")])
    );
    let (out, _) = sync(&a, &remote, &va, None, None, &none).unwrap();
    assert!(matches!(out, Outcome::UpToDate));
}

#[test]
fn an_unreachable_remote_changes_nothing_here() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(HOSTS_FILE), hosts(&[("web", "1.1.1.1")])).unwrap();
    let down = Mem {
        down: true,
        ..Mem::default()
    };
    let err = sync(
        dir.path(),
        &down,
        &fake("pw"),
        None,
        None,
        &Resolutions::new(),
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(err, "unreachable");
    assert!(
        !dir.path().join(STATE_FILE).exists(),
        "no record of a sync that did not happen"
    );
}

#[test]
fn encode_decode_round_trips_and_drops_unknown_names() {
    let mut files = Files::new();
    files.insert("hosts.json".into(), b"{\"v\":1}".to_vec());
    files.insert("settings.json".into(), vec![0, 255, 7]);
    assert_eq!(decode(&encode(&files).unwrap()).unwrap(), files);

    let evil = br#"{"../../.ssh/authorized_keys":"aGk=","hosts.json":"e30="}"#;
    let out = decode(evil).unwrap();
    assert_eq!(out.keys().collect::<Vec<_>>(), vec!["hosts.json"]);
}

fn hosts(list: &[(&str, &str)]) -> Vec<u8> {
    let items: Vec<_> = list
        .iter()
        .map(|(n, h)| serde_json::json!({"name": n, "host": h, "port": 22, "user": "root"}))
        .collect();
    serde_json::to_vec_pretty(&serde_json::json!({"version": 1, "connections": items})).unwrap()
}

fn bundle(pairs: &[(&str, &[u8])]) -> Files {
    pairs
        .iter()
        .map(|(n, b)| ((*n).to_owned(), b.to_vec()))
        .collect()
}

#[test]
fn item_decisions_take_the_side_that_changed_and_ask_only_when_both_did() {
    let base = bundle(&[
        (HOSTS_FILE, &hosts(&[("web", "1.1.1.1")])),
        ("settings.json", b"{\"a\":1}"),
        ("keymap.json", b"[]"),
        (VAULT_FILE, b"vault-1"),
    ]);
    let state = SyncState {
        last_hash: Some("x".into()),
        items: crate::item_hashes(&base, &ITEMS),
        hosts_base: Some(String::from_utf8(hosts(&[("web", "1.1.1.1")])).unwrap()),
        seq: 0,
    };
    let mut local = base.clone();
    let mut remote = base.clone();
    // Mac edits the keymap; the remote edits the vault; both edit the settings.
    local.insert("keymap.json".into(), b"[1]".to_vec());
    remote.insert(VAULT_FILE.into(), b"vault-2".to_vec());
    local.insert("settings.json".into(), b"{\"a\":2}".to_vec());
    remote.insert("settings.json".into(), b"{\"a\":3}".to_vec());

    let asks = decide_items(&state, &local, &remote, &Resolutions::new())
        .unwrap()
        .unwrap_err();
    assert_eq!(asks, ["settings.json"], "only the item changed on both");

    let resolve = Resolutions::from([("settings.json".to_owned(), Side::Remote)]);
    let picks = decide_items(&state, &local, &remote, &resolve)
        .unwrap()
        .unwrap();
    assert_eq!(picks["keymap.json"], Pick::Mac);
    assert_eq!(picks[VAULT_FILE], Pick::Remote);
    assert_eq!(picks["settings.json"], Pick::Remote);
    assert_eq!(picks[HOSTS_FILE], Pick::Mac, "untouched");

    let resolve = Resolutions::from([("settings.json".to_owned(), Side::Mac)]);
    let picks = decide_items(&state, &local, &remote, &resolve)
        .unwrap()
        .unwrap();
    assert_eq!(picks["settings.json"], Pick::Mac);
}

#[test]
fn hosts_changed_on_both_sides_merge_without_asking() {
    let base = bundle(&[(HOSTS_FILE, &hosts(&[("web", "1.1.1.1")]))]);
    let state = SyncState {
        last_hash: Some("x".into()),
        items: crate::item_hashes(&base, &ITEMS),
        hosts_base: Some(String::from_utf8(hosts(&[("web", "1.1.1.1")])).unwrap()),
        seq: 0,
    };
    let local = bundle(&[(HOSTS_FILE, &hosts(&[("web", "1.1.1.1"), ("db", "2.2.2.2")]))]);
    let remote = bundle(&[(
        HOSTS_FILE,
        &hosts(&[("web", "1.1.1.1"), ("cache", "3.3.3.3")]),
    )]);
    let picks = decide_items(&state, &local, &remote, &Resolutions::new())
        .unwrap()
        .unwrap();
    let Pick::Merged(bytes) = &picks[HOSTS_FILE] else {
        unreachable!("{:?}", picks[HOSTS_FILE]);
    };
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(text.contains("db") && text.contains("cache") && text.contains("web"));

    // The same connection edited differently on both sides is the one real conflict.
    let local = bundle(&[(HOSTS_FILE, &hosts(&[("web", "10.0.0.1")]))]);
    let remote = bundle(&[(HOSTS_FILE, &hosts(&[("web", "11.0.0.1")]))]);
    let asks = decide_items(&state, &local, &remote, &Resolutions::new())
        .unwrap()
        .unwrap_err();
    assert_eq!(asks, [HOSTS_FILE]);
}

fn bare_repo(root: &Path) -> String {
    let bare = root.join("remote.git");
    let ok = std::process::Command::new("git")
        .args(["init", "--quiet", "--bare", "-b", "main"])
        .arg(&bare)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    bare.to_string_lossy().into_owned()
}

/// This Mac's clone of the bare repository at `url`, as the app makes it.
fn git(dir: &Path, url: &str) -> Box<dyn Remote> {
    remote_for(dir, Some(url)).unwrap()
}

fn run_sync(dir: &Path, url: &str, vault: &Fake, resolve: &Resolutions) -> Outcome {
    sync(dir, &*git(dir, url), vault, None, None, resolve)
        .unwrap()
        .0
}

#[test]
fn two_macs_adding_different_hosts_converge_through_a_git_remote() {
    let root = tempfile::tempdir().unwrap();
    let url = bare_repo(root.path());
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let (va, vb) = (fake("pw"), fake("pw"));
    let none = Resolutions::new();

    std::fs::write(a.join(HOSTS_FILE), hosts(&[("web", "1.1.1.1")])).unwrap();
    assert!(matches!(run_sync(&a, &url, &va, &none), Outcome::Pushed));
    // B has nothing of its own, so it simply takes A's copy (and A's vault).
    let vb_pulled = match sync(&b, &*git(&b, &url), &vb, Some("pw"), None, &none).unwrap() {
        (Outcome::Pulled, Some(v)) => v,
        (o, _) => unreachable!("{}", outcome_name(&o)),
    };

    std::fs::write(
        a.join(HOSTS_FILE),
        hosts(&[("web", "1.1.1.1"), ("db", "2.2.2.2")]),
    )
    .unwrap();
    std::fs::write(
        b.join(HOSTS_FILE),
        hosts(&[("web", "1.1.1.1"), ("cache", "3.3.3.3")]),
    )
    .unwrap();
    assert!(matches!(run_sync(&a, &url, &va, &none), Outcome::Pushed));
    // Both changed: B merges instead of asking, and uploads the union.
    assert!(matches!(
        run_sync(&b, &url, &vb_pulled, &none),
        Outcome::Merged
    ));
    let on_b = std::fs::read_to_string(b.join(HOSTS_FILE)).unwrap();
    assert!(on_b.contains("db") && on_b.contains("cache") && on_b.contains("web"));
    assert!(matches!(run_sync(&a, &url, &va, &none), Outcome::Pulled));
    let on_a = std::fs::read_to_string(a.join(HOSTS_FILE)).unwrap();
    assert!(on_a.contains("db") && on_a.contains("cache"));
    assert!(matches!(
        run_sync(&b, &url, &vb_pulled, &none),
        Outcome::UpToDate
    ));
}

#[test]
fn a_record_without_item_hashes_asks_the_whole_bundle_question_once() {
    let root = tempfile::tempdir().unwrap();
    let url = bare_repo(root.path());
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let none = Resolutions::new();
    let (va, vb) = (fake("pw"), fake("pw"));
    std::fs::write(a.join(HOSTS_FILE), hosts(&[("web", "1.1.1.1")])).unwrap();
    run_sync(&a, &url, &va, &none);
    // B holds something of its own and has the old single-hash record.
    std::fs::write(b.join(HOSTS_FILE), hosts(&[("other", "9.9.9.9")])).unwrap();
    std::fs::write(b.join(STATE_FILE), br#"{"last_hash":"stale"}"#).unwrap();
    let (out, _) = sync(&b, &*git(&b, &url), &vb, None, None, &none).unwrap();
    assert!(matches!(out, Outcome::Conflict { whole: true, .. }));
    // Keep this Mac: pushed, and the record now has per-item hashes.
    let (out, _) = sync(&b, &*git(&b, &url), &vb, None, Some(Plan::Push), &none).unwrap();
    assert!(matches!(out, Outcome::Pushed));
    assert!(!load_state(&b).items.is_empty());
}

#[test]
fn the_same_host_edited_on_both_sides_is_asked_and_the_answer_applies() {
    let root = tempfile::tempdir().unwrap();
    let url = bare_repo(root.path());
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let none = Resolutions::new();
    let va = fake("pw");
    std::fs::write(a.join(HOSTS_FILE), hosts(&[("web", "1.1.1.1")])).unwrap();
    run_sync(&a, &url, &va, &none);
    let vb = match sync(&b, &*git(&b, &url), &va, Some("pw"), None, &none).unwrap() {
        (Outcome::Pulled, Some(v)) => v,
        (o, _) => unreachable!("{}", outcome_name(&o)),
    };
    std::fs::write(a.join(HOSTS_FILE), hosts(&[("web", "10.0.0.1")])).unwrap();
    std::fs::write(b.join(HOSTS_FILE), hosts(&[("web", "11.0.0.1")])).unwrap();
    run_sync(&a, &url, &va, &none);
    match run_sync(&b, &url, &vb, &none) {
        Outcome::Conflict {
            whole: false,
            items,
        } => assert_eq!(items, [HOSTS_FILE]),
        o => unreachable!("{}", outcome_name(&o)),
    }
    assert!(
        std::fs::read_to_string(b.join(HOSTS_FILE))
            .unwrap()
            .contains("11.0.0.1"),
        "asking changes nothing"
    );
    let choose = Resolutions::from([(HOSTS_FILE.to_owned(), Side::Remote)]);
    assert!(matches!(run_sync(&b, &url, &vb, &choose), Outcome::Merged));
    assert!(
        std::fs::read_to_string(b.join(HOSTS_FILE))
            .unwrap()
            .contains("10.0.0.1")
    );
}

fn outcome_name(o: &Outcome) -> &'static str {
    match o {
        Outcome::UpToDate => "UpToDate",
        Outcome::Pushed => "Pushed",
        Outcome::Pulled => "Pulled",
        Outcome::Merged => "Merged",
        Outcome::Conflict { .. } => "Conflict",
    }
}

#[test]
fn approved_password_commands_never_travel_in_either_direction() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("approved-commands.json"),
        br#"["gopass show -o x"]"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("hosts.json"), hosts(&[("a", "h")])).unwrap();
    let sent = read_local(dir.path()).unwrap();
    assert!(sent.contains_key("hosts.json"));
    assert!(!sent.contains_key("approved-commands.json"), "{sent:?}");
    // A bundle from another Mac cannot pre-approve a command here either.
    let incoming = br#"{"approved-commands.json":"WyJjdXJsIGV2aWwiXQ==","hosts.json":"e30="}"#;
    let got = decode(incoming).unwrap();
    assert!(!got.contains_key("approved-commands.json"));
}

/// A remote as a push leaves it, for the bundle checks below.
fn remote_of(files: &Files, seq: u64, with: &Fake) -> crate::RemoteBundle {
    let hash = crate::hash(files);
    let (vault, sealed) = seal_bundle(files, &hash, seq, with).unwrap();
    crate::RemoteBundle {
        hash,
        updated_at: 0,
        device: "mac".into(),
        seq,
        vault,
        sealed,
    }
}

#[test]
fn a_pull_takes_only_a_bundle_whose_parts_belong_together_and_are_not_older() {
    let v = fake("pw");
    let new = bundle(&[(HOSTS_FILE, b"[]"), (VAULT_FILE, b"newer vault")]);
    let old = bundle(&[(HOSTS_FILE, b"[1]"), (VAULT_FILE, b"older vault")]);
    let good = remote_of(&new, 7, &v);
    let (files, seq) = open_bundle(&good, &v, 3).unwrap();
    assert_eq!((files, seq), (new.clone(), 7));

    // An older vault.age put back beside the newer sealed data.
    let mut swapped = remote_of(&new, 7, &v);
    swapped.vault = b"older vault".to_vec();
    assert!(
        open_bundle(&swapped, &v, 3)
            .unwrap_err()
            .contains("changed outside tern")
    );

    // tern-meta.json edited to name another bundle.
    let mut meta_edited = remote_of(&new, 7, &v);
    meta_edited.hash = crate::hash(&old);
    assert!(open_bundle(&meta_edited, &v, 3).is_err());

    // A whole earlier bundle restored: it holds together but is older than one seen here.
    let rolled_back = remote_of(&new, 2, &v);
    assert!(
        open_bundle(&rolled_back, &v, 3)
            .unwrap_err()
            .contains("rolled back")
    );
}

#[test]
fn a_bundle_from_a_tern_before_the_manifest_is_checked_against_its_meta() {
    let v = fake("pw");
    let files = bundle(&[(HOSTS_FILE, b"[]"), (VAULT_FILE, b"vault")]);
    let mut rest = files.clone();
    let vault = rest.remove(VAULT_FILE).unwrap();
    let legacy = |hash: String| crate::RemoteBundle {
        hash,
        updated_at: 0,
        device: "old".into(),
        seq: 0,
        vault: vault.clone(),
        sealed: v.seal(&encode(&rest).unwrap()).unwrap(),
    };
    assert_eq!(
        open_bundle(&legacy(crate::hash(&files)), &v, 4).unwrap(),
        (files, 4)
    );
    assert!(open_bundle(&legacy("edited".into()), &v, 4).is_err());
}

#[test]
fn every_push_is_numbered_above_the_last_one_seen() {
    let root = tempfile::tempdir().unwrap();
    let url = bare_repo(root.path());
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let none = Resolutions::new();
    let va = fake("pw");
    std::fs::write(a.join(HOSTS_FILE), hosts(&[("web", "1.1.1.1")])).unwrap();
    run_sync(&a, &url, &va, &none);
    assert_eq!(load_state(&a).seq, 1);
    let vb = match sync(&b, &*git(&b, &url), &va, Some("pw"), None, &none).unwrap() {
        (Outcome::Pulled, Some(v)) => v,
        (o, _) => unreachable!("{}", outcome_name(&o)),
    };
    assert_eq!(load_state(&b).seq, 1);
    std::fs::write(b.join(HOSTS_FILE), hosts(&[("db", "2.2.2.2")])).unwrap();
    assert!(matches!(run_sync(&b, &url, &vb, &none), Outcome::Pushed));
    assert_eq!(load_state(&b).seq, 2);
    assert!(matches!(run_sync(&a, &url, &va, &none), Outcome::Pulled));
    assert_eq!(load_state(&a).seq, 2);
}
