use std::{fmt, path::Path, str::FromStr, sync::Arc};

/// Identifies one source file within a compilation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct SourceId(u32);

impl SourceId {
    /// Creates an identifier from its compiler-assigned value.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the compiler-assigned value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl From<u32> for SourceId {
    fn from(value: u32) -> Self {
        Self::new(value)
    }
}
/// The canonical identity of one source in a resolved program.
///
/// The path is filesystem-canonical and the numeric id is assigned once by the
/// compiler's program loader; consumers must not substitute mtimes or allocations.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceIdentity {
    source_id: SourceId,
    path: Arc<Path>,
}

impl SourceIdentity {
    pub(crate) fn new(source_id: SourceId, path: Arc<Path>) -> Self {
        Self { source_id, path }
    }

    #[must_use]
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A zero-based offset measured in UTF-16 code units.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct Utf16Pos(usize);

impl Utf16Pos {
    /// The first UTF-16 position in a source.
    pub const ZERO: Self = Self(0);

    /// Creates a UTF-16 coordinate. A [`SourceText`] validates it against text.
    #[must_use]
    pub const fn new(offset: usize) -> Self {
        Self(offset)
    }

    /// Returns the coordinate's UTF-16 code-unit offset.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl From<usize> for Utf16Pos {
    fn from(offset: usize) -> Self {
        Self::new(offset)
    }
}

/// A half-open range of UTF-16 source positions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TextRange {
    start: Utf16Pos,
    end: Utf16Pos,
}

impl TextRange {
    /// Creates a range when its endpoints are ordered.
    pub const fn new(start: Utf16Pos, end: Utf16Pos) -> Result<Self, SourcePositionError> {
        if start.get() > end.get() {
            return Err(SourcePositionError::RangeStartAfterEnd { start, end });
        }

        Ok(Self { start, end })
    }

    /// Returns the inclusive start coordinate.
    #[must_use]
    pub const fn start(self) -> Utf16Pos {
        self.start
    }

    /// Returns the exclusive end coordinate.
    #[must_use]
    pub const fn end(self) -> Utf16Pos {
        self.end
    }

    /// Returns whether the range contains no UTF-16 code units.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.get() == self.end.get()
    }

    /// Returns the range length in UTF-16 code units.
    #[must_use]
    pub const fn len(self) -> usize {
        self.end.get() - self.start.get()
    }
}

/// The syntax accepted for a source file.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ScriptKind {
    JavaScript,
    JavaScriptReact,
    TypeScript,
    TypeScriptReact,
    Json,
}

/// Controls how JSX syntax is represented in JavaScript output.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum JsxEmit {
    Preserve,
    React,
    ReactNative,
    ReactJsx,
    ReactJsxDev,
}

impl JsxEmit {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::React => "react",
            Self::ReactNative => "react-native",
            Self::ReactJsx => "react-jsx",
            Self::ReactJsxDev => "react-jsxdev",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "preserve" => Some(Self::Preserve),
            "react" => Some(Self::React),
            "react-native" => Some(Self::ReactNative),
            "react-jsx" => Some(Self::ReactJsx),
            "react-jsxdev" => Some(Self::ReactJsxDev),
            _ => None,
        }
    }
}

impl FromStr for JsxEmit {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        JsxEmit::parse(value).ok_or(())
    }
}

impl fmt::Display for JsxEmit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
/// The per-file UTF-8 byte budget enforced before a source is indexed.
///
/// A source at exactly this length is accepted; one byte more is rejected with
/// [`SourcePositionError::SourceTooLarge`].
pub const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

/// Monotonic identities for synthesized AST nodes.
///
/// Constructing the source after the largest parser-assigned id keeps
/// synthesized nodes disjoint from the source tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeIdSource {
    next: u32,
}

impl NodeIdSource {
    #[must_use]
    pub fn after(max_source_id: crate::syntax::NodeId) -> Self {
        Self {
            next: max_source_id
                .get()
                .checked_add(1)
                .expect("node id space exhausted"),
        }
    }

    #[must_use]
    pub fn fresh(&mut self) -> crate::syntax::NodeId {
        let id = crate::syntax::NodeId::new(self.next);
        self.next = self.next.checked_add(1).expect("node id space exhausted");
        id
    }
}

