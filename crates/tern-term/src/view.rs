// Adapted from zeron crates/ui/src/terminal/{panel,view}.rs (MIT) and zed
// crates/terminal_view/src/terminal_view.rs (GPL-3.0-or-later).
//
//! The terminal view: focus, keyboard -> bytes, paste/copy, mouse selection,
//! wheel scrolling (or mouse reporting when the remote app asked for it), and
//! debounced resize. Repaints only when the [`Terminal`] model or the view
//! itself calls `cx.notify()`; there is no frame loop, only a 500ms tick that
//! repaints when a blinking cursor changes phase.

use std::time::{Duration, Instant};

use alacritty_terminal::index::{Column, Line, Point as GridPoint};
use alacritty_terminal::term::TermMode;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, App, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, ScrollDelta, ScrollWheelEvent, Styled,
    Subscription, Task, TouchPhase, Window, div, px,
};

use crate::config::TerminalOptions;
use crate::config::TerminalTheme;
use crate::element::TerminalElement;
use crate::links::Link;
use crate::mappings::keys::keystroke_bytes;
use crate::mappings::mouse::{alt_scroll, mouse_button_report, mouse_moved_report, scroll_report};
use crate::search::{FindKey, apply_key, paste_into};
use crate::terminal::{SelectionType, Side, Terminal, TerminalEvent};

/// Inner padding of the grid area.
pub const TERM_PADDING: f32 = 8.0;
/// Debounce for the remote resize callback after viewport-driven size changes.
pub const RESIZE_DEBOUNCE_MS: u64 = 80;
/// Selection auto-scroll tick while the pointer is dragged past an edge.
const SELECTION_SCROLL_TICK_MS: u64 = 50;
/// Minimum pointer travel before a press turns into a selection (gpui's own
/// `div` drag threshold). Without it the focusing click can start a one-cell
/// selection.
pub const SELECTION_DRAG_THRESHOLD: f32 = 2.0;

/// Where the grid sits and how big its cells are, as measured by the element.
#[derive(Clone, Copy, Debug)]
pub struct GridGeometry {
    pub origin: gpui::Point<Pixels>,
    pub cell_w: f32,
    pub line_h: f32,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Copy)]
struct SelectionDrag {
    origin: gpui::Point<Pixels>,
    position: gpui::Point<Pixels>,
    armed: bool,
}

pub struct TerminalView {
    pub(crate) terminal: Entity<Terminal>,
    /// macOS: Option sends ESC-prefixed keys (Meta) instead of composing characters.
    option_as_meta: bool,
    focus_handle: FocusHandle,
    geometry: Option<GridGeometry>,
    resize_task: Option<Task<()>>,
    selection_drag: Option<SelectionDrag>,
    selection_scroll_task: Option<Task<()>>,
    scroll_remainder: f32,
    /// A mouse-reporting button is held, so its release must be reported.
    reporting_button: bool,
    /// The find bar's query while the bar is open.
    find: Option<String>,
    /// The link under the pointer while the link modifier is held.
    hover_link: Option<Link>,
    last_pointer: Option<gpui::Point<Pixels>>,
    options: TerminalOptions,
    /// Blink phase: whether a blinking cursor is currently drawn.
    cursor_on: bool,
    last_input: Instant,
    _blink_task: Task<()>,
    /// Counts bells, so each flash restarts its fade animation.
    bell_seq: usize,
    bell_active: bool,
    bell_task: Option<Task<()>>,
    _bell_subscription: Subscription,
    _subscription: Subscription,
    /// Prepaints so far; tests count them to tell a rebuilt view from a reused one.
    #[cfg(test)]
    prepaints: usize,
}

impl std::fmt::Debug for TerminalView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalView")
            .field("geometry", &self.geometry)
            .finish_non_exhaustive()
    }
}

impl TerminalView {
    pub fn new(
        terminal: Entity<Terminal>,
        theme: TerminalTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        terminal.update(cx, |t, _| t.set_theme(theme));
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        // Repaint exactly when the model changes.
        let subscription = cx.observe(&terminal, |this, _, cx| {
            // Output moved the grid under any hover; the next pointer move or
            // modifier change recomputes it.
            this.hover_link = None;
            cx.notify();
        });
        let bell_subscription = cx.subscribe(&terminal, |this, _, event: &TerminalEvent, cx| {
            if this.options.flashes_on(event) {
                this.flash(cx);
            }
        });
        Self {
            terminal,
            option_as_meta: true,
            focus_handle,
            geometry: None,
            resize_task: None,
            selection_drag: None,
            selection_scroll_task: None,
            scroll_remainder: 0.0,
            reporting_button: false,
            find: None,
            hover_link: None,
            last_pointer: None,
            options: TerminalOptions::default(),
            cursor_on: true,
            last_input: Instant::now(),
            _blink_task: Self::blink_loop(cx),
            bell_seq: 0,
            bell_active: false,
            bell_task: None,
            _bell_subscription: bell_subscription,
            _subscription: subscription,
            #[cfg(test)]
            prepaints: 0,
        }
    }

