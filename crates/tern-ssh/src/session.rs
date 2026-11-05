// Adapted from russh russh/examples/client_exec_interactive.rs (Apache-2.0) for the
// PTY/shell/event loop, and from CrabPort crabport-ssh/src/backend.rs (Apache-2.0)
// for window_change handling and protocol-level keepalive.
//! Remote output on its way to the UI, coalesced and bounded. While the outbox is full the
//! session stops reading the channel, so russh stops reading the socket (backpressure).
//!
//! Round-trip time of the live connection, probed with our own `keepalive@openssh.com`
//! global request so it does not depend on the user's ServerAliveInterval (often off).

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::oneshot;
use russh::client::{self, Handle};
use russh::{ChannelMsg, Disconnect as SshDisconnect, Preferred};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::authn::{self, Authenticator};
use crate::config;
use crate::error::{Cause, Failure, classify};
use crate::forward::{self, Registry};
use crate::hostkey::{Asking, Handler, known_algorithms, within};
use crate::sftp;
use crate::{
    ConnectSpec, Disconnect, Forward, ForwardError, ForwardInfo, SessionEvent, Sftp, SftpError,
    TermSize,
};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use tokio::task::JoinHandle;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long to wait for russh to say why a connection that went quiet has died.
const CAUSE_GRACE: Duration = Duration::from_millis(500);
/// After exit-status, how long to wait for the server to close the channel.
const EXIT_GRACE: Duration = Duration::from_secs(2);
const TERM: &str = "xterm-256color";
/// Unread SSH messages russh buffers per channel before it stops reading the socket.
/// russh's default is 100; at up to 32 KiB a packet that is 3 MiB per session.
const CHANNEL_BUFFER: usize = 16;

#[derive(Debug)]
pub(crate) enum Command {
    Write(Vec<u8>),
    Resize(TermSize),
    /// Start a forward; the session answers on `reply`.
    Forward {
        forward: Forward,
        reply: oneshot::Sender<Result<ForwardInfo, ForwardError>>,
    },
    /// Open the SFTP subsystem; the session answers on `reply`.
    Sftp {
        reply: oneshot::Sender<Result<Sftp, SftpError>>,
    },
    /// Tell the server to stop a remote forward.
    CancelRemote {
        host: String,
        port: u16,
    },
    Close,
}

struct Outcome {
    exit_status: Option<u32>,
    error: Option<String>,
    reason: Disconnect,
}

pub(crate) async fn run(
    spec: ConnectSpec,
    size: TermSize,
    mut cmds: mpsc::Receiver<Command>,
    events: async_channel::Sender<SessionEvent>,
    forwards: Arc<Registry>,
) {
    let cause = Cause::default();
    let mut connected = false;
    let outcome = match run_inner(
        &spec,
        size,
        &mut cmds,
        &events,
        &cause,
        &mut connected,
        &forwards,
    )
    .await
    {
        Ok(o) => o,
        Err(f) => {
            tracing::warn!(host = %spec.host, port = spec.port, error = %f, "ssh_session_failed");
            // What russh saw on the dead link is more exact than the error it surfaced as.
            let reason = match connected {
                true => match cause.wait(CAUSE_GRACE).await {
                    Some(c) => c,
                    None => classify(&f, true),
                },
                false => classify(&f, false),
            };
            Outcome {
                exit_status: None,
                error: Some(f.to_string()),
                reason,
            }
        }
    };
    forwards.clear();
    tracing::info!(host = %spec.host, port = spec.port, exit_status = ?outcome.exit_status, reason = ?outcome.reason, "ssh_closed");
    let _ = events
        .send(SessionEvent::Closed {
            exit_status: outcome.exit_status,
            error: outcome.error,
            reason: outcome.reason,
        })
        .await;
}

/// The proxy's first stderr line, which says why it failed when it does.
#[derive(Clone, Default)]
struct ProxyReason(Arc<std::sync::Mutex<Option<String>>>);

impl ProxyReason {
    fn get(&self) -> Option<String> {
        self.0.lock().ok().and_then(|s| s.clone())
    }
}

