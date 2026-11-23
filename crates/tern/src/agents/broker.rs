//! Local Unix socket broker (`agent.sock`, mode `0o600`) and request orchestration (ADR 0003).
//!
//! Binds `agent.sock` on [`SshRuntime`] only when Agents are enabled, leaving `tern.sock`
//! untouched. Validates `(host, service)` against `agents.json` and Saved connections before
//! raising a click-only [`DecisionTag::Agent`] Decision, then runs `systemctl show` through
//! [`super::headless_login`] and [`super::systemctl`].

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use gpui::{AsyncApp, Context, Task, WeakEntity};
use tern_ssh::{ConnectSpec, ExecOptions};
use tokio::io::BufReader;
use tokio::sync::{Semaphore, watch};

use super::headless_login::{self, HeadlessExec};
use super::protocol::{
    AgentError, AgentErrorCode, AllowedHost, BrokerMessage, BrokerRequest, BrokerResult,
    ListHostsResponse, MAX_FRAME_BYTES, read_line_bounded, write_json_line,
};
use super::store::AgentConfig;
use super::systemctl;
use crate::connections::Connection;
use crate::decisions::{
    DecisionChoice, DecisionOutcome, DecisionPrompt, PendingDecision, QueueFull,
    sanitize_display_text,
};
use crate::password_command::{Approvals, Auth};
use crate::platform::SshRuntime;

/// Decision button identifier for one-time execution approval.
pub const CHOICE_ALLOW_ONCE: &str = "allow_once";
/// Decision button identifier for declining the request.
pub const CHOICE_DENY: &str = "deny";

const LIST_HOSTS_USAGE: &str = "Call service_status with an alias from hosts and one of its listed services. Optional timeout_secs defaults to 15 (clamped to 1..=60).";
const MAX_PENDING_REQUESTS: usize = 32;
const MAX_CONCURRENT_CONNECTIONS: usize = 32;

/// Point-in-time configuration and Saved connections read from the host entity on the UI thread.
#[derive(Debug, Clone)]
pub struct BrokerSnapshot {
    pub dir: PathBuf,
    pub known_hosts: Option<PathBuf>,
    pub config: AgentConfig,
    pub connections: Vec<Connection>,
}

/// Summary of a completed `service_status` request, passed to [`AgentHost::on_agent_outcome`]
/// as the seam for local audit logging and Activity entries (#64).
#[derive(Debug, Clone)]
#[allow(dead_code)] // Consumed by audit logging and Activity integration (#64).
pub struct AgentOutcome {
    pub caller: String,
    pub host: String,
    pub service: String,
    pub command: Option<String>,
    pub result: BrokerResult,
}

/// Host UI seam implemented by `Shell` (and test hosts in `gpui::TestApp`).
pub trait AgentHost: Sized + 'static {
    fn agent_snapshot(&self) -> BrokerSnapshot;

    /// Enqueues an Agent approval Decision.
    ///
    /// # Errors
    /// Returns [`QueueFull`] when the Decision queue is full.
    fn ask_agent_decision(
        &mut self,
        prompt: DecisionPrompt,
        deadline: Option<Instant>,
        cx: &mut Context<Self>,
    ) -> Result<PendingDecision, QueueFull>;

    /// Hook called when a `service_status` request finishes (seam for #64 audit + Activity).
    fn on_agent_outcome(&mut self, _outcome: &AgentOutcome, _cx: &mut Context<Self>) {}
}

/// Per-connection response and cancellation channel for one accepted `agent.sock` client.
#[derive(Debug)]
struct Responder {
    tx: async_channel::Sender<BrokerMessage>,
    /// Fires when the client sends `BrokerRequest::Cancel` or disconnects (#62).
    #[allow(dead_code)]
    cancelled: watch::Receiver<bool>,
}

impl Responder {
    async fn finish(self, result: BrokerResult) {
        let _ = self.tx.send(BrokerMessage::Result(result)).await;
    }
}

#[derive(Debug)]
struct IncomingRequest {
    request: BrokerRequest,
    responder: Responder,
}

