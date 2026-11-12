//! Dragging a host in the sidebar: what the drag carries, the line that shows where it will
//! land, and unfolding a collapsed group the drag rests on.

use std::cmp::Ordering;
use std::time::Duration;

use gpui::{
    BoxShadow, Context, DragMoveEvent, Global, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Stateful, StyleRefinement, Styled, Task, Window, div, px,
};

use crate::shell::Shell;

/// What a host drag carries: the connection it started from and what to draw under the
/// pointer.
#[derive(Clone)]
pub struct DraggedHost {
    pub(crate) ix: usize,
    pub(super) label: SharedString,
    pub(super) background: Hsla,
    pub(super) color: Hsla,
}

impl Render for DraggedHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(26.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(8.))
            .bg(self.background)
            .text_color(self.color)
            .text_size(px(13.))
            .shadow_md()
            .child(self.label.clone())
    }
}

const LINE: f32 = 2.;

/// Whether a host dragged from `from` lands above the row at `target`: it takes that row's
/// place, so one from further down is inserted before it and one from further up after it.
/// `None` over itself, where nothing moves.
fn lands_before(from: usize, target: usize) -> Option<bool> {
    match from.cmp(&target) {
        Ordering::Equal => None,
        Ordering::Greater => Some(true),
        Ordering::Less => Some(false),
    }
}

/// The drop line on the row at `target`: an inset shadow along its top or bottom edge, which
/// takes no room, so the list does not shift while the pointer moves over it.
pub(super) fn drop_line(
    mut style: StyleRefinement,
    from: usize,
    target: usize,
    color: Hsla,
) -> StyleRefinement {
    if let Some(before) = lands_before(from, target) {
        let y = if before { LINE } else { -LINE };
        style.box_shadow = Some(vec![BoxShadow::new(px(0.), px(y), color).inset()]);
    }
    style
}

/// How long a drag rests on a collapsed group before it opens, as Finder's folders do.
const SPRING_DELAY: Duration = Duration::from_millis(600);

/// What a pointer move over a group header asks of the pending unfold.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Arm,
    Disarm,
    Keep,
}

/// The group a drag is resting on and the timer that will open it. Dropping the timer cancels
/// it, so nothing runs while the pointer is elsewhere.
#[derive(Default)]
struct Spring {
    over: Option<String>,
    timer: Option<Task<()>>,
}

impl Global for Spring {}

impl Spring {
    fn step(&mut self, group: &str, over: bool) -> Step {
        let here = self.over.as_deref() == Some(group);
        match (over, here) {
            (true, false) => {
                self.over = Some(group.to_owned());
                Step::Arm
            }
            (false, true) => {
                self.over = None;
                self.timer = None;
                Step::Disarm
            }
            _ => Step::Keep,
        }
    }

    fn rests_on(&self, group: &str) -> bool {
        self.over.as_deref() == Some(group)
    }

    fn fired(&mut self) {
        self.over = None;
        // The timer is the task running this: let it finish instead of cancelling it.
        if let Some(timer) = self.timer.take() {
            timer.detach();
        }
    }
}

/// Opens the collapsed group `name` once a host drag has rested on its header for 600 ms.
/// Only collapsed headers carry it, and a pointer move is all that wakes it.
pub(super) fn spring_load(
    header: Stateful<gpui::Div>,
    name: String,
    cx: &mut Context<Shell>,
) -> Stateful<gpui::Div> {
    header.on_drag_move(cx.listener(
        move |_, e: &DragMoveEvent<DraggedHost>, _, cx: &mut Context<Shell>| {
            let over = e.bounds.contains(&e.event.position);
            if cx.default_global::<Spring>().step(&name, over) != Step::Arm {
                return;
            }
            let group = name.clone();
            let timer = cx.spawn(async move |shell, cx| {
                cx.background_executor().timer(SPRING_DELAY).await;
                shell
                    .update(cx, |shell, cx| {
                        let spring = cx.default_global::<Spring>();
                        let armed = spring.rests_on(&group);
                        spring.fired();
                        if armed && cx.has_active_drag() {
                            shell.toggle_group(&group, cx);
                        }
                    })
                    .ok();
            });
            cx.default_global::<Spring>().timer = Some(timer);
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_from_below_lands_above_the_row_and_one_from_above_lands_below() {
        assert_eq!(lands_before(5, 2), Some(true));
        assert_eq!(lands_before(1, 2), Some(false));
        assert_eq!(lands_before(2, 2), None);
    }

    #[test]
    fn over_itself_draws_no_line() {
        let style = drop_line(StyleRefinement::default(), 3, 3, gpui::black());
        assert!(style.box_shadow.is_none());
        let style = drop_line(StyleRefinement::default(), 4, 3, gpui::black());
        assert_eq!(style.box_shadow.map(|s| s.len()), Some(1));
    }

    #[test]
    fn resting_on_a_header_arms_once_and_leaving_cancels() {
        let mut s = Spring::default();
        assert_eq!(s.step("prod", false), Step::Keep);
        assert_eq!(s.step("prod", true), Step::Arm);
        assert_eq!(s.step("prod", true), Step::Keep);
        // Another header's moves do not cancel this one.
        assert_eq!(s.step("dev", false), Step::Keep);
        assert!(s.rests_on("prod"));
        assert_eq!(s.step("prod", false), Step::Disarm);
        assert!(!s.rests_on("prod"));
    }

    #[test]
    fn moving_straight_to_the_next_header_hands_the_timer_over() {
        let mut s = Spring::default();
        s.step("a", true);
        assert_eq!(s.step("b", true), Step::Arm);
        assert_eq!(s.step("a", false), Step::Keep);
        assert!(s.rests_on("b"));
    }

    #[test]
    fn a_fired_spring_forgets_the_group() {
        let mut s = Spring::default();
        s.step("a", true);
        s.fired();
        assert!(!s.rests_on("a"));
        assert_eq!(s.step("a", true), Step::Arm);
    }
}