/// A failed checked source-position operation.
///
/// This enum is deliberately closed: callers can exhaustively distinguish an
/// out-of-bounds coordinate from a coordinate that splits one encoded character.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourcePositionError {
    ByteOffsetOutOfBounds { offset: usize, len: usize },
    ByteOffsetInsideCodePoint { offset: usize },
    Utf16PositionOutOfBounds { position: Utf16Pos, len: Utf16Pos },
    Utf16PositionInsideSurrogatePair { position: Utf16Pos },
    RangeStartAfterEnd { start: Utf16Pos, end: Utf16Pos },
    SourceTooLarge { len: usize, limit: usize },
}

impl fmt::Display for SourcePositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ByteOffsetOutOfBounds { offset, len } => {
                write!(
                    formatter,
                    "byte offset {offset} exceeds source length {len}"
                )
            }
            Self::ByteOffsetInsideCodePoint { offset } => {
                write!(formatter, "byte offset {offset} splits a UTF-8 code point")
            }
            Self::Utf16PositionOutOfBounds { position, len } => write!(
                formatter,
                "UTF-16 position {} exceeds source length {}",
                position.get(),
                len.get()
            ),
            Self::Utf16PositionInsideSurrogatePair { position } => write!(
                formatter,
                "UTF-16 position {} splits a surrogate pair",
                position.get()
            ),
            Self::RangeStartAfterEnd { start, end } => write!(
                formatter,
                "range start {} follows range end {}",
                start.get(),
                end.get()
            ),
            Self::SourceTooLarge { len, limit } => write!(
                formatter,
                "source text of {len} bytes exceeds the {limit}-byte per-file budget"
            ),
        }
    }
}

impl std::error::Error for SourcePositionError {}

#[derive(Clone, Copy, Debug)]
struct BoundaryCheckpoint {
    byte: usize,
    utf16: Utf16Pos,
}

/// Immutable source text with checked UTF-8 byte and UTF-16 coordinate mapping.
///
/// The map records only boundaries immediately after non-ASCII code points. Between
/// checkpoints, byte and UTF-16 offsets advance together through ASCII, so binary
/// search plus one subtraction converts either direction without a per-code-point map.
#[derive(Clone, Debug)]
pub struct SourceText {
    text: Arc<str>,
    checkpoints: Arc<[BoundaryCheckpoint]>,
    line_starts: Arc<[Utf16Pos]>,
    utf16_len: Utf16Pos,
    /// Whether this source originated from a `.d.ts`/`.d.mts`/`.d.cts` file.
    is_declaration_file: bool,
}

impl SourceText {
    /// Stores source text and precomputes its immutable position indexes.
    ///
    /// Rejects text longer than [`MAX_SOURCE_BYTES`] before allocating index
    /// storage.
    pub fn new(text: impl Into<Arc<str>>) -> Result<Self, SourcePositionError> {
        Self::from_arc(text.into())
    }

    /// Stores an existing shared source allocation under the same byte budget
    /// as [`SourceText::new`].
    pub fn from_arc(text: Arc<str>) -> Result<Self, SourcePositionError> {
        if text.len() > MAX_SOURCE_BYTES {
            return Err(SourcePositionError::SourceTooLarge {
                len: text.len(),
                limit: MAX_SOURCE_BYTES,
            });
        }
        let mut checkpoints = vec![BoundaryCheckpoint {
            byte: 0,
            utf16: Utf16Pos::ZERO,
        }];
        let mut line_starts = vec![Utf16Pos::ZERO];
        let mut utf16_offset = 0;
        let mut characters = text.char_indices().peekable();

        while let Some((byte_start, character)) = characters.next() {
            utf16_offset += character.len_utf16();

            if !character.is_ascii() {
                checkpoints.push(BoundaryCheckpoint {
                    byte: byte_start + character.len_utf8(),
                    utf16: Utf16Pos::new(utf16_offset),
                });
            }

            let ends_line = character == '\n'
                || (character == '\r' && !matches!(characters.peek(), Some(&(_, '\n'))))
                || character == '\u{2028}'
                || character == '\u{2029}';
            if ends_line {
                line_starts.push(Utf16Pos::new(utf16_offset));
            }
        }

        Ok(Self {
            text,
            checkpoints: Arc::from(checkpoints),
            line_starts: Arc::from(line_starts),
            utf16_len: Utf16Pos::new(utf16_offset),
            is_declaration_file: false,
        })
    }

    /// Marks this source as originating from a `.d.ts`/`.d.mts`/`.d.cts` file.
    #[must_use]
    pub fn with_declaration_file(mut self, value: bool) -> Self {
        self.is_declaration_file = value;
        self
    }

