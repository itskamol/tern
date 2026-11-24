//! In-window contextual `Unlock Vault` card for approved Agent requests waiting on a locked Vault.
//!
//! Only appears while at least one Agent request is actively waiting in `AgentsUi::vault_waits`
//! (never on normal window focus when nothing is waiting). Unlocks via [`Keeper::try_unlock`]
//! without ever calling [`Keeper::need_vault`] (no automatic macOS Keychain prompt) and without
//! stealing application focus.

use std::time::Instant;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, Context, Entity, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled, Subscription,
    Window, div, px,
};
use tern_ssh::SecretString;

use super::Shell;
use crate::agents::attention::{self, AttentionCue};
use crate::agents::vault_wait::PendingVaultUnlock;
use crate::hover::HoverFade as _;
use crate::keeper::{Ask, Keeper, Tried, Typed};
use crate::text_input::TextInput;
use crate::theme::{CONTROL_RADIUS, MONO_FONT, SPACE_SM, Theme};

pub(super) struct AgentVaultPromptUi {
    pin: Entity<TextInput>,
    passphrase: Entity<TextInput>,
    use_passphrase: bool,
    busy: bool,
    error: Option<String>,
    focused_for_active: bool,
    _repaint: Vec<Subscription>,
}

impl std::fmt::Debug for AgentVaultPromptUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentVaultPromptUi")
            .field("use_passphrase", &self.use_passphrase)
            .field("busy", &self.busy)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

fn card_button(
    id: &'static str,
    label: impl Into<SharedString>,
    primary: bool,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(28.))
        .px(px(12.))
        .flex()
        .items_center()
        .rounded(px(8.))
        .text_sm()
        .cursor_pointer()
        .when(primary, |el| el.bg(t.accent).text_color(t.on_accent))
        .when(!primary, |el| {
            el.hover_fade(id, t.row_hover, t.row_active)
                .text_color(t.text)
        })
        .child(label.into())
}

impl Shell {
    /// Registers an approved Agent request waiting for the Vault to be unlocked, posts a
    /// non-focus-stealing attention cue when tern is inactive, and starts countdown/cancel timers.
    pub(super) fn wait_for_agent_vault_unlock(
        &mut self,
        caller: &str,
        host: &str,
        deadline: Instant,
        cx: &mut Context<Self>,
    ) -> Option<PendingVaultUnlock> {
        let now = Instant::now();
        let pending = self
            .agents
            .vault_waits
            .enqueue(caller, host, deadline, now)
            .ok()?;
        let cue = AttentionCue::for_vault_unlock(caller, host);
        attention::notify_if_inactive(self.agents.attention.as_ref(), &cue);
        self.ensure_agent_vault_prompt(cx);
        if let Some(ui) = self.agents.vault_prompt.as_mut() {
            ui.error = None;
            ui.focused_for_active = false;
        }
        self.sync_agent_vault_wait_tasks(cx);
        cx.notify();
        Some(pending)
    }

    /// Wakes every non-expired Agent waiting on Vault unlock and clears the contextual card.
    /// Called whenever the Vault unlocks (from this card, Settings → Vault, or another panel).
    pub(super) fn on_vault_unlocked(&mut self, cx: &mut Context<Self>) {
        let woken = self.agents.vault_waits.resolve_unlocked(Instant::now());
        self.clear_agent_vault_prompt(cx);
        self.sync_agent_vault_wait_tasks(cx);
        if woken > 0 {
            tracing::info!(woken, "agent_vault_waits_unlocked");
            cx.notify();
        }
    }

    /// Cancels every waiting Agent Vault unlock request and clears the contextual card.
    pub(super) fn cancel_agent_vault_waits(&mut self, cx: &mut Context<Self>) -> usize {
        let cancelled = self.agents.vault_waits.cancel_all();
        self.clear_agent_vault_prompt(cx);
        self.sync_agent_vault_wait_tasks(cx);
        if cancelled > 0 {
            tracing::info!(cancelled, "agent_vault_waits_cancelled");
            cx.notify();
        }
        cancelled
    }

    fn ensure_agent_vault_prompt(&mut self, cx: &mut Context<Self>) {
        if self.agents.vault_prompt.is_some() {
            return;
        }
        let colors = self.theme.input_colors();
        let pin = cx.new(|cx| TextInput::new("Vault PIN", true, colors, cx));
        let passphrase = cx.new(|cx| TextInput::new("Vault passphrase", true, colors, cx));
        let repaint = vec![
            cx.observe(&pin, |_, _, cx| cx.notify()),
            cx.observe(&passphrase, |_, _, cx| cx.notify()),
        ];
        self.agents.vault_prompt = Some(AgentVaultPromptUi {
            pin,
            passphrase,
            use_passphrase: false,
            busy: false,
            error: None,
            focused_for_active: false,
            _repaint: repaint,
        });
    }

