//! The transfer queue: copy jobs in, progress and questions out. It copies between any two
//! [`FileSource`]s, so one queue serves upload, download and host-to-host alike. A copy job names
//! one file or folder; folders are expanded into files by a planner (`transfer_plan`), and the
//! files are what runs: at most [`MAX_RUNNING`] at once and at most one per destination.
//!
//! The queue knows nothing about the UI and owns its tasks on the runtime it is given, so it
//! outlives any view. Events leave over a bounded channel: progress is dropped when the consumer
//! falls behind, answers to the person's questions and the end of a batch never are.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};

use crate::files::{FileEntry, FileSource};

/// Copies running at once.
pub const MAX_RUNNING: usize = 2;
/// Events waiting for the consumer before progress starts being dropped.
const EVENT_BACKLOG: usize = 256;
/// The least time between progress events of one batch.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// Speed is the average over this long.
const SPEED_WINDOW: Duration = Duration::from_secs(3);

/// What to do when a name is already taken at the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// Write over it (a Mac's old file goes to the Trash).
    Replace,
    /// Copy under a free name: `name 2`.
    KeepBoth,
    Skip,
}

/// One thing to copy: a file or a folder (with everything in it) into a destination folder.
#[derive(Debug, Clone)]
pub struct CopyJob {
    pub from: Arc<dyn FileSource>,
    pub item: FileEntry,
    pub to: Arc<dyn FileSource>,
    /// The destination folder, which exists.
    pub dir: String,
    /// What to do about a name already there; `None` asks the person.
    pub rule: Option<Conflict>,
    /// Move instead of copy: the source goes once everything in it landed, and stays if any
    /// part was skipped, failed or cancelled.
    pub moving: bool,
}

/// A name is taken: the person picks a [`Conflict`] and answers with [`TransferQueue::answer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: u64,
    pub batch: u64,
    /// The name at the destination.
    pub name: String,
    pub is_dir: bool,
    /// The name Keep both would give the copy, so the person sees it before choosing.
    pub keep_both: String,
}

/// How far a batch has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub batch: u64,
    /// A file being copied now.
    pub current: String,
    pub done: u64,
    pub total: u64,
    pub files_done: usize,
    pub files_total: usize,
    /// Bytes per second over the last few seconds.
    pub speed: u64,
    pub eta: Option<Duration>,
}

/// How a batch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub batch: u64,
    /// The destination's [`FileSource::id`] and folder, so a panel showing it can refresh.
    pub dest: String,
    pub dir: String,
    pub copied: usize,
    pub skipped: usize,
    /// Folders inside a folder that are links to another folder: not followed, since a link
    /// can lead back and the copy could never end.
    pub links: usize,
    /// One line for each file or folder that could not be copied.
    pub failed: Vec<String>,
    pub cancelled: bool,
    /// Each moved source's [`FileSource::id`] and the folder it left, once: a panel showing one
    /// lists it again.
    pub removed_from: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Progress(Progress),
    Question(Question),
    /// Always the last event of a batch.
    Finished(Summary),
}

/// A file waiting or running.
pub(crate) struct FileJob {
    pub(crate) batch: u64,
    pub(crate) from: Arc<dyn FileSource>,
    pub(crate) to: Arc<dyn FileSource>,
    pub(crate) src: String,
    pub(crate) dest: String,
    pub(crate) name: String,
    pub(crate) size: u64,
    /// A file already has the destination name and is replaced when this one is whole.
    pub(crate) replace: bool,
    /// The moved item this file is part of, by its place in [`Batch::moves`].
    pub(crate) origin: Option<usize>,
}

/// An item being moved: when the batch's copies are over, and if all of it landed, the files
/// copied go and then each folder walked, if nothing new arrived in it meanwhile.
pub(crate) struct Moved {
    pub(crate) from: Arc<dyn FileSource>,
    pub(crate) path: String,
    landed: bool,
    /// The source files queued to copy.
    pub(crate) files: Vec<String>,
    /// The source folders walked, outermost first.
    pub(crate) dirs: Vec<String>,
}

