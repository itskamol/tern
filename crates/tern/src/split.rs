//! The panes of one tab as a binary tree: a leaf is a pane, a split holds two subtrees side
//! by side or one above the other. Closing a pane collapses its parent, so the sibling takes
//! the space. Each split keeps the first side's share of its length (its ratio), and no pane
//! may be given less than a [`Floor`]. Pure data, so the rules are tested without a window.

/// How a split lays out its two halves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// First on the left, second on the right.
    Row,
    /// First on top, second below.
    Column,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Fewest columns and rows a pane may be left with.
pub const MIN_COLS: f32 = 20.0;
pub const MIN_ROWS: f32 = 5.0;

/// Epsilon for comparing shares of the unit square.
const EPS: f32 = 1e-4;

/// Smallest share of the tab a pane may take, as fractions of its width and height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Floor {
    pub w: f32,
    pub h: f32,
}

impl Floor {
    /// The floor for a tab `cols` columns wide and `rows` rows high (as measured, so fractional).
    pub fn cells(cols: f32, rows: f32) -> Self {
        Self {
            w: MIN_COLS / cols.max(1.0),
            h: MIN_ROWS / rows.max(1.0),
        }
    }

    /// The floor for a tab measured from its panes: each is `(columns, rows, share)` of the tab
    /// as drawn. `pad` is what a pane's border and one divider take besides its grid, in cells,
    /// so a pane at the floor still has [`MIN_COLS`] by [`MIN_ROWS`] to show.
    pub fn of_panes(panes: &[(f32, f32, Rect)], pad: (f32, f32)) -> Self {
        // A pane's grid and padding are its share of the tab; the biggest estimate is the least
        // thrown off by the dividers.
        let tab = |cells: fn(&(f32, f32, Rect)) -> (f32, f32), pad: f32| {
            panes
                .iter()
                .map(|p| {
                    let (len, share) = cells(p);
                    (len + pad) / share.max(0.01)
                })
                .fold(1.0_f32, f32::max)
        };
        let cols = tab(|(c, _, r)| (*c, r.w), pad.0);
        let rows = tab(|(_, h, r)| (*h, r.h), pad.1);
        Self {
            w: (MIN_COLS + pad.0) / cols,
            h: (MIN_ROWS + pad.1) / rows,
        }
    }

    fn along(self, axis: Axis) -> f32 {
        match axis {
            Axis::Row => self.w,
            Axis::Column => self.h,
        }
    }
}

impl Direction {
    /// The axis a divider on this side of a pane lies across.
    pub fn axis(self) -> Axis {
        match self {
            Direction::Left | Direction::Right => Axis::Row,
            Direction::Up | Direction::Down => Axis::Column,
        }
    }

    /// Whether a pane with a divider on this side is the first child of that split.
    fn pane_is_first(self) -> bool {
        matches!(self, Direction::Right | Direction::Down)
    }
}

