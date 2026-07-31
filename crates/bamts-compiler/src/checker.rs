//! Semantic analysis: the first real checker slice over an immutable
//! [`SourceFile`].
//!
//! The checker never mutates the syntax tree. It builds three immutable models
//! from one traversal: a lexical [`Scope`] tree, a [`Symbol`] table populated by
//! binding declarations, and an interned structural [`TypeTable`] used by the
//! named type algebra and [`TypeTable::assignable`]. Binding detects duplicate
//! declarations, reference resolution detects unresolved local names, and
//! variable initializers are checked against their annotations. Its diagnostics
//! are merged with the front-end hard warnings and returned in canonical order
//! alongside the [`SemanticModel`], following the crate's `Recovered` contract.

#[path = "checker/intrinsic_environment.rs"]
mod intrinsic_environment;

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::diagnostic::{Diagnostic, DiagnosticCode, Recovered};
use crate::lint::{LintProfile, LintTable};
use crate::source::{SourceId, TextRange};
use crate::syntax::{
    ArrayElement, AssignmentTarget, BindingPattern, CallArgument, ClassDeclaration, ClassMember,
    EntityName, Expr, Expression, ForBinding, ForInitializer, FunctionBody, FunctionLike,
    FunctionType, IdentifierNode, ImportBinding, InterfaceDeclaration, KeywordType, Literal,
    MemberProperty, NodeId, ObjectMember, PropertyName, SourceFile, Statement, Token, Ty,
    TypeAliasDeclaration, TypeLiteral, TypeMember, TypeNode, TypeReference, UnaryOperator,
    VariableDeclaration, VariableKind,
};
use crate::warning::analyze_warnings;
use intrinsic_environment::GlobalEnvironment;

/// Diagnostic emitted when a block-scoped name redeclares an existing binding.
pub const DUPLICATE_DECLARATION: DiagnosticCode = DiagnosticCode::new("BAMTS-C001");
/// Diagnostic emitted when a value reference resolves to no local binding.
pub const CANNOT_FIND_NAME: DiagnosticCode = DiagnosticCode::new("BAMTS-C002");
/// Diagnostic emitted when a type reference resolves to no local type name.
pub const CANNOT_FIND_TYPE: DiagnosticCode = DiagnosticCode::new("BAMTS-C003");
/// Diagnostic emitted when an initializer is not assignable to its annotation.
pub const TYPE_NOT_ASSIGNABLE: DiagnosticCode = DiagnosticCode::new("BAMTS-C004");

const DUPLICATE_MESSAGE: &str = "A block-scoped declaration cannot redeclare an existing binding.";
const CANNOT_FIND_NAME_MESSAGE: &str = "Cannot find name in any enclosing scope.";
const CANNOT_FIND_TYPE_MESSAGE: &str = "Cannot find type name in any enclosing scope.";
const NOT_ASSIGNABLE_MESSAGE: &str = "Initializer type is not assignable to the annotated type.";

/// A lexical scope's identity within a [`SemanticModel`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScopeId(u32);

impl ScopeId {
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A bound name's identity within a [`SemanticModel`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SymbolId(u32);

impl SymbolId {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// An interned structural type's identity within a [`TypeTable`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TypeId(u32);

impl TypeId {
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The kind of lexical scope, used only to describe the model to callers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ScopeKind {
    Global,
    Module,
    Function,
    Block,
    For,
    Catch,
    Class,
}

/// One immutable lexical scope with its two-namespace symbol tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scope {
    kind: ScopeKind,
    parent: Option<ScopeId>,
    values: BTreeMap<String, SymbolId>,
    types: BTreeMap<String, SymbolId>,
}

impl Scope {
    #[must_use]
    pub const fn kind(&self) -> ScopeKind {
        self.kind
    }

    #[must_use]
    pub const fn parent(&self) -> Option<ScopeId> {
        self.parent
    }

    /// Returns the value binding declared directly in this scope, if any.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<SymbolId> {
        self.values.get(name).copied()
    }

    /// Returns the type binding declared directly in this scope, if any.
    #[must_use]
    pub fn type_binding(&self, name: &str) -> Option<SymbolId> {
        self.types.get(name).copied()
    }
}

/// What a bound name declares. This drives namespace membership and whether a
/// redeclaration is a legal merge or a duplicate error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SymbolKind {
    IntrinsicValue,
    IntrinsicType,
    Variable(VariableKind),
    Function,
    Parameter,
    Class,
    Interface,
    TypeAlias,
    Enum,
    TypeParameter,
    Import,
    Namespace,
}

impl SymbolKind {
    const fn occupies_value(self) -> bool {
        matches!(
            self,
            Self::IntrinsicValue
                | Self::Variable(_)
                | Self::Function
                | Self::Parameter
                | Self::Class
                | Self::Enum
                | Self::Import
                | Self::Namespace
        )
    }

    const fn occupies_type(self) -> bool {
        matches!(
            self,
            Self::IntrinsicType
                | Self::Class
                | Self::Enum
                | Self::Interface
                | Self::TypeAlias
                | Self::TypeParameter
                | Self::Import
        )
    }

    /// Two value bindings may share a name only when both are `var`/`function`.
    const fn value_mergeable(self) -> bool {
        matches!(self, Self::Variable(VariableKind::Var) | Self::Function)
    }

    /// Two type bindings may share a name only when both are interfaces.
    const fn type_mergeable(self) -> bool {
        matches!(self, Self::Interface)
    }
}

/// One immutable bound name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Symbol {
    name: String,
    kind: SymbolKind,
    scope: ScopeId,
    declaration: NodeId,
    range: TextRange,
}

impl Symbol {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn kind(&self) -> SymbolKind {
        self.kind
    }

    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    #[must_use]
    pub const fn declaration(&self) -> NodeId {
        self.declaration
    }

    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.range
    }
}

/// One member of an interned object type.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PropertyType {
    name: Box<str>,
    optional: bool,
    type_id: TypeId,
}

impl PropertyType {
    #[must_use]
    pub fn new(name: impl Into<Box<str>>, optional: bool, type_id: TypeId) -> Self {
        Self {
            name: name.into(),
            optional,
            type_id,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn optional(&self) -> bool {
        self.optional
    }

    #[must_use]
    pub const fn type_id(&self) -> TypeId {
        self.type_id
    }
}

/// One interned function signature.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FunctionSignature {
    parameters: Vec<TypeId>,
    return_type: TypeId,
}

impl FunctionSignature {
    #[must_use]
    pub fn parameters(&self) -> &[TypeId] {
        &self.parameters
    }

    #[must_use]
    pub const fn return_type(&self) -> TypeId {
        self.return_type
    }
}

/// The closed space of structural types the first checker slice models.
///
/// `Error` is a recovery type produced for unresolved or unsupported syntax; it
/// behaves like `Any` in assignability so one upstream mistake never cascades.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Type {
    Error,
    Any,
    Unknown,
    Never,
    Void,
    Null,
    Undefined,
    Boolean,
    Number,
    BigInt,
    String,
    Symbol,
    Object,
    BooleanLiteral(bool),
    NumberLiteral(Box<str>),
    StringLiteral(Box<str>),
    BigIntLiteral(Box<str>),
    Array(TypeId),
    Union(Vec<TypeId>),
    ObjectType(Vec<PropertyType>),
    Function(FunctionSignature),
    /// A nominal named type (type parameter, class, or enum) compared by identity.
    Named(SymbolId),
    /// A numeric enum value, distinct from both its runtime enum object and number.
    NumericEnum(SymbolId),
}

/// An interning table for structural types plus the assignability relation.
///
/// The table is append-only while checking and frozen into the immutable
/// [`SemanticModel`]. It is also a standalone reusable value: [`TypeTable::new`]
/// yields the primitive types so the algebra can be exercised directly.
#[derive(Clone, Debug)]
pub struct TypeTable {
    types: Vec<Type>,
    index: HashMap<Type, TypeId>,
    error: TypeId,
    any: TypeId,
    unknown: TypeId,
    never: TypeId,
    void: TypeId,
    null: TypeId,
    undefined: TypeId,
    boolean: TypeId,
    number: TypeId,
    bigint: TypeId,
    string: TypeId,
    symbol: TypeId,
    object: TypeId,
}

impl Default for TypeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeTable {
    /// Creates a table pre-populated with every primitive type.
    #[must_use]
    pub fn new() -> Self {
        let mut table = Self {
            types: Vec::new(),
            index: HashMap::new(),
            error: TypeId(0),
            any: TypeId(0),
            unknown: TypeId(0),
            never: TypeId(0),
            void: TypeId(0),
            null: TypeId(0),
            undefined: TypeId(0),
            boolean: TypeId(0),
            number: TypeId(0),
            bigint: TypeId(0),
            string: TypeId(0),
            symbol: TypeId(0),
            object: TypeId(0),
        };
        table.error = table.intern(Type::Error);
        table.any = table.intern(Type::Any);
        table.unknown = table.intern(Type::Unknown);
        table.never = table.intern(Type::Never);
        table.void = table.intern(Type::Void);
        table.null = table.intern(Type::Null);
        table.undefined = table.intern(Type::Undefined);
        table.boolean = table.intern(Type::Boolean);
        table.number = table.intern(Type::Number);
        table.bigint = table.intern(Type::BigInt);
        table.string = table.intern(Type::String);
        table.symbol = table.intern(Type::Symbol);
        table.object = table.intern(Type::Object);
        table
    }

    fn intern(&mut self, ty: Type) -> TypeId {
        if let Some(existing) = self.index.get(&ty) {
            return *existing;
        }
        let id = TypeId(u32::try_from(self.types.len()).expect("type count fits in u32"));
        self.types.push(ty.clone());
        self.index.insert(ty, id);
        id
    }

    /// Returns the interned representation of a type identity.
    #[must_use]
    pub fn get(&self, id: TypeId) -> &Type {
        &self.types[id.0 as usize]
    }

    #[must_use]
    pub const fn error_type(&self) -> TypeId {
        self.error
    }
    #[must_use]
    pub const fn any(&self) -> TypeId {
        self.any
    }
    #[must_use]
    pub const fn unknown(&self) -> TypeId {
        self.unknown
    }
    #[must_use]
    pub const fn never(&self) -> TypeId {
        self.never
    }
    #[must_use]
    pub const fn void(&self) -> TypeId {
        self.void
    }
    #[must_use]
    pub const fn null_type(&self) -> TypeId {
        self.null
    }
    #[must_use]
    pub const fn undefined_type(&self) -> TypeId {
        self.undefined
    }
    #[must_use]
    pub const fn boolean(&self) -> TypeId {
        self.boolean
    }
    #[must_use]
    pub const fn number(&self) -> TypeId {
        self.number
    }
    #[must_use]
    pub const fn bigint(&self) -> TypeId {
        self.bigint
    }
    #[must_use]
    pub const fn string(&self) -> TypeId {
        self.string
    }
    #[must_use]
    pub const fn symbol_type(&self) -> TypeId {
        self.symbol
    }
    #[must_use]
    pub const fn object(&self) -> TypeId {
        self.object
    }

    /// Interns a boolean literal type.
    pub fn boolean_literal(&mut self, value: bool) -> TypeId {
        self.intern(Type::BooleanLiteral(value))
    }

    /// Interns a numeric literal type keyed by its source lexeme.
    pub fn number_literal(&mut self, text: &str) -> TypeId {
        self.intern(Type::NumberLiteral(text.into()))
    }

    /// Interns a string literal type keyed by its source lexeme.
    pub fn string_literal(&mut self, text: &str) -> TypeId {
        self.intern(Type::StringLiteral(text.into()))
    }

    /// Interns a bigint literal type keyed by its source lexeme.
    pub fn bigint_literal(&mut self, text: &str) -> TypeId {
        self.intern(Type::BigIntLiteral(text.into()))
    }

    /// Interns a nominal named type.
    pub fn named(&mut self, symbol: SymbolId) -> TypeId {
        self.intern(Type::Named(symbol))
    }

    /// Interns a numeric enum value type.
    pub fn numeric_enum(&mut self, symbol: SymbolId) -> TypeId {
        self.intern(Type::NumericEnum(symbol))
    }

    /// Interns an array type over `element`.
    pub fn array(&mut self, element: TypeId) -> TypeId {
        self.intern(Type::Array(element))
    }

    /// Interns an object type after canonically ordering its members by name.
    pub fn object_type(&mut self, mut properties: Vec<PropertyType>) -> TypeId {
        properties.sort_by(|left, right| left.name.cmp(&right.name));
        properties.dedup_by(|left, right| left.name == right.name);
        self.intern(Type::ObjectType(properties))
    }

    /// Interns a function type.
    pub fn function(&mut self, parameters: Vec<TypeId>, return_type: TypeId) -> TypeId {
        self.intern(Type::Function(FunctionSignature {
            parameters,
            return_type,
        }))
    }

    /// Interns a union, normalizing absorption, `never` removal, and duplicates.
    pub fn union(&mut self, members: &[TypeId]) -> TypeId {
        let mut flat = Vec::new();
        for member in members {
            match self.get(*member) {
                Type::Any => return self.any,
                Type::Unknown => return self.unknown,
                Type::Never => {}
                Type::Union(nested) => flat.extend(nested.iter().copied()),
                _ => flat.push(*member),
            }
        }
        flat.sort_by_key(|id| id.get());
        flat.dedup();
        match flat.len() {
            0 => self.never,
            1 => flat[0],
            _ => self.intern(Type::Union(flat)),
        }
    }