    /// Returns whether this source is a declaration file.
    #[must_use]
    pub const fn is_declaration_file(&self) -> bool {
        self.is_declaration_file
    }

    /// Returns the original UTF-8 source text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.text.as_ref()
    }

    /// Returns the source length in UTF-16 code units.
    #[must_use]
    pub const fn len_utf16(&self) -> Utf16Pos {
        self.utf16_len
    }

    /// Returns whether the source has no code points.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Converts a UTF-8 byte boundary to a UTF-16 boundary.
    pub fn byte_to_utf16(&self, byte_offset: usize) -> Result<Utf16Pos, SourcePositionError> {
        if byte_offset > self.text.len() {
            return Err(SourcePositionError::ByteOffsetOutOfBounds {
                offset: byte_offset,
                len: self.text.len(),
            });
        }
        if !self.text.is_char_boundary(byte_offset) {
            return Err(SourcePositionError::ByteOffsetInsideCodePoint {
                offset: byte_offset,
            });
        }

        let checkpoint_index = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.byte <= byte_offset)
            .saturating_sub(1);
        let checkpoint = &self.checkpoints[checkpoint_index];
        Ok(Utf16Pos::new(
            checkpoint.utf16.get() + (byte_offset - checkpoint.byte),
        ))
    }

    /// Converts a UTF-16 boundary to a UTF-8 byte boundary.
    pub fn utf16_to_byte(&self, position: Utf16Pos) -> Result<usize, SourcePositionError> {
        if position > self.utf16_len {
            return Err(SourcePositionError::Utf16PositionOutOfBounds {
                position,
                len: self.utf16_len,
            });
        }

        let checkpoint_index = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.utf16 <= position)
            .saturating_sub(1);
        let checkpoint = &self.checkpoints[checkpoint_index];
        let byte_offset = checkpoint.byte + (position.get() - checkpoint.utf16.get());

        if !self.text.is_char_boundary(byte_offset) {
            return Err(SourcePositionError::Utf16PositionInsideSurrogatePair { position });
        }

        Ok(byte_offset)
    }

    /// Returns a source-relative, ordered range after validating both endpoints.
    pub fn range(&self, start: Utf16Pos, end: Utf16Pos) -> Result<TextRange, SourcePositionError> {
        self.utf16_to_byte(start)?;
        self.utf16_to_byte(end)?;
        TextRange::new(start, end)
    }

    /// Returns the zero-based line and UTF-16 column of a valid source boundary.
    ///
    /// `\r\n` contributes one line break, with the next line beginning after the
    /// `\n`. A bare `\r`, a bare `\n`, `\u{2028}` (LINE SEPARATOR), and `\u{2029}`
    /// (PARAGRAPH SEPARATOR) each also begin a new line.
    pub fn line_column(&self, position: Utf16Pos) -> Result<(usize, usize), SourcePositionError> {
        self.utf16_to_byte(position)?;

        let line_index = self
            .line_starts
            .partition_point(|line_start| *line_start <= position)
            .saturating_sub(1);
        let line_start = self.line_starts[line_index];
        Ok((line_index, position.get() - line_start.get()))
    }
}

#[cfg(test)]
mod tests {
    use super::{JsxEmit, NodeIdSource, SourcePositionError, SourceText, TextRange, Utf16Pos};

    #[test]
    fn jsx_emit_strings_round_trip_exactly() {
        for (text, emit) in [
            ("preserve", JsxEmit::Preserve),
            ("react", JsxEmit::React),
            ("react-native", JsxEmit::ReactNative),
            ("react-jsx", JsxEmit::ReactJsx),
            ("react-jsxdev", JsxEmit::ReactJsxDev),
        ] {
            assert_eq!(JsxEmit::parse(text), Some(emit));
            assert_eq!(text.parse::<JsxEmit>(), Ok(emit));
            assert_eq!(emit.as_str(), text);
            assert_eq!(emit.to_string(), text);
        }
        assert_eq!(JsxEmit::parse("React"), None);
        assert!("react_jsx".parse::<JsxEmit>().is_err());
    }

    #[test]
    fn synthesized_node_ids_start_after_source_ids_and_advance() {
        let mut ids = NodeIdSource::after(crate::syntax::NodeId::new(40));
        assert_eq!(ids.fresh(), crate::syntax::NodeId::new(41));
        assert_eq!(ids.fresh(), crate::syntax::NodeId::new(42));
    }

