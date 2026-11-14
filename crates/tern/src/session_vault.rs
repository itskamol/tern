//! The vault side of logging in: answer prompts from the vault, unlock it when it is locked
//! (with the PIN first, when one is set), and offer to save what the user typed once the login
//! worked.
//!
//! The password-command side of logging in: when a server asks for a password, run the
//! connection's command before the vault, and ask first if this Mac never approved it.

use std::time::{Duration, Instant};

use gpui::{AppContext, Context};
use tern_ssh::{ExposeSecret, Prompt, SecretString};
use tern_vault::{Key, Vault};

use super::Session;
use crate::keeper::{Ask, Keeper, Opening, Pending, Tried, Typed};
use crate::login::Login;
use crate::login_ladder::{Facts, InVault, Next};
use crate::password_command::{self as pc, Purpose, Question};

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
    /// "Run `…` on this Mac? (yes/no)" for a password command, ProxyCommand or fill command.
    Approve(Question, Then),
    /// "Save … in tern's vault? (yes/no)".
    OfferSave(Key, SecretString),
    NewPassphrase(Key, SecretString),
    RepeatPassphrase(Key, SecretString, SecretString),
}

/// What an approved command was asked for.
pub(super) enum Then {
    /// The server's password prompt, answered by the command's output.
    Login(Prompt),
    /// Dialling, through the ProxyCommand.
    Proxy,
    /// A password fill; the number is the screen it was asked on.
    Fill(u64),
}

impl Session {
    pub(super) fn on_prompt(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        let command = self.link.password_command.as_deref();
        let facts = Facts {
            host: &self.link.spec.host,
            port: self.link.spec.port,
            command,
            approval: command.and_then(|c| self.link.approvals.ask(Purpose::Login, c)),
            vault_skipped: self.vault_skipped,
        };
        let next = self
            .attempt
            .ladder
            .next(&prompt, facts, |key| match Keeper::lookup(key, cx) {
                Some(secret) => InVault::Secret(secret),
                None if Keeper::locked(cx) => InVault::Locked,
                None => InVault::Nothing,
            });
        match next {
            Next::ApproveCommand(question) => self.ask_approval(question, Then::Login(prompt), cx),
            Next::RunCommand(command) => self.run_command(command, prompt, cx),
            Next::FromVault(key, secret) => {
                self.show(
                    format!("\x1b[2m[{}]\x1b[0m\r\n", crate::login::from_vault(&key)).as_bytes(),
                    cx,
                );
                if let Some(other) = crate::login::answer(prompt, secret) {
                    self.ask_user(other, cx);
                }
            }
            Next::OpenVault => {
                let after = After {
                    prompt: Some(prompt),
                    ..After::default()
                };
                self.ask_unlock(after, cx);
            }
            Next::Ask { refused } => self.put_question(prompt, refused, cx),
        }
    }

    /// Asks for the vault passphrase before the connection dials (see [`Flow::Unlock`]).
    pub(super) fn ask_vault_before_dialling(&mut self, cx: &mut Context<Self>) {
        self.connect_after_unlock = true;
        self.ask_unlock(After::default(), cx);
    }

    /// Opens the vault: the Keychain first, once per run, then the PIN when one is set, else the
    /// passphrase. A Keychain that cannot open it falls through without a word.
    pub(super) fn ask_unlock(&mut self, after: After, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let need = Keeper::need_vault(cx, after, move |after, cx| {
            let _ = this.update(cx, |s, cx| s.keychain_resolved(after, cx));
        });
        match need {
            Pending::Waiting => {
                self.attempt.vault_busy = true;
                self.note("Asking the Keychain…", cx);
            }
            Pending::Now(Opening::Open, after) => self.keychain_resolved(after, cx),
            Pending::Now(Opening::Ask(ask), after) => self.ask_for(ask, after, cx),
        }
    }

    fn ask_for(&mut self, ask: Ask, after: After, cx: &mut Context<Self>) {
        match ask {
            Ask::Pin => {
                let text = format!("{}Vault PIN (Enter to use the passphrase): ", lead(&after));
                self.ask_local(&text, false, Flow::Pin(after), cx);
            }
            Ask::Passphrase => self.ask_passphrase(after, cx),
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
        self.attempt.vault_busy = false;
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
        if let Some((key, secret)) = entry {
            self.store(key, secret, cx);
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

    /// The server's question, asked in the terminal without going down the ladder again.
    fn ask_user(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        let refused = self
            .attempt
            .ladder
            .ask(&prompt, &self.link.spec.host, self.link.spec.port);
        self.put_question(prompt, refused, cx);
    }

    fn put_question(&mut self, prompt: Prompt, refused: bool, cx: &mut Context<Self>) {
        // Asked again for the same secret means the last answer was refused; say so as
        // OpenSSH does, or a retry looks like the same prompt shown twice.
        if refused {
            self.show(b"Permission denied, please try again.\r\n", cx);
        }
        let (login, question) = Login::start(prompt);
        self.attempt.login = Some(login);
        self.show(&question, cx);
        // The tab, sidebar and status dots read `dot`; a session nobody is drawing would
        // otherwise keep the connecting dot (with Reduce Motion nothing else redraws it).
        cx.notify();
    }

    pub(super) fn ask_local(&mut self, text: &str, echo: bool, flow: Flow, cx: &mut Context<Self>) {
        let (login, question) = Login::ask(text, echo);
        self.attempt.login = Some(login);
        self.attempt.flow = Some(flow);
        self.show(&question, cx);
        cx.notify();
    }

    /// Asks to save a typed secret once the remote has been quiet for [`QUIET`] (at most
    /// [`QUIET_MAX`] after login), so the question does not land inside the login banner.
    pub(super) fn offer_save(&mut self, cx: &mut Context<Self>) {
        if !self.attempt.ladder.has_typed() {
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
                if s.attempt.login.is_some() || s.life.status() != super::Status::Connected {
                    return;
                }
                if let Some((key, secret)) = s.attempt.ladder.take_typed() {
                    let lead = if s.ends_line { "" } else { "\r\n" };
                    let text = format!(
                        "{lead}Save {} in tern's vault? (yes/no) ",
                        crate::login::describe(&key)
                    );
                    s.ask_local(&text, true, Flow::OfferSave(key, secret), cx);
                }
            });
        })
        .detach();
    }

