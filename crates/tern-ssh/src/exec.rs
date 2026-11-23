//! Short-lived, isolated, non-PTY SSH `exec` channel built on [`crate::connect`].
//!
//! Each call opens its own [`AuthenticatedConnection`] with agent forwarding and port forwards
//! disabled, runs one remote command with immediate stdin EOF, bounds stdout and stderr to
//! [`EXEC_OUTPUT_CAP`] each, enforces a wall-clock timeout, and disconnects cleanly on exit,
//! error, timeout, or caller cancellation.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use russh::ChannelMsg;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::connect::{AuthenticatedConnection, CONNECT_TIMEOUT, ConnectOptions};
use crate::error::{Cause, Disconnect, Failure};
use crate::{ConnectSpec, Prompt, SessionEvent};

/// Default wall-clock limit covering connection setup and command execution.
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(15);
/// Minimum permitted wall-clock timeout for [`exec`].
pub const MIN_EXEC_TIMEOUT: Duration = Duration::from_secs(1);
/// Maximum permitted wall-clock timeout for [`exec`].
pub const MAX_EXEC_TIMEOUT: Duration = Duration::from_secs(60);
/// Independent byte cap for each of `stdout` and `stderr` captured by [`exec`].
pub const EXEC_OUTPUT_CAP: usize = 64 * 1024;

/// RFC 4254 §5.2 `SSH_EXTENDED_DATA_STDERR`.
const SSH_EXTENDED_DATA_STDERR: u32 = 1;
/// Capacity of the internal/public exec event queues (at most one prompt is in flight at once).
const EXEC_EVENT_QUEUE: usize = 4;
/// Grace window after `exit-status` / `exit-signal` for the server to close the channel.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long to wait for russh to report why an unexpectedly dropped connection ended.
const CAUSE_GRACE: Duration = Duration::from_millis(500);
/// Upper bound on clean SSH disconnect + proxy child shutdown before dropping the transport.
const DISCONNECT_GRACE: Duration = Duration::from_millis(500);

/// Configuration for a single isolated non-PTY [`exec`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOptions {
    /// Wall-clock limit covering connection setup and command execution.
    /// Defaults to [`DEFAULT_EXEC_TIMEOUT`] (15 s) and is clamped to
    /// [`MIN_EXEC_TIMEOUT`]..=[`MAX_EXEC_TIMEOUT`] (1..=60 s).
    pub timeout: Duration,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_EXEC_TIMEOUT,
        }
    }
}

impl ExecOptions {
    /// Creates options with the given wall-clock `timeout` (clamped when executed).
    #[must_use]
    pub const fn with_timeout(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Returns `timeout` clamped to [`MIN_EXEC_TIMEOUT`]..=[`MAX_EXEC_TIMEOUT`].
    #[must_use]
    pub fn effective_timeout(&self) -> Duration {
        self.timeout.clamp(MIN_EXEC_TIMEOUT, MAX_EXEC_TIMEOUT)
    }
}

/// Output and termination status from one isolated non-PTY [`exec`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    /// Exit status reported by the remote process (`None` if closed without an exit status).
    pub exit_status: Option<u32>,
    /// Signal name if the remote process was terminated by a signal.
    pub exit_signal: Option<String>,
    /// Captured standard output, capped at [`EXEC_OUTPUT_CAP`] bytes.
    pub stdout: Vec<u8>,
    /// Captured standard error, capped at [`EXEC_OUTPUT_CAP`] bytes.
    pub stderr: Vec<u8>,
    /// `true` if more than [`EXEC_OUTPUT_CAP`] bytes arrived on stdout and excess was discarded.
    pub stdout_truncated: bool,
    /// `true` if more than [`EXEC_OUTPUT_CAP`] bytes arrived on stderr and excess was discarded.
    pub stderr_truncated: bool,
}