    fn clear_agent_vault_prompt(&mut self, cx: &mut Context<Self>) {
        if let Some(ui) = self.agents.vault_prompt.take() {
            ui.pin.update(cx, |i, cx| i.set_text("", cx));
            ui.passphrase.update(cx, |i, cx| i.set_text("", cx));
        }
    }

    fn sync_agent_vault_wait_tasks(&mut self, cx: &mut Context<Self>) {
        if self.agents.vault_waits.is_empty() {
            self.agents.vault_wait_timer = None;
            self.agents.vault_wait_cancel_watch = None;
            self.clear_agent_vault_prompt(cx);
            return;
        }
        if let Some(delay) = self.agents.vault_waits.next_wake_delay(Instant::now()) {
            let delay = delay.max(std::time::Duration::from_millis(1));
            self.agents.vault_wait_timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                let _ = this.update(cx, |s, cx| {
                    if !Keeper::locked(cx) {
                        s.on_vault_unlocked(cx);
                        return;
                    }
                    s.agents.vault_waits.sweep(Instant::now());
                    s.sync_agent_vault_wait_tasks(cx);
                    cx.notify();
                });
            }));
        } else {
            self.agents.vault_wait_timer = None;
        }

        if self.agents.vault_wait_cancel_watch.is_none() {
            let wake_rx = self.agents.vault_waits.cancel_wake();
            self.agents.vault_wait_cancel_watch = Some(cx.spawn(async move |this, cx| {
                while wake_rx.recv().await.is_ok() {
                    let keep = this
                        .update(cx, |s, cx| {
                            s.agents.vault_waits.sweep(Instant::now());
                            s.sync_agent_vault_wait_tasks(cx);
                            cx.notify();
                            !s.agents.vault_waits.is_empty()
                        })
                        .unwrap_or(false);
                    if !keep {
                        break;
                    }
                }
            }));
        }
    }

    fn submit_agent_vault_unlock(&mut self, cx: &mut Context<Self>) {
        if !Keeper::locked(cx) {
            self.on_vault_unlocked(cx);
            return;
        }
        let pin_set = Keeper::pin_set(cx);
        let Some(ui) = self.agents.vault_prompt.as_mut() else {
            return;
        };
        if ui.busy {
            return;
        }
        let (input, typed) = if pin_set && !ui.use_passphrase {
            let pin = ui.pin.read(cx).text().to_owned();
            if pin.is_empty() {
                ui.error = Some("Enter the vault PIN.".to_owned());
                cx.notify();
                return;
            }
            (ui.pin.clone(), Typed::Pin(pin))
        } else {
            let pass = ui.passphrase.read(cx).text().to_owned();
            if pass.is_empty() {
                ui.error = Some("Enter the vault passphrase.".to_owned());
                cx.notify();
                return;
            }
            (
                ui.passphrase.clone(),
                Typed::Passphrase(SecretString::from(pass)),
            )
        };

        ui.busy = true;
        ui.error = None;
        cx.notify();

        let work = Keeper::try_unlock(typed, cx);
        cx.spawn(async move |this, cx| {
            let tried = work.await;
            let _ = this.update(cx, |s, cx| {
                if let Some(ui) = s.agents.vault_prompt.as_mut() {
                    ui.busy = false;
                    ui.use_passphrase |= matches!(
                        tried,
                        Tried::Again {
                            ask: Ask::Passphrase,
                            ..
                        }
                    );
                }
                match tried {
                    Tried::Opened => {
                        input.update(cx, |i, cx| i.set_text("", cx));
                        s.activity("Vault unlocked", cx);
                        s.on_vault_unlocked(cx);
                    }
                    Tried::Again { message, .. } | Tried::Failed(message) => {
                        if let Some(ui) = s.agents.vault_prompt.as_mut() {
                            ui.error = Some(message);
                        }
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn toggle_agent_vault_passphrase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ui) = self.agents.vault_prompt.as_mut() {
            ui.use_passphrase = !ui.use_passphrase;
            ui.error = None;
            let by_pin = Keeper::pin_set(cx) && !ui.use_passphrase;
            let focus = if by_pin {
                ui.pin.focus_handle(cx)
            } else {
                ui.passphrase.focus_handle(cx)
            };
            if window.is_window_active() {
                window.focus(&focus, cx);
            }
            cx.notify();
        }
    }

    /// Draws the contextual `Unlock Vault` card in the top-right stack when at least one approved
    /// Agent request is actively waiting for the Vault to unlock.
    pub(super) fn agent_vault_unlock_card(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !Keeper::locked(cx) && !self.agents.vault_waits.is_empty() {
            self.on_vault_unlocked(cx);
            return None;
        }
        let Some(front) = self.agents.vault_waits.front(Instant::now()) else {
            if self.agents.vault_prompt.is_some() {
                self.sync_agent_vault_wait_tasks(cx);
            }
            return None;
        };

        self.ensure_agent_vault_prompt(cx);
        let pin_set = Keeper::pin_set(cx);
        let window_active = window.is_window_active();
        let (active_input, by_pin, busy, error, should_focus) = {
            let ui = self.agents.vault_prompt.as_mut()?;
            let by_pin = pin_set && !ui.use_passphrase;
            let input = if by_pin {
                ui.pin.clone()
            } else {
                ui.passphrase.clone()
            };
            let should_focus = if window_active {
                if ui.focused_for_active {
                    false
                } else {
                    ui.focused_for_active = true;
                    true
                }
            } else {
                ui.focused_for_active = false;
                false
            };
            (input, by_pin, ui.busy, ui.error.clone(), should_focus)
        };
        if should_focus {
            window.focus(&active_input.focus_handle(cx), cx);
        }

        let t = self.theme;
        let urgent = front.remaining_secs <= 10;
        let countdown = format!("{}s", front.remaining_secs);
        let header_label = if front.waiting_count > 1 {
            format!("Unlock Vault · 1 of {}", front.waiting_count)
        } else {
            "Unlock Vault".to_owned()
        };
        let prompt_line = format!("{} is waiting to connect to {}", front.caller, front.host);

        Some(
            div()
                .id("agent-vault-unlock")
                .occlude()
                .w(px(340.))
                .p(px(SPACE_SM))
                .flex()
                .flex_col()
                .gap(px(SPACE_SM))
                .rounded(px(12.))
                .border_1()
                .border_color(t.border)
                .bg(t.popup)
                .shadow_lg()
                .text_size(px(13.))
                .text_color(t.text)
                .on_key_down(cx.listener(|s, e: &KeyDownEvent, _, cx| {
                    if e.keystroke.key == "enter" {
                        s.submit_agent_vault_unlock(cx);
                    } else if e.keystroke.key == "escape" {
                        s.cancel_agent_vault_waits(cx);
                    }
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .text_size(px(11.))
                        .child(
                            div()
                                .text_color(t.muted)
                                .child(SharedString::from(header_label)),
                        )
                        .child(
                            div()
                                .font_family(MONO_FONT)
                                .text_color(if urgent { t.danger } else { t.muted })
                                .child(SharedString::from(countdown)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(SharedString::from(prompt_line)),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(t.muted)
                                .child(if by_pin {
                                    "Enter the vault PIN to continue once."
                                } else {
                                    "Enter the vault passphrase to continue once."
                                }),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(30.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(px(8.))
                        .border_1()
                        .border_color(t.border)
                        .bg(t.row_hover)
                        .overflow_hidden()
                        .text_sm()
                        .child(active_input),
                )
                .when_some(error, |el, msg| {
                    el.child(
                        div()
                            .text_size(px(12.))
                            .text_color(t.danger)
                            .child(SharedString::from(msg)),
                    )
                })
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(CONTROL_RADIUS))
                        .child(
                            card_button(
                                "agent-vault-submit",
                                if busy { "Unlocking…" } else { "Unlock" },
                                true,
                                &t,
                            )
                            .on_click(cx.listener(|s, _, _, cx| s.submit_agent_vault_unlock(cx))),
                        )
                        .child(
                            card_button("agent-vault-cancel", "Cancel", false, &t).on_click(
                                cx.listener(|s, _, _, cx| {
                                    s.cancel_agent_vault_waits(cx);
                                }),
                            ),
                        )
                        .when(pin_set, |row| {
                            row.child(
                                card_button(
                                    "agent-vault-switch",
                                    if by_pin { "Use passphrase" } else { "Use PIN" },
                                    false,
                                    &t,
                                )
                                .on_click(cx.listener(
                                    |s, _, w, cx| {
                                        s.toggle_agent_vault_passphrase(w, cx);
                                    },
                                )),
                            )
                        }),
                )
                .into_any_element(),
        )
    }
}
