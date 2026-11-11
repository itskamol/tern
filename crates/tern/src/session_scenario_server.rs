// Adapted from russh 0.64 examples/echoserver.rs (Apache-2.0) for the server handler shape.
//! The in-process SSH server the session scenario tests log in to: it takes one password, then
//! gives a shell that echoes what is typed and, for `sudo …`, asks for a password and checks
//! the line typed back. [`Typed`] records every line the shell received, so a test can say what
//! reached the server, and in which order, without reading the screen.
#![allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg};
use russh::{Channel, ChannelId, MethodKind, MethodSet};

/// The login password, and the one `sudo` accepts.
pub(crate) const PASSWORD: &str = "hunter2";
pub(super) const PROMPT: &str = "test@demo:~$ ";
/// What `sudo` asks.
pub(super) const SUDO_PROMPT: &str = "[sudo] password for test:";

/// Every line the shell received, passwords included, in order.
#[derive(Clone, Default)]
pub(crate) struct Typed(Arc<Mutex<Vec<String>>>);

impl Typed {
    pub(super) fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// The shell's line editing and its one password question. Pure: bytes in, bytes to show out.
#[derive(Default)]
pub(super) struct DemoShell {
    line: String,
    /// `sudo` has asked and the next line is the answer (not echoed).
    asking: bool,
}

impl DemoShell {
    /// What the terminal shows after `bytes` were typed: the echo, and the answer to a finished
    /// line.
    pub(super) fn input(&mut self, bytes: &[u8], typed: &Typed) -> Vec<u8> {
        let mut out = Vec::new();
        for &b in bytes {
            match b {
                b'\r' | b'\n' => {
                    out.extend_from_slice(b"\r\n");
                    let line = std::mem::take(&mut self.line);
                    typed.0.lock().unwrap().push(line.clone());
                    out.extend_from_slice(self.answer(&line).as_bytes());
                }
                0x7f if !self.line.is_empty() => {
                    self.line.pop();
                    if !self.asking {
                        out.extend_from_slice(b"\x08 \x08");
                    }
                }
                0x20..=0x7e => {
                    self.line.push(char::from(b));
                    if !self.asking {
                        out.push(b);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The first thing the shell shows at a started session.
    pub(super) fn greeting() -> &'static str {
        PROMPT
    }

    fn answer(&mut self, line: &str) -> String {
        if std::mem::take(&mut self.asking) {
            return if line == PASSWORD {
                format!("ok\r\n{PROMPT}")
            } else {
                format!("Sorry, try again.\r\n{PROMPT}")
            };
        }
        if line.starts_with("sudo ") {
            self.asking = true;
            return format!("{SUDO_PROMPT} ");
        }
        PROMPT.to_owned()
    }
}

#[derive(Clone)]
struct OnePassword {
    shell: Arc<Mutex<DemoShell>>,
    typed: Typed,
}

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
        s.data(ch, DemoShell::greeting().as_bytes().to_vec())?;
        Ok(())
    }

    async fn data(
        &mut self,
        ch: ChannelId,
        data: &[u8],
        s: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let out = self.shell.lock().unwrap().input(data, &self.typed);
        if !out.is_empty() {
            s.data(ch, out)?;
        }
        Ok(())
    }
}

/// A server on 127.0.0.1 taking only [`PASSWORD`], on its own runtime (kept alive by the
/// caller), and what its shells were typed.
pub(crate) fn serve() -> (tokio::runtime::Runtime, u16, Typed) {
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
    let typed = Typed::default();
    let seen = typed.clone();
    rt.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let config = config.clone();
            let handler = OnePassword {
                shell: Arc::default(),
                typed: seen.clone(),
            };
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, stream, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    (rt, port, typed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(shell: &mut DemoShell, typed: &Typed, bytes: &str) -> String {
        String::from_utf8(shell.input(bytes.as_bytes(), typed)).unwrap()
    }

    #[test]
    fn it_echoes_what_is_typed_and_answers_a_line_with_a_prompt() {
        let (mut shell, typed) = (DemoShell::default(), Typed::default());
        assert_eq!(shown(&mut shell, &typed, "ls\r"), format!("ls\r\n{PROMPT}"));
        assert_eq!(typed.lines(), ["ls"]);
    }

    #[test]
    fn sudo_asks_and_does_not_echo_the_answer_and_checks_it() {
        let (mut shell, typed) = (DemoShell::default(), Typed::default());
        assert_eq!(
            shown(&mut shell, &typed, "sudo true\r"),
            format!("sudo true\r\n{SUDO_PROMPT} ")
        );
        assert_eq!(
            shown(&mut shell, &typed, &format!("{PASSWORD}\r")),
            format!("\r\nok\r\n{PROMPT}")
        );
        shown(&mut shell, &typed, "sudo x\r");
        assert!(shown(&mut shell, &typed, "wrong\r").contains("Sorry, try again."));
        assert_eq!(typed.lines(), ["sudo true", PASSWORD, "sudo x", "wrong"]);
    }
}
