//! Local Unix socket broker (`agent.sock`, mode `0o600`) and request orchestration (ADR 0003,
//! ADR 0004).
//!
//! Binds `agent.sock` on [`SshRuntime`] only when Agents are enabled, leaving `tern.sock`
//! untouched. Exposes all **Saved connections** (`hosts.json`) to Agents, gates ungranted
//! `run_command` requests behind a click-only [`DecisionTag::Agent`] Decision with per-Host
//! in-memory **Grants**, and executes commands through [`super::headless_login`].

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{AsyncApp, Context, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::attention::AttentionCue;
use super::broker_exec::{AbortOnDrop, caller_display, execute_with_vault_wait};
pub use super::broker_exec::{build_run_command_prompt, format_activity, summarize_command};
use super::broker_socket::{IncomingRequest, Responder, bind_and_serve};
use super::grants::GrantFingerprint;
use super::protocol::{
    AgentError, AgentErrorCode, BrokerRequest, BrokerResult, HostSummary, ListHostsResponse,
    MAX_COMMAND_BYTES, TimeoutPhase,
};
use super::store::AgentConfig;
use super::vault_wait::PendingVaultUnlock;
use crate::connections::Connection;
use crate::decisions::{
    DecisionOutcome, DecisionPrompt, PendingDecision, QueueFull, sanitize_display_text,
};
use crate::platform::SshRuntime;

/// Decision button identifier for one-time execution approval.
pub const CHOICE_ALLOW_ONCE: &str = "allow_once";
/// Decision button identifier for creating a per-Host in-memory Grant until lock/sleep/quit/revoke.
pub const CHOICE_ALLOW_UNTIL_LOCK: &str = "allow_until_lock";
/// Decision button identifier for declining the request.
pub const CHOICE_DENY: &str = "deny";
/// Default wall-clock deadline before an unanswered Agent Decision expires with `timed_out`.
pub const DEFAULT_DECISION_TIMEOUT: Duration = Duration::from_secs(60);

/// Progress message sent while an Agent request is waiting for human approval in tern.
pub const WAITING_FOR_APPROVAL_MESSAGE: &str = "Waiting for approval in tern...";
/// Progress message sent while an approved Agent request is waiting for Vault unlock in tern.
pub const WAITING_FOR_VAULT_UNLOCK_MESSAGE: &str = "Waiting for vault unlock in tern...";

const LIST_HOSTS_USAGE: &str = "Call run_command with an alias from hosts and a shell command or script (up to 64 KiB). Optional timeout_secs defaults to 30 (clamped to 1..=300).";
const MAX_PENDING_REQUESTS: usize = 32;

/// Point-in-time configuration and Saved connections read from the host entity on the UI thread.
#[derive(Debug, Clone)]
pub struct BrokerSnapshot {
    pub dir: PathBuf,
    pub known_hosts: Option<PathBuf>,
    pub config: AgentConfig,
    pub connections: Vec<Connection>,
}

/// How authorization was resolved for a `run_command` request.
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

/// Summary of a completed `run_command` request, passed to [`AgentHost::on_agent_outcome`]
/// for quiet **Activity** entries.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub caller: String,
    pub host: String,
    pub decision: AgentDecision,
    pub result: BrokerResult,
}

