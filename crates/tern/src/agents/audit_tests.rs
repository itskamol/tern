//! Unit tests for [`super`] covering audit schema, sanitization, `0o600` file permissions,
//! bounded rotation, and GLOSSARY-compliant Activity wording.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use chrono::{DateTime, TimeZone as _, Utc};
use serde_json::Value;

use super::*;
use crate::agents::protocol::AgentError;

fn fixed_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 24, 9, 30, 0).unwrap()
}

fn status_ok(load: LoadState, active: ActiveState, sub: &str) -> BrokerResult {
    BrokerResult::Ok {
        data: serde_json::to_value(ServiceStatusResponse {
            host: "staging".into(),
            service: "api.service".into(),
            load_state: load,
            active_state: active,
            sub_state: sub.into(),
            checked_at: "2026-11-24T09:30:00Z".into(),
        })
        .unwrap(),
    }
}

fn outcome(
    caller: &str,
    host: &str,
    service: &str,
    decision: AgentDecision,
    result: BrokerResult,
) -> AgentOutcome {
    AgentOutcome {
        caller: caller.into(),
        host: host.into(),
        service: service.into(),
        decision,
        result,
    }
}

#[test]
fn from_outcome_maps_decisions_outcomes_reasons_and_sanitizes_without_leaking_secrets() {
    // Expired maps to "timed_out" in audit JSON.
    let expired = AuditRecord::from_outcome(
        &outcome(
            "Cursor",
            "staging",
            "api.service",
            AgentDecision::Expired,
            BrokerResult::Error(AgentError::new(
                AgentErrorCode::TimedOut,
                "secret-passphrase-hunter2 must never appear in audit",
            )),
        ),
        fixed_time(),
    );
    let json: Value = serde_json::to_value(&expired).unwrap();
    assert_eq!(json["timestamp"], "2026-11-24T09:30:00Z");
    assert_eq!(json["caller"], "Cursor");
    assert_eq!(json["host"], "staging");
    assert_eq!(json["service"], "api.service");
    assert_eq!(json["decision"], "timed_out");
    assert_eq!(json["outcome"], "timed_out");
    assert!(json.get("reason").is_none());
    let raw = serde_json::to_string(&expired).unwrap();
    assert!(!raw.contains("hunter2"));
    assert!(!raw.contains("systemctl"));

    // LoadState::NotFound takes precedence over ActiveState::Inactive.
    let missing = AuditRecord::from_outcome(
        &outcome(
            "Claude Code",
            "staging",
            "ghost.service",
            AgentDecision::AllowOnce,
            status_ok(LoadState::NotFound, ActiveState::Inactive, "dead"),
        ),
        fixed_time(),
    );
    let missing_json: Value = serde_json::to_value(&missing).unwrap();
    assert_eq!(missing_json["decision"], "allow_once");
    assert_eq!(missing_json["outcome"], "not_found");

    // Loaded + active/failed/activating/deactivating map to active_state.
    for (active, expected) in [
        (ActiveState::Active, "active"),
        (ActiveState::Inactive, "inactive"),
        (ActiveState::Failed, "failed"),
        (ActiveState::Activating, "activating"),
        (ActiveState::Deactivating, "deactivating"),
        (ActiveState::Unknown, "unknown"),
    ] {
        let rec = AuditRecord::from_outcome(
            &outcome(
                "Cursor",
                "staging",
                "api.service",
                AgentDecision::GrantReused,
                status_ok(LoadState::Loaded, active, "raw-sub-state-ignored"),
            ),
            fixed_time(),
        );
        let val: Value = serde_json::to_value(&rec).unwrap();
        assert_eq!(val["decision"], "grant_reused");
        assert_eq!(val["outcome"], expected);
        assert!(
            !serde_json::to_string(&rec)
                .unwrap()
                .contains("raw-sub-state")
        );
    }

    // user_action_required includes only structured enum reason, never free-form message.
    let locked = AuditRecord::from_outcome(
        &outcome(
            "Cursor",
            "staging",
            "api.service",
            AgentDecision::AllowUntilLock,
            BrokerResult::Error(AgentError::user_action_required(
                UserActionReason::VaultLocked,
                "do-not-log-this-detail",
            )),
        ),
        fixed_time(),
    );
    let locked_json: Value = serde_json::to_value(&locked).unwrap();
    assert_eq!(locked_json["decision"], "allow_until_lock");
    assert_eq!(locked_json["outcome"], "user_action_required");
    assert_eq!(locked_json["reason"], "vault_locked");
    assert!(
        !serde_json::to_string(&locked)
            .unwrap()
            .contains("do-not-log-this-detail")
    );

    // Sanitization strips ANSI escapes, newlines, control chars, and bidi overrides, and bounds length.
    let hostile_caller = format!("\x1b[31m{}\n\r\u{202e}\x00\x1b[0m", "A".repeat(200));
    let sanitized = AuditRecord::from_outcome(
        &outcome(
            &hostile_caller,
            "prod\n\x1b[1m-1\x1b[0m",
            "api\r.service",
            AgentDecision::Denied,
            BrokerResult::Error(AgentError::new(AgentErrorCode::Denied, "denied")),
        ),
        fixed_time(),
    );
    assert_eq!(sanitized.caller.chars().count(), MAX_CALLER_CHARS);
    assert!(sanitized.caller.chars().all(|c| c == 'A'));
    assert_eq!(sanitized.host, "prod-1");
    assert_eq!(sanitized.service, "api.service");
    let serialized = serde_json::to_string(&sanitized).unwrap();
    assert!(!serialized.contains('\n'));
    assert!(!serialized.contains('\r'));
}

