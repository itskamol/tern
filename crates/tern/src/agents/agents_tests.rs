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
use super::audit::{
    self, AuditDecision, AuditOutcome, AuditRecord, AuditRetention, audit_path, rotated_path,
};
use super::broker::{
    AgentDecision, AgentHost, AgentOutcome, BrokerHandle, BrokerSnapshot, CHOICE_ALLOW_ONCE,
    CHOICE_ALLOW_UNTIL_LOCK, CHOICE_DENY, DEFAULT_DECISION_TIMEOUT,
};
use super::grants::{Grant, GrantFingerprint, GrantStore};
use super::mcp;
use super::protocol::{AgentErrorCode, BrokerResult, UserActionReason};
use super::socket_path;
use super::store::{self, AgentConfig};
use crate::connections::Connection;
use crate::decisions::{
    DecisionChoice, DecisionId, DecisionPrompt, DecisionQueue, DecisionRequest, DecisionTag,
    PendingDecision, QueueFull,
};
use crate::keeper::Keeper;
use crate::lock_on_sleep::{self, Cause};
use crate::messages::{Entry as MessageEntry, Kind as MessageKind, Messages};
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum TestDecisionKind {
    Clipboard,
    External(DecisionTag),
}

#[derive(Debug)]
struct SpyAttention {
    active: std::sync::atomic::AtomicBool,
    cues: Mutex<Vec<AttentionCue>>,
}

impl SpyAttention {
    fn new(active: bool) -> Arc<Self> {
        Arc::new(Self {
            active: std::sync::atomic::AtomicBool::new(active),
            cues: Mutex::new(Vec::new()),
        })
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }

    fn take_cues(&self) -> Vec<AttentionCue> {
        std::mem::take(&mut *self.cues.lock().unwrap())
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

struct TestHost {
    dir: PathBuf,
    known_hosts: PathBuf,
    config: AgentConfig,
    connections: Vec<Connection>,
    grants: GrantStore,
    decisions: DecisionQueue<TestDecisionKind>,
    decision_timer: Option<gpui::Task<()>>,
    decision_cancel_watch: Option<gpui::Task<()>>,
    decision_timeout: Duration,
    attention: Arc<SpyAttention>,
    audit_retention: AuditRetention,
    messages: Messages,
    outcomes: Vec<AgentOutcome>,
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
            grants: GrantStore::new(),
            decisions: DecisionQueue::new(),
            decision_timer: None,
            decision_cancel_watch: None,
            decision_timeout: DEFAULT_DECISION_TIMEOUT,
            attention: SpyAttention::new(true),
            audit_retention: AuditRetention::default(),
            messages: Messages::default(),
            outcomes: Vec::new(),
            broker: None,
        }
    }

    fn set_attention(&mut self, attention: Arc<SpyAttention>) {
        self.attention = attention;
    }

    fn set_decision_timeout(&mut self, timeout: Duration) {
        self.decision_timeout = timeout;
    }

    fn set_audit_retention(&mut self, retention: AuditRetention) {
        self.audit_retention = retention;
    }

    fn activity_entries(&self) -> Vec<MessageEntry> {
        self.messages.list().cloned().collect()
    }

