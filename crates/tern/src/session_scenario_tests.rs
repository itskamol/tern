//! A real `Session` in a test window, logging in to an in-process SSH server that takes one
//! password: the server's questions, tern's own (vault passphrase, save offer) and the order
//! they come in, as a person at the terminal meets them. No GUI, network or Keychain beyond
//! 127.0.0.1; the vault is a file in a temp dir.
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::time::{Duration, Instant};

use gpui::{Entity, IntoElement, Render, TestApp};
use tern_ssh::{ConnectSpec, SecretString};
use tern_vault::{Key, Vault};

use super::scenario_server::{PASSWORD, PROMPT, STAR_PROMPT, SUDO_PROMPT, Typed, serve};
use super::{Launch, Put, Session, Status};
use crate::command_history::{CommandHistory, Scope};
use crate::keeper::Keeper;
use crate::password_command::Auth as LinkAuth;
use crate::platform::SshRuntime;

struct Host(Entity<Session>);

/// Notes seen so far, taken out as they are checked.
#[derive(Clone, Default)]
struct Notes(std::rc::Rc<std::cell::RefCell<Vec<String>>>);

impl Notes {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.borrow_mut())
    }
}

impl Render for Host {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::div()
    }
}

/// One login: the app, the session, and what keeps the server and temp files alive.
struct Scene {
    app: TestApp,
    session: Entity<Session>,
    /// What the server's shell was typed.
    typed: Typed,
    _server: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

/// A session to the test server whose vault (locked, passphrase `vp1`) holds the password.
fn scene() -> Scene {
    let (server, port, typed) = serve();
    let dir = tempfile::tempdir().unwrap();
    let vault_path = dir.path().join("vault.age");
    let mut vault = Vault::new(SecretString::from("vp1".to_owned()));
    vault.set(
        Key::Password {
            user: "test".into(),
            host: "127.0.0.1".into(),
            port,
        },
        SecretString::from(PASSWORD.to_owned()),
    );
    vault.save(&vault_path).unwrap();
    let spec = ConnectSpec {
        host: "127.0.0.1".into(),
        port,
        user: "test".into(),
        known_hosts: Some(dir.path().join("known_hosts")),
        ..Default::default()
    };
    let mut app = TestApp::new();
    app.update(|cx| {
        SshRuntime::install(cx).unwrap();
        Keeper::install_at(vault_path, cx);
        cx.set_global(CommandHistory::new(dir.path().join("history")));
    });
    let window = app.open_window(|window, cx| {
        let theme = crate::theme::Theme::zeron_dark().terminal(13.0, None);
        Host(Session::open(
            Launch::Ssh(spec),
            LinkAuth::default(),
            "demo".into(),
            false,
            theme,
            window,
            cx,
        ))
    });
    let session = window.read(|host, _| host.0.clone());
    Scene {
        app,
        session,
        typed,
        _server: server,
        _dir: dir,
    }
}

impl Scene {
    /// Everything on the terminal's screen, one line per row.
    fn screen(&mut self) -> String {
        let terminal = self.app.read(|cx| self.session.read(cx).terminal.clone());
        self.app.read(|cx| {
            let t = terminal.read(cx);
            (0..t.rows())
                .map(|row| t.row_text(row))
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    /// Runs the app until the screen shows `text`, as the server and the vault answer on other
    /// threads.
    fn wait_for(&mut self, text: &str) {
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until {
            self.app.run_until_parked();
            if self.screen().contains(text) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("never saw {text:?} on the screen:\n{}", self.screen());
    }

    /// Typing, as the terminal hands it to the session.
    fn type_line(&mut self, line: &str) {
        let bytes = format!("{line}\r").into_bytes();
        let session = self.session.clone();
        self.app.update(|cx| {
            session.update(cx, |s, cx| {
                s.on_terminal_event(&tern_term::TerminalEvent::Output(bytes), cx);
            });
        });
    }

    /// Keys as the terminal view hands them to the terminal, which is where the session hears
    /// what the person types.
    fn keys(&mut self, text: &str) {
        let terminal = self.app.read(|cx| self.session.read(cx).terminal.clone());
        let bytes = text.as_bytes().to_vec();
        self.app
            .update(|cx| terminal.update(cx, |t, cx| t.write(bytes, cx)));
    }

    /// A line typed key by key, then Enter once the server has echoed it: how a person types
    /// (`line` must not be on the screen already).
    fn run(&mut self, line: &str) {
        self.keys(line);
        self.wait_for(line);
        self.keys("\r");
        self.settle();
    }

    /// What the remote wrote, as the session shows it.
    fn remote_says(&mut self, text: &str) {
        let (session, bytes) = (self.session.clone(), text.as_bytes().to_vec());
        self.app
            .update(|cx| session.update(cx, |s, cx| s.show(&bytes, cx)));
    }

    /// What the remote wrote, as it arrives from the connection.
    fn remote_prints(&mut self, text: &str) {
        let (session, bytes) = (self.session.clone(), text.as_bytes().to_vec());
        self.app.update(|cx| {
            session.update(cx, |s, cx| {
                s.on_session_event(tern_ssh::SessionEvent::Data(bytes), cx)
            })
        });
    }

    /// What is on the clipboard now, as text.
    fn clipboard(&mut self) -> Option<String> {
        self.app
            .read_from_clipboard()
            .and_then(|item| item.text())
            .filter(|t| !t.is_empty())
    }

    fn set_clipboard(&mut self, policy: crate::clipboard::Policy) {
        let session = self.session.clone();
        self.app
            .update(|cx| session.update(cx, |s, _| s.set_clipboard_policy(policy)));
    }

    fn answer_clipboard(&mut self, answer: crate::clipboard::Answer) {
        let session = self.session.clone();
        self.app
            .update(|cx| session.update(cx, |s, cx| s.answer_clipboard(answer, cx)));
        self.settle();
    }

    /// The clipboard notes the session sends the app from now on: "ask 5", "always", ...
    fn notes(&mut self) -> Notes {
        let notes = Notes::default();
        let seen = notes.clone();
        let session = self.session.clone();
        self.app.update(|cx| {
            cx.subscribe(&session, move |_, note: &super::SessionNote, _| {
                let text = match note {
                    super::SessionNote::ClipboardAsk(chars) => format!("ask {chars}"),
                    super::SessionNote::ClipboardAlways => "always".into(),
                    _ => return,
                };
                seen.0.borrow_mut().push(text);
            })
            .detach();
        });
        notes
    }

    /// The session's Shell folder as `host:path`.
    fn shell_folder(&mut self) -> Option<String> {
        let session = self.session.clone();
        self.app.read(|cx| {
            session
                .read(cx)
                .shell_folder()
                .map(|f| format!("{}:{}", f.host, f.path))
        })
    }

    /// Settles until `line` is in the history, or gives up after a few seconds.
    fn wait_for_history(&mut self, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.history().iter().any(|l| l == line) && Instant::now() < deadline {
            self.settle();
        }
    }

    /// The command history of the host this session is on, newest first.
    fn history(&mut self) -> Vec<String> {
        self.app.read(|cx| {
            cx.global::<CommandHistory>()
                .search(Scope::Host("demo"), "")
                .into_iter()
                .map(|e| e.line)
                .collect()
        })
    }

    /// The Fill shortcut on this session.
    fn fill(&mut self) {
        let session = self.session.clone();
        self.app
            .update(|cx| session.update(cx, |s, cx| s.fill_password(cx)));
    }

    /// Whether the session would offer Fill now: the pane's "password asked" state.
    fn waiting(&mut self) -> bool {
        self.app
            .read(|cx| self.session.read(cx).waiting_for_fill(cx))
    }

    /// Lets the app and the server settle, so what a press sent has had time to arrive.
    fn settle(&mut self) {
        for _ in 0..15 {
            self.app.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Logs in with the vault's passphrase and gets to the shell prompt.
    fn log_in_with_vault(&mut self) {
        self.wait_for("continue connecting (yes/no)?");
        self.type_line("yes");
        self.wait_for("Vault passphrase (Enter to skip):");
        self.type_line("vp1");
        self.wait_for(PROMPT.trim_end());
    }

    fn status(&mut self) -> Status {
        self.app.read(|cx| self.session.read(cx).status())
    }
}

#[test]
fn a_locked_vault_opened_at_the_prompt_answers_the_server() {
    let mut s = scene();
    s.wait_for("continue connecting (yes/no)?");
    s.type_line("yes");
    // The server asks for the password; the vault holds it but is locked: tern asks for it.
    s.wait_for("Vault passphrase (Enter to skip):");
    s.type_line("vp1");
    s.wait_for("from the vault]");
    s.wait_for(PROMPT.trim_end());
    assert_eq!(s.status(), Status::Connected);
    assert!(
        !s.screen().contains("Save the password"),
        "a password that came from the vault is not offered for saving again"
    );
}

#[test]
fn a_wrong_vault_passphrase_falls_through_to_the_server_s_own_question() {
    let mut s = scene();
    s.wait_for("continue connecting (yes/no)?");
    s.type_line("yes");
    s.wait_for("Vault passphrase (Enter to skip):");
    s.type_line("not-it");
    s.wait_for("Wrong vault passphrase.");
    s.wait_for("test@127.0.0.1's password:");
    s.type_line(PASSWORD);
    s.wait_for(PROMPT.trim_end());
    assert_eq!(s.status(), Status::Connected);
}

#[test]
fn one_fill_at_a_sudo_prompt_types_the_vault_password_once_and_the_server_accepts_it() {
    let mut s = scene();
    s.log_in_with_vault();
    assert!(!s.waiting(), "no prompt yet, so no badge");
    s.type_line("sudo true");
    s.wait_for(SUDO_PROMPT);
    assert!(s.waiting(), "the sudo prompt is waiting for a fill");
    s.fill();
    s.wait_for("ok");
    assert_eq!(s.typed.lines(), ["sudo true", PASSWORD]);
    assert!(!s.waiting(), "the answered prompt is not waiting");
    // Pressed again with no prompt on the line: nothing is typed.
    s.fill();
    s.settle();
    assert_eq!(s.typed.lines(), ["sudo true", PASSWORD]);
}

#[test]
fn a_locked_vault_asks_for_its_passphrase_before_the_fill_types_anything() {
    let mut s = scene();
    // Log in by hand, so the vault stays locked.
    s.wait_for("continue connecting (yes/no)?");
    s.type_line("yes");
    s.wait_for("Vault passphrase (Enter to skip):");
    s.type_line("not-it");
    s.wait_for("test@127.0.0.1's password:");
    s.type_line(PASSWORD);
    s.wait_for(PROMPT.trim_end());
    s.type_line("sudo true");
    s.wait_for(SUDO_PROMPT);
    assert!(
        s.waiting(),
        "a locked vault may hold the password, so it is offered"
    );
    s.fill();
    s.wait_for("Vault passphrase (Enter to skip):");
    assert_eq!(
        s.typed.lines(),
        ["sudo true"],
        "nothing is typed while it is locked"
    );
    s.type_line("vp1");
    s.wait_for("ok");
    assert_eq!(s.typed.lines(), ["sudo true", PASSWORD]);
}

#[test]
fn a_command_typed_at_the_prompt_reaches_the_history_and_the_login_does_not() {
    let mut s = scene();
    s.log_in_with_vault();
    assert!(s.history().is_empty(), "the login answers are not commands");
    s.run("echo hi");
    s.run("uptime");
    assert_eq!(s.history(), ["uptime", "echo hi"]);
    assert_eq!(s.typed.lines(), ["echo hi", "uptime"]);
}

#[test]
fn nothing_typed_at_a_password_prompt_is_recorded_shown_or_not() {
    let mut s = scene();
    s.log_in_with_vault();
    s.run("sudo true");
    s.wait_for(SUDO_PROMPT);
    s.keys(PASSWORD);
    s.keys("\r");
    s.wait_for("ok");
    // A prompt that shows a star per key leaves text on the screen to read: still not kept.
    s.run("starpass");
    s.wait_for(STAR_PROMPT);
    s.keys("abc");
    s.wait_for("***");
    s.keys("\r");
    // Under load the history can trail the screen: wait for the command, then look again.
    s.wait_for_history("starpass");
    s.settle();
    assert_eq!(s.history(), ["starpass", "sudo true"]);
}

#[test]
fn lines_that_look_like_secrets_or_start_with_a_space_are_not_recorded() {
    let mut s = scene();
    s.log_in_with_vault();
    s.run("export DB_TOKEN=abc");
    s.run(" echo private");
    s.run("echo public");
    assert_eq!(s.history(), ["echo public"]);
}

#[test]
fn nothing_typed_inside_a_full_screen_program_is_recorded() {
    let mut s = scene();
    s.log_in_with_vault();
    s.remote_says("\x1b[?1049h");
    s.keys("ihello\r");
    s.remote_says("\x1b[?1049l");
    s.settle();
    assert!(s.history().is_empty());
    s.run("echo after");
    assert_eq!(s.history(), ["echo after"]);
}

#[test]
fn nothing_is_recorded_while_this_tab_broadcasts_its_typing() {
    let mut s = scene();
    s.log_in_with_vault();
    // Another tab that gets a copy of what is typed here.
    let other = s
        .app
        .open_window(|window, cx| {
            let theme = crate::theme::Theme::zeron_dark().terminal(13.0, None);
            let spec = ConnectSpec {
                host: "127.0.0.1".into(),
                port: 1,
                user: "x".into(),
                ..Default::default()
            };
            let launch = Launch::Ssh(spec);
            Host(Session::open(
                launch,
                LinkAuth::default(),
                "other".into(),
                false,
                theme,
                window,
                cx,
            ))
        })
        .read(|host, _| host.0.downgrade());
    s.app.update(|cx| {
        s.session
            .update(cx, |sess, _| sess.set_mirrors(vec![other]))
    });
    s.run("echo broadcast");
    assert!(s.history().is_empty());
    s.app
        .update(|cx| s.session.update(cx, |sess, _| sess.set_mirrors(Vec::new())));
    s.run("echo alone");
    assert_eq!(s.history(), ["echo alone"]);
}

#[test]
fn a_line_put_at_the_prompt_is_recorded_when_the_person_presses_enter() {
    let mut s = scene();
    s.log_in_with_vault();
    let put = s.session.clone();
    assert_eq!(
        s.app
            .update(|cx| put.update(cx, |sess, cx| sess.put_at_prompt("echo placed", false, cx))),
        Put::Typed
    );
    s.wait_for("echo placed");
    assert!(s.history().is_empty(), "not run yet, so not a command yet");
    s.keys("\r");
    s.settle();
    assert_eq!(s.history(), ["echo placed"]);
}

#[test]
fn a_line_put_after_text_already_at_the_prompt_is_recorded_as_the_shell_will_run_it() {
    let mut s = scene();
    s.log_in_with_vault();
    s.keys("echo half");
    s.wait_for("echo half");
    let put = s.session.clone();
    s.app
        .update(|cx| put.update(cx, |sess, cx| sess.put_at_prompt(" joined", true, cx)));
    s.settle();
    assert_eq!(s.history(), ["echo half joined"]);
}

#[test]
fn nothing_is_put_into_a_full_screen_program_or_at_a_password_prompt() {
    let mut s = scene();
    s.log_in_with_vault();
    let put = s.session.clone();
    s.remote_says("\x1b[?1049h");
    let typed = |s: &mut Scene, line: &str| {
        s.app
            .update(|cx| put.update(cx, |sess, cx| sess.put_at_prompt(line, true, cx)))
    };
    assert_eq!(typed(&mut s, "echo no"), Put::FullScreen);
    s.remote_says("\x1b[?1049l");
    s.remote_says("\r\n[sudo] password for test: ");
    assert_eq!(typed(&mut s, "echo no"), Put::PasswordPrompt);
    s.settle();
    assert!(s.typed.lines().is_empty(), "{:?}", s.typed.lines());
    assert!(s.history().is_empty());
}

#[test]
fn a_start_from_before_the_remote_printed_a_new_line_is_not_used() {
    let mut s = scene();
    s.log_in_with_vault();
    s.keys("echo stale");
    s.wait_for("echo stale");
    // Output of something else, then a fresh prompt; what was half typed is no longer there.
    s.remote_prints("\r\nbackground job done\r\n\r\ntest@demo:~$ ");
    s.keys("echo fresh");
    s.wait_for("echo fresh");
    s.keys("\r");
    s.settle();
    assert_eq!(s.history(), ["echo fresh"]);
}

#[test]
fn running_a_history_line_types_it_with_enter_and_moves_it_to_the_newest_place() {
    let mut s = scene();
    s.log_in_with_vault();
    s.run("echo one");
    s.run("echo two");
    let put = s.session.clone();
    s.app
        .update(|cx| put.update(cx, |sess, cx| sess.put_at_prompt("echo one", true, cx)));
    s.settle();
    assert_eq!(s.typed.lines(), ["echo one", "echo two", "echo one"]);
    assert_eq!(s.history(), ["echo one", "echo two"]);
}

#[test]
fn with_prompt_marks_the_command_is_what_follows_the_mark_even_after_the_line_was_redrawn() {
    let mut s = scene();
    s.log_in_with_vault();
    s.keys("ec");
    s.wait_for("ec");
    // The shell completes by drawing a fresh prompt line; the marks say where the command is.
    s.remote_says("\r\n\x1b]133;A\x07test@demo:~$ \x1b]133;B\x07echo completed");
    s.keys("\r");
    s.settle();
    assert_eq!(s.history(), ["echo completed"]);
}

#[test]
fn without_marks_a_line_redrawn_further_down_is_not_guessed_at() {
    let mut s = scene();
    s.log_in_with_vault();
    s.keys("ec");
    s.wait_for("ec");
    s.remote_says("\r\ntest@demo:~$ echo completed");
    s.keys("\r");
    s.settle();
    assert!(s.history().is_empty());
}

#[test]
fn the_folder_the_server_s_shell_reports_becomes_the_session_s_shell_folder() {
    let mut s = scene();
    s.log_in_with_vault();
    assert_eq!(s.shell_folder(), None);
    s.remote_prints("\x1b]7;file://demo/var/log\x07test@demo:/var/log$ ");
    s.settle();
    assert_eq!(s.shell_folder().as_deref(), Some("demo:/var/log"));
    // A nested ssh reports another host's folder: kept, with that host, for the app to judge.
    s.remote_prints("\x1b]7;file://db-02/srv%20data\x1b\\");
    s.settle();
    assert_eq!(s.shell_folder().as_deref(), Some("db-02:/srv data"));
    assert!(s.screen().contains("test@demo:/var/log$"));
}

/// "hello" as a program sets the clipboard: OSC 52 with base64.
const SET_CLIPBOARD_HELLO: &str = "\x1b]52;c;aGVsbG8=\x07";

#[test]
fn in_ask_a_clipboard_write_waits_for_its_answer_and_deny_drops_it() {
    let mut s = scene();
    s.log_in_with_vault();
    let notes = s.notes();
    s.set_clipboard(crate::clipboard::Policy::Ask);
    s.remote_prints(SET_CLIPBOARD_HELLO);
    s.settle();
    assert_eq!(s.clipboard(), None, "Ask leaves the clipboard alone");
    assert_eq!(notes.take(), ["ask 5"]);
    s.answer_clipboard(crate::clipboard::Answer::Deny);
    assert_eq!(s.clipboard(), None);

    s.remote_prints(SET_CLIPBOARD_HELLO);
    s.settle();
    assert_eq!(notes.take(), ["ask 5"]);
    s.answer_clipboard(crate::clipboard::Answer::Once);
    assert_eq!(s.clipboard().as_deref(), Some("hello"));
}

#[test]
fn always_for_this_host_writes_and_stops_asking() {
    let mut s = scene();
    s.log_in_with_vault();
    let notes = s.notes();
    s.set_clipboard(crate::clipboard::Policy::Ask);
    s.remote_prints(SET_CLIPBOARD_HELLO);
    s.settle();
    s.answer_clipboard(crate::clipboard::Answer::Always);
    assert_eq!(s.clipboard().as_deref(), Some("hello"));
    assert_eq!(notes.take(), ["ask 5", "always"]);
    s.app
        .update(|cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new())));
    s.remote_prints("\x1b]52;c;aGk=\x07");
    s.settle();
    assert_eq!(s.clipboard().as_deref(), Some("hi"));
}

#[test]
fn allow_writes_at_once_and_deny_never_does() {
    let mut s = scene();
    s.log_in_with_vault();
    s.set_clipboard(crate::clipboard::Policy::Deny);
    s.remote_prints(SET_CLIPBOARD_HELLO);
    s.settle();
    assert_eq!(s.clipboard(), None);
    s.set_clipboard(crate::clipboard::Policy::Allow);
    s.remote_prints(SET_CLIPBOARD_HELLO);
    s.settle();
    assert_eq!(s.clipboard().as_deref(), Some("hello"));
}

#[test]
fn leaving_ask_drops_the_write_it_held_without_writing_it() {
    let mut s = scene();
    s.log_in_with_vault();
    s.set_clipboard(crate::clipboard::Policy::Ask);
    s.remote_prints(SET_CLIPBOARD_HELLO);
    s.settle();
    s.set_clipboard(crate::clipboard::Policy::Allow);
    s.answer_clipboard(crate::clipboard::Answer::Once);
    assert_eq!(s.clipboard(), None);
}
