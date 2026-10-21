//! The split layout of one tab: a tree of panes. Generic over the leaf so it is tested with
//! plain ids.

use gpui_kit::Axis;

/// Direction for moving focus between panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Horizontal,
            Self::Up | Self::Down => Axis::Vertical,
        }
    }

    fn forward(self) -> bool {
        matches!(self, Self::Right | Self::Down)
    }
}

/// Where a dragged pane lands on another pane: beside it on one side, or swapped with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaneTree<T> {
    Leaf(T),
    /// Children laid out along `axis` (horizontal = side by side). `id` keys the resize state.
    Split {
        id: u64,
        axis: Axis,
        children: Vec<PaneTree<T>>,
    },
}

impl<T: Clone + PartialEq> PaneTree<T> {
    /// Leaves in reading order.
    pub fn leaves(&self) -> Vec<T> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<T>) {
        match self {
            Self::Leaf(leaf) => out.push(leaf.clone()),
            Self::Split { children, .. } => children.iter().for_each(|c| c.collect(out)),
        }
    }

    pub fn contains(&self, target: &T) -> bool {
        match self {
            Self::Leaf(leaf) => leaf == target,
            Self::Split { children, .. } => children.iter().any(|c| c.contains(target)),
        }
    }

    /// Puts `new` next to `target` along `axis` (after it). A split of the same axis gains a
    /// child; otherwise `target` becomes a new two-child split. `next_id` gives split ids.
    pub fn split(&mut self, target: &T, axis: Axis, new: T, next_id: &mut u64) -> bool {
        self.insert(target, axis, new, false, next_id)
    }

    /// [`Self::split`], placing `new` before `target` (left / above) when `before` is set.
    fn insert(&mut self, target: &T, axis: Axis, new: T, before: bool, next_id: &mut u64) -> bool {
        match self {
            Self::Leaf(leaf) if leaf == target => {
                *next_id += 1;
                let old = std::mem::replace(
                    self,
                    Self::Split {
                        id: *next_id,
                        axis,
                        children: Vec::new(),
                    },
                );
                if let Self::Split { children, .. } = self {
                    children.push(old);
                    children.insert(usize::from(!before), Self::Leaf(new));
                }
                true
            }
            Self::Leaf(_) => false,
            Self::Split {
                axis: own,
                children,
                ..
            } => {
                if *own == axis
                    && let Some(i) = children
                        .iter()
                        .position(|c| matches!(c, Self::Leaf(l) if l == target))
                {
                    children.insert(if before { i } else { i + 1 }, Self::Leaf(new));
                    return true;
                }
                children
                    .iter_mut()
                    .any(|c| c.insert(target, axis, new.clone(), before, next_id))
            }
        }
    }

    /// Moves leaf `moving` onto `target`: beside it for a side zone, or swaps the two for
    /// [`DropZone::Center`]. The split `moving` leaves collapses if one child is left.
    pub fn move_leaf(&mut self, moving: &T, target: &T, zone: DropZone, next_id: &mut u64) -> bool {
        if moving == target || !self.contains(moving) || !self.contains(target) {
            return false;
        }
        let (axis, before) = match zone {
            DropZone::Center => {
                self.swap(moving, target);
                return true;
            }
            DropZone::Left => (Axis::Horizontal, true),
            DropZone::Right => (Axis::Horizontal, false),
            DropZone::Top => (Axis::Vertical, true),
            DropZone::Bottom => (Axis::Vertical, false),
        };
        self.remove(moving) && self.insert(target, axis, moving.clone(), before, next_id)
    }

    fn swap(&mut self, a: &T, b: &T) {
        match self {
            Self::Leaf(leaf) if leaf == a => *leaf = b.clone(),
            Self::Leaf(leaf) if leaf == b => *leaf = a.clone(),
            Self::Leaf(_) => {}
            Self::Split { children, .. } => children.iter_mut().for_each(|c| c.swap(a, b)),
        }
    }

    /// Removes `target`. Splits left with one child collapse into it. Returns false when
    /// `target` is the only leaf (the caller closes the tab instead).
    pub fn remove(&mut self, target: &T) -> bool {
        let Self::Split { children, .. } = self else {
            return false;
        };
        if let Some(i) = children
            .iter()
            .position(|c| matches!(c, Self::Leaf(l) if l == target))
        {
            children.remove(i);
        } else if !children.iter_mut().any(|c| c.remove(target)) {
            return false;
        }
        if children.len() == 1 {
            let only = children.pop().expect("one child");
            *self = only;
        }
        true
    }

    /// The pane focus moves to from `from` in `direction`, if any: the nearest pane on that
    /// side in the closest enclosing split along the same axis.
    pub fn neighbour(&self, from: &T, direction: Direction) -> Option<T> {
        let path = self.path_to(from)?;
        // Walk up from the leaf to the innermost split with a sibling on the wanted side.
        for depth in (0..path.len()).rev() {
            let node = self.node_at(&path[..depth]);
            let Self::Split { axis, children, .. } = node else {
                continue;
            };
            if *axis != direction.axis() {
                continue;
            }
            let index = path[depth];
            let sibling = if direction.forward() {
                children.get(index + 1)
            } else {
                index.checked_sub(1).and_then(|i| children.get(i))
            };
            if let Some(sibling) = sibling {
                return Some(sibling.edge_leaf(direction));
            }
        }
        None
    }

    /// The leaf of this subtree that is closest when entering it moving in `direction`.
    fn edge_leaf(&self, direction: Direction) -> T {
        match self {
            Self::Leaf(leaf) => leaf.clone(),
            Self::Split { axis, children, .. } => {
                let child = if *axis == direction.axis() && !direction.forward() {
                    children.last()
                } else {
                    children.first()
                };
                child.expect("splits have children").edge_leaf(direction)
            }
        }
    }

    /// Child indices from the root down to `target`.
    fn path_to(&self, target: &T) -> Option<Vec<usize>> {
        match self {
            Self::Leaf(leaf) => (leaf == target).then(Vec::new),
            Self::Split { children, .. } => children.iter().enumerate().find_map(|(i, c)| {
                c.path_to(target).map(|mut rest| {
                    rest.insert(0, i);
                    rest
                })
            }),
        }
    }

    fn node_at(&self, path: &[usize]) -> &Self {
        path.iter().fold(self, |node, &i| match node {
            Self::Split { children, .. } => &children[i],
            leaf => leaf,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(tree: &PaneTree<u32>) -> Vec<u32> {
        tree.leaves()
    }

    #[test]
    fn split_and_remove() {
        let mut next = 0;
        let mut tree = PaneTree::Leaf(1);
        assert!(tree.split(&1, Axis::Horizontal, 2, &mut next));
        assert!(
            tree.split(&2, Axis::Horizontal, 3, &mut next),
            "same axis appends to the split"
        );
        assert!(matches!(&tree, PaneTree::Split { children, .. } if children.len() == 3));
        assert!(tree.split(&2, Axis::Vertical, 4, &mut next));
        assert_eq!(ids(&tree), [1, 2, 4, 3]);

        assert!(tree.remove(&4));
        assert_eq!(ids(&tree), [1, 2, 3]);
        assert!(
            matches!(&tree, PaneTree::Split { children, .. } if children.len() == 3),
            "collapsed"
        );
        assert!(tree.remove(&1) && tree.remove(&2));
        assert_eq!(tree, PaneTree::Leaf(3));
        assert!(!tree.remove(&3), "the last pane is not removed here");
    }

    #[test]
    fn focus_moves_to_the_nearest_neighbour() {
        // [1 | [2 / 3]]
        let mut next = 0;
        let mut tree = PaneTree::Leaf(1);
        tree.split(&1, Axis::Horizontal, 2, &mut next);
        tree.split(&2, Axis::Vertical, 3, &mut next);
        assert_eq!(tree.neighbour(&1, Direction::Right), Some(2));
        assert_eq!(tree.neighbour(&3, Direction::Left), Some(1));
        assert_eq!(tree.neighbour(&2, Direction::Down), Some(3));
        assert_eq!(tree.neighbour(&3, Direction::Up), Some(2));
        assert_eq!(tree.neighbour(&1, Direction::Left), None);
        assert_eq!(tree.neighbour(&2, Direction::Up), None);
    }

    /// [1 | [2 / 3]]
    fn three() -> (PaneTree<u32>, u64) {
        let mut next = 0;
        let mut tree = PaneTree::Leaf(1);
        tree.split(&1, Axis::Horizontal, 2, &mut next);
        tree.split(&2, Axis::Vertical, 3, &mut next);
        (tree, next)
    }

    #[test]
    fn center_swaps() {
        let (mut tree, mut next) = three();
        assert!(tree.move_leaf(&1, &3, DropZone::Center, &mut next));
        assert_eq!(ids(&tree), [3, 2, 1]);
    }

    #[test]
    fn side_zones_move_and_collapse() {
        let (mut tree, mut next) = three();
        // 3 leaves the vertical split, which collapses into 2; 3 lands left of 1.
        assert!(tree.move_leaf(&3, &1, DropZone::Left, &mut next));
        assert_eq!(ids(&tree), [3, 1, 2]);
        assert!(
            matches!(&tree, PaneTree::Split { axis: Axis::Horizontal, children, .. } if children.len() == 3),
            "joins the existing horizontal split"
        );

        assert!(tree.move_leaf(&2, &1, DropZone::Top, &mut next));
        assert_eq!(ids(&tree), [3, 2, 1]);
        assert_eq!(tree.neighbour(&2, Direction::Down), Some(1));

        assert!(tree.move_leaf(&3, &1, DropZone::Bottom, &mut next));
        assert_eq!(ids(&tree), [2, 1, 3]);
        assert_eq!(tree.neighbour(&1, Direction::Down), Some(3));
    }

    #[test]
    fn dropping_on_itself_does_nothing() {
        let (mut tree, mut next) = three();
        let before = tree.clone();
        assert!(!tree.move_leaf(&2, &2, DropZone::Right, &mut next));
        assert!(!tree.move_leaf(&9, &2, DropZone::Right, &mut next));
        assert_eq!(tree, before);
    }
}