    fn sync_decision_tasks(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.decisions.sweep(now);
        if let Some(wake_at) = self.decisions.next_wake(now) {
            let wait = wake_at
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            self.decision_timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |host, cx| {
                    host.decisions.sweep(Instant::now());
                    host.sync_decision_tasks(cx);
                });
            }));
        } else {
            self.decision_timer = None;
        }

        if self.decisions.is_empty() {
            self.decision_cancel_watch = None;
            return;
        }
        if self.decision_cancel_watch.is_none() {
            let wake_rx = self.decisions.cancel_wake();
            self.decision_cancel_watch = Some(cx.spawn(async move |this, cx| {
                while wake_rx.recv().await.is_ok() {
                    let keep = this
                        .update(cx, |host, cx| {
                            host.decisions.sweep(Instant::now());
                            host.sync_decision_tasks(cx);
                            if host.decisions.is_empty() {
                                host.decision_cancel_watch = None;
                                false
                            } else {
                                true
                            }
                        })
                        .unwrap_or(false);
                    if !keep {
                        break;
                    }
                }
            }));
        }
    }

    fn front_remaining_secs(&mut self) -> Option<u64> {
        self.decisions.front(Instant::now())?.remaining_secs
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

    fn enqueue_clipboard_decision(&mut self) -> PendingDecision {
        let prompt = DecisionPrompt::new(
            "staging wants to copy 12 characters",
            vec![
                DecisionChoice::primary("once", "Allow once"),
                DecisionChoice::secondary("deny", "Deny"),
            ],
        );
        self.decisions
            .enqueue(
                DecisionRequest::new(TestDecisionKind::Clipboard, prompt),
                Instant::now(),
            )
            .unwrap()
    }

    fn cancel_agent_decisions(&mut self) -> usize {
        self.decisions
            .cancel_where(|k| matches!(k, TestDecisionKind::External(DecisionTag::Agent)))
    }

    fn active_grants(&self) -> Vec<Grant> {
        self.grants.active()
    }

    fn edit_connections(&mut self, f: impl FnOnce(&mut Vec<Connection>)) {
        f(&mut self.connections);
        self.grants.prune_stale(&self.config, &self.connections);
    }

    fn set_allowed_services(&mut self, host: &str, services: Vec<String>) {
        self.config
            .set_allowed_services(host, services, &self.connections)
            .unwrap();
        self.grants.prune_stale(&self.config, &self.connections);
        store::save(&self.dir, &self.config, &self.connections).unwrap();
    }

    fn revoke_grant(&mut self, host: &str, service: &str) -> bool {
        self.grants.revoke(host, service)
    }

    fn revoke_all_grants(&mut self) -> usize {
        let count = self.grants.revoke_all();
        self.cancel_agent_decisions();
        count
    }

    fn set_agents_paused(&mut self, paused: bool) {
        self.config.paused = paused;
        if paused {
            self.grants.revoke_all();
            self.cancel_agent_decisions();
        }
        store::save(&self.dir, &self.config, &self.connections).unwrap();
    }

    fn on_sleep_or_lock(
        &mut self,
        _cause: Cause,
        vault_lock_on_sleep: bool,
        cx: &mut Context<Self>,
    ) {
        let _ = lock_on_sleep::lock(vault_lock_on_sleep, cx);
        self.grants.revoke_all();
        self.cancel_agent_decisions();
    }

    fn take_outcomes(&mut self) -> Vec<AgentOutcome> {
        std::mem::take(&mut self.outcomes)
    }

    fn has_pending_agent_decision(&self) -> bool {
        self.decisions
            .has_matching(|k| matches!(k, TestDecisionKind::External(DecisionTag::Agent)))
    }

    fn has_pending_clipboard_decision(&self) -> bool {
        self.decisions
            .has_matching(|k| matches!(k, TestDecisionKind::Clipboard))
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

    fn verify_agent_grant(
        &mut self,
        host: &str,
        service: &str,
        fingerprint: GrantFingerprint,
        _cx: &mut Context<Self>,
    ) -> bool {
        self.grants.verify(host, service, fingerprint)
    }

    fn record_agent_grant(
        &mut self,
        host: &str,
        service: &str,
        fingerprint: GrantFingerprint,
        _cx: &mut Context<Self>,
    ) {
        self.grants
            .insert(host, service, fingerprint, SystemTime::now());
    }

    fn agent_decision_timeout(&self) -> Duration {
        self.decision_timeout
    }

    fn ask_agent_decision(
        &mut self,
        prompt: DecisionPrompt,
        deadline: Option<Instant>,
        cx: &mut Context<Self>,
    ) -> Result<PendingDecision, QueueFull> {
        let mut req = DecisionRequest::new(TestDecisionKind::External(DecisionTag::Agent), prompt);
        if let Some(d) = deadline {
            req = req.with_deadline(d);
        }
        let pending = self.decisions.enqueue(req, Instant::now())?;
        self.sync_decision_tasks(cx);
        Ok(pending)
    }

    fn notify_agent_attention(&mut self, cue: &AttentionCue, _cx: &mut Context<Self>) {
        attention::notify_if_inactive(self.attention.as_ref(), cue);
    }

    fn on_agent_outcome(&mut self, outcome: &AgentOutcome, _cx: &mut Context<Self>) {
        let text = audit::record_outcome(
            Some(&self.dir),
            self.audit_retention,
            outcome,
            chrono::Utc::now(),
        );
        self.messages.raise(
            MessageKind::Activity,
            text,
            false,
            false,
            SystemTime::now(),
            Instant::now(),
        );
        self.outcomes.push(outcome.clone());
    }
}

// ---- Async-friendly MCP stdio test client ---------------------------------------------------

/// Async-friendly MCP stdio test client that lets the main thread pump `gpui::TestApp` while
/// an MCP call is in flight waiting for a Decision or SSH exec.
struct McpClient {
    _rt: tokio::runtime::Runtime,
    req_tx: Option<async_channel::Sender<Value>>,
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
            req_tx: Some(req_tx),
            resp_rx,
            next_id: 1,
        }
    }

    fn notify(&self, method: &str) {
        self.req_tx
            .as_ref()
            .unwrap()
            .send_blocking(json!({ "jsonrpc": "2.0", "method": method }))
            .unwrap();
    }

    fn notify_with_params(&self, method: &str, params: &Value) {
        self.req_tx
            .as_ref()
            .unwrap()
            .send_blocking(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .unwrap();
    }

    fn close_stdin(&mut self) {
        self.req_tx = None;
    }

    fn send(&mut self, method: &str, params: &Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.req_tx
            .as_ref()
            .unwrap()
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
            app.advance_clock(Duration::from_millis(10));
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
            assert_eq!(
                choice_ids,
                vec![CHOICE_ALLOW_ONCE, CHOICE_ALLOW_UNTIL_LOCK, CHOICE_DENY]
            );
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

// ---- Allow until lock Grants, fingerprint invalidation, lock_on_sleep, Pause ----

fn wait_for_agent_queued(host: &Entity<TestHost>, app: &mut TestApp) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        app.run_until_parked();
        if app.update(|cx| host.read(cx).has_pending_agent_decision()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for Agent Decision to enter queue");
}

#[test]
fn end_to_end_allow_until_lock_creates_in_memory_grant_reused_without_decision() {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let (key_path, pub_key, _) = generate_unencrypted_key(dir.path(), "id_ed25519");

    let ssh = TestSshServer::start(ServerAuth::PublicKey(pub_key), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    ssh.write_known_hosts(&kh);

    let mut staging = make_conn("staging", ssh.port);
    staging.identity_file = Some(key_path.display().to_string());
    let mut prod = make_conn("prod", ssh.port);
    prod.identity_file = Some(key_path.display().to_string());
    let connections = vec![staging, prod];

    let mut config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: BTreeMap::new(),
    };
    config
        .set_allowed_services(
            "staging",
            vec!["api.service".into(), "worker@1.service".into()],
            &connections,
        )
        .unwrap();
    config
        .set_allowed_services("prod", vec!["api.service".into()], &connections)
        .unwrap();
    store::save(dir.path(), &config, &connections).unwrap();
    let agents_json_before = std::fs::read_to_string(dir.path().join(store::FILE_NAME)).unwrap();

    let mut app = setup_test_app(dir.path().join("vault.age"), false);
    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh, config, connections);
            h.start_broker(cx);
            h
        })
    });
    let mut mcp = McpClient::start(socket_path(dir.path()));
    mcp.call_sync(
        "initialize",
        &json!({ "clientInfo": { "name": "Cursor" } }),
        &mut app,
    );

    // 1. First call raises Decision offering Allow once, Allow until lock, and Deny.
    let first = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |prompt| {
            let ids: Vec<&str> = prompt.choices.iter().map(|c| c.id.as_str()).collect();
            let labels: Vec<&str> = prompt.choices.iter().map(|c| c.label.as_str()).collect();
            assert_eq!(
                ids,
                vec![CHOICE_ALLOW_ONCE, CHOICE_ALLOW_UNTIL_LOCK, CHOICE_DENY]
            );
            assert_eq!(labels, vec!["Allow once", "Allow until lock", "Deny"]);
            assert!(prompt.choices[0].primary);
            assert!(!prompt.choices[1].primary);
            assert!(!prompt.choices[2].primary);
            CHOICE_ALLOW_UNTIL_LOCK
        },
    );
    assert_eq!(first["result"]["isError"], false);
    assert_eq!(
        first["result"]["structuredContent"]["active_state"],
        "active"
    );
    assert_eq!(ssh.connection_count(), 1);

    // Grant lives in memory only; agents.json on disk is completely unchanged.
    let grants = app.update(|cx| host_entity.read(cx).active_grants());
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].host, "staging");
    assert_eq!(grants[0].service, "api.service");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(store::FILE_NAME)).unwrap(),
        agents_json_before
    );
    assert!(!dir.path().join("grants.json").exists());

    // 2. Second call for the same (host, service) succeeds immediately WITHOUT raising a Decision.
    let second = mcp.call_sync(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
        &mut app,
    );
    assert_eq!(second["result"]["isError"], false);
    assert_eq!(
        second["result"]["structuredContent"]["active_state"],
        "active"
    );
    assert_eq!(ssh.connection_count(), 2);
    assert!(
        app.update(|cx| host_entity.update(cx, |h, _| h.front_prompt()))
            .is_none()
    );

    // Audit seam distinguishes allow_until_lock vs grant_reused.
    let outcomes = app.update(|cx| host_entity.update(cx, |h, _| h.take_outcomes()));
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].decision, AgentDecision::AllowUntilLock);
    assert_eq!(outcomes[1].decision, AgentDecision::GrantReused);

    // 3. Different service on same host and same service on different host still require Decisions.
    let other_svc = mcp.service_status_with_decision(
        "staging",
        "worker@1.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(other_svc["result"]["isError"], false);

    let other_host = mcp.service_status_with_decision(
        "prod",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(other_host["result"]["isError"], false);

    let outcomes = app.update(|cx| host_entity.update(cx, |h, _| h.take_outcomes()));
    assert_eq!(outcomes[0].decision, AgentDecision::AllowOnce);
    assert_eq!(outcomes[1].decision, AgentDecision::AllowOnce);
}

#[test]
fn end_to_end_fingerprint_invalidation_on_every_connection_and_service_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let (key_path, pub_key, _) = generate_unencrypted_key(dir.path(), "id_ed25519");

    let ssh1 = TestSshServer::start(ServerAuth::PublicKey(pub_key.clone()), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    let ssh2 = TestSshServer::start(ServerAuth::PublicKey(pub_key), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    let kh_text = format!(
        "[127.0.0.1]:{} {}\n[127.0.0.1]:{} {}\n",
        ssh1.port,
        ssh1.host_key.to_openssh().unwrap(),
        ssh2.port,
        ssh2.host_key.to_openssh().unwrap(),
    );
    std::fs::write(&kh, &kh_text).unwrap();

    let mut staging = make_conn("staging", ssh1.port);
    staging.identity_file = Some(key_path.display().to_string());
    let mut bastion = make_conn("bastion", ssh1.port);
    bastion.identity_file = Some(key_path.display().to_string());
    let connections = vec![staging, bastion];

    let mut config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: BTreeMap::new(),
    };
    config
        .set_allowed_services("staging", vec!["api.service".into()], &connections)
        .unwrap();
    store::save(dir.path(), &config, &connections).unwrap();

    let mut app = setup_test_app(dir.path().join("vault.age"), false);
    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh, config, connections);
            h.start_broker(cx);
            h
        })
    });
    let mut mcp = McpClient::start(socket_path(dir.path()));
    mcp.call_sync("initialize", &json!({}), &mut app);

    let grant_staging = |mcp: &mut McpClient, app: &mut TestApp| {
        let r = mcp.service_status_with_decision(
            "staging",
            "api.service",
            None,
            &host_entity,
            app,
            |_| CHOICE_ALLOW_UNTIL_LOCK,
        );
        assert_eq!(r["result"]["isError"], false);
        // Confirm grant is active and reused immediately.
        let r2 = mcp.call_sync(
            "tools/call",
            &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "api.service" } }),
            app,
        );
        assert_eq!(r2["result"]["isError"], false);
    };

    // 1. Editing `host` — even WITHOUT calling eager prune_stale, broker re-verifies live
    //    fingerprint before execution and forces a fresh Decision!
    grant_staging(&mut mcp, &mut app);
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.connections[0].host = "localhost".into();
        });
    });
    let r = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |prompt| {
            assert!(prompt.details.iter().any(|d| d.value.contains("localhost")));
            CHOICE_DENY
        },
    );
    assert_eq!(r["result"]["structuredContent"]["error"], "denied");
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));

    // Restore host to 127.0.0.1.
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.edit_connections(|c| c[0].host = "127.0.0.1".into())
        });
    });

    // 2. Editing `port` invalidates Grant immediately and forces a fresh Decision.
    grant_staging(&mut mcp, &mut app);
    app.update(|cx| {
        host_entity.update(cx, |h, _| h.edit_connections(|c| c[0].port = ssh2.port));
    });
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));
    let r = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_UNTIL_LOCK,
    );
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(ssh2.connection_count(), 1);

    // 3. Editing `user` invalidates Grant immediately and forces a fresh Decision.
    app.update(|cx| {
        host_entity.update(cx, |h, _| h.edit_connections(|c| c[0].user = "ops".into()));
    });
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));
    let r = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |prompt| {
            assert!(prompt.details.iter().any(|d| d.value.contains("ops@")));
            CHOICE_ALLOW_UNTIL_LOCK
        },
    );
    assert_eq!(r["result"]["isError"], false);

    // 4. Editing `proxy_jump` directly invalidates Grant immediately.
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.edit_connections(|c| c[0].proxy_jump = "bastion".into())
        });
    });
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));
    let r = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_DENY,
    );
    assert_eq!(r["result"]["structuredContent"]["error"], "denied");

    // 4b. Editing a referenced jump Saved connection invalidates the target's Grant too.
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            let fp =
                GrantFingerprint::for_connection(&h.connections[0], &h.connections, "api.service");
            h.grants
                .insert("staging", "api.service", fp, SystemTime::now());
            h.edit_connections(|c| c[1].port = ssh2.port);
        });
    });
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));
    app.update(|cx| {
        host_entity.update(cx, |h, _| h.edit_connections(|c| c[0].proxy_jump.clear()));
    });

    // 5. Editing `proxy_command` invalidates Grant immediately.
    grant_staging(&mut mcp, &mut app);
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.edit_connections(|c| c[0].proxy_command = Some("nc %h %p".into()));
        });
    });
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));
    let r = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_DENY,
    );
    assert_eq!(r["result"]["structuredContent"]["error"], "denied");
    app.update(|cx| {
        host_entity.update(cx, |h, _| h.edit_connections(|c| c[0].proxy_command = None));
    });

    // 6. Removing allowed service immediately invalidates Grant (returns denied); re-adding it
    //    does NOT restore the Grant and forces a fresh Decision.
    grant_staging(&mut mcp, &mut app);
    app.update(|cx| {
        host_entity.update(cx, |h, _| h.set_allowed_services("staging", vec![]));
    });
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));
    let denied = mcp.call_sync(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "api.service" } }),
        &mut app,
    );
    assert_eq!(denied["result"]["structuredContent"]["error"], "denied");

    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.set_allowed_services("staging", vec!["api.service".into()]);
        });
    });
    let r = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_UNTIL_LOCK,
    );
    assert_eq!(r["result"]["isError"], false);

    // 7. Editing a Saved connection while a Decision is open rejects approval (TOCTOU guard) and
    //    records no Grant.
    app.update(|cx| host_entity.update(cx, |h, _| h.revoke_all_grants()));
    let req_id = mcp.send(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "api.service" } }),
    );
    let (dec_id, _) = wait_for_decision(&host_entity, &mut app);
    let ssh2_before = ssh2.connection_count();
    app.update(|cx| {
        host_entity.update(cx, |h, _| h.edit_connections(|c| c[0].port = ssh1.port));
    });
    let answered =
        app.update(|cx| host_entity.update(cx, |h, _| h.answer(dec_id, CHOICE_ALLOW_UNTIL_LOCK)));
    assert!(answered);
    let toctou = mcp.recv_pumping(&mut app);
    assert_eq!(toctou["id"], req_id);
    assert_eq!(toctou["result"]["structuredContent"]["error"], "denied");
    assert_eq!(ssh2.connection_count(), ssh2_before);
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));

    // 8. Renaming Saved connection invalidates Grant; old and unallowlisted new names return denied,
    //    and allowlisting the renamed connection forces a fresh Decision.
    grant_staging(&mut mcp, &mut app);
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.edit_connections(|c| c[0].name = "staging-v2".into());
        });
    });
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));
    let old_name = mcp.call_sync(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "api.service" } }),
        &mut app,
    );
    assert_eq!(old_name["result"]["structuredContent"]["error"], "denied");
    let new_unlisted = mcp.call_sync(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging-v2", "service": "api.service" } }),
        &mut app,
    );
    assert_eq!(
        new_unlisted["result"]["structuredContent"]["error"],
        "denied"
    );
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.config.hosts.remove("staging");
            h.set_allowed_services("staging-v2", vec!["api.service".into()]);
        });
    });
    let renamed_ok = mcp.service_status_with_decision(
        "staging-v2",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_UNTIL_LOCK,
    );
    assert_eq!(renamed_ok["result"]["isError"], false);

    // 9. Deleting Saved connection invalidates Grant immediately and returns denied without SSH.
    let count_before_delete = ssh1.connection_count();
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.edit_connections(|c| c.retain(|conn| conn.name != "staging-v2"));
        });
    });
    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));
    let deleted = mcp.call_sync(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging-v2", "service": "api.service" } }),
        &mut app,
    );
    assert_eq!(deleted["result"]["structuredContent"]["error"], "denied");
    assert_eq!(ssh1.connection_count(), count_before_delete);
}