#[cfg(unix)]
#[test]
fn append_enforces_0600_mode_and_bounded_rotation_dropping_oldest_backups() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    let make_rec = |n: usize| {
        AuditRecord::from_outcome(
            &outcome(
                "Cursor",
                "staging",
                &format!("svc-{n}.service"),
                AgentDecision::AllowOnce,
                status_ok(LoadState::Loaded, ActiveState::Active, "running"),
            ),
            fixed_time(),
        )
    };

    let sample_line_len = serde_json::to_vec(&make_rec(0)).unwrap().len() as u64 + 1;
    // Fit at most 2 records per file and keep 2 rotated backups (.1 and .2).
    let retention = AuditRetention {
        max_bytes: sample_line_len * 2 + 10,
        max_backups: 2,
    };

    // Pre-seed a loose-mode file to verify append tightens it to 0o600.
    let primary = audit_path(dir.path());
    std::fs::write(&primary, "").unwrap();
    std::fs::set_permissions(&primary, std::fs::Permissions::from_mode(0o644)).unwrap();

    for i in 0..8 {
        if i == 4 {
            // Also verify rotation tightens an already-rotated backup with loose permissions.
            let b1 = rotated_path(dir.path(), 1);
            assert!(b1.exists());
            std::fs::set_permissions(&b1, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        append_with_retention(dir.path(), &make_rec(i), retention).unwrap();
    }

    let backup1 = rotated_path(dir.path(), 1);
    let backup2 = rotated_path(dir.path(), 2);
    let backup3 = rotated_path(dir.path(), 3);

    assert!(primary.exists());
    assert!(backup1.exists());
    assert!(backup2.exists());
    assert!(
        !backup3.exists(),
        "oldest backup past max_backups must be dropped"
    );

    assert_eq!(mode(&primary), 0o600);
    assert_eq!(mode(&backup1), 0o600);
    assert_eq!(mode(&backup2), 0o600);

    let read_services = |p: &Path| -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<AuditRecord>(line).unwrap().service)
            .collect()
    };

    // 0,1 -> rotated out when 6 arrived; 2,3 -> .2; 4,5 -> .1; 6,7 -> primary.
    assert_eq!(
        read_services(&backup2),
        vec!["svc-2.service", "svc-3.service"]
    );
    assert_eq!(
        read_services(&backup1),
        vec!["svc-4.service", "svc-5.service"]
    );
    assert_eq!(
        read_services(&primary),
        vec!["svc-6.service", "svc-7.service"]
    );

    for p in [&primary, &backup1, &backup2] {
        let size = std::fs::metadata(p).unwrap().len();
        assert!(
            size <= retention.max_bytes,
            "file {} exceeded max_bytes ({size} > {})",
            p.display(),
            retention.max_bytes
        );
    }
}

