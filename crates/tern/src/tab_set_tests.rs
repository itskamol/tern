//! Tab set rules, driven the way the window drives them: split, drop, resize, move focus,
//! close. The pane is a stand-in and the store a recorder.
#![allow(clippy::unwrap_used)]

use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::split::{Floor, Rect};

#[derive(Debug)]
struct P(PaneId, &'static str);

impl PartialEq for P {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Paned for P {
    fn pane_id(&self) -> PaneId {
        self.0
    }

    fn host(&self) -> &str {
        self.1
    }

    /// Ids from 1000 up stand for a Files tab's pane.
    fn is_session(&self) -> bool {
        self.0 < 1000
    }
}

/// Records every save as "alias,alias|active".
#[derive(Clone, Default)]
struct Saves(Rc<RefCell<Vec<String>>>);

impl TabStore for Saves {
    fn save(&self, tabs: &SavedTabs) {
        let names: Vec<_> = tabs
            .tabs
            .iter()
            .map(|t| match &t.kind {
                SavedKind::Ssh { alias } => alias.clone(),
                SavedKind::Unknown => "?".into(),
            })
            .collect();
        self.0
            .borrow_mut()
            .push(format!("{}|{}", names.join(","), tabs.active));
    }
}

impl Saves {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.borrow_mut())
    }
}

/// Every pane `host-N` for its own id, so a title says which pane it came from.
fn pane(id: PaneId) -> P {
    P(id, "host")
}

fn set(names: &[&str]) -> (TabSet<P>, Saves) {
    let saves = Saves::default();
    let mut tabs = TabSet::new(Box::new(saves.clone()), true);
    for name in names {
        let id = tabs.new_pane_id();
        tabs.open((*name).into(), P(id, "host"));
    }
    saves.take();
    (tabs, saves)
}

fn aliases(tabs: &TabSet<P>) -> Vec<&str> {
    tabs.iter().map(|t| t.alias.as_str()).collect()
}

/// A tab 100 columns wide and 20 rows high: a pane may not take under 20% of its width or
/// 25% of its height.
fn floor() -> Floor {
    Floor::cells(100.0, 20.0)
}

fn near(a: f32, b: f32) {
    assert!((a - b).abs() < 1e-4, "{a} is not {b}");
}

fn rect(tabs: &TabSet<P>, id: PaneId) -> Rect {
    let (_, r) = *tabs
        .active_tab()
        .unwrap()
        .layout()
        .iter()
        .find(|(p, _)| *p == id)
        .unwrap();
    r
}

/// One tab, panes 1 and 2 side by side; 2 has the keyboard.
fn side_by_side() -> TabSet<P> {
    let (mut tabs, _) = set(&["a"]);
    let id = tabs.new_pane_id();
    tabs.split(1, Direction::Right, pane(id), floor()).unwrap();
    tabs
}

/// Pane 1 on the left; 2 top right; 3 bottom right.
fn l_shape() -> TabSet<P> {
    let mut tabs = side_by_side();
    let id = tabs.new_pane_id();
    tabs.split(2, Direction::Down, pane(id), floor()).unwrap();
    tabs
}

#[test]
fn closing_keeps_the_front_tab_in_front_or_falls_back_to_the_one_before() {
    let (mut tabs, saves) = set(&["a", "b", "c", "d"]);
    tabs.activate(2);
    // A tab before the front one: the front tab moves down a slot with it.
    tabs.close(0);
    assert_eq!((aliases(&tabs), tabs.active()), (vec!["b", "c", "d"], 1));
    // A tab after it: nothing moves.
    tabs.close(2);
    assert_eq!((aliases(&tabs), tabs.active()), (vec!["b", "c"], 1));
    // The front tab, last in the strip: the one before comes to the front.
    tabs.close(1);
    assert_eq!((aliases(&tabs), tabs.active()), (vec!["b"], 0));
    tabs.close(0);
    assert!(tabs.is_empty());
    assert_eq!(tabs.active(), 0);
    assert!(tabs.close(0).is_none());
    assert_eq!(
        saves.take(),
        ["a,b,c,d|2", "b,c,d|1", "b,c|1", "b|0", "|0"],
        "one save per change"
    );
}

