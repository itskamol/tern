//! End-to-end tests exercising the full local Agent seam (ADR 0003, ADR 0004):
//! MCP stdio (`mcp::serve_stdio`) <-> real `agent.sock` (mode `0o600`) <-> `gpui::TestApp` broker
//! (`broker::BrokerHandle`) <-> real `DecisionQueue` <-> strict headless Login ladder
//! (`headless_login`) <-> in-process loopback `russh` `exec` server + `TempDir`.
//!
//! No real network, no `~/.ssh`, no user config dir, and no real macOS Keychain access.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods
)]

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gpui::{AppContext as _, Context, Entity, TestApp};
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use serde_json::{Value, json};
use tern_ssh::SecretString;
use tern_vault::{Key, Vault};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use super::attention::{self, AgentAttention, AttentionCue};
use super::broker::{
    AgentDecision, AgentHost, AgentOutcome, BrokerHandle, BrokerSnapshot, CHOICE_ALLOW_ONCE,
    CHOICE_ALLOW_UNTIL_LOCK, CHOICE_DENY, DEFAULT_DECISION_TIMEOUT, WAITING_FOR_APPROVAL_MESSAGE,
    format_activity,
};
use super::grants::{GrantFingerprint, GrantStore};
use super::mcp;
use super::protocol::{AgentErrorCode, MAX_COMMAND_BYTES, UserActionReason};
use super::socket_path;
use super::store::AgentConfig;
use crate::connections::Connection;
use crate::decisions::{
    DecisionId, DecisionPrompt, DecisionQueue, DecisionRequest, DecisionTag, PendingDecision,
    QueueFull,
};
use crate::keeper::Keeper;
use crate::lock_on_sleep::Cause;
use crate::password_command::Approved;
use crate::platform::SshRuntime;

const TEST_PASSWORD: &str = "hunter2";

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
    ack: bool,
    delay_after_output: Duration,
}

impl ExecReply {
    fn ok(stdout: &str) -> Self {
        Self {
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            exit_status: 0,
            delay: Duration::ZERO,
            ack: true,
            delay_after_output: Duration::ZERO,
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
        let handler: ExecHandler = Arc::new(on_exec);

        let conns_bg = Arc::clone(&connections);
        let cmds_bg = Arc::clone(&commands);
        let non_exec_bg = Arc::clone(&non_exec_requests);
        rt.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                conns_bg.fetch_add(1, Ordering::SeqCst);
                let cfg = Arc::clone(&config);
                let h = PeerHandler {
                    auth: auth.clone(),
                    on_exec: Arc::clone(&handler),
                    commands: Arc::clone(&cmds_bg),
                    non_exec_requests: Arc::clone(&non_exec_bg),
                };
                tokio::spawn(async move {
                    if let Ok(running) = server::run_stream(cfg, stream, h).await {
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

    fn write_known_hosts(&self, dir: &Path) -> PathBuf {
        let path = dir.join("known_hosts");
        let line = format!(
            "[127.0.0.1]:{} {}\n",
            self.port,
            self.host_key.to_openssh().unwrap()
        );
        std::fs::write(&path, line).unwrap();
        path
    }
}

struct PeerHandler {
    auth: ServerAuth,
    on_exec: ExecHandler,
    commands: Arc<Mutex<Vec<String>>>,
    non_exec_requests: Arc<Mutex<Vec<&'static str>>>,
}

impl server::Handler for PeerHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        match &self.auth {
            ServerAuth::Password(expected) if password == *expected => Ok(Auth::Accept),
            _ => Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }),
        }
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
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
        _user: &str,
        _submethods: &str,
        _response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        match &self.auth {
            ServerAuth::Challenge => Ok(Auth::Partial {
                name: Cow::Borrowed("2FA"),
                instructions: Cow::Borrowed("Enter OTP"),
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
        _channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        _channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.non_exec_requests.lock().unwrap().push("pty");
        Ok(())
    }

    async fn agent_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.non_exec_requests.lock().unwrap().push("agent");
        session.channel_failure(channel)?;
        Ok(false)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).into_owned();
        self.commands.lock().unwrap().push(cmd.clone());
        let reply = (self.on_exec)(&cmd);
        if reply.ack {
            session.channel_success(channel)?;
        }
        let handle = session.handle();
        tokio::spawn(async move {
            if !reply.delay.is_zero() {
                tokio::time::sleep(reply.delay).await;
            }
            if !reply.stdout.is_empty() {
                let _ = handle.data(channel, reply.stdout).await;
            }
            if !reply.stderr.is_empty() {
                let _ = handle.extended_data(channel, 1, reply.stderr).await;
            }
            if !reply.delay_after_output.is_zero() {
                tokio::time::sleep(reply.delay_after_output).await;
            }
            let _ = handle.exit_status_request(channel, reply.exit_status).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });
        Ok(())
    }
}

#[derive(Debug)]
struct SpyAttention {
    active: AtomicBool,
    cues: Mutex<Vec<AttentionCue>>,
}

impl SpyAttention {
    fn new(active: bool) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(active),
            cues: Mutex::new(Vec::new()),
        })
    }
}

