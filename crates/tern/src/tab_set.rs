//! The open tabs and their panes, kept consistent: which tab is in front, how each tab's panes
//! are laid out and which one has the keyboard, and the saved list (`tabs.json`), written once
//! for each change that alters it. Generic over the pane, so these rules are tested without
//! sessions or a window; the shell supplies real panes and does the focusing and drawing.

use std::ops::Index;

use crate::split::{Axis, Direction, Node, PaneId};
use crate::tabs::reorder;
use crate::tabs_store::{SavedKind, SavedTab, SavedTabs};

/// A tab's identity for as long as it is open; unlike its slot it survives reordering.
pub type TabId = u64;

/// What the tab set needs from a pane: its id in the layout tree.
pub trait Paned {
    fn pane_id(&self) -> PaneId;

    /// The pane is a session's terminal; the others (the Files tab) are not saved with the tab
    /// list and are not a host's tab.
    fn is_session(&self) -> bool {
        true
    }
}

/// Where the tab list is saved: `tabs.json` in the app, a recorder in tests.
pub trait TabStore {
    fn save(&self, tabs: &SavedTabs);
}

pub struct Tab<P> {
    pub id: TabId,
    pub alias: String,
    /// The user's name for the tab. It belongs to the tab, not the session, so a reconnect
    /// keeps it.
    title: Option<String>,
    /// How the panes are laid out; its leaves are exactly the panes' ids.
    tree: Node,
    /// The pane with the keyboard; always one of `panes`.
    focused: PaneId,
    panes: Vec<P>,
}