    /// `view` as a child that fills its parent and is rebuilt only when it changes: when it or
    /// its terminal notifies (output, selection, theme, options, blink phase, find, bell), when
    /// its bounds or inherited text style change, or when the window is refreshed (a focus
    /// change, a scale change). A parent that re-renders around an unchanged terminal then
    /// replays the terminal's last frame instead of snapshotting and shaping the grid again.
    /// The size is definite (the parent's), as GPUI's cache needs.
    pub fn cached_child(view: &Entity<Self>) -> impl IntoElement {
        view.clone()
            .cached(gpui::StyleRefinement::default().size_full())
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn set_theme(&mut self, theme: TerminalTheme, cx: &mut Context<Self>) {
        self.terminal.update(cx, |t, _| t.set_theme(theme));
        cx.notify();
    }

    pub fn set_option_as_meta(&mut self, on: bool) {
        self.option_as_meta = on;
    }

    fn mode(&self, cx: &App) -> TermMode {
        self.terminal.read(cx).mode()
    }

    /// Called from element prepaint with the frame's measured grid. Resizes
    /// the local grid immediately and debounces the remote notification.
    pub(crate) fn on_grid_metrics(&mut self, geometry: GridGeometry, cx: &mut Context<Self>) {
        #[cfg(test)]
        {
            self.prepaints += 1;
        }
        self.geometry = Some(geometry);
        let changed = self.terminal.update(cx, |t, _| {
            t.resize(
                geometry.cols,
                geometry.rows,
                geometry.cell_w,
                geometry.line_h,
            )
        });
        if !changed {
            return;
        }
        self.resize_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(RESIZE_DEBOUNCE_MS))
                .await;
            // Read the current size: later prepaints may have resized again
            // inside the debounce window.
            let _ = this.update(cx, |view, cx| {
                view.terminal.update(cx, |t, cx| t.notify_remote_resize(cx));
            });
        }));
        // No cx.notify(): this runs during prepaint, which already paints the
        // resized grid.
    }

    // ---- keyboard ----

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = &ks.modifiers;
        if is_find_chord(ks) {
            let query = self.find.clone().unwrap_or_default();
            self.find(&query, cx);
            cx.stop_propagation();
            return;
        }
        if self.find.is_some() {
            self.on_find_key(ks, cx);
            cx.stop_propagation();
            return;
        }
        // Paste: Cmd+V (macOS) / Ctrl+Shift+V.
        if ks.key == "v" && (mods.platform || (mods.control && mods.shift)) {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }
        // Copy: Cmd+C / Ctrl+Shift+C, only swallowed when something was
        // copied, so plain Ctrl+C always reaches the remote as the interrupt.
        if ks.key == "c" && (mods.platform || (mods.control && mods.shift)) && self.copy(cx) {
            cx.stop_propagation();
            return;
        }
        let mode = self.mode(cx);
        // The alternate screen has no scrollback; the program there gets these keys.
        if let Some(key) = scrollback_key(ks).filter(|_| !mode.contains(TermMode::ALT_SCREEN)) {
            self.terminal.update(cx, |t, cx| {
                let page = t.rows() as i32;
                match key {
                    ScrollbackKey::PageUp => t.scroll(page),
                    ScrollbackKey::PageDown => t.scroll(-page),
                    ScrollbackKey::Top => t.scroll(t.history_size() as i32),
                    ScrollbackKey::Bottom => t.scroll_to_bottom(),
                }
                cx.notify();
            });
            cx.stop_propagation();
            return;
        }
        let bytes = keystroke_bytes(ks, mode, self.option_as_meta);
        if let Some(bytes) = bytes {
            self.send_input(bytes, cx);
            cx.stop_propagation();
        }
    }

    /// Keyboard input: snap back to the live bottom, clear any selection, and
    /// write to the remote.
    fn send_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        self.note_input();
        self.terminal.update(cx, |t, cx| {
            if t.display_offset() > 0 {
                t.scroll_to_bottom();
                cx.notify();
            }
            if t.has_selection() {
                t.clear_selection();
                cx.notify();
            }
            t.write(bytes, cx);
        });
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = self.mode(cx).contains(TermMode::BRACKETED_PASTE);
        self.send_input(paste_bytes(&text, bracketed), cx);
    }

    /// Copy the selection; returns whether anything was copied.
    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.terminal.read(cx).selection_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    // ---- mouse ----

    /// Viewport cell under a window position, plus which edge of it.
    fn hit(&self, position: gpui::Point<Pixels>) -> Option<CellHit> {
        let g = self.geometry?;
        Some(cell_at(
            f32::from(position.x - g.origin.x),
            f32::from(position.y - g.origin.y),
            g.cell_w,
            g.line_h,
            g.cols as usize,
            g.rows as usize,
        ))
    }

    fn report_point(hit: CellHit) -> GridPoint {
        GridPoint::new(Line(hit.row as i32), Column(hit.col))
    }

    /// Mouse reporting applies when the remote asked for it and Shift is not
    /// held (Shift is the standard override to select text anyway).
    fn reporting(&self, shift: bool, cx: &App) -> bool {
        !shift && self.mode(cx).intersects(TermMode::MOUSE_MODE)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some(hit) = self.hit(event.position) else {
            return;
        };
        if event.button == MouseButton::Left
            && let Some(link) = self.link_under(event.position, &event.modifiers, cx)
        {
            cx.open_url(&link.uri);
            return;
        }
        if self.reporting(event.modifiers.shift, cx) {
            let mode = self.mode(cx);
            if let Some(bytes) = mouse_button_report(
                Self::report_point(hit),
                event.button,
                event.modifiers,
                true,
                mode,
            ) {
                self.reporting_button = true;
                self.terminal.update(cx, |t, cx| t.write(bytes, cx));
            }
            return;
        }
        if event.button == MouseButton::Middle {
            if self.options.middle_click_paste {
                self.paste(cx);
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let ty = match event.click_count {
            0 => return,
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        let point = self.terminal.read(cx).grid_point(hit.row, hit.col);
        if ty == SelectionType::Simple {
            // Shift+click extends an existing selection.
            let extended = event.modifiers.shift
                && self.terminal.update(cx, |t, _| {
                    let has = t.has_selection();
                    if has {
                        t.update_selection(point, hit.side);
                    }
                    has
                });
            if !extended {
                // Clear and arm; the selection itself only begins once the
                // pointer travels far enough to mean it.
                self.terminal.update(cx, |t, _| t.clear_selection());
            }
            self.selection_drag = Some(SelectionDrag {
                origin: event.position,
                position: event.position,
                armed: extended,
            });
        } else {
            self.terminal
                .update(cx, |t, _| t.start_selection(ty, point, hit.side));
            self.selection_drag = Some(SelectionDrag {
                origin: event.position,
                position: event.position,
                armed: true,
            });
        }
        cx.notify();
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_hover(event.position, &event.modifiers, cx);
        if self.reporting(event.modifiers.shift, cx) {
            if let Some(hit) = self.hit(event.position) {
                let mode = self.mode(cx);
                if let Some(bytes) = mouse_moved_report(
                    Self::report_point(hit),
                    event.pressed_button,
                    event.modifiers,
                    mode,
                ) {
                    self.terminal.update(cx, |t, cx| t.write(bytes, cx));
                }
            }
            return;
        }
        if !event.dragging() {
            return;
        }
        let Some(mut drag) = self.selection_drag else {
            return;
        };
        drag.position = event.position;
        self.selection_drag = Some(drag);
        if !drag.armed {
            let dx = f32::from(event.position.x - drag.origin.x);
            let dy = f32::from(event.position.y - drag.origin.y);
            if dx.hypot(dy) < SELECTION_DRAG_THRESHOLD {
                return;
            }
            // Anchor at the press, not here, so the selection covers the whole
            // gesture.
            let Some(hit) = self.hit(drag.origin) else {
                return;
            };
            self.terminal.update(cx, |t, _| {
                let p = t.grid_point(hit.row, hit.col);
                t.start_selection(SelectionType::Simple, p, hit.side)
            });
            self.selection_drag = Some(SelectionDrag {
                armed: true,
                ..drag
            });
        }
        let Some(hit) = self.hit(event.position) else {
            return;
        };
        self.terminal.update(cx, |t, _| {
            let p = t.grid_point(hit.row, hit.col);
            t.update_selection(p, hit.side)
        });
        cx.notify();
        self.schedule_selection_scroll(cx);
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.reporting_button {
            self.reporting_button = false;
            if let Some(hit) = self.hit(event.position) {
                let mode = self.mode(cx);
                if let Some(bytes) = mouse_button_report(
                    Self::report_point(hit),
                    event.button,
                    event.modifiers,
                    false,
                    mode,
                ) {
                    self.terminal.update(cx, |t, cx| t.write(bytes, cx));
                }
            }
        }
        let selected = self.terminal.read(cx).has_selection();
        let dragged = self.selection_drag.is_some_and(|drag| drag.armed);
        if self
            .options
            .copies_on_release(event.button == MouseButton::Left, dragged, selected)
        {
            self.copy(cx);
        }
        self.selection_drag = None;
        self.selection_scroll_task = None;
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let line_h = self.geometry.map(|g| g.line_h).unwrap_or(16.0);
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => f32::from(delta.y) / line_h,
        };
        if event.touch_phase == TouchPhase::Started {
            self.scroll_remainder = 0.0;
        }
        self.scroll_remainder += lines;
        let step = self.scroll_remainder.trunc() as i32;
        self.scroll_remainder -= step as f32;
        if step == 0 {
            return;
        }
        let mode = self.mode(cx);
        if self.reporting(event.modifiers.shift, cx) {
            if let Some(hit) = self.hit(event.position)
                && let Some(reports) = scroll_report(Self::report_point(hit), step, event, mode)
            {
                for report in reports {
                    self.terminal.update(cx, |t, cx| t.write(report, cx));
                }
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            && !event.modifiers.shift
        {
            // Full-screen apps without mouse mode (less, man): wheel = arrows.
            self.terminal
                .update(cx, |t, cx| t.write(alt_scroll(step), cx));
        } else {
            self.terminal.update(cx, |t, cx| {
                t.scroll(step);
                cx.notify();
            });
        }
        cx.stop_propagation();
    }

    // ---- selection auto-scroll ----

    /// Lines to scroll per tick while a drag sits past the top/bottom edge:
    /// positive = into history.
    fn selection_scroll_lines(&self, position: gpui::Point<Pixels>) -> i32 {
        let Some(g) = self.geometry else { return 0 };
        let grid_height = g.line_h * g.rows as f32;
        if grid_height <= 0.0 {
            return 0;
        }
        let edge = g.line_h.min(grid_height / 3.0);
        let y = f32::from(position.y);
        let top = f32::from(g.origin.y);
        let bottom = top + grid_height;
        let speed = |penetration: f32| {
            let t = (penetration / edge).clamp(0.0, 1.0);
            (1.0 + 2.0 * t * t).round() as i32
        };
        if y < top + edge {
            speed(top + edge - y)
        } else if y > bottom - edge {
            -speed(y - (bottom - edge))
        } else {
            0
        }
    }

    fn schedule_selection_scroll(&mut self, cx: &mut Context<Self>) {
        if self.selection_scroll_task.is_some() {
            return;
        }
        let Some(drag) = self.selection_drag else {
            return;
        };
        if !drag.armed || self.selection_scroll_lines(drag.position) == 0 {
            return;
        }
        self.selection_scroll_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SELECTION_SCROLL_TICK_MS))
                .await;
            let _ = this.update(cx, |view, cx| {
                view.selection_scroll_task = None;
                view.step_selection_scroll(cx);
            });
        }));
    }

    fn step_selection_scroll(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.selection_drag else {
            return;
        };
        let lines = self.selection_scroll_lines(drag.position);
        if !drag.armed || lines == 0 {
            return;
        }
        let hit = self.hit(drag.position);
        self.terminal.update(cx, |t, cx| {
            t.scroll(lines);
            if let Some(hit) = hit {
                let p = t.grid_point(hit.row, hit.col);
                t.update_selection(p, hit.side);
            }
            cx.notify();
        });
        self.schedule_selection_scroll(cx);
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let find_bar = self.find.as_ref().map(|query| {
            let t = self.terminal.read(cx);
            let theme = t.theme();
            let status = find_status(query, t.search_count(), t.search_index());
            div()
                .absolute()
                .top(px(10.0))
                .right(px(18.0))
                .px(px(10.0))
                .py(px(5.0))
                .gap(px(10.0))
                .flex()
                .items_center()
                .rounded(px(6.0))
                .border_1()
                .border_color(theme.ansi[8])
                .bg(theme.ansi[0])
                .text_color(theme.foreground)
                .font_family(theme.font_family.clone())
                .text_size(px(theme.font_size))
                .child(div().min_w(px(160.0)).child(format!("{query}\u{258f}")))
                .child(div().text_color(theme.ansi[8]).child(status))
        });
        let linking = self.hover_link.is_some();
        let bell = self.bell_active.then(|| {
            let wash = self.terminal.read(cx).theme().foreground;
            div().absolute().size_full().bg(wash).with_animation(
                ("bell", self.bell_seq),
                Animation::new(Duration::from_millis(BELL_FLASH_MS)),
                |overlay, t| overlay.opacity(BELL_PEAK_OPACITY * (1.0 - t)),
            )
        });
        div()
            .id("tern-terminal")
            .size_full()
            .bg({
                let theme = self.terminal.read(cx).theme();
                theme.background.opacity(theme.background_alpha)
            })
            .when(linking, |d| d.cursor_pointer())
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            // Bound on the window as well: a drag that ends outside the view
            // still has to end the gesture.
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(
                cx.listener(|this, event: &ScrollWheelEvent, _, cx| this.on_scroll(event, cx)),
            )
            .child(TerminalElement::new(cx.entity(), focused))
            .children(find_bar)
            .children(bell)
    }
}

