//! The shell's copies: the one transfer queue and what the window shows of it. The queue lives
//! here, not in a Files tab or the Files panel, so closing either does not stop a copy; they
//! only read the rows. A name clash waits here, on its batch's row, until it is answered.

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
    /// Where from and where to: `web-01:/srv/app.log → Mac:~/Downloads`.
    pub route: String,
    /// The sources go once the copies landed.
    pub moving: bool,
    /// More than one name could clash, so an answer can be for the rest of the batch.
    pub many: bool,
    /// The next answer is for the rest of the batch too.
    pub rest: bool,
    pub progress: Option<Progress>,
}

/// A batch ended; the shell tells the person, and a Files tab or panel refreshes what it shows.
#[derive(Debug, Clone)]
pub struct Finished {
    pub label: String,
    pub moving: bool,
    pub summary: Summary,
}

impl Finished {
    /// The batch changed folder `dir` of the file source `id`: it landed there, or a moved
    /// source left it.
    pub fn touched(&self, id: &str, dir: &str) -> bool {
        let s = &self.summary;
        (s.dest == id && s.dir == dir) || s.removed_from.iter().any(|(i, d)| i == id && d == dir)
    }
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
        let route = route(&jobs, std::env::home_dir().as_deref());
        let moving = jobs.iter().any(|j| j.moving);
        let many = jobs.len() > 1 || jobs.iter().any(|j| j.item.is_dir);
        let batch = self.queue.submit(jobs);
        self.rows.push(Row {
            batch,
            label,
            route,
            moving,
            many,
            rest: false,
            progress: None,
        });
        cx.notify();
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The first name clash waiting for an answer, with its row.
    pub fn question(&self) -> Option<(&Question, &Row)> {
        let q = self.questions.front()?;
        Some((q, self.rows.iter().find(|r| r.batch == q.batch)?))
    }

    /// The clash batch `batch` waits on. A batch plans one name at a time, so it has at most
    /// one.
    pub fn question_for(&self, batch: u64) -> Option<&Question> {
        self.questions.iter().find(|q| q.batch == batch)
    }

    /// Answers batch `batch`'s clash; with the row's "for the rest" on, the rest of the batch
    /// gets the same answer.
    pub fn answer(&mut self, batch: u64, choice: tern_ssh::Conflict, cx: &mut Context<Self>) {
        let Some(at) = self.questions.iter().position(|q| q.batch == batch) else {
            return;
        };
        let rest = self
            .rows
            .iter()
            .find(|r| r.batch == batch)
            .is_some_and(|r| r.rest);
        if let Some(q) = self.questions.remove(at) {
            self.queue.answer(q.id, choice, rest);
        }
        cx.notify();
    }

