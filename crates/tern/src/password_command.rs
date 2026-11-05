//! A connection's "Password command": a local shell command whose stdout is the login
//! password (`gopass show -o …`), so opening a host lands in a logged-in shell.
//!
//! The order when a server asks for a password is command, vault, the user. A command that
//! came in through sync does not run until this Mac approved it once; see [`Approved`].

// `run` blocks by design: the session calls it from a background-executor task, never from the
// UI thread, and a timeout needs a polling wait.
#![allow(clippy::disallowed_methods)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tern_ssh::SecretString;

/// gopass may pop a pinentry / GPG prompt, so the user gets time to answer it.
pub const TIMEOUT: Duration = Duration::from_secs(20);

const APPROVED_FILE: &str = "approved-commands.json";

/// What a session knows about logging in beyond the server's own prompts.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    /// Names of vault keys offered at login. They are looked up each time the session dials,
    /// so the spec never holds key material between connections.
    pub vault_keys: Vec<String>,
    pub password_command: Option<String>,
    /// The "Sudo password command"; see `session_fill`.
    pub sudo_command: Option<String>,
    /// The saved connection's ProxyCommand. Like the password command it may have come in
    /// through sync or an import, so it runs only once this Mac approved it. A `~/.ssh/config`
    /// host's is not here: that file is the person's own, and runs as ssh would run it.
    pub proxy_command: Option<String>,
}

/// What to do about the command when a password prompt arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// No command, or it already had its turn on this connection: go on to the vault.
    Skip,
    /// The command was never approved on this Mac: ask first.
    Approve,
    Run,
}

/// The command gets one turn per connection, whatever came of it. A server that rejects the
/// password it printed asks again, and running it again would only count failures toward a
/// lockout; the second ask goes to the vault and then the user.
pub fn plan(command: Option<&str>, already_tried: bool, approved: bool) -> Step {
    match command {
        Some(_) if already_tried => Step::Skip,
        Some(_) if !approved => Step::Approve,
        Some(_) => Step::Run,
        None => Step::Skip,
    }
}

/// Why a command gave no password.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Exit {
        code: i32,
        stderr: String,
    },
    /// Killed by a signal, so there is no exit code.
    Signal {
        stderr: String,
    },
    Timeout,
    Start(String),
}

impl Failure {
    /// The dim line shown in the terminal. Only the first stderr line, with control characters
    /// dropped so a command cannot write escape codes into the terminal.
    pub fn line(&self) -> String {
        let clean = |s: &str| -> String {
            s.lines()
                .next()
                .unwrap_or("")
                .chars()
                .filter(|c| !c.is_control())
                .collect()
        };
        match self {
            Failure::Exit { code, stderr } => {
                format!("password command failed (exit {code}): {}", clean(stderr))
            }
            Failure::Signal { stderr } => {
                format!("password command failed (signal): {}", clean(stderr))
            }
            Failure::Timeout => {
                format!("password command failed (timeout {}s)", TIMEOUT.as_secs())
            }
            Failure::Start(e) => format!("password command failed (not started): {}", clean(e)),
        }
    }
}

/// `PATH` for the command: a GUI app launched from Finder gets a minimal one without
/// Homebrew, where gopass lives.
#[cfg(unix)]
fn command_path(inherited: Option<&str>, home: Option<&str>) -> String {
    let mut parts = vec!["/opt/homebrew/bin".to_owned(), "/usr/local/bin".to_owned()];
    if let Some(home) = home {
        parts.push(format!("{home}/go/bin"));
    }
    parts.extend(inherited.filter(|p| !p.is_empty()).map(str::to_owned));
    parts.join(":")
}

/// One trailing newline off, no other trimming: a password may start or end with spaces.
fn secret_from(mut out: Vec<u8>) -> Option<SecretString> {
    if out.last() == Some(&b'\n') {
        out.pop();
        if out.last() == Some(&b'\r') {
            out.pop();
        }
    }
    if out.is_empty() {
        return None;
    }
    match String::from_utf8(out) {
        Ok(s) => Some(SecretString::from(s)),
        Err(e) => {
            let mut bytes = e.into_bytes();
            let s = String::from_utf8_lossy(&bytes).into_owned();
            bytes.fill(0);
            Some(SecretString::from(s))
        }
    }
}