impl<P: Paned> Tab<P> {
    fn new(id: TabId, alias: String, title: Option<String>, pane: P) -> Self {
        let first = pane.pane_id();
        Self {
            id,
            alias,
            title,
            tree: Node::Leaf(first),
            focused: first,
            panes: vec![pane],
        }
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn tree(&self) -> &Node {
        &self.tree
    }

    pub fn focused(&self) -> PaneId {
        self.focused
    }

    pub fn panes(&self) -> &[P] {
        &self.panes
    }

    pub fn pane(&self, id: PaneId) -> Option<&P> {
        self.panes.iter().find(|p| p.pane_id() == id)
    }

    /// The focused pane: what the strip shows and what typing reaches.
    /// A host's tab, as against a Files tab.
    pub fn is_session(&self) -> bool {
        self.front().is_session()
    }

    pub fn front(&self) -> &P {
        // `focused` always names a pane and a tab always has one; the first is the fallback.
        self.pane(self.focused).unwrap_or(&self.panes[0])
    }
}

pub struct TabSet<P> {
    tabs: Vec<Tab<P>>,
    active: usize,
    next_tab: TabId,
    next_pane: PaneId,
    store: Box<dyn TabStore>,
    /// The "Reopen tabs" setting: off, nothing is saved.
    saving: bool,
}

impl<P: Paned> TabSet<P> {
    pub fn new(store: Box<dyn TabStore>, saving: bool) -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            next_tab: 0,
            next_pane: 0,
            store,
            saving,
        }
    }

    /// An id for a pane about to be made.
    pub fn new_pane_id(&mut self) -> PaneId {
        self.next_pane += 1;
        self.next_pane
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn get(&self, ix: usize) -> Option<&Tab<P>> {
        self.tabs.get(ix)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Tab<P>> {
        self.tabs.iter()
    }

    /// The slot of the tab in front; 0 when there are none.
    pub fn active(&self) -> usize {
        self.active
    }

    pub fn active_tab(&self) -> Option<&Tab<P>> {
        self.tabs.get(self.active)
    }

    /// Opens a tab with one pane and brings it to the front.
    pub fn open(&mut self, alias: String, pane: P) -> usize {
        self.next_tab += 1;
        self.tabs.push(Tab::new(self.next_tab, alias, None, pane));
        self.active = self.tabs.len() - 1;
        self.changed();
        self.active
    }

    /// The last run's tabs, all at once, so a half-restored list is never saved.
    pub fn restore(&mut self, tabs: Vec<(String, Option<String>, P)>, active: usize) {
        if tabs.is_empty() {
            return;
        }
        for (alias, title, pane) in tabs {
            self.next_tab += 1;
            self.tabs.push(Tab::new(self.next_tab, alias, title, pane));
        }
        self.active = active.min(self.tabs.len() - 1);
        self.changed();
    }

    /// Brings tab `ix` to the front. False when there is no such tab.
    pub fn activate(&mut self, ix: usize) -> bool {
        if ix >= self.tabs.len() {
            return false;
        }
        if ix != self.active {
            self.active = ix;
            self.changed();
        }
        true
    }

    /// Closes tab `ix` and hands it back (dropping it ends its sessions). The tab in front
    /// stays in front; closing the front tab brings the one before it, or the next for the
    /// first.
    pub fn close(&mut self, ix: usize) -> Option<Tab<P>> {
        if ix >= self.tabs.len() {
            return None;
        }
        let closed = self.tabs.remove(ix);
        if self.active > ix || (self.active == self.tabs.len() && self.active > 0) {
            self.active -= 1;
        }
        self.changed();
        Some(closed)
    }

    /// Keeps tab `keep` only, in front, and hands the others back.
    pub fn close_others(&mut self, keep: usize) -> Vec<Tab<P>> {
        if keep >= self.tabs.len() {
            return Vec::new();
        }
        let kept = self.tabs.remove(keep);
        let others = std::mem::replace(&mut self.tabs, vec![kept]);
        self.active = 0;
        self.changed();
        others
    }

    /// Moves the tab at `from` to `to`; the tab in front stays in front.
    pub fn reorder(&mut self, from: usize, to: usize) {
        let active = reorder(&mut self.tabs, self.active, from, to);
        if from != to && from < self.tabs.len() && to < self.tabs.len() {
            self.active = active;
            self.changed();
        }
    }

    pub fn set_title(&mut self, ix: usize, title: Option<String>) {
        if let Some(tab) = self.tabs.get_mut(ix)
            && tab.title != title
        {
            tab.title = title;
            self.changed();
        }
    }

    /// The front tab's focused pane can be split: check before making the pane, so a pane
    /// (and the connection it dials) is never made for a split that cannot happen.
    pub fn can_split(&self) -> bool {
        self.active_tab()
            .is_some_and(|tab| tab.is_session() && tab.tree.contains(tab.focused))
    }

    /// Splits the front tab's focused pane along `axis`; the new pane goes second and takes
    /// the keyboard. Hands `pane` back when it cannot (see [`TabSet::can_split`]).
    pub fn split(&mut self, axis: Axis, pane: P) -> Result<(), P> {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return Err(pane);
        };
        let id = pane.pane_id();
        if !tab.tree.split(tab.focused, axis, id) {
            return Err(pane);
        }
        tab.panes.push(pane);
        tab.focused = id;
        Ok(())
    }

    /// Closes the front tab's focused pane and hands it back; the pane before it, or the one
    /// after for the first, takes the keyboard. `None` when the tab has a single pane: closing
    /// that is closing the tab.
    pub fn close_pane(&mut self) -> Option<P> {
        let tab = self.tabs.get_mut(self.active)?;
        if tab.panes.len() < 2 {
            return None;
        }
        let order = tab.tree.leaves();
        let at = order.iter().position(|id| *id == tab.focused)?;
        let next = if at > 0 { order[at - 1] } else { order[1] };
        let tree = std::mem::replace(&mut tab.tree, Node::Leaf(tab.focused));
        tab.tree = tree.remove(tab.focused)?;
        let ix = tab.panes.iter().position(|p| p.pane_id() == tab.focused)?;
        tab.focused = next;
        Some(tab.panes.remove(ix))
    }

    /// Gives pane `id` of the front tab the keyboard. False when it already has it or is not
    /// in that tab.
    pub fn focus_pane(&mut self, id: PaneId) -> bool {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return false;
        };
        if tab.focused == id || !tab.tree.contains(id) {
            return false;
        }
        tab.focused = id;
        true
    }

    /// The pane next to the front tab's focused one in direction `dir`.
    pub fn neighbour(&self, dir: Direction) -> Option<PaneId> {
        let tab = self.active_tab()?;
        tab.tree.neighbour(tab.focused, dir)
    }

    /// The "Reopen tabs" setting, as it is now; turned on, the list is saved at once.
    pub fn set_saving(&mut self, on: bool) {
        let was = std::mem::replace(&mut self.saving, on);
        if on && !was {
            self.changed();
        }
    }

    /// What `tabs.json` holds for these tabs.
    pub fn saved(&self) -> SavedTabs {
        let kept: Vec<&Tab<P>> = self.tabs.iter().filter(|t| t.is_session()).collect();
        // The slot among the saved tabs: a Files tab in front is saved as the tab before it.
        let before = self.tabs[..self.active.min(self.tabs.len())]
            .iter()
            .filter(|t| t.is_session())
            .count();
        SavedTabs {
            tabs: kept
                .iter()
                .map(|tab| SavedTab {
                    kind: SavedKind::Ssh {
                        alias: tab.alias.clone(),
                    },
                    title: tab.title.clone(),
                })
                .collect(),
            active: if self.active_tab().is_some_and(Tab::is_session) {
                before
            } else {
                before.saturating_sub(1)
            },
        }
    }

    fn changed(&self) {
        if self.saving {
            self.store.save(&self.saved());
        }
    }
}

