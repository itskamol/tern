//! The strip's layout and scrolling, drawn in gpui's test window. The tabs are stand-ins with
//! the real size and inner layout (`pill_box`), and the strip is the real `frame`; the click,
//! drag and menu wiring needs a `Shell` and is not exercised here.
#![allow(clippy::unwrap_used)]

use gpui::prelude::*;
use gpui::{
    AnyWindowHandle, Bounds, Context, Entity, IntoElement, Modifiers, Pixels, Render, ScrollDelta,
    ScrollWheelEvent, TestAppContext, TouchPhase, VisualTestContext, Window, div, point, px, size,
};

use super::{StripScroll, frame, pill_box};

/// The titlebar as `chrome::render_titlebar` lays it out: buttons, tabs, a drag strip.
struct Bar {
    tabs: usize,
    active: usize,
    scroll: StripScroll,
}

impl Render for Bar {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let pills = (0..self.tabs)
            .map(|ix| {
                pill_box(ix)
                    .child(div().flex_none().size(px(6.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child("production-database"),
                    )
                    .child(div().flex_none().size(px(18.)))
                    .into_any_element()
            })
            .collect();
        let button = self.scroll.overflows().then(|| {
            div()
                .debug_selector(|| "overflow".into())
                .size(px(24.))
                .into_any_element()
        });
        let buttons = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(div().size(px(24.)))
            .child(div().size(px(24.)));
        div().size_full().flex().flex_col().child(
            div()
                .h(px(38.))
                .flex_none()
                .pt(px(4.))
                .pl(px(88.))
                .pr(px(8.))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(buttons)
                .child(frame(pills, button, &self.scroll, self.active))
                .child(
                    div()
                        .debug_selector(|| "drag".into())
                        .flex_1()
                        .min_w(px(48.))
                        .h_full(),
                ),
        )
    }
}

/// A 900 pt wide window, the smallest the app allows, holding a [`Bar`].
struct Rig {
    view: Entity<Bar>,
    window: AnyWindowHandle,
    scroll: StripScroll,
    cx: VisualTestContext,
}

impl Rig {
    fn open(cx: &mut TestAppContext, tabs: usize) -> Self {
        Self::open_at(cx, tabs, 0)
    }

    /// The window opens with `active` already asked for, as when tabs are restored at launch, and
    /// is then sized: the first draw is of a larger window than the app will keep.
    fn open_at(cx: &mut TestAppContext, tabs: usize, active: usize) -> Self {
        let scroll = StripScroll::default();
        scroll.reveal(active);
        let handle = cx.add_window({
            let scroll = scroll.clone();
            move |_, _| Bar {
                tabs,
                active,
                scroll,
            }
        });
        let window: AnyWindowHandle = handle.into();
        let mut vcx = VisualTestContext::from_window(window, cx);
        let mut rig = Self {
            view: handle.root(&mut vcx).unwrap(),
            window,
            scroll,
            cx: vcx,
        };
        rig.cx.simulate_resize(size(px(900.), px(600.)));
        rig.settle();
        rig
    }

    /// Draws, then plays the next-frame requests the draw made, until a frame makes none: the
    /// test window has no frame loop of its own. A strip that kept asking would redraw at
    /// idle, which the cap catches. Returns how many frames the strip asked for.
    fn settle(&mut self) -> usize {
        let window = self.window;
        let mut asked = 0;
        for _ in 0..4 {
            let requests = self
                .cx
                .update_window(window, |_, w, cx| {
                    w.draw(cx).clear(cx);
                    w.simulate_next_frame(cx)
                })
                .unwrap();
            if requests == 0 {
                return asked;
            }
            asked += requests;
        }
        assert_eq!(asked, 0, "the strip kept asking for another frame");
        asked
    }

    /// Changes the tabs and settles; returns how many frames the strip asked for on its own,
    /// which is what the button's appearing or going costs.
    fn set(&mut self, tabs: usize, active: usize) -> usize {
        self.view.update(&mut self.cx, |bar, cx| {
            bar.tabs = tabs;
            bar.active = active;
            cx.notify();
        });
        self.scroll.reveal(active);
        self.settle()
    }

    fn button(&mut self) -> Option<Bounds<Pixels>> {
        self.cx.debug_bounds("overflow")
    }

    /// Where tab `ix` is on screen, scrolling included.
    fn tab(&self, ix: usize) -> Bounds<Pixels> {
        let mut b = self.scroll.handle.bounds_for_item(ix).unwrap();
        b.origin += self.scroll.handle.offset();
        b
    }