    /// Returns whether a value of `source` may be assigned where `target` is
    /// expected, using structural rules over the modeled type space.
    #[must_use]
    pub fn assignable(&self, source: TypeId, target: TypeId) -> bool {
        if source == target {
            return true;
        }
        let (from, to) = (self.get(source), self.get(target));
        match (from, to) {
            (Type::Error, _) | (_, Type::Error) => true,
            // `any` is the deliberate escape hatch in both directions.
            (Type::Any, _) | (_, Type::Any) => true,
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
            (Type::NumericEnum(_), Type::Number) | (Type::Number, Type::NumericEnum(_)) => true,
            (Type::Union(sources), _) => sources.iter().all(|s| self.assignable(*s, target)),
            (_, Type::Union(targets)) => targets.iter().any(|t| self.assignable(source, *t)),
            (Type::Array(source_element), Type::Array(target_element)) => {
                self.assignable(*source_element, *target_element)
            }
            (Type::ObjectType(source_props), Type::ObjectType(target_props)) => {
                self.object_assignable(source_props, target_props)
            }
            (Type::Function(source_sig), Type::Function(target_sig)) => {
                self.function_assignable(source_sig, target_sig)
            }
            _ => false,
        }
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
        if let (Type::Function(from), Type::Function(to)) = (self.get(source), self.get(target)) {
            if from.parameters.len() < to.parameters.len() {
                hazards.push(RelationHazard::FewerCallbackParameters);
            }
            if matches!(self.get(to.return_type), Type::Void)
                && !matches!(self.get(from.return_type), Type::Void | Type::Never)
            {
                hazards.push(RelationHazard::ValueReturnedToVoid);
            }
        }
        if matches!(
            (self.get(source), self.get(target)),
            (Type::NumericEnum(_), Type::Number) | (Type::Number, Type::NumericEnum(_))
        ) {
            hazards.push(RelationHazard::NumericEnumNumber);
        }
        if let (Type::ObjectType(from), Type::ObjectType(to)) = (self.get(source), self.get(target))
        {
            for target_property in to.iter().filter(|property| property.optional) {
                let Some(source_property) = from
                    .iter()
                    .find(|property| property.name == target_property.name)
                else {
                    continue;
                };
                if matches!(self.get(source_property.type_id), Type::Undefined)
                    && !self.contains_undefined(target_property.type_id)
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

    fn contains_undefined(&self, type_id: TypeId) -> bool {
        match self.get(type_id) {
            Type::Any | Type::Unknown | Type::Undefined => true,
            Type::Union(members) => members
                .iter()
                .any(|member| self.contains_undefined(*member)),
            _ => false,
        }
    }

    fn object_assignable(&self, source: &[PropertyType], target: &[PropertyType]) -> bool {
        // Excess source properties are allowed; each target property must be
        // satisfied. Members are name-sorted, so a merge walk suffices.
        target.iter().all(
            |want| match source.iter().find(|have| have.name == want.name) {
                Some(have) => {
                    self.assignable(have.type_id, want.type_id)
                        || (want.optional && matches!(self.get(have.type_id), Type::Undefined))
                }
                None => want.optional,
            },
        )
    }

    fn function_assignable(&self, source: &FunctionSignature, target: &FunctionSignature) -> bool {
        if source.parameters.len() > target.parameters.len() {
            return false;
        }
        for (source_param, target_param) in source.parameters.iter().zip(&target.parameters) {
            // Parameters are contravariant: the target must supply a value the
            // source accepts.
            if !self.assignable(*target_param, *source_param) {
                return false;
            }
        }
        matches!(self.get(target.return_type), Type::Void)
            || self.assignable(source.return_type, target.return_type)
    }
}

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

/// Compact identity for one allocated object literal and its aliases.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId(u32);

/// Source-qualified syntax identity used by cross-file facts.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeKey {
    pub source_id: SourceId,
    pub node_id: NodeId,
}

/// One checker-derived condition consumed by a semantic rule.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SemanticHazard {
    UncheckedIndexRead,
    ExplicitUndefinedOptional,
    DetachedMethod,
    DivergentAccessor,
    ReadonlyAliasMutation,
    FewerCallbackParameters,
    ValueReturnedToVoid,
    OpenObjectKeys,
    IndexSignatureDotAccess,
    ImplicitAny,
    UncheckedAssertion,
    DeclarationInferenceDependency,
    TypeImportedAsValue,
    TypeReexportedAsValue,
    UncheckedSideEffectImport,
    InteropDependentDefaultImport,
    CjsEsmNamedExportMismatch,
    VirtualCallInConstructor,
    InitializedFieldShadowsAccessor,
    ImplicitOverride,
    NumericEnumNumber,
    NumericEnumReverseLookup,
    NonExhaustiveSwitch,
    InvalidNumberFormatting,
    NumericKeyOrder,
    JsonStringifyUnserializable,
    UncheckedJsonParse,
    NumericDefaultSort,
    LooseEqualityCoercion,
    ObjectToPrimitive,
    SymbolInterpolation,
    UnsafeToStringTag,
    UninitializedFieldShadowsAccessor,
}

/// Immutable evidence for one semantic lint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HazardFact {
    pub hazard: SemanticHazard,
    pub range: TextRange,
    pub note: Option<Box<str>>,
}

/// Frozen checker facts. Rule implementations only query this product.
#[derive(Clone, Debug, Default)]
pub struct AnalysisFacts {
    hazards: Vec<HazardFact>,
    index: HashSet<(SemanticHazard, TextRange)>,
}

impl AnalysisFacts {
    #[must_use]
    pub fn hazards(&self) -> &[HazardFact] {
        &self.hazards
    }

    pub(crate) fn push(&mut self, fact: HazardFact) {
        if self.index.insert((fact.hazard, fact.range)) {
            self.hazards.push(fact);
        }
    }

    pub(crate) fn extend(&mut self, facts: impl IntoIterator<Item = HazardFact>) {
        for fact in facts {
            self.push(fact);
        }
    }
}

/// The immutable product of semantic analysis.
#[derive(Clone, Debug)]
pub struct SemanticModel {
    scopes: Vec<Scope>,
    symbols: Vec<Symbol>,
    symbol_types: Vec<TypeId>,
    references: HashMap<NodeId, SymbolId>,
    types: TypeTable,
    module_scope: ScopeId,
    facts: AnalysisFacts,
}

impl SemanticModel {
    /// Returns every lexical scope, with the module scope first.
    #[must_use]
    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    /// Returns a scope by identity.
    #[must_use]
    pub fn scope(&self, id: ScopeId) -> &Scope {
        &self.scopes[id.0 as usize]
    }

    /// Returns the top-level module scope.
    #[must_use]
    pub const fn module_scope(&self) -> ScopeId {
        self.module_scope
    }

    /// Returns every bound name.
    #[must_use]
    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    /// Returns a symbol by identity.
    #[must_use]
    pub fn symbol(&self, id: SymbolId) -> &Symbol {
        &self.symbols[id.0 as usize]
    }

    /// Returns the declared or inferred type of a bound name.
    #[must_use]
    pub fn symbol_type(&self, id: SymbolId) -> TypeId {
        self.symbol_types[id.0 as usize]
    }

    /// Returns the interned type table.
    #[must_use]
    pub const fn types(&self) -> &TypeTable {
        &self.types
    }

    /// Returns the immutable semantic evidence consumed by lint rules.
    #[must_use]
    pub const fn facts(&self) -> &AnalysisFacts {
        &self.facts
    }

    pub(crate) const fn facts_mut(&mut self) -> &mut AnalysisFacts {
        &mut self.facts
    }

    pub(crate) fn replace_facts(&mut self, facts: AnalysisFacts) {
        self.facts = facts;
    }

    /// Returns the symbol an identifier reference resolved to, if any.
    #[must_use]
    pub fn reference(&self, node: NodeId) -> Option<SymbolId> {
        self.references.get(&node).copied()
    }

    /// Returns how many identifier references resolved to a local binding.
    #[must_use]
    pub fn resolved_reference_count(&self) -> usize {
        self.references.len()
    }

    /// Resolves a value name from `scope` outward through its ancestors.
    #[must_use]
    pub fn lookup_value(&self, scope: ScopeId, name: &str) -> Option<SymbolId> {
        let mut current = Some(scope);
        while let Some(id) = current {
            let scope = &self.scopes[id.0 as usize];
            if let Some(symbol) = scope.values.get(name) {
                return Some(*symbol);
            }
            current = scope.parent;
        }
        None
    }

    /// Resolves a type name from `scope` outward through its ancestors.
    #[must_use]
    pub fn lookup_type(&self, scope: ScopeId, name: &str) -> Option<SymbolId> {
        let mut current = Some(scope);
        while let Some(id) = current {
            let scope = &self.scopes[id.0 as usize];
            if let Some(symbol) = scope.types.get(name) {
                return Some(*symbol);
            }
            current = scope.parent;
        }
        None
    }
}

/// Analyzes one source with the default lint profile.
#[must_use]
pub fn check(source_file: &Recovered<SourceFile>) -> Recovered<SemanticModel> {
    check_with_lints(source_file, &LintTable::new(LintProfile::Default))
}

/// Analyzes one source using an already-resolved lint table.
#[must_use]
pub fn check_with_lints(
    source_file: &Recovered<SourceFile>,
    levels: &LintTable,
) -> Recovered<SemanticModel> {
    let source = source_file.product();
    let (mut model, mut diagnostics) = check_core(source);
    model.replace_facts(crate::rules::semantic::collect_facts(source, &model));
    diagnostics.extend(analyze_warnings(source_file, levels));
    diagnostics.extend(crate::rules::analyze_semantic(source, &model, None, levels));
    Recovered::new(model, diagnostics)
}

/// One resolved module edge supplied by the project loader.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResolvedModuleEdge {
    pub from: SourceId,
    pub specifier: NodeId,
    pub to: SourceId,
}

/// Borrowed input for a linked multi-file checker run.
#[derive(Clone, Copy)]
pub struct ProgramCheckInput<'a> {
    pub files: &'a [Recovered<SourceFile>],
    pub edges: &'a [ResolvedModuleEdge],
}

/// Checker environment selected by the project's module compiler option.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct ProgramCheckOptions {
    commonjs: bool,
}

impl ProgramCheckOptions {
    #[must_use]
    pub const fn standard() -> Self {
        Self { commonjs: false }
    }

    #[must_use]
    pub const fn commonjs() -> Self {
        Self { commonjs: true }
    }

    const fn environment(self) -> GlobalEnvironment {
        if self.commonjs {
            GlobalEnvironment::commonjs()
        } else {
            GlobalEnvironment::standard()
        }
    }
}

/// Immutable linked checker product.
#[derive(Clone, Debug)]
pub struct ProgramSemanticModel {
    files: BTreeMap<SourceId, SemanticModel>,
    edges: Box<[ResolvedModuleEdge]>,
}

impl ProgramSemanticModel {
    #[must_use]
    pub fn file(&self, source_id: SourceId) -> Option<&SemanticModel> {
        self.files.get(&source_id)
    }

    #[must_use]
    pub fn edges(&self) -> &[ResolvedModuleEdge] {
        &self.edges
    }
}

/// Checks a set of loaded files after module resolution.
#[must_use]
pub fn check_program(
    input: ProgramCheckInput<'_>,
    levels: &LintTable,
) -> Recovered<ProgramSemanticModel> {
    check_program_with_options(input, levels, ProgramCheckOptions::standard())
}

/// Checks a set of loaded files using the environment selected by module options.
#[must_use]
pub fn check_program_with_options(
    input: ProgramCheckInput<'_>,
    levels: &LintTable,
    options: ProgramCheckOptions,
) -> Recovered<ProgramSemanticModel> {
    let mut files = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for recovered in input.files {
        let source = recovered.product();
        let (mut model, core_diagnostics) =
            check_core_with_environment(source, options.environment());
        model.replace_facts(crate::rules::semantic::collect_facts(source, &model));
        diagnostics.extend(core_diagnostics);
        diagnostics.extend(analyze_warnings(recovered, levels));
        files.insert(source.source_id(), model);
    }
    crate::rules::semantic::collect_program_facts(input.files, input.edges, &mut files);
    let program = ProgramSemanticModel {
        files,
        edges: input.edges.into(),
    };
    for recovered in input.files {
        let source = recovered.product();
        let model = program
            .file(source.source_id())
            .expect("program model contains every input source");
        diagnostics.extend(crate::rules::analyze_semantic(
            source,
            model,
            Some(&program),
            levels,
        ));
    }
    Recovered::new(program, diagnostics)
}

fn check_core(source: &SourceFile) -> (SemanticModel, Vec<Diagnostic>) {
    let mut checker = Checker::new(source);
    checker.run();
    checker.finish()
}

fn check_core_with_environment(
    source: &SourceFile,
    environment: GlobalEnvironment,
) -> (SemanticModel, Vec<Diagnostic>) {
    let mut checker = Checker::with_environment(source, environment);
    checker.run();
    checker.finish()
}

/// Lazy resolution state for a type-declaring symbol.
#[derive(Clone, Copy)]
enum TypeState {
    Unresolved,
    InProgress,
    Done(TypeId),
}

/// A named type definition kept by reference for lazy, memoized resolution.
#[derive(Clone, Copy)]
enum TypeDef<'src> {
    Alias {
        scope: ScopeId,
        type_parameters: Option<&'src crate::syntax::TypeParameterList>,
        node: &'src Ty,
    },
    Interface {
        scope: ScopeId,
        type_parameters: Option<&'src crate::syntax::TypeParameterList>,
        extends: &'src [TypeReference],
        members: &'src [crate::syntax::TypeMemberNode],
    },
    Enum {
        numeric: bool,
    },
}