struct Ask {
    batch: u64,
    reply: oneshot::Sender<Conflict>,
}

#[derive(Default)]
struct Rate(VecDeque<(Instant, u64)>);

impl Rate {
    fn note(&mut self, now: Instant, done: u64) -> u64 {
        self.0.push_back((now, done));
        while self
            .0
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > SPEED_WINDOW)
        {
            self.0.pop_front();
        }
        match (self.0.front(), self.0.back()) {
            (Some((t0, d0)), Some((t1, d1)))
                if t1.duration_since(*t0) >= Duration::from_millis(200) =>
            {
                let secs = t1.duration_since(*t0).as_secs_f64();
                ((d1 - d0) as f64 / secs) as u64
            }
            _ => 0,
        }
    }
}

pub(crate) struct Batch {
    dest: String,
    dir: String,
    pub(crate) rule: Option<Conflict>,
    planning: bool,
    /// Files waiting or running.
    outstanding: usize,
    files_total: usize,
    files_done: usize,
    bytes_total: u64,
    bytes_done: u64,
    copied: usize,
    skipped: usize,
    links: usize,
    failed: Vec<String>,
    cancelled: bool,
    moves: Vec<Moved>,
    /// The copies are over and the moved sources are being removed.
    removing: bool,
    removed_from: Vec<(String, String)>,
    stop: watch::Sender<bool>,
    rate: Rate,
    last_progress: Option<Instant>,
    speed: u64,
    current: String,
}

impl Batch {
    fn progress(&self, id: u64) -> Progress {
        let left = self.bytes_total.saturating_sub(self.bytes_done);
        Progress {
            batch: id,
            current: self.current.clone(),
            done: self.bytes_done,
            total: self.bytes_total,
            files_done: self.files_done,
            files_total: self.files_total,
            speed: self.speed,
            eta: (self.speed > 0).then(|| Duration::from_secs(left / self.speed)),
        }
    }

    fn is_over(&self) -> bool {
        !self.planning && self.outstanding == 0 && !self.removing
    }

    /// A part of a moved item did not land: its source stays.
    fn spoil(&mut self, origin: Option<usize>) {
        if let Some(m) = origin.and_then(|ix| self.moves.get_mut(ix)) {
            m.landed = false;
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    next: u64,
    waiting: VecDeque<FileJob>,
    /// The destination of each running copy, with its batch.
    running: Vec<(u64, String)>,
    batches: HashMap<u64, Batch>,
    questions: HashMap<u64, Ask>,
}

pub(crate) struct Inner {
    pub(crate) rt: Handle,
    tx: Sender<Event>,
    state: Mutex<State>,
}

/// The queue. Cheap to clone; every clone is the same queue.
#[derive(Debug, Clone)]
pub struct TransferQueue(pub(crate) Arc<Inner>);

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TransferQueue")
    }
}

impl TransferQueue {
    /// A queue running its copies on `rt`, and the events it reports.
    #[must_use]
    pub fn new(rt: Handle) -> (Self, Receiver<Event>) {
        let (tx, rx) = async_channel::bounded(EVENT_BACKLOG);
        let inner = Inner {
            rt,
            tx,
            state: Mutex::new(State::default()),
        };
        (Self(Arc::new(inner)), rx)
    }

