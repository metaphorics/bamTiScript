//! Construction helpers for canonical syntax nodes.
//!
//! The caller supplies both identity and range. The payload's [`NodeData`]
//! implementation remains the sole authority for its syntax kind.

use crate::source::TextRange;
use crate::syntax::{Node, NodeData, NodeId};

#[must_use]
pub fn create_node<T: NodeData>(id: NodeId, range: TextRange, data: T) -> Node<T> {
    Node::new(id, range, data)
}

/// Updates a canonical node without changing its caller-owned identity.
///
/// An equal payload and range return the original allocation by borrow. A real
/// change is allocated with the identity supplied by the caller; this module
/// never owns a global identity counter.
#[must_use]
pub fn update_node<'a, T>(
    original: &'a Node<T>,
    changed_id: NodeId,
    range: TextRange,
    data: T,
) -> UpdatedNode<'a, T>
where
    T: NodeData + Eq,
{
    if original.range() == range && original.data() == &data {
        UpdatedNode::Original(original)
    } else {
        UpdatedNode::Changed(Node::new(changed_id, range, data))
    }
}

#[derive(Debug)]
pub enum UpdatedNode<'a, T> {
    Original(&'a Node<T>),
    Changed(Node<T>),
}

impl<T> UpdatedNode<'_, T> {
    #[must_use]
    pub fn as_node(&self) -> &Node<T> {
        match self {
            Self::Original(node) => node,
            Self::Changed(node) => node,
        }
    }

    #[must_use]
    pub const fn is_original(&self) -> bool {
        matches!(self, Self::Original(_))
    }

    #[must_use]
    pub fn into_owned(self) -> Option<Node<T>> {
        match self {
            Self::Original(_) => None,
            Self::Changed(node) => Some(node),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Utf16Pos;
    use crate::syntax::{Expression, NodeKind};

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).unwrap()
    }

    #[test]
    fn caller_identity_and_payload_kind_are_authoritative() {
        let node = create_node(NodeId::new(41), range(1, 3), Expression::This);
        assert_eq!(node.id(), NodeId::new(41));
        assert_eq!(node.kind(), NodeKind::ThisExpression);
    }

    #[test]
    fn update_reuses_unchanged_node_and_allocates_real_change() {
        let node = create_node(NodeId::new(1), range(0, 4), Expression::This);
        let unchanged = update_node(&node, NodeId::new(99), range(0, 4), Expression::This);
        assert!(unchanged.is_original());
        assert!(std::ptr::eq(unchanged.as_node(), &node));

        let changed = update_node(&node, NodeId::new(2), range(0, 5), Expression::Super);
        assert!(!changed.is_original());
        assert_eq!(changed.as_node().id(), NodeId::new(2));
        assert_eq!(changed.as_node().kind(), NodeKind::SuperExpression);
    }
}