struct Checker<'src> {
    source: &'src SourceFile,
    intrinsics: GlobalEnvironment,
    scopes: Vec<Scope>,
    symbols: Vec<Symbol>,
    symbol_types: Vec<TypeId>,
    type_state: Vec<TypeState>,
    type_defs: HashMap<SymbolId, TypeDef<'src>>,
    references: HashMap<NodeId, SymbolId>,
    diagnostics: Vec<Diagnostic>,
    types: TypeTable,
    module_scope: ScopeId,
}

impl<'src> Checker<'src> {
    fn new(source: &'src SourceFile) -> Self {
        Self::with_environment(source, GlobalEnvironment::standard())
    }

    fn with_environment(source: &'src SourceFile, intrinsics: GlobalEnvironment) -> Self {
        let mut checker = Self {
            source,
            intrinsics,
            scopes: Vec::new(),
            symbols: Vec::new(),
            symbol_types: Vec::new(),
            type_state: Vec::new(),
            type_defs: HashMap::new(),
            references: HashMap::new(),
            diagnostics: Vec::new(),
            types: TypeTable::new(),
            module_scope: ScopeId(0),
        };
        let global_scope = checker.new_scope(ScopeKind::Global, None);
        checker.module_scope = checker.new_scope(ScopeKind::Module, Some(global_scope));
        checker.bind_intrinsic_environment(global_scope);
        checker
    }

    fn bind_intrinsic_environment(&mut self, scope: ScopeId) {
        for name in self
            .intrinsics
            .values()
            .iter()
            .chain(self.intrinsics.module_values())
        {
            self.declare(
                name,
                SymbolKind::IntrinsicValue,
                scope,
                NodeId::default(),
                NodeId::default_range(),
            );
        }
        for name in self.intrinsics.types() {
            self.declare(
                name,
                SymbolKind::IntrinsicType,
                scope,
                NodeId::default(),
                NodeId::default_range(),
            );
        }
    }

    fn run(&mut self) {
        let statements = self.source.statements();
        let scope = self.module_scope;
        self.bind_statements(statements, scope);
        self.bind_hoisted_statements(statements, scope);
        self.resolve_statements(statements, scope);
    }

    fn finish(self) -> (SemanticModel, Vec<Diagnostic>) {
        let model = SemanticModel {
            scopes: self.scopes,
            symbols: self.symbols,
            symbol_types: self.symbol_types,
            references: self.references,
            types: self.types,
            module_scope: self.module_scope,
            facts: AnalysisFacts::default(),
        };
        (model, self.diagnostics)
    }

    // -- text and scope helpers ------------------------------------------------