    /// Queues the jobs as one batch and returns its number. The batch ends with exactly one
    /// [`Event::Finished`].
    pub fn submit(&self, jobs: Vec<CopyJob>) -> u64 {
        let (dest, dir) = jobs
            .first()
            .map(|j| (j.to.id().to_owned(), j.dir.clone()))
            .unwrap_or_default();
        let (stop, stopped) = watch::channel(false);
        let id = {
            let mut st = self.0.lock();
            st.next += 1;
            let id = st.next;
            st.batches.insert(
                id,
                Batch {
                    dest,
                    dir,
                    rule: None,
                    planning: true,
                    outstanding: 0,
                    files_total: 0,
                    files_done: 0,
                    bytes_total: 0,
                    bytes_done: 0,
                    copied: 0,
                    skipped: 0,
                    links: 0,
                    failed: Vec::new(),
                    cancelled: false,
                    moves: Vec::new(),
                    removing: false,
                    removed_from: Vec::new(),
                    stop,
                    rate: Rate::default(),
                    last_progress: None,
                    speed: 0,
                    current: String::new(),
                },
            );
            id
        };
        let inner = self.0.clone();
        self.0
            .rt
            .spawn(crate::transfer_plan::plan(inner, id, jobs, stopped));
        id
    }

    /// The person's answer to a [`Question`]; with `all`, the rest of the batch gets it too.
    pub fn answer(&self, question: u64, choice: Conflict, all: bool) {
        let ask = {
            let mut st = self.0.lock();
            let ask = st.questions.remove(&question);
            if all && let Some(batch) = ask.as_ref().and_then(|a| st.batches.get_mut(&a.batch)) {
                batch.rule = Some(choice);
            }
            ask
        };
        if let Some(ask) = ask {
            let _ = ask.reply.send(choice);
        }
    }

    /// Stops a batch: running copies are cut off and their `.part` files removed, waiting ones
    /// never start.
    pub fn cancel(&self, batch: u64) {
        let events = {
            let mut st = self.0.lock();
            st.cancel(batch)
        };
        self.0.emit_later(events);
    }

    pub fn cancel_all(&self) {
        let events = {
            let mut st = self.0.lock();
            let ids: Vec<u64> = st.batches.keys().copied().collect();
            ids.into_iter().flat_map(|id| st.cancel(id)).collect()
        };
        self.0.emit_later(events);
    }

    /// Copies are running, waiting or being planned: quitting now would cut them off.
    #[must_use]
    pub fn busy(&self) -> bool {
        !self.0.lock().batches.is_empty()
    }
}

impl State {
    fn cancel(&mut self, id: u64) -> Vec<Event> {
        let Some(batch) = self.batches.get_mut(&id) else {
            return Vec::new();
        };
        batch.cancelled = true;
        let _ = batch.stop.send(true);
        let before = self.waiting.len();
        self.waiting.retain(|j| j.batch != id);
        let dropped = before - self.waiting.len();
        if let Some(batch) = self.batches.get_mut(&id) {
            batch.outstanding -= dropped;
        }
        self.questions.retain(|_, ask| ask.batch != id);
        // Cancelled, so there is nothing to remove: only the closing events.
        self.end_if_over(id).events
    }

    /// The batch's closing events, once nothing of it is left. A cancelled batch never
    /// removes a source: what is left of its copies is not the whole item.
    fn end_if_over(&mut self, id: u64) -> Ending {
        let Some(batch) = self.batches.get_mut(&id).filter(|b| b.is_over()) else {
            return Ending::default();
        };
        if !batch.cancelled {
            let remove: Vec<Moved> = batch.moves.drain(..).filter(|m| m.landed).collect();
            if !remove.is_empty() {
                batch.removing = true;
                return Ending {
                    events: Vec::new(),
                    remove,
                };
            }
        }
        let Some(batch) = self.batches.remove(&id) else {
            return Ending::default();
        };
        let events = vec![
            Event::Progress(batch.progress(id)),
            Event::Finished(Summary {
                batch: id,
                dest: batch.dest,
                dir: batch.dir,
                copied: batch.copied,
                skipped: batch.skipped,
                links: batch.links,
                failed: batch.failed,
                cancelled: batch.cancelled,
                removed_from: batch.removed_from,
            }),
        ];
        Ending {
            events,
            remove: Vec::new(),
        }
    }

