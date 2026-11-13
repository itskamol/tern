//! Moving through the transfer queue, and the name a clash question offers for Keep both. A
//! move is a copy that removes each item's source once everything in it landed, so these tests
//! look at the source as well as the destination. Kept apart from `transfer.rs` to avoid growing
//! a 1,000-line test file.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{serve_sftp, spec, start};
use tern_ssh::{
    Call, Conflict, CopyJob, Event, FileEntry, FileError, FileSource, LocalFs, Question, Reader,
    SftpFiles, Summary, TransferQueue, Writer,
};
use tokio::sync::Semaphore;

fn pattern(len: u32) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
}

fn put(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

fn text(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

/// A temp dir seen as the Mac, with its own Trash so a removed source can be found again.
struct Mac {
    source: Arc<dyn FileSource>,
    dir: tempfile::TempDir,
    trash: tempfile::TempDir,
}

fn mac() -> Mac {
    let trash = tempfile::tempdir().unwrap();
    Mac {
        source: Arc::new(LocalFs::with_trash(trash.path().to_path_buf())),
        dir: tempfile::tempdir().unwrap(),
        trash,
    }
}

impl Mac {
    fn at(&self, rel: &str) -> std::path::PathBuf {
        self.dir.path().join(rel)
    }
    fn path(&self, rel: &str) -> String {
        text(&self.at(rel))
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
    fn disk(&self, rel: &str) -> std::path::PathBuf {
        self.root.path().join("home").join(rel)
    }
}

async fn entry(source: &Arc<dyn FileSource>, path: &str) -> FileEntry {
    let (dir, name) = (tern_ssh::parent(path), tern_ssh::file_name(path));
    source
        .list(&dir)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("{path} is not listed"))
}

async fn moving(
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
        moving: true,
    }
}

/// Events until the batch finishes, answering each question with `answer`; also runs `at_end`
/// the moment `Finished` arrives, before anything else can happen.
async fn finish(
    events: &async_channel::Receiver<Event>,
    queue: &TransferQueue,
    batch: u64,
    mut answer: impl FnMut(&Question) -> (Conflict, bool),
    at_end: impl FnOnce(),
) -> Summary {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), events.recv())
            .await
            .expect("the batch did not finish in time")
            .unwrap();
        match event {
            Event::Question(q) => {
                let (choice, all) = answer(&q);
                queue.answer(q.id, choice, all);
            }
            Event::Finished(s) if s.batch == batch => {
                at_end();
                return s;
            }
            _ => {}
        }
    }
}

fn no_question(q: &Question) -> (Conflict, bool) {
    panic!("unexpected question about {}", q.name)
}

fn queue() -> (TransferQueue, async_channel::Receiver<Event>) {
    TransferQueue::new(tokio::runtime::Handle::current())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_between_two_hosts_lands_byte_for_byte_then_removes_the_source() {
    let (h1, h2) = (host("one").await, host("two").await);
    let data = pattern(250_000);
    put(&h1.disk(""), "tree/sub/f.bin", &data);
    put(&h1.disk(""), "tree/g", b"g");
    let (q, events) = queue();
    let batch = q.submit(vec![
        moving(&h1.source, "/home/tree", &h2.source, "/home", None).await,
    ]);
    let s = finish(&events, &q, batch, no_question, || {}).await;
    assert_eq!((s.copied, s.failed.len()), (2, 0));
    assert_eq!(std::fs::read(h2.disk("tree/sub/f.bin")).unwrap(), data);
    assert_eq!(std::fs::read(h2.disk("tree/g")).unwrap(), b"g");
    assert!(
        !h1.disk("tree").exists(),
        "the moved folder left the source"
    );
    assert_eq!(
        s.removed_from,
        [("one".to_owned(), "/home".to_owned())],
        "the source's folder is named so a panel showing it lists it again"
    );
}

/// A Mac whose renames wait for a permit: the copy's last step, `.part` to its name, is held.
#[derive(Debug)]
struct Held {
    inner: Arc<dyn FileSource>,
    gate: Arc<Semaphore>,
}

impl FileSource for Held {
    fn id(&self) -> &str {
        "held"
    }
    fn home(&self) -> Call<'_, String> {
        self.inner.home()
    }
    fn list<'a>(&'a self, dir: &'a str) -> Call<'a, Vec<FileEntry>> {
        self.inner.list(dir)
    }
    fn read<'a>(&'a self, path: &'a str) -> Call<'a, Reader> {
        self.inner.read(path)
    }
    fn write<'a>(&'a self, path: &'a str) -> Call<'a, Writer> {
        self.inner.write(path)
    }
    fn mkdir<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        self.inner.mkdir(path)
    }
    fn rename<'a>(&'a self, from: &'a str, to: &'a str) -> Call<'a, ()> {
        Box::pin(async move {
            let _permit = self.gate.acquire().await.map_err(|_| FileError::Closed)?;
            self.inner.rename(from, to).await
        })
    }
    fn delete<'a>(&'a self, path: &'a str) -> Call<'a, ()> {
        self.inner.delete(path)
    }
}

