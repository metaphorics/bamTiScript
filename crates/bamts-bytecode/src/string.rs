use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::ops::Range;
use std::sync::Arc;

/// An immutable ECMAScript string represented exactly as UTF-16 code units.
///
/// Unlike Rust's [`str`], this type preserves every `u16` sequence, including
/// unpaired surrogate code units. Converting to UTF-8 is therefore explicit.
#[derive(Clone, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EcmaString(Arc<[u16]>);

impl EcmaString {
    /// Encodes a well-formed UTF-8 string as ECMAScript UTF-16 code units.
    #[must_use]
    pub fn from_utf8(value: &str) -> Self {
        Self(Arc::from(value.encode_utf16().collect::<Vec<_>>()))
    }

    /// Copies exact UTF-16 code units, including unpaired surrogates.
    #[must_use]
    pub fn from_units(units: &[u16]) -> Self {
        Self(Arc::from(units))
    }

    /// Decodes little-endian UTF-16 code units from a checked wire slice.
    #[must_use]
    pub(crate) fn from_le_bytes(bytes: &[u8]) -> Self {
        debug_assert!(bytes.len().is_multiple_of(2));
        Self(
            bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Arc<[u16]>>(),
        )
    }

    /// Returns the exact UTF-16 code units.
    #[must_use]
    pub fn as_units(&self) -> &[u16] {
        &self.0
    }

    /// Returns the number of UTF-16 code units.
    #[must_use]
    pub fn len_units(&self) -> usize {
        self.0.len()
    }

    /// Returns whether this string has no UTF-16 code units.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns a code unit at `offset`, if present.
    #[must_use]
    pub fn unit_at(&self, offset: usize) -> Option<u16> {
        self.0.get(offset).copied()
    }

    /// Returns a copy of the requested code-unit range.
    ///
    /// This deliberately permits ranges that split a surrogate pair, matching
    /// ECMAScript's code-unit indexing semantics.
    #[must_use]
    pub fn slice_units(&self, range: Range<usize>) -> Self {
        Self::from_units(&self.0[range])
    }

    /// Returns whether every surrogate code unit is part of a valid pair.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.first_ill_formed_offset().is_none()
    }

    /// Compares with an ASCII string without allocating.
    #[must_use]
    pub fn eq_ascii(&self, value: &str) -> bool {
        value.is_ascii()
            && self.0.len() == value.len()
            && self
                .0
                .iter()
                .zip(value.bytes())
                .all(|(&unit, byte)| unit == u16::from(byte))
    }

    /// Iterates decoded code points with their UTF-16 code-unit offsets.
    ///
    /// Valid surrogate pairs yield one supplementary code point. Unpaired
    /// surrogates yield their raw code-unit values.
    pub fn code_points(&self) -> impl Iterator<Item = (usize, u32)> + '_ {
        let mut offset = 0;
        std::iter::from_fn(move || {
            let unit = *self.0.get(offset)?;
            let current_offset = offset;
            offset += 1;
            if is_high_surrogate(unit)
                && let Some(&low) = self.0.get(offset)
                && is_low_surrogate(low)
            {
                offset += 1;
                Some((
                    current_offset,
                    0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00),
                ))
            } else {
                Some((current_offset, u32::from(unit)))
            }
        })
    }

    /// Converts to UTF-8, rejecting the first unpaired surrogate.
    ///
    /// # Errors
    ///
    /// Returns [`IllFormedUtf16`] with the offset of the first unpaired
    /// surrogate code unit.
    pub fn to_utf8_strict(&self) -> Result<String, IllFormedUtf16> {
        if let Some(unit_offset) = self.first_ill_formed_offset() {
            return Err(IllFormedUtf16 { unit_offset });
        }
        String::from_utf16(&self.0).map_err(|_| unreachable!("UTF-16 was validated"))
    }

    /// Converts to UTF-8, replacing each unpaired surrogate with U+FFFD.
    #[must_use]
    pub fn to_utf8_lossy(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }

    fn first_ill_formed_offset(&self) -> Option<usize> {
        let mut offset = 0;
        while let Some(&unit) = self.0.get(offset) {
            if is_high_surrogate(unit) {
                if self
                    .0
                    .get(offset + 1)
                    .is_some_and(|&next| is_low_surrogate(next))
                {
                    offset += 2;
                } else {
                    return Some(offset);
                }
            } else if is_low_surrogate(unit) {
                return Some(offset);
            } else {
                offset += 1;
            }
        }
        None
    }
}

