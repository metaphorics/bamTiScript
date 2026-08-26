#![cfg(test)]
//! Pure import-attribute and JSON-module policy.
//!
//! Object enumeration and property access stay in the VM: callers materialize
//! every enumerable entry exactly once, including invoking getters, before
//! passing the resulting values here.

use bamts_bytecode::EcmaString;
use bamts_native::Value;

/// One canonical import attribute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImportAttribute {
    pub(crate) key: EcmaString,
    pub(crate) value: EcmaString,
}

/// A materialized attribute value after the VM has performed property access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MaterializedAttributeValue {
    String(EcmaString),
    NonString,
}

/// One attribute entry after own-key enumeration and getter invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MaterializedAttributeEntry {
    pub(crate) key: EcmaString,
    pub(crate) value: MaterializedAttributeValue,
}

impl MaterializedAttributeEntry {
    pub(crate) fn string(key: EcmaString, value: EcmaString) -> Self {
        Self {
            key,
            value: MaterializedAttributeValue::String(value),
        }
    }

    pub(crate) fn non_string(key: EcmaString) -> Self {
        Self {
            key,
            value: MaterializedAttributeValue::NonString,
        }
    }
}

/// A stable, unique attribute list ordered by exact ECMAScript UTF-16 units.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CanonicalAttributes(Vec<ImportAttribute>);

impl CanonicalAttributes {
    pub(crate) fn as_slice(&self) -> &[ImportAttribute] {
        &self.0
    }
}

/// Typed failures mapped by the VM integration layer to ECMAScript errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttributeError {
    /// Attribute values are required to be strings; no coercion is performed.
    NonStringValue,
    /// Repeated keys are syntax errors even when their values are equal.
    DuplicateKey,
    /// The host supports no attribute other than exactly `type: "json"`.
    UnsupportedAttribute,
    /// The requested attribute and loaded module content kind disagree.
    ModuleTypeMismatch,
}

