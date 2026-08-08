//! Type relations over the interned [`TypeTable`]: assignability, subtyping,
//! variance, and a bounded relation cache.
//!
//! The algebra is deliberately split into two modes:
//!
//! - [`TypeRelations::assignable`] is TypeScript compatibility: every
//!   intentional concession (the bidirectional `any` hatch, number-to-enum,
//!   explicit `undefined` for an optional property) is accepted and reported
//!   as a [`RelationHazard`] through [`TypeRelations::relation`].
//! - [`TypeRelations::subtype`] is structural subtyping: the concessions are
//!   rejected, while genuine subtyping rules (fewer parameters, `void` return
//!   absorption, literal widening) are retained. [`TypeRelations::supertype`]
//!   and [`TypeRelations::equivalent`] are derived from it.
//!
//! Recursive structural queries are memoized in a bounded cache keyed by the
//! interned [`TypeId`] pair and the mode. Because generic instantiations intern
//! to distinct [`TypeId`]s just like structural types, the same cache covers
//! both once the type space grows generic forms. Interned types can only
//! reference already-interned members, so the type graph is acyclic and no
//! recursion guard is needed.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use super::binder::{FunctionSignature, PropertyType, SymbolId, Type, TypeId, TypeTable};
/// are computed without being stored; results stay deterministic because the
/// algebra itself never depends on cache state.
const RELATION_CACHE_CAPACITY: usize = 4096;

/// A compatibility decision plus the intentional TypeScript hazards that made
/// the conversion possible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeRelation {
    compatible: bool,
    hazards: Box<[RelationHazard]>,
}

impl TypeRelation {
    #[must_use]
    pub const fn compatible(&self) -> bool {
        self.compatible
    }

    #[must_use]
    pub fn hazards(&self) -> &[RelationHazard] {
        &self.hazards
    }
}

/// A type-system concession retained for the semantic lint pass.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RelationHazard {
    ExplicitUndefinedForOptional,
    FewerCallbackParameters,
    ValueReturnedToVoid,
    NumericEnumNumber,
}

/// How a relation query treats TypeScript's intentional compatibility
/// concessions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Strictness {
    /// Full assignability: every documented concession is accepted.
    Assignable,
    /// Structural subtyping: the `any` escape hatch, number-to-enum, and
    /// explicit-`undefined`-for-optional concessions are rejected.
    Strict,
    /// Assignability with `strictNullChecks` enabled: `null` and `undefined`
    /// flow only to types that explicitly include them.
    StrictNull,
}

/// Relation queries over one interned [`TypeTable`].
///
/// Construct once per relation-heavy pass and reuse it so the memoized pairs
/// amortize across queries; the convenience delegates on [`TypeTable`] build a
/// short-lived instance per call.
pub struct TypeRelations<'table> {
    table: &'table TypeTable,
    cache: RefCell<HashMap<(TypeId, TypeId, Strictness), bool>>,
    /// Pairs currently being compared, to break recursive structural types.
    visiting: RefCell<HashSet<(TypeId, TypeId, Strictness)>>,
    /// Type parameters paired positionally by the generic signatures currently
    /// being compared, so `<T>(x: T) => T` relates to `<U>(x: U) => U`.
    /// Non-empty only inside such a comparison.
    parameter_aliases: RefCell<Vec<(SymbolId, SymbolId)>>,
}

impl<'table> TypeRelations<'table> {
    #[must_use]
    pub fn new(table: &'table TypeTable) -> Self {
        Self {
            table,
            cache: RefCell::new(HashMap::new()),
            visiting: RefCell::new(HashSet::new()),
            parameter_aliases: RefCell::new(Vec::new()),
        }
    }

    /// Number of memoized pairs, exposed for cache consumers and tests.
    #[must_use]
    pub fn cached_pairs(&self) -> usize {
        self.cache.borrow().len()
    }

    /// Returns whether a value of `source` may be assigned where `target` is
    /// expected, using structural rules over the modeled type space.
    #[must_use]
    pub fn assignable(&self, source: TypeId, target: TypeId) -> bool {
        self.relates(source, target, Strictness::Assignable)
    }

    /// Returns whether `source` may be assigned to `target` under
    /// `strictNullChecks`.
    #[must_use]
    pub fn assignable_with_strict_null(&self, source: TypeId, target: TypeId) -> bool {
        self.relates(source, target, Strictness::StrictNull)
    }