    fn text(&self, token: &Token) -> &'src str {
        self.source.token_text(token).unwrap_or("")
    }

    fn identifier_text(&self, identifier: &IdentifierNode) -> &'src str {
        self.text(identifier.data().token())
    }

    fn new_scope(&mut self, kind: ScopeKind, parent: Option<ScopeId>) -> ScopeId {
        let id = ScopeId(u32::try_from(self.scopes.len()).expect("scope count fits in u32"));
        self.scopes.push(Scope {
            kind,
            parent,
            values: BTreeMap::new(),
            types: BTreeMap::new(),
        });
        id
    }

    /// Walks outward from `scope` to the nearest Function or Module scope, the
    /// declaration target for JS-hoisted `var` and function bindings. Stopping
    /// at the first such scope keeps inner-function `var`s from escaping into an
    /// outer function; the Module scope has no parent and terminates the walk.
    fn value_hoist_scope(&self, scope: ScopeId) -> ScopeId {
        let mut current = scope;
        loop {
            let node = &self.scopes[current.0 as usize];
            if matches!(node.kind, ScopeKind::Function | ScopeKind::Module) {
                return current;
            }
            match node.parent {
                Some(parent) => current = parent,
                None => return current,
            }
        }
    }

    fn emit(&mut self, code: DiagnosticCode, range: TextRange, message: &'static str) {
        self.diagnostics.push(Diagnostic::error(
            code,
            self.source.source_id(),
            range,
            message,
        ));
    }

    // -- declaration binding ---------------------------------------------------

    fn declare(
        &mut self,
        name: &str,
        kind: SymbolKind,
        scope: ScopeId,
        declaration: NodeId,
        range: TextRange,
    ) -> SymbolId {
        // `var` and function declarations are hoisted to the nearest Function or
        // Module scope, so a binding textually nested in a block, `for`, or
        // `catch` scope is owned by its enclosing function. `let`/`const` and all
        // other kinds stay in the scope they were written in.
        let scope = if matches!(
            kind,
            SymbolKind::Variable(VariableKind::Var) | SymbolKind::Function
        ) {
            self.value_hoist_scope(scope)
        } else {
            scope
        };
        if kind.occupies_value()
            && let Some(existing) = self.scopes[scope.0 as usize].values.get(name)
            && kind.value_mergeable()
            && self.symbols[existing.get() as usize].kind.value_mergeable()
        {
            return *existing;
        }
        let id = SymbolId(u32::try_from(self.symbols.len()).expect("symbol count fits in u32"));
        self.symbols.push(Symbol {
            name: name.to_owned(),
            kind,
            scope,
            declaration,
            range,
        });
        self.symbol_types.push(self.types.any());
        self.type_state.push(TypeState::Unresolved);

        let mut conflict = false;
        if kind.occupies_value() {
            conflict |= self.insert_value(scope, name, id, kind);
        }
        if kind.occupies_type() {
            conflict |= self.insert_type(scope, name, id, kind);
        }
        if conflict {
            self.emit(DUPLICATE_DECLARATION, range, DUPLICATE_MESSAGE);
        }
        id
    }

    fn insert_value(&mut self, scope: ScopeId, name: &str, id: SymbolId, kind: SymbolKind) -> bool {
        match self.scopes[scope.0 as usize].values.get(name) {
            None => {
                self.scopes[scope.0 as usize]
                    .values
                    .insert(name.to_owned(), id);
                false
            }
            Some(existing) => {
                let existing_kind = self.symbols[existing.get() as usize].kind;
                !(kind.value_mergeable() && existing_kind.value_mergeable())
            }
        }
    }

    fn insert_type(&mut self, scope: ScopeId, name: &str, id: SymbolId, kind: SymbolKind) -> bool {
        match self.scopes[scope.0 as usize].types.get(name) {
            None => {
                self.scopes[scope.0 as usize]
                    .types
                    .insert(name.to_owned(), id);
                false
            }
            Some(existing) => {
                let existing_kind = self.symbols[existing.get() as usize].kind;
                !(kind.type_mergeable() && existing_kind.type_mergeable())
            }
        }
    }

    fn bind_statements(&mut self, statements: &'src [crate::syntax::Stmt], scope: ScopeId) {
        for statement in statements {
            self.bind_statement(statement, scope);
        }
    }

    /// Pre-binds `var` and function names that occur beneath lexical child
    /// scopes. The traversal never enters a function body: its own call to this
    /// pass supplies the correct function hoist target.
    fn bind_hoisted_statements(&mut self, statements: &'src [crate::syntax::Stmt], scope: ScopeId) {
        for statement in statements {
            self.bind_hoisted_statement(statement, scope);
        }
    }

    fn bind_hoisted_statement(&mut self, statement: &'src crate::syntax::Stmt, scope: ScopeId) {
        match statement.data() {
            Statement::Variable(variable) if variable.kind == VariableKind::Var => {
                self.bind_variable(variable, scope, statement.id());
            }
            Statement::Function(function) => {
                if let Some(name) = &function.function.name {
                    self.declare(
                        self.identifier_text(name),
                        SymbolKind::Function,
                        scope,
                        statement.id(),
                        name.range(),
                    );
                }
            }
            Statement::Block(block) => {
                self.bind_hoisted_statements(&block.data().statements, scope)
            }
            Statement::If(statement) => {
                self.bind_hoisted_statement(&statement.consequent, scope);
                if let Some(alternate) = &statement.alternate {
                    self.bind_hoisted_statement(alternate, scope);
                }
            }
            Statement::Switch(statement) => {
                for case in &statement.cases {
                    self.bind_hoisted_statements(&case.data().consequent, scope);
                }
            }
            Statement::For(for_statement) => {
                if let Some(ForInitializer::Variable(variable)) = &for_statement.initializer
                    && variable.kind == VariableKind::Var
                {
                    self.bind_variable(variable, scope, NodeId::default());
                }
                self.bind_hoisted_statement(&for_statement.body, scope);
            }
            Statement::ForIn(for_statement) => {
                if let ForBinding::Variable(variable) = &for_statement.binding
                    && variable.kind == VariableKind::Var
                {
                    self.bind_variable(variable, scope, NodeId::default());
                }
                self.bind_hoisted_statement(&for_statement.body, scope);
            }
            Statement::ForOf(for_statement) => {
                if let ForBinding::Variable(variable) = &for_statement.binding
                    && variable.kind == VariableKind::Var
                {
                    self.bind_variable(variable, scope, NodeId::default());
                }
                self.bind_hoisted_statement(&for_statement.body, scope);
            }
            Statement::While(statement) => self.bind_hoisted_statement(&statement.body, scope),
            Statement::DoWhile(statement) => self.bind_hoisted_statement(&statement.body, scope),
            Statement::Try(statement) => {
                self.bind_hoisted_statements(&statement.block.data().statements, scope);
                if let Some(handler) = &statement.handler {
                    self.bind_hoisted_statements(&handler.data().body.data().statements, scope);
                }
                if let Some(finalizer) = &statement.finalizer {
                    self.bind_hoisted_statements(&finalizer.data().statements, scope);
                }
            }
            Statement::With(statement) => self.bind_hoisted_statement(&statement.body, scope),
            Statement::Labeled(statement) => self.bind_hoisted_statement(&statement.body, scope),
            Statement::Namespace(namespace) => {
                self.bind_hoisted_statements(&namespace.body.data().statements, scope);
            }
            Statement::Declare(inner) => self.bind_hoisted_statement(inner, scope),
            Statement::Export(crate::syntax::ExportDeclaration::Named(
                crate::syntax::ExportNamedDeclaration::Declaration(inner),
            )) => self.bind_hoisted_statement(inner, scope),
            _ => {}
        }
    }

    fn bind_statement(&mut self, statement: &'src crate::syntax::Stmt, scope: ScopeId) {
        let declaration = statement.id();
        match statement.data() {
            Statement::Variable(variable) => self.bind_variable(variable, scope, declaration),
            Statement::Function(function) => {
                if let Some(name) = &function.function.name {
                    self.declare(
                        self.identifier_text(name),
                        SymbolKind::Function,
                        scope,
                        declaration,
                        name.range(),
                    );
                }
            }
            Statement::Class(class) => {
                if let Some(name) = &class.name {
                    self.declare(
                        self.identifier_text(name),
                        SymbolKind::Class,
                        scope,
                        declaration,
                        name.range(),
                    );
                }
            }
            Statement::Interface(interface) => self.bind_interface(interface, scope, declaration),
            Statement::TypeAlias(alias) => self.bind_type_alias(alias, scope, declaration),
            Statement::Enum(declaration_node) => {
                let id = self.declare(
                    self.identifier_text(&declaration_node.name),
                    SymbolKind::Enum,
                    scope,
                    declaration,
                    declaration_node.name.range(),
                );
                self.type_defs.insert(
                    id,
                    TypeDef::Enum {
                        numeric: declaration_node.members.iter().all(|member| {
                            member
                                .data()
                                .initializer
                                .as_deref()
                                .is_none_or(Self::is_numeric_enum_initializer)
                        }),
                    },
                );
            }
            Statement::Namespace(namespace) => {
                self.declare(
                    self.identifier_text(&namespace.name),
                    SymbolKind::Namespace,
                    scope,
                    declaration,
                    namespace.name.range(),
                );
            }
            Statement::Import(import) => self.bind_import(import, scope, declaration),
            Statement::ImportEquals(import) => {
                self.declare(
                    self.identifier_text(&import.local),
                    SymbolKind::Import,
                    scope,
                    declaration,
                    import.local.range(),
                );
            }
            Statement::Declare(inner) => self.bind_statement(inner, scope),
            Statement::Export(crate::syntax::ExportDeclaration::Named(
                crate::syntax::ExportNamedDeclaration::Declaration(inner),
            )) => {
                self.bind_statement(inner, scope);
            }
            _ => {}
        }
    }

    fn bind_variable(
        &mut self,
        variable: &'src VariableDeclaration,
        scope: ScopeId,
        declaration: NodeId,
    ) {
        for declarator in &variable.declarations {
            self.bind_pattern(
                &declarator.data().binding,
                variable.kind,
                scope,
                declaration,
            );
        }
    }

    fn bind_pattern(
        &mut self,
        pattern: &'src crate::syntax::Pattern,
        kind: VariableKind,
        scope: ScopeId,
        declaration: NodeId,
    ) {
        match pattern.data() {
            BindingPattern::Identifier(name) => {
                self.declare(
                    self.identifier_text(name),
                    SymbolKind::Variable(kind),
                    scope,
                    declaration,
                    name.range(),
                );
            }
            BindingPattern::Object(object) => {
                for property in &object.properties {
                    self.bind_pattern(&property.binding, kind, scope, declaration);
                }
            }
            BindingPattern::Array(array) => {
                for element in &array.elements {
                    if let crate::syntax::ArrayBindingElement::Binding(inner) = element {
                        self.bind_pattern(inner, kind, scope, declaration);
                    }
                }
            }
            BindingPattern::Rest(rest) => {
                self.bind_pattern(&rest.argument, kind, scope, declaration);
            }
            BindingPattern::Assignment(assignment) => {
                self.bind_pattern(&assignment.left, kind, scope, declaration);
            }
            BindingPattern::Missing(_) => {}
        }
    }

    fn bind_interface(
        &mut self,
        interface: &'src InterfaceDeclaration,
        scope: ScopeId,
        declaration: NodeId,
    ) {
        let id = self.declare(
            self.identifier_text(&interface.name),
            SymbolKind::Interface,
            scope,
            declaration,
            interface.name.range(),
        );
        let type_scope = self.new_scope(ScopeKind::Block, Some(scope));
        self.bind_type_parameter_names(interface.type_parameters.as_ref(), type_scope);
        // Only the first interface of a mergeable set owns the definition slot;
        // later merges keep their symbol but reuse the representative's shape.
        self.type_defs.entry(id).or_insert(TypeDef::Interface {
            scope: type_scope,
            type_parameters: interface.type_parameters.as_ref(),
            extends: &interface.extends,
            members: &interface.members,
        });
    }

    fn bind_type_alias(
        &mut self,
        alias: &'src TypeAliasDeclaration,
        scope: ScopeId,
        declaration: NodeId,
    ) {
        let id = self.declare(
            self.identifier_text(&alias.name),
            SymbolKind::TypeAlias,
            scope,
            declaration,
            alias.name.range(),
        );
        let type_scope = self.new_scope(ScopeKind::Block, Some(scope));
        self.bind_type_parameter_names(alias.type_parameters.as_ref(), type_scope);
        self.type_defs.insert(
            id,
            TypeDef::Alias {
                scope: type_scope,
                type_parameters: alias.type_parameters.as_ref(),
                node: &alias.type_node,
            },
        );
    }

    fn bind_import(
        &mut self,
        import: &'src crate::syntax::ImportDeclaration,
        scope: ScopeId,
        declaration: NodeId,
    ) {
        let Some(clause) = &import.clause else {
            return;
        };
        if let Some(default) = &clause.default {
            self.declare(
                self.identifier_text(default),
                SymbolKind::Import,
                scope,
                declaration,
                default.range(),
            );
        }
        match &clause.binding {
            Some(ImportBinding::Namespace(name)) => {
                self.declare(
                    self.identifier_text(name),
                    SymbolKind::Import,
                    scope,
                    declaration,
                    name.range(),
                );
            }
            Some(ImportBinding::Named(specifiers)) => {
                for specifier in specifiers {
                    let local = &specifier.data().local;
                    self.declare(
                        self.identifier_text(local),
                        SymbolKind::Import,
                        scope,
                        declaration,
                        local.range(),
                    );
                }
            }
            None => {}
        }
    }

    // -- reference resolution and assignability --------------------------------

    fn resolve_statements(&mut self, statements: &'src [crate::syntax::Stmt], scope: ScopeId) {
        for statement in statements {
            self.resolve_statement(statement, scope);
        }
    }

    fn resolve_statement(&mut self, statement: &'src crate::syntax::Stmt, scope: ScopeId) {
        match statement.data() {
            Statement::Variable(variable) => self.resolve_variable(variable, scope),
            Statement::Function(function) => self.resolve_function(&function.function, scope),
            Statement::Class(class) => self.resolve_class(class, scope),
            Statement::Interface(interface) => {
                if let Some(id) = self.scopes[scope.0 as usize]
                    .types
                    .get(self.identifier_text(&interface.name))
                    .copied()
                {
                    let _ = self.resolve_type_symbol(id);
                }
            }
            Statement::TypeAlias(alias) => {
                if let Some(id) = self.scopes[scope.0 as usize]
                    .types
                    .get(self.identifier_text(&alias.name))
                    .copied()
                {
                    let _ = self.resolve_type_symbol(id);
                }
            }
            Statement::Block(block) => {
                let child = self.new_scope(ScopeKind::Block, Some(scope));
                self.bind_statements(&block.data().statements, child);
                self.resolve_statements(&block.data().statements, child);
            }
            Statement::Expression(statement) => self.resolve_expr(&statement.expression, scope),
            Statement::If(statement) => {
                self.resolve_expr(&statement.test, scope);
                self.resolve_statement(&statement.consequent, scope);
                if let Some(alternate) = &statement.alternate {
                    self.resolve_statement(alternate, scope);
                }
            }
            Statement::Switch(statement) => {
                self.resolve_expr(&statement.discriminant, scope);
                let child = self.new_scope(ScopeKind::Block, Some(scope));
                for case in &statement.cases {
                    if let Some(test) = &case.data().test {
                        self.resolve_expr(test, child);
                    }
                    self.bind_statements(&case.data().consequent, child);
                }
                for case in &statement.cases {
                    self.resolve_statements(&case.data().consequent, child);
                }
            }
            Statement::For(for_statement) => {
                let child = self.new_scope(ScopeKind::For, Some(scope));
                if let Some(initializer) = &for_statement.initializer {
                    self.resolve_for_initializer(initializer, child);
                }
                if let Some(test) = &for_statement.test {
                    self.resolve_expr(test, child);
                }
                if let Some(update) = &for_statement.update {
                    self.resolve_expr(update, child);
                }
                self.resolve_statement(&for_statement.body, child);
            }
            Statement::ForIn(for_statement) => {
                let child = self.new_scope(ScopeKind::For, Some(scope));
                self.resolve_for_binding(&for_statement.binding, child);
                self.resolve_expr(&for_statement.object, child);
                self.resolve_statement(&for_statement.body, child);
            }
            Statement::ForOf(for_statement) => {
                let child = self.new_scope(ScopeKind::For, Some(scope));
                self.resolve_for_binding(&for_statement.binding, child);
                self.resolve_expr(&for_statement.iterable, child);
                self.resolve_statement(&for_statement.body, child);
            }
            Statement::While(statement) => {
                self.resolve_expr(&statement.test, scope);
                self.resolve_statement(&statement.body, scope);
            }
            Statement::DoWhile(statement) => {
                self.resolve_statement(&statement.body, scope);
                self.resolve_expr(&statement.test, scope);
            }
            Statement::Try(statement) => {
                let block = &statement.block;
                let try_scope = self.new_scope(ScopeKind::Block, Some(scope));
                self.bind_statements(&block.data().statements, try_scope);
                self.resolve_statements(&block.data().statements, try_scope);
                if let Some(handler) = &statement.handler {
                    let catch_scope = self.new_scope(ScopeKind::Catch, Some(scope));
                    if let Some(binding) = &handler.data().binding {
                        self.bind_pattern(binding, VariableKind::Let, catch_scope, handler.id());
                    }
                    let body = &handler.data().body;
                    self.bind_statements(&body.data().statements, catch_scope);
                    self.resolve_statements(&body.data().statements, catch_scope);
                }
                if let Some(finalizer) = &statement.finalizer {
                    let finally_scope = self.new_scope(ScopeKind::Block, Some(scope));
                    self.bind_statements(&finalizer.data().statements, finally_scope);
                    self.resolve_statements(&finalizer.data().statements, finally_scope);
                }
            }
            Statement::With(statement) => {
                self.resolve_expr(&statement.object, scope);
                self.resolve_statement(&statement.body, scope);
            }
            Statement::Labeled(statement) => self.resolve_statement(&statement.body, scope),
            Statement::Return(statement) => {
                if let Some(argument) = &statement.argument {
                    self.resolve_expr(argument, scope);
                }
            }
            Statement::Throw(statement) => self.resolve_expr(&statement.argument, scope),
            Statement::Enum(declaration) => {
                for member in &declaration.members {
                    if let Some(initializer) = &member.data().initializer {
                        self.resolve_expr(initializer, scope);
                    }
                }
            }
            Statement::Namespace(namespace) => {
                let child = self.new_scope(ScopeKind::Block, Some(scope));
                let body = &namespace.body;
                self.bind_statements(&body.data().statements, child);
                self.resolve_statements(&body.data().statements, child);
            }
            Statement::Declare(inner) => self.resolve_statement(inner, scope),
            Statement::Export(export) => self.resolve_export(export, scope),
            _ => {}
        }
    }

    fn resolve_export(&mut self, export: &'src crate::syntax::ExportDeclaration, scope: ScopeId) {
        match export {
            crate::syntax::ExportDeclaration::Named(
                crate::syntax::ExportNamedDeclaration::Declaration(inner),
            ) => self.resolve_statement(inner, scope),
            crate::syntax::ExportDeclaration::Default(default) => match &default.value {
                crate::syntax::ExportDefaultValue::Function(function) => {
                    self.resolve_function(function, scope);
                }
                crate::syntax::ExportDefaultValue::Class(class) => self.resolve_class(class, scope),
                crate::syntax::ExportDefaultValue::Expression(expression) => {
                    self.resolve_expr(expression, scope);
                }
                crate::syntax::ExportDefaultValue::Missing(_) => {}
            },
            crate::syntax::ExportDeclaration::Assignment(expression) => {
                self.resolve_expr(expression, scope);
            }
            _ => {}
        }
    }

    fn resolve_for_initializer(&mut self, initializer: &'src ForInitializer, scope: ScopeId) {
        match initializer {
            ForInitializer::Variable(variable) => {
                self.bind_variable(variable, scope, NodeId::default());
                self.resolve_variable(variable, scope);
            }
            ForInitializer::Expression(expression) => self.resolve_expr(expression, scope),
        }
    }

    fn resolve_for_binding(&mut self, binding: &'src ForBinding, scope: ScopeId) {
        match binding {
            ForBinding::Variable(variable) => {
                self.bind_variable(variable, scope, NodeId::default());
                self.resolve_variable(variable, scope);
            }
            ForBinding::Target(target) => self.resolve_assignment_target(target, scope),
        }
    }

    fn resolve_variable(&mut self, variable: &'src VariableDeclaration, scope: ScopeId) {
        for declarator in &variable.declarations {
            let declarator = declarator.data();
            if let Some(initializer) = &declarator.initializer {
                self.resolve_expr(initializer, scope);
            }
            let annotation = declarator
                .type_annotation
                .as_ref()
                .map(|annotation| self.resolve_type(&annotation.data().type_node, scope));
            let initializer_type = declarator
                .initializer
                .as_ref()
                .map(|initializer| self.type_of_expr(initializer, scope));

            // Only a plain identifier binding carries a checkable declared type.
            if let BindingPattern::Identifier(name) = declarator.binding.data() {
                let declared = annotation
                    .or(initializer_type)
                    .unwrap_or_else(|| self.types.any());
                if let Some(symbol) = self.lookup_value(scope, self.identifier_text(name)) {
                    self.symbol_types[symbol.get() as usize] = declared;
                }
                if let (Some(target), Some(source)) = (annotation, initializer_type)
                    && !self.types.assignable(source, target)
                {
                    let range = declarator
                        .initializer
                        .as_ref()
                        .map_or_else(|| name.range(), |initializer| initializer.range());
                    self.emit(TYPE_NOT_ASSIGNABLE, range, NOT_ASSIGNABLE_MESSAGE);
                }
            }
        }
    }

    fn is_numeric_enum_initializer(expression: &Expr) -> bool {
        match expression.data() {
            Expression::Literal(Literal::Number(_)) => true,
            Expression::Unary(unary)
                if matches!(unary.operator, UnaryOperator::Plus | UnaryOperator::Minus) =>
            {
                matches!(
                    unary.argument.data(),
                    Expression::Literal(Literal::Number(_))
                )
            }
            _ => false,
        }
    }

    fn bind_implicit_function_values(
        &mut self,
        parameters: &'src [crate::syntax::ParameterNode],
        scope: ScopeId,
    ) {
        for name in ["arguments", "this"] {
            let explicitly_bound = parameters.iter().any(|parameter| {
                matches!(
                    parameter.data().binding.data(),
                    BindingPattern::Identifier(identifier)
                        if self.identifier_text(identifier) == name
                )
            });
            if !explicitly_bound {
                self.declare(
                    name,
                    SymbolKind::Parameter,
                    scope,
                    NodeId::default(),
                    NodeId::default_range(),
                );
            }
        }
    }

    fn resolve_function(&mut self, function: &'src FunctionLike, parent: ScopeId) {
        let scope = self.new_scope(ScopeKind::Function, Some(parent));
        self.bind_implicit_function_values(&function.parameters, scope);
        if let Some(name) = &function.name {
            self.declare(
                self.identifier_text(name),
                SymbolKind::Function,
                scope,
                name.id(),
                name.range(),
            );
        }
        self.bind_type_parameters(function.type_parameters.as_ref(), scope);
        for parameter in &function.parameters {
            self.resolve_parameter(parameter, scope);
        }
        if let Some(return_type) = &function.return_type {
            let _ = self.resolve_type(&return_type.data().type_node, scope);
        }
        match &function.body {
            Some(FunctionBody::Block(block)) => {
                self.bind_statements(&block.data().statements, scope);
                self.bind_hoisted_statements(&block.data().statements, scope);
                self.resolve_statements(&block.data().statements, scope);
            }
            Some(FunctionBody::Expression(expression)) => self.resolve_expr(expression, scope),
            _ => {}
        }
    }

    fn bind_type_parameters(
        &mut self,
        list: Option<&'src crate::syntax::TypeParameterList>,
        scope: ScopeId,
    ) {
        self.bind_type_parameter_names(list, scope);
        self.resolve_type_parameter_bounds(list, scope);
    }

    fn bind_type_parameter_names(
        &mut self,
        list: Option<&'src crate::syntax::TypeParameterList>,
        scope: ScopeId,
    ) {
        let Some(list) = list else {
            return;
        };
        for parameter in &list.parameters {
            let data = parameter.data();
            self.declare(
                self.identifier_text(&data.name),
                SymbolKind::TypeParameter,
                scope,
                parameter.id(),
                data.name.range(),
            );
        }
    }

    fn resolve_type_parameter_bounds(
        &mut self,
        list: Option<&'src crate::syntax::TypeParameterList>,
        scope: ScopeId,
    ) {
        let Some(list) = list else {
            return;
        };
        for parameter in &list.parameters {
            let data = parameter.data();
            if let Some(constraint) = &data.constraint {
                let _ = self.resolve_type(constraint, scope);
            }
            if let Some(default) = &data.default {
                let _ = self.resolve_type(default, scope);
            }
        }
    }

    fn resolve_parameter(&mut self, parameter: &'src crate::syntax::ParameterNode, scope: ScopeId) {
        let data = parameter.data();
        self.bind_pattern(&data.binding, VariableKind::Let, scope, parameter.id());
        if let (BindingPattern::Identifier(name), Some(annotation)) =
            (data.binding.data(), &data.type_annotation)
        {
            let resolved = self.resolve_type(&annotation.data().type_node, scope);
            if let Some(symbol) = self.scopes[scope.0 as usize]
                .values
                .get(self.identifier_text(name))
                .copied()
            {
                self.symbol_types[symbol.get() as usize] = resolved;
            }
        } else if let Some(annotation) = &data.type_annotation {
            let _ = self.resolve_type(&annotation.data().type_node, scope);
        }
        if let Some(initializer) = &data.initializer {
            self.resolve_expr(initializer, scope);
        }
    }

    fn resolve_class(&mut self, class: &'src ClassDeclaration, parent: ScopeId) {
        let scope = self.new_scope(ScopeKind::Class, Some(parent));
        self.bind_type_parameters(class.type_parameters.as_ref(), scope);
        if let Some(heritage) = &class.extends {
            self.resolve_expr(&heritage.expression, parent);
        }
        for implemented in &class.implements {
            let _ = self.resolve_type(implemented, scope);
        }
        for member in &class.members {
            self.resolve_class_member(member.data(), scope);
        }
    }

    fn resolve_class_member(&mut self, member: &'src ClassMember, scope: ScopeId) {
        match member {
            ClassMember::Method(method) => {
                self.resolve_property_name(&method.name, scope);
                self.resolve_function(&method.function, scope);
            }
            ClassMember::Constructor(constructor) => {
                let child = self.new_scope(ScopeKind::Function, Some(scope));
                self.bind_implicit_function_values(&constructor.parameters, child);
                for parameter in &constructor.parameters {
                    self.resolve_parameter(parameter, child);
                }
                self.bind_statements(&constructor.body.data().statements, child);
                self.resolve_statements(&constructor.body.data().statements, child);
            }
            ClassMember::Property(property) => {
                self.resolve_property_name(&property.name, scope);
                if let Some(annotation) = &property.type_annotation {
                    let _ = self.resolve_type(&annotation.data().type_node, scope);
                }
                if let Some(initializer) = &property.initializer {
                    self.resolve_expr(initializer, scope);
                }
            }
            ClassMember::AutoAccessor(accessor) => {
                self.resolve_property_name(&accessor.name, scope);
                if let Some(initializer) = &accessor.initializer {
                    self.resolve_expr(initializer, scope);
                }
            }
            ClassMember::StaticBlock(block) => {
                let child = self.new_scope(ScopeKind::Block, Some(scope));
                self.bind_statements(&block.data().statements, child);
                self.resolve_statements(&block.data().statements, child);
            }
            _ => {}
        }
    }

    fn resolve_property_name(&mut self, name: &'src PropertyName, scope: ScopeId) {
        if let PropertyName::Computed(expression) = name {
            self.resolve_expr(expression, scope);
        }
    }

    fn resolve_expr(&mut self, expression: &'src Expr, scope: ScopeId) {
        match expression.data() {
            Expression::Identifier(identifier) => self.resolve_value(identifier, scope),
            Expression::Array(array) => {
                for element in &array.elements {
                    match element {
                        ArrayElement::Expression(inner) => self.resolve_expr(inner, scope),
                        ArrayElement::Spread(spread) => self.resolve_expr(&spread.argument, scope),
                        _ => {}
                    }
                }
            }
            Expression::Object(object) => {
                for member in &object.members {
                    self.resolve_object_member(member.data(), scope);
                }
            }
            Expression::Function(function) => self.resolve_function(&function.function, scope),
            Expression::Class(class) => self.resolve_class(&class.class, scope),
            Expression::Arrow(arrow) => {
                let child = self.new_scope(ScopeKind::Function, Some(scope));
                self.bind_type_parameters(arrow.type_parameters.as_ref(), child);
                for parameter in &arrow.parameters {
                    self.resolve_parameter(parameter, child);
                }
                if let Some(return_type) = &arrow.return_type {
                    let _ = self.resolve_type(&return_type.data().type_node, child);
                }
                match &arrow.body {
                    FunctionBody::Block(block) => {
                        self.bind_statements(&block.data().statements, child);
                        self.resolve_statements(&block.data().statements, child);
                    }
                    FunctionBody::Expression(inner) => self.resolve_expr(inner, child),
                    FunctionBody::Missing(_) => {}
                }
            }
            Expression::Call(call) => {
                self.resolve_expr(&call.callee, scope);
                self.resolve_type_arguments(call.type_arguments.as_ref(), scope);
                self.resolve_arguments(&call.arguments, scope);
            }
            Expression::New(new) => {
                self.resolve_expr(&new.callee, scope);
                self.resolve_type_arguments(new.type_arguments.as_ref(), scope);
                self.resolve_arguments(&new.arguments, scope);
            }
            Expression::Member(member) => {
                self.resolve_expr(&member.object, scope);
                if let MemberProperty::Computed(inner) = &member.property {
                    self.resolve_expr(inner, scope);
                }
            }
            Expression::Await(await_expression) => {
                self.resolve_expr(&await_expression.argument, scope);
            }
            Expression::Yield(yield_expression) => {
                if let Some(argument) = &yield_expression.argument {
                    self.resolve_expr(argument, scope);
                }
            }
            Expression::Unary(unary) => self.resolve_expr(&unary.argument, scope),
            Expression::Update(update) => self.resolve_assignment_target(&update.argument, scope),
            Expression::Binary(binary) => {
                self.resolve_expr(&binary.left, scope);
                self.resolve_expr(&binary.right, scope);
            }
            Expression::Logical(logical) => {
                self.resolve_expr(&logical.left, scope);
                self.resolve_expr(&logical.right, scope);
            }
            Expression::Conditional(conditional) => {
                self.resolve_expr(&conditional.test, scope);
                self.resolve_expr(&conditional.consequent, scope);
                self.resolve_expr(&conditional.alternate, scope);
            }
            Expression::Assignment(assignment) => {
                self.resolve_assignment_target(&assignment.left, scope);
                self.resolve_expr(&assignment.right, scope);
            }
            Expression::Sequence(sequence) => {
                for inner in &sequence.expressions {
                    self.resolve_expr(inner, scope);
                }
            }
            Expression::Parenthesized(inner) => self.resolve_expr(inner, scope),
            Expression::As(cast) => {
                self.resolve_expr(&cast.expression, scope);
                if let Some(type_node) = &cast.type_node {
                    let _ = self.resolve_type(type_node, scope);
                }
            }
            Expression::Satisfies(satisfies) => {
                self.resolve_expr(&satisfies.expression, scope);
                let _ = self.resolve_type(&satisfies.type_node, scope);
            }
            Expression::TypeAssertion(assertion) => {
                self.resolve_expr(&assertion.expression, scope);
                let _ = self.resolve_type(&assertion.type_node, scope);
            }
            Expression::NonNull(non_null) => self.resolve_expr(&non_null.expression, scope),
            Expression::TaggedTemplate(tagged) => {
                self.resolve_expr(&tagged.tag, scope);
                for inner in &tagged.template.expressions {
                    self.resolve_expr(inner, scope);
                }
            }
            Expression::Template(template) => {
                for inner in &template.expressions {
                    self.resolve_expr(inner, scope);
                }
            }
            Expression::Import(import) => {
                self.resolve_expr(&import.source, scope);
                if let Some(options) = &import.options {
                    self.resolve_expr(options, scope);
                }
            }
            _ => {}
        }
    }

    fn resolve_object_member(&mut self, member: &'src ObjectMember, scope: ScopeId) {
        match member {
            ObjectMember::Property(property) => {
                self.resolve_property_name(&property.name, scope);
                self.resolve_expr(&property.value, scope);
            }
            ObjectMember::Method(method) => {
                self.resolve_property_name(&method.name, scope);
                self.resolve_function(&method.function, scope);
            }
            ObjectMember::Spread(spread) => self.resolve_expr(&spread.argument, scope),
            ObjectMember::Missing(_) => {}
        }
    }

    fn resolve_arguments(&mut self, arguments: &'src [CallArgument], scope: ScopeId) {
        for argument in arguments {
            match argument {
                CallArgument::Expression(inner) => self.resolve_expr(inner, scope),
                CallArgument::Spread(spread) => self.resolve_expr(&spread.argument, scope),
                CallArgument::Missing(_) => {}
            }
        }
    }

    fn resolve_type_arguments(
        &mut self,
        arguments: Option<&'src crate::syntax::TypeArgumentList>,
        scope: ScopeId,
    ) {
        if let Some(list) = arguments {
            for argument in &list.arguments {
                let _ = self.resolve_type(argument, scope);
            }
        }
    }

    fn resolve_assignment_target(
        &mut self,
        target: &'src crate::syntax::AssignmentTargetNode,
        scope: ScopeId,
    ) {
        match target.data() {
            AssignmentTarget::Identifier(identifier) => self.resolve_value(identifier, scope),
            AssignmentTarget::Member(member) => {
                self.resolve_expr(&member.object, scope);
                if let MemberProperty::Computed(inner) = &member.property {
                    self.resolve_expr(inner, scope);
                }
            }
            AssignmentTarget::Object(object) => {
                for property in &object.properties {
                    self.resolve_property_name(&property.name, scope);
                    self.resolve_assignment_target(&property.target, scope);
                    if let Some(initializer) = &property.initializer {
                        self.resolve_expr(initializer, scope);
                    }
                }
            }
            AssignmentTarget::Array(array) => {
                for element in &array.elements {
                    if let crate::syntax::AssignmentArrayElement::Target(inner) = element {
                        self.resolve_assignment_target(inner, scope);
                    }
                }
            }
            AssignmentTarget::Missing(_) => {}
        }
    }

    fn resolve_value(&mut self, identifier: &IdentifierNode, scope: ScopeId) {
        let name = self.identifier_text(identifier);
        if name.is_empty() {
            return;
        }
        if let Some(symbol) = self.lookup_value(scope, name) {
            self.references.insert(identifier.id(), symbol);
        } else {
            self.emit(
                CANNOT_FIND_NAME,
                identifier.range(),
                CANNOT_FIND_NAME_MESSAGE,
            );
        }
    }

    fn lookup_value(&self, scope: ScopeId, name: &str) -> Option<SymbolId> {
        let mut current = Some(scope);
        while let Some(id) = current {
            let scope = &self.scopes[id.0 as usize];
            if let Some(symbol) = scope.values.get(name) {
                return Some(*symbol);
            }
            current = scope.parent;
        }
        None
    }

    fn lookup_type(&self, scope: ScopeId, name: &str) -> Option<SymbolId> {
        let mut current = Some(scope);
        while let Some(id) = current {
            let scope = &self.scopes[id.0 as usize];
            if let Some(symbol) = scope.types.get(name) {
                return Some(*symbol);
            }
            current = scope.parent;
        }
        None
    }

    // -- the named type algebra ------------------------------------------------

    fn resolve_type(&mut self, node: &'src Ty, scope: ScopeId) -> TypeId {
        match node.data() {
            TypeNode::Keyword(keyword) => self.keyword_type(*keyword),
            TypeNode::Literal(literal) => self.literal_type(literal),
            TypeNode::Reference(reference) => {
                self.resolve_type_reference(reference, scope, node.range())
            }
            TypeNode::Union(members) => {
                let resolved: Vec<TypeId> = members
                    .iter()
                    .map(|member| self.resolve_type(member, scope))
                    .collect();
                self.types.union(&resolved)
            }
            TypeNode::Array(element) => {
                let resolved = self.resolve_type(element, scope);
                self.types.array(resolved)
            }
            TypeNode::Object(object) => self.resolve_object_type(&object.members, scope),
            TypeNode::Function(function) => self.resolve_function_type(function, scope),
            TypeNode::Parenthesized(inner) => self.resolve_type(inner, scope),
            TypeNode::Tuple(tuple) => {
                let element_types: Vec<TypeId> = tuple
                    .elements
                    .iter()
                    .map(|element| self.resolve_type(&element.type_node, scope))
                    .collect();
                let element = self.types.union(&element_types);
                self.types.array(element)
            }
            _ => self.types.error_type(),
        }
    }

    fn keyword_type(&self, keyword: KeywordType) -> TypeId {
        match keyword {
            KeywordType::Any => self.types.any(),
            KeywordType::Unknown => self.types.unknown(),
            KeywordType::Never => self.types.never(),
            KeywordType::Void => self.types.void(),
            KeywordType::Undefined => self.types.undefined_type(),
            KeywordType::Null => self.types.null_type(),
            KeywordType::Boolean => self.types.boolean(),
            KeywordType::Number => self.types.number(),
            KeywordType::BigInt => self.types.bigint(),
            KeywordType::String => self.types.string(),
            KeywordType::Symbol => self.types.symbol_type(),
            KeywordType::Object => self.types.object(),
            KeywordType::Intrinsic => self.types.error_type(),
        }
    }

    fn literal_type(&mut self, literal: &TypeLiteral) -> TypeId {
        match literal {
            TypeLiteral::String(token) => {
                let text = self.text(token.data().token());
                self.types.string_literal(text)
            }
            TypeLiteral::Number(token) => {
                let text = self.text(token.data().token());
                self.types.number_literal(text)
            }
            TypeLiteral::BigInt(token) => {
                let text = self.text(token.data().token());
                self.types.bigint_literal(text)
            }
            TypeLiteral::Boolean(token) => {
                let value = self.text(token.data().token()) == "true";
                self.types.boolean_literal(value)
            }
            TypeLiteral::Null(_) => self.types.null_type(),
            TypeLiteral::Unary { .. } => self.types.number(),
        }
    }

    fn resolve_type_reference(
        &mut self,
        reference: &'src TypeReference,
        scope: ScopeId,
        range: TextRange,
    ) -> TypeId {
        if let Some(argument_list) = &reference.type_arguments {
            for argument in &argument_list.arguments {
                let _ = self.resolve_type(argument, scope);
            }
        }
        let EntityName::Identifier(identifier) = &reference.name else {
            // Qualified and missing names are opaque in this slice.
            return self.types.error_type();
        };
        let name = self.identifier_text(identifier);
        match self.lookup_type(scope, name) {
            Some(symbol) => match self.symbols[symbol.get() as usize].kind {
                SymbolKind::Interface | SymbolKind::TypeAlias | SymbolKind::Enum => {
                    self.resolve_type_symbol(symbol)
                }
                SymbolKind::Class | SymbolKind::TypeParameter => self.types.named(symbol),
                _ => self.types.error_type(),
            },
            None => {
                self.emit(CANNOT_FIND_TYPE, range, CANNOT_FIND_TYPE_MESSAGE);
                self.types.error_type()
            }
        }
    }

    fn resolve_type_symbol(&mut self, symbol: SymbolId) -> TypeId {
        match self.type_state[symbol.get() as usize] {
            TypeState::Done(id) => return id,
            TypeState::InProgress => return self.types.error_type(),
            TypeState::Unresolved => {}
        }
        let Some(definition) = self.type_defs.get(&symbol).copied() else {
            let id = self.types.error_type();
            self.type_state[symbol.get() as usize] = TypeState::Done(id);
            return id;
        };
        self.type_state[symbol.get() as usize] = TypeState::InProgress;
        let resolved = match definition {
            TypeDef::Alias {
                scope,
                type_parameters,
                node,
            } => {
                self.resolve_type_parameter_bounds(type_parameters, scope);
                self.resolve_type(node, scope)
            }
            TypeDef::Interface {
                scope,
                type_parameters,
                extends,
                members,
            } => {
                self.resolve_type_parameter_bounds(type_parameters, scope);
                self.resolve_interface_type(scope, extends, members)
            }
            TypeDef::Enum { numeric } => {
                if numeric {
                    self.types.numeric_enum(symbol)
                } else {
                    self.types.named(symbol)
                }
            }
        };
        self.type_state[symbol.get() as usize] = TypeState::Done(resolved);
        resolved
    }

    fn resolve_interface_type(
        &mut self,
        scope: ScopeId,
        extends: &'src [TypeReference],
        members: &'src [crate::syntax::TypeMemberNode],
    ) -> TypeId {
        let mut properties = self.type_member_properties(members, scope);
        for base in extends {
            let base_type = self.resolve_type_reference(base, scope, NodeId::default_range());
            if let Type::ObjectType(base_props) = self.types.get(base_type) {
                for base_prop in base_props.clone() {
                    if !properties.iter().any(|prop| prop.name == base_prop.name) {
                        properties.push(base_prop);
                    }
                }
            }
        }
        self.types.object_type(properties)
    }

    fn resolve_object_type(
        &mut self,
        members: &'src [crate::syntax::TypeMemberNode],
        scope: ScopeId,
    ) -> TypeId {
        let properties = self.type_member_properties(members, scope);
        self.types.object_type(properties)
    }

    fn type_member_properties(
        &mut self,
        members: &'src [crate::syntax::TypeMemberNode],
        scope: ScopeId,
    ) -> Vec<PropertyType> {
        let mut properties = Vec::new();
        for member in members {
            match member.data() {
                TypeMember::Property(property) => {
                    if let Some(name) = self.property_key(&property.name) {
                        let type_id = match &property.type_annotation {
                            Some(annotation) => {
                                self.resolve_type(&annotation.data().type_node, scope)
                            }
                            None => self.types.any(),
                        };
                        properties.push(PropertyType::new(name, property.optional, type_id));
                    }
                }
                TypeMember::Method(method) => {
                    if let Some(name) = self.property_key(&method.name) {
                        let type_id = self.resolve_function_type(&method.function, scope);
                        properties.push(PropertyType::new(name, method.optional, type_id));
                    }
                }
                _ => {}
            }
        }
        properties
    }

    fn resolve_function_type(&mut self, function: &'src FunctionType, scope: ScopeId) -> TypeId {
        let child = self.new_scope(ScopeKind::Function, Some(scope));
        self.bind_type_parameters(function.type_parameters.as_ref(), child);
        let parameters: Vec<TypeId> = function
            .parameters
            .iter()
            .map(|parameter| self.resolve_type(&parameter.type_annotation.data().type_node, child))
            .collect();
        let return_type = self.resolve_type(&function.return_type, child);
        self.types.function(parameters, return_type)
    }

    fn property_key(&self, name: &PropertyName) -> Option<String> {
        match name {
            PropertyName::Identifier(identifier) => {
                Some(self.identifier_text(identifier).to_owned())
            }
            PropertyName::String(string) => {
                let text = self.text(string.data().token());
                Some(
                    text.trim_matches(|c| c == '"' || c == '\'' || c == '`')
                        .to_owned(),
                )
            }
            PropertyName::Number(number) => Some(self.text(number.data().token()).to_owned()),
            _ => None,
        }
    }

    // -- expression typing (bounded, permissive) -------------------------------

    fn type_of_expr(&mut self, expression: &'src Expr, scope: ScopeId) -> TypeId {
        match expression.data() {
            Expression::Identifier(identifier) => {
                self.references.get(&identifier.id()).map_or_else(
                    || self.types.any(),
                    |symbol| self.symbol_types[symbol.get() as usize],
                )
            }
            Expression::Literal(literal) => self.type_of_literal(literal),
            Expression::Parenthesized(inner) => self.type_of_expr(inner, scope),
            Expression::NonNull(non_null) => self.type_of_expr(&non_null.expression, scope),
            Expression::As(cast) => match &cast.type_node {
                Some(type_node) => self.resolve_type(type_node, scope),
                None => self.type_of_expr(&cast.expression, scope),
            },
            Expression::TypeAssertion(assertion) => self.resolve_type(&assertion.type_node, scope),
            Expression::Array(array) => {
                let mut element_types = Vec::new();
                for element in &array.elements {
                    if let ArrayElement::Expression(inner) = element {
                        let inner_type = self.type_of_expr(inner, scope);
                        element_types.push(inner_type);
                    }
                }
                let element = if element_types.is_empty() {
                    self.types.never()
                } else {
                    self.types.union(&element_types)
                };
                self.types.array(element)
            }
            Expression::Object(object) => {
                let mut properties = Vec::new();
                for member in &object.members {
                    match member.data() {
                        ObjectMember::Property(property) => {
                            if let Some(name) = self.property_key(&property.name) {
                                let value_type = self.type_of_expr(&property.value, scope);
                                properties.push(PropertyType::new(name, false, value_type));
                            }
                        }
                        ObjectMember::Method(method) => {
                            if let Some(name) = self.property_key(&method.name) {
                                let method_type =
                                    self.type_of_function_like(&method.function, scope);
                                properties.push(PropertyType::new(name, false, method_type));
                            }
                        }
                        _ => {}
                    }
                }
                self.types.object_type(properties)
            }
            _ => self.types.any(),
        }
    }

    fn type_of_function_like(&mut self, function: &'src FunctionLike, parent: ScopeId) -> TypeId {
        let scope = self.new_scope(ScopeKind::Function, Some(parent));
        self.bind_type_parameters(function.type_parameters.as_ref(), scope);
        let mut parameters = Vec::with_capacity(function.parameters.len());
        for parameter in &function.parameters {
            let parameter_type = match &parameter.data().type_annotation {
                Some(annotation) => self.resolve_type(&annotation.data().type_node, scope),
                None => self.types.any(),
            };
            parameters.push(parameter_type);
        }
        let return_type = match &function.return_type {
            Some(annotation) => self.resolve_type(&annotation.data().type_node, scope),
            None => self.types.any(),
        };
        self.types.function(parameters, return_type)
    }

    fn type_of_literal(&mut self, literal: &Literal) -> TypeId {
        match literal {
            Literal::String(token) => {
                let text = self.text(token.data().token());
                self.types.string_literal(text)
            }
            Literal::Number(token) => {
                let text = self.text(token.data().token());
                self.types.number_literal(text)
            }
            Literal::BigInt(token) => {
                let text = self.text(token.data().token());
                self.types.bigint_literal(text)
            }
            Literal::Boolean(token) => {
                let value = self.text(token.data().token()) == "true";
                self.types.boolean_literal(value)
            }
            Literal::Null(_) => self.types.null_type(),
            Literal::Regex(_) => self.types.object(),
        }
    }
}

