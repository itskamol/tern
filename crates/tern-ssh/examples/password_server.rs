// Adapted from russh 0.64 examples/echoserver.rs (Apache-2.0) for the server handler shape.
//! A local SSH server that accepts one password, for checking tern's login flow by hand or
//! from a script. Never expose it: it binds 127.0.0.1 only.
//!
//! `cargo run -p tern-ssh --example password_server -- 2299 hunter2`
//!
//! With `--sftp DIR` it also serves the `sftp` subsystem over DIR (`/` is DIR, the login
//! directory is DIR/home), the same file service tern-ssh's tests use.
//!
//! With `--sudo` the shell prints `[sudo] password for test: ` after login, reads one line
//! without echo and checks it against the login password (`sudo: ok` or `Sorry, try again.`),
//! for checking tern's "Fill password" shortcut.
//!
//! With `--open` any client logs in with no credentials (the `none` method), so other SSH
//! apps can be measured connected without typing a password into them.
//!
//! With `--demo` the shell is a canned Ubuntu box (`web-01`, all data made up) with a MOTD, a
//! coloured prompt and a table of commands (`docker ps`, `htop`, `df -h`, `tail -f`, `sudo ...`),
//! for taking README screenshots of tern. `osc52` asks the terminal to set the clipboard (OSC 52)
//! and `flood [lines]` prints a burst of long, wrapping log
//! lines (20,000 by default) for measuring a terminal under load (docs/perf.md).
//!
//! With `--otp 123456` it behaves like a 2FA server: the password is only the first step
//! (partial success), then a keyboard-interactive "Verification code:" prompt with echo off
//! must be answered with the code.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PrivateKey;
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};

#[path = "../tests/common/sftp.rs"]
mod sftp;

#[path = "demo/shell.rs"]
mod demo;

struct PasswordServer {
    password: Arc<String>,
    otp: Option<Arc<String>>,
    /// The password step passed on this connection; the code is asked only after it.
    password_ok: bool,
    /// Served as `/` for the sftp subsystem, when asked for.
    sftp_root: Option<PathBuf>,
    /// Print a sudo prompt after login and check the reply (`--sudo`).
    sudo: bool,
    /// What has been typed at the sudo prompt so far.
    typed: Vec<u8>,
    /// The canned shell (`--demo`).
    demo: bool,
    /// Let anyone in with no credentials (`--open`), for benchmarking other clients.
    open: bool,
    user: String,
    shell: demo::Shell,
    /// Session channels not yet claimed by a subsystem.
    sessions: HashMap<ChannelId, Channel<Msg>>,
    /// The channel the shell runs on. Bytes on any other (an SFTP channel) are not typing.
    shell_channel: Option<ChannelId>,
}

