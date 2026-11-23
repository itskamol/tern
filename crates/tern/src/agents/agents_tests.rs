//! End-to-end tests exercising the full local Agent seam (ADR 0003):
//! MCP stdio (`mcp::serve_stdio`) ↔ real `agent.sock` (mode `0o600`) ↔ `gpui::TestApp` broker
//! (`broker::BrokerHandle`) ↔ real `DecisionQueue` ↔ strict headless Login ladder
//! (`headless_login`) ↔ in-process loopback `russh` `exec` server + `TempDir`.
//!
//! No real network, no `~/.ssh`, no user config dir, and no real macOS Keychain access.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{AppContext as _, Context, Entity, TestApp};
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use serde_json::{Value, json};
use tern_ssh::SecretString;
use tern_vault::{Key, Vault};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::broker::{AgentHost, BrokerHandle, BrokerSnapshot, CHOICE_ALLOW_ONCE, CHOICE_DENY};
use super::mcp;
use super::socket_path;
use super::store::AgentConfig;
use crate::connections::Connection;
use crate::decisions::{
    DecisionId, DecisionPrompt, DecisionQueue, DecisionTag, PendingDecision, QueueFull,
};
use crate::keeper::Keeper;
use crate::password_command::Approved;
use crate::platform::SshRuntime;

const TEST_PASSWORD: &str = "hunter2";

// ---- In-process loopback russh exec server --------------------------------------------------

#[derive(Clone)]
enum ServerAuth {
    Password(&'static str),
    PublicKey(PublicKey),
    Challenge,
}

#[derive(Clone)]
struct ExecReply {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_status: u32,
    delay: Duration,
}

impl ExecReply {
    fn ok(stdout: &str) -> Self {
        Self {
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            exit_status: 0,
            delay: Duration::ZERO,
        }
    }
}

type ExecHandler = Arc<dyn Fn(&str) -> ExecReply + Send + Sync>;

struct TestSshServer {
    _rt: tokio::runtime::Runtime,
    port: u16,
    host_key: PublicKey,
    connections: Arc<AtomicUsize>,
    commands: Arc<Mutex<Vec<String>>>,
    non_exec_requests: Arc<Mutex<Vec<&'static str>>>,
}

impl TestSshServer {
    fn start(
        auth: ServerAuth,
        on_exec: impl Fn(&str) -> ExecReply + Send + Sync + 'static,
    ) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let host_priv = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let host_key = host_priv.public_key().clone();
        let methods = match &auth {
            ServerAuth::Password(_) => MethodSet::from(&[MethodKind::Password][..]),
            ServerAuth::PublicKey(_) => MethodSet::from(&[MethodKind::PublicKey][..]),
            ServerAuth::Challenge => MethodSet::from(&[MethodKind::KeyboardInteractive][..]),
        };
        let config = Arc::new(server::Config {
            methods,
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            keys: vec![host_priv],
            ..Default::default()
        });
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let non_exec_requests = Arc::new(Mutex::new(Vec::new()));

        let on_exec: ExecHandler = Arc::new(on_exec);
        let connections_c = Arc::clone(&connections);
        let commands_c = Arc::clone(&commands);
        let non_exec_c = Arc::clone(&non_exec_requests);

        rt.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                connections_c.fetch_add(1, Ordering::SeqCst);
                let config = Arc::clone(&config);
                let handler = LoopbackHandler {
                    auth: auth.clone(),
                    on_exec: Arc::clone(&on_exec),
                    commands: Arc::clone(&commands_c),
                    non_exec_requests: Arc::clone(&non_exec_c),
                };
                tokio::spawn(async move {
                    if let Ok(running) = server::run_stream(config, stream, handler).await {
                        let _ = running.await;
                    }
                });
            }
        });

        Self {
            _rt: rt,
            port,
            host_key,
            connections,
            commands,
            non_exec_requests,
        }
    }

    fn write_known_hosts(&self, path: &Path) {
        let pub_openssh = self.host_key.to_openssh().unwrap();
        let line = format!("[127.0.0.1]:{} {}\n", self.port, pub_openssh);
        std::fs::write(path, line).unwrap();
    }

    fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn recorded_commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn recorded_non_exec(&self) -> Vec<&'static str> {
        self.non_exec_requests.lock().unwrap().clone()
    }
}

