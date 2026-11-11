//! The open tabs, kept in `tabs.json` next to the settings so the next launch can reopen them.
//! It holds names and order only: which host, a custom title, the active slot. No secret, no
//! command, no terminal contents.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "tabs.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SavedKind {
    /// A connection, by the alias or `user@host:port` it was opened with.
    Ssh { alias: String },
    /// A kind this version does not know, such as the local shell tabs older versions kept.
    /// It is dropped when the file is read and never written.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    #[serde(flatten)]
    pub kind: SavedKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTabs {
    pub tabs: Vec<SavedTab>,
    pub active: usize,
}

impl SavedTabs {
    /// The tabs of the last run. A missing file, or one that does not parse, is no tabs: it is
    /// left where it is, and only the next change replaces it.
    pub fn load(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join(FILE_NAME)).ok()?;
        match serde_json::from_str::<Self>(&text) {
            Ok(saved) => Some(saved.without_unknown()),
            Err(e) => {
                tracing::warn!(error = %e, "tabs_json_unreadable_ignored");
                None
            }
        }
    }

    /// Drops tabs of a kind this version cannot open. The active slot keeps pointing at the
    /// same tab, or at the one that took the dropped tab's place.
    fn without_unknown(mut self) -> Self {
        let dropped_before_active = self
            .tabs
            .iter()
            .take(self.active)
            .filter(|t| t.kind == SavedKind::Unknown)
            .count();
        self.active = self.active.saturating_sub(dropped_before_active);
        self.tabs.retain(|t| t.kind != SavedKind::Unknown);
        self
    }

    /// Temp file + rename, so a crash mid-write never leaves half a file.
    ///
    /// # Errors
    ///
    /// When the directory or file cannot be written.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_string_pretty(self).map_err(io::Error::other)?,
        )?;
        std::fs::rename(&tmp, &path)
    }

    /// The active slot, held to a tab that exists.
    pub fn active_slot(&self) -> usize {
        self.active.min(self.tabs.len().saturating_sub(1))
    }
}

/// `tabs.json` in the settings folder; a failed write is logged, not fatal. It runs on every
/// change, never at launch, so an unreadable file survives until the user changes something.
pub struct DiskTabs;

impl crate::tab_set::TabStore for DiskTabs {
    fn save(&self, tabs: &SavedTabs) {
        let Some(dir) = crate::settings::dir() else {
            return;
        };
        if let Err(e) = tabs.save(&dir) {
            tracing::warn!(error = %e, "tabs_save_failed");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample() -> SavedTabs {
        SavedTabs {
            tabs: vec![
                SavedTab {
                    kind: SavedKind::Ssh {
                        alias: "prod".into(),
                    },
                    title: Some("prod db".into()),
                },
                SavedTab {
                    kind: SavedKind::Ssh {
                        alias: "staging".into(),
                    },
                    title: None,
                },
                SavedTab {
                    kind: SavedKind::Ssh {
                        alias: "root@10.0.0.2:2222".into(),
                    },
                    title: None,
                },
            ],
            active: 1,
        }
    }

    #[test]
    fn tabs_round_trip_with_order_titles_and_the_active_slot() {
        let dir = tempfile::tempdir().unwrap();
        sample().save(dir.path()).unwrap();
        assert_eq!(SavedTabs::load(dir.path()), Some(sample()));
    }

    fn ssh(alias: &str) -> SavedTab {
        SavedTab {
            kind: SavedKind::Ssh {
                alias: alias.into(),
            },
            title: None,
        }
    }

    #[test]
    fn a_file_with_old_local_tabs_loads_and_skips_them() {
        let old = |active: usize| {
            format!(
                r#"{{"tabs":[{{"kind":"ssh","alias":"a"}},{{"kind":"local","title":"mine"}},{{"kind":"ssh","alias":"b"}}],"active":{active}}}"#
            )
        };
        for (active, expect) in [(0, 0), (1, 1), (2, 1)] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(FILE_NAME), old(active)).unwrap();
            let loaded = SavedTabs::load(dir.path()).unwrap();
            assert_eq!(loaded.tabs, [ssh("a"), ssh("b")]);
            assert_eq!(loaded.active, expect, "active {active}");
        }
    }

    #[test]
    fn a_missing_file_is_no_tabs() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(SavedTabs::load(dir.path()), None);
    }

    #[test]
    fn a_corrupt_file_is_ignored_and_left_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(SavedTabs::load(dir.path()), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        // The next change replaces it.
        sample().save(dir.path()).unwrap();
        assert_eq!(SavedTabs::load(dir.path()), Some(sample()));
    }

    #[test]
    fn the_file_holds_names_only() {
        let dir = tempfile::tempdir().unwrap();
        sample().save(dir.path()).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("\"kind\": \"ssh\""), "{text}");
        assert!(!text.contains("unknown"), "{text}");
        for secret in ["password", "passphrase", "identity", "key"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }

    #[test]
    fn a_stale_active_slot_is_held_inside_the_list() {
        let mut saved = sample();
        saved.active = 9;
        assert_eq!(saved.active_slot(), 2);
        assert_eq!(SavedTabs::default().active_slot(), 0);
    }
}
