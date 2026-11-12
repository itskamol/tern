// Adapted from zeron crates/ui/src/shell.rs (JumpSession slot action, titlebar group rhythm) (MIT).
//! Session tabs in the titlebar strip and the shortcuts that move between them.

use crate::hover::Accessible as _;
use crate::hover::HoverFade as _;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    Action, Animation, AnimationExt, AnyElement, AppContext, Context, ElementId, Entity,
    FontWeight, Hsla, InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement,
    Pixels, Render, Role, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window,
    actions, div, pulsating_between, px,
};

use crate::icons::{self, icon};
use crate::indicator::Dot;
use crate::shell::Shell;
use crate::tab_set::Heading;
use crate::text_input::TextInput;
use crate::theme::{CONTROL_RADIUS, Theme};

actions!(tern, [CloseTab, NextTab, PrevTab]);

/// Activate the tab at a zero-based slot: one action carrying the slot, as zeron's
/// `JumpSession`, rather than nine near-identical actions.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = tern, no_json)]
pub struct ActivateTab(pub usize);

/// Focus the front tab's pane numbered `n` (1–9) in reading order: ⌥⌘ and the digit.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = tern, no_json)]
pub struct FocusPaneNumber(pub usize);

pub fn bindings() -> Vec<KeyBinding> {
    // ⌘W and ⌘⇧[ / ⌘⇧] are in the keymap; these second chords and the slots stay fixed.
    let mut b = vec![
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
    ];
    for slot in 0..9 {
        b.push(KeyBinding::new(
            // Alt+1..9 off macOS: plain Ctrl+digit is the shell's, and Super is the desktop's.
            &format!(
                "{}-{}",
                if cfg!(target_os = "macos") {
                    "cmd"
                } else {
                    "alt"
                },
                slot + 1
            ),
            ActivateTab(slot),
            None,
        ));
        b.push(KeyBinding::new(
            &crate::keymap::pane_slot(slot + 1),
            FocusPaneNumber(slot + 1),
            None,
        ));
    }
    b
}

/// What the strip needs to know about one tab.
pub struct TabInfo {
    pub alias: String,
    /// What the tab is called and how many other panes it holds.
    pub heading: Heading,
    /// The session's output is being written to a file.
    pub logging: bool,
    /// Typing here, or receiving what is typed in another tab.
    pub broadcast: bool,
    /// One per pane, in the tab's pane order: a dropped pane's is red even in a background
    /// tab.
    pub dots: Vec<Dot>,
    /// Which of `dots` is the focused pane's.
    pub front: usize,
    /// One pane fills the tab for now; the split comes back with ⇧⌘↩.
    pub zoomed: bool,
    /// The field that is editing this tab's title right now.
    pub rename: Option<Entity<TextInput>>,
}

impl TabInfo {
    pub fn label(&self) -> &str {
        &self.heading.text
    }

    /// The focused pane's dot: what the sidebar shows for the host.
    pub fn dot(&self) -> Dot {
        self.dots.get(self.front).copied().unwrap_or(Dot::Idle)
    }
}

/// A tab's dots from its panes, in order; a pane with no session (a Files tab) is idle.
pub fn tab_dots(panes: impl IntoIterator<Item = Option<Dot>>) -> Vec<Dot> {
    panes
        .into_iter()
        .map(|dot| dot.unwrap_or(Dot::Idle))
        .collect()
}

/// What a tab drag carries: the slot it started from and what to draw under the pointer.
#[derive(Clone)]
pub struct DraggedTab {
    pub ix: usize,
    label: SharedString,
    background: Hsla,
    color: Hsla,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(26.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(CONTROL_RADIUS))
            .bg(self.background)
            .text_color(self.color)
            .text_sm()
            .shadow_md()
            .child(self.label.clone())
    }
}

/// The strip's scroll position, and the tab it still owes the user a view of.
#[derive(Clone, Default)]
pub struct StripScroll {
    handle: ScrollHandle,
    /// The tab to bring into view and how many more tries it gets.
    pending: Rc<Cell<Option<(usize, u8)>>>,
    /// The tab in front, and the strip's width when it was last drawn: a strip that changed
    /// size, with the window or the tab count, owes that tab a view again.
    front: Rc<Cell<usize>>,
    width: Rc<Cell<Pixels>>,
}

impl StripScroll {
    /// Scrolls the least that shows tab `ix` whole, on the next frame.
    pub fn reveal(&self, ix: usize) {
        self.handle.scroll_to_item(ix);
        self.pending.set(Some((ix, 3)));
    }

