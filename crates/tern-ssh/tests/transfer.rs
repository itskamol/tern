//! The transfer queue against real file sources: the Mac's file system on temp dirs and the
//! in-process SFTP server. A test copies through the queue and looks at what ends up on the
//! destination; it does not look at the calls made to get there.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use common::{serve_sftp, spec, start};
use tern_ssh::{
    Call, Conflict, CopyJob, Event, FileEntry, FileSource, LocalFs, Reader, SftpFiles, Summary,
    TransferQueue, Writer,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

fn pattern(len: u32) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
}

/// A temp dir seen as the Mac, with its own Trash.
struct Mac {
    source: Arc<dyn FileSource>,
    dir: tempfile::TempDir,
    _trash: tempfile::TempDir,
}

fn mac() -> Mac {
    let trash = tempfile::tempdir().unwrap();
    Mac {
        source: Arc::new(LocalFs::with_trash(trash.path().to_path_buf())),
        dir: tempfile::tempdir().unwrap(),
        _trash: trash,
    }
}

impl Mac {
    fn path(&self, rel: &str) -> String {
        self.dir.path().join(rel).to_str().unwrap().to_owned()
    }
    fn put(&self, rel: &str, data: &[u8]) {
        let p = self.dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }
    fn get(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.dir.path().join(rel)).unwrap()
    }
    fn has(&self, rel: &str) -> bool {
        self.dir.path().join(rel).exists()
    }
    fn names(&self, rel: &str) -> Vec<String> {
        let mut v: Vec<_> = std::fs::read_dir(self.dir.path().join(rel))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }
}

struct Host {
    source: Arc<dyn FileSource>,
    root: tempfile::TempDir,
    _live: common::Live,
    _keys: tempfile::TempDir,
}

async fn host(id: &str) -> Host {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("home")).unwrap();
    let keys = tempfile::tempdir().unwrap();
    let server = serve_sftp(root.path().to_path_buf()).await;
    let live = start(spec(server.port, &keys)).await.unwrap();
    let sftp = live.handle.open_sftp().await.unwrap();
    Host {
        source: Arc::new(SftpFiles::new(id, sftp)),
        root,
        _live: live,
        _keys: keys,
    }
}

impl Host {
    fn disk(&self, rel: &str) -> PathBuf {
        self.root.path().join("home").join(rel)
    }
}

async fn entry(source: &Arc<dyn FileSource>, path: &str) -> FileEntry {
    let dir = tern_ssh::parent(path);
    let name = tern_ssh::file_name(path);
    source
        .list(&dir)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("{path} is not listed"))
}

async fn job(
    from: &Arc<dyn FileSource>,
    path: &str,
    to: &Arc<dyn FileSource>,
    dir: &str,
    rule: Option<Conflict>,
) -> CopyJob {
    CopyJob {
        from: from.clone(),
        item: entry(from, path).await,
        to: to.clone(),
        dir: dir.to_owned(),
        rule,
    }
}

struct Run {
    queue: TransferQueue,
    events: async_channel::Receiver<Event>,
    /// Batches seen to finish while waiting for another.
    ended: Mutex<Vec<Summary>>,
}

fn queue() -> Run {
    let (queue, events) = TransferQueue::new(tokio::runtime::Handle::current());
    Run {
        queue,
        events,
        ended: Mutex::default(),
    }
}

impl Run {
    /// Events until the batch finishes; asks `on_question` for each conflict question.
    async fn finish(
        &self,
        batch: u64,
        mut on_question: impl FnMut(&TransferQueue, &tern_ssh::Question),
    ) -> (Summary, Vec<Event>) {
        let mut seen = Vec::new();
        loop {
            let early = {
                let mut ended = self.ended.lock().unwrap();
                ended
                    .iter()
                    .position(|s| s.batch == batch)
                    .map(|at| ended.remove(at))
            };
            if let Some(summary) = early {
                return (summary, seen);
            }
            let event = tokio::time::timeout(Duration::from_secs(20), self.events.recv())
                .await
                .expect("the batch did not finish in time")
                .unwrap();
            if let Event::Question(q) = &event {
                on_question(&self.queue, q);
            }
            seen.push(event.clone());
            if let Event::Finished(s) = event {
                if s.batch == batch {
                    return (s, seen);
                }
                self.ended.lock().unwrap().push(s);
            }
        }
    }

