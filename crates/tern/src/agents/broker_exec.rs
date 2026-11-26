//! Execution, Decision prompt, and Activity formatting helpers for [`super::broker`] (ADR 0004).
//!
//! Runs approved shell commands and scripts through [`super::headless_login`], bridges caller
//! cancellation/disconnect after SSH `exec` dispatch to `ExecCancel` and `unknown_outcome`, and
//! builds compact sanitized click-only Decision prompts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::AsyncApp;
use tern_ssh::{ConnectSpec, DEFAULT_EXEC_TIMEOUT, ExecCollector, ExecOptions, ExecStreamOptions};

use super::broker::{
    AgentHost, AgentOutcome, BrokerSnapshot, CHOICE_ALLOW_ONCE, CHOICE_ALLOW_UNTIL_LOCK,
    CHOICE_DENY, CommandRequest, Postcheck, WAITING_FOR_VAULT_UNLOCK_MESSAGE, saved_connection,
};
use super::broker_socket::Responder;
use super::grants::GrantFingerprint;
use super::headless_login::{self, HeadlessExec};
use super::protocol::{
    AgentError, AgentErrorCode, BrokerResult, ExecStreamResponse, RunCommandResponse,
    UserActionReason,
};
use super::vault_wait::VaultUnlockOutcome;
use crate::connections::Connection;
use crate::decisions::{
    DecisionChoice, DecisionPrompt, sanitize_display_text, sanitize_multiline_display_text,
};
use crate::password_command::{Approvals, Auth};
use crate::platform::SshRuntime;

const MAX_COMMAND_PREVIEW_CHARS: usize = 120;

/// Machine-readable reason why an Agent request was denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    Denied,
    UnknownHost,
    InvalidCommand,
    HostChanged,
    QueueFull,
}

/// Typed internal outcome carried on [`AgentOutcome`] for **Activity** formatting without
/// re-parsing wire JSON or substring-matching English error prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOutcomeKind {
    Ran {
        exit_code: Option<u32>,
        signal: Option<String>,
    },
    Paused,
    Cancelled,
    Expired,
    Denied(DenyReason),
    TimedOut,
    UserActionRequired(Option<UserActionReason>),
    UnknownOutcome,
    Failed,
}

