//! Strict headless **Login ladder** over isolated non-PTY [`tern_ssh::exec`] (ADR 0003).
//!
//! Succeeds silently with an already-open vault (`Keeper::lookup`), already-approved
//! `password_command` or `ProxyCommand`, `ssh-agent`, or unencrypted key files (including when
//! the vault is locked or absent for vaultless Hosts). Fails closed with
//! [`AgentErrorCode::UserActionRequired`] and a machine-readable [`UserActionReason`] on a
//! locked vault (even when `vault_keychain` opt-in is enabled — never calling `Keeper::need_vault`),
//! an unapproved command, an unknown host key, a password prompt, or a 2FA challenge.

use std::time::Instant;

use gpui::AsyncApp;
use tern_ssh::{
    ConnectSpec, ExecCancel, ExecError, ExecEvent, ExecOptions, ExecOutput, ExecTimeoutPhase,
    MemoryKey, Prompt,
};
use tern_vault::Key;

use super::protocol::{AgentError, AgentErrorCode, TimeoutPhase, UserActionReason};
use crate::keeper::Keeper;
use crate::login_ladder::{Facts, InVault, Ladder, Next};
use crate::password_command::{self as pc, Approvals, Auth, Purpose};
use crate::platform::SshRuntime;

/// Inputs for one isolated, headless SSH `exec` call on a Saved connection.
#[derive(Debug)]
pub struct HeadlessExec {
    pub spec: ConnectSpec,
    pub auth: Auth,
    pub approvals: Approvals,
    pub command: String,
    pub options: ExecOptions,
}

/// Runs `req` strictly headlessly on `SshRuntime`, never opening UI or unlocking Keychain.
///
/// Calls `on_dispatched` with the [`ExecCancel`] handle at the moment `tern_ssh::exec` is
/// spawned so callers can distinguish pre-dispatch cancellation from post-dispatch disconnect
/// (`unknown_outcome`).
///
/// # Errors
/// Returns [`AgentError`] with:
/// - [`AgentErrorCode::UserActionRequired`] and a specific [`UserActionReason`] when human
///   interaction (vault unlock, command approval, host-key trust, password, or 2FA) is required;
/// - [`AgentErrorCode::TimedOut`] when the wall-clock timeout expires;
/// - [`AgentErrorCode::SshError`] on changed host key (hard stop), authentication rejection,
///   exec refusal, or transport error.
pub async fn run(
    req: HeadlessExec,
    on_dispatched: impl FnOnce(ExecCancel),
    cx: &mut AsyncApp,
) -> Result<ExecOutput, AgentError> {
    let HeadlessExec {
        mut spec,
        auth,
        approvals,
        command,
        options,
    } = req;

    // 1. Refuse unapproved ProxyCommand before dialing anything.
    if let Some(proxy_cmd) = &auth.proxy_command
        && approvals.ask(Purpose::Proxy, proxy_cmd).is_some()
    {
        return Err(AgentError::user_action_required(
            UserActionReason::CommandApprovalRequired,
            "proxy command requires approval in tern before agents can use this host",
        ));
    }

    // 2. If the Saved connection names a vault SSH key, require an already-open vault.
    //    NEVER call Keeper::need_vault so vault_keychain opt-in never touches Keychain.
    if !auth.vault_keys.is_empty() {
        let memory_keys = cx.update(|cx| -> Result<Vec<MemoryKey>, AgentError> {
            if Keeper::locked(cx) {
                return Err(AgentError::user_action_required(
                    UserActionReason::VaultLocked,
                    "unlock the tern vault to use this host's saved ssh key",
                ));
            }
            let mut keys = Vec::with_capacity(auth.vault_keys.len());
            for name in &auth.vault_keys {
                let key = Key::SshKey { name: name.clone() };
                if let Some(openssh) = Keeper::lookup(&key, cx) {
                    keys.push(MemoryKey {
                        name: name.clone(),
                        openssh,
                    });
                }
            }
            Ok(keys)
        })?;
        spec.memory_keys.extend(memory_keys);
    }

    // 3. Enforce isolation defensively (ADR 0003).
    spec.forward_agent = false;
    spec.forwards.clear();

    let host = spec.host.clone();
    let port = spec.port;
    let deadline = Instant::now() + options.effective_timeout();
    let rt = cx.update(|cx| SshRuntime::handle(cx));

    let mut exec = tern_ssh::exec(spec, command, options, &rt);
    on_dispatched(exec.cancel_handle());

    let prompt_ctx = PromptContext {
        host: &host,
        port,
        password_command: auth.password_command.as_deref(),
        approvals: &approvals,
        deadline,
    };
    let mut ladder = Ladder::default();
    let mut prompt_failure: Option<AgentError> = None;

    while let Some(event) = exec.next().await {
        match event {
            ExecEvent::Prompt(prompt) => {
                // Keep only the latest prompt failure; if a later prompt succeeds (returns None),
                // clear any earlier skipped-key failure so it cannot leak into later outcomes.
                prompt_failure = handle_prompt(prompt, &mut ladder, &prompt_ctx, cx).await;
            }
            ExecEvent::Finished(result) => {
                return match result {
                    Ok(output) => Ok(output),
                    Err(exec_err) => Err(resolve_exec_error(&exec_err, prompt_failure)),
                };
            }
        }
    }

    Err(AgentError::new(
        AgentErrorCode::SshError,
        "ssh exec channel ended unexpectedly",
    ))
}