    async fn done(&self, batch: u64) -> Summary {
        self.finish(batch, |_, q| panic!("unexpected question about {}", q.name))
            .await
            .0
    }
}

// ---- copying ----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_file_is_copied_whole_and_progress_ends_at_the_total() {
    let (a, b) = (mac(), mac());
    let data = pattern(300_001);
    a.put("big.bin", &data);
    let run = queue();
    let batch = run.queue.submit(vec![
        job(&a.source, &a.path("big.bin"), &b.source, &b.path(""), None).await,
    ]);
    let (summary, events) = run.finish(batch, |_, _| {}).await;
    assert_eq!(
        (summary.copied, summary.failed.len(), summary.cancelled),
        (1, 0, false)
    );
    assert_eq!(b.get("big.bin"), data);
    assert!(!b.has("big.bin.part"));
    let last = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress(p) => Some(p),
            _ => None,
        })
        .next_back()
        .expect("progress was reported");
    assert_eq!((last.done, last.total), (300_001, 300_001));
    assert_eq!((last.files_done, last.files_total), (1, 1));
    assert!(!run.queue.busy());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_is_copied_with_everything_inside_it_empty_folders_too() {
    let (a, b) = (mac(), mac());
    a.put("proj/readme", b"r");
    a.put("proj/src/main.rs", b"fn main() {}");
    a.put("proj/src/deep/er/x.txt", b"x");
    std::fs::create_dir_all(a.dir.path().join("proj/empty")).unwrap();
    a.put("loose.txt", b"loose");
    let run = queue();
    let batch = run.queue.submit(vec![
        job(&a.source, &a.path("proj"), &b.source, &b.path(""), None).await,
        job(
            &a.source,
            &a.path("loose.txt"),
            &b.source,
            &b.path(""),
            None,
        )
        .await,
    ]);
    let summary = run.done(batch).await;
    assert_eq!(summary.copied, 4);
    assert_eq!(b.get("proj/readme"), b"r");
    assert_eq!(b.get("proj/src/main.rs"), b"fn main() {}");
    assert_eq!(b.get("proj/src/deep/er/x.txt"), b"x");
    assert!(b.dir.path().join("proj/empty").is_dir());
    assert_eq!(b.get("loose.txt"), b"loose");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mac_uploads_to_a_host_and_downloads_from_it() {
    let m = mac();
    let h = host("h").await;
    let data = pattern(200_000);
    m.put("up/a.bin", &data);
    let run = queue();
    let up = run.queue.submit(vec![
        job(&m.source, &m.path("up"), &h.source, "/home", None).await,
    ]);
    assert_eq!(run.done(up).await.copied, 1);
    assert_eq!(std::fs::read(h.disk("up/a.bin")).unwrap(), data);
    // The destination folder has to exist: the queue copies into folders, it does not make them.
    std::fs::create_dir(m.dir.path().join("back")).unwrap();
    let down = run.queue.submit(vec![
        job(&h.source, "/home/up", &m.source, &m.path("back"), None).await,
    ]);
    assert_eq!(run.done(down).await.copied, 1);
    assert_eq!(m.get("back/up/a.bin"), data);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_host_copies_straight_to_another() {
    let (h1, h2) = (host("one").await, host("two").await);
    let data = pattern(250_000);
    std::fs::create_dir_all(h1.disk("tree/sub")).unwrap();
    std::fs::write(h1.disk("tree/sub/f.bin"), &data).unwrap();
    std::fs::write(h1.disk("tree/g"), b"g").unwrap();
    let run = queue();
    let batch = run.queue.submit(vec![
        job(&h1.source, "/home/tree", &h2.source, "/home", None).await,
    ]);
    assert_eq!(run.done(batch).await.copied, 2);
    assert_eq!(std::fs::read(h2.disk("tree/sub/f.bin")).unwrap(), data);
    assert_eq!(std::fs::read(h2.disk("tree/g")).unwrap(), b"g");
    assert!(!h2.disk("tree/sub/f.bin.part").exists());
}

// ---- a gate to hold copies open ----------------------------------------------------------------

#[derive(Default)]
struct GateState {
    closed: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

impl GateState {
    fn closed() -> Arc<Self> {
        Arc::new(Self {
            closed: AtomicBool::new(true),
            wakers: Mutex::default(),
        })
    }
    fn open(&self) {
        self.closed.store(false, Ordering::SeqCst);
        for w in self.wakers.lock().unwrap().drain(..) {
            w.wake();
        }
    }
}

/// How many writers are open now, in all and the most there ever were, across a group of sources.
#[derive(Default)]
struct Meter {
    now: AtomicUsize,
    peak: AtomicUsize,
}

impl Meter {
    fn enter(&self) {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }
}

/// A source whose reads wait at a gate and whose open writers are counted.
#[derive(Debug)]
struct Gated {
    inner: Arc<dyn FileSource>,
    id: String,
    gate: Arc<GateState>,
    all: Arc<Meter>,
    own: Arc<Meter>,
}

impl std::fmt::Debug for GateState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("gate")
    }
}

impl std::fmt::Debug for Meter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("meter")
    }
}