impl AgentAttention for SpyAttention {
    fn is_app_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn alert_background(&self, cue: &AttentionCue) {
        self.cues.lock().unwrap().push(cue.clone());
    }
}

struct FakeHost {
    dir: PathBuf,
    known_hosts: PathBuf,
    config: AgentConfig,
    connections: Vec<Connection>,
    grants: GrantStore,
    decisions: DecisionQueue<DecisionTag>,
    decision_timeout: Duration,
    attention: Arc<SpyAttention>,
    outcomes: Vec<AgentOutcome>,
    activities: Vec<String>,
    _broker: Option<BrokerHandle>,
}

impl FakeHost {
    fn new(dir: PathBuf, known_hosts: PathBuf, connections: Vec<Connection>) -> Self {
        Self {
            dir,
            known_hosts,
            config: AgentConfig {
                enabled: true,
                paused: false,
            },
            connections,
            grants: GrantStore::default(),
            decisions: DecisionQueue::new(),
            decision_timeout: DEFAULT_DECISION_TIMEOUT,
            attention: SpyAttention::new(false),
            outcomes: Vec::new(),
            activities: Vec::new(),
            _broker: None,
        }
    }

    fn start_broker(&mut self, cx: &mut Context<Self>) {
        let handle =
            BrokerHandle::start(socket_path(&self.dir), cx.entity().downgrade(), cx).unwrap();
        self._broker = Some(handle);
    }

    fn simulate_lock(&mut self, _cause: Cause) {
        self.grants.revoke_all();
        self.decisions.cancel_where(|k| *k == DecisionTag::Agent);
    }

    fn set_paused(&mut self, paused: bool) {
        self.config.paused = paused;
        if paused {
            self.grants.revoke_all();
            self.decisions.cancel_where(|k| *k == DecisionTag::Agent);
        }
    }
}

impl AgentHost for FakeHost {
    fn agent_snapshot(&self) -> BrokerSnapshot {
        BrokerSnapshot {
            dir: self.dir.clone(),
            known_hosts: Some(self.known_hosts.clone()),
            config: self.config.clone(),
            connections: self.connections.clone(),
        }
    }

    fn verify_agent_grant(
        &mut self,
        host: &str,
        fingerprint: GrantFingerprint,
        _cx: &mut Context<Self>,
    ) -> bool {
        self.grants.verify(host, fingerprint)
    }

    fn record_agent_grant(
        &mut self,
        host: &str,
        fingerprint: GrantFingerprint,
        _cx: &mut Context<Self>,
    ) {
        self.grants.insert(host, fingerprint, SystemTime::now());
    }

    fn agent_decision_timeout(&self) -> Duration {
        self.decision_timeout
    }

    fn ask_agent_decision(
        &mut self,
        prompt: DecisionPrompt,
        deadline: Option<Instant>,
        _cx: &mut Context<Self>,
    ) -> Result<PendingDecision, QueueFull> {
        let mut req = DecisionRequest::new(DecisionTag::Agent, prompt);
        if let Some(d) = deadline {
            req = req.with_deadline(d);
        }
        self.decisions.enqueue(req, Instant::now())
    }

    fn notify_agent_attention(&mut self, cue: &AttentionCue, _cx: &mut Context<Self>) {
        attention::notify_if_inactive(self.attention.as_ref(), cue);
    }

    fn on_agent_outcome(&mut self, outcome: &AgentOutcome, _cx: &mut Context<Self>) {
        self.activities.push(format_activity(outcome));
        self.outcomes.push(outcome.clone());
    }
}