#[test]
fn end_to_end_lock_on_sleep_clears_grants_and_cancels_agent_decisions_independent_of_vault_setting()
{
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let vault_path = dir.path().join("vault.age");
    let (key_path, pub_key, _) = generate_unencrypted_key(dir.path(), "id_ed25519");

    let ssh = TestSshServer::start(ServerAuth::PublicKey(pub_key), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    ssh.write_known_hosts(&kh);
    write_locked_vault(&vault_path, vec![]);

    let mut conn = make_conn("staging", ssh.port);
    conn.identity_file = Some(key_path.display().to_string());
    let connections = vec![conn];
    let mut config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: BTreeMap::new(),
    };
    config
        .set_allowed_services(
            "staging",
            vec!["api.service".into(), "worker@1.service".into()],
            &connections,
        )
        .unwrap();
    store::save(dir.path(), &config, &connections).unwrap();
    let agents_json_before = std::fs::read_to_string(dir.path().join(store::FILE_NAME)).unwrap();

    let mut app = setup_test_app(vault_path.clone(), false);
    unlock_vault(&vault_path, &mut app);

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh, config, connections);
            h.start_broker(cx);
            h
        })
    });

    // Wire lock_on_sleep::listen with vault_lock_on_sleep = FALSE.
    let (sleep_tx, sleep_rx) = async_channel::unbounded::<Cause>();
    let host_weak = host_entity.downgrade();
    app.update(|cx| {
        lock_on_sleep::listen(sleep_rx, cx, move |cause, cx| {
            let _ = host_weak.update(cx, |h, cx| h.on_sleep_or_lock(cause, false, cx));
        });
    });

    let mut mcp = McpClient::start(socket_path(dir.path()));
    mcp.call_sync("initialize", &json!({}), &mut app);

    for cause in [Cause::Sleep, Cause::ScreenLocked, Cause::SessionSwitched] {
        // Grant api.service via Allow until lock and prove Grant reuse works.
        let ok = mcp.service_status_with_decision(
            "staging",
            "api.service",
            None,
            &host_entity,
            &mut app,
            |_| CHOICE_ALLOW_UNTIL_LOCK,
        );
        assert_eq!(ok["result"]["isError"], false);
        let reused = mcp.call_sync(
            "tools/call",
            &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "api.service" } }),
            &mut app,
        );
        assert_eq!(reused["result"]["isError"], false);

        // Enqueue a Clipboard Decision AND an in-flight Agent Decision (worker@1.service).
        let clipboard_pending =
            app.update(|cx| host_entity.update(cx, |h, _| h.enqueue_clipboard_decision()));
        let pending_id = mcp.send(
            "tools/call",
            &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "worker@1.service" } }),
        );
        wait_for_agent_queued(&host_entity, &mut app);

        // Inject lock_on_sleep event.
        sleep_tx.send_blocking(cause).unwrap();
        let cancelled = mcp.recv_pumping(&mut app);
        assert_eq!(cancelled["id"], pending_id);
        assert_eq!(cancelled["result"]["isError"], true);
        assert_eq!(cancelled["result"]["structuredContent"]["error"], "denied");

        // Vault stayed unlocked (vault_lock_on_sleep = false), active Grants cleared,
        // Agent Decisions cancelled, Clipboard Decision untouched, agents.json intact.
        app.update(|cx| {
            assert!(
                !Keeper::locked(cx),
                "vault must stay open when vault_lock_on_sleep is false"
            );
            let h = host_entity.read(cx);
            assert!(
                h.active_grants().is_empty(),
                "{cause:?} must clear all Grants"
            );
            assert!(
                !h.has_pending_agent_decision(),
                "{cause:?} must cancel Agent Decisions"
            );
            assert!(
                h.has_pending_clipboard_decision(),
                "{cause:?} must leave clipboard Decisions untouched"
            );
        });
        assert_eq!(clipboard_pending.try_outcome(), None);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(store::FILE_NAME)).unwrap(),
            agents_json_before
        );

        let outcomes = app.update(|cx| host_entity.update(cx, |h, _| h.take_outcomes()));
        assert_eq!(outcomes.last().unwrap().decision, AgentDecision::Cancelled);

        // Clear the clipboard Decision before next iteration.
        assert!(clipboard_pending.cancel());
        app.update(|cx| host_entity.update(cx, |h, _| h.decisions.sweep(Instant::now())));
    }
}

