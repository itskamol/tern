//! Shared local Unix socket client for `agent.sock` used by `tern mcp` and `tern exec`.
//!
//! Handles connection setup, cancellation during connect/write/read, bounded newline-delimited
//! JSON framing, and `BrokerRequest::Cancel` notification under a short write timeout.

use std::io;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use tokio::sync::watch;

use super::protocol::{
    BoundedLineReader, BrokerMessage, BrokerRequest, BrokerResult, FrameError, MAX_FRAME_BYTES,
    write_json_line,
};

const CANCEL_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// Frame received from the local `agent.sock` broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame {
    Progress(String),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Result(BrokerResult),
}

/// Transport or framing failure while talking to the local `agent.sock` broker.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cancelled by caller")]
    Cancelled,
    #[error("permission denied connecting to tern agent.sock ({0})")]
    PermissionDenied(io::Error),
    #[error("tern is not running or local broker is unavailable ({0})")]
    Unavailable(io::Error),
    #[error("failed to send request to tern broker ({0})")]
    WriteFailed(io::Error),
    #[error("broker read error ({0})")]
    ReadFailed(FrameError),
    #[error("malformed broker response ({0})")]
    MalformedResponse(serde_json::Error),
    #[error("broker closed connection unexpectedly")]
    ClosedUnexpectedly,
    #[cfg(not(unix))]
    #[error("local unix socket broker is unsupported on this platform")]
    UnsupportedPlatform,
}

#[cfg(unix)]
async fn wait_cancelled(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|&c| c).await;
}

/// Active single-request session over `agent.sock`.
#[cfg(unix)]
pub struct ClientSession {
    reader: BoundedLineReader<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
    cancel_rx: watch::Receiver<bool>,
}

#[cfg(unix)]
impl ClientSession {
    /// Connects to `socket_path` and sends `request`.
    ///
    /// # Errors
    /// Returns [`ClientError`] on cancellation, connection failure, or write failure.
    pub async fn connect(
        socket_path: &Path,
        request: &BrokerRequest,
        mut cancel_rx: watch::Receiver<bool>,
    ) -> Result<Self, ClientError> {
        let stream = tokio::select! {
            biased;
            () = wait_cancelled(&mut cancel_rx) => return Err(ClientError::Cancelled),
            connected = tokio::net::UnixStream::connect(socket_path) => match connected {
                Ok(stream) => stream,
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(ClientError::PermissionDenied(e));
                }
                Err(e) => return Err(ClientError::Unavailable(e)),
            },
        };

        let (read_half, mut write_half) = stream.into_split();
        let write_res = tokio::select! {
            biased;
            () = wait_cancelled(&mut cancel_rx) => return Err(ClientError::Cancelled),
            res = write_json_line(&mut write_half, request) => res,
        };
        if let Err(e) = write_res {
            return Err(ClientError::WriteFailed(e));
        }

        Ok(Self {
            reader: BoundedLineReader::new(tokio::io::BufReader::new(read_half)),
            writer: write_half,
            cancel_rx,
        })
    }

    /// Reads the next broker frame, sending `BrokerRequest::Cancel` if `cancel_rx` fires.
    ///
    /// # Errors
    /// Returns [`ClientError`] on cancellation, EOF, read error, or malformed JSON.
    pub async fn next_frame(&mut self) -> Result<ClientFrame, ClientError> {
        loop {
            let next = tokio::select! {
                biased;
                () = wait_cancelled(&mut self.cancel_rx) => {
                    let _ = tokio::time::timeout(
                        CANCEL_WRITE_TIMEOUT,
                        write_json_line(&mut self.writer, &BrokerRequest::Cancel),
                    )
                    .await;
                    return Err(ClientError::Cancelled);
                }
                line = self.reader.next_line(MAX_FRAME_BYTES) => line,
            };

            match next {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<BrokerMessage>(&line) {
                        Ok(BrokerMessage::Progress { message }) => {
                            return Ok(ClientFrame::Progress(message));
                        }
                        Ok(BrokerMessage::StdoutChunk { data_b64 }) => {
                            if let Ok(bytes) =
                                base64::engine::general_purpose::STANDARD.decode(&data_b64)
                            {
                                return Ok(ClientFrame::Stdout(bytes));
                            }
                        }
                        Ok(BrokerMessage::StderrChunk { data_b64 }) => {
                            if let Ok(bytes) =
                                base64::engine::general_purpose::STANDARD.decode(&data_b64)
                            {
                                return Ok(ClientFrame::Stderr(bytes));
                            }
                        }
                        Ok(BrokerMessage::Result(result)) => {
                            return Ok(ClientFrame::Result(result));
                        }
                        Err(e) => return Err(ClientError::MalformedResponse(e)),
                    }
                }
                Ok(None) => return Err(ClientError::ClosedUnexpectedly),
                Err(e) => return Err(ClientError::ReadFailed(e)),
            }
        }
    }
}

