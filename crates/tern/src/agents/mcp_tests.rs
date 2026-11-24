#![allow(clippy::unwrap_used)]

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::*;
use crate::agents::protocol::read_line_bounded;

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
    assert_eq!(list[1]["name"], "run_command");
    assert_eq!(
        list[1]["inputSchema"]["required"],
        json!(["host", "command"])
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
                    caller: Some("Cursor".into()),
                    query: None,
                }
            );
            // Progress frames without a client progressToken are ignored cleanly.
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
                        "hosts": [{ "alias": "staging", "host": "10.0.0.10", "user": "deploy", "port": 22, "grant_active": false }],
                        "usage": "call run_command"
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
            "params": { "name": "run_command", "arguments": { "host": "staging" } }
        }),
    )
    .await;
    assert_eq!(bad_tool["error"]["code"], ERR_INVALID_PARAMS);

    drop(client_tx);
    server.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn notifications_cancelled_with_string_id_sends_broker_cancel_and_suppresses_response() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();

    let (broker_ready_tx, broker_ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (cancel_seen_tx, cancel_seen_rx) = tokio::sync::oneshot::channel::<BrokerRequest>();

    let fake_broker = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);
        let first = read_line_bounded(&mut r, MAX_FRAME_BYTES)
            .await
            .unwrap()
            .unwrap();
        let req: BrokerRequest = serde_json::from_str(&first).unwrap();
        assert!(matches!(
            req,
            BrokerRequest::RunCommand {
                wants_progress: true,
                ..
            }
        ));
        let _ = broker_ready_tx.send(());

        let second = read_line_bounded(&mut r, MAX_FRAME_BYTES)
            .await
            .unwrap()
            .unwrap();
        let cancel_req: BrokerRequest = serde_json::from_str(&second).unwrap();
        let _ = cancel_seen_tx.send(cancel_req);

        // Even if the broker still writes a late result, tern mcp must suppress it.
        let _ = write_json_line(
            &mut w,
            &BrokerMessage::Result(BrokerResult::Error(AgentError::new(
                AgentErrorCode::Denied,
                "cancelled",
            ))),
        )
        .await;
    });

    let (mut client_tx, server_rx) = tokio::io::duplex(64 * 1024);
    let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
    let mut client_rx = BufReader::new(client_rx);
    let server = tokio::spawn(serve_stdio(BufReader::new(server_rx), server_tx, sock));

    let call = json!({
        "jsonrpc": "2.0",
        "id": "req-str-1",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "staging", "command": "echo hi" },
            "_meta": { "progressToken": "p-1" }
        }
    });
    let mut bytes = serde_json::to_vec(&call).unwrap();
    bytes.push(b'\n');
    client_tx.write_all(&bytes).await.unwrap();
    client_tx.flush().await.unwrap();

    broker_ready_rx.await.unwrap();

    let cancel = json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": "req-str-1" }
    });
    let mut cbytes = serde_json::to_vec(&cancel).unwrap();
    cbytes.push(b'\n');
    client_tx.write_all(&cbytes).await.unwrap();
    client_tx.flush().await.unwrap();

    let cancel_frame = cancel_seen_rx.await.unwrap();
    assert_eq!(cancel_frame, BrokerRequest::Cancel);
    fake_broker.await.unwrap();

    // Next ping responds immediately with its own id (no suppressed response for req-str-1).
    let pong = exchange(
        &mut client_tx,
        &mut client_rx,
        json!({ "jsonrpc": "2.0", "id": "req-str-2", "method": "ping" }),
    )
    .await;
    assert_eq!(pong["id"], "req-str-2");
    assert_eq!(pong["result"], json!({}));

    drop(client_tx);
    server.await.unwrap().unwrap();
}

#[test]
fn extract_progress_token_accepts_non_empty_string_and_integers_only() {
    assert_eq!(
        extract_progress_token(&json!({ "_meta": { "progressToken": "tok" } })),
        Some(json!("tok"))
    );
    assert_eq!(
        extract_progress_token(&json!({ "_meta": { "progressToken": 7 } })),
        Some(json!(7))
    );
    assert_eq!(
        extract_progress_token(&json!({ "_meta": { "progressToken": "" } })),
        None
    );
    assert_eq!(
        extract_progress_token(&json!({ "_meta": { "progressToken": null } })),
        None
    );
    assert_eq!(
        extract_progress_token(&json!({ "_meta": { "progressToken": true } })),
        None
    );
    assert_eq!(extract_progress_token(&json!({})), None);
}