/// Which cell a pointer landed on, and which edge of it a selection anchors to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellHit {
    pub row: usize,
    pub col: usize,
    pub side: Side,
}

/// Map a position relative to the grid's top-left onto a cell. Positions
/// outside the grid clamp to the nearest cell (a drag routinely leaves the
/// view), and overshoot forces the side so dragging past the bottom takes the
/// last line whole.
pub fn cell_at(x: f32, y: f32, cell_w: f32, line_h: f32, cols: usize, rows: usize) -> CellHit {
    let usable = |v: f32| v.is_finite() && v > 0.0;
    if cols == 0 || rows == 0 || !usable(cell_w) || !usable(line_h) {
        return CellHit {
            row: 0,
            col: 0,
            side: Side::Left,
        };
    }
    let x = if x.is_finite() { x } else { 0.0 };
    let y = if y.is_finite() { y } else { 0.0 };
    let last_col = cols - 1;
    let last_row = rows - 1;

    let raw_col = (x / cell_w).floor();
    let mut side = if x.max(0.0) % cell_w > cell_w / 2.0 {
        Side::Right
    } else {
        Side::Left
    };
    let col = if raw_col > last_col as f32 {
        side = Side::Right;
        last_col
    } else {
        raw_col.max(0.0) as usize
    };

    let raw_row = (y / line_h).floor();
    let row = if raw_row > last_row as f32 {
        side = Side::Right;
        last_row
    } else if raw_row < 0.0 {
        side = Side::Left;
        0
    } else {
        raw_row as usize
    };
    CellHit { row, col, side }
}

