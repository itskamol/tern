//! The Mac adapter of the file source: this machine's own file system. Deleting moves to the
//! Trash, so nothing a person removes here is lost for good.
//!
//! The Trash is `~/.Trash`: a rename puts the item there. On another volume a rename cannot, and
//! Finder is asked to move it instead (the same call `trash` does from a terminal).
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use futures::FutureExt;

use crate::files::{
    Call, FileEntry, FileError, FileSource, Reader, Writer, keep_both, sort_entries,
};

/// The Mac's files.
#[derive(Debug, Clone)]
pub struct LocalFs {
    trash: Option<PathBuf>,
}

impl LocalFs {
    /// The Mac, deleting into the user's `~/.Trash`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            trash: std::env::home_dir().map(|h| h.join(".Trash")),
        }
    }

    /// The Mac, deleting into `trash` instead of the user's Trash.
    #[must_use]
    pub fn with_trash(trash: PathBuf) -> Self {
        Self { trash: Some(trash) }
    }
}

impl Default for LocalFs {
    fn default() -> Self {
        Self::new()
    }
}

fn io_error(path: &str, e: &std::io::Error) -> FileError {
    match e.kind() {
        ErrorKind::NotFound => FileError::NotFound(path.to_owned()),
        ErrorKind::PermissionDenied => FileError::PermissionDenied(path.to_owned()),
        ErrorKind::AlreadyExists => FileError::AlreadyExists(path.to_owned()),
        _ => FileError::Other(format!("{path}: {e}")),
    }
}

async fn taken(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path).await.is_ok()
}

impl FileSource for LocalFs {
    fn id(&self) -> &str {
        "mac"
    }

    fn home(&self) -> Call<'_, String> {
        async move {
            let home =
                std::env::home_dir().ok_or_else(|| FileError::Other("no home folder".into()))?;
            Ok(home.to_string_lossy().into_owned())
        }
        .boxed()
    }

    fn list<'a>(&'a self, dir: &'a str) -> Call<'a, Vec<FileEntry>> {
        async move {
            let mut read = tokio::fs::read_dir(dir)
                .await
                .map_err(|e| io_error(dir, &e))?;
            let mut out = Vec::new();
            while let Some(entry) = read.next_entry().await.map_err(|e| io_error(dir, &e))? {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let path = crate::files::join(dir, &name);
                let Ok(link) = entry.metadata().await else {
                    continue;
                };
                let is_symlink = link.file_type().is_symlink();
                // A link is shown as what it points at, so a linked folder can be entered.
                let meta = if is_symlink {
                    tokio::fs::metadata(&path).await.unwrap_or(link)
                } else {
                    link
                };
                out.push(FileEntry {
                    name,
                    path,
                    is_dir: meta.is_dir(),
                    is_symlink,
                    size: meta.len(),
                    modified: meta.modified().ok(),
                });
            }
            sort_entries(&mut out);
            Ok(out)
        }
        .boxed()
    }

    fn read<'a>(&'a self, path: &'a str) -> Call<'a, Reader> {
        async move {
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|e| io_error(path, &e))?;
            Ok(Box::new(file) as Reader)
        }
        .boxed()
    }

    fn write<'a>(&'a self, path: &'a str) -> Call<'a, Writer> {
        async move {
            let file = tokio::fs::File::create(path)
                .await
                .map_err(|e| io_error(path, &e))?;
            Ok(Box::new(file) as Writer)
        }
        .boxed()
    }

    fn mkdir<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        async move {
            tokio::fs::create_dir(path)
                .await
                .map_err(|e| io_error(path, &e))
        }
        .boxed()
    }

    fn rename<'a>(&'a self, from: &'a str, to: &'a str) -> Call<'a, ()> {
        async move {
            if taken(Path::new(to)).await {
                return Err(FileError::AlreadyExists(to.to_owned()));
            }
            tokio::fs::rename(from, to)
                .await
                .map_err(|e| io_error(from, &e))
        }
        .boxed()
    }

    fn delete<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        async move {
            let from = Path::new(path);
            if !taken(from).await {
                return Err(FileError::NotFound(path.to_owned()));
            }
            let trash = self
                .trash
                .as_deref()
                .ok_or_else(|| FileError::Other("there is no Trash to move it to".into()))?;
            tokio::fs::create_dir_all(trash)
                .await
                .map_err(|e| io_error(path, &e))?;
            let name = from
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or_else(|| FileError::Other(format!("{path}: not a plain name")))?;
            let is_dir = tokio::fs::symlink_metadata(from)
                .await
                .is_ok_and(|m| m.is_dir());
            let mut target = trash.join(name);
            if taken(&target).await {
                let mut names = Vec::new();
                if let Ok(mut read) = tokio::fs::read_dir(trash).await {
                    while let Ok(Some(e)) = read.next_entry().await {
                        names.push(e.file_name().to_string_lossy().into_owned());
                    }
                }
                target = trash.join(keep_both(name, is_dir, |n| names.iter().any(|x| x == n)));
            }
            match tokio::fs::rename(from, &target).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == ErrorKind::CrossesDevices => finder_trash(path).await,
                Err(e) => Err(io_error(path, &e)),
            }
        }
        .boxed()
    }
}

/// Asks Finder to move `path` to the Trash: the way for items on other volumes.
async fn finder_trash(path: &str) -> Result<(), FileError> {
    let quoted = path.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("tell application \"Finder\" to delete POSIX file \"{quoted}\"");
    let status = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .await
        .map_err(|e| FileError::Other(format!("{path}: could not ask Finder: {e}")))?;
    if status.status.success() {
        Ok(())
    } else {
        Err(FileError::Other(format!(
            "{path}: Finder could not move it to the Trash"
        )))
    }
}
