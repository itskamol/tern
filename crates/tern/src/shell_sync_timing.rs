//! When automatic sync runs. Every rule is decided here from the time and the facts handed in,
//! so none of them needs a clock, a vault or a remote to be checked.

use std::time::{Duration, Instant};

/// What one look at the synced files found.
#[derive(Debug)]
pub(super) struct Facts<'a> {
    pub(super) now: Instant,
    /// Automatic sync is on and has somewhere to sync to.
    pub(super) enabled: bool,
    /// The fingerprint of the files now, and at the last sync.
    pub(super) local: &'a str,
    pub(super) last: Option<&'a str>,
    /// A sync is running, or one is waiting for the person's answer.
    pub(super) busy: bool,
    pub(super) vault_open: bool,
}

/// What to do after a look.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next {
    Run,
    Wait,
    /// Wait, and say why in Settings → Sync.
    WaitSaying(&'static str),
}

/// Edits are batched this long, so typing a host name is one sync, not twenty.
const DEBOUNCE: Duration = Duration::from_secs(5);
/// After a failed automatic sync, the same data is not retried for this long (offline).
const BACKOFF: Duration = Duration::from_secs(300);
/// Bringing the window to the front looks at the remote, but not more than once a minute.
const ACTIVATE_EVERY: Duration = Duration::from_secs(60);
/// While tern stays open the remote is looked at this often, for changes from other Macs.
const PERIODIC_EVERY: Duration = Duration::from_secs(300);

/// Lets an event through at most once per `every`; only an allowed event restarts the clock.
#[derive(Debug)]
struct Throttle {
    every: Duration,
    last: Option<Instant>,
}

impl Throttle {
    fn new(every: Duration) -> Self {
        Self { every, last: None }
    }

    fn allow(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < self.every)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// How a sync ended, as far as its timing goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum End {
    Worked,
    /// An automatic sync failed: the data it ran on is not retried for a while.
    Failed,
    /// It ended some other way (a question for the person, a sync they started that failed).
    Other,
}

/// What automatic sync remembers between looks.
#[derive(Debug)]
pub(super) struct Timing {
    periodic: Throttle,
    /// The remote is to be looked at whether or not anything changed here: at launch, every
    /// [`PERIODIC_EVERY`] after, and when the window comes back to the front.
    remote_due: bool,
    /// The changed files the debounce is waiting on, and when it may run.
    seen: Option<String>,
    deadline: Option<Instant>,
    /// The data the last automatic sync failed on, and when.
    failed: Option<(String, Instant)>,
    on_activate: Throttle,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            periodic: Throttle::new(PERIODIC_EVERY),
            remote_due: false,
            seen: None,
            deadline: None,
            failed: None,
            on_activate: Throttle::new(ACTIVATE_EVERY),
        }
    }
}

impl Timing {
    /// One look at the synced files: whether a sync is due now.
    pub(super) fn look(&mut self, facts: &Facts<'_>) -> Next {
        if !facts.enabled {
            return Next::Wait;
        }
        let now = facts.now;
        if self.periodic.allow(now) {
            self.remote_due = true;
        }
        // One sync at a time; a change meanwhile shows up as a difference afterwards.
        if facts.busy {
            return Next::Wait;
        }
        let dirty = facts.last != Some(facts.local);
        if !dirty && !self.remote_due {
            self.seen = None;
            self.deadline = None;
            return Next::Wait;
        }
        if dirty && self.seen.as_deref() != Some(facts.local) {
            self.seen = Some(facts.local.to_owned());
            self.deadline = Some(now + DEBOUNCE);
        }
        if !(self.remote_due || self.deadline.is_some_and(|d| now >= d)) {
            return Next::Wait;
        }
        if let Some((data, at)) = &self.failed
            && data == facts.local
            && now.duration_since(*at) < BACKOFF
        {
            return Next::Wait;
        }
        if !facts.vault_open {
            // Never ask for a passphrase from the background.
            return Next::WaitSaying("Automatic sync is waiting for the vault to be unlocked");
        }
        self.remote_due = false;
        self.deadline = None;
        self.seen = Some(facts.local.to_owned());
        Next::Run
    }

