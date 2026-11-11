//! What the session hands to the command history: the command line the person finished typing.
//!
//! With OSC 133 prompt marks the command starts at the shell's "command start" mark. Without
//! them it starts where the cursor stood at the person's first keystroke after the remote last
//! printed a line. Either
//! way it is read off the screen when Enter is sent, so completion and recall are in it. A
//! line typed in the same write as its Enter (a paste without bracketed paste) has not reached
//! the screen yet and is missed; the history window records what it runs itself.

use gpui::{BorrowAppContext as _, Context};
use tern_term::{GridPos, PromptMark};

use super::Session;
use super::actions::is_password_prompt;
use crate::command_history::CommandHistory;

/// Bracketed paste begins: a pasted newline is not Enter.
const PASTE_START: &[u8] = b"\x1b[200~";

/// What [`Session::put_at_prompt`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    Typed,
    NotConnected,
    /// An editor or pager has the screen: a line typed there would be keystrokes for it.
    FullScreen,
    /// The line asks for a password: whatever is typed would be the answer.
    PasswordPrompt,
}

/// The line being typed, as far as the history needs to know.
#[derive(Default)]
pub(super) struct Typing {
    /// A prompt mark was seen on this connection, so marks decide where a command starts.
    marks: bool,
    start: Option<Start>,
    /// The bytes of the last [`TerminalEvent::Output`] were the person's, for the remote shell.
    ///
    /// [`TerminalEvent::Output`]: tern_term::TerminalEvent::Output
    pub(super) mine: bool,
}

#[derive(Clone, Copy)]
struct Start {
    at: GridPos,
    /// The line this began on was asking for a password.
    secret: bool,
}

impl Session {
    pub(super) fn on_prompt_mark(&mut self, mark: PromptMark, at: GridPos, cx: &mut Context<Self>) {
        self.typing.marks = true;
        self.typing.start = match mark {
            PromptMark::CommandStart => Some(Start {
                at,
                secret: self.on_password_prompt(cx),
            }),
            _ => None,
        };
    }

    /// The person typed `bytes` at the remote shell.
    pub(super) fn record_typed(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if !self.typing.mine || !self.may_record(cx) {
            self.typing.start = None;
            return;
        }
        if bytes.contains(&0x03) {
            self.typing.start = None;
            return;
        }
        if self.typing.start.is_none() && !self.typing.marks {
            self.typing.start = Some(self.start_here(cx));
        }
        let pasted = bytes.windows(PASTE_START.len()).any(|w| w == PASTE_START);
        if pasted || !bytes.contains(&b'\r') {
            return;
        }
        let Some(start) = self.typing.start.take() else {
            return;
        };
        if start.secret || self.on_password_prompt(cx) {
            return;
        }
        let line = self.terminal.read(cx).text_from(start.at);
        if let Some(line) = line {
            self.record_command(&line, cx);
        }
    }

    /// The remote printed: with no prompt marks, a line it started is no longer where the
    /// command began, so the next keystroke starts a new one.
    pub(super) fn remote_printed(&mut self, bytes: &[u8]) {
        if !self.typing.marks && bytes.contains(&b'\n') {
            self.typing.start = None;
        }
    }

    /// Types `line` at the prompt as the person would, and with `run` also presses Enter, which
    /// is then a command run like any other. What is kept is what the shell will run: the text
    /// already at the prompt followed by `line`. Nothing is typed into a full-screen program or
    /// at a password prompt.
    pub fn put_at_prompt(&mut self, line: &str, run: bool, cx: &mut Context<Self>) -> Put {
        if !self.accepts_input() {
            return Put::NotConnected;
        }
        if self.terminal.read(cx).on_alt_screen() {
            return Put::FullScreen;
        }
        if self.on_password_prompt(cx) {
            return Put::PasswordPrompt;
        }
        let may = self.may_record(cx);
        if may && self.typing.start.is_none() && !self.typing.marks {
            self.typing.start = Some(self.start_here(cx));
        }
        let at_prompt = self
            .typing
            .start
            .and_then(|s| self.terminal.read(cx).text_from(s.at))
            .unwrap_or_default();
        self.send(line.as_bytes().to_vec());
        if run {
            self.send(b"\r".to_vec());
            let secret = self.typing.start.take().is_some_and(|s| s.secret);
            if may && !secret {
                self.record_command(&format!("{at_prompt}{line}"), cx);
            }
        }
        Put::Typed
    }

    /// Typing here may be kept: not while broadcast input is on, and not inside a full-screen
    /// program.
    fn may_record(&self, cx: &Context<Self>) -> bool {
        self.mirrors.is_empty() && !self.terminal.read(cx).on_alt_screen()
    }

    fn start_here(&self, cx: &Context<Self>) -> Start {
        Start {
            at: self.terminal.read(cx).cursor_pos(),
            secret: self.on_password_prompt(cx),
        }
    }

    /// The cursor's line ends in a question for a password.
    fn on_password_prompt(&self, cx: &Context<Self>) -> bool {
        self.terminal
            .read(cx)
            .cursor_line_text()
            .is_some_and(|text| is_password_prompt(&text))
    }

    fn record_command(&self, line: &str, cx: &mut Context<Self>) {
        if !cx.has_global::<CommandHistory>() {
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let host = self.name.clone();
        let line = line.to_owned();
        cx.update_global(|history: &mut CommandHistory, _| {
            if let Err(e) = history.record(&host, &line, now) {
                tracing::warn!(error = %e, "command_history_write_failed");
            }
        });
    }

    /// The host this session's command history is kept under: the connection's name.
    pub fn alias(&self) -> &str {
        &self.name
    }
}
