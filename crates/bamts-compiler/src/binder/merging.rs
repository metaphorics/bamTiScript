//! Canonical declaration merging and module-augmentation resolution.
//!
//! One path decides every same-scope redeclaration: namespace occupancy of the
//! incoming kind, then [`merge_decision`] against the kind already bound to that
//! name. A successful merge keeps the first declaration's [`SymbolId`] and
//! records the new declaration against it. A conflicting occupancy emits
//! [`crate::checker::DUPLICATE_DECLARATION`] and leaves the bound identity
//! unchanged, so later lookups still resolve to the first declaration.
//!
//! Module augmentation is the same path applied to the target module's scope:
//! [`MergeTable::declare_in_module`] resolves the module's registered scope and
//! calls [`MergeTable::declare`]. Augmentation edges are checked for cycles
//! before any binding happens, so a cycle fails deterministically instead of
//! recursing.

use std::collections::{HashMap, HashSet};

use crate::checker::{DUPLICATE_DECLARATION, SymbolId, SymbolKind};
use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::source::{SourceId, TextRange};
use crate::syntax::{NodeId, VariableKind};

/// Diagnostic emitted when `declare module` augmentation forms a cycle.
pub const AUGMENTATION_CYCLE: DiagnosticCode = DiagnosticCode::new("BAMTS-C021");

const DUPLICATE_MESSAGE: &str = "A block-scoped declaration cannot redeclare an existing binding.";
const AUGMENTATION_CYCLE_MESSAGE: &str = "Module augmentation cannot form a cycle.";

/// A binder-owned lexical scope identity.
///
/// Callers map [`crate::checker::ScopeId`] onto this through
/// [`BinderScopeId::new`] and back through [`BinderScopeId::get`]; the binder
/// never depends on checker scope internals.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BinderScopeId(u32);

impl BinderScopeId {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Why a declaration is being bound.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DeclareOrigin {
    /// A declaration written directly in the module being bound.
    Written,
    /// An ambient `declare` form.
    Ambient,
    /// A `declare module` augmentation applied to another module's scope.
    Augmentation,
}

/// Which of TypeScript's two declaration namespaces a kind occupies.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Occupancy {
    value: bool,
    type_space: bool,
}

impl Occupancy {
    /// Returns whether the kind occupies the value namespace.
    #[must_use]
    pub const fn value(self) -> bool {
        self.value
    }

    /// Returns whether the kind occupies the type namespace.
    #[must_use]
    pub const fn type_space(self) -> bool {
        self.type_space
    }

    /// Returns the occupancy covering both operands.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            value: self.value || other.value,
            type_space: self.type_space || other.type_space,
        }
    }
}

/// The result of comparing a bound kind with an incoming kind.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MergeDecision {
    /// The declarations share one [`SymbolId`].
    Merge,
    /// The declarations occupy disjoint namespaces and stay separate symbols.
    Distinct,
    /// The declarations collide in a namespace TypeScript 7.0.2 rejects.
    Conflict,
}

/// One declaration contributing to a merged symbol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MergedDeclaration {
    source: SourceId,
    node: NodeId,
    range: TextRange,
    origin: DeclareOrigin,
}

impl MergedDeclaration {
    #[must_use]
    pub const fn source(self) -> SourceId {
        self.source
    }

    #[must_use]
    pub const fn node(self) -> NodeId {
        self.node
    }

    #[must_use]
    pub const fn range(self) -> TextRange {
        self.range
    }

    #[must_use]
    pub const fn origin(self) -> DeclareOrigin {
        self.origin
    }
}

/// One declaration to bind through [`MergeTable::declare`].
pub struct DeclareRequest<'a> {
    pub name: &'a str,
    pub kind: SymbolKind,
    pub scope: BinderScopeId,
    pub declaration: NodeId,
    pub range: TextRange,
    pub source: SourceId,
    pub origin: DeclareOrigin,
}

/// The identity of a module targeted by augmentation.
///
/// Specifiers are compared exactly; resolving a specifier to a file is the
/// caller's decision, not this table's.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ModuleKey(String);

