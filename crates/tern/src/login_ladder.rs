//! The order in which a host's login question is answered, for a Session and for a Files tab
//! login alike: the saved connection's password command, then the vault, then the person.
//!
//! The ladder keeps what one connection has already tried and decides what happens next. It
//! reads no vault, runs no command and draws nothing: the caller says what the vault holds and
//! whether the command needs approval, carries the outcome out, and reports back. What
//! outlives a connection (the vault was skipped) stays with the caller and comes in as a fact.

use tern_ssh::{Prompt, SecretString};
use tern_vault::Key;

use crate::password_command::{self as pc, Question};

/// What the vault has for a question, as the caller found out.
pub enum InVault {
    Secret(SecretString),
    /// It may hold the answer but is shut.
    Locked,
    Nothing,
}

/// What the caller knows that the ladder must not look up itself.
#[derive(Debug)]
pub struct Facts<'a> {
    pub host: &'a str,
    pub port: u16,
    pub command: Option<&'a str>,
    /// The question to put before the command first runs; `None` once this Mac approved it.
    pub approval: Option<Question>,
    pub vault_skipped: bool,
}

/// What happens next for a question.
#[derive(Debug)]
pub enum Next {
    /// Ask whether the password command may run. Yes runs it; no goes on down the ladder.
    ApproveCommand(Question),
    RunCommand(String),
    FromVault(Key, SecretString),
    /// Ask to open the vault; the question is put again once it is open, or answered by the
    /// person if it stays shut.
    OpenVault,
    /// The person answers. `refused` when the same secret was asked for before.
    Ask {
        refused: bool,
    },
}

/// One connection's login memory.
#[derive(Default)]
pub struct Ladder {
    /// Entries already answered from the vault.
    tried: Vec<Key>,
    /// The password command already had its turn.
    command_tried: bool,
    /// Secrets the person was already asked for.
    asked: Vec<Key>,
    /// The entry the open question would answer.
    asking: Option<Key>,
    /// What the person typed for a question, offered for saving once connected.
    typed: Option<(Key, SecretString)>,
}

impl Ladder {
    /// Decides what happens next for `prompt`. `vault` is asked at most once, and only for an
    /// entry not yet answered from it.
    pub fn next(
        &mut self,
        prompt: &Prompt,
        facts: Facts<'_>,
        vault: impl FnOnce(&Key) -> InVault,
    ) -> Next {
        if matches!(prompt, Prompt::Password { .. }) {
            match pc::plan(facts.command, self.command_tried, facts.approval.is_none()) {
                pc::Step::Approve => {
                    if let Some(question) = facts.approval {
                        // Yes runs the command and no refuses it: either way the turn is spent.
                        self.command_tried = true;
                        return Next::ApproveCommand(question);
                    }
                }
                pc::Step::Run => {
                    if let Some(command) = facts.command {
                        self.command_tried = true;
                        return Next::RunCommand(command.to_owned());
                    }
                }
                pc::Step::Skip => {}
            }
        }
        let key = crate::login::key_for(prompt, facts.host, facts.port);
        if let Some(key) = key.filter(|k| !self.tried.contains(k)) {
            match vault(&key) {
                InVault::Secret(secret) => {
                    self.tried.push(key.clone());
                    return Next::FromVault(key, secret);
                }
                InVault::Locked if !facts.vault_skipped => return Next::OpenVault,
                InVault::Locked | InVault::Nothing => {}
            }
        }
        Next::Ask {
            refused: self.ask(prompt, facts.host, facts.port),
        }
    }

    /// The person is about to be asked `prompt`. Says whether the last answer to it was refused.
    pub fn ask(&mut self, prompt: &Prompt, host: &str, port: u16) -> bool {
        self.asking = crate::login::key_for(prompt, host, port);
        match &self.asking {
            Some(key) if self.asked.contains(key) => true,
            Some(key) => {
                self.asked.push(key.clone());
                false
            }
            None => false,
        }
    }

    /// The person answered the question put by [`Ladder::ask`]; `None` after Ctrl-C.
    pub fn answered(&mut self, answer: Option<SecretString>) {
        if let (Some(key), Some(secret)) = (self.asking.take(), answer) {
            self.typed = Some((key, secret));
        }
    }