/// The command wrapped in the platform shell: `/bin/sh -c` on Unix, with a `PATH` that finds
/// Homebrew; `cmd /C` on Windows, with the inherited `PATH`.
fn shell_command(command: &str) -> Command {
    #[cfg(unix)]
    {
        let home = std::env::var("HOME").ok();
        let path = command_path(std::env::var("PATH").ok().as_deref(), home.as_deref());
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(command).env("PATH", path);
        cmd
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        /// `CREATE_NO_WINDOW`: no console window flashes up for a GUI app.
        const NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = Command::new("cmd");
        // `raw_arg`: cmd parses its own command line, so Rust's quoting must not touch it.
        cmd.arg("/C").raw_arg(command).creation_flags(NO_WINDOW);
        cmd
    }
}

/// Runs `command` with `/bin/sh -c` (`cmd /C` on Windows) and returns what it printed as the
/// password. Blocks for up to `timeout`, so call it off the UI thread. Never logs stdout.
pub fn run(command: &str, timeout: Duration) -> Result<SecretString, Failure> {
    let mut child = shell_command(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Failure::Start(e.to_string()))?;
    // Read on threads so a chatty command cannot fill a pipe and stall; they are not joined
    // after a timeout, since a grandchild may keep the pipe open.
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = |pipe: Option<Box<dyn Read + Send>>, is_out: bool| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send((is_out, buf));
        });
    };
    reader(child.stdout.take().map(|p| Box::new(p) as _), true);
    reader(child.stderr.take().map(|p| Box::new(p) as _), false);
    drop(tx);
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(Failure::Start(e.to_string())),
        }
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    for (is_out, buf) in rx.iter().take(2) {
        if is_out {
            out = buf;
        } else {
            err = buf;
        }
    }
    let stderr = String::from_utf8_lossy(&err).into_owned();
    if !status.success() {
        out.fill(0);
        return Err(match status.code() {
            Some(code) => Failure::Exit { code, stderr },
            None => Failure::Signal { stderr },
        });
    }
    secret_from(out).ok_or(Failure::Exit {
        code: 0,
        stderr: "no output".into(),
    })
}

/// Command strings this Mac has approved, kept in `approved-commands.json` in the config
/// directory. It is deliberately not in the synced file list: a synced-in command must not
/// arrive pre-approved.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Approved(Vec<String>);

impl Approved {
    fn file(dir: &Path) -> PathBuf {
        dir.join(APPROVED_FILE)
    }

