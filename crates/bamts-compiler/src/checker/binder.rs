//! Binder for one parsed [`SourceFile`]: scope-tree construction, symbol-table
//! population, and [`TypeTable`] interning behind the non-mutating
//! `bind_source` / `bind_source_with_environment` contract consumed by
//! `crate::checker`.
//!
//! The [`Binder`] walks declarations, hoists block-scoped bindings, resolves
//! references against the two-namespace scope tree, and freezes the product
//! into an immutable [`SemanticModel`] with canonical diagnostics.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

use bamts_bytecode::EcmaString;

use super::AnalysisFacts;
use super::ProgramCheckOptions;
use super::inference::{
    InferenceContext, InferenceParameter, InferenceProvenance, InferredTypeArgument,
    InferredTypeArguments,
};
use super::intrinsic_environment::GlobalEnvironment;
use super::jsx::JsxCallable;
use super::relations::{TypeRelation, TypeRelations};
use super::{
    ACCESSOR_THIS_PARAMETER, AMBIENT_IMPLEMENTATION, ARGUMENT_COUNT_MISMATCH,
    ARGUMENT_NOT_ASSIGNABLE, ASSIGNMENT_TO_FUNCTION, ASSIGNMENT_TO_NAMESPACE,
    AWAIT_USING_DECLARATION_IN_FOR_IN, BARE_SUPER_EXPRESSION, CANNOT_FIND_NAME,
    CANNOT_FIND_NAMESPACE, CANNOT_FIND_TYPE, CONSTRUCTOR_DECORATOR_NOT_SUPPORTED,
    CONSTRUCTOR_TYPE_PARAMETERS, DUPLICATE_DECLARATION, EXPRESSION_NOT_CALLABLE,
    FOR_IN_LEFT_HAND_SIDE_INVALID, FUNCTION_DECLARATION_IN_BLOCK_ES5_STRICT,
    FUNCTION_IMPLEMENTATION_WRONG_NAME, FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION,
    GET_ACCESSOR_NO_RETURN, GET_ACCESSOR_PARAMETERS, IMPORT_CONFLICTS_WITH_LOCAL,
    INVALID_ASSIGNMENT_TARGET, MISSING_METHOD_RETURN_TYPE, MIXED_EXPORT_ASSIGNMENT,
    NEW_TARGET_OUTSIDE_FUNCTION, PARAMETER_DECORATOR_NOT_SUPPORTED, PROPERTY_DOES_NOT_EXIST,
    PROPERTY_NOT_INITIALIZED, SET_ACCESSOR_PARAMETER_INITIALIZER,
    STATEMENT_NOT_ALLOWED_IN_AMBIENT_CONTEXT, SUPER_CALL_IN_CONSTRUCTOR_ARGUMENTS,
    SUPER_CALL_OUTSIDE_CONSTRUCTOR, SUPER_REFERENCE_NON_DERIVED, TYPE_NOT_ASSIGNABLE,
    USED_BEFORE_ASSIGNED, USING_DECLARATION_BINDING_PATTERN, USING_DECLARATION_IN_FOR_IN,
    USING_DECLARATION_MISSING_INITIALIZER, WITH_STATEMENT_NOT_ALLOWED,
};
use super::{
    ACCESSOR_THIS_PARAMETER_MESSAGE, AMBIENT_IMPLEMENTATION_MESSAGE,
    ARGUMENT_COUNT_MISMATCH_MESSAGE, ARGUMENT_NOT_ASSIGNABLE_MESSAGE,
    ASSIGNMENT_TO_FUNCTION_MESSAGE, ASSIGNMENT_TO_NAMESPACE_MESSAGE,
    AWAIT_USING_DECLARATION_IN_FOR_IN_MESSAGE, BARE_SUPER_EXPRESSION_MESSAGE,
    CANNOT_FIND_NAME_MESSAGE, CANNOT_FIND_NAMESPACE_MESSAGE, CANNOT_FIND_TYPE_MESSAGE,
    CONSTRUCTOR_DECORATOR_NOT_SUPPORTED_MESSAGE, CONSTRUCTOR_TYPE_PARAMETERS_MESSAGE,
    DUPLICATE_MESSAGE, EXPRESSION_NOT_CALLABLE_MESSAGE, FOR_IN_LEFT_HAND_SIDE_INVALID_MESSAGE,
    FUNCTION_DECLARATION_IN_BLOCK_ES5_STRICT_MESSAGE, FUNCTION_IMPLEMENTATION_WRONG_NAME_MESSAGE,
    FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION_MESSAGE, GET_ACCESSOR_NO_RETURN_MESSAGE,
    GET_ACCESSOR_PARAMETERS_MESSAGE, IMPORT_CONFLICTS_WITH_LOCAL_MESSAGE,
    INVALID_ASSIGNMENT_TARGET_MESSAGE, MISSING_METHOD_RETURN_TYPE_MESSAGE,
    MIXED_EXPORT_ASSIGNMENT_MESSAGE, NEW_TARGET_OUTSIDE_FUNCTION_MESSAGE, NOT_ASSIGNABLE_MESSAGE,
    PARAMETER_DECORATOR_NOT_SUPPORTED_MESSAGE, PROPERTY_DOES_NOT_EXIST_MESSAGE,
    PROPERTY_NOT_INITIALIZED_MESSAGE, SET_ACCESSOR_PARAMETER_INITIALIZER_MESSAGE,
    STATEMENT_NOT_ALLOWED_IN_AMBIENT_CONTEXT_MESSAGE, SUPER_CALL_IN_CONSTRUCTOR_ARGUMENTS_MESSAGE,
    SUPER_CALL_OUTSIDE_CONSTRUCTOR_MESSAGE, SUPER_REFERENCE_NON_DERIVED_MESSAGE,
    USED_BEFORE_ASSIGNED_MESSAGE, USING_DECLARATION_BINDING_PATTERN_MESSAGE,
    USING_DECLARATION_IN_FOR_IN_MESSAGE, USING_DECLARATION_MISSING_INITIALIZER_MESSAGE,
    WITH_STATEMENT_NOT_ALLOWED_MESSAGE,
};
use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::enum_plan::{self, EnumDeclarationBinding, EnumFacts};
use crate::literal::string_value;
use crate::namespace_plan::{self, NamespaceDeclarationBinding, NamespaceFacts};
use crate::source::{ScriptKind, TextRange};
use crate::syntax::{
    ArrayElement, ArrowFunction, AssignmentOperator, AssignmentTarget, BindingPattern,
    CallArgument, CallExpression, ClassDeclaration, ClassMember, EntityName, Expr, Expression,
    ForBinding, ForInitializer, FunctionBody, FunctionLike, FunctionType, IdentifierNode,
    ImportBinding, InterfaceDeclaration, KeywordType, Literal, MemberProperty, MetaProperty,
    NamespaceName, NodeId, ObjectMember, ParameterNode, PropertyModifier, PropertyName, SourceFile,
    Statement, Stmt, Token, TokenKind, Ty, TypeAliasDeclaration, TypeAnnotationNode, TypeLiteral,
    TypeMember, TypeNode, TypeReference, UnaryOperator, VariableDeclaration, VariableKind,
};

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
    Namespace,
    /// Marks the body of a sloppy-mode `with` statement. Unresolved value
    /// references inside this scope may bind to the runtime object instead.
    With,
}

