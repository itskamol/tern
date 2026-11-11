//! OSC 133 (the shell-integration prompt marks of FinalTerm, iTerm2 and others). The emulator
//! drops OSC sequences it does not know, so the marks are found here, in the bytes, before the
//! parser sees them; the bytes themselves go on to the parser unchanged. The scanner keeps a
//! few bytes between reads, so a mark split across two reads still counts, and it never
//! buffers a long sequence (an OSC 52 payload is skipped as it goes by).

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

#[derive(Debug, Default)]
pub(crate) struct Scanner {
    state: State,
    body: Vec<u8>,
}

impl Scanner {
    /// The marks in `bytes`, each with the offset just past its terminator.
    pub(crate) fn scan(&mut self, bytes: &[u8]) -> Vec<(usize, PromptMark)> {
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

    fn step(&mut self, b: u8) -> Option<PromptMark> {
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
                _ if self.body.len() >= BODY_MAX => self.state = State::Skip,
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
                self.state = if b == b'\\' { State::Ground } else { State::Esc };
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

    fn finish(&mut self) -> Option<PromptMark> {
        self.state = State::Ground;
        let rest = self.body.strip_prefix(b"133;")?;
        let mark = match rest.first()? {
            b'A' => PromptMark::PromptStart,
            b'B' => PromptMark::CommandStart,
            b'C' => PromptMark::CommandOutput,
            b'D' => PromptMark::CommandEnd,
            _ => return None,
        };
        matches!(rest.get(1), None | Some(b';')).then_some(mark)
    }
}

#[cfg(test)]
#[path = "osc133_tests.rs"]
mod tests;