impl fmt::Debug for EcmaString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EcmaString(\"")?;
        for (_, code_point) in self.code_points() {
            if let Some(character) = char::from_u32(code_point) {
                for escaped in character.escape_debug() {
                    formatter.write_char(escaped)?;
                }
            } else {
                write!(formatter, "\\u{code_point:04X}")?;
            }
        }
        formatter.write_str("\")")
    }
}

/// The first unpaired surrogate encountered while validating UTF-16.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IllFormedUtf16 {
    /// Offset, in UTF-16 code units, of the unpaired surrogate.
    pub unit_offset: usize,
}

impl fmt::Display for IllFormedUtf16 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "ill-formed UTF-16: unpaired surrogate at code-unit offset {}",
            self.unit_offset
        )
    }
}

impl Error for IllFormedUtf16 {}

/// A code point outside the Unicode scalar-value range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct InvalidCodePoint {
    /// The rejected code point.
    pub value: u32,
}

impl fmt::Display for InvalidCodePoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid Unicode code point U+{:X}", self.value)
    }
}

impl Error for InvalidCodePoint {}

/// The owned accumulation path for exact ECMAScript strings.
#[derive(Default)]
pub struct EcmaStringBuilder(Vec<u16>);

impl EcmaStringBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Creates an empty builder with room for `capacity` code units.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    /// Appends one exact UTF-16 code unit.
    pub fn push_unit(&mut self, unit: u16) {
        self.0.push(unit);
    }

    /// Encodes and appends a UTF-8 string as UTF-16 code units.
    pub fn push_utf8(&mut self, value: &str) {
        self.0.extend(value.encode_utf16());
    }

    /// Appends one Unicode code point as UTF-16 code units.
    ///
    /// Values in the BMP, including surrogate values, append one code unit;
    /// supplementary scalar values append a surrogate pair.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCodePoint`] for values greater than U+10FFFF rather
    /// than truncating them to a `u16`.
    pub fn push_code_point(&mut self, code_point: u32) -> Result<(), InvalidCodePoint> {
        if code_point > 0x10_FFFF {
            return Err(InvalidCodePoint { value: code_point });
        }
        if code_point <= 0xFFFF {
            self.0.push(code_point as u16);
        } else {
            let supplementary = code_point - 0x1_0000;
            self.0.push(0xD800 | ((supplementary >> 10) as u16));
            self.0.push(0xDC00 | ((supplementary as u16) & 0x03FF));
        }
        Ok(())
    }

    /// Returns the number of accumulated UTF-16 code units.
    #[must_use]
    pub fn len_units(&self) -> usize {
        self.0.len()
    }

    /// Finishes this builder into an immutable string.
    #[must_use]
    pub fn finish(self) -> EcmaString {
        EcmaString(Arc::from(self.0))
    }
}

const fn is_high_surrogate(unit: u16) -> bool {
    unit >= 0xD800 && unit <= 0xDBFF
}

const fn is_low_surrogate(unit: u16) -> bool {
    unit >= 0xDC00 && unit <= 0xDFFF
}

#[cfg(test)]
mod tests {
    use super::{EcmaString, EcmaStringBuilder};
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    #[test]
    fn little_endian_wire_units_are_exact() {
        let string = EcmaString::from_le_bytes(&[0x61, 0, 0, 0xD8, 0, 0xDC]);
        assert_eq!(string.as_units(), &[0x0061, 0xD800, 0xDC00]);
    }