#[test]
fn closing_the_front_tab_first_in_the_strip_brings_the_next() {
    let (mut tabs, _) = set(&["a", "b", "c"]);
    tabs.activate(0);
    tabs.close(0);
    assert_eq!((aliases(&tabs), tabs.active()), (vec!["b", "c"], 0));
}

#[test]
fn close_others_saves_once() {
    let (mut tabs, saves) = set(&["a", "b", "c"]);
    let closed = tabs.close_others(1);
    assert_eq!(closed.len(), 2);
    assert_eq!((aliases(&tabs), tabs.active()), (vec!["b"], 0));
    assert_eq!(saves.take(), ["b|0"]);
}

#[test]
fn a_restore_saves_once_and_only_whole() {
    let saves = Saves::default();
    let mut tabs: TabSet<P> = TabSet::new(Box::new(saves.clone()), true);
    let restored: Vec<_> = ["a", "b", "c"]
        .into_iter()
        .map(|n| (n.to_owned(), None, pane(tabs.new_pane_id())))
        .collect();
    tabs.restore(restored, 9);
    assert_eq!(tabs.active(), 2, "an out-of-range slot is the last tab");
    assert_eq!(saves.take(), ["a,b,c|2"]);
    tabs.restore(Vec::new(), 0);
    assert!(saves.take().is_empty());
}

#[test]
fn nothing_is_saved_with_reopen_tabs_off_and_turning_it_on_saves_now() {
    let saves = Saves::default();
    let mut tabs: TabSet<P> = TabSet::new(Box::new(saves.clone()), false);
    let id = tabs.new_pane_id();
    tabs.open("a".into(), pane(id));
    tabs.set_title(0, Some("prod".into()));
    assert!(saves.take().is_empty());
    tabs.set_saving(true);
    assert_eq!(saves.take(), ["a|0"]);
    tabs.set_saving(true);
    assert!(saves.take().is_empty(), "already on: nothing new to save");
    assert_eq!(tabs[0].title(), Some("prod"));
}

#[test]
fn activating_the_front_tab_again_or_an_unchanged_title_saves_nothing() {
    let (mut tabs, saves) = set(&["a", "b"]);
    assert!(tabs.activate(1));
    assert!(!tabs.activate(5));
    tabs.set_title(0, None);
    assert!(saves.take().is_empty());
    tabs.reorder(0, 1);
    assert_eq!((aliases(&tabs), tabs.active()), (vec!["b", "a"], 0));
    assert_eq!(saves.take(), ["b,a|0"]);
}

#[test]
fn a_split_focuses_the_new_pane_and_closing_it_gives_the_keyboard_back() {
    let (mut tabs, _) = set(&["a"]);
    let first = tabs[0].focused();
    assert!(tabs.can_split(first, Direction::Right, floor()));
    let second = tabs.new_pane_id();
    tabs.split(first, Direction::Right, pane(second), floor())
        .unwrap();
    assert_eq!(tabs[0].focused(), second);
    assert_eq!(tabs[0].tree().leaves(), [first, second]);
    assert!(tabs.focus_pane(first));
    assert!(!tabs.focus_pane(first), "already focused");
    assert!(!tabs.focus_pane(99), "not in this tab");
    // The first pane closes: the one after it takes the keyboard.
    assert_eq!(tabs.close_pane(), Some(pane(first)));
    assert_eq!(tabs[0].focused(), second);
    assert_eq!(tabs[0].tree().leaves(), [second]);
    assert_eq!(tabs[0].panes(), [pane(second)]);
    assert_eq!(tabs.close_pane(), None, "the last pane closes with its tab");
}

#[test]
fn a_split_with_no_tab_hands_the_pane_back() {
    let mut tabs: TabSet<P> = TabSet::new(Box::new(Saves::default()), true);
    assert!(!tabs.can_split(1, Direction::Down, floor()));
    assert_eq!(
        tabs.split(1, Direction::Down, pane(1), floor()),
        Err(pane(1))
    );
}

