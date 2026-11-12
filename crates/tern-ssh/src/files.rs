//! The file source: the one seam panels and the transfer queue see. A file source lists a folder,
//! reads and writes files as streams, makes folders, renames, deletes and names its home folder.
//! Two adapters stand behind it, the Mac's own file system (`files_local`) and an SFTP connection
//! (`files_sftp`); the queue copies between any two of them without knowing which is which.
//!
//! Paths are `/`-separated strings on both sides. Streams must be used on the tokio runtime the
//! source was made on.
use std::cmp::Reverse;
use std::fmt;
use std::time::SystemTime;

use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite};

pub use crate::files_local::LocalFs;
pub use crate::files_sftp::SftpFiles;

/// What a copy in progress is called on the destination until it is whole.
pub const PART_SUFFIX: &str = ".part";

/// A file's bytes, read front to back.
pub type Reader = Box<dyn AsyncRead + Send + Unpin>;
/// A file being written; `shutdown` finishes it.
pub type Writer = Box<dyn AsyncWrite + Send + Unpin>;
/// What every file source operation returns.
pub type Call<'a, T> = BoxFuture<'a, Result<T, FileError>>;

/// Why a file operation failed, worded for the person using the app.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    #[error("{0}: no such file or directory")]
    NotFound(String),
    #[error("{0}: permission denied")]
    PermissionDenied(String),
    #[error("{0}: already exists")]
    AlreadyExists(String),
    /// The host behind the source stopped answering.
    #[error("the host stopped answering")]
    Closed,
    #[error("{0}")]
    Other(String),
}

/// One folder entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    /// The folder listed, joined with `name`.
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// A place files live: the Mac, or one host.
pub trait FileSource: Send + Sync + fmt::Debug {
    /// Names the place: copies onto the same id run one at a time.
    fn id(&self) -> &str;

    /// The folder a person starts in.
    fn home(&self) -> Call<'_, String>;

    /// The entries of a folder, folders first, then by name without regard to case.
    fn list<'a>(&'a self, dir: &'a str) -> Call<'a, Vec<FileEntry>>;

    /// Opens a file for reading.
    fn read<'a>(&'a self, path: &'a str) -> Call<'a, Reader>;

    /// Opens a file for writing, creating or emptying it. The queue always passes a temporary
    /// name.
    fn write<'a>(&'a self, path: &'a str) -> Call<'a, Writer>;

    /// Makes one folder; [`FileError::AlreadyExists`] when the name is taken.
    fn mkdir<'a>(&'a self, path: &'a str) -> Call<'a, ()>;

    /// Renames a file or folder; [`FileError::AlreadyExists`] rather than replacing `to`.
    fn rename<'a>(&'a self, from: &'a str, to: &'a str) -> Call<'a, ()>;

    /// Removes a file, or a folder with everything in it. The Mac moves it to the Trash; a host
    /// deletes for good, and a `.part` file ([`PART_SUFFIX`]) is always removed outright, so an
    /// abandoned copy never fills the Trash.
    fn delete<'a>(&'a self, path: &'a str) -> Call<'a, ()>;
}

/// `dir` and `name` as one path.
#[must_use]
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The folder above a path; the root is its own parent.
#[must_use]
pub fn parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(at) => trimmed[..at].to_owned(),
    }
}

/// The last part of a path.
#[must_use]
pub fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// Extensions that wrap another: `dump.sql.gz` is still SQL, so the number goes before `.sql`.
const COMPRESSION: [&str; 5] = ["gz", "bz2", "xz", "zst", "lz4"];

/// `name` made unique the way Finder does it: `a.txt` becomes `a 2.txt`, then `a 3.txt`, and a
/// folder `docs` becomes `docs 2`. A compression extension stays with the word extension it
/// wraps, so `dump.sql.gz` becomes `dump 2.sql.gz` but `api.log.1.gz` becomes `api.log.1 2.gz`.
/// `taken` says whether a name is in use.
#[must_use]
pub fn keep_both(name: &str, is_dir: bool, taken: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(at) if at > 0 && !is_dir => split_extension(name, at),
        _ => (name, ""),
    };
    (2u64..)
        .map(|n| format!("{stem} {n}{ext}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| name.to_owned())
}

/// `name` as stem and extension, given the byte index of its last dot. `.sql.gz` is one
/// extension, `.1.gz` is not: only a word wrapped in compression counts.
fn split_extension(name: &str, last: usize) -> (&str, &str) {
    let (stem, ext) = name.split_at(last);
    if !COMPRESSION
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&ext[1..]))
    {
        return (stem, ext);
    }
    match stem.rfind('.') {
        Some(at)
            if at > 0
                && stem.len() > at + 1
                && stem[at + 1..].bytes().all(|b| b.is_ascii_alphabetic()) =>
        {
            name.split_at(at)
        }
        _ => (stem, ext),
    }
}

/// Folders first, then names, ignoring case.
pub(crate) fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by_cached_key(|e| (Reverse(e.is_dir), e.name.to_lowercase()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_join_split_and_name() {
        assert_eq!(join("/a", "b"), "/a/b");
        assert_eq!(join("/", "b"), "/b");
        assert_eq!(parent("/a/b/"), "/a");
        assert_eq!(parent("/a"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(file_name("/a/b.txt"), "b.txt");
    }

    #[test]
    fn keep_both_numbers_before_the_extension_and_skips_taken_names() {
        let taken = |n: &str| ["a 2.txt", "docs 2"].contains(&n);
        assert_eq!(keep_both("a.txt", false, taken), "a 3.txt");
        assert_eq!(keep_both("docs", true, taken), "docs 3");
        assert_eq!(keep_both("v1.2", true, |_| false), "v1.2 2");
        assert_eq!(keep_both(".env", false, |_| false), ".env 2");
        assert_eq!(keep_both("x", false, |_| false), "x 2");
    }

    #[test]
    fn keep_both_keeps_a_compressed_word_extension_with_its_compression() {
        let free = |_: &str| false;
        assert_eq!(keep_both("dump.sql.gz", false, free), "dump 2.sql.gz");
        assert_eq!(keep_both("release.tar.gz", false, free), "release 2.tar.gz");
        assert_eq!(keep_both("a.tar.bz2", false, free), "a 2.tar.bz2");
        assert_eq!(keep_both("a.tar.xz", false, free), "a 2.tar.xz");
        assert_eq!(keep_both("a.tar.zst", false, free), "a 2.tar.zst");
        assert_eq!(keep_both("a.tar.lz4", false, free), "a 2.tar.lz4");
        assert_eq!(keep_both("DUMP.SQL.GZ", false, free), "DUMP 2.SQL.GZ");
        assert_eq!(keep_both("api.log.1.gz", false, free), "api.log.1 2.gz");
        assert_eq!(keep_both("a..gz", false, free), "a. 2.gz");
        assert_eq!(keep_both(".env.gz", false, free), ".env 2.gz");
        assert_eq!(keep_both("notes.gz", false, free), "notes 2.gz");
        assert_eq!(keep_both("a.sql.zip", false, free), "a.sql 2.zip");
        assert_eq!(keep_both("x.tar.gz", true, free), "x.tar.gz 2");
        let taken = |n: &str| n == "dump 2.sql.gz";
        assert_eq!(keep_both("dump.sql.gz", false, taken), "dump 3.sql.gz");
    }
}
