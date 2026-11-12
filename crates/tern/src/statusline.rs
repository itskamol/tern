// Sizes from zeron crates/ui/src/composer.rs: SESSION_FOOTER_HEIGHT 24 (line 89), the footer
// row's 10 px side padding (line ~11474) and its 11 px text (lines 558, 4725) (MIT).
//! The strip under the terminal panel: which host the active tab is on, whether the
//! connection is up, its round-trip latency, and how long it has been.

use std::time::Duration;

use gpui::{Div, Hsla, IntoElement, ParentElement, Styled, div, px};

use crate::indicator::Dot;
use crate::theme::Theme;

/// zeron `SESSION_FOOTER_HEIGHT`.
pub const HEIGHT: f32 = 24.0;
const SIDE_PAD: f32 = 10.0;
const TEXT_SIZE: f32 = 11.0;

/// `user@host:port`.
pub fn host_label(user: &str, host: &str, port: u16) -> String {
    format!("{user}@{host}:{port}")
}

/// `mm:ss` under an hour, `h:mm` from an hour on.
pub fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3600 {
        format!("{}:{:02}", secs / 3600, secs % 3600 / 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

/// How the latency figure is coloured.
#[derive(Debug, PartialEq, Eq)]
pub enum Lag {
    Fine,
    Slow,
    Bad,
}

const SLOW_OVER: Duration = Duration::from_millis(150);
const BAD_OVER: Duration = Duration::from_millis(400);

pub fn lag(rtt: Duration) -> Lag {
    if rtt > BAD_OVER {
        Lag::Bad
    } else if rtt > SLOW_OVER {
        Lag::Slow
    } else {
        Lag::Fine
    }
}

/// `42 ms`, rounded to whole milliseconds.
pub fn format_latency(rtt: Duration) -> String {
    // Under half a millisecond rounds to 0, which reads like a broken meter.
    match (rtt.as_micros() + 500) / 1000 {
        0 => "<1 ms".to_owned(),
        ms => format!("{ms} ms"),
    }
}

/// The strip for one tab; `elapsed` is `None` until the session has connected.
pub fn render(
    t: &Theme,
    label: &str,
    dot: Dot,
    latency: Option<Duration>,
    elapsed: Option<Duration>,
    bg: Hsla,
) -> impl IntoElement + use<> {
    // `bg` is the terminal scheme's, which may be dark under the light appearance.
    let t = &t.on(bg);
    let disc = div().size(px(6.)).rounded_full();
    let marker = if dot.hollow() {
        disc.border_1().border_color(dot.color(t))
    } else {
        disc.bg(dot.color(t))
    };
    let part = |text: String| div().flex_none().child(text);
    let mut row: Div = div()
        .flex_none()
        .h(px(HEIGHT))
        .px(px(SIDE_PAD))
        .flex()
        .items_center()
        .gap(px(12.))
        .bg(bg)
        .border_t_1()
        .border_color(t.hairline)
        .text_size(px(TEXT_SIZE))
        .text_color(t.muted)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(t.text)
                .child(label.to_owned()),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(marker)
                .child(dot.label()),
        );
    if let Some(rtt) = latency {
        let colour = match lag(rtt) {
            Lag::Fine => t.muted,
            Lag::Slow => t.warning,
            Lag::Bad => t.danger,
        };
        row = row.child(part(format_latency(rtt)).text_color(colour));
    }
    if let Some(elapsed) = elapsed {
        row = row.child(part(format_elapsed(elapsed)));
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_is_minutes_and_seconds_then_hours_and_minutes() {
        let s = Duration::from_secs;
        assert_eq!(format_elapsed(s(0)), "00:00");
        assert_eq!(format_elapsed(s(59)), "00:59");
        assert_eq!(format_elapsed(s(61)), "01:01");
        assert_eq!(format_elapsed(s(3599)), "59:59");
        assert_eq!(format_elapsed(s(3600)), "1:00");
        assert_eq!(format_elapsed(s(3600 + 7 * 60 + 59)), "1:07");
        assert_eq!(format_elapsed(s(26 * 3600 + 5 * 60)), "26:05");
    }

    #[test]
    fn latency_reads_in_whole_milliseconds() {
        let ms = Duration::from_micros;
        assert_eq!(format_latency(ms(42_400)), "42 ms");
        assert_eq!(format_latency(ms(42_600)), "43 ms");
        assert_eq!(format_latency(ms(300)), "<1 ms");
        assert_eq!(format_latency(ms(600)), "1 ms");
        assert_eq!(format_latency(Duration::from_secs(2)), "2000 ms");
    }

    #[test]
    fn latency_turns_amber_over_150_and_red_over_400() {
        let ms = Duration::from_millis;
        assert_eq!(lag(ms(0)), Lag::Fine);
        assert_eq!(lag(ms(150)), Lag::Fine);
        assert_eq!(lag(ms(151)), Lag::Slow);
        assert_eq!(lag(ms(400)), Lag::Slow);
        assert_eq!(lag(ms(401)), Lag::Bad);
        assert_eq!(lag(ms(5000)), Lag::Bad);
    }

    #[test]
    fn the_host_label_is_user_at_host_and_port() {
        assert_eq!(host_label("root", "10.0.0.5", 2222), "root@10.0.0.5:2222");
        assert_eq!(host_label("me", "example.org", 22), "me@example.org:22");
    }
}
