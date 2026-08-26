//! Deterministic Source Map v3 construction for emitted output.
//!
//! [`SourceMapBuilder`] accumulates generated-to-original position mappings as
//! the emitter prints, interns `sources` and `names` in first-encounter order,
//! and finalizes into a [`SourceMap`] whose `mappings` field is the canonical
//! Base64 VLQ encoding described by the Source Map v3 specification.
//!
//! # Guarantees
//! * **Deterministic.** Interning follows first-encounter order and mappings are
//!   ordered by generated position with a stable tie-break, so the same emit
//!   always produces byte-identical JSON. No unordered container reaches output.
//! * **UTF-16 columns.** Every line and column is a zero-based UTF-16 code-unit
//!   coordinate, matching [`Utf16Pos`] and the specification's column units. The
//!   builder never re-measures printed text, so generated offsets stay exactly
//!   what the printer reported.
//! * **Lossless VLQ.** [`encode_vlq`] and [`decode_vlq`] round-trip every `i32`
//!   that fits the specification's continuation encoding, including negatives
//!   and multi-digit values.
//! * **Validated.** [`SourceMap::validate`] rejects a map whose mapping refers
//!   to an out-of-range source or name, or whose `sourcesContent` length does
//!   not match `sources`; the builder cannot construct such a map.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::source::{SourcePositionError, SourceText, Utf16Pos};

/// The Base64 alphabet used by Source Map v3 VLQ digits and inline payloads.
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The continuation bit of a Base64 VLQ digit.
const VLQ_CONTINUATION: u32 = 0b10_0000;

/// The value mask of a Base64 VLQ digit.
const VLQ_VALUE_MASK: u32 = 0b01_1111;

/// The number of value bits carried by one Base64 VLQ digit.
const VLQ_SHIFT: u32 = 5;

/// A zero-based line and UTF-16 column in either the generated or an original file.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LineColumn {
    /// The zero-based line index.
    pub line: usize,
    /// The zero-based column measured in UTF-16 code units.
    pub column: usize,
}

impl LineColumn {
    /// The first position of the first line.
    pub const ZERO: Self = Self { line: 0, column: 0 };

    /// Creates a position from a zero-based line and UTF-16 column.
    #[must_use]
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

/// The original-file half of a mapping.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OriginalPosition {
    /// The index into [`SourceMap::sources`].
    pub source: usize,
    /// The original position.
    pub position: LineColumn,
    /// The optional index into [`SourceMap::names`].
    pub name: Option<usize>,
}

/// One generated-to-original correspondence.
///
/// A mapping with no [`OriginalPosition`] marks a generated position that has no
/// original counterpart, which the specification encodes as a one-field segment.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Mapping {
    /// The position in the generated file.
    pub generated: LineColumn,
    /// The original position, when the generated text came from a source.
    pub original: Option<OriginalPosition>,
}

/// Why a [`SourceMap`] is not internally consistent.
///
/// This enum is deliberately closed so callers can distinguish a dangling source
/// index from a dangling name index without string matching.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceMapError {
    /// A mapping refers to a source index that `sources` does not contain.
    SourceIndexOutOfRange {
        /// The offending mapping's index.
        mapping: usize,
        /// The referenced source index.
        index: usize,
        /// The number of registered sources.
        sources: usize,
    },
    /// A mapping refers to a name index that `names` does not contain.
    NameIndexOutOfRange {
        /// The offending mapping's index.
        mapping: usize,
        /// The referenced name index.
        index: usize,
        /// The number of registered names.
        names: usize,
    },
    /// `sourcesContent` is present but does not have one entry per source.
    SourcesContentLengthMismatch {
        /// The number of registered sources.
        sources: usize,
        /// The number of content entries.
        contents: usize,
    },
}

/// A finalized Source Map v3 document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceMap {
    file: Option<String>,
    source_root: Option<String>,
    sources: Vec<String>,
    sources_content: Option<Vec<Option<String>>>,
    names: Vec<String>,
    mappings: Vec<Mapping>,
}