    /// Moves waiting files into the running set while there is room, one per destination.
    fn start(&mut self) -> Vec<(FileJob, watch::Receiver<bool>)> {
        let mut started = Vec::new();
        while self.running.len() < MAX_RUNNING {
            let Some(at) = self
                .waiting
                .iter()
                .position(|j| !self.running.iter().any(|(_, d)| d == j.to.id()))
            else {
                break;
            };
            let Some(job) = self.waiting.remove(at) else {
                break;
            };
            let Some(stop) = self.batches.get(&job.batch).map(|b| b.stop.subscribe()) else {
                continue;
            };
            self.running.push((job.batch, job.to.id().to_owned()));
            started.push((job, stop));
        }
        started
    }
}

impl Inner {
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends events that must arrive, from wherever the caller runs.
    pub(crate) fn emit_later(self: &Arc<Self>, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        let this = self.clone();
        self.rt.spawn(async move { this.emit(events).await });
    }

    pub(crate) async fn emit(&self, events: Vec<Event>) {
        for event in events {
            let _ = self.tx.send(event).await;
        }
    }

    /// The events to send now; moved sources are removed first, then the batch ends.
    fn ended(self: &Arc<Self>, batch: u64, ending: Ending) -> Vec<Event> {
        if !ending.remove.is_empty() {
            self.rt.spawn(crate::transfer_plan::remove_moved(
                self.clone(),
                batch,
                ending.remove,
            ));
        }
        ending.events
    }

    /// An item of the batch is moved; returns its place in [`Batch::moves`].
    pub(crate) fn moving(
        &self,
        batch: u64,
        from: &Arc<dyn FileSource>,
        path: &str,
    ) -> Option<usize> {
        let mut st = self.lock();
        let b = st.batches.get_mut(&batch)?;
        b.moves.push(Moved {
            from: from.clone(),
            path: path.to_owned(),
            landed: true,
            files: Vec::new(),
            dirs: Vec::new(),
        });
        Some(b.moves.len() - 1)
    }

    /// A folder of a moved item is being walked: it goes after its files, if it is empty then.
    pub(crate) fn walked(&self, batch: u64, origin: Option<usize>, dir: &str) {
        let mut st = self.lock();
        let moved = st
            .batches
            .get_mut(&batch)
            .and_then(|b| b.moves.get_mut(origin?));
        if let Some(m) = moved {
            m.dirs.push(dir.to_owned());
        }
    }

    /// The moved sources are removed (or `failed` says why not); the batch can end.
    pub(crate) async fn removed(
        self: &Arc<Self>,
        batch: u64,
        failed: Vec<String>,
        left: Vec<(String, String)>,
    ) {
        let ending = {
            let mut st = self.lock();
            if let Some(b) = st.batches.get_mut(&batch) {
                b.removing = false;
                b.failed.extend(failed);
                b.removed_from = left;
            }
            st.end_if_over(batch)
        };
        let events = self.ended(batch, ending);
        self.emit(events).await;
    }

    fn spawn_started(self: &Arc<Self>, started: Vec<(FileJob, watch::Receiver<bool>)>) {
        for (job, stop) in started {
            self.rt
                .spawn(crate::transfer_plan::run(self.clone(), job, stop));
        }
    }

    /// A file is planned: it waits for a slot.
    pub(crate) fn push(self: &Arc<Self>, job: FileJob) {
        let started = {
            let mut st = self.lock();
            let Some(batch) = st.batches.get_mut(&job.batch).filter(|b| !b.cancelled) else {
                return;
            };
            batch.outstanding += 1;
            batch.files_total += 1;
            batch.bytes_total += job.size;
            if let Some(m) = job.origin.and_then(|ix| batch.moves.get_mut(ix)) {
                m.files.push(job.src.clone());
            }
            st.waiting.push_back(job);
            st.start()
        };
        self.spawn_started(started);
    }

