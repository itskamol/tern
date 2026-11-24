//! Newline-delimited JSON protocol over `agent.sock` and structured Agent response/error types.
//!
//! Includes `Progress` (broker -> client) and `Cancel` (client -> broker) variants on the same
//! framed stream so cancellation and approval progress notifications share one wire framing.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Upper bound on one newline-delimited JSON frame on stdio or `agent.sock`.
/// Sized to hold a full 64 KiB `stdout` + 64 KiB `stderr` response even after JSON escaping.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Upper bound on one remote shell command or inline script sent via `run_command` (ADR 0004).
/// Stays well below Linux `MAX_ARG_STRLEN` (128 KiB) so `sshd` never fails with `E2BIG`.
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;

/// Structured error codes returned to Agents (ADR 0003, ADR 0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorCode {
    TernNotRunning,
    AgentsPaused,
    Denied,
    TimedOut,
    UserActionRequired,
    SshError,
    UnknownOutcome,
}

/// Machine-readable reason explaining why [`AgentErrorCode::UserActionRequired`] failed closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserActionReason {
    VaultLocked,
    CommandApprovalRequired,
    UnknownHostKey,
    InteractiveAuthRequired,
}

/// Structured error returned from the broker and surfaced by `tern mcp`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentError {
    pub error: AgentErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<UserActionReason>,
    pub message: String,
}

impl AgentError {
    #[must_use]
    pub fn new(error: AgentErrorCode, message: impl Into<String>) -> Self {
        Self {
            error,
            reason: None,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn user_action_required(reason: UserActionReason, message: impl Into<String>) -> Self {
        Self {
            error: AgentErrorCode::UserActionRequired,
            reason: Some(reason),
            message: message.into(),
        }
    }
}

/// One Saved connection summary returned by `list_hosts` (never exposes keys or passwords).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSummary {
    pub alias: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    #[serde(default)]
    pub grant_active: bool,
}

/// Structured response from the `list_hosts` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListHostsResponse {
    pub hosts: Vec<HostSummary>,
    pub usage: String,
}

/// Structured response from the `run_command` tool (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCommandResponse {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: u64,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Request frames sent from `tern mcp` to tern over `agent.sock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerRequest {
    ListHosts {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    RunCommand {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
        host: String,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
        #[serde(default, skip_serializing_if = "is_false")]
        wants_progress: bool,
    },
    /// Signals cancellation of the in-flight request on this connection.
    Cancel,
}

/// Terminal result of a single broker request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BrokerResult {
    Ok { data: serde_json::Value },
    Error(AgentError),
}

/// Response frames sent from tern to `tern mcp` over `agent.sock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerMessage {
    /// Non-terminal status update while waiting on approval or execution.
    Progress { message: String },
    /// Final result for the request; closes the exchange.
    Result(BrokerResult),
}

/// Why reading a bounded newline-delimited frame failed.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame exceeded maximum size of {0} bytes")]
    TooLarge(usize),
    #[error("frame was not valid utf-8")]
    Utf8,
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Cancel-safe, bounded newline-delimited UTF-8 line reader safe to poll inside `tokio::select!`.
///
/// Partial bytes read before a `select!` branch cancels [`Self::next_line`] stay in the internal
/// buffer and resume seamlessly on the next poll without ever exceeding `max_bytes`.
#[derive(Debug)]
pub struct BoundedLineReader<R> {
    reader: R,
    buf: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> BoundedLineReader<R> {
    #[must_use]
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
        }
    }

    /// Reads one newline-delimited UTF-8 line without ever buffering more than `max_bytes` bytes.
    /// Returns `Ok(None)` on clean EOF with zero unread bytes.
    ///
    /// # Errors
    /// Returns [`FrameError::TooLarge`] when a line exceeds `max_bytes`, [`FrameError::Utf8`] when
    /// bytes are not valid UTF-8, or [`FrameError::Io`] on underlying read failure.
    pub async fn next_line(&mut self, max_bytes: usize) -> Result<Option<String>, FrameError> {
        loop {
            let available = self.reader.fill_buf().await?;
            if available.is_empty() {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            if let Some(nl) = available.iter().position(|&b| b == b'\n') {
                if self.buf.len() + nl > max_bytes {
                    self.reader.consume(nl + 1);
                    self.buf.clear();
                    return Err(FrameError::TooLarge(max_bytes));
                }
                self.buf.extend_from_slice(&available[..nl]);
                self.reader.consume(nl + 1);
                break;
            }
            if self.buf.len() + available.len() > max_bytes {
                let len = available.len();
                self.reader.consume(len);
                self.buf.clear();
                return Err(FrameError::TooLarge(max_bytes));
            }
            self.buf.extend_from_slice(available);
            let len = available.len();
            self.reader.consume(len);
        }
        let mut line = std::mem::take(&mut self.buf);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8(line)
            .map(Some)
            .map_err(|_| FrameError::Utf8)
    }
}

