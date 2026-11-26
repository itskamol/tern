//! Activity, Alert, and Decision on the window (ADR 0002). [`crate::messages`] decides where a
//! message goes and [`crate::decisions`] owns the FIFO Decision queue; this file raises messages,
//! schedules wake-ups, and draws the Decision card and Activity list.

use std::time::{Duration, Instant, SystemTime};

use gpui::prelude::FluentBuilder;
use gpui::{
    Anchor, AnyElement, App, Context, Entity, InteractiveElement, IntoElement, MouseDownEvent,
    ParentElement, Pixels, Point, SharedString, StatefulInteractiveElement, Styled, Task,
    WeakEntity, Window, anchored, deferred, div, point, px,
};

#[path = "shell_toast.rs"]
mod toast;

pub(crate) use toast::{Alert, CubicBezier};

use super::{Shell, connections_ui::action_button};
use crate::clipboard::Answer;
use crate::decisions::{
    DecisionChoice, DecisionId, DecisionPrompt, DecisionQueue, DecisionRequest, DecisionTag,
    PendingDecision, QueueFull,
};
use crate::messages::{Kind, Messages, Place, SHOWN_FOR, Work};
use crate::session::Session;
use crate::theme::{CONTROL_RADIUS, MONO_FONT, SPACE_SM, TITLEBAR_HEIGHT};

const CLIPBOARD_ONCE: &str = "once";
const CLIPBOARD_ALWAYS: &str = "always";
const CLIPBOARD_DENY: &str = "deny";

/// Whether the Activity list is open. A press outside it closes it before the titlebar button
/// hears the same press, so the button is told which press that was and does not reopen it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum ListState {
    #[default]
    Closed,
    Open,
    ClosedBy(Point<Pixels>),
    /// The button's own press closed it; the click that follows must not reopen it.
    Swallow,
}

impl ListState {
    fn pressed_outside(self, at: Point<Pixels>) -> Self {
        match self {
            Self::Open => Self::ClosedBy(at),
            other => other,
        }
    }

    fn button_down(self, at: Point<Pixels>) -> Self {
        match self {
            Self::ClosedBy(by) if by == at => Self::Swallow,
            other => other,
        }
    }

    fn button_click(self) -> Self {
        match self {
            Self::Open | Self::Swallow => Self::Closed,
            Self::Closed | Self::ClosedBy(_) => Self::Open,
        }
    }
}

/// What a queued Decision represents inside the main window.
#[derive(Debug)]
enum DecisionKind {
    Clipboard {
        session: WeakEntity<Session>,
        who: String,
    },
    External {
        tag: DecisionTag,
    },
}

fn clipboard_choices() -> Vec<DecisionChoice> {
    vec![
        DecisionChoice::primary(CLIPBOARD_ONCE, "Allow once"),
        DecisionChoice::secondary(CLIPBOARD_ALWAYS, "Always for this host"),
        DecisionChoice::secondary(CLIPBOARD_DENY, "Deny"),
    ]
}

#[derive(Debug, Default)]
pub(super) struct ActivityUi {
    /// The snackbar an Alert shows as; nothing outside this module can show one.
    snackbars: toast::Snackbars,
    /// Questions tern cannot go on without, oldest first; the first one's card shows.
    decisions: DecisionQueue<DecisionKind>,
    /// Which Decision currently has its full multi-line command expanded.
    expanded_decision: Option<DecisionId>,
    vault_first: bool,
    messages: Messages,
    list_state: ListState,
    /// The one wake-up for when the newest Activity's time is up; a newer one replaces it.
    wake: Option<Task<()>>,
    /// The one wake-up for Decision countdown ticks and expiry deadlines; `None` when idle.
    decision_timer: Option<Task<()>>,
    /// Watches external Decision cancellation signals while Decisions are pending.
    decision_cancel_watch: Option<Task<()>>,
}

