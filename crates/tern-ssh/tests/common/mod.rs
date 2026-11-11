// Adapted from russh 0.64 examples/echoserver.rs, client_open_direct_tcpip.rs and
// sftp_server.rs (Apache-2.0) for the server handler shape and the direct-tcpip, tcpip-forward
// and sftp-subsystem plumbing.
//! An in-process SSH server (127.0.0.1 only) and a session driver shared by the integration tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod sftp;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tern_ssh::{ConnectSpec, Disconnect, Prompt, SessionEvent, SessionHandle, TermSize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// What the server's shell does once asked for.
#[derive(Clone, Copy)]
pub enum Shell {
    /// Says hi, reports this exit status and closes, as `exit` does.
    Exit(u32),
    /// Stays open and echoes what it is sent.
    Echo,
    /// Sends SSH_MSG_DISCONNECT.
    Hangup,
}

/// When the server tries to open an `auth-agent@openssh.com` channel back to the client.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AgentProbe {
    Never,
    /// Only after the client asked for agent forwarding.
    WhenAsked,
    /// Even if it did not: a hostile or buggy server.
    Always,
}

struct TestServer {
    shell: Shell,
    probe: AgentProbe,
    agent_asked: bool,
    /// What came back over the agent channel, or `refused`.
    agent_log: Arc<Mutex<Vec<String>>>,
    /// Every direct-tcpip request this server accepted, as `host:port`.
    opened: Arc<Mutex<Vec<String>>>,
    /// The channel the shell runs on; other channels (tunnels) are not echoed.
    shell_channel: Option<ChannelId>,
    /// Serve SFTP from this directory when asked for the subsystem.
    sftp_root: Option<PathBuf>,
    sessions: HashMap<ChannelId, Channel<Msg>>,
    /// `pty`, `shell` and `agent` requests received, in order.
    requests: Arc<Mutex<Vec<&'static str>>>,
}

impl server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_none(&mut self, _: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
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

    async fn agent_request(&mut self, _: ChannelId, _: &mut Session) -> Result<bool, Self::Error> {
        self.agent_asked = true;
        self.requests.lock().unwrap().push("agent");
        Ok(true)
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
        s: &mut Session,
    ) -> Result<(), Self::Error> {
        self.requests.lock().unwrap().push("pty");
        s.channel_success(ch)?;
        Ok(())
    }

    async fn shell_request(&mut self, ch: ChannelId, s: &mut Session) -> Result<(), Self::Error> {
        self.requests.lock().unwrap().push("shell");
        s.channel_success(ch)?;
        self.shell_channel = Some(ch);
        if self.probe == AgentProbe::Always
            || (self.probe == AgentProbe::WhenAsked && self.agent_asked)
        {
            let (h, log) = (s.handle(), self.agent_log.clone());
            tokio::spawn(async move {
                let entry = match h.channel_open_agent().await {
                    Ok(ch) => {
                        let mut st = ch.into_stream();
                        let _ = st.write_all(b"agent-ping").await;
                        let mut buf = [0u8; 10];
                        match tokio::time::timeout(Duration::from_secs(5), st.read_exact(&mut buf))
                            .await
                        {
                            Ok(Ok(_)) => String::from_utf8_lossy(&buf).into_owned(),
                            _ => "no answer".into(),
                        }
                    }
                    Err(_) => "refused".into(),
                };
                log.lock().unwrap().push(entry);
            });
        }
        match self.shell {
            Shell::Exit(code) => {
                s.data(ch, &b"hi\r\n"[..])?;
                s.exit_status_request(ch, code)?;
                s.eof(ch)?;
                s.close(ch)?;
            }
            Shell::Echo => {}
            Shell::Hangup => {
                let h = s.handle();
                tokio::spawn(async move {
                    let _ = h
                        .disconnect(russh::Disconnect::ByApplication, "bye".into(), "en".into())
                        .await;
                });
            }
        }
        Ok(())
    }

    async fn data(
        &mut self,
        ch: ChannelId,
        data: &[u8],
        s: &mut Session,
    ) -> Result<(), Self::Error> {
        if matches!(self.shell, Shell::Echo) && self.shell_channel == Some(ch) {
            s.data(ch, data.to_vec())?;
        }
        Ok(())
    }

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
        let target = format!("{host}:{port}");
        let Ok(tcp) = TcpStream::connect(&target).await else {
            reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };
        self.opened.lock().unwrap().push(target);
        reply.accept().await;
        tokio::spawn(pipe(tcp, channel));
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        s: &mut Session,
    ) -> Result<bool, Self::Error> {
        let Ok(listener) = TcpListener::bind(("127.0.0.1", *port as u16)).await else {
            return Ok(false);
        };
        *port = u32::from(listener.local_addr().unwrap().port());
        let (handle, address, bound) = (s.handle(), address.to_string(), *port);
        tokio::spawn(async move {
            while let Ok((tcp, peer)) = listener.accept().await {
                let Ok(ch) = handle
                    .channel_open_forwarded_tcpip(
                        address.clone(),
                        bound,
                        peer.ip().to_string(),
                        u32::from(peer.port()),
                    )
                    .await
                else {
                    continue;
                };
                tokio::spawn(pipe(tcp, ch));
            }
        });
        Ok(true)
    }
}

/// Copies both ways between a TCP stream and an SSH channel until either ends.
async fn pipe(mut tcp: TcpStream, channel: Channel<Msg>) {
    let mut ch = channel.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut ch).await;
}

pub struct Server {
    pub port: u16,
    /// How agent channels the server opened ended up: the bytes echoed back, or `refused`.
    pub agent_log: Arc<Mutex<Vec<String>>>,
    /// Direct-tcpip targets this server was asked to open.
    pub opened: Arc<Mutex<Vec<String>>>,
    /// `pty`, `shell` and `agent` requests received, in order.
    pub requests: Arc<Mutex<Vec<&'static str>>>,
}

