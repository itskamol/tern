//! In-memory Agent Grants (`Allow until lock`) and canonical target fingerprinting (ADR 0003).
//!
//! Grants live strictly in memory — never serialized to `agents.json`, disk, or `tern-sync`.
//! Every Grant binds `(host_alias, service)` to a length-prefixed SHA-256 fingerprint over the
//! resolved target `(resolved_host, port, user, proxy_jump, proxy_command, service)`, re-verified
//! before every execution and pruned whenever Saved connections or allowed services change.

use std::collections::BTreeMap;
use std::time::SystemTime;

use sha2::{Digest as _, Sha256};
use tern_ssh::HostEntry;

use super::store::AgentConfig;
use crate::connections::Connection;

const FINGERPRINT_DOMAIN: &[u8] = b"tern-agent-grant-v1\0";

/// Key identifying a single in-memory Agent Grant: `(host_alias, service)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GrantKey {
    pub host: String,
    pub service: String,
}

impl GrantKey {
    #[must_use]
    pub fn new(host: impl Into<String>, service: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            service: service.into(),
        }
    }
}

/// Canonical SHA-256 fingerprint over `(resolved_host, port, user, proxy_jump, proxy_command,
/// service)` using length-prefixed encoding so field boundaries can never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GrantFingerprint([u8; 32]);

impl GrantFingerprint {
    /// Computes the canonical fingerprint for a Saved connection resolved against
    /// `saved_connections` (so `ProxyJump` references to other Saved connections resolve to their
    /// live address, port, user, and identity files) and `service`.
    #[must_use]
    pub fn for_connection(
        conn: &Connection,
        saved_connections: &[Connection],
        service: &str,
    ) -> Self {
        let entry = conn.entry_in(saved_connections);
        Self::for_host_entry(&entry, service)
    }

    /// Computes the canonical fingerprint directly from a resolved [`HostEntry`] and `service`.
    #[must_use]
    pub fn for_host_entry(entry: &HostEntry, service: &str) -> Self {
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
        write_str(&mut h, service);
        Self(h.finalize().into())
    }

    /// Raw 32-byte SHA-256 digest.
    #[must_use]
    #[allow(dead_code)] // Useful for tests and diagnostics.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
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

/// One active in-memory Grant letting Agents repeat `service_status` for `(host, service)` until
/// lock, sleep, session switch, quit, pause, manual revoke, or target edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub host: String,
    pub service: String,
    pub fingerprint: GrantFingerprint,
    pub granted_at: SystemTime,
}

/// In-memory-only store of active Agent Grants. Intentionally does not implement `Serialize` or
/// `Deserialize` so Grants can never be written to `agents.json` or `tern-sync`.
#[derive(Debug, Default)]
pub struct GrantStore {
    grants: BTreeMap<GrantKey, Grant>,
}

impl GrantStore {
    #[must_use]
    #[allow(dead_code)] // Constructed directly in tests and `AgentsUi`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records or replaces an in-memory Grant for `(host, service)`.
    pub fn insert(
        &mut self,
        host: impl Into<String>,
        service: impl Into<String>,
        fingerprint: GrantFingerprint,
        granted_at: SystemTime,
    ) {
        let host = host.into();
        let service = service.into();
        let key = GrantKey::new(host.clone(), service.clone());
        self.grants.insert(
            key,
            Grant {
                host,
                service,
                fingerprint,
                granted_at,
            },
        );
    }

    /// Re-verifies whether an active Grant exists for `(host, service)` whose stored fingerprint
    /// matches `current_fingerprint`. Any mismatched Grant is evicted immediately.
    pub fn verify(
        &mut self,
        host: &str,
        service: &str,
        current_fingerprint: GrantFingerprint,
    ) -> bool {
        let key = GrantKey::new(host, service);
        match self.grants.get(&key) {
            Some(grant) if grant.fingerprint == current_fingerprint => true,
            Some(_) => {
                self.grants.remove(&key);
                false
            }
            None => false,
        }
    }

    /// Immediately removes every Grant whose Saved connection was edited, renamed, or deleted, or
    /// whose service was removed from `config`. Returns how many Grants were invalidated.
    pub fn prune_stale(&mut self, config: &AgentConfig, connections: &[Connection]) -> usize {
        let before = self.grants.len();
        self.grants.retain(|_, grant| {
            if !config.is_service_allowed(&grant.host, &grant.service, connections) {
                return false;
            }
            let Some(conn) = connections.iter().find(|c| c.name == grant.host) else {
                return false;
            };
            let current = GrantFingerprint::for_connection(conn, connections, &grant.service);
            current == grant.fingerprint
        });
        before - self.grants.len()
    }