    #[test]
    fn ascii_boundaries_round_trip() {
        let source = SourceText::new("hello").expect("test source fits the per-file budget");

        assert_eq!(source.len_utf16(), Utf16Pos::new(5));
        for offset in 0..=5 {
            assert_eq!(source.byte_to_utf16(offset), Ok(Utf16Pos::new(offset)));
            assert_eq!(source.utf16_to_byte(Utf16Pos::new(offset)), Ok(offset));
        }
        assert_eq!(source.line_column(Utf16Pos::new(5)), Ok((0, 5)));
    }

    #[test]
    fn bmp_code_points_preserve_utf16_width_but_not_byte_width() {
        let source = SourceText::new("aé中").expect("test source fits the per-file budget");

        assert_eq!(source.byte_to_utf16(0), Ok(Utf16Pos::new(0)));
        assert_eq!(source.byte_to_utf16(1), Ok(Utf16Pos::new(1)));
        assert_eq!(source.byte_to_utf16(3), Ok(Utf16Pos::new(2)));
        assert_eq!(source.byte_to_utf16(6), Ok(Utf16Pos::new(3)));
        assert_eq!(source.utf16_to_byte(Utf16Pos::new(2)), Ok(3));
        assert_eq!(
            source.byte_to_utf16(2),
            Err(SourcePositionError::ByteOffsetInsideCodePoint { offset: 2 })
        );
    }

    #[test]
    fn astral_code_points_use_two_utf16_units() {
        let source = SourceText::new("a😀b").expect("test source fits the per-file budget");

        assert_eq!(source.byte_to_utf16(1), Ok(Utf16Pos::new(1)));
        assert_eq!(source.byte_to_utf16(5), Ok(Utf16Pos::new(3)));
        assert_eq!(source.byte_to_utf16(6), Ok(Utf16Pos::new(4)));
        assert_eq!(source.utf16_to_byte(Utf16Pos::new(1)), Ok(1));
        assert_eq!(source.utf16_to_byte(Utf16Pos::new(3)), Ok(5));
        assert_eq!(source.utf16_to_byte(Utf16Pos::new(4)), Ok(6));
        assert_eq!(
            source.utf16_to_byte(Utf16Pos::new(2)),
            Err(SourcePositionError::Utf16PositionInsideSurrogatePair {
                position: Utf16Pos::new(2),
            })
        );
    }

    #[test]
    fn combining_marks_each_advance_the_utf16_column() {
        let source = SourceText::new("e\u{301}x").expect("test source fits the per-file budget");

        assert_eq!(source.byte_to_utf16(1), Ok(Utf16Pos::new(1)));
        assert_eq!(source.byte_to_utf16(3), Ok(Utf16Pos::new(2)));
        assert_eq!(source.byte_to_utf16(4), Ok(Utf16Pos::new(3)));
        assert_eq!(source.line_column(Utf16Pos::new(2)), Ok((0, 2)));
    }

    #[test]
    fn crlf_is_one_line_break_and_columns_are_utf16_units() {
        let source = SourceText::new("a\r\n😀\nb").expect("test source fits the per-file budget");

        assert_eq!(source.len_utf16(), Utf16Pos::new(7));
        assert_eq!(source.line_column(Utf16Pos::new(0)), Ok((0, 0)));
        assert_eq!(source.line_column(Utf16Pos::new(2)), Ok((0, 2)));
        assert_eq!(source.line_column(Utf16Pos::new(3)), Ok((1, 0)));
        assert_eq!(source.line_column(Utf16Pos::new(5)), Ok((1, 2)));
        assert_eq!(source.line_column(Utf16Pos::new(6)), Ok((2, 0)));
        assert_eq!(source.line_column(Utf16Pos::new(7)), Ok((2, 1)));
    }

    #[test]
    fn unicode_line_and_paragraph_separators_advance_line_and_column() {
        // "a\u{2028}b\u{2029}c"
        // Offsets in UTF-16 code units:
        // 0: 'a' (len_utf16 = 1)
        // 1: '\u{2028}' (len_utf16 = 1) -> line break after offset 1
        // 2: 'b' (len_utf16 = 1)
        // 3: '\u{2029}' (len_utf16 = 1) -> line break after offset 3
        // 4: 'c' (len_utf16 = 1)
        let source =
            SourceText::new("a\u{2028}b\u{2029}c").expect("test source fits the per-file budget");

        assert_eq!(source.len_utf16(), Utf16Pos::new(5));

        // Before U+2028
        assert_eq!(source.line_column(Utf16Pos::new(0)), Ok((0, 0)));
        assert_eq!(source.line_column(Utf16Pos::new(1)), Ok((0, 1)));

        // After U+2028 / before 'b'
        assert_eq!(source.line_column(Utf16Pos::new(2)), Ok((1, 0)));

        // Before U+2029
        assert_eq!(source.line_column(Utf16Pos::new(3)), Ok((1, 1)));

        // After U+2029 / before 'c'
        assert_eq!(source.line_column(Utf16Pos::new(4)), Ok((2, 0)));
        assert_eq!(source.line_column(Utf16Pos::new(5)), Ok((2, 1)));
    }