impl server::Handler for PasswordServer {
    type Error = russh::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        eprintln!("auth_none user={user} accepted={}", self.open);
        self.user = user.to_owned();
        Ok(if self.open {
            Auth::Accept
        } else {
            Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }
        })
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        let ok = password == self.password.as_str();
        eprintln!("auth_password user={user} accepted={ok}");
        self.user = user.to_owned();
        Ok(if ok && self.otp.is_some() {
            self.password_ok = true;
            Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::KeyboardInteractive][..])),
                partial_success: true,
            }
        } else if ok {
            Auth::Accept
        } else {
            Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
                partial_success: false,
            }
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let (Some(code), true) = (self.otp.clone(), self.password_ok) else {
            return Ok(Auth::reject());
        };
        let Some(mut response) = response else {
            return Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: vec![("Verification code: ".into(), false)].into(),
            });
        };
        let given = response.next().unwrap_or_default();
        let ok = given.as_ref() == code.as_bytes();
        eprintln!("auth_keyboard_interactive user={user} accepted={ok}");
        Ok(if ok { Auth::Accept } else { Auth::reject() })
    }

    /// Carries `-L` / `-D` connections: dials the destination from here and bridges it, so a
    /// forward can be checked end to end against 127.0.0.1.
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _: &str,
        _: u32,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        let dest = (host.to_owned(), port as u16);
        match tokio::net::TcpStream::connect(dest).await {
            Ok(mut tcp) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
                });
            }
            Err(e) => eprintln!("direct-tcpip {host}:{port} failed: {e}"),
        }
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.sessions.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        ch: ChannelId,
        name: &str,
        s: &mut Session,
    ) -> Result<(), Self::Error> {
        match (name, self.sftp_root.clone(), self.sessions.remove(&ch)) {
            ("sftp", Some(root), Some(channel)) => {
                s.channel_success(ch)?;
                tokio::spawn(russh_sftp::server::run(
                    channel.into_stream(),
                    sftp::FsHandler::new(root),
                ));
            }
            _ => s.channel_failure(ch)?,
        }
        Ok(())
    }

    async fn shell_request(&mut self, ch: ChannelId, s: &mut Session) -> Result<(), Self::Error> {
        s.channel_success(ch)?;
        self.shell_channel = Some(ch);
        if self.demo {
            let out = self.shell.start(&self.user);
            s.data(ch, out)?;
            return Ok(());
        }
        s.data(ch, &b"password_server: logged in\r\n"[..])?;
        if self.sudo {
            s.data(ch, &b"[sudo] password for test: "[..])?;
        }
        Ok(())
    }

    async fn data(
        &mut self,
        ch: ChannelId,
        data: &[u8],
        s: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.shell_channel != Some(ch) {
            return Ok(());
        }
        if self.demo {
            let (out, close) = self.shell.feed(&self.user, &self.password, data);
            if !out.is_empty() {
                s.data(ch, out)?;
            }
            if close {
                s.eof(ch)?;
                s.close(ch)?;
            }
            return Ok(());
        }
        if !self.sudo {
            return Ok(());
        }
        for &b in data {
            if b != b'\r' && b != b'\n' {
                self.typed.push(b);
                continue;
            }
            let ok = self.typed == self.password.as_bytes();
            eprintln!("sudo reply accepted={ok} length={}", self.typed.len());
            self.typed.clear();
            let reply: &[u8] = if ok {
                b"\r\nsudo: ok\r\n"
            } else {
                b"\r\nSorry, try again.\r\n[sudo] password for test: "
            };
            s.data(ch, reply)?;
        }
        Ok(())
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let otp = args.iter().position(|a| a == "--otp").map(|i| {
        let code = args.get(i + 1).cloned().expect("--otp needs a code");
        args.drain(i..=i + 1);
        Arc::new(code)
    });
    let sudo = args
        .iter()
        .position(|a| a == "--sudo")
        .map(|i| args.remove(i))
        .is_some();
    let demo = args
        .iter()
        .position(|a| a == "--demo")
        .map(|i| args.remove(i))
        .is_some();
    let open = args
        .iter()
        .position(|a| a == "--open")
        .map(|i| args.remove(i))
        .is_some();
    let sftp_root = args.iter().position(|a| a == "--sftp").map(|i| {
        let dir = PathBuf::from(args.get(i + 1).cloned().expect("--sftp needs a directory"));
        args.drain(i..=i + 1);
        std::fs::create_dir_all(dir.join("home")).expect("create the sftp home");
        dir
    });
    let mut args = args.into_iter();
    let port: u16 = args.next().map_or(2299, |p| p.parse().expect("port"));
    let password = Arc::new(args.next().unwrap_or_else(|| "hunter2".into()));
    let config = Arc::new(server::Config {
        // Only the password is offered at first; the code is offered after it, as sshd's
        // `AuthenticationMethods password,keyboard-interactive` does.
        methods: if open {
            MethodSet::from(&[MethodKind::None, MethodKind::Password][..])
        } else {
            MethodSet::from(&[MethodKind::Password][..])
        },
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        // A fixed key, so a restarted server is the same host to known_hosts (reconnect checks).
        keys: vec![PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[7; 32]),
        )],
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    eprintln!("password_server listening on 127.0.0.1:{port}");
    while let Ok((stream, _)) = listener.accept().await {
        let handler = PasswordServer {
            password: password.clone(),
            otp: otp.clone(),
            password_ok: false,
            sftp_root: sftp_root.clone(),
            sudo,
            demo,
            open,
            user: String::new(),
            shell: demo::Shell::default(),
            typed: Vec::new(),
            sessions: HashMap::new(),
            shell_channel: None,
        };
        let config = config.clone();
        tokio::spawn(async move {
            if let Ok(running) = server::run_stream(config, stream, handler).await {
                let _ = running.await;
            }
        });
    }
}
