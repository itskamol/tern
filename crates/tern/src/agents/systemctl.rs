//! `systemctl show` command construction, timeout clamping, and structured output parsing.
//!
//! Validates every systemd unit name before formatting the remote command, clamps caller
//! timeouts to `1..=60` s (default 15 s), and cleanly separates structured service states
//! (`active`, `inactive`, `failed`, `not_found`, etc.) from host/transport failures
//! (`unsupported_host`, `ssh_error`).

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use tern_ssh::ExecOutput;

use super::protocol::{ActiveState, AgentError, AgentErrorCode, LoadState, ServiceStatusResponse};
use super::store::{self, StoreError};
use crate::decisions::sanitize_display_text;

/// Default wall-clock timeout for `service_status` when the caller omits `timeout_secs`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;
/// Minimum permitted `timeout_secs` value.
pub const MIN_TIMEOUT_SECS: u64 = 1;
/// Maximum permitted `timeout_secs` value.
pub const MAX_TIMEOUT_SECS: u64 = 60;

/// Returns the clamped wall-clock timeout for a `service_status` call.
#[must_use]
pub fn effective_timeout(timeout_secs: Option<u64>) -> Duration {
    Duration::from_secs(
        timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS),
    )
}

/// Builds the exact non-PTY remote command for checking `unit`.
///
/// # Errors
/// Returns [`StoreError::InvalidUnitName`] if `unit` fails [`store::validate_unit_name`].
pub fn command_for(unit: &str) -> Result<String, StoreError> {
    store::validate_unit_name(unit)?;
    Ok(format!(
        "systemctl show -p LoadState,ActiveState,SubState --value -- {unit}"
    ))
}

/// Parses the output of `systemctl show -p LoadState,ActiveState,SubState --value -- <unit>`
/// into a structured [`ServiceStatusResponse`] or a distinct [`AgentError`].
///
/// # Errors
/// Returns [`AgentErrorCode::UnsupportedHost`] when `systemctl` is missing, unsupported, or the
/// host is not running systemd, or [`AgentErrorCode::SshError`] for other remote command failures
/// or unparseable output.
pub fn parse_output(
    host: &str,
    service: &str,
    output: &ExecOutput,
    checked_at: DateTime<Utc>,
) -> Result<ServiceStatusResponse, AgentError> {
    let stderr = String::from_utf8_lossy(&output.stderr);

    if is_unsupported_host(output.exit_status, &stderr) {
        return Err(AgentError::new(
            AgentErrorCode::UnsupportedHost,
            unsupported_message(&stderr),
        ));
    }

    if let Some(signal) = &output.exit_signal {
        return Err(AgentError::new(
            AgentErrorCode::SshError,
            format!(
                "remote systemctl terminated by signal {}",
                sanitize_display_text(signal)
            ),
        ));
    }

    if output.exit_status != Some(0) {
        let detail = first_sanitized_line(&stderr);
        let msg = if detail.is_empty() {
            match output.exit_status {
                Some(code) => format!("remote systemctl exited with status {code}"),
                None => "remote systemctl exited without status".to_owned(),
            }
        } else {
            format!("remote systemctl failed: {detail}")
        };
        return Err(AgentError::new(AgentErrorCode::SshError, msg));
    }

    if output.stdout_truncated {
        return Err(AgentError::new(
            AgentErrorCode::SshError,
            "remote systemctl output exceeded cap",
        ));
    }

    let stdout = std::str::from_utf8(&output.stdout).map_err(|_| {
        AgentError::new(
            AgentErrorCode::SshError,
            "remote systemctl output was not valid utf-8",
        )
    })?;

    let trimmed = strip_single_trailing_newline(stdout);
    let lines: Vec<&str> = trimmed
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).trim())
        .collect();

    if lines.len() != 3 {
        return Err(AgentError::new(
            AgentErrorCode::SshError,
            format!(
                "unexpected systemctl output (expected 3 lines, got {})",
                lines.len()
            ),
        ));
    }

    let load_state = parse_load_state(lines[0]);
    let active_state = parse_active_state(lines[1]);
    let sub_state = sanitize_display_text(lines[2]);

    Ok(ServiceStatusResponse {
        host: host.to_owned(),
        service: service.to_owned(),
        load_state,
        active_state,
        sub_state,
        checked_at: checked_at.to_rfc3339_opts(SecondsFormat::Secs, true),
    })
}

fn strip_single_trailing_newline(s: &str) -> &str {
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.strip_suffix('\r').unwrap_or(s)
}

fn is_unsupported_host(exit_status: Option<u32>, stderr: &str) -> bool {
    if matches!(exit_status, Some(126 | 127)) {
        return true;
    }
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("has not been booted with systemd")
        || lower.contains("not running systemd")
        || lower.contains("failed to connect to bus")
        || lower.contains("failed to connect to system scope bus")
        || lower.contains("unrecognized option")
        || lower.contains("invalid option")
    {
        return true;
    }
    exit_status != Some(0)
        && (lower.contains("command not found")
            || lower.contains("systemctl: not found")
            || (lower.contains("systemctl") && lower.contains("no such file or directory")))
}