/// A default zero range for synthesized diagnostics anchored on missing syntax.
trait DefaultRange {
    fn default_range() -> TextRange;
}

impl DefaultRange for NodeId {
    fn default_range() -> TextRange {
        use crate::source::Utf16Pos;
        TextRange::new(Utf16Pos::ZERO, Utf16Pos::ZERO).expect("zero range is ordered")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CANNOT_FIND_NAME, CANNOT_FIND_TYPE, DUPLICATE_DECLARATION, PropertyType, ScopeKind,
        SymbolKind, TYPE_NOT_ASSIGNABLE, Type, TypeTable, check,
    };
    use crate::diagnostic::{DiagnosticSeverity, Recovered};
    use crate::source::{ScriptKind, SourceId, SourceText, TextRange, Utf16Pos};
    use crate::syntax::{
        ArrowFunction, BindingPattern, Block, EntityName, Expr, Expression, ExpressionStatement,
        FunctionBody, Identifier, IdentifierNode, KeywordType, Literal, MissingNode, Node, NodeId,
        NodeKind, NumericLiteral, Parameter, ParameterNode, SourceFile, Statement, Stmt,
        StringLiteral, Token, TokenKind, TypeAnnotation, TypeNode,
    };
    use crate::{parser, scanner};
    use std::sync::Arc;

