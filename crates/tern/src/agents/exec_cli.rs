//! Streaming CLI client for `tern exec [--timeout <secs>] <host> -- <command>`.
//!
//! Connects to tern's local `agent.sock` broker so CLI execution reuses Saved connections, Vault,
//! Decision approval, Host Grants, and Activity without opening a GUI window. Streams remote
//! stdout and stderr chunks to local stdout and stderr as they arrive, preserves the remote exit
//! code, and on `Ctrl-C` requests remote cancellation while honestly noting that SSH servers may
//! ignore signal requests (RFC 4254 §6.9).

use std::io;
use std::path::Path;

use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use tokio::sync::watch;

use super::client::{ClientError, ClientFrame, ClientSession};
use super::protocol::{BrokerRequest, BrokerResult, ExecStreamResponse};
use super::socket_path;

const EXIT_INTERRUPTED: i32 = 130;

/// Parsed CLI arguments for `tern exec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCliArgs {
    pub host: String,
    pub command: String,
    pub timeout_secs: Option<u64>,
}

/// Runs `tern exec` against the running tern broker and returns the desired process exit code.
///
/// # Errors
/// Returns an error if the settings directory cannot be resolved or the tokio runtime fails.
pub fn run(args: ExecCliArgs) -> anyhow::Result<i32> {
    let dir = crate::settings::dir()
        .ok_or_else(|| anyhow::anyhow!("settings directory is unavailable"))?;
    let sock = socket_path(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let code = rt.block_on(async move {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let sig_task = tokio::spawn(async move {
            if wait_interrupt().await {
                cancel_tx.send_replace(true);
            }
        });
        let mut stdout = tokio::io::stdout();
        let mut stderr = tokio::io::stderr();
        let res = run_client(&sock, args, &mut stdout, &mut stderr, cancel_rx).await;
        sig_task.abort();
        res
    })?;
    Ok(code)
}

async fn wait_interrupt() -> bool {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            res = tokio::signal::ctrl_c() => res.is_ok(),
            _ = async {
                match term.as_mut() {
                    Some(sig) => {
                        sig.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => true,
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.is_ok()
    }
}

fn exit_code_for_signal(sig: &str) -> i32 {
    let upper = sig.to_ascii_uppercase();
    let num = match upper.as_str() {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ABRT" => 6,
        "KILL" => 9,
        "ALRM" => 14,
        "TERM" => 15,
        _ => 0,
    };
    128 + num
}

async fn write_err_line<W: AsyncWrite + Unpin>(stderr: &mut W, msg: &str) -> io::Result<()> {
    stderr.write_all(msg.as_bytes()).await?;
    stderr.write_all(b"\n").await?;
    stderr.flush().await
}

/// Core testable streaming client driving one `BrokerRequest::ExecStream` over `socket_path`.
///
/// # Errors
/// Returns an [`io::Error`] if writing to `stdout` or `stderr` fails.
pub async fn run_client<WOut, WErr>(
    socket_path: &Path,
    args: ExecCliArgs,
    stdout: &mut WOut,
    stderr: &mut WErr,
    cancel_rx: watch::Receiver<bool>,
) -> io::Result<i32>
where
    WOut: AsyncWrite + Unpin,
    WErr: AsyncWrite + Unpin,
{
    let request = BrokerRequest::ExecStream {
        caller: Some("tern exec".to_owned()),
        host: args.host,
        command: args.command,
        timeout_secs: args.timeout_secs,
        wants_progress: true,
    };

    let mut session = match ClientSession::connect(socket_path, &request, cancel_rx).await {
        Ok(s) => s,
        Err(err) => return handle_client_error(stderr, err).await,
    };

    loop {
        match session.next_frame().await {
            Ok(ClientFrame::Progress(message)) => {
                write_err_line(stderr, &format!("tern exec: {message}")).await?;
            }
            Ok(ClientFrame::Stdout(bytes)) => {
                stdout.write_all(&bytes).await?;
                stdout.flush().await?;
            }
            Ok(ClientFrame::Stderr(bytes)) => {
                stderr.write_all(&bytes).await?;
                stderr.flush().await?;
            }
            Ok(ClientFrame::Result(BrokerResult::Ok { data })) => {
                let Ok(done) = serde_json::from_value::<ExecStreamResponse>(data) else {
                    return Ok(0);
                };
                if let Some(code) = done.exit_code {
                    return Ok(i32::try_from(code.min(255)).unwrap_or(255));
                }
                if let Some(sig) = done.signal {
                    write_err_line(
                        stderr,
                        &format!("tern exec: remote process terminated by signal {sig}"),
                    )
                    .await?;
                    return Ok(exit_code_for_signal(&sig));
                }
                return Ok(0);
            }
            Ok(ClientFrame::Result(BrokerResult::Error(err))) => {
                write_err_line(stderr, &format!("tern exec: {}", err.message)).await?;
                return Ok(1);
            }
            Err(err) => return handle_client_error(stderr, err).await,
        }
    }
}

async fn handle_client_error<WErr: AsyncWrite + Unpin>(
    stderr: &mut WErr,
    err: ClientError,
) -> io::Result<i32> {
    match err {
        ClientError::Cancelled => {
            write_err_line(
                stderr,
                "tern exec: interrupted (requested remote cancellation; remote process stop is not guaranteed)",
            )
            .await?;
            Ok(EXIT_INTERRUPTED)
        }
        ClientError::PermissionDenied(e) => {
            write_err_line(
                stderr,
                &format!("tern exec: permission denied connecting to tern agent.sock ({e})"),
            )
            .await?;
            Ok(1)
        }
        ClientError::Unavailable(e) => {
            write_err_line(
                stderr,
                &format!(
                    "tern exec: tern is not running or local MCP/Agent broker is disabled ({e})"
                ),
            )
            .await?;
            Ok(1)
        }
        ClientError::WriteFailed(e) => {
            write_err_line(stderr, &format!("tern exec: failed to send request ({e})")).await?;
            Ok(1)
        }
        ClientError::ReadFailed(e) => {
            write_err_line(stderr, &format!("tern exec: broker read error ({e})")).await?;
            Ok(1)
        }
        ClientError::MalformedResponse(e) => {
            write_err_line(
                stderr,
                &format!("tern exec: malformed broker response ({e})"),
            )
            .await?;
            Ok(1)
        }
        ClientError::ClosedUnexpectedly => {
            write_err_line(stderr, "tern exec: broker closed connection unexpectedly").await?;
            Ok(1)
        }
        #[cfg(not(unix))]
        ClientError::UnsupportedPlatform => {
            write_err_line(
                stderr,
                "tern exec: local unix socket broker is unsupported on this platform",
            )
            .await?;
            Ok(1)
        }
    }
}
