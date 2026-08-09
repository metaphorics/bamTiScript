//! Type-parameter inference over the interned [`TypeTable`]: generic call
//! inference, contextual signatures, and inference priorities.
//!
//! Generic functions are modeled as a [`Type::Function`] whose parameter and
//! return types mention [`Type::Named`] occurrences of type-parameter symbols.
//! [`InferenceContext`] opens one inference session over such a signature: each
//! [`InferenceContext::infer_from_argument`] call walks a declared parameter
//! type against a concrete argument type and records candidates for every
//! type-parameter occurrence it reaches. [`InferenceContext::resolve`] then
//! collapses the accumulated candidates into one [`InferredTypeArguments`]
//! mapping per type parameter, applying the parameter's `extends` constraint
//! and default as fallbacks.
//!
//! The resulting [`InferredTypeArguments`] also drives contextual typing:
//! [`InferredTypeArguments::instantiate`] and
//! [`InferredTypeArguments::instantiate_signature`] substitute the inferred
//! arguments through a type or signature, yielding the concrete signature a
//! contextually typed lambda is checked against.
//!
//! # Inference priorities
//!
//! Every candidate carries an [`InferencePriority`] tier, ordered
//! `Top > Middle > Low`:
//!
//! - **Top** — a *naked* type variable in a covariant position: the declared
//!   parameter type is exactly `T` (`identity<T>(value: T)`).
//! - **Middle** — a type variable nested inside a covariant composite: array
//!   elements, object properties, union members, and function return types
//!   (`first<T>(items: T[])`).
//! - **Low** — a type variable reached through a contravariant position:
//!   the parameter types of a function-typed argument
//!   (`map<T, U>(items: T[], f: (item: T) => U)` reaches `T` through `f`).
//!
//! Resolution is deterministic. Candidates are processed in argument
//! (encounter) order; the highest non-empty tier wins, and lower tiers are
//! discarded. Within one tier, distinct candidates combine by tier kind:
//!
//! - Upper-bound tiers (`Top`, `Middle`) pick the *best common supertype* —
//!   the first candidate in encounter order that every other tier candidate
//!   structurally subtypes. With no such candidate the tier resolves to the
//!   normalized union of its candidates.
//! - The lower-bound tier (`Low`) picks the *best common subtype* — the first
//!   candidate that subtypes every other. With no such candidate the first
//!   candidate in encounter order wins; intersection types are not modeled.
//!
//! A parameter with no candidates falls back to its declared default, then to
//! its `extends` constraint, then to `unknown`. An inferred candidate that is
//! not assignable to the parameter's `extends` constraint is replaced by the
//! constraint. Each outcome is recorded as [`InferenceProvenance`] so future
//! diagnostics can attribute a type argument to its source.

use super::binder::{
    FunctionParameter, FunctionSignature, IndexSignature, ObjectType, PropertyType, SymbolId, Type,
    TypeId, TypeTable,
};
use super::relations::TypeRelations;

/// The tier of one inference candidate, deciding which candidates a type
/// parameter resolves from. Ordered `Low < Middle < Top`; see the module
/// documentation for the exact position rules.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialOrd, PartialEq)]
pub enum InferencePriority {
    /// Reached through a contravariant position (function parameter types).
    Low,
    /// Nested inside a covariant composite (elements, properties, returns).
    Middle,
    /// A naked type variable in a covariant position.
    Top,
}

/// Where one resolved type argument came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InferenceProvenance {
    /// Inferred from an argument candidate.
    Inferred,
    /// No candidate survived; the `extends` constraint was substituted, or an
    /// inferred candidate failed the constraint check.
    Constraint,
    /// No candidates existed; the declared default type argument was used.
    Default,
    /// No candidates, default, or constraint; `unknown` was substituted.
    Unknown,
    /// Supplied explicitly in a type-argument list.
    Explicit,
}

/// One type parameter opened for inference, with its resolved bounds.
///
/// The constraint (`extends` clause) and default (`= ...` clause) are consumed
/// as already-resolved [`TypeId`]s; binding and resolving those syntax nodes is
/// the binder's job, not this module's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InferenceParameter {
    symbol: SymbolId,
    constraint: Option<TypeId>,
    default: Option<TypeId>,
}

impl InferenceParameter {
    /// Opens the type parameter identified by `symbol` for inference.
    #[must_use]
    pub const fn new(symbol: SymbolId) -> Self {
        Self {
            symbol,
            constraint: None,
            default: None,
        }
    }

