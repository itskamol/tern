// Adapted from zeron crates/ui/src/terminal/view.rs (MIT) and zed
// crates/terminal_view/src/terminal_element.rs (GPL-3.0-or-later); box-drawing
// pass follows gpui-terminal src/render.rs (MIT OR Apache-2.0).
//
//! The grid-painting element. Measures cell metrics from the real mono font,
//! reports the resulting grid to the owning [`TerminalView`], and paints:
//! background quads for non-default cells, the selection wash, one shaped
//! segment per run of same-width glyphs, programmatic box-drawing, and the
//! cursor.

use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
    App, Bounds, Entity, GlobalElementId, Hsla, LayoutId, PaintQuad, Pixels, ShapedLine,
    SharedString, Style, TextRun, Window, fill, font, outline, point, px, relative, size,
};

use crate::box_drawing;
use crate::config::TerminalTheme;
use crate::search::SearchMark;
use crate::terminal::{CellColor, CellSnapshot, CursorSnapshot};
use crate::view::{GridGeometry, TERM_PADDING, TerminalView};

pub struct TerminalElement {
    view: Entity<TerminalView>,
    focused: bool,
}

impl TerminalElement {
    pub fn new(view: Entity<TerminalView>, focused: bool) -> Self {
        Self { view, focused }
    }
}

struct BoxCell {
    col: usize,
    ch: char,
    color: Hsla,
}

pub struct TerminalPrepaint {
    bg_quads: Vec<PaintQuad>,
    /// Painted after `bg_quads`, before glyphs: it tints a cell's own
    /// background and must not wash out the text it highlights.
    sel_quads: Vec<PaintQuad>,
    /// Per row, the shaped segments and the grid column each starts at.
    lines: Vec<Vec<(usize, ShapedLine)>>,
    box_rows: Vec<Vec<BoxCell>>,
    cell_w: Pixels,
    line_h: Pixels,
    origin: gpui::Point<Pixels>,
    cursor_under: Option<PaintQuad>,
    cursor_over: Option<PaintQuad>,
    /// Underline under the hovered link, painted over the glyphs.
    link_quads: Vec<PaintQuad>,
}