    /// Whether the tabs are wider than the strip, as of the last frame drawn.
    fn overflows(&self) -> bool {
        self.handle.max_offset().x > px(0.)
    }

    /// Checks a requested reveal against the frame just painted; true when it needs another.
    /// gpui places the tab by the strip's size in the frame before, which is wrong on the
    /// first frame and whenever the strip changed size at the same time.
    fn check_reveal(&self) -> bool {
        let width = self.handle.bounds().size.width;
        if self.width.replace(width) != width && self.pending.get().is_none() {
            self.pending.set(Some((self.front.get(), 3)));
        }
        let Some((ix, tries)) = self.pending.take() else {
            return false;
        };
        let Some(mut tab) = self.handle.bounds_for_item(ix) else {
            return false;
        };
        tab.origin += self.handle.offset();
        let strip = self.handle.bounds();
        if tries == 0 || (tab.left() >= strip.left() && tab.right() <= strip.right()) {
            return false;
        }
        self.handle.scroll_to_item(ix);
        self.pending.set(Some((ix, tries - 1)));
        true
    }
}

/// Tabs shrink like a browser's (200 wide down to 72) and the strip scrolls sideways once
/// they no longer fit; `scroll` keeps the active one in view. Then a button at the end lists
/// every tab, since a tab scrolled out of sight is otherwise only a guess away.
pub fn strip(
    tabs: &[TabInfo],
    active: usize,
    scroll: &StripScroll,
    t: &Theme,
    cx: &mut Context<Shell>,
) -> impl IntoElement + use<> {
    let pills = tabs
        .iter()
        .enumerate()
        .map(|(ix, tab)| tab_pill(ix, tab, ix == active, t, cx).into_any_element())
        .collect();
    let menu = scroll.overflows().then(|| {
        crate::chrome::control("tab-overflow", t)
            .icon_button("Show all tabs", t)
            .on_click(cx.listener(|shell, e: &gpui::ClickEvent, _, cx| {
                shell.open_tabs_menu(e.position(), cx)
            }))
            .child(
                icon(icons::CHEVRON_RIGHT)
                    .size(px(16.))
                    .text_color(t.muted)
                    .with_transformation(gpui::Transformation::rotate(gpui::radians(
                        std::f32::consts::FRAC_PI_2,
                    ))),
            )
            .into_any_element()
    });
    frame(pills, menu, scroll, active)
}

/// The scrolling row, then a slot for the overflow button. The slot is always there, so the
/// button appearing moves nothing: the row keeps its width, and the active tab stays where
/// `reveal` put it. gpui measures overflow while drawing, after the button was chosen, so a
/// probe asks for one more frame when the button should have appeared or gone.
fn frame(
    pills: Vec<AnyElement>,
    menu: Option<AnyElement>,
    scroll: &StripScroll,
    active: usize,
) -> impl IntoElement + use<> {
    scroll.front.set(active);
    let shown = menu.is_some();
    let probed = scroll.clone();
    div()
        .flex()
        .items_center()
        .gap(px(2.))
        .min_w_0()
        .flex_shrink(1.)
        .child(
            div()
                .id("tab-strip")
                .role(Role::TabList)
                .aria_label("Sessions")
                .flex()
                .items_center()
                .gap(px(2.))
                .min_w_0()
                .flex_shrink(1.)
                .overflow_x_scroll()
                .track_scroll(&scroll.handle)
                .children(pills),
        )
        .child(div().flex_none().size(px(24.)).children(menu))
        .child(
            gpui::canvas(
                |_, _, _| (),
                move |_, (), window, _| {
                    // A notify now would be dropped: the window is mid-draw.
                    if probed.overflows() != shown || probed.check_reveal() {
                        window.request_animation_frame();
                    }
                },
            )
            .absolute()
            .size_0(),
        )
}

/// A tab's size and inner layout, apart from what it shows and does.
fn pill_box(ix: usize) -> gpui::Stateful<gpui::Div> {
    div()
        .id(("tab", ix))
        .group("tab")
        .h(px(26.))
        .flex_shrink(1.)
        .min_w(px(72.))
        .max_w(px(200.))
        .pl(px(10.))
        .pr(px(4.))
        .flex()
        .items_center()
        .gap(px(6.))
}