    /// Returns whether `source` is a structural subtype of `target`, without
    /// the assignability concessions.
    #[must_use]
    pub fn subtype(&self, source: TypeId, target: TypeId) -> bool {
        self.relates(source, target, Strictness::Strict)
    }

    /// Returns whether `source` is a structural supertype of `target`.
    #[must_use]
    pub fn supertype(&self, source: TypeId, target: TypeId) -> bool {
        self.subtype(target, source)
    }

    /// Returns whether the two types are mutually subtypes.
    #[must_use]
    pub fn equivalent(&self, left: TypeId, right: TypeId) -> bool {
        left == right || (self.subtype(left, right) && self.subtype(right, left))
    }

    /// Computes compatibility once while retaining every accepted unsound
    /// concession for rule consumers.
    #[must_use]
    pub fn relation(&self, source: TypeId, target: TypeId) -> TypeRelation {
        let compatible = self.assignable(source, target);
        if !compatible {
            return TypeRelation {
                compatible,
                hazards: Box::new([]),
            };
        }

        let mut hazards = Vec::new();
        if let (Type::Function(from), Type::Function(to)) =
            (self.table.get(source), self.table.get(target))
        {
            if from.parameters().len() < to.parameters().len() {
                hazards.push(RelationHazard::FewerCallbackParameters);
            }
            if matches!(self.table.get(to.return_type()), Type::Void)
                && !matches!(self.table.get(from.return_type()), Type::Void | Type::Never)
            {
                hazards.push(RelationHazard::ValueReturnedToVoid);
            }
        }
        if matches!(
            (self.table.get(source), self.table.get(target)),
            (Type::NumericEnum(_), Type::Number) | (Type::Number, Type::NumericEnum(_))
        ) {
            hazards.push(RelationHazard::NumericEnumNumber);
        }
        if let (Type::ObjectType(from), Type::ObjectType(to)) =
            (self.table.get(source), self.table.get(target))
        {
            for target_property in to.iter().filter(|property| property.optional()) {
                let Some(source_property) = from
                    .iter()
                    .find(|property| property.name() == target_property.name())
                else {
                    continue;
                };
                if matches!(self.table.get(source_property.type_id()), Type::Undefined)
                    && !self.contains_undefined(target_property.type_id())
                {
                    hazards.push(RelationHazard::ExplicitUndefinedForOptional);
                    break;
                }
            }
        }
        TypeRelation {
            compatible,
            hazards: hazards.into_boxed_slice(),
        }
    }

    fn relates(&self, source: TypeId, target: TypeId, strictness: Strictness) -> bool {
        if source == target {
            return true;
        }
        let key = (source, target, strictness);
        // Only structural queries are memoized; primitive pairs resolve in one
        // match arm, so caching them would only spend memory. A result reached
        // under a type-parameter alias holds only for that comparison, so it is
        // never cached.
        let cacheable = (self.is_structural(source) || self.is_structural(target))
            && self.parameter_aliases.borrow().is_empty();
        if cacheable && let Some(result) = self.cache.borrow().get(&key) {
            return *result;
        }
        // Break cycles in recursive structural types: assume compatibility
        // while the pair is already being compared.
        {
            let mut visiting = self.visiting.borrow_mut();
            if !visiting.insert(key) {
                return true;
            }
        }
        let result = self.relates_uncached(source, target, strictness);
        self.visiting.borrow_mut().remove(&key);
        if cacheable {
            let mut cache = self.cache.borrow_mut();
            if cache.len() < RELATION_CACHE_CAPACITY {
                cache.insert(key, result);
            }
        }
        result
    }

