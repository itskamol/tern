// Adapted from russh 0.64 examples/echoserver.rs (Apache-2.0) for the server handler shape.
//! Login paths a key-accepting server never reaches, driven against an in-process russh server.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Response, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tern_ssh::{ChallengePrompt, ConnectSpec, Prompt, SecretString, SessionEvent, TermSize};

#[derive(Clone)]
enum Want {
    Password(&'static str),
    Code(&'static str),
    /// Two steps like `AuthenticationMethods password,keyboard-interactive`: only the password
    /// is offered at first; once it is right, the code is.
    PasswordThenCode(&'static str, &'static str),
    Key(PublicKey),
}

#[derive(Clone)]
struct TestServer(Want);

impl server::Handler for TestServer {
    type Error = russh::Error;

    /// Rejects like OpenSSH: password stays on offer. russh's default drops it after one try.
    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(match self.0 {
            Want::Password(p) if p == password => Auth::Accept,
            Want::PasswordThenCode(p, _) if p == password => Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::KeyboardInteractive][..])),
                partial_success: true,
            },
            _ => Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
                partial_success: false,
            },
        })
    }

    async fn auth_publickey(
        &mut self,
        _: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        Ok(match &self.0 {
            Want::Key(k) if k.key_data() == key.public_key.key_data() => Auth::Accept,
            _ => Auth::reject(),
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let (Want::Code(code) | Want::PasswordThenCode(_, code)) = self.0 else {
            return Ok(Auth::reject());
        };
        Ok(match response {
            None => Auth::Partial {
                name: Cow::Borrowed(""),
                instructions: Cow::Borrowed(""),
                prompts: Cow::Owned(vec![(Cow::Borrowed("Verification code: "), false)]),
            },
            Some(mut r) => match r.next() {
                Some(answer) if answer.as_ref() == code.as_bytes() => Auth::Accept,
                _ => Auth::reject(),
            },
        })
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn shell_request(&mut self, ch: ChannelId, s: &mut Session) -> Result<(), Self::Error> {
        s.channel_success(ch)?;
        Ok(())
    }
}

/// Starts a server that accepts only `want`, over only `method`, and returns its port.
async fn serve(want: Want, method: MethodKind) -> u16 {
    let config = Arc::new(server::Config {
        methods: MethodSet::from(&[method][..]),
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()],
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (config, handler) = (config.clone(), TestServer(want.clone()));
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, stream, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    port
}

#[derive(Debug, PartialEq)]
enum Ask {
    HostKey,
    Password,
    Passphrase,
    Challenge(Vec<ChallengePrompt>),
}

/// Logs in, answering secret prompts from `answers` in order (`None` cancels).
/// Returns every prompt seen and the close error, `None` meaning the shell opened.
async fn login(
    port: u16,
    identity_files: Vec<PathBuf>,
    answers: &[Option<&str>],
) -> (Vec<Ask>, Option<String>) {
    login_with(port, identity_files, Vec::new(), answers).await
}

async fn login_with(
    port: u16,
    identity_files: Vec<PathBuf>,
    memory_keys: Vec<tern_ssh::MemoryKey>,
    answers: &[Option<&str>],
) -> (Vec<Ask>, Option<String>) {
    let dir = tempfile::tempdir().unwrap();
    let spec = ConnectSpec {
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        identity_files,
        proxy_command: None,
        known_hosts: Some(dir.path().join("known_hosts")),
        memory_keys,
        ..Default::default()
    };
    let size = TermSize {
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
    };
    let (handle, events) = tern_ssh::connect(spec, size, &tokio::runtime::Handle::current());
    let mut answers = answers.iter().map(|a| a.map(SecretString::from));
    let mut asked = Vec::new();
    let run = async {
        loop {
            match events.recv().await.expect("session ended without Closed") {
                SessionEvent::Prompt(Prompt::UnknownHostKey { reply, .. }) => {
                    asked.push(Ask::HostKey);
                    let _ = reply.send(true);
                }
                SessionEvent::Prompt(Prompt::Password { reply, .. }) => {
                    asked.push(Ask::Password);
                    let _ = reply.send(answers.next().flatten());
                }
                SessionEvent::Prompt(Prompt::Challenge { prompts, reply, .. }) => {
                    let answer = answers.next().flatten().map(|a| vec![a; prompts.len()]);
                    asked.push(Ask::Challenge(prompts));
                    let _ = reply.send(answer);
                }
                SessionEvent::Prompt(Prompt::KeyPassphrase { reply, .. }) => {
                    asked.push(Ask::Passphrase);
                    let _ = reply.send(answers.next().flatten());
                }
                SessionEvent::Connected => handle.close(),
                SessionEvent::Data(_) | SessionEvent::Latency(_) => {}
                SessionEvent::Closed { error, .. } => return error,
            }
        }
    };
    let error = tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("login timed out");
    (asked, error)
}

fn unused_key(dir: &tempfile::TempDir) -> Vec<PathBuf> {
    let path = dir.path().join("id_unused");
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .unwrap()
        .write_openssh_file(&path, LineEnding::LF)
        .unwrap();
    vec![path]
}

#[tokio::test(flavor = "multi_thread")]
async fn password_is_retried_after_a_wrong_one() {
    let port = serve(Want::Password("hunter2"), MethodKind::Password).await;
    let dir = tempfile::tempdir().unwrap();
    let (asked, error) = login(port, unused_key(&dir), &[Some("wrong"), Some("hunter2")]).await;
    assert_eq!(error, None);
    assert_eq!(asked, [Ask::HostKey, Ask::Password, Ask::Password]);
}

#[tokio::test(flavor = "multi_thread")]
async fn password_gives_up_after_three_wrong_answers() {
    let port = serve(Want::Password("hunter2"), MethodKind::Password).await;
    let dir = tempfile::tempdir().unwrap();
    let (asked, error) = login(port, unused_key(&dir), &[Some("a"), Some("b"), Some("c")]).await;
    assert!(error.unwrap().contains("permission denied"));
    assert_eq!(asked.iter().filter(|a| **a == Ask::Password).count(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_the_password_prompt_ends_the_login() {
    let port = serve(Want::Password("hunter2"), MethodKind::Password).await;
    let dir = tempfile::tempdir().unwrap();
    let (_, error) = login(port, unused_key(&dir), &[None]).await;
    assert!(error.unwrap().contains("cancelled"));
}

#[tokio::test(flavor = "multi_thread")]
async fn keyboard_interactive_shows_the_servers_prompt() {
    let port = serve(Want::Code("123456"), MethodKind::KeyboardInteractive).await;
    let dir = tempfile::tempdir().unwrap();
    let (asked, error) = login(port, unused_key(&dir), &[Some("123456")]).await;
    assert_eq!(error, None);
    let code = ChallengePrompt {
        text: "Verification code: ".into(),
        echo: false,
    };
    assert_eq!(asked, [Ask::HostKey, Ask::Challenge(vec![code])]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_password_then_a_code_asks_for_the_code_after_the_password() {
    let port = serve(
        Want::PasswordThenCode("hunter2", "123456"),
        MethodKind::Password,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let (asked, error) = login(port, unused_key(&dir), &[Some("hunter2"), Some("123456")]).await;
    assert_eq!(error, None);
    let code = ChallengePrompt {
        text: "Verification code: ".into(),
        echo: false,
    };
    assert_eq!(
        asked,
        [Ask::HostKey, Ask::Password, Ask::Challenge(vec![code])]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_code_is_asked_for_again_not_the_password() {
    let port = serve(
        Want::PasswordThenCode("hunter2", "123456"),
        MethodKind::Password,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    // Password, wrong code, then the code again (never the password, which already passed).
    let answers = [Some("hunter2"), Some("000000"), None];
    let (asked, error) = login(port, unused_key(&dir), &answers).await;
    assert!(error.unwrap().contains("cancelled"));
    let kinds: Vec<&str> = asked
        .iter()
        .map(|a| match a {
            Ask::HostKey => "host",
            Ask::Password => "password",
            Ask::Passphrase => "passphrase",
            Ask::Challenge(_) => "code",
        })
        .collect();
    assert_eq!(kinds, ["host", "password", "code", "code"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_key_is_unlocked_after_a_wrong_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let path = dir.path().join("id_locked");
    key.encrypt(&mut rand::rng(), "correct horse")
        .unwrap()
        .write_openssh_file(&path, LineEnding::LF)
        .unwrap();
    let port = serve(Want::Key(key.public_key().clone()), MethodKind::PublicKey).await;
    let (asked, error) = login(port, vec![path], &[Some("nope"), Some("correct horse")]).await;
    assert_eq!(error, None);
    assert_eq!(asked, [Ask::HostKey, Ask::Passphrase, Ask::Passphrase]);
}

fn memory_key(name: &str, key: &PrivateKey) -> tern_ssh::MemoryKey {
    tern_ssh::MemoryKey {
        name: name.into(),
        openssh: SecretString::from(key.to_openssh(LineEnding::LF).unwrap().to_string()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_held_in_memory_logs_in_without_any_prompt() {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let port = serve(Want::Key(key.public_key().clone()), MethodKind::PublicKey).await;
    let dir = tempfile::tempdir().unwrap();
    let wrong = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    // The first key is refused by the server; the second is the one it wants.
    let keys = vec![memory_key("wrong", &wrong), memory_key("right", &key)];
    let (asked, error) = login_with(port, unused_key(&dir), keys, &[]).await;
    assert_eq!(error, None);
    assert_eq!(asked, [Ask::HostKey]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_memory_key_the_server_does_not_know_falls_through_to_the_password() {
    let port = serve(Want::Password("hunter2"), MethodKind::Password).await;
    let dir = tempfile::tempdir().unwrap();
    let other = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let keys = vec![memory_key("other", &other)];
    let (asked, error) = login_with(port, unused_key(&dir), keys, &[Some("hunter2")]).await;
    assert_eq!(error, None);
    assert_eq!(asked, [Ask::HostKey, Ask::Password]);
}
