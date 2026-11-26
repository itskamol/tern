//! Streaming, isolated, non-PTY SSH `exec` channel built on [`crate::connect`].
//!
//! Shares the same security isolation as [`crate::exec`] (`forward_agent = false`, zero port
//! forwards, immediate stdin EOF, fresh connection), but delivers `stdout` and `stderr` chunks as
//! they arrive over a bounded backpressure channel without a 64 KiB output cap or mandatory 30 s
//! timeout. On cancellation, sends a best-effort `SIGINT` (RFC 4254 §6.9) and closes the channel.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use russh::{ChannelMsg, Sig};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::connect::{AuthenticatedConnection, CONNECT_TIMEOUT, ConnectOptions};
use crate::error::{Cause, Failure};
use crate::exec::{
    CAUSE_GRACE, CancelSignal, DISCONNECT_GRACE, EXEC_EVENT_QUEUE, EXIT_GRACE,
    SSH_EXTENDED_DATA_STDERR, describe_drop, map_failure,
};
use crate::{
    ConnectSpec, ExecCancel, ExecError, ExecPartialOutput, ExecTimeoutPhase, MIN_EXEC_TIMEOUT,
    Prompt, SessionEvent,
};

/// Bounded event queue between the SSH task and the consumer for backpressure.
const STREAM_EVENT_QUEUE: usize = 64;
/// Bounded wait for best-effort remote `SIGINT` and channel close on local cancellation.
const SIGNAL_GRACE: Duration = Duration::from_millis(200);

/// Configuration for a single isolated non-PTY streaming [`exec_stream`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecStreamOptions {
    /// Optional wall-clock limit covering connection setup and command execution.
    /// `None` means no execution timeout (connection setup still enforces [`CONNECT_TIMEOUT`]).
    pub timeout: Option<Duration>,
}

impl ExecStreamOptions {
    #[must_use]
    pub const fn with_timeout(timeout: Option<Duration>) -> Self {
        Self { timeout }
    }

    #[must_use]
    pub fn effective_timeout(&self) -> Option<Duration> {
        self.timeout.map(|t| t.max(MIN_EXEC_TIMEOUT))
    }
}

/// Terminal exit metadata from a completed streaming [`exec_stream`] run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecStreamResult {
    /// Exit status reported by the remote process (`None` if closed without an exit status).
    pub exit_status: Option<u32>,
    /// Signal name if the remote process was terminated by a signal.
    pub exit_signal: Option<String>,
}

/// Live events emitted by a running [`ExecStream`].
#[derive(Debug)]
pub enum ExecStreamEvent {
    /// Connection setup or authentication needs an answer; dropping the reply declines/cancels.
    Prompt(Prompt),
    /// Raw stdout bytes received from the remote command.
    Stdout(Vec<u8>),
    /// Raw stderr bytes received from the remote command.
    Stderr(Vec<u8>),
    /// Terminal outcome of the stream. Always the last event delivered.
    Finished(std::result::Result<ExecStreamResult, ExecError>),
}

/// Drives one isolated non-PTY streaming remote command on a tokio runtime.
///
/// Dropping `ExecStream` cancels the command, attempts best-effort `SIGINT`, and tears down the
/// SSH connection.
#[must_use = "dropping an ExecStream cancels the running command"]
pub struct ExecStream {
    events: async_channel::Receiver<ExecStreamEvent>,
    cancel: ExecCancel,
    finished: bool,
}

impl Drop for ExecStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl fmt::Debug for ExecStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecStream")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl ExecStream {
    /// Returns a cloneable handle that can cancel this stream from another task.
    #[must_use]
    pub fn cancel_handle(&self) -> ExecCancel {
        self.cancel.clone()
    }

    /// Awaits the next [`ExecStreamEvent`]. Returns `None` after [`ExecStreamEvent::Finished`].
    pub async fn next(&mut self) -> Option<ExecStreamEvent> {
        if self.finished {
            return None;
        }
        let event = self.events.recv().await.ok()?;
        if matches!(event, ExecStreamEvent::Finished(_)) {
            self.finished = true;
        }
        Some(event)
    }
}

/// Starts one isolated non-PTY streaming remote command on `rt`.
#[must_use = "dropping an ExecStream cancels the running command"]
pub fn exec_stream(
    spec: ConnectSpec,
    command: impl Into<String>,
    options: ExecStreamOptions,
    rt: &tokio::runtime::Handle,
) -> ExecStream {
    let command = command.into();
    let (etx, erx) = async_channel::bounded(STREAM_EVENT_QUEUE);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    rt.spawn(async move {
        let mut cancel = CancelSignal(cancel_rx);
        let result = run_stream(spec, command, options, &etx, &mut cancel).await;
        let _ = etx.send(ExecStreamEvent::Finished(result)).await;
    });
    ExecStream {
        events: erx,
        cancel: ExecCancel::new(cancel_tx),
        finished: false,
    }
}