/// Wrap pasted text for the remote (bracketed-paste aware; strips the one
/// control sequence a paste could use to break out of the bracket).
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    // Until none is left: removing one can join the bytes around it into another
    // (`\x1b[20\x1b[201~1~`), which would end the paste early and run the rest.
    let mut sanitized = text.to_owned();
    while sanitized.contains("\x1b[201~") {
        sanitized = sanitized.replace("\x1b[201~", "");
    }
    if bracketed {
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        sanitized.into_bytes()
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "view_cache_tests.rs"]
mod cache_tests;

mod cursor;
mod find_links;
use cursor::{BELL_FLASH_MS, BELL_PEAK_OPACITY};
use find_links::{find_status, is_find_chord};

/// Moving through the scrollback from the keyboard, as Terminal.app does.
#[derive(Debug, PartialEq, Eq)]
enum ScrollbackKey {
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// ⇧PageUp and ⇧PageDown move a page; ⌘Home and ⌘End (Ctrl+Shift+Home/End off macOS) go to
/// the ends. Plain PageUp and Home still reach the remote.
fn scrollback_key(ks: &gpui::Keystroke) -> Option<ScrollbackKey> {
    let m = &ks.modifiers;
    let ends = if cfg!(target_os = "macos") {
        m.platform && !m.shift && !m.control && !m.alt
    } else {
        m.control && m.shift && !m.alt && !m.platform
    };
    let page = m.shift && !m.platform && !m.control && !m.alt;
    match ks.key.as_str() {
        "pageup" if page => Some(ScrollbackKey::PageUp),
        "pagedown" if page => Some(ScrollbackKey::PageDown),
        "home" if ends => Some(ScrollbackKey::Top),
        "end" if ends => Some(ScrollbackKey::Bottom),
        _ => None,
    }
}