#[test]
fn activity_text_uses_glossary_wording_for_every_decision_and_outcome() {
    let check = |decision, result| {
        let rec = AuditRecord::from_outcome(
            &outcome("Cursor", "staging", "api.service", decision, result),
            fixed_time(),
        );
        let text = activity_text(&rec);
        let lower = text.to_ascii_lowercase();
        for avoided in [
            "server",
            "machine",
            "mcp client",
            "permission",
            "toast",
            "snackbar",
            "prompt",
        ] {
            assert!(
                !lower.contains(avoided),
                "activity text must obey GLOSSARY.md (found '{avoided}' in '{text}')"
            );
        }
        text
    };

    assert_eq!(
        check(
            AgentDecision::AllowOnce,
            status_ok(LoadState::Loaded, ActiveState::Active, "running")
        ),
        "Agent checked api.service on staging: active"
    );
    assert_eq!(
        check(
            AgentDecision::GrantReused,
            status_ok(LoadState::NotFound, ActiveState::Inactive, "")
        ),
        "Agent checked api.service on staging: not found"
    );
    assert_eq!(
        check(
            AgentDecision::AllowUntilLock,
            status_ok(LoadState::Loaded, ActiveState::Failed, "failed")
        ),
        "Agent checked api.service on staging: failed"
    );
    assert_eq!(
        check(
            AgentDecision::Denied,
            BrokerResult::Error(AgentError::new(AgentErrorCode::Denied, "denied"))
        ),
        "Denied api.service check on staging for Cursor"
    );
    assert_eq!(
        check(
            AgentDecision::Paused,
            BrokerResult::Error(AgentError::new(AgentErrorCode::AgentsPaused, "paused"))
        ),
        "Agent check paused for api.service on staging"
    );
    assert_eq!(
        check(
            AgentDecision::Expired,
            BrokerResult::Error(AgentError::new(AgentErrorCode::TimedOut, "expired"))
        ),
        "Agent request timed out for api.service on staging"
    );
    assert_eq!(
        check(
            AgentDecision::Cancelled,
            BrokerResult::Error(AgentError::new(AgentErrorCode::Denied, "cancelled"))
        ),
        "Agent check cancelled for api.service on staging"
    );
    assert_eq!(
        check(
            AgentDecision::AllowOnce,
            BrokerResult::Error(AgentError::new(
                AgentErrorCode::UnknownOutcome,
                "disconnected"
            ))
        ),
        "Agent check outcome unknown for api.service on staging"
    );
    assert_eq!(
        check(
            AgentDecision::AllowOnce,
            BrokerResult::Error(AgentError::user_action_required(
                UserActionReason::VaultLocked,
                "unlock vault"
            ))
        ),
        "Agent check failed for api.service on staging (vault locked)"
    );
    assert_eq!(
        check(
            AgentDecision::AllowOnce,
            BrokerResult::Error(AgentError::new(
                AgentErrorCode::UnsupportedHost,
                "no systemctl"
            ))
        ),
        "Agent check failed for api.service on staging (unsupported host)"
    );
    assert_eq!(
        check(
            AgentDecision::AllowOnce,
            BrokerResult::Error(AgentError::new(AgentErrorCode::SshError, "conn refused"))
        ),
        "Agent check failed for api.service on staging (SSH error)"
    );
    assert_eq!(
        check(
            AgentDecision::AllowOnce,
            BrokerResult::Error(AgentError::new(AgentErrorCode::TimedOut, "exec timed out"))
        ),
        "Agent check timed out for api.service on staging"
    );
    for (reason, expected_label) in [
        (
            UserActionReason::CommandApprovalRequired,
            "command approval required",
        ),
        (UserActionReason::UnknownHostKey, "unknown host key"),
        (
            UserActionReason::InteractiveAuthRequired,
            "interactive login required",
        ),
    ] {
        assert_eq!(
            check(
                AgentDecision::AllowOnce,
                BrokerResult::Error(AgentError::user_action_required(reason, "ignored detail"))
            ),
            format!("Agent check failed for api.service on staging ({expected_label})")
        );
    }
}

#[test]
fn zero_backup_retention_replaces_primary_without_creating_backups_and_write_errors_never_panic() {
    let dir = tempfile::tempdir().unwrap();
    let make_rec = |service: &str| {
        AuditRecord::from_outcome(
            &outcome(
                "   ",
                "staging",
                service,
                AgentDecision::AllowOnce,
                status_ok(LoadState::Loaded, ActiveState::Active, "running"),
            ),
            fixed_time(),
        )
    };
    let first = make_rec("first.service");
    assert_eq!(first.caller, "Unknown Agent");
    let max_bytes = serde_json::to_vec(&first).unwrap().len() as u64 + 2;
    let retention = AuditRetention {
        max_bytes,
        max_backups: 0,
    };
    append_with_retention(dir.path(), &first, retention).unwrap();
    append_with_retention(dir.path(), &make_rec("second.service"), retention).unwrap();

    assert!(!rotated_path(dir.path(), 1).exists());
    let lines: Vec<AuditRecord> = std::fs::read_to_string(audit_path(dir.path()))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].service, "second.service");

    // Exact max_bytes boundary fits without rotating; next byte rotates.
    let exact_dir = tempfile::tempdir().unwrap();
    let line_len = serde_json::to_vec(&first).unwrap().len() as u64 + 1;
    let exact_retention = AuditRetention {
        max_bytes: line_len * 2,
        max_backups: 1,
    };
    append_with_retention(exact_dir.path(), &first, exact_retention).unwrap();
    append_with_retention(exact_dir.path(), &first, exact_retention).unwrap();
    assert!(
        !rotated_path(exact_dir.path(), 1).exists(),
        "exact max_bytes fit must not rotate prematurely"
    );
    append_with_retention(exact_dir.path(), &first, exact_retention).unwrap();
    assert!(rotated_path(exact_dir.path(), 1).exists());

    // Unwritable audit path (directory collision) and None dir log a warning without panicking
    // and still return the formatted Activity text.
    let bad_dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(audit_path(bad_dir.path())).unwrap();
    let req = outcome(
        "Cursor",
        "staging",
        "api.service",
        AgentDecision::AllowOnce,
        status_ok(LoadState::Loaded, ActiveState::Active, "running"),
    );
    assert_eq!(
        record_outcome(
            Some(bad_dir.path()),
            AuditRetention::default(),
            &req,
            fixed_time()
        ),
        "Agent checked api.service on staging: active"
    );
    assert_eq!(
        record_outcome(None, AuditRetention::default(), &req, fixed_time()),
        "Agent checked api.service on staging: active"
    );
}
