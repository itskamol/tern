//! Command history: the lines a person ran on each host, kept on this Mac only. One small
//! interface (record, search, delete, clear) hides what makes it safe to keep: a word list that
//! leaves secrets out, merging repeats, a cap per host, and the files, which sit under
//! `history/` in the config folder, outside what sync carries. Files are read when a host is
//! first used, never at launch. A command line is never logged.
//!
//! The filter works from words, so it both misses secrets and drops harmless lines
//! (`ssh -p2222`); a leading space and [`CommandHistory::delete`] are the person's corrections.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Commands kept per host; the oldest are dropped first.
pub const CAP: usize = 5000;

/// The longest file name a host may have once encoded; longer ones are not recorded.
const NAME_MAX: usize = 200;

/// Words that keep a line out, whatever the case.
const SECRET_WORDS: [&str; 7] = [
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "private_key",
];

/// One command line, as it comes back from a search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub host: String,
    pub line: String,
    /// Unix seconds when it was run.
    pub at: u64,
}

/// Which hosts a search or a clear covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    Host(&'a str),
    All,
}

#[derive(Debug)]
pub struct CommandHistory {
    dir: PathBuf,
    /// Lines in each file this run has touched, so the cap is checked without reading it.
    counts: HashMap<PathBuf, usize>,
}

impl gpui::Global for CommandHistory {}

impl CommandHistory {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            counts: HashMap::new(),
        }
    }

    /// Stores `line` as run on `host` at `at`, unless it looks like a secret or starts with a
    /// space. True when it was stored.
    ///
    /// # Errors
    ///
    /// When the file cannot be written.
    pub fn record(&mut self, host: &str, line: &str, at: u64) -> io::Result<bool> {
        let Some(line) = keep(line) else {
            return Ok(false);
        };
        let path = self.file(host)?;
        let entry = Entry {
            host: host.to_owned(),
            line: line.to_owned(),
            at,
        };
        let mut text = serde_json::to_string(&entry).map_err(io::Error::other)?;
        text.push('\n');
        self.private_dir()?;
        let before = self.count(&path);
        open_private(&path, true)?.write_all(text.as_bytes())?;
        self.counts.insert(path.clone(), before + 1);
        if before + 1 > CAP {
            self.compact(&path)?;
        }
        Ok(true)
    }

    /// Entries for `scope`, newest first with repeats merged at their newest place; a query
    /// keeps the fuzzy matches, best first.
    pub fn search(&self, scope: Scope, query: &str) -> Vec<Entry> {
        let found = match scope {
            Scope::Host(host) => self
                .file(host)
                .map(|p| newest_first(&p, Some(host)))
                .unwrap_or_default(),
            Scope::All => {
                let mut all: Vec<Entry> = self
                    .files()
                    .iter()
                    .flat_map(|p| newest_first(p, None))
                    .collect();
                all.sort_by(|a, b| b.at.cmp(&a.at));
                all
            }
        };
        if query.trim().is_empty() {
            return found;
        }
        let lines: Vec<&str> = found.iter().map(|e| e.line.as_str()).collect();
        let labels: Vec<String> = found
            .iter()
            .map(|e| format!("{} {}", e.line, e.host))
            .collect();
        crate::picker::rank_named(query, &lines, &labels)
            .into_iter()
            .map(|ix| found[ix].clone())
            .collect()
    }

    /// Removes every copy of `line` from `host`'s history.
    ///
    /// # Errors
    ///
    /// When the file cannot be rewritten.
    pub fn delete(&mut self, host: &str, line: &str) -> io::Result<()> {
        let path = self.file(host)?;
        self.retain(&path, |e| !(e.host == host && e.line == line))
    }

    /// Empties one host's history, or every host's.
    ///
    /// # Errors
    ///
    /// When a file cannot be removed or rewritten.
    pub fn clear(&mut self, scope: Scope) -> io::Result<()> {
        match scope {
            Scope::Host(host) => {
                let path = self.file(host)?;
                self.retain(&path, |e| e.host != host)
            }
            Scope::All => {
                for path in self.files() {
                    fs::remove_file(&path)?;
                }
                self.counts.clear();
                Ok(())
            }
        }
    }

    /// The hosts that have history, sorted.
    pub fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .files()
            .iter()
            .filter_map(|p| p.file_stem()?.to_str().and_then(decode))
            .collect();
        hosts.sort();
        hosts
    }

    fn file(&self, host: &str) -> io::Result<PathBuf> {
        let name = encode(host);
        if name.len() > NAME_MAX {
            return Err(io::Error::other("host name too long for a history file"));
        }
        Ok(self.dir.join(format!("{name}.jsonl")))
    }

    /// The history files, by name.
    fn files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        files.sort();
        files
    }

    fn count(&mut self, path: &Path) -> usize {
        *self
            .counts
            .entry(path.to_owned())
            .or_insert_with(|| read(path).len())
    }

    fn private_dir(&self) -> io::Result<()> {
        let mut dir = fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
        dir.create(&self.dir)
    }

    /// Keeps the entries `keep` accepts; a file left empty is removed.
    fn retain(&mut self, path: &Path, keep: impl Fn(&Entry) -> bool) -> io::Result<()> {
        let entries = read(path);
        let kept: Vec<Entry> = entries.iter().filter(|e| keep(e)).cloned().collect();
        if kept.len() == entries.len() {
            return Ok(());
        }
        self.counts.insert(path.to_owned(), kept.len());
        if kept.is_empty() {
            return fs::remove_file(path);
        }
        rewrite(path, &kept)
    }

    /// Over the cap: merges repeats and drops the oldest.
    fn compact(&mut self, path: &Path) -> io::Result<()> {
        let mut kept = newest_first(path, None);
        kept.reverse();
        self.counts.insert(path.to_owned(), kept.len());
        rewrite(path, &kept)
    }
}