    /// A secret the person typed for `key`.
    pub fn record_typed(&mut self, key: Key, secret: SecretString) {
        self.typed = Some((key, secret));
    }

    #[must_use]
    pub fn has_typed(&self) -> bool {
        self.typed.is_some()
    }

    /// What was typed, to offer for saving. Secrets from the vault or a command never are here.
    pub fn take_typed(&mut self) -> Option<(Key, SecretString)> {
        self.typed.take()
    }

    /// A new connection starts from nothing.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;

    use tern_ssh::{ExposeSecret, oneshot};

    use super::*;
    use crate::password_command::{Approvals, Purpose};

    const HOST: &str = "demo";
    const PORT: u16 = 22;

    fn password() -> Prompt {
        let (reply, _) = oneshot::channel();
        Prompt::Password {
            user: "test".into(),
            host: HOST.into(),
            reply,
        }
    }

    fn passphrase() -> Prompt {
        let (reply, _) = oneshot::channel();
        Prompt::KeyPassphrase {
            path: PathBuf::from("/k/id"),
            reply,
        }
    }

    fn host_key() -> Prompt {
        let (reply, _) = oneshot::channel();
        Prompt::UnknownHostKey {
            host: HOST.into(),
            port: PORT,
            algorithm: "ssh-ed25519".into(),
            fingerprint_sha256: "SHA256:x".into(),
            reply,
        }
    }

    fn secret(text: &str) -> SecretString {
        SecretString::from(text.to_owned())
    }