struct McpHarness {
    tx: tokio::sync::mpsc::UnboundedSender<Value>,
    rx: async_channel::Receiver<Value>,
    _server: tokio::task::JoinHandle<()>,
}

impl McpHarness {
    fn start(sock: PathBuf, app: &TestApp) -> Self {
        let rt = app.read(SshRuntime::handle);
        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let (out_tx, out_rx) = async_channel::bounded::<Value>(32);

        let server = rt.spawn(async move {
            let (mut client_tx, server_rx) = tokio::io::duplex(256 * 1024);
            let (server_tx, client_rx) = tokio::io::duplex(256 * 1024);
            let mcp_task = tokio::spawn(async move {
                let _ = mcp::serve_stdio(BufReader::new(server_rx), server_tx, sock).await;
            });
            let writer = tokio::spawn(async move {
                while let Some(msg) = in_rx.recv().await {
                    let mut line = serde_json::to_vec(&msg).unwrap();
                    line.push(b'\n');
                    if client_tx.write_all(&line).await.is_err() {
                        break;
                    }
                    let _ = client_tx.flush().await;
                }
            });
            let reader = tokio::spawn(async move {
                let mut reader = BufReader::new(client_rx);
                let mut line = String::new();
                while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                    if let Ok(val) = serde_json::from_str::<Value>(&line) {
                        let _ = out_tx.send(val).await;
                    }
                    line.clear();
                }
            });
            let _ = tokio::join!(writer, reader, mcp_task);
        });

        let harness = Self {
            tx: in_tx,
            rx: out_rx,
            _server: server,
        };
        harness.send(json!({
            "jsonrpc": "2.0",
            "id": "init",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "clientInfo": { "name": "Claude Code", "version": "1.0" }
            }
        }));
        let init = harness.recv_id("init", app);
        assert_eq!(init["result"]["serverInfo"]["name"], "tern");
        harness
    }

    fn send(&self, msg: Value) {
        self.tx.send(msg).unwrap();
    }

    fn recv_next(&self, app: &TestApp) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            app.run_until_parked();
            if let Ok(val) = self.rx.try_recv() {
                return val;
            }
            assert!(Instant::now() < deadline, "timed out waiting for MCP frame");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn recv_id(&self, id: &str, app: &TestApp) -> Value {
        loop {
            let msg = self.recv_next(app);
            if msg.get("id").and_then(Value::as_str) == Some(id) {
                return msg;
            }
        }
    }
}