/// `line` as it is stored, or `None` when it stays out of the history.
fn keep(line: &str) -> Option<&str> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let line = line.trim_end();
    if line.is_empty() {
        return None;
    }
    let lower = line.to_lowercase();
    if SECRET_WORDS.iter().any(|w| lower.contains(w)) {
        return None;
    }
    // `mysql -psecret`: a password glued to its flag.
    if line
        .split_whitespace()
        .any(|t| t.starts_with("-p") && t.len() > 2)
    {
        return None;
    }
    Some(line)
}

/// A host's file name: lowercase letters, digits, `-` and `_` as they are, every other byte
/// as `%xx`. Injective, so no two hosts share a file, and safe on a case-insensitive disk.
fn encode(host: &str) -> String {
    if host.is_empty() {
        return "%".into();
    }
    host.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => char::from(b).to_string(),
            _ => format!("%{b:02x}"),
        })
        .collect()
}

fn decode(name: &str) -> Option<String> {
    if name == "%" {
        return Some(String::new());
    }
    let mut bytes = Vec::new();
    let mut raw = name.bytes();
    while let Some(b) = raw.next() {
        if b == b'%' {
            let hex = [raw.next()?, raw.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

/// Every entry in a file, oldest first; what cannot be read is skipped.
fn read(path: &Path) -> Vec<Entry> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// A file's entries newest first, each line once at its newest place, at most [`CAP`]; only
/// `host`'s when given.
fn newest_first(path: &Path, host: Option<&str>) -> Vec<Entry> {
    let mut seen = HashSet::new();
    let mut out: Vec<Entry> = read(path)
        .into_iter()
        .rev()
        .filter(|e| host.is_none_or(|h| e.host == h))
        .filter(|e| seen.insert((e.host.clone(), e.line.clone())))
        .collect();
    out.truncate(CAP);
    out
}

fn open_private(path: &Path, append: bool) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Replaces a file with `entries` (oldest first) through a private temporary file.
fn rewrite(path: &Path, entries: &[Entry]) -> io::Result<()> {
    let temp = path.with_extension("tmp");
    let mut text = String::new();
    for e in entries {
        text.push_str(&serde_json::to_string(e).map_err(io::Error::other)?);
        text.push('\n');
    }
    open_private(&temp, false)?.write_all(text.as_bytes())?;
    fs::rename(&temp, path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn history() -> (CommandHistory, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (CommandHistory::new(dir.path().join("history")), dir)
    }

    fn lines(h: &CommandHistory, scope: Scope, query: &str) -> Vec<String> {
        h.search(scope, query).into_iter().map(|e| e.line).collect()
    }

    fn host_lines(h: &CommandHistory, host: &str) -> Vec<String> {
        lines(h, Scope::Host(host), "")
    }

    #[test]
    fn a_recorded_line_is_found_newest_first() {
        let (mut h, _d) = history();
        assert!(h.record("web", "ls -la", 1).unwrap());
        assert!(h.record("web", "uptime", 2).unwrap());
        let found = h.search(Scope::Host("web"), "");
        assert_eq!(found[0].line, "uptime");
        assert_eq!((found[0].host.as_str(), found[0].at), ("web", 2));
        assert_eq!(host_lines(&h, "web"), ["uptime", "ls -la"]);
    }

    #[test]
    fn lines_that_look_like_secrets_are_left_out_whatever_the_case() {
        let (mut h, _d) = history();
        for line in [
            "mysql --password=hunter2",
            "echo $DB_PASSWD",
            "curl -H 'X-Secret: a' x",
            "gh auth login --with-token",
            "export AWS_SECRET_ACCESS_KEY=abc",
            "export Api_Key=1",
            "APIKEY=1 ./run",
            "cat private_key.pem",
            "mysql -psecretvalue",
            "mysql -pabc",
            "TOKEN=abc deploy",
            "export MY_PASSWORD=x",
        ] {
            assert!(!h.record("web", line, 1).unwrap(), "{line} was kept");
        }
        assert!(host_lines(&h, "web").is_empty());
    }

    #[test]
    fn ordinary_lines_that_come_close_are_kept() {
        let (mut h, _d) = history();
        for line in [
            "ssh -p 2222 host",
            "ls -l",
            "export PATH=/usr/bin",
            "git push --set-upstream origin main",
            "echo pass",
        ] {
            assert!(h.record("web", line, 1).unwrap(), "{line} was dropped");
        }
    }

    #[test]
    fn a_line_that_starts_with_a_space_is_left_out_and_blank_lines_too() {
        let (mut h, _d) = history();
        assert!(!h.record("web", " ls", 1).unwrap());
        assert!(!h.record("web", "", 1).unwrap());
        assert!(!h.record("web", "   ", 1).unwrap());
        assert!(h.record("web", "ls  ", 2).unwrap());
        assert_eq!(host_lines(&h, "web"), ["ls"]);
    }

    #[test]
    fn a_repeated_line_shows_once_at_its_newest_place() {
        let (mut h, _d) = history();
        h.record("web", "a", 1).unwrap();
        h.record("web", "b", 2).unwrap();
        h.record("web", "a", 3).unwrap();
        let found = h.search(Scope::Host("web"), "");
        assert_eq!(
            found.iter().map(|e| e.line.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(found[0].at, 3);
    }

    #[test]
    fn only_the_newest_five_thousand_are_kept() {
        let (mut h, _d) = history();
        for n in 0..CAP + 7 {
            h.record("web", &format!("cmd {n}"), n as u64).unwrap();
        }
        let kept = host_lines(&h, "web");
        assert_eq!(kept.len(), CAP);
        assert_eq!(kept[0], format!("cmd {}", CAP + 6));
        assert_eq!(kept[CAP - 1], "cmd 7");
    }

    #[test]
    fn a_fuzzy_query_finds_scattered_letters_and_ranks_the_closer_match_first() {
        let (mut h, _d) = history();
        h.record("web", "dxxxxxxxxxkxxxxxxxxxcxxxxxxxxxmxxxxxxxxxp up", 1)
            .unwrap();
        h.record("web", "docker compose up -d", 2).unwrap();
        h.record("web", "ls", 3).unwrap();
        let found = lines(&h, Scope::Host("web"), "dkcmp up");
        assert_eq!(found[0], "docker compose up -d");
        assert!(!found.contains(&"ls".to_owned()));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn each_host_keeps_its_own_lines_and_all_hosts_mixes_them_labelled() {
        let (mut h, _d) = history();
        h.record("web", "uptime", 1).unwrap();
        h.record("db", "psql", 2).unwrap();
        h.record("db", "uptime", 3).unwrap();
        assert_eq!(host_lines(&h, "web"), ["uptime"]);
        assert_eq!(host_lines(&h, "db"), ["uptime", "psql"]);
        let all: Vec<(String, String)> = h
            .search(Scope::All, "")
            .into_iter()
            .map(|e| (e.host, e.line))
            .collect();
        assert_eq!(
            all,
            [
                ("db".into(), "uptime".into()),
                ("db".into(), "psql".into()),
                ("web".into(), "uptime".into())
            ]
        );
        assert_eq!(h.hosts(), ["db", "web"]);
    }

    #[test]
    fn hosts_named_with_path_characters_stay_apart() {
        let (mut h, _d) = history();
        for (host, n) in [
            ("me@a:22", 1),
            ("me@a/22", 2),
            ("..", 3),
            ("Web", 4),
            ("web", 5),
        ] {
            h.record(host, &format!("cmd {n}"), n).unwrap();
        }
        assert_eq!(host_lines(&h, "me@a:22"), ["cmd 1"]);
        assert_eq!(host_lines(&h, "me@a/22"), ["cmd 2"]);
        assert_eq!(host_lines(&h, ".."), ["cmd 3"]);
        assert_eq!(host_lines(&h, "Web"), ["cmd 4"]);
        assert_eq!(host_lines(&h, "web"), ["cmd 5"]);
    }

    #[test]
    fn delete_removes_every_copy_of_a_line_on_that_host_only() {
        let (mut h, _d) = history();
        h.record("web", "a", 1).unwrap();
        h.record("web", "b", 2).unwrap();
        h.record("web", "a", 3).unwrap();
        h.record("db", "a", 4).unwrap();
        h.delete("web", "a").unwrap();
        assert_eq!(host_lines(&h, "web"), ["b"]);
        assert_eq!(host_lines(&h, "db"), ["a"]);
    }

    #[test]
    fn clear_empties_one_host_or_all() {
        let (mut h, _d) = history();
        h.record("web", "a", 1).unwrap();
        h.record("db", "b", 2).unwrap();
        h.clear(Scope::Host("web")).unwrap();
        assert!(host_lines(&h, "web").is_empty());
        assert_eq!(host_lines(&h, "db"), ["b"]);
        h.clear(Scope::All).unwrap();
        assert!(h.search(Scope::All, "").is_empty());
        assert!(h.hosts().is_empty());
        h.record("web", "again", 3).unwrap();
        assert_eq!(host_lines(&h, "web"), ["again"]);
    }

    #[test]
    fn entries_survive_a_reopen_and_a_tab_in_a_line_comes_back_whole() {
        let (mut h, d) = history();
        h.record("web", "printf 'a\tb'", 1).unwrap();
        let again = CommandHistory::new(d.path().join("history"));
        assert_eq!(host_lines(&again, "web"), ["printf 'a\tb'"]);
    }

    #[cfg(unix)]
    #[test]
    fn files_are_owner_only_when_created_and_after_a_compaction() {
        use std::os::unix::fs::PermissionsExt;
        let (mut h, d) = history();
        h.record("web", "ls", 1).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let root = d.path().join("history");
        assert_eq!(mode(&root), 0o700);
        let file = std::fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(mode(&file), 0o600);
        for n in 0..CAP + 1 {
            h.record("web", &format!("c{n}"), n as u64).unwrap();
        }
        assert_eq!(mode(&file), 0o600);
        h.delete("web", "c5").unwrap();
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn the_history_lives_in_its_own_folder_apart_from_the_files_sync_carries() {
        let (mut h, d) = history();
        h.record("web", "ls", 1).unwrap();
        assert!(d.path().join("history").is_dir());
        let synced = [
            "hosts.json",
            "settings.json",
            "keymap.json",
            "snippets.json",
        ];
        assert!(synced.iter().all(|n| !d.path().join(n).exists()));
    }
}
