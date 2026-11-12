//! Programs on the remote writing the clipboard (OSC 52): Allow, Ask or Deny (ADR 0002). In
//! Ask, a write is held, not written, until the person answers its Decision: once, always for
//! this host, or not. The gate here holds no window and no session, so the rule is tested on
//! its own; the session wires it to the terminal and the clipboard.

use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize};

/// What a program's clipboard write gets. Allow is the default, so nothing changes for anyone
/// who never opens the setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    #[default]
    Allow,
    Ask,
    Deny,
}

/// Reads the setting as it is written now (`"allow"`, `"ask"`, `"deny"`) and as it was before
/// Ask existed (`clipboardWrite: true` or `false`).
impl<'de> Deserialize<'de> for Policy {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Written {
            Old(bool),
            Name(String),
        }
        match Written::deserialize(de)? {
            Written::Old(true) => Ok(Self::Allow),
            Written::Old(false) => Ok(Self::Deny),
            Written::Name(name) => match name.as_str() {
                "allow" => Ok(Self::Allow),
                "ask" => Ok(Self::Ask),
                "deny" => Ok(Self::Deny),
                _ => Err(serde::de::Error::unknown_variant(
                    &name,
                    &["allow", "ask", "deny"],
                )),
            },
        }
    }
}

/// The person's answer to a held write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Once,
    /// For this host from now on: kept on a saved connection, for this session otherwise.
    Always,
    Deny,
}

/// What to do with one write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Store {
    Write(String),
    Refuse,
    /// Held; ask the person.
    Ask,
}

/// One session's clipboard writes: the setting, whether its host is always allowed, the
/// write waiting for an answer and when a write was last mentioned. A newer write replaces a
/// held one, so at most one is ever kept.
#[derive(Debug, Default)]
pub struct Gate {
    pub policy: Policy,
    pub always: bool,
    held: Option<String>,
    said: Option<Instant>,
}

/// How often a session's clipboard writes are mentioned, so a stream of copies is said once.
pub const MENTION_EVERY: Duration = Duration::from_secs(60);

impl Gate {
    pub fn store(&mut self, text: String) -> Store {
        match self.policy {
            Policy::Deny => Store::Refuse,
            Policy::Allow => Store::Write(text),
            Policy::Ask if self.always => Store::Write(text),
            Policy::Ask => {
                self.held = Some(text);
                Store::Ask
            }
        }
    }

    /// The text to write now, if the answer lets it through.
    pub fn answer(&mut self, answer: Answer) -> Option<String> {
        let held = self.held.take()?;
        match answer {
            Answer::Once => Some(held),
            Answer::Always => {
                self.always = true;
                Some(held)
            }
            Answer::Deny => None,
        }
    }

    /// Whether a write at `now` should be mentioned: the first, then at most once a minute.
    pub fn mention(&mut self, now: Instant) -> bool {
        if self
            .said
            .is_some_and(|at| now.saturating_duration_since(at) < MENTION_EVERY)
        {
            return false;
        }
        self.said = Some(now);
        true
    }

    /// Drops a held write unanswered: the connection it came from is gone, or the setting no
    /// longer asks.
    pub fn forget(&mut self) {
        self.held = None;
    }

    /// Characters in the held write, while one waits.
    pub fn held_chars(&self) -> Option<usize> {
        self.held.as_ref().map(|t| t.chars().count())
    }
}

/// The Activity when a program on `who` set the clipboard, or was refused.
pub fn note(who: &str, chars: usize, allowed: bool) -> String {
    if allowed {
        format!("{who} copied {chars} characters to the clipboard")
    } else {
        format!("{who} tried to set the clipboard; allow it in Settings \u{2192} Terminal")
    }
}

