//! Undo and redo for the host list (⌘Z, ⇧⌘Z). Each entry is the whole list before and after
//! one edit made here, so an undo never has to know what kind of edit it was. An edit is only
//! undone while the list is still as it left it: after a sync or an import changed it, the
//! history is dropped rather than overwrite what arrived.

use crate::connections::Connection;

/// Edits kept; a host list is small, so this is a few kilobytes at most.
const CAP: usize = 20;

#[derive(Debug, Clone)]
struct Edit {
    before: Vec<Connection>,
    after: Vec<Connection>,
    what: String,
}

#[derive(Debug, Default)]
pub struct HostHistory {
    undo: Vec<Edit>,
    redo: Vec<Edit>,
}

impl HostHistory {
    /// Remembers an edit that turned `before` into `after`; `what` names it for the toast
    /// ("Remove web-01"). A new edit ends what could be redone.
    pub fn record(&mut self, before: Vec<Connection>, after: Vec<Connection>, what: String) {
        if before == after {
            return;
        }
        self.undo.push(Edit {
            before,
            after,
            what,
        });
        if self.undo.len() > CAP {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// The list to write back for ⌘Z and what it undoes, if `now` is still what that edit left.
    pub fn undo(&mut self, now: &[Connection]) -> Option<(Vec<Connection>, String)> {
        let edit = self.undo.pop()?;
        if edit.after != now {
            self.clear();
            return None;
        }
        let back = (edit.before.clone(), edit.what.clone());
        self.redo.push(edit);
        Some(back)
    }

    /// The list to write back for ⇧⌘Z and what it redoes, if `now` is what the undo left.
    pub fn redo(&mut self, now: &[Connection]) -> Option<(Vec<Connection>, String)> {
        let edit = self.redo.pop()?;
        if edit.before != now {
            self.clear();
            return None;
        }
        let back = (edit.after.clone(), edit.what.clone());
        self.undo.push(edit);
        Some(back)
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{CAP, HostHistory};
    use crate::connections::Connection;

    fn list(names: &[&str]) -> Vec<Connection> {
        names
            .iter()
            .map(|n| Connection {
                name: (*n).into(),
                ..Connection::default()
            })
            .collect()
    }

    #[test]
    fn undo_puts_the_list_back_and_redo_does_the_edit_again() {
        let mut h = HostHistory::default();
        h.record(list(&["a", "b"]), list(&["a"]), "Remove b".into());
        assert_eq!(
            h.undo(&list(&["a"])),
            Some((list(&["a", "b"]), "Remove b".into()))
        );
        assert_eq!(h.undo(&list(&["a", "b"])), None);
        assert_eq!(
            h.redo(&list(&["a", "b"])),
            Some((list(&["a"]), "Remove b".into()))
        );
        assert_eq!(h.redo(&list(&["a"])), None);
    }

    #[test]
    fn a_list_changed_elsewhere_is_never_overwritten_and_drops_the_history() {
        let mut h = HostHistory::default();
        h.record(list(&["a"]), list(&["a", "b"]), "Add b".into());
        h.record(list(&["a", "b"]), list(&["b"]), "Remove a".into());
        // A sync replaced the list meanwhile.
        assert_eq!(h.undo(&list(&["b", "synced"])), None);
        assert_eq!(h.undo(&list(&["a", "b"])), None);
    }

    #[test]
    fn a_new_edit_ends_redo_and_an_edit_that_changes_nothing_is_not_kept() {
        let mut h = HostHistory::default();
        h.record(list(&["a"]), list(&["a", "b"]), "Add b".into());
        assert!(h.undo(&list(&["a", "b"])).is_some());
        h.record(list(&["a"]), list(&["c"]), "Rename a".into());
        assert_eq!(h.redo(&list(&["c"])), None);
        h.record(list(&["c"]), list(&["c"]), "Nothing".into());
        assert_eq!(
            h.undo(&list(&["c"])),
            Some((list(&["a"]), "Rename a".into()))
        );
    }

    #[test]
    fn only_the_newest_edits_are_kept() {
        let mut h = HostHistory::default();
        let names: Vec<String> = (0..=CAP + 1).map(|i| i.to_string()).collect();
        for pair in names.windows(2) {
            h.record(list(&[&pair[0]]), list(&[&pair[1]]), pair[1].clone());
        }
        let mut undone = 0;
        let mut now = list(&[&names[CAP + 1]]);
        while let Some((back, _)) = h.undo(&now) {
            now = back;
            undone += 1;
        }
        assert_eq!(undone, CAP);
    }
}
