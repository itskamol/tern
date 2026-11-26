//! MCP tool definitions and JSON output schemas for `tern mcp` (ADR 0003, ADR 0004).
//!
//! Kept separate from the stdio JSON-RPC transport loop so tool contracts and protocol-version
//! gating stay concise and easy to audit alongside `mcp_tests.rs`.

use serde_json::{Value, json};

pub(super) const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

fn supports_output_schema(protocol_version: &str) -> bool {
    protocol_version >= "2025-06-18"
}

fn list_hosts_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "hosts": {
                "type": "array",
                "description": "Saved connections available to Agents.",
                "items": {
                    "type": "object",
                    "properties": {
                        "alias": { "type": "string", "description": "Exact Saved connection alias to pass to run_command." },
                        "host": { "type": "string", "description": "Remote hostname or IP address." },
                        "user": { "type": "string", "description": "SSH login username." },
                        "port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "SSH port." },
                        "group": { "type": "string", "description": "Optional Saved connection group name when set." },
                        "tags": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Saved connection tags (always an array; empty when untagged)."
                        },
                        "grant_active": { "type": "boolean", "description": "Point-in-time hint whether an active Host Grant covers this connection; run_command still re-verifies authorization." }
                    },
                    "required": ["alias", "host", "user", "port", "tags", "grant_active"]
                }
            },
            "usage": { "type": "string", "description": "Short guidance for calling run_command." }
        },
        "required": ["hosts", "usage"]
    })
}

fn run_command_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "host": { "type": "string", "description": "Saved connection alias where the command ran." },
            "exit_code": { "type": "integer", "minimum": 0, "description": "Remote process exit status when exited normally (absent if terminated by a signal or closed without status)." },
            "signal": { "type": "string", "description": "Signal name when terminated by a signal." },
            "stdout": { "type": "string", "description": "Captured standard output (UTF-8 lossy), capped at 64 KiB." },
            "stderr": { "type": "string", "description": "Captured standard error (UTF-8 lossy), capped at 64 KiB." },
            "stdout_truncated": { "type": "boolean", "description": "True when stdout exceeded 64 KiB and trailing bytes were discarded." },
            "stderr_truncated": { "type": "boolean", "description": "True when stderr exceeded 64 KiB and trailing bytes were discarded." },
            "duration_ms": { "type": "integer", "minimum": 0, "description": "Elapsed wall-clock duration in milliseconds." }
        },
        "required": [
            "host",
            "stdout",
            "stderr",
            "stdout_truncated",
            "stderr_truncated",
            "duration_ms"
        ]
    })
}

pub(super) fn tools_list_payload_for_version(protocol_version: &str) -> Value {
    let mut list_hosts = json!({
        "name": "list_hosts",
        "description": "List Saved connections in tern available to Agents, optionally filtered by query (exact alias ranked first, then prefix/substring, host/user, and group/tags). Returns optional group, tags (always an array), and grant_active as a point-in-time hint. Read-only and safe to retry.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Optional case-insensitive filter matched against alias (exact, prefix, substring), host or user, then group or tags."
                }
            },
            "additionalProperties": false
        }
    });
    let mut run_command = json!({
        "name": "run_command",
        "description": "Run a shell command or inline script on a Saved connection over an isolated non-PTY, non-interactive SSH exec channel (stdin closed immediately). PATH and environment may differ from an interactive terminal; hand password-requiring sudo to the human. Limits: 64 KiB command, 64 KiB each for stdout/stderr (check stdout_truncated and stderr_truncated). isError reflects broker or SSH failure, not remote exit status (non-zero exit_code still returns isError: false). Retries: tern_not_running is safe to retry; unknown_outcome is not; on timed_out, only phase=not_dispatched is safe to retry blindly, and remote process stop is not guaranteed. Requires approval in tern unless an active Host Grant exists.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "host": {
                    "type": "string",
                    "description": "Exact Saved connection alias returned by list_hosts."
                },
                "command": {
                    "type": "string",
                    "description": "Shell command or inline script executed by the remote user's default shell over SSH exec (up to 64 KiB)."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 300,
                    "description": "Optional wall-clock timeout in seconds covering connection setup and execution (defaults to 30, clamped to 1..=300)."
                }
            },
            "required": ["host", "command"],
            "additionalProperties": false
        }
    });

    if supports_output_schema(protocol_version) {
        if let Some(obj) = list_hosts.as_object_mut() {
            obj.insert("outputSchema".to_owned(), list_hosts_output_schema());
        }
        if let Some(obj) = run_command.as_object_mut() {
            obj.insert("outputSchema".to_owned(), run_command_output_schema());
        }
    }

    json!({
        "tools": [list_hosts, run_command]
    })
}