    fn relates_uncached(&self, source: TypeId, target: TypeId, strictness: Strictness) -> bool {
        let (from, to) = (self.table.get(source), self.table.get(target));
        match (from, to) {
            (Type::Error, _) | (_, Type::Error) => true,
            // `any` is the deliberate escape hatch in both directions for
            // assignability and for strict-null assignability; it is not a
            // subtype of anything.
            (Type::Any, _) | (_, Type::Any) => {
                matches!(strictness, Strictness::Assignable | Strictness::StrictNull)
            }
            // `unknown` is the top type: everything flows in, nothing flows out.
            (_, Type::Unknown) => true,
            (Type::Unknown, _) => false,
            // `never` is the bottom type: it flows into everything, nothing else
            // flows into it (identity already handled above).
            (Type::Never, _) => true,
            (_, Type::Never) => false,
            (Type::StringLiteral(_), Type::String) => true,
            (Type::NumberLiteral(_), Type::Number) => true,
            (Type::BooleanLiteral(_), Type::Boolean) => true,
            (Type::BigIntLiteral(_), Type::BigInt) => true,
            // Enum member types are genuine subtypes of `number`; the reverse
            // direction is an assignability concession.
            (Type::NumericEnum(_), Type::Number) => true,
            (Type::Number, Type::NumericEnum(_)) => strictness == Strictness::Assignable,
            // Null/undefined are assignable to any type in non-strict mode,
            // but not to `never`. Under strict null checks they only flow to
            // types that explicitly include them.
            (Type::Null | Type::Undefined, _) if strictness == Strictness::Assignable => {
                !matches!(to, Type::Never)
            }
            (Type::Union(sources), _) => sources
                .iter()
                .all(|member| self.relates(*member, target, strictness)),
            (_, Type::Union(targets)) => targets
                .iter()
                .any(|member| self.relates(source, *member, strictness)),
            (Type::Undefined, _) if strictness == Strictness::StrictNull => {
                matches!(to, Type::Any | Type::Unknown | Type::Void | Type::Undefined)
            }
            (Type::Null, _) if strictness == Strictness::StrictNull => {
                matches!(to, Type::Any | Type::Unknown | Type::Null)
            }
            // Array elements are covariant, matching TypeScript's accepted
            // unsoundness for mutable arrays.
            (Type::Array(source_element), Type::Array(target_element)) => {
                self.relates(*source_element, *target_element, strictness)
            }
            (Type::ObjectType(source_props), Type::ObjectType(target_props)) => {
                self.object_relates(source_props, target_props, strictness)
            }
            (Type::Function(source_sig), Type::Function(target_sig)) => {
                self.function_relates(source_sig, target_sig, strictness)
            }
            // `object` is the non-primitive type: object literals, arrays,
            // functions, and class instances all flow into it.
            (
                Type::ObjectType(_) | Type::Array(_) | Type::Function(_) | Type::Named(_),
                Type::Object,
            ) => true,
            // `Object` (capital O, the boxed object type) is the top object
            // type that includes primitives: number, string, boolean, etc.
            // are assignable to `Object`, and `Object` is assignable to `object`
            // when the target is `Object` (any) or `object` with optional props.
            (
                Type::Number
                | Type::String
                | Type::Boolean
                | Type::BigInt
                | Type::Symbol
                | Type::NumberLiteral(_)
                | Type::StringLiteral(_)
                | Type::BooleanLiteral(_)
                | Type::BigIntLiteral(_)
                | Type::NumericEnum(_),
                Type::Named(symbol),
            ) if self.is_object_symbol(*symbol) => true,
            // `object` can be assigned to an empty or all-optional object type,
            // but not to a type that requires specific properties.
            (Type::Object, Type::ObjectType(target_props)) => {
                target_props.iter().all(|property| property.optional())
            }
            // Two generic signatures under comparison pair their type parameters
            // positionally, which is what makes them relate up to renaming.
            (Type::Named(source_symbol), Type::Named(target_symbol))
                if self
                    .parameter_aliases
                    .borrow()
                    .contains(&(*source_symbol, *target_symbol)) =>
            {
                true
            }
            // A class name stands for its instance structure. Comparing that
            // structure is what makes a derived class relate to its base, since
            // the derived instance type carries the base's members.
            (Type::Named(symbol), _) if self.table.class_instance(*symbol).is_some() => {
                let instance = self.table.class_instance(*symbol).unwrap_or(source);
                instance != source && self.relates(instance, target, strictness)
            }
            (_, Type::Named(symbol)) if self.table.class_instance(*symbol).is_some() => {
                let instance = self.table.class_instance(*symbol).unwrap_or(target);
                instance != target && self.relates(source, instance, strictness)
            }
            _ => false,
        }
    }
    fn is_object_symbol(&self, symbol: SymbolId) -> bool {
        self.table.is_object_symbol(symbol)
    }

