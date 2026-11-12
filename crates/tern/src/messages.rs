//! Where a message shows (ADR 0002). An Activity needs nothing from the person: it goes to the
//! status line when one is showing, else next to the titlebar's Activity button. An Alert is a
//! failure or a message with an action: the snackbar at the top right. Both are kept in the
//! Activity list. Nothing here knows about a window, so the routing is tested on its own.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

/// How long an Activity stays in its place before it lives only in the list: seed-design's
/// snackbar time, so a quiet message is on screen as long as a loud one.
pub const SHOWN_FOR: Duration = Duration::from_secs(4);
/// The list keeps this many, oldest dropped first, so it stays bounded however long tern runs:
/// tern's own cap, a long day of messages at well under 100 KB.
pub const KEPT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Activity,
    Alert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    StatusLine,
    Titlebar,
    Snackbar,
}

/// Where a message of `kind` shows. A message with an action is an Alert whatever it was
/// called, since an Activity has nothing to click.
#[must_use]
pub fn place(kind: Kind, has_action: bool, status_line: bool) -> Place {
    match (kind, has_action) {
        (Kind::Activity, false) if status_line => Place::StatusLine,
        (Kind::Activity, false) => Place::Titlebar,
        _ => Place::Snackbar,
    }
}

/// One message in the Activity list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub text: String,
    /// Something did not work: the list marks it.
    pub failed: bool,
    pub at: SystemTime,
}

/// The Activity list and the Activity on screen now.
#[derive(Debug, Default)]
pub struct Messages {
    list: VecDeque<Entry>,
    latest: Option<(String, Instant)>,
    /// Work in progress, oldest first; the newest is the one shown.
    progress: Vec<(&'static str, String)>,
}

impl Messages {
    /// Keeps a message and returns the kind it shows as. `now` times the Activity on screen;
    /// `at` is the wall clock the list shows.
    pub fn raise(
        &mut self,
        kind: Kind,
        text: impl Into<String>,
        has_action: bool,
        failed: bool,
        at: SystemTime,
        now: Instant,
    ) -> Kind {
        let kind = if has_action { Kind::Alert } else { kind };
        let text = text.into();
        if kind == Kind::Activity {
            self.latest = Some((text.clone(), now));
        }
        if self.list.len() == KEPT {
            self.list.pop_front();
        }
        self.list.push_back(Entry {
            kind,
            text,
            failed,
            at,
        });
        kind
    }

    /// A message that follows work in progress ("Downloading\u{2026} 40%"): shown until
    /// [`Self::end_progress`], its text replaced in place, never added to the list. `key`
    /// names the work, so two can run at once.
    pub fn set_progress(&mut self, key: &'static str, text: impl Into<String>) {
        let text = text.into();
        match self.progress.iter_mut().find(|(k, _)| *k == key) {
            Some((_, shown)) => *shown = text,
            None => self.progress.push((key, text)),
        }
    }

    pub fn end_progress(&mut self, key: &'static str) {
        self.progress.retain(|(k, _)| *k != key);
    }

    /// The list, oldest first.
    pub fn list(&self) -> impl DoubleEndedIterator<Item = &Entry> + ExactSizeIterator {
        self.list.iter()
    }