fn tab_pill(
    ix: usize,
    tab: &TabInfo,
    active: bool,
    t: &Theme,
    cx: &mut Context<Shell>,
) -> impl IntoElement + use<> {
    let dragged = DraggedTab {
        ix,
        label: SharedString::from(middle_ellipsis(tab.label(), 18)),
        background: t.popup,
        color: t.text,
    };
    let accent = t.accent;
    let renaming = tab.rename.clone();
    pill_box(ix)
        .role(Role::Tab)
        .aria_label(tab_label(tab))
        .aria_selected(active)
        .rounded(px(CONTROL_RADIUS))
        .cursor_pointer()
        .text_sm()
        .when(active, |el| el.bg(t.row_active).text_color(t.text))
        .when(!active, |el| {
            el.text_color(t.muted).hover_fade(
                format!("tab-{ix}"),
                gpui::transparent_black(),
                t.row_hover,
            )
        })
        // A click on a tab must not also start a window drag from the titlebar.
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |shell, e: &gpui::ClickEvent, window, cx| {
            // A click inside the title field is for the field, not for the tab.
            if shell.is_renaming(ix) {
                return;
            }
            if e.click_count() >= 2 {
                shell.start_rename(ix, window, cx);
            } else {
                shell.show_tab(ix, window, cx);
            }
        }))
        .on_drag(dragged, |tab, _, _, cx| cx.new(|_| tab.clone()))
        .drag_over::<DraggedTab>(move |style, _, _, _| style.bg(accent.opacity(0.25)))
        .on_drop(cx.listener(move |shell, from: &DraggedTab, window, cx| {
            shell.move_tab(from.ix, ix, window, cx)
        }))
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |shell, e: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                shell.open_tab_menu(ix, e.position, cx);
            }),
        )
        .on_mouse_up(
            MouseButton::Middle,
            cx.listener(move |shell, _, window, cx| shell.close_tab_at(ix, window, cx)),
        )
        .child(pane_dots(ix, &tab.dots, t))
        .child(match renaming {
            Some(input) => div()
                .flex_1()
                .min_w_0()
                .child(input)
                .on_mouse_down_out(cx.listener(|shell, _, window, cx| {
                    shell.commit_rename(window, cx);
                }))
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(active, |el| el.font_weight(FontWeight::MEDIUM))
                .child(SharedString::from(middle_ellipsis(tab.label(), 18)))
                .into_any_element(),
        })
        .when(tab.heading.others > 0, |el| {
            el.child(badge(format!("+{}", tab.heading.others).into(), t))
        })
        // The tab's other panes are still there, out of sight.
        .when(tab.zoomed, |el| el.child(badge("zoomed".into(), t)))
        .when(tab.broadcast, |el| {
            el.child(
                div()
                    .flex_none()
                    .px(px(5.))
                    .py(px(1.))
                    .rounded(px(3.))
                    .bg(t.accent.opacity(0.22))
                    .text_color(t.accent)
                    .text_size(px(10.))
                    .child("BCAST"),
            )
        })
        .when(tab.logging, |el| {
            el.child(
                div()
                    .flex_none()
                    .px(px(5.))
                    .py(px(1.))
                    .rounded(px(3.))
                    .bg(t.danger.opacity(0.18))
                    .text_color(t.danger)
                    .text_size(px(10.))
                    .child("LOG"),
            )
        })
        .child(
            div()
                .id(("close", ix))
                .icon_button("Close tab", t)
                .size(px(20.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .text_color(t.faint)
                .invisible()
                .group_hover("tab", |s| s.visible())
                .when(active, |el| el.visible())
                .hover_fade(
                    format!("tab-close-{ix}"),
                    gpui::transparent_black(),
                    t.row_hover,
                )
                .hover(|s| s.text_color(t.text))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |shell, _, window, cx| {
                    cx.stop_propagation();
                    shell.close_tab_at(ix, window, cx);
                }))
                .child("×"),
        )
}

/// How a dot is said aloud; a host that is not connected has nothing to add.
pub fn status_words(dot: Dot) -> Option<&'static str> {
    (dot != Dot::Idle).then(|| dot.label())
}

/// A tab as VoiceOver reads it: the title, then what the badges and the dot show.
fn tab_label(tab: &TabInfo) -> String {
    let dropped = tab
        .dots
        .iter()
        .enumerate()
        .filter(|(n, d)| *n != tab.front && **d == Dot::Closed)
        .count();
    let also = if tab.dot() == Dot::Closed {
        " more"
    } else {
        ""
    };
    let dropped_others = match dropped {
        0 => String::new(),
        1 => format!("1{also} pane disconnected"),
        n => format!("{n}{also} panes disconnected"),
    };
    let mut label = tab.label().to_owned();
    let more = format!("{} more panes", tab.heading.others);
    for (on, words) in [
        (tab.heading.others > 0, more.as_str()),
        (tab.zoomed, "zoomed"),
        (tab.broadcast, "broadcasting"),
        (tab.logging, "logging"),
        (true, status_words(tab.dot()).unwrap_or_default()),
        (!dropped_others.is_empty(), dropped_others.as_str()),
    ] {
        if on && !words.is_empty() {
            label.push_str(", ");
            label.push_str(words);
        }
    }
    label
}