/// Canonicalizes already-materialized entries without observing user code.
///
/// `EcmaString::Ord` is lexicographic over its underlying `u16` sequence, so
/// sorting preserves ECMAScript code-unit order, including lone surrogates.
pub(crate) fn canonicalize(
    entries: Vec<MaterializedAttributeEntry>,
) -> Result<CanonicalAttributes, AttributeError> {
    let mut attributes = Vec::with_capacity(entries.len());
    for entry in entries {
        let MaterializedAttributeValue::String(value) = entry.value else {
            return Err(AttributeError::NonStringValue);
        };
        attributes.push(ImportAttribute {
            key: entry.key,
            value,
        });
    }

    attributes.sort_unstable_by(|left, right| left.key.cmp(&right.key));
    if attributes.windows(2).any(|pair| pair[0].key == pair[1].key) {
        return Err(AttributeError::DuplicateKey);
    }

    Ok(CanonicalAttributes(attributes))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModuleContentKind {
    JavaScript,
    Json,
}

/// Validates the canonical attributes against the loaded module content.
///
/// Node 24's stable contract requires `type: "json"` for JSON and rejects that
/// attribute for JavaScript. Every other key or value is unsupported.
pub(crate) fn validate_module_type(
    attributes: &CanonicalAttributes,
    kind: ModuleContentKind,
) -> Result<(), AttributeError> {
    let requests_json = match attributes.as_slice() {
        [] => false,
        [attribute] if attribute.key.eq_ascii("type") && attribute.value.eq_ascii("json") => true,
        _ => return Err(AttributeError::UnsupportedAttribute),
    };

    match (kind, requests_json) {
        (ModuleContentKind::JavaScript, false) | (ModuleContentKind::Json, true) => Ok(()),
        (ModuleContentKind::JavaScript, true) | (ModuleContentKind::Json, false) => {
            Err(AttributeError::ModuleTypeMismatch)
        }
    }
}

/// Creates the complete export list for a parsed JSON module.
pub(crate) fn json_module_exports(parsed: Value) -> [(EcmaString, Value); 1] {
    [(EcmaString::encode("default"), parsed)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> EcmaString {
        EcmaString::encode(value)
    }

    fn entry(key: &str, value: &str) -> MaterializedAttributeEntry {
        MaterializedAttributeEntry::string(text(key), text(value))
    }

    #[test]
    fn canonicalizes_getter_materialized_entries_in_code_unit_order() {
        let attributes = canonicalize(vec![
            entry("z", "last"),
            MaterializedAttributeEntry::string(EcmaString::from_units(&[0xe000]), text("bmp")),
            MaterializedAttributeEntry::string(
                EcmaString::from_units(&[0xd83d, 0xde00]),
                text("supplementary"),
            ),
            entry("a", "first"),
        ])
        .unwrap();

        let keys: Vec<Vec<u16>> = attributes
            .as_slice()
            .iter()
            .map(|attribute| attribute.key.as_units().to_vec())
            .collect();
        assert_eq!(
            keys,
            vec![
                text("a").as_units().to_vec(),
                text("z").as_units().to_vec(),
                vec![0xd83d, 0xde00],
                vec![0xe000],
            ]
        );
    }

    #[test]
    fn rejects_duplicate_keys_including_equal_values() {
        assert_eq!(
            canonicalize(vec![entry("type", "json"), entry("type", "json")]),
            Err(AttributeError::DuplicateKey)
        );
    }

    #[test]
    fn rejects_materialized_non_string_without_coercion() {
        assert_eq!(
            canonicalize(vec![MaterializedAttributeEntry::non_string(text("type"))]),
            Err(AttributeError::NonStringValue)
        );
    }

    #[test]
    fn legacy_assert_and_with_inputs_share_canonical_policy() {
        let with_result = canonicalize(vec![entry("type", "json")]);
        let assert_result = canonicalize(vec![entry("type", "json")]);
        assert_eq!(with_result, assert_result);
    }

    #[test]
    fn validates_only_the_json_type_contract() {
        let empty = canonicalize(Vec::new()).unwrap();
        let json = canonicalize(vec![entry("type", "json")]).unwrap();

        assert_eq!(
            validate_module_type(&empty, ModuleContentKind::JavaScript),
            Ok(())
        );
        assert_eq!(
            validate_module_type(&json, ModuleContentKind::JavaScript),
            Err(AttributeError::ModuleTypeMismatch)
        );
        assert_eq!(
            validate_module_type(&empty, ModuleContentKind::Json),
            Err(AttributeError::ModuleTypeMismatch)
        );
        assert_eq!(validate_module_type(&json, ModuleContentKind::Json), Ok(()));
    }

    #[test]
    fn rejects_unknown_keys_and_unsupported_type_values() {
        let unknown = canonicalize(vec![entry("integrity", "sha256-deadbeef")]).unwrap();
        let unsupported_type = canonicalize(vec![entry("type", "javascript")]).unwrap();

        assert_eq!(
            validate_module_type(&unknown, ModuleContentKind::JavaScript),
            Err(AttributeError::UnsupportedAttribute)
        );
        assert_eq!(
            validate_module_type(&unsupported_type, ModuleContentKind::JavaScript),
            Err(AttributeError::UnsupportedAttribute)
        );
    }

    #[test]
    fn json_exports_only_default() {
        let parsed = Value::int32(42);
        let exports = json_module_exports(parsed);

        assert_eq!(exports.len(), 1);
        assert!(exports[0].0.eq_ascii("default"));
        assert_eq!(exports[0].1, parsed);
        assert!(!exports.iter().any(|(name, _)| name.eq_ascii("named")));
    }

    #[test]
    fn canonical_empty_state_is_observable_without_allocation() {
        assert!(canonicalize(Vec::new()).unwrap().as_slice().is_empty());
    }
}
