//! The shell's copies: the one transfer queue and what the window shows of it. The queue lives
//! here, not in a Files tab, so closing the tab does not stop a copy; the tab only reads the
//! rows. A name clash is a question the person answers wherever they are, so it waits here too.

use std::collections::VecDeque;
use std::time::Duration;

use gpui::{Context, EventEmitter, Task};
use tern_ssh::{CopyJob, Event, Question, Summary, TransferProgress as Progress, TransferQueue};

/// One batch the person started and has not seen finish.
#[derive(Debug, Clone)]
pub struct Row {
    pub batch: u64,
    /// What was copied and where: "3 items to web-01".
    pub label: String,
    pub progress: Option<Progress>,
}

/// A batch ended; the shell tells the person, and a Files tab refreshes what it shows.
#[derive(Debug, Clone)]
pub struct Finished {
    pub label: String,
    pub summary: Summary,
}

pub struct Transfers {
    queue: TransferQueue,
    rows: Vec<Row>,
    questions: VecDeque<Question>,
    _events: Task<()>,
}

impl EventEmitter<Finished> for Transfers {}

impl Transfers {
    pub fn new(rt: tokio::runtime::Handle, cx: &mut Context<Self>) -> Self {
        let (queue, events) = TransferQueue::new(rt);
        let task = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this.update(cx, |t, cx| t.apply(event, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            queue,
            rows: Vec::new(),
            questions: VecDeque::new(),
            _events: task,
        }
    }

    /// Queues the jobs as one batch named `label`.
    pub fn submit(&mut self, jobs: Vec<CopyJob>, label: String, cx: &mut Context<Self>) {
        let batch = self.queue.submit(jobs);
        self.rows.push(Row {
            batch,
            label,
            progress: None,
        });
        cx.notify();
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The name clash waiting for an answer, if any.
    pub fn question(&self) -> Option<(&Question, &str)> {
        let q = self.questions.front()?;
        let label = self
            .rows
            .iter()
            .find(|r| r.batch == q.batch)
            .map_or("", |r| r.label.as_str());
        Some((q, label))
    }

    pub fn answer(&mut self, choice: tern_ssh::Conflict, all: bool, cx: &mut Context<Self>) {
        if let Some(q) = self.questions.pop_front() {
            self.queue.answer(q.id, choice, all);
            cx.notify();
        }
    }

    pub fn cancel(&mut self, batch: u64) {
        self.queue.cancel(batch);
    }

    pub fn cancel_all(&mut self) {
        self.queue.cancel_all();
    }

    /// Copies are running or waiting: quitting now would cut them off.
    pub fn busy(&self) -> bool {
        self.queue.busy()
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Progress(p) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.batch == p.batch) {
                    row.progress = Some(p);
                }
            }
            Event::Question(q) => self.questions.push_back(q),
            Event::Finished(summary) => {
                self.questions.retain(|q| q.batch != summary.batch);
                if let Some(at) = self.rows.iter().position(|r| r.batch == summary.batch) {
                    let row = self.rows.remove(at);
                    cx.emit(Finished {
                        label: row.label,
                        summary,
                    });
                }
            }
        }
        cx.notify();
    }
}

/// `1.5 MB/s`.
pub fn speed(bytes_per_sec: u64) -> String {
    format!("{}/s", crate::files_text::human_size(bytes_per_sec))
}

/// `2 min left`, `45 s left`, `1 h 05 min left`.
pub fn time_left(left: Duration) -> String {
    let secs = left.as_secs();
    match secs {
        0..=59 => format!("{} s left", secs.max(1)),
        60..=3599 => format!("{} min left", secs.div_ceil(60)),
        _ => format!("{} h {:02} min left", secs / 3600, (secs % 3600) / 60),
    }
}

/// What a row says under its label: the file now copying, the share done, speed and time left.
pub fn detail(p: &Progress) -> String {
    let mut parts = vec![format!(
        "{} of {}",
        crate::files_text::human_size(p.done),
        crate::files_text::human_size(p.total)
    )];
    if p.speed > 0 {
        parts.push(speed(p.speed));
    }
    if let Some(left) = p.eta {
        parts.push(time_left(left));
    }
    parts.join(" · ")
}

/// How the end of a batch reads in a toast: `Some(true)` when it went well.
pub fn outcome(label: &str, s: &Summary) -> (bool, String) {
    if s.cancelled {
        return (false, format!("Cancelled: {label}"));
    }
    if let Some(first) = s.failed.first() {
        let more = match s.failed.len() {
            1 => String::new(),
            n => format!(" (and {} more)", n - 1),
        };
        return (false, format!("Could not copy {label}: {first}{more}"));
    }
    let skipped = match s.skipped {
        0 => String::new(),
        n => format!(", {n} skipped"),
    };
    (true, format!("Copied {label}{skipped}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(copied: usize, skipped: usize, failed: &[&str], cancelled: bool) -> Summary {
        Summary {
            batch: 1,
            dest: "mac".into(),
            dir: "/".into(),
            copied,
            skipped,
            failed: failed.iter().map(|s| (*s).to_owned()).collect(),
            cancelled,
        }
    }

    #[test]
    fn time_left_reads_in_the_unit_a_person_waits_in() {
        assert_eq!(time_left(Duration::from_millis(300)), "1 s left");
        assert_eq!(time_left(Duration::from_secs(45)), "45 s left");
        assert_eq!(time_left(Duration::from_secs(61)), "2 min left");
        assert_eq!(time_left(Duration::from_secs(3900)), "1 h 05 min left");
    }

    #[test]
    fn a_row_names_size_speed_and_time_when_it_knows_them() {
        let mut p = Progress {
            batch: 1,
            current: "a".into(),
            done: 1024,
            total: 4096,
            files_done: 0,
            files_total: 1,
            speed: 0,
            eta: None,
        };
        assert_eq!(detail(&p), "1.0 KB of 4.0 KB");
        p.speed = 2048;
        p.eta = Some(Duration::from_secs(2));
        assert_eq!(detail(&p), "1.0 KB of 4.0 KB · 2.0 KB/s · 2 s left");
    }

    #[test]
    fn the_toast_says_what_happened_and_the_first_failure() {
        assert_eq!(
            outcome("3 items to web", &summary(3, 0, &[], false)),
            (true, "Copied 3 items to web".into())
        );
        assert_eq!(
            outcome("a", &summary(1, 2, &[], false)).1,
            "Copied a, 2 skipped"
        );
        let (ok, text) = outcome("a", &summary(0, 0, &["x: denied", "y: gone"], false));
        assert!(!ok);
        assert_eq!(text, "Could not copy a: x: denied (and 1 more)");
        assert_eq!(
            outcome("a", &summary(0, 0, &[], true)),
            (false, "Cancelled: a".into())
        );
    }
}