impl Shell {
    /// An Activity: "Copied 3 files", "Synced". Quiet, in the status line or the titlebar.
    pub(crate) fn activity(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.activity.messages.raise(
            Kind::Activity,
            text,
            false,
            false,
            SystemTime::now(),
            Instant::now(),
        );
        // Only the newest Activity shows, so only its time needs a repaint: replacing the task
        // drops the older wait.
        self.activity.wake = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SHOWN_FOR).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
        cx.notify();
    }

    /// Work in progress shown as an Activity ("Downloading\u{2026}"), replaced in place until
    /// [`Self::end_progress_activity`]. Repaints only when what shows changed.
    pub(crate) fn progress_activity(
        &mut self,
        work: Work,
        text: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        if self.activity.messages.set_progress(work, text) {
            cx.notify();
        }
    }

    pub(crate) fn end_progress_activity(&mut self, work: Work, cx: &mut Context<Self>) {
        if self.activity.messages.end_progress(work) {
            cx.notify();
        }
    }

    /// An Alert for something that did not work, with no action.
    pub(crate) fn alert_failed(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.alert(Alert::failure(text), cx);
    }

    /// An Alert: a failure, or a message with one action. The snackbar, kept in the list too.
    pub(crate) fn alert(&mut self, alert: Alert, cx: &mut Context<Self>) {
        self.activity.messages.raise(
            Kind::Alert,
            alert.text(),
            alert.has_action(),
            alert.failed(),
            SystemTime::now(),
            Instant::now(),
        );
        self.show_snackbar(alert, cx);
    }

    /// The status line is drawn: the setting is on, Settings is closed and a session tab is in
    /// front.
    pub(super) fn status_line_showing(&self) -> bool {
        self.settings.show_status_line
            && self.settings_page.is_none()
            && self.tabs.active_tab().and_then(|t| t.session()).is_some()
    }

    /// The Activity on screen now and where it goes.
    fn activity_now(&self) -> Option<(Place, SharedString)> {
        let status_line = self.status_line_showing();
        self.activity
            .messages
            .showing(Instant::now(), status_line)
            .map(|(place, text)| (place, SharedString::from(text.to_owned())))
    }

    /// A session holds a clipboard write (the setting is Ask): its Decision waits in line. A
    /// second write from the same session replaces the held text, not the question.
    pub(super) fn ask_clipboard(
        &mut self,
        session: &Entity<Session>,
        chars: usize,
        cx: &mut Context<Self>,
    ) {
        self.prune_stale_decisions(cx);
        let weak = session.downgrade();
        let already_waiting = self.activity.decisions.has_matching(
            |kind| matches!(kind, DecisionKind::Clipboard { session, .. } if *session == weak),
        );
        if !already_waiting {
            let who = session.read(cx).label();
            let question = crate::clipboard::question(&who, chars);
            let now = Instant::now();
            self.activity.messages.raise(
                Kind::Decision,
                &question,
                false,
                false,
                SystemTime::now(),
                now,
            );
            let request = DecisionRequest::new(
                DecisionKind::Clipboard { session: weak, who },
                DecisionPrompt::new(question, clipboard_choices()),
            );
            if self.activity.decisions.enqueue(request, now).is_err() {
                tracing::warn!("decision_queue_full");
            }
            self.sync_decision_tasks(cx);
        }
        cx.notify();
    }

    /// Enqueues a non-clipboard Decision (such as an Agent approval request) in FIFO order.
    ///
    /// # Errors
    /// Returns [`QueueFull`] when the Decision queue is already at its pending limit.
    pub(crate) fn ask_decision(
        &mut self,
        tag: DecisionTag,
        prompt: DecisionPrompt,
        timeout: Option<Duration>,
        cx: &mut Context<Self>,
    ) -> Result<PendingDecision, QueueFull> {
        self.prune_stale_decisions(cx);
        let now = Instant::now();
        self.activity.messages.raise(
            Kind::Decision,
            &prompt.question,
            false,
            false,
            SystemTime::now(),
            now,
        );
        let mut request = DecisionRequest::new(DecisionKind::External { tag }, prompt);
        if let Some(timeout) = timeout {
            request = request.with_timeout(timeout);
        }
        let pending = self.activity.decisions.enqueue(request, now)?;
        self.sync_decision_tasks(cx);
        cx.notify();
        Ok(pending)
    }

    /// Cancels every pending external Decision matching `tag` (e.g., all Agent Decisions on
    /// sleep, screen lock, or Pause agents) without disturbing clipboard Decisions.
    pub(crate) fn cancel_decisions_for(
        &mut self,
        tag: &DecisionTag,
        cx: &mut Context<Self>,
    ) -> usize {
        let cancelled = self.activity.decisions.cancel_where(
            |kind| matches!(kind, DecisionKind::External { tag: entry_tag } if entry_tag == tag),
        );
        if cancelled > 0 {
            self.sync_decision_tasks(cx);
            cx.notify();
        }
        cancelled
    }

    /// The person clicked one of the front Decision's choices. Late or mismatched clicks are
    /// ignored by [`DecisionQueue::answer`].
    fn answer_decision(&mut self, id: DecisionId, choice: &str, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.prune_stale_decisions(cx);
        let resolved = self.activity.decisions.answer(id, choice, now);
        if self.activity.expanded_decision == Some(id) {
            self.activity.expanded_decision = None;
        }
        if let Some(DecisionKind::Clipboard { session, .. }) = resolved {
            let answer = match choice {
                CLIPBOARD_ONCE => Some(Answer::Once),
                CLIPBOARD_ALWAYS => Some(Answer::Always),
                CLIPBOARD_DENY => Some(Answer::Deny),
                _ => None,
            };
            if let (Some(answer), Some(session)) = (answer, session.upgrade()) {
                session.update(cx, |s, cx| s.answer_clipboard(answer, cx));
            }
        }
        self.sync_decision_tasks(cx);
        cx.notify();
    }

    /// Drops the clipboard Decisions of `alias`'s sessions: their writes went through or are gone.
    /// Non-clipboard Decisions for `alias` stay queued.
    pub(super) fn forget_decisions_of(&mut self, alias: &str, cx: &mut Context<Self>) {
        let removed = self.activity.decisions.cancel_where(|kind| match kind {
            DecisionKind::Clipboard { session, .. } => session
                .upgrade()
                .is_none_or(|s| s.read(cx).alias() == alias),
            DecisionKind::External { .. } => false,
        });
        if removed > 0 {
            self.sync_decision_tasks(cx);
        }
    }

    /// Removes expired, externally cancelled, and stale clipboard Decisions across the whole queue
    /// so the `1 of N` count stays accurate.
    fn prune_stale_decisions(&mut self, cx: &App) -> usize {
        let swept = self.activity.decisions.sweep(Instant::now());
        let stale = self.activity.decisions.cancel_where(|kind| match kind {
            DecisionKind::Clipboard { session, .. } => session
                .upgrade()
                .and_then(|s| s.read(cx).clipboard_waiting())
                .is_none(),
            DecisionKind::External { .. } => false,
        });
        swept + stale
    }

    fn on_decision_wake(&mut self, cx: &mut Context<Self>) {
        self.prune_stale_decisions(cx);
        self.schedule_decision_timer(cx);
        cx.notify();
    }

    fn on_decision_tick(&mut self, cx: &mut Context<Self>) {
        self.prune_stale_decisions(cx);
        self.sync_decision_tasks(cx);
        cx.notify();
    }

    /// Keeps background Decision tasks strictly idle when nothing needs a timer or cancel watch.
    fn sync_decision_tasks(&mut self, cx: &mut Context<Self>) {
        self.schedule_decision_timer(cx);
        if self.activity.decisions.is_empty() {
            self.activity.decision_cancel_watch = None;
            return;
        }
        if self.activity.decision_cancel_watch.is_none() {
            let wake_rx = self.activity.decisions.cancel_wake();
            self.activity.decision_cancel_watch = Some(cx.spawn(async move |this, cx| {
                while wake_rx.recv().await.is_ok() {
                    let keep = this
                        .update(cx, |shell, cx| {
                            shell.on_decision_wake(cx);
                            if shell.activity.decisions.is_empty() {
                                shell.activity.decision_cancel_watch = None;
                                false
                            } else {
                                true
                            }
                        })
                        .unwrap_or(false);
                    if !keep {
                        break;
                    }
                }
            }));
        }
    }

    fn schedule_decision_timer(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.activity.decisions.sweep(now);
        let Some(wake_at) = self.activity.decisions.next_wake(now) else {
            self.activity.decision_timer = None;
            return;
        };
        let wait = wake_at
            .saturating_duration_since(now)
            .max(Duration::from_millis(1));
        self.activity.decision_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |shell, cx| {
                shell.on_decision_tick(cx);
            });
        }));
    }

    /// The front Decision's card (click-only; never bound to bare Enter or Space). Card geometry
    /// matches the Activity list (340 wide, 12 radius, 13 pt, inset and gap `SPACE_SM`); meta row
    /// uses 11 pt (Activity header/time), details use 12 pt with a 52 px label column (Files panel
    /// Size column) and 4 px row gap (`shell_connections::row`), command box uses `SPACE_SM`×
    /// `CONTROL_RADIUS` inset with `CONTROL_RADIUS` radius and 11/16 `MONO_FONT`, and buttons
    /// match the connection form's, `CONTROL_RADIUS` (6) apart (`docs/ui.md`, Messages / Agents).
    fn decision_card(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.prune_stale_decisions(cx);
        let front = self.activity.decisions.front(Instant::now())?;
        let id = front.id;
        let queue_label = front.position.label();
        let urgent = front.remaining_secs.is_some_and(|s| s <= 10);
        let countdown = front.remaining_secs.map(|s| format!("{s}s"));
        let question = match front.kind {
            DecisionKind::Clipboard { session, who } => {
                let chars = session
                    .upgrade()
                    .and_then(|s| s.read(cx).clipboard_waiting())?;
                crate::clipboard::question(who, chars)
            }
            DecisionKind::External { .. } => front.prompt.question.clone(),
        };
        let details = front.prompt.details.clone();
        let command = front.prompt.command.clone();
        let full_command = front.prompt.full_command.clone();
        let footnote = front.prompt.footnote.clone();
        let choices = front.prompt.choices.clone();
        let expanded = self.activity.expanded_decision == Some(id);
        let t = self.theme;
        let show_meta = queue_label.is_some() || countdown.is_some();
        Some(
            div()
                .id("decision")
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
                .when(show_meta, |el| {
                    let left = match queue_label {
                        Some(label) => div()
                            .px(px(CONTROL_RADIUS))
                            .py(px(2.))
                            .rounded(px(4.))
                            .bg(t.row_hover)
                            .text_color(t.text)
                            .child(SharedString::from(label)),
                        None => div().text_color(t.muted).child("Decision"),
                    };
                    el.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.))
                            .child(left)
                            .when_some(countdown, |row, text| {
                                row.child(
                                    div()
                                        .font_family(MONO_FONT)
                                        .text_color(if urgent { t.danger } else { t.muted })
                                        .child(SharedString::from(text)),
                                )
                            }),
                    )
                })
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(SharedString::from(question)),
                )
                .when(!details.is_empty(), |el| {
                    el.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .text_size(px(12.))
                            .children(details.into_iter().map(|d| {
                                div()
                                    .flex()
                                    .items_start()
                                    .gap(px(CONTROL_RADIUS))
                                    .child(
                                        div()
                                            .flex_none()
                                            .min_w(px(52.))
                                            .text_color(t.muted)
                                            .child(SharedString::from(format!("{}:", d.label))),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_1()
                                            .min_w_0()
                                            .flex_wrap()
                                            .items_center()
                                            .gap(px(CONTROL_RADIUS))
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .text_color(t.text)
                                                    .child(SharedString::from(d.value)),
                                            )
                                            .when_some(d.badge, |row, badge| {
                                                row.child(
                                                    div()
                                                        .flex_none()
                                                        .px(px(CONTROL_RADIUS))
                                                        .py(px(1.))
                                                        .rounded(px(4.))
                                                        .bg(t.danger.opacity(0.14))
                                                        .text_size(px(11.))
                                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                                        .text_color(t.danger)
                                                        .child(SharedString::from(badge)),
                                                )
                                            }),
                                    )
                            })),
                    )
                })
                .when_some(command, |el, cmd| {
                    let has_full = full_command.is_some();
                    let displayed = if expanded {
                        full_command.clone().unwrap_or(cmd)
                    } else {
                        cmd
                    };
                    el.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .id("decision-command-box")
                                    .w_full()
                                    .min_w_0()
                                    .when(expanded, |b| b.max_h(px(180.)).overflow_y_scroll())
                                    .when(!expanded, |b| b.overflow_hidden())
                                    .px(px(SPACE_SM))
                                    .py(px(CONTROL_RADIUS))
                                    .rounded(px(CONTROL_RADIUS))
                                    .bg(t.row_hover)
                                    .border_1()
                                    .border_color(t.hairline)
                                    .font_family(MONO_FONT)
                                    .text_size(px(11.))
                                    .line_height(px(16.))
                                    .text_color(t.text)
                                    .child(SharedString::from(displayed)),
                            )
                            .when(has_full, |col| {
                                col.child(
                                    div()
                                        .id("decision-command-toggle")
                                        .self_start()
                                        .text_size(px(11.))
                                        .text_color(t.accent)
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |s, _, _, cx| {
                                            s.activity.expanded_decision =
                                                if s.activity.expanded_decision == Some(id) {
                                                    None
                                                } else {
                                                    Some(id)
                                                };
                                            cx.notify();
                                        }))
                                        .child(if expanded {
                                            "Show less"
                                        } else {
                                            "Show full command"
                                        }),
                                )
                            }),
                    )
                })
                .child(div().flex().flex_wrap().gap(px(CONTROL_RADIUS)).children(
                    choices.into_iter().enumerate().map(|(ix, choice)| {
                        let choice_id = choice.id;
                        action_button(
                            ("decision-choice", ix),
                            format!("decision-button-{ix}"),
                            choice.label,
                            choice.primary,
                            &t,
                        )
                        .on_click(cx.listener(move |s, _, _, cx| {
                            s.answer_decision(id, &choice_id, cx);
                        }))
                    }),
                ))
                .when_some(footnote, |el, note| {
                    el.child(
                        div()
                            .text_size(px(11.))
                            .text_color(t.muted)
                            .child(SharedString::from(note)),
                    )
                })
                .into_any_element(),
        )
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn show_agent_card(&mut self, kind: crate::agents::attention::AttentionKind) {
        self.activity.vault_first = kind == crate::agents::attention::AttentionKind::Vault;
    }

    /// The top right under the titlebar: the Decision card, then the snackbar under it, so
    /// neither covers the other.
    pub(super) fn render_top_right(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let decision = self.decision_card(cx);
        let vault_unlock = self.agent_vault_unlock_card(window, cx);
        self.activity.vault_first &= vault_unlock.is_some();
        let cards = if self.activity.vault_first {
            [vault_unlock, decision]
        } else {
            [decision, vault_unlock]
        };
        let snackbar = self.render_snackbar(window, cx);
        if cards.iter().all(Option::is_none) && snackbar.is_none() {
            return None;
        }
        Some(
            deferred(
                anchored()
                    .position(point(px(0.), px(TITLEBAR_HEIGHT)))
                    .child(
                        div()
                            .w(window.viewport_size().width)
                            .p(px(SPACE_SM))
                            .flex()
                            .flex_col()
                            .items_end()
                            .gap(px(SPACE_SM))
                            .children(cards.into_iter().flatten())
                            .children(snackbar),
                    ),
            )
            .priority(3)
            .into_any_element(),
        )
    }

    /// The Activity on screen now, for the status line and for the titlebar: at most one is set.
    pub(super) fn activity_slots(&self) -> (Option<SharedString>, Option<SharedString>) {
        match self.activity_now() {
            Some((Place::StatusLine, text)) => (Some(text), None),
            Some((Place::Titlebar, text)) => (None, Some(text)),
            Some((Place::Snackbar | Place::Card, _)) | None => (None, None),
        }
    }

    pub(super) fn activity_list_open(&self) -> bool {
        self.activity.list_state == ListState::Open
    }

    /// The titlebar button's press, heard after the list's own outside-press handler.
    pub(crate) fn activity_button_down(&mut self, e: &MouseDownEvent) {
        self.activity.list_state = self.activity.list_state.button_down(e.position);
    }

    pub(crate) fn toggle_activity_list(&mut self, cx: &mut Context<Self>) {
        self.activity.list_state = self.activity.list_state.button_click();
        cx.notify();
    }

    /// Escape closes the list; returns whether it was open.
    pub(super) fn close_activity_list(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.activity_list_open();
        if open {
            self.activity.list_state = ListState::Closed;
            cx.notify();
        }
        open
    }

    /// The Activity list under the titlebar's right end, newest first. Card, rows and header are
    /// tern's port forwards popover's (340 wide, 6 inset, 12 radius, 13 pt; rows 8 × 6 with a
    /// 7 radius and 10 gap; an 11 pt muted header); its height is the Files tab's source
    /// menu's (320). The time is the status line's 11 pt.
    pub(super) fn render_activity_list(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.activity_list_open() {
            return None;
        }
        let t = self.theme;
        let now = SystemTime::now();
        let entries = self.activity.messages.list();
        let empty = entries.len() == 0;
        let rows = entries.rev().enumerate().map(|(ix, e)| {
            div()
                .id(("activity-row", ix))
                .px(px(8.))
                .py(px(6.))
                .rounded(px(7.))
                .flex()
                .items_start()
                .gap(px(10.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(e.failed, |el| el.text_color(t.danger))
                        .child(SharedString::from(e.text.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(t.muted)
                        .child(SharedString::from(crate::files_text::modified(e.at, now))),
                )
        });
        let card = div()
            .id("activity-list")
            .occlude()
            .w(px(340.))
            .max_h(px(320.))
            .overflow_y_scroll()
            .p(px(6.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded(px(12.))
            .border_1()
            .border_color(t.border)
            .bg(t.popup)
            .shadow_lg()
            .text_size(px(13.))
            .text_color(t.text)
            .on_mouse_down_out(cx.listener(|s, e: &MouseDownEvent, _, cx| {
                s.activity.list_state = s.activity.list_state.pressed_outside(e.position);
                cx.notify();
            }))
            .child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_size(px(11.))
                    .text_color(t.muted)
                    .child("Activity"),
            )
            .when(empty, |el| {
                el.child(
                    div()
                        .px(px(8.))
                        .py(px(6.))
                        .text_color(t.muted)
                        .child("Nothing yet"),
                )
            })
            .children(rows);
        let right = window.viewport_size().width - px(SPACE_SM);
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::TopRight)
                    .position(point(right, px(TITLEBAR_HEIGHT)))
                    .snap_to_window_with_margin(px(SPACE_SM))
                    .child(card),
            )
            .priority(2)
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click on the titlebar button: its press, then the click.
    fn button(list: ListState, at: Point<Pixels>) -> ListState {
        list.button_down(at).button_click()
    }

    #[test]
    fn the_button_opens_the_list_and_closes_it_again() {
        let at = point(px(880.), px(20.));
        let open = button(ListState::Closed, at);
        assert_eq!(open, ListState::Open);
        // The press on the button is outside the list: the list hears it first.
        assert_eq!(button(open.pressed_outside(at), at), ListState::Closed);
    }

    #[test]
    fn a_press_elsewhere_closes_the_list_and_the_next_button_click_opens_it() {
        let closed = ListState::Open.pressed_outside(point(px(300.), px(300.)));
        assert_eq!(button(closed, point(px(880.), px(20.))), ListState::Open);
    }

    #[test]
    fn a_press_outside_a_closed_list_changes_nothing() {
        let at = point(px(300.), px(300.));
        assert_eq!(ListState::Closed.pressed_outside(at), ListState::Closed);
    }
}
