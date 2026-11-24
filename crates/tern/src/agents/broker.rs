//! Local Unix socket broker (`agent.sock`, mode `0o600`) and request orchestration (ADR 0003).
//!
//! Binds `agent.sock` on [`SshRuntime`] only when Agents are enabled, leaving `tern.sock`
//! untouched. Validates `(host, service)` against `agents.json` and Saved connections before
//! raising a click-only [`DecisionTag::Agent`] Decision, then runs `systemctl show` through
//! [`super::headless_login`] and [`super::systemctl`].

use std::io;
use std::path::PathBuf;
use std::time::Instant;

use chrono::Utc;
use gpui::{AsyncApp, Context, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use tern_ssh::{ConnectSpec, ExecOptions};
use tokio::sync::watch;

use super::broker_socket::{IncomingRequest, bind_and_serve};
use super::grants::GrantFingerprint;
use super::headless_login::{self, HeadlessExec};
use super::protocol::{
    AgentError, AgentErrorCode, AllowedHost, BrokerRequest, BrokerResult, ListHostsResponse,
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
/// Decision button identifier for creating an in-memory Grant until lock/sleep/quit/revoke.
pub const CHOICE_ALLOW_UNTIL_LOCK: &str = "allow_until_lock";
/// Decision button identifier for declining the request.
pub const CHOICE_DENY: &str = "deny";

const LIST_HOSTS_USAGE: &str = "Call service_status with an alias from hosts and one of its listed services. Optional timeout_secs defaults to 15 (clamped to 1..=60).";
const MAX_PENDING_REQUESTS: usize = 32;

/// Point-in-time configuration and Saved connections read from the host entity on the UI thread.
#[derive(Debug, Clone)]
pub struct BrokerSnapshot {
    pub dir: PathBuf,
    pub known_hosts: Option<PathBuf>,
    pub config: AgentConfig,
    pub connections: Vec<Connection>,
}

/// How authorization was resolved for a `service_status` request (seam for `#64` audit log).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDecision {
    AllowOnce,
    AllowUntilLock,
    GrantReused,
    Denied,
    Expired,
    Cancelled,
    Paused,
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
    pub decision: AgentDecision,
    pub result: BrokerResult,
}

/// Host UI seam implemented by `Shell` (and test hosts in `gpui::TestApp`).
pub trait AgentHost: Sized + 'static {
    fn agent_snapshot(&self) -> BrokerSnapshot;

    /// Returns `true` when an in-memory Grant exists for `(host, service)` whose stored
    /// fingerprint matches `fingerprint`, evicting any mismatched Grant immediately.
    fn verify_agent_grant(
        &mut self,
        host: &str,
        service: &str,
        fingerprint: GrantFingerprint,
        cx: &mut Context<Self>,
    ) -> bool;

    /// Records an in-memory-only Grant for `(host, service)` bound to `fingerprint`.
    fn record_agent_grant(
        &mut self,
        host: &str,
        service: &str,
        fingerprint: GrantFingerprint,
        cx: &mut Context<Self>,
    );

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

fn allowed_connection(snapshot: &BrokerSnapshot, host: &str, service: &str) -> Option<Connection> {
    snapshot
        .config
        .is_service_allowed(host, service, &snapshot.connections)
        .then(|| {
            snapshot
                .connections
                .iter()
                .find(|c| c.name == host)
                .cloned()
        })
        .flatten()
}

enum Precheck {
    Paused,
    NotAllowed,
    InvalidUnit(String),
    Granted {
        snapshot: BrokerSnapshot,
        conn: Connection,
        command: String,
    },
    NeedsDecision {
        conn: Connection,
        command: String,
        fingerprint: GrantFingerprint,
    },
}

enum Postcheck {
    Paused,
    NotAllowed,
    Changed,
    Ready(Box<(BrokerSnapshot, Connection)>),
}

struct ServiceRequest<'a, H> {
    host: &'a WeakEntity<H>,
    caller: String,
    target_host: String,
    service: String,
}

