//! Wiring between [`Shell`] and the local Agent broker ([`crate::agents::broker`]).
//!
//! Keeps Agent broker lifecycle, `agents.json` state, in-memory Grants, `Pause agents` kill
//! switch, `lock_on_sleep` Grant/Decision clearing, and [`AgentHost`] implementation out of
//! `shell.rs` so `shell.rs` stays below the 800-line ceiling.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::Utc;
use gpui::Context;

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

/// Persists `config` to `agents.json` after dropping any host keys no longer in `connections` so
/// deleting a Saved connection never blocks saving `enabled` or `paused`.
fn persist_config(
    dir: &Path,
    config: &AgentConfig,
    connections: &[Connection],
    store_error: Option<&str>,
) -> Result<(), StoreError> {
    if let Some(err) = store_error {
        return Err(StoreError::Unreadable(err.to_owned()));
    }
    let hosts: BTreeMap<String, Vec<String>> = config
        .hosts
        .iter()
        .filter(|(host, _)| connections.iter().any(|c| c.name == **host))
        .map(|(h, svcs)| (h.clone(), svcs.clone()))
        .collect();
    let clean = AgentConfig {
        enabled: config.enabled,
        paused: config.paused,
        hosts,
    };
    store::save(dir, &clean, connections)
}

impl Shell {
    /// Loads `agents.json` and starts the local `agent.sock` broker only when MCP is enabled.
    pub(super) fn start_agents(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = settings::dir() else {
            return;
        };
        match store::load(&dir) {
            Ok(config) => {
                self.agents.config = config;
                self.agents.store_error = None;
                self.reconcile_agent_broker(cx);
            }
            Err(e) => {
                tracing::warn!(error = %e, "agents_json_unreadable");
                self.agents.store_error = Some(e.to_string());
                self.agents.broker = None;
            }
        }
    }

    /// Ensures `agent.sock` is bound if and only if `agents.json` has `enabled = true`.
    pub(super) fn reconcile_agent_broker(&mut self, cx: &mut Context<Self>) {
        if !self.agents.config.enabled {
            self.agents.broker = None;
            self.agents.grants.revoke_all();
            self.cancel_decisions_for(&DecisionTag::Agent, cx);
            return;
        }
        if self.agents.broker.is_some() {
            return;
        }
        let Some(dir) = settings::dir() else {
            return;
        };
        let sock = socket_path(&dir);
        match BrokerHandle::start(sock, cx.entity().downgrade(), cx) {
            Ok(handle) => self.agents.broker = Some(handle),
            Err(e) => tracing::warn!(error = %e, "agent_broker_start_failed"),
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
    /// Returns [`StoreError`] if `agents.json` cannot be saved (or is unreadable on disk).
    pub(crate) fn set_agents_enabled(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), StoreError> {
        if let Some(err) = &self.agents.store_error {
            return Err(StoreError::Unreadable(err.clone()));
        }
        let mut next = self.agents.config.clone();
        next.enabled = enabled;
        if let Some(dir) = settings::dir() {
            persist_config(&dir, &next, &self.connections, None)?;
        }
        self.agents.config = next;
        self.reconcile_agent_broker(cx);
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
        if let Some(err) = &self.agents.store_error {
            return Err(StoreError::Unreadable(err.clone()));
        }
        let mut next = self.agents.config.clone();
        next.paused = paused;
        if let Some(dir) = settings::dir() {
            persist_config(&dir, &next, &self.connections, None)?;
        }
        self.agents.config = next;
        if paused {
            let grants = self.agents.grants.revoke_all();
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
        let mut next = self.agents.config.clone();
        next.set_allowed_services(host, services, &self.connections)?;
        if let Some(dir) = settings::dir() {
            persist_config(
                &dir,
                &next,
                &self.connections,
                self.agents.store_error.as_deref(),
            )?;
        }
        self.agents.config = next;
        self.prune_stale_agent_grants();
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
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn conn(name: &str) -> Connection {
        Connection {
            name: name.into(),
            host: "10.0.0.1".into(),
            port: 22,
            user: "deploy".into(),
            ..Connection::default()
        }
    }

    #[test]
    fn persist_config_drops_orphaned_deleted_hosts_and_refuses_when_store_error_set() {
        let dir = tempfile::tempdir().unwrap();
        let connections = vec![conn("staging")];
        let mut cfg = AgentConfig {
            enabled: true,
            paused: true,
            hosts: BTreeMap::new(),
        };
        cfg.hosts
            .insert("staging".into(), vec!["api.service".into()]);
        cfg.hosts
            .insert("deleted-host".into(), vec!["old.service".into()]);

        // Refuses to clobber disk when store_error is present.
        assert!(matches!(
            persist_config(dir.path(), &cfg, &connections, Some("corrupt")),
            Err(StoreError::Unreadable(_))
        ));
        assert!(!dir.path().join(store::FILE_NAME).exists());

        // Drops orphaned deleted host so toggling paused succeeds while keeping live hosts intact.
        persist_config(dir.path(), &cfg, &connections, None).unwrap();
        let loaded = store::load(dir.path()).unwrap();
        assert!(loaded.enabled);
        assert!(loaded.paused);
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(
            loaded.hosts.get("staging").map(Vec::as_slice),
            Some(&["api.service".to_owned()][..])
        );
    }
}