fn wait_for_decision(app: &mut TestApp, host: &Entity<FakeHost>) -> (DecisionId, DecisionPrompt) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        app.run_until_parked();
        let found = app.update(|cx| {
            host.update(cx, |h, _| {
                h.decisions
                    .front(Instant::now())
                    .map(|item| (item.id, item.prompt.clone()))
            })
        });
        if let Some(pair) = found {
            return pair;
        }
        assert!(Instant::now() < deadline, "timed out waiting for Decision");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_until(app: &mut TestApp, mut pred: impl FnMut(&mut TestApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        app.run_until_parked();
        if pred(app) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for condition");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn saved_password_conn(name: &str, port: u16) -> Connection {
    Connection {
        name: name.into(),
        host: "127.0.0.1".into(),
        port,
        user: "deploy".into(),
        password_command: Some(format!("printf {TEST_PASSWORD}")),
        ..Connection::default()
    }
}

fn approve_password_cmd(dir: &Path, conn: &Connection) {
    if let Some(cmd) = &conn.password_command {
        Approved::approve(dir, cmd).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn end_to_end_run_command_grant_reuse_invalidation_and_activity() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), |cmd| {
        if cmd.contains("fail.py") {
            ExecReply {
                stdout: b"out\n".to_vec(),
                stderr: b"boom\n".to_vec(),
                exit_status: 2,
                delay: Duration::ZERO,
                ack: true,
                delay_after_output: Duration::ZERO,
            }
        } else {
            ExecReply::ok(&format!("ran: {cmd}\n"))
        }
    });
    let known_hosts = server.write_known_hosts(dir.path());
    let staging = saved_password_conn("staging", server.port);
    let prod = saved_password_conn("prod", server.port);
    approve_password_cmd(dir.path(), &staging);

    let mut app = TestApp::new();
    app.update(|cx| SshRuntime::install(cx).unwrap());
    let host = app.update(|cx| {
        cx.new(|cx| {
            let mut h = FakeHost::new(
                dir.path().to_path_buf(),
                known_hosts,
                vec![staging.clone(), prod.clone()],
            );
            h.start_broker(cx);
            h
        })
    });

    // Verify socket mode 0o600.
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(socket_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    let mcp = McpHarness::start(socket_path(dir.path()), &app);

    // 1. list_hosts exposes all Saved connections with host/user/port metadata.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "list-1",
        "method": "tools/call",
        "params": { "name": "list_hosts", "arguments": {} }
    }));
    let list_resp = mcp.recv_id("list-1", &app);
    assert_eq!(list_resp["result"]["isError"], false);
    let hosts = list_resp["result"]["structuredContent"]["hosts"]
        .as_array()
        .unwrap();
    assert_eq!(hosts.len(), 2);
    assert_eq!(hosts[0]["alias"], "staging");
    assert_eq!(hosts[0]["user"], "deploy");
    assert_eq!(hosts[1]["alias"], "prod");

    // 2. First multi-line run_command raises compact Decision + progress + background cue.
    let script = "set -euo pipefail\nnode ./check.js\necho done";
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "run-1",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "staging", "command": script },
            "_meta": { "progressToken": "tok-1" }
        }
    }));
    let (id, prompt) = wait_for_decision(&mut app, &host);
    assert_eq!(
        prompt.command.as_deref(),
        Some("set -euo pipefail (+2 lines)")
    );
    assert!(prompt.has_choice(CHOICE_ALLOW_ONCE));
    assert!(prompt.has_choice(CHOICE_ALLOW_UNTIL_LOCK));
    assert!(prompt.has_choice(CHOICE_DENY));

    // Background attention cue fired because SpyAttention starts inactive.
    app.update(|cx| {
        let cues = host.read(cx).attention.cues.lock().unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].title, "Approval needed for staging");
    });

    // Progress notification arrives before decision is answered.
    let progress = mcp.recv_next(&app);
    assert_eq!(progress["method"], "notifications/progress");
    assert_eq!(progress["params"]["message"], WAITING_FOR_APPROVAL_MESSAGE);

    // Approve with Allow host until lock -> creates per-Host Grant.
    app.update(|cx| {
        host.update(cx, |h, _| {
            assert!(
                h.decisions
                    .answer(id, CHOICE_ALLOW_UNTIL_LOCK, Instant::now())
                    .is_some()
            );
        });
    });

    let res1 = mcp.recv_id("run-1", &app);
    assert_eq!(res1["result"]["isError"], false);
    assert_eq!(res1["result"]["structuredContent"]["exit_code"], 0);
    assert_eq!(
        res1["result"]["structuredContent"]["stdout"],
        format!("ran: {script}\n")
    );
    assert!(server.non_exec_requests.lock().unwrap().is_empty());

    // 3. Second DIFFERENT command on staging reuses the Host Grant with zero Decision!
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "run-2",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "staging", "command": "python3 ./fail.py" }
        }
    }));
    let res2 = mcp.recv_id("run-2", &app);
    assert_eq!(res2["result"]["isError"], false);
    assert_eq!(res2["result"]["structuredContent"]["exit_code"], 2);
    assert_eq!(res2["result"]["structuredContent"]["stderr"], "boom\n");

    app.update(|cx| {
        let h = host.read(cx);
        assert_eq!(h.outcomes.len(), 2);
        assert_eq!(h.outcomes[0].decision, AgentDecision::AllowUntilLock);
        assert_eq!(h.outcomes[1].decision, AgentDecision::GrantReused);
        assert_eq!(
            h.activities[0],
            "Claude Code ran command on staging (exit 0)"
        );
        assert_eq!(
            h.activities[1],
            "Claude Code ran command on staging (exit 2)"
        );
    });
    assert_eq!(
        *server.commands.lock().unwrap(),
        vec![script.to_owned(), "python3 ./fail.py".to_owned()]
    );

    // 4. Grant for staging does NOT apply to prod.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "run-prod",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "prod", "command": "uptime" }
        }
    }));
    let (prod_id, _) = wait_for_decision(&mut app, &host);
    app.update(|cx| {
        host.update(cx, |h, _| {
            assert!(
                h.decisions
                    .answer(prod_id, CHOICE_DENY, Instant::now())
                    .is_some()
            );
        });
    });
    let prod_res = mcp.recv_id("run-prod", &app);
    assert_eq!(prod_res["result"]["isError"], true);
    assert_eq!(prod_res["result"]["structuredContent"]["error"], "denied");

    // 5. Screen lock clears active Grants so staging requires a Decision again.
    app.update(|cx| {
        host.update(cx, |h, _| h.simulate_lock(Cause::ScreenLocked));
    });
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "run-after-lock",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "staging", "command": "whoami" }
        }
    }));
    let (lock_id, _) = wait_for_decision(&mut app, &host);
    app.update(|cx| {
        host.update(cx, |h, _| {
            assert!(
                h.decisions
                    .answer(lock_id, CHOICE_ALLOW_ONCE, Instant::now())
                    .is_some()
            );
        });
    });
    let after_lock = mcp.recv_id("run-after-lock", &app);
    assert_eq!(after_lock["result"]["isError"], false);

    // 6. Pause agents rejects list_hosts and run_command with agents_paused.
    app.update(|cx| host.update(cx, |h, _| h.set_paused(true)));
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "paused-run",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "staging", "command": "uptime" }
        }
    }));
    let paused_res = mcp.recv_id("paused-run", &app);
    assert_eq!(
        paused_res["result"]["structuredContent"]["error"],
        "agents_paused"
    );

    // 7. Unknown host, empty command, and oversized command are denied without dialing SSH.
    app.update(|cx| host.update(cx, |h, _| h.set_paused(false)));
    let conns_before = server.connections.load(Ordering::SeqCst);
    for (id, h_name, cmd) in [
        ("bad-host", "ssh-config-only", "ls".to_owned()),
        ("bad-empty", "staging", "   \n ".to_owned()),
        ("bad-big", "staging", "x".repeat(MAX_COMMAND_BYTES + 1)),
    ] {
        mcp.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "run_command",
                "arguments": { "host": h_name, "command": cmd }
            }
        }));
        let resp = mcp.recv_id(id, &app);
        assert_eq!(resp["result"]["structuredContent"]["error"], "denied");
    }
    assert_eq!(server.connections.load(Ordering::SeqCst), conns_before);
}

