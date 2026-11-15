//! A workspace layout contains stable session IDs, never vector indices.

pub(super) const MAX_PANES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SplitDirection {
    /// A horizontal divider: the new pane appears below the current pane.
    Horizontal,
    /// A vertical divider: the new pane appears to the right of the current pane.
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LayoutNode {
    Pane(u64),
    Split {
        id: u64,
        direction: SplitDirection,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn panes(&self) -> Vec<u64> {
        match self {
            Self::Pane(id) => vec![*id],
            Self::Split { first, second, .. } => {
                let mut ids = first.panes();
                ids.extend(second.panes());
                ids
            }
        }
    }

    pub fn split_ids(&self) -> Vec<u64> {
        match self {
            Self::Pane(_) => vec![],
            Self::Split {
                id, first, second, ..
            } => {
                let mut ids = vec![*id];
                ids.extend(first.split_ids());
                ids.extend(second.split_ids());
                ids
            }
        }
    }

    pub fn contains(&self, target: u64) -> bool {
        match self {
            Self::Pane(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    /// Only split an existing pane; stale async results must not create new workspaces.
    pub fn split(&mut self, target: u64, new: u64, direction: SplitDirection) -> bool {
        if self.contains(new) || self.panes().len() >= MAX_PANES {
            return false;
        }
        self.split_inner(target, new, direction)
    }

    fn split_inner(&mut self, target: u64, new: u64, direction: SplitDirection) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                *self = Self::Split {
                    id: new,
                    direction,
                    first: Box::new(Self::Pane(target)),
                    second: Box::new(Self::Pane(new)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split_inner(target, new, direction)
                    || second.split_inner(target, new, direction)
            }
        }
    }

    /// Prune closed sessions and collapse dividers that only have one surviving side.
    pub fn retain(self, keep: &impl Fn(u64) -> bool) -> Option<Self> {
        match self {
            Self::Pane(id) => keep(id).then_some(Self::Pane(id)),
            Self::Split {
                id,
                direction,
                first,
                second,
            } => match (first.retain(keep), second.retain(keep)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    id,
                    direction,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            },
        }
    }
}

pub(super) struct Workspace {
    pub id: u64,
    pub root: LayoutNode,
    pub focused: u64,
}

impl Workspace {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            root: LayoutNode::Pane(id),
            focused: id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutNode, SplitDirection};

    #[test]
    fn nested_splits_keep_session_identity_and_enforce_capacity() {
        let mut layout = LayoutNode::Pane(10);
        assert!(layout.split(10, 11, SplitDirection::Vertical));
        assert!(layout.split(11, 12, SplitDirection::Horizontal));
        assert!(layout.split(10, 13, SplitDirection::Horizontal));
        assert_eq!(layout.panes(), [10, 13, 11, 12]);
        assert!(!layout.split(11, 14, SplitDirection::Vertical));
        assert!(!layout.split(10, 12, SplitDirection::Vertical));
        assert_eq!(layout.panes(), [10, 13, 11, 12]);
    }

    #[test]
    fn closing_panes_and_locking_remote_sessions_collapse_the_tree() {
        let mut layout = LayoutNode::Pane(1);
        layout.split(1, 2, SplitDirection::Vertical);
        layout.split(2, 3, SplitDirection::Horizontal);
        let layout = layout.retain(&|id| id != 2).unwrap();
        assert_eq!(layout.panes(), [1, 3]);
        assert_eq!(layout.split_ids(), [2]);
        let layout = layout.retain(&|id| id == 3).unwrap();
        assert_eq!(layout, LayoutNode::Pane(3));
        assert!(layout.retain(&|_| false).is_none());
    }

    #[test]
    fn stale_or_duplicate_connection_results_cannot_mutate_layout() {
        let mut layout = LayoutNode::Pane(5);
        assert!(!layout.split(6, 7, SplitDirection::Vertical));
        assert!(!layout.split(5, 5, SplitDirection::Vertical));
        assert_eq!(layout, LayoutNode::Pane(5));
    }
}
