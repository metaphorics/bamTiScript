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
//! Recursive structural queries are memoized in a bounded cache and guarded by
//! the complete interned [`TypeId`] pair plus mode while in progress. Generic
//! applications with different arguments therefore remain distinct, while an
//! exact repeated pair is a sound coinductive recursion boundary.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use super::binder::{
    FunctionSignature, ObjectType, PropertyType, SymbolId, TupleShape, Type, TypeId, TypeTable,
};
use crate::syntax::Accessibility;
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
    /// Symmetric overlap used by TypeScript assertions. Union sources need one
    /// related constituent rather than every constituent.
    Comparable,
}

#[derive(Clone, Copy)]
enum ParameterVariance {
    Contravariant,
    Bivariant,
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
    /// Signature type parameters erased to `any` for the comparable relation.
    /// Non-empty only while one signature comparison is active.
    erased_parameters: RefCell<Vec<SymbolId>>,
}

impl<'table> TypeRelations<'table> {
    #[must_use]
    pub fn new(table: &'table TypeTable) -> Self {
        Self {
            table,
            cache: RefCell::new(HashMap::new()),
            visiting: RefCell::new(HashSet::new()),
            parameter_aliases: RefCell::new(Vec::new()),
            erased_parameters: RefCell::new(Vec::new()),
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

    /// Returns whether either direction has sufficient structural overlap for
    /// a TypeScript type assertion.
    #[must_use]
    pub fn comparable(&self, left: TypeId, right: TypeId) -> bool {
        self.relates(left, right, Strictness::Comparable)
            || self.relates(right, left, Strictness::Comparable)
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
            for target_property in to.properties.iter().filter(|property| property.optional()) {
                let Some(source_property) = from
                    .properties
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
            && self.parameter_aliases.borrow().is_empty()
            && self.erased_parameters.borrow().is_empty();
        if cacheable && let Some(result) = self.cache.borrow().get(&key) {
            return *result;
        }

        let is_root = self.visiting.borrow().is_empty();
        {
            let mut visiting = self.visiting.borrow_mut();
            if !visiting.insert(key) {
                return true;
            }
        }

        // Applications relate through their substituted structural views.
        // The in-progress key above uses the applied TypeIds themselves, so a
        // recursive Box<1>/Box<number> comparison terminates without conflating
        // either application with a different argument pair.
        let source_applied = matches!(self.table.get(source), Type::AppliedClass { .. });
        let target_applied = matches!(self.table.get(target), Type::AppliedClass { .. });
        let result = if source_applied || target_applied {
            let source_view = source_applied
                .then(|| self.table.applied_class_view(source))
                .flatten();
            let target_view = target_applied
                .then(|| self.table.applied_class_view(target))
                .flatten();
            if source_view.is_some() || target_view.is_some() {
                self.relates(
                    source_view.unwrap_or(source),
                    target_view.unwrap_or(target),
                    strictness,
                )
            } else {
                self.relates_uncached(source, target, strictness)
            }
        } else {
            self.relates_uncached(source, target, strictness)
        };

        self.visiting.borrow_mut().remove(&key);
        if cacheable && is_root {
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
                matches!(
                    strictness,
                    Strictness::Assignable | Strictness::StrictNull | Strictness::Comparable
                )
            }
            (Type::Named(symbol), _)
                if strictness == Strictness::Comparable && self.is_erased_parameter(*symbol) =>
            {
                true
            }
            (_, Type::Named(symbol))
                if strictness == Strictness::Comparable && self.is_erased_parameter(*symbol) =>
            {
                true
            }
            // An unresolved constraint is already represented by `Error`.
            // Preserve that recovery boundary in both directions so one
            // unsupported generic does not emit unrelated compatibility errors.
            (_, Type::Named(symbol))
                if self
                    .table
                    .type_parameter_constraint(*symbol)
                    .is_some_and(|constraint| {
                        matches!(self.table.get(constraint), Type::Error)
                    }) =>
            {
                true
            }
            // `unknown` is the top type: everything flows in, nothing flows out.
            (_, Type::Unknown) => true,
            (Type::Unknown, _) => false,
            // Generic signatures compare their type parameters by position.
            // Resolve that temporary identity before consulting constraints;
            // otherwise two renamed constrained parameters decay to their
            // bounds and can never match each other.
            (Type::Named(source_symbol), Type::Named(target_symbol))
                if self
                    .parameter_aliases
                    .borrow()
                    .contains(&(*source_symbol, *target_symbol)) =>
            {
                true
            }
            // A value whose type is a type parameter carries at least its
            // declared constraint. Class and interface names are absent from
            // this table and keep their nominal handling below.
            (Type::Named(symbol), _) if self.table.type_parameter_constraint(*symbol).is_some() => {
                let constraint = self
                    .table
                    .type_parameter_constraint(*symbol)
                    .expect("guard checked constraint");
                constraint != source && self.relates(constraint, target, strictness)
            }
            // An interface with a completed structural body expands through that body
            // once. The `visiting` set in `relates` terminates recursive self and mutual
            // interface pairs; returned member types that are the interface's own head stay inert.
            (Type::Named(source_symbol), _)
                if self.table.interface_structure(*source_symbol).is_some() =>
            {
                let view = self.table.named_structural_view(source);
                self.relates(view, target, strictness)
            }
            (_, Type::Named(target_symbol))
                if self.table.interface_structure(*target_symbol).is_some() =>
            {
                let view = self.table.named_structural_view(target);
                self.relates(source, view, strictness)
            }
            // `never` is the bottom type: it flows into everything, nothing else
            // flows into it (identity already handled above).
            (Type::Never, _) => true,
            (_, Type::Never) => false,
            (_, Type::Intersection(targets)) => targets
                .iter()
                .all(|member| self.relates(source, *member, strictness)),
            (Type::Intersection(sources), Type::ObjectType(target)) => {
                self.intersection_object_relates(sources, target, strictness)
            }
            (Type::Intersection(sources), _) => sources
                .iter()
                .any(|member| self.relates(*member, target, strictness)),
            (Type::StringLiteral(_), Type::String) => true,
            (Type::NumberLiteral(_), Type::Number) => true,
            (Type::BooleanLiteral(_), Type::Boolean) => true,
            (Type::BigIntLiteral(_), Type::BigInt) => true,
            // Enum member types are genuine subtypes of `number`; the reverse
            // direction is an assignability concession in both assignable modes.
            (Type::NumericEnum(_), Type::Number) => true,
            (Type::Number, Type::NumericEnum(_)) => {
                matches!(
                    strictness,
                    Strictness::Assignable | Strictness::StrictNull | Strictness::Comparable
                )
            }
            // Null/undefined are assignable to any type in non-strict mode,
            // but not to `never`. Under strict null checks they only flow to
            // types that explicitly include them.
            (Type::Null | Type::Undefined, _) if strictness == Strictness::Assignable => {
                !matches!(to, Type::Never)
            }
            (Type::Union(sources), _) => {
                if strictness == Strictness::Comparable {
                    sources
                        .iter()
                        .any(|member| self.relates(*member, target, strictness))
                } else {
                    sources
                        .iter()
                        .all(|member| self.relates(*member, target, strictness))
                }
            }
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
            (Type::Tuple(source_shape), Type::Tuple(target_shape)) => {
                self.tuple_relates(source_shape, target_shape, strictness)
            }
            (Type::Tuple(source_shape), Type::Array(target_element)) => source_shape
                .all_element_types()
                .iter()
                .all(|&source| self.relates(source, *target_element, strictness)),
            (Type::Array(_), Type::Tuple(_)) => false,
            (Type::ObjectType(source), Type::ObjectType(target)) => {
                self.object_relates(source, target, strictness)
            }
            (Type::Function(source_sig), Type::Function(target_sig)) => {
                self.function_relates(source_sig, target_sig, strictness)
            }
            // `object` is the non-primitive type: object literals, arrays,
            // functions, and class instances all flow into it.
            (
                Type::ObjectType(_)
                | Type::Array(_)
                | Type::Tuple(_)
                | Type::Function(_)
                | Type::Named(_)
                | Type::AppliedClass { .. },
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
                | Type::NumericEnum(_)
                | Type::AppliedClass { .. },
                Type::Named(symbol),
            ) if self.is_object_symbol(*symbol) => true,
            // `object` can be assigned to an empty or all-optional object type,
            // but not to a type that requires specific properties.
            (Type::Object, Type::ObjectType(target)) => {
                target.properties.iter().all(|property| property.optional())
            }
            (
                Type::Void
                | Type::Null
                | Type::Undefined
                | Type::Boolean
                | Type::Number
                | Type::BigInt
                | Type::String
                | Type::Symbol
                | Type::Object
                | Type::BooleanLiteral(_)
                | Type::NumberLiteral(_)
                | Type::StringLiteral(_)
                | Type::BigIntLiteral(_)
                | Type::Array(_)
                | Type::Tuple(_)
                | Type::ObjectType(_)
                | Type::Function(_)
                | Type::Named(_)
                | Type::AppliedClass { .. }
                | Type::NumericEnum(_)
                | Type::Keyof(_)
                | Type::IndexedAccess { .. },
                _,
            ) => false,
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
        strictness: Strictness,
        variance: ParameterVariance,
        compare: impl FnOnce() -> bool,
    ) -> bool {
        if strictness == Strictness::Comparable {
            return self.with_erased_parameters(source, target, compare);
        }
        let (source_parameters, target_parameters) =
            (source.type_parameters(), target.type_parameters());
        if source_parameters.is_empty() || source_parameters.len() != target_parameters.len() {
            return compare();
        }
        let same_declaration = source_parameters == target_parameters;
        // Renaming is symmetric. A method instantiated from one declaration
        // may compare the outer substitutions in either direction. Distinct
        // generic declarations still require target-domain coverage.
        let added = source_parameters.len() * 2;
        let mut aliases = self.parameter_aliases.borrow_mut();
        for (&source_parameter, &target_parameter) in
            source_parameters.iter().zip(target_parameters.iter())
        {
            aliases.push((source_parameter, target_parameter));
            aliases.push((target_parameter, source_parameter));
        }
        drop(aliases);
        let constraints_relate = source
            .type_parameter_bounds()
            .iter()
            .zip(target.type_parameter_bounds())
            .all(|(source_bound, target_bound)| {
                let source_constraint = source_bound
                    .constraint()
                    .unwrap_or_else(|| self.table.unknown());
                let target_constraint = target_bound
                    .constraint()
                    .unwrap_or_else(|| self.table.unknown());
                self.relates(target_constraint, source_constraint, strictness)
                    || (same_declaration
                        && matches!(variance, ParameterVariance::Bivariant)
                        && self.relates(source_constraint, target_constraint, strictness))
            });
        let result = constraints_relate && compare();
        let mut aliases = self.parameter_aliases.borrow_mut();
        let kept = aliases.len() - added;
        aliases.truncate(kept);
        result
    }
    fn with_erased_parameters(
        &self,
        source: &FunctionSignature,
        target: &FunctionSignature,
        compare: impl FnOnce() -> bool,
    ) -> bool {
        let added = source.type_parameters().len() + target.type_parameters().len();
        let mut erased = self.erased_parameters.borrow_mut();
        erased.extend(source.type_parameters());
        erased.extend(target.type_parameters());
        drop(erased);
        let result = compare();
        let mut erased = self.erased_parameters.borrow_mut();
        let kept = erased.len() - added;
        erased.truncate(kept);
        result
    }

    fn is_erased_parameter(&self, symbol: SymbolId) -> bool {
        self.erased_parameters.borrow().contains(&symbol)
    }

    fn is_structural(&self, type_id: TypeId) -> bool {
        matches!(
            self.table.get(type_id),
            Type::Union(_)
                | Type::Array(_)
                | Type::Tuple(_)
                | Type::ObjectType(_)
                | Type::Function(_)
                | Type::Named(_)
                | Type::AppliedClass { .. }
        )
    }
    fn contains_undefined(&self, type_id: TypeId) -> bool {
        match self.table.get(type_id) {
            Type::Any | Type::Unknown | Type::Undefined => true,
            Type::Union(members) => members
                .iter()
                .any(|member| self.contains_undefined(*member)),
            Type::AppliedClass { .. } => self
                .table
                .applied_class_view(type_id)
                .map(|view| self.contains_undefined(view))
                .unwrap_or(false),
            Type::Error
            | Type::Intersection(_)
            | Type::Never
            | Type::Void
            | Type::Null
            | Type::Boolean
            | Type::Number
            | Type::BigInt
            | Type::String
            | Type::Symbol
            | Type::Object
            | Type::BooleanLiteral(_)
            | Type::NumberLiteral(_)
            | Type::StringLiteral(_)
            | Type::BigIntLiteral(_)
            | Type::Array(_)
            | Type::Tuple(_)
            | Type::ObjectType(_)
            | Type::Function(_)
            | Type::Named(_)
            | Type::NumericEnum(_)
            | Type::Keyof(_)
            | Type::IndexedAccess { .. } => false,
        }
    }

    /// Structural relation between two tuple shapes.
    ///
    /// A tuple relation must hold for every runtime length admitted by the
    /// source. The target must admit that full length interval, and every
    /// source element possible at a position must fit at least one target
    /// element possible at the same position.
    fn tuple_relates(
        &self,
        source: &TupleShape,
        target: &TupleShape,
        strictness: Strictness,
    ) -> bool {
        if source.min_arity() < target.min_arity() {
            return false;
        }
        match (source.max_arity(), target.max_arity()) {
            (Some(source_max), Some(target_max)) if source_max > target_max => return false,
            (None, Some(_)) => return false,
            _ => {}
        }

        let last_length = source.max_arity().unwrap_or_else(|| {
            (source.prefix.len() + source.suffix.len())
                .max(target.prefix.len() + target.suffix.len())
                .max(source.min_arity())
                .saturating_add(1)
        });
        (source.min_arity()..=last_length)
            .all(|length| self.tuple_length_relates(source, target, length, strictness))
    }

    fn tuple_length_relates(
        &self,
        source: &TupleShape,
        target: &TupleShape,
        length: usize,
        strictness: Strictness,
    ) -> bool {
        let source_layouts = source.prefix_lengths_at_length(length);
        let target_layouts = target.prefix_lengths_at_length(length);
        source_layouts.iter().all(|&source_prefix_len| {
            target_layouts.iter().any(|&target_prefix_len| {
                (0..length).all(|index| {
                    self.relates(
                        source.element_at_layout(index, length, source_prefix_len),
                        target.element_at_layout(index, length, target_prefix_len),
                        strictness,
                    )
                })
            })
        })
    }
    fn property_origin_compatible(have: &PropertyType, want: &PropertyType) -> bool {
        if want.access() == Accessibility::Public {
            return true;
        }
        have.access() != Accessibility::Public && have.declaring_class() == want.declaring_class()
    }

    fn object_relates(
        &self,
        source: &ObjectType,
        target: &ObjectType,
        strictness: Strictness,
    ) -> bool {
        let properties_relate = target.properties.iter().all(|want| {
            match source
                .properties
                .iter()
                .find(|have| have.name() == want.name())
            {
                Some(have) => {
                    Self::property_origin_compatible(have, want)
                        && (self.property_type_relates(have, want, strictness)
                            || (matches!(
                                strictness,
                                Strictness::Assignable
                                    | Strictness::StrictNull
                                    | Strictness::Comparable
                            ) && want.optional()
                                && matches!(self.table.get(have.type_id()), Type::Undefined)))
                }
                None => want.optional(),
            }
        });
        properties_relate
            && self.signature_sets_relate(
                &source.call_signatures,
                &target.call_signatures,
                strictness,
            )
            && self.signature_sets_relate(
                &source.construct_signatures,
                &target.construct_signatures,
                strictness,
            )
            && target.index_signatures.iter().all(|want| {
                let Some(want_parameter) = want.parameters.first() else {
                    return false;
                };
                let numeric = matches!(self.table.get(want_parameter.type_id()), Type::Number);
                let properties_relate = source.properties.iter().all(|property| {
                    if numeric && property.name().parse::<usize>().is_err() {
                        return true;
                    }
                    self.relates(property.type_id(), want.value_type, strictness)
                });
                let source_signature = match self.table.get(want_parameter.type_id()) {
                    Type::Number => source
                        .index_signatures
                        .iter()
                        .find(|have| {
                            have.parameters.first().is_some_and(|parameter| {
                                matches!(self.table.get(parameter.type_id()), Type::Number)
                            })
                        })
                        .or_else(|| {
                            source.index_signatures.iter().find(|have| {
                                have.parameters.first().is_some_and(|parameter| {
                                    matches!(self.table.get(parameter.type_id()), Type::String)
                                })
                            })
                        }),
                    Type::String => source.index_signatures.iter().find(|have| {
                        have.parameters.first().is_some_and(|parameter| {
                            matches!(self.table.get(parameter.type_id()), Type::String)
                        })
                    }),
                    _ => return false,
                };
                let signatures_relate = source_signature
                    .map(|have| self.relates(have.value_type, want.value_type, strictness))
                    .unwrap_or_else(|| source.index_signatures.is_empty());
                properties_relate && signatures_relate
            })
    }

    fn property_type_relates(
        &self,
        source: &PropertyType,
        target: &PropertyType,
        strictness: Strictness,
    ) -> bool {
        self.relates(source.type_id(), target.type_id(), strictness)
            || (strictness == Strictness::Comparable
                && self.relates(target.type_id(), source.type_id(), strictness))
            || (target.is_method()
                && self.method_overloads_relate(source.type_id(), target.type_id(), strictness))
    }
    fn relation_object_view(&self, type_id: TypeId) -> Option<&ObjectType> {
        let type_id = match self.table.get(type_id) {
            Type::AppliedClass { .. } => self.table.applied_class_view(type_id).unwrap_or(type_id),
            Type::Named(symbol) => self
                .table
                .type_parameter_constraint(*symbol)
                .unwrap_or(type_id),
            _ => type_id,
        };
        let type_id = self.table.named_structural_view(type_id);
        match self.table.get(type_id) {
            Type::ObjectType(object) => Some(object),
            _ => None,
        }
    }

    fn intersection_object_relates(
        &self,
        sources: &[TypeId],
        target: &ObjectType,
        strictness: Strictness,
    ) -> bool {
        let properties_relate = target.properties.iter().all(|want| {
            let satisfied = sources.iter().any(|source| {
                let Some(object) = self.relation_object_view(*source) else {
                    return false;
                };
                object
                    .properties
                    .iter()
                    .find(|have| have.name() == want.name())
                    .is_some_and(|have| {
                        Self::property_origin_compatible(have, want)
                            && self.property_type_relates(have, want, strictness)
                    })
            });
            satisfied || want.optional()
        });
        if !properties_relate {
            return false;
        }

        let mut combined = ObjectType {
            properties: Vec::new(),
            call_signatures: Vec::new(),
            construct_signatures: Vec::new(),
            index_signatures: Vec::new(),
        };
        let mut has_object = false;
        for source in sources {
            let Some(object) = self.relation_object_view(*source) else {
                continue;
            };
            has_object = true;
            combined
                .properties
                .extend(object.properties.iter().cloned());
            combined
                .call_signatures
                .extend(object.call_signatures.iter().cloned());
            combined
                .construct_signatures
                .extend(object.construct_signatures.iter().cloned());
            combined
                .index_signatures
                .extend(object.index_signatures.iter().cloned());
        }
        if !has_object {
            return false;
        }
        let index_target = ObjectType {
            properties: Vec::new(),
            call_signatures: Vec::new(),
            construct_signatures: Vec::new(),
            index_signatures: target.index_signatures.clone(),
        };
        self.signature_sets_relate(
            &combined.call_signatures,
            &target.call_signatures,
            strictness,
        ) && self.signature_sets_relate(
            &combined.construct_signatures,
            &target.construct_signatures,
            strictness,
        ) && self.object_relates(&combined, &index_target, strictness)
    }

    fn signature_sets_relate(
        &self,
        source: &[FunctionSignature],
        target: &[FunctionSignature],
        strictness: Strictness,
    ) -> bool {
        target.iter().all(|want| {
            source
                .iter()
                .any(|have| self.function_relates(have, want, strictness))
        })
    }

    fn function_relates(
        &self,
        source: &FunctionSignature,
        target: &FunctionSignature,
        strictness: Strictness,
    ) -> bool {
        self.signature_relates(source, target, strictness, ParameterVariance::Contravariant)
    }

    fn method_overloads_relate(
        &self,
        source: TypeId,
        target: TypeId,
        strictness: Strictness,
    ) -> bool {
        match self.table.get(target) {
            Type::Function(_) => self.method_target_overload_relates(source, target, strictness),
            Type::Intersection(targets) => targets
                .iter()
                .all(|target| self.method_target_overload_relates(source, *target, strictness)),
            _ => false,
        }
    }

    fn method_target_overload_relates(
        &self,
        source: TypeId,
        target: TypeId,
        strictness: Strictness,
    ) -> bool {
        let Type::Function(target_signature) = self.table.get(target) else {
            return false;
        };
        match self.table.get(source) {
            Type::Function(source_signature) => self.signature_relates(
                source_signature,
                target_signature,
                strictness,
                ParameterVariance::Bivariant,
            ),
            Type::Intersection(sources) => sources
                .iter()
                .any(|source| self.method_target_overload_relates(*source, target, strictness)),
            _ => false,
        }
    }

    fn signature_relates(
        &self,
        source: &FunctionSignature,
        target: &FunctionSignature,
        strictness: Strictness,
        variance: ParameterVariance,
    ) -> bool {
        self.with_parameter_aliases(source, target, strictness, variance, || {
            let (source_required, _, _) = source.arity();
            let (target_required, _, target_rest) = target.arity();
            if target_rest.is_none() && source_required > target_required {
                return false;
            }
            let positions = source.parameters().len().max(target.parameters().len());
            for index in 0..positions {
                let (Some(source_types), Some(target_types)) = (
                    self.parameter_types_at(source, index),
                    self.parameter_types_at(target, index),
                ) else {
                    continue;
                };
                if !target_types.iter().all(|target_type| {
                    source_types.iter().any(|source_type| {
                        self.relates(*target_type, *source_type, strictness)
                            || (matches!(variance, ParameterVariance::Bivariant)
                                && self.relates(*source_type, *target_type, strictness))
                    })
                }) {
                    return false;
                }
            }
            matches!(self.table.get(target.return_type()), Type::Void)
                || self.relates(source.return_type(), target.return_type(), strictness)
        })
    }

    /// Types a signature accepts at `index`, or `None` when it accepts nothing
    /// there. A trailing tuple rest preserves its positional shape instead of
    /// comparing the tuple object itself with one scalar argument.
    fn parameter_types_at(
        &self,
        signature: &FunctionSignature,
        index: usize,
    ) -> Option<Vec<TypeId>> {
        let parameters = signature.parameters();
        if let Some(parameter) = parameters.get(index) {
            return Some(if parameter.rest() {
                self.rest_types_at(parameter.type_id(), 0)
            } else {
                vec![parameter.type_id()]
            });
        }
        let rest_index = parameters.len().checked_sub(1)?;
        let rest = &parameters[rest_index];
        rest.rest()
            .then(|| self.rest_types_at(rest.type_id(), index - rest_index))
    }

    fn rest_types_at(&self, type_id: TypeId, index: usize) -> Vec<TypeId> {
        let mut current = type_id;
        let mut seen = HashSet::new();
        while seen.insert(current) {
            match self.table.get(current) {
                Type::Array(element) => return vec![*element],
                Type::Tuple(shape) => return shape.element_types_at(index),
                Type::Any | Type::Error => return vec![current],
                Type::Named(symbol) => {
                    if let Some(constraint) = self.table.type_parameter_constraint(*symbol) {
                        current = constraint;
                        continue;
                    }
                    return vec![type_id];
                }
                _ => return vec![type_id],
            }
        }
        vec![type_id]
    }
}

#[cfg(test)]
mod tests {
    use super::super::binder::{FunctionParameter, TypeParameterBounds};
    use super::super::{PropertyType, SymbolId};
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
    fn strict_null_assignability_keeps_numeric_enum_concession() {
        let mut table = TypeTable::new();
        let enum_type = table.numeric_enum(SymbolId::new(201));
        let number = table.number();
        let null = table.null_type();
        let relations = TypeRelations::new(&table);

        assert!(relations.assignable_with_strict_null(number, enum_type));
        assert!(!relations.assignable_with_strict_null(null, enum_type));
    }

    #[test]
    fn strict_null_allows_explicit_undefined_for_optional_property() {
        // `{p?: number} = {p: undefined}` is accepted under strict null checks
        // but still rejected by structural subtyping, and rejected when the
        // target property is non-optional.
        let mut table = TypeTable::new();
        let number = table.number();
        let undefined = table.undefined_type();
        let source = table.object_type(vec![PropertyType::new("p", false, undefined)]);
        let optional_target = table.object_type(vec![PropertyType::new("p", true, number)]);
        let required_target = table.object_type(vec![PropertyType::new("p", false, number)]);
        let relations = TypeRelations::new(&table);

        assert!(relations.assignable_with_strict_null(source, optional_target));
        assert!(relations.assignable(source, optional_target));
        assert!(!relations.subtype(source, optional_target));
        assert!(!relations.assignable_with_strict_null(source, required_target));
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
    fn comparable_relation_accepts_one_union_overlap_only() {
        let mut table = TypeTable::new();
        let a = table.string_literal("a");
        let b = table.string_literal("b");
        let c = table.string_literal("c");
        let number = table.number();
        let string = table.string();
        let broad_tag = table.union(&[a, b]);
        let broad = table.object_type(vec![
            PropertyType::new("kind", false, broad_tag),
            PropertyType::new("value", false, number),
        ]);
        let narrow = table.object_type(vec![
            PropertyType::new("kind", false, a),
            PropertyType::new("value", false, number),
        ]);
        let other = table.object_type(vec![
            PropertyType::new("kind", false, c),
            PropertyType::new("other", false, string),
        ]);
        let target = table.union(&[narrow, other]);
        let conflicting = table.object_type(vec![PropertyType::new("value", false, string)]);
        let disjoint = table.object_type(vec![PropertyType::new("other", false, string)]);
        let relations = TypeRelations::new(&table);

        assert!(!relations.assignable(broad, target));
        assert!(!relations.assignable(target, broad));
        assert!(relations.comparable(broad, target));
        assert!(!relations.comparable(broad, conflicting));
        assert!(!relations.comparable(broad, disjoint));
        assert!(!relations.comparable(table.null_type(), string));
    }
    #[test]
    fn comparable_signatures_erase_generic_parameters() {
        let mut table = TypeTable::new();
        let parameter = SymbolId::new(700);
        let parameter_type = table.named(parameter);
        let generic = table.function_with_parameters(
            vec![parameter],
            vec![FunctionParameter::new(
                "value".to_owned(),
                parameter_type,
                false,
                false,
            )],
            parameter_type,
        );
        let number = table.number();
        let string = table.string();
        let concrete = table.function(vec![number], string);
        let relations = TypeRelations::new(&table);

        assert!(!relations.assignable(generic, concrete));
        assert!(!relations.assignable(concrete, generic));
        assert!(relations.comparable(generic, concrete));
    }

    #[test]
    fn intersection_uses_type_parameter_constraint_for_object_view() {
        let mut table = TypeTable::new();
        let number = table.number();
        let constraint = table.object_type(vec![PropertyType::new("value", false, number)]);
        let constrained_symbol = SymbolId::new(701);
        table.set_type_parameter_constraint(constrained_symbol, constraint);
        let constrained = table.named(constrained_symbol);
        let unconstrained = table.named(SymbolId::new(702));
        let error = table.error_type();
        let constrained_intersection = table.intersection(vec![error, constrained]);
        let unconstrained_intersection = table.intersection(vec![error, unconstrained]);
        let propertyless = table.object_type(Vec::new());
        let error_constrained_symbol = SymbolId::new(703);
        table.set_type_parameter_constraint(error_constrained_symbol, error);
        let error_constrained = table.named(error_constrained_symbol);
        let recovery_intersection = table.intersection(vec![error, error_constrained]);
        let relations = TypeRelations::new(&table);

        assert!(relations.assignable(constrained_intersection, propertyless));
        assert!(relations.assignable(propertyless, recovery_intersection));
        assert!(!relations.assignable(unconstrained_intersection, propertyless));
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
    fn method_parameters_are_bivariant_but_function_properties_are_not() {
        let mut table = TypeTable::new();
        let number = table.number();
        let literal = table.number_literal("1");
        let void = table.void();
        let takes_number = table.function(vec![number], void);
        let takes_literal = table.function(vec![literal], void);
        let narrow_method = table.object_type(vec![
            PropertyType::new("accept", false, takes_literal).with_method(true),
        ]);
        let broad_method = table.object_type(vec![
            PropertyType::new("accept", false, takes_number).with_method(true),
        ]);
        let narrow_property =
            table.object_type(vec![PropertyType::new("accept", false, takes_literal)]);
        let broad_property =
            table.object_type(vec![PropertyType::new("accept", false, takes_number)]);
        let relations = TypeRelations::new(&table);

        assert!(relations.assignable(narrow_method, broad_method));
        assert!(relations.assignable(broad_method, narrow_method));
        assert!(!relations.assignable(narrow_property, broad_property));
        assert!(relations.assignable(broad_property, narrow_property));
    }

    #[test]
    fn instantiated_method_constraints_are_bivariant_within_one_declaration() {
        let mut table = TypeTable::new();
        let parameter = SymbolId::new(401);
        let distinct_parameter = SymbolId::new(402);
        let parameter_type = table.named(parameter);
        let distinct_type = table.named(distinct_parameter);
        let number = table.number();
        let literal = table.number_literal("1");
        let void = table.void();
        let source = table.function_with_parameter_bounds(
            vec![parameter],
            vec![TypeParameterBounds::new(Some(literal), None)],
            vec![FunctionParameter::new(
                "value".to_owned(),
                parameter_type,
                false,
                false,
            )],
            void,
            false,
        );
        let target = table.function_with_parameter_bounds(
            vec![parameter],
            vec![TypeParameterBounds::new(Some(number), None)],
            vec![FunctionParameter::new(
                "value".to_owned(),
                parameter_type,
                false,
                false,
            )],
            void,
            false,
        );
        let distinct_target = table.function_with_parameter_bounds(
            vec![distinct_parameter],
            vec![TypeParameterBounds::new(Some(number), None)],
            vec![FunctionParameter::new(
                "value".to_owned(),
                distinct_type,
                false,
                false,
            )],
            void,
            false,
        );
        let source_method = table.object_type(vec![
            PropertyType::new("accept", false, source).with_method(true),
        ]);
        let target_method = table.object_type(vec![
            PropertyType::new("accept", false, target).with_method(true),
        ]);
        let distinct_target_method = table.object_type(vec![
            PropertyType::new("accept", false, distinct_target).with_method(true),
        ]);
        let source_property = table.object_type(vec![PropertyType::new("accept", false, source)]);
        let target_property = table.object_type(vec![PropertyType::new("accept", false, target)]);
        let relations = TypeRelations::new(&table);

        assert!(relations.assignable(source_method, target_method));
        assert!(!relations.assignable(source_method, distinct_target_method));
        assert!(!relations.assignable(source_property, target_property));
    }

    #[test]
    fn method_overloads_require_target_coverage_and_covariant_returns() {
        let mut table = TypeTable::new();
        let number = table.number();
        let string = table.string();
        let literal = table.number_literal("1");
        let void = table.void();
        let number_source = table.function(vec![number], void);
        let string_source = table.function(vec![string], void);
        let full_source = table.intersection_ordered(vec![number_source, string_source]);
        let literal_target = table.function(vec![literal], void);
        let string_target = table.function(vec![string], void);
        let target_overloads = table.intersection_ordered(vec![literal_target, string_target]);
        let missing = table.object_type(vec![
            PropertyType::new("accept", false, number_source).with_method(true),
        ]);
        let full = table.object_type(vec![
            PropertyType::new("accept", false, full_source).with_method(true),
        ]);
        let target = table.object_type(vec![
            PropertyType::new("accept", false, target_overloads).with_method(true),
        ]);
        let bad_return = table.function(vec![number], number);
        let literal_return = table.function(vec![literal], literal);
        let bad_return_source = table.object_type(vec![
            PropertyType::new("accept", false, bad_return).with_method(true),
        ]);
        let literal_return_target = table.object_type(vec![
            PropertyType::new("accept", false, literal_return).with_method(true),
        ]);
        let relations = TypeRelations::new(&table);

        assert!(!relations.assignable(missing, target));
        assert!(relations.assignable(full, target));
        assert!(!relations.assignable(bad_return_source, literal_return_target));
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
        // The first distinct query memoizes only the outermost pair (depth 1),
        // not its nested structural sub-queries which run at depth >1.
        assert!(relations.assignable(literal_outer, outer));
        let grown = relations.cached_pairs();
        assert_eq!(grown, 1);
        // Repeating the same pair returns the identical result from the cache.
        assert!(relations.assignable(literal_outer, outer));
        assert_eq!(relations.cached_pairs(), grown);
        // The strict mode memoizes under its own key.
        assert!(relations.subtype(literal_outer, outer));
        let after_strict = relations.cached_pairs();
        assert_eq!(after_strict, 2);
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

    // --- AppliedClass variance tests (TypeScript 7.0.2 direction semantics) ---

    /// Helper: declare a generic class with one type parameter `T` and a raw
    /// object template, then publish it as final. Returns the parameter symbol.
    fn declare_generic_class(
        table: &mut TypeTable,
        class_symbol: SymbolId,
        param_symbol: SymbolId,
        raw: TypeId,
    ) {
        table.declare_class(class_symbol, vec![param_symbol]);
        table.finish_class_bounds(class_symbol, vec![TypeParameterBounds::NONE]);
        table.publish_final_class_template(class_symbol, raw);
    }

    /// `Producer<T> { value: T }` — covariant in T.
    /// `Producer<Dog> <: Producer<Animal>` but not the reverse.
    #[test]
    fn producer_is_covariant_in_its_type_parameter() {
        let mut table = TypeTable::new();
        let string = table.string();
        let animal = table.object_type(vec![PropertyType::new("name", false, string)]);
        let dog = table.object_type(vec![
            PropertyType::new("name", false, string),
            PropertyType::new("breed", false, string),
        ]);
        let producer = SymbolId::new(400);
        let t = SymbolId::new(401);
        let named_t = table.named(t);
        let raw = table.object_type(vec![PropertyType::new("value", false, named_t)]);
        declare_generic_class(&mut table, producer, t, raw);
        let producer_animal = table.applied_class(producer, vec![animal]);
        let producer_dog = table.applied_class(producer, vec![dog]);
        let relations = TypeRelations::new(&table);
        // Covariant: Dog <: Animal, so Producer<Dog> <: Producer<Animal>.
        assert!(relations.subtype(producer_dog, producer_animal));
        assert!(!relations.subtype(producer_animal, producer_dog));
        // Assignability follows the same direction.
        assert!(relations.assignable(producer_dog, producer_animal));
        assert!(!relations.assignable(producer_animal, producer_dog));
    }

    /// `Sink<T> { consume(value: T): void }` — contravariant in T.
    /// `Sink<Animal> <: Sink<Dog>` but not the reverse.
    #[test]
    fn sink_is_contravariant_in_its_type_parameter() {
        let mut table = TypeTable::new();
        let string = table.string();
        let void = table.void();
        let animal = table.object_type(vec![PropertyType::new("name", false, string)]);
        let dog = table.object_type(vec![
            PropertyType::new("name", false, string),
            PropertyType::new("breed", false, string),
        ]);
        let sink = SymbolId::new(410);
        let t = SymbolId::new(411);
        let named_t = table.named(t);
        let consume = table.function(vec![named_t], void);
        let raw = table.object_type(vec![PropertyType::new("consume", false, consume)]);
        declare_generic_class(&mut table, sink, t, raw);
        let sink_animal = table.applied_class(sink, vec![animal]);
        let sink_dog = table.applied_class(sink, vec![dog]);
        let relations = TypeRelations::new(&table);
        // Contravariant: Animal is a supertype of Dog, so Sink<Animal> <: Sink<Dog>.
        assert!(relations.subtype(sink_animal, sink_dog));
        assert!(!relations.subtype(sink_dog, sink_animal));
        assert!(relations.assignable(sink_animal, sink_dog));
        assert!(!relations.assignable(sink_dog, sink_animal));
    }

    /// `Cell<T> { value: T; set(value: T): void }` — invariant in T.
    /// Neither direction relates when the arguments differ.
    #[test]
    fn cell_is_invariant_in_its_type_parameter() {
        let mut table = TypeTable::new();
        let string = table.string();
        let void = table.void();
        let animal = table.object_type(vec![PropertyType::new("name", false, string)]);
        let dog = table.object_type(vec![
            PropertyType::new("name", false, string),
            PropertyType::new("breed", false, string),
        ]);
        let cell = SymbolId::new(420);
        let t = SymbolId::new(421);
        let named_t = table.named(t);
        let set = table.function(vec![named_t], void);
        let raw = table.object_type(vec![
            PropertyType::new("value", false, named_t),
            PropertyType::new("set", false, set),
        ]);
        declare_generic_class(&mut table, cell, t, raw);
        let cell_animal = table.applied_class(cell, vec![animal]);
        let cell_dog = table.applied_class(cell, vec![dog]);
        let relations = TypeRelations::new(&table);
        // Invariant: the covariant `value` and contravariant `set` cancel, so
        // neither direction relates when the arguments differ.
        assert!(!relations.subtype(cell_dog, cell_animal));
        assert!(!relations.subtype(cell_animal, cell_dog));
        assert!(!relations.assignable(cell_dog, cell_animal));
        assert!(!relations.assignable(cell_animal, cell_dog));
        // Identity still relates.
        assert!(relations.subtype(cell_dog, cell_dog));
    }

    /// `Phantom<T> { tag: number }` — T is unused, so the argument is
    /// irrelevant and all applications are mutually assignable.
    #[test]
    fn phantom_type_parameter_is_irrelevant() {
        let mut table = TypeTable::new();
        let string = table.string();
        let animal = table.object_type(vec![PropertyType::new("name", false, string)]);
        let dog = table.object_type(vec![
            PropertyType::new("name", false, string),
            PropertyType::new("breed", false, string),
        ]);
        let phantom = SymbolId::new(430);
        let t = SymbolId::new(431);
        let raw = table.object_type(vec![PropertyType::new("tag", false, table.number())]);
        declare_generic_class(&mut table, phantom, t, raw);
        let phantom_animal = table.applied_class(phantom, vec![animal]);
        let phantom_dog = table.applied_class(phantom, vec![dog]);
        let relations = TypeRelations::new(&table);
        // Phantom: T is unused, so both applications share the same view and
        // relate in both directions.
        assert!(relations.subtype(phantom_dog, phantom_animal));
        assert!(relations.subtype(phantom_animal, phantom_dog));
        assert!(relations.assignable(phantom_dog, phantom_animal));
        assert!(relations.assignable(phantom_animal, phantom_dog));
    }

    /// AppliedClass instances relate structurally across class symbols and flow
    /// into the non-primitive object top type.
    #[test]
    fn applied_classes_are_structural_and_flow_into_object_top() {
        let mut table = TypeTable::new();
        let foo = SymbolId::new(440);
        let t = SymbolId::new(441);
        let named_t = table.named(t);
        let foo_raw = table.object_type(vec![PropertyType::new("x", false, named_t)]);
        declare_generic_class(&mut table, foo, t, foo_raw);
        let bar = SymbolId::new(450);
        let u = SymbolId::new(451);
        let named_u = table.named(u);
        let bar_raw = table.object_type(vec![PropertyType::new("x", false, named_u)]);
        declare_generic_class(&mut table, bar, u, bar_raw);
        let string = table.string();
        let number = table.number();
        let foo_string = table.applied_class(foo, vec![string]);
        let bar_string = table.applied_class(bar, vec![string]);
        let bar_number = table.applied_class(bar, vec![number]);
        let object = table.object();
        let relations = TypeRelations::new(&table);

        assert!(relations.subtype(foo_string, object));
        assert!(relations.subtype(foo_string, bar_string));
        assert!(relations.subtype(bar_string, foo_string));
        assert!(!relations.subtype(bar_number, foo_string));
    }

    /// Nested applications relate through their own substituted views.
    #[test]
    fn nested_applied_class_heads_relate_structurally() {
        let mut table = TypeTable::new();
        let string = table.string();
        let animal = table.object_type(vec![PropertyType::new("name", false, string)]);
        let dog = table.object_type(vec![
            PropertyType::new("name", false, string),
            PropertyType::new("breed", false, string),
        ]);
        let inner = SymbolId::new(460);
        let t_inner = SymbolId::new(461);
        let named_inner = table.named(t_inner);
        let inner_raw = table.object_type(vec![PropertyType::new("value", false, named_inner)]);
        declare_generic_class(&mut table, inner, t_inner, inner_raw);
        let inner_animal = table.applied_class(inner, vec![animal]);
        let inner_dog = table.applied_class(inner, vec![dog]);
        let outer = SymbolId::new(470);
        let t_outer = SymbolId::new(471);
        let named_outer = table.named(t_outer);
        let applied_inner = table.applied_class(inner, vec![named_outer]);
        let outer_raw = table.object_type(vec![PropertyType::new("child", false, applied_inner)]);
        declare_generic_class(&mut table, outer, t_outer, outer_raw);
        let outer_animal = table.applied_class(outer, vec![animal]);
        let outer_dog = table.applied_class(outer, vec![dog]);
        let relations = TypeRelations::new(&table);

        assert!(relations.subtype(outer_dog, outer_animal));
        assert!(!relations.subtype(outer_animal, outer_dog));
        assert_ne!(inner_animal, inner_dog);
    }

    #[test]
    fn recursive_applied_classes_use_argument_sensitive_pair_keys() {
        let mut table = TypeTable::new();
        let box_symbol = SymbolId::new(480);
        let t = SymbolId::new(481);
        table.declare_class(box_symbol, vec![t]);
        table.finish_class_bounds(box_symbol, vec![TypeParameterBounds::NONE]);
        let named_t = table.named(t);
        let recursive = table.applied_class(box_symbol, vec![named_t]);
        let raw = table.object_type(vec![
            PropertyType::new("value", false, named_t),
            PropertyType::new("next", false, recursive),
        ]);
        table.publish_final_class_template(box_symbol, raw);
        let one = table.number_literal("1");
        let number = table.number();
        let box_one = table.applied_class(box_symbol, vec![one]);
        let box_number = table.applied_class(box_symbol, vec![number]);
        let relations = TypeRelations::new(&table);

        assert!(relations.subtype(box_one, box_number));
        assert!(!relations.subtype(box_number, box_one));
    }

    #[test]
    fn applied_classes_with_private_members_require_same_origin() {
        use crate::syntax::Accessibility;

        let mut table = TypeTable::new();
        let left = SymbolId::new(490);
        let left_t = SymbolId::new(491);
        let named_left = table.named(left_t);
        let left_raw = table.object_type(vec![
            PropertyType::new("value", false, named_left)
                .with_accessibility(Accessibility::Private, Some(left)),
        ]);
        declare_generic_class(&mut table, left, left_t, left_raw);
        let right = SymbolId::new(492);
        let right_t = SymbolId::new(493);
        let named_right = table.named(right_t);
        let right_raw = table.object_type(vec![
            PropertyType::new("value", false, named_right)
                .with_accessibility(Accessibility::Private, Some(right)),
        ]);
        declare_generic_class(&mut table, right, right_t, right_raw);
        let number = table.number();
        let left_number = table.applied_class(left, vec![number]);
        let right_number = table.applied_class(right, vec![number]);
        let relations = TypeRelations::new(&table);

        assert!(relations.assignable(left_number, left_number));
        assert!(!relations.assignable(left_number, right_number));
        assert!(!relations.assignable(right_number, left_number));
    }

    #[test]
    fn object_literal_satisfies_class_with_public_member() {
        use crate::syntax::Accessibility;
        let mut table = TypeTable::new();
        let number = table.number();
        let class_symbol = SymbolId::new(500);
        table.declare_class(class_symbol, Vec::new());
        let raw = table.object_type(vec![
            PropertyType::new("x", false, number)
                .with_accessibility(Accessibility::Public, Some(class_symbol)),
        ]);
        table.publish_final_class_template(class_symbol, raw);
        let class = table.applied_class(class_symbol, Vec::new());
        let literal = table.object_type(vec![PropertyType::new("x", false, number)]);
        let relations = TypeRelations::new(&table);

        assert!(relations.assignable(literal, class));
        assert!(relations.subtype(literal, class));
    }

    #[test]
    fn object_literal_cannot_satisfy_class_by_private_member() {
        use crate::syntax::Accessibility;
        let mut table = TypeTable::new();
        let number = table.number();
        let class_symbol = SymbolId::new(501);
        table.declare_class(class_symbol, Vec::new());
        let raw = table.object_type(vec![
            PropertyType::new("x", false, number)
                .with_accessibility(Accessibility::Private, Some(class_symbol)),
        ]);
        table.publish_final_class_template(class_symbol, raw);
        let class = table.applied_class(class_symbol, Vec::new());
        let literal = table.object_type(vec![PropertyType::new("x", false, number)]);
        let relations = TypeRelations::new(&table);

        assert!(!relations.assignable(literal, class));
        assert!(!relations.subtype(literal, class));
    }

    #[test]
    fn object_literal_cannot_satisfy_class_by_protected_member() {
        use crate::syntax::Accessibility;
        let mut table = TypeTable::new();
        let number = table.number();
        let class_symbol = SymbolId::new(502);
        table.declare_class(class_symbol, Vec::new());
        let raw = table.object_type(vec![
            PropertyType::new("x", false, number)
                .with_accessibility(Accessibility::Protected, Some(class_symbol)),
        ]);
        table.publish_final_class_template(class_symbol, raw);
        let class = table.applied_class(class_symbol, Vec::new());
        let literal = table.object_type(vec![PropertyType::new("x", false, number)]);
        let relations = TypeRelations::new(&table);

        assert!(!relations.assignable(literal, class));
        assert!(!relations.subtype(literal, class));
    }

    #[test]
    fn class_private_member_requires_same_declaring_origin() {
        use crate::syntax::Accessibility;
        let mut table = TypeTable::new();
        let number = table.number();
        let class_symbol = SymbolId::new(503);
        let other_symbol = SymbolId::new(504);
        table.declare_class(class_symbol, Vec::new());
        table.declare_class(other_symbol, Vec::new());
        let raw = table.object_type(vec![
            PropertyType::new("x", false, number)
                .with_accessibility(Accessibility::Private, Some(class_symbol)),
        ]);
        table.publish_final_class_template(class_symbol, raw);
        let class = table.applied_class(class_symbol, Vec::new());
        let mimic = table.object_type(vec![
            PropertyType::new("x", false, number)
                .with_accessibility(Accessibility::Private, Some(other_symbol)),
        ]);
        let relations = TypeRelations::new(&table);

        assert!(!relations.assignable(mimic, class));
        assert!(!relations.subtype(mimic, class));
    }

    #[test]
    fn optional_tuple_accepts_every_source_length_and_checks_present_values() {
        let mut table = TypeTable::new();
        let number = table.number();
        let string = table.string();
        let empty = table.tuple(Vec::new());
        let optional_number = table.tuple_shape(TupleShape {
            prefix: vec![number],
            required: 0,
            rest: None,
            suffix: Vec::new(),
        });
        let optional_string = table.tuple_shape(TupleShape {
            prefix: vec![string],
            required: 0,
            rest: None,
            suffix: Vec::new(),
        });
        let relations = TypeRelations::new(&table);

        assert!(relations.assignable(empty, optional_number));
        assert!(!relations.assignable(optional_number, empty));
        assert!(!relations.assignable(optional_number, optional_string));
    }

    #[test]
    fn tuple_relation_checks_required_positions_and_movable_suffixes() {
        let mut table = TypeTable::new();
        let number = table.number();
        let string = table.string();
        let boolean = table.boolean();
        let empty = table.tuple(Vec::new());
        let required_number = table.tuple(vec![number]);
        let string_suffix = table.tuple_shape(TupleShape {
            prefix: Vec::new(),
            required: 0,
            rest: Some(number),
            suffix: vec![string],
        });
        let boolean_suffix = table.tuple_shape(TupleShape {
            prefix: Vec::new(),
            required: 0,
            rest: Some(number),
            suffix: vec![boolean],
        });
        let relations = TypeRelations::new(&table);

        assert!(!relations.assignable(empty, required_number));
        assert!(!relations.assignable(string_suffix, boolean_suffix));
    }

    #[test]
    fn tuple_relation_preserves_layout_correlation() {
        let mut table = TypeTable::new();
        let a = table.number();
        let b = table.string();
        let c = table.boolean();
        let source = table.tuple(vec![c, b]);
        let target = table.tuple_shape(TupleShape {
            prefix: vec![a, b],
            required: 0,
            rest: Some(c),
            suffix: Vec::new(),
        });
        let relations = TypeRelations::new(&table);

        assert!(!relations.assignable(source, target));
    }
}