/// Active `agent.sock` listener and dispatch task. Dropping it stops accepting new connections
/// and removes `agent.sock` cleanly.
pub struct BrokerHandle {
    socket_path: PathBuf,
    shutdown: watch::Sender<bool>,
    _dispatch: Task<()>,
}

impl std::fmt::Debug for BrokerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerHandle")
            .field("socket_path", &self.socket_path)
            .finish_non_exhaustive()
    }
}

impl Drop for BrokerHandle {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        let _ = std::fs::remove_file(&self.socket_path);
        tracing::info!(socket = %self.socket_path.display(), "agent_broker_stopped");
    }
}

impl BrokerHandle {
    /// Binds `socket_path` (mode `0o600`) on [`SshRuntime`] and spawns the GPUI dispatch loop
    /// targeting `host`.
    ///
    /// # Errors
    /// Returns an [`io::Error`] if the parent directory cannot be created, the socket cannot be
    /// bound, or permissions cannot be restricted to `0o600`.
    pub fn start<H: AgentHost>(
        socket_path: PathBuf,
        host: WeakEntity<H>,
        cx: &mut Context<H>,
    ) -> io::Result<Self> {
        let rt = SshRuntime::handle(cx);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (req_tx, req_rx) = async_channel::bounded::<IncomingRequest>(MAX_PENDING_REQUESTS);

        bind_and_serve(&socket_path, &rt, req_tx, shutdown_rx)?;

        let dispatch = cx.spawn(async move |_, cx| {
            while let Ok(incoming) = req_rx.recv().await {
                let host = host.clone();
                cx.spawn(async move |cx| {
                    handle_incoming(host, incoming, cx).await;
                })
                .detach();
            }
        });

        tracing::info!(socket = %socket_path.display(), "agent_broker_started");
        Ok(Self {
            socket_path,
            shutdown: shutdown_tx,
            _dispatch: dispatch,
        })
    }
}

