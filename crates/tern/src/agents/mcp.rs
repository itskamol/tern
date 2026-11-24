//! Stdio MCP (JSON-RPC 2.0) server for `tern mcp` (ADR 0003).
//!
//! Runs before GPUI and single-instance startup so `stdout` carries exclusively newline-delimited
//! MCP JSON-RPC frames and `stderr` carries structured logs. Completes `initialize`, `ping`, and
//! `tools/list` even when tern is closed, dials `agent.sock` fresh per `tools/call`, returns
//! `tern_not_running` without exiting when tern is unavailable, and recovers automatically on the
//! next call once tern starts.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::protocol::{
    AgentError, AgentErrorCode, BoundedLineReader, BrokerMessage, BrokerRequest, BrokerResult,
    FrameError, MAX_FRAME_BYTES, write_json_line,
};
use super::socket_path;

const JSONRPC_VERSION: &str = "2.0";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-03-26";
const ERR_PARSE: i64 = -32700;
const ERR_INVALID_REQUEST: i64 = -32600;
const ERR_METHOD_NOT_FOUND: i64 = -32601;
const ERR_INVALID_PARAMS: i64 = -32602;
const MAX_IN_FLIGHT_CALLS: usize = 32;
const OUTGOING_CHANNEL_CAP: usize = 64;
const CANCEL_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// Runs `tern mcp` over process stdin/stdout until stdin closes.
///
/// # Errors
/// Returns an error if the settings directory cannot be resolved, the tokio runtime cannot start,
/// or stdio framing encounters an unrecoverable I/O error.
pub fn run() -> anyhow::Result<()> {
    init_stderr_logging();
    let dir = crate::settings::dir()
        .ok_or_else(|| anyhow::anyhow!("settings directory is unavailable"))?;
    let sock = socket_path(&dir);
    tracing::info!(socket = %sock.display(), "mcp_stdio_start");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let stdin = BufReader::new(tokio::io::stdin());
        let stdout = tokio::io::stdout();
        serve_stdio(stdin, stdout, sock).await
    })?;
    tracing::info!("mcp_stdio_exit");
    Ok(())
}

fn init_stderr_logging() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = EnvFilter::try_from_env("TERN_LOG").unwrap_or_else(|_| "warn".into());
    let stderr = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(std::io::stderr);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .try_init();
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: Option<String>,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl JsonRpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Default)]
struct SessionState {
    /// Self-reported `clientInfo.name` from MCP `initialize` (unverified label).
    caller: Option<String>,
}

/// Normalized JSON-RPC request ID key for tracking concurrent in-flight `tools/call` tasks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RequestKey {
    String(String),
    Int(i64),
    UInt(u64),
}

impl RequestKey {
    fn from_value(value: &Value) -> Option<Self> {
        if let Some(s) = value.as_str() {
            Some(Self::String(s.to_owned()))
        } else if let Some(i) = value.as_i64() {
            Some(Self::Int(i))
        } else {
            value.as_u64().map(Self::UInt)
        }
    }
}

struct InFlightCall {
    cancel_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

enum Outgoing {
    Progress {
        key: Option<RequestKey>,
        notification: Value,
    },
    Response {
        key: Option<RequestKey>,
        response: JsonRpcResponse,
    },
}

struct ParsedToolCall {
    request: BrokerRequest,
    progress_token: Option<Value>,
}

/// Serves MCP JSON-RPC 2.0 over newline-delimited `reader` and `writer`, dialing `socket_path`
/// fresh for each `tools/call` while remaining responsive to concurrent requests and progress
/// notifications.
///
/// # Errors
/// Returns an [`io::Error`] if reading from `reader` or writing to `writer` fails.
pub async fn serve_stdio<R, W>(reader: R, mut writer: W, socket_path: PathBuf) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut line_reader = BoundedLineReader::new(reader);
    let mut state = SessionState::default();
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(OUTGOING_CHANNEL_CAP);
    let mut in_flight: HashMap<RequestKey, InFlightCall> = HashMap::new();

