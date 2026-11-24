//! Local-only Agent configuration in `agents.json` (mode `0o600`, strictly excluded from
//! `tern-sync`). Stores MCP enablement, pause state, and per-Saved-connection allowed systemd
//! service unit names. Refuses unreadable or newer-schema files without overwriting them.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::connections::Connection;

/// File name inside the settings directory. Never added to `tern-sync`.
pub const FILE_NAME: &str = "agents.json";
/// Current on-disk schema version.
pub const VERSION: u32 = 1;
/// Maximum permitted systemd unit name length (`UNIT_NAME_MAX`).
pub const MAX_UNIT_NAME_LEN: usize = 256;

/// Local-only Agent broker settings and per-Saved-connection allowed systemd service units.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentConfig {
    pub enabled: bool,
    pub paused: bool,
    /// Allowed systemd service unit names keyed by **Saved connection** name (`hosts.json`).
    pub hosts: BTreeMap<String, Vec<String>>,
}

/// Why loading, validating, or saving `agents.json` failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("agents.json could not be read ({0}); fix or move it, tern will not overwrite it")]
    Unreadable(String),
    #[error("invalid systemd unit name: {0}")]
    InvalidUnitName(String),
    #[error("host is not a saved connection: {0}")]
    UnknownSavedConnection(String),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Returns `true` when `unit` is a safe systemd unit name: non-empty, at most
/// [`MAX_UNIT_NAME_LEN`] bytes, ASCII `[A-Za-z0-9:_@.-]` only, and never starting with `-`.
#[must_use]
pub fn is_valid_unit_name(unit: &str) -> bool {
    !unit.is_empty()
        && unit.len() <= MAX_UNIT_NAME_LEN
        && !unit.starts_with('-')
        && unit
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'@' | b'.' | b'-'))
}

/// Validates a systemd unit name.
///
/// # Errors
/// Returns [`StoreError::InvalidUnitName`] when `unit` is empty, exceeds [`MAX_UNIT_NAME_LEN`],
/// starts with `-`, or contains any character outside ASCII `[A-Za-z0-9:_@.-]`.
pub fn validate_unit_name(unit: &str) -> Result<(), StoreError> {
    if is_valid_unit_name(unit) {
        Ok(())
    } else {
        Err(StoreError::InvalidUnitName(unit.to_owned()))
    }
}

/// Validates that `host` is a Saved connection and returns its validated, deduplicated unit names.
fn validate_host_services<S: AsRef<str> + Into<String>>(
    host: &str,
    services: impl IntoIterator<Item = S>,
    saved_connections: &[Connection],
) -> Result<Vec<String>, StoreError> {
    if !saved_connections.iter().any(|c| c.name == host) {
        return Err(StoreError::UnknownSavedConnection(host.to_owned()));
    }
    let mut deduped = Vec::new();
    for service in services {
        validate_unit_name(service.as_ref())?;
        if !deduped
            .iter()
            .any(|existing: &String| existing == service.as_ref())
        {
            deduped.push(service.into());
        }
    }
    Ok(deduped)
}

impl AgentConfig {
    /// Replaces the allowed systemd service units for Saved connection `host`.
    /// Passing an empty list removes `host` from the allowlist.
    ///
    /// # Errors
    /// Returns [`StoreError::UnknownSavedConnection`] if `host` is not in `saved_connections`
    /// (rejecting `~/.ssh/config`-only Hosts), or [`StoreError::InvalidUnitName`] if any service
    /// name fails [`validate_unit_name`].
    pub fn set_allowed_services(
        &mut self,
        host: &str,
        services: Vec<String>,
        saved_connections: &[Connection],
    ) -> Result<(), StoreError> {
        let deduped = validate_host_services(host, services, saved_connections)?;
        if deduped.is_empty() {
            self.hosts.remove(host);
        } else {
            self.hosts.insert(host.to_owned(), deduped);
        }
        Ok(())
    }

