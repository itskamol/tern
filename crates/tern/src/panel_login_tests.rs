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
use crate::password_command::{Approvals, Auth};
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
    scene_with(vault, Auth::default())
}

fn scene_with(vault: bool, auth: Auth) -> Scene {
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
    let approved = dir.path().join("approved");
    let mut app = TestApp::new();
    app.update(|cx| {
        SshRuntime::install(cx).unwrap();
        Keeper::install_at(vault_path, cx);
    });
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = events.clone();
    let login = app.update(|cx| {
        let colors = crate::theme::Theme::zeron_dark().input_colors();
        let approvals = Approvals::in_dir(Some(approved));
        let login = cx.new(|cx| PanelLogin::start_with(spec, auth, approvals, colors, cx));
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
fn a_password_answered_from_the_vault_says_so_as_a_session_does() {
    let mut s = scene(true);
    s.asking("can't be established");
    s.say_yes();
    s.asking("Vault passphrase");
    s.type_in("vp1");
    s.connected();
    let note = s.app.read(|cx| s.login.read(cx).note().map(str::to_owned));
    assert_eq!(
        note.as_deref(),
        Some("Used the password for test@127.0.0.1 from the vault.")
    );
    let login = s.login.clone();
    s.app
        .update(|cx| login.update(cx, |l, cx| l.clear_note(cx)));
    assert_eq!(
        s.app.read(|cx| s.login.read(cx).note().map(str::to_owned)),
        None
    );
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

#[test]
fn an_unapproved_proxy_command_is_asked_about_in_the_panel_before_anything_is_dialled() {
    let auth = Auth {
        proxy_command: Some("ssh -W %h:%p bastion".into()),
        ..Auth::default()
    };
    let mut s = scene_with(false, auth);
    s.asking("ssh -W %h:%p bastion");
    s.say_yes();
    s.asking("can't be established");
}

#[test]
fn refusing_the_proxy_command_fails_the_panel_without_dialling() {
    let auth = Auth {
        proxy_command: Some("ssh -W %h:%p bastion".into()),
        ..Auth::default()
    };
    let mut s = scene_with(false, auth);
    s.asking("ssh -W %h:%p bastion");
    let login = s.login.clone();
    s.app
        .update(|cx| login.update(cx, |l, cx| l.answer_yes(false, cx)));
    s.wait(
        "failed",
        |l| matches!(&l.step, Step::Failed(why) if why.contains("proxy command was not run")),
    );
}

#[cfg(unix)]
#[test]
fn a_password_command_answers_the_password_question_after_it_is_approved() {
    let auth = Auth {
        password_command: Some(format!("printf {PASSWORD}")),
        ..Auth::default()
    };
    let mut s = scene_with(false, auth);
    s.asking("can't be established");
    s.say_yes();
    s.asking("to get its password");
    s.say_yes();
    s.connected();
}

#[cfg(unix)]
#[test]
fn a_refused_password_command_falls_through_to_asking_the_person() {
    let auth = Auth {
        password_command: Some(format!("printf {PASSWORD}")),
        ..Auth::default()
    };
    let mut s = scene_with(false, auth);
    s.asking("can't be established");
    s.say_yes();
    s.asking("to get its password");
    let login = s.login.clone();
    s.app
        .update(|cx| login.update(cx, |l, cx| l.answer_yes(false, cx)));
    s.asking("password");
    s.type_in(PASSWORD);
    s.connected();
}

#[cfg(unix)]
#[test]
fn a_failing_password_command_says_so_and_the_person_is_asked() {
    let auth = Auth {
        password_command: Some("echo nope >&2; exit 1".into()),
        ..Auth::default()
    };
    let mut s = scene_with(false, auth);
    s.asking("can't be established");
    s.say_yes();
    s.asking("to get its password");
    s.say_yes();
    s.asking("password");
    s.type_in(PASSWORD);
    s.connected();
}

#[test]
fn a_typed_password_is_offered_for_saving_even_with_no_vault_and_asks_for_a_new_passphrase() {
    let mut s = scene(false);
    s.asking("can't be established");
    s.say_yes();
    s.asking("password");
    s.type_in(PASSWORD);
    s.connected();
    s.wait("offered", |l| l.save_offer().is_some());
    let login = s.login.clone();
    s.app
        .update(|cx| login.update(cx, |l, cx| l.answer_offer(true, cx)));
    s.asking("New vault passphrase");
    s.type_in("vp9");
    s.asking("Repeat vault passphrase");
    s.type_in("vp9");
    s.wait("saved", |l| l.note() == Some("Saved in the vault."));
    s.wait("connected again", |l| matches!(l.step, Step::Connected));
}

#[test]
fn a_host_key_accepted_in_the_panel_is_saved_so_a_terminal_does_not_ask_again() {
    let mut s = scene(false);
    s.asking("can't be established");
    s.say_yes();
    s.asking("password");
    s.type_in(PASSWORD);
    s.connected();
    let saved = std::fs::read_to_string(s._dir.path().join("known_hosts")).unwrap();
    assert!(saved.contains("127.0.0.1"), "known_hosts: {saved:?}");
}