#[cfg(unix)]
#[test]
fn decision_expiry_pre_dispatch_cancel_and_post_dispatch_unknown_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), |_| ExecReply {
        stdout: b"slow\n".to_vec(),
        stderr: Vec::new(),
        exit_status: 0,
        delay: Duration::from_secs(5),
        ack: true,
        delay_after_output: Duration::ZERO,
    });
    let known_hosts = server.write_known_hosts(dir.path());
    let staging = saved_password_conn("staging", server.port);
    approve_password_cmd(dir.path(), &staging);

    let mut app = TestApp::new();
    app.update(|cx| SshRuntime::install(cx).unwrap());
    let host = app.update(|cx| {
        cx.new(|cx| {
            let mut h = FakeHost::new(dir.path().to_path_buf(), known_hosts, vec![staging]);
            h.decision_timeout = Duration::from_secs(60);
            h.start_broker(cx);
            h
        })
    });
    let mcp = McpHarness::start(socket_path(dir.path()), &app);

    // 1. Unanswered Decision expires with timed_out and never dials SSH.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "exp-1",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "staging", "command": "uptime" }
        }
    }));
    let _ = wait_for_decision(&mut app, &host);
    app.update(|cx| {
        let expired = host.update(cx, |h, _| {
            h.decisions.sweep(Instant::now() + Duration::from_secs(61))
        });
        assert_eq!(expired, 1);
    });
    let exp = mcp.recv_id("exp-1", &app);
    assert_eq!(exp["result"]["structuredContent"]["error"], "timed_out");
    assert_eq!(
        exp["result"]["structuredContent"]["phase"],
        "not_dispatched"
    );
    assert_eq!(exp["result"]["structuredContent"]["stdout"], "");
    assert_eq!(exp["result"]["structuredContent"]["stderr"], "");
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);

    // 2. Pre-dispatch notifications/cancelled dismisses pending Decision cleanly.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "can-1",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "staging", "command": "uptime" }
        }
    }));
    let _ = wait_for_decision(&mut app, &host);
    mcp.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": "can-1" }
    }));
    wait_until(&mut app, |app| {
        app.update(|cx| host.update(cx, |h, _| h.decisions.front(Instant::now()).is_none()))
    });
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);

    // 3. Post-dispatch cancel records unknown_outcome.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "can-2",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "staging", "command": "sleep 5" }
        }
    }));
    let (id, _) = wait_for_decision(&mut app, &host);
    app.update(|cx| {
        host.update(cx, |h, _| {
            assert!(
                h.decisions
                    .answer(id, CHOICE_ALLOW_ONCE, Instant::now())
                    .is_some()
            );
        });
    });
    wait_until(&mut app, |_| server.connections.load(Ordering::SeqCst) >= 1);
    mcp.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": "can-2" }
    }));
    wait_until(&mut app, |app| {
        app.update(|cx| {
            host.read(cx).outcomes.iter().any(|o| {
                matches!(
                    &o.result,
                    super::protocol::BrokerResult::Error(e)
                        if e.error == AgentErrorCode::UnknownOutcome
                )
            })
        })
    });
}