/// Why an isolated non-PTY [`exec`] run did not complete.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    /// The server's host key is not in `known_hosts` and the prompt was not accepted.
    #[error("{}", format_with_hop("unknown host key", .hop.as_ref()))]
    UnknownHostKey {
        /// Set when the unknown key belonged to a `ProxyJump` hop.
        hop: Option<(String, u16)>,
    },
    /// The server's host key does not match `known_hosts` (hard stop).
    #[error("{}", format_host_key_changed(*.line, .hop.as_ref()))]
    HostKeyChanged {
        line: usize,
        /// Set when the changed key belonged to a `ProxyJump` hop.
        hop: Option<(String, u16)>,
    },
    /// Authentication required an interactive prompt that was declined or cancelled.
    #[error("{}", format_with_hop("interactive authentication required", .hop.as_ref()))]
    InteractiveAuthRequired {
        /// Set when the prompt belonged to a `ProxyJump` hop.
        hop: Option<(String, u16)>,
    },
    /// The server rejected every authentication method tried.
    #[error("{}", format_with_hop("authentication failed", .hop.as_ref()))]
    AuthFailed {
        /// Set when authentication failed on a `ProxyJump` hop.
        hop: Option<(String, u16)>,
    },
    /// Connection setup and command execution exceeded the wall-clock timeout.
    #[error("command timed out after {}", format_duration(*.timeout))]
    Timeout {
        /// The wall-clock limit that expired.
        timeout: Duration,
    },
    /// The caller cancelled the run before it finished.
    #[error("command cancelled")]
    Cancelled,
    /// The server refused the `exec` channel request.
    #[error("server refused exec request")]
    Refused,
    /// Transport, proxy, jump-host, or protocol failure.
    #[error("{0}")]
    Ssh(String),
}

fn format_with_hop(base: &str, hop: Option<&(String, u16)>) -> String {
    match hop {
        Some((host, port)) => format!("{base} for jump host {host}:{port}"),
        None => base.to_owned(),
    }
}

fn format_host_key_changed(line: usize, hop: Option<&(String, u16)>) -> String {
    match hop {
        Some((host, port)) => {
            format!("host key changed for jump host {host}:{port} at known_hosts line {line}")
        }
        None => format!("host key changed at known_hosts line {line}"),
    }
}

