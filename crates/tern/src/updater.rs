//! Self-update on macOS. A release the update check found is downloaded and kept only if it is
//! signed like the running app: the same designated requirement (bundle id and the signing
//! certificate, "tern local" or a Developer ID). It is swapped in when the person asks, after
//! this process has exited. An ad-hoc signed build's requirement is its own hash, which no
//! other build meets, so it never updates itself; the check then only offers the release page.

// Blocking by design: everything here runs on the background executor, never the UI thread.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The largest download accepted; a release zip is about 20 MB.
const MAX_ZIP: u64 = 256 * 1024 * 1024;

/// Swaps the new bundle in once the old process has gone, keeping the old one until the new one
/// has been opened. `$0` is a label; `$1` pid, `$2` the installed app, `$3` the new app, `$4` the
/// command that opens an app (`open`).
const INSTALL: &str = r#"
pid=$1; old=$2; new=$3; opener=$4; keep="$old.previous"
while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
rm -rf "$keep"
mv "$old" "$keep" || exit 1
if ! mv "$new" "$old" 2>/dev/null && ! /usr/bin/ditto "$new" "$old"; then
  rm -rf "$old"; mv "$keep" "$old"; "$opener" "$old"; exit 1
fi
/usr/bin/xattr -dr com.apple.quarantine "$old" 2>/dev/null
"$opener" "$old" && rm -rf "$keep"
"#;

/// An HTTPS agent with the platform's roots, as the update check uses.
fn agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .timeout_global(Some(timeout))
        .http_status_as_error(true)
        .build();
    ureq::Agent::new_with_config(config)
}

/// The running app bundle, when tern runs from one (`…/tern.app/Contents/MacOS/tern`).
pub fn running_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

/// Whether this tern can update itself: it runs from a bundle in a folder it may write, signed
/// with a certificate. Returns the bundle and the requirement a new build must meet.
pub fn self_updatable() -> Option<(PathBuf, String)> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return None;
    }
    let app = running_bundle()?;
    let requirement = requirement(&app)?;
    writable_parent(&app).then_some((app, requirement))
}

/// The designated requirement `app` is signed with, when it names a certificate. `None` for an
/// ad-hoc signature, whose requirement is only implied (`# designated => cdhash …`).
fn requirement(app: &Path) -> Option<String> {
    let out = Command::new("/usr/bin/codesign")
        .args(["-d", "-r-"])
        .arg(app)
        .output()
        .ok()?;
    let text = [out.stdout, out.stderr].concat();
    parse_requirement(&String::from_utf8_lossy(&text))
}

fn parse_requirement(codesign: &str) -> Option<String> {
    codesign
        .lines()
        .find_map(|line| line.strip_prefix("designated => "))
        .filter(|requirement| requirement.contains("certificate"))
        .map(str::to_owned)
}

/// The folder holding `app` takes a new entry, so the swap can rename in place.
fn writable_parent(app: &Path) -> bool {
    let Some(parent) = app.parent() else {
        return false;
    };
    let probe = parent.join(".tern-update-probe");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// A release downloaded, unpacked and checked, waiting to be swapped in.
#[derive(Debug)]
pub struct Staged {
    pub version: String,
    app: PathBuf,
    /// The installed bundle it replaces.
    current: PathBuf,
}

/// Downloads release `version` from `zip_url` into `cache` and keeps it only if [`check`]
/// passes against the running bundle `current` and its `requirement`.
///
/// # Errors
/// What went wrong, for the log: the download, the unpacking, or a check.
pub fn stage(
    zip_url: &str,
    version: &str,
    current: &Path,
    requirement: &str,
    cache: &Path,
) -> Result<Staged, String> {
    let dir = cache.join("update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    private(&dir)?;
    let zip = dir.join("tern.zip");
    download(zip_url, &zip)?;
    let unpacked = dir.join("unpacked");
    run(Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(&zip)
        .arg(&unpacked))?;
    let app = unpacked.join("tern.app");
    check(&app, version, current, requirement)?;
    Ok(Staged {
        version: version.to_owned(),
        app,
        current: current.to_path_buf(),
    })
}

/// What a downloaded bundle must be: the running app's bundle id, the version announced, and
/// signed so that it meets the running app's designated requirement.
///
/// # Errors
/// Which of those failed.
pub fn check(app: &Path, version: &str, current: &Path, requirement: &str) -> Result<(), String> {
    let id = |app: &Path| plist(app, "CFBundleIdentifier");
    if id(app)? != id(current)? {
        return Err("the update is another app".into());
    }
    let found = plist(app, "CFBundleShortVersionString")?;
    if found != version {
        return Err(format!("the update is {found}, not {version}"));
    }
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(format!("-R={requirement}"))
        .arg(app))
    .map_err(|e| format!("the update is not signed like this tern: {e}"))
}

/// Swaps `staged` in for the installed bundle once this process has exited, then opens it.
/// Runs detached, so it outlives this process; the caller quits right after.
///
/// # Errors
/// The helper could not be started.
pub fn install(staged: &Staged) -> Result<(), String> {
    spawn_install(
        std::process::id(),
        &staged.current,
        &staged.app,
        "/usr/bin/open",
    )
    .map(|_| ())
}

fn spawn_install(
    pid: u32,
    current: &Path,
    new: &Path,
    opener: &str,
) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt as _;
    Command::new("/bin/sh")
        .args(["-c", INSTALL, "tern-update", &pid.to_string()])
        .arg(current)
        .arg(new)
        .arg(opener)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())
}

fn download(url: &str, to: &Path) -> Result<(), String> {
    let mut response = agent(Duration::from_secs(300))
        .get(url)
        .header("User-Agent", "tern")
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let mut reader = response.body_mut().with_config().limit(MAX_ZIP).reader();
    let mut file = std::fs::File::create(to).map_err(|e| e.to_string())?;
    std::io::copy(&mut reader, &mut file).map_err(|e| format!("{url}: {e}"))?;
    Ok(())
}

fn plist(app: &Path, key: &str) -> Result<String, String> {
    let out = Command::new("/usr/bin/plutil")
        .args(["-extract", key, "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("{key} missing in {}", app.display()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn run(command: &mut Command) -> Result<(), String> {
    let out = command.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
}

fn private(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "updater_tests.rs"]
mod tests;