#[test]
fn end_to_end_manual_revoke_and_app_quit_clear_grants_preserving_agents_json() {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let (key_path, pub_key, _) = generate_unencrypted_key(dir.path(), "id_ed25519");

    let ssh = TestSshServer::start(ServerAuth::PublicKey(pub_key), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    ssh.write_known_hosts(&kh);

    let mut conn = make_conn("staging", ssh.port);
    conn.identity_file = Some(key_path.display().to_string());
    let connections = vec![conn];
    let mut config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: BTreeMap::new(),
    };
    config
        .set_allowed_services(
            "staging",
            vec![
                "api.service".into(),
                "worker@1.service".into(),
                "db.service".into(),
            ],
            &connections,
        )
        .unwrap();
    store::save(dir.path(), &config, &connections).unwrap();
    let agents_json_before = std::fs::read_to_string(dir.path().join(store::FILE_NAME)).unwrap();

    let mut app = setup_test_app(dir.path().join("vault.age"), false);
    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(
                dir.path().to_path_buf(),
                kh.clone(),
                config.clone(),
                connections.clone(),
            );
            h.start_broker(cx);
            h
        })
    });
    let mut mcp = McpClient::start(socket_path(dir.path()));
    mcp.call_sync("initialize", &json!({}), &mut app);

    // Grant api.service and worker@1.service.
    for svc in ["api.service", "worker@1.service"] {
        let r =
            mcp.service_status_with_decision("staging", svc, None, &host_entity, &mut app, |_| {
                CHOICE_ALLOW_UNTIL_LOCK
            });
        assert_eq!(r["result"]["isError"], false);
    }

    // Single-grant revoke clears only api.service; worker@1.service still reuses its Grant.
    let revoked_one =
        app.update(|cx| host_entity.update(cx, |h, _| h.revoke_grant("staging", "api.service")));
    assert!(revoked_one);
    let worker_reused = mcp.call_sync(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "worker@1.service" } }),
        &mut app,
    );
    assert_eq!(worker_reused["result"]["isError"], false);

    // Manual Revoke active Grants clears all Grants, cancels pending Agent Decisions, preserves
    // clipboard Decisions and agents.json.
    let clipboard_pending =
        app.update(|cx| host_entity.update(cx, |h, _| h.enqueue_clipboard_decision()));
    let pending_id = mcp.send(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "db.service" } }),
    );
    wait_for_agent_queued(&host_entity, &mut app);

    let revoked_all = app.update(|cx| host_entity.update(cx, |h, _| h.revoke_all_grants()));
    assert_eq!(revoked_all, 1);

    let cancelled = mcp.recv_pumping(&mut app);
    assert_eq!(cancelled["id"], pending_id);
    assert_eq!(cancelled["result"]["structuredContent"]["error"], "denied");
    assert_eq!(clipboard_pending.try_outcome(), None);
    assert_eq!(
        std::fs::read_to_string(dir.path().join(store::FILE_NAME)).unwrap(),
        agents_json_before
    );
    assert!(clipboard_pending.cancel());
    app.update(|cx| host_entity.update(cx, |h, _| h.decisions.sweep(Instant::now())));

    // App quit simulation: active Grant disappears and pending Agent Decision cancels cleanly.
    let ok = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_UNTIL_LOCK,
    );
    assert_eq!(ok["result"]["isError"], false);

    let pending_quit_id = mcp.send(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "db.service" } }),
    );
    wait_for_agent_queued(&host_entity, &mut app);

    // Dropping the broker + host simulates app quit (DecisionQueue::drop cancels pending items).
    drop(host_entity);
    app.update(|_| {});
    app.run_until_parked();
    let quit_resp = mcp.recv_pumping(&mut app);
    assert_eq!(quit_resp["id"], pending_quit_id);
    assert_eq!(quit_resp["result"]["isError"], true);
    assert_eq!(
        quit_resp["result"]["structuredContent"]["error"],
        "tern_not_running"
    );
    assert!(!socket_path(dir.path()).exists());

    // Relaunching a fresh host reading the same agents.json has zero Grants and raises a fresh
    // Decision for api.service.
    let reloaded_cfg = store::load(dir.path()).unwrap();
    let restarted = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh, reloaded_cfg, connections);
            h.start_broker(cx);
            h
        })
    });
    assert!(app.update(|cx| restarted.read(cx).active_grants().is_empty()));
    let after_restart = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &restarted,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(after_restart["result"]["isError"], false);
}

#[test]
fn end_to_end_pause_agents_rejects_cancels_clears_grants_and_does_not_restore_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let (key_path, pub_key, _) = generate_unencrypted_key(dir.path(), "id_ed25519");

    let ssh = TestSshServer::start(ServerAuth::PublicKey(pub_key), |_| {
        ExecReply::ok("loaded\nactive\nrunning\n")
    });
    ssh.write_known_hosts(&kh);

    let mut conn = make_conn("staging", ssh.port);
    conn.identity_file = Some(key_path.display().to_string());
    let connections = vec![conn];
    let mut config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: BTreeMap::new(),
    };
    config
        .set_allowed_services(
            "staging",
            vec!["api.service".into(), "worker@1.service".into()],
            &connections,
        )
        .unwrap();
    store::save(dir.path(), &config, &connections).unwrap();

    let mut app = setup_test_app(dir.path().join("vault.age"), false);
    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh, config, connections);
            h.start_broker(cx);
            h
        })
    });
    let mut mcp = McpClient::start(socket_path(dir.path()));
    mcp.call_sync("initialize", &json!({}), &mut app);

    // 1. Grant api.service via Allow until lock and confirm reuse.
    let ok = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_UNTIL_LOCK,
    );
    assert_eq!(ok["result"]["isError"], false);
    let reused = mcp.call_sync(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "api.service" } }),
        &mut app,
    );
    assert_eq!(reused["result"]["isError"], false);
    app.update(|cx| host_entity.update(cx, |h, _| h.take_outcomes()));

    // 2. Enqueue a Clipboard Decision AND an in-flight Agent Decision (worker@1.service).
    let clipboard_pending =
        app.update(|cx| host_entity.update(cx, |h, _| h.enqueue_clipboard_decision()));
    let pending_id = mcp.send(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "worker@1.service" } }),
    );
    wait_for_agent_queued(&host_entity, &mut app);

    // 3. Enable Pause agents: cancels pending Agent Decision with agents_paused, clears Grants,
    //    persists paused=true while keeping allowlist intact, leaves clipboard Decision alone.
    let count_before_pause = ssh.connection_count();
    app.update(|cx| host_entity.update(cx, |h, _| h.set_agents_paused(true)));

    let paused_pending = mcp.recv_pumping(&mut app);
    assert_eq!(paused_pending["id"], pending_id);
    assert_eq!(paused_pending["result"]["isError"], true);
    assert_eq!(
        paused_pending["result"]["structuredContent"]["error"],
        "agents_paused"
    );

    assert!(app.update(|cx| host_entity.read(cx).active_grants().is_empty()));
    assert_eq!(clipboard_pending.try_outcome(), None);

    let on_disk = store::load(dir.path()).unwrap();
    assert!(on_disk.paused);
    assert_eq!(
        on_disk.hosts.get("staging").map(Vec::as_slice),
        Some(&["api.service".to_owned(), "worker@1.service".to_owned()][..])
    );

    // 4. New list_hosts and service_status calls reject immediately with agents_paused without
    //    raising a Decision or dialing SSH.
    let list_paused = mcp.call_sync(
        "tools/call",
        &json!({ "name": "list_hosts", "arguments": {} }),
        &mut app,
    );
    assert_eq!(list_paused["result"]["isError"], true);
    assert_eq!(
        list_paused["result"]["structuredContent"]["error"],
        "agents_paused"
    );

    let status_paused = mcp.call_sync(
        "tools/call",
        &json!({ "name": "service_status", "arguments": { "host": "staging", "service": "api.service" } }),
        &mut app,
    );
    assert_eq!(status_paused["result"]["isError"], true);
    assert_eq!(
        status_paused["result"]["structuredContent"]["error"],
        "agents_paused"
    );
    assert_eq!(ssh.connection_count(), count_before_pause);

    let outcomes = app.update(|cx| host_entity.update(cx, |h, _| h.take_outcomes()));
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].decision, AgentDecision::Paused);
    assert_eq!(outcomes[1].decision, AgentDecision::Paused);

    // Clean up clipboard Decision.
    assert!(clipboard_pending.cancel());
    app.update(|cx| host_entity.update(cx, |h, _| h.decisions.sweep(Instant::now())));

    // 5. Disable Pause agents: resumes normal handling, persists paused=false, does NOT restore
    //    cleared Grants (api.service requires a fresh Decision).
    app.update(|cx| host_entity.update(cx, |h, _| h.set_agents_paused(false)));
    let on_disk_resumed = store::load(dir.path()).unwrap();
    assert!(!on_disk_resumed.paused);
    assert_eq!(on_disk_resumed.hosts, on_disk.hosts);

    let list_ok = mcp.call_sync(
        "tools/call",
        &json!({ "name": "list_hosts", "arguments": {} }),
        &mut app,
    );
    assert_eq!(list_ok["result"]["isError"], false);

    let fresh_decision = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(fresh_decision["result"]["isError"], false);
    assert_eq!(ssh.connection_count(), count_before_pause + 1);
}

