//! A host panel with no session logging in to a real SSH server (the session scenario harness's:
//! one account, one password), asked the same questions a session asks, in the panel. No GUI,
//! network beyond 127.0.0.1 or Keychain; the vault is a file in a temp dir.
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::time::{Duration, Instant};

use gpui::{AppContext, Entity, TestApp};
use tern_ssh::{ConnectSpec, SecretString};
use tern_vault::{Key, Vault};

use super::{LoginEvent, PanelLogin, Step};
use crate::keeper::Keeper;
use crate::password_command::Auth;
use crate::platform::SshRuntime;
use crate::session::scenario_server::{PASSWORD, serve};

struct Scene {
    app: TestApp,
    login: Entity<PanelLogin>,
    events: std::rc::Rc<std::cell::RefCell<Vec<LoginEvent>>>,
    _server: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

/// A panel login to the test server; with `vault`, a locked vault (passphrase `vp1`) holds the
/// password.
fn scene(vault: bool) -> Scene {
    let (server, port, _typed) = serve();
    let dir = tempfile::tempdir().unwrap();
    let vault_path = dir.path().join("vault.age");
    if vault {
        let mut v = Vault::new(SecretString::from("vp1".to_owned()));
        v.set(
            Key::Password {
                user: "test".into(),
                host: "127.0.0.1".into(),
                port,
            },
            SecretString::from(PASSWORD.to_owned()),
        );
        v.save(&vault_path).unwrap();
    }
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
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    let login = app.update(|cx| {
        let colors = crate::theme::Theme::zeron_dark().input_colors();
        let login = cx.new(|cx| PanelLogin::start(spec, Auth::default(), colors, cx));
        cx.subscribe(&login, move |_, event: &LoginEvent, _| {
            seen.borrow_mut().push(event.clone());
        })
        .detach();
        login
    });
    Scene {
        app,
        login,
        events,
        _server: server,
        _dir: dir,
    }
}

impl Scene {
    /// Runs the app until `ready` holds for the login, as the server and the vault answer on
    /// other threads.
    fn wait(&mut self, what: &str, ready: impl Fn(&PanelLogin) -> bool) {
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until {
            self.app.run_until_parked();
            if self.app.read(|cx| ready(self.login.read(cx))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("never {what}");
    }

    fn asking(&mut self, first_line: &str) {
        let first_line = first_line.to_owned();
        self.wait(&format!("asked {first_line:?}"), move |l| {
            matches!(&l.step, Step::Asking(q) if q.lines.iter().any(|x| x.contains(&first_line)))
        });
    }

    fn type_in(&mut self, text: &str) {
        let (login, text) = (self.login.clone(), text.to_owned());
        self.app.update(|cx| {
            let input = match &login.read(cx).step {
                Step::Asking(q) => q.input.clone().unwrap(),
                _ => panic!("no field is open"),
            };
            input.update(cx, |i, cx| i.set_text(text, cx));
            login.update(cx, |l, cx| l.submit(cx));
        });
    }

    fn say_yes(&mut self) {
        let login = self.login.clone();
        self.app
            .update(|cx| login.update(cx, |l, cx| l.answer_yes(true, cx)));
    }

    fn connected(&mut self) {
        self.wait("connected", |l| matches!(l.step, Step::Connected));
        assert!(self.app.read(|cx| self.login.read(cx).handle().is_some()));
        assert_eq!(*self.events.borrow(), [LoginEvent::Connected]);
    }
}

#[test]
fn a_panel_with_no_session_asks_the_host_key_and_the_password_and_logs_in() {
    let mut s = scene(false);
    s.asking("can't be established");
    s.say_yes();
    s.asking("password");
    s.type_in(PASSWORD);
    s.connected();
}

#[test]
fn a_refused_password_is_asked_again_and_says_so() {
    let mut s = scene(false);
    s.asking("can't be established");
    s.say_yes();
    s.asking("password");
    s.type_in("not-it");
    s.asking("Permission denied");
    s.type_in(PASSWORD);
    s.connected();
}

#[test]
fn a_locked_vault_is_opened_in_the_panel_and_answers_the_password() {
    let mut s = scene(true);
    s.asking("can't be established");
    s.say_yes();
    s.asking("Vault passphrase");
    s.type_in("vp1");
    s.connected();
}

#[test]
fn a_wrong_vault_passphrase_falls_through_to_the_server_s_own_question() {
    let mut s = scene(true);
    s.asking("can't be established");
    s.say_yes();
    s.asking("Vault passphrase");
    s.type_in("not-it");
    s.asking("password");
    s.type_in(PASSWORD);
    s.connected();
}

#[test]
fn refusing_the_host_key_ends_the_login_and_retry_asks_again() {
    let mut s = scene(false);
    s.asking("can't be established");
    let login = s.login.clone();
    s.app
        .update(|cx| login.update(cx, |l, cx| l.answer_yes(false, cx)));
    s.wait("failed", |l| matches!(l.step, Step::Failed(_)));
    let login = s.login.clone();
    s.app.update(|cx| login.update(cx, |l, cx| l.retry(cx)));
    s.asking("can't be established");
}