    /// Pairs the type parameters of two generic signatures for the duration of
    /// `compare`. Pairing is positional, and only applies when both signatures
    /// declare the same number of parameters; otherwise they are compared as-is.
    fn with_parameter_aliases(
        &self,
        source: &FunctionSignature,
        target: &FunctionSignature,
        compare: impl FnOnce() -> bool,
    ) -> bool {
        let (source_parameters, target_parameters) =
            (source.type_parameters(), target.type_parameters());
        if source_parameters.is_empty() || source_parameters.len() != target_parameters.len() {
            return compare();
        }
        // Renaming is symmetric, and parameters are compared contravariantly, so
        // the pair arrives in either order. Record both.
        let added = source_parameters.len() * 2;
        let mut aliases = self.parameter_aliases.borrow_mut();
        for (&source_parameter, &target_parameter) in
            source_parameters.iter().zip(target_parameters.iter())
        {
            aliases.push((source_parameter, target_parameter));
            aliases.push((target_parameter, source_parameter));
        }
        drop(aliases);
        let result = compare();
        let mut aliases = self.parameter_aliases.borrow_mut();
        let kept = aliases.len() - added;
        aliases.truncate(kept);
        result
    }

    fn is_structural(&self, type_id: TypeId) -> bool {
        matches!(
            self.table.get(type_id),
            Type::Union(_) | Type::Array(_) | Type::ObjectType(_) | Type::Function(_)
        )
    }

    fn contains_undefined(&self, type_id: TypeId) -> bool {
        match self.table.get(type_id) {
            Type::Any | Type::Unknown | Type::Undefined => true,
            Type::Union(members) => members
                .iter()
                .any(|member| self.contains_undefined(*member)),
            _ => false,
        }
    }

    fn object_relates(
        &self,
        source: &[PropertyType],
        target: &[PropertyType],
        strictness: Strictness,
    ) -> bool {
        // Excess source properties are allowed; each target property must be
        // satisfied. Members are name-sorted, so a merge walk suffices.
        target.iter().all(
            |want| match source.iter().find(|have| have.name() == want.name()) {
                Some(have) => {
                    self.relates(have.type_id(), want.type_id(), strictness)
                        || (strictness == Strictness::Assignable
                            && want.optional()
                            && matches!(self.table.get(have.type_id()), Type::Undefined))
                }
                None => want.optional(),
            },
        )
    }

    fn function_relates(
        &self,
        source: &FunctionSignature,
        target: &FunctionSignature,
        strictness: Strictness,
    ) -> bool {
        self.with_parameter_aliases(source, target, || {
            // A source signature is only too narrow when it *requires* more than the
            // target *requires*. Optional parameters on the target may be left
            // unsupplied, so only the required counts are compared.
            let (source_required, _, _) = source.arity();
            let (target_required, _, _) = target.arity();
            if source_required > target_required {
                return false;
            }
            let positions = source.parameters().len().max(target.parameters().len());
            for index in 0..positions {
                // A position absent from either side needs no check: extra target
                // parameters go unread, and extra optional source parameters go unsupplied.
                let (Some(source_type), Some(target_type)) = (
                    self.parameter_type_at(source, index),
                    self.parameter_type_at(target, index),
                ) else {
                    continue;
                };
                // Parameters are contravariant: the target must supply a value the
                // source accepts.
                if !self.relates(target_type, source_type, strictness) {
                    return false;
                }
            }
            // The return position is covariant; a `void` target return absorbs any
            // source return, a genuine subtyping rule kept in both modes.
            matches!(self.table.get(target.return_type()), Type::Void)
                || self.relates(source.return_type(), target.return_type(), strictness)
        })
    }

    /// Type a signature accepts at `index`, or `None` when it accepts nothing
    /// there. A trailing rest parameter covers every position from its own index
    /// onward and contributes its element type, not the array type it declares.
    fn parameter_type_at(&self, signature: &FunctionSignature, index: usize) -> Option<TypeId> {
        let parameters = signature.parameters();
        if let Some(parameter) = parameters.get(index) {
            return Some(if parameter.rest() {
                self.rest_element(parameter.type_id())
            } else {
                parameter.type_id()
            });
        }
        let last = parameters.last()?;
        last.rest().then(|| self.rest_element(last.type_id()))
    }

