// Adapted from seed-design (daangn/seed-design, MIT): packages/css/recipes/snackbar.css and
// snackbar-region.css for sizes, colours and motion; docs/content/components/snackbar.mdx for
// behaviour (4 s, pause while held, one at a time, queued, at most one action). CubicBezier is
// from zeron crates/ui/src/motion.rs (MIT).
//! The snackbar: what an Alert shows as (ADR 0002), and nothing else. One at a time, at the
//! top right under the titlebar, below a Decision card when one waits. seed-design places it at
//! the bottom; tern keeps the terminal's last lines clear, and the right edge covers less than
//! the centre. Only the message module can show one, through `Shell::alert`.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::shell::Shell;
use crate::theme::hex;

/// seed-design: "기본적으로 4초 동안 표시됩니다."
pub const DEFAULT_DURATION: Duration = Duration::from_secs(4);
/// `--seed-duration-d3`, `--seed-timing-function-enter`.
const ENTER: Duration = Duration::from_millis(150);
/// `--seed-duration-d2`, `--seed-timing-function-exit`.
const EXIT: Duration = Duration::from_millis(100);
const ENTER_CURVE: CubicBezier = CubicBezier::new(0.0, 0.0, 0.15, 1.0);
const EXIT_CURVE: CubicBezier = CubicBezier::new(0.35, 0.0, 1.0, 1.0);

/// Whether the Alert says something did not work: its card shows seed-design's critical mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Default,
    Failure,
}

type Action = Rc<dyn Fn(&mut Shell, &mut Window, &mut Context<Shell>)>;

/// An Alert (ADR 0002): a failure, or a message with one action. Shown only through
/// [`Shell::alert`], which also keeps it in the Activity list.
#[derive(Clone)]
pub(crate) struct Alert {
    severity: Severity,
    message: SharedString,
    action: Option<(SharedString, Action)>,
    duration: Duration,
}

impl Alert {
    /// An Alert that is not a failure: it carries an action (Undo, Show, Restart now).
    pub(crate) fn new(message: impl Into<SharedString>) -> Self {
        Self::with(Severity::Default, message)
    }

    /// Something did not work.
    pub(crate) fn failure(message: impl Into<SharedString>) -> Self {
        Self::with(Severity::Failure, message)
    }

    fn with(severity: Severity, message: impl Into<SharedString>) -> Self {
        Self {
            severity,
            message: message.into(),
            action: None,
            duration: DEFAULT_DURATION,
        }
    }

    /// Stays up for `secs` instead of the usual few seconds.
    pub(crate) fn staying(mut self, secs: u64) -> Self {
        self.duration = Duration::from_secs(secs);
        self
    }

    /// The text the Activity list keeps.
    pub(crate) fn text(&self) -> &str {
        &self.message
    }

    pub(crate) fn failed(&self) -> bool {
        self.severity == Severity::Failure
    }

    pub(crate) fn has_action(&self) -> bool {
        self.action.is_some()
    }

    /// One short, specific action ("Undo", "Show"), as seed-design allows at most one.
    pub(crate) fn action(
        mut self,
        label: impl Into<SharedString>,
        run: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
    ) -> Self {
        self.action = Some((label.into(), Rc::new(run)));
        self
    }
}

/// Where a shown snackbar is in its life, from its own clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Phase {
    Entering(f32),
    Shown,
    Exiting(f32),
    Gone,
}

/// The timing of the snackbar on screen. Time spent under the pointer does not count towards
/// its duration.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clock {
    started: Instant,
    duration: Duration,
    paused: Duration,
    hovered_since: Option<Instant>,
    /// Set when it starts leaving, early (dismissed) or on time.
    leaving_at: Option<Instant>,
}

impl Clock {
    pub(crate) fn new(now: Instant, duration: Duration) -> Self {
        Self {
            started: now,
            duration,
            paused: Duration::ZERO,
            hovered_since: None,
            leaving_at: None,
        }
    }

    pub(crate) fn hover(&mut self, hovered: bool, now: Instant) {
        match (hovered, self.hovered_since) {
            (true, None) => self.hovered_since = Some(now),
            (false, Some(since)) => {
                self.paused += now.saturating_duration_since(since);
                self.hovered_since = None;
            }
            _ => {}
        }
    }

    pub(crate) fn dismiss(&mut self, now: Instant) {
        self.leaving_at.get_or_insert(now);
    }

    fn visible_for(&self, now: Instant) -> Duration {
        let held = self
            .hovered_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        now.saturating_duration_since(self.started)
            .saturating_sub(self.paused + held)
    }

