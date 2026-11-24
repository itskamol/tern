//! Local Unix socket broker (`agent.sock`, mode `0o600`) and request orchestration (ADR 0003).
//!
//! Binds `agent.sock` on [`SshRuntime`] only when Agents are enabled, leaving `tern.sock`
//! untouched. Validates `(host, service)` against `agents.json` and Saved connections before
//! raising a click-only [`DecisionTag::Agent`] Decision, then runs `systemctl show` through
//! [`super::headless_login`] and [`super::systemctl`].

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{AsyncApp, Context, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::attention::AttentionCue;
pub use super::broker_exec::build_service_status_prompt;
use super::broker_exec::{AbortOnDrop, caller_display, execute_service_status};
use super::broker_socket::{IncomingRequest, Responder, bind_and_serve};
use super::grants::GrantFingerprint;
use super::protocol::{
    AgentError, AgentErrorCode, AllowedHost, BrokerRequest, BrokerResult, ListHostsResponse,
};
use super::store::AgentConfig;
use super::systemctl;
use crate::connections::Connection;
use crate::decisions::{
    DecisionOutcome, DecisionPrompt, PendingDecision, QueueFull, sanitize_display_text,
};
use crate::platform::SshRuntime;

/// Decision button identifier for one-time execution approval.
pub const CHOICE_ALLOW_ONCE: &str = "allow_once";
/// Decision button identifier for creating an in-memory Grant until lock/sleep/quit/revoke.
pub const CHOICE_ALLOW_UNTIL_LOCK: &str = "allow_until_lock";
/// Decision button identifier for declining the request.
pub const CHOICE_DENY: &str = "deny";
/// Default wall-clock deadline before an unanswered Agent Decision expires with `timed_out`.
pub const DEFAULT_DECISION_TIMEOUT: Duration = Duration::from_secs(60);

/// Progress message sent while an Agent request is waiting for human approval in tern.
pub const WAITING_FOR_APPROVAL_MESSAGE: &str = "Waiting for approval in tern...";

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

/// How authorization was resolved for a `service_status` request.
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
/// for local audit logging and Activity entries.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub caller: String,
    pub host: String,
    pub service: String,
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

    /// Wall-clock duration before an unanswered Agent Decision expires with `timed_out`.
    fn agent_decision_timeout(&self) -> Duration {
        DEFAULT_DECISION_TIMEOUT
    }

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

    /// Triggers a non-focus-stealing attention cue when a new Agent Decision is enqueued while
    /// tern is not the active app.
    fn notify_agent_attention(&mut self, _cue: &AttentionCue, _cx: &mut Context<Self>) {}

    /// Hook called when a `service_status` request finishes for local audit logging and Activity.
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
            wants_progress,
        } => {
            handle_service_status(
                &host,
                ServiceStatusCall {
                    caller,
                    target_host,
                    service,
                    timeout_secs,
                    wants_progress,
                },
                &responder,
                cx,
            )
            .await
        }
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

struct ServiceStatusCall {
    caller: Option<String>,
    target_host: String,
    service: String,
    timeout_secs: Option<u64>,
    wants_progress: bool,
}

struct ServiceRequest<'a, H> {
    host: &'a WeakEntity<H>,
    caller: String,
    target_host: String,
    service: String,
}

impl<H: AgentHost> ServiceRequest<'_, H> {
    fn report(&self, decision: AgentDecision, result: &BrokerResult, cx: &mut AsyncApp) {
        let outcome = AgentOutcome {
            caller: self.caller.clone(),
            host: self.target_host.clone(),
            service: self.service.clone(),
            decision,
            result: result.clone(),
        };
        let _ = self
            .host
            .update(cx, |h, cx| h.on_agent_outcome(&outcome, cx));
    }

    fn fail(
        &self,
        decision: AgentDecision,
        code: AgentErrorCode,
        message: impl Into<String>,
        cx: &mut AsyncApp,
    ) -> BrokerResult {
        let res = BrokerResult::Error(AgentError::new(code, message));
        self.report(decision, &res, cx);
        res
    }
}