/// Aborts a spawned tokio helper task if the owning broker future finishes or is dropped.
pub(super) struct AbortOnDrop(pub(super) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Returns the clamped wall-clock timeout for a `run_command` call (`1..=300 s`, default `30 s`).
#[must_use]
pub fn effective_timeout(timeout_secs: Option<u64>) -> Duration {
    match timeout_secs {
        Some(secs) => ExecOptions::with_timeout(Duration::from_secs(secs)).effective_timeout(),
        None => DEFAULT_EXEC_TIMEOUT,
    }
}

/// Formats a compact, sanitized single-line summary for a Decision card or attention cue:
/// the first non-empty line (capped at 120 characters) plus ` (+N lines)` for multi-line scripts.
#[must_use]
pub fn summarize_command(command: &str) -> String {
    let total_lines = command.lines().count();
    let first = command
        .lines()
        .map(|line| sanitize_display_text(line).trim().to_owned())
        .find(|line| !line.is_empty())
        .unwrap_or_default();

    let mut preview = truncate_chars(&first, MAX_COMMAND_PREVIEW_CHARS);
    if total_lines > 1 {
        let extra = total_lines - 1;
        let noun = if extra == 1 { "line" } else { "lines" };
        if preview.is_empty() {
            preview = format!("({total_lines} lines)");
        } else {
            preview.push_str(&format!(" (+{extra} {noun})"));
        }
    }
    preview
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Whether a broker command executes buffered (`run_command` MCP) or streamed (`tern exec` CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExecMode {
    Buffered,
    Stream,
}

fn is_vault_locked(result: &BrokerResult) -> bool {
    matches!(
        result,
        BrokerResult::Error(err)
            if err.error == AgentErrorCode::UserActionRequired
                && err.reason == Some(UserActionReason::VaultLocked)
    )
}

/// Runs `command` in `mode`; if the host requires a locked Vault and the host UI supports bounded
/// Vault unlock waiting, waits once for the human to unlock the Vault and retries execution
/// strictly once (re-checking pause and target fingerprint first).
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_with_vault_wait<H: AgentHost>(
    req: &CommandRequest<'_, H>,
    conn: &Connection,
    snapshot: &BrokerSnapshot,
    command: &str,
    timeout_secs: Option<u64>,
    wants_progress: bool,
    mode: ExecMode,
    responder: &Responder,
    cx: &mut AsyncApp,
) -> (BrokerResult, AgentOutcomeKind) {
    let first = execute_command(conn, snapshot, command, timeout_secs, mode, responder, cx).await;
    if !is_vault_locked(&first.0) || responder.is_cancelled() {
        return first;
    }

    let original_fp = GrantFingerprint::for_connection(conn, &snapshot.connections);
    let pending = match req.host.update(cx, |h, cx| {
        let deadline = Instant::now() + h.agent_decision_timeout();
        h.wait_for_vault_unlock(&req.caller, &conn.name, deadline, cx)
    }) {
        Ok(Some(pending)) => pending,
        Ok(None) => return first,
        Err(_) => {
            return (
                BrokerResult::Error(AgentError::new(
                    AgentErrorCode::TernNotRunning,
                    "tern window is no longer available",
                )),
                AgentOutcomeKind::Failed,
            );
        }
    };

    let vault_cancel = pending.cancel_handle();
    let rt = cx.update(|cx| SshRuntime::handle(cx));
    let mut cancel_rx = responder.cancel_receiver();
    let cancel_bridge = AbortOnDrop(rt.spawn(async move {
        if cancel_rx.wait_for(|&c| c).await.is_ok() {
            vault_cancel.cancel();
        }
    }));

    if wants_progress && !responder.is_cancelled() {
        responder
            .send_progress(WAITING_FOR_VAULT_UNLOCK_MESSAGE)
            .await;
    }

    let outcome = pending.wait().await;
    drop(cancel_bridge);

    match outcome {
        VaultUnlockOutcome::Expired => (
            BrokerResult::Error(AgentError::user_action_required(
                UserActionReason::VaultLocked,
                "timed out waiting for vault unlock in tern",
            )),
            AgentOutcomeKind::UserActionRequired(Some(UserActionReason::VaultLocked)),
        ),
        VaultUnlockOutcome::Cancelled => {
            let Ok(paused) = req.host.update(cx, |h, _| h.agent_snapshot().config.paused) else {
                return (
                    BrokerResult::Error(AgentError::new(
                        AgentErrorCode::TernNotRunning,
                        "tern window is no longer available",
                    )),
                    AgentOutcomeKind::Failed,
                );
            };
            if paused {
                (
                    BrokerResult::Error(AgentError::new(
                        AgentErrorCode::AgentsPaused,
                        "agents are paused in tern",
                    )),
                    AgentOutcomeKind::Paused,
                )
            } else if responder.is_cancelled() {
                (
                    BrokerResult::Error(AgentError::new(
                        AgentErrorCode::Denied,
                        "request cancelled by agent",
                    )),
                    AgentOutcomeKind::Cancelled,
                )
            } else {
                (
                    BrokerResult::Error(AgentError::user_action_required(
                        UserActionReason::VaultLocked,
                        "vault unlock cancelled in tern",
                    )),
                    AgentOutcomeKind::UserActionRequired(Some(UserActionReason::VaultLocked)),
                )
            }
        }
        VaultUnlockOutcome::Unlocked => {
            if responder.is_cancelled() {
                return (
                    BrokerResult::Error(AgentError::new(
                        AgentErrorCode::Denied,
                        "request cancelled by agent",
                    )),
                    AgentOutcomeKind::Cancelled,
                );
            }
            let Ok(recheck) = req.host.update(cx, |h, _| {
                let fresh = h.agent_snapshot();
                if fresh.config.paused {
                    return Postcheck::Paused;
                }
                let Some(fresh_conn) = saved_connection(&fresh, &req.target_host) else {
                    return Postcheck::UnknownHost;
                };
                if GrantFingerprint::for_connection(&fresh_conn, &fresh.connections) != original_fp
                {
                    return Postcheck::Changed;
                }
                Postcheck::Ready(Box::new((fresh, fresh_conn)))
            }) else {
                return (
                    BrokerResult::Error(AgentError::new(
                        AgentErrorCode::TernNotRunning,
                        "tern window is no longer available",
                    )),
                    AgentOutcomeKind::Failed,
                );
            };
            match recheck {
                Postcheck::Paused => (
                    BrokerResult::Error(AgentError::new(
                        AgentErrorCode::AgentsPaused,
                        "agents are paused in tern",
                    )),
                    AgentOutcomeKind::Paused,
                ),
                Postcheck::UnknownHost => (
                    BrokerResult::Error(AgentError::new(
                        AgentErrorCode::Denied,
                        "saved connection is no longer available in tern",
                    )),
                    AgentOutcomeKind::Denied(DenyReason::UnknownHost),
                ),
                Postcheck::Changed => (
                    BrokerResult::Error(AgentError::new(
                        AgentErrorCode::Denied,
                        "saved connection changed while waiting for vault unlock",
                    )),
                    AgentOutcomeKind::Denied(DenyReason::HostChanged),
                ),
                Postcheck::Ready(ready) => {
                    let (fresh, fresh_conn) = *ready;
                    execute_command(
                        &fresh_conn,
                        &fresh,
                        command,
                        timeout_secs,
                        mode,
                        responder,
                        cx,
                    )
                    .await
                }
            }
        }
    }
}

fn outcome_kind_for_error(err: &AgentError) -> AgentOutcomeKind {
    match err.error {
        AgentErrorCode::AgentsPaused => AgentOutcomeKind::Paused,
        AgentErrorCode::Denied => AgentOutcomeKind::Denied(DenyReason::Denied),
        AgentErrorCode::TimedOut => AgentOutcomeKind::TimedOut,
        AgentErrorCode::UserActionRequired => AgentOutcomeKind::UserActionRequired(err.reason),
        AgentErrorCode::UnknownOutcome => AgentOutcomeKind::UnknownOutcome,
        AgentErrorCode::SshError | AgentErrorCode::TernNotRunning => AgentOutcomeKind::Failed,
    }
}

async fn execute_command(
    conn: &Connection,
    snapshot: &BrokerSnapshot,
    command: &str,
    timeout_secs: Option<u64>,
    mode: ExecMode,
    responder: &Responder,
    cx: &mut AsyncApp,
) -> (BrokerResult, AgentOutcomeKind) {
    let (spec, auth, approvals) = match build_spec_and_auth(conn, snapshot) {
        Ok(parts) => parts,
        Err(err) => return (err, AgentOutcomeKind::Failed),
    };
    let options = match mode {
        ExecMode::Buffered => {
            ExecStreamOptions::with_timeout(Some(effective_timeout(timeout_secs)))
        }
        ExecMode::Stream => ExecStreamOptions::with_timeout(timeout_secs.map(Duration::from_secs)),
    };
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
    let started = Instant::now();
    let collector = std::sync::Mutex::new(ExecCollector::default());

    let exec_res = headless_login::run_stream(
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
        |bytes| {
            let collector = &collector;
            async move {
                match mode {
                    ExecMode::Buffered => {
                        if let Ok(mut guard) = collector.lock() {
                            guard.push_stdout(&bytes);
                        }
                    }
                    ExecMode::Stream => responder.send_stdout(&bytes).await,
                }
            }
        },
        |bytes| {
            let collector = &collector;
            async move {
                match mode {
                    ExecMode::Buffered => {
                        if let Ok(mut guard) = collector.lock() {
                            guard.push_stderr(&bytes);
                        }
                    }
                    ExecMode::Stream => responder.send_stderr(&bytes).await,
                }
            }
        },
        cx,
    )
    .await;
    drop(cancel_bridge);

    if cancelled_after_dispatch.load(Ordering::Acquire) && exec_res.is_err() {
        let msg = match mode {
            ExecMode::Buffered => "agent disconnected after command was dispatched",
            ExecMode::Stream => "client disconnected or interrupted after command was dispatched",
        };
        return (
            BrokerResult::Error(AgentError::new(AgentErrorCode::UnknownOutcome, msg)),
            AgentOutcomeKind::UnknownOutcome,
        );
    }

    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let collected = collector.into_inner().unwrap_or_default();
    match exec_res {
        Ok(done) => {
            let kind = AgentOutcomeKind::Ran {
                exit_code: done.exit_status,
                signal: done.exit_signal.clone(),
            };
            let serialized = match mode {
                ExecMode::Buffered => {
                    let output = collected.into_output(done);
                    serde_json::to_value(RunCommandResponse {
                        host: conn.name.clone(),
                        exit_code: output.exit_status,
                        signal: output.exit_signal,
                        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                        stdout_truncated: output.stdout_truncated,
                        stderr_truncated: output.stderr_truncated,
                        duration_ms,
                    })
                }
                ExecMode::Stream => serde_json::to_value(ExecStreamResponse {
                    host: conn.name.clone(),
                    exit_code: done.exit_status,
                    signal: done.exit_signal,
                    duration_ms,
                }),
            };
            match serialized {
                Ok(data) => (BrokerResult::Ok { data }, kind),
                Err(e) => (
                    BrokerResult::Error(AgentError::new(AgentErrorCode::SshError, e.to_string())),
                    AgentOutcomeKind::Failed,
                ),
            }
        }
        Err(mut err) => {
            if err.error == AgentErrorCode::TimedOut {
                if mode == ExecMode::Buffered {
                    let partial = collected.into_partial();
                    err = err.with_partial_output(
                        String::from_utf8_lossy(&partial.stdout).into_owned(),
                        String::from_utf8_lossy(&partial.stderr).into_owned(),
                        partial.stdout_truncated,
                        partial.stderr_truncated,
                    );
                }
                err = err.with_duration_ms(duration_ms);
            }
            let kind = outcome_kind_for_error(&err);
            (BrokerResult::Error(err), kind)
        }
    }
}

fn build_spec_and_auth(
    conn: &Connection,
    snapshot: &BrokerSnapshot,
) -> Result<(ConnectSpec, Auth, Approvals), BrokerResult> {
    let entry = conn.entry_in(&snapshot.connections);
    let mut spec = match ConnectSpec::from_host_entry(&entry) {
        Ok(spec) => spec,
        Err(e) => {
            return Err(BrokerResult::Error(AgentError::new(
                AgentErrorCode::SshError,
                e.to_string(),
            )));
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
    Ok((spec, auth, approvals))
}

pub(super) fn caller_display(caller: Option<&str>) -> String {
    caller
        .map(sanitize_display_text)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown Agent".to_owned())
}

/// Returns `true` when `conn` has a case-insensitive `prod` or `production` tag.
#[must_use]
pub fn is_production_host(conn: &Connection) -> bool {
    conn.tags
        .iter()
        .any(|tag| tag.eq_ignore_ascii_case("prod") || tag.eq_ignore_ascii_case("production"))
}

/// Builds the compact click-only Decision card prompt for a `run_command` request, attaching
/// the sanitized full command for expandable inspection when truncated or multi-line and noting
/// the shared scope of `Allow host until lock`.
#[must_use]
pub fn build_run_command_prompt(caller: &str, conn: &Connection, command: &str) -> DecisionPrompt {
    let preview = summarize_command(command);
    let full = sanitize_multiline_display_text(command);
    let host_detail = format!("{} ({}@{})", conn.name, conn.user, conn.host);
    let mut prompt = DecisionPrompt::new(
        format!("{caller} wants to run a command on {}", conn.name),
        vec![
            DecisionChoice::primary(CHOICE_ALLOW_ONCE, "Allow once"),
            DecisionChoice::secondary(CHOICE_ALLOW_UNTIL_LOCK, "Allow host until lock"),
            DecisionChoice::secondary(CHOICE_DENY, "Deny"),
        ],
    )
    .with_detail("Agent", format!("{caller} (unverified)"));
    prompt = if is_production_host(conn) {
        prompt.with_detail_badge("Host", host_detail, "PROD")
    } else {
        prompt.with_detail("Host", host_detail)
    };
    prompt = prompt.with_command(preview.clone()).with_footnote(format!(
        "Host grant lets any local Agent run commands on {} until lock.",
        conn.name
    ));
    if full != preview {
        prompt = prompt.with_full_command(full);
    }
    prompt
}

/// Formats a quiet **Activity** list entry summarizing a completed `run_command` outcome.
#[must_use]
pub fn format_activity(outcome: &AgentOutcome) -> String {
    let caller = sanitize_display_text(&outcome.caller);
    let actor = if caller.is_empty() {
        "Agent"
    } else {
        caller.as_str()
    };
    let host = sanitize_display_text(&outcome.host);
    match &outcome.kind {
        AgentOutcomeKind::Ran {
            exit_code: Some(code),
            ..
        } => format!("{actor} ran command on {host} (exit {code})"),
        AgentOutcomeKind::Ran {
            signal: Some(sig), ..
        } => {
            let sig = sanitize_display_text(sig);
            format!("{actor} ran command on {host} (signal {sig})")
        }
        AgentOutcomeKind::Ran { .. } => format!("{actor} ran command on {host}"),
        AgentOutcomeKind::Paused => format!("{actor} request paused on {host}"),
        AgentOutcomeKind::Cancelled => format!("{actor} request cancelled on {host}"),
        AgentOutcomeKind::Expired => format!("{actor} approval timed out on {host}"),
        AgentOutcomeKind::Denied(DenyReason::UnknownHost) => {
            format!("{actor} unknown host {host}")
        }
        AgentOutcomeKind::Denied(DenyReason::InvalidCommand) => {
            format!("{actor} invalid command for {host}")
        }
        AgentOutcomeKind::Denied(DenyReason::HostChanged) => {
            format!("{actor} host changed before running on {host}")
        }
        AgentOutcomeKind::Denied(DenyReason::Denied | DenyReason::QueueFull) => {
            format!("{actor} request denied on {host}")
        }
        AgentOutcomeKind::TimedOut => format!("{actor} command timed out on {host}"),
        AgentOutcomeKind::UserActionRequired(Some(UserActionReason::VaultLocked)) => {
            format!("{actor} blocked: vault locked for {host}")
        }
        AgentOutcomeKind::UserActionRequired(Some(UserActionReason::UnknownHostKey)) => {
            format!("{actor} blocked: unknown host key for {host}")
        }
        AgentOutcomeKind::UserActionRequired(Some(UserActionReason::CommandApprovalRequired)) => {
            format!("{actor} blocked: command approval needed for {host}")
        }
        AgentOutcomeKind::UserActionRequired(Some(UserActionReason::InteractiveAuthRequired)) => {
            format!("{actor} blocked: interactive auth required on {host}")
        }
        AgentOutcomeKind::UserActionRequired(None) => {
            format!("{actor} needs attention in tern for {host}")
        }
        AgentOutcomeKind::UnknownOutcome => {
            format!("{actor} disconnected during command on {host}")
        }
        AgentOutcomeKind::Failed => format!("{actor} command failed on {host}"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::agents::broker::AgentDecision;

    #[test]
    fn summarize_command_formats_single_and_multiline_and_strips_control_chars() {
        assert_eq!(summarize_command("node ./check.js"), "node ./check.js");
        assert_eq!(
            summarize_command("set -euo pipefail\necho ok\n"),
            "set -euo pipefail (+1 line)"
        );
        assert_eq!(
            summarize_command("\n\n  echo \x1b[31mhi\x1b[0m\nline2\nline3\n"),
            "echo hi (+4 lines)"
        );
        let long = "a".repeat(200);
        let summarized = summarize_command(&long);
        assert_eq!(summarized.chars().count(), MAX_COMMAND_PREVIEW_CHARS + 1);
        assert!(summarized.ends_with('…'));
    }

    #[test]
    fn effective_timeout_defaults_to_30s_and_clamps_to_1_to_300s() {
        assert_eq!(effective_timeout(None), Duration::from_secs(30));
        assert_eq!(effective_timeout(Some(0)), Duration::from_secs(1));
        assert_eq!(effective_timeout(Some(45)), Duration::from_secs(45));
        assert_eq!(effective_timeout(Some(999)), Duration::from_secs(300));
    }

    #[test]
    fn format_activity_covers_exit_code_signal_and_errors() {
        let ok_exit = AgentOutcome {
            caller: "Claude".into(),
            host: "prod".into(),
            decision: AgentDecision::AllowOnce,
            kind: AgentOutcomeKind::Ran {
                exit_code: Some(0),
                signal: None,
            },
        };
        assert_eq!(
            format_activity(&ok_exit),
            "Claude ran command on prod (exit 0)"
        );

        let denied = AgentOutcome {
            caller: "Claude".into(),
            host: "prod".into(),
            decision: AgentDecision::Denied,
            kind: AgentOutcomeKind::Denied(DenyReason::Denied),
        };
        assert_eq!(format_activity(&denied), "Claude request denied on prod");

        let unknown_host = AgentOutcome {
            caller: "Claude".into(),
            host: "ghost".into(),
            decision: AgentDecision::Denied,
            kind: AgentOutcomeKind::Denied(DenyReason::UnknownHost),
        };
        assert_eq!(format_activity(&unknown_host), "Claude unknown host ghost");

        let locked = AgentOutcome {
            caller: "Codex".into(),
            host: "alpha".into(),
            decision: AgentDecision::AllowOnce,
            kind: AgentOutcomeKind::UserActionRequired(Some(UserActionReason::VaultLocked)),
        };
        assert_eq!(
            format_activity(&locked),
            "Codex blocked: vault locked for alpha"
        );
    }

    #[test]
    fn build_run_command_prompt_attaches_full_command_for_multiline_and_explains_host_grant() {
        let conn = Connection {
            name: "alpha".into(),
            host: "10.0.0.5".into(),
            port: 22,
            user: "deploy".into(),
            ..Connection::default()
        };
        let single = build_run_command_prompt("Codex", &conn, "uptime");
        assert_eq!(single.command.as_deref(), Some("uptime"));
        assert_eq!(single.full_command, None);
        assert!(
            single
                .footnote
                .as_deref()
                .is_some_and(|f| f.contains("any local Agent"))
        );

        let multi = build_run_command_prompt("Codex", &conn, "set -e\necho \x1b[31mhi\x1b[0m\n");
        assert_eq!(multi.command.as_deref(), Some("set -e (+1 line)"));
        assert_eq!(multi.full_command.as_deref(), Some("set -e\necho hi"));
        assert_eq!(single.details[1].badge, None);

        for tags in [
            vec!["prod".to_owned()],
            vec!["PROD".to_owned()],
            vec!["web".to_owned(), "Production".to_owned()],
        ] {
            let prod_conn = Connection {
                tags,
                ..conn.clone()
            };
            let prod_prompt = build_run_command_prompt("Codex", &prod_conn, "uptime");
            assert_eq!(prod_prompt.details[1].label, "Host");
            assert_eq!(prod_prompt.details[1].badge.as_deref(), Some("PROD"));
        }

        let non_prod = Connection {
            tags: vec!["staging".into(), "preprod".into()],
            ..conn
        };
        let non_prod_prompt = build_run_command_prompt("Codex", &non_prod, "uptime");
        assert_eq!(non_prod_prompt.details[1].badge, None);
    }
}