    // ---- direct algebra tests -------------------------------------------------

    #[test]
    fn top_and_bottom_types_bound_the_lattice() {
        let table = TypeTable::new();
        // never flows into everything; nothing else flows into never.
        assert!(table.assignable(table.never(), table.number()));
        assert!(!table.assignable(table.number(), table.never()));
        // unknown is the top: everything in, nothing out.
        assert!(table.assignable(table.number(), table.unknown()));
        assert!(!table.assignable(table.unknown(), table.number()));
        // any is the escape hatch both directions.
        assert!(table.assignable(table.any(), table.number()));
        assert!(table.assignable(table.number(), table.any()));
    }

    #[test]
    fn literals_widen_to_their_base_primitive_only() {
        let mut table = TypeTable::new();
        let one = table.number_literal("1");
        assert!(table.assignable(one, table.number()));
        assert!(!table.assignable(table.number(), one));
        assert!(!table.assignable(one, table.string()));
    }

    #[test]
    fn union_source_requires_all_members_target_requires_one() {
        let mut table = TypeTable::new();
        let number_or_string = table.union(&[table.number(), table.string()]);
        assert!(table.assignable(table.number(), number_or_string));
        assert!(!table.assignable(table.boolean(), number_or_string));
        assert!(table.assignable(number_or_string, table.unknown()));
        assert!(!table.assignable(number_or_string, table.number()));
    }

    #[test]
    fn union_normalizes_absorption_and_duplicates() {
        let mut table = TypeTable::new();
        assert_eq!(
            table.union(&[table.number(), table.number()]),
            table.number()
        );
        assert_eq!(
            table.union(&[table.number(), table.never()]),
            table.number()
        );
        assert_eq!(table.union(&[table.number(), table.any()]), table.any());
    }

    #[test]
    fn arrays_are_covariant_in_their_element() {
        let mut table = TypeTable::new();
        let number_literal = table.number_literal("1");
        let literal_array = table.array(number_literal);
        let number_array = table.array(table.number());
        assert!(table.assignable(literal_array, number_array));
        assert!(!table.assignable(number_array, literal_array));
    }

    #[test]
    fn objects_are_structural_with_optional_and_excess_rules() {
        let mut table = TypeTable::new();
        let required = table.object_type(vec![PropertyType::new("x", false, table.number())]);
        let with_excess = table.object_type(vec![
            PropertyType::new("x", false, table.number()),
            PropertyType::new("y", false, table.string()),
        ]);
        let missing = table.object_type(vec![PropertyType::new("y", false, table.string())]);
        let optional = table.object_type(vec![PropertyType::new("x", true, table.number())]);
        let empty = table.object_type(vec![]);

        // Excess source properties are allowed structurally.
        assert!(table.assignable(with_excess, required));
        // A missing required property is rejected.
        assert!(!table.assignable(missing, required));
        // An optional target property may be absent.
        assert!(table.assignable(empty, optional));
    }

    #[test]
    fn functions_are_contravariant_in_params_covariant_in_return() {
        let mut table = TypeTable::new();
        let animal = table.named(super::SymbolId::new(100));
        let dog = table.named(super::SymbolId::new(101));
        // fn(animal) -> dog  <:  fn(dog) -> animal  is unrelated nominally, but
        // the variance shape is what we assert here.
        let number = table.number();
        let takes_number = table.function(vec![number], table.void());
        let number_literal = table.number_literal("1");
        let takes_number_literal = table.function(vec![number_literal], table.void());
        // target param number must be assignable to source param numberLiteral?
        // No: number is not a number literal, so it is rejected (contravariant).
        assert!(!table.assignable(takes_number_literal, takes_number));
        // Fewer source params is fine.
        let takes_none = table.function(vec![], table.void());
        assert!(table.assignable(takes_none, takes_number));
        // Return void absorbs any source return.
        let returns_number = table.function(vec![], table.number());
        assert!(table.assignable(returns_number, takes_none));
        // Silence unused nominal helpers when variance path above suffices.
        assert_ne!(animal, dog);
    }

