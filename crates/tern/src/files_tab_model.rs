//! What one Files tab panel shows, apart from the window: where it stands, which folder, the
//! rows, the sort, what is picked, the notice, and which answer from the host is still wanted.
//!
//! The model is told what the person did and what the host answered; it says what to ask the
//! host next ([`Ask`]) and starts nothing. Every ask carries a [`Ticket`]; an answer with an
//! older ticket than the last ask is ignored, so a slow listing never shows a folder the person
//! left. File sources, the path field and the login stay with the window.

use tern_ssh::{FileEntry, FileError};

use crate::files_model::{Sort, SortKey, order};
use crate::files_pick::Picked;

/// What a panel shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Mac,
    /// A host by its name in tern (or the `user@host` it was opened by).
    Host(String),
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Self::Mac => "This Mac".to_owned(),
            Self::Host(alias) => alias.clone(),
        }
    }
}

/// Why a panel could not show a host's files, worded in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The host stopped answering while the panel was open.
    Gone,
    /// The panel was asked for a name tern has no host for.
    Unknown(String),
    /// The host went away after the login; what it said, if anything.
    Lost(String),
    /// A host's or the login's own message, as it came.
    Said(String),
}

impl Reason {
    pub fn text(&self) -> String {
        match self {
            Self::Gone => "The host stopped answering.".to_owned(),
            Self::Unknown(alias) => format!("{alias}: not a host tern knows"),
            Self::Lost(why) if why.is_empty() => "The host went away.".to_owned(),
            Self::Lost(why) => format!("The host went away: {why}"),
            Self::Said(text) => text.clone(),
        }
    }
}

impl From<&FileError> for Reason {
    fn from(e: &FileError) -> Self {
        match e {
            FileError::Closed => Self::Gone,
            other => Self::Said(other.to_string()),
        }
    }
}

/// Where a panel stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Opening the connection, or answering its login questions.
    Connecting,
    Loading,
    Ready,
    /// Could not connect or list; Retry tries again.
    Failed(Reason),
    /// The host was there and went away; Reconnect starts over.
    Dropped(Reason),
}

/// Names one ask of the host; the answer to it says which ask it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket(u64);

/// What the panel needs from its file source next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    Home(Ticket),
    List(Ticket, String),
}

#[derive(Debug)]
pub struct PanelModel {
    pub target: Target,
    pub state: State,
    /// The folder listed; empty until one was.
    pub cwd: String,
    pub entries: Vec<FileEntry>,
    pub sort: Sort,
    pub selection: Picked,
    pub notice: Option<String>,
    /// The ticket of the last ask.
    wanted: u64,
    waiting: bool,
}

impl PanelModel {
    pub fn new(target: Target) -> Self {
        Self {
            target,
            state: State::Connecting,
            cwd: String::new(),
            entries: Vec::new(),
            sort: Sort::default(),
            selection: Picked::default(),
            notice: None,
            wanted: 0,
            waiting: false,
        }
    }

    /// Starts over on the current target, dropping everything still pending for the last one.
    pub fn start(&mut self) {
        self.wanted += 1;
        self.waiting = false;
        self.state = State::Connecting;
        self.cwd.clear();
        self.entries.clear();
        self.selection.clear();
        self.notice = None;
    }

    /// Shows `target` from its start.
    pub fn switch_to(&mut self, target: Target) {
        self.target = target;
        self.start();
    }

    /// A name tern has no host for.
    pub fn unknown_host(&mut self, alias: &str) {
        self.state = State::Failed(Reason::Unknown(alias.to_owned()));
    }

    /// The connection is up and the files are being opened on it.
    pub fn opening(&mut self) -> Ticket {
        self.state = State::Loading;
        self.waiting = true;
        Ticket(self.wanted)
    }

    /// The panel's files are open: ask for the host's home folder. The Mac's are open at once.
    pub fn files_open(&mut self) -> Ask {
        self.waiting = true;
        Ask::Home(Ticket(self.wanted))
    }

    /// The person asked for the home folder again.
    pub fn go_home(&mut self) -> Ask {
        self.wanted += 1;
        self.files_open()
    }

    /// The answer to [`Self::opening`].
    pub fn opened(&mut self, ticket: Ticket, answer: Result<(), Reason>) -> Option<Ask> {
        if !self.is_wanted(ticket) {
            return None;
        }
        match answer {
            Ok(()) => Some(self.files_open()),
            Err(why) => {
                self.waiting = false;
                self.state = State::Failed(why);
                None
            }
        }
    }

