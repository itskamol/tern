//! Short-lived, isolated, non-PTY SSH `exec` channel built on [`crate::connect`].
//!
//! Each call opens its own [`AuthenticatedConnection`] with agent forwarding and port forwards
//! disabled, runs one remote command with immediate stdin EOF, bounds stdout and stderr to
//! [`EXEC_OUTPUT_CAP`] each, enforces a wall-clock timeout, and disconnects cleanly on exit,
//! error, timeout, or caller cancellation.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;

use crate::connect::CONNECT_TIMEOUT;
use crate::error::{Disconnect, Failure};
use crate::exec_stream::{
    ExecStream, ExecStreamEvent, ExecStreamOptions, ExecStreamResult, exec_stream,
};
use crate::{ConnectSpec, Prompt};

/// Default wall-clock limit covering connection setup and command execution.
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(30);
/// Minimum permitted wall-clock timeout for [`exec`].
pub const MIN_EXEC_TIMEOUT: Duration = Duration::from_secs(1);
/// Maximum permitted wall-clock timeout for [`exec`].
pub const MAX_EXEC_TIMEOUT: Duration = Duration::from_secs(300);
/// Independent byte cap for each of `stdout` and `stderr` captured by [`exec`].
pub const EXEC_OUTPUT_CAP: usize = 64 * 1024;

/// RFC 4254 §5.2 `SSH_EXTENDED_DATA_STDERR`.
pub(crate) const SSH_EXTENDED_DATA_STDERR: u32 = 1;
/// Capacity of the internal/public exec event queues (at most one prompt is in flight at once).
pub(crate) const EXEC_EVENT_QUEUE: usize = 4;
/// Grace window after `exit-status` / `exit-signal` for the server to close the channel.
pub(crate) const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long to wait for russh to report why an unexpectedly dropped connection ended.
pub(crate) const CAUSE_GRACE: Duration = Duration::from_millis(500);
/// Upper bound on clean SSH disconnect + proxy child shutdown before dropping the transport.
pub(crate) const DISCONNECT_GRACE: Duration = Duration::from_millis(500);

/// Configuration for a single isolated non-PTY [`exec`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOptions {
    /// Wall-clock limit covering connection setup and command execution.
    /// Defaults to [`DEFAULT_EXEC_TIMEOUT`] (30 s) and is clamped to
    /// [`MIN_EXEC_TIMEOUT`]..=[`MAX_EXEC_TIMEOUT`] (1..=300 s).
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

/// Execution phase reached when [`ExecError::Timeout`] occurred.
///
/// Lets callers distinguish whether a timed-out command is safe to retry:
/// - [`Self::NotDispatched`]: timed out during connection setup, authentication, or opening the
///   session channel before the `exec` request was sent (safe to retry).
/// - [`Self::DispatchUncertain`]: timed out after handing the `exec` request to SSH transport
///   but before the server confirmed acceptance (`SSH_MSG_CHANNEL_SUCCESS` or command output);
///   do not blindly retry side-effecting commands.
/// - [`Self::Running`]: the server accepted the `exec` request and the command was running when
///   the timeout expired; partial captured `stdout`/`stderr` are attached to the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecTimeoutPhase {
    NotDispatched,
    DispatchUncertain,
    Running,
}