/// What the Decision asks, on its card and in the Activity list.
pub fn question(who: &str, chars: usize) -> String {
    format!("{who} wants to copy {chars} characters to the clipboard")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn store(gate: &mut Gate, policy: Policy, text: &str) -> Store {
        gate.policy = policy;
        gate.store(text.into())
    }

    fn ask(gate: &mut Gate, text: &str) {
        assert_eq!(store(gate, Policy::Ask, text), Store::Ask);
    }

    #[test]
    fn allow_writes_and_deny_refuses_without_holding_anything() {
        let mut g = Gate::default();
        assert_eq!(store(&mut g, Policy::Allow, "a"), Store::Write("a".into()));
        assert_eq!(store(&mut g, Policy::Deny, "b"), Store::Refuse);
        assert_eq!(g.held_chars(), None);
    }

    #[test]
    fn ask_holds_the_write_until_it_is_answered() {
        let mut g = Gate::default();
        ask(&mut g, "secret");
        assert_eq!(g.held_chars(), Some(6));
        assert_eq!(g.answer(Answer::Once).as_deref(), Some("secret"));
        assert_eq!(g.held_chars(), None);
        // Once is once: the next write asks again.
        ask(&mut g, "again");
    }

    #[test]
    fn deny_drops_the_held_write() {
        let mut g = Gate::default();
        ask(&mut g, "x");
        assert_eq!(g.answer(Answer::Deny), None);
        assert_eq!(g.held_chars(), None);
        assert_eq!(g.answer(Answer::Once), None, "nothing is left to write");
    }

    #[test]
    fn always_writes_the_held_text_and_lets_the_next_writes_through() {
        let mut g = Gate::default();
        ask(&mut g, "x");
        assert_eq!(g.answer(Answer::Always).as_deref(), Some("x"));
        assert!(g.always);
        assert_eq!(store(&mut g, Policy::Ask, "y"), Store::Write("y".into()));
        // Deny for everyone still wins over a host allowed while asking.
        assert_eq!(store(&mut g, Policy::Deny, "z"), Store::Refuse);
    }

    #[test]
    fn a_stream_of_writes_is_mentioned_once_a_minute() {
        let mut g = Gate::default();
        let t0 = Instant::now();
        assert!(g.mention(t0));
        assert!(!g.mention(t0 + Duration::from_secs(59)));
        assert!(g.mention(t0 + MENTION_EVERY));
    }

    #[test]
    fn a_forgotten_write_is_gone_and_nothing_is_written_for_it() {
        let mut g = Gate::default();
        ask(&mut g, "from the old connection");
        g.forget();
        assert_eq!(g.held_chars(), None);
        assert_eq!(g.answer(Answer::Once), None);
    }

    #[test]
    fn the_notes_say_who_and_what_to_do() {
        assert_eq!(
            note("me@web:22", 5, true),
            "me@web:22 copied 5 characters to the clipboard"
        );
        assert!(note("me@web:22", 5, false).contains("Settings \u{2192} Terminal"));
        assert_eq!(
            question("me@web:22", 5),
            "me@web:22 wants to copy 5 characters to the clipboard"
        );
    }

    #[test]
    fn a_newer_write_replaces_the_one_held() {
        let mut g = Gate::default();
        ask(&mut g, "old");
        ask(&mut g, "newer");
        assert_eq!(g.answer(Answer::Once).as_deref(), Some("newer"));
    }

    #[test]
    fn the_setting_reads_old_files_and_new_ones() {
        let read = |json: &str| serde_json::from_str::<Policy>(json).unwrap();
        assert_eq!(read("true"), Policy::Allow);
        assert_eq!(read("false"), Policy::Deny);
        assert_eq!(read("\"allow\""), Policy::Allow);
        assert_eq!(read("\"ask\""), Policy::Ask);
        assert_eq!(read("\"deny\""), Policy::Deny);
        assert!(serde_json::from_str::<Policy>("\"sometimes\"").is_err());
        assert_eq!(serde_json::to_string(&Policy::Ask).unwrap(), "\"ask\"");
    }
}
