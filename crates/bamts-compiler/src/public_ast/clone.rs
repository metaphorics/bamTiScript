//! Deep cloning for canonical syntax nodes.
//!
//! Syntax payloads own their children, so cloning the payload recursively copies
//! the subtree. The caller explicitly chooses whether the cloned root preserves
//! its parser identity or receives a new identity; no hidden allocator or global
//! counter participates.

use crate::syntax::{Node, NodeData, NodeId};

#[must_use]
pub fn clone_node<T>(node: &Node<T>) -> Node<T>
where
    T: NodeData + Clone,
{
    Node::new(node.id(), node.range(), node.data().clone())
}

#[must_use]
pub fn clone_node_with_id<T>(node: &Node<T>, id: NodeId) -> Node<T>
where
    T: NodeData + Clone,
{
    Node::new(id, node.range(), node.data().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{TextRange, Utf16Pos};
    use crate::syntax::Expression;

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).unwrap()
    }

    #[test]
    fn clone_is_structurally_equal_and_all_owned_nodes_are_distinct() {
        let child = Node::new(NodeId::new(2), range(1, 5), Expression::This);
        let root = Node::new(
            NodeId::new(1),
            range(0, 6),
            Expression::Parenthesized(Box::new(child)),
        );
        let cloned = clone_node(&root);
        assert_eq!(cloned, root);
        assert!(!std::ptr::eq(&cloned, &root));
        let (Expression::Parenthesized(original), Expression::Parenthesized(copy)) =
            (root.data(), cloned.data())
        else {
            panic!("expected parenthesized expressions");
        };
        assert!(!std::ptr::eq(original.as_ref(), copy.as_ref()));
        assert_eq!(original.id(), copy.id());
    }

    #[test]
    fn caller_can_assign_a_fresh_root_identity() {
        let node = Node::new(NodeId::new(3), range(0, 1), Expression::This);
        let cloned = clone_node_with_id(&node, NodeId::new(90));
        assert_eq!(cloned.id(), NodeId::new(90));
        assert_eq!(cloned.range(), node.range());
        assert_eq!(cloned.data(), node.data());
    }
}
