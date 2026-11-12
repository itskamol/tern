//! A split tab in the window: dragging and stepping the dividers, splitting with a chosen or a
//! dropped host, and the focus marking. The rules (minimums, which side a pane lands on, the
//! title) are in `tab_set.rs` and `split.rs`; this file only feeds them the tab's size in cells
//! and draws what they say.

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Bounds, Context, DragMoveEvent, InteractiveElement, IntoElement, MouseButton,
    MouseUpEvent, ParentElement, Pixels, Point, Role, StatefulInteractiveElement, Styled, Window,
    div, px, relative, transparent_black,
};
use tern_ssh::{ConnectSpec, HostEntry};

use super::*;
use crate::keymap::{self, Keymap, ShortcutId};
use crate::sidebar::DraggedHost;
use crate::split::{Axis, Direction, Floor, Node, PaneId, drop_side};
use crate::tab_set::Paned as _;
use crate::theme::{FOCUS_BORDER, UNFOCUSED_OPACITY};

/// What a divider drag carries: a pane at the divider's first side and the side of it the
/// divider is on, which is how `TabSet::resize` names a divider.
#[derive(Clone, Copy)]
pub(super) struct Divider {
    pane: PaneId,
    side: Direction,
}

const NO_ROOM: &str = "No room to split: a pane keeps at least 20 columns and 5 rows";

impl Shell {
    /// The least share of the front tab a pane may take, from the tab's own area in cells: what
    /// each pane's grid and its border show, scaled up by its share of the tab.
    fn pane_floor(&self, cx: &gpui::App) -> Floor {
        let Some(tab) = self.tabs.active_tab() else {
            return Floor::cells(1.0, 1.0);
        };
        let layout = tab.layout();
        let mut cell = (0.0, 0.0);
        let measured: Vec<_> = tab
            .panes()
            .iter()
            .filter_map(|pane| {
                let terminal = pane.session()?.read(cx).view.read(cx).terminal().read(cx);
                cell = terminal.cell_px();
                let (_, share) = layout.iter().find(|(id, _)| *id == pane.pane_id())?;
                Some((terminal.cols() as f32, terminal.rows() as f32, *share))
            })
            .collect();
        if measured.is_empty() || cell.0 < 1.0 || cell.1 < 1.0 {
            return Floor::cells(1.0, 1.0);
        }
        // A pane's focus border on both sides and one divider, in cells.
        let edge = 2.0 * FOCUS_BORDER + 1.0;
        Floor::of_panes(&measured, (edge / cell.0, edge / cell.1))
    }

    pub(crate) fn focused_pane(&self) -> Option<PaneId> {
        self.tabs.active_tab().map(|tab| tab.focused())
    }

    /// Checks the split first and says why when it cannot be made, so no pane is made for it.
    fn room_to_split(&mut self, pane: PaneId, side: Direction, cx: &mut Context<Self>) -> bool {
        if self.tabs.is_empty() {
            return false;
        }
        let floor = self.pane_floor(cx);
        let room = self.tabs.can_split(pane, side, floor);
        if !room {
            self.activity(NO_ROOM.to_owned(), cx);
        }
        room
    }

    /// ⌘D and ⌘⇧D: another session to the focused pane's host, beside it.
    pub(crate) fn split_pane(
        &mut self,
        side: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.active_tab() else {
            return;
        };
        let target = tab.focused();
        let Some(session) = tab.session() else {
            return;
        };
        let launch = session.read(cx).launch_again();
        let host = tab.front().host.clone();
        self.split_with(target, side, launch, &host, window, cx);
    }

    /// Splits `target` with a session of `launch` for `host`, on `side`.
    pub(crate) fn split_with(
        &mut self,
        target: PaneId,
        side: Direction,
        launch: Launch,
        host: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.room_to_split(target, side, cx) {
            return;
        }
        let floor = self.pane_floor(cx);
        let pane = self.make_pane(launch, host, window, cx);
        if self.tabs.split(target, side, pane, floor).is_ok() {
            self.settings_page = None;
            self.focus_front_pane(window, cx);
        }
    }

    /// A host chosen in the picker or dropped on a pane: the new pane opens it.
    pub(crate) fn split_with_host(
        &mut self,
        target: PaneId,
        side: Direction,
        host: &HostEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.record_recent(&host.alias);
        match ConnectSpec::from_host_entry(host) {
            Ok(spec) => self.split_with(target, side, Launch::Ssh(spec), &host.alias, window, cx),
            Err(e) => self.fail(e.to_string(), cx),
        }
    }