struct LoopbackHandler {
    auth: ServerAuth,
    on_exec: ExecHandler,
    commands: Arc<Mutex<Vec<String>>>,
    non_exec_requests: Arc<Mutex<Vec<&'static str>>>,
}

impl server::Handler for LoopbackHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, _: &str, password: &str) -> Result<Auth, Self::Error> {
        match &self.auth {
            ServerAuth::Password(expected) if password == *expected => Ok(Auth::Accept),
            ServerAuth::Password(_) => Ok(Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
                partial_success: false,
            }),
            _ => Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }),
        }
    }

    async fn auth_publickey(
        &mut self,
        _: &str,
        key: server::VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        match &self.auth {
            ServerAuth::PublicKey(expected) if key.public_key == expected => Ok(Auth::Accept),
            _ => Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }),
        }
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _: &str,
        _: &str,
        _: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        match &self.auth {
            ServerAuth::Challenge => Ok(Auth::Partial {
                name: Cow::Borrowed("2FA"),
                instructions: Cow::Borrowed("Enter one-time code"),
                prompts: Cow::Owned(vec![(Cow::Borrowed("Code: "), false)]),
            }),
            _ => Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }),
        }
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        ch: ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        s: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.non_exec_requests.lock().unwrap().push("pty");
        s.channel_success(ch)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        ch: ChannelId,
        s: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.non_exec_requests.lock().unwrap().push("shell");
        s.channel_success(ch)?;
        Ok(())
    }

    async fn agent_request(
        &mut self,
        ch: ChannelId,
        s: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.non_exec_requests.lock().unwrap().push("agent");
        s.channel_success(ch)?;
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        ch: ChannelId,
        data: &[u8],
        s: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).into_owned();
        self.commands.lock().unwrap().push(cmd.clone());
        s.channel_success(ch)?;
        let reply = (self.on_exec)(&cmd);
        let handle = s.handle();
        tokio::spawn(async move {
            if !reply.delay.is_zero() {
                tokio::time::sleep(reply.delay).await;
            }
            if !reply.stdout.is_empty() {
                let _ = handle.data(ch, reply.stdout).await;
            }
            if !reply.stderr.is_empty() {
                let _ = handle.extended_data(ch, 1, reply.stderr).await;
            }
            let _ = handle.exit_status_request(ch, reply.exit_status).await;
            let _ = handle.eof(ch).await;
            let _ = handle.close(ch).await;
        });
        Ok(())
    }
}

// ---- Test broker host inside gpui::TestApp --------------------------------------------------

struct TestHost {
    dir: PathBuf,
    known_hosts: PathBuf,
    config: AgentConfig,
    connections: Vec<Connection>,
    decisions: DecisionQueue<DecisionTag>,
    broker: Option<BrokerHandle>,
}

impl TestHost {
    fn new(
        dir: PathBuf,
        known_hosts: PathBuf,
        config: AgentConfig,
        connections: Vec<Connection>,
    ) -> Self {
        Self {
            dir,
            known_hosts,
            config,
            connections,
            decisions: DecisionQueue::new(),
            broker: None,
        }
    }

    fn start_broker(&mut self, cx: &mut Context<Self>) {
        let sock = socket_path(&self.dir);
        self.broker = Some(BrokerHandle::start(sock, cx.entity().downgrade(), cx).unwrap());
    }

    fn stop_broker(&mut self) {
        self.broker = None;
    }

    fn front_prompt(&mut self) -> Option<(DecisionId, DecisionPrompt)> {
        self.decisions
            .front(Instant::now())
            .map(|f| (f.id, f.prompt.clone()))
    }

    fn answer(&mut self, id: DecisionId, choice: &str) -> bool {
        self.decisions.answer(id, choice, Instant::now()).is_some()
    }
}

