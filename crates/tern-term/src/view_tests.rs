#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn paste_wraps_when_bracketed_and_strips_injection() {
    assert_eq!(paste_bytes("hi", false), b"hi".to_vec());
    assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
    assert_eq!(
        paste_bytes("a\x1b[201~rm -rf", true),
        b"\x1b[200~arm -rf\x1b[201~".to_vec()
    );
}

fn hit(x: f32, y: f32) -> CellHit {
    cell_at(x, y, 10.0, 20.0, 8, 4)
}

#[test]
fn pointer_maps_to_cell_and_side() {
    assert_eq!(
        hit(25.0, 45.0),
        CellHit {
            row: 2,
            col: 2,
            side: Side::Left
        }
    );
    assert_eq!(hit(26.0, 45.0).side, Side::Right);
    assert_eq!(
        hit(70.0, 60.0),
        CellHit {
            row: 3,
            col: 7,
            side: Side::Left
        }
    );
}

#[test]
fn overshoot_clamps_and_forces_side() {
    assert_eq!(hit(9_999.0, 0.0).col, 7);
    assert_eq!(
        hit(1.0, 9_999.0),
        CellHit {
            row: 3,
            col: 0,
            side: Side::Right
        }
    );
    assert_eq!(
        hit(75.0, -50.0),
        CellHit {
            row: 0,
            col: 7,
            side: Side::Left
        }
    );
    assert_eq!(cell_at(f32::NAN, f32::INFINITY, 10.0, 20.0, 8, 4).col, 0);
    assert_eq!(cell_at(5.0, 5.0, 0.0, 20.0, 8, 4).row, 0);
}

#[test]
fn scrollback_keys_need_their_modifier() {
    let key = |s: &str| scrollback_key(&gpui::Keystroke::parse(s).unwrap());
    assert_eq!(key("shift-pageup"), Some(ScrollbackKey::PageUp));
    assert_eq!(key("shift-pagedown"), Some(ScrollbackKey::PageDown));
    // Plain paging is the remote's (less, vim).
    assert_eq!(key("pageup"), None);
    assert_eq!(key("home"), None);
    let (top, bottom) = if cfg!(target_os = "macos") {
        ("cmd-home", "cmd-end")
    } else {
        ("ctrl-shift-home", "ctrl-shift-end")
    };
    assert_eq!(key(top), Some(ScrollbackKey::Top));
    assert_eq!(key(bottom), Some(ScrollbackKey::Bottom));
}

#[test]
fn a_nested_end_marker_cannot_break_out_of_a_bracketed_paste() {
    let out = paste_bytes("a\x1b[20\x1b[201~1~rm -rf ~\n", true);
    let inner = &out[6..out.len() - 6];
    assert!(!inner.windows(6).any(|w| w == b"\x1b[201~"), "{inner:?}");
    assert!(out.ends_with(b"\x1b[201~"));
}

#[test]
fn display_height_is_rows_at_the_line_height_plus_padding() {
    let theme = TerminalTheme {
        font_size: 13.0,
        line_height_ratio: 1.5,
        ..TerminalTheme::default()
    };
    // 13 * 1.5 = 19.5 rounds to 20, as the element lays rows out.
    assert_eq!(
        TerminalView::height_for_rows(&theme, 5),
        5.0 * 20.0 + 2.0 * TERM_PADDING
    );
}
