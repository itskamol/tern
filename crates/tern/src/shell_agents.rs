//! Wiring between [`Shell`] and the local Agent broker ([`crate::agents::broker`]).
//!
//! Keeps Agent broker lifecycle, `agents.json` state, and [`AgentHost`] implementation out of
//! `shell.rs` so `shell.rs` stays below the 800-line ceiling.

use std::time::Instant;

use gpui::Context;

use super::Shell;
use crate::agents::broker::{AgentHost, BrokerHandle, BrokerSnapshot};
use crate::agents::socket_path;
use crate::agents::store::{self, AgentConfig};
use crate::decisions::{DecisionPrompt, DecisionTag, PendingDecision, QueueFull};
use crate::settings;

/// Local Agent configuration and active `agent.sock` listener owned by [`Shell`].
#[derive(Debug, Default)]
pub(super) struct AgentsUi {
    pub(super) config: AgentConfig,
    /// Set when `agents.json` exists but cannot be read; saving is refused so it is not clobbered.
    pub(super) store_error: Option<String>,
    broker: Option<BrokerHandle>,
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

    fn ask_agent_decision(
        &mut self,
        prompt: DecisionPrompt,
        deadline: Option<Instant>,
        cx: &mut Context<Self>,
    ) -> Result<PendingDecision, QueueFull> {
        self.ask_decision(DecisionTag::Agent, prompt, deadline, cx)
    }
}
