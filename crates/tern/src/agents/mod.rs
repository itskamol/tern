//! Local Agent broker and MCP stdio server (ADR 0003, ADR 0004).
//!
//! Keeps SSH credentials, host-key verification, and human Decisions inside tern while exposing
//! `list_hosts` and `run_command` over stdio (`tern mcp`) and a dedicated `0o600` local Unix
//! socket (`agent.sock`).

pub mod attention;
pub mod broker;
mod broker_exec;
mod broker_socket;
pub mod grants;
pub mod headless_login;
pub mod mcp;
pub mod protocol;
pub mod store;

#[cfg(test)]
#[path = "agents_tests.rs"]
mod tests;

use std::path::{Path, PathBuf};

/// Dedicated Unix domain socket for local Agent requests (kept separate from `tern.sock`).
pub const SOCKET_FILE: &str = "agent.sock";

/// Path to the local Agent Unix socket inside `dir`.
#[must_use]
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_FILE)
}