    /// A sync ended, whoever started it.
    pub(super) fn finished(&mut self, end: End, now: Instant) {
        let ran_on = self.seen.take();
        match end {
            End::Worked => self.failed = None,
            End::Failed => {
                if let Some(data) = ran_on {
                    self.failed = Some((data, now));
                }
            }
            End::Other => {}
        }
    }

    /// The window came to the front, perhaps after work on another Mac. True when that asks
    /// for a look at the remote.
    #[must_use]
    pub(super) fn window_came_forward(&mut self, now: Instant) -> bool {
        let due = self.on_activate.allow(now);
        self.remote_due |= due;
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Automatic sync on, the vault open, nothing running, files as the last sync left them.
    fn settled(now: Instant) -> Facts<'static> {
        Facts {
            now,
            enabled: true,
            local: "a",
            last: Some("a"),
            busy: false,
            vault_open: true,
        }
    }

    #[test]
    fn the_first_look_after_launch_syncs_even_with_nothing_changed() {
        let mut timing = Timing::default();
        assert_eq!(timing.look(&settled(Instant::now())), Next::Run);
    }

    #[test]
    fn with_nothing_changed_the_remote_is_looked_at_again_only_after_five_minutes() {
        let t0 = Instant::now();
        let mut timing = Timing::default();
        assert_eq!(timing.look(&settled(t0)), Next::Run);
        assert_eq!(timing.look(&settled(t0 + secs(2))), Next::Wait);
        assert_eq!(timing.look(&settled(t0 + secs(299))), Next::Wait);
        assert_eq!(timing.look(&settled(t0 + secs(300))), Next::Run);
    }

    #[test]
    fn with_nothing_changed_and_no_look_at_the_remote_due_nothing_runs() {
        let t0 = Instant::now();
        let mut timing = Timing::default();
        assert_eq!(timing.look(&settled(t0)), Next::Run);
        timing.finished(End::Worked, t0 + secs(1));
        for at in [2, 4, 60, 298] {
            assert_eq!(timing.look(&settled(t0 + secs(at))), Next::Wait);
        }
    }

