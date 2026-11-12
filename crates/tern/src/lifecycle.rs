//! Where a session's connection is, and what happens next: whether a dial opens the vault
//! first, when a dropped connection dials again by itself and how long it waits, and when it
//! must not. Pure decisions only; `session.rs` runs them (the timer, the dial, the terminal
//! text), so every transition is tested without a window or a server.

use std::time::{Duration, Instant};

use tern_ssh::Disconnect;

use crate::indicator::Dot;

/// What the rest of the app sees of a session's connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Restored from the last run and not dialled yet: Enter connects.
    Idle,
    Connecting,
    Connected,
    Closed,
}

impl Status {
    /// Nothing is running; Enter starts it.
    pub fn is_dormant(&self) -> bool {
        matches!(self, Status::Idle | Status::Closed)
    }
}

/// How a connection ended for good, until the person dials again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum End {
    /// Closed, exited or failed: Enter reconnects.
    Ended,
    /// A host key changed: the target's, or with `hop` set, that jump host's. Never retried by
    /// itself: it would only fail again, and a changed key is also what an attack looks like.
    HostKeyChanged { hop: Option<(String, u16)> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Opening the vault, asking about a ProxyCommand, dialling or logging in.
    Connecting,
    Connected,
    /// Dropped; dials again by itself at `at`.
    Retrying {
        reason: &'static str,
        at: Instant,
    },
    Closed(End),
}

/// What a dial does first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// The connection names keys in a locked vault: open it, then dial.
    UnlockFirst,
    Dial,
}

/// What follows a connection that closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Count down and dial again at this time.
    RetryAt(Instant),
    Stop(End),
}

/// One session's connection over its life: the phase it is in, whether it ever reached the
/// shell (only then does a drop retry by itself), and the backoff between tries.
#[derive(Debug)]
pub struct Lifecycle {
    phase: Phase,
    ever_connected: bool,
    backoff: Backoff,
}

impl Lifecycle {
    /// `idle` for a tab restored from the last run, which waits for Enter.
    pub fn new(idle: bool) -> Self {
        Self {
            phase: if idle { Phase::Idle } else { Phase::Connecting },
            ever_connected: false,
            backoff: Backoff::default(),
        }
    }

    pub fn status(&self) -> Status {
        match self.phase {
            Phase::Idle => Status::Idle,
            Phase::Connecting => Status::Connecting,
            Phase::Connected => Status::Connected,
            Phase::Retrying { .. } | Phase::Closed(_) => Status::Closed,
        }
    }

    /// The tab's dot. `asking` is whether a question in the terminal waits for the person.
    pub fn dot(&self, asking: bool) -> Dot {
        Dot::of(&self.status(), asking)
    }

    /// The countdown's cause and when it dials, while one runs.
    pub fn retrying(&self) -> Option<(&'static str, Instant)> {
        match self.phase {
            Phase::Retrying { reason, at } => Some((reason, at)),
            _ => None,
        }
    }

    /// A dial is asked for: a tab opening, Enter, or a countdown coming due. `vault_first` is
    /// true when the connection names keys in a locked vault the person has not skipped.
    pub fn start(&mut self, vault_first: bool) -> Start {
        self.phase = Phase::Connecting;
        if vault_first {
            Start::UnlockFirst
        } else {
            Start::Dial
        }
    }

    pub fn connected(&mut self) {
        self.phase = Phase::Connected;
        self.ever_connected = true;
        self.backoff.reset();
    }

    /// The connection closed at `now` for `reason`. Only a link that broke is retried, and only
    /// for a host that answered before; a changed host key is never retried.
    pub fn closed(&mut self, reason: &Disconnect, now: Instant) -> Next {
        if should_retry(reason, self.ever_connected) {
            let at = now + self.backoff.next_delay();
            self.phase = Phase::Retrying {
                reason: label(reason),
                at,
            };
            return Next::RetryAt(at);
        }
        let end = match reason {
            Disconnect::HostKeyChanged { hop } => End::HostKeyChanged { hop: hop.clone() },
            _ => End::Ended,
        };
        self.phase = Phase::Closed(end.clone());
        Next::Stop(end)
    }