#[test]
fn end_to_end_decision_expires_after_deadline_with_timed_out_and_default_60s() {
    assert_eq!(DEFAULT_DECISION_TIMEOUT, Duration::from_secs(60));

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

    let mut hosts_map = BTreeMap::new();
    hosts_map.insert("staging".to_owned(), vec!["api.service".to_owned()]);
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };
    let connections = vec![make_conn("staging", ssh.port)];

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.start_broker(cx);
            h
        })
    });

    let mut mcp = McpClient::start(sock);
    mcp.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Claude Code" }
        }),
        &mut app,
    );

    // 1. Default Agent Decision carries a 60-second countdown deadline.
    let id_default = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let (decision_id, _) = wait_for_decision(&host_entity, &mut app);
    let remaining = app.update(|cx| host_entity.update(cx, |h, _| h.front_remaining_secs()));
    assert!(
        matches!(remaining, Some(58..=60)),
        "expected default 60s countdown, got {remaining:?}"
    );
    assert!(app.update(|cx| host_entity.update(cx, |h, _| h.answer(decision_id, CHOICE_DENY))));
    let denied = mcp.recv_pumping(&mut app);
    assert_eq!(denied["id"], id_default);
    app.update(|cx| host_entity.update(cx, |h, _| h.take_outcomes()));

    // 2. With a short test decision timeout, an unanswered Decision dismisses automatically,
    //    returns timed_out to the Agent, records AgentDecision::Expired + TimedOut, and never
    //    dials SSH.
    app.update(|cx| {
        host_entity.update(cx, |h, _| h.set_decision_timeout(Duration::from_millis(80)));
    });
    let id_expire = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let _ = wait_for_decision(&host_entity, &mut app);
    let expired = mcp.recv_pumping(&mut app);
    assert_eq!(expired["id"], id_expire);
    assert_eq!(expired["result"]["isError"], true);
    assert_eq!(expired["result"]["structuredContent"]["error"], "timed_out");
    assert!(!app.update(|cx| host_entity.read(cx).has_pending_agent_decision()));
    assert_eq!(ssh.connection_count(), 0);

    let outcomes = app.update(|cx| host_entity.update(cx, |h, _| h.take_outcomes()));
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].decision, AgentDecision::Expired);
    assert!(matches!(
        &outcomes[0].result,
        BrokerResult::Error(err) if err.error == AgentErrorCode::TimedOut
    ));
}

#[test]
fn end_to_end_progress_notification_emitted_only_with_progress_token_while_decision_pending() {
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

    let mut hosts_map = BTreeMap::new();
    hosts_map.insert("staging".to_owned(), vec!["api.service".to_owned()]);
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };
    let connections = vec![make_conn("staging", ssh.port)];

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.start_broker(cx);
            h
        })
    });

    let mut mcp = McpClient::start(sock);
    mcp.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Claude Code" }
        }),
        &mut app,
    );

    // 1. String progressToken emits notifications/progress while waiting on the Decision card.
    let id_str = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" },
            "_meta": { "progressToken": "tok-alpha" }
        }),
    );
    let (dec_str, _) = wait_for_decision(&host_entity, &mut app);
    let progress = mcp.recv_pumping(&mut app);
    assert_eq!(progress["jsonrpc"], "2.0");
    assert!(progress.get("id").is_none());
    assert_eq!(progress["method"], "notifications/progress");
    assert_eq!(progress["params"]["progressToken"], "tok-alpha");
    assert_eq!(progress["params"]["progress"], 1);
    assert_eq!(
        progress["params"]["message"],
        "Waiting for approval in tern..."
    );

    assert!(app.update(|cx| host_entity.update(cx, |h, _| h.answer(dec_str, CHOICE_ALLOW_ONCE))));
    let resp_str = mcp.recv_pumping(&mut app);
    assert_eq!(resp_str["id"], id_str);
    assert_eq!(resp_str["result"]["isError"], false);

    // 2. Integer progressToken also emits notifications/progress, preserving the numeric token.
    let id_int = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" },
            "_meta": { "progressToken": 42 }
        }),
    );
    let (dec_int, _) = wait_for_decision(&host_entity, &mut app);
    let progress_int = mcp.recv_pumping(&mut app);
    assert_eq!(progress_int["method"], "notifications/progress");
    assert_eq!(progress_int["params"]["progressToken"], 42);
    assert_eq!(
        progress_int["params"]["message"],
        "Waiting for approval in tern..."
    );
    assert!(app.update(|cx| {
        host_entity.update(cx, |h, _| h.answer(dec_int, CHOICE_ALLOW_UNTIL_LOCK))
    }));
    let resp_int = mcp.recv_pumping(&mut app);
    assert_eq!(resp_int["id"], id_int);
    assert_eq!(resp_int["result"]["isError"], false);

    // 3. Reusing an active Grant skips the Decision queue so no approval progress frame is sent.
    let id_grant = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" },
            "_meta": { "progressToken": "tok-grant" }
        }),
    );
    let resp_grant = mcp.recv_pumping(&mut app);
    assert_eq!(resp_grant["id"], id_grant);
    assert_eq!(resp_grant["result"]["isError"], false);

    // 4. Without _meta.progressToken, no notifications/progress frame is emitted while waiting.
    app.update(|cx| host_entity.update(cx, |h, _| h.revoke_all_grants()));
    let id_none = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let (dec_none, _) = wait_for_decision(&host_entity, &mut app);
    for _ in 0..5 {
        app.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        mcp.resp_rx.try_recv().is_err(),
        "no progress notification should be sent without progressToken"
    );
    assert!(app.update(|cx| host_entity.update(cx, |h, _| h.answer(dec_none, CHOICE_ALLOW_ONCE))));
    let resp_none = mcp.recv_pumping(&mut app);
    assert_eq!(resp_none["id"], id_none);
    assert_eq!(resp_none["result"]["isError"], false);
}

fn wait_for_no_pending_agent_decision(host: &Entity<TestHost>, app: &mut TestApp) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.run_until_parked();
        if !app.update(|cx| host.read(cx).has_pending_agent_decision()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for Agent Decision to dismiss");
}