    #[test]
    fn one_change_syncs_five_seconds_later_and_not_a_second_sooner() {
        let t0 = Instant::now();
        let mut timing = Timing::default();
        assert_eq!(timing.look(&settled(t0)), Next::Run);
        timing.finished(End::Worked, t0 + secs(1));
        let edited = |now| Facts {
            local: "b",
            ..settled(now)
        };
        assert_eq!(timing.look(&edited(t0 + secs(10))), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(14))), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(15))), Next::Run);
    }

    #[test]
    fn a_change_waits_five_seconds_after_the_last_edit_then_syncs_once() {
        let t0 = Instant::now();
        let mut timing = Timing::default();
        assert_eq!(timing.look(&settled(t0)), Next::Run);
        let edited = |now, local| Facts {
            local,
            ..settled(now)
        };
        assert_eq!(timing.look(&edited(t0 + secs(10), "b")), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(14), "b")), Next::Wait);
        // A second edit restarts the wait: 15 s is five after the first, one after this.
        assert_eq!(timing.look(&edited(t0 + secs(14), "c")), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(15), "c")), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(18), "c")), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(19), "c")), Next::Run);
        assert_eq!(timing.look(&edited(t0 + secs(21), "c")), Next::Wait);
    }

    #[test]
    fn nothing_runs_on_its_own_when_automatic_sync_is_off_or_has_nowhere_to_sync_to() {
        let t0 = Instant::now();
        let mut timing = Timing::default();
        let off = |now, local| Facts {
            enabled: false,
            local,
            ..settled(now)
        };
        assert_eq!(timing.look(&off(t0, "a")), Next::Wait);
        assert_eq!(timing.look(&off(t0 + secs(2), "b")), Next::Wait);
        assert_eq!(timing.look(&off(t0 + secs(600), "b")), Next::Wait);
    }

    #[test]
    fn nothing_starts_while_a_sync_runs_or_waits_for_an_answer_and_the_change_is_picked_up_after() {
        let t0 = Instant::now();
        let mut timing = Timing::default();
        assert_eq!(timing.look(&settled(t0)), Next::Run);
        let during = |now| Facts {
            local: "b",
            busy: true,
            ..settled(now)
        };
        assert_eq!(timing.look(&during(t0 + secs(2))), Next::Wait);
        assert_eq!(timing.look(&during(t0 + secs(60))), Next::Wait);
        assert_eq!(timing.look(&during(t0 + secs(400))), Next::Wait);
        let after = Facts {
            local: "b",
            ..settled(t0 + secs(402))
        };
        assert_eq!(timing.look(&after), Next::Run);
    }

    #[test]
    fn a_locked_vault_holds_the_sync_and_says_so_until_it_is_opened() {
        let t0 = Instant::now();
        let mut timing = Timing::default();
        let locked = |now| Facts {
            vault_open: false,
            ..settled(now)
        };
        let waiting = Next::WaitSaying("Automatic sync is waiting for the vault to be unlocked");
        assert_eq!(timing.look(&locked(t0)), waiting);
        assert_eq!(timing.look(&locked(t0 + secs(2))), waiting);
        assert_eq!(timing.look(&settled(t0 + secs(4))), Next::Run);
    }

    #[test]
    fn data_that_failed_is_not_retried_for_five_minutes_but_changed_data_is() {
        let t0 = Instant::now();
        let edited = |now, local| Facts {
            local,
            ..settled(now)
        };
        let mut timing = Timing::default();
        assert_eq!(timing.look(&edited(t0, "b")), Next::Run);
        timing.finished(End::Failed, t0 + secs(1));
        // The debounce comes round again and again; the back-off holds it.
        assert_eq!(timing.look(&edited(t0 + secs(2), "b")), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(7), "b")), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(300), "b")), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(301), "b")), Next::Run);

        let mut timing = Timing::default();
        assert_eq!(timing.look(&edited(t0, "b")), Next::Run);
        timing.finished(End::Failed, t0 + secs(1));
        assert_eq!(timing.look(&edited(t0 + secs(2), "c")), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(7), "c")), Next::Run);
    }

    #[test]
    fn a_sync_that_worked_forgets_the_earlier_failure() {
        let t0 = Instant::now();
        let edited = |now| Facts {
            local: "b",
            ..settled(now)
        };
        let mut timing = Timing::default();
        assert_eq!(timing.look(&edited(t0)), Next::Run);
        timing.finished(End::Failed, t0 + secs(1));
        timing.finished(End::Worked, t0 + secs(2));
        assert_eq!(timing.look(&edited(t0 + secs(3))), Next::Wait);
        assert_eq!(timing.look(&edited(t0 + secs(8))), Next::Run);
    }

    #[test]
    fn coming_back_to_the_window_looks_at_the_remote_at_most_once_a_minute() {
        let t0 = Instant::now();
        let mut timing = Timing::default();
        assert_eq!(timing.look(&settled(t0)), Next::Run);
        timing.finished(End::Worked, t0 + secs(1));
        assert!(timing.window_came_forward(t0 + secs(10)));
        assert_eq!(timing.look(&settled(t0 + secs(11))), Next::Run);
        timing.finished(End::Worked, t0 + secs(12));
        assert!(!timing.window_came_forward(t0 + secs(69)));
        assert_eq!(timing.look(&settled(t0 + secs(70))), Next::Wait);
        assert!(timing.window_came_forward(t0 + secs(71)));
        assert_eq!(timing.look(&settled(t0 + secs(72))), Next::Run);
    }

    #[test]
    fn a_throttle_counts_from_the_event_it_let_through_not_from_a_refused_one() {
        let t0 = Instant::now();
        let mut gate = Throttle::new(secs(60));
        assert!(gate.allow(t0), "the first event passes");
        assert!(!gate.allow(t0 + secs(1)));
        assert!(!gate.allow(t0 + secs(59)));
        assert!(gate.allow(t0 + secs(61)));
        assert!(!gate.allow(t0 + secs(100)), "the clock restarted at 61");
        assert!(gate.allow(t0 + secs(121)));
    }
}