async fn sleep_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

async fn race_opt<T>(
    deadline: Option<(Instant, Duration)>,
    phase: ExecTimeoutPhase,
    cancel: &mut CancelSignal,
    fut: impl Future<Output = T>,
) -> std::result::Result<T, ExecError> {
    tokio::select! {
        biased;
        () = cancel.wait() => Err(ExecError::Cancelled),
        () = sleep_deadline(deadline.map(|(d, _)| d)) => Err(ExecError::Timeout {
            timeout: deadline.map_or(CONNECT_TIMEOUT, |(_, t)| t),
            phase,
            partial: ExecPartialOutput::default(),
        }),
        out = fut => Ok(out),
    }
}

async fn send_open_prompt(
    exec_tx: &async_channel::Sender<ExecStreamEvent>,
    prompt: Prompt,
    deadline: Option<(Instant, Duration)>,
    cancel: &mut CancelSignal,
) -> std::result::Result<(), ExecError> {
    tokio::select! {
        biased;
        () = cancel.wait() => Err(ExecError::Cancelled),
        () = sleep_deadline(deadline.map(|(d, _)| d)) => Err(ExecError::Timeout {
            timeout: deadline.map_or(CONNECT_TIMEOUT, |(_, t)| t),
            phase: ExecTimeoutPhase::NotDispatched,
            partial: ExecPartialOutput::default(),
        }),
        res = exec_tx.send(ExecStreamEvent::Prompt(prompt)) => {
            if res.is_err() {
                return Err(ExecError::Cancelled);
            }
            Ok(())
        }
    }
}

async fn cancel_channel(channel: &mut russh::Channel<russh::client::Msg>) {
    // Best-effort remote SIGINT (RFC 4254 §6.9; server may ignore).
    let _ = tokio::time::timeout(SIGNAL_GRACE, channel.signal(Sig::INT)).await;
    let _ = tokio::time::timeout(SIGNAL_GRACE, channel.close()).await;
}

async fn send_stream_event(
    exec_tx: &async_channel::Sender<ExecStreamEvent>,
    event: ExecStreamEvent,
    channel: &mut russh::Channel<russh::client::Msg>,
    deadline: Option<(Instant, Duration)>,
    phase: ExecTimeoutPhase,
    cancel: &mut CancelSignal,
) -> std::result::Result<(), ExecError> {
    tokio::select! {
        biased;
        () = cancel.wait() => {
            cancel_channel(channel).await;
            Err(ExecError::Cancelled)
        }
        () = sleep_deadline(deadline.map(|(d, _)| d)) => {
            cancel_channel(channel).await;
            Err(ExecError::Timeout {
                timeout: deadline.map_or(CONNECT_TIMEOUT, |(_, t)| t),
                phase,
                partial: ExecPartialOutput::default(),
            })
        }
        res = exec_tx.send(event) => {
            if res.is_err() {
                cancel_channel(channel).await;
                return Err(ExecError::Cancelled);
            }
            Ok(())
        }
    }
}

async fn run_stream(
    spec: ConnectSpec,
    command: String,
    options: ExecStreamOptions,
    exec_tx: &async_channel::Sender<ExecStreamEvent>,
    cancel: &mut CancelSignal,
) -> std::result::Result<ExecStreamResult, ExecError> {
    let timeout = options.effective_timeout();
    let deadline = timeout.map(|t| (Instant::now() + t, t));
    tracing::info!(
        host = %spec.host,
        port = spec.port,
        timeout_ms = timeout.map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
        "ssh_exec_stream_start"
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
                    tracing::info!(host = %spec.host, port = spec.port, "ssh_exec_stream_cancelled");
                    return Err(ExecError::Cancelled);
                }
                () = sleep_deadline(deadline.map(|(d, _)| d)) => {
                    let t = deadline.map_or(CONNECT_TIMEOUT, |(_, t)| t);
                    tracing::warn!(host = %spec.host, port = spec.port, "ssh_exec_timeout");
                    return Err(ExecError::Timeout {
                        timeout: t,
                        phase: ExecTimeoutPhase::NotDispatched,
                        partial: ExecPartialOutput::default(),
                    });
                }
                res = &mut open => break match res {
                    Ok(conn) => conn,
                    Err(failure) => {
                        let t = timeout.unwrap_or(CONNECT_TIMEOUT);
                        let err = map_failure(failure, t);
                        tracing::warn!(host = %spec.host, port = spec.port, error = %err, "ssh_exec_failed");
                        return Err(err);
                    }
                },
                ev = session_rx.recv() => {
                    if let Ok(SessionEvent::Prompt(prompt)) = ev
                        && let Err(err) = send_open_prompt(exec_tx, prompt, deadline, cancel).await
                    {
                        match &err {
                            ExecError::Timeout { .. } => {
                                tracing::warn!(host = %spec.host, port = spec.port, "ssh_exec_timeout");
                            }
                            ExecError::Cancelled => {
                                tracing::info!(host = %spec.host, port = spec.port, "ssh_exec_stream_cancelled");
                            }
                            _ => {}
                        }
                        return Err(err);
                    }
                }
            }
        }
    };

    let result = run_stream_channel(&conn, &command, deadline, cancel, &cause, exec_tx).await;
    let _ = tokio::time::timeout(DISCONNECT_GRACE, conn.disconnect()).await;

    match &result {
        Ok(done) => {
            tracing::info!(
                host = %spec.host,
                port = spec.port,
                exit_status = ?done.exit_status,
                exit_signal = ?done.exit_signal,
                "ssh_exec_finished"
            );
        }
        Err(ExecError::Timeout { phase, .. }) => {
            tracing::warn!(
                host = %spec.host,
                port = spec.port,
                ?phase,
                "ssh_exec_timeout"
            );
        }
        Err(ExecError::Cancelled) => {
            tracing::info!(host = %spec.host, port = spec.port, "ssh_exec_stream_cancelled");
        }
        Err(err) => {
            tracing::warn!(host = %spec.host, port = spec.port, error = %err, "ssh_exec_failed");
        }
    }
    result
}

