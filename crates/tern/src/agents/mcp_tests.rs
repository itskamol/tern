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
            BrokerRequest::ServiceStatus {
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
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" },
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
