//! Local-only bounded Agent audit log (`agent-audit.jsonl`, mode `0o600`, excluded from
//! `tern-sync`) and quiet **Activity** list formatting (ADR 0003).
//!
//! Records structured `service_status` outcomes with sanitized, length-bounded fields and
//! machine-readable enums only — never secrets, passwords, passphrases, command output, or raw
//! error strings. Enforces bounded size-based rotation keeping every file at mode `0o600`.

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::broker::{AgentDecision, AgentOutcome};
use super::protocol::{
    ActiveState, AgentErrorCode, BrokerResult, LoadState, ServiceStatusResponse, UserActionReason,
};
use crate::decisions::sanitize_display_text;

/// File name inside the settings directory. Strictly excluded from `tern-sync`.
pub const FILE_NAME: &str = "agent-audit.jsonl";
/// Default maximum size in bytes of `agent-audit.jsonl` before rotating to `.1`.
pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024;
/// Default number of rotated backups (`agent-audit.jsonl.1` .. `.N`) kept on disk.
pub const DEFAULT_MAX_BACKUPS: usize = 4;

const MAX_CALLER_CHARS: usize = 128;
const MAX_HOST_CHARS: usize = 256;
const MAX_SERVICE_CHARS: usize = 256;

/// Path to the active local-only Agent audit log inside `dir`.
#[must_use]
pub fn audit_path(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// Path to rotated backup `index` (`1`-based: `agent-audit.jsonl.1`, `.2`, ...).
#[must_use]
pub fn rotated_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("{FILE_NAME}.{index}"))
}

/// Bounded size and rotated-backup retention policy for `agent-audit.jsonl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditRetention {
    /// Maximum byte size of `agent-audit.jsonl` before rotating to `agent-audit.jsonl.1`.
    pub max_bytes: u64,
    /// Number of rotated backup files (`agent-audit.jsonl.1` .. `.N`) kept on disk.
    pub max_backups: usize,
}

impl Default for AuditRetention {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_backups: DEFAULT_MAX_BACKUPS,
        }
    }
}

/// Normalized authorization decision recorded in `agent-audit.jsonl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    AllowOnce,
    AllowUntilLock,
    GrantReused,
    Denied,
    TimedOut,
    Cancelled,
    Paused,
}

impl From<AgentDecision> for AuditDecision {
    fn from(decision: AgentDecision) -> Self {
        match decision {
            AgentDecision::AllowOnce => Self::AllowOnce,
            AgentDecision::AllowUntilLock => Self::AllowUntilLock,
            AgentDecision::GrantReused => Self::GrantReused,
            AgentDecision::Denied => Self::Denied,
            AgentDecision::Expired => Self::TimedOut,
            AgentDecision::Cancelled => Self::Cancelled,
            AgentDecision::Paused => Self::Paused,
        }
    }
}

/// Normalized terminal outcome recorded in `agent-audit.jsonl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Active,
    Inactive,
    Failed,
    Activating,
    Deactivating,
    Reloading,
    Refreshing,
    Maintenance,
    NotFound,
    Unknown,
    UnknownOutcome,
    Denied,
    AgentsPaused,
    TimedOut,
    UserActionRequired,
    SshError,
    UnsupportedHost,
    TernNotRunning,
}

/// One structured JSON line appended to `agent-audit.jsonl`.
///
/// Never includes secrets, passwords, passphrases, raw error text, or command output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRecord {
    pub timestamp: String,
    pub caller: String,
    pub host: String,
    pub service: String,
    pub decision: AuditDecision,
    pub outcome: AuditOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<UserActionReason>,
}

impl AuditRecord {
    /// Builds a sanitized, bounded audit record from a completed `service_status` outcome.
    #[must_use]
    pub fn from_outcome(outcome: &AgentOutcome, at: DateTime<Utc>) -> Self {
        let (audit_outcome, reason) = classify_result(&outcome.result);
        Self {
            timestamp: at.to_rfc3339_opts(SecondsFormat::Secs, true),
            caller: sanitize_bounded(&outcome.caller, MAX_CALLER_CHARS, "Unknown Agent"),
            host: sanitize_bounded(&outcome.host, MAX_HOST_CHARS, "unknown"),
            service: sanitize_bounded(&outcome.service, MAX_SERVICE_CHARS, "unknown"),
            decision: AuditDecision::from(outcome.decision),
            outcome: audit_outcome,
            reason,
        }
    }
}