fn wait_for_outcomes(
    host: &Entity<TestHost>,
    expected: usize,
    app: &mut TestApp,
) -> Vec<AgentOutcome> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.run_until_parked();
        let len = app.update(|cx| host.read(cx).outcomes.len());
        if len >= expected {
            return app.update(|cx| host.update(cx, |h, _| h.take_outcomes()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {expected} AgentOutcome(s)");
}

#[test]
fn end_to_end_pre_dispatch_mcp_cancel_and_disconnect_dismiss_decision_and_prevent_exec() {
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

    let mut hosts_map = BTreeMap::new();
    hosts_map.insert("staging".to_owned(), vec!["api.service".to_owned()]);
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };
    let connections = vec![make_conn("staging", ssh.port)];

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.start_broker(cx);
            h
        })
    });

    let mut mcp = McpClient::start(sock.clone());
    mcp.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Claude Code" }
        }),
        &mut app,
    );

    // 1. MCP notifications/cancelled before dispatch dismisses the Decision immediately,
    //    blocks late/racing clicks, suppresses the cancelled response, and keeps tern mcp alive.
    let id_cancel = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let (dec_cancel, _) = wait_for_decision(&host_entity, &mut app);
    mcp.notify_with_params(
        "notifications/cancelled",
        &json!({ "requestId": id_cancel, "reason": "abandoned" }),
    );
    wait_for_no_pending_agent_decision(&host_entity, &mut app);
    assert!(
        !app.update(|cx| {
            host_entity.update(cx, |h, _| h.answer(dec_cancel, CHOICE_ALLOW_ONCE))
        })
    );
    let outcomes = wait_for_outcomes(&host_entity, 1, &mut app);
    assert_eq!(outcomes[0].decision, AgentDecision::Cancelled);
    assert_eq!(ssh.connection_count(), 0);

    // Next MCP request succeeds and no stray response arrived for id_cancel.
    let pong = mcp.call_sync("ping", &json!({}), &mut app);
    assert_eq!(pong["result"], json!({}));

    // 2. Stdio EOF before dispatch immediately cancels the in-flight agent.sock call and
    //    dismisses the Decision card without dialing SSH.
    let _id_eof = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let (dec_eof, _) = wait_for_decision(&host_entity, &mut app);
    mcp.close_stdin();
    wait_for_no_pending_agent_decision(&host_entity, &mut app);
    assert!(
        !app.update(|cx| { host_entity.update(cx, |h, _| h.answer(dec_eof, CHOICE_ALLOW_ONCE)) })
    );
    let outcomes = wait_for_outcomes(&host_entity, 1, &mut app);
    assert_eq!(outcomes[0].decision, AgentDecision::Cancelled);
    assert_eq!(ssh.connection_count(), 0);

    // 3. Raw IPC disconnect on agent.sock before dispatch also dismisses immediately and prevents
    //    execution even if a click races.
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(&sock).unwrap();
        let line = serde_json::to_string(&super::protocol::BrokerRequest::ServiceStatus {
            caller: Some("RawClient".into()),
            host: "staging".into(),
            service: "api.service".into(),
            timeout_secs: None,
            wants_progress: false,
        })
        .unwrap();
        stream.write_all(format!("{line}\n").as_bytes()).unwrap();
        stream.flush().unwrap();

        let (dec_ipc, _) = wait_for_decision(&host_entity, &mut app);
        drop(stream);
        wait_for_no_pending_agent_decision(&host_entity, &mut app);
        assert!(
            !app.update(|cx| {
                host_entity.update(cx, |h, _| h.answer(dec_ipc, CHOICE_ALLOW_ONCE))
            })
        );
        let outcomes = wait_for_outcomes(&host_entity, 1, &mut app);
        assert_eq!(outcomes[0].decision, AgentDecision::Cancelled);
        assert_eq!(ssh.connection_count(), 0);
    }
}

fn wait_for_signal(rx: &async_channel::Receiver<()>, app: &mut TestApp) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        app.run_until_parked();
        if rx.try_recv().is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for SSH exec_request to arrive on loopback server");
}

#[test]
fn end_to_end_post_dispatch_disconnect_and_cancel_record_unknown_outcome() {
    use std::sync::atomic::AtomicBool;

    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let vault_path = dir.path().join("vault.age");
    let sock = socket_path(dir.path());

    let slow_mode = Arc::new(AtomicBool::new(true));
    let slow_c = Arc::clone(&slow_mode);
    let (started_tx, started_rx) = async_channel::bounded::<()>(4);
    let ssh = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), move |_| {
        let _ = started_tx.try_send(());
        let mut reply = ExecReply::ok("loaded\nactive\nrunning\n");
        if slow_c.load(Ordering::SeqCst) {
            reply.delay = Duration::from_secs(10);
        }
        reply
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

    let mut hosts_map = BTreeMap::new();
    hosts_map.insert("staging".to_owned(), vec!["api.service".to_owned()]);
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };
    let connections = vec![make_conn("staging", ssh.port)];

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.start_broker(cx);
            h
        })
    });

    // 1. Caller disconnects (stdio EOF) AFTER Allow once and SSH exec has reached the Host.
    //    Broker must cancel ExecCancel promptly (not wait 10 s) and record unknown_outcome
    //    with AgentDecision::AllowOnce (NOT Cancelled).
    let mut mcp1 = McpClient::start(sock.clone());
    mcp1.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Claude Code" }
        }),
        &mut app,
    );
    let _id1 = mcp1.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let (dec1, _) = wait_for_decision(&host_entity, &mut app);
    assert!(app.update(|cx| host_entity.update(cx, |h, _| h.answer(dec1, CHOICE_ALLOW_ONCE))));
    wait_for_signal(&started_rx, &mut app);

    let cancel_at = Instant::now();
    mcp1.close_stdin();
    let outcomes = wait_for_outcomes(&host_entity, 1, &mut app);
    assert!(
        cancel_at.elapsed() < Duration::from_secs(3),
        "in-flight SSH exec should abort promptly instead of waiting for the 10s delay"
    );
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].decision, AgentDecision::AllowOnce);
    assert!(matches!(
        &outcomes[0].result,
        BrokerResult::Error(err) if err.error == AgentErrorCode::UnknownOutcome
    ));

    // 2. Grant-reused exec followed by MCP notifications/cancelled after dispatch also records
    //    unknown_outcome (with AgentDecision::GrantReused) and keeps tern mcp alive.
    slow_mode.store(false, Ordering::SeqCst);
    let mut mcp2 = McpClient::start(sock);
    mcp2.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Cursor" }
        }),
        &mut app,
    );
    let grant_resp = mcp2.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_UNTIL_LOCK,
    );
    assert_eq!(grant_resp["result"]["isError"], false);
    let _ = started_rx.try_recv();
    app.update(|cx| host_entity.update(cx, |h, _| h.take_outcomes()));

    slow_mode.store(true, Ordering::SeqCst);
    let id_grant_cancel = mcp2.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    wait_for_signal(&started_rx, &mut app);

    let cancel_at2 = Instant::now();
    mcp2.notify_with_params(
        "notifications/cancelled",
        &json!({ "requestId": id_grant_cancel }),
    );
    let outcomes2 = wait_for_outcomes(&host_entity, 1, &mut app);
    assert!(
        cancel_at2.elapsed() < Duration::from_secs(3),
        "grant-reused in-flight SSH exec should abort promptly on notifications/cancelled"
    );
    assert_eq!(outcomes2.len(), 1);
    assert_eq!(outcomes2[0].decision, AgentDecision::GrantReused);
    assert!(matches!(
        &outcomes2[0].result,
        BrokerResult::Error(err) if err.error == AgentErrorCode::UnknownOutcome
    ));

    let pong = mcp2.call_sync("ping", &json!({}), &mut app);
    assert_eq!(pong["result"], json!({}));
}

#[test]
fn end_to_end_background_decision_triggers_attention_cue_without_focus_steal() {
    use gpui::{IntoElement, Render, Window, div};

    struct PassiveView {
        focus: gpui::FocusHandle,
    }

    impl Render for PassiveView {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

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

    let window = app.open_window(|_window, cx| PassiveView {
        focus: cx.focus_handle(),
    });
    let win_handle = window.handle();
    drop(window);
    let was_focused = app.update(|cx| {
        win_handle
            .update(cx, |view, window, _| view.focus.is_focused(window))
            .unwrap()
    });
    assert!(!was_focused, "passive view starts unfocused");

    let mut hosts_map = BTreeMap::new();
    hosts_map.insert("staging".to_owned(), vec!["api.service".to_owned()]);
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };
    let connections = vec![make_conn("staging", ssh.port)];
    let spy = SpyAttention::new(false);

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.set_attention(Arc::clone(&spy));
            h.start_broker(cx);
            h
        })
    });

    let mut mcp = McpClient::start(sock);
    mcp.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Claude\x1b[31m\u{202e} Code\x1b[0m" }
        }),
        &mut app,
    );

    // 1. Background Decision (active = false) triggers sanitized attention cue and never steals focus.
    let id_bg = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let (dec_bg, _) = wait_for_decision(&host_entity, &mut app);
    let cues = spy.take_cues();
    assert_eq!(cues.len(), 1);
    assert!(cues[0].title.contains("staging"));
    assert!(cues[0].body.contains("Claude Code"));
    assert!(cues[0].body.contains("api.service"));
    assert!(!cues[0].body.contains('\x1b'));
    assert!(!cues[0].body.contains('\u{202e}'));
    let still_focused = app.update(|cx| {
        win_handle
            .update(cx, |view, window, _| view.focus.is_focused(window))
            .unwrap()
    });
    assert!(
        !still_focused,
        "background Decision must never steal window focus"
    );

    assert!(app.update(|cx| host_entity.update(cx, |h, _| h.answer(dec_bg, CHOICE_DENY))));
    let resp_bg = mcp.recv_pumping(&mut app);
    assert_eq!(resp_bg["id"], id_bg);

    // 2. Foreground Decision (active = true) stays quiet (zero background cues).
    spy.set_active(true);
    let resp_fg = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_UNTIL_LOCK,
    );
    assert_eq!(resp_fg["result"]["isError"], false);
    assert!(spy.take_cues().is_empty());

    // 3. Reusing an active Grant while in the background does not trigger an attention cue.
    spy.set_active(false);
    let resp_grant = mcp.call_sync(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
        &mut app,
    );
    assert_eq!(resp_grant["result"]["isError"], false);
    assert!(spy.take_cues().is_empty());

    // 4. Rejecting a call while paused in the background does not trigger an attention cue.
    app.update(|cx| host_entity.update(cx, |h, _| h.set_agents_paused(true)));
    let resp_paused = mcp.call_sync(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
        &mut app,
    );
    assert_eq!(resp_paused["result"]["isError"], true);
    assert!(spy.take_cues().is_empty());
}