    /// Revokes one Grant by `(host, service)`. Returns `true` if a Grant was present.
    pub fn revoke(&mut self, host: &str, service: &str) -> bool {
        self.grants.remove(&GrantKey::new(host, service)).is_some()
    }

    /// Clears every active Grant. Returns how many Grants were removed.
    pub fn revoke_all(&mut self) -> usize {
        let count = self.grants.len();
        self.grants.clear();
        count
    }

    /// Read-only sorted snapshot of active Grants for Settings UI (#65) and inspection.
    #[must_use]
    pub fn active(&self) -> Vec<Grant> {
        self.grants.values().cloned().collect()
    }

    #[must_use]
    #[allow(dead_code)] // Used in tests and Settings UI (#65).
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    #[must_use]
    #[allow(dead_code)] // Used in tests and Settings UI (#65).
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
    fn fingerprint_is_deterministic_and_flips_on_every_target_field_change() {
        let base = base_conn();
        let bastion = bastion_conn();
        let all = vec![base.clone(), bastion.clone()];

        let fp = GrantFingerprint::for_connection(&base, &all, "api.service");
        assert_eq!(
            fp,
            GrantFingerprint::for_connection(&base, &all, "api.service")
        );

        // 1. resolved_host change
        let mut c = base.clone();
        c.host = "10.0.0.11".into();
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(&c, &all, "api.service")
        );