/// The ProxyCommand wrapped in the platform shell: `sh -c` on Unix, `cmd /C` on Windows.
fn proxy_shell(cmd: &str) -> tokio::process::Command {
    #[cfg(unix)]
    {
        let mut shell = tokio::process::Command::new("sh");
        shell.arg("-c").arg(cmd);
        shell
    }
    #[cfg(windows)]
    {
        /// `CREATE_NO_WINDOW`: no console window flashes up for a GUI app.
        const NO_WINDOW: u32 = 0x0800_0000;
        let mut shell = tokio::process::Command::new("cmd");
        // `raw_arg`: cmd parses its own command line, so Rust's quoting must not touch it.
        shell.arg("/C").raw_arg(cmd).creation_flags(NO_WINDOW);
        shell
    }
}

/// ProxyCommand transport: the child's stdio is the byte stream.
fn spawn_proxy(
    cmd: &str,
) -> Result<
    (
        Child,
        impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        ProxyReason,
    ),
    Failure,
> {
    let mut shell = proxy_shell(cmd);
    let mut child = shell
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Failure::Proxy(e.to_string()))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(Failure::Proxy("stdio unavailable".into()));
    };
    let reason = ProxyReason::default();
    let first = reason.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(proxy_stderr = %line, "ssh_proxy_stderr");
            // The first line is usually the error; what follows tends to be usage text.
            if let Ok(mut slot) = first.0.lock()
                && slot.is_none()
                && !line.trim().is_empty()
            {
                *slot = Some(line.trim().to_owned());
            }
        }
    });
    Ok((child, tokio::io::join(stdout, stdin), reason))
}

/// The russh settings for one connection: keep-alive from the spec, and the host key types
/// already in known_hosts for `host:port` asked for first.
pub(crate) fn client_config(spec: &ConnectSpec, host: &str, port: u16) -> Arc<client::Config> {
    let mut preferred = Preferred::default();
    let known = known_algorithms(host, port, spec.known_hosts.as_deref());
    if !known.is_empty() {
        let mut keys = known;
        let rest: Vec<_> = preferred
            .key
            .iter()
            .filter(|a| !keys.contains(a))
            .cloned()
            .collect();
        keys.extend(rest);
        preferred.key = Cow::Owned(keys);
    }
    Arc::new(client::Config {
        // OpenSSH: an interval of 0 turns keep-alives off; a count of 0 never gives up (russh
        // reads 0 the same way).
        keepalive_interval: Some(spec.server_alive_interval).filter(|d| !d.is_zero()),
        keepalive_max: spec.server_alive_count_max as usize,
        nodelay: true,
        channel_buffer_size: CHANNEL_BUFFER,
        preferred,
        ..Default::default()
    })
}