impl gpui::IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalPrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let _timed = crate::frame_stats::start(crate::frame_stats::Span::TermPrepaint);
        let theme = self.view.read(cx).terminal.read(cx).theme().clone();
        let mono = mono_font(&theme);
        let font_size = px(theme.font_size);
        // Font probe: measure the resolved font's real advance so cols/rows
        // track glyph metrics, not a guessed aspect ratio.
        let font_id = window.text_system().resolve_font(&mono);
        let cell_w = window
            .text_system()
            .em_advance(font_id, font_size)
            .unwrap_or(px(theme.font_size * 0.6));
        let line_h = px((theme.font_size * theme.line_height_ratio).round());

        let inner_w = f32::from(bounds.size.width) - 2.0 * TERM_PADDING;
        let inner_h = f32::from(bounds.size.height) - 2.0 * TERM_PADDING;
        let cols = ((inner_w / f32::from(cell_w)).floor() as i64).clamp(2, 1000) as u16;
        let rows = ((inner_h / f32::from(line_h)).floor() as i64).clamp(1, 1000) as u16;
        let origin = point(
            bounds.left() + px(TERM_PADDING),
            bounds.top() + px(TERM_PADDING),
        );

        // Report the measured grid before snapshotting so this frame paints
        // the resized grid. The view is not borrowed during prepaint.
        let terminal = self.view.update(cx, |view, cx| {
            view.on_grid_metrics(
                GridGeometry {
                    origin,
                    cell_w: f32::from(cell_w),
                    line_h: f32::from(line_h),
                    cols,
                    rows,
                },
                cx,
            );
            view.terminal.clone()
        });
        let link_segments = self
            .view
            .read(cx)
            .hover_link()
            .map(|link| link.segments.clone())
            .unwrap_or_default();
        // A focused blinking cursor is dark in its off phase; an unfocused
        // one is a steady hollow box.
        let cursor_on = !self.focused || self.view.read(cx).cursor_on();
        let (grid, cursor) = {
            let t = terminal.read(cx);
            (t.lines(), t.cursor().filter(|_| cursor_on))
        };

        let mut bg_quads = Vec::new();
        let mut sel_quads = Vec::new();
        let mut lines = Vec::with_capacity(grid.len());
        let mut box_rows = Vec::with_capacity(grid.len());

        let block_cursor_col =
            cursor.filter(|c| self.focused && matches!(c.shape, CursorShape::Block));

        for (row_ix, row) in grid.iter().enumerate() {
            let y = origin.y + line_h * row_ix as f32;
            let quad_at = |start: usize, end: usize, color: Hsla| {
                fill(
                    Bounds::new(
                        point(origin.x + cell_w * start as f32, y),
                        size(cell_w * (end - start) as f32, line_h),
                    ),
                    color,
                )
            };
            // Selected and found runs, one quad per contiguous span.
            for (start, end) in spans(row, |cell| cell.selected) {
                sel_quads.push(quad_at(start, end, theme.selection));
            }
            for (start, end) in spans(row, |cell| cell.search == SearchMark::Match) {
                sel_quads.push(quad_at(start, end, theme.search_match));
            }
            for (start, end) in spans(row, |cell| cell.search == SearchMark::Current) {
                sel_quads.push(quad_at(start, end, theme.search_current));
            }
            // Merge consecutive non-default background cells into quads.
            let mut run_start: Option<(usize, Hsla)> = None;
            for (col, color) in row
                .iter()
                .map(|cell| cell.display_colors().1)
                .chain(std::iter::once(CellColor::Background))
                .enumerate()
            {
                let paint = match color {
                    CellColor::Background => None,
                    other => Some(theme.resolve(other)),
                };
                match (&run_start, paint) {
                    (None, Some(color)) => run_start = Some((col, color)),
                    (Some((start, current)), next) if next != Some(*current) => {
                        bg_quads.push(quad_at(*start, col, *current));
                        run_start = next.map(|color| (col, color));
                    }
                    _ => {}
                }
            }
            let cursor_col = block_cursor_col.filter(|c| c.row == row_ix).map(|c| c.col);
            let (segments, boxes) = shape_row(row, &theme, &mono, font_size, cursor_col, window);
            lines.push(segments);
            box_rows.push(boxes);
        }

        let (cursor_under, cursor_over) = match cursor {
            Some(c) => cursor_quads(c, &grid, &theme, self.focused, origin, cell_w, line_h),
            None => (None, None),
        };

        let link_quads = link_segments
            .iter()
            .map(|&(row, start, end)| {
                fill(
                    Bounds::new(
                        point(
                            origin.x + cell_w * start as f32,
                            origin.y + line_h * row as f32 + line_h - px(2.0),
                        ),
                        size(cell_w * (end - start) as f32, px(1.0)),
                    ),
                    theme.foreground,
                )
            })
            .collect();

        TerminalPrepaint {
            bg_quads,
            sel_quads,
            lines,
            box_rows,
            cell_w,
            line_h,
            origin,
            cursor_under,
            cursor_over,
            link_quads,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (cell_w, line_h, origin) = (prepaint.cell_w, prepaint.line_h, prepaint.origin);
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in prepaint.bg_quads.drain(..) {
                window.paint_quad(quad);
            }
            for quad in prepaint.sel_quads.drain(..) {
                window.paint_quad(quad);
            }
            if let Some(quad) = prepaint.cursor_under.take() {
                window.paint_quad(quad);
            }
            for (ix, segments) in prepaint.lines.iter().enumerate() {
                let y = origin.y + line_h * ix as f32;
                for (col, line) in segments {
                    let _ = line.paint(
                        point(origin.x + cell_w * *col as f32, y),
                        line_h,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }
            for (ix, boxes) in prepaint.box_rows.iter().enumerate() {
                paint_box_row(boxes, origin, ix, cell_w, line_h, window);
            }
            for quad in prepaint.link_quads.drain(..) {
                window.paint_quad(quad);
            }
            if let Some(quad) = prepaint.cursor_over.take() {
                window.paint_quad(quad);
            }
        });
    }
}

/// Contiguous `[start, end)` column runs of cells satisfying `pred`.
fn spans(row: &[CellSnapshot], pred: impl Fn(&CellSnapshot) -> bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for col in 0..=row.len() {
        match (start, row.get(col).is_some_and(&pred)) {
            (None, true) => start = Some(col),
            (Some(s), false) => {
                out.push((s, col));
                start = None;
            }
            _ => {}
        }
    }
    out
}

