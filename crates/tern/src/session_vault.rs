//! The vault side of logging in: answer prompts from the vault, unlock it when it is locked
//! (with the PIN first, when one is set), and offer to save what the user typed once the login
//! worked.
//!
//! The password-command side of logging in: when a server asks for a password, run the
//! connection's command before the vault, and ask first if this Mac never approved it.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext, Context};
use tern_ssh::{ExposeSecret, Prompt, SecretString};
use tern_vault::{Key, Vault, VaultError};

use super::Session;
use crate::keeper::{self, Keeper};
use crate::login::Login;
use crate::password_command::{self as pc, Approved, Step};
use crate::vault_pin::{self, Outcome};

const QUIET: Duration = Duration::from_millis(500);
const QUIET_MAX: Duration = Duration::from_secs(3);

/// What waits for the vault to open. Several can be pending at once only in the sense that
/// each field is a different reason; the vault opening is what they all need.
#[derive(Default)]
pub(super) struct After {
    /// The server's question, answered from the vault once it is open.
    pub(super) prompt: Option<Prompt>,
    /// A secret to save once it is open.
    pub(super) entry: Option<(Key, SecretString)>,
    /// A password fill, as the screen was when it was asked for (see `session_fill`).
    pub(super) fill: Option<u64>,
}

/// A question tern is asking for itself, and what to do with the answer.
pub(super) enum Flow {
    /// "Vault PIN (Enter to use the passphrase)", asked first when a PIN is set.
    Pin(After),
    /// "Vault passphrase (Enter to skip)". With nothing in [`After`] it was asked before
    /// dialling because the connection names a vault key; skipping dials anyway and falls
    /// back to the usual prompts.
    Unlock(After),
    /// "This connection runs `…` to get its password. Run it? (yes/no)".
    ApproveCommand(Prompt),
    /// The same for a password fill; the number is the screen it was asked on.
    ApproveFill(String, u64),
    /// "Save … in tern's vault? (yes/no)".
    OfferSave(Key, SecretString),
    NewPassphrase(Key, SecretString),
    RepeatPassphrase(Key, SecretString, SecretString),
}

/// How the person is opening the vault.
enum Opener {
    Passphrase(SecretString),
    Pin(String),
}

impl Session {
    pub(super) fn on_prompt(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        // The password command goes first, then the vault, then the user.
        let Some(prompt) = self.try_command(prompt, cx) else {
            return;
        };
        let key = keeper::key_for(&prompt, self.host(), self.port());
        // Each secret is tried from the vault once per connection, so a stale one falls
        // through to asking instead of failing in a loop.
        if let Some(key) = key.filter(|k| !self.tried.contains(k)) {
            if let Some(secret) = Keeper::lookup(&key, cx) {
                self.tried.push(key.clone());
                self.show(
                    format!(
                        "\x1b[2m[{} from the vault]\x1b[0m\r\n",
                        keeper::describe(&key)
                    )
                    .as_bytes(),
                    cx,
                );
                if let Some(other) = keeper::answer(prompt, secret) {
                    self.ask_user(other, cx);
                }
                return;
            }
            if Keeper::locked(cx) && !self.vault_skipped {
                let after = After {
                    prompt: Some(prompt),
                    ..After::default()
                };
                return self.ask_unlock(after, cx);
            }
        }
        self.ask_user(prompt, cx);
    }

    /// Asks for the vault passphrase before the connection dials (see [`Flow::Unlock`]).
    pub(super) fn ask_vault_before_dialling(&mut self, cx: &mut Context<Self>) {
        self.connect_after_unlock = true;
        self.ask_unlock(After::default(), cx);
    }

    /// Opens the vault: the Keychain first, once per run, then the PIN when one is set, else the
    /// passphrase. A Keychain that cannot open it falls through without a word.
    pub(super) fn ask_unlock(&mut self, after: After, cx: &mut Context<Self>) {
        let slot = Rc::new(RefCell::new(Some(after)));
        let wake = {
            let (slot, this) = (slot.clone(), cx.weak_entity());
            move |cx: &mut App| {
                let Some(after) = slot.borrow_mut().take() else {
                    return;
                };
                let _ = this.update(cx, |s, cx| s.keychain_resolved(after, cx));
            }
        };
        if Keeper::keychain_then(cx, wake) {
            self.vault_busy = true;
            self.note("Asking the Keychain…", cx);
            return;
        }
        let after = slot.borrow_mut().take().unwrap_or_default();
        if Keeper::pin_set(cx) {
            let text = format!("{}Vault PIN (Enter to use the passphrase): ", lead(&after));
            self.ask_local(&text, false, Flow::Pin(after), cx);
        } else {
            self.ask_passphrase(after, cx);
        }
    }

