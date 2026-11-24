//! Wiring between [`Shell`] and the local Agent broker ([`crate::agents::broker`]).
//!
//! Keeps Agent broker lifecycle, `agents.json` state, in-memory Grants, `Pause agents` kill
//! switch, `lock_on_sleep` Grant/Decision clearing, and [`AgentHost`] implementation out of
//! `shell.rs` so `shell.rs` stays below the 800-line ceiling.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::Utc;
use gpui::{Context, WeakEntity};

use super::Shell;
use crate::agents::attention::{self, AgentAttention, AttentionCue};
use crate::agents::audit::{self, AuditRetention};
use crate::agents::broker::{
    AgentHost, AgentOutcome, BrokerHandle, BrokerSnapshot, DEFAULT_DECISION_TIMEOUT,
};
use crate::agents::grants::{Grant, GrantFingerprint, GrantStore};
use crate::agents::socket_path;
use crate::agents::store::{self, AgentConfig, StoreError};
use crate::connections::Connection;
use crate::decisions::{DecisionPrompt, DecisionTag, PendingDecision, QueueFull};
use crate::lock_on_sleep::Cause;
use crate::settings;

/// Local Agent configuration, in-memory Grants, and active `agent.sock` listener owned by [`Shell`].
#[derive(Debug)]
pub(super) struct AgentsUi {
    pub(super) config: AgentConfig,
    /// Set when `agents.json` exists but cannot be read; saving is refused so it is not clobbered.
    pub(super) store_error: Option<String>,
    /// Active in-memory Grants; never persisted to `agents.json`, disk, or `tern-sync`.
    grants: GrantStore,
    /// Wall-clock deadline for unanswered Agent Decisions (60 s in production; overridable in tests).
    decision_timeout: Duration,
    /// Non-focus-stealing background attention cue boundary (Dock bounce + macOS notification).
    attention: Arc<dyn AgentAttention>,
    /// Bounded size/backup retention policy for local `agent-audit.jsonl`.
    audit_retention: AuditRetention,
    broker: Option<BrokerHandle>,
}

impl Default for AgentsUi {
    fn default() -> Self {
        Self {
            config: AgentConfig::default(),
            store_error: None,
            grants: GrantStore::default(),
            decision_timeout: DEFAULT_DECISION_TIMEOUT,
            attention: attention::default_attention(),
            audit_retention: AuditRetention::default(),
            broker: None,
        }
    }
}

/// Drops any host keys no longer in `connections`, persists the pruned config to `agents.json`
/// when `dir` is available, and returns the pruned [`AgentConfig`] so callers never keep stale
/// host entries in memory.
fn persist_config(
    dir: Option<&Path>,
    config: &AgentConfig,
    connections: &[Connection],
    store_error: Option<&str>,
) -> Result<AgentConfig, StoreError> {
    if let Some(err) = store_error {
        return Err(StoreError::Unreadable(err.to_owned()));
    }
    let mut clean = config.clone();
    clean.prune_unknown_hosts(connections);
    if let Some(dir) = dir {
        store::save(dir, &clean, connections)?;
    }
    Ok(clean)
}

