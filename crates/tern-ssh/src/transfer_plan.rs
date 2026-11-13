//! The two halves of a copy that the queue's bookkeeping leaves out: the planner, which walks the
//! source folders, makes the destination folders, settles name clashes and hands the queue one
//! job per file; and the runner, which copies one file through a `.part` name and renames it
//! when whole.
use std::collections::HashMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::files::{self, FileEntry, FileSource, PART_SUFFIX, keep_both};
use crate::transfer::{Conflict, CopyJob, FileJob, Inner, Moved, Stopped};

/// Bytes moved per read and write, and so how often progress is noted.
const CHUNK: usize = 64 * 1024;

async fn stopped(stop: &mut watch::Receiver<bool>) {
    if stop.wait_for(|s| *s).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Plans every job of a batch, one after the other, then tells the queue planning is over.
pub(crate) async fn plan(
    inner: Arc<Inner>,
    batch: u64,
    jobs: Vec<CopyJob>,
    stop: watch::Receiver<bool>,
) {
    for job in jobs {
        let origin = if job.moving {
            inner.moving(batch, &job.from, &job.item.path)
        } else {
            None
        };
        let mut planner = Planner {
            inner: inner.clone(),
            batch,
            origin,
            from: job.from,
            to: job.to,
            stop: stop.clone(),
            listed: HashMap::new(),
        };
        planner.item(job.item, job.dir, job.rule, 0).await;
    }
    inner.planned(batch).await;
}

struct Planner {
    inner: Arc<Inner>,
    batch: u64,
    /// The moved item being planned, when the job is a move.
    origin: Option<usize>,
    from: Arc<dyn FileSource>,
    to: Arc<dyn FileSource>,
    stop: watch::Receiver<bool>,
    /// Destination folders already listed: name to whether it is a folder. This planner is the
    /// only writer, so what it adds is kept here instead of listing again.
    listed: HashMap<String, HashMap<String, bool>>,
}

impl Planner {
    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    fn fail(&self, line: String) {
        self.inner.note(self.batch, self.origin, Some(line));
    }

    fn same_place(&self) -> bool {
        self.from.id() == self.to.id()
    }

    async fn names_in(
        &mut self,
        dir: &str,
    ) -> Result<&mut HashMap<String, bool>, files::FileError> {
        if !self.listed.contains_key(dir) {
            let names = self
                .to
                .list(dir)
                .await?
                .into_iter()
                .map(|e| (e.name, e.is_dir))
                .collect();
            self.listed.insert(dir.to_owned(), names);
        }
        Ok(self.listed.entry(dir.to_owned()).or_default())
    }

    /// The free name Keep both gives `item` in `dir`: the number goes before a compound
    /// extension (`dump 2.sql.gz`).
    async fn keep_both_name(&mut self, item: &FileEntry, dir: &str) -> String {
        match self.names_in(dir).await {
            Ok(names) => keep_both(&item.name, item.is_dir, |n| names.contains_key(n)),
            Err(_) => keep_both(&item.name, item.is_dir, |_| false),
        }
    }

    /// The rule for a clash: the job's, then the batch's, else the person's answer.
    async fn settle(
        &mut self,
        rule: Option<Conflict>,
        item: &FileEntry,
        dir: &str,
        taken_is_dir: bool,
    ) -> Option<Conflict> {
        if let Some(rule) = rule.or_else(|| self.inner.rule(self.batch)) {
            return Some(rule);
        }
        let keep_both = self.keep_both_name(item, dir).await;
        let answer = self
            .inner
            .ask(self.batch, item.name.clone(), taken_is_dir, keep_both)
            .await;
        tokio::select! {
            choice = answer => choice.ok(),
            () = stopped(&mut self.stop) => None,
        }
    }

    fn item<'a>(
        &'a mut self,
        item: FileEntry,
        dir: String,
        rule: Option<Conflict>,
        depth: usize,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.stopped() {
                return;
            }
            if item.is_symlink && item.is_dir && depth > 0 {
                // A link to a folder inside a folder could lead back to it; copying it as a
                // folder could never end.
                return self.inner.note_link(self.batch, self.origin);
            }
            if item.is_dir
                && self.same_place()
                && (dir == item.path || dir.starts_with(&format!("{}/", item.path)))
            {
                return self.fail(format!(
                    "{}: a folder cannot be copied into itself",
                    item.path
                ));
            }
            let taken = match self.names_in(&dir).await {
                Ok(names) => names.get(&item.name).copied(),
                Err(e) => return self.fail(e.to_string()),
            };
            let mut name = item.name.clone();
            let mut merge = false;
            let mut replace = false;
            let mut inner_rule = rule;
            if let Some(is_dir) = taken {
                // No answer only when the batch was cancelled, which removes nothing.
                let Some(choice) = self.settle(rule, &item, &dir, is_dir).await else {
                    return;
                };
                match choice {
                    Conflict::Skip => return self.inner.note(self.batch, self.origin, None),
                    Conflict::KeepBoth => name = self.keep_both_name(&item, &dir).await,
                    Conflict::Replace if is_dir != item.is_dir => {
                        let what = if is_dir { "a folder" } else { "a file" };
                        return self.fail(format!(
                            "{}: {what} with that name is in the way",
                            files::join(&dir, &item.name)
                        ));
                    }
                    Conflict::Replace => {
                        // A folder merged into itself would copy nothing, and a move would
                        // then remove it.
                        if self.same_place() && files::join(&dir, &name) == item.path {
                            let what = if item.is_dir { "folder" } else { "file" };
                            return self.fail(format!(
                                "{}: the source and the destination are the same {what}",
                                item.path
                            ));
                        }
                        merge = item.is_dir;
                        replace = !item.is_dir;
                        inner_rule = Some(Conflict::Replace);
                    }
                }
            }
            let dest = files::join(&dir, &name);
            if let Ok(names) = self.names_in(&dir).await {
                names.insert(name.clone(), item.is_dir);
            }
            if !item.is_dir {
                return self.inner.push(FileJob {
                    batch: self.batch,
                    from: self.from.clone(),
                    to: self.to.clone(),
                    src: item.path,
                    dest,
                    name,
                    size: item.size,
                    replace,
                    origin: self.origin,
                });
            }
            if !merge {
                if let Err(e) = self.to.mkdir(&dest).await {
                    return self.fail(e.to_string());
                }
                self.listed.insert(dest.clone(), HashMap::new());
            }
            self.inner.walked(self.batch, self.origin, &item.path);
            let children = match self.from.list(&item.path).await {
                Ok(children) => children,
                Err(e) => return self.fail(e.to_string()),
            };
            for child in children {
                self.item(child, dest.clone(), inner_rule, depth + 1).await;
            }
        })
    }
}