struct GatedReader(Reader, Arc<GateState>);

impl AsyncRead for GatedReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.1.closed.load(Ordering::SeqCst) {
            self.1.wakers.lock().unwrap().push(cx.waker().clone());
            return Poll::Pending;
        }
        std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

struct CountedWriter(Writer, Vec<Arc<Meter>>);

impl Drop for CountedWriter {
    fn drop(&mut self) {
        for m in &self.1 {
            m.now.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl AsyncWrite for CountedWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl FileSource for Gated {
    fn id(&self) -> &str {
        &self.id
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
            Ok(Box::new(GatedReader(r, self.gate.clone())) as Reader)
        })
    }
    fn write<'a>(&'a self, path: &'a str) -> Call<'a, Writer> {
        Box::pin(async move {
            let w = self.inner.write(path).await?;
            self.all.enter();
            self.own.enter();
            Ok(Box::new(CountedWriter(w, vec![self.all.clone(), self.own.clone()])) as Writer)
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

fn gated(
    inner: &Mac,
    id: &str,
    gate: &Arc<GateState>,
    all: &Arc<Meter>,
) -> (Arc<Gated>, Arc<Meter>) {
    let own = Arc::new(Meter::default());
    (
        Arc::new(Gated {
            inner: inner.source.clone(),
            id: id.to_owned(),
            gate: gate.clone(),
            all: all.clone(),
            own: own.clone(),
        }),
        own,
    )
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..400 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never happened: {what}");
}

// ---- .part, cancel, failure ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_in_progress_is_a_part_file_and_takes_its_name_only_when_whole() {
    let (a, b) = (mac(), mac());
    a.put("f.bin", &pattern(100_000));
    let gate = GateState::closed();
    let (src, _) = gated(&a, "src", &gate, &Arc::default());
    let run = queue();
    let from: Arc<dyn FileSource> = src;
    let batch = run.queue.submit(vec![
        job(&from, &a.path("f.bin"), &b.source, &b.path(""), None).await,
    ]);
    until("the part file appears", || b.has("f.bin.part")).await;
    assert!(
        !b.has("f.bin"),
        "the real name must not exist before the copy is whole"
    );
    assert!(run.queue.busy());
    gate.open();
    assert_eq!(run.done(batch).await.copied, 1);
    assert!(b.has("f.bin") && !b.has("f.bin.part"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_removes_the_part_file_and_leaves_the_old_one_alone() {
    let (a, b) = (mac(), mac());
    a.put("f.bin", &pattern(100_000));
    b.put("f.bin", b"old");
    let gate = GateState::closed();
    let (src, _) = gated(&a, "src", &gate, &Arc::default());
    let run = queue();
    let from: Arc<dyn FileSource> = src;
    let batch = run.queue.submit(vec![
        job(
            &from,
            &a.path("f.bin"),
            &b.source,
            &b.path(""),
            Some(Conflict::Replace),
        )
        .await,
    ]);
    until("the part file appears", || b.has("f.bin.part")).await;
    run.queue.cancel(batch);
    let summary = run.done(batch).await;
    assert!(summary.cancelled);
    assert_eq!(summary.copied, 0);
    assert!(!b.has("f.bin.part"));
    assert_eq!(b.get("f.bin"), b"old");
    assert!(!run.queue.busy());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_all_stops_every_batch_including_queued_ones() {
    let a = mac();
    let (b1, b2) = (mac(), mac());
    for n in ["x", "y"] {
        a.put(n, &pattern(50_000));
    }
    let gate = GateState::closed();
    let (src, _) = gated(&a, "src", &gate, &Arc::default());
    let from: Arc<dyn FileSource> = src;
    let run = queue();
    let one = run.queue.submit(vec![
        job(&from, &a.path("x"), &b1.source, &b1.path(""), None).await,
    ]);
    let two = run.queue.submit(vec![
        job(&from, &a.path("y"), &b2.source, &b2.path(""), None).await,
    ]);
    until("a part file appears", || {
        b1.has("x.part") || b2.has("y.part")
    })
    .await;
    run.queue.cancel_all();
    assert!(run.done(one).await.cancelled);
    assert!(run.done(two).await.cancelled);
    assert!(!b1.has("x.part") && !b1.has("x") && !b2.has("y.part") && !b2.has("y"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_copy_removes_its_part_file_and_the_rest_of_the_batch_goes_on() {
    let h = host("h").await;
    let m = mac();
    std::fs::write(h.disk("flaky"), pattern(300_000)).unwrap();
    std::fs::write(h.disk("fine"), b"fine").unwrap();
    let run = queue();
    let batch = run.queue.submit(vec![
        job(&h.source, "/home/flaky", &m.source, &m.path(""), None).await,
        job(&h.source, "/home/fine", &m.source, &m.path(""), None).await,
    ]);
    let summary = run.done(batch).await;
    assert_eq!(summary.copied, 1);
    assert_eq!(summary.failed.len(), 1);
    assert!(summary.failed[0].contains("flaky"), "{:?}", summary.failed);
    assert!(!m.has("flaky") && !m.has("flaky.part"));
    assert_eq!(m.get("fine"), b"fine");
}

/// A destination whose rename of a finished `.part` file always fails.
#[derive(Debug)]
struct RefusesToFinish(Arc<dyn FileSource>);

impl FileSource for RefusesToFinish {
    fn id(&self) -> &str {
        self.0.id()
    }
    fn home(&self) -> Call<'_, String> {
        self.0.home()
    }
    fn list<'a>(&'a self, dir: &'a str) -> Call<'a, Vec<FileEntry>> {
        self.0.list(dir)
    }
    fn read<'a>(&'a self, path: &'a str) -> Call<'a, Reader> {
        self.0.read(path)
    }
    fn write<'a>(&'a self, path: &'a str) -> Call<'a, Writer> {
        self.0.write(path)
    }
    fn mkdir<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        self.0.mkdir(path)
    }
    fn rename<'a>(&'a self, from: &'a str, to: &'a str) -> Call<'a, ()> {
        if from.ends_with(".part") {
            return Box::pin(async { Err(tern_ssh::FileError::Other("disk said no".into())) });
        }
        self.0.rename(from, to)
    }
    fn delete<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        self.0.delete(path)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_that_cannot_finish_keeps_the_old_file_on_the_mac() {
    let (from, to) = (mac(), mac());
    from.put("a.txt", b"new");
    to.put("a.txt", b"old");
    let dest: Arc<dyn FileSource> = Arc::new(RefusesToFinish(to.source.clone()));
    let run = queue();
    let batch = run.queue.submit(vec![
        job(
            &from.source,
            &from.path("a.txt"),
            &dest,
            &to.path(""),
            Some(Conflict::Replace),
        )
        .await,
    ]);
    let summary = run.done(batch).await;
    assert_eq!((summary.copied, summary.failed.len()), (0, 1));
    assert_eq!(to.get("a.txt"), b"old");
    assert_eq!(to.names(""), ["a.txt"], "no part or aside file is left");
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_that_cannot_finish_keeps_the_old_file_on_a_host() {
    let from = mac();
    let h = host("h").await;
    from.put("a.txt", b"new");
    std::fs::write(h.disk("a.txt"), b"old").unwrap();
    let dest: Arc<dyn FileSource> = Arc::new(RefusesToFinish(h.source.clone()));
    let run = queue();
    let batch = run.queue.submit(vec![
        job(
            &from.source,
            &from.path("a.txt"),
            &dest,
            "/home",
            Some(Conflict::Replace),
        )
        .await,
    ]);
    let summary = run.done(batch).await;
    assert_eq!((summary.copied, summary.failed.len()), (0, 1));
    assert_eq!(std::fs::read(h.disk("a.txt")).unwrap(), b"old");
    assert_eq!(names(&h.disk("")), ["a.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn replace_on_a_host_leaves_only_the_new_file() {
    let from = mac();
    let h = host("h").await;
    from.put("a.txt", b"new");
    std::fs::write(h.disk("a.txt"), b"old").unwrap();
    let run = queue();
    let batch = run.queue.submit(vec![
        job(
            &from.source,
            &from.path("a.txt"),
            &h.source,
            "/home",
            Some(Conflict::Replace),
        )
        .await,
    ]);
    assert_eq!(run.done(batch).await.copied, 1);
    assert_eq!(std::fs::read(h.disk("a.txt")).unwrap(), b"new");
    assert_eq!(names(&h.disk("")), ["a.txt"]);
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

// ---- limits -------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn at_most_two_copies_run_at_once_and_the_queued_ones_follow() {
    let a = mac();
    let dests: Vec<Mac> = (0..4).map(|_| mac()).collect();
    for i in 0..4 {
        a.put(&format!("f{i}"), &pattern(30_000));
    }
    let gate = GateState::closed();
    let all = Arc::new(Meter::default());
    let (from, _) = gated(&a, "src", &gate, &Arc::default());
    let from: Arc<dyn FileSource> = from;
    let run = queue();
    let mut batches = Vec::new();
    for (i, d) in dests.iter().enumerate() {
        let (to, _) = gated(d, &format!("host{i}"), &gate, &all);
        let to: Arc<dyn FileSource> = to;
        batches.push(run.queue.submit(vec![
            job(&from, &a.path(&format!("f{i}")), &to, &d.path(""), None).await,
        ]));
    }
    until("two copies are open", || {
        all.now.load(Ordering::SeqCst) == 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(all.now.load(Ordering::SeqCst), 2, "a third copy started");
    gate.open();
    for b in batches {
        assert_eq!(run.done(b).await.copied, 1);
    }
    assert_eq!(all.peak.load(Ordering::SeqCst), 2);
    for (i, d) in dests.iter().enumerate() {
        assert_eq!(d.get(&format!("f{i}")).len(), 30_000);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn copies_to_one_destination_run_one_at_a_time() {
    let a = mac();
    let d = mac();
    for i in 0..3 {
        a.put(&format!("f{i}"), &pattern(30_000));
    }
    let gate = GateState::closed();
    let all = Arc::new(Meter::default());
    let (to, own) = gated(&d, "one-host", &gate, &all);
    let to: Arc<dyn FileSource> = to;
    let (from, _) = gated(&a, "src", &gate, &Arc::default());
    let from: Arc<dyn FileSource> = from;
    let run = queue();
    let mut jobs = Vec::new();
    for i in 0..3 {
        jobs.push(job(&from, &a.path(&format!("f{i}")), &to, &d.path(""), None).await);
    }
    let batch = run.queue.submit(jobs);
    until("a copy is open", || own.now.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(own.now.load(Ordering::SeqCst), 1);
    gate.open();
    assert_eq!(run.done(batch).await.copied, 3);
    assert_eq!(own.peak.load(Ordering::SeqCst), 1);
}

// ---- conflicts ----------------------------------------------------------------------------------

fn clash() -> (Mac, Mac) {
    let (a, b) = (mac(), mac());
    a.put("a.txt", b"new a");
    a.put("b.txt", b"new b");
    b.put("a.txt", b"old a");
    b.put("b.txt", b"old b");
    (a, b)
}

async fn copy_both(a: &Mac, b: &Mac, rule: Option<Conflict>) -> (Run, u64) {
    let run = queue();
    let batch = run.queue.submit(vec![
        job(&a.source, &a.path("a.txt"), &b.source, &b.path(""), rule).await,
        job(&a.source, &a.path("b.txt"), &b.source, &b.path(""), rule).await,
    ]);
    (run, batch)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_preset_rule_replaces_keeps_both_or_skips_without_asking() {
    let (a, b) = clash();
    let (run, batch) = copy_both(&a, &b, Some(Conflict::Replace)).await;
    assert_eq!(run.done(batch).await.copied, 2);
    assert_eq!(
        (b.get("a.txt"), b.get("b.txt")),
        (b"new a".to_vec(), b"new b".to_vec())
    );
    assert_eq!(b.names(""), ["a.txt", "b.txt"]);

    let (a, b) = clash();
    let (run, batch) = copy_both(&a, &b, Some(Conflict::KeepBoth)).await;
    assert_eq!(run.done(batch).await.copied, 2);
    assert_eq!(b.names(""), ["a 2.txt", "a.txt", "b 2.txt", "b.txt"]);
    assert_eq!(
        (b.get("a.txt"), b.get("a 2.txt")),
        (b"old a".to_vec(), b"new a".to_vec())
    );

    let (a, b) = clash();
    let (run, batch) = copy_both(&a, &b, Some(Conflict::Skip)).await;
    let s = run.done(batch).await;
    assert_eq!((s.copied, s.skipped), (0, 2));
    assert_eq!(
        (b.get("a.txt"), b.get("b.txt")),
        (b"old a".to_vec(), b"old b".to_vec())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_rule_each_clash_is_asked_and_answered_on_its_own() {
    let (a, b) = clash();
    let (run, batch) = copy_both(&a, &b, None).await;
    let mut asked = Vec::new();
    let (summary, _) = run
        .finish(batch, |q, question| {
            asked.push(question.name.clone());
            let choice = if question.name == "a.txt" {
                Conflict::Replace
            } else {
                Conflict::Skip
            };
            q.answer(question.id, choice, false);
        })
        .await;
    assert_eq!(asked, ["a.txt", "b.txt"]);
    assert_eq!((summary.copied, summary.skipped), (1, 1));
    assert_eq!(
        (b.get("a.txt"), b.get("b.txt")),
        (b"new a".to_vec(), b"old b".to_vec())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_to_all_answers_the_rest_of_the_batch_without_asking_again() {
    let (a, b) = clash();
    let (run, batch) = copy_both(&a, &b, None).await;
    let mut asked = 0;
    let (summary, _) = run
        .finish(batch, |q, question| {
            asked += 1;
            q.answer(question.id, Conflict::KeepBoth, true);
        })
        .await;
    assert_eq!(asked, 1);
    assert_eq!(summary.copied, 2);
    assert_eq!(b.names(""), ["a 2.txt", "a.txt", "b 2.txt", "b.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_onto_a_folder_of_the_same_name_follows_the_rule() {
    let (a, b) = (mac(), mac());
    a.put("d/one", b"new one");
    a.put("d/two", b"new two");
    b.put("d/one", b"old one");
    b.put("d/keep", b"kept");
    let run = queue();
    let batch = run.queue.submit(vec![
        job(
            &a.source,
            &a.path("d"),
            &b.source,
            &b.path(""),
            Some(Conflict::Replace),
        )
        .await,
    ]);
    run.done(batch).await;
    assert_eq!(b.names("d"), ["keep", "one", "two"]);
    assert_eq!(b.get("d/one"), b"new one");

    let (a, b) = (mac(), mac());
    a.put("d/one", b"x");
    b.put("d/one", b"old");
    let batch = run.queue.submit(vec![
        job(
            &a.source,
            &a.path("d"),
            &b.source,
            &b.path(""),
            Some(Conflict::KeepBoth),
        )
        .await,
    ]);
    run.done(batch).await;
    assert_eq!(b.get("d/one"), b"old");
    assert_eq!(b.get("d 2/one"), b"x");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_onto_itself_or_a_folder_into_itself_is_refused() {
    let a = mac();
    a.put("f", b"data");
    a.put("d/x", b"x");
    let run = queue();
    let same = run.queue.submit(vec![
        job(
            &a.source,
            &a.path("f"),
            &a.source,
            &a.path(""),
            Some(Conflict::Replace),
        )
        .await,
    ]);
    let s = run.done(same).await;
    assert_eq!((s.copied, s.failed.len()), (0, 1));
    assert_eq!(a.get("f"), b"data");
    let into = run.queue.submit(vec![
        job(&a.source, &a.path("d"), &a.source, &a.path("d"), None).await,
    ]);
    let s = run.done(into).await;
    assert_eq!((s.copied, s.failed.len()), (0, 1));
    assert_eq!(a.names("d"), ["x"]);
}