    let result = loop {
        tokio::select! {
            biased;
            outgoing = out_rx.recv() => {
                let Some(outgoing) = outgoing else {
                    continue;
                };
                match outgoing {
                    Outgoing::Progress { key, notification } => {
                        let active = key.as_ref().is_none_or(|k| in_flight.contains_key(k));
                        if active
                            && let Err(e) = write_json_line(&mut writer, &notification).await
                        {
                            break Err(e);
                        }
                    }
                    Outgoing::Response { key, response } => {
                        let active = match key {
                            Some(k) => in_flight.remove(&k).is_some(),
                            None => true,
                        };
                        if active
                            && let Err(e) = write_json_line(&mut writer, &response).await
                        {
                            break Err(e);
                        }
                    }
                }
            }
            next = line_reader.next_line(MAX_FRAME_BYTES) => {
                let line = match next {
                    Ok(Some(line)) => line,
                    Ok(None) => break Ok(()),
                    Err(FrameError::TooLarge(limit)) => {
                        let resp = JsonRpcResponse::err(
                            Value::Null,
                            ERR_INVALID_REQUEST,
                            format!("frame exceeded maximum size of {limit} bytes"),
                        );
                        if let Err(e) = write_json_line(&mut writer, &resp).await {
                            break Err(e);
                        }
                        continue;
                    }
                    Err(FrameError::Utf8) => {
                        let resp = JsonRpcResponse::err(
                            Value::Null,
                            ERR_PARSE,
                            "request frame was not valid utf-8",
                        );
                        if let Err(e) = write_json_line(&mut writer, &resp).await {
                            break Err(e);
                        }
                        continue;
                    }
                    Err(FrameError::Io(e)) => break Err(e),
                };

                if line.trim().is_empty() {
                    continue;
                }

                let req: JsonRpcRequest = match serde_json::from_str(&line) {
                    Ok(req) => req,
                    Err(e) => {
                        let resp = JsonRpcResponse::err(
                            Value::Null,
                            ERR_PARSE,
                            format!("invalid json-rpc request: {e}"),
                        );
                        if let Err(e) = write_json_line(&mut writer, &resp).await {
                            break Err(e);
                        }
                        continue;
                    }
                };

                if let Some(version) = req.jsonrpc.as_deref()
                    && version != JSONRPC_VERSION
                {
                    if let Some(id) = req.id {
                        let resp = JsonRpcResponse::err(
                            id,
                            ERR_INVALID_REQUEST,
                            format!("unsupported jsonrpc version: {version}"),
                        );
                        if let Err(e) = write_json_line(&mut writer, &resp).await {
                            break Err(e);
                        }
                    }
                    continue;
                }

                if let Some(resp) = dispatch_request(
                    req,
                    &mut state,
                    &socket_path,
                    &out_tx,
                    &mut in_flight,
                ) && let Err(e) = write_json_line(&mut writer, &resp).await
                {
                    break Err(e);
                }
            }
        }
    };

    for (_, call) in in_flight.drain() {
        call.cancel_tx.send_replace(true);
        call.task.abort();
        let _ = call.task.await;
    }

    result
}

