//! "Fill password": types the saved password into whatever is asking for one on the remote
//! (`[sudo] password for …:`), when the person presses the shortcut. Never automatic: the
//! remote can print a fake prompt, so tern only offers (a hint), and only types on the key
//! press, and only when the cursor line really ends in a password prompt.
//!
//! Port forwards of one session: the ones the connection lists start once it is connected, and
//! more can be added or stopped while it is open. What was added stays in the spec, so an
//! automatic reconnect brings it back; what was stopped is dropped from it.

use gpui::{App, AppContext, Context, EventEmitter};
use tern_ssh::{ExposeSecret, Forward, ForwardInfo, SecretString};
use tern_vault::Key;

use super::Session;
use super::Status;
use super::vault::{After, Then};
use crate::keeper::Keeper;
use crate::password_command::{self as pc, Purpose};

/// Whether the text left of the cursor is a password prompt: `[sudo] password for me:`,
/// `Password:`, `me@host's password:`, `Enter passphrase for key '/x':`. The line must END at
/// the prompt (the colon), so a `password:` in the middle of a typed command or in a line of
/// output is not one, and the words before the keyword may only be plain prompt words, so a
/// shell prompt with an `echo password:` on it is not either.
pub fn is_password_prompt(line: &str) -> bool {
    let line = line.trim_end();
    let Some(body) = line.strip_suffix(':') else {
        return false;
    };
    if body.is_empty() || body.len() > 120 {
        return false;
    }
    let lower = body.to_lowercase();
    // The last keyword is the one the prompt asks for.
    let Some((at, word)) = ["password", "passphrase"]
        .iter()
        .filter_map(|w| lower.rfind(w).map(|at| (at, *w)))
        .max_by_key(|(at, _)| *at)
    else {
        return false;
    };
    let (before, after) = (&lower[..at], &lower[at + word.len()..]);
    // Only "for <who or what>" may follow the keyword.
    let after_ok = after.is_empty() || (after.starts_with(" for ") && !after.contains(':'));
    // Plain prompt words before it: `[sudo] `, `user@host's `, `Enter `, `(current) `.
    let before_ok = before
        .chars()
        .all(|c| c.is_alphanumeric() || " []()@._-'’".contains(c));
    after_ok && before_ok
}

/// Whether anything may be typed for the cursor line `line` (`None`: no cursor to read): only
/// a password prompt may, and otherwise this is the toast that says why not.
pub fn guard(line: Option<&str>) -> Result<(), &'static str> {
    match line {
        Some(l) if is_password_prompt(l) => Ok(()),
        _ => Err("No password prompt on this line"),
    }
}

/// Where the password to fill comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The connection's "Sudo password command".
    Sudo(String),
    /// Its "Password command": the same user's login password.
    Password(String),
    /// The vault entry for `user@host:port`.
    Vault,
    None,
}

/// The first source there is: the sudo command, the password command, the vault.
pub fn source(sudo: Option<&str>, password: Option<&str>, vault: bool) -> Source {
    match (sudo, password) {
        (Some(c), _) => Source::Sudo(c.to_owned()),
        (None, Some(c)) => Source::Password(c.to_owned()),
        (None, None) if vault => Source::Vault,
        (None, None) => Source::None,
    }
}

impl Session {
    /// Whether a password could be filled here, as far as can be told without unlocking.
    fn fill_source(&self, cx: &App) -> Source {
        let key = self.password_key();
        // An open vault is looked in; a locked one may hold it, so say yes and ask later.
        let vault = Keeper::lookup(&key, cx).is_some() || Keeper::locked(cx);
        source(
            self.link.sudo_command.as_deref(),
            self.link.password_command.as_deref(),
            vault,
        )
    }

    fn password_key(&self) -> Key {
        Key::Password {
            user: self.link.spec.user.clone(),
            host: self.host().to_owned(),
            port: self.port(),
        }
    }

    /// The text left of the cursor, when it is a password prompt.
    fn at_password_prompt(&self, cx: &App) -> bool {
        self.prompt_guard(cx).is_ok()
    }