fn format_duration(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Events emitted by a running [`Exec`].
#[derive(Debug)]
pub enum ExecEvent {
    /// Connection setup or authentication needs an answer; dropping the reply declines/cancels.
    Prompt(Prompt),
    /// The final outcome of the run. Always the last event delivered.
    Finished(std::result::Result<ExecOutput, ExecError>),
}

/// Cloneable cancellation trigger for a running [`Exec`].
///
/// Dropping an `ExecCancel` alone does not cancel the command; call [`Self::cancel`] or drop
/// the owning [`Exec`].
#[derive(Debug, Clone)]
pub struct ExecCancel {
    tx: watch::Sender<bool>,
}

impl ExecCancel {
    /// Signals the running [`Exec`] to abort and tear down its SSH connection.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }
}

struct CancelSignal(watch::Receiver<bool>);

impl CancelSignal {
    async fn wait(&mut self) {
        let _ = self.0.wait_for(|&cancelled| cancelled).await;
    }
}

/// Drives one isolated non-PTY remote command running on a tokio runtime.
///
/// Executor-agnostic: [`Self::next`], [`Self::run`], and [`Self::run_headless`] may be awaited
/// from any async executor (including GPUI) without a tokio runtime context on the calling thread.
/// Dropping `Exec` cancels the command and tears down its SSH connection, jump hops, and any
/// `ProxyCommand` child process.
#[must_use = "dropping an Exec cancels the running command"]
pub struct Exec {
    events: async_channel::Receiver<ExecEvent>,
    cancel: ExecCancel,
    finished: bool,
}

impl Drop for Exec {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl fmt::Debug for Exec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Exec")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl Exec {
    /// Returns a cloneable handle that can cancel this run from another task or callback.
    #[must_use]
    pub fn cancel_handle(&self) -> ExecCancel {
        self.cancel.clone()
    }

    /// Cancels the running command; the next [`Self::next`] delivers
    /// [`ExecEvent::Finished`] with [`ExecError::Cancelled`] once teardown finishes.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Awaits the next [`ExecEvent`] (zero or more [`ExecEvent::Prompt`]s followed by one
    /// [`ExecEvent::Finished`]). Returns `None` once [`ExecEvent::Finished`] has been delivered.
    pub async fn next(&mut self) -> Option<ExecEvent> {
        if self.finished {
            return None;
        }
        let event = self.events.recv().await.ok()?;
        if matches!(event, ExecEvent::Finished(_)) {
            self.finished = true;
        }
        Some(event)
    }

    /// Drives the run to completion by passing each [`Prompt`] to `on_prompt`.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError`] if connection setup, host-key verification, authentication,
    /// command execution, cancellation, or the wall-clock timeout fails.
    pub async fn run<F, Fut>(
        mut self,
        mut on_prompt: F,
    ) -> std::result::Result<ExecOutput, ExecError>
    where
        F: FnMut(Prompt) -> Fut,
        Fut: Future<Output = ()>,
    {
        while let Some(event) = self.next().await {
            match event {
                ExecEvent::Prompt(prompt) => on_prompt(prompt).await,
                ExecEvent::Finished(result) => return result,
            }
        }
        Err(ExecError::Cancelled)
    }

    /// Runs to completion in strict headless mode, rejecting every interactive prompt.
    ///
    /// # Errors
    ///
    /// Returns [`ExecError::UnknownHostKey`] for an unknown host key,
    /// [`ExecError::InteractiveAuthRequired`] when any authentication prompt is needed, or
    /// another [`ExecError`] on changed host key, timeout, cancellation, or SSH failure.
    pub async fn run_headless(self) -> std::result::Result<ExecOutput, ExecError> {
        // Dropping each Prompt closes its reply channel, rejecting unknown host keys and
        // cancelling interactive authentication prompts deterministically.
        self.run(|prompt| async move { drop(prompt) }).await
    }
}

/// Starts one isolated non-PTY remote command on `rt`.
///
/// Always dials a fresh connection with `forward_agent = false`, zero port forwards, no PTY,
/// and immediate stdin EOF. Captures stdout and stderr independently up to [`EXEC_OUTPUT_CAP`]
/// bytes each and enforces a wall-clock timeout covering connection setup and command execution.
#[must_use = "dropping an Exec cancels the running command"]
pub fn exec(
    spec: ConnectSpec,
    command: impl Into<String>,
    options: ExecOptions,
    rt: &tokio::runtime::Handle,
) -> Exec {
    let command = command.into();
    let (etx, erx) = async_channel::bounded(EXEC_EVENT_QUEUE);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    rt.spawn(async move {
        let mut cancel = CancelSignal(cancel_rx);
        let result = run_exec(spec, command, options, &etx, &mut cancel).await;
        let _ = etx.send(ExecEvent::Finished(result)).await;
    });
    Exec {
        events: erx,
        cancel: ExecCancel { tx: cancel_tx },
        finished: false,
    }
}

async fn run_exec(
    spec: ConnectSpec,
    command: String,
    options: ExecOptions,
    exec_tx: &async_channel::Sender<ExecEvent>,
    cancel: &mut CancelSignal,
) -> std::result::Result<ExecOutput, ExecError> {
    let timeout = options.effective_timeout();
    let deadline = Instant::now() + timeout;
    tracing::info!(
        host = %spec.host,
        port = spec.port,
        timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        "ssh_exec_start"
    );

    let (session_tx, session_rx) = async_channel::bounded::<SessionEvent>(EXEC_EVENT_QUEUE);
    let connect_options = ConnectOptions::default();
    let cause = connect_options.cause.clone();
    let conn = {
        let open = AuthenticatedConnection::open(&spec, connect_options, &session_tx);
        tokio::pin!(open);
        loop {
            tokio::select! {
                biased;
                () = cancel.wait() => {
                    tracing::info!(host = %spec.host, port = spec.port, "ssh_exec_cancelled");
                    return Err(ExecError::Cancelled);
                }
                () = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(host = %spec.host, port = spec.port, "ssh_exec_timeout");
                    return Err(ExecError::Timeout { timeout });
                }
                res = &mut open => break match res {
                    Ok(conn) => conn,
                    Err(failure) => {
                        let err = map_failure(failure, timeout);
                        tracing::warn!(host = %spec.host, port = spec.port, error = %err, "ssh_exec_failed");
                        return Err(err);
                    }
                },
                ev = session_rx.recv() => {
                    if let Ok(SessionEvent::Prompt(prompt)) = ev
                        && exec_tx.send(ExecEvent::Prompt(prompt)).await.is_err()
                    {
                        return Err(ExecError::Cancelled);
                    }
                }
            }
        }
    };

    let result = run_channel(&conn, &command, deadline, timeout, cancel, &cause).await;
    let _ = tokio::time::timeout(DISCONNECT_GRACE, conn.disconnect()).await;

    match &result {
        Ok(out) => {
            tracing::info!(
                host = %spec.host,
                port = spec.port,
                exit_status = ?out.exit_status,
                stdout_bytes = out.stdout.len(),
                stderr_bytes = out.stderr.len(),
                stdout_truncated = out.stdout_truncated,
                stderr_truncated = out.stderr_truncated,
                "ssh_exec_finished"
            );
        }
        Err(err) => {
            tracing::warn!(host = %spec.host, port = spec.port, error = %err, "ssh_exec_failed");
        }
    }
    result
}