    /// The Keychain attempt is over. The vault is open if it worked, or if the person opened it
    /// another way meanwhile; otherwise the usual question.
    fn keychain_resolved(
        &mut self,
        After {
            prompt,
            entry,
            fill,
        }: After,
        cx: &mut Context<Self>,
    ) {
        self.vault_busy = false;
        if Keeper::locked(cx) {
            return self.ask_unlock(
                After {
                    prompt,
                    entry,
                    fill,
                },
                cx,
            );
        }
        if let Some((key, secret)) = entry
            && let Some(mut vault) = Keeper::take(cx)
        {
            vault.set(key, secret);
            self.save(vault, cx);
        }
        self.resume(prompt, fill, true, cx);
        self.release_held(cx);
    }

    fn ask_passphrase(&mut self, after: After, cx: &mut Context<Self>) {
        let ask = if after.entry.is_some() {
            "Vault passphrase: "
        } else {
            "Vault passphrase (Enter to skip): "
        };
        let text = format!("{}{ask}", lead(&after));
        self.ask_local(&text, false, Flow::Unlock(after), cx);
    }

    /// The vault was not opened and the person gave up: back to what each reason falls to.
    fn skipped(&mut self, after: After, cx: &mut Context<Self>) {
        if let Some(prompt) = after.prompt {
            self.vault_skipped = true;
            self.ask_user(prompt, cx);
        } else if after.entry.is_some() {
            self.note("Not saved.", cx);
        } else if after.fill.is_some() {
            self.note("Not filled.", cx);
        } else {
            self.vault_skipped = true;
            self.connect_after_unlock = false;
            self.dial(cx);
        }
    }

    /// The server's question, asked in the terminal; a typed password is remembered so it can
    /// be offered for saving once the login succeeds.
    fn ask_user(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        self.asking = keeper::key_for(&prompt, self.host(), self.port());
        // Asked again for the same secret means the last answer was refused; say so as
        // OpenSSH does, or a retry looks like the same prompt shown twice.
        if let Some(key) = &self.asking {
            if self.asked.contains(key) {
                self.show(b"Permission denied, please try again.\r\n", cx);
            } else {
                self.asked.push(key.clone());
            }
        }
        let (login, question) = Login::start(prompt);
        self.login = Some(login);
        self.show(&question, cx);
        // The tab, sidebar and status dots read `is_asking`; a session nobody is drawing would
        // otherwise keep the connecting dot (with Reduce Motion nothing else redraws it).
        cx.notify();
    }

    pub(super) fn ask_local(&mut self, text: &str, echo: bool, flow: Flow, cx: &mut Context<Self>) {
        let (login, question) = Login::ask(text, echo);
        self.login = Some(login);
        self.flow = Some(flow);
        self.show(&question, cx);
        cx.notify();
    }

