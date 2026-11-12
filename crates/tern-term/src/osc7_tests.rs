//! Bytes in, events out: what a shell's OSC 7 turns into, and what the screen keeps.
#![allow(clippy::unwrap_used)]

use crate::terminal::{Notice, Terminal, TerminalEvent};

/// The folders a `process` call reported, in order, as `host:path`.
fn folders(notices: &[Notice]) -> Vec<String> {
    notices
        .iter()
        .filter_map(|n| match n {
            Notice::Event(TerminalEvent::ShellFolder(f)) => Some(format!("{}:{}", f.host, f.path)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_folder_ends_with_bel_or_st() {
    let mut t = Terminal::new(40, 4);
    assert_eq!(
        folders(&t.process(b"\x1b]7;file://web-01/var/log\x07")),
        ["web-01:/var/log"]
    );
    assert_eq!(
        folders(&t.process(b"\x1b]7;file://web-01/etc\x1b\\")),
        ["web-01:/etc"]
    );
}

#[test]
fn a_folder_split_across_two_reads_counts_once() {
    let mut t = Terminal::new(40, 4);
    assert!(folders(&t.process(b"$ \x1b]7;file://web-01/ho")).is_empty());
    assert_eq!(folders(&t.process(b"me/me\x07$ ")), ["web-01:/home/me"]);
    assert!(folders(&t.process(b"ls")).is_empty());
}

#[test]
fn a_percent_encoded_path_comes_back_as_text() {
    let mut t = Terminal::new(40, 4);
    assert_eq!(
        folders(&t.process(b"\x1b]7;file://web-01/srv/my%20site/caf%C3%A9\x07")),
        ["web-01:/srv/my site/caf\u{e9}"]
    );
}

#[test]
fn another_host_and_an_empty_host_are_kept_as_they_came() {
    let mut t = Terminal::new(40, 4);
    assert_eq!(
        folders(&t.process(b"\x1b]7;file://db-02/tmp\x07\x1b]7;file:///root\x07")),
        ["db-02:/tmp", ":/root"]
    );
}

#[test]
fn garbage_reports_nothing() {
    let mut t = Terminal::new(40, 4);
    for bad in [
        &b"\x1b]7;http://web-01/tmp\x07"[..],
        b"\x1b]7;file://web-01\x07",
        b"\x1b]7;file://web-01/a%2\x07",
        b"\x1b]7;file://web-01/a%zz\x07",
        b"\x1b]7;file://web-01/a%00b\x07",
        b"\x1b]7;file://web-01/a%0Ab\x07",
        b"\x1b]7;file://web-01/\xff\x07",
        b"\x1b]77;file://web-01/tmp\x07",
        b"\x1b]7file://web-01/tmp\x07",
    ] {
        assert!(folders(&t.process(bad)).is_empty(), "{bad:?}");
    }
    // An 8 KiB path, twice PATH_MAX, is skipped as it goes by, and the next one still counts.
    let mut long = b"\x1b]7;file://h/".to_vec();
    long.extend(std::iter::repeat_n(b'a', 8192));
    long.extend(b"\x07\x1b]7;file://h/ok\x07");
    assert_eq!(folders(&t.process(&long)), ["h:/ok"]);
}

#[test]
fn a_path_as_long_as_path_max_still_counts() {
    let mut t = Terminal::new(40, 4);
    let deep = format!("/{}", "d/".repeat(2000));
    let mut bytes = format!("\x1b]7;file://web-01{deep}").into_bytes();
    bytes.push(0x07);
    assert_eq!(folders(&t.process(&bytes)), [format!("web-01:{deep}")]);
}

#[test]
fn the_screen_is_the_same_with_or_without_the_folder() {
    let mut with = Terminal::new(40, 4);
    let mut without = Terminal::new(40, 4);
    with.process(b"one\x1b]7;file://h/x\x07 two\r\nthree\x1b]7;file://h/y\x1b\\");
    without.process(b"one two\r\nthree");
    for row in 0..4 {
        assert_eq!(with.row_text(row), without.row_text(row));
    }
    assert_eq!(with.cursor_pos(), without.cursor_pos());
}

#[test]
fn prompt_marks_and_folders_come_out_in_the_order_they_were_written() {
    let mut t = Terminal::new(40, 4);
    let n = t.process(b"\x1b]133;A\x07\x1b]7;file://h/srv\x07$ \x1b]133;B\x07");
    let kinds: Vec<&str> = n
        .iter()
        .filter_map(|n| match n {
            Notice::Event(TerminalEvent::PromptMark { .. }) => Some("mark"),
            Notice::Event(TerminalEvent::ShellFolder(_)) => Some("folder"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["mark", "folder", "mark"]);
}
