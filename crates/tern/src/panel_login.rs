//! A host panel's own connection, with the login questions a session asks in its terminal, asked
//! in the panel instead: a saved ProxyCommand and password command not yet approved on this Mac,
//! the host key, the password or key passphrase, the vault passphrase when the vault is locked
//! (or a new one, when the offer to save a typed password finds no vault). The answers are the same as a
//! session's (see `session_vault.rs`); only where they are shown differs. A changed host key is
//! a hard stop here too: the connection ends with the reason and nothing offers to go on.

use std::collections::VecDeque;

use gpui::{AppContext, Context, Entity, EventEmitter, Task};
use tern_ssh::oneshot::Sender;
use tern_ssh::{
    ChallengePrompt, ConnectSpec, Disconnect, ExposeSecret, MemoryKey, Prompt, SecretString,
    SessionEvent, SessionHandle,
};
use tern_vault::{Key, Vault};

use crate::keeper::{Ask as VaultAsk, Keeper, Tried, Typed};
use crate::password_command::{self as pc, Approvals, Auth, Purpose};
use crate::platform::SshRuntime;
use crate::text_input::{InputColors, TextInput};

/// Where the login stands.
pub enum Step {
    Connecting,
    /// Waiting for the person.
    Asking(Question),
    Connected,
    /// Ended without a connection; retry starts over.
    Failed(String),
}

/// A question shown in the panel: some lines, and either a field or Yes and No.
pub struct Question {
    pub lines: Vec<String>,
    pub input: Option<Entity<TextInput>>,
    /// Buttons Yes and No instead of a field.
    pub yes_no: bool,
    /// Enter on an empty field skips (the vault passphrase).
    pub skippable: bool,
    kind: Kind,
}

enum Kind {
    HostKey(Option<Sender<bool>>),
    Secret(Option<Sender<Option<SecretString>>>, Option<Key>),
    Challenge {
        pending: VecDeque<ChallengePrompt>,
        answers: Vec<SecretString>,
        reply: Option<Sender<Option<Vec<SecretString>>>>,
    },
    /// The vault passphrase or PIN, for what waits on it.
    Vault(VaultAsk, Then),
    /// Run this command here? The answer goes on with what waited for it.
    Approve(Option<pc::Question>, Option<Then>),
    /// A first vault's passphrase, then again, to keep a typed password.
    NewVault(Key, SecretString, Option<SecretString>),
}

/// What waits for the vault to open.
enum Then {
    /// The connection, which names vault keys.
    Dial,
    /// A server question the vault may answer.
    Prompt(Prompt),
    /// A typed password to save.
    Save(Key, SecretString),
    /// The saved ProxyCommand, before dialling.
    Proxy,
    /// A password question the password command may answer.
    Login(Prompt),
}

/// What a panel hears from its login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginEvent {
    Connected,
    /// The connection ended after it was up.
    Dropped(String),
}

pub struct PanelLogin {
    spec: ConnectSpec,
    vault_keys: Vec<String>,
    proxy_command: Option<String>,
    password_command: Option<String>,
    /// The password command gets one turn per connection, whatever came of it.
    command_tried: bool,
    approvals: Approvals,
    /// The connection is up, so a question asked now returns to [`Step::Connected`].
    up: bool,
    colors: InputColors,
    step: Step,
    handle: Option<SessionHandle>,
    /// Entries already answered from the vault on this connection.
    tried: Vec<Key>,
    asked: Vec<Key>,
    /// What the person typed for the password question, offered for saving once connected.
    typed: Option<(Key, SecretString)>,
    /// The offer to save it, once connected.
    offer: Option<(Key, SecretString)>,
    skipped_vault: bool,
    note: Option<String>,
    _events: Task<()>,
}

impl EventEmitter<LoginEvent> for PanelLogin {}

impl PanelLogin {
    /// Starts connecting. Nothing is dialled before the saved ProxyCommand, if there is one, is
    /// approved on this Mac; the panel asks.
    pub fn start(
        spec: ConnectSpec,
        auth: Auth,
        colors: InputColors,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::start_with(spec, auth, Approvals::here(), colors, cx)
    }