    /// Asks to save a typed secret once the remote has been quiet for [`QUIET`] (at most
    /// [`QUIET_MAX`] after login), so the question does not land inside the login banner.
    pub(super) fn offer_save(&mut self, cx: &mut Context<Self>) {
        if self.typed.is_none() {
            return;
        }
        let started = Instant::now();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(QUIET / 2).await;
                let quiet = this
                    .update(cx, |s, _| s.last_output.elapsed() >= QUIET)
                    .unwrap_or(true);
                if quiet || started.elapsed() >= QUIET_MAX {
                    break;
                }
            }
            let _ = this.update(cx, |s, cx| {
                if s.login.is_some() || s.status != super::Status::Connected {
                    return;
                }
                if let Some((key, secret)) = s.typed.take() {
                    let lead = if s.ends_line { "" } else { "\r\n" };
                    let text = format!(
                        "{lead}Save {} in tern's vault? (yes/no) ",
                        keeper::describe(&key)
                    );
                    s.ask_local(&text, true, Flow::OfferSave(key, secret), cx);
                }
            });
        })
        .detach();
    }

    /// A question finished; `answer` is what was typed, `None` after Ctrl-C.
    pub(super) fn answered(&mut self, answer: Option<SecretString>, cx: &mut Context<Self>) {
        let Some(flow) = self.flow.take() else {
            if let (Some(key), Some(secret)) = (self.asking.take(), answer) {
                self.typed = Some((key, secret));
            }
            return;
        };
        let typed = answer.filter(|a| !a.expose_secret().is_empty());
        match flow {
            Flow::Pin(after) => match typed {
                Some(pin) => self.unlock(Opener::Pin(pin.expose_secret().to_owned()), after, cx),
                None => self.ask_passphrase(after, cx),
            },
            Flow::Unlock(after) => match typed {
                Some(passphrase) => self.unlock(Opener::Passphrase(passphrase), after, cx),
                None => self.skipped(after, cx),
            },
            Flow::ApproveCommand(prompt) => {
                self.command_approved(prompt, keeper::is_yes(typed.as_ref()), cx)
            }
            Flow::ApproveFill(command, seq) => {
                self.fill_approved(command, seq, keeper::is_yes(typed.as_ref()), cx)
            }
            Flow::OfferSave(key, secret) => {
                if !keeper::is_yes(typed.as_ref()) {
                    return;
                }
                if !Keeper::exists(cx) {
                    self.ask_local(
                        "New vault passphrase: ",
                        false,
                        Flow::NewPassphrase(key, secret),
                        cx,
                    );
                } else if Keeper::locked(cx) {
                    let after = After {
                        entry: Some((key, secret)),
                        ..After::default()
                    };
                    self.ask_unlock(after, cx);
                } else if let Some(mut vault) = Keeper::take(cx) {
                    vault.set(key, secret);
                    self.save(vault, cx);
                }
            }
            Flow::NewPassphrase(key, secret) => match typed {
                Some(first) => self.ask_local(
                    "Repeat vault passphrase: ",
                    false,
                    Flow::RepeatPassphrase(key, secret, first),
                    cx,
                ),
                None => self.note("Not saved.", cx),
            },
            Flow::RepeatPassphrase(key, secret, first) => {
                if typed.is_some_and(|again| again.expose_secret() == first.expose_secret()) {
                    let mut vault = Vault::new(first);
                    vault.set(key, secret);
                    self.save(vault, cx);
                } else {
                    self.note("Passphrases differ; not saved.", cx);
                }
            }
        }
    }

    /// Opens the vault off the UI thread, then does what was waiting for it.
    fn unlock(&mut self, opener: Opener, after: After, cx: &mut Context<Self>) {
        let Some(path) = Keeper::path(cx) else {
            return;
        };
        self.vault_busy = true;
        self.note("Unlocking the vault…", cx);
        let work = match opener {
            Opener::Passphrase(passphrase) => cx.background_spawn(async move {
                match Vault::unlock(&path, passphrase) {
                    Ok(vault) => Outcome::Opened(vault),
                    Err(VaultError::WrongPassphrase) => {
                        Outcome::Failed("Wrong vault passphrase.".into())
                    }
                    Err(e) => Outcome::Failed(format!("Vault unavailable: {e}")),
                }
            }),
            Opener::Pin(pin) => Keeper::pin_unlock(pin, cx),
        };
        cx.spawn(async move |this, cx| {
            let outcome = work.await;
            let _ = this.update(cx, |s, cx| s.unlocked(outcome, after, cx));
        })
        .detach();
    }

    /// The vault opened, or it did not and the person is asked again.
    fn unlocked(&mut self, outcome: Outcome, after: After, cx: &mut Context<Self>) {
        self.vault_busy = false;
        let vault = match outcome {
            Outcome::Opened(vault) => Some(vault),
            Outcome::Wrong { .. } => {
                // A wrong PIN is asked again, with the tries left said first.
                if let Some(text) = vault_pin::message(&outcome) {
                    self.note(&text, cx);
                }
                return self.ask_unlock(after, cx);
            }
            Outcome::Wiped | Outcome::Gone => {
                // The PIN is off now: say so, and ask for the passphrase.
                if let Some(text) = Keeper::pin_done(outcome, cx) {
                    self.note(&text, cx);
                }
                return self.ask_passphrase(after, cx);
            }
            Outcome::Failed(text) => {
                self.note(&text, cx);
                None
            }
        };
        let After {
            prompt,
            entry,
            fill,
        } = after;
        let unlocked = vault.is_some();
        if let Some(mut vault) = vault {
            match entry {
                Some((key, secret)) => {
                    vault.set(key, secret);
                    self.save(vault, cx);
                }
                None => Keeper::put(vault, cx),
            }
        }
        self.resume(prompt, fill, unlocked, cx);
        self.release_held(cx);
    }

    /// What waited for the vault goes on. The connection dials, with the keys if the vault
    /// opened and without them if it did not, and exactly once.
    fn resume(
        &mut self,
        prompt: Option<Prompt>,
        fill: Option<u64>,
        unlocked: bool,
        cx: &mut Context<Self>,
    ) {
        if std::mem::take(&mut self.connect_after_unlock) {
            self.dial(cx);
        }
        // A failed unlock goes straight to the server's question rather than asking for the
        // vault passphrase again.
        match prompt {
            Some(prompt) if unlocked => self.on_prompt(prompt, cx),
            Some(prompt) => self.ask_user(prompt, cx),
            None => {}
        }
        if let (Some(seq), true) = (fill, unlocked) {
            self.fill_now(seq, cx);
        }
    }

    /// Encrypts and writes off the UI thread; the vault returns to the keeper afterwards.
    fn save(&mut self, vault: Vault, cx: &mut Context<Self>) {
        let Some(path) = Keeper::path(cx) else {
            return;
        };
        self.vault_busy = true;
        let work = cx.background_spawn(async move {
            let result = vault.save(&path);
            (vault, result)
        });
        cx.spawn(async move |this, cx| {
            let (vault, result) = work.await;
            let _ = this.update(cx, |s, cx| {
                Keeper::put(vault, cx);
                match result {
                    Ok(()) => s.note("Saved in the vault.", cx),
                    Err(e) => {
                        tracing::warn!(error = %e, "vault_save_failed");
                        s.note(&format!("Could not save the vault: {e}"), cx);
                    }
                }
                s.vault_busy = false;
                s.release_held(cx);
            });
        })
        .detach();
    }

    pub(super) fn note(&self, text: &str, cx: &mut Context<Self>) {
        self.show(format!("\x1b[2m{text}\x1b[0m\r\n").as_bytes(), cx);
    }
}