async fn handle_service_status<H: AgentHost>(
    host: &WeakEntity<H>,
    call: ServiceStatusCall,
    responder: &Responder,
    cx: &mut AsyncApp,
) -> BrokerResult {
    let ServiceStatusCall {
        caller,
        target_host,
        service,
        timeout_secs,
        wants_progress,
    } = call;
    let req = ServiceRequest {
        host,
        caller: caller_display(caller.as_deref()),
        target_host,
        service,
    };

    if responder.is_cancelled() {
        return req.fail(
            AgentDecision::Cancelled,
            AgentErrorCode::Denied,
            "request cancelled by agent",
            cx,
        );
    }

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
            return req.fail(AgentDecision::Denied, AgentErrorCode::Denied, msg, cx);
        }
        Precheck::InvalidUnit(msg) => {
            return req.fail(AgentDecision::Denied, AgentErrorCode::Denied, msg, cx);
        }
        Precheck::Granted {
            snapshot,
            conn,
            command,
        } => {
            if responder.is_cancelled() {
                return req.fail(
                    AgentDecision::Cancelled,
                    AgentErrorCode::Denied,
                    "request cancelled by agent",
                    cx,
                );
            }
            let res = execute_service_status(
                &conn,
                &snapshot,
                &req.service,
                &command,
                timeout_secs,
                responder,
                cx,
            )
            .await;
            req.report(AgentDecision::GrantReused, &res, cx);
            return res;
        }
        Precheck::NeedsDecision {
            conn,
            command,
            fingerprint,
        } => (conn, command, fingerprint),
    };

    // Ask explicit human approval via the click-only Decision queue with a 60 s default deadline.
    let prompt = build_service_status_prompt(&req.caller, &conn, &command);
    let cue = AttentionCue::for_service_status(&req.caller, &conn.name, &req.service);
    let pending = match host.update(cx, |h, cx| {
        let deadline = Instant::now() + h.agent_decision_timeout();
        let pending = h.ask_agent_decision(prompt, Some(deadline), cx)?;
        h.notify_agent_attention(&cue, cx);
        Ok::<_, QueueFull>(pending)
    }) {
        Ok(Ok(pending)) => pending,
        Ok(Err(QueueFull)) => {
            return req.fail(
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

    let (_, waiter, decision_cancel) = pending.into_parts();
    let rt = cx.update(|cx| SshRuntime::handle(cx));
    let mut cancel_rx = responder.cancel_receiver();
    let cancel_bridge = AbortOnDrop(rt.spawn(async move {
        if cancel_rx.wait_for(|&c| c).await.is_ok() {
            decision_cancel.cancel();
        }
    }));

    if wants_progress && !responder.is_cancelled() {
        responder.send_progress(WAITING_FOR_APPROVAL_MESSAGE).await;
    }

    let outcome = waiter.wait().await;
    drop(cancel_bridge);

    let decision = match outcome {
        DecisionOutcome::Chosen(choice) if choice == CHOICE_ALLOW_ONCE => AgentDecision::AllowOnce,
        DecisionOutcome::Chosen(choice) if choice == CHOICE_ALLOW_UNTIL_LOCK => {
            AgentDecision::AllowUntilLock
        }
        DecisionOutcome::Expired => {
            return req.fail(
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
                    AgentDecision::Paused,
                    AgentErrorCode::AgentsPaused,
                    "agents are paused in tern",
                    cx,
                );
            }
            let message = if responder.is_cancelled() {
                "request cancelled by agent"
            } else {
                "request cancelled in tern"
            };
            return req.fail(
                AgentDecision::Cancelled,
                AgentErrorCode::Denied,
                message,
                cx,
            );
        }
        DecisionOutcome::Chosen(_) => {
            return req.fail(
                AgentDecision::Denied,
                AgentErrorCode::Denied,
                "request denied in tern",
                cx,
            );
        }
    };

    // Guard against a click racing with cancellation right before SSH exec is dispatched:
    // never record a Grant or dial SSH if the caller already cancelled or disconnected.
    if responder.is_cancelled() {
        return req.fail(
            AgentDecision::Cancelled,
            AgentErrorCode::Denied,
            "request cancelled by agent",
            cx,
        );
    }

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
                AgentDecision::Paused,
                AgentErrorCode::AgentsPaused,
                "agents are paused in tern",
                cx,
            );
        }
        Postcheck::NotAllowed => {
            return req.fail(
                AgentDecision::Denied,
                AgentErrorCode::Denied,
                "host or service is no longer allowed in tern",
                cx,
            );
        }
        Postcheck::Changed => {
            return req.fail(
                AgentDecision::Denied,
                AgentErrorCode::Denied,
                "saved connection changed before approval completed",
                cx,
            );
        }
        Postcheck::Ready(ready) => *ready,
    };

    let res = execute_service_status(
        &conn,
        &fresh,
        &req.service,
        &command,
        timeout_secs,
        responder,
        cx,
    )
    .await;
    req.report(decision, &res, cx);
    res
}