    pub(crate) fn phase(&mut self, now: Instant) -> Phase {
        if self.leaving_at.is_none() && self.visible_for(now) >= ENTER + self.duration {
            self.leaving_at = Some(now);
        }
        if let Some(at) = self.leaving_at {
            let t = now.saturating_duration_since(at).as_secs_f32() / EXIT.as_secs_f32();
            return if t >= 1.0 {
                Phase::Gone
            } else {
                Phase::Exiting(EXIT_CURVE.eval(t))
            };
        }
        let t = now.saturating_duration_since(self.started).as_secs_f32() / ENTER.as_secs_f32();
        if t < 1.0 {
            Phase::Entering(ENTER_CURVE.eval(t))
        } else {
            Phase::Shown
        }
    }
}

/// One at a time, in order: the rest wait here.
#[derive(Default)]
pub(super) struct Snackbars {
    queue: VecDeque<Alert>,
    showing: Option<(Alert, Clock)>,
}

impl std::fmt::Debug for Snackbars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snackbars")
            .field("waiting", &self.queue.len())
            .field("showing", &self.showing.as_ref().map(|(a, _)| a.text()))
            .finish()
    }
}

impl Shell {
    /// Shows a snackbar now, or after the current one: one at a time, in order. Only the
    /// message module reaches it, through [`Shell::alert`].
    pub(super) fn show_snackbar(&mut self, alert: Alert, cx: &mut Context<Self>) {
        self.activity.snackbars.queue.push_back(alert);
        if self.activity.snackbars.showing.is_none() {
            self.next_snackbar(cx);
        }
    }