impl AgentsUi {
    /// Loads `agents.json`, prunes any hosts removed from Saved connections, and starts the local
    /// `agent.sock` broker when `enabled = true`.
    ///
    /// # Errors
    /// Returns a user-facing error message if `enabled = true` and binding `agent.sock` fails.
    fn load_and_start<H: AgentHost>(
        &mut self,
        dir: &Path,
        connections: &[Connection],
        hosts_store_error: bool,
        host: WeakEntity<H>,
        cx: &mut Context<H>,
    ) -> Result<(), String> {
        match store::load(dir) {
            Ok(config) => {
                self.config = config;
                self.store_error = None;
                self.prune_orphaned_hosts(Some(dir), connections, hosts_store_error);
                if self.config.enabled && self.broker.is_none() {
                    let sock = socket_path(dir);
                    match BrokerHandle::start(sock, host, cx) {
                        Ok(handle) => self.broker = Some(handle),
                        Err(e) => {
                            tracing::warn!(error = %e, "agent_broker_start_failed");
                            // Reflect the actual non-listening state in memory without clobbering
                            // `agents.json` so toggling Enable MCP retries binding cleanly.
                            self.config.enabled = false;
                            return Err(format!("Could not start local Agent broker: {e}"));
                        }
                    }
                }
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = %e, "agents_json_unreadable");
                self.store_error = Some(e.to_string());
                self.broker = None;
                Ok(())
            }
        }
    }

    /// Drops any allowlisted host entries whose Saved connection no longer exists and persists the
    /// pruned `agents.json` when both `hosts.json` and `agents.json` are readable.
    fn prune_orphaned_hosts(
        &mut self,
        dir: Option<&Path>,
        connections: &[Connection],
        hosts_store_error: bool,
    ) -> usize {
        if hosts_store_error {
            return 0;
        }
        let removed = self.config.prune_unknown_hosts(connections);
        if removed > 0 {
            if self.store_error.is_none()
                && let Some(dir) = dir
                && let Err(e) = persist_config(Some(dir), &self.config, connections, None)
            {
                tracing::warn!(error = %e, removed, "agent_orphaned_hosts_save_failed");
            }
            tracing::info!(removed, "agent_orphaned_hosts_pruned");
        }
        removed
    }

    /// Enables or disables local MCP Agent access. When enabling, binds `agent.sock` before
    /// persisting `enabled = true` and rolls back on failure so `enabled` never pretends the
    /// broker is listening. Returns `Ok(true)` when disabled so the caller can cancel pending
    /// Agent Decisions.
    ///
    /// # Errors
    /// Returns a user-facing error message if `agents.json` is unreadable, binding `agent.sock`
    /// fails, or saving `agents.json` fails.
    fn set_enabled<H: AgentHost>(
        &mut self,
        dir: Option<&Path>,
        enabled: bool,
        connections: &[Connection],
        host: WeakEntity<H>,
        cx: &mut Context<H>,
    ) -> Result<bool, String> {
        let mut next = self.config.clone();
        next.enabled = enabled;
        if enabled {
            if let Some(err) = &self.store_error {
                return Err(StoreError::Unreadable(err.clone()).to_string());
            }
            if self.broker.is_none() {
                let Some(dir_path) = dir else {
                    return Err("settings directory is unavailable".to_owned());
                };
                let handle = BrokerHandle::start(socket_path(dir_path), host, cx).map_err(|e| {
                    tracing::warn!(error = %e, "agent_broker_start_failed");
                    format!("Could not start local Agent broker: {e}")
                })?;
                match persist_config(dir, &next, connections, self.store_error.as_deref()) {
                    Ok(clean) => {
                        self.config = clean;
                        self.broker = Some(handle);
                        return Ok(false);
                    }
                    Err(e) => {
                        drop(handle);
                        return Err(e.to_string());
                    }
                }
            }
            let clean = persist_config(dir, &next, connections, self.store_error.as_deref())
                .map_err(|e| e.to_string())?;
            self.config = clean;
            Ok(false)
        } else {
            let clean = persist_config(dir, &next, connections, self.store_error.as_deref())
                .map_err(|e| e.to_string())?;
            self.config = clean;
            self.broker = None;
            self.grants.revoke_all();
            Ok(true)
        }
    }

    fn set_paused(
        &mut self,
        dir: Option<&Path>,
        paused: bool,
        connections: &[Connection],
    ) -> Result<usize, StoreError> {
        let mut next = self.config.clone();
        next.paused = paused;
        let clean = persist_config(dir, &next, connections, self.store_error.as_deref())?;
        self.config = clean;
        Ok(if paused { self.grants.revoke_all() } else { 0 })
    }

    fn set_allowed_services(
        &mut self,
        dir: Option<&Path>,
        host: &str,
        services: Vec<String>,
        connections: &[Connection],
    ) -> Result<(), StoreError> {
        let mut next = self.config.clone();
        next.set_allowed_services(host, services, connections)?;
        let clean = persist_config(dir, &next, connections, self.store_error.as_deref())?;
        self.config = clean;
        self.grants.prune_stale(&self.config, connections);
        Ok(())
    }
}

