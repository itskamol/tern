//! Stdio MCP (JSON-RPC 2.0) server for `tern mcp` (ADR 0003).
//!
//! Runs before GPUI and single-instance startup so `stdout` carries exclusively newline-delimited
//! MCP JSON-RPC frames and `stderr` carries structured logs. Completes `initialize`, `ping`, and
//! `tools/list` even when tern is closed, dials `agent.sock` fresh per `tools/call`, returns
//! `tern_not_running` without exiting when tern is unavailable, and recovers automatically on the
//! next call once tern starts.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};

use super::protocol::{
    AgentError, AgentErrorCode, BrokerMessage, BrokerRequest, BrokerResult, FrameError,
    MAX_FRAME_BYTES, read_line_bounded, write_json_line,
};
use super::socket_path;

const JSONRPC_VERSION: &str = "2.0";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-03-26";
const ERR_PARSE: i64 = -32700;
const ERR_INVALID_REQUEST: i64 = -32600;
const ERR_METHOD_NOT_FOUND: i64 = -32601;
const ERR_INVALID_PARAMS: i64 = -32602;

/// Runs `tern mcp` over process stdin/stdout until stdin closes.
///
/// # Errors
/// Returns an error if the settings directory cannot be resolved, the tokio runtime cannot start,
/// or stdio framing encounters an unrecoverable I/O error.
pub fn run(socket_override: Option<PathBuf>) -> anyhow::Result<()> {
    init_stderr_logging();
    let sock = match socket_override {
        Some(path) => path,
        None => {
            let dir = crate::settings::dir()
                .ok_or_else(|| anyhow::anyhow!("settings directory is unavailable"))?;
            socket_path(&dir)
        }
    };
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

/// Serves MCP JSON-RPC 2.0 over newline-delimited `reader` and `writer`, dialing `socket_path`
/// fresh for each `tools/call`.
///
/// # Errors
/// Returns an [`io::Error`] if reading from `reader` or writing to `writer` fails.
pub async fn serve_stdio<R, W>(mut reader: R, mut writer: W, socket_path: PathBuf) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut state = SessionState::default();

    loop {
        let line = match read_line_bounded(&mut reader, MAX_FRAME_BYTES).await {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(()),
            Err(FrameError::TooLarge(limit)) => {
                let resp = JsonRpcResponse::err(
                    Value::Null,
                    ERR_INVALID_REQUEST,
                    format!("frame exceeded maximum size of {limit} bytes"),
                );
                write_json_line(&mut writer, &resp).await?;
                continue;
            }
            Err(FrameError::Utf8) => {
                let resp = JsonRpcResponse::err(
                    Value::Null,
                    ERR_PARSE,
                    "request frame was not valid utf-8",
                );
                write_json_line(&mut writer, &resp).await?;
                continue;
            }
            Err(FrameError::Io(e)) => return Err(e),
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
                write_json_line(&mut writer, &resp).await?;
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
                write_json_line(&mut writer, &resp).await?;
            }
            continue;
        }

        let response = handle_request(req, &mut state, &socket_path).await;
        if let Some(resp) = response {
            write_json_line(&mut writer, &resp).await?;
        }
    }
}

async fn handle_request(
    req: JsonRpcRequest,
    state: &mut SessionState,
    socket_path: &Path,
) -> Option<JsonRpcResponse> {
    // Notifications have no `id` and must never receive a JSON-RPC response.
    let id = req.id?;

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
            Ok(broker_req) => {
                let result = call_broker(socket_path, &broker_req).await;
                JsonRpcResponse::ok(id, format_tool_result(result))
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

/// Static typed MCP tool definitions for `list_hosts` and `service_status`.
#[must_use]
pub fn tools_list_payload() -> Value {
    json!({
        "tools": [
            {
                "name": "list_hosts",
                "description": "List Saved connections in tern that allow Agent systemd service checks, their allowed service unit names, and usage instructions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "service_status",
                "description": "Check systemd status (LoadState, ActiveState, SubState) of an allowlisted service on a Saved connection in tern. Requires explicit approval in tern.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "host": {
                            "type": "string",
                            "description": "Saved connection alias returned by list_hosts."
                        },
                        "service": {
                            "type": "string",
                            "description": "Allowlisted systemd service unit name (e.g. api.service)."
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 60,
                            "description": "Optional wall-clock timeout in seconds (defaults to 15, clamped to 1..=60)."
                        }
                    },
                    "required": ["host", "service"],
                    "additionalProperties": false
                }
            }
        ]
    })
}

fn parse_tool_call(params: &Value, caller: Option<String>) -> Result<BrokerRequest, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing string parameter 'name'".to_owned())?;
    let args = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(obj) if obj.is_object() => obj.clone(),
        Some(_) => return Err("parameter 'arguments' must be an object".to_owned()),
    };

    match name {
        "list_hosts" => Ok(BrokerRequest::ListHosts { caller }),
        "service_status" => {
            let host = args
                .get("host")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument 'host'".to_owned())?
                .to_owned();
            let service = args
                .get("service")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing string argument 'service'".to_owned())?
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
            Ok(BrokerRequest::ServiceStatus {
                caller,
                host,
                service,
                timeout_secs,
            })
        }
        other => Err(format!("unknown tool: {other}")),
    }
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
async fn call_broker(socket_path: &Path, request: &BrokerRequest) -> BrokerResult {
    let stream = match tokio::net::UnixStream::connect(socket_path).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::debug!(error = %e, "mcp_broker_unavailable");
            return tern_not_running(&e.to_string());
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    if let Err(e) = write_json_line(&mut write_half, request).await {
        tracing::warn!(error = %e, "mcp_broker_write_failed");
        return tern_not_running(&e.to_string());
    }
    let mut reader = BufReader::new(read_half);
    loop {
        match read_line_bounded(&mut reader, MAX_FRAME_BYTES).await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<BrokerMessage>(&line) {
                    Ok(BrokerMessage::Progress { message }) => {
                        tracing::debug!(progress = %message, "mcp_broker_progress");
                    }
                    Ok(BrokerMessage::Result(result)) => return result,
                    Err(e) => {
                        tracing::warn!(error = %e, "mcp_broker_bad_frame");
                        return BrokerResult::Error(AgentError::new(
                            AgentErrorCode::SshError,
                            "received malformed response from tern broker",
                        ));
                    }
                }
            }
            Ok(None) => {
                return tern_not_running("connection closed before response");
            }
            Err(e) => {
                return tern_not_running(&e.to_string());
            }
        }
    }
}