/// A fill asks in the middle of the remote's own prompt line: start on a fresh line.
fn lead(after: &After) -> &'static str {
    if after.fill.is_some() { "\r\n" } else { "" }
}

impl Session {
    /// Runs the connection's password command for `prompt` when it has a turn. Returns the
    /// prompt back when the command does not apply (no command, not a password prompt, already
    /// tried); `None` when it is being run or asked about now.
    pub(super) fn try_command(&mut self, prompt: Prompt, cx: &mut Context<Self>) -> Option<Prompt> {
        if !matches!(prompt, Prompt::Password { .. }) {
            return Some(prompt);
        }
        let command = self.link.password_command.clone();
        let approved = command.as_deref().is_some_and(|c| {
            crate::settings::dir().is_some_and(|d| Approved::load(&d).contains(c))
        });
        match pc::plan(command.as_deref(), self.command_tried, approved) {
            Step::Skip => Some(prompt),
            Step::Approve => {
                let text = format!(
                    "This connection runs `{}` to get its password. Run it? (yes/no) ",
                    command.unwrap_or_default().replace(char::is_control, " ")
                );
                self.ask_local(&text, true, Flow::ApproveCommand(prompt), cx);
                None
            }
            Step::Run => {
                self.run_command(command.unwrap_or_default(), prompt, cx);
                None
            }
        }
    }

    /// The answer to the approval question.
    pub(super) fn command_approved(&mut self, prompt: Prompt, yes: bool, cx: &mut Context<Self>) {
        let Some(command) = self.link.password_command.clone().filter(|_| yes) else {
            // Declined: the command is not run, and the prompt goes the usual way.
            self.command_tried = true;
            return self.on_prompt(prompt, cx);
        };
        if let Some(dir) = crate::settings::dir() {
            let _ = Approved::approve(&dir, &command);
        }
        self.run_command(command, prompt, cx);
    }

    fn run_command(&mut self, command: String, prompt: Prompt, cx: &mut Context<Self>) {
        self.command_tried = true;
        let work = cx.background_spawn(async move { pc::run(&command, pc::TIMEOUT) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |s, cx| match result {
                Ok(secret) => {
                    s.show(b"\x1b[2m[password from the command]\x1b[0m\r\n", cx);
                    // Answered straight to the server, never through `typed`, so it is not
                    // offered for saving in the vault.
                    if let Some(other) = keeper::answer(prompt, secret) {
                        s.on_prompt(other, cx);
                    }
                }
                Err(failure) => {
                    s.show(
                        format!("\x1b[2m{}\x1b[0m\r\n", failure.line()).as_bytes(),
                        cx,
                    );
                    s.on_prompt(prompt, cx);
                }
            });
        })
        .detach();
    }
}