impl ModuleKey {
    #[must_use]
    pub fn specifier(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Returns the namespaces `kind` occupies.
#[must_use]
pub const fn occupancy(kind: SymbolKind) -> Occupancy {
    match kind {
        SymbolKind::IntrinsicValue
        | SymbolKind::Variable(_)
        | SymbolKind::Function
        | SymbolKind::Parameter
        | SymbolKind::EnumMember => Occupancy {
            value: true,
            type_space: false,
        },
        SymbolKind::IntrinsicType
        | SymbolKind::Interface
        | SymbolKind::TypeAlias
        | SymbolKind::TypeParameter => Occupancy {
            value: false,
            type_space: true,
        },
        SymbolKind::Class | SymbolKind::Enum | SymbolKind::Import | SymbolKind::Namespace => {
            Occupancy {
                value: true,
                type_space: true,
            }
        }
    }
}

/// The single merge matrix used by every declaration and augmentation.
///
/// Kinds that share no namespace are [`MergeDecision::Distinct`]. Kinds that
/// share a namespace merge only when both shared namespaces accept the pair;
/// otherwise the pair is a [`MergeDecision::Conflict`].
#[must_use]
pub const fn merge_decision(existing: SymbolKind, incoming: SymbolKind) -> MergeDecision {
    let existing_occupancy = occupancy(existing);
    let incoming_occupancy = occupancy(incoming);
    let value_overlap = existing_occupancy.value && incoming_occupancy.value;
    let type_overlap = existing_occupancy.type_space && incoming_occupancy.type_space;
    if !value_overlap && !type_overlap {
        return MergeDecision::Distinct;
    }
    let value_ok = !value_overlap || accepts_value_merge(existing, incoming);
    let type_ok = !type_overlap || accepts_type_merge(existing, incoming);
    if value_ok && type_ok {
        MergeDecision::Merge
    } else {
        MergeDecision::Conflict
    }
}

/// Value-namespace merges. Namespace merges onto a function, class, or enum are
/// order-sensitive: the callable or class declaration must already exist.
const fn accepts_value_merge(existing: SymbolKind, incoming: SymbolKind) -> bool {
    matches!(
        (existing, incoming),
        (
            SymbolKind::Variable(VariableKind::Var) | SymbolKind::Function,
            SymbolKind::Variable(VariableKind::Var) | SymbolKind::Function
        ) | (SymbolKind::Enum, SymbolKind::Enum)
            | (SymbolKind::Namespace, SymbolKind::Namespace)
            | (
                SymbolKind::Function | SymbolKind::Class | SymbolKind::Enum,
                SymbolKind::Namespace
            )
    )
}

/// Type-namespace merges. Interface pairs, interface/namespace, and
/// class/interface are order-independent; class or enum plus namespace is not.
const fn accepts_type_merge(existing: SymbolKind, incoming: SymbolKind) -> bool {
    matches!(
        (existing, incoming),
        (SymbolKind::Interface, SymbolKind::Interface)
            | (SymbolKind::Enum, SymbolKind::Enum)
            | (SymbolKind::Namespace, SymbolKind::Namespace)
            | (SymbolKind::Interface, SymbolKind::Namespace)
            | (SymbolKind::Namespace, SymbolKind::Interface)
            | (SymbolKind::Class | SymbolKind::Enum, SymbolKind::Namespace)
            | (SymbolKind::Class, SymbolKind::Interface)
            | (SymbolKind::Interface, SymbolKind::Class)
    )
}

/// The canonical merge table: per-scope value and type maps over one symbol list.
#[derive(Clone, Debug, Default)]
pub struct MergeTable {
    scopes: HashMap<BinderScopeId, ScopeBindings>,
    symbols: Vec<BoundSymbol>,
    modules: HashMap<ModuleKey, BinderScopeId>,
    augmentation_edges: HashMap<ModuleKey, ModuleKey>,
    pending_augmentations: HashMap<ModuleKey, Vec<ModuleKey>>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Default)]
struct ScopeBindings {
    values: HashMap<String, SymbolId>,
    types: HashMap<String, SymbolId>,
}

#[derive(Clone, Debug)]
struct BoundSymbol {
    name: String,
    kind: SymbolKind,
    occupancy: Occupancy,
    scope: BinderScopeId,
    declarations: Vec<MergedDeclaration>,
}

impl MergeTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds one declaration, merging it into an existing symbol when
    /// [`merge_decision`] allows and reporting a duplicate otherwise.
    pub fn declare(&mut self, request: &DeclareRequest<'_>) -> SymbolId {
        let incoming = occupancy(request.kind);
        let mergeable = self.mergeable_symbol(request, incoming);
        if let Some(existing) = mergeable {
            self.record_merge(existing, request, incoming);
            return existing;
        }
        let id =
            SymbolId::new(u32::try_from(self.symbols.len()).expect("symbol count fits in u32"));
        self.symbols.push(BoundSymbol {
            name: request.name.to_owned(),
            kind: request.kind,
            occupancy: incoming,
            scope: request.scope,
            declarations: vec![MergedDeclaration {
                source: request.source,
                node: request.declaration,
                range: request.range,
                origin: request.origin,
            }],
        });
        let bindings = self.scopes.entry(request.scope).or_default();
        let mut conflict = false;
        if incoming.value {
            conflict |= claim_slot(&mut bindings.values, request.name, id);
        }
        if incoming.type_space {
            conflict |= claim_slot(&mut bindings.types, request.name, id);
        }
        if conflict {
            self.diagnostics.push(Diagnostic::error(
                DUPLICATE_DECLARATION,
                request.source,
                request.range,
                DUPLICATE_MESSAGE,
            ));
        }
        id
    }