        // 2. port change
        let mut c = base.clone();
        c.port = 2200;
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(&c, &all, "api.service")
        );

        // 3. user change
        let mut c = base.clone();
        c.user = "root".into();
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(&c, &all, "api.service")
        );

        // 4. proxy_jump change (direct)
        let mut c = base.clone();
        c.proxy_jump = String::new();
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(&c, &all, "api.service")
        );

        // 5. proxy_jump change via referenced Saved connection edit (host/port/user/key)
        for mutate in [
            Box::new(|b: &mut Connection| b.host = "10.0.0.2".into())
                as Box<dyn Fn(&mut Connection)>,
            Box::new(|b: &mut Connection| b.port = 2223),
            Box::new(|b: &mut Connection| b.user = "other".into()),
            Box::new(|b: &mut Connection| b.identity_file = Some("/tmp/jump_key".into())),
        ] {
            let mut b2 = bastion.clone();
            mutate(&mut b2);
            let all2 = vec![base.clone(), b2];
            assert_ne!(
                fp,
                GrantFingerprint::for_connection(&base, &all2, "api.service")
            );
        }

        // 6. proxy_command change (edited, cleared, empty vs None)
        let mut c = base.clone();
        c.proxy_command = Some("nc -X connect %h %p".into());
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(&c, &all, "api.service")
        );

        let mut c_none = base.clone();
        c_none.proxy_command = None;
        let mut c_empty = base.clone();
        c_empty.proxy_command = Some(String::new());
        let fp_none = GrantFingerprint::for_connection(&c_none, &all, "api.service");
        let fp_empty = GrantFingerprint::for_connection(&c_empty, &all, "api.service");
        assert_ne!(fp, fp_none);
        assert_ne!(fp_none, fp_empty);

        // 7. service change
        assert_ne!(
            fp,
            GrantFingerprint::for_connection(&base, &all, "worker.service")
        );
    }

    #[test]
    fn fingerprint_length_prefixing_prevents_field_boundary_collisions() {
        // host="ab", user="c" vs host="a", user="bc"
        let e1 = HostEntry {
            host_name: "ab".into(),
            port: 22,
            user: Some("c".into()),
            ..HostEntry::default()
        };
        let e2 = HostEntry {
            host_name: "a".into(),
            port: 22,
            user: Some("bc".into()),
            ..HostEntry::default()
        };
        assert_ne!(
            GrantFingerprint::for_host_entry(&e1, "svc"),
            GrantFingerprint::for_host_entry(&e2, "svc")
        );

        // proxy_command="ab", service="c" vs proxy_command="a", service="bc"
        let e3 = HostEntry {
            host_name: "h".into(),
            port: 22,
            user: Some("u".into()),
            proxy_command: Some("ab".into()),
            ..HostEntry::default()
        };
        let e4 = HostEntry {
            host_name: "h".into(),
            port: 22,
            user: Some("u".into()),
            proxy_command: Some("a".into()),
            ..HostEntry::default()
        };
        assert_ne!(
            GrantFingerprint::for_host_entry(&e3, "c"),
            GrantFingerprint::for_host_entry(&e4, "bc")
        );

        // user=None vs user=Some("")
        let e_user_none = HostEntry {
            host_name: "h".into(),
            port: 22,
            user: None,
            ..HostEntry::default()
        };
        let e_user_empty = HostEntry {
            host_name: "h".into(),
            port: 22,
            user: Some(String::new()),
            ..HostEntry::default()
        };
        assert_ne!(
            GrantFingerprint::for_host_entry(&e_user_none, "svc"),
            GrantFingerprint::for_host_entry(&e_user_empty, "svc")
        );

        // proxy_jump hop field boundary: hop(host="ab", user=Some("c")) vs hop(host="a", user=Some("bc"))
        let hop_entry = |h: &str, u: &str, keys: Vec<&str>| HostEntry {
            host_name: "target".into(),
            port: 22,
            user: Some("deploy".into()),
            proxy_jump: vec![JumpHop {
                host: h.into(),
                port: 22,
                user: Some(u.into()),
                identity_files: keys.into_iter().map(PathBuf::from).collect(),
            }],
            ..HostEntry::default()
        };
        assert_ne!(
            GrantFingerprint::for_host_entry(&hop_entry("ab", "c", vec![]), "svc"),
            GrantFingerprint::for_host_entry(&hop_entry("a", "bc", vec![]), "svc")
        );
        // Multiple identity files vs concatenated single path
        assert_ne!(
            GrantFingerprint::for_host_entry(&hop_entry("j", "u", vec!["/a", "/b"]), "svc"),
            GrantFingerprint::for_host_entry(&hop_entry("j", "u", vec!["/a/b"]), "svc")
        );
        // Hop ordering matters
        let two_a = HostEntry {
            host_name: "target".into(),
            port: 22,
            user: Some("deploy".into()),
            proxy_jump: vec![
                JumpHop {
                    host: "j1".into(),
                    port: 22,
                    ..JumpHop::default()
                },
                JumpHop {
                    host: "j2".into(),
                    port: 22,
                    ..JumpHop::default()
                },
            ],
            ..HostEntry::default()
        };
        let two_b = HostEntry {
            host_name: "target".into(),
            port: 22,
            user: Some("deploy".into()),
            proxy_jump: vec![
                JumpHop {
                    host: "j2".into(),
                    port: 22,
                    ..JumpHop::default()
                },
                JumpHop {
                    host: "j1".into(),
                    port: 22,
                    ..JumpHop::default()
                },
            ],
            ..HostEntry::default()
        };
        assert_ne!(
            GrantFingerprint::for_host_entry(&two_a, "svc"),
            GrantFingerprint::for_host_entry(&two_b, "svc")
        );
    }

    #[test]
    fn grant_store_verifies_evicts_mismatches_prunes_and_revokes() {
        let mut store = GrantStore::new();
        let conn = base_conn();
        let bastion = bastion_conn();
        let conns = vec![conn.clone(), bastion.clone()];
        let mut cfg = AgentConfig {
            enabled: true,
            paused: false,
            hosts: BTreeMap::new(),
        };
        cfg.set_allowed_services(
            "staging",
            vec!["api.service".into(), "worker.service".into()],
            &conns,
        )
        .unwrap();

        let fp_api = GrantFingerprint::for_connection(&conn, &conns, "api.service");
        let fp_worker = GrantFingerprint::for_connection(&conn, &conns, "worker.service");
        let now = SystemTime::now();
        store.insert("staging", "api.service", fp_api, now);
        store.insert("staging", "worker.service", fp_worker, now);
        assert_eq!(store.len(), 2);
        assert_eq!(store.active().len(), 2);

        // Matching fingerprint verifies true.
        assert!(store.verify("staging", "api.service", fp_api));
        // Mismatched fingerprint returns false AND evicts the stale Grant.
        assert!(!store.verify("staging", "api.service", fp_worker));
        assert!(!store.verify("staging", "api.service", fp_api));
        assert_eq!(store.len(), 1);

        // Re-insert and prune when service removed from allowlist.
        store.insert("staging", "api.service", fp_api, now);
        cfg.set_allowed_services("staging", vec!["worker.service".into()], &conns)
            .unwrap();
        assert_eq!(store.prune_stale(&cfg, &conns), 1);
        assert!(!store.verify("staging", "api.service", fp_api));
        assert!(store.verify("staging", "worker.service", fp_worker));

        // Prune when connection port changes.
        let mut edited = conn.clone();
        edited.port = 2222;
        assert_eq!(store.prune_stale(&cfg, &[edited, bastion]), 1);
        assert!(store.is_empty());

        // Single revoke and revoke_all.
        store.insert("staging", "api.service", fp_api, now);
        store.insert("staging", "worker.service", fp_worker, now);
        assert!(store.revoke("staging", "api.service"));
        assert!(!store.revoke("staging", "api.service"));
        assert_eq!(store.len(), 1);
        assert_eq!(store.revoke_all(), 1);
        assert!(store.is_empty());
    }
}