impl AgentHost for TestHost {
    fn agent_snapshot(&self) -> BrokerSnapshot {
        BrokerSnapshot {
            dir: self.dir.clone(),
            known_hosts: Some(self.known_hosts.clone()),
            config: self.config.clone(),
            connections: self.connections.clone(),
        }
    }

    fn ask_agent_decision(
        &mut self,
        prompt: DecisionPrompt,
        deadline: Option<Instant>,
        _cx: &mut Context<Self>,
    ) -> Result<PendingDecision, QueueFull> {
        let mut req = crate::decisions::DecisionRequest::new(DecisionTag::Agent, prompt);
        if let Some(d) = deadline {
            req = req.with_deadline(d);
        }
        self.decisions.enqueue(req, Instant::now())
    }
}

// ---- Async-friendly MCP stdio test client ---------------------------------------------------

/// Async-friendly MCP stdio test client that lets the main thread pump `gpui::TestApp` while
/// an MCP call is in flight waiting for a Decision or SSH exec.
struct McpClient {
    _rt: tokio::runtime::Runtime,
    req_tx: async_channel::Sender<Value>,
    resp_rx: async_channel::Receiver<Value>,
    next_id: u64,
}

impl McpClient {
    fn start(sock: PathBuf) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (mut client_tx, server_rx) = tokio::io::duplex(64 * 1024);
        let (server_tx, client_rx) = tokio::io::duplex(64 * 1024);
        rt.spawn(async move {
            let _ = mcp::serve_stdio(BufReader::new(server_rx), server_tx, sock).await;
        });

        let (req_tx, req_rx) = async_channel::bounded::<Value>(16);
        let (resp_tx, resp_rx) = async_channel::bounded::<Value>(16);

        rt.spawn(async move {
            while let Ok(frame) = req_rx.recv().await {
                let mut bytes = serde_json::to_vec(&frame).unwrap();
                bytes.push(b'\n');
                if client_tx.write_all(&bytes).await.is_err() || client_tx.flush().await.is_err() {
                    break;
                }
            }
        });