    /// Registers `scope` as the scope of the module named by `key`, then applies
    /// augmentations that named this module before it was registered.
    pub fn register_module(&mut self, key: ModuleKey, scope: BinderScopeId) {
        self.modules.insert(key.clone(), scope);
        if let Some(pending) = self.pending_augmentations.remove(&key) {
            for source in pending {
                self.augmentation_edges.insert(source, key.clone());
            }
        }
    }

    /// Records that module `from` augments module `target`.
    ///
    /// A module augmenting itself is an ordinary same-module merge, not an edge.
    /// An edge that would close a cycle emits [`AUGMENTATION_CYCLE`] and is not
    /// recorded.
    pub fn augment(
        &mut self,
        from: ModuleKey,
        target: ModuleKey,
        source: SourceId,
        range: TextRange,
    ) {
        if from == target {
            return;
        }
        if self.reaches(&target, &from) {
            self.diagnostics.push(Diagnostic::error(
                AUGMENTATION_CYCLE,
                source,
                range,
                AUGMENTATION_CYCLE_MESSAGE,
            ));
            return;
        }
        if self.modules.contains_key(&target) {
            self.augmentation_edges.insert(from, target);
        } else {
            self.pending_augmentations
                .entry(target)
                .or_default()
                .push(from);
        }
    }

    /// Binds `request` into a registered module's scope, returning `None` when
    /// that module has no scope yet.
    pub fn declare_in_module(
        &mut self,
        module: &ModuleKey,
        request: &DeclareRequest<'_>,
    ) -> Option<SymbolId> {
        let scope = *self.modules.get(module)?;
        Some(self.declare(&DeclareRequest {
            name: request.name,
            kind: request.kind,
            scope,
            declaration: request.declaration,
            range: request.range,
            source: request.source,
            origin: request.origin,
        }))
    }

    /// Returns the value binding declared directly in `scope`.
    #[must_use]
    pub fn lookup_value(&self, scope: BinderScopeId, name: &str) -> Option<SymbolId> {
        self.scopes.get(&scope)?.values.get(name).copied()
    }

    /// Returns the type binding declared directly in `scope`.
    #[must_use]
    pub fn lookup_type(&self, scope: BinderScopeId, name: &str) -> Option<SymbolId> {
        self.scopes.get(&scope)?.types.get(name).copied()
    }

    #[must_use]
    pub fn name(&self, id: SymbolId) -> &str {
        &self.symbols[id.get() as usize].name
    }