impl<H: AgentHost> ServiceRequest<'_, H> {
    fn report(
        &self,
        command: Option<&str>,
        decision: AgentDecision,
        result: &BrokerResult,
        cx: &mut AsyncApp,
    ) {
        let outcome = AgentOutcome {
            caller: self.caller.clone(),
            host: self.target_host.clone(),
            service: self.service.clone(),
            command: command.map(str::to_owned),
            decision,
            result: result.clone(),
        };
        let _ = self
            .host
            .update(cx, |h, cx| h.on_agent_outcome(&outcome, cx));
    }

    fn fail(
        &self,
        command: Option<&str>,
        decision: AgentDecision,
        code: AgentErrorCode,
        message: impl Into<String>,
        cx: &mut AsyncApp,
    ) -> BrokerResult {
        let res = BrokerResult::Error(AgentError::new(code, message));
        self.report(command, decision, &res, cx);
        res
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
    let req = ServiceRequest {
        host,
        caller: caller_display(caller.as_deref()),
        target_host,
        service,
    };

    let Ok(precheck) = host.update(cx, |h, cx| {
        let snapshot = h.agent_snapshot();
        if snapshot.config.paused {
            return Precheck::Paused;
        }
        let Some(conn) = allowed_connection(&snapshot, &req.target_host, &req.service) else {
            return Precheck::NotAllowed;
        };
        let command = match systemctl::command_for(&req.service) {
            Ok(cmd) => cmd,
            Err(e) => return Precheck::InvalidUnit(e.to_string()),
        };
        let fingerprint =
            GrantFingerprint::for_connection(&conn, &snapshot.connections, &req.service);
        if h.verify_agent_grant(&req.target_host, &req.service, fingerprint, cx) {
            Precheck::Granted {
                snapshot,
                conn,
                command,
            }
        } else {
            Precheck::NeedsDecision {
                conn,
                command,
                fingerprint,
            }
        }
    }) else {
        return BrokerResult::Error(AgentError::new(
            AgentErrorCode::TernNotRunning,
            "tern window is no longer available",
        ));
    };

    let (conn, command, fingerprint) = match precheck {
        Precheck::Paused => {
            return req.fail(
                None,
                AgentDecision::Paused,
                AgentErrorCode::AgentsPaused,
                "agents are paused in tern",
                cx,
            );
        }
        Precheck::NotAllowed => {
            let msg = format!(
                "service '{}' on host '{}' is not allowed; call list_hosts to see allowed hosts and services",
                sanitize_display_text(&req.service),
                sanitize_display_text(&req.target_host),
            );
            return req.fail(None, AgentDecision::Denied, AgentErrorCode::Denied, msg, cx);
        }
        Precheck::InvalidUnit(msg) => {
            return req.fail(None, AgentDecision::Denied, AgentErrorCode::Denied, msg, cx);
        }
        Precheck::Granted {
            snapshot,
            conn,
            command,
        } => {
            let res =
                execute_service_status(&conn, &snapshot, &req.service, &command, timeout_secs, cx)
                    .await;
            req.report(Some(&command), AgentDecision::GrantReused, &res, cx);
            return res;
        }
        Precheck::NeedsDecision {
            conn,
            command,
            fingerprint,
        } => (conn, command, fingerprint),
    };

    // Ask explicit human approval via the click-only Decision queue.
    // #62 adds a 60 s deadline & background attention cues here.
    let prompt = build_service_status_prompt(&req.caller, &conn, &command);
    let pending = match host.update(cx, |h, cx| h.ask_agent_decision(prompt, None, cx)) {
        Ok(Ok(pending)) => pending,
        Ok(Err(QueueFull)) => {
            return req.fail(
                Some(&command),
                AgentDecision::Denied,
                AgentErrorCode::Denied,
                "too many pending approval decisions in tern",
                cx,
            );
        }
        Err(_) => {
            return BrokerResult::Error(AgentError::new(
                AgentErrorCode::TernNotRunning,
                "tern window is no longer available",
            ));
        }
    };

    let decision = match pending.wait().await {
        DecisionOutcome::Chosen(choice) if choice == CHOICE_ALLOW_ONCE => AgentDecision::AllowOnce,
        DecisionOutcome::Chosen(choice) if choice == CHOICE_ALLOW_UNTIL_LOCK => {
            AgentDecision::AllowUntilLock
        }
        DecisionOutcome::Expired => {
            return req.fail(
                Some(&command),
                AgentDecision::Expired,
                AgentErrorCode::TimedOut,
                "approval decision expired",
                cx,
            );
        }
        DecisionOutcome::Cancelled => {
            let Ok(paused) = host.update(cx, |h, _| h.agent_snapshot().config.paused) else {
                return BrokerResult::Error(AgentError::new(
                    AgentErrorCode::TernNotRunning,
                    "tern window is no longer available",
                ));
            };
            if paused {
                return req.fail(
                    Some(&command),
                    AgentDecision::Paused,
                    AgentErrorCode::AgentsPaused,
                    "agents are paused in tern",
                    cx,
                );
            }
            return req.fail(
                Some(&command),
                AgentDecision::Cancelled,
                AgentErrorCode::Denied,
                "request cancelled in tern",
                cx,
            );
        }
        DecisionOutcome::Chosen(_) => {
            return req.fail(
                Some(&command),
                AgentDecision::Denied,
                AgentErrorCode::Denied,
                "request denied in tern",
                cx,
            );
        }
    };

    // Re-read current Saved connections, allowlist, and live fingerprint before dialing or recording
    // a Grant in case settings changed while the Decision was open.
    let Ok(postcheck) = host.update(cx, |h, cx| {
        let fresh = h.agent_snapshot();
        if fresh.config.paused {
            return Postcheck::Paused;
        }
        let Some(conn) = allowed_connection(&fresh, &req.target_host, &req.service) else {
            return Postcheck::NotAllowed;
        };
        let fresh_fp = GrantFingerprint::for_connection(&conn, &fresh.connections, &req.service);
        if fresh_fp != fingerprint {
            return Postcheck::Changed;
        }
        if decision == AgentDecision::AllowUntilLock {
            h.record_agent_grant(&req.target_host, &req.service, fresh_fp, cx);
        }
        Postcheck::Ready(Box::new((fresh, conn)))
    }) else {
        return BrokerResult::Error(AgentError::new(
            AgentErrorCode::TernNotRunning,
            "tern window is no longer available",
        ));
    };

    let (fresh, conn) = match postcheck {
        Postcheck::Paused => {
            return req.fail(
                Some(&command),
                AgentDecision::Paused,
                AgentErrorCode::AgentsPaused,
                "agents are paused in tern",
                cx,
            );
        }
        Postcheck::NotAllowed => {
            return req.fail(
                Some(&command),
                AgentDecision::Denied,
                AgentErrorCode::Denied,
                "host or service is no longer allowed in tern",
                cx,
            );
        }
        Postcheck::Changed => {
            return req.fail(
                Some(&command),
                AgentDecision::Denied,
                AgentErrorCode::Denied,
                "saved connection changed before approval completed",
                cx,
            );
        }
        Postcheck::Ready(ready) => *ready,
    };

    let res = execute_service_status(&conn, &fresh, &req.service, &command, timeout_secs, cx).await;
    req.report(Some(&command), decision, &res, cx);
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
            DecisionChoice::secondary(CHOICE_ALLOW_UNTIL_LOCK, "Allow until lock"),
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