    fn prompt_guard(&self, cx: &App) -> Result<(), &'static str> {
        guard(self.terminal.read(cx).cursor_line_text().as_deref())
    }

    /// Whether this session is at a password prompt that Fill could answer: connected with no
    /// login question open, a prompt on the cursor line, and a password to type. The pane's
    /// "password asked" badge and Fill's choice of pane both follow it; it is read, not stored,
    /// so it stops being true the moment the prompt is answered.
    pub fn waiting_for_fill(&self, cx: &App) -> bool {
        self.accepts_input() && self.at_password_prompt(cx) && self.fill_source(cx) != Source::None
    }

    /// After the remote wrote: if it now asks for a password and one could be filled, tells
    /// the shell once so it can show the shortcut. Types nothing.
    pub(super) fn offer_fill_hint(&mut self, cx: &mut Context<Self>) {
        if !self.accepts_input() || !self.at_password_prompt(cx) {
            self.fill_hinted = false;
            return;
        }
        if !self.fill_hinted && self.fill_source(cx) != Source::None {
            self.fill_hinted = true;
            cx.emit(SessionNote::PasswordPrompt);
        }
    }

    /// The shortcut: fills the password if, and only if, the cursor line is a password prompt.
    pub fn fill_password(&mut self, cx: &mut Context<Self>) {
        if self.life.status() != Status::Connected || self.attempt.login.is_some() {
            return cx.emit(SessionNote::Message("The session is not connected".into()));
        }
        if let Err(why) = self.prompt_guard(cx) {
            return cx.emit(SessionNote::Message(why.into()));
        }
        if self.filling {
            return;
        }
        self.fill_now(self.output_seq, cx);
    }

    /// Gets the password and types it. `seq` is how many times the remote had written when the
    /// prompt was seen: the screen cannot be checked again once tern's own question (unlock,
    /// approval) has been drawn over the prompt line, so if the remote wrote since, the prompt
    /// may be gone or replaced and nothing is typed.
    pub(super) fn fill_now(&mut self, seq: u64, cx: &mut Context<Self>) {
        match self.fill_source(cx) {
            Source::None => cx.emit(SessionNote::Problem(
                "No password for this host: set a Sudo password command or Password command, or save it in the vault"
                    .into(),
            )),
            Source::Vault => match Keeper::lookup(&self.password_key(), cx) {
                Some(secret) => self.type_password(&secret, seq, cx),
                None if Keeper::locked(cx) => {
                    let after = After {
                        fill: Some(seq),
                        ..After::default()
                    };
                    self.ask_unlock(after, cx);
                }
                None => cx.emit(SessionNote::Problem(format!(
                    "No saved password for {} in the vault",
                    self.label()
                ))),
            },
            Source::Sudo(command) | Source::Password(command) => {
                match self.link.approvals.ask(Purpose::Fill, &command) {
                    None => self.run_fill_command(command, seq, cx),
                    Some(question) => self.ask_approval(question, Then::Fill(seq), cx),
                }
            }
        }
    }

    pub(super) fn run_fill_command(&mut self, command: String, seq: u64, cx: &mut Context<Self>) {
        self.filling = true;
        let work = cx.background_spawn(async move { pc::run(&command, pc::TIMEOUT) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |s, cx| {
                s.filling = false;
                match result {
                    Ok(secret) => s.type_password(&secret, seq, cx),
                    Err(failure) => cx.emit(SessionNote::Problem(failure.line())),
                }
            });
        })
        .detach();
    }

    /// Types the password and Enter into the remote, exactly as the keyboard would, unless the
    /// remote has written since the prompt was checked. Not mirrored to broadcast tabs.
    fn type_password(&mut self, secret: &SecretString, seq: u64, cx: &mut Context<Self>) {
        if self.life.status() != Status::Connected || self.attempt.login.is_some() {
            return;
        }
        if seq != self.output_seq {
            return cx.emit(SessionNote::Message(
                "The terminal changed meanwhile, so nothing was typed. Press the shortcut again."
                    .into(),
            ));
        }
        let mut bytes = secret.expose_secret().as_bytes().to_vec();
        bytes.push(b'\r');
        self.send(bytes);
        // The prompt is answered: the next one is a new prompt, with its own hint.
        self.fill_hinted = false;
    }
}

/// Something the shell should tell the person about, without the session knowing how.
pub enum SessionNote {
    ForwardFailed {
        forward: String,
        error: String,
    },
    /// The remote is asking for a password and one could be filled: show the shortcut.
    PasswordPrompt,
    /// A line for a toast.
    Message(String),
    /// A line for a toast that is an error.
    Problem(String),
}

impl EventEmitter<SessionNote> for Session {}