/// Partial standard output and standard error captured before a timeout (each capped at
/// [`EXEC_OUTPUT_CAP`] bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecPartialOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
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
    #[error("{}", format_timeout(*.timeout, *.phase))]
    Timeout {
        /// The wall-clock limit that expired.
        timeout: Duration,
        /// How far execution progressed before the timeout fired.
        phase: ExecTimeoutPhase,
        /// Bounded stdout/stderr bytes captured before the timeout fired.
        partial: ExecPartialOutput,
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

fn format_timeout(timeout: Duration, phase: ExecTimeoutPhase) -> String {
    let duration = format_duration(timeout);
    match phase {
        ExecTimeoutPhase::NotDispatched => format!("command timed out after {duration}"),
        ExecTimeoutPhase::DispatchUncertain | ExecTimeoutPhase::Running => format!(
            "command timed out after {duration} (requested remote interrupt; remote process stop is not guaranteed)"
        ),
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
    pub(crate) fn new(tx: watch::Sender<bool>) -> Self {
        Self { tx }
    }

    /// Signals the running [`Exec`] to abort and tear down its SSH connection.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }
}

pub(crate) struct CancelSignal(pub(crate) watch::Receiver<bool>);

impl CancelSignal {
    pub(crate) async fn wait(&mut self) {
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
    let stream_options = ExecStreamOptions::with_timeout(Some(options.effective_timeout()));
    let mut stream = exec_stream(spec, command, stream_options, rt);
    let cancel = stream.cancel_handle();
    let (etx, erx) = async_channel::bounded(EXEC_EVENT_QUEUE);
    rt.spawn(async move {
        let result = collect_stream(&mut stream, &etx).await;
        let _ = etx.send(ExecEvent::Finished(result)).await;
    });
    Exec {
        events: erx,
        cancel,
        finished: false,
    }
}

async fn collect_stream(
    stream: &mut ExecStream,
    exec_tx: &async_channel::Sender<ExecEvent>,
) -> std::result::Result<ExecOutput, ExecError> {
    let mut collector = ExecCollector::default();
    while let Some(event) = stream.next().await {
        match event {
            ExecStreamEvent::Prompt(prompt) => {
                if exec_tx.send(ExecEvent::Prompt(prompt)).await.is_err() {
                    stream.cancel_handle().cancel();
                    return Err(ExecError::Cancelled);
                }
            }
            ExecStreamEvent::Stdout(bytes) => collector.push_stdout(&bytes),
            ExecStreamEvent::Stderr(bytes) => collector.push_stderr(&bytes),
            ExecStreamEvent::Finished(result) => {
                return match result {
                    Ok(done) => Ok(collector.into_output(done)),
                    Err(ExecError::Timeout { timeout, phase, .. }) => Err(ExecError::Timeout {
                        timeout,
                        phase,
                        partial: collector.into_partial(),
                    }),
                    Err(other) => Err(other),
                };
            }
        }
    }
    Err(ExecError::Cancelled)
}

pub(crate) fn describe_drop(reason: Option<&Disconnect>) -> &'static str {
    match reason {
        Some(Disconnect::Timeout) => "connection timed out before command finished",
        Some(Disconnect::Reset) => "connection reset before command finished",
        Some(Disconnect::ServerClosed) => "server closed connection before command finished",
        _ => "connection closed before command finished",
    }
}

/// Accumulates bounded `stdout` and `stderr` up to [`EXEC_OUTPUT_CAP`] bytes each from an
/// [`ExecStream`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecCollector {
    stdout: BoundedBuf,
    stderr: BoundedBuf,
}

impl ExecCollector {
    /// Appends `chunk` to `stdout`, discarding bytes beyond [`EXEC_OUTPUT_CAP`].
    pub fn push_stdout(&mut self, chunk: &[u8]) {
        self.stdout.push(chunk);
    }

    /// Appends `chunk` to `stderr`, discarding bytes beyond [`EXEC_OUTPUT_CAP`].
    pub fn push_stderr(&mut self, chunk: &[u8]) {
        self.stderr.push(chunk);
    }

    /// Consumes collected bytes and terminal stream metadata into [`ExecOutput`].
    #[must_use]
    pub fn into_output(self, done: ExecStreamResult) -> ExecOutput {
        ExecOutput {
            exit_status: done.exit_status,
            exit_signal: done.exit_signal,
            stdout: self.stdout.bytes,
            stderr: self.stderr.bytes,
            stdout_truncated: self.stdout.truncated,
            stderr_truncated: self.stderr.truncated,
        }
    }

    /// Consumes collected bytes into [`ExecPartialOutput`] for [`ExecError::Timeout`].
    #[must_use]
    pub fn into_partial(self) -> ExecPartialOutput {
        ExecPartialOutput {
            stdout: self.stdout.bytes,
            stderr: self.stderr.bytes,
            stdout_truncated: self.stdout.truncated,
            stderr_truncated: self.stderr.truncated,
        }
    }
}

/// Accumulates up to [`EXEC_OUTPUT_CAP`] bytes without growing unboundedly, recording whether
/// any bytes beyond the cap were discarded.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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

pub(crate) fn map_failure(failure: Failure, timeout: Duration) -> ExecError {
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
            phase: ExecTimeoutPhase::NotDispatched,
            partial: ExecPartialOutput::default(),
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
#[path = "exec_tests.rs"]
mod tests;