/// zeron's `zeron-pulse` loader: 2.4 s, opacity 0.08 → 1.
const PULSE: Duration = Duration::from_millis(2400);

/// A small note on a tab's pill: how many more panes it holds, or that one is zoomed.
fn badge(text: SharedString, t: &Theme) -> impl IntoElement + use<> {
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(3.))
        .bg(t.row_active)
        .text_color(t.muted)
        .text_size(px(10.))
        .child(text)
}

/// zeron's status dot.
const DOT: f32 = 6.0;

/// The tab's dots side by side, half a dot apart.
fn pane_dots(ix: usize, dots: &[Dot], t: &Theme) -> impl IntoElement + use<> {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(DOT / 2.0))
        .children(
            dots.iter().enumerate().map(|(n, dot)| {
                status_dot(SharedString::from(format!("tab-dot-{ix}-{n}")), *dot, t)
            }),
        )
}

/// A connecting dot breathes; gpui holds it still when `App::reduce_motion` is set. Synced, so
/// a host's tab dot and sidebar dot pulse together. A dot waiting for an answer is a still ring.
pub fn status_dot(id: impl Into<ElementId>, dot: Dot, t: &Theme) -> AnyElement {
    let color = dot.color(t);
    let disc = div().flex_none().size(px(DOT)).rounded_full();
    let disc = if dot.hollow() {
        disc.border_1().border_color(color)
    } else {
        disc.bg(color)
    };
    if dot.pulses() {
        disc.with_animation(
            id,
            Animation::new(PULSE)
                .repeat_synced()
                .with_easing(pulsating_between(0.08, 1.0)),
            |disc, alpha| disc.opacity(alpha),
        )
        .into_any_element()
    } else {
        disc.into_any_element()
    }
}

/// Long names lose their middle, not their end: hosts named alike ("db-replica-01",
/// "db-replica-02") usually differ at the end, which plain truncation would cut.
pub fn middle_ellipsis(name: &str, max_chars: usize) -> String {
    let count = name.chars().count();
    if count <= max_chars || max_chars < 5 {
        return name.to_owned();
    }
    // A third for the head, the rest for the tail, where names like these differ.
    let keep = max_chars - 1;
    let head = keep / 3;
    let tail = keep - head;
    let start: String = name.chars().take(head).collect();
    let end: String = name.chars().skip(count - tail).collect();
    format!("{start}…{end}")
}

/// Moves the tab at `from` so it ends up at `to`, and returns where the active tab is now, so
/// the one in front stays in front whichever tab was dragged.
pub fn reorder<T>(items: &mut Vec<T>, active: usize, from: usize, to: usize) -> usize {
    if from >= items.len() || to >= items.len() || from == to {
        return active;
    }
    let item = items.remove(from);
    items.insert(to, item);
    if active == from {
        to
    } else if from < active && to >= active {
        active - 1
    } else if from > active && to <= active {
        active + 1
    } else {
        active
    }
}