#[test]
fn a_split_opens_half_and_half() {
    let tabs = side_by_side();
    near(rect(&tabs, 1).w, 0.5);
    near(rect(&tabs, 2).w, 0.5);
    near(rect(&tabs, 2).x, 0.5);
}

#[test]
fn a_divider_gives_each_side_its_share() {
    let mut tabs = side_by_side();
    assert!(tabs.resize(1, Direction::Right, 0.7, floor()));
    near(rect(&tabs, 1).w, 0.7);
    near(rect(&tabs, 2).w, 0.3);
    near(rect(&tabs, 2).x, 0.7);
}

#[test]
fn a_divider_is_reached_from_either_pane_beside_it() {
    let mut tabs = side_by_side();
    // The first child's share is what the ratio means, whichever pane names the divider.
    assert!(tabs.resize(2, Direction::Left, 0.25, floor()));
    near(rect(&tabs, 1).w, 0.25);
    assert!(
        !tabs.resize(1, Direction::Left, 0.25, floor()),
        "no divider on the tab's edge"
    );
    assert!(
        !tabs.resize(1, Direction::Down, 0.25, floor()),
        "no divider below"
    );
    near(rect(&tabs, 1).w, 0.25);
}

#[test]
fn a_pane_stops_shrinking_at_twenty_columns_and_five_rows() {
    let mut tabs = side_by_side();
    tabs.resize(1, Direction::Right, 0.99, floor());
    near(rect(&tabs, 2).w, 0.2);
    tabs.resize(1, Direction::Right, 0.01, floor());
    near(rect(&tabs, 1).w, 0.2);

    let (mut tabs, _) = set(&["a"]);
    let id = tabs.new_pane_id();
    tabs.split(1, Direction::Down, pane(id), floor()).unwrap();
    tabs.resize(1, Direction::Down, 0.99, floor());
    near(rect(&tabs, 2).h, 0.25);
}

#[test]
fn the_minimum_holds_for_every_pane_in_a_subtree() {
    // 2 and 3 share the right side, so it may not shrink under two panes' worth.
    let mut tabs = side_by_side();
    let id = tabs.new_pane_id();
    tabs.split(2, Direction::Right, pane(id), floor()).unwrap();
    tabs.resize(1, Direction::Right, 0.95, floor());
    near(rect(&tabs, 1).w, 0.6);
    near(rect(&tabs, 2).w, 0.2);
    near(rect(&tabs, 3).w, 0.2);
    // Pane 1 can shrink to its own minimum, not the subtree's.
    tabs.resize(1, Direction::Right, 0.0, floor());
    near(rect(&tabs, 1).w, 0.2);
}

#[test]
fn the_arrow_moves_the_divider_beside_the_focused_pane_five_percent() {
    let mut tabs = side_by_side();
    // Pane 2 has the keyboard; its divider is on its left.
    assert!(tabs.step_focused(Direction::Left, floor()));
    near(rect(&tabs, 1).w, 0.45);
    assert!(tabs.step_focused(Direction::Right, floor()));
    assert!(tabs.step_focused(Direction::Right, floor()));
    near(rect(&tabs, 1).w, 0.55);
    assert!(tabs.focus_pane(1));
    assert!(tabs.step_focused(Direction::Right, floor()));
    near(rect(&tabs, 1).w, 0.6);
}

#[test]
fn a_step_with_no_divider_that_way_changes_nothing() {
    let mut tabs = side_by_side();
    assert!(!tabs.step_focused(Direction::Up, floor()));
    assert!(!tabs.step_focused(Direction::Down, floor()));
    near(rect(&tabs, 1).w, 0.5);
    let (mut single, _) = set(&["a"]);
    assert!(!single.step_focused(Direction::Left, floor()));
}

#[test]
fn steps_stop_at_the_minimum() {
    let mut tabs = side_by_side();
    for _ in 0..20 {
        tabs.step_focused(Direction::Left, floor());
    }
    near(rect(&tabs, 1).w, 0.2);
}

#[test]
fn a_divider_resets_to_half_and_half() {
    let mut tabs = l_shape();
    tabs.resize(1, Direction::Right, 0.8, floor());
    tabs.resize(2, Direction::Down, 0.3, floor());
    assert!(tabs.reset_divider(1, Direction::Right, floor()));
    near(rect(&tabs, 1).w, 0.5);
    near(rect(&tabs, 2).h, 0.3);
}