    #[test]
    fn relation_retains_optional_callback_void_and_enum_hazards() {
        let mut table = TypeTable::new();
        let source_object =
            table.object_type(vec![PropertyType::new("x", false, table.undefined_type())]);
        let target_object = table.object_type(vec![PropertyType::new("x", true, table.number())]);
        let optional = table.relation(source_object, target_object);
        assert!(optional.compatible());
        assert!(
            optional
                .hazards()
                .contains(&super::RelationHazard::ExplicitUndefinedForOptional)
        );

        let source_function = table.function(Vec::new(), table.number());
        let target_function = table.function(vec![table.number()], table.void());
        let callback = table.relation(source_function, target_function);
        assert!(
            callback
                .hazards()
                .contains(&super::RelationHazard::FewerCallbackParameters)
        );
        assert!(
            callback
                .hazards()
                .contains(&super::RelationHazard::ValueReturnedToVoid)
        );

        let enum_type = table.numeric_enum(super::SymbolId::new(200));
        let enum_boundary = table.relation(enum_type, table.number());
        assert!(enum_boundary.compatible());
        assert!(
            enum_boundary
                .hazards()
                .contains(&super::RelationHazard::NumericEnumNumber)
        );
    }

    #[test]
    fn optional_any_and_unknown_already_contain_undefined() {
        for value_type in [TypeTable::new().any(), TypeTable::new().unknown()] {
            let mut table = TypeTable::new();
            let source =
                table.object_type(vec![PropertyType::new("x", false, table.undefined_type())]);
            let target = table.object_type(vec![PropertyType::new("x", true, value_type)]);
            assert!(
                !table
                    .relation(source, target)
                    .hazards()
                    .contains(&super::RelationHazard::ExplicitUndefinedForOptional)
            );
        }
    }

    // ---- checker behavior tests ----------------------------------------------

    fn source(text: &str) -> Arc<SourceText> {
        Arc::new(SourceText::new(text))
    }