    /// Element type a rest parameter binds per position. A rest parameter is
    /// declared as an array; a non-array declaration is already an error
    /// elsewhere, so it stands for itself here rather than failing the relation.
    fn rest_element(&self, type_id: TypeId) -> TypeId {
        match self.table.get(type_id) {
            Type::Array(element) => *element,
            _ => type_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::SymbolId;
    use super::*;

    #[test]
    fn primitives_follow_the_lattice_in_both_modes() {
        let table = TypeTable::new();
        let relations = TypeRelations::new(&table);
        for strict in [false, true] {
            let relates = |source, target| {
                if strict {
                    relations.subtype(source, target)
                } else {
                    relations.assignable(source, target)
                }
            };
            // never flows into everything; nothing else flows into never.
            assert!(relates(table.never(), table.number()));
            assert!(!relates(table.number(), table.never()));
            // unknown is the top: everything in, nothing out.
            assert!(relates(table.number(), table.unknown()));
            assert!(!relates(table.unknown(), table.number()));
        }
        // `any` is an assignability-only escape hatch.
        assert!(relations.assignable(table.any(), table.number()));
        assert!(relations.assignable(table.number(), table.any()));
        assert!(!relations.subtype(table.any(), table.number()));
        assert!(!relations.subtype(table.number(), table.any()));
        // Identity keeps `any` related to itself in both modes.
        assert!(relations.subtype(table.any(), table.any()));
    }

    #[test]
    fn subtype_is_assignability_minus_the_concessions() {
        let mut table = TypeTable::new();
        let enum_type = table.numeric_enum(SymbolId::new(200));
        let (number, undefined) = (table.number(), table.undefined_type());
        let undefined_source = table.object_type(vec![PropertyType::new("x", false, undefined)]);
        let optional_target = table.object_type(vec![PropertyType::new("x", true, number)]);
        let relations = TypeRelations::new(&table);

        // Enum-to-number is genuine subtyping; number-to-enum is a concession.
        assert!(relations.subtype(enum_type, number));
        assert!(relations.assignable(number, enum_type));
        assert!(!relations.subtype(number, enum_type));
        // Explicit `undefined` for an optional property is a concession.
        assert!(relations.assignable(undefined_source, optional_target));
        assert!(!relations.subtype(undefined_source, optional_target));
    }

    #[test]
    fn structural_objects_arrays_and_unions_relate_in_both_modes() {
        let mut table = TypeTable::new();
        let number = table.number();
        let string = table.string();
        let required = table.object_type(vec![PropertyType::new("x", false, number)]);
        let with_excess = table.object_type(vec![
            PropertyType::new("x", false, number),
            PropertyType::new("y", false, string),
        ]);
        let missing = table.object_type(vec![PropertyType::new("y", false, string)]);
        let optional = table.object_type(vec![PropertyType::new("x", true, number)]);
        let empty = table.object_type(Vec::new());
        let one = table.number_literal("1");
        let literal_array = table.array(one);
        let number_array = table.array(number);
        let union = table.union(&[number, string]);
        let relations = TypeRelations::new(&table);

        assert!(relations.subtype(with_excess, required));
        assert!(!relations.subtype(missing, required));
        assert!(relations.subtype(empty, optional));
        assert!(relations.subtype(literal_array, number_array));
        assert!(!relations.subtype(number_array, literal_array));
        assert!(relations.subtype(number, union));
        assert!(!relations.subtype(union, number));
    }

    #[test]
    fn functions_are_contravariant_in_params_covariant_in_return() {
        let mut table = TypeTable::new();
        let number = table.number();
        let void = table.void();
        let one = table.number_literal("1");
        let two = table.number_literal("2");
        let takes_number = table.function(vec![number], void);
        let takes_literal = table.function(vec![one], void);
        let takes_none = table.function(Vec::new(), void);
        let returns_number = table.function(Vec::new(), number);
        let returns_literal = table.function(Vec::new(), two);
        let relations = TypeRelations::new(&table);

        // Contravariant parameter: the general target parameter cannot feed the
        // narrower source parameter.
        assert!(!relations.subtype(takes_literal, takes_number));
        assert!(relations.subtype(takes_number, takes_literal));
        // Fewer source parameters is genuine subtyping, kept in strict mode.
        assert!(relations.subtype(takes_none, takes_number));
        assert!(!relations.subtype(takes_number, takes_none));
        // Covariant return, plus `void` absorption in both modes.
        assert!(relations.subtype(returns_literal, returns_number));
        assert!(!relations.subtype(returns_number, returns_literal));
        assert!(relations.subtype(returns_number, takes_none));
    }

    #[test]
    fn supertype_and_equivalent_are_derived_from_subtype() {
        let mut table = TypeTable::new();
        let number = table.number();
        let literal = table.number_literal("1");
        let left = table.named(SymbolId::new(300));
        let right = table.named(SymbolId::new(301));
        let relations = TypeRelations::new(&table);

        assert!(relations.supertype(number, literal));
        assert!(!relations.supertype(literal, number));
        assert!(relations.equivalent(number, number));
        assert!(!relations.equivalent(number, literal));
        // Distinct named types are nominal: neither direction relates.
        assert!(!relations.equivalent(left, right));
        assert!(relations.equivalent(left, left));
    }

    #[test]
    fn repeated_queries_hit_the_cache_without_growing_it() {
        let mut table = TypeTable::new();
        let number = table.number();
        let one = table.number_literal("1");
        let inner = table.array(number);
        let outer = table.array(inner);
        let literal_inner = table.array(one);
        let literal_outer = table.array(literal_inner);
        let relations = TypeRelations::new(&table);

        // Identity short-circuits before the cache is consulted.
        assert!(relations.assignable(outer, outer));
        assert_eq!(relations.cached_pairs(), 0);
        // The first distinct query memoizes its structural sub-queries.
        assert!(relations.assignable(literal_outer, outer));
        let grown = relations.cached_pairs();
        assert!(grown > 0);
        // Repeating the same pair returns the identical result from the cache.
        assert!(relations.assignable(literal_outer, outer));
        assert_eq!(relations.cached_pairs(), grown);
        // The strict mode memoizes under its own key.
        assert!(relations.subtype(literal_outer, outer));
        let after_strict = relations.cached_pairs();
        assert!(after_strict > grown);
        assert!(relations.subtype(literal_outer, outer));
        assert_eq!(relations.cached_pairs(), after_strict);
    }

    #[test]
    fn cache_is_bounded_and_results_stay_deterministic() {
        // 100 distinct array-of-literal types yield 9900 ordered pairs, far
        // beyond the cache capacity.
        let mut table = TypeTable::new();
        let arrays: Vec<TypeId> = (0..100)
            .map(|index| {
                let literal = table.number_literal(&index.to_string());
                table.array(literal)
            })
            .collect();
        let relations = TypeRelations::new(&table);
        for &source in &arrays {
            for &target in &arrays {
                let expected = source == target;
                assert_eq!(relations.assignable(source, target), expected);
            }
        }
        assert!(relations.cached_pairs() <= RELATION_CACHE_CAPACITY);

        // A fresh relation engine over the same table computes identical
        // results, proving the cache never influences outcomes.
        let fresh = TypeRelations::new(&table);
        for &source in &arrays {
            for &target in &arrays {
                let expected = source == target;
                assert_eq!(fresh.assignable(source, target), expected);
                assert_eq!(relations.assignable(source, target), expected);
            }
        }
    }

    #[test]
    fn union_distributes_before_strict_null_leaf() {
        // Under StrictNull, a union target must be checked before the scalar
        // strict-null leaf. Old order: (Undefined, _) matched before
        // (_, Type::Union) and rejected `undefined -> number|undefined`.
        let mut table = TypeTable::new();
        let number = table.number();
        let undefined = table.undefined_type();
        let null = table.null_type();
        let union_undefined = table.union(&[number, undefined]);
        let union_null = table.union(&[number, null]);
        let string = table.string();
        let union_number_only = table.union(&[number, string]);
        let relations = TypeRelations::new(&table);
        // Scalar undefined/null must flow into a union that contains it (strict mode)
        assert!(relations.assignable_with_strict_null(undefined, union_undefined));
        assert!(relations.assignable_with_strict_null(null, union_null));
        // But not into a union that does not contain it
        assert!(!relations.assignable_with_strict_null(undefined, union_number_only));
        assert!(!relations.assignable_with_strict_null(null, union_number_only));
    }

    #[test]
    fn function_arity_compares_required_to_required() {
        // A source requiring more than the target requires is too narrow,
        // even if the target could accept more via optional params.
        // Old: source_required > target_total (2 > 2 false, would pass)
        // New: source_required > target_required (2 > 1 true, correctly fails)
        let mut table = TypeTable::new();
        let number = table.number();
        let void = table.void();
        // target: (a: number, b?: number) => void  => required 1, total 2
        let target = table.function_with_parameters(
            vec![],
            vec![
                crate::checker::binder::FunctionParameter::new(
                    "a".to_string(),
                    number,
                    false,
                    false,
                ),
                crate::checker::binder::FunctionParameter::new(
                    "b".to_string(),
                    number,
                    true,
                    false,
                ),
            ],
            void,
        );
        // source: (a: number, b: number) => void  => required 2
        let source = table.function(vec![number, number], void);
        let relations = TypeRelations::new(&table);
        assert!(!relations.subtype(source, target));
        assert!(relations.subtype(target, source));
    }
}