fn unsupported_message(stderr: &str) -> String {
    let detail = first_sanitized_line(stderr);
    if detail.is_empty() {
        "host does not support systemd systemctl".to_owned()
    } else {
        format!("host does not support systemd systemctl: {detail}")
    }
}

fn first_sanitized_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(sanitize_display_text)
        .unwrap_or_default()
}

fn parse_load_state(raw: &str) -> LoadState {
    match raw {
        "loaded" => LoadState::Loaded,
        "not-found" => LoadState::NotFound,
        "bad-setting" => LoadState::BadSetting,
        "error" => LoadState::Error,
        "masked" => LoadState::Masked,
        "stub" => LoadState::Stub,
        "merged" => LoadState::Merged,
        _ => LoadState::Unknown,
    }
}

fn parse_active_state(raw: &str) -> ActiveState {
    match raw {
        "active" => ActiveState::Active,
        "inactive" => ActiveState::Inactive,
        "failed" => ActiveState::Failed,
        "activating" => ActiveState::Activating,
        "deactivating" => ActiveState::Deactivating,
        "reloading" => ActiveState::Reloading,
        "refreshing" => ActiveState::Refreshing,
        "maintenance" => ActiveState::Maintenance,
        _ => ActiveState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use chrono::TimeZone as _;

    use super::*;

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 23, 15, 30, 45).unwrap()
    }

    fn exec_out(exit_status: Option<u32>, stdout: &[u8], stderr: &[u8]) -> ExecOutput {
        ExecOutput {
            exit_status,
            exit_signal: None,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[test]
    fn timeout_clamps_to_1_through_60_and_defaults_to_15() {
        assert_eq!(effective_timeout(None), Duration::from_secs(15));
        assert_eq!(effective_timeout(Some(0)), Duration::from_secs(1));
        assert_eq!(effective_timeout(Some(1)), Duration::from_secs(1));
        assert_eq!(effective_timeout(Some(30)), Duration::from_secs(30));
        assert_eq!(effective_timeout(Some(60)), Duration::from_secs(60));
        assert_eq!(effective_timeout(Some(61)), Duration::from_secs(60));
        assert_eq!(effective_timeout(Some(999)), Duration::from_secs(60));
    }

    #[test]
    fn command_builder_formats_exact_systemctl_command_and_rejects_invalid_units() {
        assert_eq!(
            command_for("api.service").unwrap(),
            "systemctl show -p LoadState,ActiveState,SubState --value -- api.service"
        );
        assert_eq!(
            command_for("worker@1.service").unwrap(),
            "systemctl show -p LoadState,ActiveState,SubState --value -- worker@1.service"
        );
        assert!(command_for("-flag").is_err());
        assert!(command_for("api.service; id").is_err());
    }

    #[test]
    fn parses_active_inactive_failed_not_found_and_transitional_states() {
        let active = parse_output(
            "staging",
            "api.service",
            &exec_out(Some(0), b"loaded\nactive\nrunning\n", b""),
            fixed_time(),
        )
        .unwrap();
        assert_eq!(active.host, "staging");
        assert_eq!(active.service, "api.service");
        assert_eq!(active.load_state, LoadState::Loaded);
        assert_eq!(active.active_state, ActiveState::Active);
        assert_eq!(active.sub_state, "running");
        assert_eq!(active.checked_at, "2026-11-23T15:30:45Z");

        let inactive = parse_output(
            "staging",
            "api.service",
            &exec_out(Some(0), b"loaded\r\ninactive\r\ndead\r\n", b""),
            fixed_time(),
        )
        .unwrap();
        assert_eq!(inactive.load_state, LoadState::Loaded);
        assert_eq!(inactive.active_state, ActiveState::Inactive);
        assert_eq!(inactive.sub_state, "dead");

        let failed = parse_output(
            "staging",
            "api.service",
            &exec_out(Some(0), b"loaded\nfailed\nfailed\n", b""),
            fixed_time(),
        )
        .unwrap();
        assert_eq!(failed.load_state, LoadState::Loaded);
        assert_eq!(failed.active_state, ActiveState::Failed);
        assert_eq!(failed.sub_state, "failed");

        // systemd reports missing units with exit 0 and LoadState=not-found (even empty SubState).
        let missing = parse_output(
            "staging",
            "ghost.service",
            &exec_out(Some(0), b"not-found\ninactive\n\n", b""),
            fixed_time(),
        )
        .unwrap();
        assert_eq!(missing.load_state, LoadState::NotFound);
        assert_eq!(missing.active_state, ActiveState::Inactive);
        assert_eq!(missing.sub_state, "");
        let json = serde_json::to_value(&missing).unwrap();
        assert_eq!(json["load_state"], "not_found");

        for (raw_load, want_load) in [
            ("bad-setting", LoadState::BadSetting),
            ("error", LoadState::Error),
            ("masked", LoadState::Masked),
            ("stub", LoadState::Stub),
            ("merged", LoadState::Merged),
            ("weird", LoadState::Unknown),
        ] {
            let out = format!("{raw_load}\nactivating\nstart-pre\n");
            let parsed = parse_output(
                "h",
                "s",
                &exec_out(Some(0), out.as_bytes(), b""),
                fixed_time(),
            )
            .unwrap();
            assert_eq!(parsed.load_state, want_load);
            assert_eq!(parsed.active_state, ActiveState::Activating);
        }

        for (raw_active, want_active) in [
            ("deactivating", ActiveState::Deactivating),
            ("reloading", ActiveState::Reloading),
            ("refreshing", ActiveState::Refreshing),
            ("maintenance", ActiveState::Maintenance),
            ("mystery", ActiveState::Unknown),
        ] {
            let out = format!("loaded\n{raw_active}\n\x1b[31msub\x1b[0m\n");
            let parsed = parse_output(
                "h",
                "s",
                &exec_out(Some(0), out.as_bytes(), b""),
                fixed_time(),
            )
            .unwrap();
            assert_eq!(parsed.active_state, want_active);
            assert_eq!(parsed.sub_state, "sub");
        }
    }

    #[test]
    fn maps_missing_systemctl_and_non_systemd_hosts_to_unsupported_host() {
        let cases = [
            exec_out(Some(127), b"", b"sh: 1: systemctl: not found\n"),
            exec_out(Some(126), b"", b""),
            exec_out(
                Some(1),
                b"",
                b"System has not been booted with systemd as init system (PID 1). Can't operate.\n",
            ),
            exec_out(
                Some(1),
                b"",
                b"Failed to connect to bus: No such file or directory\n",
            ),
            exec_out(Some(1), b"", b"systemctl: unrecognized option '--value'\n"),
            exec_out(Some(1), b"", b"bash: systemctl: command not found\n"),
            exec_out(Some(1), b"", b"sh: systemctl: No such file or directory\n"),
            exec_out(Some(1), b"", b"Host is not running systemd\n"),
        ];
        for out in cases {
            let err = parse_output("bsd", "api.service", &out, fixed_time()).unwrap_err();
            assert_eq!(err.error, AgentErrorCode::UnsupportedHost, "got {err:?}");
            assert!(
                err.message.contains("systemd systemctl"),
                "expected clear systemd message, got {}",
                err.message
            );
            if out.stderr.is_empty() {
                assert_eq!(err.message, "host does not support systemd systemctl");
            } else {
                let first = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned();
                assert!(
                    err.message.contains(&first),
                    "missing detail in {}",
                    err.message
                );
            }
        }
    }

    #[test]
    fn maps_signals_unexpected_exit_truncation_and_malformed_stdout_to_ssh_error() {
        let mut signaled = exec_out(None, b"", b"");
        signaled.exit_signal = Some("KILL".into());
        let err = parse_output("h", "s", &signaled, fixed_time()).unwrap_err();
        assert_eq!(err.error, AgentErrorCode::SshError);
        assert!(err.message.contains("KILL"));

        // Non-zero exit with generic "No such file or directory" (not mentioning systemctl)
        // or mentioning systemctl without "no such file" is an ssh_error, not unsupported_host.
        for stderr in [
            b"Permission denied\n".as_slice(),
            b"systemctl: permission denied\n".as_slice(),
            b"/etc/foo: No such file or directory\n".as_slice(),
        ] {
            let err =
                parse_output("h", "s", &exec_out(Some(1), b"", stderr), fixed_time()).unwrap_err();
            assert_eq!(err.error, AgentErrorCode::SshError, "got {err:?}");
        }

        let err = parse_output("h", "s", &exec_out(Some(2), b"", b""), fixed_time()).unwrap_err();
        assert_eq!(err.error, AgentErrorCode::SshError);
        assert!(err.message.contains('2'));

        let err = parse_output("h", "s", &exec_out(None, b"", b""), fixed_time()).unwrap_err();
        assert_eq!(err.error, AgentErrorCode::SshError);

        let mut truncated = exec_out(Some(0), b"loaded\nactive\nrunning\n", b"");
        truncated.stdout_truncated = true;
        assert_eq!(
            parse_output("h", "s", &truncated, fixed_time())
                .unwrap_err()
                .error,
            AgentErrorCode::SshError
        );

        assert_eq!(
            parse_output("h", "s", &exec_out(Some(0), b"\xff\xfe", b""), fixed_time())
                .unwrap_err()
                .error,
            AgentErrorCode::SshError
        );

        assert_eq!(
            parse_output(
                "h",
                "s",
                &exec_out(Some(0), b"only-two\nlines\n", b""),
                fixed_time()
            )
            .unwrap_err()
            .error,
            AgentErrorCode::SshError
        );
    }
}