pub type PaneId = u32;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(PaneId),
    Split {
        axis: Axis,
        /// The first child's share of the split's length.
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// Where a divider is looked for: next to a pane on one side, or on whichever side there is one.
#[derive(Clone, Copy)]
enum Beside {
    Side(bool),
    Any,
}

fn halves(axis: Axis, ratio: f32, area: Rect) -> (Rect, Rect) {
    match axis {
        Axis::Row => {
            let w = area.w * ratio;
            (
                Rect { w, ..area },
                Rect {
                    x: area.x + w,
                    w: area.w - w,
                    ..area
                },
            )
        }
        Axis::Column => {
            let h = area.h * ratio;
            (
                Rect { h, ..area },
                Rect {
                    y: area.y + h,
                    h: area.h - h,
                    ..area
                },
            )
        }
    }
}

const UNIT: Rect = Rect {
    x: 0.0,
    y: 0.0,
    w: 1.0,
    h: 1.0,
};

/// A pane's place in the unit square, as fractions of the tab's area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Node {
    /// Every pane, left to right and top to bottom.
    pub fn leaves(&self) -> Vec<PaneId> {
        match self {
            Node::Leaf(id) => vec![*id],
            Node::Split { first, second, .. } => {
                let mut all = first.leaves();
                all.extend(second.leaves());
                all
            }
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(leaf) => *leaf == id,
            Node::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    /// Splits pane `target` in two on `side`, half and half; `new` goes first for left and up,
    /// second for right and down. False when `target` is not in the tree.
    pub fn split(&mut self, target: PaneId, side: Direction, new: PaneId) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                let (first, second) = if side.pane_is_first() {
                    (target, new)
                } else {
                    (new, target)
                };
                *self = Node::Split {
                    axis: side.axis(),
                    ratio: 0.5,
                    first: Box::new(Node::Leaf(first)),
                    second: Box::new(Node::Leaf(second)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { first, second, .. } => {
                first.split(target, side, new) || second.split(target, side, new)
            }
        }
    }

    /// Whether splitting `target` on `side` leaves both halves at least `floor`.
    pub fn can_split(&self, target: PaneId, side: Direction, floor: Floor) -> bool {
        self.layout(UNIT)
            .into_iter()
            .find(|(id, _)| *id == target)
            .is_some_and(|(_, r)| {
                let len = match side.axis() {
                    Axis::Row => r.w,
                    Axis::Column => r.h,
                };
                len / 2.0 >= floor.along(side.axis()) - EPS
            })
    }

    /// The fewest of the tab's length along `axis` this subtree may take.
    fn min_len(&self, axis: Axis, floor: Floor) -> f32 {
        match self {
            Node::Leaf(_) => floor.along(axis),
            Node::Split {
                axis: a,
                first,
                second,
                ..
            } => {
                let (x, y) = (first.min_len(axis, floor), second.min_len(axis, floor));
                if *a == axis { x + y } else { x.max(y) }
            }
        }
    }

    /// Moves the divider beside `pane` on `side` so the first child has share `ratio`, as far
    /// as `floor` allows. False when there is no divider there.
    pub fn set_ratio(&mut self, pane: PaneId, side: Direction, ratio: f32, floor: Floor) -> bool {
        let beside = Beside::Side(side.pane_is_first());
        self.adjust(pane, side.axis(), beside, UNIT, floor, &|_| ratio)
    }

    /// Moves the nearest divider across `dir`'s axis that has a neighbour of `pane` in `dir` by
    /// 5% of its split, toward `dir`; a pane with none that way moves its own divider. False
    /// when the pane has no divider on that axis.
    pub fn step_ratio(&mut self, pane: PaneId, dir: Direction, floor: Floor) -> bool {
        let delta = if dir.pane_is_first() { 0.05 } else { -0.05 };
        let to = |r: f32| r + delta;
        // The divider with a neighbour that way; a pane at the edge moves its own divider.
        let toward = Beside::Side(dir.pane_is_first());
        self.adjust(pane, dir.axis(), toward, UNIT, floor, &to)
            || self.adjust(pane, dir.axis(), Beside::Any, UNIT, floor, &to)
    }

    /// Puts the divider beside `pane` on `side` back to half and half.
    pub fn reset_ratio(&mut self, pane: PaneId, side: Direction, floor: Floor) -> bool {
        self.set_ratio(pane, side, 0.5, floor)
    }

    /// Every split half and half, where `floor` allows.
    pub fn equalize(&mut self, floor: Floor) {
        self.equalize_in(UNIT, floor);
    }

    fn equalize_in(&mut self, area: Rect, floor: Floor) {
        let Node::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        else {
            return;
        };
        *ratio = clamp_ratio(*axis, 0.5, area, first, second, floor).unwrap_or(0.5);
        let (a, b) = halves(*axis, *ratio, area);
        first.equalize_in(a, floor);
        second.equalize_in(b, floor);
    }

    /// Applies `to` to the ratio of the deepest split across `axis` that holds `pane` (on the
    /// given side of it) and clamps the result to the floors of both subtrees.
    fn adjust(
        &mut self,
        pane: PaneId,
        axis: Axis,
        beside: Beside,
        area: Rect,
        floor: Floor,
        to: &dyn Fn(f32) -> f32,
    ) -> bool {
        let Node::Split {
            axis: own,
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        let (a, b) = halves(*own, *ratio, area);
        let in_first = first.contains(pane);
        let deeper = if in_first {
            first.adjust(pane, axis, beside, a, floor, to)
        } else {
            second.adjust(pane, axis, beside, b, floor, to)
        };
        if deeper {
            return true;
        }
        let here = match beside {
            Beside::Side(want_first) => want_first == in_first,
            Beside::Any => true,
        };
        if *own != axis || !here || !(in_first || second.contains(pane)) {
            return false;
        }
        match clamp_ratio(axis, to(*ratio), area, first, second, floor) {
            Some(r) => {
                *ratio = r;
                true
            }
            None => false,
        }
    }

    /// Removes pane `id`; its sibling takes the parent's place. `None` when `id` was the only
    /// pane (the caller closes the tab), and the tree unchanged when `id` is not in it.
    pub fn remove(self, id: PaneId) -> Option<Node> {
        match self {
            Node::Leaf(leaf) if leaf == id => None,
            leaf @ Node::Leaf(_) => Some(leaf),
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(id), second.remove(id)) {
                // Only one side can hold the pane; the other comes back whole, so a side that
                // disappeared leaves its sibling to take over.
                (None, Some(rest)) | (Some(rest), None) => Some(rest),
                (Some(first), Some(second)) => Some(Node::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (None, None) => None,
            },
        }
    }

    /// Where each pane sits when the whole tree fills `area`, every split cut by its ratio.
    pub fn layout(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        match self {
            Node::Leaf(id) => vec![(*id, area)],
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = halves(*axis, *ratio, area);
                let mut all = first.layout(a);
                all.extend(second.layout(b));
                all
            }
        }
    }

    /// The pane next to `from` in `dir`: the one that touches its edge and shares the most of
    /// it. `None` at the edge of the tab.
    pub fn neighbour(&self, from: PaneId, dir: Direction) -> Option<PaneId> {
        let all = self.layout(UNIT);
        let (_, me) = all.iter().find(|(id, _)| *id == from)?;
        let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| (a1.min(b1) - a0.max(b0)).max(0.0);
        all.iter()
            .filter(|(id, _)| *id != from)
            .filter_map(|(id, r)| {
                let (touches, shared) = match dir {
                    Direction::Left => (
                        (r.x + r.w - me.x).abs() < EPS,
                        overlap(me.y, me.y + me.h, r.y, r.y + r.h),
                    ),
                    Direction::Right => (
                        (me.x + me.w - r.x).abs() < EPS,
                        overlap(me.y, me.y + me.h, r.y, r.y + r.h),
                    ),
                    Direction::Up => (
                        (r.y + r.h - me.y).abs() < EPS,
                        overlap(me.x, me.x + me.w, r.x, r.x + r.w),
                    ),
                    Direction::Down => (
                        (me.y + me.h - r.y).abs() < EPS,
                        overlap(me.x, me.x + me.w, r.x, r.x + r.w),
                    ),
                };
                (touches && shared > EPS).then_some((*id, shared))
            })
            // The most shared edge wins; on a tie the earlier pane (top or left) does.
            .fold(None::<(PaneId, f32)>, |best, (id, shared)| match best {
                Some((_, s)) if s >= shared - EPS => best,
                _ => Some((id, shared)),
            })
            .map(|(id, _)| id)
    }
}

/// The side of a pane a host dropped at (`x`, `y`), as fractions of the pane, would take: the
/// nearest edge, the corners split along the diagonals.
pub fn drop_side(x: f32, y: f32) -> Direction {
    let (dx, dy) = (x - 0.5, y - 0.5);
    match (dx.abs() > dy.abs(), dx < 0.0, dy < 0.0) {
        (true, true, _) => Direction::Left,
        (true, false, _) => Direction::Right,
        (false, _, true) => Direction::Up,
        (false, _, false) => Direction::Down,
    }
}

/// `want` held to what both subtrees of a split filling `area` need; `None` when they need
/// more than there is.
fn clamp_ratio(
    axis: Axis,
    want: f32,
    area: Rect,
    first: &Node,
    second: &Node,
    floor: Floor,
) -> Option<f32> {
    let len = match axis {
        Axis::Row => area.w,
        Axis::Column => area.h,
    };
    let lo = first.min_len(axis, floor) / len;
    let hi = 1.0 - second.min_len(axis, floor) / len;
    (lo <= hi + EPS).then(|| want.clamp(lo.min(hi), hi))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Pane 1 on the left; 2 top right; 3 bottom right.
    fn l_shape() -> Node {
        let mut t = Node::Leaf(1);
        assert!(t.split(1, Direction::Right, 2));
        assert!(t.split(2, Direction::Down, 3));
        t
    }

    /// The two ratios of `Row(1, Row(2, 3))`.
    fn ratios(t: &Node) -> (f32, f32) {
        match t {
            Node::Split { ratio, second, .. } => match &**second {
                Node::Split { ratio: inner, .. } => (*ratio, *inner),
                Node::Leaf(_) => (*ratio, f32::NAN),
            },
            Node::Leaf(_) => (f32::NAN, f32::NAN),
        }
    }

    #[test]
    fn the_floor_leaves_a_pane_its_minimum_grid_after_border_and_divider() {
        let half = Rect {
            x: 0.0,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        };
        // Two panes of 100 columns each, 1 column of border and divider apiece: a tab of 202.
        let floor = Floor::of_panes(&[(100.0, 30.0, half)], (1.0, 0.5));
        assert!((floor.w * 202.0 - 1.0 - MIN_COLS).abs() < 1e-3, "{floor:?}");
        assert!((floor.h * 30.5 - 0.5 - MIN_ROWS).abs() < 1e-3, "{floor:?}");
    }

    #[test]
    fn an_arrow_moves_the_nearest_divider_that_has_a_neighbour_that_way() {
        let floor = Floor { w: 0.1, h: 0.1 };
        let three = || {
            let mut t = Node::Leaf(1);
            assert!(t.split(1, Direction::Right, 2));
            assert!(t.split(2, Direction::Right, 3));
            t
        };
        // The middle pane has 1 on its left: ← moves the divider with 1, not the one with 3.
        let mut t = three();
        assert!(t.step_ratio(2, Direction::Left, floor));
        assert_eq!(ratios(&t), (0.45, 0.5));
        // ... and → the divider with 3.
        let mut t = three();
        assert!(t.step_ratio(2, Direction::Right, floor));
        assert_eq!(ratios(&t), (0.5, 0.55));
        // The left pane grows to the right.
        let mut t = three();
        assert!(t.step_ratio(1, Direction::Right, floor));
        assert_eq!(ratios(&t), (0.55, 0.5));
        // The last pane has nothing on its right, so → moves its own divider and shrinks it.
        let mut t = three();
        assert!(t.step_ratio(3, Direction::Right, floor));
        assert_eq!(ratios(&t), (0.5, 0.55));
    }

    #[test]
    fn splitting_puts_the_new_pane_second_and_keeps_order() {
        let t = l_shape();
        assert_eq!(t.leaves(), [1, 2, 3]);
        assert_eq!(
            t,
            Node::Split {
                axis: Axis::Row,
                ratio: 0.5,
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Split {
                    axis: Axis::Column,
                    ratio: 0.5,
                    first: Box::new(Node::Leaf(2)),
                    second: Box::new(Node::Leaf(3)),
                }),
            }
        );
    }

    #[test]
    fn splitting_a_missing_pane_changes_nothing() {
        let mut t = l_shape();
        assert!(!t.split(9, Direction::Right, 4));
        assert_eq!(t.leaves(), [1, 2, 3]);
    }

    #[test]
    fn closing_a_pane_collapses_its_parent_into_the_sibling() {
        // Closing 3 leaves 2 to fill the right half: the column split is gone.
        let t = l_shape().remove(3).unwrap();
        assert_eq!(
            t,
            Node::Split {
                axis: Axis::Row,
                ratio: 0.5,
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Leaf(2)),
            }
        );
        // Closing 1 leaves the column alone: it takes the whole tab.
        let t = l_shape().remove(1).unwrap();
        assert_eq!(
            t,
            Node::Split {
                axis: Axis::Column,
                ratio: 0.5,
                first: Box::new(Node::Leaf(2)),
                second: Box::new(Node::Leaf(3)),
            }
        );
    }

    #[test]
    fn the_last_pane_cannot_be_removed_and_unknown_ones_change_nothing() {
        assert_eq!(Node::Leaf(1).remove(1), None);
        assert_eq!(l_shape().remove(9), Some(l_shape()));
    }

    #[test]
    fn focus_moves_to_the_pane_that_touches_the_edge() {
        let t = l_shape();
        assert_eq!(
            t.neighbour(1, Direction::Right),
            Some(2),
            "top wins the tie"
        );
        assert_eq!(t.neighbour(2, Direction::Left), Some(1));
        assert_eq!(t.neighbour(3, Direction::Left), Some(1));
        assert_eq!(t.neighbour(2, Direction::Down), Some(3));
        assert_eq!(t.neighbour(3, Direction::Up), Some(2));
    }

    #[test]
    fn there_is_no_neighbour_past_the_edge() {
        let t = l_shape();
        assert_eq!(t.neighbour(1, Direction::Left), None);
        assert_eq!(t.neighbour(1, Direction::Up), None);
        assert_eq!(t.neighbour(2, Direction::Up), None);
        assert_eq!(t.neighbour(3, Direction::Down), None);
        assert_eq!(t.neighbour(3, Direction::Right), None);
        assert_eq!(Node::Leaf(1).neighbour(1, Direction::Right), None);
    }

    #[test]
    fn a_drop_takes_the_nearest_edge() {
        assert_eq!(drop_side(0.1, 0.5), Direction::Left);
        assert_eq!(drop_side(0.9, 0.4), Direction::Right);
        assert_eq!(drop_side(0.5, 0.1), Direction::Up);
        assert_eq!(drop_side(0.3, 0.95), Direction::Down);
        // Near a corner the longer way from the middle decides.
        assert_eq!(drop_side(0.05, 0.2), Direction::Left);
        assert_eq!(drop_side(0.2, 0.05), Direction::Up);
    }

    fn leaf(id: PaneId) -> Box<Node> {
        Box::new(Node::Leaf(id))
    }

    fn pair(axis: Axis, first: Box<Node>, second: Box<Node>) -> Box<Node> {
        Box::new(Node::Split {
            axis,
            ratio: 0.5,
            first,
            second,
        })
    }

    #[test]
    fn the_neighbour_sharing_most_of_the_edge_wins() {
        // Pane 1 is tall on the left. On the right 3 and 4 share the top half and 5 has the
        // bottom half. Going right from 1, pane 5 shares half of the edge, 3 and 4 a quarter.
        let right = pair(Axis::Column, pair(Axis::Column, leaf(3), leaf(4)), leaf(5));
        let mut t = *pair(Axis::Row, leaf(1), right);
        assert_eq!(t.leaves(), [1, 3, 4, 5]);
        assert_eq!(t.neighbour(1, Direction::Right), Some(5));
        assert_eq!(t.neighbour(3, Direction::Left), Some(1));
        // Cut pane 1 into two stacked halves: from 5 only the lower one touches; from 3 only
        // the upper one does.
        assert!(t.split(1, Direction::Down, 2));
        assert_eq!(t.neighbour(5, Direction::Left), Some(2));
        assert_eq!(t.neighbour(3, Direction::Left), Some(1));
    }
}
