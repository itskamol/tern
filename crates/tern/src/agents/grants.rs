//! In-memory per-Host Agent Grants (`Allow host until lock`) and canonical target fingerprinting
//! (ADR 0004).
//!
//! Grants live strictly in memory — never serialized to `agents.json`, disk, or `tern-sync`.
//! Every Grant binds a Saved connection alias to a length-prefixed SHA-256 fingerprint over the
//! resolved target `(resolved_host, port, user, proxy_jump, proxy_command)`, re-verified before
//! every execution and pruned whenever Saved connections change.

use std::collections::BTreeMap;
use std::time::SystemTime;

use sha2::{Digest as _, Sha256};
use tern_ssh::HostEntry;

use crate::connections::Connection;

const FINGERPRINT_DOMAIN: &[u8] = b"tern-agent-host-grant-v1\0";

/// Canonical SHA-256 fingerprint over `(resolved_host, port, user, proxy_jump, proxy_command)`
/// using length-prefixed encoding so field boundaries can never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GrantFingerprint([u8; 32]);

impl GrantFingerprint {
    /// Computes the canonical fingerprint for a Saved connection resolved against
    /// `saved_connections` (so `ProxyJump` references to other Saved connections resolve to their
    /// live address, port, user, and identity files).
    #[must_use]
    pub fn for_connection(conn: &Connection, saved_connections: &[Connection]) -> Self {
        let entry = conn.entry_in(saved_connections);
        Self::for_host_entry(&entry)
    }

    /// Computes the canonical fingerprint directly from a resolved [`HostEntry`].
    #[must_use]
    pub fn for_host_entry(entry: &HostEntry) -> Self {
        let mut h = Sha256::new();
        h.update(FINGERPRINT_DOMAIN);
        write_str(&mut h, &entry.host_name);
        write_u16(&mut h, entry.port);
        write_opt_str(&mut h, entry.user.as_deref());
        write_u64(&mut h, entry.proxy_jump.len() as u64);
        for hop in &entry.proxy_jump {
            write_str(&mut h, &hop.host);
            write_u16(&mut h, hop.port);
            write_opt_str(&mut h, hop.user.as_deref());
            write_u64(&mut h, hop.identity_files.len() as u64);
            for path in &hop.identity_files {
                write_bytes(&mut h, path.as_os_str().as_encoded_bytes());
            }
        }
        write_opt_str(&mut h, entry.proxy_command.as_deref());
        Self(h.finalize().into())
    }
}

fn write_u16(h: &mut Sha256, v: u16) {
    h.update(v.to_be_bytes());
}

fn write_u64(h: &mut Sha256, v: u64) {
    h.update(v.to_be_bytes());
}

fn write_bytes(h: &mut Sha256, bytes: &[u8]) {
    write_u64(h, bytes.len() as u64);
    h.update(bytes);
}

fn write_str(h: &mut Sha256, s: &str) {
    write_bytes(h, s.as_bytes());
}

fn write_opt_str(h: &mut Sha256, opt: Option<&str>) {
    match opt {
        None => h.update([0u8]),
        Some(s) => {
            h.update([1u8]);
            write_str(h, s);
        }
    }
}

/// One active in-memory Grant letting Agents run commands on `host` until lock, sleep, session
/// switch, quit, pause, manual revoke, or Saved connection edit/delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub host: String,
    pub fingerprint: GrantFingerprint,
    pub granted_at: SystemTime,
}

/// In-memory-only store of active per-Host Agent Grants. Intentionally does not implement
/// `Serialize` or `Deserialize` so Grants can never be written to `agents.json` or `tern-sync`.
#[derive(Debug, Default)]
pub struct GrantStore {
    grants: BTreeMap<String, Grant>,
}

impl GrantStore {
    #[cfg(test)]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records or replaces an in-memory Grant for `host`.
    pub fn insert(
        &mut self,
        host: impl Into<String>,
        fingerprint: GrantFingerprint,
        granted_at: SystemTime,
    ) {
        let host = host.into();
        self.grants.insert(
            host.clone(),
            Grant {
                host,
                fingerprint,
                granted_at,
            },
        );
    }

    /// Re-verifies whether an active Grant exists for `host` whose stored fingerprint matches
    /// `current_fingerprint`. Any mismatched Grant is evicted immediately.
    pub fn verify(&mut self, host: &str, current_fingerprint: GrantFingerprint) -> bool {
        match self.grants.get(host) {
            Some(grant) if grant.fingerprint == current_fingerprint => true,
            Some(_) => {
                self.grants.remove(host);
                false
            }
            None => false,
        }
    }

    /// Immediately removes every Grant whose Saved connection was edited, renamed, or deleted.
    /// Returns how many Grants were invalidated.
    pub fn prune_stale(&mut self, connections: &[Connection]) -> usize {
        let before = self.grants.len();
        self.grants.retain(|_, grant| {
            let Some(conn) = connections.iter().find(|c| c.name == grant.host) else {
                return false;
            };
            GrantFingerprint::for_connection(conn, connections) == grant.fingerprint
        });
        before - self.grants.len()
    }

