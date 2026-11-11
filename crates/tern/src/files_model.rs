//! What a Files tab panel shows and picks, apart from the window: how its rows are ordered and
//! which of them a click selects. Rows are the entries of the folder listed, in the order shown.

use std::collections::BTreeSet;

use tern_ssh::FileEntry;

/// The column the rows are ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            ascending: true,
        }
    }
}

impl Sort {
    /// A click on a column title: the same column flips the order, another starts ascending.
    #[must_use]
    pub fn click(self, key: SortKey) -> Self {
        if self.key == key {
            Self {
                key,
                ascending: !self.ascending,
            }
        } else {
            Self {
                key,
                ascending: true,
            }
        }
    }
}

/// Folders first, then by the column, whatever the direction: a folder never sinks below files.
pub fn order(entries: &mut [FileEntry], sort: Sort) {
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let by = match sort.key {
                SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortKey::Size => a.size.cmp(&b.size),
                SortKey::Modified => a.modified.cmp(&b.modified),
            }
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            if sort.ascending { by } else { by.reverse() }
        })
    });
}

/// The rows picked: ⌘-click toggles one, ⇧-click extends from the last plain click.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    picked: BTreeSet<usize>,
    anchor: Option<usize>,
}

impl Selection {
    /// A click on row `ix`.
    pub fn click(&mut self, ix: usize, command: bool, shift: bool) {
        match (command, shift, self.anchor) {
            (_, true, Some(anchor)) => {
                let (from, to) = (anchor.min(ix), anchor.max(ix));
                if !command {
                    self.picked.clear();
                }
                self.picked.extend(from..=to);
            }
            (true, _, _) => {
                if !self.picked.remove(&ix) {
                    self.picked.insert(ix);
                }
                self.anchor = Some(ix);
            }
            _ => {
                self.picked.clear();
                self.picked.insert(ix);
                self.anchor = Some(ix);
            }
        }
    }

    /// Row `ix` alone, as an arrow key picks it.
    pub fn only(&mut self, ix: usize) {
        self.click(ix, false, false);
    }

    pub fn clear(&mut self) {
        self.picked.clear();
        self.anchor = None;
    }

    pub fn contains(&self, ix: usize) -> bool {
        self.picked.contains(&ix)
    }

    pub fn len(&self) -> usize {
        self.picked.len()
    }

    /// The rows picked, top to bottom.
    pub fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.picked.iter().copied()
    }

    /// The row an arrow key moves from: the last one clicked.
    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    fn entry(name: &str, dir: bool, size: u64, age_secs: u64) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: format!("/{name}"),
            is_dir: dir,
            is_symlink: false,
            size,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs)),
        }
    }

    fn names(entries: &[FileEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn sample() -> Vec<FileEntry> {
        vec![
            entry("b.txt", false, 30, 10),
            entry("Zed", true, 0, 500),
            entry("a.txt", false, 10, 300),
            entry("alpha", true, 0, 20),
            entry("C.txt", false, 20, 5),
        ]
    }

    #[test]
    fn folders_stay_first_whatever_the_column_and_direction() {
        let mut rows = sample();
        order(&mut rows, Sort::default());
        assert_eq!(names(&rows), ["alpha", "Zed", "a.txt", "b.txt", "C.txt"]);
        order(
            &mut rows,
            Sort {
                key: SortKey::Name,
                ascending: false,
            },
        );
        assert_eq!(names(&rows), ["Zed", "alpha", "C.txt", "b.txt", "a.txt"]);
    }

    #[test]
    fn size_and_modified_order_the_files_and_break_ties_by_name() {
        let mut rows = sample();
        order(
            &mut rows,
            Sort {
                key: SortKey::Size,
                ascending: true,
            },
        );
        assert_eq!(names(&rows), ["alpha", "Zed", "a.txt", "C.txt", "b.txt"]);
        order(
            &mut rows,
            Sort {
                key: SortKey::Modified,
                ascending: false,
            },
        );
        // Newest first.
        assert_eq!(names(&rows), ["alpha", "Zed", "C.txt", "b.txt", "a.txt"]);
    }

    #[test]
    fn a_title_click_flips_its_own_column_and_starts_another_ascending() {
        let by_name = Sort::default();
        let flipped = by_name.click(SortKey::Name);
        assert!(!flipped.ascending);
        let size = flipped.click(SortKey::Size);
        assert_eq!((size.key, size.ascending), (SortKey::Size, true));
    }

    #[test]
    fn a_plain_click_picks_one_command_toggles_and_shift_extends_from_the_anchor() {
        let mut s = Selection::default();
        s.click(2, false, false);
        s.click(5, true, false);
        assert_eq!(s.rows().collect::<Vec<_>>(), [2, 5]);
        s.click(2, true, false);
        assert_eq!(s.rows().collect::<Vec<_>>(), [5]);
        s.click(1, false, false);
        s.click(4, false, true);
        assert_eq!(
            s.rows().collect::<Vec<_>>(),
            [1, 2, 3, 4],
            "shift replaces the old pick"
        );
        s.click(7, true, true);
        assert_eq!(s.rows().collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6, 7]);
        s.clear();
        assert!(s.rows().next().is_none() && s.anchor().is_none());
    }

    #[test]
    fn shift_with_no_anchor_is_a_plain_click() {
        let mut s = Selection::default();
        s.click(3, false, true);
        assert_eq!(s.rows().collect::<Vec<_>>(), [3]);
    }
}
