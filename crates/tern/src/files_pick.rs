//! Which rows a click picks, for a Files tab panel and the Files panel alike. Rows are kept by
//! path, so a pick survives a new listing and a re-sort, where a row number would point at
//! something else; rows that are gone fall out with [`Picked::keep_only`].

use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picked {
    paths: BTreeSet<String>,
    anchor: Option<String>,
}

impl Picked {
    /// A click on row `ix` of `rows` (the paths shown, top to bottom): ⌘-click toggles one,
    /// ⇧-click extends from the last plain click (⌘⇧ keeps what was picked), and ⇧-click with
    /// no anchor in `rows` is a plain click. A row past the end picks nothing.
    pub fn click(&mut self, rows: &[String], ix: usize, command: bool, shift: bool) {
        let Some(row) = rows.get(ix) else { return };
        let anchor = self
            .anchor
            .as_ref()
            .and_then(|a| rows.iter().position(|r| r == a));
        match (command, shift, anchor) {
            (_, true, Some(from)) => {
                if !command {
                    self.paths.clear();
                }
                let (lo, hi) = (from.min(ix), from.max(ix));
                self.paths.extend(rows[lo..=hi].iter().cloned());
            }
            (true, _, _) => {
                if !self.paths.remove(row) {
                    self.paths.insert(row.clone());
                }
                self.anchor = Some(row.clone());
            }
            _ => {
                self.paths.clear();
                self.paths.insert(row.clone());
                self.anchor = Some(row.clone());
            }
        }
    }

    /// The row at `ix` alone, as an arrow key picks it.
    pub fn only(&mut self, rows: &[String], ix: usize) {
        self.click(rows, ix, false, false);
    }

    pub fn clear(&mut self) {
        self.paths.clear();
        self.anchor = None;
    }

    /// A new listing: what is no longer among `rows` is no longer picked.
    pub fn keep_only(&mut self, rows: &[String]) {
        let here: BTreeSet<&String> = rows.iter().collect();
        self.paths.retain(|p| here.contains(p));
        if self.anchor.as_ref().is_some_and(|a| !here.contains(a)) {
            self.anchor = None;
        }
    }

    pub fn contains(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The paths picked, in path order.
    pub fn paths(&self) -> impl Iterator<Item = &String> {
        self.paths.iter()
    }

    /// The row an arrow key moves from, or Enter opens: the last one plainly clicked.
    pub fn anchor(&self) -> Option<&str> {
        self.anchor.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<String> {
        ["a", "b", "c", "d", "e"]
            .map(|n| format!("/x/{n}"))
            .to_vec()
    }

    fn picked(p: &Picked) -> Vec<&str> {
        p.paths().map(String::as_str).collect()
    }

    #[test]
    fn a_plain_click_picks_one_row_and_drops_the_rest() {
        let rows = rows();
        let mut p = Picked::default();
        p.click(&rows, 1, false, false);
        p.click(&rows, 3, false, false);
        assert_eq!(picked(&p), ["/x/d"]);
    }

    #[test]
    fn command_click_toggles_one_row_and_keeps_the_others() {
        let rows = rows();
        let mut p = Picked::default();
        p.click(&rows, 1, false, false);
        p.click(&rows, 3, true, false);
        assert_eq!(picked(&p), ["/x/b", "/x/d"]);
        p.click(&rows, 1, true, false);
        assert_eq!(picked(&p), ["/x/d"]);
    }

    #[test]
    fn shift_click_extends_from_the_last_click_and_replaces_the_old_pick() {
        let rows = rows();
        let mut p = Picked::default();
        p.click(&rows, 0, false, false);
        p.click(&rows, 3, true, false);
        p.click(&rows, 1, false, true);
        assert_eq!(
            picked(&p),
            ["/x/b", "/x/c", "/x/d"],
            "from the last click, a ⌘-click included"
        );
    }

    #[test]
    fn command_shift_click_extends_and_keeps_what_was_picked() {
        let rows = rows();
        let mut p = Picked::default();
        p.click(&rows, 0, false, false);
        p.click(&rows, 4, true, false);
        p.click(&rows, 2, true, true);
        assert_eq!(picked(&p), ["/x/a", "/x/c", "/x/d", "/x/e"]);
    }

    #[test]
    fn shift_click_with_nothing_picked_is_a_plain_click() {
        let rows = rows();
        let mut p = Picked::default();
        p.click(&rows, 3, false, true);
        assert_eq!(picked(&p), ["/x/d"]);
        assert_eq!(p.anchor(), Some("/x/d"));
    }

    #[test]
    fn a_click_past_the_last_row_picks_nothing() {
        let mut p = Picked::default();
        p.click(&rows(), 9, false, false);
        assert!(p.is_empty());
    }

    #[test]
    fn a_row_stays_picked_through_a_new_listing_and_a_gone_row_falls_out() {
        let mut p = Picked::default();
        p.click(&rows(), 1, false, false);
        p.click(&rows(), 2, true, false);
        let listed_again: Vec<String> = ["/x/z", "/x/b", "/x/d"].map(String::from).to_vec();
        p.keep_only(&listed_again);
        assert_eq!(picked(&p), ["/x/b"]);
        assert_eq!(p.anchor(), None, "the anchor row is gone");
    }

    #[test]
    fn a_row_stays_picked_when_the_order_changes() {
        let mut p = Picked::default();
        p.click(&rows(), 0, false, false);
        let mut reversed = rows();
        reversed.reverse();
        p.keep_only(&reversed);
        p.click(&reversed, 3, false, true);
        assert_eq!(picked(&p), ["/x/a", "/x/b"], "the anchor follows its row");
    }
}