fn mono_font(theme: &TerminalTheme) -> gpui::Font {
    // Ligatures OFF: a terminal is a fixed grid, and a contextual substitution
    // collapses several cells into fewer glyphs so the row renders short while
    // the cursor stays on the true column.
    let mut mono = font(theme.font_family.clone());
    mono.features = gpui::FontFeatures(std::sync::Arc::new(vec![
        ("liga".into(), 0),
        ("calt".into(), 0),
        ("dlig".into(), 0),
    ]));
    mono
}

/// The cursor quad, split by z-order: a focused block goes under the glyph
/// (which is recolored to the background), everything else over it.
fn cursor_quads(
    c: CursorSnapshot,
    grid: &[Vec<CellSnapshot>],
    theme: &TerminalTheme,
    focused: bool,
    origin: gpui::Point<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
) -> (Option<PaintQuad>, Option<PaintQuad>) {
    let wide = grid
        .get(c.row)
        .and_then(|r| r.get(c.col))
        .is_some_and(|cell| cell.wide);
    let w = if wide { cell_w * 2.0 } else { cell_w };
    let top_left = point(
        origin.x + cell_w * c.col as f32,
        origin.y + line_h * c.row as f32,
    );
    let full = Bounds::new(top_left, size(w, line_h));
    match c.shape {
        CursorShape::Hidden => (None, None),
        CursorShape::Block if focused => (Some(fill(full, theme.cursor)), None),
        CursorShape::Block | CursorShape::HollowBlock => (
            None,
            Some(outline(full, theme.cursor, gpui::BorderStyle::Solid)),
        ),
        CursorShape::Underline => (
            None,
            Some(fill(
                Bounds::new(
                    point(top_left.x, top_left.y + line_h - px(2.0)),
                    size(w, px(2.0)),
                ),
                theme.cursor,
            )),
        ),
        CursorShape::Beam => (
            None,
            Some(fill(
                Bounds::new(top_left, size(px(2.0), line_h)),
                theme.cursor,
            )),
        ),
    }
}

/// Paint one row of box-drawing cells: continuous horizontal spans first (no
/// gaps between cells), then the vertical components / whole glyphs.
fn paint_box_row(
    cells: &[BoxCell],
    origin: gpui::Point<Pixels>,
    row: usize,
    cell_w: Pixels,
    line_h: Pixels,
    window: &mut Window,
) {
    if cells.is_empty() {
        return;
    }
    let y_base = origin.y + line_h * row as f32;
    let cy = y_base + line_h / 2.0;
    let mut horizontal = std::collections::HashSet::new();
    let mut i = 0;
    while i < cells.len() {
        let BoxCell { col, ch, color } = &cells[i];
        let Some(weight) = box_drawing::get_horizontal_weight(*ch) else {
            i += 1;
            continue;
        };
        let (start, mut end, mut j) = (*col, *col, i + 1);
        while let Some(next) = cells.get(j) {
            if next.col == end + 1
                && next.color == *color
                && box_drawing::get_horizontal_weight(next.ch) == Some(weight)
            {
                end = next.col;
                j += 1;
            } else {
                break;
            }
        }
        box_drawing::draw_horizontal_span(
            origin.x + cell_w * start as f32,
            origin.x + cell_w * (end + 1) as f32,
            cy,
            weight,
            cell_w,
            *color,
            window,
        );
        horizontal.extend(start..=end);
        i = j;
    }
    for BoxCell { col, ch, color } in cells {
        let bounds = Bounds::new(
            point(origin.x + cell_w * *col as f32, y_base),
            size(cell_w, line_h),
        );
        if horizontal.contains(col) {
            box_drawing::draw_vertical_components(*ch, bounds, *color, cell_w, window);
        } else {
            box_drawing::draw_box_character(*ch, bounds, *color, cell_w, window);
        }
    }
}

