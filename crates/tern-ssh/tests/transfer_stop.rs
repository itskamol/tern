//! A copy whose destination fails stops reading the source: no bytes are pulled for a file that
//! can no longer be written. Kept apart from `transfer.rs` to avoid growing a 1,000-line test file.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use tern_ssh::{
    Call, CopyJob, Event, FileEntry, FileSource, LocalFs, Reader, TransferQueue, Writer,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const FAIL_AFTER: usize = 64 * 1024;

/// Counts the bytes handed out by the source.
struct CountedReader(Reader, Arc<AtomicUsize>);

impl AsyncRead for CountedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.0).poll_read(cx, buf);
        self.1
            .fetch_add(buf.filled().len() - before, Ordering::SeqCst);
        r
    }
}

/// Takes `FAIL_AFTER` bytes, then every write fails.
struct FailingWriter(Writer, usize);

impl AsyncWrite for FailingWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.1 >= FAIL_AFTER {
            return Poll::Ready(Err(std::io::Error::other("disk full")));
        }
        let r = Pin::new(&mut self.0).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &r {
            self.1 += n;
        }
        r
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[derive(Debug)]
struct Wrapped {
    inner: Arc<dyn FileSource>,
    id: &'static str,
    read: Arc<AtomicUsize>,
    fail_writes: bool,
}

impl std::fmt::Debug for CountedReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("counted")
    }
}

impl FileSource for Wrapped {
    fn id(&self) -> &str {
        self.id
    }
    fn home(&self) -> Call<'_, String> {
        self.inner.home()
    }
    fn list<'a>(&'a self, dir: &'a str) -> Call<'a, Vec<FileEntry>> {
        self.inner.list(dir)
    }
    fn read<'a>(&'a self, path: &'a str) -> Call<'a, Reader> {
        Box::pin(async move {
            let r = self.inner.read(path).await?;
            Ok(Box::new(CountedReader(r, self.read.clone())) as Reader)
        })
    }
    fn write<'a>(&'a self, path: &'a str) -> Call<'a, Writer> {
        Box::pin(async move {
            let w = self.inner.write(path).await?;
            if self.fail_writes {
                return Ok(Box::new(FailingWriter(w, 0)) as Writer);
            }
            Ok(w)
        })
    }
    fn mkdir<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        self.inner.mkdir(path)
    }
    fn rename<'a>(&'a self, from: &'a str, to: &'a str) -> Call<'a, ()> {
        self.inner.rename(from, to)
    }
    fn delete<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        self.inner.delete(path)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_error_stops_the_read_and_leaves_nothing_behind() {
    let (src_dir, dst_dir, trash) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let size = 8 * 1024 * 1024;
    std::fs::write(src_dir.path().join("big.bin"), vec![7u8; size]).unwrap();
    let local: Arc<dyn FileSource> = Arc::new(LocalFs::with_trash(trash.path().to_path_buf()));
    let read = Arc::new(AtomicUsize::new(0));
    let from: Arc<dyn FileSource> = Arc::new(Wrapped {
        inner: local.clone(),
        id: "src",
        read: read.clone(),
        fail_writes: false,
    });
    let to: Arc<dyn FileSource> = Arc::new(Wrapped {
        inner: local.clone(),
        id: "dst",
        read: Arc::default(),
        fail_writes: true,
    });
    let item = from
        .list(src_dir.path().to_str().unwrap())
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.name == "big.bin")
        .unwrap();
    let (queue, events) = TransferQueue::new(tokio::runtime::Handle::current());
    queue.submit(vec![CopyJob {
        from,
        item,
        to,
        dir: dst_dir.path().to_str().unwrap().to_owned(),
        rule: None,
        moving: false,
    }]);
    let summary = loop {
        let ev = tokio::time::timeout(Duration::from_secs(20), events.recv())
            .await
            .expect("the batch did not finish")
            .unwrap();
        if let Event::Finished(s) = ev {
            break s;
        }
    };
    assert_eq!((summary.copied, summary.failed.len()), (0, 1));
    assert!(
        summary.failed[0].contains("disk full"),
        "{:?}",
        summary.failed
    );
    assert_eq!(std::fs::read_dir(dst_dir.path()).unwrap().count(), 0);
    // Reading ahead of the writer is bounded, so far less than the file was read.
    let got = read.load(Ordering::SeqCst);
    assert!(got < size / 4, "{got} of {size} bytes were read");
}