    /// Returns the kind of the symbol's first declaration, which a later merge
    /// never rewrites.
    #[must_use]
    pub fn kind(&self, id: SymbolId) -> SymbolKind {
        self.symbols[id.get() as usize].kind
    }

    #[must_use]
    pub fn scope_of(&self, id: SymbolId) -> BinderScopeId {
        self.symbols[id.get() as usize].scope
    }

    /// Returns the namespaces the symbol occupies after every merge so far.
    #[must_use]
    pub fn occupancy_of(&self, id: SymbolId) -> Occupancy {
        self.symbols[id.get() as usize].occupancy
    }

    /// Returns every declaration merged into the symbol, in binding order.
    #[must_use]
    pub fn merged_declarations(&self, id: SymbolId) -> &[MergedDeclaration] {
        &self.symbols[id.get() as usize].declarations
    }

    #[must_use]
    pub fn module_scope(&self, key: &ModuleKey) -> Option<BinderScopeId> {
        self.modules.get(key).copied()
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// Returns the symbol `request` merges into, checking the value namespace
    /// before the type namespace so a value-bearing merge keeps its identity.
    fn mergeable_symbol(
        &self,
        request: &DeclareRequest<'_>,
        incoming: Occupancy,
    ) -> Option<SymbolId> {
        let bindings = self.scopes.get(&request.scope)?;
        let value = incoming
            .value
            .then(|| bindings.values.get(request.name).copied())
            .flatten();
        let type_space = incoming
            .type_space
            .then(|| bindings.types.get(request.name).copied())
            .flatten();
        value
            .filter(|&existing| self.merges_into(existing, request.kind))
            .or_else(|| type_space.filter(|&existing| self.merges_into(existing, request.kind)))
    }

    fn merges_into(&self, existing: SymbolId, incoming: SymbolKind) -> bool {
        matches!(
            merge_decision(self.symbols[existing.get() as usize].kind, incoming),
            MergeDecision::Merge
        )
    }

    fn record_merge(
        &mut self,
        existing: SymbolId,
        request: &DeclareRequest<'_>,
        incoming: Occupancy,
    ) {
        let symbol = &mut self.symbols[existing.get() as usize];
        symbol.occupancy = symbol.occupancy.union(incoming);
        symbol.declarations.push(MergedDeclaration {
            source: request.source,
            node: request.declaration,
            range: request.range,
            origin: request.origin,
        });
        // A merge can widen occupancy, so claim the namespace the earlier
        // declarations left empty without displacing an existing binding.
        let bindings = self.scopes.entry(request.scope).or_default();
        if incoming.value {
            bindings
                .values
                .entry(request.name.to_owned())
                .or_insert(existing);
        }
        if incoming.type_space {
            bindings
                .types
                .entry(request.name.to_owned())
                .or_insert(existing);
        }
    }

    /// Returns whether `target` is reachable from `start` through augmentation
    /// edges, so adding `start -> target` would close a cycle.
    fn reaches(&self, start: &ModuleKey, target: &ModuleKey) -> bool {
        let mut seen = HashSet::new();
        let mut current = Some(start);
        while let Some(key) = current {
            if key == target {
                return true;
            }
            if !seen.insert(key) {
                return true;
            }
            current = self.augmentation_edges.get(key);
        }
        false
    }
}

/// Claims `name` for `id`, reporting whether a different symbol already held it.
fn claim_slot(slots: &mut HashMap<String, SymbolId>, name: &str, id: SymbolId) -> bool {
    match slots.get(name) {
        None => {
            slots.insert(name.to_owned(), id);
            false
        }
        Some(existing) => *existing != id,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AUGMENTATION_CYCLE, BinderScopeId, DeclareOrigin, DeclareRequest, MergeDecision,
        MergeTable, ModuleKey, merge_decision, occupancy,
    };
    use crate::checker::{DUPLICATE_DECLARATION, SymbolKind};
    use crate::diagnostic::{Diagnostic, DiagnosticCode};
    use crate::source::{SourceId, TextRange, Utf16Pos};
    use crate::syntax::{NodeId, VariableKind};

    fn range() -> TextRange {
        TextRange::new(Utf16Pos::ZERO, Utf16Pos::new(1)).expect("endpoints are ordered")
    }

    fn request(
        name: &str,
        kind: SymbolKind,
        scope: BinderScopeId,
        node: u32,
    ) -> DeclareRequest<'_> {
        DeclareRequest {
            name,
            kind,
            scope,
            declaration: NodeId::new(node),
            range: range(),
            source: SourceId::new(0),
            origin: DeclareOrigin::Written,
        }
    }