    fn next_snackbar(&mut self, cx: &mut Context<Self>) {
        let Some(toast) = self.activity.snackbars.queue.pop_front() else {
            self.activity.snackbars.showing = None;
            return cx.notify();
        };
        let clock = Clock::new(Instant::now(), toast.duration);
        self.activity.snackbars.showing = Some((toast, clock));
        // The clock decides; this task only wakes the view so the phase is read again.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let gone = this
                    .update(cx, |s, cx| {
                        let gone = match s.activity.snackbars.showing.as_mut() {
                            Some((_, clock)) => clock.phase(Instant::now()) == Phase::Gone,
                            None => true,
                        };
                        if gone {
                            s.next_snackbar(cx);
                        } else {
                            cx.notify();
                        }
                        gone
                    })
                    .unwrap_or(true);
                if gone {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_snackbar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = Instant::now();
        let (toast, clock) = self.activity.snackbars.showing.as_mut()?;
        let phase = clock.phase(now);
        let (opacity, lift) = match phase {
            Phase::Gone => return None,
            // seed-design scales 0.8 → 1; gpui has no scale for divs, so the same curve drives
            // a fade and a short drop instead.
            Phase::Entering(p) => {
                window.request_animation_frame();
                (p, (1.0 - p) * -8.0)
            }
            Phase::Shown => (1.0, 0.0),
            Phase::Exiting(p) => {
                window.request_animation_frame();
                (1.0 - p, p * -8.0)
            }
        };
        let toast = toast.clone();
        let colors = Colors::for_theme(self.theme.light);
        let icon = match toast.severity {
            Severity::Default => None,
            Severity::Failure => Some(("!", colors.critical)),
        };
        let viewport = window.viewport_size();
        let card = div()
            .id("snackbar")
            .opacity(opacity)
            .mt(px(lift))
            .w(px(464.0_f32.min(f32::from(viewport.width) - 16.0)))
            .min_h(px(44.))
            .p(px(10.))
            .rounded(px(8.))
            .bg(colors.background)
            .flex()
            .items_center()
            .on_hover(cx.listener(|s, hovered: &bool, _, cx| {
                if let Some((_, clock)) = s.activity.snackbars.showing.as_mut() {
                    clock.hover(*hovered, Instant::now());
                }
                cx.notify();
            }))
            .when_some(icon, |el, (glyph, color)| {
                el.child(
                    div()
                        .flex_none()
                        .size(px(24.))
                        .pr(px(2.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .size(px(18.))
                                .rounded_full()
                                .bg(color)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(12.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(colors.background)
                                .child(glyph),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .child(
                        // Takes the width the action leaves and wraps in it; without this a long
                        // message ran past the card.
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(14.))
                            .line_height(px(19.))
                            .text_color(colors.text)
                            .child(toast.message.clone()),
                    )
                    .when_some(toast.action.clone(), |el, (label, run)| {
                        el.child(
                            div()
                                .id("snackbar-action")
                                .flex_none()
                                .cursor_pointer()
                                .text_size(px(14.))
                                .line_height(px(19.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(colors.action)
                                .on_click(cx.listener(move |s, _, window, cx| {
                                    run(s, window, cx);
                                    if let Some((_, clock)) = s.activity.snackbars.showing.as_mut()
                                    {
                                        clock.dismiss(Instant::now());
                                    }
                                    cx.notify();
                                }))
                                .child(label),
                        )
                    }),
            );
        Some(card.into_any_element())
    }
}

/// seed-design's snackbar colours: an inverted neutral-solid surface, so it reads over the
/// content behind it in either theme.
struct Colors {
    background: Hsla,
    text: Hsla,
    action: Hsla,
    critical: Hsla,
}

impl Colors {
    fn for_theme(light: bool) -> Self {
        if light { Self::light() } else { Self::dark() }
    }

    /// seed-design `theme-light` (packages/rootage/__generated__/color.json): bg-neutral-solid =
    /// gray-900, fg-on-neutral-solid = gray-00, fg-critical = red-700.
    /// The action is tern's dark-palette accent, which has the contrast the light accent lacks
    /// on this dark surface.
    fn light() -> Self {
        Self {
            background: hex(0x2a3038),
            text: hex(0xffffff),
            action: hex(0x8b7cf6),
            critical: hex(0xfa342c),
        }
    }

    /// seed-design `theme-dark`.
    fn dark() -> Self {
        Self {
            // bg-neutral-solid = palette gray-1000, fg-on-neutral-solid = gray-100 (dark).
            background: hex(0xf3f4f5),
            text: hex(0x16171b),
            // fg-brand in seed is its carrot orange; tern's brand is its accent.
            action: hex(0x6d5ce8),
            // fg-critical = red-700 (dark).
            critical: hex(0xff6e60),
        }
    }
}

/// CSS `cubic-bezier()` evaluated exactly (zeron `motion::CubicBezier`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CubicBezier {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

impl CubicBezier {
    pub(crate) const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    fn coefficients(a: f32, b: f32) -> (f32, f32, f32) {
        let c = 3.0 * a;
        let bb = 3.0 * (b - a) - c;
        (1.0 - c - bb, bb, c)
    }

    fn sample(a: f32, b: f32, t: f32) -> f32 {
        let (ca, cb, cc) = Self::coefficients(a, b);
        ((ca * t + cb) * t + cc) * t
    }

    fn solve_t(&self, x: f32) -> f32 {
        let mut t = x;
        for _ in 0..8 {
            let err = Self::sample(self.x1, self.x2, t) - x;
            if err.abs() < 1e-6 {
                return t;
            }
            let (a, b, c) = Self::coefficients(self.x1, self.x2);
            let d = (3.0 * a * t + 2.0 * b) * t + c;
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        for _ in 0..32 {
            let mid = (lo + hi) / 2.0;
            if Self::sample(self.x1, self.x2, mid) < x {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0
    }

    pub(crate) fn eval(&self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        Self::sample(self.y1, self.y2, self.solve_t(x)).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_snackbar_enters_stays_four_seconds_then_leaves() {
        let t0 = Instant::now();
        let mut c = Clock::new(t0, DEFAULT_DURATION);
        assert!(matches!(c.phase(t0 + ms(50)), Phase::Entering(p) if p > 0.0 && p < 1.0));
        assert_eq!(c.phase(t0 + ms(2000)), Phase::Shown);
        assert_eq!(c.phase(t0 + ENTER + DEFAULT_DURATION - ms(1)), Phase::Shown);
        assert!(matches!(
            c.phase(t0 + ENTER + DEFAULT_DURATION + ms(10)),
            Phase::Exiting(_)
        ));
        assert_eq!(
            c.phase(t0 + ENTER + DEFAULT_DURATION + ms(200)),
            Phase::Gone
        );
    }

    #[test]
    fn time_under_the_pointer_does_not_count() {
        let t0 = Instant::now();
        let mut c = Clock::new(t0, DEFAULT_DURATION);
        c.hover(true, t0 + ms(1000));
        // Held for ten seconds: still shown.
        assert_eq!(c.phase(t0 + ms(11_000)), Phase::Shown);
        c.hover(false, t0 + ms(11_000));
        // 1 s before the hold + 3.1 s after it is the full 4 s + enter.
        assert_eq!(c.phase(t0 + ms(14_000)), Phase::Shown);
        assert!(matches!(c.phase(t0 + ms(14_200)), Phase::Exiting(_)));
    }

    #[test]
    fn dismiss_leaves_at_once() {
        let t0 = Instant::now();
        let mut c = Clock::new(t0, DEFAULT_DURATION);
        c.dismiss(t0 + ms(500));
        assert!(matches!(c.phase(t0 + ms(550)), Phase::Exiting(_)));
        assert_eq!(c.phase(t0 + ms(650)), Phase::Gone);
    }

    #[test]
    fn curves_match_css() {
        // cubic-bezier(0, 0, .15, 1) is front-loaded; (.35, 0, 1, 1) is back-loaded.
        assert!(ENTER_CURVE.eval(0.5) > 0.8);
        assert!(EXIT_CURVE.eval(0.5) < 0.4);
        assert_eq!(ENTER_CURVE.eval(1.0), 1.0);
    }
}