    /// Drops any allowlisted host entries whose alias no longer exists in `saved_connections` so
    /// recreating a deleted or renamed Saved connection under the same alias never inherits stale
    /// permissions. Returns how many host entries were removed.
    pub fn prune_unknown_hosts(&mut self, saved_connections: &[Connection]) -> usize {
        let before = self.hosts.len();
        self.hosts
            .retain(|host, _| saved_connections.iter().any(|c| c.name == *host));
        before - self.hosts.len()
    }

    /// Returns only Saved connections that currently exist in `saved_connections` and have at
    /// least one valid allowed service unit, in `saved_connections` order.
    #[must_use]
    pub fn allowed_hosts(&self, saved_connections: &[Connection]) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        for conn in saved_connections {
            let Some(configured) = self.hosts.get(&conn.name) else {
                continue;
            };
            let mut valid = Vec::new();
            for unit in configured {
                if is_valid_unit_name(unit) && !valid.contains(unit) {
                    valid.push(unit.clone());
                }
            }
            if !valid.is_empty() {
                out.push((conn.name.clone(), valid));
            }
        }
        out
    }

    /// Returns `true` only when `host` is a Saved connection in `saved_connections`, `service` is
    /// a valid systemd unit name, and `service` is allowlisted on `host`.
    #[must_use]
    pub fn is_service_allowed(
        &self,
        host: &str,
        service: &str,
        saved_connections: &[Connection],
    ) -> bool {
        is_valid_unit_name(service)
            && saved_connections.iter().any(|c| c.name == host)
            && self
                .hosts
                .get(host)
                .is_some_and(|services| services.iter().any(|s| s == service))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiskFile {
    version: u32,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    hosts: BTreeMap<String, Vec<String>>,
}

/// Reads `agents.json` from `dir`. A missing file yields [`AgentConfig::default`]; a corrupt or
/// newer-schema file returns [`StoreError::Unreadable`] without touching disk.
///
/// # Errors
/// Returns [`StoreError::Unreadable`] if the file is malformed or has an unsupported schema
/// version, or [`StoreError::Io`] on filesystem read failure.
pub fn load(dir: &Path) -> Result<AgentConfig, StoreError> {
    let text = match std::fs::read_to_string(dir.join(FILE_NAME)) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(AgentConfig::default()),
        Err(e) => return Err(e.into()),
    };
    let file: DiskFile =
        serde_json::from_str(&text).map_err(|e| StoreError::Unreadable(e.to_string()))?;
    if file.version != VERSION {
        return Err(StoreError::Unreadable(format!("version {}", file.version)));
    }
    Ok(AgentConfig {
        enabled: file.enabled,
        paused: file.paused,
        hosts: file.hosts,
    })
}

