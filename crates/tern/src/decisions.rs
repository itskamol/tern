//! FIFO queue for Decisions (ADR 0002): questions tern cannot go on without, from any caller
//! (Session clipboard writes, Agents, or future prompts). Handles ordering (`1 of N`), optional
//! expiry deadlines, cross-thread external cancellation, and single-resolution delivery without
//! knowing about GPUI or windows.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// Maximum pending Decisions kept at once so a misbehaving caller cannot grow memory without bound.
pub const MAX_PENDING: usize = 64;

/// Unique identifier for one queued Decision, preventing late clicks from answering the next item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecisionId(u64);

/// Caller category for a non-clipboard Decision, allowing group cancellation (sleep, lock, pause)
/// without disturbing unrelated Decisions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DecisionTag {
    Agent,
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Strips ANSI escape sequences, control characters, and Unicode bidi overrides from text
/// displayed on a Decision card.
#[must_use]
pub fn sanitize_display_text(text: &str) -> String {
    strip_ansi_escapes::strip_str(text)
        .chars()
        .filter(|&c| !c.is_control() && !is_bidi_control(c))
        .collect()
}

/// Sanitizes multi-line text for expanded command inspection while preserving line breaks.
#[must_use]
pub fn sanitize_multiline_display_text(text: &str) -> String {
    text.lines()
        .map(sanitize_display_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One clickable choice shown on a Decision card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionChoice {
    pub id: String,
    pub label: String,
    pub primary: bool,
}

impl DecisionChoice {
    #[must_use]
    pub fn primary(id: impl Into<String>, label: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            id: id.into(),
            label: sanitize_display_text(&label),
            primary: true,
        }
    }

    #[must_use]
    pub fn secondary(id: impl Into<String>, label: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            id: id.into(),
            label: sanitize_display_text(&label),
            primary: false,
        }
    }
}

/// One labeled metadata line shown beneath the Decision question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionDetail {
    pub label: String,
    pub value: String,
}

/// Display content of a Decision card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionPrompt {
    pub question: String,
    pub details: Vec<DecisionDetail>,
    pub command: Option<String>,
    /// Optional sanitized full multi-line command shown when the user expands a truncated preview.
    pub full_command: Option<String>,
    /// Optional muted explanatory footnote shown beneath the choices.
    pub footnote: Option<String>,
    pub choices: Vec<DecisionChoice>,
}

impl DecisionPrompt {
    #[must_use]
    pub fn new(question: impl Into<String>, choices: Vec<DecisionChoice>) -> Self {
        let question = question.into();
        Self {
            question: sanitize_display_text(&question),
            details: Vec::new(),
            command: None,
            full_command: None,
            footnote: None,
            choices,
        }
    }

    #[must_use]
    pub fn with_detail(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        let label = label.into();
        let value = value.into();
        self.details.push(DecisionDetail {
            label: sanitize_display_text(&label),
            value: sanitize_display_text(&value),
        });
        self
    }

    #[must_use]
    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        let command = command.into();
        self.command = Some(sanitize_display_text(&command));
        self
    }

    #[must_use]
    pub fn with_full_command(mut self, command: impl Into<String>) -> Self {
        let command = command.into();
        self.full_command = Some(sanitize_multiline_display_text(&command));
        self
    }

    #[must_use]
    pub fn with_footnote(mut self, footnote: impl Into<String>) -> Self {
        let footnote = footnote.into();
        self.footnote = Some(sanitize_display_text(&footnote));
        self
    }

    #[must_use]
    pub fn has_choice(&self, id: &str) -> bool {
        self.choices.iter().any(|c| c.id == id)
    }
}

/// A Decision to enqueue, carrying caller metadata `K`, display content, and an optional deadline
/// or front-of-queue timeout.
#[derive(Debug)]
pub struct DecisionRequest<K> {
    pub kind: K,
    pub prompt: DecisionPrompt,
    pub deadline: Option<Instant>,
    /// Relative timeout activated only when this Decision reaches the front of the queue so a
    /// queued card never expires before the person has a chance to see it.
    pub front_timeout: Option<Duration>,
}

impl<K> DecisionRequest<K> {
    #[must_use]
    pub fn new(kind: K, prompt: DecisionPrompt) -> Self {
        Self {
            kind,
            prompt,
            deadline: None,
            front_timeout: None,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self.front_timeout = None;
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.front_timeout = Some(timeout);
        self.deadline = None;
        self
    }
}

/// How a Decision ended. Delivered at most once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionOutcome {
    Chosen(String),
    Expired,
    Cancelled,
}

/// Returned when the Decision queue is already at its pending limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("decision queue is full")]
pub struct QueueFull;

/// Position of the front Decision in the FIFO queue (`1 of N`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuePosition {
    pub current: usize,
    pub total: usize,
}

impl QueuePosition {
    /// Human-readable indicator (`"1 of 3"`) when more than one Decision is waiting.
    #[must_use]
    pub fn label(self) -> Option<String> {
        (self.total > 1).then(|| format!("{} of {}", self.current, self.total))
    }
}