    /// Text typed into the picker that names no listed host: `user@host:port` or an alias.
    pub(crate) fn split_with_target(
        &mut self,
        target: PaneId,
        side: Direction,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(host) = self.hosts.iter().find(|h| h.alias == text).cloned() {
            return self.split_with_host(target, side, &host, window, cx);
        }
        match ConnectSpec::parse(text) {
            Ok(spec) => self.split_with(target, side, Launch::Ssh(spec), text, window, cx),
            Err(e) => self.fail(e.to_string(), cx),
        }
    }

    /// ⌥⌘D and the Shell menu: the host picker, whose choice opens in the new pane. Nothing
    /// changes when it is closed.
    pub(crate) fn pick_host_for_split(
        &mut self,
        side: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.tabs.active_tab().map(|t| t.focused()) else {
            return;
        };
        if !self.room_to_split(target, side, cx) {
            return;
        }
        let picker = Picker::for_split(side, cx);
        window.focus(picker.focus(), cx);
        self.picker = Some(picker);
        cx.notify();
    }

    /// A host from the sidebar dropped on a pane: it opens on the half the highlight showed.
    fn drop_host(&mut self, pane: PaneId, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let side = self
            .drop_hint
            .take()
            .filter(|(p, _)| *p == pane)
            .map(|(_, s)| s);
        cx.notify();
        if let (Some(side), Some(host)) = (side, self.hosts.get(ix).cloned()) {
            self.split_with_host(pane, side, &host, window, cx);
        }
    }

    /// The pointer of a host drag is over `pane`: the half it would land on is highlighted.
    fn hint_drop(
        &mut self,
        pane: PaneId,
        bounds: Bounds<Pixels>,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let hint = bounds.contains(&at).then(|| {
            let x = f32::from(at.x - bounds.left()) / f32::from(bounds.size.width).max(1.0);
            let y = f32::from(at.y - bounds.top()) / f32::from(bounds.size.height).max(1.0);
            (pane, drop_side(x, y))
        });
        let next = match hint {
            Some(h) => Some(h),
            None => self.drop_hint.filter(|(p, _)| *p != pane),
        };
        if next != self.drop_hint {
            self.drop_hint = next;
            cx.notify();
        }
    }

    /// While a divider is held the remote is not told of each size the panes pass through; on
    /// release it hears the last one. Released in every tab, since a drag can end anywhere: on
    /// another tab, over the sidebar, or outside the window.
    fn hold_pane_resizes(&self, held: bool, cx: &mut Context<Self>) {
        for session in self
            .tabs
            .iter()
            .filter(|tab| !held || self.tabs.active_tab().is_some_and(|a| a.id == tab.id))
            .flat_map(|tab| tab.panes().iter())
            .filter_map(Pane::session)
        {
            let view = session.read(cx).view.clone();
            view.update(cx, |v, cx| v.hold_remote_resize(held, cx));
        }
    }

    /// The pointer of a divider drag is at `ratio` of the split's length.
    fn drag_divider(&mut self, divider: Divider, ratio: f32, cx: &mut Context<Self>) {
        let floor = self.pane_floor(cx);
        if self.tabs.resize(divider.pane, divider.side, ratio, floor) {
            cx.notify();
        }
    }

    fn reset_divider(&mut self, divider: Divider, cx: &mut Context<Self>) {
        let floor = self.pane_floor(cx);
        if self.tabs.reset_divider(divider.pane, divider.side, floor) {
            cx.notify();
        }
    }

    /// ⌘⌃ and an arrow: the divider next to the focused pane moves 5% that way.
    pub(crate) fn step_pane(&mut self, dir: Direction, cx: &mut Context<Self>) {
        let floor = self.pane_floor(cx);
        if self.tabs.step_focused(dir, floor) {
            cx.notify();
        }
    }

    pub(crate) fn equalize_panes(&mut self, cx: &mut Context<Self>) {
        let floor = self.pane_floor(cx);
        self.tabs.equalize(floor);
        cx.notify();
    }

    /// The tab's terminals, in their split layout.
    pub(super) fn render_panes(&self, tab: &Tab, cx: &mut Context<Self>) -> AnyElement {
        self.render_node(tab, tab.tree(), tab.panes().len() > 1, cx)
    }