impl Shell {
    /// Loads `agents.json` and starts the local `agent.sock` broker only when MCP is enabled.
    pub(super) fn start_agents(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = settings::dir() else {
            return;
        };
        if let Err(msg) = self.agents.load_and_start(
            &dir,
            &self.connections,
            self.store_error.is_some(),
            cx.entity().downgrade(),
            cx,
        ) {
            self.alert_failed(msg, cx);
        }
    }

    /// Whether local MCP Agent access is enabled in `agents.json`.
    #[must_use]
    pub(crate) fn agents_enabled(&self) -> bool {
        self.agents.config.enabled
    }

    /// Whether the `Pause agents` kill switch is active.
    #[must_use]
    pub(crate) fn agents_paused(&self) -> bool {
        self.agents.config.paused
    }

    /// Error recorded when `agents.json` could not be loaded (corrupt or newer schema).
    #[must_use]
    pub(crate) fn agent_store_error(&self) -> Option<&str> {
        self.agents.store_error.as_deref()
    }

    /// Configured allowed systemd service unit names for Saved connection `host`.
    #[must_use]
    pub(crate) fn agent_allowed_services(&self, host: &str) -> Vec<String> {
        self.agents
            .config
            .hosts
            .get(host)
            .cloned()
            .unwrap_or_default()
    }

    /// Saved connections that currently have at least one valid allowed service unit.
    #[must_use]
    pub(crate) fn agent_allowed_hosts(&self) -> Vec<(String, Vec<String>)> {
        self.agents.config.allowed_hosts(&self.connections)
    }

    /// Read-only snapshot of active in-memory Grants for Settings → Agents.
    #[must_use]
    pub(crate) fn active_agent_grants(&self) -> Vec<Grant> {
        self.agents.grants.active()
    }

    /// Enables or disables local MCP Agent access, starting or stopping `agent.sock`, clearing
    /// active Grants and pending Agent Decisions when disabled, and persisting `agents.json`.
    ///
    /// # Errors
    /// Returns an error message if `agents.json` cannot be saved (or is unreadable on disk) or
    /// binding `agent.sock` fails.
    pub(crate) fn set_agents_enabled(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let dir = settings::dir();
        let disabled = self.agents.set_enabled(
            dir.as_deref(),
            enabled,
            &self.connections,
            cx.entity().downgrade(),
            cx,
        )?;
        if disabled {
            self.cancel_decisions_for(&DecisionTag::Agent, cx);
        }
        cx.notify();
        Ok(())
    }

    /// Enables or disables the `Pause agents` kill switch and persists `paused` in `agents.json`
    /// while keeping allowed services intact.
    ///
    /// Enabling immediately clears all active Grants and cancels all pending Agent Decisions
    /// (leaving clipboard Decisions untouched). Disabling resumes normal Decision handling without
    /// restoring previously cleared Grants.
    ///
    /// # Errors
    /// Returns [`StoreError`] if `agents.json` cannot be saved (or is unreadable on disk).
    pub(crate) fn set_agents_paused(
        &mut self,
        paused: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), StoreError> {
        let dir = settings::dir();
        let grants = self
            .agents
            .set_paused(dir.as_deref(), paused, &self.connections)?;
        if paused {
            let decisions = self.cancel_decisions_for(&DecisionTag::Agent, cx);
            tracing::info!(grants, decisions, "agents_paused");
        } else {
            tracing::info!("agents_resumed");
        }
        cx.notify();
        Ok(())
    }