async fn race<T>(
    deadline: Instant,
    timeout: Duration,
    cancel: &mut CancelSignal,
    fut: impl Future<Output = T>,
) -> std::result::Result<T, ExecError> {
    tokio::select! {
        biased;
        () = cancel.wait() => Err(ExecError::Cancelled),
        () = tokio::time::sleep_until(deadline) => Err(ExecError::Timeout { timeout }),
        out = fut => Ok(out),
    }
}

async fn run_channel(
    conn: &AuthenticatedConnection,
    command: &str,
    deadline: Instant,
    timeout: Duration,
    cancel: &mut CancelSignal,
    cause: &Cause,
) -> std::result::Result<ExecOutput, ExecError> {
    let mut channel = race(
        deadline,
        timeout,
        cancel,
        conn.session().channel_open_session(),
    )
    .await?
    .map_err(|e| ExecError::Ssh(Failure::from(e).to_string()))?;
    race(
        deadline,
        timeout,
        cancel,
        channel.exec(true, command.as_bytes()),
    )
    .await?
    .map_err(|e| ExecError::Ssh(Failure::from(e).to_string()))?;
    race(deadline, timeout, cancel, channel.eof())
        .await?
        .map_err(|e| ExecError::Ssh(Failure::from(e).to_string()))?;

    let mut stdout = BoundedBuf::default();
    let mut stderr = BoundedBuf::default();
    let mut exit_status = None;
    let mut exit_signal = None;
    let mut closed_cleanly = false;
    let mut close_deadline: Option<Instant> = None;

    loop {
        let msg = tokio::select! {
            biased;
            () = cancel.wait() => return Err(ExecError::Cancelled),
            () = tokio::time::sleep_until(deadline) => return Err(ExecError::Timeout { timeout }),
            () = async {
                match close_deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            } => break,
            msg = channel.wait() => msg,
        };
        match msg {
            Some(ChannelMsg::Data { data }) => stdout.push(&data),
            Some(ChannelMsg::ExtendedData { data, ext }) if ext == SSH_EXTENDED_DATA_STDERR => {
                stderr.push(&data);
            }
            Some(ChannelMsg::ExitStatus { exit_status: code }) => {
                exit_status = Some(code);
                close_deadline.get_or_insert_with(|| Instant::now() + EXIT_GRACE);
            }
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                exit_signal = Some(format!("{signal_name:?}"));
                close_deadline.get_or_insert_with(|| Instant::now() + EXIT_GRACE);
            }
            Some(ChannelMsg::Failure) => return Err(ExecError::Refused),
            Some(ChannelMsg::Close) => {
                closed_cleanly = true;
                break;
            }
            None => break,
            Some(_) => {}
        }
    }

    if exit_status.is_none() && exit_signal.is_none() && !closed_cleanly {
        let grace = CAUSE_GRACE.min(deadline.saturating_duration_since(Instant::now()));
        let detail = describe_drop(cause.wait(grace).await.as_ref());
        return Err(ExecError::Ssh(detail.to_owned()));
    }

    Ok(ExecOutput {
        exit_status,
        exit_signal,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    })
}

fn describe_drop(reason: Option<&Disconnect>) -> &'static str {
    match reason {
        Some(Disconnect::Timeout) => "connection timed out before command finished",
        Some(Disconnect::Reset) => "connection reset before command finished",
        Some(Disconnect::ServerClosed) => "server closed connection before command finished",
        _ => "connection closed before command finished",
    }
}