    /// Turns the row's "for the rest" on or off.
    pub fn toggle_rest(&mut self, batch: u64, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.batch == batch) {
            row.rest = !row.rest;
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
                        moving: row.moving,
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

/// The Activity that follows running copies: the first batch's label and progress, and how
/// many more wait. `None` when nothing runs.
pub fn progress_line(rows: &[Row]) -> Option<String> {
    let first = rows.first()?;
    let mut line = match &first.progress {
        Some(p) => format!("Copying {}: {}", first.label, detail(p)),
        None => format!("Copying {}\u{2026}", first.label),
    };
    if rows.len() > 1 {
        line += &format!(", {} more waiting", rows.len() - 1);
    }
    Some(line)
}

/// One end of a route: `host:path`, the Mac as `Mac` with its home as `~`. A host's file
/// source is named `user@host:port`; the host alone is what the person knows it by.
pub fn place(id: &str, path: &str, home: Option<&std::path::Path>) -> String {
    if id == tern_ssh::FileSource::id(&tern_ssh::LocalFs::new()) {
        let home = home.and_then(|h| h.to_str()).filter(|h| !h.is_empty());
        let path = match home.and_then(|h| path.strip_prefix(h)) {
            Some("") => "~".to_owned(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_owned(),
        };
        return format!("Mac:{path}");
    }
    let host = id.rsplit_once('@').map_or(id, |(_, h)| h);
    let host = match host.rsplit_once(':') {
        Some((h, port)) if !h.is_empty() && port.parse::<u16>().is_ok() => h,
        _ => host,
    };
    format!("{host}:{path}")
}

/// Where a batch goes from and to. One item names itself; several name the folder they are in.
pub fn route(jobs: &[CopyJob], home: Option<&std::path::Path>) -> String {
    let Some(first) = jobs.first() else {
        return String::new();
    };
    let from = match jobs {
        [one] => one.item.path.clone(),
        _ => tern_ssh::parent(&first.item.path),
    };
    format!(
        "{} \u{2192} {}",
        place(first.from.id(), &from, home),
        place(first.to.id(), &first.dir, home)
    )
}

/// How the end of a batch reads: `true` when it went well.
pub fn outcome(label: &str, moving: bool, s: &Summary) -> (bool, String) {
    let (verb, done) = if moving {
        ("move", "Moved")
    } else {
        ("copy", "Copied")
    };
    if s.cancelled {
        return (false, format!("Cancelled: {label}"));
    }
    if let Some(first) = s.failed.first() {
        let more = match s.failed.len() {
            1 => String::new(),
            n => format!(" (and {} more)", n - 1),
        };
        return (false, format!("Could not {verb} {label}: {first}{more}"));
    }
    let links = match s.links {
        0 => String::new(),
        1 => ", 1 folder not copied because it is a link".to_owned(),
        n => format!(", {n} folders not copied because they are links"),
    };
    if s.copied == 0 && s.skipped > 0 {
        // Nothing landed, so "Copied" would be untrue; the label already names what was skipped.
        return (true, format!("Skipped {label}{links}"));
    }
    let skipped = match s.skipped {
        0 => String::new(),
        n => format!(", {n} skipped"),
    };
    (true, format!("{done} {label}{skipped}{links}"))
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
            links: 0,
            failed: failed.iter().map(|s| (*s).to_owned()).collect(),
            cancelled,
            removed_from: Vec::new(),
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

    fn row(label: &str, progress: Option<Progress>) -> Row {
        Row {
            batch: 1,
            label: label.into(),
            route: String::new(),
            moving: false,
            many: false,
            rest: false,
            progress,
        }
    }

    #[test]
    fn running_copies_read_as_one_line_with_the_first_batch_s_progress() {
        assert_eq!(progress_line(&[]), None);
        assert_eq!(
            progress_line(&[row("a.txt to web-01", None)]).as_deref(),
            Some("Copying a.txt to web-01\u{2026}")
        );
        let p = Progress {
            batch: 1,
            current: "a.txt".into(),
            done: 1024,
            total: 4096,
            files_done: 0,
            files_total: 1,
            speed: 0,
            eta: None,
        };
        let rows = [row("a.txt to web-01", Some(p)), row("logs to db-02", None)];
        assert_eq!(
            progress_line(&rows).as_deref(),
            Some("Copying a.txt to web-01: 1.0 KB of 4.0 KB, 1 more waiting")
        );
    }

    #[test]
    fn the_end_says_what_happened_and_the_first_failure() {
        assert_eq!(
            outcome("3 items to web", false, &summary(3, 0, &[], false)),
            (true, "Copied 3 items to web".into())
        );
        assert_eq!(
            outcome("a", false, &summary(1, 2, &[], false)).1,
            "Copied a, 2 skipped"
        );
        let mut linked = summary(1, 0, &[], false);
        linked.links = 2;
        assert_eq!(
            outcome("a", false, &linked).1,
            "Copied a, 2 folders not copied because they are links"
        );
        let (ok, text) = outcome("a", false, &summary(0, 0, &["x: denied", "y: gone"], false));
        assert!(!ok);
        assert_eq!(text, "Could not copy a: x: denied (and 1 more)");
        assert_eq!(
            outcome("a", false, &summary(0, 0, &[], true)),
            (false, "Cancelled: a".into())
        );
    }

    #[test]
    fn a_batch_where_everything_was_skipped_does_not_say_it_copied() {
        let (ok, text) = outcome("a.txt to web", false, &summary(0, 1, &[], false));
        assert!(ok, "skipping is an Activity, not a failure");
        assert_eq!(text, "Skipped a.txt to web");
        assert_eq!(
            outcome("a.txt to web", true, &summary(0, 3, &[], false)).1,
            "Skipped a.txt to web"
        );
    }

    #[test]
    fn a_move_says_moved_and_a_failed_one_says_it_could_not_move() {
        assert_eq!(
            outcome("a.log to web-02", true, &summary(1, 0, &[], false)).1,
            "Moved a.log to web-02"
        );
        assert_eq!(
            outcome("a", true, &summary(0, 0, &["x: denied"], false)).1,
            "Could not move a: x: denied"
        );
    }

    #[test]
    fn a_place_is_the_host_and_its_path_and_the_mac_s_home_is_a_tilde() {
        let home = std::path::Path::new("/Users/me");
        assert_eq!(
            place("deploy@web-01:22", "/srv/a.log", Some(home)),
            "web-01:/srv/a.log"
        );
        assert_eq!(place("[::1]:2299", "/tmp", None), "[::1]:/tmp");
        assert_eq!(
            place("mac", "/Users/me/Downloads", Some(home)),
            "Mac:~/Downloads"
        );
        assert_eq!(place("mac", "/Users/me", Some(home)), "Mac:~");
        assert_eq!(
            place("mac", "/Users/meg/x", Some(home)),
            "Mac:/Users/meg/x",
            "a home that is only a prefix of the name is not home"
        );
    }

    fn job(path: &str, to: &std::sync::Arc<dyn tern_ssh::FileSource>) -> CopyJob {
        let mac: std::sync::Arc<dyn tern_ssh::FileSource> =
            std::sync::Arc::new(tern_ssh::LocalFs::new());
        CopyJob {
            from: mac,
            item: tern_ssh::FileEntry {
                name: tern_ssh::file_name(path).to_owned(),
                path: path.into(),
                is_dir: false,
                is_symlink: false,
                size: 1,
                modified: None,
            },
            to: to.clone(),
            dir: "/Users/me/Downloads".into(),
            rule: None,
            moving: false,
        }
    }

    #[test]
    fn a_route_names_one_item_or_the_folder_several_are_in() {
        let home = std::path::Path::new("/Users/me");
        let mac: std::sync::Arc<dyn tern_ssh::FileSource> =
            std::sync::Arc::new(tern_ssh::LocalFs::new());
        let one = [job("/Users/me/a.txt", &mac)];
        assert_eq!(
            route(&one, Some(home)),
            "Mac:~/a.txt \u{2192} Mac:~/Downloads"
        );
        let two = [job("/Users/me/x/a", &mac), job("/Users/me/x/b", &mac)];
        assert_eq!(route(&two, Some(home)), "Mac:~/x \u{2192} Mac:~/Downloads");
        assert_eq!(route(&[], Some(home)), "");
    }

    #[test]
    fn a_finished_batch_touches_where_it_landed_and_where_a_move_left() {
        let mut s = summary(1, 0, &[], false);
        s.dest = "web".into();
        s.dir = "/srv".into();
        s.removed_from = vec![("db".into(), "/tmp".into())];
        let done = Finished {
            label: String::new(),
            moving: true,
            summary: s,
        };
        assert!(done.touched("web", "/srv"));
        assert!(done.touched("db", "/tmp"));
        assert!(!done.touched("db", "/srv"));
        assert!(!done.touched("web", "/tmp"));
    }
}
