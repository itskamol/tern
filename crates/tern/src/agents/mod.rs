//! Local Agent broker and MCP stdio server (ADR 0003).
//!
//! Keeps SSH credentials, host-key verification, allowlists, and human Decisions inside tern
//! while exposing typed read-only service status checks over stdio (`tern mcp`) and a dedicated
//! `0o600` local Unix socket (`agent.sock`).

pub mod broker;
pub mod headless_login;
pub mod mcp;
pub mod protocol;
pub mod store;
pub mod systemctl;

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
