//! The git side: a private repository the user owns, driven through their own `git`, so their
//! SSH keys and credential helpers do the authentication. The clone lives in tern's settings
//! directory; each sync is one commit. Blocking; callers run it off the UI thread.

use std::path::{Path, PathBuf};
use std::process::Output;

use serde::{Deserialize, Serialize};

use crate::github::{GhCli, GithubError, RemoteBundle, current_gh};

const META: &str = "tern-meta.json";
const VAULT: &str = "vault.age";
const DATA: &str = "data.age";
const BRANCH: &str = "main";
/// The most a synced `vault.age` or `data.age` may hold; a real vault is kilobytes, so a bigger
/// file is a broken or hostile remote, and reading it whole would balloon memory.
const MAX_FILE: u64 = 16 * 1024 * 1024;
/// `tern-meta.json` is a hash, a time and a device name.
const MAX_META: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    version: u32,
    hash: String,
    updated_at: u64,
    device: String,
    /// The pusher's bundle number, a hint for the next one; 0 from a tern before numbering.
    /// The checked number travels sealed (the app's `tern-bundle` manifest).
    #[serde(default)]
    seq: u64,
}

#[derive(Debug)]
pub struct GitRepo {
    remote: String,
    dir: PathBuf,
}

/// The file at `path`, refused before it is read when it is longer than `cap` bytes.
fn read_capped(path: &Path, name: &str, cap: u64) -> Result<Vec<u8>, GithubError> {
    let format = |e: &dyn std::fmt::Display| GithubError::Format(format!("{name}: {e}"));
    let len = std::fs::metadata(path).map_err(|e| format(&e))?.len();
    if len > cap {
        return Err(format(&format_args!(
            "{len} bytes is over the {cap} byte limit"
        )));
    }
    std::fs::read(path).map_err(|e| format(&e))
}

// Runs on a background thread, never on an executor, so blocking on git is fine here.
#[allow(clippy::disallowed_methods)]
fn run(dir: Option<&Path>, args: &[&str]) -> Result<Output, GithubError> {
    command(dir, args)
        .output()
        .map_err(|e| GithubError::Http(format!("git: {e}")))
}

/// Variables git exports to hooks (and that a parent shell may carry) which point git at a
/// different repository; they beat `-C`, so a sync started from inside a hook would read and
/// rewrite that repository instead of the sync clone.
const REPO_VARS: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];

fn command(dir: Option<&Path>, args: &[&str]) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    for var in REPO_VARS {
        cmd.env_remove(var);
    }
    // Never stop for a password prompt: tern has no terminal for git to ask in.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.args(args);
    cmd
}

