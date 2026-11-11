// Adapted from russh russh/examples/sftp_client.rs (Apache-2.0): open a session channel, request
// the `sftp` subsystem, give the channel's stream to russh-sftp's `SftpSession::new`.
//! SFTP over a session: list, download, upload, and the login directory. No UI; the app drives it.
//!
//! The protocol work is `russh-sftp` (Apache-2.0), used as its `examples/sftp_client.rs` in the
//! russh repository does: open a session channel, request the `sftp` subsystem, and hand the
//! channel's stream to `SftpSession::new`. Every call runs on the session's tokio runtime, so the
//! futures here may be awaited from any executor.
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::SystemTime;

use russh::ChannelMsg;
use russh::client::Handle;
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpWireError;
use russh_sftp::client::fs::File;
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::files::{FileEntry, sort_entries};
use crate::hostkey::Handler;

/// Bytes moved per read or write; also how often progress is reported.
const CHUNK: usize = 64 * 1024;

/// Why an SFTP call failed, worded for the person using the app.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SftpError {
    /// The session has ended.
    #[error("session closed")]
    Closed,
    /// The server has no SFTP service.
    #[error("the server does not offer SFTP")]
    NotOffered,
    #[error("{0}: no such file or directory")]
    NotFound(String),
    #[error("{0}: permission denied")]
    PermissionDenied(String),
    #[error("{0}: already exists")]
    AlreadyExists(String),
    /// A problem on this machine's side of a transfer.
    #[error("{path}: {reason}")]
    Local { path: String, reason: String },
    /// Anything else the server or the protocol reported.
    #[error("{0}")]
    Other(String),
}

/// How far a transfer has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
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

fn local(path: &Path, e: &std::io::Error) -> SftpError {
    SftpError::Local {
        path: path.display().to_string(),
        reason: e.to_string(),
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

    /// Copies a remote file to `local`, replacing it. The data lands in `<local>.part` first and
    /// is renamed on success, so a failed transfer never leaves a half-written `local`.
    /// `progress` is called after every chunk. Returns the bytes copied.
    ///
    /// # Errors
    ///
    /// The remote file's errors as in [`Self::list`], or [`SftpError::Local`].
    pub async fn download(
        &self,
        remote: &str,
        local_path: &Path,
        mut progress: impl FnMut(Progress) + Send + 'static,
    ) -> Result<u64, SftpError> {
        let (s, remote, dest) = (
            self.inner.clone(),
            remote.to_string(),
            local_path.to_path_buf(),
        );
        self.on_rt(async move {
            let total = s
                .metadata(remote.clone())
                .await
                .map_err(|e| wire(&remote, &e))?
                .len();
            let mut src = s
                .open(remote.clone())
                .await
                .map_err(|e| wire(&remote, &e))?;
            let part = part_path(&dest);
            let copied = copy_down(&mut src, &part, total, &mut progress).await;
            let _ = src.shutdown().await;
            match copied {
                Ok(n) => {
                    tokio::fs::rename(&part, &dest)
                        .await
                        .map_err(|e| local(&dest, &e))?;
                    Ok(n)
                }
                Err(e) => {
                    let _ = tokio::fs::remove_file(&part).await;
                    Err(match e {
                        Down::Local(io) => local(&dest, &io),
                        Down::Remote(io) => SftpError::Other(format!("{remote}: {io}")),
                    })
                }
            }
        })
        .await
    }

    /// Copies `local` to a remote file, replacing it. `progress` is called after every chunk.
    /// Returns the bytes copied.
    ///
    /// # Errors
    ///
    /// [`SftpError::Local`] for the local file, or the remote path's errors as in [`Self::list`].
    pub async fn upload(
        &self,
        local_path: &Path,
        remote: &str,
        mut progress: impl FnMut(Progress) + Send + 'static,
    ) -> Result<u64, SftpError> {
        let (s, remote, src) = (
            self.inner.clone(),
            remote.to_string(),
            local_path.to_path_buf(),
        );
        self.on_rt(async move {
            let mut file = tokio::fs::File::open(&src)
                .await
                .map_err(|e| local(&src, &e))?;
            let total = file.metadata().await.map_err(|e| local(&src, &e))?.len();
            let mut dest = s
                .create(remote.clone())
                .await
                .map_err(|e| wire(&remote, &e))?;
            let mut buf = vec![0u8; CHUNK];
            let mut done = 0u64;
            loop {
                let n = file.read(&mut buf).await.map_err(|e| local(&src, &e))?;
                if n == 0 {
                    break;
                }
                dest.write_all(&buf[..n])
                    .await
                    .map_err(|e| SftpError::Other(format!("{remote}: {e}")))?;
                done += n as u64;
                progress(Progress { done, total });
            }
            dest.shutdown()
                .await
                .map_err(|e| SftpError::Other(format!("{remote}: {e}")))?;
            Ok(done)
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

enum Down {
    Local(std::io::Error),
    Remote(std::io::Error),
}

async fn copy_down(
    src: &mut russh_sftp::client::fs::File,
    part: &Path,
    total: u64,
    progress: &mut (impl FnMut(Progress) + Send),
) -> Result<u64, Down> {
    let mut out = tokio::fs::File::create(part).await.map_err(Down::Local)?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    loop {
        let n = src.read(&mut buf).await.map_err(Down::Remote)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).await.map_err(Down::Local)?;
        done += n as u64;
        progress(Progress { done, total });
    }
    out.flush().await.map_err(Down::Local)?;
    Ok(done)
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}