    fn wheel(&mut self, delta: ScrollDelta) {
        let at = point(px(300.), px(20.));
        self.cx.simulate_mouse_move(at, None, Modifiers::default());
        self.cx.simulate_event(ScrollWheelEvent {
            position: at,
            delta,
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
    }

    fn in_view(&self, ix: usize) -> bool {
        let (tab, strip) = (self.tab(ix), self.scroll.handle.bounds());
        tab.left() >= strip.left() && tab.right() <= strip.right()
    }
}

#[gpui::test]
fn a_few_tabs_fit_and_show_no_button(cx: &mut TestAppContext) {
    let mut rig = Rig::open(cx, 3);
    assert!(!rig.scroll.overflows());
    assert!(rig.button().is_none());
    assert_eq!(
        rig.tab(0).size.width,
        px(200.),
        "tabs keep their full width"
    );
}

#[gpui::test]
fn many_tabs_stop_shrinking_at_72_and_scroll(cx: &mut TestAppContext) {
    let mut rig = Rig::open(cx, 20);
    assert!(
        rig.scroll.overflows(),
        "20 tabs of at least 72 cannot fit in 900"
    );
    assert_eq!(rig.tab(0).size.width, px(72.));
    let drag = rig.cx.debug_bounds("drag").unwrap();
    assert!(
        rig.scroll.handle.bounds().right() <= drag.left(),
        "the strip stays inside the window and leaves the drag strip its room"
    );
    assert!(drag.size.width >= px(48.));
}

#[gpui::test]
fn the_button_shows_while_the_tabs_overflow_and_goes_when_they_fit(cx: &mut TestAppContext) {
    let mut rig = Rig::open(cx, 20);
    let button = rig.button().unwrap();
    assert!(
        rig.scroll.handle.bounds().right() <= button.left(),
        "the button sits after the strip, not over a tab"
    );
    assert_eq!(rig.set(4, 0), 1, "one more frame to take the button away");
    assert!(rig.button().is_none());
    assert!(!rig.scroll.overflows());
    assert_eq!(rig.set(4, 1), 0, "and none once it is settled");
    assert_eq!(rig.set(20, 0), 1, "one more frame to bring it back");
    assert!(rig.button().is_some());
    assert_eq!(rig.set(20, 5), 0);
}

#[gpui::test]
fn the_button_appearing_does_not_hide_the_active_tab(cx: &mut TestAppContext) {
    let mut rig = Rig::open(cx, 3);
    for n in 4..=24 {
        rig.set(n, n - 1);
        assert!(rig.in_view(n - 1), "tab {} of {n} is out of view", n - 1);
    }
    assert!(rig.button().is_some());
}

#[gpui::test]
fn the_active_tab_is_brought_into_view_from_either_side(cx: &mut TestAppContext) {
    let mut rig = Rig::open(cx, 20);
    for active in [19, 0, 10, 3, 17, 19, 1] {
        rig.set(20, active);
        assert!(rig.in_view(active), "tab {active} is out of view");
    }
}

#[gpui::test]
fn closing_tabs_keeps_the_active_one_in_view(cx: &mut TestAppContext) {
    let mut rig = Rig::open(cx, 20);
    rig.set(20, 19);
    for n in (5..20).rev() {
        rig.set(n, n - 1);
        assert!(rig.in_view(n - 1), "tab {} of {n} is out of view", n - 1);
    }
}

#[gpui::test]
fn a_mouse_wheel_and_a_trackpad_swipe_both_scroll_sideways(cx: &mut TestAppContext) {
    let mut rig = Rig::open(cx, 20);
    let start = rig.scroll.handle.offset().x;
    rig.wheel(ScrollDelta::Lines(point(0., -3.)));
    let wheel = rig.scroll.handle.offset().x;
    assert!(wheel < start, "a vertical wheel moves the row");
    rig.wheel(ScrollDelta::Pixels(point(px(-40.), px(0.))));
    assert!(
        rig.scroll.handle.offset().x < wheel,
        "a horizontal swipe moves the row"
    );
    rig.wheel(ScrollDelta::Pixels(point(px(5000.), px(0.))));
    assert_eq!(
        rig.scroll.handle.offset().x,
        px(0.),
        "it stops at the first tab"
    );
    rig.wheel(ScrollDelta::Pixels(point(px(-5000.), px(0.))));
    assert_eq!(
        rig.scroll.handle.offset().x,
        -rig.scroll.handle.max_offset().x,
        "and at the last"
    );
}

#[gpui::test]
fn a_restored_tab_is_in_view_once_the_window_has_its_size(cx: &mut TestAppContext) {
    for active in [0, 3, 10, 19] {
        let rig = Rig::open_at(cx, 20, active);
        assert!(rig.in_view(active), "restored tab {active} is out of view");
    }
}

#[gpui::test]
fn shrinking_the_window_keeps_the_active_tab_in_view(cx: &mut TestAppContext) {
    let mut rig = Rig::open(cx, 20);
    rig.set(20, 12);
    for width in [800., 700., 640.] {
        rig.cx.simulate_resize(size(px(width), px(600.)));
        rig.settle();
        assert!(rig.in_view(12), "tab 12 is out of view at {width} wide");
    }
}