async fn run_inner(
    spec: &ConnectSpec,
    size: TermSize,
    cmds: &mut mpsc::Receiver<Command>,
    events: &async_channel::Sender<SessionEvent>,
    cause: &Cause,
    handshaken: &mut bool,
    forwards: &Arc<Registry>,
) -> Result<Outcome, Failure> {
    let agent_socket = authn::socket_for(spec.forward_agent, spec.agent_socket.as_deref());
    let asking = Asking::default();
    let handler = Handler {
        host: spec.host.clone(),
        port: spec.port,
        known_hosts: spec.known_hosts.clone(),
        events: events.clone(),
        cause: cause.clone(),
        remote: forwards.remote_targets.clone(),
        agent_socket: agent_socket.clone(),
        asking: asking.clone(),
    };

    let cfg = client_config(spec, &spec.host, spec.port);

    tracing::info!(host = %spec.host, port = spec.port, user = %spec.user, "ssh_connecting");
    // Jump hosts are logged in to first; their connections stay open for the whole session.
    let mut _hops = Vec::new();
    let mut jump_stream = None;
    if !spec.proxy_jump.is_empty() {
        let chain = authn::open_jump_chain(spec, events).await?;
        _hops = chain.hops;
        jump_stream = Some(chain.stream);
    }
    let proxy = spec.proxy_command.as_deref();
    // Held for the whole session so the proxy process lives as long as the connection.
    let mut _proxy_child: Option<Child> = None;
    let mut proxy_reason: Option<ProxyReason> = None;
    let connecting = async {
        if let Some(stream) = jump_stream.take() {
            return Ok(client::connect_stream(cfg, stream, handler).await);
        }
        match proxy {
            Some(cmd) => {
                let cmd = config::expand_proxy_command(cmd, &spec.host, spec.port, &spec.user);
                tracing::debug!("ssh_proxy_command_start");
                let (child, stream, reason) = spawn_proxy(&cmd)?;
                _proxy_child = Some(child);
                proxy_reason = Some(reason);
                Ok(client::connect_stream(cfg, stream, handler).await)
            }
            None => Ok::<_, Failure>(
                client::connect(cfg, (spec.host.as_str(), spec.port), handler).await,
            ),
        }
    };
    let connected = within(CONNECT_TIMEOUT, &asking, connecting)
        .await
        .ok_or(Failure::ConnectTimeout)??;
    let mut session = match connected {
        Ok(s) => s,
        Err(e) => {
            return Err(proxy_exit(&mut _proxy_child, proxy_reason.as_ref())
                .unwrap_or_else(|| connect_failure(e, &spec.host, spec.port)));
        }
    };

    let identity_files = spec.identity_files.clone();
    Authenticator::new(&mut session, &spec.user, &spec.host, identity_files, events)
        .with_memory_keys(spec.memory_keys.clone())
        .run()
        .await?;

    *handshaken = true;
    let session = Arc::new(session);
    let mut channel = session.channel_open_session().await?;
    if agent_socket.is_some() {
        // No reply wanted: a refusal would arrive as a channel failure and read as a refused shell.
        channel.agent_forward(false).await?;
    }
    channel
        .request_pty(
            false,
            TERM,
            size.cols.into(),
            size.rows.into(),
            size.pixel_width.into(),
            size.pixel_height.into(),
            &[],
        )
        .await?;
    channel.request_shell(true).await?;
    events
        .send(SessionEvent::Connected)
        .await
        .map_err(|_| Failure::UiGone)?;
    tracing::info!(host = %spec.host, port = spec.port, "ssh_connected");
    // Started after auth, so a prompt never waits behind a probe; dropped with the session.
    let _latency = start_latency_probe(session.clone(), events.clone());

    let mut exit_status = None;
    let mut error = None;
    let mut signal = None;
    let mut deadline: Option<Instant> = None;
    let mut closing = false;
    let mut outbox = Outbox::new(events.clone());
    loop {
        tokio::select! {
            cmd = cmds.recv(), if !closing => match cmd {
                Some(Command::Write(bytes)) => channel.data_bytes(bytes).await?,
                Some(Command::Resize(s)) => {
                    channel.window_change(s.cols.into(), s.rows.into(), s.pixel_width.into(), s.pixel_height.into()).await?
                }
                Some(Command::Forward { forward, reply }) => {
                    let (session, forwards) = (session.clone(), forwards.clone());
                    tokio::spawn(async move {
                        let _ = reply.send(forward::start(&session, &forwards, forward).await);
                    });
                }
                Some(Command::Sftp { reply }) => {
                    let session = session.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(sftp::open(&session).await);
                    });
                }
                Some(Command::CancelRemote { host, port }) => {
                    let session = session.clone();
                    tokio::spawn(async move {
                        let _ = session.cancel_tcpip_forward(host, u32::from(port)).await;
                    });
                }
                Some(Command::Close) | None => {
                    closing = true;
                    let _ = channel.close().await;
                    deadline = Some(Instant::now() + EXIT_GRACE);
                }
            },
            delivered = outbox.progress(), if outbox.has_work() => {
                if !delivered { break; }
            }
            // Not reading while the outbox is full is the backpressure: see `Outbox`.
            msg = channel.wait(), if outbox.wants_input() => match msg {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    outbox.push(&data);
                }
                Some(ChannelMsg::ExitStatus { exit_status: code }) => {
                    exit_status = Some(code);
                    deadline.get_or_insert_with(|| Instant::now() + EXIT_GRACE);
                }
                Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                    error = Some(format!("terminated by signal {signal_name:?}"));
                    signal = Some(format!("{signal_name:?}"));
                    deadline.get_or_insert_with(|| Instant::now() + EXIT_GRACE);
                }
                Some(ChannelMsg::Failure) => {
                    error = Some("server refused the shell request".to_string());
                    break;
                }
                Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
            () = async { match deadline { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending().await } } => break,
        }
    }
    // Output that arrived just before exit (a final prompt, an error message) still reaches the UI.
    outbox.flush().await;
    let reason = match (exit_status, signal) {
        (Some(code), _) => Disconnect::Exited(code),
        (None, Some(name)) => Disconnect::Signaled(name),
        (None, None) if closing => Disconnect::Local,
        // The shell channel ended with no exit status: the link died or the server hung up.
        (None, None) => cause
            .wait(CAUSE_GRACE)
            .await
            .unwrap_or(Disconnect::ServerClosed),
    };
    disconnect(&session).await;
    Ok(Outcome {
        exit_status,
        error,
        reason,
    })
}