    /// Attaches the resolved `extends` constraint.
    #[must_use]
    pub const fn with_constraint(mut self, constraint: TypeId) -> Self {
        self.constraint = Some(constraint);
        self
    }

    /// Attaches the resolved default type argument.
    #[must_use]
    pub const fn with_default(mut self, default: TypeId) -> Self {
        self.default = Some(default);
        self
    }

    /// The symbol identifying the type parameter in [`Type::Named`].
    #[must_use]
    pub const fn symbol(&self) -> SymbolId {
        self.symbol
    }

    /// The resolved `extends` constraint, if any.
    #[must_use]
    pub const fn constraint(&self) -> Option<TypeId> {
        self.constraint
    }

    /// The resolved default type argument, if any.
    #[must_use]
    pub const fn default(&self) -> Option<TypeId> {
        self.default
    }
}

/// One candidate type recorded for a type parameter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InferenceCandidate {
    type_id: TypeId,
    priority: InferencePriority,
    source: u32,
}
/// The variance of the position currently being walked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Variance {
    Covariant,
    Contravariant,
}

/// One resolved type argument for one type parameter, with provenance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InferredTypeArgument {
    symbol: SymbolId,
    type_id: TypeId,
    provenance: InferenceProvenance,
}

impl InferredTypeArgument {
    /// The type parameter this argument was resolved for.
    #[must_use]
    pub const fn symbol(&self) -> SymbolId {
        self.symbol
    }

    /// The resolved type argument.
    #[must_use]
    pub const fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// Where the resolved type argument came from.
    #[must_use]
    pub const fn provenance(&self) -> InferenceProvenance {
        self.provenance
    }

    /// Creates a resolved argument for `symbol` from `type_id` with the given provenance.
    #[must_use]
    pub const fn new(symbol: SymbolId, type_id: TypeId, provenance: InferenceProvenance) -> Self {
        Self {
            symbol,
            type_id,
            provenance,
        }
    }
}

/// The resolved type arguments of one inference session, in type-parameter
/// declaration order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferredTypeArguments {
    arguments: Box<[InferredTypeArgument]>,
}

impl InferredTypeArguments {
    /// The resolved type arguments in type-parameter declaration order.
    #[must_use]
    pub fn arguments(&self) -> &[InferredTypeArgument] {
        &self.arguments
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.arguments.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.arguments.is_empty()
    }

    /// Build a substitution from explicit type arguments.
    #[must_use]
    pub fn new(arguments: Vec<InferredTypeArgument>) -> Self {
        Self {
            arguments: arguments.into_boxed_slice(),
        }
    }

    /// The resolved type argument for `symbol`, if it was part of the session.
    #[must_use]
    pub fn get(&self, symbol: SymbolId) -> Option<TypeId> {
        self.arguments
            .iter()
            .find(|argument| argument.symbol() == symbol)
            .map(InferredTypeArgument::type_id)
    }

    /// The provenance of the resolved argument for `symbol`.
    #[must_use]
    pub fn provenance(&self, symbol: SymbolId) -> Option<InferenceProvenance> {
        self.arguments
            .iter()
            .find(|argument| argument.symbol() == symbol)
            .map(InferredTypeArgument::provenance)
    }

    /// Substitutes the inferred arguments through `ty`, returning the interned
    /// result. Types that mention no inferred type parameter intern back to
    /// their original [`TypeId`].
    pub fn instantiate(&self, table: &mut TypeTable, ty: TypeId) -> TypeId {
        match table.get(ty).clone() {
            Type::Named(symbol) => self.get(symbol).unwrap_or(ty),
            Type::Array(element) => {
                let element = self.instantiate(table, element);
                table.array(element)
            }
            Type::Tuple(elements) => {
                let elements = elements
                    .iter()
                    .map(|element| self.instantiate(table, *element))
                    .collect();
                table.tuple(elements)
            }
            Type::Union(members) => {
                let members: Vec<TypeId> = members
                    .iter()
                    .map(|member| self.instantiate(table, *member))
                    .collect();
                table.union(&members)
            }
            Type::ObjectType(object) => {
                let properties: Vec<PropertyType> = object
                    .properties
                    .iter()
                    .map(|property| {
                        PropertyType::new(
                            property.name(),
                            property.optional(),
                            self.instantiate(table, property.type_id()),
                        )
                        .with_readonly(property.readonly())
                    })
                    .collect();
                let call_signatures = object
                    .call_signatures
                    .iter()
                    .map(|signature| {
                        let type_id = self.instantiate_function(
                            table,
                            signature.type_parameters(),
                            signature,
                        );
                        let Type::Function(signature) = table.get(type_id).clone() else {
                            unreachable!("function instantiation must produce a function type");
                        };
                        signature
                    })
                    .collect();
                let index_signatures = object
                    .index_signatures
                    .iter()
                    .map(|signature| IndexSignature {
                        readonly: signature.readonly,
                        parameters: signature
                            .parameters
                            .iter()
                            .map(|parameter| {
                                FunctionParameter::new(
                                    parameter.name().to_owned(),
                                    self.instantiate(table, parameter.type_id()),
                                    parameter.optional(),
                                    parameter.rest(),
                                )
                            })
                            .collect(),
                        value_type: self.instantiate(table, signature.value_type),
                    })
                    .collect();
                table.object_type_with_members(ObjectType {
                    properties,
                    call_signatures,
                    index_signatures,
                })
            }
            Type::Function(signature) => {
                self.instantiate_function(table, signature.type_parameters(), &signature)
            }
            Type::Error
            | Type::Intersection(_)
            | Type::Any
            | Type::Unknown
            | Type::Never
            | Type::Void
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
            | Type::NumericEnum(_) => ty,
        }
    }