    /// Missing or unreadable means nothing is approved.
    pub fn load(dir: &Path) -> Self {
        let list = std::fs::read(Self::file(dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self(list)
    }

    /// Exact text only: an edited command is a new command.
    pub fn contains(&self, command: &str) -> bool {
        self.0.iter().any(|c| c == command)
    }

    /// Approves `command` and writes the file.
    pub fn approve(dir: &Path, command: &str) -> std::io::Result<()> {
        let mut now = Self::load(dir);
        if now.contains(command) {
            return Ok(());
        }
        now.0.push(command.to_owned());
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(&now.0).map_err(std::io::Error::other)?;
        let tmp = dir.join(format!("{APPROVED_FILE}.tmp"));
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, Self::file(dir))
    }
}

/// Whether `command` was approved on this Mac.
pub fn is_approved(command: &str) -> bool {
    crate::settings::dir().is_some_and(|d| Approved::load(&d).contains(command))
}

/// A command saved from this Mac's form counts as seen, so it is approved with the save.
pub fn approve_saved(connection: &crate::connections::Connection) {
    if let Some(dir) = crate::settings::dir() {
        approve_saved_in(&dir, connection);
    }
}

fn approve_saved_in(dir: &Path, connection: &crate::connections::Connection) {
    for command in [
        &connection.password_command,
        &connection.sudo_password_command,
        &connection.proxy_command,
    ]
    .into_iter()
    .flatten()
    {
        let _ = Approved::approve(dir, command);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tern_ssh::ExposeSecret;

    const SHORT: Duration = Duration::from_secs(5);

    #[test]
    fn the_command_runs_first_then_gets_no_second_turn() {
        // Fresh connection, approved: run. After it ran, accepted or rejected: never again.
        assert_eq!(plan(Some("c"), false, true), Step::Run);
        assert_eq!(plan(Some("c"), true, true), Step::Skip);
        assert_eq!(plan(Some("c"), true, false), Step::Skip);
        assert_eq!(plan(None, false, true), Step::Skip);
    }

    #[test]
    fn an_unapproved_command_is_asked_about_not_run() {
        assert_eq!(plan(Some("c"), false, false), Step::Approve);
    }

    #[cfg(unix)]
    #[test]
    fn stdout_is_the_password_minus_one_trailing_newline() {
        let ok = |c: &str| run(c, SHORT).unwrap().expose_secret().to_owned();
        assert_eq!(ok("printf hunter2"), "hunter2");
        assert_eq!(ok("echo hunter2"), "hunter2");
        assert_eq!(ok("printf ' pa ss \\n\\n'"), " pa ss \n");
        assert_eq!(ok("printf 'x\\r\\n'"), "x");
        assert_eq!(ok("printf 'ab '"), "ab ", "only a newline is trimmed");
    }

    #[cfg(unix)]
    #[test]
    fn failures_carry_the_exit_code_and_first_stderr_line() {
        assert_eq!(
            run("echo boom first >&2; echo second >&2; exit 3", SHORT).unwrap_err(),
            Failure::Exit {
                code: 3,
                stderr: "boom first\nsecond\n".into()
            }
        );
        let f = run("false", SHORT).unwrap_err();
        assert_eq!(f.line(), "password command failed (exit 1): ");
        let f = run("echo 'no such secret' >&2; exit 2", SHORT).unwrap_err();
        assert_eq!(f.line(), "password command failed (exit 2): no such secret");
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_command_never_yields_its_stdout() {
        assert!(run("echo leaked; exit 1", SHORT).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn empty_output_is_a_failure() {
        let f = run("true", SHORT).unwrap_err();
        assert!(f.line().contains("no output"), "{}", f.line());
        assert!(run("printf '\\n'", SHORT).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_command_times_out_and_is_killed() {
        let started = Instant::now();
        let f = run("sleep 30", Duration::from_millis(300)).unwrap_err();
        assert_eq!(f, Failure::Timeout);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(f.line(), "password command failed (timeout 20s)");
    }

    #[test]
    fn control_characters_in_stderr_do_not_reach_the_terminal() {
        let f = Failure::Exit {
            code: 1,
            stderr: "\x1b[31mred\x07\nnext".into(),
        };
        assert_eq!(f.line(), "password command failed (exit 1): [31mred");
    }

    #[cfg(unix)]
    #[test]
    fn the_path_gets_homebrew_and_go_bin_ahead_of_the_inherited_one() {
        assert_eq!(
            command_path(Some("/usr/bin:/bin"), Some("/Users/m")),
            "/opt/homebrew/bin:/usr/local/bin:/Users/m/go/bin:/usr/bin:/bin"
        );
        assert_eq!(command_path(None, None), "/opt/homebrew/bin:/usr/local/bin");
    }

    #[cfg(unix)]
    #[test]
    fn the_command_sees_the_extended_path() {
        let out = run("printf %s \"$PATH\"", SHORT).unwrap();
        assert!(
            out.expose_secret()
                .starts_with("/opt/homebrew/bin:/usr/local/bin")
        );
    }

    #[test]
    fn saving_a_connection_here_approves_its_proxy_command_too() {
        let dir = tempfile::tempdir().unwrap();
        let connection = crate::connections::Connection {
            proxy_command: Some("ssh -W %h:%p bastion".into()),
            password_command: Some("gopass show -o web".into()),
            ..crate::connections::Connection::default()
        };
        approve_saved_in(dir.path(), &connection);
        let approved = Approved::load(dir.path());
        assert!(approved.contains("ssh -W %h:%p bastion"));
        assert!(approved.contains("gopass show -o web"));
    }

    #[test]
    fn approval_is_per_exact_command_and_survives_the_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!Approved::load(dir.path()).contains("gopass show -o a"));
        Approved::approve(dir.path(), "gopass show -o a").unwrap();
        Approved::approve(dir.path(), "gopass show -o a").unwrap();
        let loaded = Approved::load(dir.path());
        assert!(loaded.contains("gopass show -o a"));
        assert!(!loaded.contains("gopass show -o a "), "edited text is new");
        assert!(!loaded.contains("gopass show -o b"));
        assert_eq!(loaded.0.len(), 1, "approving twice stores it once");
        Approved::approve(dir.path(), "printf x").unwrap();
        let loaded = Approved::load(dir.path());
        assert!(loaded.contains("gopass show -o a") && loaded.contains("printf x"));
    }

    #[test]
    fn a_corrupt_approval_file_approves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(APPROVED_FILE), b"{not json").unwrap();
        assert!(!Approved::load(dir.path()).contains("x"));
    }
}