    /// [`Self::start`] with the commands this Mac approved given, for tests.
    pub fn start_with(
        spec: ConnectSpec,
        auth: Auth,
        approvals: Approvals,
        colors: InputColors,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            spec,
            vault_keys: auth.vault_keys,
            proxy_command: auth.proxy_command,
            password_command: auth.password_command,
            command_tried: false,
            approvals,
            up: false,
            colors,
            step: Step::Connecting,
            handle: None,
            tried: Vec::new(),
            asked: Vec::new(),
            typed: None,
            offer: None,
            skipped_vault: false,
            note: None,
            _events: Task::ready(()),
        };
        this.begin(cx);
        this
    }

    pub fn step(&self) -> &Step {
        &self.step
    }

    /// The connection, once there is one.
    pub fn handle(&self) -> Option<SessionHandle> {
        self.handle.clone()
    }

    /// What the vault did, for the line under the panel ("Wrong vault passphrase.").
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// The offer to save a typed password, once connected and while it stands.
    pub fn save_offer(&self) -> Option<String> {
        self.offer
            .as_ref()
            .map(|(key, _)| crate::login::describe(key))
    }

    fn begin(&mut self, cx: &mut Context<Self>) {
        self.step = Step::Connecting;
        self.up = false;
        self.command_tried = false;
        self.tried.clear();
        self.asked.clear();
        self.typed = None;
        self.offer = None;
        self.note = None;
        if let Some(command) = self.proxy_command.clone()
            && let Some(question) = self.approvals.ask(Purpose::Proxy, &command)
        {
            return self.ask_approval(question, Then::Proxy, cx);
        }
        self.unlock_and_dial(cx);
    }

    fn unlock_and_dial(&mut self, cx: &mut Context<Self>) {
        if !self.vault_keys.is_empty() && Keeper::locked(cx) && !self.skipped_vault {
            // The connection names vault keys and the vault is shut: open it first.
            return self.need_vault(Then::Dial, cx);
        }
        self.dial(cx);
    }

    /// What shows when nothing is being asked: connecting, or the connection up.
    fn between(&self) -> Step {
        if self.up {
            Step::Connected
        } else {
            Step::Connecting
        }
    }

    fn ask_approval(&mut self, question: pc::Question, then: Then, cx: &mut Context<Self>) {
        let text = question.text();
        let line = text.trim().trim_end_matches("(yes/no)").trim_end();
        self.step = Step::Asking(Question {
            lines: vec![line.to_owned()],
            input: None,
            yes_no: true,
            skippable: false,
            kind: Kind::Approve(Some(question), Some(then)),
        });
        cx.notify();
    }

    /// A no goes on as each use needs: the password question falls to the vault and the person,
    /// a proxy leaves the panel failed.
    fn approval_answered(
        &mut self,
        question: pc::Question,
        then: Then,
        yes: bool,
        cx: &mut Context<Self>,
    ) {
        match (then, self.approvals.answer(question, yes)) {
            (Then::Proxy, Some(_)) => self.unlock_and_dial(cx),
            (Then::Proxy, None) => {
                self.step = Step::Failed(
                    "Not connected: the proxy command was not run. Retry asks again.".into(),
                );
            }
            (Then::Login(prompt), Some(command)) => self.run_command(command, prompt, cx),
            (Then::Login(prompt), None) => {
                self.command_tried = true;
                self.on_prompt(prompt, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    /// The password command, when it has a turn, answers a password question: asked about first
    /// if this Mac never approved it. Gives the prompt back when it does not apply.
    fn try_command(&mut self, prompt: Prompt, cx: &mut Context<Self>) -> Option<Prompt> {
        if !matches!(prompt, Prompt::Password { .. }) {
            return Some(prompt);
        }
        let command = self.password_command.clone();
        let question = command
            .as_deref()
            .and_then(|c| self.approvals.ask(Purpose::Login, c));
        match (
            pc::plan(command.as_deref(), self.command_tried, question.is_none()),
            question,
            command,
        ) {
            (pc::Step::Approve, Some(question), _) => {
                self.ask_approval(question, Then::Login(prompt), cx);
                None
            }
            (pc::Step::Run, _, Some(command)) => {
                self.run_command(command, prompt, cx);
                None
            }
            _ => Some(prompt),
        }
    }

    fn run_command(&mut self, command: String, prompt: Prompt, cx: &mut Context<Self>) {
        self.command_tried = true;
        self.step = self.between();
        self.note = None;
        let work = cx.background_spawn(async move { pc::run(&command, pc::TIMEOUT) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |s, cx| {
                match result {
                    Ok(secret) => {
                        // Answered straight to the server, never through `typed`, so it is not
                        // offered for saving in the vault.
                        s.note = Some("Password from the command.".into());
                        if let Some(other) = crate::login::answer(prompt, secret) {
                            s.on_prompt(other, cx);
                        }
                    }
                    Err(failure) => {
                        s.note = Some(failure.line());
                        s.on_prompt(prompt, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn dial(&mut self, cx: &mut Context<Self>) {
        tracing::info!(host = %self.spec.host, port = self.spec.port, "panel_connect");
        let mut dial = self.spec.clone();
        for name in &self.vault_keys {
            let key = Key::SshKey { name: name.clone() };
            if let Some(openssh) = Keeper::lookup(&key, cx) {
                dial.memory_keys.push(MemoryKey {
                    name: name.clone(),
                    openssh,
                });
            }
        }
        let rt = SshRuntime::handle(cx);
        // No shell and no agent forwarding: the panel only needs the connection.
        let (handle, events) = tern_ssh::connect_files(dial, &rt);
        self.handle = Some(handle);
        // The connection may outlive this login (a copy still using it): its events are read
        // on the runtime until it ends, and passed on only while somebody listens.
        let (tx, forwarded) = async_channel::bounded(64);
        rt.spawn(async move {
            while let Ok(event) = events.recv().await {
                let last = matches!(event, SessionEvent::Closed { .. });
                if !tx.is_closed() {
                    let _ = tx.send(event).await;
                }
                if last {
                    break;
                }
            }
        });
        self._events = cx.spawn(async move |this, cx| {
            while let Ok(event) = forwarded.recv().await {
                if this.update(cx, |s, cx| s.on_event(event, cx)).is_err() {
                    break;
                }
            }
        });
    }

    fn on_event(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::Data(_) | SessionEvent::Latency(_) => {}
            SessionEvent::Prompt(prompt) => self.on_prompt(prompt, cx),
            SessionEvent::Connected => {
                self.step = Step::Connected;
                self.up = true;
                self.offer = self.typed.take();
                cx.emit(LoginEvent::Connected);
                cx.notify();
            }
            SessionEvent::Closed {
                error,
                exit_status,
                reason,
            } => {
                let why = closed_reason(error, exit_status, &reason, &self.spec);
                self.handle = None;
                let was_up = std::mem::replace(&mut self.up, false);
                if was_up {
                    self.step = Step::Failed(why.clone());
                    cx.emit(LoginEvent::Dropped(why));
                } else {
                    self.step = Step::Failed(why);
                }
                cx.notify();
            }
        }
    }

    fn on_prompt(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        if let Some(prompt) = self.try_command(prompt, cx) {
            self.after_command(prompt, cx);
        }
    }

    fn after_command(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        let key = crate::login::key_for(&prompt, &self.spec.host, self.spec.port);
        // Each secret is tried from the vault once per connection, so a stale one falls through
        // to asking instead of failing in a loop.
        if let Some(key) = key.filter(|k| !self.tried.contains(k)) {
            if let Some(secret) = Keeper::lookup(&key, cx) {
                self.tried.push(key);
                if let Some(other) = crate::login::answer(prompt, secret) {
                    self.ask_user(other, cx);
                }
                return;
            }
            if Keeper::locked(cx) && !self.skipped_vault {
                return self.need_vault(Then::Prompt(prompt), cx);
            }
        }
        self.ask_user(prompt, cx);
    }

    fn field(&self, masked: bool, cx: &mut Context<Self>) -> Entity<TextInput> {
        let colors = self.colors;
        let input = cx.new(|cx| TextInput::new("", masked, colors, cx));
        cx.observe(&input, |_, _, cx| cx.notify()).detach();
        input
    }

    fn ask_user(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        let key = crate::login::key_for(&prompt, &self.spec.host, self.spec.port);
        let mut lines = Vec::new();
        if let Some(key) = &key {
            // Asked again for the same secret means the last answer was refused.
            if self.asked.contains(key) {
                lines.push("Permission denied, please try again.".to_owned());
            } else {
                self.asked.push(key.clone());
            }
        }
        let question = match prompt {
            Prompt::Password { user, host, reply } => {
                lines.push(format!("{user}@{host}'s password"));
                self.secret_question(lines, reply, key, cx)
            }
            Prompt::KeyPassphrase { path, reply } => {
                lines.push(format!("Passphrase for key '{}'", path.display()));
                self.secret_question(lines, reply, key, cx)
            }
            Prompt::UnknownHostKey {
                host,
                port,
                algorithm,
                fingerprint_sha256,
                reply,
            } => Question {
                lines: vec![
                    format!("The authenticity of host '{host}:{port}' can't be established."),
                    format!("{algorithm} key fingerprint is {fingerprint_sha256}."),
                    "Continue connecting?".to_owned(),
                ],
                input: None,
                yes_no: true,
                skippable: false,
                kind: Kind::HostKey(Some(reply)),
            },
            Prompt::Challenge {
                name,
                instructions,
                prompts,
                reply,
            } => {
                let mut pending: VecDeque<_> = prompts.into();
                let mut head: Vec<String> = [name, instructions]
                    .into_iter()
                    .filter(|l| !l.is_empty())
                    .collect();
                let first = pending.pop_front();
                let echo = first.as_ref().is_some_and(|p| p.echo);
                head.extend(first.map(|p| p.text));
                Question {
                    lines: head,
                    input: Some(self.field(!echo, cx)),
                    yes_no: false,
                    skippable: false,
                    kind: Kind::Challenge {
                        pending,
                        answers: Vec::new(),
                        reply: Some(reply),
                    },
                }
            }
        };
        self.step = Step::Asking(question);
        cx.notify();
    }

    fn secret_question(
        &self,
        lines: Vec<String>,
        reply: Sender<Option<SecretString>>,
        key: Option<Key>,
        cx: &mut Context<Self>,
    ) -> Question {
        Question {
            lines,
            input: Some(self.field(true, cx)),
            yes_no: false,
            skippable: false,
            kind: Kind::Secret(Some(reply), key),
        }
    }

    /// Enter in the field.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        let Step::Asking(question) = &mut self.step else {
            return;
        };
        let typed = question
            .input
            .as_ref()
            .map(|i| i.read(cx).text().to_owned())
            .unwrap_or_default();
        let skippable = question.skippable;
        match &mut question.kind {
            Kind::Secret(reply, key) => {
                let secret = SecretString::from(typed);
                if let (Some(key), false) = (key.take(), secret.expose_secret().is_empty()) {
                    self.typed = Some((key, secret.clone()));
                }
                let reply = reply.take();
                self.step = Step::Connecting;
                if let Some(reply) = reply {
                    let _ = reply.send(Some(secret));
                }
            }
            Kind::Challenge {
                pending,
                answers,
                reply,
            } => {
                answers.push(SecretString::from(typed));
                if let Some(next) = pending.pop_front() {
                    question.lines = vec![next.text];
                    if let Some(input) = &question.input {
                        input.update(cx, |i, cx| i.set_text("", cx));
                    }
                } else {
                    let all = std::mem::take(answers);
                    let reply = reply.take();
                    self.step = Step::Connecting;
                    if let Some(reply) = reply {
                        let _ = reply.send(Some(all));
                    }
                }
            }
            Kind::NewVault(..) => {
                let up = self.between();
                let Step::Asking(Question {
                    input,
                    kind: Kind::NewVault(key, secret, first),
                    ..
                }) = std::mem::replace(&mut self.step, up)
                else {
                    return;
                };
                if typed.is_empty() {
                    self.note = Some("Not saved.".into());
                    return cx.notify();
                }
                let typed = SecretString::from(typed);
                match first {
                    None => {
                        if let Some(input) = &input {
                            input.update(cx, |i, cx| i.set_text("", cx));
                        }
                        self.step = Step::Asking(Question {
                            lines: vec!["Repeat vault passphrase".to_owned()],
                            input,
                            yes_no: false,
                            skippable: false,
                            kind: Kind::NewVault(key, secret, Some(typed)),
                        });
                    }
                    Some(first) if first.expose_secret() == typed.expose_secret() => {
                        Keeper::adopt(Vault::new(first), cx);
                        self.store(key, secret, cx);
                    }
                    Some(_) => self.note = Some("Passphrases differ; not saved.".into()),
                }
            }
            Kind::Vault(..) => {
                let up = self.between();
                let Step::Asking(Question {
                    kind: Kind::Vault(ask, then),
                    ..
                }) = std::mem::replace(&mut self.step, up)
                else {
                    return;
                };
                if typed.is_empty() && skippable {
                    return self.vault_skipped(ask, then, cx);
                }
                let typed = match ask {
                    VaultAsk::Pin => Typed::Pin(typed),
                    VaultAsk::Passphrase => Typed::Passphrase(SecretString::from(typed)),
                };
                self.note = Some("Unlocking the vault…".into());
                let work = Keeper::try_unlock(typed, cx);
                cx.spawn(async move |this, cx| {
                    let tried = work.await;
                    let _ = this.update(cx, |s, cx| s.unlocked(tried, then, cx));
                })
                .detach();
            }
            Kind::HostKey(_) | Kind::Approve(..) => {}
        }
        cx.notify();
    }

    fn unlocked(&mut self, tried: Tried, then: Then, cx: &mut Context<Self>) {
        self.note = None;
        match tried {
            Tried::Opened => self.resume(then, true, cx),
            Tried::Again { ask, message } => {
                self.note = Some(message);
                self.ask_vault(ask, then, cx);
            }
            Tried::Failed(message) => {
                self.note = Some(message);
                self.resume(then, false, cx);
            }
        }
        cx.notify();
    }

    /// The person gave up opening the vault: back to asking the server's own question.
    fn vault_skipped(&mut self, ask: VaultAsk, then: Then, cx: &mut Context<Self>) {
        if ask == VaultAsk::Pin {
            return self.ask_vault(VaultAsk::Passphrase, then, cx);
        }
        self.skipped_vault = true;
        self.resume(then, false, cx);
    }

    /// Yes or No to the host key, or to running a command.
    pub fn answer_yes(&mut self, yes: bool, cx: &mut Context<Self>) {
        let Step::Asking(question) = &mut self.step else {
            return;
        };
        match &mut question.kind {
            Kind::HostKey(reply) => {
                let reply = reply.take();
                self.step = Step::Connecting;
                if let Some(reply) = reply {
                    let _ = reply.send(yes);
                }
                cx.notify();
            }
            Kind::Approve(asked, then) => {
                if let (Some(asked), Some(then)) = (asked.take(), then.take()) {
                    self.step = self.between();
                    self.approval_answered(asked, then, yes, cx);
                }
            }
            _ => {}
        }
    }

    /// Escape or Cancel: refuses whatever is asked, which ends the connection.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.step, Step::Connecting) {
            Step::Asking(Question { kind, .. }) => match kind {
                Kind::HostKey(Some(reply)) => {
                    let _ = reply.send(false);
                }
                Kind::Secret(Some(reply), _) => {
                    let _ = reply.send(None);
                }
                Kind::Challenge {
                    reply: Some(reply), ..
                } => {
                    let _ = reply.send(None);
                }
                Kind::Vault(ask, then) => return self.vault_skipped(ask, then, cx),
                Kind::Approve(Some(asked), Some(then)) => {
                    self.step = self.between();
                    return self.approval_answered(asked, then, false, cx);
                }
                Kind::NewVault(..) => {
                    self.step = self.between();
                    self.note = Some("Not saved.".into());
                }
                _ => {}
            },
            other => self.step = other,
        }
        cx.notify();
    }

    /// Start over after a failure.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        if matches!(self.step, Step::Failed(_)) {
            self.handle = None;
            self.begin(cx);
            cx.notify();
        }
    }

    /// "Save it" or "Not now" to the offer.
    pub fn answer_offer(&mut self, save: bool, cx: &mut Context<Self>) {
        let Some((key, secret)) = self.offer.take() else {
            return;
        };
        if !save {
            return cx.notify();
        }
        if !Keeper::exists(cx) {
            self.step = Step::Asking(Question {
                lines: vec!["New vault passphrase".to_owned()],
                input: Some(self.field(true, cx)),
                yes_no: false,
                skippable: false,
                kind: Kind::NewVault(key, secret, None),
            });
        } else if Keeper::locked(cx) {
            self.need_vault(Then::Save(key, secret), cx);
        } else {
            self.store(key, secret, cx);
        }
        cx.notify();
    }
}

/// Why a login ended, in the panel's words. A changed host key says how to forget the old
/// one and leaves that to the person.
fn closed_reason(
    error: Option<String>,
    exit_status: Option<u32>,
    reason: &Disconnect,
    spec: &ConnectSpec,
) -> String {
    let base = match (error, exit_status) {
        (Some(e), _) => e,
        (None, Some(code)) => format!("exit status {code}"),
        (None, None) => "the host closed the session".into(),
    };
    match reason {
        Disconnect::HostKeyChanged { hop } => {
            let (host, port) = hop
                .as_ref()
                .map_or((spec.host.as_str(), spec.port), |(h, p)| (h.as_str(), *p));
            let target = if port == 22 {
                host.to_owned()
            } else {
                format!("'[{host}]:{port}'")
            };
            format!(
                "{base}. If the host was reinstalled, run ssh-keygen -R {target} and try again."
            )
        }
        _ => base,
    }
}

#[path = "panel_login_vault.rs"]
mod vault;

#[cfg(test)]
#[path = "panel_login_tests.rs"]
mod tests;
