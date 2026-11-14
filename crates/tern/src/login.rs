//! Login questions asked inside the terminal, worded as OpenSSH asks them, so answering a
//! password or host-key prompt in tern feels the same as in `ssh`. Pure: bytes in, bytes out.

use std::collections::VecDeque;

use tern_ssh::oneshot::Sender;
use tern_ssh::{ChallengePrompt, ExposeSecret, Prompt, SecretString};
use tern_vault::Key;

/// What the terminal should show after a keystroke, and whether the question is answered.
#[derive(Debug, PartialEq, Eq)]
pub struct Step {
    pub display: Vec<u8>,
    pub done: bool,
}

/// One login question in progress. Keystrokes go here instead of to the remote.
#[derive(Debug)]
pub struct Login {
    kind: Kind,
    line: String,
    echo: bool,
    /// The submitted secret or local answer, kept for the caller (offer to save, vault unlock).
    answer: Option<SecretString>,
}

#[derive(Debug)]
enum Kind {
    Secret(Option<Sender<Option<SecretString>>>),
    YesNo(Option<Sender<bool>>),
    /// A question tern asks for itself; the answer stays here for [`Login::take_answer`].
    Local,
    Challenge {
        pending: VecDeque<ChallengePrompt>,
        answers: Vec<SecretString>,
        reply: Option<Sender<Option<Vec<SecretString>>>>,
    },
}

impl Login {
    /// Starts a question; returns it with the text to show.
    pub fn start(prompt: Prompt) -> (Self, Vec<u8>) {
        match prompt {
            Prompt::Password { user, host, reply } => (
                Self::new(Kind::Secret(Some(reply)), false),
                format!("{user}@{host}'s password: ").into_bytes(),
            ),
            Prompt::KeyPassphrase { path, reply } => (
                Self::new(Kind::Secret(Some(reply)), false),
                format!("Enter passphrase for key '{}': ", path.display()).into_bytes(),
            ),
            Prompt::UnknownHostKey {
                host,
                port,
                algorithm,
                fingerprint_sha256,
                reply,
            } => (
                Self::new(Kind::YesNo(Some(reply)), true),
                format!(
                    "The authenticity of host '{host} ({host}:{port})' can't be established.\r\n\
                     {algorithm} key fingerprint is {fingerprint_sha256}.\r\n\
                     Are you sure you want to continue connecting (yes/no)? "
                )
                .into_bytes(),
            ),
            Prompt::Challenge {
                name,
                instructions,
                prompts,
                reply,
            } => {
                let pending: VecDeque<_> = prompts.into();
                let mut display = Vec::new();
                for line in [name, instructions].iter().filter(|l| !l.is_empty()) {
                    display.extend_from_slice(&from_server(line));
                    display.extend_from_slice(b"\r\n");
                }
                let echo = pending.front().is_some_and(|p| p.echo);
                if let Some(first) = pending.front() {
                    display.extend_from_slice(&from_server(&first.text));
                }
                let kind = Kind::Challenge {
                    pending,
                    answers: Vec::new(),
                    reply: Some(reply),
                };
                (Self::new(kind, echo), display)
            }
        }
    }

    /// A question tern asks for itself, such as the vault passphrase.
    pub fn ask(text: &str, echo: bool) -> (Self, Vec<u8>) {
        (Self::new(Kind::Local, echo), text.as_bytes().to_vec())
    }

    /// The answer once the question is done: the line typed for a password, passphrase or
    /// local question; `None` after Ctrl-C or for other kinds.
    pub fn take_answer(&mut self) -> Option<SecretString> {
        self.answer.take()
    }

    fn new(kind: Kind, echo: bool) -> Self {
        Self {
            kind,
            line: String::new(),
            echo,
            answer: None,
        }
    }

