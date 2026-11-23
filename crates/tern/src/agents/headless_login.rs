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
    ConnectSpec, ExecCancel, ExecError, ExecEvent, ExecOptions, ExecOutput, MemoryKey, Prompt,
};
use tern_vault::Key;

use super::protocol::{AgentError, AgentErrorCode, UserActionReason};
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
/// spawned so callers (including #62) can distinguish pre-dispatch cancellation from
/// post-dispatch disconnect (`unknown_outcome`).
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

    let mut ladder = Ladder::default();
    let mut prompt_failure: Option<AgentError> = None;

    while let Some(event) = exec.next().await {
        match event {
            ExecEvent::Prompt(prompt) => {
                if let Some(err) = handle_prompt(
                    prompt,
                    &mut ladder,
                    &host,
                    port,
                    auth.password_command.as_deref(),
                    &approvals,
                    deadline,
                    cx,
                )
                .await
                {
                    prompt_failure.get_or_insert(err);
                }
            }
            ExecEvent::Finished(result) => {
                return match result {
                    Ok(output) => Ok(output),
                    Err(exec_err) => {
                        if let Some(err) = prompt_failure {
                            Err(err)
                        } else {
                            Err(map_exec_error(&exec_err))
                        }
                    }
                };
            }
        }
    }

    Err(prompt_failure.unwrap_or_else(|| {
        AgentError::new(
            AgentErrorCode::SshError,
            "ssh exec channel ended unexpectedly",
        )
    }))
}

#[allow(clippy::too_many_arguments)]
async fn handle_prompt(
    mut prompt: Prompt,
    ladder: &mut Ladder,
    host: &str,
    port: u16,
    password_command: Option<&str>,
    approvals: &Approvals,
    deadline: Instant,
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
            host,
            port,
            command: password_command,
            approval: password_command.and_then(|c| approvals.ask(Purpose::Login, c)),
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
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    drop(prompt);
                    return Some(AgentError::new(
                        AgentErrorCode::TimedOut,
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

fn map_exec_error(err: &ExecError) -> AgentError {
    match err {
        ExecError::UnknownHostKey { .. } => {
            AgentError::user_action_required(UserActionReason::UnknownHostKey, err.to_string())
        }
        ExecError::InteractiveAuthRequired { .. } => AgentError::user_action_required(
            UserActionReason::InteractiveAuthRequired,
            err.to_string(),
        ),
        ExecError::Timeout { .. } => AgentError::new(AgentErrorCode::TimedOut, err.to_string()),
        ExecError::HostKeyChanged { .. }
        | ExecError::AuthFailed { .. }
        | ExecError::Refused
        | ExecError::Ssh(_)
        | ExecError::Cancelled => AgentError::new(AgentErrorCode::SshError, err.to_string()),
    }
}