    /// The person stopped it before it connected: Esc on a countdown, or no to a ProxyCommand.
    pub fn stop(&mut self) {
        self.phase = Phase::Closed(End::Ended);
    }
}

/// Waits between tries; the last one repeats until the user stops it.
const DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// Only a link that broke is retried, and only for a host that has answered before: a first
/// connect that never worked (wrong address, firewall) is a setup problem, so it stops at the
/// message and waits for Enter instead of dialling a dead address forever.
fn should_retry(reason: &Disconnect, ever_connected: bool) -> bool {
    reason.is_network() && ever_connected
}

/// Escalating delays that start over after a connection succeeds.
#[derive(Debug, Default)]
struct Backoff {
    tries: usize,
}

impl Backoff {
    fn next_delay(&mut self) -> Duration {
        let d = DELAYS[self.tries.min(DELAYS.len() - 1)];
        self.tries += 1;
        d
    }

    fn reset(&mut self) {
        self.tries = 0;
    }
}

/// What a keystroke means while a retry is counting down.
#[derive(Debug, PartialEq, Eq)]
pub enum Key {
    RetryNow,
    Stop,
    Other,
}

pub fn key(input: &[u8]) -> Key {
    if input.contains(&b'\r') {
        Key::RetryNow
    } else if input == b"\x1b" {
        Key::Stop
    } else {
        Key::Other
    }
}

/// A short cause for the notice line.
fn label(reason: &Disconnect) -> &'static str {
    match reason {
        Disconnect::Timeout => "timeout",
        Disconnect::Reset => "connection reset",
        Disconnect::ServerClosed => "closed by server",
        Disconnect::Unreachable => "unreachable",
        _ => "closed",
    }
}