    /// Replaces the allowlisted systemd service units for Saved connection `host`, immediately
    /// invalidates any affected Grants, and persists `agents.json`.
    ///
    /// # Errors
    /// Returns [`StoreError`] if `host` is not a Saved connection, any unit name is invalid, or
    /// `agents.json` cannot be saved.
    pub(crate) fn set_agent_allowed_services(
        &mut self,
        host: &str,
        services: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Result<(), StoreError> {
        let dir = settings::dir();
        self.agents
            .set_allowed_services(dir.as_deref(), host, services, &self.connections)?;
        cx.notify();
        Ok(())
    }

    /// Clears every active in-memory Grant and cancels all pending Agent Decisions while keeping
    /// `agents.json` allowlists and clipboard Decisions intact. Returns how many Grants were
    /// revoked.
    pub(crate) fn revoke_all_agent_grants(&mut self, cx: &mut Context<Self>) -> usize {
        let grants = self.agents.grants.revoke_all();
        let decisions = self.cancel_decisions_for(&DecisionTag::Agent, cx);
        if grants > 0 || decisions > 0 {
            tracing::info!(grants, decisions, "agent_grants_revoked");
        }
        cx.notify();
        grants
    }

    /// Revokes one active in-memory Grant by `(host, service)` while keeping `agents.json` intact.
    pub(crate) fn revoke_agent_grant(
        &mut self,
        host: &str,
        service: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let removed = self.agents.grants.revoke(host, service);
        if removed {
            tracing::info!(host, service, "agent_grant_revoked");
            cx.notify();
        }
        removed
    }

    /// Drops any allowlisted host entries whose Saved connection was deleted or renamed and
    /// persists the pruned `agents.json`.
    pub(super) fn prune_orphaned_agent_hosts(&mut self) -> usize {
        let dir = settings::dir();
        self.agents.prune_orphaned_hosts(
            dir.as_deref(),
            &self.connections,
            self.store_error.is_some(),
        )
    }

    /// Drops any active Grants whose Saved connection or allowlisted service changed.
    pub(super) fn prune_stale_agent_grants(&mut self) -> usize {
        let removed = self
            .agents
            .grants
            .prune_stale(&self.agents.config, &self.connections);
        if removed > 0 {
            tracing::info!(removed, "agent_grants_invalidated");
        }
        removed
    }

    /// Handles `lock_on_sleep` signals (`Sleep`, `ScreenLocked`, `SessionSwitched`): locks the
    /// vault if `settings.vault_lock_on_sleep` is on, and **unconditionally** clears every active
    /// Agent Grant and cancels every pending Agent Decision while keeping clipboard Decisions and
    /// `agents.json` intact.
    pub(crate) fn on_sleep_or_lock(&mut self, cause: Cause, cx: &mut Context<Self>) {
        self.lock_vault_for(cause, cx);
        let grants = self.agents.grants.revoke_all();
        let decisions = self.cancel_decisions_for(&DecisionTag::Agent, cx);
        if grants > 0 || decisions > 0 {
            tracing::info!(?cause, grants, decisions, "agent_grants_cleared_on_lock");
        }
        cx.notify();
    }
}

impl AgentHost for Shell {
    fn agent_snapshot(&self) -> BrokerSnapshot {
        BrokerSnapshot {
            dir: settings::dir().unwrap_or_default(),
            known_hosts: None,
            config: self.agents.config.clone(),
            connections: self.connections.clone(),
        }
    }

    fn verify_agent_grant(
        &mut self,
        host: &str,
        service: &str,
        fingerprint: GrantFingerprint,
        cx: &mut Context<Self>,
    ) -> bool {
        let valid = self.agents.grants.verify(host, service, fingerprint);
        if !valid {
            cx.notify();
        }
        valid
    }

    fn record_agent_grant(
        &mut self,
        host: &str,
        service: &str,
        fingerprint: GrantFingerprint,
        cx: &mut Context<Self>,
    ) {
        self.agents
            .grants
            .insert(host, service, fingerprint, SystemTime::now());
        cx.notify();
    }

    fn agent_decision_timeout(&self) -> Duration {
        self.agents.decision_timeout
    }

    fn ask_agent_decision(
        &mut self,
        prompt: DecisionPrompt,
        deadline: Option<Instant>,
        cx: &mut Context<Self>,
    ) -> Result<PendingDecision, QueueFull> {
        self.ask_decision(DecisionTag::Agent, prompt, deadline, cx)
    }

    fn notify_agent_attention(&mut self, cue: &AttentionCue, _cx: &mut Context<Self>) {
        attention::notify_if_inactive(self.agents.attention.as_ref(), cue);
    }

    fn on_agent_outcome(&mut self, outcome: &AgentOutcome, cx: &mut Context<Self>) {
        let dir = settings::dir();
        let text = audit::record_outcome(
            dir.as_deref(),
            self.agents.audit_retention,
            outcome,
            Utc::now(),
        );
        self.activity(text, cx);
    }
}

#[cfg(test)]
#[path = "shell_agents_tests.rs"]
mod tests;
