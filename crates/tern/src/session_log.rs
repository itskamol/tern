//! A tab's output written to a plain-text file: what the remote sent, with colours, cursor
//! moves and the other escape sequences removed, so the log reads in any editor.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use strip_ansi_escapes::Writer as Stripper;

/// `sessions` in the log directory, where every log goes.
pub fn directory() -> Option<PathBuf> {
    crate::settings::log_dir().map(|dir| dir.join("sessions"))
}

/// Creates the log folder `dir` (and any missing parent) readable by the user only, and takes
/// back any looser mode it or the files directly in it already have: a log can name hosts and
/// users, and a session log can show secrets. tracing's daily file cannot be given a mode of
/// its own, so the folder is what keeps it private.
///
/// # Errors
///
/// When the folder cannot be created or its mode cannot be set.
pub fn make_private(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        builder.mode(0o700);
        builder.create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        for entry in std::fs::read_dir(dir)?.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let wanted = if meta.is_dir() { 0o700 } else { 0o600 };
            if meta.permissions().mode() & 0o077 != 0 {
                let _ =
                    std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(wanted));
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        builder.create(dir)
    }
}

pub struct SessionLog {
    out: Stripper<BufWriter<File>>,
    path: PathBuf,
}

impl std::fmt::Debug for SessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLog")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SessionLog {
    /// Creates `<dir>/<alias>-<timestamp>.log`, readable by the user only: a session can
    /// show secrets.
    ///
    /// # Errors
    ///
    /// When the directory or file cannot be created.
    pub fn create(dir: &Path, alias: &str, now: SystemTime) -> io::Result<Self> {
        make_private(dir)?;
        let stamp = timestamp(now);
        let path = dir.join(format!("{}-{stamp}.log", file_stem(alias)));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&path)?;
        let mut out = Stripper::new(BufWriter::new(file));
        writeln!(out, "# tern session log: {alias}, started {stamp}")?;
        out.flush()?;
        Ok(Self { out, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends output as it arrived; a sequence split between two calls is still removed.
    ///
    /// # Errors
    ///
    /// When the file cannot be written; the caller stops logging.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.out.flush()
    }
}

/// A host alias as a file name: letters, digits, `.`, `-` and `_` stay; anything else (a
/// slash, `@`, `:`) becomes `_`, so no alias can name a path outside the log directory.
pub fn file_stem(alias: &str) -> String {
    let stem: String = alias
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_start_matches('.');
    if stem.is_empty() {
        "session".into()
    } else {
        stem.to_owned()
    }
}

/// `20261006T122200Z`: UTC, sortable, and safe in a file name.
pub fn timestamp(now: SystemTime) -> String {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Howard Hinnant's days-since-epoch to calendar date.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn timestamps_are_utc_calendar_dates() {
        assert_eq!(timestamp(at(0)), "19700101T000000Z");
        // 2026-10-06 12:22:00 UTC
        assert_eq!(timestamp(at(1_791_289_320)), "20261006T122200Z");
        // A leap day, and the day after it.
        assert_eq!(timestamp(at(1_709_207_999)), "20240229T115959Z");
        assert_eq!(timestamp(at(1_709_294_400)), "20240301T120000Z");
    }

    #[test]
    fn aliases_cannot_leave_the_log_directory() {
        assert_eq!(file_stem("prod-db_1.example"), "prod-db_1.example");
        assert_eq!(file_stem("root@10.0.0.2:2222"), "root_10.0.0.2_2222");
        assert_eq!(file_stem("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(file_stem(".."), "session");
        assert_eq!(file_stem(""), "session");
    }

    #[test]
    fn the_log_is_plain_text_even_when_a_sequence_is_split_between_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = SessionLog::create(dir.path(), "prod", at(0)).unwrap();
        log.write(b"\x1b[1;3").unwrap();
        log.write(b"1mred\x1b[0m text\r\n").unwrap();
        log.write(b"\x1b]0;window title\x07next line\r\n").unwrap();
        log.write(b"\x1b[2J\x1b[Hdone\r\n").unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(
            log.path().ends_with("prod-19700101T000000Z.log"),
            "{:?}",
            log.path()
        );
        assert_eq!(
            text,
            "# tern session log: prod, started 19700101T000000Z\n\
             red text\nnext line\ndone\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_and_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::create(dir.path(), "prod", at(0)).unwrap();
        let mode = std::fs::metadata(log.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(SessionLog::create(dir.path(), "prod", at(0)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_log_folder_is_made_private_and_takes_back_loose_modes() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("Logs/tern");
        make_private(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);

        let old = dir.join("tern.2026-11-04.log");
        std::fs::write(&old, "x").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        make_private(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&old), 0o600);
    }
}