    fn codes(table: &MergeTable) -> Vec<&str> {
        table
            .diagnostics()
            .iter()
            .map(Diagnostic::code)
            .map(DiagnosticCode::as_str)
            .collect()
    }

    #[test]
    fn interface_and_namespace_merge_share_one_symbol() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let first = table.declare(&request("I", SymbolKind::Interface, scope, 1));
        let second = table.declare(&request("I", SymbolKind::Namespace, scope, 2));
        assert_eq!(first, second);
        assert_eq!(table.merged_declarations(first).len(), 2);
        assert_eq!(table.lookup_type(scope, "I"), Some(first));
        assert_eq!(table.lookup_value(scope, "I"), Some(first));
        assert_eq!(table.name(first), "I");
        assert_eq!(table.scope_of(first), scope);
        assert!(!table.has_errors());
    }

    #[test]
    fn namespace_then_interface_merges_bidirectionally() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let first = table.declare(&request("I", SymbolKind::Namespace, scope, 1));
        let second = table.declare(&request("I", SymbolKind::Interface, scope, 2));
        assert_eq!(first, second);
        assert!(!table.has_errors());
    }

    #[test]
    fn function_then_namespace_merges_and_widens_occupancy() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let function = table.declare(&request("F", SymbolKind::Function, scope, 1));
        assert!(!table.occupancy_of(function).type_space());
        let merged = table.declare(&request("F", SymbolKind::Namespace, scope, 2));
        assert_eq!(function, merged);
        assert_eq!(table.kind(function), SymbolKind::Function);
        assert!(table.occupancy_of(function).value());
        assert!(table.occupancy_of(function).type_space());
        assert_eq!(table.lookup_type(scope, "F"), Some(function));
        assert!(!table.has_errors());
    }

    #[test]
    fn namespace_then_function_conflicts_and_keeps_the_namespace() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let namespace = table.declare(&request("F", SymbolKind::Namespace, scope, 1));
        let incoming = table.declare(&request("F", SymbolKind::Function, scope, 2));
        assert_ne!(namespace, incoming);
        assert_eq!(table.lookup_value(scope, "F"), Some(namespace));
        assert_eq!(codes(&table), [DUPLICATE_DECLARATION.as_str()]);
    }

    #[test]
    fn class_and_enum_accept_a_following_namespace() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let class = table.declare(&request("C", SymbolKind::Class, scope, 1));
        assert_eq!(
            table.declare(&request("C", SymbolKind::Namespace, scope, 2)),
            class
        );
        let enumeration = table.declare(&request("E", SymbolKind::Enum, scope, 3));
        assert_eq!(
            table.declare(&request("E", SymbolKind::Namespace, scope, 4)),
            enumeration
        );
        assert!(!table.has_errors());
    }

    #[test]
    fn class_and_interface_merge_in_either_order() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let class = table.declare(&request("C", SymbolKind::Class, scope, 1));
        assert_eq!(
            table.declare(&request("C", SymbolKind::Interface, scope, 2)),
            class
        );
        let interface = table.declare(&request("D", SymbolKind::Interface, scope, 3));
        assert_eq!(
            table.declare(&request("D", SymbolKind::Class, scope, 4)),
            interface
        );
        assert!(!table.has_errors());
    }

    #[test]
    fn overloads_and_repeated_interfaces_keep_the_first_identity() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let function = table.declare(&request("f", SymbolKind::Function, scope, 1));
        assert_eq!(
            table.declare(&request("f", SymbolKind::Function, scope, 2)),
            function
        );
        let interface = table.declare(&request("I", SymbolKind::Interface, scope, 3));
        assert_eq!(
            table.declare(&request("I", SymbolKind::Interface, scope, 4)),
            interface
        );
        let enumeration = table.declare(&request("E", SymbolKind::Enum, scope, 5));
        assert_eq!(
            table.declare(&request("E", SymbolKind::Enum, scope, 6)),
            enumeration
        );
        assert_eq!(table.merged_declarations(function).len(), 2);
        assert!(!table.has_errors());
    }

    #[test]
    fn var_and_function_merge_but_block_scoped_names_conflict() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let var = table.declare(&request(
            "f",
            SymbolKind::Variable(VariableKind::Var),
            scope,
            1,
        ));
        assert_eq!(
            table.declare(&request("f", SymbolKind::Function, scope, 2)),
            var
        );
        let lexical = table.declare(&request(
            "g",
            SymbolKind::Variable(VariableKind::Let),
            scope,
            3,
        ));
        table.declare(&request("g", SymbolKind::Function, scope, 4));
        assert_eq!(table.lookup_value(scope, "g"), Some(lexical));
        assert_eq!(codes(&table), [DUPLICATE_DECLARATION.as_str()]);
    }

    #[test]
    fn disjoint_namespaces_stay_distinct_symbols() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let interface = table.declare(&request("Foo", SymbolKind::Interface, scope, 1));
        let var = table.declare(&request(
            "Foo",
            SymbolKind::Variable(VariableKind::Var),
            scope,
            2,
        ));
        assert_ne!(interface, var);
        assert_eq!(table.lookup_type(scope, "Foo"), Some(interface));
        assert_eq!(table.lookup_value(scope, "Foo"), Some(var));
        assert!(!table.has_errors());
        assert_eq!(
            merge_decision(
                SymbolKind::Interface,
                SymbolKind::Variable(VariableKind::Var)
            ),
            MergeDecision::Distinct
        );
    }

    #[test]
    fn type_alias_never_merges() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let alias = table.declare(&request("T", SymbolKind::TypeAlias, scope, 1));
        table.declare(&request("T", SymbolKind::Interface, scope, 2));
        table.declare(&request("T", SymbolKind::Namespace, scope, 3));
        assert_eq!(table.lookup_type(scope, "T"), Some(alias));
        assert_eq!(table.merged_declarations(alias).len(), 1);
        assert_eq!(
            codes(&table),
            [
                DUPLICATE_DECLARATION.as_str(),
                DUPLICATE_DECLARATION.as_str()
            ]
        );
        assert!(occupancy(SymbolKind::TypeAlias).type_space());
        assert!(!occupancy(SymbolKind::TypeAlias).value());
    }

    #[test]
    fn repeated_classes_conflict_and_keep_the_first_identity() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let first = table.declare(&request("C", SymbolKind::Class, scope, 1));
        table.declare(&request("C", SymbolKind::Class, scope, 2));
        assert_eq!(table.lookup_value(scope, "C"), Some(first));
        assert_eq!(table.lookup_type(scope, "C"), Some(first));
        assert_eq!(table.merged_declarations(first).len(), 1);
        assert_eq!(codes(&table), [DUPLICATE_DECLARATION.as_str()]);
    }

    #[test]
    fn ambient_and_written_declarations_merge_on_one_path() {
        let scope = BinderScopeId::new(0);
        let mut table = MergeTable::new();
        let written = table.declare(&request("Box", SymbolKind::Interface, scope, 1));
        let ambient = table.declare(&DeclareRequest {
            name: "Box",
            kind: SymbolKind::Interface,
            scope,
            declaration: NodeId::new(2),
            range: range(),
            source: SourceId::new(0),
            origin: DeclareOrigin::Ambient,
        });
        assert_eq!(written, ambient);
        assert_eq!(
            table.merged_declarations(written)[1].origin(),
            DeclareOrigin::Ambient
        );
        assert!(!table.has_errors());
    }

    #[test]
    fn augmentation_declares_into_the_target_module_scope() {
        let mut table = MergeTable::new();
        let host = BinderScopeId::new(1);
        let augmenting = BinderScopeId::new(2);
        let target = ModuleKey::specifier("./box");
        let augmenter = ModuleKey::specifier("./augment");
        table.register_module(target.clone(), host);
        table.register_module(augmenter.clone(), augmenting);
        table.augment(augmenter, target.clone(), SourceId::new(1), range());
        assert_eq!(table.module_scope(&target), Some(host));

        let declared = table
            .declare_in_module(
                &target,
                &DeclareRequest {
                    name: "Box",
                    kind: SymbolKind::Interface,
                    scope: augmenting,
                    declaration: NodeId::new(10),
                    range: range(),
                    source: SourceId::new(1),
                    origin: DeclareOrigin::Augmentation,
                },
            )
            .expect("the target module is registered");
        let merged = table
            .declare_in_module(
                &target,
                &DeclareRequest {
                    name: "Box",
                    kind: SymbolKind::Interface,
                    scope: host,
                    declaration: NodeId::new(11),
                    range: range(),
                    source: SourceId::new(0),
                    origin: DeclareOrigin::Written,
                },
            )
            .expect("the target module is registered");
        assert_eq!(declared, merged);
        assert_eq!(table.scope_of(declared), host);
        assert_eq!(table.merged_declarations(declared).len(), 2);
        assert!(!table.has_errors());
    }

    #[test]
    fn declaring_into_an_unregistered_module_yields_none() {
        let mut table = MergeTable::new();
        let missing = ModuleKey::specifier("missing");
        assert!(
            table
                .declare_in_module(
                    &missing,
                    &request("X", SymbolKind::Interface, BinderScopeId::new(0), 1),
                )
                .is_none()
        );
        assert!(!table.has_errors());
    }

    #[test]
    fn self_augmentation_is_a_merge_not_a_cycle() {
        let mut table = MergeTable::new();
        let key = ModuleKey::specifier("self");
        table.register_module(key.clone(), BinderScopeId::new(0));
        table.augment(key, ModuleKey::specifier("self"), SourceId::new(0), range());
        assert!(!table.has_errors());
    }

    #[test]
    fn two_module_augmentation_cycle_fails_deterministically() {
        let mut table = MergeTable::new();
        let a = ModuleKey::specifier("a");
        let b = ModuleKey::specifier("b");
        table.register_module(a.clone(), BinderScopeId::new(0));
        table.register_module(b.clone(), BinderScopeId::new(1));
        table.augment(a.clone(), b.clone(), SourceId::new(0), range());
        table.augment(b, a, SourceId::new(1), range());
        assert_eq!(codes(&table), [AUGMENTATION_CYCLE.as_str()]);
    }

    #[test]
    fn three_module_augmentation_cycle_fails_deterministically() {
        let mut table = MergeTable::new();
        let a = ModuleKey::specifier("a");
        let b = ModuleKey::specifier("b");
        let c = ModuleKey::specifier("c");
        table.register_module(a.clone(), BinderScopeId::new(0));
        table.register_module(b.clone(), BinderScopeId::new(1));
        table.register_module(c.clone(), BinderScopeId::new(2));
        table.augment(a.clone(), b.clone(), SourceId::new(0), range());
        table.augment(b, c.clone(), SourceId::new(1), range());
        table.augment(c, a, SourceId::new(2), range());
        assert_eq!(codes(&table), [AUGMENTATION_CYCLE.as_str()]);
    }

    #[test]
    fn augmentation_recorded_before_registration_still_detects_a_cycle() {
        let mut table = MergeTable::new();
        let a = ModuleKey::specifier("a");
        let b = ModuleKey::specifier("b");
        table.register_module(a.clone(), BinderScopeId::new(0));
        table.augment(a.clone(), b.clone(), SourceId::new(0), range());
        assert!(!table.has_errors());
        table.register_module(b.clone(), BinderScopeId::new(1));
        table.augment(b, a, SourceId::new(1), range());
        assert_eq!(codes(&table), [AUGMENTATION_CYCLE.as_str()]);
    }
}