impl<P> std::fmt::Debug for Tab<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tab")
            .field("id", &self.id)
            .field("alias", &self.alias)
            .field("title", &self.title)
            .field("focused", &self.focused)
            .field("panes", &self.panes.len())
            .finish()
    }
}

impl<P> std::fmt::Debug for TabSet<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TabSet")
            .field("tabs", &self.tabs)
            .field("active", &self.active)
            .field("saving", &self.saving)
            .finish_non_exhaustive()
    }
}

impl<P> Index<usize> for TabSet<P> {
    type Output = Tab<P>;

    fn index(&self, ix: usize) -> &Tab<P> {
        &self.tabs[ix]
    }
}

impl<'a, P> IntoIterator for &'a TabSet<P> {
    type Item = &'a Tab<P>;
    type IntoIter = std::slice::Iter<'a, Tab<P>>;

    fn into_iter(self) -> Self::IntoIter {
        self.tabs.iter()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct P(PaneId);

    impl Paned for P {
        fn pane_id(&self) -> PaneId {
            self.0
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

    fn set(names: &[&str]) -> (TabSet<P>, Saves) {
        let saves = Saves::default();
        let mut tabs = TabSet::new(Box::new(saves.clone()), true);
        for name in names {
            let id = tabs.new_pane_id();
            tabs.open((*name).into(), P(id));
        }
        saves.take();
        (tabs, saves)
    }

    fn aliases(tabs: &TabSet<P>) -> Vec<&str> {
        tabs.iter().map(|t| t.alias.as_str()).collect()
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
            .map(|n| (n.to_owned(), None, P(tabs.new_pane_id())))
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
        tabs.open("a".into(), P(id));
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
        assert!(tabs.can_split());
        let second = tabs.new_pane_id();
        tabs.split(Axis::Row, P(second)).unwrap();
        assert_eq!(tabs[0].focused(), second);
        assert_eq!(tabs[0].tree().leaves(), [first, second]);
        assert!(tabs.focus_pane(first));
        assert!(!tabs.focus_pane(first), "already focused");
        assert!(!tabs.focus_pane(99), "not in this tab");
        // The first pane closes: the one after it takes the keyboard.
        assert_eq!(tabs.close_pane(), Some(P(first)));
        assert_eq!(tabs[0].focused(), second);
        assert_eq!(tabs[0].tree().leaves(), [second]);
        assert_eq!(tabs[0].panes(), [P(second)]);
        assert_eq!(tabs.close_pane(), None, "the last pane closes with its tab");
    }

    #[test]
    fn a_files_tab_is_not_saved_and_the_saved_slot_skips_it() {
        let (mut tabs, saves) = set(&["a", "b"]);
        tabs.open("Files".into(), P(1000));
        assert_eq!(
            saves.take(),
            ["a,b|1"],
            "in front, it is saved as the tab before it"
        );
        tabs.open("c".into(), P(7));
        assert_eq!(
            saves.take(),
            ["a,b,c|2"],
            "slot 3 here, the third of the saved"
        );
        tabs.activate(0);
        assert_eq!(saves.take(), ["a,b,c|0"]);
        tabs.activate(2);
        assert!(!tabs.can_split(), "a Files tab does not split");
        assert!(!tabs[2].is_session() && tabs[1].is_session());
    }

    #[test]
    fn a_split_with_no_tab_hands_the_pane_back() {
        let mut tabs: TabSet<P> = TabSet::new(Box::new(Saves::default()), true);
        assert!(!tabs.can_split());
        assert_eq!(tabs.split(Axis::Column, P(1)), Err(P(1)));
    }
}