fn read_audit_records(path: &Path) -> Vec<AuditRecord> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<AuditRecord>(line).unwrap())
        .collect()
}

#[test]
fn end_to_end_audit_log_and_activity_cover_every_decision_and_service_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let vault_path = dir.path().join("vault.age");
    let sock = socket_path(dir.path());

    let ssh = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), |cmd| {
        if cmd.ends_with("api.service") {
            ExecReply::ok("loaded\nactive\nrunning\n")
        } else if cmd.ends_with("worker.service") {
            ExecReply::ok("loaded\nfailed\nfailed\n")
        } else if cmd.ends_with("ghost.service") {
            ExecReply::ok("not-found\ninactive\n\n")
        } else if cmd.ends_with("bsd.service") {
            ExecReply {
                stdout: Vec::new(),
                stderr: b"sh: 1: systemctl: not found\n".to_vec(),
                exit_status: 127,
                delay: Duration::ZERO,
            }
        } else if cmd.ends_with("broken.service") {
            ExecReply {
                stdout: Vec::new(),
                stderr: b"Permission denied\n".to_vec(),
                exit_status: 1,
                delay: Duration::ZERO,
            }
        } else if cmd.ends_with("slow.service") {
            ExecReply {
                stdout: b"loaded\nactive\nrunning\n".to_vec(),
                stderr: Vec::new(),
                exit_status: 0,
                delay: Duration::from_secs(3),
            }
        } else {
            ExecReply::ok("loaded\ninactive\ndead\n")
        }
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

    let mut hosts_map = BTreeMap::new();
    hosts_map.insert(
        "staging".to_owned(),
        vec![
            "api.service".to_owned(),
            "worker.service".to_owned(),
            "ghost.service".to_owned(),
            "bsd.service".to_owned(),
            "broken.service".to_owned(),
            "slow.service".to_owned(),
        ],
    );
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };
    let connections = vec![make_conn("staging", ssh.port)];

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.start_broker(cx);
            h
        })
    });

    let mut mcp = McpClient::start(sock);
    mcp.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Cursor" }
        }),
        &mut app,
    );

    // 1. Allowlist denial (unlisted host).
    let r = mcp.call_sync(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "prod", "service": "api.service" }
        }),
        &mut app,
    );
    assert_eq!(r["result"]["isError"], true);

    // 2. Allowlist denial (unlisted service on allowed host).
    let r = mcp.call_sync(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "secret.service" }
        }),
        &mut app,
    );
    assert_eq!(r["result"]["isError"], true);

    // 3. Allow once -> active.
    let r = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], false);

    // 4. Allow until lock -> failed, then Grant reuse -> failed without a Decision.
    let r = mcp.service_status_with_decision(
        "staging",
        "worker.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_UNTIL_LOCK,
    );
    assert_eq!(r["result"]["isError"], false);
    let r = mcp.call_sync(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "worker.service" }
        }),
        &mut app,
    );
    assert_eq!(r["result"]["isError"], false);

    // 5. Allow once -> not_found (LoadState::NotFound takes precedence over ActiveState::Inactive).
    let r = mcp.service_status_with_decision(
        "staging",
        "ghost.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], false);

    // 6. Allow once -> unsupported_host.
    let r = mcp.service_status_with_decision(
        "staging",
        "bsd.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], true);

    // 7. Allow once -> ssh_error.
    let r = mcp.service_status_with_decision(
        "staging",
        "broken.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], true);

    // 8. Allow once -> exec timed_out.
    let r = mcp.service_status_with_decision(
        "staging",
        "slow.service",
        Some(1),
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], true);

    // 9. Explicit Deny.
    let r = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_DENY,
    );
    assert_eq!(r["result"]["isError"], true);

    // 10. Decision expiry -> decision = timed_out, outcome = timed_out.
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.set_decision_timeout(Duration::from_millis(80));
        });
    });
    let id_exp = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let _ = wait_for_decision(&host_entity, &mut app);
    let exp_resp = mcp.recv_pumping(&mut app);
    assert_eq!(exp_resp["id"], id_exp);
    assert_eq!(exp_resp["result"]["isError"], true);
    app.update(|cx| {
        host_entity.update(cx, |h, _| {
            h.set_decision_timeout(DEFAULT_DECISION_TIMEOUT);
        });
    });

    // 11. Headless Login ladder failure (vault locked) -> user_action_required (vault_locked).
    app.update(Keeper::lock);
    let r = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_ONCE,
    );
    assert_eq!(r["result"]["isError"], true);

    // 12. Paused -> decision = paused, outcome = agents_paused.
    app.update(|cx| host_entity.update(cx, |h, _| h.set_agents_paused(true)));
    let r = mcp.call_sync(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
        &mut app,
    );
    assert_eq!(r["result"]["isError"], true);

    // Verify agent-audit.jsonl mode (0o600), zero secrets, and exact records.
    let path = audit_path(dir.path());
    assert!(path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "agent-audit.jsonl must be mode 0o600");
    }
    let raw_log = std::fs::read_to_string(&path).unwrap();
    assert!(!raw_log.contains(TEST_PASSWORD));
    assert!(!raw_log.contains("vp1"));
    assert!(!raw_log.contains("Permission denied"));

    let records = read_audit_records(&path);
    let summary: Vec<_> = records
        .iter()
        .map(|r| {
            assert_eq!(r.caller, "Cursor");
            assert!(chrono::DateTime::parse_from_rfc3339(&r.timestamp).is_ok());
            (
                r.host.as_str(),
                r.service.as_str(),
                r.decision,
                r.outcome,
                r.reason,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                "prod",
                "api.service",
                AuditDecision::Denied,
                AuditOutcome::Denied,
                None
            ),
            (
                "staging",
                "secret.service",
                AuditDecision::Denied,
                AuditOutcome::Denied,
                None
            ),
            (
                "staging",
                "api.service",
                AuditDecision::AllowOnce,
                AuditOutcome::Active,
                None
            ),
            (
                "staging",
                "worker.service",
                AuditDecision::AllowUntilLock,
                AuditOutcome::Failed,
                None
            ),
            (
                "staging",
                "worker.service",
                AuditDecision::GrantReused,
                AuditOutcome::Failed,
                None
            ),
            (
                "staging",
                "ghost.service",
                AuditDecision::AllowOnce,
                AuditOutcome::NotFound,
                None
            ),
            (
                "staging",
                "bsd.service",
                AuditDecision::AllowOnce,
                AuditOutcome::UnsupportedHost,
                None
            ),
            (
                "staging",
                "broken.service",
                AuditDecision::AllowOnce,
                AuditOutcome::SshError,
                None
            ),
            (
                "staging",
                "slow.service",
                AuditDecision::AllowOnce,
                AuditOutcome::TimedOut,
                None
            ),
            (
                "staging",
                "api.service",
                AuditDecision::Denied,
                AuditOutcome::Denied,
                None
            ),
            (
                "staging",
                "api.service",
                AuditDecision::TimedOut,
                AuditOutcome::TimedOut,
                None
            ),
            (
                "staging",
                "api.service",
                AuditDecision::AllowOnce,
                AuditOutcome::UserActionRequired,
                Some(UserActionReason::VaultLocked),
            ),
            (
                "staging",
                "api.service",
                AuditDecision::Paused,
                AuditOutcome::AgentsPaused,
                None
            ),
        ]
    );

    // Verify every outcome emitted a quiet Activity (never an Alert) with GLOSSARY.md wording.
    let entries = app.update(|cx| host_entity.read(cx).activity_entries());
    assert_eq!(entries.len(), records.len());
    for entry in &entries {
        assert_eq!(entry.kind, MessageKind::Activity);
        assert!(!entry.failed);
    }
    let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "Denied api.service check on prod for Cursor",
            "Denied secret.service check on staging for Cursor",
            "Agent checked api.service on staging: active",
            "Agent checked worker.service on staging: failed",
            "Agent checked worker.service on staging: failed",
            "Agent checked ghost.service on staging: not found",
            "Agent check failed for bsd.service on staging (unsupported host)",
            "Agent check failed for broken.service on staging (SSH error)",
            "Agent check timed out for slow.service on staging",
            "Denied api.service check on staging for Cursor",
            "Agent request timed out for api.service on staging",
            "Agent check failed for api.service on staging (vault locked)",
            "Agent check paused for api.service on staging",
        ]
    );
}

