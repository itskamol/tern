//! The panel login's vault: opening it for a question or the dial, and saving a typed
//! password in it.

use gpui::Context;
use tern_ssh::SecretString;
use tern_vault::Key;

use super::{Kind, PanelLogin, Question, Step, Then};
use crate::keeper::{Ask as VaultAsk, Keeper, Opening, Pending};

impl PanelLogin {
    /// The vault is needed for `then` (a server question, a save), or to dial.
    pub(super) fn need_vault(&mut self, then: Then, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let need = Keeper::need_vault(cx, then, move |then, cx| {
            let _ = this.update(cx, |s, cx| s.vault_resolved(then, cx));
        });
        match need {
            Pending::Waiting => {
                self.step = self.between();
                self.note = Some("Asking the Keychain…".into());
                cx.notify();
            }
            Pending::Now(Opening::Open, then) => self.vault_resolved(then, cx),
            Pending::Now(Opening::Ask(ask), then) => self.ask_vault(ask, then, cx),
        }
    }

    /// The Keychain attempt is over: the vault is open, or the usual question follows.
    pub(super) fn vault_resolved(&mut self, then: Then, cx: &mut Context<Self>) {
        self.note = None;
        if Keeper::locked(cx) {
            return self.need_vault(then, cx);
        }
        self.resume(then, true, cx);
    }

    pub(super) fn ask_vault(&mut self, ask: VaultAsk, then: Then, cx: &mut Context<Self>) {
        let (title, skippable) = match (&then, ask) {
            (_, VaultAsk::Pin) => ("Vault PIN (Enter to use the passphrase)", true),
            (Then::Save(..), _) => ("Vault passphrase", false),
            _ => ("Vault passphrase (Enter to skip)", true),
        };
        self.step = Step::Asking(Question {
            lines: vec![title.to_owned()],
            input: Some(self.field(true, cx)),
            yes_no: false,
            skippable,
            kind: Kind::Vault(ask, then),
        });
        cx.notify();
    }

    /// What waited for the vault goes on, with the keys if it opened.
    pub(super) fn resume(&mut self, then: Then, opened: bool, cx: &mut Context<Self>) {
        match then {
            Then::Dial | Then::Proxy => self.dial(cx),
            Then::Login(prompt) => self.on_prompt(prompt, cx),
            Then::Prompt(prompt) if opened => self.on_prompt(prompt, cx),
            Then::Prompt(prompt) => self.ask_user(prompt, cx),
            Then::Save(key, secret) => {
                if opened {
                    self.store(key, secret, cx);
                }
            }
        }
    }

    pub(super) fn store(&mut self, key: Key, secret: SecretString, cx: &mut Context<Self>) {
        let Some(task) = Keeper::store(key, secret, cx) else {
            self.note = Some("Not saved: the vault is locked.".into());
            return cx.notify();
        };
        self.offer = None;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |s, cx| {
                s.note = Some(match result {
                    Ok(()) => "Saved in the vault.".to_owned(),
                    Err(e) => format!("Could not save the vault: {e}"),
                });
                cx.notify();
            });
        })
        .detach();
    }
}