    fn check_text(text: &str) -> Recovered<super::SemanticModel> {
        let parsed = parser::parse(scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            source(text),
        ));
        check(&parsed)
    }

    fn checker_codes(result: &Recovered<super::SemanticModel>) -> Vec<&'static str> {
        result
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .filter(|code| code.starts_with("BAMTS-C"))
            .collect()
    }

    fn range(start: usize, end: usize) -> TextRange {
        TextRange::new(Utf16Pos::new(start), Utf16Pos::new(end)).expect("ordered range")
    }

    fn identifier(id: u32, name: &str, start: usize) -> IdentifierNode {
        let end = start + name.len();
        Node::new(
            NodeId::new(id),
            range(start, end),
            Identifier::new(Token::new(TokenKind::Identifier, range(start, end))),
        )
    }

    /// Builds a `SourceFile` whose statements are supplied directly, so binding
    /// and typing can be exercised without depending on a parser.
    fn file(text: &str, statements: Vec<Stmt>) -> Recovered<SourceFile> {
        let source = source(text);
        let end = source.len_utf16().get();
        let eof = Token::new(TokenKind::EndOfFile, range(end, end));
        let file = SourceFile::new(
            NodeId::new(0),
            SourceId::new(1),
            ScriptKind::TypeScript,
            range(0, end),
            source,
            Vec::new(),
            statements,
            eof,
            Vec::new(),
        );
        Recovered::clean(file)
    }

    fn keyword_annotation(
        id: u32,
        keyword: KeywordType,
        start: usize,
        end: usize,
    ) -> Node<TypeAnnotation> {
        let type_node = Node::new(
            NodeId::new(id),
            range(start, end),
            TypeNode::Keyword(keyword),
        );
        Node::new(
            NodeId::new(id + 1),
            range(start, end),
            TypeAnnotation {
                type_node: Box::new(type_node),
            },
        )
    }

    fn variable(
        id: u32,
        text: &str,
        name: &str,
        name_start: usize,
        annotation: Option<Node<TypeAnnotation>>,
        initializer: Option<Box<Expr>>,
    ) -> Stmt {
        let name_node = identifier(id + 1, name, name_start);
        let binding = Node::new(
            NodeId::new(id + 2),
            name_node.range(),
            BindingPattern::Identifier(name_node),
        );
        let declarator = Node::new(
            NodeId::new(id + 3),
            range(0, text.len()),
            crate::syntax::VariableDeclarator {
                binding,
                definite: false,
                type_annotation: annotation,
                initializer,
            },
        );
        Node::new(
            NodeId::new(id),
            range(0, text.len()),
            Statement::Variable(crate::syntax::VariableDeclaration {
                kind: crate::syntax::VariableKind::Const,
                declarations: vec![declarator],
            }),
        )
    }

    fn number_expr(id: u32, text: &str, start: usize) -> Box<Expr> {
        let end = start + text.len();
        let literal = Node::new(
            NodeId::new(id + 1),
            range(start, end),
            NumericLiteral::new(Token::new(TokenKind::NumericLiteral, range(start, end))),
        );
        Box::new(Node::new(
            NodeId::new(id),
            range(start, end),
            Expression::Literal(Literal::Number(literal)),
        ))
    }

    fn string_expr(id: u32, text: &str, start: usize) -> Box<Expr> {
        let end = start + text.len();
        let literal = Node::new(
            NodeId::new(id + 1),
            range(start, end),
            StringLiteral::new(Token::new(TokenKind::StringLiteral, range(start, end))),
        );
        Box::new(Node::new(
            NodeId::new(id),
            range(start, end),
            Expression::Literal(Literal::String(literal)),
        ))
    }

    fn identifier_expr(id: u32, name: &str, start: usize) -> Box<Expr> {
        Box::new(Node::new(
            NodeId::new(id),
            range(start, start + name.len()),
            Expression::Identifier(identifier(id + 1, name, start)),
        ))
    }

    fn expression_statement(id: u32, expression: Box<Expr>) -> Stmt {
        Node::new(
            NodeId::new(id),
            expression.range(),
            Statement::Expression(ExpressionStatement { expression }),
        )
    }

    fn semantic_codes(model: &Recovered<super::SemanticModel>) -> Vec<&'static str> {
        model
            .diagnostics()
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect()
    }

    #[test]
    fn binds_a_variable_and_resolves_its_later_reference() {
        let statements = vec![
            variable(
                10,
                "const a = 1;",
                "a",
                6,
                None,
                Some(number_expr(20, "1", 10)),
            ),
            expression_statement(30, identifier_expr(31, "a", 13)),
        ];
        let result = check(&file("const a = 1; a;", statements));
        assert!(semantic_codes(&result).is_empty());
        let model = result.product();
        let symbol = model
            .lookup_value(model.module_scope(), "a")
            .expect("a is bound");
        assert!(matches!(
            model.symbol(symbol).kind(),
            SymbolKind::Variable(_)
        ));
        assert_eq!(model.resolved_reference_count(), 1);
        assert_eq!(model.scope(model.module_scope()).kind(), ScopeKind::Module);
    }

    #[test]
    fn reports_an_unresolved_local_value_reference() {
        let statements = vec![expression_statement(30, identifier_expr(31, "missing", 0))];
        let result = check(&file("missing;", statements));
        assert_eq!(semantic_codes(&result), [CANNOT_FIND_NAME.as_str()]);
    }

    #[test]
    fn a_global_value_reference_is_not_unresolved() {
        let statements = vec![expression_statement(30, identifier_expr(31, "console", 0))];
        let result = check(&file("console;", statements));
        assert!(semantic_codes(&result).is_empty());
    }

    #[test]
    fn standard_global_families_bind_as_intrinsics() {
        let names = [
            // ECMAScript values and constructors.
            "JSON",
            "Math",
            "Object",
            "Array",
            "Promise",
            "Error",
            "TypeError",
            "escape",
            "unescape",
            // Collections, reflection, shared-memory, and typed-array families.
            "Map",
            "Set",
            "Symbol",
            "Reflect",
            "Atomics",
            "Int8Array",
            "BigUint64Array",
            // Timers and URL/text host APIs.
            "setTimeout",
            "clearInterval",
            "queueMicrotask",
            "URL",
            "URLSearchParams",
            "TextEncoder",
            "TextDecoder",
            // Node host globals that the runtime installs.
            "console",
            "process",
            "globalThis",
        ];
        let text = names.join(";");
        let mut start = 0;
        let statements = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let statement = expression_statement(
                    u32::try_from(index * 2 + 30).expect("test node id fits u32"),
                    identifier_expr(
                        u32::try_from(index * 2 + 31).expect("test node id fits u32"),
                        name,
                        start,
                    ),
                );
                start += name.len() + 1;
                statement
            })
            .collect();
        let result = check(&file(&text, statements));
        assert!(
            semantic_codes(&result).is_empty(),
            "intrinsic diagnostics: {:?}",
            result.diagnostics()
        );
        assert_eq!(result.product().resolved_reference_count(), names.len());
    }

    #[test]
    fn primitive_wrapper_names_bind_in_type_position() {
        let result = check_text(
            "let a: Boolean; let b: Number; let c: String; let d: Symbol; let e: BigInt;",
        );
        assert!(
            !checker_codes(&result).contains(&CANNOT_FIND_TYPE.as_str()),
            "primitive wrappers must be checker-visible types: {:?}",
            result.diagnostics()
        );
    }

    #[test]
    fn source_enum_references_construct_numeric_enum_types() {
        let result = check_text("enum E { A } let e: E = E.A;");
        let model = result.product();
        let symbol = model
            .lookup_value(model.module_scope(), "e")
            .expect("enum-typed binding is present");
        assert!(matches!(
            model.types().get(model.symbol_type(symbol)),
            Type::NumericEnum(_)
        ));
    }

    #[test]
    fn local_bindings_shadow_intrinsics() {
        let statements = vec![
            variable(
                10,
                "const console = 1;",
                "console",
                6,
                None,
                Some(number_expr(20, "1", 16)),
            ),
            expression_statement(30, identifier_expr(31, "console", 19)),
        ];
        let result = check(&file("const console = 1; console;", statements));
        assert!(semantic_codes(&result).is_empty());
        let model = result.product();
        let local = model
            .lookup_value(model.module_scope(), "console")
            .expect("local console binding exists");
        assert_eq!(model.reference(NodeId::new(32)), Some(local));
    }

    #[test]
    fn reports_an_unknown_name_even_with_intrinsics() {
        let statements = vec![expression_statement(
            30,
            identifier_expr(31, "notAGlobal", 0),
        )];
        let result = check(&file("notAGlobal;", statements));
        assert_eq!(semantic_codes(&result), [CANNOT_FIND_NAME.as_str()]);
    }

    #[test]
    fn reports_a_duplicate_block_scoped_declaration() {
        let statements = vec![
            variable(
                10,
                "const a = 1;",
                "a",
                6,
                None,
                Some(number_expr(20, "1", 10)),
            ),
            variable(
                40,
                "const a = 2;",
                "a",
                19,
                None,
                Some(number_expr(50, "2", 23)),
            ),
        ];
        let result = check(&file("const a = 1; const a = 2;", statements));
        assert_eq!(semantic_codes(&result), [DUPLICATE_DECLARATION.as_str()]);
    }

    #[test]
    fn a_shadowing_binding_in_a_nested_block_is_not_a_duplicate() {
        let inner = variable(
            40,
            "const a = 2;",
            "a",
            21,
            None,
            Some(number_expr(50, "2", 25)),
        );
        let block = Node::new(
            NodeId::new(60),
            range(13, 29),
            Statement::Block(Node::new(
                NodeId::new(61),
                range(13, 29),
                Block {
                    statements: vec![inner],
                },
            )),
        );
        let statements = vec![
            variable(
                10,
                "const a = 1;",
                "a",
                6,
                None,
                Some(number_expr(20, "1", 10)),
            ),
            block,
        ];
        let result = check(&file("const a = 1; { const a = 2; }", statements));
        assert!(semantic_codes(&result).is_empty());
    }

    #[test]
    fn a_number_literal_is_not_assignable_to_a_string_annotation() {
        let annotation = keyword_annotation(70, KeywordType::String, 9, 15);
        let statements = vec![variable(
            10,
            "const x: string = 1;",
            "x",
            6,
            Some(annotation),
            Some(number_expr(20, "1", 18)),
        )];
        let result = check(&file("const x: string = 1;", statements));
        assert_eq!(semantic_codes(&result), [TYPE_NOT_ASSIGNABLE.as_str()]);
    }

    #[test]
    fn a_matching_literal_initializer_is_accepted() {
        let annotation = keyword_annotation(70, KeywordType::Number, 9, 15);
        let statements = vec![variable(
            10,
            "const x: number = 1;",
            "x",
            6,
            Some(annotation),
            Some(number_expr(20, "1", 18)),
        )];
        let result = check(&file("const x: number = 1;", statements));
        assert!(semantic_codes(&result).is_empty());
    }

    #[test]
    fn an_unresolved_type_annotation_reports_cannot_find_type() {
        let reference = crate::syntax::TypeReference {
            name: EntityName::Identifier(identifier(71, "Foo", 9)),
            type_arguments: None,
        };
        let type_node = Node::new(
            NodeId::new(72),
            range(9, 12),
            TypeNode::Reference(reference),
        );
        let annotation = Node::new(
            NodeId::new(73),
            range(9, 12),
            TypeAnnotation {
                type_node: Box::new(type_node),
            },
        );
        let statements = vec![variable(
            10,
            "const x: Foo;",
            "x",
            6,
            Some(annotation),
            None,
        )];
        let result = check(&file("const x: Foo;", statements));
        assert_eq!(semantic_codes(&result), [CANNOT_FIND_TYPE.as_str()]);
    }

    #[test]
    fn generic_declarations_bind_their_type_parameters() {
        let result = check_text(
            "type Box<T> = { value: T };\
             interface Pair<T> { left: T; map<U>(value: U): T; }\
             class Store<T> { value: T; method<U>(value: U): T { return this.value; } }",
        );
        assert!(checker_codes(&result).is_empty());
    }

    #[test]
    fn imported_names_bind_in_the_type_namespace_through_exports() {
        let result = check_text(
            "import type { Remote } from './remote.ts';\
             export type Local<T> = Remote;\
             export interface Public<T> { value: Local<T>; remote: Remote; }",
        );
        assert!(checker_codes(&result).is_empty());
    }

    #[test]
    fn standard_iterator_and_generator_interfaces_are_bound() {
        let result = check_text(
            "declare let iterator: IterableIterator<number>;\
             async function* values(): AsyncGenerator<number> { yield 1; }",
        );
        assert!(checker_codes(&result).is_empty());
    }

    #[test]
    fn functions_bind_arguments_this_and_their_local_name() {
        let result = check_text(
            "const recursive = function self(this: void) { arguments; return self; };\
             class C { method() { arguments; return this; } }",
        );
        assert!(checker_codes(&result).is_empty());
    }

    #[test]
    fn ambient_declarations_bind_before_their_uses() {
        let result = check_text(
            "const before: Box<number> = make<number>();\
             declare interface Box<T> { value: T; }\
             declare function make<T>(): Box<T>;",
        );
        assert!(checker_codes(&result).is_empty());
    }

    #[test]
    fn local_generic_casts_resolve_in_the_enclosing_function() {
        let result = check_text(
            "function copy<T>(value: T): T { const result = value as T; return result; }",
        );
        assert!(checker_codes(&result).is_empty());
    }

    #[test]
    fn const_assertions_preserve_literal_expression_types() {
        let result = check_text("const state: \"ready\" = \"ready\" as const;");
        assert!(checker_codes(&result).is_empty());
    }

    #[test]
    fn object_methods_satisfy_structural_function_members() {
        let result = check_text(
            "interface Service { compute(value: number): Promise<number>; }\
             const service: Service = { async compute(value: number) { return value; } };",
        );
        assert!(checker_codes(&result).is_empty());
    }

    #[test]
    fn unknown_names_and_real_initializer_mismatches_remain_errors() {
        let result =
            check_text("missingValue; let missing: MissingType; const count: number = 'wrong';");
        assert_eq!(
            checker_codes(&result),
            [
                CANNOT_FIND_NAME.as_str(),
                CANNOT_FIND_TYPE.as_str(),
                TYPE_NOT_ASSIGNABLE.as_str(),
            ]
        );
    }

    #[test]
    fn hard_warnings_merge_into_ordered_diagnostics() {
        // A single-string source that triggers hard-warning W005 plus an
        // unresolved reference should yield both, canonically ordered.
        let text = "try {} catch (error) { error.message; }";
        // Reference `nope` (unresolved) placed before via an expression stmt with
        // an earlier range so ordering is observable.
        let statements = vec![expression_statement(30, identifier_expr(31, "nope", 0))];
        let result = check(&file(text, statements));
        let diagnostics = result.diagnostics();
        // Both a semantic error and a hard warning are present.
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == CANNOT_FIND_NAME)
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity() == DiagnosticSeverity::Warning)
        );
        // Diagnostics are in canonical (sorted) order.
        let mut sorted = diagnostics.to_vec();
        sorted.sort();
        assert_eq!(diagnostics, sorted.as_slice());
    }

    #[test]
    fn a_parameter_reference_resolves_within_its_function_scope() {
        let parameter_name = identifier(81, "p", 11);
        let binding = Node::new(
            NodeId::new(82),
            parameter_name.range(),
            BindingPattern::Identifier(parameter_name),
        );
        let parameter: ParameterNode = Node::new(
            NodeId::new(83),
            range(11, 12),
            Parameter {
                decorators: Vec::new(),
                modifiers: crate::syntax::ParameterModifiers::default(),
                binding,
                optional: false,
                type_annotation: Some(keyword_annotation(84, KeywordType::Unknown, 14, 21)),
                initializer: None,
            },
        );
        let body = identifier_expr(90, "p", 26);
        let arrow = Node::new(
            NodeId::new(80),
            range(10, 27),
            Expression::Arrow(ArrowFunction {
                is_async: false,
                type_parameters: None,
                parameters: vec![parameter],
                return_type: None,
                body: FunctionBody::Expression(body),
            }),
        );
        let statements = vec![variable(
            10,
            "const f = (p: unknown) => p;",
            "f",
            6,
            None,
            Some(Box::new(arrow)),
        )];
        let result = check(&file("const f = (p: unknown) => p;", statements));
        assert!(semantic_codes(&result).is_empty());
        // Scope tree contains a function scope for the arrow.
        let model = result.product();
        assert!(
            model
                .scopes()
                .iter()
                .any(|scope| scope.kind() == ScopeKind::Function)
        );
    }

    #[test]
    fn a_string_initializer_matches_a_string_annotation() {
        let annotation = keyword_annotation(70, KeywordType::String, 9, 15);
        let statements = vec![variable(
            10,
            "const s: string = \"ok\";",
            "s",
            6,
            Some(annotation),
            Some(string_expr(20, "\"ok\"", 18)),
        )];
        let result = check(&file("const s: string = \"ok\";", statements));
        assert!(semantic_codes(&result).is_empty());
        let model = result.product();
        let symbol = model
            .lookup_value(model.module_scope(), "s")
            .expect("s is bound");
        assert_eq!(model.symbol_type(symbol), model.types().string());
    }

    #[test]
    fn missing_identifiers_never_panic_the_checker() {
        // An identifier with an empty lexeme must be ignored, not reported.
        let missing = Node::new(
            NodeId::new(31),
            range(0, 0),
            Expression::Missing(MissingNode::new(NodeKind::IdentifierExpression)),
        );
        let statements = vec![expression_statement(30, Box::new(missing))];
        let result = check(&file("", statements));
        assert!(semantic_codes(&result).is_empty());
    }

    // ---- var hoisting regression tests ---------------------------------------

    fn var_declaration(
        kind: crate::syntax::VariableKind,
        id: u32,
        name: &str,
        name_start: usize,
        initializer: Option<Box<Expr>>,
    ) -> crate::syntax::VariableDeclaration {
        let name_node = identifier(id + 1, name, name_start);
        let binding = Node::new(
            NodeId::new(id + 2),
            name_node.range(),
            BindingPattern::Identifier(name_node),
        );
        let declarator = Node::new(
            NodeId::new(id + 3),
            range(name_start, name_start + name.len()),
            crate::syntax::VariableDeclarator {
                binding,
                definite: false,
                type_annotation: None,
                initializer,
            },
        );
        crate::syntax::VariableDeclaration {
            kind,
            declarations: vec![declarator],
        }
    }

    fn variable_kind(
        kind: crate::syntax::VariableKind,
        id: u32,
        text: &str,
        name: &str,
        name_start: usize,
        initializer: Option<Box<Expr>>,
    ) -> Stmt {
        Node::new(
            NodeId::new(id),
            range(0, text.len()),
            Statement::Variable(var_declaration(kind, id, name, name_start, initializer)),
        )
    }

    fn block_statement(id: u32, statements: Vec<Stmt>) -> Stmt {
        Node::new(
            NodeId::new(id),
            range(0, 1),
            Statement::Block(Node::new(
                NodeId::new(id + 1),
                range(0, 1),
                Block { statements },
            )),
        )
    }

    #[test]
    fn a_var_in_a_block_hoists_to_the_module_and_resolves_outside() {
        let inner = variable_kind(
            crate::syntax::VariableKind::Var,
            40,
            "var a = 1;",
            "a",
            6,
            Some(number_expr(50, "1", 10)),
        );
        let block = block_statement(60, vec![inner]);
        let statements = vec![
            block,
            expression_statement(70, identifier_expr(71, "a", 15)),
        ];
        let result = check(&file("{ var a = 1; } a;", statements));
        assert!(semantic_codes(&result).is_empty());
        let model = result.product();
        let symbol = model
            .lookup_value(model.module_scope(), "a")
            .expect("var a hoists to the module scope");
        assert!(matches!(
            model.symbol(symbol).kind(),
            SymbolKind::Variable(crate::syntax::VariableKind::Var)
        ));
        assert_eq!(model.resolved_reference_count(), 1);
    }

    #[test]
    fn a_var_in_a_nested_block_binds_before_its_declaration() {
        let inner = variable_kind(
            crate::syntax::VariableKind::Var,
            40,
            "var a = 1;",
            "a",
            9,
            Some(number_expr(50, "1", 13)),
        );
        let statements = vec![
            expression_statement(30, identifier_expr(31, "a", 0)),
            block_statement(60, vec![inner]),
        ];
        let result = check(&file("a; { var a = 1; }", statements));
        assert!(semantic_codes(&result).is_empty());
        assert_eq!(result.product().resolved_reference_count(), 1);
    }

    #[test]
    fn a_for_initializer_var_hoists_to_the_module() {
        let for_stmt = Node::new(
            NodeId::new(60),
            range(0, 1),
            Statement::For(crate::syntax::ForStatement {
                initializer: Some(crate::syntax::ForInitializer::Variable(var_declaration(
                    crate::syntax::VariableKind::Var,
                    40,
                    "i",
                    9,
                    Some(number_expr(50, "0", 13)),
                ))),
                test: None,
                update: None,
                body: Box::new(block_statement(80, vec![])),
            }),
        );
        let statements = vec![
            for_stmt,
            expression_statement(90, identifier_expr(91, "i", 23)),
        ];
        let result = check(&file("for (var i = 0; ; ) {} i;", statements));
        assert!(semantic_codes(&result).is_empty());
        let model = result.product();
        assert!(
            model.lookup_value(model.module_scope(), "i").is_some(),
            "for-initializer var hoists out of the for scope"
        );
        assert_eq!(model.resolved_reference_count(), 1);
    }

    #[test]
    fn a_var_in_a_nested_function_does_not_escape_to_the_outer_scope() {
        let inner = variable_kind(
            crate::syntax::VariableKind::Var,
            40,
            "var x = 1;",
            "x",
            15,
            Some(number_expr(50, "1", 19)),
        );
        let body = Node::new(
            NodeId::new(70),
            range(0, 1),
            Block {
                statements: vec![inner],
            },
        );
        let function = crate::syntax::FunctionLike {
            decorators: Vec::new(),
            name: Some(identifier(81, "f", 9)),
            is_async: false,
            is_generator: false,
            type_parameters: None,
            parameters: Vec::new(),
            return_type: None,
            body: Some(FunctionBody::Block(body)),
        };
        let fn_stmt = Node::new(
            NodeId::new(80),
            range(0, 1),
            Statement::Function(crate::syntax::FunctionDeclaration { function }),
        );
        let result = check(&file("function f() { var x = 1; }", vec![fn_stmt]));
        assert!(semantic_codes(&result).is_empty());
        let model = result.product();
        assert!(
            model.lookup_value(model.module_scope(), "f").is_some(),
            "the function declaration binds at the module scope"
        );
        assert!(
            model.lookup_value(model.module_scope(), "x").is_none(),
            "the inner var stays inside its own function scope"
        );
    }

    #[test]
    fn a_function_declaration_in_a_block_hoists_to_the_module() {
        let function = crate::syntax::FunctionLike {
            decorators: Vec::new(),
            name: Some(identifier(81, "g", 14)),
            is_async: false,
            is_generator: false,
            type_parameters: None,
            parameters: Vec::new(),
            return_type: None,
            body: Some(FunctionBody::Block(Node::new(
                NodeId::new(82),
                range(15, 17),
                Block {
                    statements: Vec::new(),
                },
            ))),
        };
        let declaration = Node::new(
            NodeId::new(80),
            range(2, 17),
            Statement::Function(crate::syntax::FunctionDeclaration { function }),
        );
        let statements = vec![
            expression_statement(70, identifier_expr(71, "g", 0)),
            block_statement(90, vec![declaration]),
        ];
        let result = check(&file("g; { function g() {} }", statements));
        assert!(semantic_codes(&result).is_empty());
        let model = result.product();
        assert!(
            model.lookup_value(model.module_scope(), "g").is_some(),
            "block function declaration hoists to the module scope"
        );
        assert_eq!(model.resolved_reference_count(), 1);
    }

    #[test]
    fn a_let_in_a_block_does_not_hoist_and_is_unresolved_outside() {
        let inner = variable_kind(
            crate::syntax::VariableKind::Let,
            40,
            "let b = 1;",
            "b",
            6,
            Some(number_expr(50, "1", 10)),
        );
        let block = block_statement(60, vec![inner]);
        let statements = vec![
            block,
            expression_statement(70, identifier_expr(71, "b", 15)),
        ];
        let result = check(&file("{ let b = 1; } b;", statements));
        assert_eq!(semantic_codes(&result), [CANNOT_FIND_NAME.as_str()]);
        let model = result.product();
        assert!(
            model.lookup_value(model.module_scope(), "b").is_none(),
            "let stays block-scoped and never reaches the module scope"
        );
    }
}