/// A proxy that died during the handshake explains the failure better than russh's
/// "Disconnected".
fn proxy_exit(child: &mut Option<Child>, reason: Option<&ProxyReason>) -> Option<Failure> {
    let status = child.as_mut()?.try_wait().ok()??;
    Some(Failure::Proxy(
        reason
            .and_then(ProxyReason::get)
            .unwrap_or_else(|| status.to_string()),
    ))
}

/// A connect error in words a person can act on, naming where tern tried to go.
pub(crate) fn connect_failure(e: Failure, host: &str, port: u16) -> Failure {
    match e {
        Failure::Io(io) => Failure::Unreachable(describe_io(&io, host, port)),
        other => other,
    }
}

pub(crate) fn describe_io(e: &std::io::Error, host: &str, port: u16) -> String {
    use std::io::ErrorKind as K;
    let text = e.to_string();
    match e.kind() {
        K::ConnectionRefused => {
            format!("{host}:{port} refused the connection (is an SSH server running there?)")
        }
        K::TimedOut => format!("{host}:{port} did not answer"),
        K::HostUnreachable | K::NetworkUnreachable => format!("no route to {host}"),
        K::ConnectionReset => format!("{host}:{port} closed the connection"),
        _ if text.contains("lookup address") || text.contains("nodename nor servname") => {
            format!("could not find host {host}")
        }
        _ => text,
    }
}

async fn disconnect(session: &Handle<Handler>) {
    let _ = session
        .disconnect(SshDisconnect::ByApplication, "", "en")
        .await;
}

#[cfg(test)]
mod describe_tests {
    use super::describe_io;
    use std::io::{Error, ErrorKind};

    #[test]
    fn common_failures_read_as_sentences_naming_the_target() {
        let refused = Error::from(ErrorKind::ConnectionRefused);
        assert_eq!(
            describe_io(&refused, "10.0.0.5", 2222),
            "10.0.0.5:2222 refused the connection (is an SSH server running there?)"
        );
        let dns =
            Error::other("failed to lookup address information: nodename nor servname provided");
        assert_eq!(
            describe_io(&dns, "nope.example", 22),
            "could not find host nope.example"
        );
        let odd = Error::other("something else");
        assert_eq!(describe_io(&odd, "h", 22), "something else");
    }
}

/// Largest coalesced chunk handed to the UI in one [`SessionEvent::Data`].
pub(crate) const MAX_CHUNK: usize = 64 * 1024;

/// Capacity of the event channel to the UI. Chunks are coalesced, so a short queue suffices.
pub(crate) const EVENT_QUEUE: usize = 8;

type InFlight = Pin<Box<dyn Future<Output = bool> + Send>>;

pub(crate) struct Outbox {
    tx: async_channel::Sender<SessionEvent>,
    buf: Vec<u8>,
    in_flight: Option<InFlight>,
}

impl fmt::Debug for Outbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outbox")
            .field("buffered", &self.buf.len())
            .field("sending", &self.in_flight.is_some())
            .finish()
    }
}

impl Outbox {
    pub(crate) fn new(tx: async_channel::Sender<SessionEvent>) -> Self {
        Self {
            tx,
            buf: Vec::new(),
            in_flight: None,
        }
    }

    /// Whether the session may read more from the SSH channel.
    pub(crate) fn wants_input(&self) -> bool {
        self.buf.len() < MAX_CHUNK
    }

    pub(crate) fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Whether [`Self::progress`] has anything to do.
    pub(crate) fn has_work(&self) -> bool {
        self.in_flight.is_some() || !self.buf.is_empty()
    }

    /// Delivers one chunk; `false` once the receiver is gone. Cancel-safe: the in-flight send
    /// lives in `self`, so the next call resumes it.
    pub(crate) async fn progress(&mut self) -> bool {
        if self.in_flight.is_none() {
            if self.buf.is_empty() {
                return true;
            }
            let chunk = std::mem::take(&mut self.buf);
            let tx = self.tx.clone();
            self.in_flight = Some(Box::pin(async move {
                tx.send(SessionEvent::Data(chunk)).await.is_ok()
            }));
        }
        let delivered = match self.in_flight.as_mut() {
            Some(send) => send.await,
            None => true,
        };
        self.in_flight = None;
        delivered
    }

