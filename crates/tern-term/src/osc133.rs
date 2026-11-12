//! OSC 133 (the shell-integration prompt marks of FinalTerm, iTerm2 and others) and OSC 7 (the
//! shell's folder, see `osc7.rs`). The emulator drops OSC sequences it does not know, so they
//! are found here, in the bytes, before the parser sees them; the bytes themselves go on to the
//! parser unchanged. The scanner keeps the body between reads, so a sequence split across two
//! reads still counts. It keeps at most 64 bytes of any other OSC and `PATH_MAX` of an OSC 7
//! path, so memory stays bounded: an OSC 52 payload is skipped as it goes by.

use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::TermMode;

use super::Terminal;
use super::osc7::{self, ShellFolder};

/// What a prompt mark says: `A` the prompt starts, `B` the person's command starts (the
/// prompt has ended), `C` the command's output starts, `D` the command finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    PromptStart,
    CommandStart,
    CommandOutput,
    CommandEnd,
}

/// A cell of the grid as it was when something happened there: the row (0 is the top of the
/// screen, negative rows are scrollback), the column, and how much history there was, so the
/// row can be found again after output scrolled the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPos {
    pub line: i32,
    pub col: usize,
    pub history: usize,
}

/// Longest `133;…` body kept; a longer sequence is not a prompt mark.
const BODY_MAX: usize = 64;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
    /// Inside an OSC that is not ours: wait for its end without keeping it.
    Skip,
    SkipEsc,
}

/// What the scanner found in the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    Mark(PromptMark),
    Folder(ShellFolder),
}

#[derive(Debug, Default)]
pub(crate) struct Scanner {
    state: State,
    body: Vec<u8>,
}

impl Scanner {
    /// The marks and folders in `bytes`, each with the offset just past its terminator.
    pub(crate) fn scan(&mut self, bytes: &[u8]) -> Vec<(usize, Found)> {
        let mut found = Vec::new();
        if self.state == State::Ground && !bytes.contains(&0x1b) {
            return found;
        }
        for (at, &b) in bytes.iter().enumerate() {
            if let Some(mark) = self.step(b) {
                found.push((at + 1, mark));
            }
        }
        found
    }

    fn step(&mut self, b: u8) -> Option<Found> {
        match self.state {
            State::Ground => self.ground(b),
            State::Esc => {
                self.state = match b {
                    b']' => {
                        self.body.clear();
                        State::Osc
                    }
                    0x1b => State::Esc,
                    _ => State::Ground,
                }
            }
            State::Osc => match b {
                0x07 => return self.finish(),
                0x1b => self.state = State::OscEsc,
                0x18 | 0x1a => self.state = State::Ground,
                _ if self.body.len() >= self.body_max() => self.state = State::Skip,
                _ => self.body.push(b),
            },
            State::OscEsc => {
                if b == b'\\' {
                    return self.finish();
                }
                // The ESC ended the string and began another sequence.
                self.state = State::Esc;
                return self.step(b);
            }
            State::Skip => match b {
                0x07 | 0x18 | 0x1a => self.state = State::Ground,
                0x1b => self.state = State::SkipEsc,
                _ => {}
            },
            State::SkipEsc => {
                self.state = if b == b'\\' {
                    State::Ground
                } else {
                    State::Esc
                };
                if self.state == State::Esc {
                    return self.step(b);
                }
            }
        }
        None
    }

    fn ground(&mut self, b: u8) {
        if b == 0x1b {
            self.state = State::Esc;
        }
    }

    /// A folder may be a long path; anything else this scanner keeps is short.
    fn body_max(&self) -> usize {
        if self.body.starts_with(b"7;") {
            osc7::BODY_MAX
        } else {
            BODY_MAX
        }
    }

    fn finish(&mut self) -> Option<Found> {
        self.state = State::Ground;
        if let Some(folder) = osc7::parse(&self.body) {
            return Some(Found::Folder(folder));
        }
        let rest = self.body.strip_prefix(b"133;")?;
        let mark = match rest.first()? {
            b'A' => PromptMark::PromptStart,
            b'B' => PromptMark::CommandStart,
            b'C' => PromptMark::CommandOutput,
            b'D' => PromptMark::CommandEnd,
            _ => return None,
        };
        matches!(rest.get(1), None | Some(b';')).then_some(Found::Mark(mark))
    }
}

impl Terminal {
    /// Where the cursor is now, for [`Self::text_from`].
    pub fn cursor_pos(&self) -> GridPos {
        let at = self.term.grid().cursor.point;
        GridPos {
            line: at.line.0,
            col: at.column.0,
            history: self.history_size(),
        }
    }

    /// The text from `at` to the end of the cursor's line, wrapped rows joined: what has been
    /// typed since `at`. `None` when `at` is no longer part of the cursor's line (output went
    /// by, or the screen was cleared). Scrolling since `at` is followed through the history
    /// length; once the scrollback is full that cannot be told, so only a start on the
    /// cursor's own row is trusted.
    pub fn text_from(&self, at: GridPos) -> Option<String> {
        let cursor = self.term.grid().cursor.point;
        let grown = self.history_size().checked_sub(at.history)? as i32;
        let full = self.history_size() >= self.config.scrolling_history;
        let line = match (grown, full) {
            (_, true) if cursor.line.0 != at.line => return None,
            (0, _) | (_, true) => at.line,
            _ => at.line - grown,
        };
        if line > cursor.line.0 || line < -(self.history_size() as i32) {
            return None;
        }
        let from = Point::new(
            Line(line),
            Column(at.col.min(self.cols().saturating_sub(1))),
        );
        let to = Point::new(cursor.line, Column(self.cols().saturating_sub(1)));
        let text = self.term.bounds_to_string(from, to);
        (!text.contains('\n')).then_some(text)
    }

    /// Whether a full-screen program (vim, htop, less) is on the screen.
    pub fn on_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }
}

#[cfg(test)]
#[path = "osc133_tests.rs"]
mod tests;
