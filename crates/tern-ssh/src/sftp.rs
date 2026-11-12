// Adapted from russh russh/examples/sftp_client.rs (Apache-2.0): open a session channel, request
// the `sftp` subsystem, give the channel's stream to russh-sftp's `SftpSession::new`.
//! SFTP over a session: the calls the file source adapter (`files_sftp`) is made of. No UI; the
//! app drives it through the file source.
//!
//! The protocol work is `russh-sftp` (Apache-2.0), used as its `examples/sftp_client.rs` in the
//! russh repository does: open a session channel, request the `sftp` subsystem, and hand the
//! channel's stream to `SftpSession::new`. Every call runs on the session's tokio runtime, so the
//! futures here may be awaited from any executor.
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use russh::ChannelMsg;
use russh::client::Handle;
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpWireError;
use russh_sftp::client::fs::File;
use russh_sftp::protocol::StatusCode;

use crate::files::{FileEntry, sort_entries};
use crate::hostkey::Handler;

/// Why an SFTP call failed, worded for the person using the app.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SftpError {
    /// The session has ended.
    #[error("session closed")]
    Closed,
    /// The host has no SFTP service.
    #[error("the host does not offer SFTP")]
    NotOffered,
    #[error("{0}: no such file or directory")]
    NotFound(String),
    #[error("{0}: permission denied")]
    PermissionDenied(String),
    #[error("{0}: already exists")]
    AlreadyExists(String),
    /// Anything else the server or the protocol reported.
    #[error("{0}")]
    Other(String),
}

/// An SFTP connection on one SSH session. Cheap to clone.
#[derive(Clone)]
pub struct Sftp {
    inner: Arc<SftpSession>,
    rt: tokio::runtime::Handle,
}

impl fmt::Debug for Sftp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sftp")
    }
}

/// Opens the `sftp` subsystem on a new channel of `session`.
pub(crate) async fn open(session: &Handle<Handler>) -> Result<Sftp, SftpError> {
    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|_| SftpError::Closed)?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|_| SftpError::Closed)?;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => break,
            Some(ChannelMsg::Failure) => return Err(SftpError::NotOffered),
            Some(_) => {}
            None => return Err(SftpError::Closed),
        }
    }
    let inner = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| wire("sftp", &e))?;
    Ok(Sftp {
        inner: Arc::new(inner),
        rt: tokio::runtime::Handle::current(),
    })
}

fn wire(path: &str, e: &SftpWireError) -> SftpError {
    match e {
        SftpWireError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => SftpError::NotFound(path.to_string()),
            StatusCode::PermissionDenied => SftpError::PermissionDenied(path.to_string()),
            StatusCode::ConnectionLost | StatusCode::NoConnection => SftpError::Closed,
            _ => SftpError::Other(format!("{path}: {}", s.error_message)),
        },
        SftpWireError::Timeout => SftpError::Other(format!("{path}: the server did not answer")),
        other => SftpError::Other(format!("{path}: {other}")),
    }
}

impl Sftp {
    /// Runs `fut` on the session's runtime and waits for it from wherever the caller runs.
    async fn on_rt<T, F>(&self, fut: F) -> Result<T, SftpError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, SftpError>> + Send + 'static,
    {
        self.rt
            .spawn(fut)
            .await
            .map_err(|_| SftpError::Closed)
            .and_then(|r| r)
    }

    /// The login directory on the server (what `.` resolves to).
    ///
    /// # Errors
    ///
    /// [`SftpError::Closed`] once the session has ended, or the server's refusal.
    pub async fn home(&self) -> Result<String, SftpError> {
        let s = self.inner.clone();
        self.on_rt(async move { s.canonicalize(".").await.map_err(|e| wire(".", &e)) })
            .await
    }

    /// The entries of a remote directory, folders first, then by name; `.` and `..` left out.
    ///
    /// # Errors
    ///
    /// [`SftpError::NotFound`] or [`SftpError::PermissionDenied`] for the directory, else
    /// [`SftpError::Other`] / [`SftpError::Closed`].
    pub async fn list(&self, path: &str) -> Result<Vec<FileEntry>, SftpError> {
        let (s, dir) = (self.inner.clone(), path.to_string());
        self.on_rt(async move {
            let entries = s.read_dir(dir.clone()).await.map_err(|e| wire(&dir, &e))?;
            let mut out: Vec<FileEntry> = entries
                .filter(|e| !matches!(e.file_name().as_str(), "." | ".."))
                .map(|e| {
                    let m = e.metadata();
                    let kind = m.file_type();
                    FileEntry {
                        name: e.file_name(),
                        path: e.path(),
                        is_dir: kind.is_dir(),
                        is_symlink: kind.is_symlink(),
                        size: m.len(),
                        modified: m.modified().ok().filter(|t| *t > SystemTime::UNIX_EPOCH),
                    }
                })
                .collect();
            sort_entries(&mut out);
            Ok(out)
        })
        .await
    }

