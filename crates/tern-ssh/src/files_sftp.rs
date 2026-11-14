//! The SFTP adapter of the file source: one host's files over an open SFTP connection. The wire
//! work is [`Sftp`]; this maps it to the seam's operations and errors.
use futures::FutureExt;

use crate::SessionHandle;
use crate::files::{Call, FileEntry, FileError, FileSource, Reader, Writer};
use crate::sftp::{Sftp, SftpError};

/// A host's files. `id` names the host, so copies onto it share one slot in the queue.
#[derive(Debug, Clone)]
pub struct SftpFiles {
    id: String,
    sftp: Sftp,
    /// The connection this source was opened on, when the source owns it: the connection ends
    /// when the last clone of the source is dropped.
    _keep: Option<SessionHandle>,
}

impl SftpFiles {
    #[must_use]
    pub fn new(id: impl Into<String>, sftp: Sftp) -> Self {
        Self {
            id: id.into(),
            sftp,
            _keep: None,
        }
    }

    /// Opens a host's files on a connection that is already up. The source keeps the connection
    /// (see [`keeping`](Self::keeping)), so callers never build one without it.
    ///
    /// # Errors
    /// The connection is gone, or the host refuses SFTP.
    pub async fn open(id: impl Into<String>, handle: SessionHandle) -> Result<Self, SftpError> {
        let sftp = handle.open_sftp().await?;
        Ok(Self::new(id, sftp).keeping(handle))
    }

    /// Ties the connection's life to this source (and every clone of it): a copy still using
    /// the source keeps the connection open after whoever opened it is done.
    #[must_use]
    pub fn keeping(mut self, handle: SessionHandle) -> Self {
        self._keep = Some(handle);
        self
    }

    /// The connection underneath, for closing it.
    #[must_use]
    pub fn sftp(&self) -> &Sftp {
        &self.sftp
    }
}

impl From<SftpError> for FileError {
    fn from(e: SftpError) -> Self {
        match e {
            SftpError::Closed => Self::Closed,
            SftpError::NotFound(p) => Self::NotFound(p),
            SftpError::PermissionDenied(p) => Self::PermissionDenied(p),
            SftpError::AlreadyExists(p) => Self::AlreadyExists(p),
            other => Self::Other(other.to_string()),
        }
    }
}

impl FileSource for SftpFiles {
    fn id(&self) -> &str {
        &self.id
    }

    fn home(&self) -> Call<'_, String> {
        async move { Ok(self.sftp.home().await?) }.boxed()
    }

    fn list<'a>(&'a self, dir: &'a str) -> Call<'a, Vec<FileEntry>> {
        async move { Ok(self.sftp.list(dir).await?) }.boxed()
    }

    fn read<'a>(&'a self, path: &'a str) -> Call<'a, Reader> {
        async move {
            let (file, _) = self.sftp.open_read(path).await?;
            Ok(Box::new(file) as Reader)
        }
        .boxed()
    }

    fn write<'a>(&'a self, path: &'a str) -> Call<'a, Writer> {
        async move { Ok(Box::new(self.sftp.open_write(path).await?) as Writer) }.boxed()
    }

    fn mkdir<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        async move { Ok(self.sftp.mkdir(path).await?) }.boxed()
    }

    fn rename<'a>(&'a self, from: &'a str, to: &'a str) -> Call<'a, ()> {
        async move { Ok(self.sftp.rename(from, to).await?) }.boxed()
    }

    fn delete<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        async move { Ok(self.sftp.remove(path).await?) }.boxed()
    }
}