    /// Substitutes the inferred arguments through a generic signature and
    /// interns the result: the *contextual signature* a lambda argument is
    /// checked against once the call's type arguments are known.
    pub fn instantiate_signature(
        &self,
        table: &mut TypeTable,
        signature: &FunctionSignature,
    ) -> TypeId {
        self.instantiate_function(table, &[], signature)
    }

    /// Shared core of [`Self::instantiate`] and [`Self::instantiate_signature`]:
    /// substitutes the inferred arguments through `signature`'s parameters and
    /// return type, preserving `type_parameters` on the interned result. The
    /// top-level contextual signature passes `&[]` (its own generics are
    /// resolved); nested function types pass their own `type_parameters` so
    /// inner generics survive instantiation.
    fn instantiate_function(
        &self,
        table: &mut TypeTable,
        type_parameters: &[SymbolId],
        signature: &FunctionSignature,
    ) -> TypeId {
        let parameters: Vec<FunctionParameter> = signature
            .parameters()
            .iter()
            .map(|parameter| {
                FunctionParameter::new(
                    parameter.name().to_owned(),
                    self.instantiate(table, parameter.type_id()),
                    parameter.optional(),
                    parameter.rest(),
                )
            })
            .collect();
        let return_type = self.instantiate(table, signature.return_type());
        table.function_with_parameters(type_parameters.to_vec(), parameters, return_type)
    }
}

/// One inference session over an interned [`TypeTable`].
///
/// Construct with the type parameters to solve, feed every argument of the
/// call through [`InferenceContext::infer_from_argument`] (or
/// [`InferenceContext::infer_from_arguments`] for a whole signature), then
/// consume the context with [`InferenceContext::resolve`].
pub struct InferenceContext<'table> {
    table: &'table mut TypeTable,
    parameters: Vec<ParameterInference>,
}

struct ParameterInference {
    parameter: InferenceParameter,
    candidates: Vec<InferenceCandidate>,
}

impl<'table> InferenceContext<'table> {
    /// Opens an inference session for `parameters`, in declaration order.
    #[must_use]
    pub fn new(table: &'table mut TypeTable, parameters: &[InferenceParameter]) -> Self {
        Self {
            table,
            parameters: parameters
                .iter()
                .map(|parameter| ParameterInference {
                    parameter: *parameter,
                    candidates: Vec::new(),
                })
                .collect(),
        }
    }

    /// Records inferences from one argument against its declared parameter
    /// type. `argument_type` is covariant: it flows into the parameter.
    pub fn infer_from_argument(
        &mut self,
        parameter_type: TypeId,
        argument_type: TypeId,
        source: u32,
    ) {
        self.infer_types(
            parameter_type,
            argument_type,
            true,
            Variance::Covariant,
            source,
        );
    }
    /// Records inferences for a whole call: each argument is zipped against
    /// the declared parameters of `signature` in order. Extra arguments and
    /// missing (optional/rest) parameters are ignored.
    pub fn infer_from_arguments(&mut self, signature: &FunctionSignature, arguments: &[TypeId]) {
        for (argument_index, parameter) in signature.parameters().iter().enumerate() {
            if parameter.rest() {
                // A rest parameter collects arguments[argument_index..], but
                // argument_index is the *parameter* index, not an argument
                // index. When fixed parameters before the rest are unsupplied
                // (e.g. f<T>(a: number, ...rest: T[]) called as f()), the
                // parameter index can exceed arguments.len() and the slice
                // would panic. Guard here, mirroring the non-rest bounds check
                // below.
                if argument_index >= arguments.len() {
                    break;
                }
                if let Type::Array(element) = self.table.get(parameter.type_id()).clone() {
                    for (offset, &argument_type) in arguments[argument_index..].iter().enumerate() {
                        self.infer_from_argument(
                            element,
                            argument_type,
                            (argument_index + offset) as u32,
                        );
                    }
                }
                break;
            }
            if argument_index >= arguments.len() {
                break;
            }
            self.infer_from_argument(
                parameter.type_id(),
                arguments[argument_index],
                argument_index as u32,
            );
        }
    }