        rt.spawn(async move {
            let mut reader = BufReader::new(client_rx);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let val: Value = serde_json::from_str(&line).unwrap();
                        if resp_tx.send(val).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Self {
            _rt: rt,
            req_tx,
            resp_rx,
            next_id: 1,
        }
    }

    fn notify(&self, method: &str) {
        self.req_tx
            .send_blocking(json!({ "jsonrpc": "2.0", "method": method }))
            .unwrap();
    }

    fn send(&mut self, method: &str, params: &Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.req_tx
            .send_blocking(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .unwrap();
        id
    }

    fn recv_pumping(&self, app: &mut TestApp) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            app.run_until_parked();
            if let Ok(val) = self.resp_rx.try_recv() {
                return val;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for MCP response");
    }

    fn call_sync(&mut self, method: &str, params: &Value, app: &mut TestApp) -> Value {
        let id = self.send(method, params);
        let resp = self.recv_pumping(app);
        assert_eq!(resp["id"], id);
        resp
    }

    /// Calls `service_status`, waits for the Decision card to appear, runs `inspect_and_choose`
    /// to pick a button, and returns the final MCP JSON-RPC response.
    fn service_status_with_decision(
        &mut self,
        host: &str,
        service: &str,
        timeout_secs: Option<i64>,
        test_host: &Entity<TestHost>,
        app: &mut TestApp,
        inspect_and_choose: impl FnOnce(&DecisionPrompt) -> &'static str,
    ) -> Value {
        let mut args = json!({ "host": host, "service": service });
        if let Some(t) = timeout_secs {
            args["timeout_secs"] = json!(t);
        }
        let id = self.send(
            "tools/call",
            &json!({ "name": "service_status", "arguments": args }),
        );
        let (decision_id, prompt) = wait_for_decision(test_host, app);
        let choice = inspect_and_choose(&prompt);
        let answered = app.update(|cx| test_host.update(cx, |h, _| h.answer(decision_id, choice)));
        assert!(answered, "failed to answer decision with {choice}");
        let resp = self.recv_pumping(app);
        assert_eq!(resp["id"], id);
        resp
    }
}

fn wait_for_decision(host: &Entity<TestHost>, app: &mut TestApp) -> (DecisionId, DecisionPrompt) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        app.run_until_parked();
        if let Some(front) = app.update(|cx| host.update(cx, |h, _| h.front_prompt())) {
            return front;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for Agent Decision to enqueue");
}

fn make_conn(name: &str, port: u16) -> Connection {
    Connection {
        name: name.to_owned(),
        host: "127.0.0.1".to_owned(),
        port,
        user: "deploy".to_owned(),
        ..Connection::default()
    }
}

fn setup_test_app(vault_path: PathBuf, keychain: bool) -> TestApp {
    let mut app = TestApp::new();
    app.update(|cx| {
        SshRuntime::install(cx).unwrap();
        Keeper::install_at_with_keychain(vault_path, keychain, cx);
    });
    app
}

fn write_locked_vault(path: &Path, entries: Vec<(Key, &str)>) {
    let mut vault = Vault::new(SecretString::from("vp1".to_owned()));
    for (k, v) in entries {
        vault.set(k, SecretString::from(v.to_owned()));
    }
    vault.save(path).unwrap();
}

fn unlock_vault(path: &Path, app: &mut TestApp) {
    let vault = Vault::unlock(path, SecretString::from("vp1".to_owned())).unwrap();
    app.update(|cx| Keeper::put(vault, cx));
}

fn generate_unencrypted_key(dir: &Path, name: &str) -> (PathBuf, PublicKey, String) {
    let priv_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let pub_key = priv_key.public_key().clone();
    let pem = priv_key
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .unwrap()
        .to_string();
    let path = dir.join(name);
    std::fs::write(&path, &pem).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    (path, pub_key, pem)
}

// ---- End-to-end tests -----------------------------------------------------------------------

#[test]
fn end_to_end_closed_recovery_list_hosts_allowlist_decision_allow_once_and_deny() {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let vault_path = dir.path().join("vault.age");
    let sock = socket_path(dir.path());

    let ssh = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    ssh.write_known_hosts(&kh);
    write_locked_vault(
        &vault_path,
        vec![(
            Key::Password {
                user: "deploy".into(),
                host: "127.0.0.1".into(),
                port: ssh.port,
            },
            TEST_PASSWORD,
        )],
    );

    let mut app = setup_test_app(vault_path.clone(), false);
    unlock_vault(&vault_path, &mut app);

    // 1. Start MCP stdio while tern broker is closed.
    let mut mcp = McpClient::start(sock.clone());
    let init = mcp.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Claude\x1b[31m Code\x1b[0m" }
        }),
        &mut app,
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "tern");
    mcp.notify("notifications/initialized");

    let tools = mcp.call_sync("tools/list", &json!({}), &mut app);
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 2);

    // tools/call while tern is closed returns tern_not_running without exiting tern mcp.
    let closed = mcp.call_sync(
        "tools/call",
        &json!({ "name": "list_hosts", "arguments": {} }),
        &mut app,
    );
    assert_eq!(closed["result"]["isError"], true);
    assert_eq!(
        closed["result"]["structuredContent"]["error"],
        "tern_not_running"
    );

