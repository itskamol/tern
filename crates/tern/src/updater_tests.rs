//! The checks a downloaded update must pass, against real (ad-hoc signed) bundles, and the
//! swap helper against plain folders. Neither reaches the network or opens an app.
use std::path::Path;

use super::*;

/// A minimal bundle: `id` and `version` in its Info.plist, a copy of `/usr/bin/true` as its
/// executable (`marker` makes the copies differ, so their signatures do), signed ad hoc.
fn bundle(dir: &Path, name: &str, id: &str, version: &str, marker: &str) -> PathBuf {
    let app = dir.join(name);
    std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
    std::fs::write(
        app.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleExecutable</key><string>tern</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#
        ),
    )
    .unwrap();
    std::fs::copy("/usr/bin/true", app.join("Contents/MacOS/tern")).unwrap();
    std::fs::write(app.join("Contents/Resources/marker"), marker).unwrap();
    let signed = Command::new("/usr/bin/codesign")
        .args(["--force", "-s", "-"])
        .arg(&app)
        .output()
        .unwrap();
    assert!(signed.status.success(), "{signed:?}");
    app
}

/// What `codesign` names as `app`'s requirement, implied or not.
fn designated(app: &Path) -> String {
    let out = Command::new("/usr/bin/codesign")
        .args(["-d", "-r-"])
        .arg(app)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&[out.stdout, out.stderr].concat()).into_owned();
    text.lines()
        .find_map(|l| l.split_once("designated => ").map(|(_, r)| r.to_owned()))
        .unwrap()
}

#[test]
fn only_a_certificate_requirement_lets_a_build_update_itself() {
    let named = "Executable=/Applications/tern.app/Contents/MacOS/tern\n\
                 designated => identifier \"mn.tern.app\" and certificate leaf = H\"f787\"\n";
    assert_eq!(
        parse_requirement(named).as_deref(),
        Some("identifier \"mn.tern.app\" and certificate leaf = H\"f787\"")
    );
    // Ad hoc: the requirement is the build's own hash, which no other build meets.
    let adhoc = "Executable=/x\n# designated => cdhash H\"6bba0b0e\"\n";
    assert_eq!(parse_requirement(adhoc), None);
    assert_eq!(parse_requirement(""), None);
}

#[test]
fn an_update_must_be_this_app_at_the_announced_version_and_signed_like_it() {
    let dir = tempfile::tempdir().unwrap();
    let current = bundle(dir.path(), "current.app", "mn.tern.app", "0.2.0", "a");
    let next = bundle(dir.path(), "next.app", "mn.tern.app", "0.2.1", "b");
    // `next`'s own requirement stands in for a certificate both builds were signed with.
    let signed_like_next = designated(&next);
    assert_eq!(check(&next, "0.2.1", &current, &signed_like_next), Ok(()));

    let err = check(&next, "0.2.2", &current, &signed_like_next).unwrap_err();
    assert!(err.contains("not 0.2.2"), "{err}");

    let other = bundle(dir.path(), "other.app", "com.example.other", "0.2.1", "b");
    let err = check(&other, "0.2.1", &current, &designated(&other)).unwrap_err();
    assert!(err.contains("another app"), "{err}");

    // Signed by someone else: it does not meet the running app's requirement.
    let forged = bundle(dir.path(), "forged.app", "mn.tern.app", "0.2.1", "c");
    let err = check(&forged, "0.2.1", &current, &signed_like_next).unwrap_err();
    assert!(err.contains("not signed like this tern"), "{err}");

    // Changed after signing: the seal is broken.
    std::fs::write(next.join("Contents/Resources/marker"), "tampered").unwrap();
    assert!(check(&next, "0.2.1", &current, &signed_like_next).is_err());
}

#[test]
fn the_swap_waits_for_the_old_process_then_keeps_only_the_new_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let installed = dir.path().join("tern.app");
    let new = dir.path().join("staged/tern.app");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(installed.join("which"), "old").unwrap();
    std::fs::create_dir_all(&new).unwrap();
    std::fs::write(new.join("which"), "new").unwrap();
    // Stands in for the running tern: alive for a moment, then gone.
    let mut running = Command::new("/bin/sleep").arg("0.5").spawn().unwrap();
    let mut helper = spawn_install(running.id(), &installed, &new, "/usr/bin/true").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        std::fs::read_to_string(installed.join("which")).unwrap(),
        "old",
        "nothing moves while the old process runs"
    );
    running.wait().unwrap();
    assert!(helper.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(installed.join("which")).unwrap(),
        "new"
    );
    assert!(!new.exists());
    assert!(
        !dir.path().join("tern.app.previous").exists(),
        "the old bundle is gone once the new one opened"
    );
}