fn sanitize_bounded(raw: &str, max_chars: usize, fallback: &str) -> String {
    let clean = sanitize_display_text(raw);
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        return fallback.to_owned();
    }
    trimmed.chars().take(max_chars).collect()
}

fn classify_result(result: &BrokerResult) -> (AuditOutcome, Option<UserActionReason>) {
    match result {
        BrokerResult::Ok { data } => {
            match serde_json::from_value::<ServiceStatusResponse>(data.clone()) {
                Ok(status) => (classify_service_status(&status), None),
                Err(_) => (AuditOutcome::Unknown, None),
            }
        }
        BrokerResult::Error(err) => {
            let outcome = match err.error {
                AgentErrorCode::TernNotRunning => AuditOutcome::TernNotRunning,
                AgentErrorCode::AgentsPaused => AuditOutcome::AgentsPaused,
                AgentErrorCode::Denied => AuditOutcome::Denied,
                AgentErrorCode::TimedOut => AuditOutcome::TimedOut,
                AgentErrorCode::UserActionRequired => AuditOutcome::UserActionRequired,
                AgentErrorCode::SshError => AuditOutcome::SshError,
                AgentErrorCode::UnsupportedHost => AuditOutcome::UnsupportedHost,
                AgentErrorCode::UnknownOutcome => AuditOutcome::UnknownOutcome,
            };
            let reason = (outcome == AuditOutcome::UserActionRequired)
                .then_some(err.reason)
                .flatten();
            (outcome, reason)
        }
    }
}

fn classify_service_status(status: &ServiceStatusResponse) -> AuditOutcome {
    if status.load_state == LoadState::NotFound {
        return AuditOutcome::NotFound;
    }
    match status.active_state {
        ActiveState::Active => AuditOutcome::Active,
        ActiveState::Inactive => AuditOutcome::Inactive,
        ActiveState::Failed => AuditOutcome::Failed,
        ActiveState::Activating => AuditOutcome::Activating,
        ActiveState::Deactivating => AuditOutcome::Deactivating,
        ActiveState::Reloading => AuditOutcome::Reloading,
        ActiveState::Refreshing => AuditOutcome::Refreshing,
        ActiveState::Maintenance => AuditOutcome::Maintenance,
        ActiveState::Unknown => AuditOutcome::Unknown,
    }
}

/// Appends `record` as one JSON line to `<dir>/agent-audit.jsonl` (mode `0o600`), rotating to
/// `agent-audit.jsonl.1`..`.N` whenever appending would exceed `retention.max_bytes`.
///
/// # Errors
/// Returns an [`io::Error`] if the audit file cannot be created, rotated, permission-restricted,
/// serialized, or written.
pub fn append_with_retention(
    dir: &Path,
    record: &AuditRecord,
    retention: AuditRetention,
) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
    line.push(b'\n');

    let path = audit_path(dir);
    let current_len = match std::fs::metadata(&path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e),
    };

    if current_len > 0 && current_len.saturating_add(line.len() as u64) > retention.max_bytes {
        rotate(dir, retention.max_backups)?;
    }

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    restrict_mode(&path)?;
    file.write_all(&line)?;
    file.flush()
}

