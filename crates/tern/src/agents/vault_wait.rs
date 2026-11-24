//! Bounded FIFO wait queue for Agent requests waiting on a human to unlock the Vault in tern.
//!
//! Keeps expiry, cancellation, and late-unlock protection pure and unit-testable without GPUI.
//! Once a waiter expires or is cancelled, unlocking the Vault later can never revive it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};

use crate::decisions::{QueueFull, sanitize_display_text};

/// Upper bound on concurrent Agent requests waiting for Vault unlock.
pub const MAX_PENDING_VAULT_WAITS: usize = 32;

/// How a waiting Agent Vault unlock request resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultUnlockOutcome {
    /// The Vault was unlocked before the deadline; the caller may retry execution once.
    Unlocked,
    /// Dismissed by the person, `Pause agents`, disabling MCP, sleep/lock, or Agent disconnect.
    Cancelled,
    /// The wait deadline elapsed before the Vault was unlocked.
    Expired,
}

/// Cloneable cancellation handle paired with a [`PendingVaultUnlock`].
#[derive(Debug, Clone)]
pub struct VaultUnlockCancel {
    tx: watch::Sender<bool>,
    wake: async_channel::Sender<()>,
}

impl VaultUnlockCancel {
    /// Signals that the waiting Agent cancelled or disconnected and wakes the UI sweep task.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
        let _ = self.wake.try_send(());
    }
}

/// Caller-facing handle returned when a request enters [`VaultWaitQueue`].
#[derive(Debug)]
pub struct PendingVaultUnlock {
    rx: oneshot::Receiver<VaultUnlockOutcome>,
    cancel: VaultUnlockCancel,
}

impl PendingVaultUnlock {
    #[must_use]
    pub fn cancel_handle(&self) -> VaultUnlockCancel {
        self.cancel.clone()
    }

    /// Awaits the final [`VaultUnlockOutcome`], returning [`VaultUnlockOutcome::Cancelled`] if
    /// the queue was dropped before answering.
    pub async fn wait(self) -> VaultUnlockOutcome {
        self.rx.await.unwrap_or(VaultUnlockOutcome::Cancelled)
    }
}

/// Read-only snapshot of the front waiting Agent request for the in-window `Unlock Vault` card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultWaitView {
    pub caller: String,
    pub host: String,
    pub remaining_secs: u64,
    pub waiting_count: usize,
}

#[derive(Debug)]
struct VaultWaitEntry {
    caller: String,
    host: String,
    deadline: Instant,
    tx: oneshot::Sender<VaultUnlockOutcome>,
    cancel_rx: watch::Receiver<bool>,
}

/// Bounded in-memory queue of Agent requests waiting for the Vault to unlock.
#[derive(Debug)]
pub struct VaultWaitQueue {
    entries: VecDeque<VaultWaitEntry>,
    cancel_wake_tx: async_channel::Sender<()>,
    cancel_wake_rx: async_channel::Receiver<()>,
}

impl Default for VaultWaitQueue {
    fn default() -> Self {
        let (cancel_wake_tx, cancel_wake_rx) = async_channel::bounded(1);
        Self {
            entries: VecDeque::new(),
            cancel_wake_tx,
            cancel_wake_rx,
        }
    }
}

impl VaultWaitQueue {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Executor-agnostic wake channel notified whenever any waiter's [`VaultUnlockCancel`] fires.
    #[must_use]
    pub fn cancel_wake(&self) -> async_channel::Receiver<()> {
        self.cancel_wake_rx.clone()
    }

    /// Enqueues a new bounded Vault unlock waiter.
    ///
    /// # Errors
    /// Returns [`QueueFull`] when [`MAX_PENDING_VAULT_WAITS`] requests are already waiting.
    pub fn enqueue(
        &mut self,
        caller: impl Into<String>,
        host: impl Into<String>,
        deadline: Instant,
        now: Instant,
    ) -> Result<PendingVaultUnlock, QueueFull> {
        self.sweep(now);
        if self.entries.len() >= MAX_PENDING_VAULT_WAITS {
            return Err(QueueFull);
        }
        let (tx, rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.entries.push_back(VaultWaitEntry {
            caller: sanitize_display_text(&caller.into()),
            host: sanitize_display_text(&host.into()),
            deadline,
            tx,
            cancel_rx,
        });
        Ok(PendingVaultUnlock {
            rx,
            cancel: VaultUnlockCancel {
                tx: cancel_tx,
                wake: self.cancel_wake_tx.clone(),
            },
        })
    }

    /// Sweeps expired or externally cancelled waiters, returning how many were removed.
    pub fn sweep(&mut self, now: Instant) -> usize {
        let before = self.entries.len();
        let mut kept = VecDeque::with_capacity(self.entries.len());
        while let Some(entry) = self.entries.pop_front() {
            if *entry.cancel_rx.borrow() {
                let _ = entry.tx.send(VaultUnlockOutcome::Cancelled);
            } else if entry.deadline <= now {
                let _ = entry.tx.send(VaultUnlockOutcome::Expired);
            } else {
                kept.push_back(entry);
            }
        }
        self.entries = kept;
        before - self.entries.len()
    }

    /// Returns the front non-expired waiter for rendering the contextual `Unlock Vault` card.
    pub fn front(&mut self, now: Instant) -> Option<VaultWaitView> {
        self.sweep(now);
        let front = self.entries.front()?;
        let remaining = front.deadline.saturating_duration_since(now);
        let remaining_secs = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
        Some(VaultWaitView {
            caller: front.caller.clone(),
            host: front.host.clone(),
            remaining_secs,
            waiting_count: self.entries.len(),
        })
    }

    /// Sweeps expired/cancelled entries first, then wakes every remaining waiter with
    /// [`VaultUnlockOutcome::Unlocked`]. Returns how many waiters were woken as unlocked.
    pub fn resolve_unlocked(&mut self, now: Instant) -> usize {
        self.sweep(now);
        let mut woken = 0;
        while let Some(entry) = self.entries.pop_front() {
            if entry.tx.send(VaultUnlockOutcome::Unlocked).is_ok() {
                woken += 1;
            }
        }
        woken
    }

    /// Cancels every waiting request immediately (e.g. Cancel click, Pause agents, sleep/lock).
    pub fn cancel_all(&mut self) -> usize {
        let count = self.entries.len();
        while let Some(entry) = self.entries.pop_front() {
            let _ = entry.tx.send(VaultUnlockOutcome::Cancelled);
        }
        count
    }

    /// Returns the earliest deadline among currently queued waiters.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries.iter().map(|e| e.deadline).min()
    }