impl SourceMap {
    /// Returns the generated file name recorded in the `file` field.
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        self.file.as_deref()
    }

    /// Returns the `sourceRoot` prefix, when one was configured.
    #[must_use]
    pub fn source_root(&self) -> Option<&str> {
        self.source_root.as_deref()
    }

    /// Returns the original source paths in mapping-index order.
    #[must_use]
    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    /// Returns the embedded original texts, when `sourcesContent` is present.
    #[must_use]
    pub fn sources_content(&self) -> Option<&[Option<String>]> {
        self.sources_content.as_deref()
    }

    /// Returns the interned identifier names in mapping-index order.
    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Returns the mappings ordered by generated position.
    #[must_use]
    pub fn mappings(&self) -> &[Mapping] {
        &self.mappings
    }

    /// Checks that every mapping index resolves and `sourcesContent` matches.
    pub fn validate(&self) -> Result<(), SourceMapError> {
        if let Some(contents) = &self.sources_content
            && contents.len() != self.sources.len()
        {
            return Err(SourceMapError::SourcesContentLengthMismatch {
                sources: self.sources.len(),
                contents: contents.len(),
            });
        }

        for (index, mapping) in self.mappings.iter().enumerate() {
            let Some(original) = mapping.original else {
                continue;
            };
            if original.source >= self.sources.len() {
                return Err(SourceMapError::SourceIndexOutOfRange {
                    mapping: index,
                    index: original.source,
                    sources: self.sources.len(),
                });
            }
            if let Some(name) = original.name
                && name >= self.names.len()
            {
                return Err(SourceMapError::NameIndexOutOfRange {
                    mapping: index,
                    index: name,
                    names: self.names.len(),
                });
            }
        }

        Ok(())
    }

    /// Encodes the `mappings` field as Base64 VLQ segment groups.
    ///
    /// Groups are separated by `;` for each generated line up to the last mapped
    /// line; segments within a line are separated by `,`. Generated columns reset
    /// per line while source, original line, original column, and name deltas
    /// carry across lines, exactly as the specification requires.
    #[must_use]
    pub fn encode_mappings(&self) -> String {
        let mut encoded = String::new();
        let mut previous_source: i64 = 0;
        let mut previous_source_line: i64 = 0;
        let mut previous_source_column: i64 = 0;
        let mut previous_name: i64 = 0;
        let mut current_line: usize = 0;
        let mut previous_generated_column: i64 = 0;
        let mut first_on_line = true;

        for mapping in &self.mappings {
            while current_line < mapping.generated.line {
                encoded.push(';');
                current_line += 1;
                previous_generated_column = 0;
                first_on_line = true;
            }

            if first_on_line {
                first_on_line = false;
            } else {
                encoded.push(',');
            }

            let generated_column = as_i64(mapping.generated.column);
            encode_vlq(generated_column - previous_generated_column, &mut encoded);
            previous_generated_column = generated_column;

            let Some(original) = mapping.original else {
                continue;
            };

            let source = as_i64(original.source);
            encode_vlq(source - previous_source, &mut encoded);
            previous_source = source;

            let source_line = as_i64(original.position.line);
            encode_vlq(source_line - previous_source_line, &mut encoded);
            previous_source_line = source_line;

            let source_column = as_i64(original.position.column);
            encode_vlq(source_column - previous_source_column, &mut encoded);
            previous_source_column = source_column;

            if let Some(name) = original.name {
                let name = as_i64(name);
                encode_vlq(name - previous_name, &mut encoded);
                previous_name = name;
            }
        }

        encoded
    }

    /// Serializes the map as Source Map v3 JSON with a fixed key order.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut json = String::from("{\"version\":3");

        if let Some(file) = &self.file {
            json.push_str(",\"file\":");
            push_json_string(file, &mut json);
        }
        if let Some(source_root) = &self.source_root {
            json.push_str(",\"sourceRoot\":");
            push_json_string(source_root, &mut json);
        }

        json.push_str(",\"sources\":[");
        for (index, source) in self.sources.iter().enumerate() {
            if index > 0 {
                json.push(',');
            }
            push_json_string(source, &mut json);
        }
        json.push(']');

        if let Some(contents) = &self.sources_content {
            json.push_str(",\"sourcesContent\":[");
            for (index, content) in contents.iter().enumerate() {
                if index > 0 {
                    json.push(',');
                }
                match content {
                    Some(text) => push_json_string(text, &mut json),
                    None => json.push_str("null"),
                }
            }
            json.push(']');
        }

        json.push_str(",\"names\":[");
        for (index, name) in self.names.iter().enumerate() {
            if index > 0 {
                json.push(',');
            }
            push_json_string(name, &mut json);
        }
        json.push(']');

        json.push_str(",\"mappings\":");
        push_json_string(&self.encode_mappings(), &mut json);
        json.push('}');
        json
    }

    /// Returns the `//# sourceMappingURL=<path>` comment for an external map.
    #[must_use]
    pub fn url_comment(path: &str) -> String {
        format!("//# sourceMappingURL={path}")
    }

    /// Returns the `//# sourceMappingURL=` comment carrying this map inline.
    #[must_use]
    pub fn inline_comment(&self) -> String {
        format!(
            "//# sourceMappingURL=data:application/json;base64,{}",
            encode_base64(self.to_json().as_bytes())
        )
    }
}