#[cfg(unix)]
#[test]
fn headless_login_fails_closed_on_locked_vault_unknown_host_key_and_2fa() {
    let dir = tempfile::tempdir().unwrap();
    let user_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_server =
        TestSshServer::start(ServerAuth::PublicKey(user_key.public_key().clone()), |_| {
            ExecReply::ok("vault-ok\n")
        });
    let challenge_server = TestSshServer::start(ServerAuth::Challenge, |_| ExecReply::ok("no"));

    let known_hosts = key_server.write_known_hosts(dir.path());
    let mut kh_text = std::fs::read_to_string(&known_hosts).unwrap();
    kh_text.push_str(&format!(
        "[127.0.0.1]:{} {}\n",
        challenge_server.port,
        challenge_server.host_key.to_openssh().unwrap()
    ));
    std::fs::write(&known_hosts, kh_text).unwrap();

    let vault_conn = Connection {
        name: "vault-host".into(),
        host: "127.0.0.1".into(),
        port: key_server.port,
        user: "deploy".into(),
        vault_key: Some("deploy-key".into()),
        ..Connection::default()
    };
    let challenge_conn = Connection {
        name: "twofa-host".into(),
        host: "127.0.0.1".into(),
        port: challenge_server.port,
        user: "deploy".into(),
        ..Connection::default()
    };

    let mut app = TestApp::new();
    app.update(|cx| {
        SshRuntime::install(cx).unwrap();
        let mut vault = Vault::new(SecretString::from("master-pass"));
        let pem = user_key
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap();
        vault.set(
            Key::SshKey {
                name: "deploy-key".into(),
            },
            SecretString::from(pem.to_string()),
        );
        Keeper::install(true, cx);
        Keeper::adopt(vault, cx);
        Keeper::lock(cx);
    });

    let host = app.update(|cx| {
        cx.new(|cx| {
            let mut h = FakeHost::new(
                dir.path().to_path_buf(),
                known_hosts,
                vec![vault_conn, challenge_conn],
            );
            h.start_broker(cx);
            h
        })
    });
    let mcp = McpHarness::start(socket_path(dir.path()), &app);

    // 1. Locked vault -> user_action_required (vault_locked).
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "v-1",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "vault-host", "command": "id" }
        }
    }));
    let (id, _) = wait_for_decision(&mut app, &host);
    app.update(|cx| {
        host.update(cx, |h, _| {
            assert!(
                h.decisions
                    .answer(id, CHOICE_ALLOW_ONCE, Instant::now())
                    .is_some()
            );
        });
    });
    let v1 = mcp.recv_id("v-1", &app);
    assert_eq!(
        v1["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(
        v1["result"]["structuredContent"]["reason"],
        serde_json::to_value(UserActionReason::VaultLocked).unwrap()
    );
    app.update(|cx| {
        assert!(
            !Keeper::keychain_attempted(cx),
            "headless Agent login must never attempt macOS Keychain unlock"
        );
    });

    // 2. 2FA challenge -> user_action_required (interactive_auth_required).
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "c-1",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "twofa-host", "command": "id" }
        }
    }));
    let (id, _) = wait_for_decision(&mut app, &host);
    app.update(|cx| {
        host.update(cx, |h, _| {
            assert!(
                h.decisions
                    .answer(id, CHOICE_ALLOW_ONCE, Instant::now())
                    .is_some()
            );
        });
    });
    let c1 = mcp.recv_id("c-1", &app);
    assert_eq!(
        c1["result"]["structuredContent"]["error"],
        "user_action_required"
    );
    assert_eq!(
        c1["result"]["structuredContent"]["reason"],
        serde_json::to_value(UserActionReason::InteractiveAuthRequired).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn list_hosts_query_ranks_exact_match_first_and_reports_grant_active() {
    let dir = tempfile::tempdir().unwrap();
    let known_hosts = dir.path().join("known_hosts");
    let make_conn = |name: &str, host: &str, user: &str| Connection {
        name: name.into(),
        host: host.into(),
        port: 22,
        user: user.into(),
        ..Connection::default()
    };

    let connections = vec![
        make_conn("alpha-public", "10.0.0.1", "ops"),
        make_conn("gamma", "10.0.0.9", "ops"),
        make_conn("prod-alpha", "10.0.0.3", "ops"),
        make_conn("alpha public", "10.0.0.2", "ops"),
        make_conn("beta", "alpha.internal", "ops"),
        make_conn("alpha", "10.0.0.5", "deploy"),
    ];

    let mut app = TestApp::new();
    app.update(|cx| SshRuntime::install(cx).unwrap());
    let host = app.update(|cx| {
        cx.new(|cx| {
            let mut h = FakeHost::new(dir.path().to_path_buf(), known_hosts, connections);
            h.start_broker(cx);
            h
        })
    });
    let mcp = McpHarness::start(socket_path(dir.path()), &app);

    // 1. Query ranks exact alias match first, then prefix, substring, host/user, excluding misses.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "q-1",
        "method": "tools/call",
        "params": { "name": "list_hosts", "arguments": { "query": "alpha" } }
    }));
    let q1 = mcp.recv_id("q-1", &app);
    assert_eq!(q1["result"]["isError"], false);
    let hosts = q1["result"]["structuredContent"]["hosts"]
        .as_array()
        .unwrap();
    let aliases: Vec<&str> = hosts.iter().map(|h| h["alias"].as_str().unwrap()).collect();
    assert_eq!(
        aliases,
        vec![
            "alpha",
            "alpha-public",
            "alpha public",
            "prod-alpha",
            "beta"
        ]
    );
    assert!(hosts.iter().all(|h| h["grant_active"] == json!(false)));

    // 2. Granting "alpha" sets grant_active = true only for "alpha".
    app.update(|cx| {
        host.update(cx, |h, _| {
            let alpha = h
                .connections
                .iter()
                .find(|c| c.name == "alpha")
                .unwrap()
                .clone();
            let fp = GrantFingerprint::for_connection(&alpha, &h.connections);
            h.grants.insert("alpha", fp, SystemTime::now());
        });
    });

    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "q-2",
        "method": "tools/call",
        "params": { "name": "list_hosts", "arguments": { "query": "alpha" } }
    }));
    let q2 = mcp.recv_id("q-2", &app);
    let hosts = q2["result"]["structuredContent"]["hosts"]
        .as_array()
        .unwrap();
    assert_eq!(hosts[0]["alias"], "alpha");
    assert_eq!(hosts[0]["grant_active"], true);
    assert!(hosts[1..].iter().all(|h| h["grant_active"] == json!(false)));

    // 3. Editing "alpha" invalidates its fingerprint so grant_active drops back to false.
    app.update(|cx| {
        host.update(cx, |h, _| {
            let alpha = h
                .connections
                .iter_mut()
                .find(|c| c.name == "alpha")
                .unwrap();
            alpha.host = "10.0.0.99".into();
        });
    });
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "q-3",
        "method": "tools/call",
        "params": { "name": "list_hosts", "arguments": { "query": "alpha" } }
    }));
    let q3 = mcp.recv_id("q-3", &app);
    let hosts = q3["result"]["structuredContent"]["hosts"]
        .as_array()
        .unwrap();
    assert_eq!(hosts[0]["alias"], "alpha");
    assert_eq!(hosts[0]["grant_active"], false);

    // 4. run_command still requires an exact alias (prefix "alph" is rejected).
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "run-prefix",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "alph", "command": "uptime" }
        }
    }));
    let prefix_res = mcp.recv_id("run-prefix", &app);
    assert_eq!(prefix_res["result"]["isError"], true);
    assert_eq!(prefix_res["result"]["structuredContent"]["error"], "denied");
}