#[cfg(unix)]
fn bind_and_serve(
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
fn bind_and_serve(
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

async fn handle_incoming<H: AgentHost>(
    host: WeakEntity<H>,
    incoming: IncomingRequest,
    cx: &mut AsyncApp,
) {
    let IncomingRequest { request, responder } = incoming;
    let result = match request {
        BrokerRequest::ListHosts { .. } => handle_list_hosts(&host, cx),
        BrokerRequest::ServiceStatus {
            caller,
            host: target_host,
            service,
            timeout_secs,
        } => handle_service_status(&host, caller, target_host, service, timeout_secs, cx).await,
        BrokerRequest::Cancel => BrokerResult::Error(AgentError::new(
            AgentErrorCode::Denied,
            "unexpected cancel frame",
        )),
    };
    responder.finish(result).await;
}

fn handle_list_hosts<H: AgentHost>(host: &WeakEntity<H>, cx: &mut AsyncApp) -> BrokerResult {
    let Ok(snapshot) = host.update(cx, |h, _| h.agent_snapshot()) else {
        return BrokerResult::Error(AgentError::new(
            AgentErrorCode::TernNotRunning,
            "tern window is no longer available",
        ));
    };
    if snapshot.config.paused {
        return BrokerResult::Error(AgentError::new(
            AgentErrorCode::AgentsPaused,
            "agents are paused in tern",
        ));
    }
    let hosts = snapshot
        .config
        .allowed_hosts(&snapshot.connections)
        .into_iter()
        .map(|(alias, services)| AllowedHost { alias, services })
        .collect();
    let payload = ListHostsResponse {
        hosts,
        usage: LIST_HOSTS_USAGE.to_owned(),
    };
    match serde_json::to_value(payload) {
        Ok(data) => BrokerResult::Ok { data },
        Err(e) => BrokerResult::Error(AgentError::new(AgentErrorCode::SshError, e.to_string())),
    }
}

async fn handle_service_status<H: AgentHost>(
    host: &WeakEntity<H>,
    caller: Option<String>,
    target_host: String,
    service: String,
    timeout_secs: Option<u64>,
    cx: &mut AsyncApp,
) -> BrokerResult {
    let caller_label = caller_display(caller.as_deref());

    let Ok(snapshot) = host.update(cx, |h, _| h.agent_snapshot()) else {
        return BrokerResult::Error(AgentError::new(
            AgentErrorCode::TernNotRunning,
            "tern window is no longer available",
        ));
    };

    if snapshot.config.paused {
        let res = BrokerResult::Error(AgentError::new(
            AgentErrorCode::AgentsPaused,
            "agents are paused in tern",
        ));
        report_outcome(host, &caller_label, &target_host, &service, None, &res, cx);
        return res;
    }

    let allowed_conn = snapshot
        .config
        .is_service_allowed(&target_host, &service, &snapshot.connections)
        .then(|| {
            snapshot
                .connections
                .iter()
                .find(|c| c.name == target_host)
                .cloned()
        })
        .flatten();

    let Some(conn) = allowed_conn else {
        let res = BrokerResult::Error(AgentError::new(
            AgentErrorCode::Denied,
            format!(
                "service '{}' on host '{}' is not allowed; call list_hosts to see allowed hosts and services",
                sanitize_display_text(&service),
                sanitize_display_text(&target_host),
            ),
        ));
        report_outcome(host, &caller_label, &target_host, &service, None, &res, cx);
        return res;
    };

    let command = match systemctl::command_for(&service) {
        Ok(cmd) => cmd,
        Err(e) => {
            let res = BrokerResult::Error(AgentError::new(AgentErrorCode::Denied, e.to_string()));
            report_outcome(host, &caller_label, &target_host, &service, None, &res, cx);
            return res;
        }
    };

    // Ask explicit human approval via the click-only Decision queue.
    // #63 inserts Grant lookup right before this prompt; #62 adds a 60 s deadline & attention cues.
    let prompt = build_service_status_prompt(&caller_label, &conn, &command);
    let pending = match host.update(cx, |h, cx| h.ask_agent_decision(prompt, None, cx)) {
        Ok(Ok(pending)) => pending,
        Ok(Err(QueueFull)) => {
            let res = BrokerResult::Error(AgentError::new(
                AgentErrorCode::Denied,
                "too many pending approval decisions in tern",
            ));
            report_outcome(
                host,
                &caller_label,
                &target_host,
                &service,
                Some(&command),
                &res,
                cx,
            );
            return res;
        }
        Err(_) => {
            return BrokerResult::Error(AgentError::new(
                AgentErrorCode::TernNotRunning,
                "tern window is no longer available",
            ));
        }
    };

    match pending.wait().await {
        DecisionOutcome::Chosen(choice) if choice == CHOICE_ALLOW_ONCE => {}
        DecisionOutcome::Expired => {
            let res = BrokerResult::Error(AgentError::new(
                AgentErrorCode::TimedOut,
                "approval decision expired",
            ));
            report_outcome(
                host,
                &caller_label,
                &target_host,
                &service,
                Some(&command),
                &res,
                cx,
            );
            return res;
        }
        DecisionOutcome::Chosen(_) | DecisionOutcome::Cancelled => {
            let res = BrokerResult::Error(AgentError::new(
                AgentErrorCode::Denied,
                "request denied in tern",
            ));
            report_outcome(
                host,
                &caller_label,
                &target_host,
                &service,
                Some(&command),
                &res,
                cx,
            );
            return res;
        }
    }

    // Re-read current Saved connections & allowlist before dialing in case settings changed.
    let Ok(fresh) = host.update(cx, |h, _| h.agent_snapshot()) else {
        return BrokerResult::Error(AgentError::new(
            AgentErrorCode::TernNotRunning,
            "tern window is no longer available",
        ));
    };
    if fresh.config.paused {
        let res = BrokerResult::Error(AgentError::new(
            AgentErrorCode::AgentsPaused,
            "agents are paused in tern",
        ));
        report_outcome(
            host,
            &caller_label,
            &target_host,
            &service,
            Some(&command),
            &res,
            cx,
        );
        return res;
    }
    let Some(conn) = fresh
        .config
        .is_service_allowed(&target_host, &service, &fresh.connections)
        .then(|| {
            fresh
                .connections
                .iter()
                .find(|c| c.name == target_host)
                .cloned()
        })
        .flatten()
    else {
        let res = BrokerResult::Error(AgentError::new(
            AgentErrorCode::Denied,
            "host or service is no longer allowed in tern",
        ));
        report_outcome(
            host,
            &caller_label,
            &target_host,
            &service,
            Some(&command),
            &res,
            cx,
        );
        return res;
    };

    let res = execute_service_status(&conn, &fresh, &service, &command, timeout_secs, cx).await;
    report_outcome(
        host,
        &caller_label,
        &target_host,
        &service,
        Some(&command),
        &res,
        cx,
    );
    res
}

async fn execute_service_status(
    conn: &Connection,
    snapshot: &BrokerSnapshot,
    service: &str,
    command: &str,
    timeout_secs: Option<u64>,
    cx: &mut AsyncApp,
) -> BrokerResult {
    let entry = conn.entry_in(&snapshot.connections);
    let mut spec = match ConnectSpec::from_host_entry(&entry) {
        Ok(spec) => spec,
        Err(e) => {
            return BrokerResult::Error(AgentError::new(AgentErrorCode::SshError, e.to_string()));
        }
    };
    if let Some(kh) = &snapshot.known_hosts {
        spec.known_hosts = Some(kh.clone());
    } else {
        #[cfg(debug_assertions)]
        {
            spec = crate::shell::with_dev_known_hosts(spec);
        }
    }

    let auth = Auth {
        vault_keys: conn.vault_key.clone().into_iter().collect(),
        password_command: conn.password_command.clone(),
        sudo_command: None,
        proxy_command: conn.proxy_command.clone(),
    };
    let approvals = Approvals::in_dir(Some(snapshot.dir.clone()));
    let options = ExecOptions::with_timeout(systemctl::effective_timeout(timeout_secs));

    let exec_req = HeadlessExec {
        spec,
        auth,
        approvals,
        command: command.to_owned(),
        options,
    };

    match headless_login::run(exec_req, |_| {}, cx).await {
        Ok(output) => match systemctl::parse_output(&conn.name, service, &output, Utc::now()) {
            Ok(status) => match serde_json::to_value(status) {
                Ok(data) => BrokerResult::Ok { data },
                Err(e) => {
                    BrokerResult::Error(AgentError::new(AgentErrorCode::SshError, e.to_string()))
                }
            },
            Err(err) => BrokerResult::Error(err),
        },
        Err(err) => BrokerResult::Error(err),
    }
}

fn caller_display(caller: Option<&str>) -> String {
    caller
        .map(sanitize_display_text)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown Agent".to_owned())
}

/// Builds the click-only Decision card prompt for a `service_status` request.
#[must_use]
pub fn build_service_status_prompt(
    caller: &str,
    conn: &Connection,
    command: &str,
) -> DecisionPrompt {
    DecisionPrompt::new(
        format!("{caller} wants to check service status on {}", conn.name),
        vec![
            DecisionChoice::primary(CHOICE_ALLOW_ONCE, "Allow once"),
            DecisionChoice::secondary(CHOICE_DENY, "Deny"),
        ],
    )
    .with_detail("Agent", format!("{caller} (unverified)"))
    .with_detail(
        "Host",
        format!("{} ({}@{})", conn.name, conn.user, conn.host),
    )
    .with_detail("Returns", "load_state, active_state, sub_state")
    .with_command(command)
}

fn report_outcome<H: AgentHost>(
    host: &WeakEntity<H>,
    caller: &str,
    target_host: &str,
    service: &str,
    command: Option<&str>,
    result: &BrokerResult,
    cx: &mut AsyncApp,
) {
    let outcome = AgentOutcome {
        caller: caller.to_owned(),
        host: target_host.to_owned(),
        service: service.to_owned(),
        command: command.map(str::to_owned),
        result: result.clone(),
    };
    let _ = host.update(cx, |h, cx| h.on_agent_outcome(&outcome, cx));
}