#[test]
fn equalize_resets_every_split_in_the_tab() {
    let mut tabs = l_shape();
    tabs.resize(1, Direction::Right, 0.8, floor());
    tabs.resize(2, Direction::Down, 0.3, floor());
    tabs.equalize(floor());
    near(rect(&tabs, 1).w, 0.5);
    near(rect(&tabs, 2).h, 0.5);
    near(rect(&tabs, 3).h, 0.5);
}

#[test]
fn a_new_pane_goes_on_the_side_of_the_pane_it_was_dropped_on() {
    // Wide enough for five panes across.
    let roomy = Floor::cells(300.0, 60.0);
    let mut tabs = side_by_side();
    // Pane 1 is not the focused pane, yet it is the one split.
    assert_eq!(tabs[0].focused(), 2);
    let left = tabs.new_pane_id();
    tabs.split(1, Direction::Left, pane(left), roomy).unwrap();
    assert_eq!(tabs[0].tree().leaves(), [left, 1, 2]);
    assert_eq!(tabs[0].focused(), left);
    near(rect(&tabs, left).w, 0.25);
    near(rect(&tabs, 1).w, 0.25);

    let above = tabs.new_pane_id();
    tabs.split(2, Direction::Up, pane(above), roomy).unwrap();
    assert_eq!(tabs[0].tree().leaves(), [left, 1, above, 2]);
    near(rect(&tabs, above).y, 0.0);
    near(rect(&tabs, 2).y, 0.5);

    let right = tabs.new_pane_id();
    tabs.split(1, Direction::Right, pane(right), roomy).unwrap();
    assert_eq!(tabs[0].tree().leaves(), [left, 1, right, above, 2]);
    let below = tabs.new_pane_id();
    tabs.split(above, Direction::Down, pane(below), roomy)
        .unwrap();
    assert_eq!(tabs[0].tree().leaves(), [left, 1, right, above, below, 2]);
}

#[test]
fn a_split_that_would_leave_a_pane_too_small_makes_no_pane() {
    // 100 columns: four panes of 25 fit, and halving one of them leaves 12.5.
    let mut tabs = side_by_side();
    let third = tabs.new_pane_id();
    tabs.split(1, Direction::Right, pane(third), floor())
        .unwrap();
    let fourth = tabs.new_pane_id();
    tabs.split(2, Direction::Left, pane(fourth), floor())
        .unwrap();
    near(rect(&tabs, 1).w, 0.25);
    assert!(!tabs.can_split(1, Direction::Right, floor()));
    assert!(!tabs.can_split(1, Direction::Left, floor()));
    let count = tabs[0].panes().len();
    let extra = tabs.new_pane_id();
    assert_eq!(
        tabs.split(1, Direction::Left, pane(extra), floor()),
        Err(pane(extra))
    );
    assert_eq!(tabs[0].panes().len(), count);
    // Rows still have room: 20 rows halve to 10.
    assert!(tabs.can_split(1, Direction::Down, floor()));
    assert!(
        !tabs.can_split(99, Direction::Down, floor()),
        "not in this tab"
    );
}

#[test]
fn a_pane_that_was_resized_small_cannot_be_split() {
    let mut tabs = side_by_side();
    tabs.resize(1, Direction::Right, 0.3, floor());
    assert!(!tabs.can_split(1, Direction::Right, floor()));
    assert!(tabs.can_split(1, Direction::Down, floor()));
    assert!(tabs.can_split(2, Direction::Right, floor()));
}

