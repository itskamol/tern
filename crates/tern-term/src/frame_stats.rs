//! Opt-in frame timing, for measuring what a streaming terminal costs per frame. Off unless
//! `TERN_FRAME_STATS` is set (to anything but `0` or empty); off, [`start`] is one relaxed
//! atomic load and no clock read. On, it logs once a second through `tracing`: frames drawn,
//! and the count, mean and worst time of the root view's render and of a terminal's prepaint.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// What is being timed.
#[derive(Debug, Clone, Copy)]
pub enum Span {
    /// The root view's `render`: building the element tree (one per frame drawn).
    ShellRender,
    /// A terminal element's `prepaint`: the grid snapshot and the shaped rows.
    TermPrepaint,
}

/// Count, total and worst time of one span since the last report.
#[derive(Debug, Default)]
struct Counter {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
}

/// A counter read and reset, in microseconds.
#[derive(Debug, Default, PartialEq, Eq)]
struct Summary {
    count: u64,
    mean_us: u64,
    max_us: u64,
}

impl Counter {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    fn record(&self, ns: u64) {
        self.count.fetch_add(1, Relaxed);
        self.total_ns.fetch_add(ns, Relaxed);
        self.max_ns.fetch_max(ns, Relaxed);
    }

    fn take(&self) -> Summary {
        let count = self.count.swap(0, Relaxed);
        let total = self.total_ns.swap(0, Relaxed);
        let max = self.max_ns.swap(0, Relaxed);
        Summary {
            count,
            mean_us: total.checked_div(count).unwrap_or(0) / 1000,
            max_us: max / 1000,
        }
    }
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static EPOCH: OnceLock<Instant> = OnceLock::new();
/// Milliseconds after `EPOCH` of the last report.
static LAST_REPORT_MS: AtomicU64 = AtomicU64::new(0);
static RENDER: Counter = Counter::new();
static PREPAINT: Counter = Counter::new();
const REPORT_EVERY_MS: u64 = 1000;

/// Turns the timing on when `TERN_FRAME_STATS` asks for it. Call once, after logging is set up.
pub fn init() {
    let on = std::env::var_os("TERN_FRAME_STATS").is_some_and(|v| !v.is_empty() && v != "0");
    if on {
        EPOCH.get_or_init(Instant::now);
        ENABLED.store(true, Relaxed);
        tracing::info!("frame_stats_enabled");
    }
}

/// A running measurement; the time up to its drop is recorded. Inert when the timing is off.
#[derive(Debug)]
#[must_use = "the span is timed until this is dropped"]
pub struct Timer(Option<(Span, Instant)>);

/// Starts timing `span`. Hold the result for the duration to measure.
pub fn start(span: Span) -> Timer {
    Timer(ENABLED.load(Relaxed).then(|| (span, Instant::now())))
}

impl Drop for Timer {
    fn drop(&mut self) {
        let Some((span, began)) = self.0 else {
            return;
        };
        let now = Instant::now();
        let ns = u64::try_from(now.duration_since(began).as_nanos()).unwrap_or(u64::MAX);
        match span {
            Span::ShellRender => RENDER.record(ns),
            Span::TermPrepaint => PREPAINT.record(ns),
        }
        report_if_due(now);
    }
}

/// Logs and resets the counters when a second has passed since the last report. Driven by the
/// measured spans themselves, so an idle window logs nothing.
fn report_if_due(now: Instant) {
    let epoch = *EPOCH.get_or_init(Instant::now);
    let at = u64::try_from(now.duration_since(epoch).as_millis()).unwrap_or(u64::MAX);
    let last = LAST_REPORT_MS.load(Relaxed);
    let since = at.saturating_sub(last);
    if since < REPORT_EVERY_MS
        || LAST_REPORT_MS
            .compare_exchange(last, at, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let (render, prepaint) = (RENDER.take(), PREPAINT.take());
    tracing::info!(
        window_ms = since,
        frames = render.count,
        render_mean_us = render.mean_us,
        render_max_us = render.max_us,
        prepaints = prepaint.count,
        prepaint_mean_us = prepaint.mean_us,
        prepaint_max_us = prepaint.max_us,
        "frame_stats"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_reports_mean_and_worst_then_resets() {
        let c = Counter::new();
        for us in [100, 200, 600] {
            c.record(us * 1000);
        }
        assert_eq!(
            c.take(),
            Summary {
                count: 3,
                mean_us: 300,
                max_us: 600
            }
        );
        assert_eq!(c.take(), Summary::default());
    }

    #[test]
    fn off_by_default_a_timer_is_inert() {
        assert!(start(Span::ShellRender).0.is_none());
    }
}