/// Snapshot of the front Decision ready for rendering.
#[derive(Debug)]
pub struct FrontDecision<'a, K> {
    pub id: DecisionId,
    pub kind: &'a K,
    pub prompt: &'a DecisionPrompt,
    pub position: QueuePosition,
    pub remaining_secs: Option<u64>,
}

/// External cancellation handle for a pending Decision.
///
/// Cloning shares the same underlying Decision. Calling [`Self::cancel`] from any thread
/// transitions a pending Decision to [`DecisionOutcome::Cancelled`] and signals the queue's
/// wake channel; if already resolved, it is a no-op and returns `false`.
#[derive(Debug, Clone)]
pub struct DecisionCancel {
    resolution: Arc<Resolution>,
}

impl DecisionCancel {
    pub fn cancel(&self) -> bool {
        self.resolution.cancel(true)
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        !self.resolution.is_pending()
    }
}

/// Async and non-blocking receiver for a Decision's single outcome.
#[derive(Debug)]
#[must_use]
pub struct DecisionWaiter {
    rx: async_channel::Receiver<DecisionOutcome>,
}

impl DecisionWaiter {
    #[cfg(test)]
    #[must_use]
    pub fn try_recv(&self) -> Option<DecisionOutcome> {
        self.rx.try_recv().ok()
    }

    pub async fn wait(self) -> DecisionOutcome {
        self.rx.recv().await.unwrap_or(DecisionOutcome::Cancelled)
    }
}

/// Handle held by the caller that enqueued a Decision.
#[derive(Debug)]
#[must_use]
pub struct PendingDecision {
    id: DecisionId,
    waiter: DecisionWaiter,
    cancel: DecisionCancel,
}

impl PendingDecision {
    #[cfg(test)]
    #[must_use]
    pub fn id(&self) -> DecisionId {
        self.id
    }

    #[cfg(test)]
    #[must_use]
    pub fn cancel_handle(&self) -> DecisionCancel {
        self.cancel.clone()
    }

    #[cfg(test)]
    pub fn cancel(&self) -> bool {
        self.cancel.cancel()
    }

    #[cfg(test)]
    #[must_use]
    pub fn try_outcome(&self) -> Option<DecisionOutcome> {
        self.waiter.try_recv()
    }

    pub fn into_parts(self) -> (DecisionId, DecisionWaiter, DecisionCancel) {
        (self.id, self.waiter, self.cancel)
    }
}

const STATE_PENDING: u8 = 0;
const STATE_ANSWERED: u8 = 1;
const STATE_CANCELLED: u8 = 2;
const STATE_EXPIRED: u8 = 3;

/// Shared single-shot gate ensuring a Decision delivers at most one outcome across threads.
#[derive(Debug)]
struct Resolution {
    state: AtomicU8,
    tx: async_channel::Sender<DecisionOutcome>,
    wake: async_channel::Sender<()>,
}

impl Resolution {
    fn new(
        tx: async_channel::Sender<DecisionOutcome>,
        wake: async_channel::Sender<()>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(STATE_PENDING),
            tx,
            wake,
        })
    }

    fn transition(&self, target: u8, outcome: DecisionOutcome) -> bool {
        if self
            .state
            .compare_exchange(STATE_PENDING, target, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = self.tx.try_send(outcome);
            true
        } else {
            false
        }
    }

    fn answer(&self, choice: String) -> bool {
        self.transition(STATE_ANSWERED, DecisionOutcome::Chosen(choice))
    }

    fn expire(&self) -> bool {
        self.transition(STATE_EXPIRED, DecisionOutcome::Expired)
    }

    fn cancel(&self, notify_wake: bool) -> bool {
        let won = self.transition(STATE_CANCELLED, DecisionOutcome::Cancelled);
        if won && notify_wake {
            let _ = self.wake.try_send(());
        }
        won
    }

    fn is_pending(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_PENDING
    }
}

fn remaining_secs(deadline: Instant, now: Instant) -> Option<u64> {
    let rem = deadline.checked_duration_since(now)?;
    if rem.is_zero() {
        return None;
    }
    Some(rem.as_secs() + u64::from(rem.subsec_nanos() > 0))
}

fn next_countdown_tick(deadline: Instant, now: Instant) -> Instant {
    let Some(rem) = deadline.checked_duration_since(now) else {
        return now;
    };
    if rem.is_zero() {
        return now;
    }
    let step = if rem.subsec_nanos() > 0 {
        Duration::from_nanos(u64::from(rem.subsec_nanos()))
    } else {
        Duration::from_secs(1)
    };
    now.checked_add(step)
        .map_or(deadline, |at| at.min(deadline))
}

#[derive(Debug)]
struct Entry<K> {
    id: DecisionId,
    kind: K,
    prompt: DecisionPrompt,
    front_timeout: Option<Duration>,
    deadline: Option<Instant>,
    resolution: Arc<Resolution>,
}

/// FIFO queue of pending Decisions of heterogeneous kinds `K`.
#[derive(Debug)]
pub struct DecisionQueue<K> {
    entries: VecDeque<Entry<K>>,
    next_id: u64,
    limit: usize,
    wake_tx: async_channel::Sender<()>,
    wake_rx: async_channel::Receiver<()>,
}