#[test]
fn the_title_follows_the_focused_panes_host_and_counts_the_others() {
    let (mut tabs, _) = set(&["a"]);
    let one = tabs.active_tab().unwrap().heading();
    assert_eq!((one.text.as_str(), one.others), ("host", 0));

    let id = tabs.new_pane_id();
    tabs.split(1, Direction::Right, P(id, "db"), floor())
        .unwrap();
    let two = tabs.active_tab().unwrap().heading();
    assert_eq!((two.text.as_str(), two.others), ("db", 1));

    tabs.focus_pane(1);
    assert_eq!(tabs.active_tab().unwrap().heading().text, "host");

    let id = tabs.new_pane_id();
    tabs.split(1, Direction::Down, P(id, "cache"), floor())
        .unwrap();
    let three = tabs.active_tab().unwrap().heading();
    assert_eq!((three.text.as_str(), three.others), ("cache", 2));

    tabs.close_pane();
    assert_eq!(tabs.active_tab().unwrap().heading().others, 1);
}

#[test]
fn a_name_the_person_gave_wins_over_the_focused_host() {
    let (mut tabs, _) = set(&["a"]);
    tabs.set_title(0, Some("prod".into()));
    let id = tabs.new_pane_id();
    tabs.split(1, Direction::Right, P(id, "db"), floor())
        .unwrap();
    tabs.focus_pane(1);
    assert_eq!(tabs.active_tab().unwrap().heading().text, "prod");
    tabs.focus_pane(id);
    assert_eq!(tabs.active_tab().unwrap().heading().text, "prod");
    tabs.set_title(0, None);
    assert_eq!(tabs.active_tab().unwrap().heading().text, "db");
}

#[test]
fn what_a_tab_is_called_without_a_name_is_the_focused_panes_host() {
    let (mut tabs, _) = set(&["a"]);
    tabs.set_title(0, Some("prod".into()));
    let id = tabs.new_pane_id();
    tabs.split(1, Direction::Right, P(id, "db"), floor())
        .unwrap();
    let tab = tabs.active_tab().unwrap();
    assert_eq!(
        (tab.heading().text.as_str(), tab.untitled()),
        ("prod", "db")
    );
}

#[test]
fn what_is_saved_ignores_layout_and_extra_panes() {
    let (mut tabs, saves) = set(&["a"]);
    let id = tabs.new_pane_id();
    tabs.split(1, Direction::Right, P(id, "db"), floor())
        .unwrap();
    tabs.resize(1, Direction::Right, 0.7, floor());
    assert!(saves.take().is_empty(), "layout changes are not saved");
    assert_eq!(tabs.saved().tabs.len(), 1);
}

/// Panes 1, 2 and 3 side by side in one tab; 3 has the keyboard.
fn three_panes() -> TabSet<P> {
    let mut tabs = side_by_side();
    let id = tabs.new_pane_id();
    tabs.split(2, Direction::Right, pane(id), floor()).unwrap();
    tabs
}

fn asking(ids: &'static [PaneId]) -> impl Fn(&P) -> bool {
    move |p| ids.contains(&p.0)
}

#[test]
fn fill_goes_to_the_focused_pane_when_it_is_waiting_even_if_another_is_too() {
    let tabs = three_panes();
    assert_eq!(tabs.active_tab().unwrap().focused(), 3);
    assert_eq!(tabs.fill_target(asking(&[1, 3])), FillTarget::Pane(3));
}

#[test]
fn fill_goes_to_the_one_unfocused_pane_that_is_waiting() {
    let tabs = three_panes();
    assert_eq!(tabs.fill_target(asking(&[1])), FillTarget::Pane(1));
}

#[test]
fn fill_is_not_guessed_when_two_unfocused_panes_are_waiting() {
    let tabs = three_panes();
    assert_eq!(tabs.fill_target(asking(&[1, 2])), FillTarget::Several);
}

#[test]
fn fill_has_no_target_when_no_pane_is_waiting() {
    let tabs = three_panes();
    assert_eq!(tabs.fill_target(asking(&[])), FillTarget::None);
}

#[test]
fn fill_only_looks_at_the_front_tab() {
    let (mut tabs, _) = set(&["a", "b"]);
    // Tab "b" (pane 2) is in front; pane 1 is in the tab behind it.
    assert_eq!(tabs.fill_target(asking(&[1])), FillTarget::None);
    tabs.activate(0);
    assert_eq!(tabs.fill_target(asking(&[1])), FillTarget::Pane(1));
}

