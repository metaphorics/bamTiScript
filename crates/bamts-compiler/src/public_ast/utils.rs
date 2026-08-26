//! Zero-copy utilities over parser-owned syntax.

use super::NodeRef;
use crate::source::{SourceText, TextRange, Utf16Pos};

/// Converts a valid UTF-16 range to its borrowed source slice.
#[must_use]
pub fn text_of_range(source: &SourceText, range: TextRange) -> Option<&str> {
    let start = source.utf16_to_byte(range.start()).ok()?;
    let end = source.utf16_to_byte(range.end()).ok()?;
    source.as_str().get(start..end)
}

/// Returns a node's source slice without allocating.
#[must_use]
pub fn node_text<'a>(source: &'a SourceText, node: NodeRef<'_>) -> Option<&'a str> {
    text_of_range(source, node.range())
}

#[must_use]
pub fn contains_range(outer: TextRange, inner: TextRange) -> bool {
    outer.start() <= inner.start() && inner.end() <= outer.end()
}

#[must_use]
pub fn contains_position(range: TextRange, position: Utf16Pos) -> bool {
    range.start() <= position && position < range.end()
}

/// Returns the narrowest node containing `position` from a source-ordered list.
///
/// Equal ranges retain the later entry, allowing callers to pass parent-before-child
/// traversal output without a secondary tree index.
#[must_use]
pub fn narrowest_containing<'a>(
    nodes: impl IntoIterator<Item = NodeRef<'a>>,
    position: Utf16Pos,
) -> Option<NodeRef<'a>> {
    nodes.into_iter().fold(None, |best, candidate| {
        if !contains_position(candidate.range(), position) {
            return best;
        }
        match best {
            Some(current) if current.range().len() < candidate.range().len() => Some(current),
            _ => Some(candidate),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Utf16Pos;

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).unwrap()
    }

    #[test]
    fn slices_utf16_ranges_without_copying() {
        let source = SourceText::new("a😀b").expect("test source fits the per-file budget");
        let text = text_of_range(&source, range(1, 3)).unwrap();
        assert_eq!(text, "😀");
        assert_eq!(text.as_ptr(), source.as_str()[1..5].as_ptr());
    }

    #[test]
    fn containment_is_half_open() {
        let value = range(2, 5);
        assert!(contains_position(value, Utf16Pos::new(2)));
        assert!(!contains_position(value, Utf16Pos::new(5)));
        assert!(contains_range(value, range(3, 5)));
        assert!(!contains_range(value, range(1, 3)));
    }
}
