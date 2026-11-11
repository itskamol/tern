//! A real `Session` in a test window, logging in to an in-process SSH server that takes one
//! password: the server's questions, tern's own (vault passphrase, save offer) and the order
//! they come in, as a person at the terminal meets them. No GUI, network or Keychain beyond
//! 127.0.0.1; the vault is a file in a temp dir.
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::time::{Duration, Instant};

use gpui::{Entity, IntoElement, Render, TestApp};
use tern_ssh::{ConnectSpec, SecretString};
use tern_vault::{Key, Vault};

use super::scenario_server::{PASSWORD, PROMPT, SUDO_PROMPT, Typed, serve};
use super::{Launch, Session, Status};
use crate::keeper::Keeper;
use crate::password_command::Auth as LinkAuth;
use crate::platform::SshRuntime;

struct Host(Entity<Session>);

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