/// How many cells of `row` can draw anything: the cells up to the last one that is not a plain
/// space (an underlined or struck-through space draws a line). A row is mostly blanks to its
/// right edge, and shaping them costs CoreText time for no ink; their backgrounds are quads of
/// their own.
fn ink_end(row: &[CellSnapshot]) -> usize {
    row.iter()
        .rposition(|c| !c.wide_spacer && ((c.ch != ' ' && !c.hidden) || c.underline || c.strikeout))
        .map_or(0, |last| last + 1)
}

/// Shape one grid row into COLUMN-PINNED segments.
///
/// A shaped line places glyphs by font advances, which only match the grid
/// while every glyph is the mono font's width. A glyph that resolves through
/// font fallback (arrows, emoji, CJK) has some other advance and would push
/// the rest of the line out of the grid. So runs of ASCII shape together and
/// every other glyph is its own segment pinned at its own column. Box-drawing
/// cells are not shaped at all; they are returned for programmatic painting. Blank cells past
/// the last inked one are not shaped.
fn shape_row(
    row: &[CellSnapshot],
    theme: &TerminalTheme,
    mono: &gpui::Font,
    font_size: Pixels,
    block_cursor_col: Option<usize>,
    window: &Window,
) -> (Vec<(usize, ShapedLine)>, Vec<BoxCell>) {
    fn flush(
        segments: &mut Vec<(usize, ShapedLine)>,
        text: &mut String,
        runs: &mut Vec<TextRun>,
        seg_col: usize,
        font_size: Pixels,
        window: &Window,
    ) {
        if text.is_empty() {
            return;
        }
        let shaped = window.text_system().shape_line(
            SharedString::from(std::mem::take(text)),
            font_size,
            runs,
            None,
        );
        segments.push((seg_col, shaped));
        runs.clear();
    }

    let mut segments: Vec<(usize, ShapedLine)> = Vec::new();
    let mut boxes = Vec::new();
    let mut text = String::with_capacity(row.len());
    let mut runs: Vec<TextRun> = Vec::new();
    let mut seg_col = 0usize;

    for (col, cell) in row.iter().enumerate().take(ink_end(row)) {
        if cell.wide_spacer {
            continue;
        }
        let ch = if cell.hidden { ' ' } else { cell.ch };
        let (fg, _) = cell.display_colors();
        let mut color = theme.resolve(fg);
        if cell.dim {
            color.a *= 0.6;
        }
        if block_cursor_col == Some(col) {
            color = theme.background;
        }
        if box_drawing::get_box_segments(ch).is_some() {
            flush(
                &mut segments,
                &mut text,
                &mut runs,
                seg_col,
                font_size,
                window,
            );
            boxes.push(BoxCell { col, ch, color });
            continue;
        }
        // Anything that can leave the mono font gets its own pinned segment.
        let pinned = !ch.is_ascii() || cell.wide;
        if pinned {
            flush(
                &mut segments,
                &mut text,
                &mut runs,
                seg_col,
                font_size,
                window,
            );
        }
        if text.is_empty() {
            seg_col = col;
        }
        let mut cell_font = mono.clone();
        cell_font.weight = if cell.bold {
            gpui::FontWeight::BOLD
        } else {
            gpui::FontWeight::NORMAL
        };
        cell_font.style = if cell.italic {
            gpui::FontStyle::Italic
        } else {
            gpui::FontStyle::Normal
        };
        let underline = cell.underline.then_some(gpui::UnderlineStyle {
            color: Some(color),
            thickness: px(1.0),
            wavy: false,
        });
        let strikethrough = cell.strikeout.then_some(gpui::StrikethroughStyle {
            color: Some(color),
            thickness: px(1.0),
        });
        let len = ch.len_utf8();
        text.push(ch);
        match runs.last_mut() {
            Some(last)
                if last.color == color
                    && last.font == cell_font
                    && last.underline == underline
                    && last.strikethrough == strikethrough =>
            {
                last.len += len;
            }
            _ => runs.push(TextRun {
                len,
                font: cell_font,
                color,
                background_color: None,
                underline,
                strikethrough,
            }),
        }
        if pinned {
            flush(
                &mut segments,
                &mut text,
                &mut runs,
                seg_col,
                font_size,
                window,
            );
        }
    }
    flush(
        &mut segments,
        &mut text,
        &mut runs,
        seg_col,
        font_size,
        window,
    );
    (segments, boxes)
}

#[cfg(test)]
#[path = "element_tests.rs"]
mod tests;
