#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::terminal::Terminal;

/// The first row of a terminal after `bytes`.
fn row(bytes: &[u8]) -> Vec<CellSnapshot> {
    let mut t = Terminal::new(20, 2);
    t.process(bytes);
    t.line(0)
}

#[test]
fn a_blank_row_has_nothing_to_shape() {
    assert_eq!(ink_end(&row(b"")), 0);
    assert_eq!(ink_end(&row(b"   ")), 0);
}

#[test]
fn a_row_is_shaped_up_to_its_last_glyph() {
    assert_eq!(ink_end(&row(b"ab")), 2);
    assert_eq!(ink_end(&row(b"ab   cd")), 7);
    assert_eq!(ink_end(&row(b"ab   ")), 2);
}

#[test]
fn spaces_that_draw_a_line_stay() {
    assert_eq!(ink_end(&row(b"ab\x1b[4m   \x1b[0m")), 5);
    assert_eq!(ink_end(&row(b"ab\x1b[9m  \x1b[0m  ")), 4);
}

#[test]
fn a_space_with_a_background_alone_is_not_shaped() {
    // The background is its own quad; the glyph would be a space.
    assert_eq!(ink_end(&row(b"ab\x1b[41m   \x1b[0m")), 2);
}

#[test]
fn a_wide_glyph_at_the_end_stays_and_its_spacer_does_not_extend_it() {
    let r = row("ab\u{4e2d}".as_bytes());
    assert_eq!(ink_end(&r), 3);
}

#[test]
fn non_breaking_and_other_blank_looking_glyphs_are_kept() {
    // Only the ASCII space is known to carry no ink in every font.
    assert_eq!(ink_end(&row("a\u{a0}".as_bytes())), 2);
}