    /// Opens a remote file for reading, with its size.
    ///
    /// # Errors
    ///
    /// The path's errors as in [`Self::list`].
    pub async fn open_read(&self, remote: &str) -> Result<(File, u64), SftpError> {
        let (s, remote) = (self.inner.clone(), remote.to_string());
        self.on_rt(async move {
            let total = s
                .metadata(remote.clone())
                .await
                .map_err(|e| wire(&remote, &e))?
                .len();
            let file = s
                .open(remote.clone())
                .await
                .map_err(|e| wire(&remote, &e))?;
            Ok((file, total))
        })
        .await
    }

    /// Opens a remote file for writing, creating or emptying it.
    ///
    /// # Errors
    ///
    /// The path's errors as in [`Self::list`].
    pub async fn open_write(&self, remote: &str) -> Result<File, SftpError> {
        let (s, remote) = (self.inner.clone(), remote.to_string());
        self.on_rt(async move {
            s.create(remote.clone())
                .await
                .map_err(|e| wire(&remote, &e))
        })
        .await
    }

    /// Makes one remote folder.
    ///
    /// # Errors
    ///
    /// [`SftpError::AlreadyExists`] when the name is taken, else as in [`Self::list`].
    pub async fn mkdir(&self, remote: &str) -> Result<(), SftpError> {
        let (s, remote) = (self.inner.clone(), remote.to_string());
        self.on_rt(async move {
            if exists(&s, &remote).await {
                return Err(SftpError::AlreadyExists(remote));
            }
            s.create_dir(remote.clone())
                .await
                .map_err(|e| wire(&remote, &e))
        })
        .await
    }

    /// Renames a remote file or folder without replacing anything.
    ///
    /// # Errors
    ///
    /// [`SftpError::AlreadyExists`] when `to` is taken, else as in [`Self::list`].
    pub async fn rename(&self, from: &str, to: &str) -> Result<(), SftpError> {
        let (s, from, to) = (self.inner.clone(), from.to_string(), to.to_string());
        self.on_rt(async move {
            if exists(&s, &to).await {
                return Err(SftpError::AlreadyExists(to));
            }
            s.rename(from.clone(), to)
                .await
                .map_err(|e| wire(&from, &e))
        })
        .await
    }

    /// Deletes a remote file, or a folder with everything in it. There is no undo.
    ///
    /// # Errors
    ///
    /// As in [`Self::list`], for the first item that cannot be removed.
    pub async fn remove(&self, remote: &str) -> Result<(), SftpError> {
        let (s, remote) = (self.inner.clone(), remote.to_string());
        self.on_rt(remove_tree(s, remote)).await
    }

    /// Ends the SFTP channel. Dropping every clone does the same eventually.
    pub async fn close(&self) {
        let _ = self.inner.close().await;
    }
}

async fn exists(s: &SftpSession, path: &str) -> bool {
    s.symlink_metadata(path.to_string()).await.is_ok()
}

fn remove_tree(
    s: Arc<SftpSession>,
    path: String,
) -> Pin<Box<dyn Future<Output = Result<(), SftpError>> + Send>> {
    Box::pin(async move {
        let kind = s
            .symlink_metadata(path.clone())
            .await
            .map_err(|e| wire(&path, &e))?
            .file_type();
        if !kind.is_dir() {
            return s
                .remove_file(path.clone())
                .await
                .map_err(|e| wire(&path, &e));
        }
        let children: Vec<String> = s
            .read_dir(path.clone())
            .await
            .map_err(|e| wire(&path, &e))?
            .filter(|e| !matches!(e.file_name().as_str(), "." | ".."))
            .map(|e| e.path())
            .collect();
        for child in children {
            remove_tree(s.clone(), child).await?;
        }
        s.remove_dir(path.clone())
            .await
            .map_err(|e| wire(&path, &e))
    })
}
