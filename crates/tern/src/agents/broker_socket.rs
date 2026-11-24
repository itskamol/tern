//! Dedicated `0o600` Unix domain socket listener and framed connection loop for `agent.sock`.
//!
//! Runs on [`crate::platform::SshRuntime`] with bounded concurrency and hands decoded
//! [`BrokerRequest`]s to [`super::broker`] alongside a per-connection response/cancel channel.

use std::io;
use std::path::Path;
use std::sync::Arc;

use tokio::io::BufReader;
use tokio::sync::{Semaphore, watch};

use super::protocol::{
    AgentError, AgentErrorCode, BrokerMessage, BrokerRequest, BrokerResult, MAX_FRAME_BYTES,
    read_line_bounded, write_json_line,
};

const MAX_CONCURRENT_CONNECTIONS: usize = 32;

/// Per-connection response and cancellation channel for one accepted `agent.sock` client.
#[derive(Debug)]
pub(super) struct Responder {
    tx: async_channel::Sender<BrokerMessage>,
    /// Fires when the client sends `BrokerRequest::Cancel` or disconnects (#62).
    #[allow(dead_code)]
    pub(super) cancelled: watch::Receiver<bool>,
}

impl Responder {
    pub(super) async fn finish(self, result: BrokerResult) {
        let _ = self.tx.send(BrokerMessage::Result(result)).await;
    }
}

#[derive(Debug)]
pub(super) struct IncomingRequest {
    pub(super) request: BrokerRequest,
    pub(super) responder: Responder,
}

#[cfg(unix)]
pub(super) fn bind_and_serve(
    socket_path: &Path,
    rt: &tokio::runtime::Handle,
    req_tx: async_channel::Sender<IncomingRequest>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(socket_path);

    let listener = {
        let _guard = rt.enter();
        tokio::net::UnixListener::bind(socket_path)?
    };
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;

    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    rt.spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|&stop| stop) => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                            tracing::warn!("agent_broker_connection_limit_reached");
                            drop(stream);
                            continue;
                        };
                        let tx = req_tx.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            serve_connection(stream, tx).await;
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "agent_broker_accept_failed");
                        break;
                    }
                },
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn bind_and_serve(
    _socket_path: &Path,
    _rt: &tokio::runtime::Handle,
    _req_tx: async_channel::Sender<IncomingRequest>,
    _shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unix domain sockets are unsupported on this platform",
    ))
}

#[cfg(unix)]
async fn serve_connection(
    stream: tokio::net::UnixStream,
    req_tx: async_channel::Sender<IncomingRequest>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let line = match read_line_bounded(&mut reader, MAX_FRAME_BYTES).await {
        Ok(Some(line)) => line,
        Ok(None) => return,
        Err(e) => {
            let _ = write_json_line(
                &mut write_half,
                &BrokerMessage::Result(BrokerResult::Error(AgentError::new(
                    AgentErrorCode::Denied,
                    e.to_string(),
                ))),
            )
            .await;
            return;
        }
    };

    let request: BrokerRequest = match serde_json::from_str(&line) {
        Ok(req) => req,
        Err(e) => {
            let _ = write_json_line(
                &mut write_half,
                &BrokerMessage::Result(BrokerResult::Error(AgentError::new(
                    AgentErrorCode::Denied,
                    format!("invalid broker request: {e}"),
                ))),
            )
            .await;
            return;
        }
    };

    let (resp_tx, resp_rx) = async_channel::bounded::<BrokerMessage>(8);
    let (cancel_tx, cancel_rx) = watch::channel(false);

    if req_tx
        .send(IncomingRequest {
            request,
            responder: Responder {
                tx: resp_tx,
                cancelled: cancel_rx,
            },
        })
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            biased;
            next = read_line_bounded(&mut reader, MAX_FRAME_BYTES) => {
                match next {
                    Ok(None) | Err(_) => {
                        cancel_tx.send_replace(true);
                        break;
                    }
                    Ok(Some(extra)) => {
                        if matches!(serde_json::from_str::<BrokerRequest>(&extra), Ok(BrokerRequest::Cancel)) {
                            cancel_tx.send_replace(true);
                        }
                    }
                }
            }
            msg = resp_rx.recv() => match msg {
                Ok(message) => {
                    let done = matches!(message, BrokerMessage::Result(_));
                    if write_json_line(&mut write_half, &message).await.is_err() || done {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
}