    /// The answer to [`Ask::Home`]: the panel starts in that folder.
    pub fn home(&mut self, ticket: Ticket, answer: Result<String, Reason>) -> Option<Ask> {
        if !self.is_wanted(ticket) {
            return None;
        }
        match answer {
            Ok(home) => Some(self.open(home)),
            Err(why) => {
                self.waiting = false;
                self.failed(why);
                None
            }
        }
    }

    /// The person opened `path` (or the folder shown is listed again). The old rows stay on
    /// screen until it is listed, so a slow host does not flash an empty list.
    pub fn open(&mut self, path: String) -> Ask {
        self.wanted += 1;
        self.waiting = true;
        if self.entries.is_empty() {
            self.state = State::Loading;
        }
        self.notice = None;
        Ask::List(Ticket(self.wanted), path)
    }

    /// The answer to [`Ask::List`] for `path`. True when the folder shown changed, so the path
    /// field should show it.
    pub fn listed(
        &mut self,
        ticket: Ticket,
        path: &str,
        answer: Result<Vec<FileEntry>, Reason>,
    ) -> bool {
        if !self.is_wanted(ticket) {
            return false;
        }
        self.waiting = false;
        match answer {
            Ok(mut entries) => {
                order(&mut entries, self.sort);
                if self.cwd == path {
                    self.selection.keep_only(&paths(&entries));
                } else {
                    self.selection.clear();
                }
                path.clone_into(&mut self.cwd);
                self.entries = entries;
                self.state = State::Ready;
                true
            }
            Err(why) => {
                self.listing_failed(why);
                false
            }
        }
    }

    /// The host went away while the panel's login owned the connection.
    pub fn dropped(&mut self, why: String) {
        self.wanted += 1;
        self.waiting = false;
        self.state = State::Dropped(Reason::Lost(why));
    }

    /// True while an ask of the host is unanswered.
    #[cfg(test)]
    pub fn pending(&self) -> bool {
        self.waiting
    }

    /// Words for the notice line, until the next folder is opened.
    pub fn set_notice(&mut self, text: String) {
        self.notice = Some(text);
    }

    /// Re-orders the rows by `key`; the choice applies to every listing after.
    pub fn sort_by(&mut self, key: SortKey) {
        self.sort = self.sort.click(key);
        order(&mut self.entries, self.sort);
    }

    /// The paths of the rows shown, top to bottom.
    pub fn paths(&self) -> Vec<String> {
        paths(&self.entries)
    }

    /// The row last plainly clicked, if it is still listed.
    pub fn anchor_row(&self) -> Option<usize> {
        let anchor = self.selection.anchor()?;
        self.entries.iter().position(|e| e.path == anchor)
    }

    pub fn click_row(&mut self, ix: usize, command: bool, shift: bool) {
        self.selection.click(&self.paths(), ix, command, shift);
    }

    /// Up and down keys: move the pick by `delta` rows from the last one clicked.
    pub fn step(&mut self, delta: isize) {
        if let Some(next) = crate::files_text::step(self.anchor_row(), delta, self.entries.len()) {
            self.selection.only(&self.paths(), next);
        }
    }

    pub fn unpick(&mut self) {
        self.selection.clear();
    }

    /// The rows picked, as entries.
    pub fn picked(&self) -> Vec<FileEntry> {
        self.entries
            .iter()
            .filter(|e| self.selection.contains(&e.path))
            .cloned()
            .collect()
    }

    fn is_wanted(&self, ticket: Ticket) -> bool {
        ticket.0 == self.wanted
    }

    /// A gone connection is its own state; anything else stays a notice when there is a folder
    /// to stay in, and a failure with Retry when there is not.
    fn listing_failed(&mut self, why: Reason) {
        if self.cwd.is_empty() {
            return self.failed(why);
        }
        self.notice = Some(why.text());
        self.state = if why == Reason::Gone {
            State::Dropped(Reason::Gone)
        } else {
            State::Ready
        };
    }

    fn failed(&mut self, why: Reason) {
        self.state = if why == Reason::Gone {
            State::Dropped(why)
        } else {
            State::Failed(why)
        };
    }
}

fn paths(entries: &[FileEntry]) -> Vec<String> {
    entries.iter().map(|e| e.path.clone()).collect()
}

#[cfg(test)]
#[path = "files_tab_model_tests.rs"]
mod tests;