/// Non-Unix stub session.
#[cfg(not(unix))]
pub struct ClientSession;

#[cfg(not(unix))]
impl ClientSession {
    /// Always returns [`ClientError::UnsupportedPlatform`].
    ///
    /// # Errors
    /// Always returns [`ClientError::UnsupportedPlatform`].
    pub async fn connect(
        _socket_path: &Path,
        _request: &BrokerRequest,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Result<Self, ClientError> {
        Err(ClientError::UnsupportedPlatform)
    }

    /// Always returns [`ClientError::UnsupportedPlatform`].
    ///
    /// # Errors
    /// Always returns [`ClientError::UnsupportedPlatform`].
    pub async fn next_frame(&mut self) -> Result<ClientFrame, ClientError> {
        Err(ClientError::UnsupportedPlatform)
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::net::UnixListener;

    use super::*;

    #[tokio::test]
    async fn session_forwards_progress_and_chunks_and_returns_result() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let req_line = lines.next_line().await.unwrap().unwrap();
            let req: BrokerRequest = serde_json::from_str(&req_line).unwrap();
            assert!(matches!(req, BrokerRequest::ListHosts { .. }));

            write_json_line(
                &mut write_half,
                &BrokerMessage::Progress {
                    message: "waiting".into(),
                },
            )
            .await
            .unwrap();
            write_json_line(
                &mut write_half,
                &BrokerMessage::StdoutChunk {
                    data_b64: base64::engine::general_purpose::STANDARD.encode(b"hi"),
                },
            )
            .await
            .unwrap();
            write_json_line(
                &mut write_half,
                &BrokerMessage::Result(BrokerResult::Ok {
                    data: json!({"ok": true}),
                }),
            )
            .await
            .unwrap();
        });

        let (_tx, rx) = watch::channel(false);
        let mut session = ClientSession::connect(
            &sock,
            &BrokerRequest::ListHosts {
                caller: Some("test".into()),
                query: None,
            },
            rx,
        )
        .await
        .unwrap();

        assert_eq!(
            session.next_frame().await.unwrap(),
            ClientFrame::Progress("waiting".into())
        );
        assert_eq!(
            session.next_frame().await.unwrap(),
            ClientFrame::Stdout(b"hi".to_vec())
        );
        assert_eq!(
            session.next_frame().await.unwrap(),
            ClientFrame::Result(BrokerResult::Ok {
                data: json!({"ok": true})
            })
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn session_sends_cancel_frame_when_cancelled_mid_flight() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, _write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let _req = lines.next_line().await.unwrap().unwrap();
            let cancel_line = lines.next_line().await.unwrap().unwrap();
            let cancel: BrokerRequest = serde_json::from_str(&cancel_line).unwrap();
            assert_eq!(cancel, BrokerRequest::Cancel);
        });

        let (tx, rx) = watch::channel(false);
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send_replace(true);
        });

        let mut session = ClientSession::connect(
            &sock,
            &BrokerRequest::ListHosts {
                caller: None,
                query: None,
            },
            rx,
        )
        .await
        .unwrap();
        let err = session.next_frame().await.unwrap_err();

        cancel_task.await.unwrap();
        server.await.unwrap();
        assert!(matches!(err, ClientError::Cancelled));
    }

    #[tokio::test]
    async fn session_reports_unavailable_when_socket_missing() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("missing.sock");
        let (_tx, rx) = watch::channel(false);
        let err = ClientSession::connect(
            &sock,
            &BrokerRequest::ListHosts {
                caller: None,
                query: None,
            },
            rx,
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(err, ClientError::Unavailable(_)));
    }
}