struct PromptContext<'a> {
    host: &'a str,
    port: u16,
    password_command: Option<&'a str>,
    approvals: &'a Approvals,
    deadline: Instant,
}

async fn handle_prompt(
    mut prompt: Prompt,
    ladder: &mut Ladder,
    ctx: &PromptContext<'_>,
    cx: &mut AsyncApp,
) -> Option<AgentError> {
    match &prompt {
        Prompt::UnknownHostKey {
            host: key_host,
            port: key_port,
            ..
        } => {
            let msg = format!(
                "host key for {key_host}:{key_port} is not in known_hosts; connect interactively in tern first"
            );
            drop(prompt);
            return Some(AgentError::user_action_required(
                UserActionReason::UnknownHostKey,
                msg,
            ));
        }
        Prompt::Challenge { .. } => {
            drop(prompt);
            return Some(AgentError::user_action_required(
                UserActionReason::InteractiveAuthRequired,
                "host requires interactive 2fa challenge; agents cannot answer interactive prompts",
            ));
        }
        Prompt::Password { .. } | Prompt::KeyPassphrase { .. } => {}
    }

    loop {
        let facts = Facts {
            host: ctx.host,
            port: ctx.port,
            command: ctx.password_command,
            approval: ctx
                .password_command
                .and_then(|c| ctx.approvals.ask(Purpose::Login, c)),
            vault_skipped: false,
        };
        let next = ladder.next(&prompt, facts, |key| {
            // Read-only Keeper check: NEVER call Keeper::need_vault.
            cx.update(|cx| match Keeper::lookup(key, cx) {
                Some(secret) => InVault::Secret(secret),
                None if Keeper::locked(cx) => InVault::Locked,
                None => InVault::Nothing,
            })
        });

        match next {
            Next::ApproveCommand(_) => {
                drop(prompt);
                return Some(AgentError::user_action_required(
                    UserActionReason::CommandApprovalRequired,
                    "password command requires approval in tern before agents can use this host",
                ));
            }
            Next::RunCommand(cmd) => {
                let remaining = ctx.deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    drop(prompt);
                    return Some(AgentError::timed_out(
                        TimeoutPhase::NotDispatched,
                        "command timed out before password command ran",
                    ));
                }
                let cmd_timeout = remaining.min(pc::TIMEOUT);
                let res = cx
                    .background_executor()
                    .spawn(async move { pc::run(&cmd, cmd_timeout) })
                    .await;
                match res {
                    Ok(secret) => {
                        if let Some(other) = crate::login::answer(prompt, secret) {
                            prompt = other;
                            continue;
                        }
                        return None;
                    }
                    Err(failure) => {
                        tracing::warn!(error = %failure.line(), "agent_password_command_failed");
                        // Turn is spent; fall through the Login ladder (vault -> interactive).
                        continue;
                    }
                }
            }
            Next::FromVault(_, secret) => {
                if let Some(other) = crate::login::answer(prompt, secret) {
                    prompt = other;
                    continue;
                }
                return None;
            }
            Next::OpenVault => {
                drop(prompt);
                return Some(AgentError::user_action_required(
                    UserActionReason::VaultLocked,
                    "unlock the tern vault to authenticate this host",
                ));
            }
            Next::Ask { .. } => {
                drop(prompt);
                return Some(AgentError::user_action_required(
                    UserActionReason::InteractiveAuthRequired,
                    "host requires interactive authentication; configure a key, vault entry, or approved password command in tern",
                ));
            }
        }
    }
}

/// Only consults `prompt_failure` when `exec_err` itself is an authentication or unknown-host-key
/// error so a declined passphrase for a skipped key can never mask `Timeout`, `HostKeyChanged`,
/// `Cancelled`, `Refused`, or transport errors.
fn resolve_exec_error(exec_err: &ExecError, prompt_failure: Option<AgentError>) -> AgentError {
    match exec_err {
        ExecError::InteractiveAuthRequired { .. }
        | ExecError::AuthFailed { .. }
        | ExecError::UnknownHostKey { .. } => {
            prompt_failure.unwrap_or_else(|| map_exec_error(exec_err))
        }
        ExecError::HostKeyChanged { .. }
        | ExecError::Timeout { .. }
        | ExecError::Cancelled
        | ExecError::Refused
        | ExecError::Ssh(_) => map_exec_error(exec_err),
    }
}

