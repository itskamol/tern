//! Execution and Decision prompt construction helpers for [`super::broker`].
//!
//! Runs approved `systemctl show` commands through [`super::headless_login`], bridges caller
//! cancellation/disconnect after SSH `exec` dispatch to `ExecCancel` and `unknown_outcome`, and
//! builds sanitized click-only Decision prompts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use gpui::AsyncApp;
use tern_ssh::{ConnectSpec, ExecOptions};

use super::broker::{BrokerSnapshot, CHOICE_ALLOW_ONCE, CHOICE_ALLOW_UNTIL_LOCK, CHOICE_DENY};
use super::broker_socket::Responder;
use super::headless_login::{self, HeadlessExec};
use super::protocol::{AgentError, AgentErrorCode, BrokerResult};
use super::systemctl;
use crate::connections::Connection;
use crate::decisions::{DecisionChoice, DecisionPrompt, sanitize_display_text};
use crate::password_command::{Approvals, Auth};
use crate::platform::SshRuntime;

/// Aborts a spawned tokio helper task if the owning broker future finishes or is dropped.
pub(super) struct AbortOnDrop(pub(super) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) async fn execute_service_status(
    conn: &Connection,
    snapshot: &BrokerSnapshot,
    service: &str,
    command: &str,
    timeout_secs: Option<u64>,
    responder: &Responder,
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

    let rt = cx.update(|cx| SshRuntime::handle(cx));
    let cancelled_after_dispatch = Arc::new(AtomicBool::new(false));
    let mut cancel_rx = Some(responder.cancel_receiver());
    let mut cancel_bridge: Option<AbortOnDrop> = None;

    let exec_res = headless_login::run(
        exec_req,
        |exec_cancel| {
            let Some(mut rx) = cancel_rx.take() else {
                return;
            };
            let flag = Arc::clone(&cancelled_after_dispatch);
            cancel_bridge = Some(AbortOnDrop(rt.spawn(async move {
                if rx.wait_for(|&c| c).await.is_ok() {
                    flag.store(true, Ordering::Release);
                    exec_cancel.cancel();
                }
            })));
        },
        cx,
    )
    .await;
    drop(cancel_bridge);

    if cancelled_after_dispatch.load(Ordering::Acquire) && exec_res.is_err() {
        return BrokerResult::Error(AgentError::new(
            AgentErrorCode::UnknownOutcome,
            "agent disconnected after command was dispatched",
        ));
    }

    match exec_res {
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

pub(super) fn caller_display(caller: Option<&str>) -> String {
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