#[test]
fn a_files_tab_is_not_saved_and_the_saved_slot_skips_it() {
    let (mut tabs, saves) = set(&["a", "b"]);
    tabs.open("Files".into(), P(1000, "Files"));
    assert_eq!(
        saves.take(),
        ["a,b|1"],
        "in front, it is saved as the tab before it"
    );
    let id = tabs.new_pane_id();
    tabs.open("c".into(), P(id, "host"));
    assert_eq!(
        saves.take(),
        ["a,b,c|2"],
        "slot 3 here, the third of the saved"
    );
    tabs.activate(0);
    assert_eq!(saves.take(), ["a,b,c|0"]);
    tabs.activate(2);
    assert!(
        !tabs.can_split(1000, Direction::Down, floor()),
        "a Files tab does not split"
    );
    assert!(!tabs[2].is_session() && tabs[1].is_session());
}

#[test]
fn panes_read_left_to_right_and_top_to_bottom_whatever_order_they_were_made_in() {
    let (mut tabs, _) = set(&["a"]);
    let below = tabs.new_pane_id();
    tabs.split(1, Direction::Down, pane(below), floor())
        .unwrap();
    let right = tabs.new_pane_id();
    tabs.split(1, Direction::Right, pane(right), floor())
        .unwrap();
    let tab = tabs.active_tab().unwrap();
    let made: Vec<PaneId> = tab.panes().iter().map(|p| p.0).collect();
    let read: Vec<PaneId> = tab.in_reading_order().iter().map(|p| p.0).collect();
    assert_eq!(made, [1, below, right]);
    assert_eq!(read, [1, right, below]);
}

#[test]
fn the_focused_pane_s_place_in_reading_order_follows_the_keyboard() {
    let (mut tabs, _) = set(&["a"]);
    let below = tabs.new_pane_id();
    tabs.split(1, Direction::Down, pane(below), floor())
        .unwrap();
    let right = tabs.new_pane_id();
    tabs.split(1, Direction::Right, pane(right), floor())
        .unwrap();
    // Reading order is 1, right, below; the last split put the keyboard on `right`.
    assert_eq!(tabs.active_tab().unwrap().focus_in_reading_order(), 1);
    tabs.focus_pane(below);
    assert_eq!(tabs.active_tab().unwrap().focus_in_reading_order(), 2);
}

#[test]
fn zoom_shows_the_focused_pane_alone_and_unzoom_brings_the_split_back_as_it_was() {
    let mut tabs = side_by_side();
    let before = tabs.active_tab().unwrap().layout();
    assert!(tabs.toggle_zoom());
    assert_eq!(tabs.active_tab().unwrap().zoomed(), Some(2));
    assert_eq!(
        tabs.active_tab().unwrap().panes().len(),
        2,
        "every pane stays"
    );
    assert!(tabs.toggle_zoom());
    assert_eq!(tabs.active_tab().unwrap().zoomed(), None);
    assert_eq!(tabs.active_tab().unwrap().layout(), before);
}

#[test]
fn a_tab_of_one_pane_has_nothing_to_zoom() {
    let (mut tabs, _) = set(&["a"]);
    assert!(!tabs.toggle_zoom());
    assert_eq!(tabs.active_tab().unwrap().zoomed(), None);
}

#[test]
fn moving_focus_splitting_or_closing_a_pane_ends_the_zoom() {
    let mut tabs = side_by_side();
    tabs.toggle_zoom();
    assert!(tabs.focus_pane(1));
    assert_eq!(tabs.active_tab().unwrap().zoomed(), None);

    tabs.toggle_zoom();
    let id = tabs.new_pane_id();
    tabs.split(1, Direction::Down, pane(id), floor()).unwrap();
    assert_eq!(tabs.active_tab().unwrap().zoomed(), None);

    tabs.toggle_zoom();
    assert!(tabs.close_pane().is_some());
    assert_eq!(tabs.active_tab().unwrap().zoomed(), None);
}

#[test]
fn zoom_is_not_saved() {
    let mut tabs = side_by_side();
    let before = format!("{:?}", tabs.saved());
    tabs.toggle_zoom();
    assert_eq!(format!("{:?}", tabs.saved()), before);
}