/// Accumulates up to [`EXEC_OUTPUT_CAP`] bytes without growing unboundedly, recording whether
/// any bytes beyond the cap were discarded.
#[derive(Debug, Default)]
struct BoundedBuf {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedBuf {
    fn push(&mut self, chunk: &[u8]) {
        let remaining = EXEC_OUTPUT_CAP.saturating_sub(self.bytes.len());
        let take = chunk.len().min(remaining);
        self.bytes.extend_from_slice(&chunk[..take]);
        if chunk.len() > remaining {
            self.truncated = true;
        }
    }
}

fn parse_hop(label: &str) -> Option<(String, u16)> {
    label
        .rsplit_once(':')
        .and_then(|(host, port)| Some((host.to_owned(), port.parse().ok()?)))
}

fn map_failure(failure: Failure, timeout: Duration) -> ExecError {
    let message = failure.to_string();
    map_failure_inner(failure, None, message, timeout)
}

fn map_failure_inner(
    failure: Failure,
    hop: Option<(String, u16)>,
    full_message: String,
    timeout: Duration,
) -> ExecError {
    match failure {
        Failure::Jump(label, inner) => {
            let hop = hop.or_else(|| parse_hop(&label));
            map_failure_inner(*inner, hop, full_message, timeout)
        }
        Failure::HostKeyChanged { line } => ExecError::HostKeyChanged { line, hop },
        Failure::HostKeyRejected => ExecError::UnknownHostKey { hop },
        Failure::AuthCancelled => ExecError::InteractiveAuthRequired { hop },
        Failure::AuthFailed => ExecError::AuthFailed { hop },
        Failure::ConnectTimeout => ExecError::Timeout {
            timeout: timeout.min(CONNECT_TIMEOUT),
        },
        Failure::UiGone => ExecError::Cancelled,
        Failure::HostKeyUnsupported
        | Failure::HostKeyCheck(_)
        | Failure::Proxy(_)
        | Failure::BadJump(_)
        | Failure::Unreachable(_)
        | Failure::KeepaliveTimeout
        | Failure::Transport(_)
        | Failure::Io(_) => ExecError::Ssh(full_message),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Error as IoError;
    use std::io::ErrorKind;
    use std::time::Duration;

    use super::*;

    #[test]
    fn bounded_buf_preserves_exact_cap_and_truncates_on_first_excess_byte() {
        let mut buf = BoundedBuf::default();
        buf.push(&vec![b'a'; EXEC_OUTPUT_CAP - 1]);
        buf.push(b"b");
        buf.push(b"");
        assert_eq!(buf.bytes.len(), EXEC_OUTPUT_CAP);
        assert_eq!(buf.bytes[EXEC_OUTPUT_CAP - 1], b'b');
        assert!(!buf.truncated);

        buf.push(b"c");
        assert_eq!(buf.bytes.len(), EXEC_OUTPUT_CAP);
        assert_eq!(buf.bytes[EXEC_OUTPUT_CAP - 1], b'b');
        assert!(buf.truncated);

        let mut single = BoundedBuf::default();
        single.push(&vec![b'x'; EXEC_OUTPUT_CAP + 10]);
        assert_eq!(single.bytes.len(), EXEC_OUTPUT_CAP);
        assert!(single.truncated);
    }

    #[test]
    fn timeout_clamping_and_duration_formatting_cover_boundaries() {
        assert_eq!(
            ExecOptions::with_timeout(Duration::from_millis(999)).effective_timeout(),
            MIN_EXEC_TIMEOUT
        );
        assert_eq!(
            ExecOptions::with_timeout(Duration::from_secs(60) + Duration::from_nanos(1))
                .effective_timeout(),
            MAX_EXEC_TIMEOUT
        );
        assert_eq!(format_duration(Duration::from_secs(15)), "15s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
    }

    #[test]
    fn exec_error_messages_are_lowercase_without_trailing_punctuation() {
        let hop = || Some(("bastion.example".to_owned(), 2222));
        let cases = [
            (ExecError::UnknownHostKey { hop: None }, "unknown host key"),
            (
                ExecError::UnknownHostKey { hop: hop() },
                "unknown host key for jump host bastion.example:2222",
            ),
            (
                ExecError::HostKeyChanged { line: 9, hop: None },
                "host key changed at known_hosts line 9",
            ),
            (
                ExecError::HostKeyChanged {
                    line: 9,
                    hop: hop(),
                },
                "host key changed for jump host bastion.example:2222 at known_hosts line 9",
            ),
            (
                ExecError::InteractiveAuthRequired { hop: None },
                "interactive authentication required",
            ),
            (
                ExecError::InteractiveAuthRequired { hop: hop() },
                "interactive authentication required for jump host bastion.example:2222",
            ),
            (ExecError::AuthFailed { hop: None }, "authentication failed"),
            (
                ExecError::AuthFailed { hop: hop() },
                "authentication failed for jump host bastion.example:2222",
            ),
            (
                ExecError::Timeout {
                    timeout: Duration::from_secs(15),
                },
                "command timed out after 15s",
            ),
            (
                ExecError::Timeout {
                    timeout: Duration::from_millis(1500),
                },
                "command timed out after 1.5s",
            ),
            (ExecError::Cancelled, "command cancelled"),
            (ExecError::Refused, "server refused exec request"),
            (ExecError::Ssh("boom".into()), "boom"),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }

    #[test]
    fn map_failure_preserves_security_auth_timeout_and_jump_hop_context() {
        let t10 = Duration::from_secs(10);
        let t30 = Duration::from_secs(30);
        let via = |f: Failure| Failure::Jump("gw.internal:2200".into(), Box::new(f));
        let h = || Some(("gw.internal".to_owned(), 2200));
        let check = |f, want| assert_eq!(map_failure(f, t10), want);

        check(
            Failure::HostKeyChanged { line: 3 },
            ExecError::HostKeyChanged { line: 3, hop: None },
        );
        check(
            via(Failure::HostKeyChanged { line: 4 }),
            ExecError::HostKeyChanged { line: 4, hop: h() },
        );
        check(
            Failure::HostKeyRejected,
            ExecError::UnknownHostKey { hop: None },
        );
        check(
            via(Failure::HostKeyRejected),
            ExecError::UnknownHostKey { hop: h() },
        );
        check(
            Failure::AuthCancelled,
            ExecError::InteractiveAuthRequired { hop: None },
        );
        check(
            via(Failure::AuthCancelled),
            ExecError::InteractiveAuthRequired { hop: h() },
        );
        check(Failure::AuthFailed, ExecError::AuthFailed { hop: None });
        check(via(Failure::AuthFailed), ExecError::AuthFailed { hop: h() });
        check(Failure::ConnectTimeout, ExecError::Timeout { timeout: t10 });
        assert_eq!(
            map_failure(Failure::ConnectTimeout, t30),
            ExecError::Timeout {
                timeout: CONNECT_TIMEOUT
            }
        );
        check(Failure::UiGone, ExecError::Cancelled);
        check(
            via(Failure::Unreachable("gw refused".into())),
            ExecError::Ssh("gw refused (while going through jump host gw.internal:2200)".into()),
        );
        let pipe = IoError::from(ErrorKind::BrokenPipe);
        check(
            Failure::Io(IoError::from(ErrorKind::BrokenPipe)),
            ExecError::Ssh(pipe.to_string()),
        );
        assert_eq!(parse_hop("[::1]:2222"), Some(("[::1]".to_owned(), 2222)));
        assert_eq!(parse_hop("no-port"), None);
    }

    #[test]
    fn describe_drop_distinguishes_disconnect_causes() {
        assert_eq!(
            describe_drop(Some(&Disconnect::Timeout)),
            "connection timed out before command finished"
        );
        assert_eq!(
            describe_drop(Some(&Disconnect::Reset)),
            "connection reset before command finished"
        );
        assert_eq!(
            describe_drop(Some(&Disconnect::ServerClosed)),
            "server closed connection before command finished"
        );
        assert_eq!(
            describe_drop(None),
            "connection closed before command finished"
        );
    }

    #[tokio::test]
    async fn exec_next_returns_none_after_finished_and_direct_cancel_signals() {
        let (etx, erx) = async_channel::bounded(2);
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        let mut run = Exec {
            events: erx,
            cancel: ExecCancel { tx: cancel_tx },
            finished: false,
        };
        assert!(format!("{run:?}").contains("Exec"));
        run.cancel();
        assert!(*cancel_rx.borrow_and_update());
        etx.send(ExecEvent::Finished(Err(ExecError::Cancelled)))
            .await
            .unwrap();
        assert!(matches!(
            run.next().await,
            Some(ExecEvent::Finished(Err(ExecError::Cancelled)))
        ));
        assert!(run.next().await.is_none());
    }
}