/// Accumulates mappings during emit and finalizes a [`SourceMap`].
#[derive(Clone, Debug, Default)]
pub struct SourceMapBuilder {
    file: Option<String>,
    source_root: Option<String>,
    sources: Vec<String>,
    source_index: BTreeMap<String, usize>,
    sources_content: Vec<Option<String>>,
    include_sources_content: bool,
    names: Vec<String>,
    name_index: BTreeMap<String, usize>,
    mappings: Vec<Mapping>,
}

impl SourceMapBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the generated file name written to the `file` field.
    #[must_use]
    pub fn with_file(mut self, file: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// Records the `sourceRoot` prefix.
    #[must_use]
    pub fn with_source_root(mut self, source_root: impl Into<String>) -> Self {
        self.source_root = Some(source_root.into());
        self
    }

    /// Requests a `sourcesContent` array in the finalized map.
    #[must_use]
    pub const fn with_sources_content(mut self, include: bool) -> Self {
        self.include_sources_content = include;
        self
    }

    /// Interns `path`, returning its stable index in first-encounter order.
    pub fn intern_source(&mut self, path: &str) -> usize {
        if let Some(index) = self.source_index.get(path) {
            return *index;
        }
        let index = self.sources.len();
        self.sources.push(path.to_owned());
        self.sources_content.push(None);
        self.source_index.insert(path.to_owned(), index);
        index
    }

    /// Interns `path` and attaches its original text for `sourcesContent`.
    ///
    /// A later call replaces previously attached text for the same path, so the
    /// finalized content always reflects the text the emitter actually read.
    pub fn intern_source_with_content(&mut self, path: &str, content: &str) -> usize {
        let index = self.intern_source(path);
        self.sources_content[index] = Some(content.to_owned());
        index
    }

    /// Interns `name`, returning its stable index in first-encounter order.
    pub fn intern_name(&mut self, name: &str) -> usize {
        if let Some(index) = self.name_index.get(name) {
            return *index;
        }
        let index = self.names.len();
        self.names.push(name.to_owned());
        self.name_index.insert(name.to_owned(), index);
        index
    }

    /// Records a generated position that has no original counterpart.
    pub fn add_generated_only(&mut self, generated: LineColumn) {
        self.mappings.push(Mapping {
            generated,
            original: None,
        });
    }

    /// Records a generated position that came from `source` at `original`.
    ///
    /// `source` is interned in first-encounter order. `name`, when present, is
    /// interned the same way and stored as the mapping's optional name index.
    pub fn add_mapping(
        &mut self,
        source: &str,
        generated: LineColumn,
        original: LineColumn,
        name: Option<&str>,
    ) {
        let source = self.intern_source(source);
        let name = name.map(|name| self.intern_name(name));
        self.mappings.push(Mapping {
            generated,
            original: Some(OriginalPosition {
                source,
                position: original,
                name,
            }),
        });
    }

    /// Records a mapping from UTF-16 offsets, converting through [`SourceText::line_column`].
    ///
    /// Generated and original columns are the printer's UTF-16 coordinates, not
    /// recomputed byte lengths, so a non-ASCII original keeps the same column the
    /// emitter reported.
    pub fn add_mapping_utf16(
        &mut self,
        source: &str,
        generated_text: &SourceText,
        generated: Utf16Pos,
        original_text: &SourceText,
        original: Utf16Pos,
        name: Option<&str>,
    ) -> Result<(), SourcePositionError> {
        let (generated_line, generated_column) = generated_text.line_column(generated)?;
        let (original_line, original_column) = original_text.line_column(original)?;
        self.add_mapping(
            source,
            LineColumn::new(generated_line, generated_column),
            LineColumn::new(original_line, original_column),
            name,
        );
        Ok(())
    }

    /// Returns the number of accumulated mappings.
    #[must_use]
    pub fn mapping_count(&self) -> usize {
        self.mappings.len()
    }

    /// Orders and deduplicates the mappings, producing the final map.
    ///
    /// Mappings are sorted by generated position with the original position as a
    /// total tie-break, then exact duplicates are collapsed. The result always
    /// satisfies [`SourceMap::validate`].
    #[must_use]
    pub fn finish(mut self) -> SourceMap {
        self.mappings.sort_unstable();
        self.mappings.dedup();

        let sources_content = if self.include_sources_content {
            Some(self.sources_content)
        } else {
            None
        };

        SourceMap {
            file: self.file,
            source_root: self.source_root,
            sources: self.sources,
            sources_content,
            names: self.names,
            mappings: self.mappings,
        }
    }
}