    /// The Activity on screen at `now` and where it goes: one raised in the last [`SHOWN_FOR`],
    /// else the newest work in progress.
    #[must_use]
    pub fn showing(&self, now: Instant, status_line: bool) -> Option<(Place, &str)> {
        let fresh = self
            .latest
            .as_ref()
            .filter(|(_, raised)| now.saturating_duration_since(*raised) < SHOWN_FOR)
            .map(|(text, _)| text.as_str());
        let text = fresh.or_else(|| self.progress.last().map(|(_, text)| text.as_str()))?;
        Some((place(Kind::Activity, false, status_line), text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_activity_goes_to_the_status_line_when_one_shows_else_the_titlebar() {
        assert_eq!(place(Kind::Activity, false, true), Place::StatusLine);
        assert_eq!(place(Kind::Activity, false, false), Place::Titlebar);
    }

    #[test]
    fn an_alert_is_the_snackbar_with_or_without_a_status_line() {
        assert_eq!(place(Kind::Alert, false, true), Place::Snackbar);
        assert_eq!(place(Kind::Alert, false, false), Place::Snackbar);
        assert_eq!(place(Kind::Alert, true, true), Place::Snackbar);
    }

    #[test]
    fn a_message_with_an_action_is_an_alert_even_when_called_activity() {
        assert_eq!(place(Kind::Activity, true, true), Place::Snackbar);
        assert_eq!(place(Kind::Activity, true, false), Place::Snackbar);
        let mut m = Messages::default();
        let now = Instant::now();
        let shown = m.raise(
            Kind::Activity,
            "Removed web",
            true,
            false,
            SystemTime::now(),
            now,
        );
        assert_eq!(shown, Kind::Alert);
        assert_eq!(m.list().next().map(|e| e.kind), Some(Kind::Alert));
        assert_eq!(m.showing(now, true), None);
    }

    #[test]
    fn the_list_keeps_activity_and_alerts_in_the_order_they_came() {
        let mut m = Messages::default();
        let (now, at) = (Instant::now(), SystemTime::now());
        m.raise(Kind::Activity, "Copied 3 files", false, false, at, now);
        m.raise(Kind::Alert, "Sync failed", false, true, at, now);
        m.raise(Kind::Activity, "Synced", false, false, at, now);
        let seen: Vec<_> = m
            .list()
            .map(|e| (e.kind, e.text.as_str(), e.failed))
            .collect();
        assert_eq!(
            seen,
            [
                (Kind::Activity, "Copied 3 files", false),
                (Kind::Alert, "Sync failed", true),
                (Kind::Activity, "Synced", false),
            ]
        );
    }

    #[test]
    fn the_list_drops_the_oldest_past_its_bound() {
        let mut m = Messages::default();
        let (now, at) = (Instant::now(), SystemTime::now());
        for i in 0..KEPT + 3 {
            m.raise(Kind::Activity, format!("m{i}"), false, false, at, now);
        }
        assert_eq!(m.list().len(), KEPT);
        assert_eq!(m.list().next().map(|e| e.text.as_str()), Some("m3"));
        assert_eq!(
            m.list().next_back().map(|e| e.text.clone()),
            Some(format!("m{}", KEPT + 2))
        );
    }

    #[test]
    fn an_activity_shows_for_four_seconds_where_there_is_room_now() {
        let mut m = Messages::default();
        let t0 = Instant::now();
        m.raise(
            Kind::Activity,
            "Synced",
            false,
            false,
            SystemTime::now(),
            t0,
        );
        assert_eq!(m.showing(t0, true), Some((Place::StatusLine, "Synced")));
        // Settings opened meanwhile: the same Activity moves next to the titlebar button.
        assert_eq!(
            m.showing(t0 + ms(1000), false),
            Some((Place::Titlebar, "Synced"))
        );
        assert_eq!(
            m.showing(t0 + SHOWN_FOR - ms(1), true).map(|s| s.1),
            Some("Synced")
        );
        assert_eq!(m.showing(t0 + SHOWN_FOR, true), None);
    }

    #[test]
    fn a_newer_activity_takes_the_place_and_an_alert_does_not() {
        let mut m = Messages::default();
        let (t0, at) = (Instant::now(), SystemTime::now());
        m.raise(Kind::Activity, "Copied 3 files", false, false, at, t0);
        m.raise(Kind::Activity, "Synced", false, false, at, t0 + ms(3000));
        // The first one's time is up, the second's is not.
        assert_eq!(
            m.showing(t0 + ms(5000), false),
            Some((Place::Titlebar, "Synced"))
        );
        m.raise(Kind::Alert, "Sync failed", false, true, at, t0 + ms(5000));
        assert_eq!(m.showing(t0 + ms(5000), false).map(|s| s.1), Some("Synced"));
    }

    #[test]
    fn progress_shows_until_it_ends_updates_in_place_and_stays_out_of_the_list() {
        let mut m = Messages::default();
        let t0 = Instant::now();
        m.set_progress("update", "Downloading tern 0.4.0\u{2026} 10%");
        m.set_progress("update", "Downloading tern 0.4.0\u{2026} 60%");
        let later = t0 + SHOWN_FOR * 10;
        assert_eq!(
            m.showing(later, true),
            Some((Place::StatusLine, "Downloading tern 0.4.0\u{2026} 60%"))
        );
        assert_eq!(m.list().len(), 0);
        m.end_progress("update");
        assert_eq!(m.showing(later, true), None);
    }

    #[test]
    fn a_new_activity_shows_over_progress_for_its_time_then_progress_is_back() {
        let mut m = Messages::default();
        let t0 = Instant::now();
        m.set_progress("transfers", "Copying a.txt 40%");
        m.raise(
            Kind::Activity,
            "Synced",
            false,
            false,
            SystemTime::now(),
            t0,
        );
        assert_eq!(m.showing(t0, false), Some((Place::Titlebar, "Synced")));
        assert_eq!(
            m.showing(t0 + SHOWN_FOR, false),
            Some((Place::Titlebar, "Copying a.txt 40%"))
        );
    }

    #[test]
    fn ending_one_progress_leaves_the_other() {
        let mut m = Messages::default();
        let later = Instant::now() + SHOWN_FOR;
        m.set_progress("update", "Checking for updates\u{2026}");
        m.set_progress("transfers", "Copying a.txt 40%");
        assert_eq!(
            m.showing(later, true).map(|s| s.1),
            Some("Copying a.txt 40%")
        );
        m.end_progress("transfers");
        assert_eq!(
            m.showing(later, true).map(|s| s.1),
            Some("Checking for updates\u{2026}")
        );
        m.end_progress("nothing");
        m.end_progress("update");
        assert_eq!(m.showing(later, true), None);
    }
}
