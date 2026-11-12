// Adapted from zeron crates/ui/src/terminal/emulator.rs (MIT) and zed
// crates/terminal/src/terminal.rs (GPL-3.0-or-later).
//
//! The terminal model: `alacritty_terminal`'s `Term` + vte's ANSI `Processor`,
//! driven by bytes (no PTY, no event loop thread).
//!
//! Bytes in via [`Terminal::feed`]; grid snapshots out via [`Terminal::lines`] /
//! [`Terminal::cursor`]. Replies the emulator wants sent to the remote side
//! (DSR, DA, color/size queries) leave through the `write` callback given to
//! [`Terminal::new`]; everything leaving the terminal is a [`TerminalEvent`].

use std::cell::RefCell;
use std::rc::Rc;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionRange};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::Match;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb as AnsiRgb,
};
use gpui::{Context, EventEmitter};

use crate::config::CursorStyleSetting;
use crate::config::TerminalTheme;
use crate::links::Link;
use crate::search::{Search, SearchMark, mark_at};
use osc133::Scanner;

pub use alacritty_terminal::index::Side;
pub use alacritty_terminal::selection::SelectionType;
pub use osc133::{GridPos, PromptMark};

/// The most text one OSC 52 request may put on the clipboard. The program on the other end of
/// the connection chooses the payload, so a larger one is dropped rather than copied.
const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// Scrollback history kept client-side (lines).
pub const SCROLLBACK_LINES: usize = 5_000;

/// Emitted by [`Terminal`] (it is an `EventEmitter`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes for the remote side: keystrokes, paste, mouse reports and query replies.
    Output(Vec<u8>),
    /// What the user typed, pasted or clicked (mouse reports): the same bytes as the
    /// [`Self::Output`] that precedes it, without the terminal's own replies to the remote.
    /// A listener that mirrors input to other sessions wants this one.
    Typed(Vec<u8>),
    /// The grid changed size; tell the remote (debounced by the view).
    Resized {
        cols: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    /// OSC 0/2 title change. An empty string means "reset to default".
    TitleChanged(String),
    Bell,
    /// OSC 52: the remote asks to put text on the clipboard. The view writes it only while
    /// [`crate::TerminalOptions::clipboard_write`] allows; the app can say that it happened.
    ClipboardStore(String),
    /// OSC 133: the shell marked its prompt. `at` is where the cursor was when the mark was
    /// written, so [`Terminal::text_from`] can read the command typed after a `CommandStart`.
    PromptMark {
        mark: PromptMark,
        at: GridPos,
    },
}

/// Viewport dimensions in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GridSize {
    cols: u16,
    rows: u16,
}

impl GridSize {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(2),
            rows: rows.max(1),
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// A cell's paint color, decoupled from the palette; the element resolves
/// these against the [`TerminalTheme`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellColor {
    Foreground,
    Background,
    /// 0-15 ANSI, 16-231 color cube, 232-255 grayscale ramp.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

fn map_color(color: AnsiColor) -> CellColor {
    match color {
        AnsiColor::Spec(AnsiRgb { r, g, b }) => CellColor::Rgb(r, g, b),
        AnsiColor::Indexed(ix) => CellColor::Indexed(ix),
        AnsiColor::Named(named) => {
            let ix = named as usize;
            if ix < 16 {
                return CellColor::Indexed(ix as u8);
            }
            match named {
                NamedColor::Background => CellColor::Background,
                // Dim named colors fold onto their base index; DIM still
                // travels on the cell for paint-time dimming.
                NamedColor::DimBlack
                | NamedColor::DimRed
                | NamedColor::DimGreen
                | NamedColor::DimYellow
                | NamedColor::DimBlue
                | NamedColor::DimMagenta
                | NamedColor::DimCyan
                | NamedColor::DimWhite => {
                    CellColor::Indexed((ix - NamedColor::DimBlack as usize) as u8)
                }
                _ => CellColor::Foreground,
            }
        }
    }
}

/// One rendered cell: char + colors + the flags paint cares about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSnapshot {
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    pub inverse: bool,
    pub hidden: bool,
    /// Double-width char (occupies this cell plus the next spacer cell).
    pub wide: bool,
    /// Spacer half of a wide char: never shaped, only background-painted.
    pub wide_spacer: bool,
    pub selected: bool,
    /// Find-in-scrollback overlay.
    pub search: SearchMark,
}

impl CellSnapshot {
    /// Effective (fg, bg) after INVERSE/HIDDEN resolution.
    pub fn display_colors(&self) -> (CellColor, CellColor) {
        let (fg, bg) = if self.inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        if self.hidden { (bg, bg) } else { (fg, bg) }
    }
}