    pub(crate) async fn flush(&mut self) -> bool {
        while self.has_work() {
            if !self.progress().await {
                return false;
            }
        }
        true
    }

    #[cfg(test)]
    fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// Gap between probes while the session is open.
const PROBE_EVERY: Duration = Duration::from_secs(5);
/// Weight of the newest sample: a spike shows within a probe or two without the figure jittering.
const ALPHA: f64 = 0.3;

/// Folds `sample` into the running figure (`None` before the first sample).
pub(crate) fn smooth(prev: Option<Duration>, sample: Duration) -> Duration {
    match prev {
        None => sample,
        Some(p) => {
            Duration::from_secs_f64(ALPHA * sample.as_secs_f64() + (1.0 - ALPHA) * p.as_secs_f64())
        }
    }
}

/// Aborts the probe task when the session ends.
pub(crate) struct Probe(JoinHandle<()>);

impl Drop for Probe {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts probing a logged-in session; each reply sends a [`SessionEvent::Latency`].
pub(crate) fn start_latency_probe<H: client::Handler + 'static>(
    session: Arc<Handle<H>>,
    events: async_channel::Sender<SessionEvent>,
) -> Probe {
    Probe(tokio::spawn(async move {
        let mut smoothed = None;
        loop {
            let sent = std::time::Instant::now();
            match session
                .send_global_request("keepalive@openssh.com", &[], true)
                .await
            {
                // Servers that do not know the request refuse it; the refusal is still a round trip.
                Ok(_) | Err(russh::Error::RequestDenied) => {}
                Err(_) => return,
            }
            let now = smooth(smoothed, sent.elapsed());
            smoothed = Some(now);
            if events.send(SessionEvent::Latency(now)).await.is_err() {
                return;
            }
            tokio::time::sleep(PROBE_EVERY).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn slow_consumer_keeps_buffer_bounded_and_preserves_order() {
        const READ: usize = 4 * 1024;
        const TOTAL_READS: usize = 400; // 1.6 MiB, 25x MAX_CHUNK

        let (tx, rx) = async_channel::bounded(EVENT_QUEUE);
        let consumer = tokio::spawn(async move {
            let mut got = Vec::new();
            while let Ok(ev) = rx.recv().await {
                if let SessionEvent::Data(d) = ev {
                    got.extend_from_slice(&d);
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            got
        });

        let mut outbox = Outbox::new(tx);
        let mut sent = Vec::new();
        let mut peak = 0;
        let mut reads = 0;
        while reads < TOTAL_READS || outbox.has_work() {
            tokio::select! {
                () = std::future::ready(()), if reads < TOTAL_READS && outbox.wants_input() => {
                    let read: Vec<u8> = (0..READ).map(|i| ((reads * READ + i) % 251) as u8).collect();
                    outbox.push(&read);
                    sent.extend_from_slice(&read);
                    reads += 1;
                }
                ok = outbox.progress(), if outbox.has_work() => assert!(ok),
            }
            peak = peak.max(outbox.buffered());
        }
        drop(outbox);

        let got = consumer.await.unwrap();
        assert!(peak < MAX_CHUNK + READ, "buffer peaked at {peak} bytes");
        assert_eq!(got.len(), sent.len());
        assert!(got == sent, "bytes reordered or corrupted");
    }

    #[tokio::test]
    async fn progress_reports_a_dropped_receiver() {
        let (tx, rx) = async_channel::bounded(1);
        drop(rx);
        let mut outbox = Outbox::new(tx);
        outbox.push(b"x");
        assert!(!outbox.progress().await);
    }

    #[test]
    fn the_first_sample_is_taken_as_is() {
        assert_eq!(
            smooth(None, Duration::from_millis(40)),
            Duration::from_millis(40)
        );
    }

    #[test]
    fn a_new_sample_moves_the_figure_three_tenths_of_the_way() {
        let up = smooth(Some(Duration::from_millis(100)), Duration::from_millis(200));
        assert_eq!(up.as_micros(), 130_000);
        let down = smooth(Some(Duration::from_millis(200)), Duration::from_millis(100));
        assert_eq!(down.as_micros(), 170_000);
    }
}