    /// Collapses every recorded candidate into the final type arguments,
    /// applying defaults and `extends` constraints as documented in the
    /// module-level priority rules.
    #[must_use]
    pub fn resolve(mut self) -> InferredTypeArguments {
        let mut resolved = Vec::with_capacity(self.parameters.len());
        for index in 0..self.parameters.len() {
            let state = &self.parameters[index];
            let (parameter, candidates) = (state.parameter, state.candidates.clone());
            let (type_id, provenance) = self.resolve_parameter(&parameter, &candidates);
            resolved.push(InferredTypeArgument {
                symbol: parameter.symbol(),
                type_id,
                provenance,
            });
        }
        InferredTypeArguments {
            arguments: resolved.into_boxed_slice(),
        }
    }

    fn is_inference_symbol(&self, symbol: SymbolId) -> bool {
        self.parameters
            .iter()
            .any(|state| state.parameter.symbol() == symbol)
    }

    fn add_candidate(
        &mut self,
        symbol: SymbolId,
        type_id: TypeId,
        priority: InferencePriority,
        source: u32,
    ) {
        if let Some(state) = self
            .parameters
            .iter_mut()
            .find(|state| state.parameter.symbol() == symbol)
        {
            state.candidates.push(InferenceCandidate {
                type_id,
                priority,
                source,
            });
        }
    }

    fn infer_types(
        &mut self,
        parameter_type: TypeId,
        argument_type: TypeId,
        naked: bool,
        variance: Variance,
        source: u32,
    ) {
        let parameter = self.table.get(parameter_type).clone();
        match parameter {
            Type::Named(symbol) if self.is_inference_symbol(symbol) => {
                let priority = match variance {
                    Variance::Contravariant => InferencePriority::Low,
                    Variance::Covariant if naked => InferencePriority::Top,
                    Variance::Covariant => InferencePriority::Middle,
                };
                self.add_candidate(symbol, argument_type, priority, source);
            }
            Type::Array(element) => {
                if let Type::Array(argument_element) = self.table.get(argument_type).clone() {
                    self.infer_types(element, argument_element, false, variance, source);
                }
            }
            Type::Union(members) => {
                for member in members {
                    self.infer_types(member, argument_type, false, variance, source);
                }
            }
            Type::ObjectType(object) => {
                if let Type::ObjectType(argument_object) = self.table.get(argument_type).clone() {
                    for property in object.properties {
                        if let Some(argument_property) = argument_object
                            .properties
                            .iter()
                            .find(|candidate| candidate.name() == property.name())
                        {
                            self.infer_types(
                                property.type_id(),
                                argument_property.type_id(),
                                false,
                                variance,
                                source,
                            );
                        }
                    }
                }
            }
            Type::Function(signature) => {
                if let Type::Function(argument_signature) = self.table.get(argument_type).clone() {
                    for (declared, actual) in signature
                        .parameters()
                        .iter()
                        .zip(argument_signature.parameters())
                    {
                        self.infer_types(
                            declared.type_id(),
                            actual.type_id(),
                            false,
                            Variance::Contravariant,
                            source,
                        );
                    }
                    self.infer_types(
                        signature.return_type(),
                        argument_signature.return_type(),
                        false,
                        variance,
                        source,
                    );
                }
            }
            _ => {}
        }
    }

    fn resolve_parameter(
        &mut self,
        parameter: &InferenceParameter,
        candidates: &[InferenceCandidate],
    ) -> (TypeId, InferenceProvenance) {
        let Some(candidate) = self.combine_candidates(candidates) else {
            return if let Some(default) = parameter.default() {
                (default, InferenceProvenance::Default)
            } else if let Some(constraint) = parameter.constraint() {
                (constraint, InferenceProvenance::Constraint)
            } else {
                (self.table.unknown(), InferenceProvenance::Unknown)
            };
        };
        if let Some(constraint) = parameter.constraint()
            && !TypeRelations::new(self.table).assignable(candidate, constraint)
        {
            return (constraint, InferenceProvenance::Constraint);
        }
        (candidate, InferenceProvenance::Inferred)
    }