#[test]
fn end_to_end_audit_log_and_activity_cover_pre_dispatch_cancel_and_post_dispatch_unknown_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let vault_path = dir.path().join("vault.age");
    let sock = socket_path(dir.path());

    let (started_tx, started_rx) = async_channel::bounded::<()>(4);
    let ssh = TestSshServer::start(ServerAuth::Password(TEST_PASSWORD), move |_| {
        let _ = started_tx.try_send(());
        ExecReply {
            stdout: b"loaded\nactive\nrunning\n".to_vec(),
            stderr: Vec::new(),
            exit_status: 0,
            delay: Duration::from_secs(5),
        }
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

    let mut hosts_map = BTreeMap::new();
    hosts_map.insert("staging".to_owned(), vec!["api.service".to_owned()]);
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };
    let connections = vec![make_conn("staging", ssh.port)];

    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.start_broker(cx);
            h
        })
    });

    // 1. Pre-dispatch cancel via MCP notifications/cancelled while Decision is waiting.
    let mut mcp = McpClient::start(sock.clone());
    mcp.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Claude Code" }
        }),
        &mut app,
    );
    let id_cancel = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let _ = wait_for_decision(&host_entity, &mut app);
    mcp.notify_with_params(
        "notifications/cancelled",
        &json!({ "requestId": id_cancel }),
    );
    wait_for_outcomes(&host_entity, 1, &mut app);

    // 2. Post-dispatch disconnect while SSH exec is running -> unknown_outcome.
    let _ = mcp.send(
        "tools/call",
        &json!({
            "name": "service_status",
            "arguments": { "host": "staging", "service": "api.service" }
        }),
    );
    let (dec_id, _) = wait_for_decision(&host_entity, &mut app);
    assert!(app.update(|cx| host_entity.update(cx, |h, _| h.answer(dec_id, CHOICE_ALLOW_ONCE))));
    wait_for_signal(&started_rx, &mut app);
    mcp.close_stdin();
    wait_for_outcomes(&host_entity, 1, &mut app);

    let records = read_audit_records(&audit_path(dir.path()));
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].caller, "Claude Code");
    assert_eq!(records[0].decision, AuditDecision::Cancelled);
    assert_eq!(records[0].outcome, AuditOutcome::Denied);

    assert_eq!(records[1].caller, "Claude Code");
    assert_eq!(records[1].decision, AuditDecision::AllowOnce);
    assert_eq!(records[1].outcome, AuditOutcome::UnknownOutcome);

    let entries = app.update(|cx| host_entity.read(cx).activity_entries());
    let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "Agent check cancelled for api.service on staging",
            "Agent check outcome unknown for api.service on staging",
        ]
    );
}

#[cfg(unix)]
#[test]
fn end_to_end_audit_log_rotates_with_0600_mode_under_live_broker_traffic() {
    use std::os::unix::fs::PermissionsExt as _;

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

    let mut hosts_map = BTreeMap::new();
    hosts_map.insert("staging".to_owned(), vec!["api.service".to_owned()]);
    let config = AgentConfig {
        enabled: true,
        paused: false,
        hosts: hosts_map,
    };
    let connections = vec![make_conn("staging", ssh.port)];

    // ~165 bytes per record; max_bytes = 360 fits 2 records per file and keeps 2 backups (.1, .2).
    let retention = AuditRetention {
        max_bytes: 360,
        max_backups: 2,
    };
    let host_entity = app.update(|cx| {
        cx.new(|cx| {
            let mut h = TestHost::new(dir.path().to_path_buf(), kh.clone(), config, connections);
            h.set_audit_retention(retention);
            h.start_broker(cx);
            h
        })
    });

    let mut mcp = McpClient::start(sock);
    mcp.call_sync(
        "initialize",
        &json!({
            "protocolVersion": "2025-03-26",
            "clientInfo": { "name": "Cursor" }
        }),
        &mut app,
    );

    let first = mcp.service_status_with_decision(
        "staging",
        "api.service",
        None,
        &host_entity,
        &mut app,
        |_| CHOICE_ALLOW_UNTIL_LOCK,
    );
    assert_eq!(first["result"]["isError"], false);

    for _ in 0..6 {
        let resp = mcp.call_sync(
            "tools/call",
            &json!({
                "name": "service_status",
                "arguments": { "host": "staging", "service": "api.service" }
            }),
            &mut app,
        );
        assert_eq!(resp["result"]["isError"], false);
    }

    let primary = audit_path(dir.path());
    let b1 = rotated_path(dir.path(), 1);
    let b2 = rotated_path(dir.path(), 2);
    let b3 = rotated_path(dir.path(), 3);

    assert!(primary.exists());
    assert!(b1.exists());
    assert!(b2.exists());
    assert!(!b3.exists());

    for p in [&primary, &b1, &b2] {
        let meta = std::fs::metadata(p).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert!(meta.len() <= retention.max_bytes);
        assert!(!read_audit_records(p).is_empty());
    }

    // All 7 live outcomes also recorded in the Activity list.
    let entries = app.update(|cx| host_entity.read(cx).activity_entries());
    assert_eq!(entries.len(), 7);
    assert!(entries.iter().all(|e| e.kind == MessageKind::Activity
        && e.text == "Agent checked api.service on staging: active"));
}

fn generate_encrypted_key(dir: &Path, name: &str, passphrase: &str) -> PathBuf {
    let priv_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .unwrap()
        .encrypt(&mut rand::rng(), passphrase)
        .unwrap();
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
    path
}

#[test]
fn headless_login_declined_passphrase_does_not_mask_post_auth_timeout() {
    use super::headless_login::{self, HeadlessExec};
    use crate::password_command::{Approvals, Auth as PcAuth};
    use tern_ssh::{ConnectSpec, ExecOptions};

    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let vault_path = dir.path().join("vault.age");

    let enc_key_path = generate_encrypted_key(dir.path(), "enc_ed25519", "key-secret");
    let (fallback_key_path, fallback_pub_key, _) =
        generate_unencrypted_key(dir.path(), "fallback_ed25519");

    let slow_ssh = TestSshServer::start(ServerAuth::PublicKey(fallback_pub_key), |_| ExecReply {
        stdout: b"loaded\nactive\nrunning\n".to_vec(),
        stderr: Vec::new(),
        exit_status: 0,
        delay: Duration::from_secs(3),
    });
    slow_ssh.write_known_hosts(&kh);

    let app = setup_test_app(vault_path, false);

    let mut spec =
        ConnectSpec::from_host_entry(&make_conn("slow", slow_ssh.port).entry_in(&[])).unwrap();
    spec.known_hosts = Some(kh);
    // Encrypted key comes first (declined headless -> latches prompt_failure), followed by
    // unencrypted fallback key that authenticates cleanly, then the remote exec times out.
    spec.identity_files = vec![enc_key_path, fallback_key_path];

    let req = HeadlessExec {
        spec,
        auth: PcAuth::default(),
        approvals: Approvals::in_dir(Some(dir.path().to_path_buf())),
        command: "systemctl show -p LoadState,ActiveState,SubState --value -- api.service".into(),
        options: ExecOptions::with_timeout(Duration::from_secs(1)),
    };

    let (tx, rx) = async_channel::bounded(1);
    app.spawn(async move |mut cx| {
        let res = headless_login::run(req, |_| {}, &mut cx).await;
        let _ = tx.send(res).await;
    })
    .detach();

    let deadline = Instant::now() + Duration::from_secs(10);
    let err = loop {
        app.run_until_parked();
        if let Ok(res) = rx.try_recv() {
            break res.unwrap_err();
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for headless_login"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(
        err.error,
        AgentErrorCode::TimedOut,
        "declined key passphrase must not mask post-auth timeout; got {err:?}"
    );
    assert_eq!(err.reason, None);
}