    /// Recommended timer delay until the next countdown tick or expiry deadline.
    #[must_use]
    pub fn next_wake_delay(&self, now: Instant) -> Option<Duration> {
        let earliest = self.next_deadline()?;
        let until_deadline = earliest.saturating_duration_since(now);
        Some(until_deadline.min(Duration::from_secs(1)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn expired_waiter_is_never_revived_by_later_vault_unlock() {
        let now = Instant::now();
        let mut q = VaultWaitQueue::default();
        let pending = q
            .enqueue("Claude Code", "alpha", now + Duration::from_secs(30), now)
            .unwrap();
        let view = q.front(now).unwrap();
        assert_eq!(view.caller, "Claude Code");
        assert_eq!(view.host, "alpha");
        assert_eq!(view.remaining_secs, 30);
        assert_eq!(view.waiting_count, 1);

        // Unlock arrives AFTER deadline: expired entry must receive Expired, NOT Unlocked.
        let woken = q.resolve_unlocked(now + Duration::from_secs(31));
        assert_eq!(woken, 0);
        assert!(q.is_empty());
        assert_eq!(pending.wait().await, VaultUnlockOutcome::Expired);
    }

    #[tokio::test]
    async fn unlock_before_deadline_wakes_all_valid_waiters_and_clears_queue() {
        let now = Instant::now();
        let mut q = VaultWaitQueue::default();
        let w1 = q
            .enqueue("Agent A", "alpha", now + Duration::from_secs(60), now)
            .unwrap();
        let w2 = q
            .enqueue("Agent B", "beta", now + Duration::from_secs(60), now)
            .unwrap();
        assert_eq!(q.front(now).unwrap().waiting_count, 2);

        let woken = q.resolve_unlocked(now + Duration::from_secs(5));
        assert_eq!(woken, 2);
        assert!(q.is_empty());
        assert_eq!(w1.wait().await, VaultUnlockOutcome::Unlocked);
        assert_eq!(w2.wait().await, VaultUnlockOutcome::Unlocked);
    }

    #[tokio::test]
    async fn per_agent_cancel_removes_only_cancelled_entry_and_cancel_all_clears_rest() {
        let now = Instant::now();
        let mut q = VaultWaitQueue::default();
        let wake = q.cancel_wake();
        let w1 = q
            .enqueue("Agent A", "alpha", now + Duration::from_secs(60), now)
            .unwrap();
        let w2 = q
            .enqueue("Agent B", "beta", now + Duration::from_secs(60), now)
            .unwrap();

        w1.cancel_handle().cancel();
        assert!(wake.try_recv().is_ok());
        assert_eq!(q.sweep(now), 1);
        assert_eq!(w1.wait().await, VaultUnlockOutcome::Cancelled);
        let front = q.front(now).unwrap();
        assert_eq!(front.caller, "Agent B");
        assert_eq!(front.waiting_count, 1);

        assert_eq!(q.cancel_all(), 1);
        assert!(q.is_empty());
        assert_eq!(w2.wait().await, VaultUnlockOutcome::Cancelled);
    }

    #[test]
    fn queue_enforces_bounded_capacity() {
        let now = Instant::now();
        let mut q = VaultWaitQueue::default();
        let mut held = Vec::new();
        for i in 0..MAX_PENDING_VAULT_WAITS {
            held.push(
                q.enqueue(
                    format!("Agent {i}"),
                    "alpha",
                    now + Duration::from_secs(60),
                    now,
                )
                .unwrap(),
            );
        }
        assert!(matches!(
            q.enqueue("Overflow", "alpha", now + Duration::from_secs(60), now),
            Err(QueueFull)
        ));
    }
}