fn ok(out: Output, what: &str) -> Result<Output, GithubError> {
    if out.status.success() {
        return Ok(out);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let line = err
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("failed");
    Err(GithubError::Http(format!("git {what}: {}", line.trim())))
}

impl GitRepo {
    /// `dir` is where the clone is kept (inside tern's settings directory).
    pub fn new(remote: String, dir: PathBuf) -> Self {
        Self { remote, dir }
    }

    /// Clones on first use, then brings the clone to the remote's state. An empty remote
    /// (a fresh repository) is fine: there is just nothing to read yet.
    fn refresh(&self) -> Result<(), GithubError> {
        if !self.dir.join(".git").exists() {
            if let Some(parent) = self.dir.parent() {
                std::fs::create_dir_all(parent).map_err(|e| GithubError::Http(e.to_string()))?;
            }
            let dir = self.dir.to_string_lossy();
            ok(
                run(None, &["clone", "--quiet", &self.remote, &dir])?,
                "clone",
            )?;
            return Ok(());
        }
        ok(
            run(
                Some(&self.dir),
                &["remote", "set-url", "origin", &self.remote],
            )?,
            "remote",
        )?;
        ok(
            run(Some(&self.dir), &["fetch", "--quiet", "origin"])?,
            "fetch",
        )?;
        let has_remote_branch = run(
            Some(&self.dir),
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("origin/{BRANCH}"),
            ],
        )?
        .status
        .success();
        if has_remote_branch {
            ok(
                run(
                    Some(&self.dir),
                    &[
                        "checkout",
                        "--quiet",
                        "-B",
                        BRANCH,
                        &format!("origin/{BRANCH}"),
                    ],
                )?,
                "checkout",
            )?;
        }
        Ok(())
    }

    /// The repository's bundle, or `None` when nothing was pushed yet.
    ///
    /// # Errors
    /// git failures (auth, network, a remote that is not a repository) or a bundle in the
    /// wrong format.
    pub fn fetch(&self) -> Result<Option<RemoteBundle>, GithubError> {
        self.refresh()?;
        let meta_path = self.dir.join(META);
        if std::fs::metadata(&meta_path).is_err() {
            return Ok(None);
        }
        let meta = read_capped(&meta_path, META, MAX_META)?;
        let meta: Meta =
            serde_json::from_slice(&meta).map_err(|e| GithubError::Format(e.to_string()))?;
        let read = |name: &str| read_capped(&self.dir.join(name), name, MAX_FILE);
        Ok(Some(RemoteBundle {
            hash: meta.hash,
            updated_at: meta.updated_at,
            device: meta.device,
            seq: meta.seq,
            vault: read(VAULT)?,
            sealed: read(DATA)?,
        }))
    }

    /// Writes the bundle, commits and pushes.
    ///
    /// # Errors
    /// git failures, including a rejected push when the remote moved meanwhile.
    pub fn push(
        &self,
        hash: &str,
        seq: u64,
        device: &str,
        vault: &[u8],
        sealed: &[u8],
    ) -> Result<(), GithubError> {
        self.refresh()?;
        let meta = Meta {
            version: 1,
            hash: hash.to_owned(),
            updated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            device: device.to_owned(),
            seq,
        };
        let write = |name: &str, bytes: &[u8]| {
            std::fs::write(self.dir.join(name), bytes).map_err(|e| GithubError::Http(e.to_string()))
        };
        write(
            META,
            serde_json::to_string_pretty(&meta)
                .map_err(|e| GithubError::Format(e.to_string()))?
                .as_bytes(),
        )?;
        write(VAULT, vault)?;
        write(DATA, sealed)?;
        let dir = Some(self.dir.as_path());
        ok(
            run(dir, &["checkout", "--quiet", "-B", BRANCH])?,
            "checkout",
        )?;
        ok(run(dir, &["add", META, VAULT, DATA])?, "add")?;
        let message = format!("tern sync from {device}");
        // Identity is only needed if the user has none configured; it names the app, not them.
        ok(
            run(
                dir,
                &[
                    "-c",
                    "user.name=tern",
                    "-c",
                    "user.email=tern@localhost",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    &message,
                ],
            )?,
            "commit",
        )?;
        ok(
            run(dir, &["push", "--quiet", "-u", "origin", BRANCH])?,
            "push",
        )?;
        Ok(())
    }
}

/// Creates a private `tern-sync` repository with the GitHub CLI and returns its SSH URL.
///
/// # Errors
/// When `gh` is missing, signed out, or the name is taken by something unusable.
pub fn create_github_repo() -> Result<String, GithubError> {
    create_github_repo_with(&*current_gh())
}

