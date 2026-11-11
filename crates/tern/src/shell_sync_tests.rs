#![allow(clippy::unwrap_used, clippy::disallowed_methods)]
//! The app's wiring of a sync: the real vault through `run`, against a local bare repository.
//! The sync's own rules are tested in `tern_sync::bundle`.
use super::*;

#[test]
fn throttle_lets_one_event_per_window_through_and_counts_from_the_allowed_one() {
    let t0 = Instant::now();
    let s = Duration::from_secs;
    let mut gate = Throttle::new(s(60));
    assert!(gate.allow(t0), "the first event passes");
    assert!(!gate.allow(t0 + s(1)));
    assert!(!gate.allow(t0 + s(59)));
    // A refused event must not push the window out: 61 s after the allowed one passes.
    assert!(gate.allow(t0 + s(61)));
    assert!(!gate.allow(t0 + s(100)), "the clock restarted at 61");
    assert!(gate.allow(t0 + s(121)));
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

/// Real age encryption both ways: a vault created by the first sync, another Mac opening the
/// remote's vault with the typed passphrase after a wrong one was refused.
#[test]
fn a_new_vault_syncs_to_a_second_mac_that_types_its_passphrase() {
    let root = tempfile::tempdir().unwrap();
    let url = bare_repo(root.path());
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("hosts.json"), br#"{"version":1,"connections":[]}"#).unwrap();
    let none = Resolutions::new();

    let (kept, out) = run(&a, Some(&url), None, Some("pw"), None, &none);
    assert!(matches!(out, Ok(Outcome::Pushed)), "{:?}", out.err());
    assert!(
        a.join(VAULT_FILE).exists(),
        "the vault the sync started is saved"
    );
    let mut ours = kept.unwrap();
    ours.set(
        tern_vault::Key::Password {
            user: "u".into(),
            host: "h".into(),
            port: 22,
        },
        SecretString::from("secret".to_owned()),
    );
    // What travels is the file, which the keeper writes after every change.
    ours.save(&a.join(VAULT_FILE)).unwrap();
    let (ours, out) = run(&a, Some(&url), Some(ours), None, Some(Plan::Push), &none);
    assert!(matches!(out, Ok(Outcome::Pushed)));
    assert_eq!(ours.unwrap().len(), 1);

    // A mistyped passphrase on a Mac with no vault yet takes nothing and leaves nothing, so
    // the right one works next.
    let (_, wrong) = run(&b, Some(&url), None, Some("nope"), None, &none);
    assert!(wrong.unwrap_err().contains("another passphrase"));
    assert!(!b.join("hosts.json").exists(), "nothing was taken");
    assert!(
        !b.join(VAULT_FILE).exists(),
        "no vault under the wrong passphrase"
    );
    let (pulled, out) = run(&b, Some(&url), None, Some("pw"), None, &none);
    assert!(matches!(out, Ok(Outcome::Pulled)), "{out:?}");
    assert_eq!(pulled.unwrap().len(), 1, "B holds A's vault");
    assert!(b.join("hosts.json").exists());
}