    /// A name is taken and the batch has no rule: asks the person.
    pub(crate) async fn ask(
        &self,
        batch: u64,
        name: String,
        is_dir: bool,
        keep_both: String,
    ) -> oneshot::Receiver<Conflict> {
        let (reply, answer) = oneshot::channel();
        let question = {
            let mut st = self.lock();
            if let Some(rule) = st.batches.get(&batch).and_then(|b| b.rule) {
                let _ = reply.send(rule);
                return answer;
            }
            st.next += 1;
            let id = st.next;
            st.questions.insert(id, Ask { batch, reply });
            Question {
                id,
                batch,
                name,
                is_dir,
                keep_both,
            }
        };
        self.emit(vec![Event::Question(question)]).await;
        answer
    }

    /// Notes a failure or a skip of something that never became a file job; the moved item
    /// it is part of keeps its source.
    pub(crate) fn note(&self, batch: u64, origin: Option<usize>, failure: Option<String>) {
        if let Some(b) = self.lock().batches.get_mut(&batch) {
            b.spoil(origin);
            match failure {
                Some(line) => b.failed.push(line),
                None => b.skipped += 1,
            }
        }
    }

    /// Notes a folder link that was not followed: what it leads to was not copied.
    pub(crate) fn note_link(&self, batch: u64, origin: Option<usize>) {
        if let Some(b) = self.lock().batches.get_mut(&batch) {
            b.spoil(origin);
            b.links += 1;
        }
    }

    /// Planning is over; the batch ends when its files do.
    pub(crate) async fn planned(self: &Arc<Self>, batch: u64) {
        let ending = {
            let mut st = self.lock();
            if let Some(b) = st.batches.get_mut(&batch) {
                b.planning = false;
            }
            st.end_if_over(batch)
        };
        let events = self.ended(batch, ending);
        self.emit(events).await;
    }

    /// `bytes` more of a file are copied; reports at most every [`PROGRESS_EVERY`].
    pub(crate) fn advance(&self, batch: u64, name: &str, bytes: u64) {
        let event = {
            let mut st = self.lock();
            let Some(b) = st.batches.get_mut(&batch) else {
                return;
            };
            b.bytes_done += bytes;
            let now = Instant::now();
            b.speed = b.rate.note(now, b.bytes_done);
            if b.current != name {
                b.current = name.to_owned();
            }
            if b.last_progress
                .is_some_and(|t| now.duration_since(t) < PROGRESS_EVERY)
            {
                return;
            }
            b.last_progress = Some(now);
            Event::Progress(b.progress(batch))
        };
        let _ = self.tx.try_send(event);
    }

    /// A running file ended; the next waiting one may start.
    pub(crate) async fn finished(self: &Arc<Self>, job: FileJob, result: Result<(), Stopped>) {
        let (ending, started) = {
            let mut st = self.lock();
            if let Some(at) = st
                .running
                .iter()
                .position(|(b, d)| *b == job.batch && d == job.to.id())
            {
                st.running.remove(at);
            }
            if let Some(b) = st.batches.get_mut(&job.batch) {
                b.outstanding -= 1;
                match result {
                    Ok(()) => {
                        b.copied += 1;
                        b.files_done += 1;
                    }
                    Err(Stopped::Cancelled) => b.spoil(job.origin),
                    Err(Stopped::Failed(why)) => {
                        b.spoil(job.origin);
                        b.failed.push(why);
                    }
                }
            }
            let ending = st.end_if_over(job.batch);
            (ending, st.start())
        };
        self.spawn_started(started);
        let events = self.ended(job.batch, ending);
        self.emit(events).await;
    }

    pub(crate) fn rule(&self, batch: u64) -> Option<Conflict> {
        self.lock().batches.get(&batch).and_then(|b| b.rule)
    }
}

/// What follows the end of a batch's copies.
#[derive(Default)]
pub(crate) struct Ending {
    events: Vec<Event>,
    /// Moved sources to remove before the closing events.
    remove: Vec<Moved>,
}

/// Why a file's copy did not complete.
pub(crate) enum Stopped {
    Cancelled,
    Failed(String),
}