    // 2. Start broker inside gpui::TestApp with Saved connections + allowlist.
    let mut hosts_map = BTreeMap::new();
    hosts_map.insert(
        "staging".to_owned(),
        vec!["api.service".to_owned(), "worker@1.service".to_owned()],
    );
    hosts_map.insert("empty-host".to_owned(), vec![]);
    // Simulates a ~/.ssh/config-only entry that is NOT a Saved connection.
    hosts_map.insert("ssh-config-only".to_owned(), vec!["api.service".to_owned()]);

    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };
    let connections = vec![
        make_conn("staging", ssh.port),
        make_conn("empty-host", ssh.port),
        make_conn("unlisted", ssh.port),
    ];

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.start_broker(cx);
            h
        })
    });

    // Verify agent.sock exists with 0o600 permissions and is separate from tern.sock.
    assert!(sock.exists());
    assert!(!dir.path().join("tern.sock").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // 3. Without restarting tern mcp, list_hosts now succeeds and filters strictly to Saved
    //    connections with >= 1 allowed service.
    let listed = mcp.call_sync(
        "tools/call",
        &json!({ "name": "list_hosts", "arguments": {} }),
        &mut app,
    );
    assert_eq!(listed["result"]["isError"], false);
    assert_eq!(
        listed["result"]["structuredContent"]["hosts"],
        json!([{
            "alias": "staging",
            "services": ["api.service", "worker@1.service"]
        }])
    );
    assert!(
        listed["result"]["structuredContent"]["usage"]
            .as_str()
            .unwrap()
            .contains("service_status")
    );

    // 4. Unlisted host, unlisted service, or ~/.ssh/config-only host fail immediately with
    //    denied (guiding to list_hosts) without raising a Decision or dialing SSH.
    for (h, s) in [
        ("unlisted", "api.service"),
        ("staging", "db.service"),
        ("ssh-config-only", "api.service"),
        ("staging", "--flag"),
    ] {
        let denied = mcp.call_sync(
            "tools/call",
            &json!({ "name": "service_status", "arguments": { "host": h, "service": s } }),
            &mut app,
        );
        assert_eq!(denied["result"]["isError"], true);
        assert_eq!(denied["result"]["structuredContent"]["error"], "denied");
        assert!(
            denied["result"]["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .contains("list_hosts")
        );
    }
    assert_eq!(ssh.connection_count(), 0);
    assert!(
        app.update(|cx| host_entity.update(cx, |h, _| h.front_prompt()))
            .is_none()
    );

    // 5. Allowlisted service_status raises Decision with sanitized caller, Host alias + user@host,
    //    exact systemctl command, returned fields summary, and Allow once / Deny choices.
    //    Clicking Deny returns denied without touching the Host.
    let denied = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |prompt| {
            assert!(prompt.question.contains("Claude Code"));
            assert!(!prompt.question.contains('\x1b'));
            assert_eq!(
                prompt.command.as_deref(),
                Some("systemctl show -p LoadState,ActiveState,SubState --value -- api.service")
            );
            assert!(
                prompt
                    .details
                    .iter()
                    .any(|d| d.label == "Agent" && d.value == "Claude Code (unverified)")
            );
            assert!(
                prompt
                    .details
                    .iter()
                    .any(|d| d.label == "Host" && d.value == "staging (deploy@127.0.0.1)")
            );
            assert!(
                prompt
                    .details
                    .iter()
                    .any(|d| d.label == "Returns" && d.value.contains("load_state"))
            );
            let choice_ids: Vec<&str> = prompt.choices.iter().map(|c| c.id.as_str()).collect();
            assert_eq!(choice_ids, vec![CHOICE_ALLOW_ONCE, CHOICE_DENY]);
            CHOICE_DENY
        },
    );
    assert_eq!(denied["result"]["isError"], true);
    assert_eq!(denied["result"]["structuredContent"]["error"], "denied");
    assert_eq!(ssh.connection_count(), 0);

    // 6. Clicking Allow once runs isolated non-PTY exec once and returns structured JSON.
    let ok1 = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(ok1["result"]["isError"], false);
    let sc = &ok1["result"]["structuredContent"];
    assert_eq!(sc["host"], "staging");
    assert_eq!(sc["service"], "api.service");
    assert_eq!(sc["load_state"], "loaded");
    assert_eq!(sc["active_state"], "active");
    assert_eq!(sc["sub_state"], "running");
    assert!(sc["checked_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        ssh.recorded_commands(),
        vec!["systemctl show -p LoadState,ActiveState,SubState --value -- api.service"]
    );
    assert!(
        ssh.recorded_non_exec().is_empty(),
        "isolated exec must never request PTY, shell, or agent forwarding"
    );

    // 7. Allow once does NOT persist: next call raises a fresh Decision.
    let ok2 = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(ok2["result"]["isError"], false);
    assert_eq!(ssh.connection_count(), 2);

    // 8. Disabling/stopping broker removes agent.sock cleanly and next call returns tern_not_running.
    app.update(|cx| host_entity.update(cx, |h, _| h.stop_broker()));
    assert!(!sock.exists());
    let after_stop = mcp.call_sync(
        "tools/call",
        &json!({ "name": "list_hosts", "arguments": {} }),
        &mut app,
    );
    assert_eq!(
        after_stop["result"]["structuredContent"]["error"],
        "tern_not_running"
    );
}

#[test]
fn end_to_end_systemctl_states_errors_and_timeout_clamping() {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let (key_path, pub_key, _) = generate_unencrypted_key(dir.path(), "id_ed25519");

    let ssh = TestSshServer::start(ServerAuth::PublicKey(pub_key), |cmd| {
        if cmd.ends_with(" active.service") {
            ExecReply::ok("loaded\nactive\nrunning\n")
        } else if cmd.ends_with(" inactive.service") {
            ExecReply::ok("loaded\ninactive\ndead\n")
        } else if cmd.ends_with(" failed.service") {
            ExecReply::ok("loaded\nfailed\nfailed\n")
        } else if cmd.ends_with(" missing.service") {
            ExecReply::ok("not-found\ninactive\ndead\n")
        } else if cmd.ends_with(" nosystemd.service") {
            ExecReply {
                stdout: Vec::new(),
                stderr: b"sh: 1: systemctl: not found\n".to_vec(),
                exit_status: 127,
                delay: Duration::ZERO,
            }
        } else if cmd.ends_with(" broken.service") {
            ExecReply {
                stdout: Vec::new(),
                stderr: b"Permission denied\n".to_vec(),
                exit_status: 1,
                delay: Duration::ZERO,
            }
        } else {
            ExecReply {
                stdout: b"loaded\nactive\nrunning\n".to_vec(),
                stderr: Vec::new(),
                exit_status: 0,
                delay: Duration::from_secs(3),
            }
        }
    });
    ssh.write_known_hosts(&kh);

    let mut app = setup_test_app(dir.path().join("vault.age"), false);
    let mut conn = make_conn("box", ssh.port);
    conn.identity_file = Some(key_path.display().to_string());

    let mut hosts_map = BTreeMap::new();
    hosts_map.insert(
        "box".to_owned(),
        vec![
            "active.service".into(),
            "inactive.service".into(),
            "failed.service".into(),
            "missing.service".into(),
            "nosystemd.service".into(),
            "broken.service".into(),
            "slow.service".into(),
        ],
    );
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh, config, vec![conn]);
            h.start_broker(cx);
            h
        })
    });

    let mut mcp = McpClient::start(socket_path(dir.path()));
    mcp.call_sync("initialize", &json!({}), &mut app);

    let r_active = mcp.service_status_with_decision(
        "box",
        "active.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r_active["result"]["isError"], false);
    assert_eq!(
        r_active["result"]["structuredContent"]["active_state"],
        "active"
    );

    let r_inactive = mcp.service_status_with_decision(
        "box",
        "inactive.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r_inactive["result"]["isError"], false);
    assert_eq!(
        r_inactive["result"]["structuredContent"]["active_state"],
        "inactive"
    );

    let r_failed = mcp.service_status_with_decision(
        "box",
        "failed.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r_failed["result"]["isError"], false);
    assert_eq!(
        r_failed["result"]["structuredContent"]["active_state"],
        "failed"
    );

    let r_missing = mcp.service_status_with_decision(
        "box",
        "missing.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r_missing["result"]["isError"], false);
    assert_eq!(
        r_missing["result"]["structuredContent"]["load_state"],
        "not_found"
    );

    let r_unsupported = mcp.service_status_with_decision(
        "box",
        "nosystemd.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r_unsupported["result"]["isError"], true);
    assert_eq!(
        r_unsupported["result"]["structuredContent"]["error"],
        "unsupported_host"
    );

    let r_err = mcp.service_status_with_decision(
        "box",
        "broken.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r_err["result"]["isError"], true);
    assert_eq!(r_err["result"]["structuredContent"]["error"], "ssh_error");

    // timeout_secs: 0 clamps to 1 s and times out against the 3 s slow command.
    let started = Instant::now();
    let r_timeout = mcp.service_status_with_decision(
        "box",
        "slow.service",
        Some(0),
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r_timeout["result"]["isError"], true);
    assert_eq!(
        r_timeout["result"]["structuredContent"]["error"],
        "timed_out"
    );
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn end_to_end_headless_login_ladder_vault_keychain_commands_hostkeys_and_2fa() {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let vault_path = dir.path().join("vault.age");
    let (file_key_path, file_pub_key, _) = generate_unencrypted_key(dir.path(), "file_ed25519");
    let (_, vault_pub_key, vault_key_pem) = generate_unencrypted_key(dir.path(), "vault_ed25519");

    let pw_ssh = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    let file_key_ssh = TestSshServer::start(ServerAuth::PublicKey(file_pub_key), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    let vault_key_ssh = TestSshServer::start(ServerAuth::PublicKey(vault_pub_key), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    let unknown_key_ssh = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    let changed_key_ssh = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    let twofa_ssh = TestSshServer::start(ServerAuth::Challenge, |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });

    // Write known_hosts for every server EXCEPT unknown_key_ssh, and write a wrong key for
    // changed_key_ssh.
    let bogus_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .unwrap()
        .public_key()
        .to_openssh()
        .unwrap();
    let known_hosts_text = format!(
        "[127.0.0.1]:{} {}\n[127.0.0.1]:{} {}\n[127.0.0.1]:{} {}\n[127.0.0.1]:{} {}\n[127.0.0.1]:{} {}\n",
        pw_ssh.port,
        pw_ssh.host_key.to_openssh().unwrap(),
        file_key_ssh.port,
        file_key_ssh.host_key.to_openssh().unwrap(),
        vault_key_ssh.port,
        vault_key_ssh.host_key.to_openssh().unwrap(),
        twofa_ssh.port,
        twofa_ssh.host_key.to_openssh().unwrap(),
        changed_key_ssh.port,
        bogus_key,
    );
    std::fs::write(&kh, &known_hosts_text).unwrap();

    // Seed an on-disk vault with both password and SSH key, then keep Keeper LOCKED with
    // vault_keychain = true!
    write_locked_vault(
        &vault_path,
        vec![
            (
                Key::Password {
                    user: "deploy".into(),
                    host: "127.0.0.1".into(),
                    port: pw_ssh.port,
                },
                TEST_PASSWORD,
            ),
            (
                Key::SshKey {
                    name: "deploy-key".into(),
                },
                &vault_key_pem,
            ),
        ],
    );

    let mut app = setup_test_app(vault_path.clone(), true);

    let pw_conn = make_conn("pw-host", pw_ssh.port);
    let mut file_key_conn = make_conn("keyfile-host", file_key_ssh.port);
    file_key_conn.identity_file = Some(file_key_path.display().to_string());

    let mut vault_key_conn = make_conn("vaultkey-host", vault_key_ssh.port);
    vault_key_conn.vault_key = Some("deploy-key".into());

    let mut pw_cmd_conn = make_conn("pwcmd-host", pw_ssh.port);
    pw_cmd_conn.password_command = Some(format!("printf '{TEST_PASSWORD}'"));

    let mut proxy_cmd_conn = make_conn("proxycmd-host", pw_ssh.port);
    proxy_cmd_conn.proxy_command = Some("nc %h %p".into());

    let unknown_conn = make_conn("unknown-host", unknown_key_ssh.port);
    let changed_conn = make_conn("changed-host", changed_key_ssh.port);
    let twofa_conn = make_conn("twofa-host", twofa_ssh.port);
    let no_cred_conn = make_conn("nocred-host", pw_ssh.port);

    let connections = vec![
        pw_conn,
        file_key_conn,
        vault_key_conn,
        pw_cmd_conn,
        proxy_cmd_conn,
        unknown_conn,
        changed_conn,
        twofa_conn,
        no_cred_conn,
    ];
    let mut hosts_map = BTreeMap::new();
    for c in &connections {
        hosts_map.insert(c.name.clone(), vec!["api.service".into()]);
    }
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.start_broker(cx);
            h
        })
    });
    let mut mcp = McpClient::start(socket_path(dir.path()));
    mcp.call_sync("initialize", &json!({}), &mut app);

    // 1. Locked vault WITH vault_keychain=true -> password host fails closed with vault_locked,
    //    vault stays locked, and Keychain is NEVER attempted.
    let r = mcp.service_status_with_decision(
        "pw-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(
        r["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(r["result"]["structuredContent"]["reason"], "vault_locked");
    app.update(|cx| {
        assert!(Keeper::locked(cx));
        assert!(!Keeper::keychain_attempted(cx));
    });

    // 2. Locked vault WITH vault_keychain=true -> vault SSH key host fails closed before dialing.
    let r = mcp.service_status_with_decision(
        "vaultkey-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(
        r["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(r["result"]["structuredContent"]["reason"], "vault_locked");
    assert_eq!(vault_key_ssh.connection_count(), 0);
    app.update(|cx| assert!(!Keeper::keychain_attempted(cx)));

    // 3. Vaultless unencrypted key file host succeeds EVEN WHILE vault is locked!
    let r = mcp.service_status_with_decision(
        "keyfile-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(r["result"]["structuredContent"]["active_state"], "active");
    app.update(|cx| {
        assert!(Keeper::locked(cx));
        assert!(!Keeper::keychain_attempted(cx));
    });

    // 4. Unapproved password_command fails closed with command_approval_required;
    //    once approved, it succeeds EVEN WHILE vault is locked!
    let r = mcp.service_status_with_decision(
        "pwcmd-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(
        r["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(
        r["result"]["structuredContent"]["reason"],
        "command_approval_required"
    );

    Approved::approve(dir.path(), &format!("printf '{TEST_PASSWORD}'")).unwrap();
    let r = mcp.service_status_with_decision(
        "pwcmd-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(r["result"]["structuredContent"]["active_state"], "active");

    // 5. Unapproved ProxyCommand fails closed with command_approval_required before dialing.
    let before = pw_ssh.connection_count();
    let r = mcp.service_status_with_decision(
        "proxycmd-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(
        r["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(
        r["result"]["structuredContent"]["reason"],
        "command_approval_required"
    );
    assert_eq!(pw_ssh.connection_count(), before);

    // 6. Unknown host key fails closed with unknown_host_key and does NOT modify known_hosts.
    let r = mcp.service_status_with_decision(
        "unknown-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(
        r["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(
        r["result"]["structuredContent"]["reason"],
        "unknown_host_key"
    );
    assert_eq!(std::fs::read_to_string(&kh).unwrap(), known_hosts_text);

    // 7. Changed host key is a hard stop (ssh_error, never user_action_required).
    let r = mcp.service_status_with_decision(
        "changed-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["structuredContent"]["error"], "ssh_error");
    assert!(
        r["result"]["structuredContent"]["message"]
            .as_str()
            .unwrap()
            .contains("host key changed")
    );

    // 8. 2FA keyboard-interactive challenge fails closed with interactive_auth_required.
    let r = mcp.service_status_with_decision(
        "twofa-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(
        r["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(
        r["result"]["structuredContent"]["reason"],
        "interactive_auth_required"
    );

    // 9. Open vault -> both password host and vault SSH key host succeed silently!
    unlock_vault(&vault_path, &mut app);

    let r = mcp.service_status_with_decision(
        "pw-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(r["result"]["structuredContent"]["active_state"], "active");

    let r = mcp.service_status_with_decision(
        "vaultkey-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(r["result"]["structuredContent"]["active_state"], "active");

    // 10. Open vault with NO matching secret for a password prompt -> interactive_auth_required.
    //     (Change nocred-host user so the vault password for deploy@127.0.0.1 doesn't match.)
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            if let Some(c) = h.connections.iter_mut().find(|c| c.name == "nocred-host") {
                c.user = "other".into();
            }
        });
    });
    let r = mcp.service_status_with_decision(
        "nocred-host",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(
        r["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(
        r["result"]["structuredContent"]["reason"],
        "interactive_auth_required"
    );
}