    /// Combines the candidates of one parameter into a single type following
    /// the documented priority tiers. Returns `None` with no candidates.
    fn combine_candidates(&mut self, candidates: &[InferenceCandidate]) -> Option<TypeId> {
        let best_priority = candidates
            .iter()
            .map(|candidate| candidate.priority)
            .max()?;
        // Distinct candidates of the winning tier, in encounter order.
        let mut tier: Vec<&InferenceCandidate> = Vec::new();
        for candidate in candidates {
            if candidate.priority == best_priority
                && !tier
                    .iter()
                    .any(|existing| existing.type_id == candidate.type_id)
            {
                tier.push(candidate);
            }
        }
        if tier.len() == 1 {
            return Some(tier[0].type_id);
        }
        let best = {
            let relations = TypeRelations::new(self.table);
            let type_ids: Vec<TypeId> = tier.iter().map(|candidate| candidate.type_id).collect();
            if best_priority == InferencePriority::Low {
                type_ids.iter().copied().find(|candidate| {
                    type_ids
                        .iter()
                        .all(|other| relations.subtype(*candidate, *other))
                })
            } else {
                type_ids.iter().copied().find(|candidate| {
                    type_ids
                        .iter()
                        .all(|other| relations.subtype(*other, *candidate))
                })
            }
        };
        match (best_priority, best) {
            (_, Some(best)) => Some(best),
            // Contravariant candidates with no common subtype keep the first
            // candidate in encounter order; intersection types are not modeled.
            (InferencePriority::Low, None) => Some(tier[0].type_id),
            // Covariant candidates from the same argument position must agree;
            // with no common supertype, keep the first candidate so the call
            // argument check reports the mismatch. Candidates from different
            // arguments are unioned.
            (_, None) => {
                if tier
                    .iter()
                    .all(|candidate| candidate.source == tier[0].source)
                {
                    Some(tier[0].type_id)
                } else {
                    Some(
                        self.table
                            .union(&tier.iter().map(|c| c.type_id).collect::<Vec<_>>()),
                    )
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parameter(id: u32) -> SymbolId {
        SymbolId::new(id)
    }

    /// `identity<T>(value: T): T` with a concrete argument infers the argument
    /// type at top priority and instantiates the signature end to end.
    #[test]
    fn a_naked_parameter_infers_the_argument_type_at_top_priority() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let signature = table.function(vec![t], t);
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let one = table.number_literal("1");
        let mut context =
            InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        context.infer_from_arguments(&signature, &[one]);
        let inferred = context.resolve();

        assert_eq!(inferred.get(parameter(1)), Some(one));
        assert_eq!(
            inferred.provenance(parameter(1)),
            Some(InferenceProvenance::Inferred)
        );
        // The contextual signature is (value: 1) => 1.
        let contextual = inferred.instantiate_signature(&mut table, &signature);
        assert_eq!(contextual, table.function(vec![one], one));
    }

    /// `first<T>(items: T[]): T` reaches the variable through an array element:
    /// a middle-priority candidate.
    #[test]
    fn a_nested_variable_infers_the_element_type() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let items = table.array(t);
        let signature = table.function(vec![items], t);
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let strings = table.array(table.string());
        let mut context =
            InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        context.infer_from_arguments(&signature, &[strings]);
        let inferred = context.resolve();

        assert_eq!(inferred.get(parameter(1)), Some(table.string()));
    }

    /// `map<T, U>(items: T[], f: (item: T) => U): U[]` exercises all three
    /// tiers at once: `T` wins on its top/middle candidates even though the
    /// callback parameter reaches it contravariantly, and `U` infers from the
    /// callback return.
    #[test]
    fn higher_order_inference_orders_priorities_and_types_the_callback_contextually() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let u = table.named(parameter(2));
        let items = table.array(t);
        let callback = table.function(vec![t], u);
        let result = table.array(u);
        let signature = table.function(vec![items, callback], result);
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let numbers = table.array(table.number());
        // The callback argument claims (item: string) => boolean: `T` sees a
        // contravariant `string` candidate and `U` sees `boolean`.
        let argument_callback = table.function(vec![table.string()], table.boolean());
        let mut context = InferenceContext::new(
            &mut table,
            &[
                InferenceParameter::new(parameter(1)),
                InferenceParameter::new(parameter(2)),
            ],
        );
        context.infer_from_arguments(&signature, &[numbers, argument_callback]);
        let inferred = context.resolve();

        // The array's middle-priority `number` beats the callback's
        // low-priority `string` for `T`; `U` infers `boolean` from the
        // covariant callback return.
        assert_eq!(inferred.get(parameter(1)), Some(table.number()));
        assert_eq!(inferred.get(parameter(2)), Some(table.boolean()));

        // Instantiating the signature yields the contextual signature used to
        // check the lambda: (items: number[], f: (item: number) => boolean).
        let contextual = inferred.instantiate_signature(&mut table, &signature);
        let expected_callback = table.function(vec![table.number()], table.boolean());
        let booleans = table.array(table.boolean());
        let expected = table.function(vec![numbers, expected_callback], booleans);
        assert_eq!(contextual, expected);
    }
    #[test]
    fn rest_parameter_with_unsupplied_fixed_params_does_not_panic() {
        // f<T>(a: number, ...rest: T[]) called as f() — the rest branch must not
        // slice arguments[1..] when arguments.len() == 0. With no candidates,
        // T remains unconstrained and resolves to `unknown`.
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let number = table.number();
        let rest_array = table.array(t);
        let a = FunctionParameter::new("a".to_owned(), number, false, false);
        let rest = FunctionParameter::new("rest".to_owned(), rest_array, false, true);
        let sig_type = table.function_with_parameters(vec![parameter(1)], vec![a, rest], t);
        let Type::Function(sig) = table.get(sig_type).clone() else {
            panic!("function type");
        };
        let mut ctx = InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        // No arguments supplied — fixed param and rest both unsupplied.
        ctx.infer_from_arguments(&sig, &[]);
        let inferred = ctx.resolve();
        // No panic, and T has no candidates so it falls back to `unknown`.
        assert_eq!(inferred.get(parameter(1)), Some(table.unknown()));
    }
    #[test]
    fn nested_generic_signature_is_preserved_through_instantiation() {
        // Outer: <T>(value: T): <U>(x: U) => T
        // Inner: <U>(x: U) => T  — inner <U> must survive instantiation of outer T.
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let u = table.named(parameter(2));
        let inner = table.function_with_parameters(
            vec![parameter(2)],
            vec![FunctionParameter::new("x".to_owned(), u, false, false)],
            t,
        );
        let outer = table.function_with_parameters(
            vec![parameter(1)],
            vec![FunctionParameter::new("value".to_owned(), t, false, false)],
            inner,
        );
        let Type::Function(outer_sig) = table.get(outer).clone() else {
            panic!("function type");
        };
        let string = table.string();
        let mut ctx = InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        ctx.infer_from_arguments(&outer_sig, &[string]);
        let inferred = ctx.resolve();
        assert_eq!(inferred.get(parameter(1)), Some(string));
        // Instantiate the outer return type (the inner function) — T -> string, U preserved.
        let instantiated = inferred.instantiate(&mut table, inner);
        let expected = table.function_with_parameters(
            vec![parameter(2)],
            vec![FunctionParameter::new("x".to_owned(), u, false, false)],
            string,
        );
        assert_eq!(instantiated, expected);
    }

    /// Two distinct top-priority candidates with no common supertype resolve
    /// to their union.
    #[test]
    fn equal_top_candidates_combine_into_a_union() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let signature = table.function(vec![t, t], t);
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let (one, two) = (table.number_literal("1"), table.number_literal("2"));
        let mut context =
            InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        context.infer_from_arguments(&signature, &[one, two]);
        let inferred = context.resolve();

        assert_eq!(inferred.get(parameter(1)), Some(table.union(&[one, two])));
    }
    /// Two candidates for the same type parameter from the *same* argument
    /// position must agree; with no common supertype the first one wins.
    #[test]
    fn same_source_incomparable_candidates_pick_first() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let obj = table.object_type(vec![
            PropertyType::new("a", false, t),
            PropertyType::new("b", false, t),
        ]);
        let signature = table.function(vec![obj], t);
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let one = table.number_literal("1");
        let two = table.number_literal("2");
        let arg = table.object_type(vec![
            PropertyType::new("a", false, one),
            PropertyType::new("b", false, two),
        ]);
        let mut context =
            InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        context.infer_from_arguments(&signature, &[arg]);
        let inferred = context.resolve();