fn dispatch_request(
    req: JsonRpcRequest,
    state: &mut SessionState,
    socket_path: &Path,
    out_tx: &mpsc::Sender<Outgoing>,
    in_flight: &mut HashMap<RequestKey, InFlightCall>,
) -> Option<JsonRpcResponse> {
    // Notifications have no `id` and must never receive a JSON-RPC response.
    let Some(id) = req.id else {
        if req.method == "notifications/cancelled"
            && let Some(key) = req.params.get("requestId").and_then(RequestKey::from_value)
            && let Some(call) = in_flight.remove(&key)
        {
            call.cancel_tx.send_replace(true);
        }
        return None;
    };

    let resp = match req.method.as_str() {
        "initialize" => {
            state.caller = extract_caller_name(&req.params);
            let protocol_version = req
                .params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            JsonRpcResponse::ok(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "tern",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
        }
        "ping" => JsonRpcResponse::ok(id, json!({})),
        "tools/list" => JsonRpcResponse::ok(id, tools_list_payload()),
        "tools/call" => match parse_tool_call(&req.params, state.caller.clone()) {
            Ok(parsed) => {
                if in_flight.len() >= MAX_IN_FLIGHT_CALLS {
                    return Some(JsonRpcResponse::err(
                        id,
                        ERR_INVALID_REQUEST,
                        "too many concurrent tool calls in flight",
                    ));
                }
                let key = RequestKey::from_value(&id);
                let socket = socket_path.to_path_buf();
                let tx = out_tx.clone();
                let key_for_task = key.clone();
                let (cancel_tx, cancel_rx) = watch::channel(false);
                let task = tokio::spawn(async move {
                    let Some(result) = call_broker(
                        &socket,
                        &parsed.request,
                        parsed.progress_token,
                        key_for_task.clone(),
                        &tx,
                        cancel_rx,
                    )
                    .await
                    else {
                        return;
                    };
                    let response = JsonRpcResponse::ok(id, format_tool_result(result));
                    let _ = tx
                        .send(Outgoing::Response {
                            key: key_for_task,
                            response,
                        })
                        .await;
                });
                if let Some(k) = key
                    && let Some(prev) = in_flight.insert(k, InFlightCall { cancel_tx, task })
                {
                    prev.cancel_tx.send_replace(true);
                    prev.task.abort();
                }
                return None;
            }
            Err(message) => JsonRpcResponse::err(id, ERR_INVALID_PARAMS, message),
        },
        other => JsonRpcResponse::err(
            id,
            ERR_METHOD_NOT_FOUND,
            format!("method not found: {other}"),
        ),
    };
    Some(resp)
}

fn extract_caller_name(params: &Value) -> Option<String> {
    params
        .get("clientInfo")
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Static typed MCP tool definitions for `list_hosts` and `run_command`.
#[must_use]
pub fn tools_list_payload() -> Value {
    json!({
        "tools": [
            {
                "name": "list_hosts",
                "description": "List Saved connections in tern available to Agents.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "run_command",
                "description": "Run a shell command or multi-line script on a Saved connection in tern over an isolated non-PTY SSH exec connection. Requires approval in tern unless an active Host Grant exists.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": {
                            "type": "string",
                            "description": "Saved connection alias returned by list_hosts."
                        },
                        "command": {
                            "type": "string",
                            "description": "Shell command or inline script to run in the remote user's login shell (up to 64 KiB)."
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 300,
                            "description": "Optional wall-clock timeout in seconds (defaults to 30, clamped to 1..=300)."
                        }
                    },
                    "required": ["host", "command"],
                    "additionalProperties": false
                }
            }
        ]
    })
}

fn extract_progress_token(params: &Value) -> Option<Value> {
    let token = params.get("_meta")?.get("progressToken")?;
    match token {
        Value::String(s) if !s.is_empty() => Some(Value::String(s.clone())),
        Value::Number(n) if n.is_i64() || n.is_u64() => Some(Value::Number(n.clone())),
        _ => None,
    }
}

fn parse_tool_call(params: &Value, caller: Option<String>) -> Result<ParsedToolCall, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing string parameter 'name'".to_owned())?;
    let args = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(obj) if obj.is_object() => obj.clone(),
        Some(_) => return Err("parameter 'arguments' must be an object".to_owned()),
    };
    let progress_token = extract_progress_token(params);

    let request = match name {
        "list_hosts" => BrokerRequest::ListHosts { caller },
        "run_command" => {
            let host = args
                .get("host")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument 'host'".to_owned())?
                .to_owned();
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument 'command'".to_owned())?
                .to_owned();
            let timeout_secs = match args.get("timeout_secs") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    if let Some(u) = v.as_u64() {
                        Some(u)
                    } else if let Some(i) = v.as_i64() {
                        // Negative integers clamp cleanly to 0 here and to 1s in the broker.
                        Some(u64::try_from(i).unwrap_or(0))
                    } else {
                        return Err("argument 'timeout_secs' must be an integer".to_owned());
                    }
                }
            };
            BrokerRequest::RunCommand {
                caller,
                host,
                command,
                timeout_secs,
                wants_progress: progress_token.is_some(),
            }
        }
        other => return Err(format!("unknown tool: {other}")),
    };
    Ok(ParsedToolCall {
        request,
        progress_token,
    })
}