    #[test]
    fn lexical_order_is_by_code_units() {
        let high = EcmaString::from_units(&[0xD800]);
        let low = EcmaString::from_units(&[0xDFFF]);
        let supplementary = EcmaString::from_units(&[0xD800, 0xDC00]);

        assert!(high < low);
        assert!(high < supplementary);
        assert!(supplementary < low);
    }

    #[test]
    fn cloned_strings_are_equal_and_hash_equally() {
        let original = EcmaString::from_units(&[0x61, 0xD800]);
        let clone = original.clone();
        let mut original_hasher = DefaultHasher::new();
        let mut clone_hasher = DefaultHasher::new();
        original.hash(&mut original_hasher);
        clone.hash(&mut clone_hasher);

        assert_eq!(original, clone);
        assert_eq!(original_hasher.finish(), clone_hasher.finish());
    }

    #[test]
    fn strict_utf8_reports_the_first_unpaired_surrogate() {
        let string = EcmaString::from_units(&[0xD800, 0x61, 0xDC00]);

        assert_eq!(string.to_utf8_strict().unwrap_err().unit_offset, 0);
        assert_eq!(
            EcmaString::from_units(&[0x61, 0xDC00])
                .to_utf8_strict()
                .unwrap_err()
                .unit_offset,
            1
        );
    }

    #[test]
    fn lossy_utf8_replaces_only_unpaired_surrogates() {
        let string = EcmaString::from_units(&[0xD800, 0xDC00, 0xD800, 0x61, 0xDC00]);

        assert_eq!(string.to_utf8_lossy(), "𐀀�a�");
    }

    #[test]
    fn code_points_keep_code_unit_offsets_and_raw_surrogates() {
        let string = EcmaString::from_units(&[0x61, 0xD800, 0xDC00, 0xDC00, 0xD800]);

        assert_eq!(
            string.code_points().collect::<Vec<_>>(),
            vec![(0, 0x61), (1, 0x1_0000), (3, 0xDC00), (4, 0xD800)]
        );
    }

    #[test]
    fn slices_may_split_surrogate_pairs() {
        let string = EcmaString::from_units(&[0xD800, 0xDC00]);

        assert_eq!(string.slice_units(0..1).as_units(), &[0xD800]);
        assert_eq!(string.slice_units(1..2).as_units(), &[0xDC00]);
    }

    #[test]
    fn builder_preserves_supplementary_and_surrogate_code_points() {
        let mut builder = EcmaStringBuilder::new();
        builder.push_code_point(0x1F600).unwrap();
        builder.push_code_point(0xD800).unwrap();

        assert_eq!(builder.len_units(), 3);
        assert_eq!(builder.finish().as_units(), &[0xD83D, 0xDE00, 0xD800]);
    }

    #[test]
    fn builder_rejects_out_of_range_code_points() {
        let mut builder = EcmaStringBuilder::new();

        assert_eq!(
            builder.push_code_point(0x11_0000).unwrap_err().value,
            0x11_0000
        );
        assert!(builder.finish().is_empty());
    }

    #[test]
    fn ascii_comparison_rejects_non_ascii_values() {
        let ascii = EcmaString::from_utf8("ascii");
        let non_ascii = EcmaString::from_utf8("é");

        assert!(ascii.eq_ascii("ascii"));
        assert!(!ascii.eq_ascii("ASCII"));
        assert!(!ascii.eq_ascii("é"));
        assert!(!non_ascii.eq_ascii("é"));
    }

    #[test]
    fn debug_renders_lone_surrogates_visibly() {
        let string = EcmaString::from_units(&[0x61, 0xD800, 0xDC00, 0xDC00]);

        assert_eq!(format!("{string:?}"), "EcmaString(\"a𐀀\\uDC00\")");
    }
}