/// Host UI seam implemented by `Shell` (and test hosts in `gpui::TestApp`).
pub trait AgentHost: Sized + 'static {
    fn agent_snapshot(&self) -> BrokerSnapshot;

    /// Returns `true` when an in-memory Grant exists for `host` whose stored fingerprint matches
    /// `fingerprint`, evicting any mismatched Grant immediately.
    fn verify_agent_grant(
        &mut self,
        host: &str,
        fingerprint: GrantFingerprint,
        cx: &mut Context<Self>,
    ) -> bool;

    /// Records an in-memory-only Grant for `host` bound to `fingerprint`.
    fn record_agent_grant(
        &mut self,
        host: &str,
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

    /// Registers a bounded wait for the human to unlock the Vault in tern so an approved Agent
    /// request can resume once unlocked. Returns `None` when interactive waiting is unavailable
    /// (causing the broker to fail closed immediately with `vault_locked`).
    fn wait_for_vault_unlock(
        &mut self,
        _caller: &str,
        _host: &str,
        _deadline: Instant,
        _cx: &mut Context<Self>,
    ) -> Option<PendingVaultUnlock> {
        None
    }

    /// Hook called when a `run_command` request finishes for quiet **Activity** reporting.
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
        BrokerRequest::ListHosts { query, .. } => handle_list_hosts(&host, query.as_deref(), cx),
        BrokerRequest::RunCommand {
            caller,
            host: target_host,
            command,
            timeout_secs,
            wants_progress,
        } => {
            handle_run_command(
                &host,
                RunCommandCall {
                    caller,
                    target_host,
                    command,
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

fn match_host_query(conn: &Connection, query: &str, query_lower: &str) -> Option<u8> {
    if conn.name == query {
        return Some(0);
    }
    let alias_lower = conn.name.to_lowercase();
    if alias_lower == query_lower {
        return Some(1);
    }
    if alias_lower.starts_with(query_lower) {
        return Some(2);
    }
    if alias_lower.contains(query_lower) {
        return Some(3);
    }
    if conn.host.to_lowercase().contains(query_lower)
        || conn.user.to_lowercase().contains(query_lower)
    {
        return Some(4);
    }
    None
}

fn handle_list_hosts<H: AgentHost>(
    host: &WeakEntity<H>,
    query: Option<&str>,
    cx: &mut AsyncApp,
) -> BrokerResult {
    let trimmed = query.map(str::trim).filter(|q| !q.is_empty());
    let Ok(outcome) = host.update(cx, |h, cx| {
        let snapshot = h.agent_snapshot();
        if snapshot.config.paused {
            return Err(AgentError::new(
                AgentErrorCode::AgentsPaused,
                "agents are paused in tern",
            ));
        }

        let selected: Vec<&Connection> = match trimmed {
            None => snapshot.connections.iter().collect(),
            Some(q) => {
                let q_lower = q.to_lowercase();
                let mut ranked: Vec<(u8, &Connection)> = snapshot
                    .connections
                    .iter()
                    .filter_map(|c| match_host_query(c, q, &q_lower).map(|rank| (rank, c)))
                    .collect();
                ranked.sort_by_key(|(rank, _)| *rank);
                ranked.into_iter().map(|(_, c)| c).collect()
            }
        };

        let hosts = selected
            .into_iter()
            .map(|c| {
                let fingerprint = GrantFingerprint::for_connection(c, &snapshot.connections);
                let grant_active = h.verify_agent_grant(&c.name, fingerprint, cx);
                HostSummary {
                    alias: c.name.clone(),
                    host: c.host.clone(),
                    user: c.user.clone(),
                    port: c.port,
                    grant_active,
                }
            })
            .collect();
        Ok(ListHostsResponse {
            hosts,
            usage: LIST_HOSTS_USAGE.to_owned(),
        })
    }) else {
        return BrokerResult::Error(AgentError::new(
            AgentErrorCode::TernNotRunning,
            "tern window is no longer available",
        ));
    };

    match outcome {
        Ok(payload) => match serde_json::to_value(payload) {
            Ok(data) => BrokerResult::Ok { data },
            Err(e) => BrokerResult::Error(AgentError::new(AgentErrorCode::SshError, e.to_string())),
        },
        Err(err) => BrokerResult::Error(err),
    }
}

pub(super) fn saved_connection(snapshot: &BrokerSnapshot, host: &str) -> Option<Connection> {
    snapshot
        .connections
        .iter()
        .find(|c| c.name == host)
        .cloned()
}

enum Precheck {
    Paused,
    UnknownHost,
    InvalidCommand(String),
    Granted {
        snapshot: BrokerSnapshot,
        conn: Connection,
    },
    NeedsDecision {
        conn: Connection,
        fingerprint: GrantFingerprint,
    },
}

pub(super) enum Postcheck {
    Paused,
    UnknownHost,
    Changed,
    Ready(Box<(BrokerSnapshot, Connection)>),
}

struct RunCommandCall {
    caller: Option<String>,
    target_host: String,
    command: String,
    timeout_secs: Option<u64>,
    wants_progress: bool,
}

pub(super) struct CommandRequest<'a, H> {
    pub(super) host: &'a WeakEntity<H>,
    pub(super) caller: String,
    pub(super) target_host: String,
}

impl<H: AgentHost> CommandRequest<'_, H> {
    fn report(&self, decision: AgentDecision, result: &BrokerResult, cx: &mut AsyncApp) {
        let outcome = AgentOutcome {
            caller: self.caller.clone(),
            host: self.target_host.clone(),
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

fn validate_command(command: &str) -> Result<(), String> {
    if command.trim().is_empty() {
        return Err("command must not be empty".to_owned());
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err(format!(
            "command exceeded maximum size of {MAX_COMMAND_BYTES} bytes"
        ));
    }
    Ok(())
}

async fn handle_run_command<H: AgentHost>(
    host: &WeakEntity<H>,
    call: RunCommandCall,
    responder: &Responder,
    cx: &mut AsyncApp,
) -> BrokerResult {
    let RunCommandCall {
        caller,
        target_host,
        command,
        timeout_secs,
        wants_progress,
    } = call;
    let req = CommandRequest {
        host,
        caller: caller_display(caller.as_deref()),
        target_host,
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
        if let Err(msg) = validate_command(&command) {
            return Precheck::InvalidCommand(msg);
        }
        let Some(conn) = saved_connection(&snapshot, &req.target_host) else {
            return Precheck::UnknownHost;
        };
        let fingerprint = GrantFingerprint::for_connection(&conn, &snapshot.connections);
        if h.verify_agent_grant(&req.target_host, fingerprint, cx) {
            Precheck::Granted { snapshot, conn }
        } else {
            Precheck::NeedsDecision { conn, fingerprint }
        }
    }) else {
        return BrokerResult::Error(AgentError::new(
            AgentErrorCode::TernNotRunning,
            "tern window is no longer available",
        ));
    };

    let (conn, fingerprint) = match precheck {
        Precheck::Paused => {
            return req.fail(
                AgentDecision::Paused,
                AgentErrorCode::AgentsPaused,
                "agents are paused in tern",
                cx,
            );
        }
        Precheck::InvalidCommand(msg) => {
            return req.fail(AgentDecision::Denied, AgentErrorCode::Denied, msg, cx);
        }
        Precheck::UnknownHost => {
            let msg = format!(
                "host '{}' is not a saved connection; call list_hosts to see available hosts",
                sanitize_display_text(&req.target_host),
            );
            return req.fail(AgentDecision::Denied, AgentErrorCode::Denied, msg, cx);
        }
        Precheck::Granted { snapshot, conn } => {
            if responder.is_cancelled() {
                return req.fail(
                    AgentDecision::Cancelled,
                    AgentErrorCode::Denied,
                    "request cancelled by agent",
                    cx,
                );
            }
            let res = execute_with_vault_wait(
                &req,
                &conn,
                &snapshot,
                &command,
                timeout_secs,
                wants_progress,
                responder,
                cx,
            )
            .await;
            req.report(AgentDecision::GrantReused, &res, cx);
            return res;
        }
        Precheck::NeedsDecision { conn, fingerprint } => (conn, fingerprint),
    };

    // Ask explicit human approval via the click-only Decision queue with a 60 s default deadline.
    let prompt = build_run_command_prompt(&req.caller, &conn, &command);
    let cue = AttentionCue::for_run_command(&req.caller, &conn.name, &summarize_command(&command));
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
            let res = BrokerResult::Error(AgentError::timed_out(
                TimeoutPhase::NotDispatched,
                "approval decision expired",
            ));
            req.report(AgentDecision::Expired, &res, cx);
            return res;
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

    // Re-read current Saved connections and live fingerprint before dialing or recording
    // a Grant in case settings changed while the Decision was open.
    let Ok(postcheck) = host.update(cx, |h, cx| {
        let fresh = h.agent_snapshot();
        if fresh.config.paused {
            return Postcheck::Paused;
        }
        let Some(conn) = saved_connection(&fresh, &req.target_host) else {
            return Postcheck::UnknownHost;
        };
        let fresh_fp = GrantFingerprint::for_connection(&conn, &fresh.connections);
        if fresh_fp != fingerprint {
            return Postcheck::Changed;
        }
        if decision == AgentDecision::AllowUntilLock {
            h.record_agent_grant(&req.target_host, fresh_fp, cx);
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
        Postcheck::UnknownHost => {
            return req.fail(
                AgentDecision::Denied,
                AgentErrorCode::Denied,
                "saved connection is no longer available in tern",
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

    let res = execute_with_vault_wait(
        &req,
        &conn,
        &fresh,
        &command,
        timeout_secs,
        wants_progress,
        responder,
        cx,
    )
    .await;
    req.report(decision, &res, cx);
    res
}