impl Session {
    /// Starts every forward the connection lists. Called once the shell is up.
    pub(super) fn start_spec_forwards(&mut self, cx: &mut Context<Self>) {
        for forward in self.link.spec.forwards.clone() {
            self.run_forward(forward, false, cx);
        }
    }

    /// Adds a forward to the live connection (and to its reconnects).
    pub fn add_forward(&mut self, forward: Forward, cx: &mut Context<Self>) {
        if !self.link.spec.forwards.contains(&forward) {
            self.link.spec.forwards.push(forward.clone());
        }
        self.run_forward(forward, true, cx);
    }

    fn run_forward(&mut self, forward: Forward, added: bool, cx: &mut Context<Self>) {
        let Some(handle) = self.link.handle.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let started = handle.start_forward(forward.clone()).await;
            let _ = this.update(cx, |s, cx| {
                match started {
                    Ok(h) => s.forward_handles.push(h),
                    Err(e) => {
                        tracing::warn!(forward = %forward, error = %e, "forward_failed");
                        if added {
                            s.link.spec.forwards.retain(|f| *f != forward);
                        }
                        cx.emit(SessionNote::ForwardFailed {
                            forward: forward.to_string(),
                            error: e.to_string(),
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Stops one forward and forgets it, so a reconnect does not revive it.
    pub fn stop_forward(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(at) = self.forward_handles.iter().position(|h| h.info().id == id) else {
            return;
        };
        let handle = self.forward_handles.remove(at);
        handle.stop();
        let gone = handle.info().forward.clone();
        self.link.spec.forwards.retain(|f| *f != gone);
        cx.notify();
    }

    /// The forwards running now.
    pub fn active_forwards(&self) -> Vec<ForwardInfo> {
        self.forward_handles
            .iter()
            .map(|h| h.info().clone())
            .collect()
    }

    /// A connection that ended took its forwards with it.
    pub(super) fn forget_forwards(&mut self) {
        self.forward_handles.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_prompts_are_recognised_whatever_their_case() {
        for line in [
            "[sudo] password for test:",
            "[sudo] password for test: ",
            "[SUDO] PASSWORD FOR Test:",
            "Password:",
            "password:",
            "Password: ",
            "me@10.0.0.5's password:",
            "Enter passphrase for key '/home/me/.ssh/id_ed25519':",
            "Enter passphrase:",
            "Passphrase:",
            "(current) password:",
        ] {
            assert!(is_password_prompt(line), "{line:?} is a prompt");
        }
    }

    #[test]
    fn a_password_word_that_is_not_the_end_of_the_line_is_not_a_prompt() {
        for line in [
            // Typed after the prompt, or in the middle of output.
            "echo password: hunter2",
            "password: is what you type",
            "Password: hunter2",
            "mid password: line of text",
            "see the docs for password: rules",
            // The keyword is followed by something that is not "for <who>".
            "password reset code:",
            // A shell prompt with a command that merely mentions it.
            "me@box:~$ echo password:",
            "me@box:~$ grep -r 'password:",
            // No colon at the cursor.
            "[sudo] password for test",
            "Password",
            "password",
            // Normal lines.
            "",
            "me@box:~$",
            "total 48",
            "drwxr-xr-x  2 me me 4096 Oct  6 12:00 passwords",
            "Last login: Tue Oct  6 12:00:00 2026:",
        ] {
            assert!(!is_password_prompt(line), "{line:?} is not a prompt");
        }
    }

    #[test]
    fn nothing_is_typed_unless_the_cursor_line_is_a_prompt() {
        assert_eq!(guard(Some("[sudo] password for test:")), Ok(()));
        for line in [Some("me@box:~$"), Some("password: hunter2"), Some(""), None] {
            assert_eq!(
                guard(line),
                Err("No password prompt on this line"),
                "{line:?}"
            );
        }
    }

    #[test]
    fn the_sudo_command_wins_then_the_password_command_then_the_vault() {
        assert_eq!(
            source(Some("sudo-cmd"), Some("pw-cmd"), true),
            Source::Sudo("sudo-cmd".into())
        );
        assert_eq!(
            source(None, Some("pw-cmd"), true),
            Source::Password("pw-cmd".into())
        );
        assert_eq!(source(None, None, true), Source::Vault);
        assert_eq!(source(None, None, false), Source::None);
        // A sudo command alone, with nothing else configured.
        assert_eq!(
            source(Some("sudo-cmd"), None, false),
            Source::Sudo("sudo-cmd".into())
        );
    }
}