#[cfg(not(unix))]
async fn call_broker(_socket_path: &Path, _request: &BrokerRequest) -> BrokerResult {
    tern_not_running("local unix socket broker is unsupported on this platform")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::*;

    async fn exchange(
        writer: &mut tokio::io::DuplexStream,
        reader: &mut BufReader<tokio::io::DuplexStream>,
        req: Value,
    ) -> Value {
        let mut line = serde_json::to_vec(&req).unwrap();
        line.push(b'\n');
        writer.write_all(&line).await.unwrap();
        writer.flush().await.unwrap();
        let mut out = String::new();
        reader.read_line(&mut out).await.unwrap();
        serde_json::from_str(&out).unwrap()
    }

    #[tokio::test]
    async fn initialize_ping_tools_list_and_closed_tern_recovery_work_without_restarting() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("agent.sock");

        let (mut client_tx, server_rx) = tokio::io::duplex(64 * 1024);
        let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
        let mut client_rx = BufReader::new(client_rx);

        let server = tokio::spawn(serve_stdio(
            BufReader::new(server_rx),
            server_tx,
            sock.clone(),
        ));

        // 1. initialize works while tern is closed and records clientInfo.name.
        let init = exchange(
            &mut client_tx,
            &mut client_rx,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "clientInfo": { "name": "Cursor", "version": "1.0" }
                }
            }),
        )
        .await;
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "tern");

        // 2. notifications/initialized emits no response; ping answers immediately.
        client_tx
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        let pong = exchange(
            &mut client_tx,
            &mut client_rx,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
        )
        .await;
        assert_eq!(pong["id"], 2);
        assert_eq!(pong["result"], json!({}));

        // 3. tools/list works while tern is closed and exposes typed schemas.
        let tools = exchange(
            &mut client_tx,
            &mut client_rx,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
        )
        .await;
        let list = tools["result"]["tools"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["name"], "list_hosts");
        assert_eq!(list[1]["name"], "service_status");
        assert_eq!(
            list[1]["inputSchema"]["required"],
            json!(["host", "service"])
        );

        // 4. tools/call while closed returns tern_not_running and keeps tern mcp alive.
        let closed = exchange(
            &mut client_tx,
            &mut client_rx,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": { "name": "list_hosts", "arguments": {} }
            }),
        )
        .await;
        assert_eq!(closed["result"]["isError"], true);
        assert_eq!(
            closed["result"]["structuredContent"]["error"],
            "tern_not_running"
        );

        // 5. Once agent.sock binds later, the next call succeeds automatically without restart.
        #[cfg(unix)]
        {
            let listener = tokio::net::UnixListener::bind(&sock).unwrap();
            let fake_broker = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (r, mut w) = stream.into_split();
                let mut r = BufReader::new(r);
                let line = read_line_bounded(&mut r, MAX_FRAME_BYTES)
                    .await
                    .unwrap()
                    .unwrap();
                let req: BrokerRequest = serde_json::from_str(&line).unwrap();
                assert_eq!(
                    req,
                    BrokerRequest::ListHosts {
                        caller: Some("Cursor".into())
                    }
                );
                // Progress frames before the final Result are tolerated seamlessly.
                write_json_line(
                    &mut w,
                    &BrokerMessage::Progress {
                        message: "checking...".into(),
                    },
                )
                .await
                .unwrap();
                write_json_line(
                    &mut w,
                    &BrokerMessage::Result(BrokerResult::Ok {
                        data: json!({
                            "hosts": [{ "alias": "staging", "services": ["api.service"] }],
                            "usage": "call service_status"
                        }),
                    }),
                )
                .await
                .unwrap();
            });

            let recovered = exchange(
                &mut client_tx,
                &mut client_rx,
                json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "tools/call",
                    "params": { "name": "list_hosts", "arguments": {} }
                }),
            )
            .await;
            fake_broker.await.unwrap();
            assert_eq!(recovered["result"]["isError"], false);
            assert_eq!(
                recovered["result"]["structuredContent"]["hosts"][0]["alias"],
                "staging"
            );
        }

        // 6. Malformed JSON and invalid params return JSON-RPC errors without crashing.
        client_tx.write_all(b"{bad json\n").await.unwrap();
        let mut line = String::new();
        client_rx.read_line(&mut line).await.unwrap();
        let parse_err: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parse_err["error"]["code"], ERR_PARSE);

        let bad_tool = exchange(
            &mut client_tx,
            &mut client_rx,
            json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": { "name": "service_status", "arguments": { "host": "staging" } }
            }),
        )
        .await;
        assert_eq!(bad_tool["error"]["code"], ERR_INVALID_PARAMS);

        drop(client_tx);
        server.await.unwrap().unwrap();
    }
}