#[cfg(unix)]
#[test]
fn run_command_timeout_reports_all_three_phases_and_partial_output() {
    let dir = tempfile::tempdir().unwrap();
    let server = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), |cmd| {
        if cmd == "no-ack" {
            ExecReply {
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_status: 0,
                delay: Duration::from_secs(5),
                ack: false,
                delay_after_output: Duration::ZERO,
            }
        } else {
            ExecReply {
                stdout: b"step 1 done\n".to_vec(),
                stderr: b"still waiting\n".to_vec(),
                exit_status: 0,
                delay: Duration::ZERO,
                ack: true,
                delay_after_output: Duration::from_secs(5),
            }
        }
    });
    let known_hosts = server.write_known_hosts(dir.path());

    // Blackhole TCP listener for not_dispatched timeout during connection setup.
    let blackhole = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let blackhole_port = blackhole.local_addr().unwrap().port();
    let blackhole_thread = std::thread::spawn(move || {
        if let Ok((stream, _)) = blackhole.accept() {
            std::thread::sleep(Duration::from_secs(3));
            drop(stream);
        }
    });

    let live = saved_password_conn("live", server.port);
    let stalled = saved_password_conn("stalled", blackhole_port);
    approve_password_cmd(dir.path(), &live);
    approve_password_cmd(dir.path(), &stalled);

    let mut app = TestApp::new();
    app.update(|cx| SshRuntime::install(cx).unwrap());
    let host = app.update(|cx| {
        cx.new(|cx| {
            let mut h = FakeHost::new(
                dir.path().to_path_buf(),
                known_hosts,
                vec![live.clone(), stalled.clone()],
            );
            for conn in [&live, &stalled] {
                let fp = GrantFingerprint::for_connection(conn, &h.connections);
                h.grants.insert(conn.name.clone(), fp, SystemTime::now());
            }
            h.start_broker(cx);
            h
        })
    });
    let mcp = McpHarness::start(socket_path(dir.path()), &app);

    // 1. Stalled TCP handshake -> timed_out with phase = not_dispatched.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "to-connect",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "stalled", "command": "uptime", "timeout_secs": 1 }
        }
    }));
    let r_connect = mcp.recv_id("to-connect", &app);
    assert_eq!(r_connect["result"]["isError"], true);
    assert_eq!(
        r_connect["result"]["structuredContent"]["error"],
        "timed_out"
    );
    assert_eq!(
        r_connect["result"]["structuredContent"]["phase"],
        "not_dispatched"
    );
    assert_eq!(r_connect["result"]["structuredContent"]["stdout"], "");
    assert_eq!(r_connect["result"]["structuredContent"]["stderr"], "");
    assert!(
        r_connect["result"]["structuredContent"]["duration_ms"]
            .as_u64()
            .unwrap()
            >= 900
    );

    // 2. Exec sent on wire without server channel_success -> phase = dispatch_uncertain.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "to-uncertain",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "live", "command": "no-ack", "timeout_secs": 1 }
        }
    }));
    let r_uncertain = mcp.recv_id("to-uncertain", &app);
    assert_eq!(r_uncertain["result"]["isError"], true);
    assert_eq!(
        r_uncertain["result"]["structuredContent"]["error"],
        "timed_out"
    );
    assert_eq!(
        r_uncertain["result"]["structuredContent"]["phase"],
        "dispatch_uncertain"
    );
    assert_eq!(r_uncertain["result"]["structuredContent"]["stdout"], "");
    assert_eq!(r_uncertain["result"]["structuredContent"]["stderr"], "");

    // 3. Server accepted and streamed partial output before timing out -> phase = running.
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": "to-running",
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": { "host": "live", "command": "slow-partial", "timeout_secs": 1 }
        }
    }));
    let r_running = mcp.recv_id("to-running", &app);
    assert_eq!(r_running["result"]["isError"], true);
    assert_eq!(
        r_running["result"]["structuredContent"]["error"],
        "timed_out"
    );
    assert_eq!(r_running["result"]["structuredContent"]["phase"], "running");
    assert_eq!(
        r_running["result"]["structuredContent"]["stdout"],
        "step 1 done\n"
    );
    assert_eq!(
        r_running["result"]["structuredContent"]["stderr"],
        "still waiting\n"
    );
    assert_eq!(
        r_running["result"]["structuredContent"]["stdout_truncated"],
        false
    );
    assert_eq!(
        r_running["result"]["structuredContent"]["stderr_truncated"],
        false
    );
    assert!(
        r_running["result"]["structuredContent"]["duration_ms"]
            .as_u64()
            .unwrap()
            >= 900
    );

    let _ = host;
    let _ = blackhole_thread.join();
}