    #[test]
    fn empty_and_end_positions_are_valid_boundaries() {
        let empty = SourceText::new("").expect("test source fits the per-file budget");
        assert!(empty.is_empty());
        assert_eq!(empty.byte_to_utf16(0), Ok(Utf16Pos::ZERO));
        assert_eq!(empty.utf16_to_byte(Utf16Pos::ZERO), Ok(0));
        assert_eq!(empty.line_column(Utf16Pos::ZERO), Ok((0, 0)));

        let source = SourceText::new("😀").expect("test source fits the per-file budget");
        assert_eq!(source.byte_to_utf16(4), Ok(Utf16Pos::new(2)));
        assert_eq!(source.utf16_to_byte(Utf16Pos::new(2)), Ok(4));
    }

    #[test]
    fn invalid_byte_and_utf16_offsets_have_distinct_errors() {
        let source = SourceText::new("😀").expect("test source fits the per-file budget");

        assert_eq!(
            source.byte_to_utf16(1),
            Err(SourcePositionError::ByteOffsetInsideCodePoint { offset: 1 })
        );
        assert_eq!(
            source.byte_to_utf16(5),
            Err(SourcePositionError::ByteOffsetOutOfBounds { offset: 5, len: 4 })
        );
        assert_eq!(
            source.utf16_to_byte(Utf16Pos::new(1)),
            Err(SourcePositionError::Utf16PositionInsideSurrogatePair {
                position: Utf16Pos::new(1),
            })
        );
        assert_eq!(
            source.utf16_to_byte(Utf16Pos::new(3)),
            Err(SourcePositionError::Utf16PositionOutOfBounds {
                position: Utf16Pos::new(3),
                len: Utf16Pos::new(2),
            })
        );
    }

    #[test]
    fn ranges_cannot_be_reversed_and_source_ranges_validate_boundaries() {
        assert_eq!(
            TextRange::new(Utf16Pos::new(3), Utf16Pos::new(1)),
            Err(SourcePositionError::RangeStartAfterEnd {
                start: Utf16Pos::new(3),
                end: Utf16Pos::new(1),
            })
        );

        let source = SourceText::new("a😀b").expect("test source fits the per-file budget");
        let range = source
            .range(Utf16Pos::new(1), Utf16Pos::new(3))
            .expect("the emoji's outer boundaries are valid");
        assert_eq!(range.start(), Utf16Pos::new(1));
        assert_eq!(range.end(), Utf16Pos::new(3));
        assert_eq!(range.len(), 2);
        assert_eq!(
            source.range(Utf16Pos::new(3), Utf16Pos::new(1)),
            Err(SourcePositionError::RangeStartAfterEnd {
                start: Utf16Pos::new(3),
                end: Utf16Pos::new(1),
            })
        );
        assert_eq!(
            source.range(Utf16Pos::new(1), Utf16Pos::new(2)),
            Err(SourcePositionError::Utf16PositionInsideSurrogatePair {
                position: Utf16Pos::new(2),
            })
        );
    }

    #[test]
    fn source_text_enforces_byte_budget_before_indexing() {
        let accepted = SourceText::new("a".repeat(super::MAX_SOURCE_BYTES))
            .expect("the exact source budget is accepted");
        assert_eq!(accepted.as_str().len(), super::MAX_SOURCE_BYTES);

        let error = SourceText::new("a".repeat(super::MAX_SOURCE_BYTES + 1))
            .expect_err("one byte above the source budget is rejected");
        assert_eq!(
            error,
            SourcePositionError::SourceTooLarge {
                len: super::MAX_SOURCE_BYTES + 1,
                limit: super::MAX_SOURCE_BYTES,
            }
        );
    }

    #[test]
    fn declaration_file_identity_is_explicit_and_preserved() {
        let ordinary = SourceText::new("declare const value: number;")
            .expect("test source fits the per-file budget");
        assert!(!ordinary.is_declaration_file());
        assert!(ordinary.with_declaration_file(true).is_declaration_file());
    }
}