/// Converts a UTF-16 source coordinate into a mapping column on `line`.
#[must_use]
pub const fn column_of(line: usize, position: Utf16Pos, line_start: Utf16Pos) -> LineColumn {
    LineColumn {
        line,
        column: position.get().saturating_sub(line_start.get()),
    }
}

/// Appends the Base64 VLQ digits of `value` to `out`.
pub fn encode_vlq(value: i64, out: &mut String) {
    // The sign occupies the low bit. Values are expected to fit an `i32` mapping
    // coordinate; `unsigned_abs` keeps `i64::MIN` defined, and the shift is
    // performed in `u128` so a full-width magnitude never wraps.
    let magnitude = u128::from(value.unsigned_abs());
    let mut remaining = if value < 0 {
        (magnitude << 1) | 1
    } else {
        magnitude << 1
    };

    loop {
        let mut digit = u32::try_from(remaining & u128::from(VLQ_VALUE_MASK)).unwrap_or(0);
        remaining >>= VLQ_SHIFT;
        if remaining > 0 {
            digit |= VLQ_CONTINUATION;
        }
        out.push(char::from(BASE64[digit as usize]));
        if remaining == 0 {
            return;
        }
    }
}

/// Decodes one Base64 VLQ value from `input`, returning it and the digits consumed.
///
/// Returns `None` when `input` does not begin with a complete VLQ value, so a
/// truncated or non-Base64 segment is rejected rather than silently clamped.
#[must_use]
pub fn decode_vlq(input: &str) -> Option<(i64, usize)> {
    let mut result: u128 = 0;
    let mut shift: u32 = 0;
    let mut consumed = 0usize;

    for byte in input.bytes() {
        let digit = base64_value(byte)?;
        consumed += 1;
        let value = u128::from(digit & VLQ_VALUE_MASK);
        let shifted = value.checked_shl(shift)?;
        result |= shifted;
        if digit & VLQ_CONTINUATION == 0 {
            let magnitude = result >> 1;
            let signed = i64::try_from(magnitude).ok()?;
            let value = if result & 1 == 1 { -signed } else { signed };
            return Some((value, consumed));
        }
        shift += VLQ_SHIFT;
    }

    None
}

/// Returns the numeric value of one Base64 digit.
fn base64_value(byte: u8) -> Option<u32> {
    let value = match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(value))
}

/// Encodes `bytes` as padded standard Base64.
#[must_use]
pub fn encode_base64(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).copied().map_or(0, u32::from);
        let third = chunk.get(2).copied().map_or(0, u32::from);
        let packed = (first << 16) | (second << 8) | third;

        encoded.push(char::from(BASE64[(packed >> 18) as usize & 0x3F]));
        encoded.push(char::from(BASE64[(packed >> 12) as usize & 0x3F]));
        if chunk.len() > 1 {
            encoded.push(char::from(BASE64[(packed >> 6) as usize & 0x3F]));
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(char::from(BASE64[packed as usize & 0x3F]));
        } else {
            encoded.push('=');
        }
    }
    encoded
}