/// Validates a JSON object against our emitted MCP `outputSchema` (object/array/primitive types,
/// required keys, and `additionalProperties: false`).
fn assert_matches_object_schema(value: &Value, schema: &Value) {
    assert_eq!(schema["type"], "object");
    let obj = value.as_object().unwrap();
    let props = schema["properties"].as_object().unwrap();

    if let Some(required) = schema["required"].as_array() {
        for req in required {
            let key = req.as_str().unwrap();
            assert!(
                obj.contains_key(key),
                "missing required field '{key}' in {value}"
            );
        }
    }

    if schema["additionalProperties"] == json!(false) {
        for key in obj.keys() {
            assert!(
                props.contains_key(key),
                "unexpected additional field '{key}' in {value}"
            );
        }
    }

    for (key, val) in obj {
        let prop_schema = &props[key];
        match prop_schema["type"].as_str().unwrap() {
            "string" => assert!(val.is_string(), "field '{key}' must be string: {val}"),
            "boolean" => assert!(val.is_boolean(), "field '{key}' must be bool: {val}"),
            "integer" => {
                assert!(
                    val.is_i64() || val.is_u64(),
                    "field '{key}' must be integer: {val}"
                );
                if let Some(min) = prop_schema.get("minimum").and_then(Value::as_i64) {
                    assert!(
                        val.as_i64().unwrap_or(i64::MAX) >= min,
                        "field '{key}' below minimum {min}: {val}"
                    );
                }
            }
            "array" => {
                let items = val.as_array().unwrap();
                let item_schema = &prop_schema["items"];
                for item in items {
                    assert_matches_object_schema(item, item_schema);
                }
            }
            other => assert_eq!(
                other, "string|boolean|integer|array",
                "unsupported schema type"
            ),
        }
    }
}

#[tokio::test]
async fn initialize_negotiates_supported_protocol_version_and_gates_output_schema() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("agent.sock");

    for (requested, expected, expect_output_schema) in [
        (Some("2025-06-18"), "2025-06-18", true),
        (Some("2025-03-26"), "2025-03-26", false),
        (Some("2024-11-05"), "2024-11-05", false),
        (Some("2099-01-01"), "2025-06-18", true),
        (None, "2025-06-18", true),
    ] {
        let (mut client_tx, server_rx) = tokio::io::duplex(64 * 1024);
        let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
        let mut client_rx = BufReader::new(client_rx);
        let server = tokio::spawn(serve_stdio(
            BufReader::new(server_rx),
            server_tx,
            sock.clone(),
        ));

        let params = match requested {
            Some(v) => json!({ "protocolVersion": v }),
            None => json!({}),
        };
        let init = exchange(
            &mut client_tx,
            &mut client_rx,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params }),
        )
        .await;
        assert_eq!(init["result"]["protocolVersion"], expected);

        let tools = exchange(
            &mut client_tx,
            &mut client_rx,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await;
        let list = tools["result"]["tools"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        for tool in list {
            assert_eq!(
                tool.get("outputSchema").is_some(),
                expect_output_schema,
                "outputSchema presence mismatch for version {expected} on tool {}",
                tool["name"]
            );
        }

        drop(client_tx);
        server.await.unwrap().unwrap();
    }
}

#[test]
fn run_command_description_documents_environment_limits_and_is_error_semantics() {
    let payload = tools_list_payload();
    let run_cmd = &payload["tools"][1];
    assert_eq!(run_cmd["name"], "run_command");
    let desc = run_cmd["description"].as_str().unwrap();
    assert!(desc.contains("non-PTY"));
    assert!(desc.contains("non-interactive"));
    assert!(desc.contains("PATH"));
    assert!(desc.contains("64 KiB"));
    assert!(desc.contains("stdout_truncated"));
    assert!(desc.contains("stderr_truncated"));
    assert!(desc.contains("isError"));
    assert!(desc.contains("exit_code"));
}