/// One lexical scope with its two-namespace symbol tables and strict-mode bit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scope {
    kind: ScopeKind,
    parent: Option<ScopeId>,
    values: BTreeMap<String, SymbolId>,
    types: BTreeMap<String, SymbolId>,
    strict: bool,
    /// The container symbol whose members this scope collects — enum member
    /// scopes and class scopes only; namespace export scopes intentionally
    /// have no owner, since upstream renders export declarations bare.
    owner: Option<SymbolId>,
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

    /// Returns the container symbol whose member declarations this scope
    /// collects, when this is a member scope (enum body, class body).
    #[must_use]
    pub const fn owner(&self) -> Option<SymbolId> {
        self.owner
    }

    /// Returns whether this scope executes in ECMAScript strict mode.
    #[must_use]
    pub const fn is_strict(&self) -> bool {
        self.strict
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
    EnumMember,
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
                | Self::EnumMember
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
                | Self::Namespace
        )
    }

    /// Returns whether an existing value binding of `self` accepts a merge from
    /// a new declaration of `new`. Namespace merges with function/class/enum are
    /// order-aware: the function/class/enum must already exist.
    const fn accepts_value_merge_from(self, new: Self) -> bool {
        matches!(
            (self, new),
            (
                Self::Variable(VariableKind::Var) | Self::Function,
                Self::Variable(VariableKind::Var) | Self::Function
            ) | (Self::Enum, Self::Enum)
                | (Self::Namespace, Self::Namespace)
                | (Self::Function | Self::Class | Self::Enum, Self::Namespace)
        )
    }

    /// Returns whether an existing type binding of `self` accepts a merge from
    /// a new declaration of `new`. Class/enum + namespace merges are order-aware;
    /// interface + namespace remains bidirectional.
    const fn accepts_type_merge_from(self, new: Self) -> bool {
        matches!(
            (self, new),
            (Self::Interface, Self::Interface)
                | (Self::Enum, Self::Enum)
                | (Self::Namespace, Self::Namespace)
                | (Self::Interface, Self::Namespace)
                | (Self::Namespace, Self::Interface)
                | (Self::Class | Self::Enum, Self::Namespace)
                | (Self::Import, Self::Interface | Self::TypeAlias)
                | (Self::Interface | Self::TypeAlias, Self::Import)
        )
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
    /// The owning container symbol, set when this symbol is declared into an
    /// owned member scope (enum body, class body). Drives `qualified_name`.
    parent: Option<SymbolId>,
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

    /// Returns the owning container symbol set at declaration time when this
    /// symbol was declared into an owned member scope (enum body, class body).
    #[must_use]
    pub const fn parent(&self) -> Option<SymbolId> {
        self.parent
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct HoistedDeclarationIdentity {
    scope: ScopeId,
    declaration: NodeId,
    range: TextRange,
    kind: SymbolKind,
}

/// One member of an interned object type.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PropertyType {
    name: Box<str>,
    optional: bool,
    readonly: bool,
    type_id: TypeId,
}

impl PropertyType {
    #[must_use]
    pub fn new(name: impl Into<Box<str>>, optional: bool, type_id: TypeId) -> Self {
        Self {
            name: name.into(),
            optional,
            readonly: false,
            type_id,
        }
    }

    #[must_use]
    pub fn with_readonly(mut self, readonly: bool) -> Self {
        self.readonly = readonly;
        self
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
    pub const fn readonly(&self) -> bool {
        self.readonly
    }

    #[must_use]
    pub const fn type_id(&self) -> TypeId {
        self.type_id
    }
}
/// One parameter of an interned function signature.
#[derive(Clone, Debug)]
pub struct FunctionParameter {
    name: String,
    type_id: TypeId,
    optional: bool,
    rest: bool,
}

impl PartialEq for FunctionParameter {
    fn eq(&self, other: &Self) -> bool {
        self.type_id == other.type_id && self.optional == other.optional && self.rest == other.rest
    }
}

impl Eq for FunctionParameter {}

impl std::hash::Hash for FunctionParameter {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.type_id.hash(state);
        self.optional.hash(state);
        self.rest.hash(state);
    }
}

impl FunctionParameter {
    #[must_use]
    pub fn new(name: String, type_id: TypeId, optional: bool, rest: bool) -> Self {
        Self {
            name,
            type_id,
            optional,
            rest,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn type_id(&self) -> TypeId {
        self.type_id
    }

    #[must_use]
    pub const fn optional(&self) -> bool {
        self.optional
    }

    #[must_use]
    pub const fn rest(&self) -> bool {
        self.rest
    }
}

/// One interned function signature.
#[derive(Clone, Debug)]
pub struct FunctionSignature {
    type_parameters: Vec<SymbolId>,
    parameters: Vec<FunctionParameter>,
    return_type: TypeId,
}

impl PartialEq for FunctionSignature {
    fn eq(&self, other: &Self) -> bool {
        self.type_parameters == other.type_parameters
            && self.parameters == other.parameters
            && self.return_type == other.return_type
    }
}

impl Eq for FunctionSignature {}

impl std::hash::Hash for FunctionSignature {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.type_parameters.hash(state);
        self.parameters.hash(state);
        self.return_type.hash(state);
    }
}

impl FunctionSignature {
    #[must_use]
    pub fn type_parameters(&self) -> &[SymbolId] {
        &self.type_parameters
    }

    #[must_use]
    pub fn parameters(&self) -> &[FunctionParameter] {
        &self.parameters
    }

    #[must_use]
    pub const fn return_type(&self) -> TypeId {
        self.return_type
    }

    /// Returns `(required, total, rest_index)` for this signature.
    /// `total` is `usize::MAX` when the signature ends in a rest parameter.
    #[must_use]
    pub fn arity(&self) -> (usize, usize, Option<usize>) {
        let mut required = 0;
        let mut rest_index = None;
        for (index, parameter) in self.parameters.iter().enumerate() {
            if parameter.rest() {
                rest_index = Some(index);
                break;
            }
            if parameter.optional() {
                break;
            }
            required += 1;
        }
        let total = if rest_index.is_some() {
            usize::MAX
        } else {
            self.parameters.len()
        };
        (required, total, rest_index)
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
    object_symbol: Option<SymbolId>,
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
            object_symbol: None,
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
    /// Returns the type of a named property on a structural type, distributing
    /// over unions. `None` means the property is not present on the type.
    pub fn property_type(&mut self, ty: TypeId, name: &str) -> Option<TypeId> {
        match self.get(ty).clone() {
            Type::ObjectType(properties) => properties
                .iter()
                .find(|property| property.name() == name)
                .map(|property| property.type_id()),
            Type::Union(members) => {
                let mut found = Vec::with_capacity(members.len());
                for member in members {
                    found.push(self.property_type(member, name)?);
                }
                Some(self.union(&found))
            }
            _ => None,
        }
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

    #[must_use]
    pub const fn object_symbol(&self) -> Option<SymbolId> {
        self.object_symbol
    }

    #[must_use]
    pub fn is_object_symbol(&self, symbol: SymbolId) -> bool {
        self.object_symbol == Some(symbol)
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

    /// Interns a function type from bare parameter types.
    pub fn function(&mut self, parameters: Vec<TypeId>, return_type: TypeId) -> TypeId {
        let parameters = parameters
            .into_iter()
            .enumerate()
            .map(|(index, type_id)| {
                FunctionParameter::new(format!("arg{index}"), type_id, false, false)
            })
            .collect();
        self.function_with_parameters(Vec::new(), parameters, return_type)
    }

    /// Interns a function type with full per-parameter metadata.
    pub fn function_with_parameters(
        &mut self,
        type_parameters: Vec<SymbolId>,
        parameters: Vec<FunctionParameter>,
        return_type: TypeId,
    ) -> TypeId {
        self.intern(Type::Function(FunctionSignature {
            type_parameters,
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
    ///
    /// The relation algebra lives in [`super::relations`]; this delegates
    /// through a short-lived [`TypeRelations`] so existing callers keep
    /// working. Relation-heavy passes should share one `TypeRelations`
    /// instead, so memoized pairs amortize across queries.
    #[must_use]
    pub fn assignable(&self, source: TypeId, target: TypeId) -> bool {
        TypeRelations::new(self).assignable(source, target)
    }

    /// Returns whether a value of `source` may be assigned where `target` is
    /// expected with `strictNullChecks` enabled.
    #[must_use]
    pub fn assignable_with_strict_null(&self, source: TypeId, target: TypeId) -> bool {
        TypeRelations::new(self).assignable_with_strict_null(source, target)
    }
    /// Widens literal types to their base types so mutable variables can be
    /// reassigned. When `keep_primitive_literals` is `true`, primitive
    /// literals (`1`, `"foo"`, `true`, `1n`) are kept as-is; their containers
    pub fn widen(&mut self, type_id: TypeId, keep_primitive_literals: bool) -> TypeId {
        let ty = self.get(type_id).clone();
        match ty {
            Type::StringLiteral(_) if !keep_primitive_literals => self.string(),
            Type::NumberLiteral(_) if !keep_primitive_literals => self.number(),
            Type::BooleanLiteral(_) if !keep_primitive_literals => self.boolean(),
            Type::BigIntLiteral(_) if !keep_primitive_literals => self.bigint(),
            // Composite types are widened as a whole: a `const a = [1]` has type
            // `number[]`, and `const o = { x: 1 }` has type `{ x: number; }`.
            // Literal assertions (`as const`) preserve the top-level expression;
            // the `widen` call for those still keeps top-level primitive literals.
            Type::Array(element) => {
                let widened = self.widen(element, false);
                self.array(widened)
            }
            Type::ObjectType(properties) => {
                let widened: Vec<_> = properties
                    .iter()
                    .map(|property| {
                        PropertyType::new(
                            property.name.clone(),
                            property.optional,
                            self.widen(property.type_id, false),
                        )
                        .with_readonly(property.readonly)
                    })
                    .collect();
                self.object_type(widened)
            }
            Type::Union(members) => {
                let widened: Vec<_> = members
                    .iter()
                    .map(|member| self.widen(*member, false))
                    .collect();
                self.union(&widened)
            }
            _ => type_id,
        }
    }

    /// Computes compatibility once while retaining every accepted unsound
    pub fn relation(&self, source: TypeId, target: TypeId) -> TypeRelation {
        TypeRelations::new(self).relation(source, target)
    }
}

/// Lazy resolution state for a type-declaring symbol.
#[derive(Clone, Copy)]
enum TypeState {
    Unresolved,
    InProgress,
    Done(TypeId),
}

enum EntityNameScopeError {
    Unresolved,
    MissingMember(TextRange),
    NotNamespace,
}

/// Separate value and type targets for an `import =` alias when a name occupies
/// both planes. Runtime/value lowering uses `value.or(ty)`; type resolution uses
/// `ty.or(value)`.
#[derive(Clone, Copy, Debug, Default)]
struct ImportEqualsTarget {
    value: Option<SymbolId>,
    ty: Option<SymbolId>,
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

pub(crate) fn source_is_module(source: &SourceFile) -> bool {
    source.statements().iter().any(|statement| {
        matches!(
            statement.data(),
            Statement::Import(_) | Statement::Export(_) | Statement::ImportEquals(_)
        )
    })
}

/// Returns whether the directive prologue of a statement list contains
/// a `"use strict"` directive before any non-directive statement.
fn directive_prologue_is_strict(source: &SourceFile, statements: &[Stmt]) -> bool {
    for statement in statements {
        if let Statement::Expression(expression) = statement.data()
            && let Expression::Literal(Literal::String(literal)) = expression.expression.data()
        {
            let value = source
                .token_text(literal.data().token())
                .map(|text| text.trim_matches(['\'', '"']))
                .unwrap_or("");
            if value == "use strict" {
                return true;
            }
            continue;
        }
        break;
    }
    false
}

/// Returns whether an enum initializer is a numeric literal expression.
pub(crate) fn is_numeric_enum_initializer(expression: &Expr) -> bool {
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

/// The immutable product of semantic analysis.
#[derive(Clone, Debug)]
pub struct SemanticModel {
    scopes: Vec<Scope>,
    symbols: Vec<Symbol>,
    symbol_types: Vec<TypeId>,
    references: HashMap<NodeId, SymbolId>,
    reference_aliases: HashMap<NodeId, SymbolId>,
    type_nodes: HashMap<NodeId, TypeId>,
    /// Types recorded for expression nodes by the expression-typing walk
    /// (U2.8 S2 types facet), distinct from `type_nodes`, which holds
    /// type-annotation node identities.
    node_types: HashMap<NodeId, TypeId>,
    /// Expression `(range, type)` records in first-seen order, feeding the S2
    /// `.types` baseline emitter.
    typed_expressions: Vec<(TextRange, TypeId)>,
    /// Resolved name occurrences (value and type references) with their source
    /// ranges, in first-seen order, feeding the S2 `.symbols` baseline emitter.
    /// This is the range-indexed projection of `references`, which is keyed by
    /// node identity and so cannot itself place records against source lines.
    symbol_references: Vec<(TextRange, SymbolId)>,
    types: TypeTable,
    module_scope: ScopeId,
    facts: AnalysisFacts,
    pub(crate) enum_facts: EnumFacts,
    namespace_facts: NamespaceFacts,
    ambient_modules: HashMap<String, SymbolId>,
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

    /// Returns the symbol's qualified display name for baseline rendering: the
    /// dot-joined chain of container owners (enum/class symbols owning the
    /// member scope the symbol was declared in) ending in its own name.
    /// Symbols without an owner chain — top-level declarations, namespace
    /// exports, locals, parameters, type parameters — render bare, matching the
    /// upstream `.symbols` contract.
    #[must_use]
    pub fn qualified_name(&self, id: SymbolId) -> String {
        let symbol = self.symbol(id);
        match symbol.parent {
            Some(parent) => format!("{}.{}", self.qualified_name(parent), symbol.name),
            None => symbol.name.clone(),
        }
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

    /// Returns the checker's canonical identity for a resolved type node.
    #[must_use]
    pub fn resolved_type(&self, node: NodeId) -> Option<TypeId> {
        self.type_nodes.get(&node).copied()
    }

    /// Returns the type the checker computed for an expression node, when the
    /// expression-typing walk recorded one (U2.8 S2 types facet).
    #[must_use]
    pub fn node_type(&self, node: NodeId) -> Option<TypeId> {
        self.node_types.get(&node).copied()
    }

    /// Returns every recorded expression `(range, type)` in first-seen order.
    #[must_use]
    pub fn typed_expressions(&self) -> &[(TextRange, TypeId)] {
        &self.typed_expressions
    }

    /// Returns every resolved reference occurrence `(range, symbol)` in
    /// first-seen order, feeding the S2 `.symbols` baseline emitter.
    #[must_use]
    pub fn symbol_references(&self) -> &[(TextRange, SymbolId)] {
        &self.symbol_references
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

    /// Returns immutable enum semantics built by this checker pass.
    #[must_use]
    pub(crate) const fn enum_facts(&self) -> &EnumFacts {
        &self.enum_facts
    }

    /// Returns immutable namespace semantics built by this checker pass.
    #[must_use]
    pub(crate) const fn namespace_facts(&self) -> &NamespaceFacts {
        &self.namespace_facts
    }

    /// Returns the ambient string-module registry for this file.
    #[must_use]
    pub fn ambient_modules(&self) -> &HashMap<String, SymbolId> {
        &self.ambient_modules
    }

    /// Iterates resolved syntax references by their source identity.
    pub(crate) fn references(&self) -> impl Iterator<Item = (NodeId, SymbolId)> + '_ {
        self.references
            .iter()
            .map(|(&node, &symbol)| (node, symbol))
    }

    /// Returns the symbol a reference resolved to, accepting either the
    /// identifier node or its enclosing expression/assignment-target node.
    #[must_use]
    pub fn reference(&self, node: NodeId) -> Option<SymbolId> {
        self.references
            .get(&node)
            .or_else(|| self.reference_aliases.get(&node))
            .copied()
    }

    /// Returns how many syntax references resolved to a local binding.
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

/// The innermost function context a `super(...)` call can legally appear in.
///
/// TypeScript permits a `super(...)` call only as a direct expression of a
/// derived class constructor body. Every other position (a base-class
/// constructor, constructor parameter initializers, or any non-constructor
/// function) maps to a distinct TypeScript diagnostic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SuperCallContext {
    DerivedConstructor,
    BaseConstructor,
    ConstructorParameters { derived: bool },
    NonConstructor,
}

pub(crate) struct Binder<'src> {
    pub(crate) source: &'src SourceFile,
    intrinsics: GlobalEnvironment,
    pub(crate) scopes: Vec<Scope>,
    pub(crate) symbols: Vec<Symbol>,
    pub(crate) symbol_types: Vec<TypeId>,
    type_state: Vec<TypeState>,
    type_defs: HashMap<SymbolId, TypeDef<'src>>,
    pub(crate) references: HashMap<NodeId, SymbolId>,
    reference_aliases: HashMap<NodeId, SymbolId>,
    type_nodes: HashMap<NodeId, TypeId>,
    node_types: HashMap<NodeId, TypeId>,
    typed_expressions: Vec<(TextRange, TypeId)>,
    symbol_references: Vec<(TextRange, SymbolId)>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) types: TypeTable,
    module_scope: ScopeId,
    global_scope: ScopeId,
    ambient_modules: HashMap<String, SymbolId>,
    enum_declarations: Vec<EnumDeclarationBinding<'src>>,
    enum_declaration_symbols: HashMap<NodeId, SymbolId>,
    enum_member_scopes: HashMap<SymbolId, ScopeId>,
    enum_member_symbols: HashMap<NodeId, SymbolId>,
    enum_member_names: HashMap<NodeId, EcmaString>,
    enum_member_symbols_by_name: HashMap<SymbolId, HashMap<EcmaString, SymbolId>>,
    enum_member_identifier_uses: HashSet<NodeId>,
    imported_enum_member_uses: HashMap<NodeId, enum_plan::ImportedEnumMemberUse>,
    local_enum_member_targets: HashMap<NodeId, SymbolId>,
    imported_enum_member_targets: HashSet<NodeId>,
    pub(crate) namespace_declarations: Vec<NamespaceDeclarationBinding<'src>>,
    namespace_export_scopes: HashMap<SymbolId, ScopeId>,
    pub(crate) namespace_local_scopes: HashMap<NodeId, ScopeId>,
    active_namespace_declarations: Vec<NodeId>,
    namespace_reference_blocks: HashMap<NodeId, NodeId>,
    namespace_qualified_type_paths: HashMap<NodeId, Box<[SymbolId]>>,
    import_equals_symbols: HashMap<NodeId, SymbolId>,
    qualified_import_paths: HashMap<NodeId, Box<[SymbolId]>>,
    import_equals_targets: HashMap<SymbolId, ImportEqualsTarget>,
    hoisted_declaration_symbols: HashMap<HoistedDeclarationIdentity, SymbolId>,
    /// JSX expression node → checked element result type, recorded by
    /// [`super::jsx`] during expression resolution.
    pub(crate) jsx_element_types: HashMap<NodeId, TypeId>,
    /// Callable declarations (function declarations, function/arrow
    /// initializers) by symbol, so [`super::jsx`] can factory-check
    /// value-based JSX elements whose symbol type stays `any`.
    pub(crate) jsx_callables: HashMap<SymbolId, JsxCallable<'src>>,
    /// Class instance structural types keyed by the class symbol, built lazily
    /// during class-body resolution so `new C()` and member access on class-typed
    /// values can resolve declared instance members.
    pub(crate) class_instance_types: HashMap<SymbolId, TypeId>,
    /// Enclosing function contexts for `super(...)` call legality, innermost
    /// last. Empty means top level, which behaves as
    /// [`SuperCallContext::NonConstructor`].
    super_call_contexts: Vec<SuperCallContext>,
    /// Whether each lexically enclosing class has a base class, innermost last.
    class_derived_stack: Vec<bool>,
    /// Enclosing `new.target` contexts: `true` when the innermost enclosing
    /// body is a function declaration, function expression, or constructor.
    /// `false` when it is a method, getter, setter, or static block.
    new_target_contexts: Vec<bool>,
    /// Enclosing `declare` contexts. `true` when the current statement is
    /// directly under a `declare` keyword.
    ambient_stack: Vec<bool>,
    /// `strictNullChecks` compiler option: `null` and `undefined` are only
    /// assignable to types that explicitly include them.
    strict_null_checks: bool,
    /// `noImplicitAny` compiler option: missing return types in method signatures
    /// are reported as implicit `any`.
    no_implicit_any: bool,
    /// Whether the compilation target is ES5 or earlier.
    es5: bool,
    /// Variable symbols declared without an initializer and not yet assigned.
    uninitialized_variables: HashSet<SymbolId>,
    /// Leaf identifier symbols bound by each declarator. Stored during binding
    /// so resolution can remove initialized variables from `uninitialized_variables`.
    declarator_symbols: HashMap<NodeId, Vec<SymbolId>>,
    /// Suppress BAMTS-C038 for identifiers in unreachable conditional branches.
    /// Resolution still proceeds so cannot-find-name errors in dead code are not lost.
    suppress_used_before_assigned: bool,
    /// Accumulated return expression types per function body node, used to
    /// infer function and getter return types when no annotation is present.
    return_types: HashMap<NodeId, Vec<TypeId>>,
    /// Stack of function body node ids currently being resolved, innermost last.
    function_body_stack: Vec<NodeId>,
    /// Enclosing `this` types for `this` expressions, innermost last.
    this_context: Vec<TypeId>,
    /// Whether the current binding context is inside a `declare` directive.
    /// Ambient declarations are not uninitialized.
    ambient_binding: bool,
    /// Whether the source file is a `.d.ts`/`.d.mts`/`.d.cts` declaration file.
    /// Top-level statements in such files must be declarations.
    is_declaration_file: bool,
}

impl<'src> Binder<'src> {
    pub(crate) fn new(source: &'src SourceFile) -> Self {
        Self::with_environment(
            source,
            GlobalEnvironment::standard(),
            source_is_module(source),
            ProgramCheckOptions::standard(),
        )
    }

    pub(crate) fn with_environment(
        source: &'src SourceFile,
        intrinsics: GlobalEnvironment,
        is_module: bool,
        options: ProgramCheckOptions,
    ) -> Self {
        let mut checker = Self {
            source,
            intrinsics,
            scopes: Vec::new(),
            symbols: Vec::new(),
            symbol_types: Vec::new(),
            type_state: Vec::new(),
            type_defs: HashMap::new(),
            references: HashMap::new(),
            reference_aliases: HashMap::new(),
            type_nodes: HashMap::new(),
            node_types: HashMap::new(),
            typed_expressions: Vec::new(),
            symbol_references: Vec::new(),
            diagnostics: Vec::new(),
            types: TypeTable::new(),
            module_scope: ScopeId(0),
            global_scope: ScopeId(0),
            ambient_modules: HashMap::new(),
            enum_declarations: Vec::new(),
            enum_declaration_symbols: HashMap::new(),
            enum_member_scopes: HashMap::new(),
            enum_member_symbols: HashMap::new(),
            enum_member_names: HashMap::new(),
            enum_member_symbols_by_name: HashMap::new(),
            enum_member_identifier_uses: HashSet::new(),
            imported_enum_member_uses: HashMap::new(),
            local_enum_member_targets: HashMap::new(),
            imported_enum_member_targets: HashSet::new(),
            namespace_declarations: Vec::new(),
            namespace_export_scopes: HashMap::new(),
            namespace_local_scopes: HashMap::new(),
            active_namespace_declarations: Vec::new(),
            namespace_reference_blocks: HashMap::new(),
            namespace_qualified_type_paths: HashMap::new(),
            import_equals_symbols: HashMap::new(),
            qualified_import_paths: HashMap::new(),
            import_equals_targets: HashMap::new(),
            hoisted_declaration_symbols: HashMap::new(),
            jsx_element_types: HashMap::new(),
            jsx_callables: HashMap::new(),
            class_instance_types: HashMap::new(),
            super_call_contexts: Vec::new(),
            class_derived_stack: Vec::new(),
            new_target_contexts: Vec::new(),
            ambient_stack: Vec::new(),
            strict_null_checks: options.strict_null_checks(),
            no_implicit_any: options.no_implicit_any(),
            es5: options.es5(),
            uninitialized_variables: HashSet::new(),
            declarator_symbols: HashMap::new(),
            suppress_used_before_assigned: false,
            return_types: HashMap::new(),
            function_body_stack: Vec::new(),
            this_context: Vec::new(),
            ambient_binding: false,
            is_declaration_file: source.source_text().is_declaration_file(),
        };
        let global_scope = checker.new_scope(ScopeKind::Global, None);
        checker.global_scope = global_scope;
        checker.module_scope = checker.new_scope(ScopeKind::Module, Some(global_scope));
        checker.scopes[checker.module_scope.0 as usize].strict = is_module
            || options.always_strict()
            || directive_prologue_is_strict(source, source.statements());
        checker.bind_intrinsic_environment(global_scope);
        checker
    }

    fn types_assignable(&self, source: TypeId, target: TypeId) -> bool {
        if self.strict_null_checks {
            self.types.assignable_with_strict_null(source, target)
        } else {
            self.types.assignable(source, target)
        }
    }

    fn is_typescript(&self) -> bool {
        matches!(
            self.source.script_kind(),
            ScriptKind::TypeScript | ScriptKind::TypeScriptReact
        )
    }

    fn bind_intrinsic_environment(&mut self, scope: ScopeId) {
        for name in self
            .intrinsics
            .values()
            .iter()
            .chain(self.intrinsics.module_values())
        {
            let id = self.declare(
                name,
                SymbolKind::IntrinsicValue,
                scope,
                NodeId::default(),
                NodeId::default_range(),
            );
            if *name == "undefined" {
                self.symbol_types[id.get() as usize] = self.types.undefined_type();
            }
        }
        for name in self.intrinsics.types() {
            let id = self.declare(
                name,
                SymbolKind::IntrinsicType,
                scope,
                NodeId::default(),
                NodeId::default_range(),
            );
            if *name == "Object" {
                self.types.object_symbol = Some(id);
            }
        }
    }

    pub(crate) fn run(&mut self) {
        let statements = self.source.statements();
        let scope = self.module_scope;
        if self.is_declaration_file {
            for statement in statements {
                if !Self::is_declaration_statement(statement.data()) {
                    self.emit(
                        STATEMENT_NOT_ALLOWED_IN_AMBIENT_CONTEXT,
                        statement.range(),
                        STATEMENT_NOT_ALLOWED_IN_AMBIENT_CONTEXT_MESSAGE,
                    );
                }
            }
        }
        self.bind_statements(statements, scope);
        self.bind_hoisted_statements(statements, scope);
        self.build_import_equals_targets(statements, scope);
        self.resolve_statements(statements, scope);
        self.check_export_assignment_conflicts();
    }

    fn is_declaration_statement(statement: &Statement) -> bool {
        matches!(
            statement,
            Statement::Import(_)
                | Statement::ImportEquals(_)
                | Statement::Export(_)
                | Statement::Variable(_)
                | Statement::Function(_)
                | Statement::Class(_)
                | Statement::Interface(_)
                | Statement::TypeAlias(_)
                | Statement::Enum(_)
                | Statement::Namespace(_)
                | Statement::Declare(_)
                | Statement::Empty
                | Statement::Missing(_)
        )
    }

    pub(crate) fn finish(mut self) -> (SemanticModel, Vec<Diagnostic>) {
        let enum_declarations = std::mem::take(&mut self.enum_declarations);
        let enum_member_symbols = std::mem::take(&mut self.enum_member_symbols);
        let enum_member_names = std::mem::take(&mut self.enum_member_names);
        let enum_member_identifier_uses = std::mem::take(&mut self.enum_member_identifier_uses);
        let imported_enum_member_uses = std::mem::take(&mut self.imported_enum_member_uses);
        let local_enum_member_targets = std::mem::take(&mut self.local_enum_member_targets);
        let imported_enum_member_targets = std::mem::take(&mut self.imported_enum_member_targets);
        let namespace_declarations = std::mem::take(&mut self.namespace_declarations);
        let namespace_reference_blocks = std::mem::take(&mut self.namespace_reference_blocks);
        let namespace_qualified_type_paths =
            std::mem::take(&mut self.namespace_qualified_type_paths);
        let qualified_import_paths = std::mem::take(&mut self.qualified_import_paths);
        let mut model = SemanticModel {
            scopes: self.scopes,
            symbols: self.symbols,
            symbol_types: self.symbol_types,
            references: self.references,
            reference_aliases: self.reference_aliases,
            type_nodes: self.type_nodes,
            node_types: self.node_types,
            typed_expressions: self.typed_expressions,
            symbol_references: self.symbol_references,
            types: self.types,
            module_scope: self.module_scope,
            facts: AnalysisFacts::default(),
            enum_facts: EnumFacts::unchecked(),
            namespace_facts: NamespaceFacts::unchecked(),
            ambient_modules: std::mem::take(&mut self.ambient_modules),
        };
        let (enum_facts, diagnostics) = enum_plan::build(
            &model,
            self.source,
            self.source.source_id(),
            &enum_declarations,
            &enum_member_symbols,
            &enum_member_names,
            &enum_member_identifier_uses,
            &local_enum_member_targets,
            &imported_enum_member_uses,
            &imported_enum_member_targets,
        );
        model.enum_facts = enum_facts;
        let mut namespace_facts = namespace_plan::build(
            &model,
            self.source,
            &namespace_declarations,
            &namespace_reference_blocks,
            namespace_qualified_type_paths,
        );
        namespace_facts.set_qualified_import_paths(qualified_import_paths);
        model.namespace_facts = namespace_facts;
        self.diagnostics.extend(diagnostics);
        (model, self.diagnostics)
    }

    // -- text and scope helpers ------------------------------------------------

    pub(crate) fn text(&self, token: &Token) -> &'src str {
        self.source.token_text(token).unwrap_or("")
    }

    pub(crate) fn identifier_text(&self, identifier: &IdentifierNode) -> Cow<'src, str> {
        self.source
            .identifier_text(identifier.data().token())
            .unwrap_or_default()
    }

    pub(crate) fn new_scope(&mut self, kind: ScopeKind, parent: Option<ScopeId>) -> ScopeId {
        let strict = parent.is_some_and(|parent| self.scopes[parent.0 as usize].strict);
        let id = ScopeId(u32::try_from(self.scopes.len()).expect("scope count fits in u32"));
        self.scopes.push(Scope {
            kind,
            parent,
            values: BTreeMap::new(),
            types: BTreeMap::new(),
            strict,
            owner: None,
        });
        id
    }

    /// Marks `scope` as the member scope owned by `owner`; symbols declared
    /// afterwards inherit it as their qualified-name parent. Idempotent.
    fn set_scope_owner(&mut self, scope: ScopeId, owner: SymbolId) {
        self.scopes[scope.0 as usize].owner = Some(owner);
    }

    /// Walks outward to the nearest function-like, namespace-export, or module
    /// scope. A namespace's per-block locals use `Function`; exported `var`
    /// declarations bind directly in `Namespace`, so neither can escape.
    fn value_hoist_scope(&self, scope: ScopeId) -> ScopeId {
        let mut current = scope;
        loop {
            let node = &self.scopes[current.0 as usize];
            if matches!(
                node.kind,
                ScopeKind::Class | ScopeKind::Function | ScopeKind::Module | ScopeKind::Namespace
            ) {
                return current;
            }
            match node.parent {
                Some(parent) => current = parent,
                None => return current,
            }
        }
    }

    pub(crate) fn emit(&mut self, code: DiagnosticCode, range: TextRange, message: &'static str) {
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
        let hoisted = matches!(
            kind,
            SymbolKind::Variable(VariableKind::Var) | SymbolKind::Function
        );
        let scope = if hoisted {
            self.value_hoist_scope(scope)
        } else {
            scope
        };
        let hoisted_identity = hoisted.then_some(HoistedDeclarationIdentity {
            scope,
            declaration,
            range,
            kind,
        });
        if let Some(identity) = hoisted_identity
            && let Some(symbol) = self.hoisted_declaration_symbols.get(&identity)
        {
            return *symbol;
        }
        let merge = self.scopes[scope.0 as usize]
            .values
            .get(name)
            .copied()
            .filter(|existing| {
                kind.occupies_value()
                    && self.symbols[existing.get() as usize]
                        .kind
                        .accepts_value_merge_from(kind)
            })
            .or_else(|| {
                self.scopes[scope.0 as usize]
                    .types
                    .get(name)
                    .copied()
                    .filter(|existing| {
                        kind.occupies_type()
                            && self.symbols[existing.get() as usize]
                                .kind
                                .accepts_type_merge_from(kind)
                    })
            });
        if let Some(existing) = merge {
            if kind.occupies_value() {
                self.scopes[scope.0 as usize]
                    .values
                    .entry(name.to_owned())
                    .or_insert(existing);
            }
            if kind.occupies_type() {
                self.scopes[scope.0 as usize]
                    .types
                    .entry(name.to_owned())
                    .or_insert(existing);
            }
            if let Some(identity) = hoisted_identity {
                self.hoisted_declaration_symbols.insert(identity, existing);
            }
            return existing;
        }
        let parent = self.scopes[scope.0 as usize].owner;
        let id = SymbolId(u32::try_from(self.symbols.len()).expect("symbol count fits in u32"));
        self.symbols.push(Symbol {
            name: name.to_owned(),
            kind,
            scope,
            declaration,
            range,
            parent,
        });
        self.symbol_types.push(self.types.any());
        self.type_state.push(TypeState::Unresolved);

        let value_conflict = if kind.occupies_value() {
            self.insert_value(scope, name, id, kind)
        } else {
            None
        };
        let type_conflict = if kind.occupies_type() {
            self.insert_type(scope, name, id, kind)
        } else {
            None
        };
        if let Some(identity) = hoisted_identity {
            self.hoisted_declaration_symbols.insert(identity, id);
        }
        let conflict = value_conflict.or(type_conflict);
        if let Some(existing) = conflict {
            let existing_kind = self.symbols[existing.get() as usize].kind;
            if existing_kind == SymbolKind::Import && kind != SymbolKind::Import {
                // The local declaration shadows the import; the diagnostic is on the import.
                let import_range = self.symbols[existing.get() as usize].range;
                self.emit(
                    IMPORT_CONFLICTS_WITH_LOCAL,
                    import_range,
                    IMPORT_CONFLICTS_WITH_LOCAL_MESSAGE,
                );
                self.scopes[scope.0 as usize]
                    .values
                    .insert(name.to_owned(), id);
                self.scopes[scope.0 as usize]
                    .types
                    .insert(name.to_owned(), id);
            } else if kind == SymbolKind::Import {
                // A duplicate import conflicts with an existing local declaration.
                self.emit(
                    IMPORT_CONFLICTS_WITH_LOCAL,
                    range,
                    IMPORT_CONFLICTS_WITH_LOCAL_MESSAGE,
                );
            } else {
                self.emit(DUPLICATE_DECLARATION, range, DUPLICATE_MESSAGE);
            }
        }
        id
    }

    fn insert_value(
        &mut self,
        scope: ScopeId,
        name: &str,
        id: SymbolId,
        kind: SymbolKind,
    ) -> Option<SymbolId> {
        match self.scopes[scope.0 as usize].values.get(name) {
            None => {
                self.scopes[scope.0 as usize]
                    .values
                    .insert(name.to_owned(), id);
                None
            }
            Some(existing) => {
                let existing_kind = self.symbols[existing.get() as usize].kind;
                if existing_kind.accepts_value_merge_from(kind) {
                    None
                } else {
                    Some(*existing)
                }
            }
        }
    }

    fn insert_type(
        &mut self,
        scope: ScopeId,
        name: &str,
        id: SymbolId,
        kind: SymbolKind,
    ) -> Option<SymbolId> {
        match self.scopes[scope.0 as usize].types.get(name) {
            None => {
                self.scopes[scope.0 as usize]
                    .types
                    .insert(name.to_owned(), id);
                None
            }
            Some(existing) => {
                let existing_kind = self.symbols[existing.get() as usize].kind;
                if existing_kind.accepts_type_merge_from(kind) {
                    None
                } else {
                    Some(*existing)
                }
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
                        &self.identifier_text(name),
                        SymbolKind::Function,
                        scope,
                        name.id(),
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
            Statement::With(with_statement) => {
                self.bind_hoisted_statement(&with_statement.body, scope)
            }
            Statement::Labeled(statement) => self.bind_hoisted_statement(&statement.body, scope),
            Statement::Namespace(_) => {}
            Statement::Declare(inner) => {
                let saved = self.ambient_binding;
                self.ambient_binding = true;
                self.bind_hoisted_statement(inner, scope);
                self.ambient_binding = saved;
            }
            Statement::Export(export) => match export {
                crate::syntax::ExportDeclaration::Named(
                    crate::syntax::ExportNamedDeclaration::Declaration(inner),
                ) => self.bind_hoisted_statement(inner, scope),
                crate::syntax::ExportDeclaration::Default(default)
                    if let crate::syntax::ExportDefaultValue::Function(function) =
                        &default.value
                        && let Some(name) = &function.name =>
                {
                    self.declare(
                        &self.identifier_text(name),
                        SymbolKind::Function,
                        scope,
                        statement.id(),
                        name.range(),
                    );
                }
                _ => {}
            },

            _ => {}
        }
    }

    fn bind_statement(&mut self, statement: &'src crate::syntax::Stmt, scope: ScopeId) {
        let declaration = statement.id();
        match statement.data() {
            Statement::Variable(variable) => self.bind_variable(variable, scope, declaration),
            Statement::Function(function) => {
                if let Some(name) = &function.function.name {
                    let symbol = self.declare(
                        &self.identifier_text(name),
                        SymbolKind::Function,
                        scope,
                        name.id(),
                        name.range(),
                    );
                    self.jsx_callables
                        .insert(symbol, JsxCallable::Function(&function.function));
                }
            }
            Statement::Class(class) => {
                if let Some(name) = &class.name {
                    self.declare(
                        &self.identifier_text(name),
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
                self.bind_enum(declaration_node, declaration, scope, false)
            }
            Statement::Namespace(namespace) => {
                self.bind_namespace(namespace, declaration, scope, false, None);
            }
            Statement::Import(import) => self.bind_import(import, scope, declaration),
            Statement::ImportEquals(import) => {
                // The TypeScript baseline places TS2440 at the start of the
                // `import X = ...` statement, not on the local name alone.
                let symbol = self.declare(
                    &self.identifier_text(&import.local),
                    SymbolKind::Import,
                    scope,
                    declaration,
                    statement.range(),
                );
                if let crate::syntax::ExternalModuleReference::Qualified(_) = &import.reference {
                    self.import_equals_symbols.insert(declaration, symbol);
                }
            }
            Statement::Declare(inner) => {
                let saved = self.ambient_binding;
                self.ambient_binding = true;
                if let Some((declaration, declaration_id)) = enum_plan::enum_declaration(statement)
                {
                    self.bind_enum(declaration, declaration_id, scope, true);
                } else if let Statement::Namespace(namespace) = inner.data() {
                    self.bind_namespace(namespace, inner.id(), scope, true, None);
                } else {
                    self.bind_statement(inner, scope);
                }
                self.ambient_binding = saved;
            }
            Statement::Export(export) => match export {
                crate::syntax::ExportDeclaration::Named(
                    crate::syntax::ExportNamedDeclaration::Declaration(inner),
                ) => self.bind_statement(inner, scope),
                crate::syntax::ExportDeclaration::Default(default) => match &default.value {
                    crate::syntax::ExportDefaultValue::Function(function)
                        if let Some(name) = &function.name =>
                    {
                        let symbol = self.declare(
                            &self.identifier_text(name),
                            SymbolKind::Function,
                            scope,
                            declaration,
                            name.range(),
                        );
                        self.jsx_callables
                            .insert(symbol, JsxCallable::Function(function));
                    }
                    crate::syntax::ExportDefaultValue::Class(class)
                        if let Some(name) = &class.name =>
                    {
                        self.declare(
                            &self.identifier_text(name),
                            SymbolKind::Class,
                            scope,
                            declaration,
                            name.range(),
                        );
                    }
                    crate::syntax::ExportDefaultValue::Interface(interface) => {
                        self.bind_interface(interface, scope, declaration);
                    }
                    _ => {}
                },

                _ => {}
            },
            _ => {}
        }
    }

    fn bind_enum(
        &mut self,
        declaration: &'src crate::syntax::EnumDeclaration,
        declaration_id: NodeId,
        scope: ScopeId,
        ambient: bool,
    ) {
        let symbol = self.declare(
            &self.identifier_text(&declaration.name),
            SymbolKind::Enum,
            scope,
            declaration_id,
            declaration.name.range(),
        );
        let member_scope = if let Some(scope) = self.enum_member_scopes.get(&symbol) {
            *scope
        } else {
            let member_scope = self.new_scope(ScopeKind::Block, Some(scope));
            self.enum_member_scopes.insert(symbol, member_scope);
            member_scope
        };
        self.set_scope_owner(member_scope, symbol);
        for member in &declaration.members {
            let Some(name) = enum_plan::cook_member_name(self.source, &member.data().name) else {
                continue;
            };
            let member_symbol = self.declare(
                &name.to_utf8_lossy(),
                SymbolKind::EnumMember,
                member_scope,
                member.id(),
                member.range(),
            );
            self.enum_member_symbols.insert(member.id(), member_symbol);
            self.enum_member_symbols_by_name
                .entry(symbol)
                .or_default()
                .entry(name.clone())
                .or_insert(member_symbol);
            self.enum_member_names.insert(member.id(), name);
        }
        let numeric = declaration.members.iter().all(|member| {
            member
                .data()
                .initializer
                .as_deref()
                .is_none_or(is_numeric_enum_initializer)
        });
        match self.type_defs.get_mut(&symbol) {
            Some(TypeDef::Enum { numeric: existing }) => *existing &= numeric,
            None => {
                self.type_defs.insert(symbol, TypeDef::Enum { numeric });
            }
            Some(_) => unreachable!("enum symbol has an enum type definition"),
        }
        self.enum_declaration_symbols.insert(declaration_id, symbol);
        self.enum_declarations.push(EnumDeclarationBinding {
            declaration,
            declaration_id,
            symbol,
            ambient,
        });
    }

    fn bind_namespace(
        &mut self,
        declaration: &'src crate::syntax::NamespaceDeclaration,
        declaration_id: NodeId,
        scope: ScopeId,
        ambient: bool,
        parent: Option<SymbolId>,
    ) {
        if self.namespace_local_scopes.contains_key(&declaration_id) {
            return;
        }
        let (symbol, export_scope) = match &declaration.name {
            NamespaceName::Identifier { name, .. } => {
                let symbol = self.declare(
                    &self.identifier_text(name),
                    SymbolKind::Namespace,
                    scope,
                    declaration_id,
                    name.range(),
                );
                let export_scope = self
                    .namespace_export_scopes
                    .get(&symbol)
                    .copied()
                    .unwrap_or_else(|| {
                        let export_scope = self.new_scope(ScopeKind::Namespace, Some(scope));
                        self.namespace_export_scopes.insert(symbol, export_scope);
                        export_scope
                    });
                (symbol, export_scope)
            }
            NamespaceName::StringLiteral(lit) => {
                let detached = self.new_scope(ScopeKind::Namespace, Some(scope));
                let key = self
                    .source
                    .token_text(lit.data().token())
                    .and_then(string_value)
                    .map(|value| value.to_utf8_lossy())
                    .unwrap_or_default();
                let symbol = self.declare(
                    &key,
                    SymbolKind::Namespace,
                    detached,
                    declaration_id,
                    lit.range(),
                );
                self.ambient_modules.entry(key).or_insert(symbol);
                let export_scope = self
                    .namespace_export_scopes
                    .get(&symbol)
                    .copied()
                    .unwrap_or_else(|| {
                        let export_scope = self.new_scope(ScopeKind::Namespace, Some(scope));
                        self.namespace_export_scopes.insert(symbol, export_scope);
                        export_scope
                    });
                (symbol, export_scope)
            }
            NamespaceName::Global { range } => {
                let detached = self.new_scope(ScopeKind::Namespace, Some(scope));
                let symbol = self.declare(
                    "global",
                    SymbolKind::Namespace,
                    detached,
                    declaration_id,
                    *range,
                );
                let export_scope = self.global_scope;
                self.namespace_export_scopes.insert(symbol, export_scope);
                (symbol, export_scope)
            }
        };
        let local_scope = self.new_scope(ScopeKind::Function, Some(export_scope));
        self.namespace_local_scopes
            .insert(declaration_id, local_scope);
        self.namespace_declarations
            .push(NamespaceDeclarationBinding {
                declaration,
                declaration_id,
                symbol,
                export_scope,
                parent,
                ambient,
            });

        for statement in &declaration.body.data().statements {
            self.bind_namespace_member(statement, local_scope, export_scope, symbol, ambient);
        }
        for statement in &declaration.body.data().statements {
            let target = if ambient
                || matches!(
                    statement.data(),
                    Statement::Export(crate::syntax::ExportDeclaration::Named(
                        crate::syntax::ExportNamedDeclaration::Declaration(_)
                    ))
                )
                || self.is_dotted_namespace_tail(statement)
            {
                export_scope
            } else {
                local_scope
            };
            self.bind_hoisted_statement(statement, target);
        }
    }

    fn bind_namespace_member(
        &mut self,
        statement: &'src crate::syntax::Stmt,
        local_scope: ScopeId,
        export_scope: ScopeId,
        container: SymbolId,
        ambient: bool,
    ) {
        match statement.data() {
            Statement::Export(crate::syntax::ExportDeclaration::Named(
                crate::syntax::ExportNamedDeclaration::Declaration(inner),
            )) => match inner.data() {
                Statement::Namespace(namespace) => self.bind_namespace(
                    namespace,
                    inner.id(),
                    export_scope,
                    ambient,
                    Some(container),
                ),
                _ => self.bind_statement(inner, export_scope),
            },
            Statement::Namespace(namespace)
                if ambient || self.is_dotted_namespace_tail(statement) =>
            {
                self.bind_namespace(
                    namespace,
                    statement.id(),
                    export_scope,
                    ambient,
                    Some(container),
                );
            }
            Statement::Declare(inner) => match inner.data() {
                Statement::Namespace(namespace) => {
                    self.bind_namespace(namespace, inner.id(), export_scope, true, Some(container))
                }
                _ => self.bind_statement(statement, export_scope),
            },
            _ => self.bind_statement(statement, if ambient { export_scope } else { local_scope }),
        }
    }

    fn is_dotted_namespace_tail(&self, statement: &crate::syntax::Stmt) -> bool {
        self.source.tokens().iter().any(|token| {
            token.kind() == TokenKind::Dot && token.range().start() == statement.range().start()
        })
    }

    fn bind_variable(
        &mut self,
        variable: &'src VariableDeclaration,
        scope: ScopeId,
        declaration: NodeId,
    ) {
        let track_uninit = !self.ambient_binding
            && declaration != NodeId::default()
            && matches!(variable.kind, VariableKind::Let | VariableKind::Var);
        for declarator in &variable.declarations {
            let symbols = self.bind_pattern(
                &declarator.data().binding,
                variable.kind,
                scope,
                declaration,
            );
            self.declarator_symbols
                .insert(declarator.id(), symbols.clone());
            if track_uninit && declarator.data().initializer.is_none() {
                for symbol in symbols {
                    self.uninitialized_variables.insert(symbol);
                }
            }
        }
    }

    fn bind_pattern(
        &mut self,
        pattern: &'src crate::syntax::Pattern,
        kind: VariableKind,
        scope: ScopeId,
        declaration: NodeId,
    ) -> Vec<SymbolId> {
        match pattern.data() {
            BindingPattern::Identifier(name) => {
                vec![self.declare(
                    &self.identifier_text(name),
                    SymbolKind::Variable(kind),
                    scope,
                    declaration,
                    name.range(),
                )]
            }
            BindingPattern::Object(object) => {
                let mut result = Vec::new();
                for property in &object.properties {
                    result.extend(self.bind_pattern(&property.binding, kind, scope, declaration));
                }
                result
            }
            BindingPattern::Array(array) => {
                let mut result = Vec::new();
                for element in &array.elements {
                    if let crate::syntax::ArrayBindingElement::Binding(inner) = element {
                        result.extend(self.bind_pattern(inner, kind, scope, declaration));
                    }
                }
                result
            }
            BindingPattern::Rest(rest) => {
                self.bind_pattern(&rest.argument, kind, scope, declaration)
            }
            BindingPattern::Assignment(assignment) => {
                self.bind_pattern(&assignment.left, kind, scope, declaration)
            }
            BindingPattern::Missing(_) => Vec::new(),
        }
    }

    fn bind_interface(
        &mut self,
        interface: &'src InterfaceDeclaration,
        scope: ScopeId,
        declaration: NodeId,
    ) {
        let id = self.declare(
            &self.identifier_text(&interface.name),
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
            &self.identifier_text(&alias.name),
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
                &self.identifier_text(default),
                SymbolKind::Import,
                scope,
                declaration,
                default.range(),
            );
        }
        match &clause.binding {
            Some(ImportBinding::Namespace(name)) => {
                self.declare(
                    &self.identifier_text(name),
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
                        &self.identifier_text(local),
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

    fn build_import_equals_targets(
        &mut self,
        statements: &'src [crate::syntax::Stmt],
        scope: ScopeId,
    ) {
        for statement in statements {
            self.build_import_equals_target_statement(statement, scope);
        }
    }

    fn build_import_equals_target_statement(
        &mut self,
        statement: &'src crate::syntax::Stmt,
        scope: ScopeId,
    ) {
        match statement.data() {
            Statement::ImportEquals(import) => {
                if let crate::syntax::ExternalModuleReference::Qualified(name) = &import.reference {
                    self.resolve_qualified_import_equals(statement.id(), name, scope);
                }
            }
            Statement::Namespace(namespace) => {
                let child = self
                    .namespace_local_scopes
                    .get(&statement.id())
                    .copied()
                    .unwrap_or(scope);
                self.build_import_equals_targets(&namespace.body.data().statements, child);
            }
            Statement::Declare(inner) => {
                self.build_import_equals_target_statement(inner, scope);
            }
            Statement::Export(crate::syntax::ExportDeclaration::Named(
                crate::syntax::ExportNamedDeclaration::Declaration(inner),
            )) => {
                self.build_import_equals_target_statement(inner, scope);
            }
            _ => {}
        }
    }

    fn resolve_statements(&mut self, statements: &'src [crate::syntax::Stmt], scope: ScopeId) {
        self.check_function_overload_order(statements, scope);
        for statement in statements {
            self.resolve_statement(statement, scope);
        }
    }

    fn check_function_overload_order(
        &mut self,
        statements: &'src [crate::syntax::Stmt],
        scope: ScopeId,
    ) {
        let kind = self.scopes[scope.0 as usize].kind;
        if !matches!(kind, ScopeKind::Global | ScopeKind::Module) {
            return;
        }
        for index in 0..statements.len() {
            let Some(current) = Self::overloaded_function_name(&statements[index]) else {
                continue;
            };
            let missing = |this: &mut Self| {
                this.emit(
                    FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION,
                    current.range(),
                    FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION_MESSAGE,
                );
            };
            let Some(next) = statements.get(index + 1) else {
                missing(self);
                continue;
            };
            match Self::statement_function_name(next) {
                Some(next_name)
                    if self.identifier_text(current) == self.identifier_text(next_name) => {}
                _ => missing(self),
            }
        }
    }

    fn overloaded_function_name(
        statement: &'src crate::syntax::Stmt,
    ) -> Option<&'src IdentifierNode> {
        match statement.data() {
            Statement::Function(function) => Self::function_overload_name(&function.function),
            // `declare function` overloads are ambient and do not require an implementation.
            Statement::Export(crate::syntax::ExportDeclaration::Named(
                crate::syntax::ExportNamedDeclaration::Declaration(inner),
            )) => match inner.data() {
                Statement::Function(function) => Self::function_overload_name(&function.function),
                _ => None,
            },
            _ => None,
        }
    }

    fn statement_function_name(
        statement: &'src crate::syntax::Stmt,
    ) -> Option<&'src IdentifierNode> {
        match statement.data() {
            Statement::Function(function) => function.function.name.as_ref(),
            Statement::Declare(inner) => match inner.data() {
                Statement::Function(function) => function.function.name.as_ref(),
                _ => None,
            },
            Statement::Export(crate::syntax::ExportDeclaration::Named(
                crate::syntax::ExportNamedDeclaration::Declaration(inner),
            )) => match inner.data() {
                Statement::Function(function) => function.function.name.as_ref(),
                _ => None,
            },
            _ => None,
        }
    }

    fn function_overload_name(function: &'src FunctionLike) -> Option<&'src IdentifierNode> {
        if function.body.is_some() {
            return None;
        }
        function.name.as_ref()
    }

    fn resolve_statement(&mut self, statement: &'src crate::syntax::Stmt, scope: ScopeId) {
        match statement.data() {
            Statement::Variable(variable) => self.resolve_variable(variable, scope, true),
            Statement::Function(function) => {
                if self.es5 && self.scopes[scope.0 as usize].strict {
                    match self.scopes[scope.0 as usize].kind {
                        ScopeKind::Block | ScopeKind::For | ScopeKind::Catch | ScopeKind::With => {
                            let range = function
                                .function
                                .name
                                .as_ref()
                                .map(|name| name.range())
                                .unwrap_or_else(|| statement.range());
                            self.emit(
                                FUNCTION_DECLARATION_IN_BLOCK_ES5_STRICT,
                                range,
                                FUNCTION_DECLARATION_IN_BLOCK_ES5_STRICT_MESSAGE,
                            );
                        }
                        _ => {}
                    }
                }
                let ambient = self.ambient_stack.last().copied().unwrap_or(false);
                if ambient && function.function.body.is_some() {
                    let range = function
                        .function
                        .body
                        .as_ref()
                        .map(|body| match body {
                            crate::syntax::FunctionBody::Block(block) => block.range(),
                            crate::syntax::FunctionBody::Expression(expression) => {
                                expression.range()
                            }
                            crate::syntax::FunctionBody::Missing(_) => function
                                .function
                                .name
                                .as_ref()
                                .map(|name| name.range())
                                .unwrap_or_else(|| statement.range()),
                        })
                        .unwrap_or_else(|| statement.range());
                    self.emit(
                        AMBIENT_IMPLEMENTATION,
                        range,
                        AMBIENT_IMPLEMENTATION_MESSAGE,
                    );
                }
                self.resolve_function(&function.function, scope, true, true, self.types.any());
            }
            Statement::Class(class) => self.resolve_class(class, scope),
            Statement::Interface(interface) => {
                if let Some(id) = self.scopes[scope.0 as usize]
                    .types
                    .get(self.identifier_text(&interface.name).as_ref())
                    .copied()
                {
                    let _ = self.resolve_type_symbol(id);
                }
            }
            Statement::TypeAlias(alias) => {
                if let Some(id) = self.scopes[scope.0 as usize]
                    .types
                    .get(self.identifier_text(&alias.name).as_ref())
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
                let mut using_diagnostic = None;
                if let ForBinding::Variable(variable) = &for_statement.binding {
                    using_diagnostic = match variable.kind {
                        VariableKind::Using => Some((
                            USING_DECLARATION_IN_FOR_IN,
                            USING_DECLARATION_IN_FOR_IN_MESSAGE,
                        )),
                        VariableKind::AwaitUsing => Some((
                            AWAIT_USING_DECLARATION_IN_FOR_IN,
                            AWAIT_USING_DECLARATION_IN_FOR_IN_MESSAGE,
                        )),
                        _ => None,
                    };
                }
                if let Some((code, message)) = using_diagnostic {
                    if let ForBinding::Variable(variable) = &for_statement.binding {
                        self.emit(code, variable.range, message);
                    }
                } else {
                    self.resolve_for_binding(&for_statement.binding, child, false);
                }
                self.resolve_expr(&for_statement.object, child);
                self.resolve_statement(&for_statement.body, child);
            }
            Statement::ForOf(for_statement) => {
                let child = self.new_scope(ScopeKind::For, Some(scope));
                self.resolve_for_binding(&for_statement.binding, child, false);
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
            Statement::With(with_statement) => {
                let forbidden = self.is_typescript() || self.scopes[scope.0 as usize].is_strict();
                if forbidden {
                    self.emit(
                        WITH_STATEMENT_NOT_ALLOWED,
                        statement.range(),
                        WITH_STATEMENT_NOT_ALLOWED_MESSAGE,
                    );
                }
                self.resolve_expr(&with_statement.object, scope);
                let body_scope = if forbidden {
                    scope
                } else {
                    self.new_scope(ScopeKind::With, Some(scope))
                };
                self.resolve_statement(&with_statement.body, body_scope);
            }
            Statement::Labeled(statement) => self.resolve_statement(&statement.body, scope),
            Statement::ImportEquals(_) => {}
            Statement::Return(statement) => {
                if let Some(argument) = &statement.argument {
                    self.resolve_expr(argument, scope);
                }
                if let Some(body_id) = self.function_body_stack.last().copied() {
                    let return_type = statement
                        .argument
                        .as_ref()
                        .map(|argument| self.type_of_expr(argument, scope))
                        .unwrap_or_else(|| self.types.void());
                    self.return_types
                        .entry(body_id)
                        .or_default()
                        .push(return_type);
                }
            }
            Statement::Throw(statement) => self.resolve_expr(&statement.argument, scope),
            Statement::Enum(declaration) => {
                let member_scope = self
                    .enum_declaration_symbols
                    .get(&statement.id())
                    .and_then(|symbol| self.enum_member_scopes.get(symbol))
                    .copied()
                    .unwrap_or(scope);
                for member in &declaration.members {
                    if let Some(initializer) = &member.data().initializer {
                        self.resolve_expr(initializer, member_scope);
                    }
                }
            }
            Statement::Namespace(namespace) => {
                let child = self
                    .namespace_local_scopes
                    .get(&statement.id())
                    .copied()
                    .unwrap_or(scope);
                self.active_namespace_declarations.push(statement.id());
                self.resolve_statements(&namespace.body.data().statements, child);
                let popped = self.active_namespace_declarations.pop();
                debug_assert_eq!(popped, Some(statement.id()));
            }
            Statement::Declare(inner) => {
                self.ambient_stack.push(true);
                self.resolve_statement(inner, scope);
                self.ambient_stack.pop();
            }
            Statement::Export(export) => self.resolve_export(export, scope),
            _ => {}
        }
    }

    fn check_export_assignment_conflicts(&mut self) {
        let mut assignments = Vec::new();
        let mut mixed = false;
        for statement in self.source.statements() {
            let Statement::Export(export) = statement.data() else {
                continue;
            };
            match export {
                crate::syntax::ExportDeclaration::Assignment(expression) => {
                    assignments.push(expression.range());
                }
                crate::syntax::ExportDeclaration::Named(
                    crate::syntax::ExportNamedDeclaration::Specifiers {
                        type_only,
                        specifiers,
                        ..
                    },
                ) if *type_only
                    || (!specifiers.is_empty()
                        && specifiers.iter().all(|specifier| {
                            specifier.data().mode == crate::syntax::ExportSpecifierMode::TypeOnly
                        })) => {}
                crate::syntax::ExportDeclaration::All(all) if all.type_only => {}
                _ => mixed = true,
            }
        }
        if mixed {
            for range in assignments {
                self.emit(
                    MIXED_EXPORT_ASSIGNMENT,
                    range,
                    MIXED_EXPORT_ASSIGNMENT_MESSAGE,
                );
            }
        }
    }

    fn resolve_export(&mut self, export: &'src crate::syntax::ExportDeclaration, scope: ScopeId) {
        match export {
            crate::syntax::ExportDeclaration::Named(
                crate::syntax::ExportNamedDeclaration::Declaration(inner),
            ) => self.resolve_statement(inner, scope),
            crate::syntax::ExportDeclaration::Default(default) => match &default.value {
                crate::syntax::ExportDefaultValue::Function(function) => {
                    self.resolve_function(function, scope, true, true, self.types.any());
                }
                crate::syntax::ExportDefaultValue::Class(class) => self.resolve_class(class, scope),
                crate::syntax::ExportDefaultValue::Expression(expression) => {
                    self.resolve_expr(expression, scope);
                }
                crate::syntax::ExportDefaultValue::Missing(_) => {}
                crate::syntax::ExportDefaultValue::Interface(interface) => {
                    let name = self.identifier_text(&interface.name);
                    if let Some(id) = self.scopes[scope.0 as usize]
                        .types
                        .get(name.as_ref())
                        .copied()
                    {
                        let _ = self.resolve_type_symbol(id);
                    }
                }
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
                self.resolve_variable(variable, scope, true);
            }
            ForInitializer::Expression(expression) => self.resolve_expr(expression, scope),
        }
    }

    fn resolve_for_binding(
        &mut self,
        binding: &'src ForBinding,
        scope: ScopeId,
        require_using_initializer: bool,
    ) {
        match binding {
            ForBinding::Variable(variable) => {
                self.bind_variable(variable, scope, NodeId::default());
                self.resolve_variable(variable, scope, require_using_initializer);
            }
            ForBinding::Target(target) => {
                if matches!(target.data(), AssignmentTarget::Missing(_)) {
                    self.emit(
                        FOR_IN_LEFT_HAND_SIDE_INVALID,
                        target.range(),
                        FOR_IN_LEFT_HAND_SIDE_INVALID_MESSAGE,
                    );
                } else {
                    self.resolve_assignment_target(target, scope);
                }
            }
        }
    }

    fn resolve_variable(
        &mut self,
        variable: &'src VariableDeclaration,
        scope: ScopeId,
        require_using_initializer: bool,
    ) {
        for declarator_node in &variable.declarations {
            let declarator = declarator_node.data();
            if let Some(initializer) = &declarator.initializer {
                self.resolve_expr(initializer, scope);
            }
            if let Some(symbols) = self.declarator_symbols.get(&declarator_node.id())
                && declarator.initializer.is_some()
            {
                for symbol in symbols {
                    self.uninitialized_variables.remove(symbol);
                }
            }
            if matches!(
                variable.kind,
                VariableKind::Using | VariableKind::AwaitUsing
            ) {
                if !matches!(declarator.binding.data(), BindingPattern::Identifier(_)) {
                    self.emit(
                        USING_DECLARATION_BINDING_PATTERN,
                        declarator_node.range(),
                        USING_DECLARATION_BINDING_PATTERN_MESSAGE,
                    );
                } else if require_using_initializer && declarator.initializer.is_none() {
                    self.emit(
                        USING_DECLARATION_MISSING_INITIALIZER,
                        declarator_node.range(),
                        USING_DECLARATION_MISSING_INITIALIZER_MESSAGE,
                    );
                }
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
                let initializer_is_as_const =
                    declarator.initializer.as_ref().is_some_and(|initializer| {
                        matches!(
                            initializer.data(),
                            Expression::As(cast) if cast.type_node.is_none()
                        )
                    });
                let keep_literal = matches!(
                    variable.kind,
                    VariableKind::Const | VariableKind::Using | VariableKind::AwaitUsing
                );
                let declared = if initializer_is_as_const {
                    declared
                } else {
                    self.types.widen(declared, keep_literal)
                };
                if let Some(symbol) = self.lookup_value(scope, &self.identifier_text(name)) {
                    self.symbol_types[symbol.get() as usize] = declared;
                    match declarator
                        .initializer
                        .as_ref()
                        .map(|initializer| initializer.data())
                    {
                        Some(Expression::Function(function)) => {
                            self.jsx_callables
                                .insert(symbol, JsxCallable::Function(&function.function));
                        }
                        Some(Expression::Arrow(arrow)) => {
                            self.jsx_callables.insert(symbol, JsxCallable::Arrow(arrow));
                        }
                        _ => {}
                    }
                }
                if let (Some(target), Some(source)) = (annotation, initializer_type)
                    && !self.types_assignable(source, target)
                {
                    self.emit(TYPE_NOT_ASSIGNABLE, name.range(), NOT_ASSIGNABLE_MESSAGE);
                }
            }
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

    fn resolve_function(
        &mut self,
        function: &'src FunctionLike,
        parent: ScopeId,
        new_target_allowed: bool,
        is_declaration: bool,
        this_type: TypeId,
    ) {
        let scope = self.new_scope(ScopeKind::Function, Some(parent));
        let new_target_marker = self.new_target_contexts.len();
        self.new_target_contexts.push(new_target_allowed);
        self.super_call_contexts
            .push(SuperCallContext::NonConstructor);
        self.bind_implicit_function_values(&function.parameters, scope);
        let function_symbol = function.name.as_ref().map(|name| {
            let symbol_scope = if is_declaration { parent } else { scope };
            self.declare(
                &self.identifier_text(name),
                SymbolKind::Function,
                symbol_scope,
                name.id(),
                name.range(),
            )
        });
        self.bind_type_parameters(function.type_parameters.as_ref(), scope);
        let this_type = self.this_parameter_type(&function.parameters, scope, this_type);
        for parameter in &function.parameters {
            if self.is_this_parameter(parameter) {
                continue;
            }
            self.resolve_parameter(parameter, scope);
        }
        if let Some(return_type) = &function.return_type {
            let _ = self.resolve_type(&return_type.data().type_node, scope);
        }
        self.this_context.push(this_type);
        if let Some(body_id) = function.body.as_ref().and_then(FunctionBody::id) {
            self.function_body_stack.push(body_id);
        }
        match &function.body {
            Some(FunctionBody::Block(block)) => {
                if directive_prologue_is_strict(self.source, &block.data().statements) {
                    self.scopes[scope.0 as usize].strict = true;
                }
                self.bind_statements(&block.data().statements, scope);
                self.bind_hoisted_statements(&block.data().statements, scope);
                self.resolve_statements(&block.data().statements, scope);
            }
            Some(FunctionBody::Expression(expression)) => self.resolve_expr(expression, scope),
            _ => {}
        }
        if let Some(symbol) = function_symbol {
            let return_type = if let Some(annotation) = &function.return_type {
                self.resolve_type(&annotation.data().type_node, scope)
            } else {
                self.inferred_return_type(function, scope)
            };
            let type_parameters = function
                .type_parameters
                .as_ref()
                .map(|list| {
                    list.parameters
                        .iter()
                        .filter_map(|param| {
                            let name = self.identifier_text(&param.data().name);
                            self.lookup_type(scope, &name)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut function_parameters = Vec::with_capacity(function.parameters.len());
            for (idx, parameter) in function.parameters.iter().enumerate() {
                if self.is_this_parameter(parameter) {
                    continue;
                }
                let data = parameter.data();
                let type_id = match &data.type_annotation {
                    Some(annotation) => self.resolve_type(&annotation.data().type_node, scope),
                    None => self.types.any(),
                };
                let rest = matches!(data.binding.data(), BindingPattern::Rest(_));
                let optional = data.optional || data.initializer.is_some();
                let name = match data.binding.data() {
                    BindingPattern::Identifier(identifier) => {
                        self.identifier_text(identifier).into_owned()
                    }
                    BindingPattern::Rest(rest) => match rest.argument.data() {
                        BindingPattern::Identifier(identifier) => {
                            self.identifier_text(identifier).into_owned()
                        }
                        _ => format!("arg{idx}"),
                    },
                    BindingPattern::Assignment(assign) => match assign.left.data() {
                        BindingPattern::Identifier(identifier) => {
                            self.identifier_text(identifier).into_owned()
                        }
                        BindingPattern::Rest(rest) => match rest.argument.data() {
                            BindingPattern::Identifier(identifier) => {
                                self.identifier_text(identifier).into_owned()
                            }
                            _ => format!("arg{idx}"),
                        },
                        _ => format!("arg{idx}"),
                    },
                    _ => format!("arg{idx}"),
                };
                function_parameters.push(FunctionParameter::new(name, type_id, optional, rest));
            }
            let function_type = self.types.function_with_parameters(
                type_parameters,
                function_parameters,
                return_type,
            );
            self.symbol_types[symbol.get() as usize] = function_type;
        }
        if let Some(body_id) = function.body.as_ref().and_then(FunctionBody::id) {
            let popped = self.function_body_stack.pop();
            debug_assert_eq!(popped, Some(body_id));
        }
        self.this_context.pop();
        self.new_target_contexts.truncate(new_target_marker);
        let popped_context = self.super_call_contexts.pop();
        debug_assert_eq!(popped_context, Some(SuperCallContext::NonConstructor));
    }

    pub(crate) fn bind_type_parameters(
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
                &self.identifier_text(&data.name),
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

    fn resolve_unsupported_legacy_decorators(
        &mut self,
        decorators: &'src [crate::syntax::DecoratorNode],
        code: DiagnosticCode,
        message: &'static str,
        scope: ScopeId,
    ) {
        for decorator in decorators {
            self.emit(code, decorator.range(), message);
            self.resolve_expr(&decorator.data().expression, scope);
        }
    }

    fn resolve_parameter(&mut self, parameter: &'src crate::syntax::ParameterNode, scope: ScopeId) {
        let data = parameter.data();
        self.resolve_unsupported_legacy_decorators(
            &data.decorators,
            PARAMETER_DECORATOR_NOT_SUPPORTED,
            PARAMETER_DECORATOR_NOT_SUPPORTED_MESSAGE,
            scope,
        );
        self.bind_pattern(&data.binding, VariableKind::Let, scope, parameter.id());
        if let (BindingPattern::Identifier(name), Some(annotation)) =
            (data.binding.data(), &data.type_annotation)
        {
            let resolved = self.resolve_type(&annotation.data().type_node, scope);
            if let Some(symbol) = self.scopes[scope.0 as usize]
                .values
                .get(self.identifier_text(name).as_ref())
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
        let ambient =
            class.modifiers.is_declare || self.ambient_stack.last().copied().unwrap_or(false);
        self.resolve_class_body(class, parent, false, ambient);
    }

    fn is_this_parameter(&self, parameter: &'src ParameterNode) -> bool {
        let data = parameter.data();
        matches!(
            data.binding.data(),
            BindingPattern::Identifier(identifier)
                if self.identifier_text(identifier).as_ref() == "this"
        )
    }

    fn this_parameter_type(
        &mut self,
        parameters: &'src [ParameterNode],
        scope: ScopeId,
        default: TypeId,
    ) -> TypeId {
        let Some(parameter) = parameters.first() else {
            return default;
        };
        if !self.is_this_parameter(parameter) {
            return default;
        }
        let data = parameter.data();
        match (&data.binding.data(), &data.type_annotation) {
            (BindingPattern::Identifier(_), Some(annotation)) => {
                self.resolve_type(&annotation.data().type_node, scope)
            }
            _ => default,
        }
    }

    fn resolve_class_expression(&mut self, class: &'src ClassDeclaration, parent: ScopeId) {
        self.resolve_class_body(class, parent, true, false);
    }

    fn class_member_method_name(
        &self,
        member: &'src ClassMember,
    ) -> Option<std::borrow::Cow<'src, str>> {
        match member {
            ClassMember::Method(method) if method.modifier == PropertyModifier::None => {
                if let PropertyName::Identifier(identifier) = &method.name {
                    Some(self.identifier_text(identifier))
                } else {
                    None
                }
            }
            ClassMember::Constructor(_) => Some(std::borrow::Cow::Borrowed("constructor")),
            _ => None,
        }
    }

    fn class_member_overload_range(member: &'src ClassMember) -> TextRange {
        match member {
            ClassMember::Method(method) => Self::property_name_range(&method.name),
            ClassMember::Constructor(constructor) => constructor.body.range(),
            _ => TextRange::new(crate::source::Utf16Pos::ZERO, crate::source::Utf16Pos::ZERO)
                .unwrap(),
        }
    }

    fn check_class_method_overload_order(
        &mut self,
        members: &'src [crate::syntax::ClassMemberNode],
        ambient: bool,
    ) {
        if ambient {
            // Ambient classes (declare class, .d.ts, or inside declare namespace)
            // carry signature-only members that do not require implementations.
            return;
        }
        let mut i = 0;
        while i < members.len() {
            let first = members[i].data();
            let Some(name) = self.class_member_method_name(first) else {
                i += 1;
                continue;
            };
            // Only method signatures with no body start an overload group;
            // a constructor or a method with a body is an implementation, not a
            // group start.
            if matches!(first, ClassMember::Constructor(_)) {
                i += 1;
                continue;
            }
            if let ClassMember::Method(method) = first
                && method.function.body.is_some()
            {
                i += 1;
                continue;
            }
            let start = i;
            i += 1;
            // Consume consecutive overload signatures with the same name.
            while i < members.len() {
                let next = members[i].data();
                let next_name = self.class_member_method_name(next);
                let is_overload_signature = matches!(next, ClassMember::Method(method)
                    if method.modifier == PropertyModifier::None && method.function.body.is_none());
                if is_overload_signature && next_name.as_ref() == Some(&name) {
                    i += 1;
                } else {
                    break;
                }
            }
            // Missing implementation at end of class.
            if i >= members.len() {
                self.emit(
                    FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION,
                    Self::class_member_overload_range(members[start].data()),
                    FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION_MESSAGE,
                );
                break;
            }
            let implementation = members[i].data();
            let implementation_name = self.class_member_method_name(implementation);
            let matches_name = implementation_name.as_ref() == Some(&name);
            let is_overload_signature = matches!(implementation, ClassMember::Method(method)
                if method.modifier == PropertyModifier::None && method.function.body.is_none());

            if matches!(implementation, ClassMember::Method(method)
                if method.modifier == PropertyModifier::None && method.function.body.is_some() && matches_name)
                || matches!(implementation, ClassMember::Constructor(_)
                    if name == "constructor")
            {
                // Valid implementation follows the overload signature(s).
                i += 1;
            } else if matches!(implementation, ClassMember::Method(_)) && is_overload_signature {
                // Missing implementation: the next member is another overload signature,
                // not the body for this group. Leave it in place for the next iteration.
                self.emit(
                    FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION,
                    Self::class_member_overload_range(members[start].data()),
                    FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION_MESSAGE,
                );
            } else if matches!(implementation, ClassMember::Method(method)
                if method.modifier == PropertyModifier::None && method.function.body.is_some() && !matches_name)
            {
                // The next member is a method body with the wrong name.
                self.emit(
                    FUNCTION_IMPLEMENTATION_WRONG_NAME,
                    Self::class_member_overload_range(implementation),
                    FUNCTION_IMPLEMENTATION_WRONG_NAME_MESSAGE,
                );
                i += 1;
            } else {
                // Any other member means the implementation is missing.
                self.emit(
                    FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION,
                    Self::class_member_overload_range(members[start].data()),
                    FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION_MESSAGE,
                );
                // Do not consume this member; it may be a normal class member.
                i = start + 1;
            }
        }
    }

    fn resolve_class_body(
        &mut self,
        class: &'src ClassDeclaration,
        parent: ScopeId,
        bind_internal_name: bool,
        ambient: bool,
    ) {
        // A class nested inside a constructor does not inherit its super-call
        // legality: only the constructor body itself may call `super(...)`.
        self.super_call_contexts
            .push(SuperCallContext::NonConstructor);
        self.class_derived_stack.push(class.extends.is_some());
        let scope = self.new_scope(ScopeKind::Class, Some(parent));
        self.scopes[scope.0 as usize].strict = true;
        // Named class expressions bind their internal name into the class scope
        // only (mirroring named function expressions). Declarations keep their
        // existing outer-scope binding from `bind_statement` unchanged.
        if bind_internal_name && let Some(name) = &class.name {
            self.declare(
                &self.identifier_text(name),
                SymbolKind::Class,
                scope,
                name.id(),
                name.range(),
            );
        }
        self.bind_type_parameters(class.type_parameters.as_ref(), scope);
        // Member-scope ownership: the class symbol owns the class scope so
        // member declarations (bound by a later workstream) qualify as
        // `Ship.isSunk`. Set only after the internal-name and type-parameter
        // bindings above, which must stay bare (pinned classExpression.symbols,
        // strictFunctionTypesErrors.symbols).
        let owner_scope = if bind_internal_name { scope } else { parent };
        let owner = class.name.as_ref().and_then(|name| {
            self.scopes[owner_scope.0 as usize]
                .values
                .get(self.identifier_text(name).as_ref())
                .copied()
        });
        if let Some(owner) = owner {
            self.set_scope_owner(scope, owner);
        }
        // Class decorator expressions evaluate in the enclosing scope, before
        // heritage and members, so they do not see a class-expression name.
        for decorator in &class.decorators {
            self.resolve_expr(&decorator.data().expression, parent);
        }
        if let Some(heritage) = &class.extends {
            self.resolve_expr(&heritage.expression, scope);
        }
        self.check_class_method_overload_order(&class.members, ambient);
        for member in &class.members {
            self.bind_class_member(member, scope);
        }
        for implemented in &class.implements {
            let _ = self.resolve_type(implemented, scope);
        }
        if let Some(owner) = owner {
            let instance_type = self.class_instance_type(class, scope);
            self.class_instance_types.insert(owner, instance_type);
            self.symbol_types[owner.get() as usize] = self.types.named(owner);
        }
        for member in &class.members {
            self.resolve_class_member(member.data(), scope, ambient);
        }
        self.check_class_property_initialization(&class.members, scope);
        let popped_derived = self.class_derived_stack.pop();
        debug_assert_eq!(popped_derived, Some(class.extends.is_some()));
        let popped_context = self.super_call_contexts.pop();
        debug_assert_eq!(popped_context, Some(SuperCallContext::NonConstructor));
    }
    fn check_class_property_initialization(
        &mut self,
        members: &'src [crate::syntax::ClassMemberNode],
        _scope: ScopeId,
    ) {
        let assigned = self.constructor_property_assignments(members);
        for member in members {
            let ClassMember::Property(property) = member.data() else {
                continue;
            };
            // TypeScript only reports TS2564 for identifier-named properties.
            // String- and numeric-literal property names are exempt from the
            // strict property initialization check.
            if property.optional
                || property.definite
                || property.initializer.is_some()
                || property.modifiers.is_static
                || property.modifiers.is_declare
                || matches!(
                    property.name,
                    PropertyName::String(_) | PropertyName::Number(_)
                )
            {
                continue;
            }
            let declared = property
                .type_annotation
                .as_ref()
                .and_then(|annotation| {
                    self.type_nodes
                        .get(&annotation.data().type_node.id())
                        .copied()
                })
                .unwrap_or(self.types.any());
            if self.types_assignable(self.types.undefined_type(), declared) {
                continue;
            }
            let Some(name) = self.property_key(&property.name) else {
                continue;
            };
            if assigned.contains(&name) {
                continue;
            }
            self.emit(
                PROPERTY_NOT_INITIALIZED,
                Self::property_name_range(&property.name),
                PROPERTY_NOT_INITIALIZED_MESSAGE,
            );
        }
    }
    fn class_instance_type(&mut self, class: &'src ClassDeclaration, scope: ScopeId) -> TypeId {
        let mut properties = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for member in &class.members {
            let (name, type_id, optional) = match member.data() {
                ClassMember::Property(property) if !property.modifiers.is_static => {
                    let Some(name) = self.property_key(&property.name) else {
                        continue;
                    };
                    let type_id = match &property.type_annotation {
                        Some(annotation) => self.resolve_type(&annotation.data().type_node, scope),
                        None => self.types.any(),
                    };
                    (name, type_id, property.optional)
                }
                ClassMember::AutoAccessor(accessor) if !accessor.modifiers.is_static => {
                    let Some(name) = self.property_key(&accessor.name) else {
                        continue;
                    };
                    let type_id = match &accessor.type_annotation {
                        Some(annotation) => self.resolve_type(&annotation.data().type_node, scope),
                        None => self.types.any(),
                    };
                    (name, type_id, false)
                }
                ClassMember::Method(method) if !method.modifiers.is_static => match method.modifier
                {
                    PropertyModifier::None => {
                        let Some(name) = self.property_key(&method.name) else {
                            continue;
                        };
                        let type_id = self.type_of_function_like(&method.function, scope);
                        (name, type_id, method.optional)
                    }
                    PropertyModifier::Get => {
                        let Some(name) = self.property_key(&method.name) else {
                            continue;
                        };
                        let type_id = match &method.function.return_type {
                            Some(annotation) => {
                                self.resolve_type(&annotation.data().type_node, scope)
                            }
                            None => self.types.any(),
                        };
                        (name, type_id, method.optional)
                    }
                    PropertyModifier::Set => continue,
                },
                _ => continue,
            };
            if !seen.insert(name.clone()) {
                continue;
            }
            properties.push(PropertyType::new(name, optional, type_id));
        }
        self.types.object_type(properties)
    }

    fn constructor_property_assignments(
        &self,
        members: &'src [crate::syntax::ClassMemberNode],
    ) -> HashSet<String> {
        let mut assigned = HashSet::new();
        let Some(constructor) = members.iter().find_map(|member| {
            if let ClassMember::Constructor(constructor) = member.data() {
                Some(constructor)
            } else {
                None
            }
        }) else {
            return assigned;
        };
        for parameter in &constructor.parameters {
            let parameter = parameter.data();
            if (parameter.modifiers.accessibility.is_some() || parameter.modifiers.is_readonly)
                && let BindingPattern::Identifier(identifier) = parameter.binding.data()
            {
                assigned.insert(self.identifier_text(identifier).into_owned());
            }
        }
        for statement in &constructor.body.data().statements {
            if let Statement::Expression(expression) = statement.data()
                && let Expression::Assignment(assignment) = expression.expression.data()
                && let AssignmentTarget::Member(member) = assignment.left.data()
                && matches!(member.object.data(), Expression::This)
                && let MemberProperty::Named(ident) = &member.property
            {
                assigned.insert(self.identifier_text(ident).into_owned());
            }
        }
        assigned
    }

    fn property_name_range(name: &crate::syntax::PropertyName) -> crate::source::TextRange {
        match name {
            crate::syntax::PropertyName::Identifier(identifier) => identifier.range(),
            crate::syntax::PropertyName::Private(identifier) => identifier.range(),
            crate::syntax::PropertyName::String(literal) => literal.range(),
            crate::syntax::PropertyName::Number(literal) => literal.range(),
            crate::syntax::PropertyName::Computed(expression) => expression.range(),
            crate::syntax::PropertyName::Missing(_) => crate::source::TextRange::new(
                crate::source::Utf16Pos::ZERO,
                crate::source::Utf16Pos::ZERO,
            )
            .unwrap(),
        }
    }

    fn check_set_accessor_parameter_initializer(
        &mut self,
        modifier: PropertyModifier,
        parameters: &[ParameterNode],
        name: &PropertyName,
    ) {
        if modifier != PropertyModifier::Set {
            return;
        }
        if parameters
            .iter()
            .any(|parameter| parameter.data().initializer.is_some())
        {
            self.emit(
                SET_ACCESSOR_PARAMETER_INITIALIZER,
                Self::property_name_range(name),
                SET_ACCESSOR_PARAMETER_INITIALIZER_MESSAGE,
            );
        }
    }
    fn check_accessor_this_parameter(
        &mut self,
        modifier: PropertyModifier,
        parameters: &'src [ParameterNode],
        name: &PropertyName,
    ) {
        if !matches!(modifier, PropertyModifier::Get | PropertyModifier::Set) {
            return;
        }
        if parameters
            .iter()
            .any(|parameter| self.is_this_parameter(parameter))
        {
            self.emit(
                ACCESSOR_THIS_PARAMETER,
                Self::property_name_range(name),
                ACCESSOR_THIS_PARAMETER_MESSAGE,
            );
        }
    }

    fn check_get_accessor(
        &mut self,
        modifier: PropertyModifier,
        function: &'src FunctionLike,
        name: &PropertyName,
    ) {
        if modifier != PropertyModifier::Get {
            return;
        }
        let range = Self::property_name_range(name);
        if function
            .parameters
            .iter()
            .any(|parameter| !self.is_this_parameter(parameter))
        {
            self.emit(
                GET_ACCESSOR_PARAMETERS,
                range,
                GET_ACCESSOR_PARAMETERS_MESSAGE,
            );
        }
        if !Self::function_body_returns_value(function) {
            self.emit(
                GET_ACCESSOR_NO_RETURN,
                range,
                GET_ACCESSOR_NO_RETURN_MESSAGE,
            );
        }
    }

    fn function_body_returns_value(function: &'src FunctionLike) -> bool {
        match function.body.as_ref() {
            Some(crate::syntax::FunctionBody::Block(block)) => {
                Self::block_returns_value(block.data())
            }
            Some(crate::syntax::FunctionBody::Expression(_)) => true,
            _ => false,
        }
    }

    fn block_returns_value(block: &'src crate::syntax::Block) -> bool {
        block
            .statements
            .iter()
            .any(|stmt| Self::statement_returns_value(stmt.data()))
    }

    fn statement_returns_value(statement: &'src Statement) -> bool {
        match statement {
            Statement::Return(ret) => ret.argument.is_some(),
            Statement::Throw(_) => true,
            Statement::Block(block) => Self::block_returns_value(block.data()),
            Statement::Labeled(labeled) => Self::statement_returns_value(labeled.body.data()),
            Statement::If(if_stmt) => {
                Self::statement_returns_value(if_stmt.consequent.data())
                    && if_stmt
                        .alternate
                        .as_ref()
                        .is_some_and(|alt| Self::statement_returns_value(alt.data()))
            }
            Statement::Switch(switch) => {
                let cases = &switch.cases;
                let has_default = cases.iter().any(|case| case.data().test.is_none());
                has_default
                    && cases.iter().all(|case| {
                        case.data()
                            .consequent
                            .iter()
                            .any(|s| Self::statement_returns_value(s.data()))
                    })
            }
            Statement::Try(try_stmt) => {
                let handler_returns = try_stmt
                    .handler
                    .as_ref()
                    .is_some_and(|h| Self::block_returns_value(h.data().body.data()));
                Self::block_returns_value(try_stmt.block.data()) && handler_returns
            }
            Statement::For(_)
            | Statement::ForIn(_)
            | Statement::ForOf(_)
            | Statement::While(_)
            | Statement::DoWhile(_) => false,
            _ => false,
        }
    }
    fn bind_class_member(&mut self, member: &'src crate::syntax::ClassMemberNode, scope: ScopeId) {
        let range = |name: &crate::syntax::PropertyName| Self::property_name_range(name);
        match member.data() {
            ClassMember::Property(property) => {
                if let Some(name) = self.property_key(&property.name) {
                    self.declare(
                        &name,
                        SymbolKind::Variable(VariableKind::Let),
                        scope,
                        member.id(),
                        range(&property.name),
                    );
                }
            }
            ClassMember::AutoAccessor(accessor) => {
                if let Some(name) = self.property_key(&accessor.name) {
                    self.declare(
                        &name,
                        SymbolKind::Variable(VariableKind::Let),
                        scope,
                        member.id(),
                        range(&accessor.name),
                    );
                }
            }
            ClassMember::Method(method) if method.modifier == PropertyModifier::None => {
                if let Some(name) = self.property_key(&method.name) {
                    self.declare(
                        &name,
                        SymbolKind::Function,
                        scope,
                        member.id(),
                        range(&method.name),
                    );
                }
            }
            _ => {}
        }
    }
    fn class_this_type(&mut self, scope: ScopeId, is_static: bool) -> TypeId {
        let Some(owner) = self.scopes[scope.0 as usize].owner else {
            return self.types.any();
        };
        if is_static {
            self.types.named(owner)
        } else {
            self.class_instance_types
                .get(&owner)
                .copied()
                .unwrap_or_else(|| self.types.any())
        }
    }

    fn resolve_class_member(&mut self, member: &'src ClassMember, scope: ScopeId, ambient: bool) {
        match member {
            ClassMember::Method(method) => {
                self.resolve_property_name(&method.name, scope);
                self.check_set_accessor_parameter_initializer(
                    method.modifier,
                    &method.function.parameters,
                    &method.name,
                );
                self.check_accessor_this_parameter(
                    method.modifier,
                    &method.function.parameters,
                    &method.name,
                );
                self.check_get_accessor(method.modifier, &method.function, &method.name);
                if let PropertyName::Identifier(identifier) = &method.name
                    && self.identifier_text(identifier).as_ref() == "constructor"
                    && let Some(type_parameters) = &method.function.type_parameters
                {
                    let range = type_parameters
                        .parameters
                        .first()
                        .map(|parameter| parameter.range())
                        .unwrap_or_else(|| Self::property_name_range(&method.name));
                    self.emit(
                        CONSTRUCTOR_TYPE_PARAMETERS,
                        range,
                        CONSTRUCTOR_TYPE_PARAMETERS_MESSAGE,
                    );
                }
                if ambient && method.function.body.is_some() {
                    let range = method
                        .function
                        .body
                        .as_ref()
                        .map(|body| match body {
                            crate::syntax::FunctionBody::Block(block) => block.range(),
                            crate::syntax::FunctionBody::Expression(expression) => {
                                expression.range()
                            }
                            crate::syntax::FunctionBody::Missing(_) => {
                                Self::property_name_range(&method.name)
                            }
                        })
                        .unwrap_or_else(|| Self::property_name_range(&method.name));
                    self.emit(
                        AMBIENT_IMPLEMENTATION,
                        range,
                        AMBIENT_IMPLEMENTATION_MESSAGE,
                    );
                }
                let this_type = self.class_this_type(scope, method.modifiers.is_static);
                self.resolve_function(&method.function, scope, false, true, this_type);
            }
            ClassMember::Constructor(constructor) => {
                if ambient {
                    self.emit(
                        AMBIENT_IMPLEMENTATION,
                        constructor.body.range(),
                        AMBIENT_IMPLEMENTATION_MESSAGE,
                    );
                }
                self.resolve_unsupported_legacy_decorators(
                    &constructor.decorators,
                    CONSTRUCTOR_DECORATOR_NOT_SUPPORTED,
                    CONSTRUCTOR_DECORATOR_NOT_SUPPORTED_MESSAGE,
                    scope,
                );
                let derived = self.class_derived_stack.last().copied().unwrap_or(false);
                let child = self.new_scope(ScopeKind::Function, Some(scope));
                let new_target_marker = self.new_target_contexts.len();
                self.new_target_contexts.push(true);
                self.bind_implicit_function_values(&constructor.parameters, child);
                self.super_call_contexts
                    .push(SuperCallContext::ConstructorParameters { derived });
                for parameter in &constructor.parameters {
                    self.resolve_parameter(parameter, child);
                }
                let popped_parameters = self.super_call_contexts.pop();
                debug_assert_eq!(
                    popped_parameters,
                    Some(SuperCallContext::ConstructorParameters { derived })
                );
                self.super_call_contexts.push(if derived {
                    SuperCallContext::DerivedConstructor
                } else {
                    SuperCallContext::BaseConstructor
                });
                let this_type = self.class_this_type(scope, false);
                self.this_context.push(this_type);
                self.bind_statements(&constructor.body.data().statements, child);
                self.resolve_statements(&constructor.body.data().statements, child);
                self.this_context.pop();
                self.new_target_contexts.truncate(new_target_marker);
                self.super_call_contexts.pop();
            }
            ClassMember::Property(property) => {
                self.resolve_property_name(&property.name, scope);
                let type_id = if let Some(annotation) = &property.type_annotation {
                    self.resolve_type(&annotation.data().type_node, scope)
                } else if let Some(initializer) = &property.initializer {
                    self.resolve_expr(initializer, scope);
                    self.type_of_expr(initializer, scope)
                } else {
                    self.types.any()
                };
                if let Some(name) = self.property_key(&property.name)
                    && let Some(&symbol) = self.scopes[scope.0 as usize].values.get(&name)
                {
                    self.symbol_types[symbol.get() as usize] = type_id;
                }
            }
            ClassMember::AutoAccessor(accessor) => {
                self.resolve_property_name(&accessor.name, scope);
                let type_id = if let Some(annotation) = &accessor.type_annotation {
                    self.resolve_type(&annotation.data().type_node, scope)
                } else if let Some(initializer) = &accessor.initializer {
                    self.resolve_expr(initializer, scope);
                    self.type_of_expr(initializer, scope)
                } else {
                    self.types.any()
                };
                if let Some(name) = self.property_key(&accessor.name)
                    && let Some(&symbol) = self.scopes[scope.0 as usize].values.get(&name)
                {
                    self.symbol_types[symbol.get() as usize] = type_id;
                }
            }
            ClassMember::StaticBlock(block) => {
                let child = self.new_scope(ScopeKind::Block, Some(scope));
                let new_target_marker = self.new_target_contexts.len();
                self.new_target_contexts.push(false);
                self.bind_statements(&block.data().statements, child);
                self.resolve_statements(&block.data().statements, child);
                self.new_target_contexts.truncate(new_target_marker);
            }
            _ => {}
        }
    }

    fn resolve_property_name(&mut self, name: &'src PropertyName, scope: ScopeId) {
        if let PropertyName::Computed(expression) = name {
            self.resolve_expr(expression, scope);
        }
    }

    pub(crate) fn resolve_expr(&mut self, expression: &'src Expr, scope: ScopeId) {
        match expression.data() {
            Expression::Identifier(identifier) => {
                self.resolve_value(identifier, expression.id(), scope);
            }
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
            Expression::Function(function) => {
                self.resolve_function(&function.function, scope, true, false, self.types.any())
            }
            Expression::Class(class) => self.resolve_class_expression(&class.class, scope),
            Expression::Arrow(arrow) => {
                let child = self.new_scope(ScopeKind::Function, Some(scope));
                // Arrows capture `this` but never inherit super-call legality.
                self.super_call_contexts
                    .push(SuperCallContext::NonConstructor);
                self.bind_type_parameters(arrow.type_parameters.as_ref(), child);
                for parameter in &arrow.parameters {
                    self.resolve_parameter(parameter, child);
                }
                if let Some(return_type) = &arrow.return_type {
                    let _ = self.resolve_type(&return_type.data().type_node, child);
                }
                match &arrow.body {
                    FunctionBody::Block(block) => {
                        if directive_prologue_is_strict(self.source, &block.data().statements) {
                            self.scopes[child.0 as usize].strict = true;
                        }
                        self.bind_statements(&block.data().statements, child);
                        self.resolve_statements(&block.data().statements, child);
                    }
                    FunctionBody::Expression(inner) => self.resolve_expr(inner, child),
                    FunctionBody::Missing(_) => {}
                }
                let popped_context = self.super_call_contexts.pop();
                debug_assert_eq!(popped_context, Some(SuperCallContext::NonConstructor));
            }
            Expression::Call(call) => {
                if matches!(call.callee.data(), Expression::Super) {
                    self.check_super_call(call.callee.range());
                } else {
                    self.resolve_expr(&call.callee, scope);
                }
                self.resolve_type_arguments(call.type_arguments.as_ref(), scope);
                self.resolve_arguments(&call.arguments, scope);
                self.check_call(call, scope, expression.range());
            }
            Expression::New(new) => {
                self.resolve_expr(&new.callee, scope);
                self.resolve_type_arguments(new.type_arguments.as_ref(), scope);
                self.resolve_arguments(&new.arguments, scope);
            }
            Expression::Member(member) => {
                if !matches!(member.object.data(), Expression::Super) {
                    self.resolve_expr(&member.object, scope);
                }
                let object_symbol = self.resolved_expression_reference(&member.object);
                if let MemberProperty::Computed(inner) = &member.property {
                    self.resolve_expr(inner, scope);
                }
                if member.optional {
                    return;
                }
                let Some(name) =
                    enum_plan::cook_member_property_name(self.source, &member.property)
                else {
                    return;
                };
                if let Some(enum_symbol) = object_symbol
                    && self.symbols[enum_symbol.get() as usize].kind == SymbolKind::Enum
                    && let Some(member_symbol) = self
                        .enum_member_symbols_by_name
                        .get(&enum_symbol)
                        .and_then(|members| members.get(&name))
                        .copied()
                {
                    self.references.insert(expression.id(), member_symbol);
                    self.enum_member_identifier_uses.insert(expression.id());
                    return;
                }
                let base = object_symbol
                    .filter(|symbol| self.symbols[symbol.get() as usize].kind == SymbolKind::Import)
                    .map(enum_plan::ImportedEnumMemberBase::Import)
                    .or_else(|| {
                        self.imported_enum_member_uses
                            .contains_key(&member.object.id())
                            .then_some(enum_plan::ImportedEnumMemberBase::MemberResult(
                                member.object.id(),
                            ))
                    });
                if let Some(base) = base {
                    self.imported_enum_member_uses.insert(
                        expression.id(),
                        enum_plan::ImportedEnumMemberUse::new(base, name, expression.range()),
                    );
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
                let literal_truthy = if let Expression::Literal(Literal::Boolean(literal)) =
                    conditional.test.data()
                {
                    Some(literal.data().token().kind() == TokenKind::KwTrue)
                } else {
                    None
                };
                match literal_truthy {
                    Some(true) => {
                        self.resolve_expr(&conditional.consequent, scope);
                        let saved = self.suppress_used_before_assigned;
                        self.suppress_used_before_assigned = true;
                        self.resolve_expr(&conditional.alternate, scope);
                        self.suppress_used_before_assigned = saved;
                    }
                    Some(false) => {
                        let saved = self.suppress_used_before_assigned;
                        self.suppress_used_before_assigned = true;
                        self.resolve_expr(&conditional.consequent, scope);
                        self.suppress_used_before_assigned = saved;
                        self.resolve_expr(&conditional.alternate, scope);
                    }
                    None => {
                        self.resolve_expr(&conditional.consequent, scope);
                        self.resolve_expr(&conditional.alternate, scope);
                    }
                }
            }
            Expression::Assignment(assignment) => {
                self.resolve_assignment_target(&assignment.left, scope);
                self.resolve_expr(&assignment.right, scope);
                if assignment.operator == AssignmentOperator::Assign {
                    let target = self.type_of_assignment_target(&assignment.left, scope);
                    let source = self.type_of_expr(&assignment.right, scope);
                    if !self.types_assignable(source, target) {
                        self.emit(
                            TYPE_NOT_ASSIGNABLE,
                            expression.range(),
                            NOT_ASSIGNABLE_MESSAGE,
                        );
                    }
                }
            }
            Expression::Sequence(sequence) => {
                for inner in &sequence.expressions {
                    self.resolve_expr(inner, scope);
                }
            }
            Expression::Parenthesized(inner) => {
                self.resolve_transparent_expression(expression, inner, scope);
            }
            Expression::As(cast) => {
                self.resolve_transparent_expression(expression, &cast.expression, scope);
                if let Some(type_node) = &cast.type_node {
                    let _ = self.resolve_type(type_node, scope);
                }
            }
            Expression::Satisfies(satisfies) => {
                self.resolve_transparent_expression(expression, &satisfies.expression, scope);
                let _ = self.resolve_type(&satisfies.type_node, scope);
            }
            Expression::TypeAssertion(assertion) => {
                self.resolve_transparent_expression(expression, &assertion.expression, scope);
                let _ = self.resolve_type(&assertion.type_node, scope);
            }
            Expression::NonNull(non_null) => {
                self.resolve_transparent_expression(expression, &non_null.expression, scope);
            }
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
            Expression::JsxElement(element) => {
                let _ = self.check_jsx_element(expression, element, scope);
            }
            Expression::JsxSelfClosingElement(element) => {
                let _ = self.check_jsx_self_closing_element(expression, element, scope);
            }
            Expression::JsxFragment(fragment) => {
                let _ = self.check_jsx_fragment(expression, fragment, scope);
            }
            Expression::Meta(MetaProperty::NewTarget) => {
                if !self.new_target_contexts.last().copied().unwrap_or(false) {
                    self.emit(
                        NEW_TARGET_OUTSIDE_FUNCTION,
                        expression.range(),
                        NEW_TARGET_OUTSIDE_FUNCTION_MESSAGE,
                    );
                }
            }
            Expression::Meta(MetaProperty::ImportMeta) => {}
            // A `super` expression that is neither a call callee (handled in
            // `Expression::Call`) nor a member-access object (skipped in
            // `Expression::Member`/`resolve_assignment_target`) has no valid
            // position, matching TS1034.
            Expression::Super => self.emit(
                BARE_SUPER_EXPRESSION,
                expression.range(),
                BARE_SUPER_EXPRESSION_MESSAGE,
            ),
            _ => {}
        }
    }

    fn check_super_call(&mut self, range: TextRange) {
        let context = self
            .super_call_contexts
            .last()
            .copied()
            .unwrap_or(SuperCallContext::NonConstructor);
        let (code, message) = match context {
            SuperCallContext::DerivedConstructor => return,
            SuperCallContext::BaseConstructor
            | SuperCallContext::ConstructorParameters { derived: false } => (
                SUPER_REFERENCE_NON_DERIVED,
                SUPER_REFERENCE_NON_DERIVED_MESSAGE,
            ),
            SuperCallContext::ConstructorParameters { derived: true } => (
                SUPER_CALL_IN_CONSTRUCTOR_ARGUMENTS,
                SUPER_CALL_IN_CONSTRUCTOR_ARGUMENTS_MESSAGE,
            ),
            SuperCallContext::NonConstructor => (
                SUPER_CALL_OUTSIDE_CONSTRUCTOR,
                SUPER_CALL_OUTSIDE_CONSTRUCTOR_MESSAGE,
            ),
        };
        self.emit(code, range, message);
    }

    fn resolve_object_member(&mut self, member: &'src ObjectMember, scope: ScopeId) {
        match member {
            ObjectMember::Property(property) => {
                self.resolve_property_name(&property.name, scope);
                self.resolve_expr(&property.value, scope);
            }
            ObjectMember::Method(method) => {
                self.resolve_property_name(&method.name, scope);
                self.check_set_accessor_parameter_initializer(
                    method.modifier,
                    &method.function.parameters,
                    &method.name,
                );
                self.check_accessor_this_parameter(
                    method.modifier,
                    &method.function.parameters,
                    &method.name,
                );
                self.check_get_accessor(method.modifier, &method.function, &method.name);
                self.resolve_function(&method.function, scope, false, false, self.types.any());
            }
            ObjectMember::Spread(spread) => self.resolve_expr(&spread.argument, scope),
            ObjectMember::Missing(_) => {}
        }
    }

    fn resolve_transparent_expression(
        &mut self,
        expression: &'src Expr,
        inner: &'src Expr,
        scope: ScopeId,
    ) {
        self.resolve_expr(inner, scope);
        if let Some(symbol) = self.resolved_expression_reference(inner) {
            self.reference_aliases.insert(expression.id(), symbol);
        }
        if let Some(candidate) = self.imported_enum_member_uses.get(&inner.id()).cloned() {
            self.imported_enum_member_uses
                .insert(expression.id(), candidate);
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
    ) -> Vec<TypeId> {
        let Some(list) = arguments else {
            return Vec::new();
        };
        list.arguments
            .iter()
            .map(|argument| self.resolve_type(argument, scope))
            .collect()
    }

    fn check_call(&mut self, call: &'src CallExpression, scope: ScopeId, call_range: TextRange) {
        if !self.is_typescript() {
            return;
        }
        let callee_symbol = if let Expression::Identifier(identifier) = call.callee.data() {
            self.references.get(&identifier.id()).copied()
        } else {
            None
        };
        let callable = callee_symbol.and_then(|symbol| self.jsx_callables.get(&symbol).copied());
        let not_callable_range = match call.callee.data() {
            Expression::Member(member) => match &member.property {
                MemberProperty::Named(identifier) => identifier.range(),
                MemberProperty::Private(identifier) => identifier.range(),
                MemberProperty::Computed(expression) => expression.range(),
            },
            _ => call.callee.range(),
        };

        let callee_type = self.type_of_expr(&call.callee, scope);
        let callee_kind = self.types.get(callee_type);
        if !matches!(
            callee_kind,
            Type::Any
                | Type::Unknown
                | Type::Error
                | Type::Function(_)
                | Type::Union(_)
                | Type::ObjectType(_)
                | Type::Named(_)
        ) {
            self.emit(
                EXPRESSION_NOT_CALLABLE,
                not_callable_range,
                EXPRESSION_NOT_CALLABLE_MESSAGE,
            );
            return;
        }
        // Avoid typing arguments when the callee is not a function or union of
        // functions, so that calls through `any` do not trigger member-access
        // diagnostics on the arguments (discriminated unions, etc.).
        if !matches!(callee_kind, Type::Function(_) | Type::Union(_)) {
            return;
        }

        let mut argument_ranges = Vec::new();
        let mut argument_types = Vec::new();
        let mut has_spread = false;
        for argument in &call.arguments {
            match argument {
                CallArgument::Expression(inner) => {
                    let argument_type = self.type_of_expr(inner, scope);
                    argument_ranges.push(inner.range());
                    argument_types.push(argument_type);
                }
                CallArgument::Spread(_) => {
                    has_spread = true;
                    break;
                }
                CallArgument::Missing(_) => {}
            }
        }
        if has_spread {
            return;
        }

        let signature = match self.types.get(callee_type).clone() {
            Type::Function(signature) => Some(signature),
            Type::Union(members) => self.union_call_signature(&members),
            _ => None,
        };

        let signature = if let Some(callable) = callable {
            if call.type_arguments.is_some() {
                self.explicit_callable_signature(call, scope, callable, callee_type)
                    .or(signature)
            } else {
                self.instantiated_callable_signature(callable, callee_type, &argument_types)
                    .or(signature)
            }
        } else {
            signature
        };

        let Some(signature) = signature else {
            return;
        };

        let argument_count = argument_types.len();
        let (required, total, rest_index) = signature.arity();
        if argument_count < required {
            self.emit(
                ARGUMENT_COUNT_MISMATCH,
                call_range,
                ARGUMENT_COUNT_MISMATCH_MESSAGE,
            );
            return;
        }
        if rest_index.is_none() && argument_count > total {
            let range = argument_ranges.get(total).copied().unwrap_or(call_range);
            self.emit(
                ARGUMENT_COUNT_MISMATCH,
                range,
                ARGUMENT_COUNT_MISMATCH_MESSAGE,
            );
            return;
        }

        let parameter_types: Vec<TypeId> = signature
            .parameters()
            .iter()
            .map(FunctionParameter::type_id)
            .collect();
        for (index, (argument_type, argument_range)) in
            argument_types.iter().zip(&argument_ranges).enumerate()
        {
            let parameter_index = if let Some(rest) = rest_index {
                if index >= rest { rest } else { index }
            } else {
                index
            };
            if *argument_type == self.types.undefined_type()
                && let Some(parameter) = signature.parameters().get(parameter_index)
                && parameter.optional()
            {
                continue;
            }
            let mut target = parameter_types
                .get(parameter_index)
                .copied()
                .unwrap_or_else(|| self.types.any());
            if rest_index == Some(parameter_index)
                && let Some(element) = self.array_element_type(target)
            {
                target = element;
            }
            if !self.types_assignable(*argument_type, target) {
                self.emit(
                    ARGUMENT_NOT_ASSIGNABLE,
                    *argument_range,
                    ARGUMENT_NOT_ASSIGNABLE_MESSAGE,
                );
            }
        }
    }

    /// Resolves a value callable (function declaration or function/arrow
    /// initializer) for an actual call, inferring type arguments from the
    /// already-resolved callee signature. Returns `None` when the callable is
    /// not generic or cannot be resolved.
    fn instantiated_callable_signature(
        &mut self,
        callable: JsxCallable<'src>,
        callee_type: TypeId,
        argument_types: &[TypeId],
    ) -> Option<FunctionSignature> {
        let (type_parameters, _, _) = callable.parts();
        type_parameters.filter(|list| !list.parameters.is_empty())?;
        let Type::Function(signature) = self.types.get(callee_type).clone() else {
            return None;
        };

        let mut inference_symbols = Vec::new();
        for parameter in signature.parameters() {
            self.collect_type_parameter_symbols(parameter.type_id(), &mut inference_symbols);
        }
        self.collect_type_parameter_symbols(signature.return_type(), &mut inference_symbols);
        if inference_symbols.is_empty() {
            return Some(signature);
        }

        let inference_parameters: Vec<_> = inference_symbols
            .iter()
            .map(|symbol| InferenceParameter::new(*symbol))
            .collect();
        let temp_signature = signature.clone();
        let mut context = InferenceContext::new(&mut self.types, &inference_parameters);
        context.infer_from_arguments(&temp_signature, argument_types);
        let inferred = context.resolve();

        let mut instantiated_parameters = Vec::with_capacity(signature.parameters().len());
        for parameter in signature.parameters() {
            let type_id = inferred.instantiate(&mut self.types, parameter.type_id());
            instantiated_parameters.push(FunctionParameter::new(
                parameter.name().to_owned(),
                type_id,
                parameter.optional(),
                parameter.rest(),
            ));
        }
        let instantiated_return = inferred.instantiate(&mut self.types, signature.return_type());
        Some(FunctionSignature {
            type_parameters: Vec::new(),
            parameters: instantiated_parameters,
            return_type: instantiated_return,
        })
    }
    fn explicit_callable_signature(
        &mut self,
        call: &'src CallExpression,
        scope: ScopeId,
        callable: JsxCallable<'src>,
        callee_type: TypeId,
    ) -> Option<FunctionSignature> {
        let Type::Function(signature) = self.types.get(callee_type).clone() else {
            return None;
        };
        let (type_parameters, _, _) = callable.parts();
        let Some(list) = type_parameters else {
            return Some(signature);
        };
        let explicit = self.resolve_type_arguments(call.type_arguments.as_ref(), scope);
        let mut inference_symbols = Vec::new();
        self.collect_type_parameter_symbols(callee_type, &mut inference_symbols);
        let mut inferred = Vec::new();
        for (index, param) in list.parameters.iter().enumerate() {
            let name = self.identifier_text(&param.data().name);
            let Some(symbol) = inference_symbols
                .iter()
                .copied()
                .find(|s| self.symbols[s.get() as usize].name() == name.as_ref())
            else {
                continue;
            };
            let type_id = explicit
                .get(index)
                .copied()
                .unwrap_or_else(|| self.types.any());
            inferred.push(InferredTypeArgument::new(
                symbol,
                type_id,
                InferenceProvenance::Explicit,
            ));
        }
        if inferred.is_empty() {
            return Some(signature);
        }
        let inferred = InferredTypeArguments::new(inferred);
        let mut instantiated_parameters = Vec::with_capacity(signature.parameters().len());
        for parameter in signature.parameters() {
            let type_id = inferred.instantiate(&mut self.types, parameter.type_id());
            instantiated_parameters.push(FunctionParameter::new(
                parameter.name().to_owned(),
                type_id,
                parameter.optional(),
                parameter.rest(),
            ));
        }
        let instantiated_return = inferred.instantiate(&mut self.types, signature.return_type());
        Some(FunctionSignature {
            type_parameters: Vec::new(),
            parameters: instantiated_parameters,
            return_type: instantiated_return,
        })
    }

    fn collect_type_parameter_symbols(&self, type_id: TypeId, out: &mut Vec<SymbolId>) {
        match self.types.get(type_id) {
            Type::Named(symbol) => {
                if self.symbols[symbol.get() as usize].kind == SymbolKind::TypeParameter
                    && !out.contains(symbol)
                {
                    out.push(*symbol);
                }
            }
            Type::Array(element) => self.collect_type_parameter_symbols(*element, out),
            Type::Union(members) => {
                for member in members {
                    self.collect_type_parameter_symbols(*member, out);
                }
            }
            Type::ObjectType(properties) => {
                for property in properties {
                    self.collect_type_parameter_symbols(property.type_id(), out);
                }
            }
            Type::Function(signature) => {
                for parameter in signature.parameters() {
                    self.collect_type_parameter_symbols(parameter.type_id(), out);
                }
                self.collect_type_parameter_symbols(signature.return_type(), out);
            }
            _ => {}
        }
    }
    fn union_call_signature(&self, members: &[TypeId]) -> Option<FunctionSignature> {
        let mut signatures = Vec::new();
        if !self.collect_function_signatures(members, &mut signatures) {
            return None;
        }
        let mut required = 0;
        let mut total = 0;
        let mut has_rest = false;
        for signature in &signatures {
            let (signature_required, signature_total, rest_index) = signature.arity();
            required = required.max(signature_required);
            if rest_index.is_some() {
                has_rest = true;
            } else {
                total = total.max(signature_total);
            }
        }
        let any = self.types.any();
        if has_rest {
            total = required + 1;
        }
        let mut parameters = Vec::with_capacity(total);
        for index in 0..total {
            let optional = index >= required;
            let rest = has_rest && index == required;
            parameters.push(FunctionParameter::new(
                format!("arg{index}"),
                any,
                optional,
                rest,
            ));
        }
        Some(FunctionSignature {
            type_parameters: Vec::new(),
            parameters,
            return_type: any,
        })
    }

    fn collect_function_signatures(
        &self,
        members: &[TypeId],
        signatures: &mut Vec<FunctionSignature>,
    ) -> bool {
        for member in members {
            match self.types.get(*member) {
                Type::Function(signature) => signatures.push(signature.clone()),
                Type::Union(nested) => {
                    if !self.collect_function_signatures(nested, signatures) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        true
    }

    fn array_element_type(&self, array_type: TypeId) -> Option<TypeId> {
        if let Type::Array(element) = self.types.get(array_type) {
            Some(*element)
        } else {
            None
        }
    }

    fn type_of_assignment_target(
        &mut self,
        target: &'src crate::syntax::AssignmentTargetNode,
        scope: ScopeId,
    ) -> TypeId {
        match target.data() {
            AssignmentTarget::Identifier(identifier) => self
                .references
                .get(&identifier.id())
                .or_else(|| self.reference_aliases.get(&target.id()))
                .map_or_else(
                    || self.types.any(),
                    |symbol| self.symbol_types[symbol.get() as usize],
                ),
            AssignmentTarget::Member(member) => {
                self.type_of_member(&member.object, &member.property, scope)
            }
            _ => self.types.any(),
        }
    }

    fn type_of_member(
        &mut self,
        object: &'src Expr,
        property: &MemberProperty,
        scope: ScopeId,
    ) -> TypeId {
        let Some(name) = enum_plan::cook_member_property_name(self.source, property) else {
            return self.types.any();
        };
        let name_str = name.to_utf8_lossy();
        if let Some(object_symbol) = self.resolved_expression_reference(object) {
            let kind = self.symbols[object_symbol.get() as usize].kind;
            if kind == SymbolKind::Namespace
                && let Some(member_scope) = self.container_member_scope(object_symbol)
                && let Some(member_symbol) = self.scopes[member_scope.0 as usize]
                    .value(name_str.as_str())
                    .or_else(|| {
                        self.scopes[member_scope.0 as usize].type_binding(name_str.as_str())
                    })
            {
                return self.symbol_types[member_symbol.get() as usize];
            } else if kind == SymbolKind::Enum
                && let Some(member_symbol) = self
                    .enum_member_symbols_by_name
                    .get(&object_symbol)
                    .and_then(|members| members.get(&name))
                    .copied()
            {
                return self.symbol_types[member_symbol.get() as usize];
            }
        }
        let object_type = self.type_of_expr(object, scope);
        let property_range = match property {
            MemberProperty::Named(identifier) => identifier.range(),
            MemberProperty::Private(identifier) => identifier.range(),
            MemberProperty::Computed(expression) => expression.range(),
        };
        if let Some(type_id) =
            self.property_type_for_member(object_type, name_str.as_str(), property_range)
        {
            return type_id;
        }
        self.types.any()
    }

    fn property_type_for_member(
        &mut self,
        object_type: TypeId,
        name: &str,
        range: TextRange,
    ) -> Option<TypeId> {
        match self.types.get(object_type).clone() {
            Type::ObjectType(_) => {
                if let Some(type_id) = self.types.property_type(object_type, name) {
                    Some(type_id)
                } else if self.is_typescript() {
                    self.emit(
                        PROPERTY_DOES_NOT_EXIST,
                        range,
                        PROPERTY_DOES_NOT_EXIST_MESSAGE,
                    );
                    Some(self.types.error_type())
                } else {
                    Some(self.types.any())
                }
            }
            Type::Named(symbol) => {
                if let Some(instance_type) = self.class_instance_types.get(&symbol).copied() {
                    return self.property_type_for_member(instance_type, name, range);
                }
                let resolved = self.resolve_named_type_symbol(symbol);
                if resolved == object_type {
                    None
                } else {
                    self.property_type_for_member(resolved, name, range)
                }
            }
            Type::Union(members) => {
                let mut found = Vec::with_capacity(members.len());
                for member in members {
                    let member_property = self.property_type_for_member(member, name, range)?;
                    if member_property == self.types.error_type() {
                        return Some(self.types.error_type());
                    }
                    found.push(member_property);
                }
                if found.len() == 1 {
                    Some(found[0])
                } else {
                    Some(self.types.union(&found))
                }
            }
            _ => None,
        }
    }

    fn resolve_assignment_target(
        &mut self,
        target: &'src crate::syntax::AssignmentTargetNode,
        scope: ScopeId,
    ) {
        match target.data() {
            AssignmentTarget::Identifier(identifier) => {
                let name = self.identifier_text(identifier);
                if let Some(symbol) = self.lookup_value(scope, &name) {
                    self.uninitialized_variables.remove(&symbol);
                }
                self.resolve_value(identifier, target.id(), scope);
                if let Some(symbol) = self.references.get(&identifier.id()).copied() {
                    match self.symbols[symbol.get() as usize].kind {
                        SymbolKind::Function => self.emit(
                            ASSIGNMENT_TO_FUNCTION,
                            identifier.range(),
                            ASSIGNMENT_TO_FUNCTION_MESSAGE,
                        ),
                        SymbolKind::Namespace => self.emit(
                            ASSIGNMENT_TO_NAMESPACE,
                            identifier.range(),
                            ASSIGNMENT_TO_NAMESPACE_MESSAGE,
                        ),
                        _ => {}
                    }
                }
            }
            AssignmentTarget::Member(member) => {
                if !matches!(member.object.data(), Expression::Super) {
                    self.resolve_expr(&member.object, scope);
                }
                let object_symbol = self.resolved_expression_reference(&member.object);
                if let MemberProperty::Computed(inner) = &member.property {
                    self.resolve_expr(inner, scope);
                }
                if let Some(enum_symbol) = object_symbol
                    && self.symbols[enum_symbol.get() as usize].kind == SymbolKind::Enum
                {
                    self.local_enum_member_targets
                        .insert(target.id(), enum_symbol);
                    return;
                }
                let Some(name) =
                    enum_plan::cook_member_property_name(self.source, &member.property)
                else {
                    return;
                };
                let base = object_symbol
                    .filter(|symbol| self.symbols[symbol.get() as usize].kind == SymbolKind::Import)
                    .map(enum_plan::ImportedEnumMemberBase::Import)
                    .or_else(|| {
                        self.imported_enum_member_uses
                            .contains_key(&member.object.id())
                            .then_some(enum_plan::ImportedEnumMemberBase::MemberResult(
                                member.object.id(),
                            ))
                    });
                if let Some(base) = base {
                    self.imported_enum_member_uses.insert(
                        target.id(),
                        enum_plan::ImportedEnumMemberUse::new(base, name, target.range()),
                    );
                    self.imported_enum_member_targets.insert(target.id());
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
                    match element {
                        crate::syntax::AssignmentArrayElement::Target(inner) => {
                            self.resolve_assignment_target(inner, scope);
                        }
                        crate::syntax::AssignmentArrayElement::Missing(_) => {
                            self.emit(
                                INVALID_ASSIGNMENT_TARGET,
                                target.range(),
                                INVALID_ASSIGNMENT_TARGET_MESSAGE,
                            );
                        }
                        _ => {}
                    }
                }
            }
            AssignmentTarget::Missing(_) => {
                self.emit(
                    INVALID_ASSIGNMENT_TARGET,
                    target.range(),
                    INVALID_ASSIGNMENT_TARGET_MESSAGE,
                );
            }
        }
    }

    fn resolve_value(&mut self, identifier: &IdentifierNode, reference: NodeId, scope: ScopeId) {
        let name = self.identifier_text(identifier);
        if name.is_empty() {
            return;
        }
        if let Some(symbol) = self.lookup_value(scope, &name) {
            self.references.insert(identifier.id(), symbol);
            self.symbol_references.push((identifier.range(), symbol));
            if reference != identifier.id() {
                self.reference_aliases.insert(reference, symbol);
            }
            if self.symbols[symbol.get() as usize].kind == SymbolKind::EnumMember {
                self.enum_member_identifier_uses.insert(reference);
            }
            if let Some(declaration) = self.active_namespace_declarations.last().copied() {
                self.namespace_reference_blocks
                    .insert(reference, declaration);
            }
            if self.strict_null_checks
                && !self.suppress_used_before_assigned
                && self.uninitialized_variables.contains(&symbol)
                && self.boundary_scope(scope)
                    == self.boundary_scope(self.symbols[symbol.get() as usize].scope())
            {
                self.emit(
                    USED_BEFORE_ASSIGNED,
                    identifier.range(),
                    USED_BEFORE_ASSIGNED_MESSAGE,
                );
            }
        } else if !self.suppresses_unresolved_value(scope) {
            self.emit(
                CANNOT_FIND_NAME,
                identifier.range(),
                CANNOT_FIND_NAME_MESSAGE,
            );
        }
    }

    /// Returns whether an unresolved value reference may bind to a sloppy `with`
    /// object at runtime instead of a lexical binding.
    pub(crate) fn suppresses_unresolved_value(&self, scope: ScopeId) -> bool {
        let mut current = Some(scope);
        while let Some(id) = current {
            let scope = &self.scopes[id.0 as usize];
            match scope.kind {
                ScopeKind::With => return true,
                ScopeKind::Module | ScopeKind::Global | ScopeKind::Namespace => return false,
                ScopeKind::Function
                | ScopeKind::Class
                | ScopeKind::Block
                | ScopeKind::For
                | ScopeKind::Catch => {}
            }
            current = scope.parent;
        }
        false
    }
    /// Returns the nearest scope that is a function/class boundary or a script
    /// unit (global, module, namespace). Used-before-assigned diagnostics are
    /// only emitted when the declaration and the use share the same boundary.
    fn boundary_scope(&self, mut scope: ScopeId) -> ScopeId {
        loop {
            let kind = self.scopes[scope.0 as usize].kind;
            match kind {
                ScopeKind::Global | ScopeKind::Module | ScopeKind::Namespace => return scope,
                ScopeKind::Function => {
                    if let Some(parent) = self.scopes[scope.0 as usize].parent
                        && self.scopes[parent.0 as usize].kind == ScopeKind::Namespace
                    {
                        scope = parent;
                        continue;
                    }
                    return scope;
                }
                ScopeKind::Class => return scope,
                ScopeKind::Block | ScopeKind::For | ScopeKind::Catch | ScopeKind::With => {
                    scope = self.scopes[scope.0 as usize]
                        .parent
                        .expect("scope chain reaches global");
                }
            }
        }
    }

    fn resolved_expression_reference(&self, expression: &Expr) -> Option<SymbolId> {
        self.references
            .get(&expression.id())
            .or_else(|| self.reference_aliases.get(&expression.id()))
            .copied()
    }

    pub(crate) fn lookup_value(&self, scope: ScopeId, name: &str) -> Option<SymbolId> {
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

    pub(crate) fn lookup_type(&self, scope: ScopeId, name: &str) -> Option<SymbolId> {
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

    pub(crate) fn resolve_type(&mut self, node: &'src Ty, scope: ScopeId) -> TypeId {
        let resolved = match node.data() {
            TypeNode::Keyword(keyword) => self.keyword_type(*keyword),
            TypeNode::Literal(literal) => self.literal_type(literal),
            TypeNode::Reference(reference) => {
                self.resolve_type_reference(reference, scope, node.id(), node.range())
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
            TypeNode::Query(query) => {
                if let Some(arguments) = &query.type_arguments {
                    for argument in &arguments.arguments {
                        let _ = self.resolve_type(argument, scope);
                    }
                }
                self.resolve_type_query(query, scope, node.range())
            }
            _ => self.types.error_type(),
        };
        self.type_nodes.insert(node.id(), resolved);
        resolved
    }

    fn resolve_type_query(
        &mut self,
        query: &'src crate::syntax::TypeQuery,
        scope: ScopeId,
        range: TextRange,
    ) -> TypeId {
        match &query.name {
            EntityName::Identifier(identifier) => {
                let name = self.identifier_text(identifier);
                let Some(symbol) = self.lookup_value(scope, &name) else {
                    self.emit(
                        CANNOT_FIND_NAME,
                        identifier.range(),
                        CANNOT_FIND_NAME_MESSAGE,
                    );
                    return self.types.any();
                };
                self.symbol_types[symbol.get() as usize]
            }
            EntityName::Qualified { left, right } => {
                let (member_scope, _path) = match self.resolve_entity_name_scope(left, scope) {
                    Ok(resolved) => resolved,
                    Err(EntityNameScopeError::NotNamespace) => {
                        self.emit(CANNOT_FIND_NAMESPACE, range, CANNOT_FIND_NAMESPACE_MESSAGE);
                        return self.types.error_type();
                    }
                    Err(EntityNameScopeError::MissingMember(missing_range)) => {
                        self.emit(CANNOT_FIND_NAME, missing_range, CANNOT_FIND_NAME_MESSAGE);
                        return self.types.error_type();
                    }
                    Err(EntityNameScopeError::Unresolved) => {
                        if self.entity_name_identifier_is_unresolved(left, scope) {
                            self.emit(
                                CANNOT_FIND_NAME,
                                self.entity_name_range(left),
                                CANNOT_FIND_NAME_MESSAGE,
                            );
                        }
                        return self.types.error_type();
                    }
                };
                let name = self.identifier_text(right);
                let Some(symbol) = self.scopes[member_scope.0 as usize].value(&name) else {
                    self.emit(CANNOT_FIND_NAME, right.range(), CANNOT_FIND_NAME_MESSAGE);
                    return self.types.error_type();
                };
                self.symbol_types[symbol.get() as usize]
            }
            EntityName::Missing(_) => self.types.error_type(),
        }
    }

    fn keyword_type(&mut self, keyword: KeywordType) -> TypeId {
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
        reference_id: NodeId,
        range: TextRange,
    ) -> TypeId {
        let explicit_arguments: Option<Vec<TypeId>> =
            reference.type_arguments.as_ref().map(|list| {
                list.arguments
                    .iter()
                    .map(|argument| self.resolve_type(argument, scope))
                    .collect()
            });
        match &reference.name {
            EntityName::Identifier(identifier) => {
                let name = self.identifier_text(identifier);
                match self.lookup_type(scope, &name) {
                    Some(symbol) => {
                        self.symbol_references.push((identifier.range(), symbol));
                        let base = self.resolve_named_type_symbol(symbol);
                        self.instantiate_explicit_type_arguments(
                            symbol,
                            explicit_arguments.as_deref(),
                            base,
                        )
                    }
                    None => {
                        self.emit(CANNOT_FIND_TYPE, range, CANNOT_FIND_TYPE_MESSAGE);
                        self.types.error_type()
                    }
                }
            }
            EntityName::Qualified { left, right } => {
                let (member_scope, mut path) = match self.resolve_entity_name_scope(left, scope) {
                    Ok(resolved) => resolved,
                    Err(EntityNameScopeError::NotNamespace) => {
                        self.emit(CANNOT_FIND_NAMESPACE, range, CANNOT_FIND_NAMESPACE_MESSAGE);
                        return self.types.error_type();
                    }
                    Err(EntityNameScopeError::MissingMember(missing_range)) => {
                        self.emit(CANNOT_FIND_TYPE, missing_range, CANNOT_FIND_TYPE_MESSAGE);
                        return self.types.error_type();
                    }
                    Err(EntityNameScopeError::Unresolved) => return self.types.error_type(),
                };
                let name = self.identifier_text(right);
                let Some(symbol) = self.scopes[member_scope.0 as usize].type_binding(&name) else {
                    self.emit(CANNOT_FIND_TYPE, right.range(), CANNOT_FIND_TYPE_MESSAGE);
                    return self.types.error_type();
                };
                path.push(symbol);
                if reference_id != NodeId::default() {
                    self.namespace_qualified_type_paths
                        .insert(reference_id, path.into_boxed_slice());
                }
                let base = self.resolve_named_type_symbol(symbol);
                self.instantiate_explicit_type_arguments(
                    symbol,
                    explicit_arguments.as_deref(),
                    base,
                )
            }
            EntityName::Missing(_) => {
                self.emit(CANNOT_FIND_TYPE, range, CANNOT_FIND_TYPE_MESSAGE);
                self.types.error_type()
            }
        }
    }

    fn instantiate_explicit_type_arguments(
        &mut self,
        symbol: SymbolId,
        arguments: Option<&[TypeId]>,
        base: TypeId,
    ) -> TypeId {
        let Some(arguments) = arguments else {
            return base;
        };
        let Some(definition) = self.type_defs.get(&symbol).copied() else {
            return base;
        };
        let (type_scope, type_parameters) = match definition {
            TypeDef::Alias {
                scope,
                type_parameters,
                ..
            } => (scope, type_parameters),
            TypeDef::Interface {
                scope,
                type_parameters,
                ..
            } => (scope, type_parameters),
            TypeDef::Enum { .. } => return base,
        };
        let Some(list) = type_parameters else {
            return base;
        };
        let parameters = &list.parameters;
        if parameters.is_empty() {
            return base;
        }
        let scope_entry = self.scopes.get(type_scope.0 as usize);
        let scope = match scope_entry {
            Some(scope) => scope,
            None => return base,
        };
        let mut inferred = Vec::with_capacity(parameters.len());
        for (index, parameter) in parameters.iter().enumerate() {
            let name = self.identifier_text(&parameter.data().name);
            let Some(parameter_symbol) = scope.types.get(name.as_ref()).copied() else {
                continue;
            };
            let type_id = arguments
                .get(index)
                .copied()
                .unwrap_or_else(|| self.types.any());
            inferred.push(InferredTypeArgument::new(
                parameter_symbol,
                type_id,
                InferenceProvenance::Explicit,
            ));
        }
        if inferred.is_empty() {
            return base;
        }
        let instantiation = InferredTypeArguments::new(inferred);
        instantiation.instantiate(&mut self.types, base)
    }

    fn resolve_named_type_symbol(&mut self, symbol: SymbolId) -> TypeId {
        match self.symbols[symbol.get() as usize].kind {
            SymbolKind::Interface | SymbolKind::TypeAlias | SymbolKind::Enum => {
                self.resolve_type_symbol(symbol)
            }
            SymbolKind::Class | SymbolKind::TypeParameter => self.types.named(symbol),
            // `Object` is the only intrinsic type the table models nominally, because
            // relations knows it is the top object type. The other intrinsics
            // (`Record`, `Promise`, `Iterable`, ...) have no structural definition yet.
            // A nominal target that no structural source can satisfy rejects valid
            // code, so they keep the permissive error type until they are modelled.
            SymbolKind::IntrinsicType if self.types.is_object_symbol(symbol) => {
                self.types.named(symbol)
            }
            SymbolKind::Import => self.resolve_import_equals_type_symbol(symbol),
            _ => self.types.error_type(),
        }
    }

    fn resolve_import_equals_type_symbol(&mut self, symbol: SymbolId) -> TypeId {
        match self.type_state[symbol.get() as usize] {
            TypeState::Done(id) => return id,
            TypeState::InProgress => return self.types.error_type(),
            TypeState::Unresolved => {}
        }
        self.type_state[symbol.get() as usize] = TypeState::InProgress;
        let target = self
            .import_equals_targets
            .get(&symbol)
            .and_then(|target| target.ty.or(target.value));
        let resolved = match target {
            Some(target) => self.resolve_named_type_symbol(target),
            None => self.types.error_type(),
        };
        self.type_state[symbol.get() as usize] = TypeState::Done(resolved);
        resolved
    }

    fn resolve_entity_name_scope(
        &self,
        name: &EntityName,
        scope: ScopeId,
    ) -> Result<(ScopeId, Vec<SymbolId>), EntityNameScopeError> {
        match name {
            EntityName::Identifier(identifier) => {
                let name = self.identifier_text(identifier);
                let Some(symbol) = self
                    .lookup_value(scope, &name)
                    .or_else(|| self.lookup_type(scope, &name))
                else {
                    return Err(EntityNameScopeError::Unresolved);
                };
                let member_scope = self.entity_name_member_scope(symbol)?;
                Ok((member_scope, vec![symbol]))
            }
            EntityName::Qualified { left, right } => {
                let (member_scope, mut path) = self.resolve_entity_name_scope(left, scope)?;
                let name = self.identifier_text(right);
                let Some(symbol) = self.scopes[member_scope.0 as usize]
                    .value(&name)
                    .or_else(|| self.scopes[member_scope.0 as usize].type_binding(&name))
                else {
                    return Err(EntityNameScopeError::MissingMember(right.range()));
                };
                let child_scope = self.entity_name_member_scope(symbol)?;
                path.push(symbol);
                Ok((child_scope, path))
            }
            EntityName::Missing(_) => Err(EntityNameScopeError::Unresolved),
        }
    }

    fn resolve_qualified_import_equals(
        &mut self,
        declaration: NodeId,
        name: &'src EntityName,
        scope: ScopeId,
    ) {
        let alias = self.import_equals_symbols.get(&declaration).copied();
        let range = alias
            .map(|symbol| self.symbols[symbol.get() as usize].range)
            .unwrap_or_else(NodeId::default_range);
        match name {
            EntityName::Identifier(identifier) => {
                let text = self.identifier_text(identifier);
                let value = self.lookup_value(scope, &text);
                let ty = self.lookup_type(scope, &text);
                let Some(member) = value.or(ty) else {
                    self.emit(
                        CANNOT_FIND_NAME,
                        identifier.range(),
                        CANNOT_FIND_NAME_MESSAGE,
                    );
                    return;
                };
                self.qualified_import_paths
                    .insert(declaration, Box::new([member]));
                if let Some(alias) = alias {
                    self.import_equals_targets
                        .insert(alias, ImportEqualsTarget { value, ty });
                }
            }
            EntityName::Qualified { left, right } => {
                let (member_scope, mut path) = match self.resolve_entity_name_scope(left, scope) {
                    Ok(resolved) => resolved,
                    Err(EntityNameScopeError::NotNamespace) => {
                        self.emit(CANNOT_FIND_NAMESPACE, range, CANNOT_FIND_NAMESPACE_MESSAGE);
                        return;
                    }
                    Err(EntityNameScopeError::MissingMember(missing_range)) => {
                        self.emit(CANNOT_FIND_NAME, missing_range, CANNOT_FIND_NAME_MESSAGE);
                        return;
                    }
                    Err(EntityNameScopeError::Unresolved) => {
                        if self.entity_name_identifier_is_unresolved(left, scope) {
                            self.emit(
                                CANNOT_FIND_NAMESPACE,
                                self.entity_name_range(left),
                                CANNOT_FIND_NAMESPACE_MESSAGE,
                            );
                        }
                        return;
                    }
                };
                let member_name = self.identifier_text(right);
                let value = self.scopes[member_scope.0 as usize].value(&member_name);
                let ty = self.scopes[member_scope.0 as usize].type_binding(&member_name);
                let Some(member) = value.or(ty) else {
                    self.emit(CANNOT_FIND_NAME, right.range(), CANNOT_FIND_NAME_MESSAGE);
                    return;
                };
                path.push(member);
                self.qualified_import_paths
                    .insert(declaration, path.into_boxed_slice());
                if let Some(alias) = alias {
                    self.import_equals_targets
                        .insert(alias, ImportEqualsTarget { value, ty });
                }
            }
            EntityName::Missing(_) => {}
        }
    }

    fn entity_name_range(&self, name: &EntityName) -> TextRange {
        match name {
            EntityName::Identifier(identifier) => identifier.range(),
            EntityName::Qualified { left, .. } => self.entity_name_range(left),
            EntityName::Missing(_) => NodeId::default_range(),
        }
    }

    fn entity_name_identifier_is_unresolved(&self, name: &EntityName, scope: ScopeId) -> bool {
        match name {
            EntityName::Identifier(identifier) => {
                let text = self.identifier_text(identifier);
                self.lookup_value(scope, &text).is_none()
                    && self.lookup_type(scope, &text).is_none()
            }
            EntityName::Qualified { left, .. } => {
                self.entity_name_identifier_is_unresolved(left, scope)
            }
            EntityName::Missing(_) => true,
        }
    }

    fn entity_name_member_scope(&self, symbol: SymbolId) -> Result<ScopeId, EntityNameScopeError> {
        let Some(member_scope) = self.container_member_scope(symbol) else {
            return Err(match self.symbols[symbol.get() as usize].kind {
                SymbolKind::Namespace | SymbolKind::Enum => EntityNameScopeError::Unresolved,
                SymbolKind::Import | SymbolKind::IntrinsicValue | SymbolKind::IntrinsicType => {
                    EntityNameScopeError::Unresolved
                }
                _ => EntityNameScopeError::NotNamespace,
            });
        };
        Ok(member_scope)
    }

    fn direct_container_member_scope(&self, symbol: SymbolId) -> Option<ScopeId> {
        self.namespace_export_scopes
            .get(&symbol)
            .or_else(|| self.enum_member_scopes.get(&symbol))
            .copied()
    }

    pub(crate) fn container_member_scope(&self, symbol: SymbolId) -> Option<ScopeId> {
        let mut pending = vec![symbol];
        let mut seen = HashSet::new();
        while let Some(current) = pending.pop() {
            if !seen.insert(current) {
                continue;
            }
            if let Some(member_scope) = self.direct_container_member_scope(current) {
                return Some(member_scope);
            }
            if self.symbols[current.get() as usize].kind != SymbolKind::Import {
                continue;
            }
            let Some(target) = self.import_equals_targets.get(&current) else {
                continue;
            };
            // Push type then value so the value plane is tried first.
            if let Some(ty) = target.ty {
                pending.push(ty);
            }
            if let Some(value) = target.value {
                pending.push(value);
            }
        }
        None
    }

    pub(crate) fn resolve_type_symbol(&mut self, symbol: SymbolId) -> TypeId {
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
            let base_type = self.resolve_type_reference(
                base,
                scope,
                NodeId::default(),
                NodeId::default_range(),
            );
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
                    self.resolve_property_name(&property.name, scope);
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
                    self.resolve_property_name(&method.name, scope);
                    if let Some(name) = self.property_key(&method.name) {
                        if self.no_implicit_any && method.function.return_type_missing {
                            self.emit(
                                MISSING_METHOD_RETURN_TYPE,
                                member.range(),
                                MISSING_METHOD_RETURN_TYPE_MESSAGE,
                            );
                        }
                        let type_id = self.resolve_function_type(&method.function, scope);
                        properties.push(PropertyType::new(name, method.optional, type_id));
                    }
                }
                TypeMember::Call(call) => {
                    if self.no_implicit_any && call.function.return_type_missing {
                        self.emit(
                            MISSING_METHOD_RETURN_TYPE,
                            member.range(),
                            MISSING_METHOD_RETURN_TYPE_MESSAGE,
                        );
                    }
                    let _ = self.resolve_function_type(&call.function, scope);
                }
                TypeMember::Construct(construct) => {
                    if self.no_implicit_any && construct.function.function.return_type_missing {
                        self.emit(
                            MISSING_METHOD_RETURN_TYPE,
                            member.range(),
                            MISSING_METHOD_RETURN_TYPE_MESSAGE,
                        );
                    }
                    let _ = self.resolve_function_type(&construct.function.function, scope);
                }
                _ => {}
            }
        }
        properties
    }

    fn resolve_function_type(&mut self, function: &'src FunctionType, scope: ScopeId) -> TypeId {
        let child = self.new_scope(ScopeKind::Function, Some(scope));
        self.bind_type_parameters(function.type_parameters.as_ref(), child);
        let mut parameters: Vec<FunctionParameter> = Vec::with_capacity(function.parameters.len());
        for parameter in &function.parameters {
            if self.identifier_text(&parameter.name).as_ref() == "this" {
                continue;
            }
            let type_id = self.resolve_type(&parameter.type_annotation.data().type_node, child);
            parameters.push(FunctionParameter::new(
                self.identifier_text(&parameter.name).into_owned(),
                type_id,
                parameter.optional,
                parameter.rest,
            ));
        }
        let return_type = self.resolve_type(&function.return_type, child);
        let type_parameters = function
            .type_parameters
            .as_ref()
            .map(|list| {
                list.parameters
                    .iter()
                    .filter_map(|param| {
                        let name = self.identifier_text(&param.data().name);
                        self.lookup_type(child, &name)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.types
            .function_with_parameters(type_parameters, parameters, return_type)
    }

    fn property_key(&self, name: &PropertyName) -> Option<String> {
        match name {
            PropertyName::Identifier(identifier) => {
                Some(self.identifier_text(identifier).into_owned())
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

    pub(crate) fn type_of_expr(&mut self, expression: &'src Expr, scope: ScopeId) -> TypeId {
        if let Some(&cached) = self.node_types.get(&expression.id()) {
            return cached;
        }
        let result = self.compute_type_of_expr(expression, scope);
        if self.node_types.insert(expression.id(), result).is_none() {
            self.typed_expressions.push((expression.range(), result));
        }
        result
    }

    fn compute_type_of_expr(&mut self, expression: &'src Expr, scope: ScopeId) -> TypeId {
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
            Expression::Assignment(assignment)
                if assignment.operator == AssignmentOperator::Assign =>
            {
                self.type_of_expr(&assignment.right, scope)
            }
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
                let mut accessors: BTreeMap<String, (Option<TypeId>, Option<TypeId>)> =
                    BTreeMap::new();
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
                                match method.modifier {
                                    PropertyModifier::Get => {
                                        let return_type =
                                            self.inferred_return_type(&method.function, scope);
                                        accessors.entry(name).or_default().0 = Some(return_type);
                                    }
                                    PropertyModifier::Set => {
                                        let method_type =
                                            self.type_of_function_like(&method.function, scope);
                                        let param_type = if let Type::Function(signature) =
                                            self.types.get(method_type)
                                        {
                                            signature
                                                .parameters()
                                                .first()
                                                .map(|parameter| parameter.type_id())
                                                .unwrap_or_else(|| self.types.any())
                                        } else {
                                            self.types.any()
                                        };
                                        accessors.entry(name).or_default().1 = Some(param_type);
                                    }
                                    _ => {
                                        let method_type =
                                            self.type_of_function_like(&method.function, scope);
                                        properties.push(PropertyType::new(
                                            name,
                                            false,
                                            method_type,
                                        ));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                for (name, (get, set)) in accessors {
                    let type_id = get.or(set).unwrap_or_else(|| self.types.any());
                    let readonly = get.is_some() && set.is_none();
                    properties
                        .push(PropertyType::new(name, false, type_id).with_readonly(readonly));
                }
                let object_type = self.types.object_type(properties);
                self.types.widen(object_type, false)
            }
            Expression::JsxElement(_)
            | Expression::JsxSelfClosingElement(_)
            | Expression::JsxFragment(_) => self
                .jsx_element_types
                .get(&expression.id())
                .copied()
                .unwrap_or_else(|| self.types.any()),
            Expression::Conditional(conditional) => {
                let literal_truthy = if let Expression::Literal(Literal::Boolean(literal)) =
                    conditional.test.data()
                {
                    Some(literal.data().token().kind() == TokenKind::KwTrue)
                } else {
                    None
                };
                match literal_truthy {
                    Some(true) => self.type_of_expr(conditional.consequent.as_ref(), scope),
                    Some(false) => self.type_of_expr(conditional.alternate.as_ref(), scope),
                    None => {
                        let consequent = self.type_of_expr(conditional.consequent.as_ref(), scope);
                        let alternate = self.type_of_expr(conditional.alternate.as_ref(), scope);
                        if self.types.assignable(consequent, alternate) {
                            alternate
                        } else if self.types.assignable(alternate, consequent) {
                            consequent
                        } else {
                            self.types.union(&[consequent, alternate])
                        }
                    }
                }
            }
            Expression::Function(function) => self.type_of_function_like(&function.function, scope),
            Expression::Arrow(arrow) => self.type_of_arrow(arrow, scope),
            Expression::Member(member) => {
                self.type_of_member(&member.object, &member.property, scope)
            }
            Expression::New(new) => {
                if let Some(symbol) = self.resolved_expression_reference(&new.callee)
                    && let Some(instance_type) = self.class_instance_types.get(&symbol).copied()
                {
                    return instance_type;
                }
                let callee_type = self.type_of_expr(&new.callee, scope);
                if let Type::Named(symbol) = self.types.get(callee_type)
                    && let Some(instance_type) = self.class_instance_types.get(symbol).copied()
                {
                    return instance_type;
                }
                self.types.any()
            }
            Expression::This => self
                .this_context
                .last()
                .copied()
                .unwrap_or_else(|| self.types.any()),
            Expression::Call(call) => {
                let callee_type = self.type_of_expr(&call.callee, scope);
                let mut argument_types = Vec::new();
                let mut has_spread = false;
                for argument in &call.arguments {
                    match argument {
                        CallArgument::Expression(inner) => {
                            argument_types.push(self.type_of_expr(inner, scope))
                        }
                        CallArgument::Spread(_) => {
                            has_spread = true;
                            break;
                        }
                        CallArgument::Missing(_) => {}
                    }
                }
                if has_spread {
                    return self.types.any();
                }
                if let Some(return_type) =
                    self.call_return_type(call, callee_type, &argument_types, scope)
                {
                    return return_type;
                }
                self.types.any()
            }
            _ => self.types.any(),
        }
    }
    fn type_of_function_like(&mut self, function: &'src FunctionLike, parent: ScopeId) -> TypeId {
        let scope = self.new_scope(ScopeKind::Function, Some(parent));
        self.bind_type_parameters(function.type_parameters.as_ref(), scope);
        self.signature_type(
            function.type_parameters.as_ref(),
            &function.parameters,
            function.return_type.as_ref(),
            scope,
        )
    }

    fn type_of_arrow(&mut self, arrow: &'src ArrowFunction, parent: ScopeId) -> TypeId {
        let scope = self.new_scope(ScopeKind::Function, Some(parent));
        self.bind_type_parameters(arrow.type_parameters.as_ref(), scope);
        self.signature_type(
            arrow.type_parameters.as_ref(),
            &arrow.parameters,
            arrow.return_type.as_ref(),
            scope,
        )
    }

    fn signature_type(
        &mut self,
        type_parameters: Option<&'src crate::syntax::TypeParameterList>,
        parameters: &'src [ParameterNode],
        return_type: Option<&'src TypeAnnotationNode>,
        scope: ScopeId,
    ) -> TypeId {
        let mut function_parameters: Vec<FunctionParameter> = Vec::with_capacity(parameters.len());
        for (idx, parameter) in parameters.iter().enumerate() {
            if self.is_this_parameter(parameter) {
                continue;
            }
            let data = parameter.data();
            let type_id = match &data.type_annotation {
                Some(annotation) => self.resolve_type(&annotation.data().type_node, scope),
                None => self.types.any(),
            };
            let rest = matches!(data.binding.data(), BindingPattern::Rest(_));
            let optional = data.optional || data.initializer.is_some();
            let name = match data.binding.data() {
                BindingPattern::Identifier(identifier) => {
                    self.identifier_text(identifier).into_owned()
                }
                BindingPattern::Rest(rest) => match rest.argument.data() {
                    BindingPattern::Identifier(identifier) => {
                        self.identifier_text(identifier).into_owned()
                    }
                    _ => format!("arg{idx}"),
                },
                BindingPattern::Assignment(assign) => match assign.left.data() {
                    BindingPattern::Identifier(identifier) => {
                        self.identifier_text(identifier).into_owned()
                    }
                    BindingPattern::Rest(rest) => match rest.argument.data() {
                        BindingPattern::Identifier(identifier) => {
                            self.identifier_text(identifier).into_owned()
                        }
                        _ => format!("arg{idx}"),
                    },
                    _ => format!("arg{idx}"),
                },
                _ => format!("arg{idx}"),
            };
            function_parameters.push(FunctionParameter::new(name, type_id, optional, rest));
        }
        let return_type = match return_type {
            Some(annotation) => self.resolve_type(&annotation.data().type_node, scope),
            None => self.types.any(),
        };
        let type_parameters = type_parameters
            .map(|list| {
                list.parameters
                    .iter()
                    .filter_map(|param| {
                        let name = self.identifier_text(&param.data().name);
                        self.lookup_type(scope, &name)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.types
            .function_with_parameters(type_parameters, function_parameters, return_type)
    }

    fn inferred_return_type(&mut self, function: &'src FunctionLike, parent: ScopeId) -> TypeId {
        if let Some(annotation) = &function.return_type {
            return self.resolve_type(&annotation.data().type_node, parent);
        }
        match &function.body {
            Some(FunctionBody::Expression(expression)) => self.type_of_expr(expression, parent),
            Some(body) => {
                let Some(body_id) = body.id() else {
                    return self.types.any();
                };
                let returns = self.return_types.get(&body_id).cloned().unwrap_or_default();
                if returns.is_empty() {
                    self.types.void()
                } else {
                    self.types.union(&returns)
                }
            }
            None => self.types.void(),
        }
    }

    fn call_return_type(
        &mut self,
        call: &'src CallExpression,
        callee_type: TypeId,
        argument_types: &[TypeId],
        scope: ScopeId,
    ) -> Option<TypeId> {
        match self.types.get(callee_type).clone() {
            Type::Function(signature) => {
                let instantiated = if call.type_arguments.is_some() {
                    self.explicit_function_signature(call, scope, &signature, argument_types)
                } else {
                    self.inferred_function_signature(&signature, argument_types)
                };
                let sig = instantiated.unwrap_or(signature);
                Some(sig.return_type())
            }
            Type::Union(members) => self
                .union_call_signature(&members)
                .map(|sig| sig.return_type()),
            _ => None,
        }
    }

    fn explicit_function_signature(
        &mut self,
        call: &'src CallExpression,
        scope: ScopeId,
        signature: &FunctionSignature,
        _argument_types: &[TypeId],
    ) -> Option<FunctionSignature> {
        let Some(type_args) = &call.type_arguments else {
            return Some(signature.clone());
        };
        let explicit = self.resolve_type_arguments(Some(type_args), scope);
        let mut inference_symbols = Vec::new();
        for parameter in signature.parameters() {
            self.collect_type_parameter_symbols(parameter.type_id(), &mut inference_symbols);
        }
        self.collect_type_parameter_symbols(signature.return_type(), &mut inference_symbols);
        let mut deduped: Vec<SymbolId> = Vec::new();
        for sym in inference_symbols {
            if !deduped.contains(&sym) {
                deduped.push(sym);
            }
        }
        if deduped.is_empty() {
            return Some(signature.clone());
        }
        let mut inferred = Vec::new();
        for (index, symbol) in deduped.iter().enumerate() {
            let type_id = explicit
                .get(index)
                .copied()
                .unwrap_or_else(|| self.types.any());
            inferred.push(InferredTypeArgument::new(
                *symbol,
                type_id,
                InferenceProvenance::Explicit,
            ));
        }
        if inferred.is_empty() {
            return Some(signature.clone());
        }
        let inferred = InferredTypeArguments::new(inferred);
        let mut instantiated_parameters = Vec::with_capacity(signature.parameters().len());
        for parameter in signature.parameters() {
            let type_id = inferred.instantiate(&mut self.types, parameter.type_id());
            instantiated_parameters.push(FunctionParameter::new(
                parameter.name().to_owned(),
                type_id,
                parameter.optional(),
                parameter.rest(),
            ));
        }
        let instantiated_return = inferred.instantiate(&mut self.types, signature.return_type());
        Some(FunctionSignature {
            type_parameters: Vec::new(),
            parameters: instantiated_parameters,
            return_type: instantiated_return,
        })
    }

    fn inferred_function_signature(
        &mut self,
        signature: &FunctionSignature,
        argument_types: &[TypeId],
    ) -> Option<FunctionSignature> {
        let mut inference_symbols = Vec::new();
        for parameter in signature.parameters() {
            self.collect_type_parameter_symbols(parameter.type_id(), &mut inference_symbols);
        }
        self.collect_type_parameter_symbols(signature.return_type(), &mut inference_symbols);
        let mut deduped: Vec<SymbolId> = Vec::new();
        for sym in inference_symbols {
            if !deduped.contains(&sym) {
                deduped.push(sym);
            }
        }
        if deduped.is_empty() {
            return Some(signature.clone());
        }
        let inference_parameters: Vec<_> = deduped
            .iter()
            .map(|symbol| InferenceParameter::new(*symbol))
            .collect();
        let mut context = InferenceContext::new(&mut self.types, &inference_parameters);
        context.infer_from_arguments(signature, argument_types);
        let inferred = context.resolve();
        let mut instantiated_parameters = Vec::with_capacity(signature.parameters().len());
        for parameter in signature.parameters() {
            let type_id = inferred.instantiate(&mut self.types, parameter.type_id());
            let widened = self.types.widen(type_id, false);
            instantiated_parameters.push(FunctionParameter::new(
                parameter.name().to_owned(),
                widened,
                parameter.optional(),
                parameter.rest(),
            ));
        }
        let instantiated_return = inferred.instantiate(&mut self.types, signature.return_type());
        let widened_return = self.types.widen(instantiated_return, false);
        Some(FunctionSignature {
            type_parameters: Vec::new(),
            parameters: instantiated_parameters,
            return_type: widened_return,
        })
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

/// Binds one parsed source into its immutable semantic model using the
/// standard intrinsic environment.
pub(crate) fn bind_source(source: &SourceFile) -> (SemanticModel, Vec<Diagnostic>) {
    let mut binder = Binder::new(source);
    binder.run();
    binder.finish()
}

/// Binds one parsed source into its immutable semantic model using an
/// explicit intrinsic environment and module/script classification.
pub(crate) fn bind_source_with_environment(
    source: &SourceFile,
    environment: GlobalEnvironment,
    is_module: bool,
    options: ProgramCheckOptions,
) -> (SemanticModel, Vec<Diagnostic>) {
    let mut binder = Binder::with_environment(source, environment, is_module, options);
    binder.run();
    binder.finish()
}

#[cfg(test)]
mod tests {
    use super::{
        ACCESSOR_THIS_PARAMETER, AMBIENT_IMPLEMENTATION, ARGUMENT_COUNT_MISMATCH,
        ARGUMENT_NOT_ASSIGNABLE, CONSTRUCTOR_TYPE_PARAMETERS, DUPLICATE_DECLARATION,
        EXPRESSION_NOT_CALLABLE, FUNCTION_IMPLEMENTATION_WRONG_NAME,
        FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION, GET_ACCESSOR_NO_RETURN, GET_ACCESSOR_PARAMETERS,
        PROPERTY_NOT_INITIALIZED, PropertyType, SET_ACCESSOR_PARAMETER_INITIALIZER,
        STATEMENT_NOT_ALLOWED_IN_AMBIENT_CONTEXT, ScopeId, ScopeKind, SymbolId, SymbolKind,
        TYPE_NOT_ASSIGNABLE, Type, TypeTable, bind_source,
    };
    use crate::diagnostic::Diagnostic;
    use crate::source::{ScriptKind, SourceId, SourceText};
    use crate::syntax::VariableKind;
    use std::sync::Arc;

    fn source(text: &str) -> Arc<SourceText> {
        Arc::new(SourceText::new(text).expect("test source fits the per-file budget"))
    }

    fn declaration_source(text: &str) -> Arc<SourceText> {
        Arc::new(
            SourceText::new(text)
                .expect("test source fits the per-file budget")
                .with_declaration_file(true),
        )
    }

    fn bound(text: &str) -> (super::SemanticModel, Vec<Diagnostic>) {
        let parsed = crate::parser::parse(crate::scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            source(text),
        ));
        bind_source(parsed.product())
    }

    fn bound_declaration(text: &str) -> (super::SemanticModel, Vec<Diagnostic>) {
        let parsed = crate::parser::parse(crate::scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            declaration_source(text),
        ));
        bind_source(parsed.product())
    }

    #[test]
    fn c052_statement_not_allowed_in_ambient_context() {
        let (_, diagnostics) = bound_declaration("try { } catch (e) { }\n");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code() == STATEMENT_NOT_ALLOWED_IN_AMBIENT_CONTEXT),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn c052_declaration_allowed_in_ambient_context() {
        let (_, diagnostics) = bound_declaration("export interface Foo { x: number; }\n");
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code() == STATEMENT_NOT_ALLOWED_IN_AMBIENT_CONTEXT),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn node_types_record_initializer_expression_types() {
        let (model, diagnostics) = bound("var a: number = 42;\nvar s: string = 'hi';\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let recorded: Vec<&Type> = model
            .typed_expressions()
            .iter()
            .map(|(_, type_id)| model.types().get(*type_id))
            .collect();
        assert!(
            recorded
                .iter()
                .any(|ty| matches!(ty, Type::NumberLiteral(text) if &**text == "42")),
            "number literal 42 recorded: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|ty| matches!(ty, Type::StringLiteral(text) if &**text == "'hi'")),
            "string literal 'hi' recorded: {recorded:?}"
        );
    }

    #[test]
    fn symbol_references_record_resolved_value_and_type_occurrences() {
        let (model, diagnostics) = bound("class C {}\nvar a: C;\nvar b = a;\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        // The value use of `a` and the type use of `C` are resolved references
        // with source ranges; the emitter places records against those ranges.
        let names: Vec<&str> = model
            .symbol_references()
            .iter()
            .map(|(_, symbol)| model.symbol(*symbol).name())
            .collect();
        assert!(
            names.contains(&"a"),
            "value reference `a` recorded: {names:?}"
        );
        assert!(
            names.contains(&"C"),
            "type reference `C` recorded: {names:?}"
        );
        // Every occurrence carries a non-empty range within the source.
        assert!(
            model
                .symbol_references()
                .iter()
                .all(|(range, _)| !range.is_empty()),
            "reference occurrences carry non-empty ranges"
        );
    }

    #[test]
    fn node_type_answers_by_node_id_for_recorded_expression() {
        let parsed = crate::parser::parse(crate::scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            source("var a: number = 42;\n"),
        ));
        let file = parsed.product();
        let (model, _diagnostics) = bind_source(file);
        let crate::syntax::Statement::Variable(variable) = file.statements()[0].data() else {
            panic!("first statement is a variable declaration");
        };
        let initializer = variable.declarations[0]
            .data()
            .initializer
            .as_ref()
            .expect("declarator has an initializer");
        let type_id = model
            .node_type(initializer.id())
            .expect("node_type records the initializer expression");
        assert!(
            matches!(model.types().get(type_id), Type::NumberLiteral(text) if &**text == "42"),
            "initializer types as the numeric literal 42"
        );
        // A node the walk never typed has no recorded type.
        assert_eq!(model.node_type(crate::syntax::NodeId::default()), None);
    }

    #[test]
    fn the_scope_tree_roots_a_module_scope_in_a_global_intrinsic_scope() {
        let (model, diagnostics) = bound("const x = 1;");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let global = &model.scopes()[0];
        let module = &model.scopes()[1];
        assert_eq!(global.kind(), ScopeKind::Global);
        assert_eq!(module.kind(), ScopeKind::Module);
        assert_eq!(module.parent(), Some(ScopeId(0)));
        assert_eq!(model.module_scope(), ScopeId(1));
        // Intrinsics bind in the global scope and resolve outward from the module.
        assert!(global.value("Object").is_some());
        assert!(model.lookup_value(model.module_scope(), "Object").is_some());
        // A top-level declaration binds in the module scope.
        let x = module.value("x").expect("x binds in the module scope");
        assert_eq!(
            model.symbol(x).kind,
            SymbolKind::Variable(VariableKind::Const)
        );
    }

    #[test]
    fn duplicate_block_scoped_declarations_are_diagnosed_once_in_their_scope() {
        let (model, diagnostics) = bound("let a = 1; let a = 2;");
        let codes: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code())
            .collect();
        assert_eq!(codes, [DUPLICATE_DECLARATION]);
        // The module scope keeps the first binding; the duplicate does not replace it.
        let a = model
            .scope(model.module_scope())
            .value("a")
            .expect("a binds in the module scope");
        assert_eq!(model.symbol(a).name, "a");
    }

    #[test]
    fn var_hoists_to_the_module_scope_but_let_stays_in_its_block() {
        let (model, diagnostics) = bound("{ var v = 1; let b = 2; }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let block = ScopeId(2);
        assert_eq!(model.scope(block).kind(), ScopeKind::Block);
        let v = model
            .lookup_value(model.module_scope(), "v")
            .expect("var hoists to the module scope");
        assert_eq!(model.symbol(v).scope, model.module_scope());
        let b = model
            .scope(block)
            .value("b")
            .expect("let stays in its block scope");
        assert_eq!(model.symbol(b).scope, block);
        assert!(model.lookup_value(model.module_scope(), "b").is_none());
    }

    #[test]
    fn references_resolve_against_the_two_namespace_scope_tree() {
        let (model, diagnostics) = bound("{ let y = 2; y; }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(model.resolved_reference_count(), 1);
        let unresolved = bound("missing;");
        let codes: Vec<_> = unresolved
            .1
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect();
        assert_eq!(codes, [super::CANNOT_FIND_NAME.as_str()]);
    }

    #[test]
    fn the_type_table_interns_primitives_literals_and_sorted_object_members() {
        let mut table = TypeTable::new();
        assert_eq!(table.get(table.number()), &Type::Number);
        let one = table.number_literal("1");
        assert_eq!(table.number_literal("1"), one, "interning is stable");
        let object = table.object_type(vec![
            PropertyType::new("b", false, table.string()),
            PropertyType::new("a", true, table.number()),
        ]);
        let Type::ObjectType(properties) = table.get(object) else {
            panic!("object_type interns an object type");
        };
        assert_eq!(properties[0].name.as_ref(), "a");
        assert_eq!(properties[1].name.as_ref(), "b");
        assert!(properties[0].optional);
    }

    fn value_symbol(model: &super::SemanticModel, name: &str) -> SymbolId {
        model
            .scopes()
            .iter()
            .find_map(|scope| scope.value(name))
            .unwrap_or_else(|| panic!("value `{name}` not bound"))
    }

    fn type_symbol(model: &super::SemanticModel, name: &str) -> SymbolId {
        model
            .scopes()
            .iter()
            .find_map(|scope| scope.type_binding(name))
            .unwrap_or_else(|| panic!("type `{name}` not bound"))
    }

    fn owning_scope(model: &super::SemanticModel, owner: SymbolId) -> Option<&super::Scope> {
        model
            .scopes()
            .iter()
            .find(|scope| scope.owner() == Some(owner))
    }

    #[test]
    fn enum_members_are_parented_to_the_enum_symbol() {
        let (model, diagnostics) = bound("enum E {\n    A,\n    B = A + 1,\n}\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let enum_id = value_symbol(&model, "E");
        let member_scope = owning_scope(&model, enum_id).expect("enum member scope is owned");
        let a = member_scope.value("A").expect("member A bound");
        assert_eq!(model.symbol(a).parent(), Some(enum_id));
        assert_eq!(model.qualified_name(a), "E.A");
        // The enum itself renders bare.
        assert_eq!(model.symbol(enum_id).parent(), None);
        assert_eq!(model.qualified_name(enum_id), "E");
    }

    #[test]
    fn merged_enum_members_share_one_owner() {
        let (model, diagnostics) = bound("enum E {\n    A = 1,\n}\nenum E {\n    B = 2,\n}\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let enum_id = value_symbol(&model, "E");
        let member_scope = owning_scope(&model, enum_id).expect("merged enum member scope owned");
        let a = member_scope.value("A").expect("member A bound");
        let b = member_scope.value("B").expect("member B bound");
        assert_eq!(model.symbol(a).parent(), Some(enum_id));
        assert_eq!(model.symbol(b).parent(), Some(enum_id));
        assert_eq!(model.qualified_name(a), "E.A");
        assert_eq!(model.qualified_name(b), "E.B");
    }

    #[test]
    fn namespace_exports_render_bare() {
        let (model, diagnostics) = bound("namespace N {\n    export var x = 1;\n}\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let x = value_symbol(&model, "x");
        assert_eq!(model.symbol(x).parent(), None);
        assert_eq!(model.qualified_name(x), "x");
        // The scope holding the export has no owner.
        let export_scope = model
            .scopes()
            .iter()
            .find(|scope| scope.value("x").is_some())
            .expect("export scope exists");
        assert_eq!(export_scope.owner(), None);
    }

    #[test]
    fn nested_namespace_exports_render_bare() {
        let (model, diagnostics) =
            bound("namespace N {\n    export namespace M {\n        export var x = 1;\n    }\n}\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let m = value_symbol(&model, "M");
        let x = value_symbol(&model, "x");
        assert_eq!(model.qualified_name(m), "M");
        assert_eq!(model.qualified_name(x), "x");
    }

    #[test]
    fn class_scope_is_owned_by_the_class_symbol() {
        let (model, diagnostics) = bound("class Ship {\n    isSunk: boolean;\n}\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let ship = value_symbol(&model, "Ship");
        assert!(
            owning_scope(&model, ship).is_some(),
            "class scope owned by the class symbol"
        );
        assert_eq!(model.qualified_name(ship), "Ship");
    }

    #[test]
    fn named_class_expression_internal_name_renders_bare() {
        let (model, diagnostics) = bound("var x = class C {};\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let internal = value_symbol(&model, "C");
        assert_eq!(model.symbol(internal).parent(), None);
        assert_eq!(model.qualified_name(internal), "C");
        // The class scope is owned by the internal-name symbol.
        assert_eq!(
            owning_scope(&model, internal).map(|_| ()),
            Some(()),
            "class scope owned by internal name"
        );
    }

    #[test]
    fn class_type_parameters_render_bare() {
        let (model, diagnostics) = bound("class C<T> {}\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let class_id = value_symbol(&model, "C");
        let t = type_symbol(&model, "T");
        assert_eq!(model.symbol(t).parent(), None);
        assert_eq!(model.qualified_name(t), "T");
        assert!(
            owning_scope(&model, class_id).is_some(),
            "class scope still owned even with type params"
        );
    }

    #[test]
    fn interface_type_parameters_and_members_render_bare() {
        let (model, diagnostics) = bound("interface I<T> {}\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let interface_id = type_symbol(&model, "I");
        let t = type_symbol(&model, "T");
        assert_eq!(model.symbol(t).parent(), None);
        assert_eq!(model.qualified_name(t), "T");
        assert!(
            owning_scope(&model, interface_id).is_none(),
            "interface type-parameter scope has no owner"
        );
    }

    #[test]
    fn type_alias_type_parameters_render_bare() {
        let (model, diagnostics) = bound("type F<T> = T;\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let t = type_symbol(&model, "T");
        assert_eq!(model.symbol(t).parent(), None);
        assert_eq!(model.qualified_name(t), "T");
    }

    #[test]
    fn explicit_type_arguments_instantiate_interface_properties() {
        let (_, diagnostics) = bound(
            "interface I<T, U> { one: T; two?: U; }\n\
             var obj: I<number, string> = { one: 1 };\n",
        );
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == TYPE_NOT_ASSIGNABLE),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn explicit_type_arguments_instantiate_type_alias() {
        let (_, diagnostics) = bound(
            "type F<T> = { x: T };\n\
             var obj: F<number> = { x: 1 };\n",
        );
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code() == TYPE_NOT_ASSIGNABLE),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn call_argument_mismatch_and_type_errors_are_diagnosed() {
        let (_, diagnostics) = bound(
            "function foo(a: string) {}\n\
             foo(2);\n\
             foo('foo', 'bar');\n\
             foo();\n",
        );
        let relevant = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code() == ARGUMENT_NOT_ASSIGNABLE
                    || diagnostic.code() == ARGUMENT_COUNT_MISMATCH
            })
            .count();
        assert_eq!(relevant, 3, "{diagnostics:?}");
    }

    #[test]
    fn non_callable_expression_emits_not_callable() {
        let (_, diagnostics) = bound("const x = 1; x();");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == EXPRESSION_NOT_CALLABLE)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    fn bound_strict(text: &str) -> (super::SemanticModel, Vec<Diagnostic>) {
        let parsed = crate::parser::parse(crate::scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScript,
            source(text),
        ));
        super::bind_source_with_environment(
            parsed.product(),
            super::super::intrinsic_environment::GlobalEnvironment::standard(),
            super::source_is_module(parsed.product()),
            super::ProgramCheckOptions::standard().with_strict_null_checks(true),
        )
    }

    #[test]
    fn used_before_assigned_only_within_same_execution_boundary() {
        let (_, diagnostics) = bound_strict(
            "var a: string;\n\
             function f() { a; }\n\
             class C { static x = a; }\n\
             namespace N { var b: string; b; }\n\
             function g() { var c: string; c; }\n\
             var d: string; d;\n",
        );
        let c038 = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code() == super::USED_BEFORE_ASSIGNED)
            .count();
        assert_eq!(
            c038, 3,
            "expected C038 for namespace b, inner g c, and top-level d"
        );
    }
    #[test]
    fn class_property_initialized_in_constructor() {
        let (_, diagnostics) = bound_strict(
            "class C { x: number; constructor() { this.x = 1; } }\n\
             class D { y: number; constructor() { return; } }\n\
             class F { w: number; }\n",
        );
        let c028 = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code() == PROPERTY_NOT_INITIALIZED)
            .count();
        assert_eq!(c028, 2, "expected C028 for D.y and F.w, not for C.x");
    }

    #[test]
    fn literal_named_properties_exempt_from_initialization_check() {
        let (_, diagnostics) = bound_strict("class C { 1: number; 'a': number; b: number; }\n");
        let c028 = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code() == PROPERTY_NOT_INITIALIZED)
            .count();
        assert_eq!(
            c028, 1,
            "expected C028 only for identifier-named property b"
        );
    }

    #[test]
    fn export_default_interface_binds_name_for_type_references() {
        let (_, diagnostics) = bound("export default interface A { x: number; }\ntype T = A;\n");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn new_target_outside_function_is_diagnosed() {
        let (_, diagnostics) = bound("const a = new.target;");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.code().as_str() == "BAMTS-C044")
        );
    }

    #[test]
    fn new_target_inside_function_and_constructor_is_allowed() {
        let (_, diagnostics) = bound(
            "function f() { new.target; } \
             const g = function () { new.target; }; \
             class C { constructor() { new.target; } }",
        );
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code().as_str() == "BAMTS-C044")
        );
    }

    #[test]
    fn new_target_inside_method_and_static_block_is_diagnosed() {
        let (_, diagnostics) = bound(
            "class C { \
                 m() { new.target; } \
                 static { new.target; } \
             }",
        );
        let new_target_errors = diagnostics
            .iter()
            .filter(|d| d.code().as_str() == "BAMTS-C044")
            .count();
        assert_eq!(new_target_errors, 2, "{diagnostics:?}");
    }

    #[test]
    fn new_target_inside_arrow_inherits_enclosing_function() {
        let (_, diagnostics) = bound("function f() { const a = () => new.target; }");
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code().as_str() == "BAMTS-C044")
        );
    }

    #[test]
    fn set_accessor_parameter_initializer_is_diagnosed() {
        let (_, diagnostics) = bound("class C { set foo(x = 1) { } }");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == SET_ACCESSOR_PARAMETER_INITIALIZER)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn object_literal_set_accessor_parameter_initializer_is_diagnosed() {
        let (_, diagnostics) = bound("const o = { set bar(y = 2) { } };");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == SET_ACCESSOR_PARAMETER_INITIALIZER)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn plain_set_accessor_without_initializer_is_not_diagnosed() {
        let (_, diagnostics) = bound("class C { set foo(x) { } }");
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code() == SET_ACCESSOR_PARAMETER_INITIALIZER),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn ambient_class_getter_implementation_is_diagnosed() {
        let (_, diagnostics) = bound("declare class C { get foo() { return 0; } }");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == AMBIENT_IMPLEMENTATION)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn ambient_class_setter_implementation_is_diagnosed() {
        let (_, diagnostics) = bound("declare class C { set foo(v) { } }");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == AMBIENT_IMPLEMENTATION)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn ambient_class_method_implementation_is_diagnosed() {
        let (_, diagnostics) = bound("declare class C { m() { } }");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == AMBIENT_IMPLEMENTATION)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn ambient_function_implementation_is_diagnosed() {
        let (_, diagnostics) = bound("declare function f() { }");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == AMBIENT_IMPLEMENTATION)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn non_ambient_class_method_implementation_is_allowed() {
        let (_, diagnostics) = bound("class C { m() { return 0; } }");
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code() == AMBIENT_IMPLEMENTATION),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn ambient_class_overload_signature_is_allowed() {
        let (_, diagnostics) = bound("declare class C { m(); }");
        assert!(
            !diagnostics.iter().any(|d| {
                d.code() == AMBIENT_IMPLEMENTATION
                    || d.code() == FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION
                    || d.code() == FUNCTION_IMPLEMENTATION_WRONG_NAME
            }),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn class_method_overload_inside_declare_namespace_is_allowed() {
        let (_, diagnostics) = bound("declare namespace N { export class C { m(); } }");
        assert!(
            !diagnostics.iter().any(|d| {
                d.code() == AMBIENT_IMPLEMENTATION
                    || d.code() == FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION
                    || d.code() == FUNCTION_IMPLEMENTATION_WRONG_NAME
            }),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn get_accessor_parameters_are_diagnosed() {
        let (_, diagnostics) = bound("const o = { get foo(v: number) { return 0; } };");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == GET_ACCESSOR_PARAMETERS)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn get_accessor_without_return_value_is_diagnosed() {
        let (_, diagnostics) = bound("const o = { get foo() { } };");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == GET_ACCESSOR_NO_RETURN)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn get_accessor_with_return_value_is_allowed() {
        let (_, diagnostics) = bound("const o = { get foo() { return 0; } };");
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code() == GET_ACCESSOR_NO_RETURN),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn class_method_overload_missing_implementation_is_diagnosed() {
        let (_, diagnostics) = bound("class C { foo(); }");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn class_method_overload_wrong_implementation_name_is_diagnosed() {
        let (_, diagnostics) = bound("class C { foo(); bar() { } }");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == FUNCTION_IMPLEMENTATION_WRONG_NAME)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn class_method_overload_with_implementation_is_allowed() {
        let (_, diagnostics) = bound("class C { foo(a: string); foo(a: number); foo(a: any) { } }");
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code() == FUNCTION_OVERLOAD_MISSING_IMPLEMENTATION
                    || d.code() == FUNCTION_IMPLEMENTATION_WRONG_NAME),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn constructor_type_parameters_are_diagnosed() {
        let (_, diagnostics) = bound("class C { constructor<T>() { } }");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == CONSTRUCTOR_TYPE_PARAMETERS)
                .count(),
            1,
            "{diagnostics:?}"
        );
    }

    #[test]
    fn constructor_without_type_parameters_is_allowed() {
        let (_, diagnostics) = bound("class C { constructor() { } }");
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.code() == CONSTRUCTOR_TYPE_PARAMETERS),
            "{diagnostics:?}"
        );
    }
    #[test]
    fn class_member_symbol_parent_is_class() {
        let (model, diagnostics) =
            bound("class Board { ships: number[] = []; allShipsSunk() { return true; } }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let ships = model
            .symbols()
            .iter()
            .find(|s| s.name() == "ships")
            .expect("ships symbol");
        let method = model
            .symbols()
            .iter()
            .find(|s| s.name() == "allShipsSunk")
            .expect("method symbol");
        assert_eq!(
            model
                .symbols()
                .iter()
                .filter(|s| s.name() == "allShipsSunk")
                .count(),
            1,
            "duplicate allShipsSunk"
        );
        assert_eq!(
            ships.parent().map(|s| model.symbol(s).name()),
            Some("Board"),
            "ships parent"
        );
        assert!(
            method.parent().is_some(),
            "method parent is None; kind={:?}",
            method.kind()
        );
    }

    #[test]
    fn function_this_parameter_typed_body_uses_it() {
        let (model, diagnostics) =
            bound("interface Foo { n: number; }\nfunction f(this: Foo) { return this.n; }");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let index = model
            .symbols()
            .iter()
            .position(|s| s.name() == "f")
            .expect("function symbol");
        let function_type = model.symbol_type(SymbolId::new(index as u32));
        assert!(
            matches!(model.types().get(function_type), Type::Function(signature) if signature.parameters().is_empty()),
            "'this' parameter is not part of the function signature"
        );
    }

    #[test]
    fn accessor_this_parameter_is_diagnosed() {
        let (_, diagnostics) = bound("const o = { get x(this: any) { return 0; } };");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == ACCESSOR_THIS_PARAMETER)
                .count(),
            1,
            "{diagnostics:?}"
        );
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.code() == GET_ACCESSOR_PARAMETERS)
                .count(),
            0,
            "this-only getter should not emit generic get-accessor-parameters error"
        );
    }

    #[test]
    fn object_literal_get_and_set_combine_into_non_readonly_property() {
        let (model, diagnostics) =
            bound("const o = { get x() { return 1; }, set x(v: number) { } };");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let index = model
            .symbols()
            .iter()
            .position(|s| s.name() == "o")
            .expect("object symbol");
        let object_type = model.symbol_type(SymbolId::new(index as u32));
        let Type::ObjectType(properties) = model.types().get(object_type) else {
            panic!(
                "object literal type expected, got {:?}",
                model.types().get(object_type)
            );
        };
        let x = properties
            .iter()
            .find(|p| p.name() == "x")
            .expect("x property");
        assert!(!x.readonly(), "get/set pair should not be readonly");
        assert_eq!(model.types().get(x.type_id()), &Type::Number);
    }
}