/// Widens a `usize` coordinate for delta arithmetic without wrapping.
fn as_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Appends `value` as a JSON string literal, escaping control characters.
fn push_json_string(value: &str, out: &mut String) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            control if control < ' ' => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::{
        LineColumn, SourceMapBuilder, SourceMapError, decode_vlq, encode_base64, encode_vlq,
    };
    use crate::emitter::transpile::transpile_text;
    use crate::emitter::{EmitFileNames, EmitOptions, codes};
    use crate::source::{ScriptKind, SourceId};
    use crate::source::{SourceText, Utf16Pos};
    use std::sync::Arc;

    fn emit(source: &str, options: &EmitOptions) -> crate::emitter::EmitOutput {
        transpile_text(
            SourceId::new(1),
            ScriptKind::TypeScript,
            Arc::from(source),
            options,
            &EmitFileNames {
                source_name: Arc::from("input.ts"),
                js_file_name: Some(Arc::from("output.js")),
                declaration_file_name: Some(Arc::from("output.d.ts")),
                source_root: None,
            },
        )
    }

    fn vlq(value: i64) -> String {
        let mut encoded = String::new();
        encode_vlq(value, &mut encoded);
        encoded
    }

    #[test]
    fn vlq_matches_specification_vectors() {
        assert_eq!(vlq(0), "A");
        assert_eq!(vlq(1), "C");
        assert_eq!(vlq(-1), "D");
        assert_eq!(vlq(15), "e");
        assert_eq!(vlq(16), "gB");
        assert_eq!(vlq(-16), "hB");
        assert_eq!(vlq(123_456), "gkxH");
    }

    #[test]
    fn vlq_round_trips_including_negatives_and_multi_digit() {
        for value in [
            0,
            1,
            -1,
            15,
            16,
            -16,
            511,
            -512,
            123_456,
            -123_456,
            i64::from(i32::MAX),
            i64::from(i32::MIN),
        ] {
            let encoded = vlq(value);
            assert_eq!(
                decode_vlq(&encoded),
                Some((value, encoded.len())),
                "round trip failed for {value}"
            );
        }
    }

    #[test]
    fn decode_rejects_truncated_and_non_base64_input() {
        // `g` sets the continuation bit with no following digit.
        assert_eq!(decode_vlq("g"), None);
        assert_eq!(decode_vlq(""), None);
        assert_eq!(decode_vlq("-"), None);
        assert_eq!(decode_vlq("!A"), None);
    }

    #[test]
    fn mappings_encode_relative_deltas_and_reset_generated_column_per_line() {
        let mut builder = SourceMapBuilder::new();
        builder.add_mapping("a.ts", LineColumn::new(0, 0), LineColumn::new(0, 0), None);
        builder.add_mapping("a.ts", LineColumn::new(0, 4), LineColumn::new(0, 4), None);
        builder.add_mapping("a.ts", LineColumn::new(1, 0), LineColumn::new(1, 0), None);
        let map = builder.finish();

        // Line 0: [0,0,0,0] then delta [+4,0,0,+4]; line 1: column resets, source
        // line advances by one and the original column returns from 4 to 0.
        assert_eq!(map.encode_mappings(), "AAAA,IAAI;AACJ");
    }

    #[test]
    fn generated_only_segment_encodes_a_single_field() {
        let mut builder = SourceMapBuilder::new();
        builder.add_generated_only(LineColumn::new(0, 0));
        builder.add_generated_only(LineColumn::new(0, 3));
        let map = builder.finish();

        assert_eq!(map.encode_mappings(), "A,G");
        assert!(map.sources().is_empty());
    }

    #[test]
    fn unmapped_generated_lines_emit_empty_groups() {
        let mut builder = SourceMapBuilder::new();
        builder.add_mapping("a.ts", LineColumn::new(0, 0), LineColumn::new(0, 0), None);
        builder.add_mapping("a.ts", LineColumn::new(3, 0), LineColumn::new(1, 0), None);
        let map = builder.finish();

        let encoded = map.encode_mappings();
        assert_eq!(encoded.matches(';').count(), 3, "encoded: {encoded}");
        assert!(encoded.starts_with("AAAA;;;"), "encoded: {encoded}");
        assert_eq!(encoded, "AAAA;;;AACA");
    }

    #[test]
    fn interning_follows_first_encounter_order_and_deduplicates() {
        let mut builder = SourceMapBuilder::new();
        builder.add_mapping(
            "second.ts",
            LineColumn::new(0, 0),
            LineColumn::new(0, 0),
            Some("beta"),
        );
        builder.add_mapping(
            "first.ts",
            LineColumn::new(1, 0),
            LineColumn::new(0, 0),
            Some("alpha"),
        );
        builder.add_mapping(
            "second.ts",
            LineColumn::new(2, 0),
            LineColumn::new(0, 0),
            Some("beta"),
        );
        let map = builder.finish();

        assert_eq!(map.sources(), ["second.ts", "first.ts"]);
        assert_eq!(map.names(), ["beta", "alpha"]);
    }

    #[test]
    fn duplicate_mappings_collapse_and_order_is_by_generated_position() {
        let mut builder = SourceMapBuilder::new();
        builder.add_mapping("a.ts", LineColumn::new(2, 0), LineColumn::new(2, 0), None);
        builder.add_mapping("a.ts", LineColumn::new(0, 0), LineColumn::new(0, 0), None);
        builder.add_mapping("a.ts", LineColumn::new(2, 0), LineColumn::new(2, 0), None);
        let map = builder.finish();

        assert_eq!(map.mappings().len(), 2);
        assert_eq!(map.mappings()[0].generated, LineColumn::new(0, 0));
        assert_eq!(map.mappings()[1].generated, LineColumn::new(2, 0));
    }

    #[test]
    fn utf16_columns_are_preserved_verbatim() {
        // `"é"` is one UTF-16 unit but two UTF-8 bytes; the builder must record
        // exactly the column it was given rather than re-measuring bytes.
        let mut builder = SourceMapBuilder::new();
        builder.add_mapping("a.ts", LineColumn::new(0, 1), LineColumn::new(0, 1), None);
        let map = builder.finish();

        assert_eq!(map.mappings()[0].generated.column, 1);
        assert_eq!(
            map.mappings()[0]
                .original
                .expect("original")
                .position
                .column,
            1
        );
    }

    #[test]
    fn utf16_offsets_convert_through_source_text_not_bytes() {
        let original = SourceText::new("a\u{e9}c").expect("static test source fits size limit");
        let generated = SourceText::new("a\u{e9}c").expect("static test source fits size limit");
        let pos = Utf16Pos::new(1);
        assert_eq!(original.line_column(pos), Ok((0, 1)));
        assert_eq!(original.utf16_to_byte(pos), Ok(1));

        let mut builder = SourceMapBuilder::new();
        builder
            .add_mapping_utf16("a.ts", &generated, pos, &original, pos, Some("c"))
            .expect("valid offsets");
        let map = builder.finish();
        assert_eq!(map.mappings()[0].generated, LineColumn::new(0, 1));
        assert_eq!(
            map.mappings()[0].original.expect("original").position,
            LineColumn::new(0, 1)
        );
    }

    #[test]
    fn json_has_fixed_key_order_and_escapes_content() {
        let mut builder = SourceMapBuilder::new()
            .with_file("out.js")
            .with_source_root("/root")
            .with_sources_content(true);
        builder.intern_source_with_content("a.ts", "const x = \"q\";\n");
        builder.add_mapping("a.ts", LineColumn::new(0, 0), LineColumn::new(0, 0), None);
        let json = builder.finish().to_json();

        assert!(json.starts_with("{\"version\":3,\"file\":\"out.js\",\"sourceRoot\":\"/root\","));
        assert!(json.contains("\"sourcesContent\":[\"const x = \\\"q\\\";\\n\"]"));
        assert!(json.contains("\"names\":[]"));
        assert!(json.ends_with("\"mappings\":\"AAAA\"}"));
    }

    #[test]
    fn sources_content_is_absent_unless_requested_and_null_when_unread() {
        let mut without = SourceMapBuilder::new();
        without.intern_source("a.ts");
        assert!(without.finish().sources_content().is_none());

        let mut with = SourceMapBuilder::new().with_sources_content(true);
        with.intern_source("unread.ts");
        let map = with.finish();
        assert_eq!(map.sources_content(), Some(&[None][..]));
        assert!(map.to_json().contains("\"sourcesContent\":[null]"));
    }

    #[test]
    fn builder_output_is_deterministic_across_identical_runs() {
        let build = || {
            let mut builder = SourceMapBuilder::new()
                .with_file("out.js")
                .with_sources_content(true);
            builder.intern_source_with_content("b.ts", "b");
            builder.add_mapping(
                "a.ts",
                LineColumn::new(1, 2),
                LineColumn::new(3, 4),
                Some("n"),
            );
            builder.add_mapping("b.ts", LineColumn::new(0, 0), LineColumn::new(0, 0), None);
            builder.finish().to_json()
        };

        assert_eq!(build(), build());
    }

    #[test]
    fn finished_maps_always_validate() {
        let mut builder = SourceMapBuilder::new();
        builder.add_mapping(
            "a.ts",
            LineColumn::new(0, 0),
            LineColumn::new(0, 0),
            Some("x"),
        );
        builder.add_generated_only(LineColumn::new(1, 0));
        assert_eq!(builder.finish().validate(), Ok(()));
    }

    #[test]
    fn validate_reports_sources_content_length_mismatch() {
        let mut builder = SourceMapBuilder::new().with_sources_content(true);
        builder.intern_source("a.ts");
        let mut map = builder.finish();

        let json_before = map.to_json();
        assert!(json_before.contains("\"sourcesContent\":[null]"));

        map = super::SourceMap {
            sources_content: Some(Vec::new()),
            ..map
        };
        assert_eq!(
            map.validate(),
            Err(SourceMapError::SourcesContentLengthMismatch {
                sources: 1,
                contents: 0,
            })
        );
    }

    #[test]
    fn validate_reports_dangling_source_and_name_indexes() {
        let mut builder = SourceMapBuilder::new();
        builder.add_mapping(
            "a.ts",
            LineColumn::new(0, 0),
            LineColumn::new(0, 0),
            Some("x"),
        );
        let map = builder.finish();

        let dangling_source = super::SourceMap {
            sources: Vec::new(),
            ..map.clone()
        };
        assert_eq!(
            dangling_source.validate(),
            Err(SourceMapError::SourceIndexOutOfRange {
                mapping: 0,
                index: 0,
                sources: 0,
            })
        );

        let dangling_name = super::SourceMap {
            names: Vec::new(),
            ..map
        };
        assert_eq!(
            dangling_name.validate(),
            Err(SourceMapError::NameIndexOutOfRange {
                mapping: 0,
                index: 0,
                names: 0,
            })
        );
    }

    #[test]
    fn add_mapping_utf16_rejects_out_of_range_offsets() {
        let source = SourceText::new("ab").expect("static test source fits size limit");
        let mut builder = SourceMapBuilder::new();
        let error = builder
            .add_mapping_utf16(
                "a.ts",
                &source,
                Utf16Pos::new(3),
                &source,
                Utf16Pos::ZERO,
                None,
            )
            .expect_err("offset 3 is past len 2");
        assert!(matches!(
            error,
            crate::source::SourcePositionError::Utf16PositionOutOfBounds { .. }
        ));
    }

    #[test]
    fn base64_matches_known_vectors_with_padding() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn inline_comment_carries_the_json_payload() {
        let mut builder = SourceMapBuilder::new().with_file("out.js");
        builder.add_mapping("a.ts", LineColumn::new(0, 0), LineColumn::new(0, 0), None);
        let map = builder.finish();

        let comment = map.inline_comment();
        let payload = comment
            .strip_prefix("//# sourceMappingURL=data:application/json;base64,")
            .expect("inline prefix");
        assert_eq!(payload, encode_base64(map.to_json().as_bytes()));
    }

    #[test]
    fn url_comment_names_an_external_map() {
        assert_eq!(
            super::SourceMap::url_comment("out.js.map"),
            "//# sourceMappingURL=out.js.map"
        );
    }
    #[test]
    fn inline_only_option_builds_and_embeds_a_map() {
        let output = emit(
            "const value: number = 1;",
            &EmitOptions {
                inline_source_map: true,
                ..EmitOptions::default()
            },
        );
        let javascript = output.javascript.expect("javascript output");
        assert!(javascript.source_map.is_some());
        assert!(
            javascript
                .code
                .contains("//# sourceMappingURL=data:application/json;base64,")
        );
    }

    #[test]
    fn external_and_declaration_maps_are_linked_from_outputs() {
        let output = emit(
            "export const value: number = 1;",
            &EmitOptions {
                source_map: true,
                declaration: true,
                declaration_map: true,
                ..EmitOptions::default()
            },
        );
        assert!(
            output
                .javascript
                .expect("javascript output")
                .code
                .contains("//# sourceMappingURL=output.js.map")
        );
        assert!(
            output
                .declaration
                .expect("declaration output")
                .code
                .contains("//# sourceMappingURL=output.d.ts.map")
        );
    }

    #[test]
    fn conflicting_source_map_options_are_diagnosed_without_double_emit() {
        let output = emit(
            "const value = 1;",
            &EmitOptions {
                source_map: true,
                inline_source_map: true,
                ..EmitOptions::default()
            },
        );
        assert!(
            output
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == codes::INVALID_OPTION_VALUE)
        );
        let javascript = output.javascript.expect("javascript output");
        assert!(javascript.code.contains("data:application/json;base64,"));
        assert!(!javascript.code.contains("output.js.map"));
    }
}