    /// A question finished; `answer` is what was typed, `None` after Ctrl-C.
    pub(super) fn answered(&mut self, answer: Option<SecretString>, cx: &mut Context<Self>) {
        let Some(flow) = self.attempt.flow.take() else {
            self.attempt.ladder.answered(answer);
            return;
        };
        let typed = answer.filter(|a| !a.expose_secret().is_empty());
        match flow {
            Flow::Pin(after) => match typed {
                Some(pin) => self.unlock(Typed::Pin(pin.expose_secret().to_owned()), after, cx),
                None => self.ask_passphrase(after, cx),
            },
            Flow::Unlock(after) => match typed {
                Some(passphrase) => self.unlock(Typed::Passphrase(passphrase), after, cx),
                None => self.skipped(after, cx),
            },
            Flow::Approve(question, then) => {
                self.approval_answered(question, then, crate::login::is_yes(typed.as_ref()), cx)
            }
            Flow::OfferSave(key, secret) => {
                if !crate::login::is_yes(typed.as_ref()) {
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
                } else {
                    self.store(key, secret, cx);
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
                    Keeper::adopt(Vault::new(first), cx);
                    self.store(key, secret, cx);
                } else {
                    self.note("Passphrases differ; not saved.", cx);
                }
            }
        }
    }

    /// Opens the vault off the UI thread, then does what was waiting for it.
    fn unlock(&mut self, typed: Typed, after: After, cx: &mut Context<Self>) {
        self.attempt.vault_busy = true;
        self.note("Unlocking the vault…", cx);
        let work = Keeper::try_unlock(typed, cx);
        cx.spawn(async move |this, cx| {
            let tried = work.await;
            let _ = this.update(cx, |s, cx| s.unlocked(tried, after, cx));
        })
        .detach();
    }

    /// The vault opened, or it did not and the person is asked again or let through.
    fn unlocked(&mut self, tried: Tried, after: After, cx: &mut Context<Self>) {
        self.attempt.vault_busy = false;
        let opened = match tried {
            Tried::Opened => true,
            Tried::Again { ask, message } => {
                self.note(&message, cx);
                return self.ask_for(ask, after, cx);
            }
            Tried::Failed(message) => {
                self.note(&message, cx);
                false
            }
        };
        let After {
            prompt,
            entry,
            fill,
        } = after;
        if let Some((key, secret)) = entry.filter(|_| opened) {
            self.store(key, secret, cx);
        }
        self.resume(prompt, fill, opened, cx);
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

    /// Stores a secret in the open vault and writes it off the UI thread, in order with every
    /// other write; the vault stays open for the other tabs meanwhile.
    fn store(&mut self, key: Key, secret: SecretString, cx: &mut Context<Self>) {
        let Some(task) = Keeper::store(key, secret, cx) else {
            return self.note("Not saved: the vault is locked.", cx);
        };
        self.attempt.vault_busy = true;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                match result {
                    Ok(()) => s.note("Saved in the vault.", cx),
                    Err(e) => {
                        tracing::warn!(error = %e, "vault_save_failed");
                        s.note(&format!("Could not save the vault: {e}"), cx);
                    }
                }
                s.attempt.vault_busy = false;
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
    /// Asks before a command first runs on this Mac: it came from `hosts.json`, which sync and
    /// imports also write.
    pub(super) fn ask_approval(&mut self, question: Question, then: Then, cx: &mut Context<Self>) {
        let text = question.text();
        self.ask_local(&text, true, Flow::Approve(question, then), cx);
    }

    /// The answer to an approval question. A no goes on as each use needs: the login prompt
    /// goes to the vault, a proxy leaves the tab closed, a fill is not run.
    fn approval_answered(
        &mut self,
        question: Question,
        then: Then,
        yes: bool,
        cx: &mut Context<Self>,
    ) {
        match (then, self.link.approvals.answer(question, yes)) {
            (Then::Login(prompt), Some(command)) => self.run_command(command, prompt, cx),
            (Then::Login(prompt), None) => {
                self.on_prompt(prompt, cx);
            }
            (Then::Proxy, Some(_)) => self.dial(cx),
            (Then::Proxy, None) => {
                self.note(
                    "Not connected: the proxy command was not run. Enter asks again.",
                    cx,
                );
                self.life.stop();
                cx.notify();
            }
            (Then::Fill(seq), Some(command)) => self.run_fill_command(command, seq, cx),
            (Then::Fill(_), None) => self.note("Not run.", cx),
        }
    }

    fn run_command(&mut self, command: String, prompt: Prompt, cx: &mut Context<Self>) {
        let work = cx.background_spawn(async move { pc::run(&command, pc::TIMEOUT) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |s, cx| match result {
                Ok(secret) => {
                    s.show(
                        format!("\x1b[2m[{}]\x1b[0m\r\n", crate::login::FROM_COMMAND).as_bytes(),
                        cx,
                    );
                    // Answered straight to the server, never through `typed`, so it is not
                    // offered for saving in the vault.
                    if let Some(other) = crate::login::answer(prompt, secret) {
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