pub(crate) fn create_github_repo_with(gh: &dyn GhCli) -> Result<String, GithubError> {
    let run = |args: &[&str]| {
        gh.run(args)
            .map_err(|e| GithubError::Http(format!("gh: {e}")))
    };
    let view = run(&[
        "repo",
        "view",
        "tern-sync",
        "--json",
        "sshUrl",
        "-q",
        ".sshUrl",
    ])?;
    if view.success {
        return Ok(String::from_utf8_lossy(&view.stdout).trim().to_owned());
    }
    let created = run(&[
        "repo",
        "create",
        "tern-sync",
        "--private",
        "--description",
        "tern sync data (encrypted)",
    ])?;
    if !created.success {
        let err = String::from_utf8_lossy(&created.stderr);
        return Err(GithubError::Http(format!("gh repo create: {}", err.trim())));
    }
    let view = run(&[
        "repo",
        "view",
        "tern-sync",
        "--json",
        "sshUrl",
        "-q",
        ".sshUrl",
    ])?;
    Ok(String::from_utf8_lossy(&view.stdout).trim().to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::github::FakeGh;

    /// Inside a git hook `GIT_DIR` names the outer repository and beats `-C`; the sync clone's
    /// git must not inherit it.
    #[test]
    fn git_runs_without_the_callers_repository_variables() {
        let cmd = command(Some(Path::new("/tmp/clone")), &["status"]);
        let envs: Vec<_> = cmd.get_envs().collect();
        for var in REPO_VARS {
            assert!(
                envs.iter().any(|(k, v)| *k == var && v.is_none()),
                "{var} is not cleared"
            );
        }
    }

    /// A bare repository on disk stands in for GitHub: push from one clone, fetch from another.
    #[test]
    fn push_from_one_clone_is_read_by_another() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("remote.git");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet", "--bare", "-b", "main"])
            .arg(&bare)
            .status()
            .unwrap();
        assert!(status.success());
        let url = bare.to_string_lossy().into_owned();

        let a = GitRepo::new(url.clone(), tmp.path().join("a"));
        assert!(
            a.fetch().unwrap().is_none(),
            "an empty remote has nothing yet"
        );
        a.push("h1", 1, "mac-a", b"vault-bytes", b"sealed-bytes")
            .unwrap();

        let b = GitRepo::new(url.clone(), tmp.path().join("b"));
        let got = b.fetch().unwrap().unwrap();
        assert_eq!(got.hash, "h1");
        assert_eq!(got.device, "mac-a");
        assert_eq!(got.vault, b"vault-bytes");
        assert_eq!(got.sealed, b"sealed-bytes");

        // A second push from b is seen by a on its next fetch.
        b.push("h2", 2, "mac-b", b"v2", b"s2").unwrap();
        assert_eq!(a.fetch().unwrap().unwrap().hash, "h2");
    }

    /// The limits are part of the behaviour: a slipped `*` in the arithmetic is a test failure.
    #[test]
    fn the_limits_are_the_documented_sizes() {
        assert_eq!(MAX_FILE, 16 * 1024 * 1024);
        assert_eq!(MAX_META, 64 * 1024);
    }

    /// The metadata file comes from the remote, so a huge one is refused like the other files.
    #[test]
    fn an_oversized_metadata_file_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("remote.git");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet", "--bare", "-b", "main"])
            .arg(&bare)
            .status()
            .unwrap();
        assert!(status.success());
        let url = bare.to_string_lossy().into_owned();

        // A device name that long would not fit on a command line (Windows allows 32 KiB), so the
        // oversized file is committed by hand into the first clone.
        let a = GitRepo::new(url.clone(), tmp.path().join("a"));
        a.push("h1", 1, "mac-a", b"v", b"s").unwrap();
        std::fs::write(a.dir.join(META), vec![b' '; 128 * 1024]).unwrap();
        let dir = Some(a.dir.as_path());
        ok(run(dir, &["add", META]).unwrap(), "add").unwrap();
        let commit = [
            "-c",
            "user.name=tern",
            "-c",
            "user.email=tern@localhost",
            "commit",
            "--quiet",
            "-m",
            "big meta",
        ];
        ok(run(dir, &commit).unwrap(), "commit").unwrap();
        ok(
            run(dir, &["push", "--quiet", "origin", BRANCH]).unwrap(),
            "push",
        )
        .unwrap();
        let err = GitRepo::new(url, tmp.path().join("b"))
            .fetch()
            .unwrap_err()
            .to_string();
        assert!(err.contains("tern-meta.json"), "{err}");
    }

    #[test]
    fn ok_extracts_first_non_empty_error_line_or_fails() {
        let mut out = command(None, &["unknown-command-xyz"]).output().unwrap();
        assert!(!out.status.success());

        out.stderr = b"\n\n  fatal: could not read repository  \nother details\n".to_vec();
        assert_eq!(
            ok(out, "clone").unwrap_err().to_string(),
            "GitHub: git clone: fatal: could not read repository"
        );

        let mut out_empty = command(None, &["unknown-command-xyz"]).output().unwrap();
        out_empty.stderr = b"\n  \t\n".to_vec();
        assert_eq!(
            ok(out_empty, "fetch").unwrap_err().to_string(),
            "GitHub: git fetch: failed"
        );
    }

    #[test]
    fn create_github_repo_returns_existing_without_creating() {
        let gh = FakeGh::default();
        gh.respond(
            "repo view tern-sync --json sshUrl -q .sshUrl",
            Ok(crate::github::GhOutput {
                success: true,
                stdout: b"git@github.com:user/tern-sync.git\n".to_vec(),
                stderr: Vec::new(),
            }),
        );
        let _g = crate::github::set_test_gh(Box::new(gh.clone()));

        let url = create_github_repo().unwrap();
        assert_eq!(url, "git@github.com:user/tern-sync.git");

        let calls = gh.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][1], "view");
    }

    #[test]
    fn create_github_repo_creates_when_not_existing() {
        let gh = FakeGh::default();
        // First view fails
        gh.respond(
            "repo view tern-sync --json sshUrl -q .sshUrl",
            Ok(crate::github::GhOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"not found".to_vec(),
            }),
        );
        // Second view succeeds
        gh.respond(
            "repo view tern-sync --json sshUrl -q .sshUrl",
            Ok(crate::github::GhOutput {
                success: true,
                stdout: b"git@github.com:user/tern-sync.git\n".to_vec(),
                stderr: Vec::new(),
            }),
        );
        // Create succeeds
        gh.respond(
            "repo create tern-sync --private --description tern sync data (encrypted)",
            Ok(crate::github::GhOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            }),
        );
        let _g = crate::github::set_test_gh(Box::new(gh.clone()));

        let url = create_github_repo().unwrap();
        assert_eq!(url, "git@github.com:user/tern-sync.git");

        let calls = gh.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0][1], "view");
        assert_eq!(calls[1][1], "create");
        assert_eq!(calls[2][1], "view");
    }

    #[test]
    fn create_github_repo_error_when_creation_fails() {
        let gh = FakeGh::default();
        gh.respond(
            "repo view tern-sync --json sshUrl -q .sshUrl",
            Ok(crate::github::GhOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"not found".to_vec(),
            }),
        );
        gh.respond(
            "repo create tern-sync --private --description tern sync data (encrypted)",
            Ok(crate::github::GhOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"name taken\n".to_vec(),
            }),
        );
        let _g = crate::github::set_test_gh(Box::new(gh));

        let err = create_github_repo().unwrap_err();
        assert_eq!(err.to_string(), "GitHub: gh repo create: name taken");
    }

    #[test]
    fn create_github_repo_error_when_cli_fails_to_spawn() {
        let gh = FakeGh::default();
        gh.respond(
            "repo view tern-sync --json sshUrl -q .sshUrl",
            Err("gh executable not found".into()),
        );
        let _g = crate::github::set_test_gh(Box::new(gh));

        let err = create_github_repo().unwrap_err();
        assert_eq!(err.to_string(), "GitHub: gh: gh executable not found");
    }

    #[test]
    fn a_file_over_the_cap_is_refused_and_one_at_the_cap_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.age");
        std::fs::write(&path, [7u8; 8]).unwrap();
        assert_eq!(read_capped(&path, "vault.age", 8).unwrap(), [7u8; 8]);
        let err = read_capped(&path, "vault.age", 7).unwrap_err().to_string();
        assert!(
            err.contains("vault.age: 8 bytes is over the 7 byte limit"),
            "{err}"
        );
    }

    #[test]
    fn the_real_cap_refuses_a_sparse_file_one_byte_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.age");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_FILE + 1).unwrap();
        assert!(read_capped(&path, "data.age", MAX_FILE).is_err());
        file.set_len(MAX_FILE).unwrap();
        assert_eq!(
            read_capped(&path, "data.age", MAX_FILE).unwrap().len() as u64,
            MAX_FILE
        );
    }
}
