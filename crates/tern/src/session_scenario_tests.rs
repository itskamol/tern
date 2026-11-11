// Adapted from russh 0.64 examples/echoserver.rs (Apache-2.0) for the server handler shape.
//! A real `Session` in a test window, logging in to an in-process SSH server that takes one
//! password: the server's questions, tern's own (vault passphrase, save offer) and the order
//! they come in, as a person at the terminal meets them. No GUI, network or Keychain beyond
//! 127.0.0.1; the vault is a file in a temp dir.
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Entity, IntoElement, Render, TestApp};
use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tern_ssh::{ConnectSpec, SecretString};
use tern_vault::{Key, Vault};

use super::{Launch, Session, Status};
use crate::keeper::Keeper;
use crate::password_command::Auth as LinkAuth;
use crate::platform::SshRuntime;

pub(crate) const PASSWORD: &str = "hunter2";
const PROMPT: &str = "test@demo:~$ ";

#[derive(Clone)]
struct OnePassword;

impl server::Handler for OnePassword {
    type Error = russh::Error;

    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if password == PASSWORD {
            Auth::Accept
        } else {
            Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
                partial_success: false,
            }
        })
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        ch: ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        s: &mut server::Session,
    ) -> Result<(), Self::Error> {
        s.channel_success(ch)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        ch: ChannelId,
        s: &mut server::Session,
    ) -> Result<(), Self::Error> {
        s.channel_success(ch)?;
        s.data(ch, PROMPT.as_bytes().to_vec())?;
        Ok(())
    }
}

/// A server on 127.0.0.1 taking only [`PASSWORD`], on its own runtime (kept alive by the caller).
pub(crate) fn serve() -> (tokio::runtime::Runtime, u16) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let config = Arc::new(server::Config {
        methods: MethodSet::from(&[MethodKind::Password][..]),
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()],
        ..Default::default()
    });
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    rt.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let config = config.clone();
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, stream, OnePassword).await {
                    let _ = running.await;
                }
            });
        }
    });
    (rt, port)
}

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
    _server: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

/// A session to the test server whose vault (locked, passphrase `vp1`) holds the password.
fn scene() -> Scene {
    let (server, port) = serve();
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