/// Removes each moved item whose copy landed whole, then lets the batch end. Only what was
/// copied goes: its files, then its folders innermost first, each only if empty by then, so a
/// file that arrived during the copy stays with its folder. What cannot go is a failure line:
/// the copy is there, and so is the original.
pub(crate) async fn remove_moved(inner: Arc<Inner>, batch: u64, moved: Vec<Moved>) {
    let mut failed = Vec::new();
    let mut left: Vec<(String, String)> = Vec::new();
    for m in moved {
        let mut kept = false;
        for file in &m.files {
            if let Err(e) = m.from.delete(file).await {
                failed.push(format!("{file}: copied, but not removed: {e}"));
                kept = true;
            }
        }
        for dir in m.dirs.iter().rev() {
            match m.from.list(dir).await {
                Ok(inside) if inside.is_empty() => {
                    if let Err(e) = m.from.delete(dir).await {
                        failed.push(format!("{dir}: copied, but not removed: {e}"));
                        kept = true;
                    }
                }
                Ok(_) if !kept => {
                    failed.push(format!(
                        "{dir}: left in place, it holds files that were not moved"
                    ));
                    kept = true;
                }
                Ok(_) => {}
                Err(e) => {
                    failed.push(format!("{dir}: copied, but not removed: {e}"));
                    kept = true;
                }
            }
        }
        let place = (m.from.id().to_owned(), files::parent(&m.path));
        if !left.contains(&place) {
            left.push(place);
        }
    }
    inner.removed(batch, failed, left).await;
}

/// Copies one file and reports how it went to the queue.
pub(crate) async fn run(inner: Arc<Inner>, job: FileJob, mut stop: watch::Receiver<bool>) {
    let result = copy(&inner, &job, &mut stop).await;
    inner.finished(job, result).await;
}

async fn copy(
    inner: &Inner,
    job: &FileJob,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), Stopped> {
    let part = format!("{}{PART_SUFFIX}", job.dest);
    let result = copy_through(inner, job, &part, stop).await;
    if result.is_err() {
        let _ = job.to.delete(&part).await;
    }
    result
}

fn failed(path: &str, e: impl std::fmt::Display) -> Stopped {
    Stopped::Failed(format!("{path}: {e}"))
}

async fn copy_through(
    inner: &Inner,
    job: &FileJob,
    part: &str,
    stop: &mut watch::Receiver<bool>,
) -> Result<(), Stopped> {
    if *stop.borrow() {
        return Err(Stopped::Cancelled);
    }
    let mut reader = job
        .from
        .read(&job.src)
        .await
        .map_err(|e| Stopped::Failed(e.to_string()))?;
    let mut writer = job
        .to
        .write(part)
        .await
        .map_err(|e| Stopped::Failed(e.to_string()))?;
    let moved = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(|e| failed(&job.src, e))?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .await
                .map_err(|e| failed(&job.dest, e))?;
            inner.advance(job.batch, &job.name, n as u64);
        }
        writer.shutdown().await.map_err(|e| failed(&job.dest, e))
    };
    tokio::select! {
        done = moved => done?,
        () = stopped(stop) => return Err(Stopped::Cancelled),
    }
    drop(reader);
    if !job.replace {
        return job
            .to
            .rename(part, &job.dest)
            .await
            .map_err(|e| Stopped::Failed(e.to_string()));
    }
    let aside = aside_name(&job.dest);
    let moved_aside = match job.to.rename(&job.dest, &aside).await {
        Ok(()) => true,
        Err(files::FileError::NotFound(_)) => false,
        Err(e) => return Err(Stopped::Failed(e.to_string())),
    };
    if let Err(e) = job.to.rename(part, &job.dest).await {
        if moved_aside {
            let _ = job.to.rename(&aside, &job.dest).await;
        }
        return Err(Stopped::Failed(e.to_string()));
    }
    if moved_aside {
        match job.to.delete(&aside).await {
            Ok(()) | Err(files::FileError::NotFound(_)) => {}
            Err(e) => return Err(Stopped::Failed(e.to_string())),
        }
    }
    Ok(())
}

/// Where the old file waits while its replacement is renamed into place: hidden, beside it.
fn aside_name(dest: &str) -> String {
    files::join(
        &files::parent(dest),
        &format!(".{}.replaced", files::file_name(dest)),
    )
}