fn rotate(dir: &Path, max_backups: usize) -> io::Result<()> {
    let primary = audit_path(dir);
    if max_backups == 0 {
        remove_if_exists(&primary)?;
        return Ok(());
    }
    remove_if_exists(&rotated_path(dir, max_backups))?;
    for idx in (1..max_backups).rev() {
        let from = rotated_path(dir, idx);
        let to = rotated_path(dir, idx + 1);
        if from.exists() {
            std::fs::rename(&from, &to)?;
            restrict_mode(&to)?;
        }
    }
    if primary.exists() {
        let first = rotated_path(dir, 1);
        std::fs::rename(&primary, &first)?;
        restrict_mode(&first)?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn restrict_mode(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Formats a concise, sanitized **Activity** list entry for `record` using `GLOSSARY.md` wording.
#[must_use]
pub fn activity_text(record: &AuditRecord) -> String {
    let service = &record.service;
    let host = &record.host;
    let caller = &record.caller;

    match record.decision {
        AuditDecision::Denied => return format!("Denied {service} check on {host} for {caller}"),
        AuditDecision::Paused => return format!("Agent check paused for {service} on {host}"),
        AuditDecision::TimedOut => {
            return format!("Agent request timed out for {service} on {host}");
        }
        AuditDecision::Cancelled => {
            return format!("Agent check cancelled for {service} on {host}");
        }
        AuditDecision::AllowOnce | AuditDecision::AllowUntilLock | AuditDecision::GrantReused => {}
    }

    match record.outcome {
        AuditOutcome::Active => format!("Agent checked {service} on {host}: active"),
        AuditOutcome::Inactive => format!("Agent checked {service} on {host}: inactive"),
        AuditOutcome::Failed => format!("Agent checked {service} on {host}: failed"),
        AuditOutcome::Activating => format!("Agent checked {service} on {host}: activating"),
        AuditOutcome::Deactivating => format!("Agent checked {service} on {host}: deactivating"),
        AuditOutcome::Reloading => format!("Agent checked {service} on {host}: reloading"),
        AuditOutcome::Refreshing => format!("Agent checked {service} on {host}: refreshing"),
        AuditOutcome::Maintenance => format!("Agent checked {service} on {host}: maintenance"),
        AuditOutcome::NotFound => format!("Agent checked {service} on {host}: not found"),
        AuditOutcome::Unknown => format!("Agent checked {service} on {host}: unknown"),
        AuditOutcome::UnknownOutcome => {
            format!("Agent check outcome unknown for {service} on {host}")
        }
        AuditOutcome::TimedOut => format!("Agent check timed out for {service} on {host}"),
        AuditOutcome::AgentsPaused => format!("Agent check paused for {service} on {host}"),
        AuditOutcome::Denied => format!("Denied {service} check on {host} for {caller}"),
        AuditOutcome::UnsupportedHost => {
            format!("Agent check failed for {service} on {host} (unsupported host)")
        }
        AuditOutcome::SshError => {
            format!("Agent check failed for {service} on {host} (SSH error)")
        }
        AuditOutcome::TernNotRunning => {
            format!("Agent check failed for {service} on {host} (tern not running)")
        }
        AuditOutcome::UserActionRequired => {
            let reason = match record.reason {
                Some(UserActionReason::VaultLocked) => "vault locked",
                Some(UserActionReason::CommandApprovalRequired) => "command approval required",
                Some(UserActionReason::UnknownHostKey) => "unknown host key",
                Some(UserActionReason::InteractiveAuthRequired) => "interactive login required",
                None => "action required",
            };
            format!("Agent check failed for {service} on {host} ({reason})")
        }
    }
}

/// Converts `outcome` into an [`AuditRecord`], appends it to `<dir>/agent-audit.jsonl` (logging
/// any I/O failure via `tracing::warn!` without panicking), and returns the **Activity** text.
#[must_use]
pub fn record_outcome(
    dir: Option<&Path>,
    retention: AuditRetention,
    outcome: &AgentOutcome,
    at: DateTime<Utc>,
) -> String {
    let record = AuditRecord::from_outcome(outcome, at);
    if let Some(dir) = dir {
        if let Err(e) = append_with_retention(dir, &record, retention) {
            tracing::warn!(
                error = %e,
                path = %audit_path(dir).display(),
                "agent_audit_write_failed"
            );
        }
    } else {
        tracing::warn!("agent_audit_dir_unavailable");
    }
    activity_text(&record)
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
