#![allow(clippy::unwrap_used)]

use super::*;
use crate::terminal::{Notice, Terminal, TerminalEvent};

fn term(cols: u16, rows: u16) -> Terminal {
    Terminal::new(cols, rows)
}

/// The marks a `process` call reported, in order, with where each was written.
fn marks(notices: &[Notice]) -> Vec<(PromptMark, i32, usize)> {
    notices
        .iter()
        .filter_map(|n| match n {
            Notice::Event(TerminalEvent::PromptMark { mark, at }) => Some((*mark, at.line, at.col)),
            _ => None,
        })
        .collect()
}

#[test]
fn marks_report_where_they_were_written_and_leave_the_screen_alone() {
    let mut t = term(40, 4);
    let n = t.process(b"\x1b]133;A\x07user@h:~$ \x1b]133;B\x07ls");
    assert_eq!(
        marks(&n),
        [
            (PromptMark::PromptStart, 0, 0),
            (PromptMark::CommandStart, 0, 10)
        ]
    );
    assert_eq!(t.row_text(0), "user@h:~$ ls");
    let n = t.process(b"\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
    assert_eq!(
        marks(&n),
        [
            (PromptMark::CommandOutput, 1, 0),
            (PromptMark::CommandEnd, 2, 0)
        ]
    );
    assert_eq!(t.row_text(1), "out");
}

#[test]
fn a_mark_split_across_reads_still_counts_once() {
    let mut t = term(40, 4);
    assert!(marks(&t.process(b"$ \x1b]13")).is_empty());
    assert!(marks(&t.process(b"3;")).is_empty());
    assert_eq!(
        marks(&t.process(b"B\x07x")),
        [(PromptMark::CommandStart, 0, 2)]
    );
    // And the string terminator, split between ESC and the backslash.
    assert!(marks(&t.process(b"\x1b]133;A\x1b")).is_empty());
    assert_eq!(marks(&t.process(b"\\")), [(PromptMark::PromptStart, 0, 3)]);
    assert_eq!(t.row_text(0), "$ x");
}

#[test]
fn other_sequences_make_no_mark_and_a_huge_one_is_skipped_unbuffered() {
    let mut t = term(40, 4);
    assert!(marks(&t.process(b"\x1b]7;file:///tmp\x07")).is_empty());
    assert!(marks(&t.process(b"\x1b]133;Z\x07")).is_empty());
    assert!(marks(&t.process(b"\x1b]1337;A\x07")).is_empty());
    assert!(marks(&t.process(b"\x1b[1m\x1b(B plain")).is_empty());
    let mut big = b"\x1b]52;c;".to_vec();
    big.extend(std::iter::repeat_n(b'A', 100_000));
    big.extend_from_slice(b"\x07\x1b]133;B\x07");
    // " plain" is on the screen, so the cursor is at column 6.
    assert_eq!(marks(&t.process(&big)), [(PromptMark::CommandStart, 0, 6)]);
}

#[test]
fn an_aborted_osc_does_not_swallow_the_next_sequence() {
    let mut t = term(40, 4);
    // ESC inside an OSC that is not a string terminator starts a new sequence.
    assert_eq!(
        marks(&t.process(b"\x1b]0;title\x1b]133;B\x07")),
        [(PromptMark::CommandStart, 0, 0)]
    );
}

#[test]
fn the_command_is_read_from_its_mark_with_wrapped_rows_joined() {
    let mut t = term(10, 4);
    t.process(b"$ \x1b]133;B\x07");
    let start = t.cursor_pos();
    assert_eq!(t.text_from(start).as_deref(), Some(""));
    t.process(b"echo hello world");
    assert_eq!(t.text_from(start).as_deref(), Some("echo hello world"));
    assert_eq!(t.row_text(1), "lo world");
}

#[test]
fn a_leading_space_after_the_mark_is_kept() {
    let mut t = term(40, 4);
    t.process(b"$ ");
    let start = t.cursor_pos();
    t.process(b" ls");
    assert_eq!(t.text_from(start).as_deref(), Some(" ls"));
}

#[test]
fn a_command_that_scrolls_the_screen_while_it_wraps_is_still_read_whole() {
    let mut t = term(10, 3);
    t.process(b"\r\n\r\n$ ");
    let start = t.cursor_pos();
    t.process(b"aaaaaaaabbbbbbbbbbcccc");
    assert_eq!(t.row_text(0), "$ aaaaaaaa");
    assert_eq!(
        t.text_from(start).as_deref(),
        Some("aaaaaaaabbbbbbbbbbcccc")
    );
}

#[test]
fn text_from_a_row_that_is_not_part_of_the_cursor_line_reads_nothing() {
    let mut t = term(10, 4);
    t.process(b"$ ");
    let start = t.cursor_pos();
    // The command ended and output follows: the start is a line above, not joined to it.
    t.process(b"\r\nout\r\n$ ");
    assert_eq!(t.text_from(start), None);
    // ⌘K moved the prompt to the top: the start is below the cursor now.
    let mut t = term(10, 4);
    t.process(b"\r\n\r\n$ ");
    let start = t.cursor_pos();
    t.clear();
    assert_eq!(t.text_from(start), None);
}