/// Atomically writes `agents.json` with mode `0o600` (temp file + rename) after validating every
/// host against `saved_connections` and every service unit name.
///
/// Refuses to overwrite an existing unreadable or newer-schema `agents.json`.
///
/// # Errors
/// Returns [`StoreError::Unreadable`] if an existing `agents.json` cannot be read or has an
/// unsupported version, [`StoreError::UnknownSavedConnection`] if any configured host is not in
/// `saved_connections`, [`StoreError::InvalidUnitName`] if any service name is invalid, or
/// [`StoreError::Io`] on filesystem write failure.
pub fn save(
    dir: &Path,
    config: &AgentConfig,
    saved_connections: &[Connection],
) -> Result<(), StoreError> {
    // Guard against clobbering a corrupt or newer-schema file already on disk.
    let _ = load(dir)?;

    let mut clean_hosts = BTreeMap::new();
    for (host, services) in &config.hosts {
        let deduped =
            validate_host_services(host, services.iter().map(String::as_str), saved_connections)?;
        if !deduped.is_empty() {
            clean_hosts.insert(host.clone(), deduped);
        }
    }

    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE_NAME);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&DiskFile {
        version: VERSION,
        enabled: config.enabled,
        paused: config.paused,
        hosts: clean_hosts,
    })
    .map_err(io::Error::other)?;

    write_private(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn saved(names: &[&str]) -> Vec<Connection> {
        names
            .iter()
            .map(|name| Connection {
                name: (*name).to_owned(),
                host: format!("{name}.internal"),
                port: 22,
                user: "deploy".to_owned(),
                ..Connection::default()
            })
            .collect()
    }

    #[test]
    fn unit_name_validation_accepts_systemd_chars_and_rejects_flags_and_shell_metachars() {
        for valid in [
            "api.service",
            "worker@1.service",
            "postgresql@16-main.service",
            "app_web:prod.service",
            "A0-._:@",
            &"a".repeat(MAX_UNIT_NAME_LEN),
        ] {
            assert!(is_valid_unit_name(valid), "should accept {valid}");
            assert!(validate_unit_name(valid).is_ok());
        }

        for invalid in [
            "",
            "-p",
            "--all",
            "-flag.service",
            "api.service; id",
            "api service",
            "api\nservice",
            "api\tservice",
            "api/service",
            "api\\service",
            "$(whoami)",
            "api`id`",
            "api$HOME",
            "api|cat",
            "api&bg",
            "café.service",
            &"a".repeat(MAX_UNIT_NAME_LEN + 1),
        ] {
            assert!(!is_valid_unit_name(invalid), "should reject {invalid:?}");
            assert!(matches!(
                validate_unit_name(invalid),
                Err(StoreError::InvalidUnitName(_))
            ));
        }
    }

    #[test]
    fn missing_file_loads_as_disabled_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(dir.path()).unwrap();
        assert_eq!(cfg, AgentConfig::default());
        assert!(!cfg.enabled);
        assert!(!cfg.paused);
        assert!(cfg.hosts.is_empty());
    }

    #[test]
    fn save_round_trips_with_0600_mode_and_deduplicates_services() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = saved(&["staging", "prod"]);
        let mut cfg = AgentConfig {
            enabled: true,
            paused: false,
            hosts: BTreeMap::new(),
        };
        cfg.set_allowed_services(
            "staging",
            vec![
                "api.service".into(),
                "worker@1.service".into(),
                "api.service".into(),
            ],
            &hosts,
        )
        .unwrap();

        save(dir.path(), &cfg, &hosts).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        assert!(
            !dir.path().join("agents.json.tmp").exists(),
            "temp file must be renamed into place"
        );

        let loaded = load(dir.path()).unwrap();
        assert!(loaded.enabled);
        assert!(!loaded.paused);
        assert_eq!(
            loaded.allowed_hosts(&hosts),
            vec![(
                "staging".to_owned(),
                vec!["api.service".to_owned(), "worker@1.service".to_owned()]
            )]
        );
        assert!(loaded.is_service_allowed("staging", "api.service", &hosts));
        assert!(loaded.is_service_allowed("staging", "worker@1.service", &hosts));
        assert!(!loaded.is_service_allowed("staging", "db.service", &hosts));
        assert!(!loaded.is_service_allowed("prod", "api.service", &hosts));
    }

    #[test]
    fn rejects_hosts_not_in_saved_connections_and_filters_deleted_or_invalid_entries() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = saved(&["staging"]);
        let mut cfg = AgentConfig::default();

        // ~/.ssh/config-only host is rejected when setting or saving.
        assert!(matches!(
            cfg.set_allowed_services("ssh-config-only", vec!["api.service".into()], &hosts),
            Err(StoreError::UnknownSavedConnection(h)) if h == "ssh-config-only"
        ));

        cfg.hosts
            .insert("ssh-config-only".into(), vec!["api.service".into()]);
        assert!(matches!(
            save(dir.path(), &cfg, &hosts),
            Err(StoreError::UnknownSavedConnection(h)) if h == "ssh-config-only"
        ));

        // Even if hand-edited on disk, lookup ignores non-Saved hosts and invalid unit names.
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{
                "version": 1,
                "enabled": true,
                "hosts": {
                    "ssh-config-only": ["api.service"],
                    "staging": ["api.service", "-bad", "evil;id", "api.service"]
                }
            }"#,
        )
        .unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(
            loaded.allowed_hosts(&hosts),
            vec![("staging".to_owned(), vec!["api.service".to_owned()])]
        );
        assert!(!loaded.is_service_allowed("ssh-config-only", "api.service", &hosts));
        assert!(!loaded.is_service_allowed("staging", "-bad", &hosts));
        assert!(!loaded.is_service_allowed("staging", "evil;id", &hosts));
        assert!(loaded.is_service_allowed("staging", "api.service", &hosts));

        // Removing all services for a host drops it from the allowlist.
        let mut cleaned = AgentConfig::default();
        cleaned
            .set_allowed_services("staging", vec!["api.service".into()], &hosts)
            .unwrap();
        cleaned
            .set_allowed_services("staging", vec![], &hosts)
            .unwrap();
        assert!(cleaned.hosts.is_empty());
        assert!(cleaned.allowed_hosts(&hosts).is_empty());
    }

    #[test]
    fn save_validates_unit_names_and_drops_empty_host_entries() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = saved(&["staging", "empty-host"]);
        let mut cfg = AgentConfig::default();
        cfg.hosts
            .insert("staging".into(), vec!["--bad-flag".into()]);
        assert!(matches!(
            save(dir.path(), &cfg, &hosts),
            Err(StoreError::InvalidUnitName(u)) if u == "--bad-flag"
        ));
        assert!(!dir.path().join(FILE_NAME).exists());

        cfg.hosts.clear();
        cfg.hosts.insert(
            "staging".into(),
            vec!["api.service".into(), "api.service".into()],
        );
        cfg.hosts.insert("empty-host".into(), vec![]);
        save(dir.path(), &cfg, &hosts).unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(loaded.hosts["staging"], vec!["api.service"]);
    }

    #[test]
    fn crash_mid_write_leaves_previous_good_file_intact() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = saved(&["staging"]);
        let mut cfg = AgentConfig {
            enabled: true,
            paused: false,
            hosts: BTreeMap::new(),
        };
        cfg.set_allowed_services("staging", vec!["api.service".into()], &hosts)
            .unwrap();
        save(dir.path(), &cfg, &hosts).unwrap();

        // Simulate a crash leaving a half-written temp file next to agents.json.
        std::fs::write(dir.path().join("agents.json.tmp"), "{truncated").unwrap();

        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded, cfg);

        // Next save recovers cleanly and tightens permissions on the reused tmp path.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                dir.path().join("agents.json.tmp"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        cfg.paused = true;
        save(dir.path(), &cfg, &hosts).unwrap();
        assert_eq!(load(dir.path()).unwrap(), cfg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(FILE_NAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn refuses_unreadable_and_newer_schema_files_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = saved(&["staging"]);
        let path = dir.path().join(FILE_NAME);

        // Newer schema version.
        let newer = r#"{"version": 2, "enabled": true, "hosts": {"staging": ["api.service"]}}"#;
        std::fs::write(&path, newer).unwrap();
        assert!(
            matches!(load(dir.path()), Err(StoreError::Unreadable(m)) if m.contains("version 2"))
        );
        assert!(matches!(
            save(dir.path(), &AgentConfig::default(), &hosts),
            Err(StoreError::Unreadable(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            newer,
            "newer-schema agents.json must not be clobbered"
        );

        // Version 0 (older/invalid).
        let v0 = r#"{"version": 0, "enabled": false}"#;
        std::fs::write(&path, v0).unwrap();
        assert!(matches!(load(dir.path()), Err(StoreError::Unreadable(_))));

        // Corrupt JSON.
        let corrupt = "{not valid json";
        std::fs::write(&path, corrupt).unwrap();
        assert!(matches!(load(dir.path()), Err(StoreError::Unreadable(_))));
        assert!(matches!(
            save(dir.path(), &AgentConfig::default(), &hosts),
            Err(StoreError::Unreadable(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            corrupt,
            "corrupt agents.json must not be clobbered"
        );

        // Non-NotFound I/O error (e.g. a directory at agents.json) returns Io error, not default.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(load(dir.path()), Err(StoreError::Io(_))));
    }
}