        assert_eq!(inferred.get(parameter(1)), Some(one));
    }

    /// A candidate that supertypes every sibling in its tier wins without a
    /// union, regardless of encounter order.
    #[test]
    fn the_best_common_supertype_wins_an_upper_tier() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let signature = table.function(vec![t, t], t);
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let (number, one) = (table.number(), table.number_literal("1"));
        for arguments in [[one, number], [number, one]] {
            let mut context =
                InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
            context.infer_from_arguments(&signature, &arguments);
            assert_eq!(context.resolve().get(parameter(1)), Some(number));
        }
    }

    /// Contravariant candidates combine toward the best common subtype; with
    /// incomparable candidates the first in encounter order wins.
    #[test]
    fn the_best_common_subtype_wins_the_lower_tier_with_an_order_fallback() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        // `compose<T>(f: (value: T) => void, g: (value: T) => void): void`
        let callback = table.function(vec![t], table.void());
        let signature = table.function(vec![callback, callback], table.void());
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let (number, one, string) = (table.number(), table.number_literal("1"), table.string());
        let takes_number = table.function(vec![number], table.void());
        let takes_one = table.function(vec![one], table.void());
        let takes_string = table.function(vec![string], table.void());

        // 1 subtypes number, so the literal wins the lower tier.
        let mut context =
            InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        context.infer_from_arguments(&signature, &[takes_number, takes_one]);
        assert_eq!(context.resolve().get(parameter(1)), Some(one));

        // string and number are incomparable: encounter order decides.
        let mut context =
            InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        context.infer_from_arguments(&signature, &[takes_number, takes_string]);
        assert_eq!(context.resolve().get(parameter(1)), Some(number));
        let mut context =
            InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        context.infer_from_arguments(&signature, &[takes_string, takes_number]);
        assert_eq!(context.resolve().get(parameter(1)), Some(string));
    }

    /// With no candidates a parameter is fixed by its default, then its
    /// constraint, then `unknown`.
    #[test]
    fn an_unreached_parameter_falls_back_to_default_constraint_unknown() {
        let mut table = TypeTable::new();
        let (number, string, unknown) = (table.number(), table.string(), table.unknown());
        let t = table.named(parameter(1));
        assert_eq!(table.get(t), &Type::Named(parameter(1)));

        // No arguments are fed, so no parameter records a candidate.
        let context = InferenceContext::new(
            &mut table,
            &[
                InferenceParameter::new(parameter(1)).with_default(string),
                InferenceParameter::new(parameter(2)).with_constraint(number),
                InferenceParameter::new(parameter(3)),
            ],
        );
        let inferred = context.resolve();

        assert_eq!(inferred.get(parameter(1)), Some(string));
        assert_eq!(
            inferred.provenance(parameter(1)),
            Some(InferenceProvenance::Default)
        );
        assert_eq!(inferred.get(parameter(2)), Some(number));
        assert_eq!(
            inferred.provenance(parameter(2)),
            Some(InferenceProvenance::Constraint)
        );
        assert_eq!(inferred.get(parameter(3)), Some(unknown));
        assert_eq!(
            inferred.provenance(parameter(3)),
            Some(InferenceProvenance::Unknown)
        );
    }

    /// An inferred candidate that violates the `extends` constraint is
    /// replaced by the constraint.
    #[test]
    fn a_candidate_failing_its_constraint_falls_back_to_the_constraint() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let number = table.number();
        let signature = table.function(vec![t], t);
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let string = table.string();
        let mut context = InferenceContext::new(
            &mut table,
            &[InferenceParameter::new(parameter(1)).with_constraint(number)],
        );
        context.infer_from_arguments(&signature, &[string]);
        let inferred = context.resolve();

        assert_eq!(inferred.get(parameter(1)), Some(number));
        assert_eq!(
            inferred.provenance(parameter(1)),
            Some(InferenceProvenance::Constraint)
        );
    }

    /// A candidate satisfying its constraint is kept.
    #[test]
    fn a_candidate_within_its_constraint_is_kept() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let number = table.number();
        let signature = table.function(vec![t], t);
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let one = table.number_literal("1");
        let mut context = InferenceContext::new(
            &mut table,
            &[InferenceParameter::new(parameter(1)).with_constraint(number)],
        );
        context.infer_from_arguments(&signature, &[one]);
        let inferred = context.resolve();

        assert_eq!(inferred.get(parameter(1)), Some(one));
        assert_eq!(
            inferred.provenance(parameter(1)),
            Some(InferenceProvenance::Inferred)
        );
    }

    /// Object properties and union members of the declared parameter type all
    /// contribute candidates.
    #[test]
    fn object_properties_and_union_members_contribute_candidates() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let u = table.named(parameter(2));
        let pair = table.object_type(vec![
            PropertyType::new("first", false, t),
            PropertyType::new("second", false, u),
        ]);
        let flexible = table.union(&[t, table.number()]);
        let signature = table.function(vec![pair, flexible], t);
        let Type::Function(signature) = table.get(signature).clone() else {
            panic!("function type");
        };

        let (string, boolean, number) = (table.string(), table.boolean(), table.number());
        let argument_pair = table.object_type(vec![
            PropertyType::new("first", false, string),
            PropertyType::new("second", false, boolean),
            PropertyType::new("extra", false, number),
        ]);
        let mut context = InferenceContext::new(
            &mut table,
            &[
                InferenceParameter::new(parameter(1)),
                InferenceParameter::new(parameter(2)),
            ],
        );
        context.infer_from_arguments(&signature, &[argument_pair, string]);
        let inferred = context.resolve();

        // `T` sees `string` from the property and from the union member;
        // deduplicated, one candidate remains. `U` sees only `boolean`.
        assert_eq!(inferred.get(parameter(1)), Some(table.string()));
        assert_eq!(inferred.get(parameter(2)), Some(table.boolean()));
    }

    /// Instantiation leaves types without inferred parameters untouched and
    /// substitutes through nested composites.
    #[test]
    fn instantiation_substitutes_only_inferred_parameters() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let other = table.named(parameter(9));
        let (items, name) = (table.array(t), table.string());
        let composite = table.object_type(vec![
            PropertyType::new("items", false, items),
            PropertyType::new("name", false, name),
            PropertyType::new("other", false, other),
        ]);

        let number = table.number();
        let mut context =
            InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        context.infer_from_argument(t, number, 0);
        let inferred = context.resolve();

        let instantiated = inferred.instantiate(&mut table, composite);
        let numbers = table.array(number);
        let expected = table.object_type(vec![
            PropertyType::new("items", false, numbers),
            PropertyType::new("name", false, name),
            PropertyType::new("other", false, other),
        ]);
        assert_eq!(instantiated, expected);
        // Primitives and unrelated named types intern back to themselves.
        let string = table.string();
        assert_eq!(inferred.instantiate(&mut table, string), string);
        assert_eq!(inferred.instantiate(&mut table, other), other);
    }

    /// A `readonly` member of a generic object type keeps its flag through
    /// instantiation; without `.with_readonly` the flag is silently dropped.
    #[test]
    fn instantiation_preserves_readonly_on_object_properties() {
        let mut table = TypeTable::new();
        let t = table.named(parameter(1));
        let composite = table.object_type(vec![
            PropertyType::new("mutable", false, t),
            PropertyType::new("locked", false, t).with_readonly(true),
        ]);

        let number = table.number();
        let mut context =
            InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
        context.infer_from_argument(t, number, 0);
        let inferred = context.resolve();
        let instantiated = inferred.instantiate(&mut table, composite);

        let Type::ObjectType(object) = table.get(instantiated).clone() else {
            panic!("object type");
        };
        let locked = object
            .properties
            .iter()
            .find(|property| property.name() == "locked")
            .expect("locked property");
        assert!(locked.readonly(), "readonly survives instantiation");
        let mutable = object
            .properties
            .iter()
            .find(|property| property.name() == "mutable")
            .expect("mutable property");
        assert!(!mutable.readonly(), "non-readonly stays non-readonly");
    }

    /// Resolution is deterministic: the same session built twice yields the
    /// same mapping.
    #[test]
    fn resolution_is_deterministic_across_identical_sessions() {
        let run = || {
            let mut table = TypeTable::new();
            let t = table.named(parameter(1));
            let items = table.array(t);
            let signature = table.function(vec![items, t], t);
            let Type::Function(signature) = table.get(signature).clone() else {
                panic!("function type");
            };
            let one = table.number_literal("1");
            let numbers = table.array(table.number());
            let mut context =
                InferenceContext::new(&mut table, &[InferenceParameter::new(parameter(1))]);
            context.infer_from_arguments(&signature, &[numbers, one]);
            let inferred = context.resolve();
            let resolved = inferred.get(parameter(1)).expect("resolved");
            table.get(resolved).clone()
        };
        assert_eq!(run(), run());
    }
}