    fn facts(command: Option<&str>, approved: bool) -> Facts<'_> {
        Facts {
            host: HOST,
            port: PORT,
            command,
            approval: command
                .filter(|_| !approved)
                .and_then(|c| Approvals::default().ask(Purpose::Login, c)),
            vault_skipped: false,
        }
    }

    fn vault_holds(text: &'static str) -> impl FnOnce(&Key) -> InVault {
        move |_| InVault::Secret(secret(text))
    }

    fn nothing(_: &Key) -> InVault {
        InVault::Nothing
    }

    fn locked(_: &Key) -> InVault {
        InVault::Locked
    }

    #[test]
    fn the_password_command_answers_before_the_vault() {
        let mut ladder = Ladder::default();
        let next = ladder.next(&password(), facts(Some("pass"), true), vault_holds("v"));
        assert!(matches!(next, Next::RunCommand(c) if c == "pass"));
    }

    #[test]
    fn an_unapproved_command_is_asked_about_before_it_runs() {
        let mut ladder = Ladder::default();
        let next = ladder.next(&password(), facts(Some("pass"), false), vault_holds("v"));
        assert!(matches!(next, Next::ApproveCommand(_)));
    }

    #[test]
    fn an_approved_command_run_after_the_question_gets_no_second_turn() {
        let mut ladder = Ladder::default();
        let asked = ladder.next(&password(), facts(Some("pass"), false), nothing);
        assert!(matches!(asked, Next::ApproveCommand(_)));
        let again = ladder.next(&password(), facts(Some("pass"), true), nothing);
        assert!(matches!(again, Next::Ask { .. }));
    }

    #[test]
    fn the_command_gets_one_turn_and_the_second_ask_goes_to_the_vault() {
        let mut ladder = Ladder::default();
        ladder.next(&password(), facts(Some("pass"), true), nothing);
        let next = ladder.next(&password(), facts(Some("pass"), true), vault_holds("v"));
        assert!(matches!(next, Next::FromVault(..)));
    }

    #[test]
    fn a_refused_command_falls_through_to_the_vault_then_the_person() {
        let mut ladder = Ladder::default();
        ladder.next(&password(), facts(Some("pass"), false), nothing);
        let next = ladder.next(&password(), facts(Some("pass"), false), vault_holds("v"));
        assert!(matches!(next, Next::FromVault(..)));
        let next = ladder.next(&password(), facts(Some("pass"), false), nothing);
        assert!(matches!(next, Next::Ask { .. }));
    }

    #[test]
    fn a_failed_command_falls_through_because_its_turn_is_spent() {
        let mut ladder = Ladder::default();
        let first = ladder.next(&password(), facts(Some("pass"), true), nothing);
        assert!(matches!(first, Next::RunCommand(_)));
        let after_failure = ladder.next(&password(), facts(Some("pass"), true), nothing);
        assert!(matches!(after_failure, Next::Ask { refused: false }));
    }

    #[test]
    fn the_command_only_answers_a_password_question() {
        let mut ladder = Ladder::default();
        let next = ladder.next(&passphrase(), facts(Some("pass"), true), nothing);
        assert!(matches!(next, Next::Ask { .. }));
    }

    #[test]
    fn a_vault_entry_answers_once_per_connection() {
        let mut ladder = Ladder::default();
        let first = ladder.next(&password(), facts(None, true), vault_holds("stale"));
        assert!(matches!(first, Next::FromVault(_, s) if s.expose_secret() == "stale"));
        let looked_up = Cell::new(false);
        let second = ladder.next(&password(), facts(None, true), |_| {
            looked_up.set(true);
            InVault::Secret(secret("stale"))
        });
        assert!(matches!(second, Next::Ask { .. }));
        assert!(!looked_up.get(), "a tried entry is not looked up again");
    }

    #[test]
    fn a_new_connection_may_use_the_vault_again() {
        let mut ladder = Ladder::default();
        ladder.next(&password(), facts(Some("pass"), true), vault_holds("v"));
        ladder.next(&password(), facts(None, true), vault_holds("v"));
        ladder.reset();
        let next = ladder.next(&password(), facts(Some("pass"), true), nothing);
        assert!(matches!(next, Next::RunCommand(_)));
    }

    #[test]
    fn a_locked_vault_is_asked_to_open() {
        let mut ladder = Ladder::default();
        let next = ladder.next(&password(), facts(None, true), locked);
        assert!(matches!(next, Next::OpenVault));
    }

    #[test]
    fn a_skipped_vault_is_not_asked_about_again() {
        let mut ladder = Ladder::default();
        let skipped = Facts {
            vault_skipped: true,
            ..facts(None, true)
        };
        let next = ladder.next(&password(), skipped, locked);
        assert!(matches!(next, Next::Ask { refused: false }));
    }

    #[test]
    fn asked_again_for_the_same_secret_says_the_last_answer_was_refused() {
        let mut ladder = Ladder::default();
        let first = ladder.next(&password(), facts(None, true), nothing);
        assert!(matches!(first, Next::Ask { refused: false }));
        let second = ladder.next(&password(), facts(None, true), nothing);
        assert!(matches!(second, Next::Ask { refused: true }));
        let other = ladder.next(&passphrase(), facts(None, true), nothing);
        assert!(matches!(other, Next::Ask { refused: false }));
    }

    #[test]
    fn a_host_key_question_always_reaches_the_person() {
        let mut ladder = Ladder::default();
        let next = ladder.next(&host_key(), facts(Some("pass"), true), vault_holds("v"));
        assert!(matches!(next, Next::Ask { refused: false }));
        let again = ladder.next(&host_key(), facts(Some("pass"), true), locked);
        assert!(matches!(again, Next::Ask { refused: false }));
    }

    #[test]
    fn a_typed_secret_is_offered_for_saving_but_vault_and_command_secrets_are_not() {
        let mut ladder = Ladder::default();
        ladder.next(&password(), facts(Some("pass"), true), nothing);
        ladder.next(&password(), facts(None, true), vault_holds("v"));
        assert!(!ladder.has_typed(), "nothing was typed yet");

        ladder.next(&passphrase(), facts(None, true), nothing);
        ladder.answered(Some(secret("typed")));
        let typed = ladder.take_typed();
        assert!(matches!(
            &typed,
            Some((Key::KeyPassphrase { .. }, s)) if s.expose_secret() == "typed"
        ));
        assert!(ladder.take_typed().is_none());
    }

    #[test]
    fn a_question_answered_by_ctrl_c_leaves_nothing_to_save() {
        let mut ladder = Ladder::default();
        ladder.next(&password(), facts(None, true), nothing);
        ladder.answered(None);
        assert!(!ladder.has_typed());
    }
}
