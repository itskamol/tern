// Adapted from russh examples/sftp_server.rs (Apache-2.0) for the handler shape; the file
// operations are a plain std::fs backing so tests move real bytes.
//! A tiny SFTP server over a directory: `/` is `root`, and `.` resolves to `/home`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};

enum Open {
    /// The file and whether it is `flaky`: reads past 64 KiB fail, as a dying disk would.
    File(std::fs::File, bool),
    Dir(Vec<File>),
}

pub struct FsHandler {
    pub root: PathBuf,
    handles: HashMap<String, Open>,
    next: u32,
}

impl FsHandler {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            handles: HashMap::new(),
            next: 0,
        }
    }

    fn real(&self, path: &str) -> PathBuf {
        let p = if path == "." { "/home" } else { path };
        self.root.join(p.trim_start_matches('/'))
    }

    fn handle(&mut self, open: Open) -> String {
        self.next += 1;
        let h = format!("h{}", self.next);
        self.handles.insert(h.clone(), open);
        h
    }
}

fn status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

fn code(e: &std::io::Error) -> StatusCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl russh_sftp::server::Handler for FsHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let shown = if path == "." {
            "/home".to_string()
        } else {
            path
        };
        Ok(Name {
            id,
            files: vec![File::dummy(shown)],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let rd = std::fs::read_dir(self.real(&path)).map_err(|e| code(&e))?;
        let files = rd
            .flatten()
            .map(|e| {
                File::new(
                    e.file_name().to_string_lossy(),
                    (&e.metadata().unwrap()).into(),
                )
            })
            .collect();
        Ok(Handle {
            id,
            handle: self.handle(Open::Dir(files)),
        })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            Some(Open::Dir(files)) if !files.is_empty() => Ok(Name {
                id,
                files: std::mem::take(files),
            }),
            _ => Err(StatusCode::Eof),
        }
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let m = std::fs::metadata(self.real(&path)).map_err(|e| code(&e))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&m),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.handles.get(&handle) {
            Some(Open::File(f, _)) => Ok(Attrs {
                id,
                attrs: FileAttributes::from(&f.metadata().map_err(|e| code(&e))?),
            }),
            _ => Err(StatusCode::Failure),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let mut opts: std::fs::OpenOptions = flags.into();
        if flags.contains(OpenFlags::CREATE) {
            opts.create(true);
        }
        if flags.contains(OpenFlags::TRUNCATE) {
            opts.truncate(true);
        }
        let f = opts.open(self.real(&filename)).map_err(|e| code(&e))?;
        Ok(Handle {
            id,
            handle: self.handle(Open::File(f, filename.ends_with("flaky"))),
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(Open::File(f, flaky)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        if *flaky && offset >= 65536 {
            return Err(StatusCode::Failure);
        }
        f.seek(SeekFrom::Start(offset)).map_err(|e| code(&e))?;
        let mut buf = vec![0u8; len as usize];
        let n = f.read(&mut buf).map_err(|e| code(&e))?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(Open::File(f, _)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        f.seek(SeekFrom::Start(offset)).map_err(|e| code(&e))?;
        f.write_all(&data).map_err(|e| code(&e))?;
        Ok(status(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, Self::Error> {
        std::fs::create_dir(self.real(&path)).map_err(|e| code(&e))?;
        Ok(status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.real(&path)).map_err(|e| code(&e))?;
        Ok(status(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.real(&filename)).map_err(|e| code(&e))?;
        Ok(status(id))
    }

    /// Like OpenSSH's plain SFTP rename: an existing target is an error, not replaced.
    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let to = self.real(&newpath);
        if to.symlink_metadata().is_ok() {
            return Err(StatusCode::Failure);
        }
        std::fs::rename(self.real(&oldpath), to).map_err(|e| code(&e))?;
        Ok(status(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(status(id))
    }
}
