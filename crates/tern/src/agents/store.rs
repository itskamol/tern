//! Local-only Agent configuration in `agents.json` (mode `0o600`, strictly excluded from
//! `tern-sync`, ADR 0004). Stores MCP enablement and pause state only; refuses unreadable or
//! newer-schema files without overwriting them.

use std::io::{self, Write as _};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// File name inside the settings directory. Never added to `tern-sync`.
pub const FILE_NAME: &str = "agents.json";
/// Current on-disk schema version.
pub const VERSION: u32 = 1;

/// Local-only Agent broker settings (`enabled` and `paused`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentConfig {
    pub enabled: bool,
    pub paused: bool,
}

/// Why loading or saving `agents.json` failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("agents.json could not be read ({0}); fix or move it, tern will not overwrite it")]
    Unreadable(String),
    #[error("{0}")]
    Io(#[from] io::Error),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiskFile {
    version: u32,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    paused: bool,
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
    })
}

/// Atomically writes `agents.json` with mode `0o600` (temp file + rename).
///
/// Refuses to overwrite an existing unreadable or newer-schema `agents.json`.
///
/// # Errors
/// Returns [`StoreError::Unreadable`] if an existing `agents.json` cannot be read or has an
/// unsupported version, or [`StoreError::Io`] on filesystem write failure.
pub fn save(dir: &Path, config: &AgentConfig) -> Result<(), StoreError> {
    // Guard against clobbering a corrupt or newer-schema file already on disk.
    let _ = load(dir)?;

    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE_NAME);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(&DiskFile {
        version: VERSION,
        enabled: config.enabled,
        paused: config.paused,
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

    #[test]
    fn missing_file_loads_disabled_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(dir.path()).unwrap();
        assert_eq!(cfg, AgentConfig::default());
        assert!(!cfg.enabled);
        assert!(!cfg.paused);
    }

    #[test]
    fn save_and_load_round_trip_with_0600_mode_and_ignores_legacy_hosts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{"version":1,"enabled":true,"paused":false,"hosts":{"prod":["api.service"]}}"#,
        )
        .unwrap();

        let loaded = load(dir.path()).unwrap();
        assert_eq!(
            loaded,
            AgentConfig {
                enabled: true,
                paused: false,
            }
        );

        let updated = AgentConfig {
            enabled: true,
            paused: true,
        };
        save(dir.path(), &updated).unwrap();
        assert_eq!(load(dir.path()).unwrap(), updated);

        let raw = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(!raw.contains("hosts"));

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
    fn corrupt_or_newer_version_is_refused_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{ not valid json").unwrap();
        assert!(matches!(load(dir.path()), Err(StoreError::Unreadable(_))));
        assert!(matches!(
            save(dir.path(), &AgentConfig::default()),
            Err(StoreError::Unreadable(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not valid json");

        let newer = format!(r#"{{"version":{},"enabled":true}}"#, VERSION + 1);
        std::fs::write(&path, &newer).unwrap();
        assert!(matches!(load(dir.path()), Err(StoreError::Unreadable(_))));
        assert!(matches!(
            save(dir.path(), &AgentConfig::default()),
            Err(StoreError::Unreadable(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }
}
