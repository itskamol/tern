//! Newline-delimited JSON protocol over `agent.sock` and structured Agent response/error types.
//!
//! Includes `Progress` (broker -> client) and `Cancel` (client -> broker) variants on the same
//! framed stream so cancellation and approval progress notifications (#62) plug in without
//! breaking wire framing.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Upper bound on one newline-delimited JSON frame on stdio or `agent.sock`.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;

/// Structured error codes returned to Agents (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorCode {
    TernNotRunning,
    AgentsPaused,
    Denied,
    TimedOut,
    UserActionRequired,
    SshError,
    UnsupportedHost,
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

/// One Saved connection and its allowlisted systemd service units returned by `list_hosts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedHost {
    pub alias: String,
    pub services: Vec<String>,
}

/// Structured response from the `list_hosts` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListHostsResponse {
    pub hosts: Vec<AllowedHost>,
    pub usage: String,
}

/// Normalized systemd `LoadState` values (`not-found` maps to `not_found`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadState {
    Loaded,
    NotFound,
    BadSetting,
    Error,
    Masked,
    Stub,
    Merged,
    Unknown,
}

/// Normalized systemd `ActiveState` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveState {
    Active,
    Inactive,
    Failed,
    Activating,
    Deactivating,
    Reloading,
    Refreshing,
    Maintenance,
    Unknown,
}

/// Structured response from the `service_status` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatusResponse {
    pub host: String,
    pub service: String,
    pub load_state: LoadState,
    pub active_state: ActiveState,
    pub sub_state: String,
    pub checked_at: String,
}

/// Request frames sent from `tern mcp` to tern over `agent.sock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerRequest {
    ListHosts {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
    },
    ServiceStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller: Option<String>,
        host: String,
        service: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// Signals cancellation of the in-flight request on this connection (#62).
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
    /// Non-terminal status update while waiting on approval or execution (#62).
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

/// Reads one newline-delimited UTF-8 line without ever buffering more than `max_bytes` bytes.
/// Returns `Ok(None)` on clean EOF with zero unread bytes.
///
/// # Errors
/// Returns [`FrameError::TooLarge`] when a line exceeds `max_bytes`, [`FrameError::Utf8`] when
/// bytes are not valid UTF-8, or [`FrameError::Io`] on underlying read failure.
pub async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, FrameError> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(nl) = available.iter().position(|&b| b == b'\n') {
            if buf.len() + nl > max_bytes {
                reader.consume(nl + 1);
                return Err(FrameError::TooLarge(max_bytes));
            }
            buf.extend_from_slice(&available[..nl]);
            reader.consume(nl + 1);
            break;
        }
        if buf.len() + available.len() > max_bytes {
            let len = available.len();
            reader.consume(len);
            return Err(FrameError::TooLarge(max_bytes));
        }
        buf.extend_from_slice(available);
        let len = available.len();
        reader.consume(len);
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| FrameError::Utf8)
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

    #[test]
    fn broker_frames_round_trip_with_snake_case_error_codes_and_reasons() {
        let req = BrokerRequest::ServiceStatus {
            caller: Some("Claude Code".into()),
            host: "staging".into(),
            service: "api.service".into(),
            timeout_secs: Some(20),
        };
        let encoded = serde_json::to_string(&req).unwrap();
        assert_eq!(
            serde_json::from_str::<BrokerRequest>(&encoded).unwrap(),
            req
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