    /// Revokes one Grant by `host`. Returns `true` if a Grant was present.
    pub fn revoke(&mut self, host: &str) -> bool {
        self.grants.remove(host).is_some()
    }

    /// Clears every active Grant. Returns how many Grants were removed.
    pub fn revoke_all(&mut self) -> usize {
        let count = self.grants.len();
        self.grants.clear();
        count
    }

    /// Read-only sorted snapshot of active Grants for Settings → Agents.
    #[must_use]
    pub fn active(&self) -> Vec<Grant> {
        self.grants.values().cloned().collect()
    }

    #[cfg(test)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::PathBuf;

    use tern_ssh::JumpHop;

    use super::*;

    fn base_conn() -> Connection {
        Connection {
            name: "staging".into(),
            host: "10.0.0.10".into(),
            port: 22,
            user: "deploy".into(),
            proxy_jump: "bastion".into(),
            proxy_command: Some("nc %h %p".into()),
            ..Connection::default()
        }
    }

    fn bastion_conn() -> Connection {
        Connection {
            name: "bastion".into(),
            host: "10.0.0.1".into(),
            port: 2222,
            user: "jump".into(),
            ..Connection::default()
        }
    }

    #[test]
    fn fingerprint_changes_on_every_security_relevant_target_field() {
        let base = base_conn();
        let bastion = bastion_conn();
        let conns = vec![base.clone(), bastion.clone()];
        let fp = GrantFingerprint::for_connection(&base, &conns);

        // Host change.
        let mut changed_host = base.clone();
        changed_host.host = "10.0.0.99".into();
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(
                &changed_host,
                &[changed_host.clone(), bastion.clone()]
            )
        );

        // Port change.
        let mut changed_port = base.clone();
        changed_port.port = 2222;
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(
                &changed_port,
                &[changed_port.clone(), bastion.clone()]
            )
        );

        // User change.
        let mut changed_user = base.clone();
        changed_user.user = "root".into();
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(
                &changed_user,
                &[changed_user.clone(), bastion.clone()]
            )
        );

        // ProxyCommand change (including Some vs None).
        let mut no_proxy = base.clone();
        no_proxy.proxy_command = None;
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(&no_proxy, &[no_proxy.clone(), bastion.clone()])
        );

        // Resolved ProxyJump hop change (bastion edited while staging itself is untouched).
        let mut edited_bastion = bastion.clone();
        edited_bastion.host = "10.0.0.2".into();
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(&base, &[base.clone(), edited_bastion])
        );

        // JumpHop identity_file change directly on HostEntry.
        let mut entry = base.entry_in(&conns);
        let fp_entry = GrantFingerprint::for_host_entry(&entry);
        assert_eq!(fp, fp_entry);
        entry.proxy_jump.push(JumpHop {
            host: "extra".into(),
            port: 22,
            user: None,
            identity_files: vec![PathBuf::from("/tmp/id_ed25519")],
        });
        assert_ne!(fp_entry, GrantFingerprint::for_host_entry(&entry));
    }

    #[test]
    fn verify_evicts_mismatched_grant_and_prune_stale_handles_edits_renames_and_deletes() {
        let staging = base_conn();
        let bastion = bastion_conn();
        let conns = vec![staging.clone(), bastion.clone()];
        let fp = GrantFingerprint::for_connection(&staging, &conns);

        let mut store = GrantStore::new();
        store.insert("staging", fp, SystemTime::UNIX_EPOCH);
        assert_eq!(store.len(), 1);
        assert!(store.verify("staging", fp));

        let mut edited = staging.clone();
        edited.host = "10.0.0.99".into();
        let new_fp = GrantFingerprint::for_connection(&edited, &[edited.clone(), bastion.clone()]);
        assert!(!store.verify("staging", new_fp));
        assert!(store.is_empty());

        store.insert("staging", fp, SystemTime::UNIX_EPOCH);
        assert_eq!(store.prune_stale(&conns), 0);
        assert_eq!(store.prune_stale(&[edited, bastion.clone()]), 1);
        assert!(store.is_empty());

        // Deleting the host removes the Grant.
        store.insert("staging", fp, SystemTime::UNIX_EPOCH);
        assert_eq!(store.prune_stale(&[bastion]), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn revoke_and_revoke_all_clear_targeted_and_all_grants() {
        let staging = base_conn();
        let bastion = bastion_conn();
        let conns = vec![staging.clone(), bastion.clone()];
        let fp_s = GrantFingerprint::for_connection(&staging, &conns);
        let fp_b = GrantFingerprint::for_connection(&bastion, &conns);

        let mut store = GrantStore::new();
        store.insert("staging", fp_s, SystemTime::UNIX_EPOCH);
        store.insert("bastion", fp_b, SystemTime::UNIX_EPOCH);
        assert_eq!(store.active().len(), 2);

        assert!(store.revoke("staging"));
        assert!(!store.revoke("staging"));
        assert_eq!(store.active().len(), 1);
        assert_eq!(store.active()[0].host, "bastion");

        assert_eq!(store.revoke_all(), 1);
        assert!(store.is_empty());
    }
}