async fn until(what: &str, ok: impl Fn() -> bool) {
    for _ in 0..400 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never happened: {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_source_stays_until_the_copy_is_whole_and_is_gone_when_the_batch_ends() {
    let (a, b) = (mac(), mac());
    std::fs::write(a.at("a.txt"), b"a").unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let to: Arc<dyn FileSource> = Arc::new(Held {
        inner: b.source.clone(),
        gate: gate.clone(),
    });
    let (q, events) = queue();
    let batch = q.submit(vec![
        moving(&a.source, &a.path("a.txt"), &to, &b.path(""), None).await,
    ]);
    until("the copy is written and waits for its name", || {
        b.at("a.txt.part").exists()
    })
    .await;
    assert!(a.at("a.txt").exists(), "nothing leaves before it landed");
    gate.add_permits(1);
    let source = a.at("a.txt");
    let s = finish(&events, &q, batch, no_question, || {
        assert!(
            !source.exists(),
            "the source is gone before the batch says it ended"
        );
    })
    .await;
    assert_eq!(std::fs::read(b.at("a.txt")).unwrap(), b"a");
    assert_eq!(s.failed, Vec::<String>::new());
    assert!(
        a.trash.path().join("a.txt").exists(),
        "a Mac's source goes to the Trash"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_move_keeps_its_source() {
    let (a, b) = (mac(), mac());
    std::fs::write(a.at("a.txt"), b"a").unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let to: Arc<dyn FileSource> = Arc::new(Held {
        inner: b.source.clone(),
        gate: gate.clone(),
    });
    let (q, events) = queue();
    let batch = q.submit(vec![
        moving(&a.source, &a.path("a.txt"), &to, &b.path(""), None).await,
    ]);
    until("the copy is under way", || b.at("a.txt.part").exists()).await;
    q.cancel(batch);
    gate.add_permits(1);
    let s = finish(&events, &q, batch, no_question, || {}).await;
    assert!(s.cancelled);
    assert_eq!(std::fs::read(a.at("a.txt")).unwrap(), b"a");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_skipped_or_failed_move_keeps_its_source() {
    let (a, b) = (mac(), mac());
    std::fs::write(a.at("a.txt"), b"new").unwrap();
    std::fs::write(b.at("a.txt"), b"old").unwrap();
    let (q, events) = queue();
    let batch = q.submit(vec![
        moving(&a.source, &a.path("a.txt"), &b.source, &b.path(""), None).await,
    ]);
    let s = finish(&events, &q, batch, |_| (Conflict::Skip, false), || {}).await;
    assert_eq!(s.skipped, 1);
    assert_eq!(std::fs::read(a.at("a.txt")).unwrap(), b"new");
    assert_eq!(std::fs::read(b.at("a.txt")).unwrap(), b"old");

    // A destination folder that is not there: the copy fails, the source stays.
    let batch = q.submit(vec![
        moving(
            &a.source,
            &a.path("a.txt"),
            &b.source,
            &b.path("gone"),
            None,
        )
        .await,
    ]);
    let s = finish(&events, &q, batch, no_question, || {}).await;
    assert_eq!(s.failed.len(), 1, "{:?}", s.failed);
    assert_eq!(std::fs::read(a.at("a.txt")).unwrap(), b"new");
    assert!(s.removed_from.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_with_one_file_that_fails_is_kept_whole_and_the_next_item_still_moves() {
    use std::os::unix::fs::PermissionsExt as _;
    let (a, b) = (mac(), mac());
    put(a.dir.path(), "tree/a.txt", b"a");
    put(a.dir.path(), "tree/locked.txt", b"locked");
    put(a.dir.path(), "c.txt", b"c");
    let locked = a.at("tree/locked.txt");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let (q, events) = queue();
    let batch = q.submit(vec![
        moving(&a.source, &a.path("tree"), &b.source, &b.path(""), None).await,
        moving(&a.source, &a.path("c.txt"), &b.source, &b.path(""), None).await,
    ]);
    let s = finish(&events, &q, batch, no_question, || {}).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!((s.copied, s.failed.len()), (2, 1), "{:?}", s.failed);
    assert_eq!(std::fs::read(b.at("tree/a.txt")).unwrap(), b"a");
    assert!(a.at("tree/a.txt").exists(), "a folder goes only whole");
    assert_eq!(std::fs::read(&locked).unwrap(), b"locked");
    assert!(!a.at("c.txt").exists(), "the file that landed moved");
    assert_eq!(std::fs::read(b.at("c.txt")).unwrap(), b"c");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_moved_onto_itself_is_refused_and_kept() {
    let a = mac();
    put(a.dir.path(), "tree/a.txt", b"a");
    let (q, events) = queue();
    let batch = q.submit(vec![
        moving(
            &a.source,
            &a.path("tree"),
            &a.source,
            &a.path(""),
            Some(Conflict::Replace),
        )
        .await,
    ]);
    let s = finish(&events, &q, batch, no_question, || {}).await;
    assert_eq!(s.failed.len(), 1, "{:?}", s.failed);
    assert_eq!(std::fs::read(a.at("tree/a.txt")).unwrap(), b"a");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clash_question_names_what_keep_both_would_call_it() {
    let (a, b) = (mac(), mac());
    std::fs::write(a.at("dump.sql.gz"), b"new").unwrap();
    std::fs::write(b.at("dump.sql.gz"), b"old").unwrap();
    std::fs::write(b.at("dump 2.sql.gz"), b"older").unwrap();
    let (q, events) = queue();
    let job = CopyJob {
        moving: false,
        ..moving(
            &a.source,
            &a.path("dump.sql.gz"),
            &b.source,
            &b.path(""),
            None,
        )
        .await
    };
    let batch = q.submit(vec![job]);
    let mut offered = String::new();
    let s = finish(
        &events,
        &q,
        batch,
        |q| {
            offered.clone_from(&q.keep_both);
            (Conflict::KeepBoth, false)
        },
        || {},
    )
    .await;
    assert_eq!(offered, "dump 3.sql.gz");
    assert_eq!(s.copied, 1);
    assert_eq!(std::fs::read(b.at("dump 3.sql.gz")).unwrap(), b"new");
    assert!(a.at("dump.sql.gz").exists(), "a copy keeps its source");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_added_to_a_moved_folder_during_the_copy_is_left_with_its_folder() {
    let (a, b) = (mac(), mac());
    put(a.dir.path(), "tree/a.txt", b"a");
    put(a.dir.path(), "tree/sub/b.txt", b"b");
    let gate = Arc::new(Semaphore::new(0));
    let to: Arc<dyn FileSource> = Arc::new(Held {
        inner: b.source.clone(),
        gate: gate.clone(),
    });
    let (q, events) = queue();
    let batch = q.submit(vec![
        moving(&a.source, &a.path("tree"), &to, &b.path(""), None).await,
    ]);
    until("the copy waits for its name", || {
        b.at("tree/sub/b.txt.part").exists()
    })
    .await;
    put(a.dir.path(), "tree/sub/late.txt", b"late");
    gate.add_permits(2);
    let s = finish(&events, &q, batch, no_question, || {}).await;
    assert_eq!(std::fs::read(b.at("tree/a.txt")).unwrap(), b"a");
    assert!(!a.at("tree/a.txt").exists(), "what was copied is removed");
    assert_eq!(
        std::fs::read(a.at("tree/sub/late.txt")).unwrap(),
        b"late",
        "what was never copied stays, and its folder with it"
    );
    assert_eq!(
        s.failed.len(),
        1,
        "the folder left behind is said: {:?}",
        s.failed
    );
}