/// The slot after `active` when moving by `delta`, wrapping at both ends.
pub fn step(active: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (active as isize + delta).rem_euclid(len as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::{Heading, TabInfo, reorder, step, tab_dots, tab_label};
    use crate::indicator::Dot;

    #[test]
    fn a_zoomed_tab_says_so_after_its_other_panes() {
        let mut tab = info(Dot::Connected);
        tab.heading.others = 2;
        tab.zoomed = true;
        assert!(tab_label(&tab).starts_with("web, 2 more panes, zoomed"));
    }

    #[test]
    fn a_tab_has_one_dot_per_pane_in_order_and_a_files_pane_is_idle() {
        assert_eq!(
            tab_dots([Some(Dot::Connected), Some(Dot::Closed), None]),
            [Dot::Connected, Dot::Closed, Dot::Idle]
        );
        assert_eq!(tab_dots([None]), [Dot::Idle]);
    }

    #[test]
    fn a_dropped_pane_behind_the_focused_one_is_said_and_red() {
        let mut tab = info(Dot::Connected);
        tab.dots = vec![Dot::Connected, Dot::Closed, Dot::Closed];
        assert_eq!(tab.dot(), Dot::Connected);
        assert!(tab_label(&tab).ends_with("Connected, 2 panes disconnected"));
        tab.front = 1;
        assert_eq!(tab.dot(), Dot::Closed);
        assert!(tab_label(&tab).ends_with("Disconnected, 1 more pane disconnected"));
    }

    fn info(dot: Dot) -> TabInfo {
        TabInfo {
            alias: "web".into(),
            heading: Heading {
                text: "web".into(),
                others: 0,
            },
            logging: false,
            broadcast: false,
            dots: vec![dot],
            front: 0,
            zoomed: false,
            rename: None,
        }
    }

    #[test]
    fn a_tab_is_read_as_title_then_badges_then_status() {
        assert_eq!(tab_label(&info(Dot::Idle)), "web");
        let mut tab = info(Dot::Connected);
        tab.heading.text = "deploy".into();
        assert_eq!(tab_label(&tab), "deploy, Connected");
        tab.heading.others = 2;
        assert_eq!(tab_label(&tab), "deploy, 2 more panes, Connected");
        tab.heading.others = 0;
        tab.broadcast = true;
        tab.logging = true;
        assert_eq!(tab_label(&tab), "deploy, broadcasting, logging, Connected");
    }

    /// Tabs named a..e, so a wrong move shows in the order and not only in an index.
    fn tabs() -> Vec<&'static str> {
        vec!["a", "b", "c", "d", "e"]
    }

    #[test]
    fn dragging_right_puts_the_tab_at_the_drop_slot() {
        let mut t = tabs();
        let active = reorder(&mut t, 4, 1, 3);
        assert_eq!(t, ["a", "c", "d", "b", "e"]);
        assert_eq!(active, 4, "e was active and did not move");
    }

    #[test]
    fn dragging_left_puts_the_tab_at_the_drop_slot() {
        let mut t = tabs();
        let active = reorder(&mut t, 0, 3, 1);
        assert_eq!(t, ["a", "d", "b", "c", "e"]);
        assert_eq!(active, 0);
    }

    #[test]
    fn first_and_last_slots() {
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 2, 4, 0), 3);
        assert_eq!(t, ["e", "a", "b", "c", "d"]);
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 2, 0, 4), 1);
        assert_eq!(t, ["b", "c", "d", "e", "a"]);
    }

    #[test]
    fn the_active_tab_follows_its_own_drag() {
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 1, 1, 3), 3);
        assert_eq!(t[3], "b");
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 3, 3, 0), 0);
        assert_eq!(t[0], "d");
    }

    #[test]
    fn another_tab_passing_the_active_one_shifts_it() {
        // c is active; a is dragged past it to the right.
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 2, 0, 3), 1);
        assert_eq!(t[1], "c");
        // c is active; e is dragged past it to the left.
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 2, 4, 1), 3);
        assert_eq!(t[3], "c");
        // Dropping exactly onto the active slot still passes it.
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 2, 0, 2), 1);
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 2, 4, 2), 3);
    }

    #[test]
    fn a_move_that_does_not_cross_the_active_tab_leaves_it() {
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 4, 0, 2), 4);
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 0, 4, 2), 0);
    }

    #[test]
    fn no_op_and_out_of_range_moves_change_nothing() {
        let mut t = tabs();
        assert_eq!(reorder(&mut t, 2, 3, 3), 2);
        assert_eq!(reorder(&mut t, 2, 9, 0), 2);
        assert_eq!(reorder(&mut t, 2, 0, 9), 2);
        assert_eq!(t, tabs());
    }

    #[test]
    fn long_names_keep_their_distinct_end() {
        use super::middle_ellipsis;
        assert_eq!(middle_ellipsis("short", 18), "short");
        let a = middle_ellipsis("production-database-replica-01", 18);
        let b = middle_ellipsis("production-database-replica-02", 18);
        assert_ne!(a, b);
        assert_eq!(a.chars().count(), 18);
        assert!(a.ends_with("replica-01"), "{a}");
    }

    #[test]
    fn stepping_wraps_both_ways() {
        assert_eq!(step(0, 3, 1), 1);
        assert_eq!(step(2, 3, 1), 0);
        assert_eq!(step(0, 3, -1), 2);
        assert_eq!(step(0, 0, 1), 0);
    }
}

#[cfg(test)]
#[path = "tabs_tests.rs"]
mod layout_tests;