/// Cursor position in viewport coordinates (row 0 = top of the visible grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
}

/// Captures `Term` callbacks. `EventListener::send_event` takes `&self`;
/// single-threaded (the model lives inside a gpui entity).
#[derive(Default, Clone)]
struct EventCapture {
    events: Rc<RefCell<Vec<Event>>>,
}

impl EventListener for EventCapture {
    fn send_event(&self, event: Event) {
        self.events.borrow_mut().push(event);
    }
}

/// Side effects of a [`Terminal::process`] call that need a gpui context.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Notice {
    Event(TerminalEvent),
    ClipboardStore(String),
}

pub struct Terminal {
    term: Term<EventCapture>,
    parser: Processor,
    marks: Scanner,
    capture: EventCapture,
    theme: TerminalTheme,
    /// Cell size in pixels, for text-area size queries and the resize callback.
    cell_px: (f32, f32),
    search: Option<Search>,
    /// Kept to re-apply with changes: `Term` has no config getter.
    config: Config,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("cols", &self.cols())
            .field("rows", &self.rows())
            .field("history", &self.history_size())
            .finish_non_exhaustive()
    }
}

impl EventEmitter<TerminalEvent> for Terminal {}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, SCROLLBACK_LINES)
    }

    /// Like [`Self::new`] with a chosen history size (lines).
    pub fn with_scrollback(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        let capture = EventCapture::default();
        let config = Config {
            scrolling_history: scrollback_lines,
            ..Config::default()
        };
        let term = Term::new(config.clone(), &GridSize::new(cols, rows), capture.clone());
        Self {
            term,
            parser: Processor::new(),
            marks: Scanner::default(),
            capture,
            theme: TerminalTheme::default(),
            cell_px: (0.0, 0.0),
            search: None,
            config,
        }
    }

    /// Advance the parser over bytes from the remote side, write any replies
    /// back through `write`, and repaint.
    pub fn feed(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        for notice in self.process(bytes) {
            match notice {
                Notice::Event(event) => cx.emit(event),
                Notice::ClipboardStore(text) => cx.emit(TerminalEvent::ClipboardStore(text)),
            }
        }
        cx.notify();
    }

    /// The context-free core of [`Self::feed`]: runs the parser, sends replies
    /// through `write`, and returns what still needs a gpui context.
    pub(crate) fn process(&mut self, bytes: &[u8]) -> Vec<Notice> {
        let mut notices = Vec::new();
        let mut from = 0;
        for (end, mark) in self.marks.scan(bytes) {
            self.parser.advance(&mut self.term, &bytes[from..end]);
            from = end;
            let at = self.cursor_pos();
            notices.push(Notice::Event(TerminalEvent::PromptMark { mark, at }));
        }
        self.parser.advance(&mut self.term, &bytes[from..]);
        if let Some(search) = self.search.as_mut() {
            search.refresh(&self.term);
        }
        let mut reply = Vec::new();
        let events: Vec<Event> = self.capture.events.borrow_mut().drain(..).collect();
        for event in events {
            match event {
                Event::PtyWrite(text) => reply.extend_from_slice(text.as_bytes()),
                Event::Title(title) => {
                    notices.push(Notice::Event(TerminalEvent::TitleChanged(title)))
                }
                Event::ResetTitle => {
                    notices.push(Notice::Event(TerminalEvent::TitleChanged(String::new())))
                }
                Event::Bell => notices.push(Notice::Event(TerminalEvent::Bell)),
                Event::ClipboardStore(_, text) if text.len() <= MAX_CLIPBOARD_BYTES => {
                    notices.push(Notice::ClipboardStore(text));
                }
                // OSC 10/11/12 and OSC 4 queries: vim/neovim use these to pick
                // a light or dark scheme.
                Event::ColorRequest(index, format) => {
                    if let Some((r, g, b)) = self.theme.rgb_for_request(index) {
                        reply.extend_from_slice(format(AnsiRgb { r, g, b }).as_bytes());
                    }
                }
                Event::TextAreaSizeRequest(format) => {
                    let size = WindowSize {
                        num_lines: self.rows() as u16,
                        num_cols: self.cols() as u16,
                        cell_width: self.cell_px.0 as u16,
                        cell_height: self.cell_px.1 as u16,
                    };
                    reply.extend_from_slice(format(size).as_bytes());
                }
                // ClipboardLoad is deliberately ignored: letting the remote
                // read the local clipboard is a data leak.
                _ => {}
            }
        }
        if !reply.is_empty() {
            notices.insert(0, Notice::Event(TerminalEvent::Output(reply)));
        }
        notices
    }

    /// Set the palette used to answer color queries.
    /// The palette, owned here because OSC 4/10/11 colour queries are answered from it.
    pub fn theme(&self) -> &TerminalTheme {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: TerminalTheme) {
        self.theme = theme;
    }

    /// Apply `config` to the emulator. `Term::set_options` re-announces the
    /// title as an event; drop it so a settings change is not a title change.
    fn apply_config(&mut self) {
        self.term.set_options(self.config.clone());
        self.capture.events.borrow_mut().clear();
    }

    /// Change the history size at runtime. Shrinking drops the oldest lines
    /// at once; growing keeps what is there and makes room for more.
    pub fn set_scrollback(&mut self, lines: usize) {
        if self.config.scrolling_history != lines {
            self.config.scrolling_history = lines;
            self.apply_config();
        }
    }

    /// The cursor used until the remote sends DECSCUSR (and after `CSI 0 SP q`).
    pub fn set_default_cursor(&mut self, style: CursorStyleSetting, blink: bool) {
        self.config.default_cursor_style = style.style(blink);
        self.apply_config();
    }

    /// Whether the cursor in effect (the default or the remote's DECSCUSR
    /// choice) blinks.
    pub fn cursor_blinking(&self) -> bool {
        self.term.cursor_style().blinking
    }

    /// Send bytes to the remote side (keyboard, paste, mouse reports).
    pub fn write(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        cx.emit(TerminalEvent::Output(bytes.clone()));
        cx.emit(TerminalEvent::Typed(bytes));
    }

    /// Resize the local grid. Returns whether the size changed. The remote
    /// side is told separately via [`Self::notify_remote_resize`] so callers
    /// can debounce it.
    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: f32, cell_h: f32) -> bool {
        self.cell_px = (cell_w, cell_h);
        let size = GridSize::new(cols, rows);
        if size.cols as usize == self.cols() && size.rows as usize == self.rows() {
            return false;
        }
        self.term.resize(size);
        true
    }

    /// Emit [`TerminalEvent::Resized`] with the current grid and its pixel size.
    pub fn notify_remote_resize(&mut self, cx: &mut Context<Self>) {
        cx.emit(self.resized_event());
    }

    fn resized_event(&self) -> TerminalEvent {
        let (cols, rows) = (self.cols() as u16, self.rows() as u16);
        TerminalEvent::Resized {
            cols,
            rows,
            pixel_width: (self.cell_px.0 * f32::from(cols)) as u16,
            pixel_height: (self.cell_px.1 * f32::from(rows)) as u16,
        }
    }

    /// A cell's width and height in pixels, as of the last frame drawn; zero before the first.
    pub fn cell_px(&self) -> (f32, f32) {
        self.cell_px
    }

    pub fn cols(&self) -> usize {
        self.term.columns()
    }

    pub fn rows(&self) -> usize {
        self.term.screen_lines()
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Lines scrolled back into history (0 = pinned to the live bottom).
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Lines available above the viewport.
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// Scroll the view: positive = up into history, negative = toward live.
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// ⌘K: forgets the scrollback and what is on screen above the cursor's line, which moves
    /// to the top, so the prompt being typed at stays (as iTerm2's Clear Buffer and Ghostty's
    /// clear_screen keep it). Nothing is sent to the remote. On the alternate screen (vim,
    /// htop) only the history goes: that screen belongs to the program, which repaints it.
    pub fn clear(&mut self) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler};
        self.term.scroll_display(Scroll::Bottom);
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            let cursor = self.term.grid().cursor.point;
            let above = cursor.line.0.max(0) as usize;
            if above > 0 {
                self.term.scroll_up(above);
                self.term.goto(0, cursor.column.0);
            }
        }
        self.term.clear_screen(ClearMode::Saved);
        self.term.selection = None;
        self.clear_search();
    }

    // ---- selection ----
    //
    // `Term` owns the selection: it rotates the anchors when output scrolls
    // the grid and drops them on clear/resize.

    /// Grid point under a viewport cell (row 0 = top of the visible area).
    pub fn grid_point(&self, viewport_row: usize, col: usize) -> Point {
        Point::new(
            Line(viewport_row as i32 - self.display_offset() as i32),
            Column(col.min(self.cols().saturating_sub(1))),
        )
    }

    pub fn start_selection(&mut self, ty: SelectionType, point: Point, side: Side) {
        self.term.selection = Some(Selection::new(ty, point, side));
    }

    pub fn update_selection(&mut self, point: Point, side: Side) {
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Selected text; `None` for no selection or an empty one.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    fn selection_range(&self) -> Option<SelectionRange> {
        self.term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term))
    }

    // ---- find ----

    /// Search the whole grid, history included, for `query` (literal,
    /// smart-case) and make the newest match current, scrolling it into view.
    /// An empty query clears the search. Returns the number of matches.
    pub fn search(&mut self, query: &str) -> usize {
        if query.is_empty() {
            self.search = None;
            return 0;
        }
        let search = Search::new(&self.term, query);
        let count = search.count();
        self.search = Some(search);
        self.reveal_current_match();
        count
    }

    /// Move to the next match, going up into older output (`older`) or down
    /// toward newer output, wrapping at the ends, and scroll it into view.
    /// Returns whether there is a current match.
    pub fn search_step(&mut self, older: bool) -> bool {
        let Some(search) = self.search.as_mut() else {
            return false;
        };
        let found = search.step(&self.term, older).is_some();
        self.reveal_current_match();
        found
    }

    pub fn clear_search(&mut self) {
        self.search = None;
    }

    /// The active query, if a search is open.
    pub fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(Search::query)
    }

    pub fn search_count(&self) -> usize {
        self.search.as_ref().map_or(0, Search::count)
    }

    /// Zero-based index of the current match, in document order.
    pub fn search_index(&self) -> Option<usize> {
        self.search.as_ref().and_then(Search::current_index)
    }

    /// Matches touching the viewport, as grid-line ranges.
    pub(crate) fn visible_matches(&self) -> Vec<(Match, bool)> {
        let Some(search) = self.search.as_ref() else {
            return Vec::new();
        };
        let top = -(self.display_offset() as i32);
        search.visible(top, top + self.rows() as i32 - 1)
    }

    /// Scroll so the current match is on screen; a match already visible does
    /// not move the view, one off screen lands mid-viewport.
    fn reveal_current_match(&mut self) {
        let Some(line) = self
            .search
            .as_ref()
            .and_then(Search::current)
            .map(|m| m.start().line.0)
        else {
            return;
        };
        let offset = self.display_offset() as i32;
        let rows = self.rows() as i32;
        if line >= -offset && line < rows - offset {
            return;
        }
        let target = (rows / 2 - line).clamp(0, self.history_size() as i32);
        self.term.scroll_display(Scroll::Delta(target - offset));
    }

    // ---- links ----

    /// The link under a viewport cell: an OSC 8 hyperlink, else a URL found in
    /// the (soft-wrap-joined) logical line. `None` when the target is not
    /// something a click may open (see [`crate::links::is_openable`]).
    pub fn link_at(&self, viewport_row: usize, col: usize) -> Option<Link> {
        struct Entry {
            line: i32,
            col: usize,
            width: usize,
            ch: char,
            hyperlink: Option<alacritty_terminal::term::cell::Hyperlink>,
        }
        let hit = self.grid_point(viewport_row, col);
        let first = self.term.line_search_left(hit).line.0;
        let last = self.term.line_search_right(hit).line.0;
        let mut entries = Vec::new();
        for line in first..=last {
            let row = &self.term.grid()[Line(line)];
            for c in 0..self.cols() {
                let cell = &row[Column(c)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                entries.push(Entry {
                    line,
                    col: c,
                    width: if cell.flags.intersects(Flags::WIDE_CHAR) {
                        2
                    } else {
                        1
                    },
                    ch: cell.c,
                    hyperlink: cell.hyperlink(),
                });
            }
        }
        let at = entries.iter().position(|e| {
            e.line == hit.line.0 && (e.col..e.col + e.width).contains(&hit.column.0)
        })?;
        let (range, uri) = if let Some(link) = entries[at].hyperlink.clone() {
            let mut lo = at;
            while lo > 0 && entries[lo - 1].hyperlink.as_ref() == Some(&link) {
                lo -= 1;
            }
            let mut hi = at + 1;
            while hi < entries.len() && entries[hi].hyperlink.as_ref() == Some(&link) {
                hi += 1;
            }
            (lo..hi, link.uri().to_string())
        } else {
            let chars: Vec<char> = entries.iter().map(|e| e.ch).collect();
            let range = crate::links::find_url(&chars, at)?;
            let uri = chars[range.clone()].iter().collect();
            (range, uri)
        };
        if !crate::links::is_openable(&uri) {
            return None;
        }
        let offset = self.display_offset() as i32;
        let mut segments: Vec<(usize, usize, usize)> = Vec::new();
        for e in &entries[range] {
            let row = e.line + offset;
            if row < 0 || row >= self.rows() as i32 {
                continue;
            }
            let (row, end) = (row as usize, e.col + e.width);
            match segments.last_mut() {
                Some(seg) if seg.0 == row => seg.2 = end,
                _ => segments.push((row, e.col, end)),
            }
        }
        Some(Link { uri, segments })
    }

    // ---- snapshots ----

    fn line_inner(
        &self,
        viewport_row: usize,
        selection: Option<SelectionRange>,
        matches: &[(Match, bool)],
    ) -> Vec<CellSnapshot> {
        let offset = self.display_offset() as i32;
        let line = Line(viewport_row as i32 - offset);
        let row = &self.term.grid()[line];
        (0..self.cols())
            .map(|col| {
                let cell = &row[Column(col)];
                CellSnapshot {
                    ch: cell.c,
                    fg: map_color(cell.fg),
                    bg: map_color(cell.bg),
                    bold: cell.flags.intersects(Flags::BOLD),
                    dim: cell.flags.intersects(Flags::DIM),
                    italic: cell.flags.intersects(Flags::ITALIC),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                    strikeout: cell.flags.intersects(Flags::STRIKEOUT),
                    inverse: cell.flags.intersects(Flags::INVERSE),
                    hidden: cell.flags.intersects(Flags::HIDDEN),
                    wide: cell.flags.intersects(Flags::WIDE_CHAR),
                    wide_spacer: cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                    selected: selection
                        .is_some_and(|range| range.contains(Point::new(line, Column(col)))),
                    search: mark_at(matches, Point::new(line, Column(col))),
                }
            })
            .collect()
    }

    /// One viewport row (0 = top), honoring the scrollback offset.
    pub fn line(&self, viewport_row: usize) -> Vec<CellSnapshot> {
        self.line_inner(
            viewport_row,
            self.selection_range(),
            &self.visible_matches(),
        )
    }

    /// All viewport rows, top to bottom.
    pub fn lines(&self) -> Vec<Vec<CellSnapshot>> {
        // Resolve the selection once: `to_range` re-walks the grid for
        // semantic and line selections.
        let selection = self.selection_range();
        let matches = self.visible_matches();
        (0..self.rows())
            .map(|r| self.line_inner(r, selection, &matches))
            .collect()
    }

    /// Cursor in viewport coordinates; `None` when hidden or scrolled out.
    pub fn cursor(&self) -> Option<CursorSnapshot> {
        let shape = self.term.renderable_content().cursor.shape;
        if shape == CursorShape::Hidden {
            return None;
        }
        let (row, col) = self.cursor_cell()?;
        Some(CursorSnapshot { row, col, shape })
    }

    /// The cell the cursor is on as (row, column) in the viewport, hidden or not; `None` only
    /// when it is scrolled out. An input method anchors its text here.
    pub fn cursor_cell(&self) -> Option<(usize, usize)> {
        let Point { line, column } = self.term.renderable_content().cursor.point;
        let row = line.0 + self.display_offset() as i32;
        if row < 0 || row >= self.rows() as i32 {
            return None;
        }
        Some((row as usize, column.0))
    }

    /// The text on the cursor's row up to (not including) the cursor, trailing blanks
    /// trimmed: what the remote has written at the line being typed, so a prompt can be
    /// recognised before anything is sent. `None` when the cursor is hidden or scrolled out.
    pub fn cursor_line_text(&self) -> Option<String> {
        let cursor = self.cursor()?;
        let mut text: String = self
            .line(cursor.row)
            .iter()
            .take(cursor.col)
            .filter(|c| !c.wide_spacer)
            .map(|c| c.ch)
            .collect();
        while text.ends_with(' ') {
            text.pop();
        }
        Some(text)
    }

    /// A viewport row as trimmed text (wide-char spacers skipped).
    pub fn row_text(&self, viewport_row: usize) -> String {
        let mut text: String = self
            .line(viewport_row)
            .iter()
            .filter(|c| !c.wide_spacer)
            .map(|c| c.ch)
            .collect();
        while text.ends_with(' ') {
            text.pop();
        }
        text
    }
}

#[path = "osc133.rs"]
mod osc133;

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "terminal_bench_tests.rs"]
mod bench_tests;

#[cfg(test)]
#[path = "history_trim_tests.rs"]
mod history_trim_tests;