fn map_exec_error(err: &ExecError) -> AgentError {
    match err {
        ExecError::UnknownHostKey { .. } => {
            AgentError::user_action_required(UserActionReason::UnknownHostKey, err.to_string())
        }
        ExecError::InteractiveAuthRequired { .. } => AgentError::user_action_required(
            UserActionReason::InteractiveAuthRequired,
            err.to_string(),
        ),
        ExecError::Timeout { phase, partial, .. } => {
            let phase = match phase {
                ExecTimeoutPhase::NotDispatched => TimeoutPhase::NotDispatched,
                ExecTimeoutPhase::DispatchUncertain => TimeoutPhase::DispatchUncertain,
                ExecTimeoutPhase::Running => TimeoutPhase::Running,
            };
            AgentError::timed_out(phase, err.to_string()).with_partial_output(
                String::from_utf8_lossy(&partial.stdout).into_owned(),
                String::from_utf8_lossy(&partial.stderr).into_owned(),
                partial.stdout_truncated,
                partial.stderr_truncated,
            )
        }
        ExecError::HostKeyChanged { .. }
        | ExecError::AuthFailed { .. }
        | ExecError::Refused
        | ExecError::Ssh(_)
        | ExecError::Cancelled => AgentError::new(AgentErrorCode::SshError, err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn resolve_exec_error_never_lets_stale_prompt_failure_mask_non_auth_failures() {
        let stale = AgentError::user_action_required(
            UserActionReason::InteractiveAuthRequired,
            "declined encrypted key",
        );

        let timeout = resolve_exec_error(
            &ExecError::Timeout {
                timeout: Duration::from_secs(1),
                phase: ExecTimeoutPhase::Running,
                partial: tern_ssh::ExecPartialOutput {
                    stdout: b"line 1\n".to_vec(),
                    stderr: b"warn\n".to_vec(),
                    stdout_truncated: false,
                    stderr_truncated: true,
                },
            },
            Some(stale.clone()),
        );
        assert_eq!(timeout.error, AgentErrorCode::TimedOut);
        assert_eq!(timeout.reason, None);
        assert_eq!(timeout.phase, Some(TimeoutPhase::Running));
        assert_eq!(timeout.stdout.as_deref(), Some("line 1\n"));
        assert_eq!(timeout.stderr.as_deref(), Some("warn\n"));
        assert_eq!(timeout.stdout_truncated, Some(false));
        assert_eq!(timeout.stderr_truncated, Some(true));

        let changed = resolve_exec_error(
            &ExecError::HostKeyChanged { line: 4, hop: None },
            Some(stale.clone()),
        );
        assert_eq!(changed.error, AgentErrorCode::SshError);
        assert_eq!(changed.reason, None);
        assert!(changed.message.contains("host key changed"));

        let refused = resolve_exec_error(&ExecError::Refused, Some(stale.clone()));
        assert_eq!(refused.error, AgentErrorCode::SshError);
        assert_eq!(refused.reason, None);

        let cancelled = resolve_exec_error(&ExecError::Cancelled, Some(stale.clone()));
        assert_eq!(cancelled.error, AgentErrorCode::SshError);

        let ssh = resolve_exec_error(&ExecError::Ssh("reset".into()), Some(stale.clone()));
        assert_eq!(ssh.error, AgentErrorCode::SshError);
        assert_eq!(ssh.message, "reset");

        // Auth / unknown-host-key failures still preserve the specific prompt failure reason.
        let vault_locked =
            AgentError::user_action_required(UserActionReason::VaultLocked, "unlock vault");
        let auth = resolve_exec_error(
            &ExecError::InteractiveAuthRequired { hop: None },
            Some(vault_locked.clone()),
        );
        assert_eq!(auth, vault_locked);

        let auth_failed = resolve_exec_error(
            &ExecError::AuthFailed { hop: None },
            Some(vault_locked.clone()),
        );
        assert_eq!(auth_failed, vault_locked);

        let unknown = resolve_exec_error(
            &ExecError::UnknownHostKey { hop: None },
            Some(AgentError::user_action_required(
                UserActionReason::UnknownHostKey,
                "unknown key",
            )),
        );
        assert_eq!(unknown.error, AgentErrorCode::UserActionRequired);
        assert_eq!(unknown.reason, Some(UserActionReason::UnknownHostKey));
    }
}
