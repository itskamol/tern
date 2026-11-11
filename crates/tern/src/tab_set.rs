//! The open tabs and their panes, kept consistent: which tab is in front, how each tab's panes
//! are laid out and which one has the keyboard, and the saved list (`tabs.json`), written once
//! for each change that alters it. Generic over the pane, so these rules are tested without
//! sessions or a window; the shell supplies real panes and does the focusing and drawing.

use std::ops::Index;

use crate::split::{Direction, Floor, Node, PaneId, Rect};
use crate::tabs::reorder;
use crate::tabs_store::{SavedKind, SavedTab, SavedTabs};

/// A tab's identity for as long as it is open; unlike its slot it survives reordering.
pub type TabId = u64;

/// What the tab set needs from a pane: its id in the layout tree and the host it is connected
/// to, which a split tab's title follows.
pub trait Paned {
    fn pane_id(&self) -> PaneId;

    /// The pane is a session's terminal; the others (the Files tab) are not saved with the tab
    /// list and are not a host's tab.
    fn is_session(&self) -> bool {
        true
    }
    fn host(&self) -> &str;
}

/// A tab's title as the strip shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// The name the person gave the tab, else the focused pane's host.
    pub text: String,
    /// How many other panes the tab holds: the "+N" badge.
    pub others: usize,
}

/// The pane Fill acts on, chosen from the front tab's panes that are waiting at a password
/// prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillTarget {
    /// The focused pane if it is waiting, else the only pane that is.
    Pane(PaneId),
    /// Two or more unfocused panes are waiting and the focused one is not: tern does not guess.
    Several,
    /// No pane is waiting.
    None,
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

    /// Where each pane sits in the tab, as fractions of its area.
    pub fn layout(&self) -> Vec<(PaneId, Rect)> {
        self.tree.layout(Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        })
    }

    /// What the tab is called when the person has not named it.
    pub fn untitled(&self) -> &str {
        self.front().host()
    }

    pub fn heading(&self) -> Heading {
        Heading {
            text: self
                .title
                .clone()
                .unwrap_or_else(|| self.untitled().to_owned()),
            others: self.panes.len() - 1,
        }
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

    /// Pane `pane` of the front tab can be split on `side` and leave both halves at least
    /// `floor`: check before making the new pane, so a pane (and the connection it dials) is
    /// never made for a split that cannot happen.
    pub fn can_split(&self, pane: PaneId, side: Direction, floor: Floor) -> bool {
        self.active_tab()
            .is_some_and(|tab| tab.is_session() && tab.tree.can_split(pane, side, floor))
    }

    /// Splits pane `target` of the front tab on `side`; the new pane goes first for left and up,
    /// second for right and down, and takes the keyboard. Hands `pane` back when it cannot
    /// (see [`TabSet::can_split`]).
    pub fn split(
        &mut self,
        target: PaneId,
        side: Direction,
        pane: P,
        floor: Floor,
    ) -> Result<(), P> {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return Err(pane);
        };
        let id = pane.pane_id();
        if !tab.tree.can_split(target, side, floor) || !tab.tree.split(target, side, id) {
            return Err(pane);
        }
        tab.panes.push(pane);
        tab.focused = id;
        Ok(())
    }

    /// Moves the divider beside `pane` of the front tab, on its `side`, so the split's first
    /// side has share `ratio`, held to `floor`. False when there is no divider there.
    pub fn resize(&mut self, pane: PaneId, side: Direction, ratio: f32, floor: Floor) -> bool {
        self.tabs
            .get_mut(self.active)
            .is_some_and(|tab| tab.tree.set_ratio(pane, side, ratio, floor))
    }

    /// Moves the divider next to the focused pane 5% toward `dir`.
    pub fn step_focused(&mut self, dir: Direction, floor: Floor) -> bool {
        self.tabs
            .get_mut(self.active)
            .is_some_and(|tab| tab.tree.step_ratio(tab.focused, dir, floor))
    }

    /// Puts the divider beside `pane` on `side` back to half and half.
    pub fn reset_divider(&mut self, pane: PaneId, side: Direction, floor: Floor) -> bool {
        self.tabs
            .get_mut(self.active)
            .is_some_and(|tab| tab.tree.reset_ratio(pane, side, floor))
    }

    /// Every split of the front tab half and half.
    pub fn equalize(&mut self, floor: Floor) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.tree.equalize(floor);
        }
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

    /// Which pane of the front tab Fill acts on: see [`FillTarget`]. `waiting` says whether a
    /// pane is at a password prompt that could be filled.
    pub fn fill_target(&self, waiting: impl Fn(&P) -> bool) -> FillTarget {
        let Some(tab) = self.active_tab() else {
            return FillTarget::None;
        };
        if waiting(tab.front()) {
            return FillTarget::Pane(tab.focused);
        }
        let mut others = tab.panes.iter().filter(|p| waiting(p));
        match (others.next(), others.next()) {
            (Some(only), None) => FillTarget::Pane(only.pane_id()),
            (Some(_), Some(_)) => FillTarget::Several,
            (None, _) => FillTarget::None,
        }
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
#[path = "tab_set_tests.rs"]
mod tests;