#[cfg(unix)]
#[tokio::test]
async fn output_schema_matches_ok_nonzero_exit_and_preserves_vault_locked_reason() {
    use crate::agents::protocol::{
        HostSummary, ListHostsResponse, RunCommandResponse, UserActionReason,
    };

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("agent.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();

    let scripted_results = vec![
        BrokerResult::Ok {
            data: serde_json::to_value(ListHostsResponse {
                hosts: vec![HostSummary {
                    alias: "alpha".into(),
                    host: "10.0.0.5".into(),
                    user: "deploy".into(),
                    port: 22,
                    grant_active: true,
                }],
                usage: "call run_command".into(),
            })
            .unwrap(),
        },
        BrokerResult::Ok {
            data: serde_json::to_value(RunCommandResponse {
                host: "alpha".into(),
                exit_code: Some(0),
                signal: None,
                stdout: "ok\n".into(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                duration_ms: 42,
            })
            .unwrap(),
        },
        BrokerResult::Ok {
            data: serde_json::to_value(RunCommandResponse {
                host: "alpha".into(),
                exit_code: Some(7),
                signal: None,
                stdout: "partial\n".into(),
                stderr: "boom\n".into(),
                stdout_truncated: false,
                stderr_truncated: false,
                duration_ms: 19,
            })
            .unwrap(),
        },
        BrokerResult::Error(AgentError::user_action_required(
            UserActionReason::VaultLocked,
            "unlock vault in tern",
        )),
    ];

    let fake_broker = tokio::spawn(async move {
        for result in scripted_results {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut r = BufReader::new(r);
            let _ = read_line_bounded(&mut r, MAX_FRAME_BYTES)
                .await
                .unwrap()
                .unwrap();
            write_json_line(&mut w, &BrokerMessage::Result(result))
                .await
                .unwrap();
        }
    });

    let (mut client_tx, server_rx) = tokio::io::duplex(64 * 1024);
    let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
    let mut client_rx = BufReader::new(client_rx);
    let server = tokio::spawn(serve_stdio(BufReader::new(server_rx), server_tx, sock));

    let init = exchange(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        }),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    let tools = exchange(
        &mut client_tx,
        &mut client_rx,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let list_hosts_schema = &tools["result"]["tools"][0]["outputSchema"];
    let run_cmd_schema = &tools["result"]["tools"][1]["outputSchema"];

    // 1. list_hosts conforms to list_hosts.outputSchema.
    let hosts_res = exchange(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "list_hosts", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(hosts_res["result"]["isError"], false);
    assert_matches_object_schema(&hosts_res["result"]["structuredContent"], list_hosts_schema);

    // 2. Normal exit 0 conforms to run_command.outputSchema.
    let ok_res = exchange(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "run_command", "arguments": { "host": "alpha", "command": "echo ok" } }
        }),
    )
    .await;
    assert_eq!(ok_res["result"]["isError"], false);
    assert_eq!(ok_res["result"]["structuredContent"]["exit_code"], 0);
    assert_matches_object_schema(&ok_res["result"]["structuredContent"], run_cmd_schema);

    // 3. Non-zero exit 7 still returns isError: false and conforms to run_command.outputSchema.
    let exit7_res = exchange(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "run_command", "arguments": { "host": "alpha", "command": "exit 7" } }
        }),
    )
    .await;
    assert_eq!(exit7_res["result"]["isError"], false);
    assert_eq!(exit7_res["result"]["structuredContent"]["exit_code"], 7);
    assert_eq!(
        exit7_res["result"]["structuredContent"]["stdout"],
        "partial\n"
    );
    assert_eq!(exit7_res["result"]["structuredContent"]["stderr"], "boom\n");
    assert_matches_object_schema(&exit7_res["result"]["structuredContent"], run_cmd_schema);

    // 4. Structured broker error (vault_locked) preserves error, reason, and message.
    let locked_res = exchange(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "run_command", "arguments": { "host": "alpha", "command": "whoami" } }
        }),
    )
    .await;
    assert_eq!(locked_res["result"]["isError"], true);
    assert_eq!(
        locked_res["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(
        locked_res["result"]["structuredContent"]["reason"],
        "vault_locked"
    );
    let text_json: Value =
        serde_json::from_str(locked_res["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text_json["reason"], "vault_locked");

    fake_broker.await.unwrap();
    drop(client_tx);
    server.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn list_hosts_forwards_query_and_rejects_non_string_query() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("agent.sock");
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
                caller: None,
                query: Some("alpha".into()),
            }
        );
        write_json_line(
            &mut w,
            &BrokerMessage::Result(BrokerResult::Ok {
                data: json!({
                    "hosts": [{ "alias": "alpha", "host": "10.0.0.5", "user": "deploy", "port": 22, "grant_active": true }],
                    "usage": "call run_command"
                }),
            }),
        )
        .await
        .unwrap();
    });

    let (mut client_tx, server_rx) = tokio::io::duplex(64 * 1024);
    let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
    let mut client_rx = BufReader::new(client_rx);
    let server = tokio::spawn(serve_stdio(BufReader::new(server_rx), server_tx, sock));

    let res = exchange(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "list_hosts", "arguments": { "query": "alpha" } }
        }),
    )
    .await;
    fake_broker.await.unwrap();
    assert_eq!(res["result"]["isError"], false);
    assert_eq!(
        res["result"]["structuredContent"]["hosts"][0]["grant_active"],
        true
    );

    let bad = exchange(
        &mut client_tx,
        &mut client_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "list_hosts", "arguments": { "query": 42 } }
        }),
    )
    .await;
    assert_eq!(bad["error"]["code"], ERR_INVALID_PARAMS);

    drop(client_tx);
    server.await.unwrap().unwrap();
}
