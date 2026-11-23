// Adapted from russh russh/examples/client_exec_interactive.rs (Apache-2.0) for the
// PTY/shell/event loop, and from CrabPort crabport-ssh/src/backend.rs (Apache-2.0)
// for window_change handling.
//! Remote output on its way to the UI, coalesced and bounded. While the outbox is full the
//! session stops reading the channel, so russh stops reading the socket (backpressure).
//!
//! Round-trip time of the live connection, probed with our own `keepalive@openssh.com`
//! global request so it does not depend on the user's ServerAliveInterval (often off).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::oneshot;
use russh::ChannelMsg;
use russh::client::{self, Handle};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::connect::{AuthenticatedConnection, ConnectOptions};
use crate::error::{Cause, Failure, classify};
use crate::forward::{self, Registry};
use crate::sftp;
use crate::{
    ConnectSpec, Disconnect, Forward, ForwardError, ForwardInfo, SessionEvent, Sftp, SftpError,
    TermSize,
};

/// How long to wait for russh to say why a connection that went quiet has died.
const CAUSE_GRACE: Duration = Duration::from_millis(500);
/// After exit-status, how long to wait for the server to close the channel.
const EXIT_GRACE: Duration = Duration::from_secs(2);
const TERM: &str = "xterm-256color";

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
    size: Option<TermSize>,
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

async fn run_inner(
    spec: &ConnectSpec,
    size: Option<TermSize>,
    cmds: &mut mpsc::Receiver<Command>,
    events: &async_channel::Sender<SessionEvent>,
    cause: &Cause,
    handshaken: &mut bool,
    forwards: &Arc<Registry>,
) -> Result<Outcome, Failure> {
    let conn = AuthenticatedConnection::open(
        spec,
        ConnectOptions {
            // A files-only connection has no shell for a server to ask the agent through.
            allow_agent_forwarding: size.is_some(),
            remote_targets: forwards.remote_targets.clone(),
            cause: cause.clone(),
        },
        events,
    )
    .await?;

    *handshaken = true;
    let session = conn.session();
    let mut channel = None;
    if let Some(size) = size {
        let opened = session.channel_open_session().await?;
        if conn.agent_forwarding() {
            // No reply wanted: a refusal would arrive as a channel failure and read as a refused shell.
            opened.agent_forward(false).await?;
        }
        opened
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
        opened.request_shell(true).await?;
        channel = Some(opened);
    }
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
    // The UI stopped listening but something still holds the connection (a copy using its
    // SFTP): the shell goes, the connection stays until the last handle is dropped.
    let mut detached = false;
    let mut outbox = Outbox::new(events.clone());
    loop {
        tokio::select! {
            cmd = cmds.recv(), if !closing => match cmd {
                Some(Command::Write(bytes)) => {
                    if let Some(channel) = &channel {
                        channel.data_bytes(bytes).await?;
                    }
                }
                Some(Command::Resize(s)) => {
                    if let Some(channel) = &channel {
                        channel.window_change(s.cols.into(), s.rows.into(), s.pixel_width.into(), s.pixel_height.into()).await?;
                    }
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
                Some(Command::Close) | None if detached => break,
                Some(Command::Close) | None => {
                    closing = true;
                    if let Some(channel) = &channel {
                        let _ = channel.close().await;
                    }
                    deadline = Some(Instant::now() + EXIT_GRACE);
                }
            },
            delivered = outbox.progress(), if !detached && outbox.has_work() => {
                if !delivered {
                    detached = true;
                    if let Some(channel) = channel.take() {
                        let _ = channel.close().await;
                    }
                }
            }
            // Not reading while the outbox is full is the backpressure: see `Outbox`.
            msg = async {
                match channel.as_mut() {
                    Some(channel) => channel.wait().await,
                    None => std::future::pending().await,
                }
            }, if outbox.wants_input() => match msg {
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
        (None, None) if closing || detached => Disconnect::Local,
        // The shell channel ended with no exit status: the link died or the server hung up.
        (None, None) => cause
            .wait(CAUSE_GRACE)
            .await
            .unwrap_or(Disconnect::ServerClosed),
    };
    drop(_latency);
    conn.disconnect().await;
    Ok(Outcome {
        exit_status,
        error,
        reason,
    })
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