pub async fn serve(shell: Shell) -> Server {
    serve_with(shell, AgentProbe::Never).await
}

pub async fn serve_with(shell: Shell, probe: AgentProbe) -> Server {
    serve_full(shell, probe, None).await
}

/// A server that also offers SFTP over `sftp_root`.
pub async fn serve_sftp(sftp_root: PathBuf) -> Server {
    serve_full(Shell::Echo, AgentProbe::Never, Some(sftp_root)).await
}

async fn serve_full(shell: Shell, probe: AgentProbe, sftp_root: Option<PathBuf>) -> Server {
    let config = Arc::new(server::Config {
        methods: MethodSet::from(&[MethodKind::None][..]),
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()],
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let opened = Arc::new(Mutex::new(Vec::new()));
    let log = opened.clone();
    let agent_log = Arc::new(Mutex::new(Vec::new()));
    let agent = agent_log.clone();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = TestServer {
                shell,
                opened: log.clone(),
                shell_channel: None,
                sftp_root: sftp_root.clone(),
                sessions: HashMap::new(),
                probe,
                agent_asked: false,
                agent_log: agent.clone(),
                requests: seen.clone(),
            };
            let config = config.clone();
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, stream, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    Server {
        port,
        agent_log,
        opened,
        requests,
    }
}

/// A TCP listener that echoes every byte back; returns its port.
pub async fn echo_listener() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

pub fn spec(port: u16, dir: &tempfile::TempDir) -> ConnectSpec {
    ConnectSpec {
        host: "127.0.0.1".into(),
        port,
        user: "tester".into(),
        known_hosts: Some(dir.path().join("known_hosts")),
        ..Default::default()
    }
}

/// A logged-in session whose prompts are answered (host keys accepted, secrets declined).
pub struct Live {
    pub handle: SessionHandle,
    pub events: async_channel::Receiver<SessionEvent>,
    /// `host:port` of every unknown-host-key prompt shown, in order.
    pub host_keys: Vec<String>,
}

pub fn size() -> TermSize {
    TermSize {
        cols: 80,
        rows: 24,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Connects and waits for the shell, or returns the close error.
pub async fn start(spec: ConnectSpec) -> Result<Live, (Option<String>, Disconnect)> {
    let (handle, events) = tern_ssh::connect(spec, size(), &tokio::runtime::Handle::current());
    let mut host_keys = Vec::new();
    let wait = async {
        loop {
            match events.recv().await.expect("session ended without Closed") {
                SessionEvent::Prompt(Prompt::UnknownHostKey {
                    host, port, reply, ..
                }) => {
                    host_keys.push(format!("{host}:{port}"));
                    let _ = reply.send(true);
                }
                SessionEvent::Prompt(_) => {}
                SessionEvent::Connected => return Ok(()),
                SessionEvent::Data(_) | SessionEvent::Latency(_) => {}
                SessionEvent::Closed { error, reason, .. } => return Err((error, reason)),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(20), wait)
        .await
        .expect("login timed out")?;
    Ok(Live {
        handle,
        events,
        host_keys,
    })
}

impl Live {
    /// Reads events until the session closes.
    pub async fn closed(&self) -> (Option<String>, Disconnect) {
        let wait = async {
            loop {
                if let SessionEvent::Closed { error, reason, .. } =
                    self.events.recv().await.expect("no Closed event")
                {
                    return (error, reason);
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), wait)
            .await
            .expect("session did not close")
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Pass,
    Freeze,
    Reset,
}

/// A TCP relay in front of a server that can go silent or reset, as a dead network does.
pub struct Proxy {
    pub port: u16,
    ctl: watch::Sender<Mode>,
}

impl Proxy {
    /// Keeps every connection open but stops moving bytes: no packets, no FIN, no RST.
    pub fn freeze(&self) {
        self.ctl.send_replace(Mode::Freeze);
    }

    /// Aborts every connection with a TCP RST.
    pub fn reset(&self) {
        self.ctl.send_replace(Mode::Reset);
    }
}

// SO_LINGER 0 is the point: closing with it sends an RST, which is what a reset network does.
#[allow(deprecated)]
pub async fn proxy(upstream: u16) -> Proxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (ctl, _) = watch::channel(Mode::Pass);
    let rx = ctl.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let mut ctl = rx.subscribe();
            tokio::spawn(async move {
                let Ok(mut server) = TcpStream::connect(("127.0.0.1", upstream)).await else {
                    return;
                };
                {
                    let (mut cr, mut cw) = client.split();
                    let (mut sr, mut sw) = server.split();
                    let (mut a, mut b) = ([0u8; 4096], [0u8; 4096]);
                    loop {
                        tokio::select! {
                            n = cr.read(&mut a) => match n {
                                Ok(n) if n > 0 => { if sw.write_all(&a[..n]).await.is_err() { break; } }
                                _ => break,
                            },
                            n = sr.read(&mut b) => match n {
                                Ok(n) if n > 0 => { if cw.write_all(&b[..n]).await.is_err() { break; } }
                                _ => break,
                            },
                            _ = ctl.changed() => {
                                if *ctl.borrow() == Mode::Freeze {
                                    while *ctl.borrow_and_update() != Mode::Reset {
                                        if ctl.changed().await.is_err() { break; }
                                    }
                                }
                                if *ctl.borrow() == Mode::Reset { break; }
                            }
                        }
                    }
                }
                if *ctl.borrow() == Mode::Reset {
                    let _ = client.set_linger(Some(Duration::ZERO));
                    let _ = server.set_linger(Some(Duration::ZERO));
                }
            });
        }
    });
    Proxy { port, ctl }
}