impl<K> Default for DecisionQueue<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> DecisionQueue<K> {
    #[must_use]
    pub fn new() -> Self {
        Self::with_limit(MAX_PENDING)
    }

    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        let (wake_tx, wake_rx) = async_channel::bounded(1);
        Self {
            entries: VecDeque::new(),
            next_id: 1,
            limit,
            wake_tx,
            wake_rx,
        }
    }

    /// Enqueues a Decision. If its deadline has already passed at `now`, it resolves immediately
    /// as [`DecisionOutcome::Expired`] without entering the queue.
    ///
    /// # Errors
    /// Returns [`QueueFull`] when the queue already holds its maximum number of pending Decisions.
    pub fn enqueue(
        &mut self,
        request: DecisionRequest<K>,
        now: Instant,
    ) -> Result<PendingDecision, QueueFull> {
        self.sweep(now);
        let already_expired = request.deadline.is_some_and(|d| now >= d);
        if !already_expired && self.entries.len() >= self.limit {
            return Err(QueueFull);
        }
        let id = DecisionId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let (tx, rx) = async_channel::bounded(1);
        let resolution = Resolution::new(tx, self.wake_tx.clone());
        let cancel = DecisionCancel {
            resolution: Arc::clone(&resolution),
        };
        let waiter = DecisionWaiter { rx };
        if already_expired {
            resolution.expire();
            return Ok(PendingDecision { id, waiter, cancel });
        }
        self.entries.push_back(Entry {
            id,
            kind: request.kind,
            prompt: request.prompt,
            front_timeout: request.front_timeout,
            deadline: request.deadline,
            resolution,
        });
        self.activate_front(now);
        Ok(PendingDecision { id, waiter, cancel })
    }

    fn activate_front(&mut self, now: Instant) {
        if let Some(front) = self.entries.front_mut()
            && front.deadline.is_none()
            && let Some(timeout) = front.front_timeout.take()
        {
            front.deadline = Some(now + timeout);
        }
    }

    pub fn front(&mut self, now: Instant) -> Option<FrontDecision<'_, K>> {
        self.sweep(now);
        let total = self.entries.len();
        let front = self.entries.front()?;
        let remaining_secs = front.deadline.and_then(|d| remaining_secs(d, now));
        Some(FrontDecision {
            id: front.id,
            kind: &front.kind,
            prompt: &front.prompt,
            position: QueuePosition { current: 1, total },
            remaining_secs,
        })
    }

    pub fn answer(&mut self, id: DecisionId, choice: &str, now: Instant) -> Option<K> {
        self.sweep(now);
        let front = self.entries.front()?;
        if front.id != id || !front.prompt.has_choice(choice) {
            return None;
        }
        let entry = self.entries.pop_front()?;
        self.activate_front(now);
        if entry.resolution.answer(choice.to_owned()) {
            Some(entry.kind)
        } else {
            None
        }
    }

    /// Expires entries whose deadline has passed and drops entries already resolved externally.
    /// Returns how many entries were removed.
    pub fn sweep(&mut self, now: Instant) -> usize {
        let mut removed = 0;
        self.entries.retain(|entry| {
            if !entry.resolution.is_pending() {
                removed += 1;
                return false;
            }
            if entry.deadline.is_some_and(|d| now >= d) {
                entry.resolution.expire();
                removed += 1;
                return false;
            }
            true
        });
        self.activate_front(now);
        removed
    }

    /// The earliest time the caller should wake to tick the front countdown or expire a queued
    /// Decision. Returns `None` when no pending Decision has a deadline.
    #[must_use]
    pub fn next_wake(&self, now: Instant) -> Option<Instant> {
        let earliest_deadline = self
            .entries
            .iter()
            .filter(|e| e.resolution.is_pending())
            .filter_map(|e| e.deadline)
            .min();
        let front_tick = self
            .entries
            .iter()
            .find(|e| e.resolution.is_pending())
            .and_then(|e| e.deadline)
            .map(|d| next_countdown_tick(d, now));
        match (earliest_deadline, front_tick) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    #[must_use]
    pub fn cancel_wake(&self) -> async_channel::Receiver<()> {
        self.wake_rx.clone()
    }

    pub fn cancel_where(&mut self, mut predicate: impl FnMut(&K) -> bool) -> usize {
        let mut cancelled = 0;
        self.entries.retain(|entry| {
            if !entry.resolution.is_pending() {
                return false;
            }
            if predicate(&entry.kind) {
                if entry.resolution.cancel(false) {
                    cancelled += 1;
                }
                return false;
            }
            true
        });
        cancelled
    }

    #[must_use]
    pub fn has_matching(&self, mut predicate: impl FnMut(&K) -> bool) -> bool {
        self.entries
            .iter()
            .any(|e| e.resolution.is_pending() && predicate(&e.kind))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.resolution.is_pending())
            .count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K> Drop for DecisionQueue<K> {
    fn drop(&mut self) {
        for entry in self.entries.drain(..) {
            entry.resolution.cancel(false);
        }
    }
}

#[cfg(test)]
#[path = "decisions_tests.rs"]
mod tests;
