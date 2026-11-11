//! The SFTP adapter of the file source: one host's files over an open SFTP connection. The wire
//! work is [`Sftp`]; this maps it to the seam's operations and errors.
use futures::FutureExt;

use crate::files::{Call, FileEntry, FileError, FileSource, Reader, Writer};
use crate::sftp::{Sftp, SftpError};

/// A host's files. `id` names the host, so copies onto it share one slot in the queue.
#[derive(Debug, Clone)]
pub struct SftpFiles {
    id: String,
    sftp: Sftp,
}

impl SftpFiles {
    #[must_use]
    pub fn new(id: impl Into<String>, sftp: Sftp) -> Self {
        Self {
            id: id.into(),
            sftp,
        }
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
