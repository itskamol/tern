//! Which status dot a tab shows. One pure mapping so the tab, the sidebar and the status line
//! agree, and so "waiting for your answer" cannot silently share a dot with "connecting".
//! Colour is never the only signal: states also differ by shape (ring), motion (pulse) and words.

use gpui::Hsla;

use crate::lifecycle::Status;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    Idle,
    Connecting,
    /// Connecting, but stopped at a password, passphrase or host-key question.
    WaitingForInput,
    Connected,
    Closed,
}

/// The theme colour a dot uses, kept apart from `Theme` so the mapping is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Faint,
    Accent,
    Warning,
    Success,
    Danger,
}

impl Dot {
    /// `asking` is whether a login question is open and waiting for the person to answer it.
    pub fn of(status: &Status, asking: bool) -> Dot {
        match status {
            Status::Idle => Dot::Idle,
            Status::Connecting if asking => Dot::WaitingForInput,
            Status::Connecting => Dot::Connecting,
            Status::Connected => Dot::Connected,
            Status::Closed => Dot::Closed,
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            Dot::Idle => Tone::Faint,
            Dot::Connecting => Tone::Accent,
            Dot::WaitingForInput => Tone::Warning,
            Dot::Connected => Tone::Success,
            Dot::Closed => Tone::Danger,
        }
    }

    pub fn color(self, t: &Theme) -> Hsla {
        match self.tone() {
            Tone::Faint => t.faint,
            Tone::Accent => t.accent,
            Tone::Warning => t.warning,
            Tone::Success => t.success,
            Tone::Danger => t.danger,
        }
    }

    /// Drawn as an outline instead of a filled disc.
    pub fn hollow(self) -> bool {
        self == Dot::WaitingForInput
    }

    /// Breathes while the app works on its own; a dot waiting on the person stays still.
    pub fn pulses(self) -> bool {
        self == Dot::Connecting
    }

    pub fn label(self) -> &'static str {
        match self {
            Dot::Idle => "Not connected · Enter connects",
            Dot::Connecting => "Connecting…",
            Dot::WaitingForInput => "Waiting for your answer",
            Dot::Connected => "Connected",
            Dot::Closed => "Disconnected",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Dot; 5] = [
        Dot::Idle,
        Dot::Connecting,
        Dot::WaitingForInput,
        Dot::Connected,
        Dot::Closed,
    ];

    #[test]
    fn a_question_while_connecting_is_waiting_for_input() {
        assert_eq!(Dot::of(&Status::Connecting, true), Dot::WaitingForInput);
        assert_eq!(Dot::of(&Status::Connecting, false), Dot::Connecting);
    }

    #[test]
    fn a_question_does_not_change_the_other_states() {
        for asking in [false, true] {
            assert_eq!(Dot::of(&Status::Idle, asking), Dot::Idle);
            assert_eq!(Dot::of(&Status::Connected, asking), Dot::Connected);
            assert_eq!(Dot::of(&Status::Closed, asking), Dot::Closed);
        }
    }

    #[test]
    fn every_state_has_its_own_colour_and_words() {
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a.tone(), b.tone(), "{a:?} and {b:?} share a colour");
                assert_ne!(a.label(), b.label(), "{a:?} and {b:?} share words");
            }
        }
    }

    #[test]
    fn waiting_differs_from_connecting_by_more_than_colour() {
        let (w, c) = (Dot::WaitingForInput, Dot::Connecting);
        assert_ne!(w.tone(), c.tone());
        assert_ne!(w.hollow(), c.hollow());
        assert_ne!(w.pulses(), c.pulses());
        assert_ne!(w.label(), c.label());
    }

    #[test]
    fn the_colours_are_distinct_in_both_palettes() {
        for t in [Theme::zeron_dark(), Theme::zeron_light()] {
            for (i, a) in ALL.iter().enumerate() {
                for b in &ALL[i + 1..] {
                    assert_ne!(a.color(&t), b.color(&t), "{a:?} and {b:?}");
                }
            }
        }
    }
}