/// The countdown line, redrawn in place every second.
pub fn notice(label: &str, secs: u64) -> String {
    format!(
        "Connection lost ({label}). Reconnecting in {secs}s\u{2026} (Enter to retry now, Esc to stop)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_network_causes_of_a_session_that_worked_retry() {
        let network = [
            Disconnect::Timeout,
            Disconnect::Reset,
            Disconnect::ServerClosed,
            Disconnect::Unreachable,
        ];
        let final_ = [
            Disconnect::Exited(0),
            Disconnect::Exited(255),
            Disconnect::Signaled("KILL".into()),
            Disconnect::Local,
            Disconnect::HostKeyChanged { hop: None },
            Disconnect::Failed,
        ];
        for r in &network {
            assert!(should_retry(r, true), "{r:?}");
            assert!(!should_retry(r, false), "{r:?} before ever connecting");
        }
        for r in &final_ {
            assert!(!should_retry(r, true), "{r:?}");
        }
    }

    #[test]
    fn delays_escalate_cap_and_reset() {
        let mut b = Backoff::default();
        let secs: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(secs, [1, 2, 5, 10, 30, 30, 30, 30]);
        b.reset();
        assert_eq!(b.next_delay().as_secs(), 1);
        assert_eq!(b.next_delay().as_secs(), 2);
    }

    #[test]
    fn keys_during_a_countdown() {
        assert_eq!(key(b"\r"), Key::RetryNow);
        assert_eq!(key(b"\x1b"), Key::Stop);
        assert_eq!(key(b"\x1b[A"), Key::Other);
        assert_eq!(key(b"q"), Key::Other);
    }

    #[test]
    fn notice_reads_as_specified() {
        assert_eq!(
            notice("timeout", 5),
            "Connection lost (timeout). Reconnecting in 5s\u{2026} (Enter to retry now, Esc to stop)"
        );
    }

    #[test]
    fn disconnect_reasons_map_to_notice_labels() {
        assert_eq!(label(&Disconnect::Timeout), "timeout");
        assert_eq!(label(&Disconnect::Reset), "connection reset");
        assert_eq!(label(&Disconnect::ServerClosed), "closed by server");
        assert_eq!(label(&Disconnect::Unreachable), "unreachable");
        assert_eq!(label(&Disconnect::Exited(0)), "closed");
        assert_eq!(label(&Disconnect::Local), "closed");
        assert_eq!(label(&Disconnect::Failed), "closed");
    }

    const SEC: Duration = Duration::from_secs(1);

    #[test]
    fn a_drop_after_login_counts_down_with_growing_waits_and_a_login_starts_them_over() {
        let t0 = Instant::now();
        let mut life = Lifecycle::new(false);
        assert_eq!(life.start(false), Start::Dial);
        assert_eq!(life.status(), Status::Connecting);
        life.connected();
        assert_eq!(life.status(), Status::Connected);
        assert_eq!(life.closed(&Disconnect::Reset, t0), Next::RetryAt(t0 + SEC));
        assert_eq!(life.retrying(), Some(("connection reset", t0 + SEC)));
        assert_eq!(life.status(), Status::Closed, "a countdown reads as closed");
        assert!(life.status().is_dormant());
        // The retry dials and the network is still down: the wait grows.
        life.start(false);
        assert_eq!(life.retrying(), None);
        assert_eq!(
            life.closed(&Disconnect::Unreachable, t0),
            Next::RetryAt(t0 + 2 * SEC)
        );
        life.start(false);
        life.connected();
        assert_eq!(
            life.closed(&Disconnect::Timeout, t0),
            Next::RetryAt(t0 + SEC)
        );
    }

    #[test]
    fn a_host_that_never_answered_is_not_dialled_again_by_itself() {
        let mut life = Lifecycle::new(false);
        life.start(false);
        assert_eq!(
            life.closed(&Disconnect::Unreachable, Instant::now()),
            Next::Stop(End::Ended)
        );
        assert_eq!(life.retrying(), None);
        assert_eq!(life.status(), Status::Closed);
    }

    #[test]
    fn a_changed_host_key_stops_even_a_session_that_worked() {
        let mut life = Lifecycle::new(false);
        life.start(false);
        life.connected();
        assert_eq!(
            life.closed(
                &Disconnect::HostKeyChanged {
                    hop: Some(("bastion".into(), 22))
                },
                Instant::now()
            ),
            Next::Stop(End::HostKeyChanged {
                hop: Some(("bastion".into(), 22))
            })
        );
        assert_eq!(life.retrying(), None);
        assert_eq!(life.status(), Status::Closed);
        // Enter dials again.
        assert_eq!(life.start(false), Start::Dial);
        assert_eq!(life.status(), Status::Connecting);
    }

    #[test]
    fn a_locked_vault_with_the_keys_opens_before_the_dial() {
        let mut life = Lifecycle::new(true);
        assert_eq!(life.status(), Status::Idle);
        assert_eq!(life.start(true), Start::UnlockFirst);
        assert_eq!(life.status(), Status::Connecting);
    }

    #[test]
    fn stopping_a_countdown_leaves_it_closed_for_enter() {
        let mut life = Lifecycle::new(false);
        life.start(false);
        life.connected();
        life.closed(&Disconnect::Reset, Instant::now());
        life.stop();
        assert_eq!(life.retrying(), None);
        assert_eq!(life.status(), Status::Closed);
    }

    #[test]
    fn the_dot_waits_for_the_person_only_while_connecting() {
        let mut life = Lifecycle::new(false);
        assert_eq!(life.dot(true), Dot::WaitingForInput);
        assert_eq!(life.dot(false), Dot::Connecting);
        life.connected();
        assert_eq!(life.dot(true), Dot::Connected);
        life.closed(&Disconnect::Reset, Instant::now());
        assert_eq!(life.dot(false), Dot::Closed);
    }
}