/// Reads one newline-delimited UTF-8 line without ever buffering more than `max_bytes` bytes.
/// Returns `Ok(None)` on clean EOF with zero unread bytes.
///
/// # Errors
/// Returns [`FrameError::TooLarge`] when a line exceeds `max_bytes`, [`FrameError::Utf8`] when
/// bytes are not valid UTF-8, or [`FrameError::Io`] on underlying read failure.
#[cfg(test)]
pub async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, FrameError> {
    let mut bounded = BoundedLineReader::new(reader);
    bounded.next_line(max_bytes).await
}

/// Serializes `value` as one compact JSON line terminated by `\n` and flushes `writer`.
///
/// # Errors
/// Returns an [`io::Error`] if serialization, writing, or flushing fails.
pub async fn write_json_line<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tokio::io::BufReader;

    use super::*;

    #[tokio::test]
    async fn bounded_line_reader_handles_normal_crlf_eof_and_oversized_frames() {
        let input = b"{\"a\":1}\r\n{\"b\":2}\nlast";
        let mut reader = BufReader::new(&input[..]);
        assert_eq!(
            read_line_bounded(&mut reader, 64).await.unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(
            read_line_bounded(&mut reader, 64).await.unwrap().as_deref(),
            Some("{\"b\":2}")
        );
        assert_eq!(
            read_line_bounded(&mut reader, 64).await.unwrap().as_deref(),
            Some("last")
        );
        assert_eq!(read_line_bounded(&mut reader, 64).await.unwrap(), None);

        let oversized = b"0123456789abcdef\n";
        let mut reader = BufReader::new(&oversized[..]);
        assert!(matches!(
            read_line_bounded(&mut reader, 8).await,
            Err(FrameError::TooLarge(8))
        ));
    }

    #[tokio::test]
    async fn bounded_line_reader_is_cancel_safe_across_partial_chunks() {
        use tokio::io::AsyncWriteExt as _;

        let (mut tx, rx) = tokio::io::duplex(64);
        let mut reader = BoundedLineReader::new(BufReader::new(rx));

        tx.write_all(b"{\"hello\":").await.unwrap();
        let wake = tokio::time::sleep(std::time::Duration::from_millis(20));
        tokio::select! {
            _ = wake => {}
            _ = reader.next_line(64) => panic!("partial line should not finish before newline"),
        }

        tx.write_all(b"\"world\"}\n").await.unwrap();
        let line = reader.next_line(64).await.unwrap();
        assert_eq!(line.as_deref(), Some("{\"hello\":\"world\"}"));
    }

    #[test]
    fn broker_frames_round_trip_with_snake_case_error_codes_and_reasons() {
        let req = BrokerRequest::RunCommand {
            caller: Some("Claude Code".into()),
            host: "staging".into(),
            command: "node ./check.js".into(),
            timeout_secs: Some(45),
            wants_progress: true,
        };
        let encoded = serde_json::to_string(&req).unwrap();
        assert_eq!(
            serde_json::from_str::<BrokerRequest>(&encoded).unwrap(),
            req
        );

        let without_progress = BrokerRequest::RunCommand {
            caller: None,
            host: "staging".into(),
            command: "df -h".into(),
            timeout_secs: None,
            wants_progress: false,
        };
        let encoded_no_p = serde_json::to_string(&without_progress).unwrap();
        assert!(!encoded_no_p.contains("wants_progress"));
        assert_eq!(
            serde_json::from_str::<BrokerRequest>(&encoded_no_p).unwrap(),
            without_progress
        );

        let msg = BrokerMessage::Result(BrokerResult::Error(AgentError::user_action_required(
            UserActionReason::VaultLocked,
            "unlock vault",
        )));
        let json: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "result");
        assert_eq!(json["status"], "error");
        assert_eq!(json["error"], "user_action_required");
        assert_eq!(json["reason"], "vault_locked");
        assert_eq!(serde_json::from_value::<BrokerMessage>(json).unwrap(), msg);

        let progress = BrokerMessage::Progress {
            message: "Waiting for approval in tern...".into(),
        };
        let encoded_p = serde_json::to_string(&progress).unwrap();
        assert_eq!(
            serde_json::from_str::<BrokerMessage>(&encoded_p).unwrap(),
            progress
        );
    }
}