fn format_tool_result(result: BrokerResult) -> Value {
    match result {
        BrokerResult::Ok { data } => {
            let text = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_owned());
            json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": data,
                "isError": false
            })
        }
        BrokerResult::Error(err) => {
            let data = serde_json::to_value(&err).unwrap_or_else(|_| json!({}));
            let text = serde_json::to_string(&err).unwrap_or_else(|_| "{}".to_owned());
            json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": data,
                "isError": true
            })
        }
    }
}

fn tern_not_running(detail: &str) -> BrokerResult {
    BrokerResult::Error(AgentError::new(
        AgentErrorCode::TernNotRunning,
        format!("tern is not running or local MCP access is disabled ({detail})"),
    ))
}

#[cfg(unix)]
async fn wait_cancelled(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|&c| c).await;
}

#[cfg(unix)]
async fn call_broker(
    socket_path: &Path,
    request: &BrokerRequest,
    progress_token: Option<Value>,
    key: Option<RequestKey>,
    out_tx: &mpsc::Sender<Outgoing>,
    mut cancel_rx: watch::Receiver<bool>,
) -> Option<BrokerResult> {
    let stream = tokio::select! {
        biased;
        () = wait_cancelled(&mut cancel_rx) => return None,
        connected = tokio::net::UnixStream::connect(socket_path) => match connected {
            Ok(stream) => stream,
            Err(e) => {
                tracing::debug!(error = %e, "mcp_broker_unavailable");
                return Some(tern_not_running(&e.to_string()));
            }
        },
    };
    let (read_half, mut write_half) = stream.into_split();
    let write_res = tokio::select! {
        biased;
        () = wait_cancelled(&mut cancel_rx) => return None,
        res = write_json_line(&mut write_half, request) => res,
    };
    if let Err(e) = write_res {
        tracing::warn!(error = %e, "mcp_broker_write_failed");
        return Some(tern_not_running(&e.to_string()));
    }
    let mut reader = BoundedLineReader::new(BufReader::new(read_half));
    let mut progress_step: u64 = 0;
    loop {
        let next = tokio::select! {
            biased;
            () = wait_cancelled(&mut cancel_rx) => {
                let _ = tokio::time::timeout(
                    CANCEL_WRITE_TIMEOUT,
                    write_json_line(&mut write_half, &BrokerRequest::Cancel),
                )
                .await;
                return None;
            }
            line = reader.next_line(MAX_FRAME_BYTES) => line,
        };
        match next {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<BrokerMessage>(&line) {
                    Ok(BrokerMessage::Progress { message }) => {
                        if let Some(token) = progress_token.as_ref() {
                            progress_step = progress_step.saturating_add(1);
                            let notification = json!({
                                "jsonrpc": JSONRPC_VERSION,
                                "method": "notifications/progress",
                                "params": {
                                    "progressToken": token,
                                    "progress": progress_step,
                                    "message": message,
                                }
                            });
                            let _ = out_tx
                                .send(Outgoing::Progress {
                                    key: key.clone(),
                                    notification,
                                })
                                .await;
                        } else {
                            tracing::debug!(progress = %message, "mcp_broker_progress");
                        }
                    }
                    Ok(BrokerMessage::Result(result)) => return Some(result),
                    Err(e) => {
                        tracing::warn!(error = %e, "mcp_broker_bad_frame");
                        return Some(BrokerResult::Error(AgentError::new(
                            AgentErrorCode::SshError,
                            "received malformed response from tern broker",
                        )));
                    }
                }
            }
            Ok(None) => {
                return Some(tern_not_running("connection closed before response"));
            }
            Err(e) => {
                return Some(tern_not_running(&e.to_string()));
            }
        }
    }
}

#[cfg(not(unix))]
async fn call_broker(
    _socket_path: &Path,
    _request: &BrokerRequest,
    _progress_token: Option<Value>,
    _key: Option<RequestKey>,
    _out_tx: &mpsc::Sender<Outgoing>,
    _cancel_rx: watch::Receiver<bool>,
) -> Option<BrokerResult> {
    Some(tern_not_running(
        "local unix socket broker is unsupported on this platform",
    ))
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