async fn run_stream_channel(
    conn: &AuthenticatedConnection,
    command: &str,
    deadline: Option<(Instant, Duration)>,
    cancel: &mut CancelSignal,
    cause: &Cause,
    exec_tx: &async_channel::Sender<ExecStreamEvent>,
) -> std::result::Result<ExecStreamResult, ExecError> {
    let mut channel = race_opt(
        deadline,
        ExecTimeoutPhase::NotDispatched,
        cancel,
        conn.session().channel_open_session(),
    )
    .await?
    .map_err(|e| ExecError::Ssh(Failure::from(e).to_string()))?;

    race_opt(
        deadline,
        ExecTimeoutPhase::DispatchUncertain,
        cancel,
        channel.exec(true, command.as_bytes()),
    )
    .await?
    .map_err(|e| ExecError::Ssh(Failure::from(e).to_string()))?;

    race_opt(
        deadline,
        ExecTimeoutPhase::DispatchUncertain,
        cancel,
        channel.eof(),
    )
    .await?
    .map_err(|e| ExecError::Ssh(Failure::from(e).to_string()))?;

    let mut phase = ExecTimeoutPhase::DispatchUncertain;
    let mut exit_status = None;
    let mut exit_signal = None;
    let mut closed_cleanly = false;
    let mut close_deadline: Option<Instant> = None;

    loop {
        let msg = tokio::select! {
            biased;
            () = cancel.wait() => {
                cancel_channel(&mut channel).await;
                return Err(ExecError::Cancelled);
            }
            () = sleep_deadline(deadline.map(|(d, _)| d)) => {
                cancel_channel(&mut channel).await;
                let t = deadline.map_or(CONNECT_TIMEOUT, |(_, t)| t);
                return Err(ExecError::Timeout {
                    timeout: t,
                    phase,
                    partial: ExecPartialOutput::default(),
                });
            }
            () = sleep_deadline(close_deadline) => break,
            msg = channel.wait() => msg,
        };
        match msg {
            Some(ChannelMsg::Success) => {
                phase = ExecTimeoutPhase::Running;
            }
            Some(ChannelMsg::Data { data }) => {
                phase = ExecTimeoutPhase::Running;
                if !data.is_empty() {
                    send_stream_event(
                        exec_tx,
                        ExecStreamEvent::Stdout(data.to_vec()),
                        &mut channel,
                        deadline,
                        phase,
                        cancel,
                    )
                    .await?;
                }
            }
            Some(ChannelMsg::ExtendedData { data, ext }) if ext == SSH_EXTENDED_DATA_STDERR => {
                phase = ExecTimeoutPhase::Running;
                if !data.is_empty() {
                    send_stream_event(
                        exec_tx,
                        ExecStreamEvent::Stderr(data.to_vec()),
                        &mut channel,
                        deadline,
                        phase,
                        cancel,
                    )
                    .await?;
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status: code }) => {
                phase = ExecTimeoutPhase::Running;
                exit_status = Some(code);
                close_deadline.get_or_insert_with(|| Instant::now() + EXIT_GRACE);
            }
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                phase = ExecTimeoutPhase::Running;
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
        let grace = match deadline {
            Some((d, _)) => CAUSE_GRACE.min(d.saturating_duration_since(Instant::now())),
            None => CAUSE_GRACE,
        };
        let detail = describe_drop(cause.wait(grace).await.as_ref());
        return Err(ExecError::Ssh(detail.to_owned()));
    }

    Ok(ExecStreamResult {
        exit_status,
        exit_signal,
    })
}