    fn render_node(
        &self,
        tab: &Tab,
        node: &Node,
        split: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            Node::Leaf(id) => self.render_pane(tab, *id, split, cx),
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let Some(&last_of_first) = first.leaves().last() else {
                    return div().into_any_element();
                };
                let axis = *axis;
                let divider = Divider {
                    pane: last_of_first,
                    side: match axis {
                        Axis::Row => Direction::Right,
                        Axis::Column => Direction::Down,
                    },
                };
                let a = self.render_node(tab, first, split, cx);
                let b = self.render_node(tab, second, split, cx);
                let share = |share: f32, child: AnyElement| {
                    let mut part = div().flex_1().min_w(px(0.)).min_h(px(0.));
                    part.style().flex_grow = Some(share);
                    part.child(child)
                };
                div()
                    .size_full()
                    .flex()
                    .when(axis == Axis::Column, |el| el.flex_col())
                    .on_drag_move(cx.listener(move |s, e: &DragMoveEvent<Divider>, _, cx| {
                        let drag = *e.drag(cx);
                        if drag.pane != divider.pane {
                            return;
                        }
                        let (at, start, len) = match axis {
                            Axis::Row => (e.event.position.x, e.bounds.left(), e.bounds.size.width),
                            Axis::Column => {
                                (e.event.position.y, e.bounds.top(), e.bounds.size.height)
                            }
                        };
                        s.drag_divider(
                            divider,
                            f32::from(at - start) / f32::from(len).max(1.0),
                            cx,
                        );
                    }))
                    .child(share(*ratio, a))
                    .child(self.render_divider(axis, divider, cx))
                    .child(share(1.0 - *ratio, b))
                    .into_any_element()
            }
        }
    }

    /// A one-pixel line with a wider grip over it: dragged to resize, double-clicked to put the
    /// split back to half and half.
    fn render_divider(
        &self,
        axis: Axis,
        divider: Divider,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        const GRIP: f32 = 7.;
        let line = div().flex_none().relative().bg(self.theme.border);
        let grip = div()
            .id(("divider", divider.pane))
            .absolute()
            .occlude()
            .on_drag(divider, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|s, _, _, cx| s.hold_pane_resizes(true, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |s, e: &MouseUpEvent, _, cx| {
                    if e.click_count == 2 {
                        s.reset_divider(divider, cx);
                    }
                    s.hold_pane_resizes(false, cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|s, _, _, cx| s.hold_pane_resizes(false, cx)),
            );
        match axis {
            Axis::Row => line.w(px(1.)).h_full().child(
                grip.top_0()
                    .bottom_0()
                    .left(px(-GRIP / 2.))
                    .w(px(GRIP))
                    .cursor_col_resize(),
            ),
            Axis::Column => line.h(px(1.)).w_full().child(
                grip.left_0()
                    .right_0()
                    .top(px(-GRIP / 2.))
                    .h(px(GRIP))
                    .cursor_row_resize(),
            ),
        }
    }

    fn render_pane(
        &self,
        tab: &Tab,
        id: PaneId,
        split: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.theme;
        let Some(pane) = tab.pane(id) else {
            return div().into_any_element();
        };
        let focused = tab.focused() == id;
        let hint = self
            .drop_hint
            .filter(|(p, _)| *p == id)
            .map(|(_, side)| side);
        // A pane not in front that waits at a password prompt says so; the badge is read from
        // the session each frame, so it is gone as soon as the prompt is.
        let asking = split && !focused && pane.waiting_for_fill(cx);
        let combo = cx.global::<Keymap>().combo(ShortcutId::FillPassword);
        div()
            .id(("pane", id))
            .relative()
            .size_full()
            // Said aloud when focus lands here, so moving between panes is not only visual.
            .role(Role::Group)
            .aria_label(match (focused && split, asking) {
                (_, true) => format!("{}, password asked", pane.host),
                (true, _) => format!("{}, focused pane", pane.host),
                _ => pane.host.clone(),
            })
            // Only a split tab marks its focus; a lone pane would just wear a frame.
            .when(split, |el| {
                el.border(px(FOCUS_BORDER))
                    .border_color(if focused {
                        t.accent
                    } else {
                        transparent_black()
                    })
                    .when(!focused, |el| el.opacity(UNFOCUSED_OPACITY))
            })
            // Before the terminal sees the click, so selecting text in a pane also gives it the
            // keyboard.
            .capture_any_mouse_down(cx.listener(move |s, _, w, cx| s.focus_pane(id, w, cx)))
            .on_drag_move(
                cx.listener(move |s, e: &DragMoveEvent<DraggedHost>, _, cx| {
                    s.hint_drop(id, e.bounds, e.event.position, cx)
                }),
            )
            .on_drop(
                cx.listener(move |s, from: &DraggedHost, w, cx| s.drop_host(id, from.ix, w, cx)),
            )
            .child(match &pane.content {
                Content::Session(term) => {
                    tern_term::TerminalView::cached_child(&term.session.read(cx).view)
                        .into_any_element()
                }
                Content::Files(files) => files.clone().into_any_element(),
            })
            .when_some(hint, |el, side| el.child(drop_zone(side, &t)))
            .when(asking, |el| el.child(password_badge(combo, &t)))
            .into_any_element()
    }
}

/// "Password asked", in a corner of a pane that is waiting at a prompt; with the Fill shortcut
/// named only while it is bound.
fn password_badge(combo: &str, t: &Theme) -> gpui::Div {
    let text = if combo.is_empty() {
        "Password asked".to_owned()
    } else {
        format!("Password asked · {}", keymap::badge(combo))
    };
    div()
        .absolute()
        .top(px(8.))
        .right(px(8.))
        .px(px(6.))
        .py(px(2.))
        .rounded(px(4.))
        .bg(t.accent)
        .text_color(t.on_accent)
        .text_size(px(11.))
        .child(text)
}

/// The half of a pane a dropped host would take.
fn drop_zone(side: Direction, t: &Theme) -> gpui::Div {
    let zone = div()
        .absolute()
        .bg(t.accent.opacity(0.22))
        .border_1()
        .border_color(t.accent);
    match side {
        Direction::Left => zone.top_0().bottom_0().left_0().w(relative(0.5)),
        Direction::Right => zone.top_0().bottom_0().right_0().w(relative(0.5)),
        Direction::Up => zone.left_0().right_0().top_0().h(relative(0.5)),
        Direction::Down => zone.left_0().right_0().bottom_0().h(relative(0.5)),
    }
}

/// The window's pane actions: split, resize, move focus, close, equalize.
pub(super) fn pane_actions(root: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    use Direction::{Down, Left, Right, Up};
    // A divider drag can end anywhere, so its hold on the remote's size ends with any mouse up.
    root.on_mouse_up(
        gpui::MouseButton::Left,
        cx.listener(|s, _, _, cx| s.hold_pane_resizes(false, cx)),
    )
    .on_mouse_up_out(
        gpui::MouseButton::Left,
        cx.listener(|s, _, _, cx| s.hold_pane_resizes(false, cx)),
    )
    .on_action(cx.listener(|s, _: &SplitRight, w, cx| s.split_pane(Right, w, cx)))
    .on_action(cx.listener(|s, _: &SplitDown, w, cx| s.split_pane(Down, w, cx)))
    .on_action(cx.listener(|s, _: &SplitRightWithHost, w, cx| s.pick_host_for_split(Right, w, cx)))
    .on_action(cx.listener(|s, _: &SplitDownWithHost, w, cx| s.pick_host_for_split(Down, w, cx)))
    .on_action(cx.listener(|s, _: &ResizePaneLeft, _, cx| s.step_pane(Left, cx)))
    .on_action(cx.listener(|s, _: &ResizePaneRight, _, cx| s.step_pane(Right, cx)))
    .on_action(cx.listener(|s, _: &ResizePaneUp, _, cx| s.step_pane(Up, cx)))
    .on_action(cx.listener(|s, _: &ResizePaneDown, _, cx| s.step_pane(Down, cx)))
    .on_action(cx.listener(|s, _: &EqualizePanes, _, cx| s.equalize_panes(cx)))
    .on_action(cx.listener(|s, _: &ClosePane, w, cx| s.close_pane_or_tab(w, cx)))
    .on_action(cx.listener(|s, _: &FocusPaneLeft, w, cx| s.move_pane_focus(Left, w, cx)))
    .on_action(cx.listener(|s, _: &FocusPaneRight, w, cx| s.move_pane_focus(Right, w, cx)))
    .on_action(cx.listener(|s, _: &FocusPaneUp, w, cx| s.move_pane_focus(Up, w, cx)))
    .on_action(cx.listener(|s, _: &FocusPaneDown, w, cx| s.move_pane_focus(Down, w, cx)))
}