    /// Feeds keystrokes. Enter submits, Backspace edits, Ctrl-C cancels; escape sequences
    /// such as arrow keys are ignored.
    pub fn input(&mut self, bytes: &[u8]) -> Step {
        let mut display = Vec::new();
        let text = String::from_utf8_lossy(bytes);
        for ch in text.chars() {
            match ch {
                '\r' | '\n' => {
                    display.extend_from_slice(b"\r\n");
                    if let Some(next) = self.submit() {
                        display.extend_from_slice(&next);
                    } else {
                        return Step {
                            display,
                            done: true,
                        };
                    }
                }
                '\u{3}' => {
                    display.extend_from_slice(b"^C\r\n");
                    self.cancel();
                    return Step {
                        display,
                        done: true,
                    };
                }
                '\u{7f}' | '\u{8}' => {
                    if self.line.pop().is_some() && self.echo {
                        display.extend_from_slice(b"\x08 \x08");
                    }
                }
                '\u{1b}' => break,
                c if !c.is_control() => {
                    self.line.push(c);
                    if self.echo {
                        let mut buf = [0; 4];
                        display.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
                _ => {}
            }
        }
        Step {
            display,
            done: false,
        }
    }

    /// Ends the line. Returns the next question's text, or `None` when this login is answered.
    fn submit(&mut self) -> Option<Vec<u8>> {
        let line = std::mem::take(&mut self.line);
        match &mut self.kind {
            Kind::Secret(reply) => {
                self.answer = Some(SecretString::from(line.clone()));
                if let Some(r) = reply.take() {
                    let _ = r.send(Some(SecretString::from(line)));
                }
                None
            }
            Kind::Local => {
                self.answer = Some(SecretString::from(line));
                None
            }
            Kind::YesNo(reply) => match line.trim().to_ascii_lowercase().as_str() {
                answer @ ("yes" | "no") => {
                    if let Some(r) = reply.take() {
                        let _ = r.send(answer == "yes");
                    }
                    None
                }
                _ => Some(b"Please type 'yes' or 'no': ".to_vec()),
            },
            Kind::Challenge {
                pending,
                answers,
                reply,
            } => {
                pending.pop_front();
                answers.push(SecretString::from(line));
                if let Some(next) = pending.front() {
                    self.echo = next.echo;
                    return Some(from_server(&next.text));
                }
                if let Some(r) = reply.take() {
                    let _ = r.send(Some(std::mem::take(answers)));
                }
                None
            }
        }
    }

    fn cancel(&mut self) {
        match &mut self.kind {
            Kind::Secret(reply) => {
                if let Some(r) = reply.take() {
                    let _ = r.send(None);
                }
            }
            Kind::YesNo(reply) => {
                if let Some(r) = reply.take() {
                    let _ = r.send(false);
                }
            }
            Kind::Local => {}
            Kind::Challenge { reply, .. } => {
                if let Some(r) = reply.take() {
                    let _ = r.send(None);
                }
            }
        }
    }
}

/// Text the server chose (keyboard-interactive name, instructions, prompts), made safe to
/// write into the terminal: line breaks and tabs stay, every other control character is shown
/// in caret notation (`^[`), so a server cannot move the cursor, retitle the tab, write the
/// clipboard or draw a fake prompt. OpenSSH escapes these the same way.
fn from_server(text: &str) -> Vec<u8> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => out.push_str("\r\n"),
            '\t' => out.push(c),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push(char::from(c as u8 + b'@'));
            }
            '\u{7f}' => out.push_str("^?"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.into_bytes()
}

/// The vault entry that answers `prompt` on a connection to `host:port`; `None` for questions
/// a vault does not answer (host keys, keyboard-interactive).
pub fn key_for(prompt: &Prompt, host: &str, port: u16) -> Option<Key> {
    match prompt {
        Prompt::Password { user, .. } => Some(Key::Password {
            user: user.clone(),
            host: host.to_owned(),
            port,
        }),
        Prompt::KeyPassphrase { path, .. } => Some(Key::KeyPassphrase {
            path: path.display().to_string(),
        }),
        Prompt::Challenge { .. } | Prompt::UnknownHostKey { .. } => None,
    }
}

/// Answers a password or passphrase prompt without asking; any other prompt is returned.
pub fn answer(prompt: Prompt, secret: SecretString) -> Option<Prompt> {
    match prompt {
        Prompt::Password { reply, .. } | Prompt::KeyPassphrase { reply, .. } => {
            let _ = reply.send(Some(secret));
            None
        }
        other => Some(other),
    }
}

pub fn is_yes(answer: Option<&SecretString>) -> bool {
    answer.is_some_and(|a| matches!(a.expose_secret().trim(), "y" | "yes" | "Y" | "Yes"))
}

/// Where an answer tern gave for the person came from, in the words a Session and a Files tab
/// both use: the source first, so a narrow line cuts the host and not the source.
pub fn from_vault(key: &Key) -> String {
    format!("From the vault: {}", describe(key))
}

pub const FROM_COMMAND: &str = "From the command: the password";

pub fn describe(key: &Key) -> String {
    match key {
        Key::Password { user, host, .. } => format!("the password for {user}@{host}"),
        Key::KeyPassphrase { path } => format!("the passphrase for {path}"),
        Key::SshKey { name } => format!("the key {name}"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;

    use tern_ssh::secrecy::ExposeSecret;
    use tern_ssh::{ChallengePrompt, Prompt, oneshot};

    use super::*;

    fn answer<T>(mut rx: oneshot::Receiver<T>) -> T {
        rx.try_recv().unwrap().expect("no reply was sent")
    }

    fn password() -> (Login, oneshot::Receiver<Option<SecretString>>, Vec<u8>) {
        let (tx, rx) = oneshot::channel();
        let (login, shown) = Login::start(Prompt::Password {
            user: "deploy".into(),
            host: "web".into(),
            reply: tx,
        });
        (login, rx, shown)
    }

    #[test]
    fn password_is_asked_like_openssh_and_never_echoed() {
        let (mut login, rx, shown) = password();
        assert_eq!(shown, b"deploy@web's password: ");
        let step = login.input(b"hunter2");
        assert_eq!(
            step,
            Step {
                display: Vec::new(),
                done: false
            }
        );
        let step = login.input(b"\r");
        assert!(step.done);
        assert_eq!(answer(rx).unwrap().expose_secret(), "hunter2");
    }

    #[test]
    fn backspace_edits_the_answer() {
        let (mut login, rx, _) = password();
        login.input(b"hunterX\x7f2\r");
        assert_eq!(answer(rx).unwrap().expose_secret(), "hunter2");
    }

    #[test]
    fn ctrl_c_cancels() {
        let (mut login, rx, _) = password();
        let step = login.input(b"abc\x03");
        assert!(step.done);
        assert_eq!(step.display, b"^C\r\n");
        assert!(answer(rx).is_none());
    }

    #[test]
    fn arrow_keys_do_not_leak_into_the_answer() {
        let (mut login, rx, _) = password();
        login.input(b"pw");
        login.input(b"\x1b[A");
        login.input(b"\r");
        assert_eq!(answer(rx).unwrap().expose_secret(), "pw");
    }

    #[test]
    fn host_key_question_echoes_and_insists_on_yes_or_no() {
        let (tx, rx) = oneshot::channel();
        let (mut login, shown) = Login::start(Prompt::UnknownHostKey {
            host: "web".into(),
            port: 22,
            algorithm: "ssh-ed25519".into(),
            fingerprint_sha256: "SHA256:abc".into(),
            reply: tx,
        });
        let shown = String::from_utf8(shown).unwrap();
        assert!(shown.starts_with("The authenticity of host 'web (web:22)' can't be established."));
        assert!(shown.ends_with("Are you sure you want to continue connecting (yes/no)? "));

        let step = login.input(b"y\r");
        assert!(!step.done);
        assert_eq!(step.display, b"y\r\nPlease type 'yes' or 'no': ");
        assert!(login.input(b"yes\r").done);
        assert!(answer(rx));
    }

    #[test]
    fn passphrase_names_the_key() {
        let (tx, _rx) = oneshot::channel();
        let (_, shown) = Login::start(Prompt::KeyPassphrase {
            path: PathBuf::from("/k/id_ed25519"),
            reply: tx,
        });
        assert_eq!(shown, b"Enter passphrase for key '/k/id_ed25519': ");
    }

    #[test]
    fn challenge_asks_each_prompt_in_turn() {
        let (tx, rx) = oneshot::channel();
        let prompt = |text: &str, echo| ChallengePrompt {
            text: text.into(),
            echo,
        };
        let (mut login, shown) = Login::start(Prompt::Challenge {
            name: String::new(),
            instructions: "Two steps".into(),
            prompts: vec![prompt("User: ", true), prompt("Code: ", false)],
            reply: tx,
        });
        assert_eq!(shown, b"Two steps\r\nUser: ");
        let step = login.input(b"me\r");
        assert_eq!(step.display, b"me\r\nCode: ");
        let step = login.input(b"123\r");
        assert_eq!(step.display, b"\r\n");
        assert!(step.done);
        let answers = answer(rx).unwrap();
        let answers: Vec<&str> = answers.iter().map(|a| a.expose_secret()).collect();
        assert_eq!(answers, ["me", "123"]);
    }

    #[test]
    fn challenge_text_from_the_server_cannot_drive_the_terminal() {
        let (tx, _rx) = oneshot::channel();
        let (mut login, shown) = Login::start(Prompt::Challenge {
            name: "\x1b]0;pwned\x07".into(),
            instructions: "line one\r\nline two\u{9b}2J".into(),
            prompts: vec![
                ChallengePrompt {
                    text: "\x1b[2KPassword: ".into(),
                    echo: false,
                },
                ChallengePrompt {
                    text: "OTP:\x1b]52;c;aGk=\x07 ".into(),
                    echo: false,
                },
            ],
            reply: tx,
        });
        assert_eq!(
            String::from_utf8(shown).unwrap(),
            "^[]0;pwned^G\r\nline one\r\nline two\\u{9b}2J\r\n^[[2KPassword: "
        );
        let step = login.input(b"x\r");
        assert_eq!(step.display, b"\r\nOTP:^[]52;c;aGk=^G ");
    }

    #[test]
    fn a_typed_password_is_kept_for_the_offer_to_save() {
        let (mut login, rx, _) = password();
        assert!(login.input(b"hunter2\r").done);
        assert_eq!(login.take_answer().unwrap().expose_secret(), "hunter2");
        assert_eq!(answer(rx).unwrap().expose_secret(), "hunter2");
    }

    #[test]
    fn a_local_question_returns_its_line_and_ctrl_c_returns_nothing() {
        let (mut login, shown) = Login::ask("Vault passphrase: ", false);
        assert_eq!(shown, b"Vault passphrase: ");
        let step = login.input(b"s3cret\r");
        assert!(step.done);
        assert_eq!(step.display, b"\r\n");
        assert_eq!(login.take_answer().unwrap().expose_secret(), "s3cret");

        let (mut login, _) = Login::ask("Vault passphrase: ", false);
        assert!(login.input(b"abc\x03").done);
        assert!(login.take_answer().is_none());
    }

    #[test]
    fn password_key_uses_the_prompt_user_and_the_connection_address() {
        let (reply, _rx) = oneshot::channel();
        let prompt = Prompt::Password {
            user: "deploy".into(),
            host: "alias".into(),
            reply,
        };
        assert_eq!(
            key_for(&prompt, "10.0.0.5", 2222),
            Some(Key::Password {
                user: "deploy".into(),
                host: "10.0.0.5".into(),
                port: 2222
            })
        );
    }

    #[test]
    fn host_key_questions_are_never_answered_from_the_vault() {
        let (reply, _rx) = oneshot::channel();
        let prompt = Prompt::UnknownHostKey {
            host: "h".into(),
            port: 22,
            algorithm: "ed25519".into(),
            fingerprint_sha256: "SHA256:x".into(),
            reply,
        };
        assert_eq!(key_for(&prompt, "h", 22), None);
        assert!(super::answer(prompt, SecretString::from("x".to_owned())).is_some());
    }

    #[test]
    fn answering_sends_the_secret() {
        let (reply, mut rx) = oneshot::channel();
        let prompt = Prompt::Password {
            user: "u".into(),
            host: "h".into(),
            reply,
        };
        assert!(super::answer(prompt, SecretString::from("hunter2".to_owned())).is_none());
        assert_eq!(
            rx.try_recv().unwrap().unwrap().unwrap().expose_secret(),
            "hunter2"
        );
    }

    #[test]
    fn only_yes_saves() {
        let s = |t: &str| SecretString::from(t.to_owned());
        assert!(is_yes(Some(&s("yes"))));
        assert!(is_yes(Some(&s("y"))));
        assert!(!is_yes(Some(&s("no"))));
        assert!(!is_yes(Some(&s(""))));
        assert!(!is_yes(None));
    }
}
