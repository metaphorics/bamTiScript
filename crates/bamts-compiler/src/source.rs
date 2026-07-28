use std::{fmt, path::Path, sync::Arc};

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
}

impl SourceText {
    /// Stores source text and precomputes its immutable position indexes.
    #[must_use]
    pub fn new(text: impl Into<Arc<str>>) -> Self {
        Self::from_arc(text.into())
    }

    /// Stores an existing shared source allocation without copying its text.
    #[must_use]
    pub fn from_arc(text: Arc<str>) -> Self {
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

        Self {
            text,
            checkpoints: Arc::from(checkpoints),
            line_starts: Arc::from(line_starts),
            utf16_len: Utf16Pos::new(utf16_offset),
        }
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
    use super::{SourcePositionError, SourceText, TextRange, Utf16Pos};

    #[test]
    fn ascii_boundaries_round_trip() {
        let source = SourceText::new("hello");

        assert_eq!(source.len_utf16(), Utf16Pos::new(5));
        for offset in 0..=5 {
            assert_eq!(source.byte_to_utf16(offset), Ok(Utf16Pos::new(offset)));
            assert_eq!(source.utf16_to_byte(Utf16Pos::new(offset)), Ok(offset));
        }
        assert_eq!(source.line_column(Utf16Pos::new(5)), Ok((0, 5)));
    }

    #[test]
    fn bmp_code_points_preserve_utf16_width_but_not_byte_width() {
        let source = SourceText::new("aé中");

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
        let source = SourceText::new("a😀b");

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
        let source = SourceText::new("e\u{301}x");

        assert_eq!(source.byte_to_utf16(1), Ok(Utf16Pos::new(1)));
        assert_eq!(source.byte_to_utf16(3), Ok(Utf16Pos::new(2)));
        assert_eq!(source.byte_to_utf16(4), Ok(Utf16Pos::new(3)));
        assert_eq!(source.line_column(Utf16Pos::new(2)), Ok((0, 2)));
    }

    #[test]
    fn crlf_is_one_line_break_and_columns_are_utf16_units() {
        let source = SourceText::new("a\r\n😀\nb");

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
        let source = SourceText::new("a\u{2028}b\u{2029}c");

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
        let empty = SourceText::new("");
        assert!(empty.is_empty());
        assert_eq!(empty.byte_to_utf16(0), Ok(Utf16Pos::ZERO));
        assert_eq!(empty.utf16_to_byte(Utf16Pos::ZERO), Ok(0));
        assert_eq!(empty.line_column(Utf16Pos::ZERO), Ok((0, 0)));

        let source = SourceText::new("😀");
        assert_eq!(source.byte_to_utf16(4), Ok(Utf16Pos::new(2)));
        assert_eq!(source.utf16_to_byte(Utf16Pos::new(2)), Ok(4));
    }

    #[test]
    fn invalid_byte_and_utf16_offsets_have_distinct_errors() {
        let source = SourceText::new("😀");

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

        let source = SourceText::new("a😀b");
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
}
