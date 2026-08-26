//! Canonical export table, symbol-flag visibility, and re-export resolution.
//!
//! Every export form is recorded in one table: named local exports, named
//! re-export forwards, `export *`, `export * as`, `export default`, and
//! `export =`. One resolution path serves both namespaces
//! ([`ExportTable::resolve_value`] and [`ExportTable::resolve_type`] differ only
//! by the [`ResolveSpace`] they pass): a module's own named export wins, then
//! `export *` edges are searched.
//!
//! Symbol identity is preserved across re-exports. A forward resolves to the
//! origin module's own [`SymbolId`], never to a fresh one, so a diamond that
//! reaches the same origin twice stays one identity while two distinct origins
//! are [`ResolvedExport::Ambiguous`]. The shared visited set makes a cycle
//! [`ResolvedExport::Cycle`] instead of unbounded recursion.

use std::collections::{BTreeMap, HashMap, HashSet};

use bamts_bytecode::EcmaString;

use crate::checker::{DUPLICATE_DECLARATION, MIXED_EXPORT_ASSIGNMENT, SymbolId, SymbolKind};
use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::source::{SourceId, TextRange, Utf16Pos};
use crate::syntax::VariableKind;

/// Diagnostic emitted when a re-export chain cycles.
pub const EXPORT_CYCLE: DiagnosticCode = DiagnosticCode::new("BAMTS-C022");
/// Diagnostic emitted when one exported name reaches two distinct origins.
pub const EXPORT_AMBIGUOUS: DiagnosticCode = DiagnosticCode::new("BAMTS-C023");

const DUPLICATE_EXPORT_MESSAGE: &str = "Cannot redeclare exported name.";
const MIXED_EXPORT_ASSIGNMENT_MESSAGE: &str =
    "An export assignment cannot be used with other exported elements.";
const EXPORT_CYCLE_MESSAGE: &str = "Re-export chain cannot form a cycle.";
const EXPORT_AMBIGUOUS_MESSAGE: &str = "Exported name resolves to more than one declaration.";

/// The UTF-16 spelling of the default export's name.
const DEFAULT_EXPORT_NAME: &[u16] = &[
    b'd' as u16,
    b'e' as u16,
    b'f' as u16,
    b'a' as u16,
    b'u' as u16,
    b'l' as u16,
    b't' as u16,
];

/// The anchor for an export the caller recorded without a syntax range.
fn unanchored_range() -> TextRange {
    TextRange::new(Utf16Pos::ZERO, Utf16Pos::ZERO).expect("endpoints are ordered")
}

/// Declaration-meaning and visibility flags for one exported name.
///
/// Bits describe namespace occupancy and export shape. They are this binder's
/// own encoding, not TypeScript's `SymbolFlags` ordinals.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct SymbolFlags(u32);

impl SymbolFlags {
    pub const NONE: Self = Self(0);
    pub const FUNCTION_SCOPED_VARIABLE: Self = Self(1 << 0);
    pub const BLOCK_SCOPED_VARIABLE: Self = Self(1 << 1);
    pub const FUNCTION: Self = Self(1 << 2);
    pub const CLASS: Self = Self(1 << 3);
    pub const INTERFACE: Self = Self(1 << 4);
    pub const ENUM: Self = Self(1 << 5);
    pub const VALUE_MODULE: Self = Self(1 << 6);
    pub const TYPE_ALIAS: Self = Self(1 << 7);
    pub const TYPE_PARAMETER: Self = Self(1 << 8);
    pub const ENUM_MEMBER: Self = Self(1 << 9);
    pub const PARAMETER: Self = Self(1 << 10);
    pub const INTRINSIC_VALUE: Self = Self(1 << 11);
    pub const INTRINSIC_TYPE: Self = Self(1 << 12);
    /// The name forwards to another declaration rather than declaring one.
    pub const ALIAS: Self = Self(1 << 13);
    pub const EXPORT: Self = Self(1 << 14);
    pub const EXPORT_STAR: Self = Self(1 << 15);
    /// The export is written `export type`, so it never reaches value space.
    pub const TYPE_ONLY: Self = Self(1 << 16);
    pub const DEFAULT: Self = Self(1 << 17);

    /// Kinds that occupy the value namespace.
    pub const VALUE: Self = Self(
        Self::FUNCTION_SCOPED_VARIABLE.0
            | Self::BLOCK_SCOPED_VARIABLE.0
            | Self::FUNCTION.0
            | Self::CLASS.0
            | Self::ENUM.0
            | Self::VALUE_MODULE.0
            | Self::ENUM_MEMBER.0
            | Self::PARAMETER.0
            | Self::INTRINSIC_VALUE.0,
    );
    /// Kinds that occupy the type namespace.
    pub const TYPE: Self = Self(
        Self::CLASS.0
            | Self::INTERFACE.0
            | Self::ENUM.0
            | Self::VALUE_MODULE.0
            | Self::TYPE_ALIAS.0
            | Self::TYPE_PARAMETER.0
            | Self::INTRINSIC_TYPE.0,
    );

    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns whether every bit of `other` is set. An empty `other` is never
    /// contained, so `contains(NONE)` is false rather than vacuously true.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        other.0 != 0 && self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns whether the export is reachable from `space`.
    ///
    /// An alias carries no declaration bits of its own, so it is visible in both
    /// namespaces until `export type` narrows it to types.
    #[must_use]
    pub const fn visible_in(self, space: ResolveSpace) -> bool {
        match space {
            ResolveSpace::Value => {
                !self.contains(Self::TYPE_ONLY)
                    && (self.intersects(Self::VALUE) || self.contains(Self::ALIAS))
            }
            ResolveSpace::Type => {
                self.intersects(Self::TYPE)
                    || self.contains(Self::TYPE_ONLY)
                    || self.contains(Self::ALIAS)
            }
        }
    }
}

impl std::ops::BitOr for SymbolFlags {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.union(other)
    }
}

/// Returns the declaration-meaning flags for a bound kind.
#[must_use]
pub const fn flags_from_kind(kind: SymbolKind) -> SymbolFlags {
    match kind {
        SymbolKind::IntrinsicValue => SymbolFlags::INTRINSIC_VALUE,
        SymbolKind::IntrinsicType => SymbolFlags::INTRINSIC_TYPE,
        SymbolKind::Variable(VariableKind::Var) => SymbolFlags::FUNCTION_SCOPED_VARIABLE,
        SymbolKind::Variable(_) => SymbolFlags::BLOCK_SCOPED_VARIABLE,
        SymbolKind::Function => SymbolFlags::FUNCTION,
        SymbolKind::Parameter => SymbolFlags::PARAMETER,
        SymbolKind::Class => SymbolFlags::CLASS,
        SymbolKind::Interface => SymbolFlags::INTERFACE,
        SymbolKind::TypeAlias => SymbolFlags::TYPE_ALIAS,
        SymbolKind::Enum => SymbolFlags::ENUM,
        SymbolKind::EnumMember => SymbolFlags::ENUM_MEMBER,
        SymbolKind::TypeParameter => SymbolFlags::TYPE_PARAMETER,
        SymbolKind::Import => SymbolFlags::ALIAS,
        SymbolKind::Namespace => SymbolFlags::VALUE_MODULE,
    }
}

/// Whether an export specifier is a value or a `type` specifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SpecifierKind {
    Value,
    TypeOnly,
}

/// The namespace a lookup searches.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResolveSpace {
    Value,
    Type,
}

/// What one exported name resolves to.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResolvedExport {
    /// The origin declaration, with the origin module's own symbol identity.
    Symbol { source: SourceId, symbol: SymbolId },
    /// A module namespace object produced by `export * as`.
    Namespace { source: SourceId },
    /// No export of that name is visible in the requested namespace.
    Unresolved,
    /// The re-export chain cycles.
    Cycle,
    /// The name reaches two or more distinct origins.
    Ambiguous,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExportTarget {
    Local { source: SourceId, symbol: SymbolId },
    Forward { source: SourceId, name: EcmaString },
    StarNamespace { source: SourceId },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NamedExport {
    target: ExportTarget,
    flags: SymbolFlags,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ExportCandidate {
    Symbol { source: SourceId, symbol: SymbolId },
    Namespace { source: SourceId },
}

/// The origins one name reached, plus whether any branch cycled.
#[derive(Default)]
struct ResolutionSet {
    candidates: HashSet<ExportCandidate>,
    cycled: bool,
}

impl ResolutionSet {
    fn candidate(candidate: ExportCandidate) -> Self {
        let mut candidates = HashSet::new();
        candidates.insert(candidate);
        Self {
            candidates,
            cycled: false,
        }
    }

    fn cycle() -> Self {
        Self {
            candidates: HashSet::new(),
            cycled: true,
        }
    }

    fn extend(&mut self, other: Self) {
        self.candidates.extend(other.candidates);
        self.cycled |= other.cycled;
    }

    /// Collapses the set. A cycle only surfaces when no origin was reached, so a
    /// resolvable diamond that also contains a cycle still resolves.
    fn into_resolution(self) -> ResolvedExport {
        match self.candidates.len() {
            0 if self.cycled => ResolvedExport::Cycle,
            0 => ResolvedExport::Unresolved,
            1 => match self
                .candidates
                .into_iter()
                .next()
                .expect("a single candidate exists")
            {
                ExportCandidate::Symbol { source, symbol } => {
                    ResolvedExport::Symbol { source, symbol }
                }
                ExportCandidate::Namespace { source } => ResolvedExport::Namespace { source },
            },
            _ => ResolvedExport::Ambiguous,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StarExport {
    target: SourceId,
    specifier: SpecifierKind,
}

#[derive(Clone, Debug, Default)]
struct ModuleExports {
    named: BTreeMap<EcmaString, NamedExport>,
    stars: Vec<StarExport>,
    assignment: Option<SymbolId>,
    assignment_anchor: Option<(SourceId, TextRange)>,
    exports_a_value: bool,
    reported_mixed_assignment: bool,
}

/// The program-wide export table.
#[derive(Clone, Debug, Default)]
pub struct ExportTable {
    modules: HashMap<SourceId, ModuleExports>,
    diagnostics: Vec<Diagnostic>,
}

impl ExportTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `module` exporting `symbol` under `name`.
    pub fn export_local(
        &mut self,
        module: SourceId,
        name: EcmaString,
        source: SourceId,
        symbol: SymbolId,
        flags: SymbolFlags,
    ) {
        self.bind_named(
            module,
            name,
            NamedExport {
                target: ExportTarget::Local { source, symbol },
                flags: flags.union(SymbolFlags::EXPORT),
            },
            source,
            unanchored_range(),
        );
    }

    /// Records `export { local as exported } from target`.
    #[expect(
        clippy::too_many_arguments,
        reason = "a named re-export binds module, both names, specifier kind, and its diagnostic anchor"
    )]
    pub fn export_forward(
        &mut self,
        module: SourceId,
        exported: EcmaString,
        target: SourceId,
        local: EcmaString,
        specifier: SpecifierKind,
        source: SourceId,
        range: TextRange,
    ) {
        let mut flags = SymbolFlags::EXPORT.union(SymbolFlags::ALIAS);
        if matches!(specifier, SpecifierKind::TypeOnly) {
            flags = flags.union(SymbolFlags::TYPE_ONLY);
        }
        self.bind_named(
            module,
            exported,
            NamedExport {
                target: ExportTarget::Forward {
                    source: target,
                    name: local,
                },
                flags,
            },
            source,
            range,
        );
    }

    /// Records `export * from target` or `export type * from target`.
    pub fn export_star(&mut self, module: SourceId, target: SourceId, specifier: SpecifierKind) {
        let star = StarExport { target, specifier };
        let entry = self.modules.entry(module).or_default();
        if !entry.stars.contains(&star) {
            entry.stars.push(star);
        }
        if matches!(specifier, SpecifierKind::Value) {
            entry.exports_a_value = true;
            self.report_mixed_assignment(module);
        }
    }

    /// Records `export * as exported from target`.
    pub fn export_star_as(
        &mut self,
        module: SourceId,
        exported: EcmaString,
        target: SourceId,
        source: SourceId,
    ) {
        self.bind_named(
            module,
            exported,
            NamedExport {
                target: ExportTarget::StarNamespace { source: target },
                flags: SymbolFlags::EXPORT
                    .union(SymbolFlags::EXPORT_STAR)
                    .union(SymbolFlags::VALUE_MODULE),
            },
            source,
            unanchored_range(),
        );
    }

    /// Records `export default`.
    pub fn export_default(
        &mut self,
        module: SourceId,
        source: SourceId,
        symbol: SymbolId,
        flags: SymbolFlags,
    ) {
        self.export_local(
            module,
            EcmaString::from_units(DEFAULT_EXPORT_NAME),
            source,
            symbol,
            flags.union(SymbolFlags::DEFAULT),
        );
    }

    /// Records `export = symbol`, which cannot coexist with a value export.
    pub fn export_assignment(
        &mut self,
        module: SourceId,
        symbol: SymbolId,
        source: SourceId,
        range: TextRange,
    ) {
        let entry = self.modules.entry(module).or_default();
        if entry.exports_a_value {
            entry.reported_mixed_assignment = true;
            self.diagnostics.push(Diagnostic::error(
                MIXED_EXPORT_ASSIGNMENT,
                source,
                range,
                MIXED_EXPORT_ASSIGNMENT_MESSAGE,
            ));
            return;
        }
        entry.assignment = Some(symbol);
        entry.assignment_anchor = Some((source, range));
    }

    /// Resolves `name` in value space.
    #[must_use]
    pub fn resolve_value(&self, module: SourceId, name: &EcmaString) -> ResolvedExport {
        self.resolve(module, name, ResolveSpace::Value)
    }

    /// Resolves `name` in type space.
    #[must_use]
    pub fn resolve_type(&self, module: SourceId, name: &EcmaString) -> ResolvedExport {
        self.resolve(module, name, ResolveSpace::Type)
    }

    /// Resolves `name` and reports a cycle or ambiguity against `range`.
    pub fn resolve_reported(
        &mut self,
        module: SourceId,
        name: &EcmaString,
        space: ResolveSpace,
        source: SourceId,
        range: TextRange,
    ) -> ResolvedExport {
        let resolved = self.resolve(module, name, space);
        match resolved {
            ResolvedExport::Cycle => self.diagnostics.push(Diagnostic::error(
                EXPORT_CYCLE,
                source,
                range,
                EXPORT_CYCLE_MESSAGE,
            )),
            ResolvedExport::Ambiguous => self.diagnostics.push(Diagnostic::error(
                EXPORT_AMBIGUOUS,
                source,
                range,
                EXPORT_AMBIGUOUS_MESSAGE,
            )),
            ResolvedExport::Symbol { .. }
            | ResolvedExport::Namespace { .. }
            | ResolvedExport::Unresolved => {}
        }
        resolved
    }

    /// Returns the flags of a module's own named export.
    #[must_use]
    pub fn flags(&self, module: SourceId, name: &EcmaString) -> Option<SymbolFlags> {
        Some(self.modules.get(&module)?.named.get(name)?.flags)
    }

    /// Returns the accepted `export =` target, if the module has one.
    #[must_use]
    pub fn assignment(&self, module: SourceId) -> Option<SymbolId> {
        self.modules.get(&module)?.assignment
    }

    /// Returns a module's own exported names in sorted order.
    pub fn exported_names(&self, module: SourceId) -> impl Iterator<Item = &EcmaString> {
        self.modules
            .get(&module)
            .into_iter()
            .flat_map(|entry| entry.named.keys())
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// Binds one named export. Re-recording the same origin unions flags; a
    /// different origin under the same name is a duplicate export.
    fn bind_named(
        &mut self,
        module: SourceId,
        name: EcmaString,
        export: NamedExport,
        source: SourceId,
        range: TextRange,
    ) {
        let entry = self.modules.entry(module).or_default();
        let flags = match entry.named.get(&name) {
            Some(existing) if existing.target != export.target => {
                self.diagnostics.push(Diagnostic::error(
                    DUPLICATE_DECLARATION,
                    source,
                    range,
                    DUPLICATE_EXPORT_MESSAGE,
                ));
                return;
            }
            Some(existing) => existing.flags.union(export.flags),
            None => export.flags,
        };
        entry.named.insert(
            name,
            NamedExport {
                target: export.target,
                flags,
            },
        );
        if !flags.visible_in(ResolveSpace::Value) {
            return;
        }
        entry.exports_a_value = true;
        self.report_mixed_assignment(module);
    }

    /// Reports `export =` mixed with a value export, once per module.
    fn report_mixed_assignment(&mut self, module: SourceId) {
        let Some(entry) = self.modules.get(&module) else {
            return;
        };
        if entry.reported_mixed_assignment {
            return;
        }
        let Some((source, range)) = entry.assignment_anchor else {
            return;
        };
        self.diagnostics.push(Diagnostic::error(
            MIXED_EXPORT_ASSIGNMENT,
            source,
            range,
            MIXED_EXPORT_ASSIGNMENT_MESSAGE,
        ));
        if let Some(entry) = self.modules.get_mut(&module) {
            entry.reported_mixed_assignment = true;
        }
    }

    fn resolve(&self, module: SourceId, name: &EcmaString, space: ResolveSpace) -> ResolvedExport {
        self.collect(module, name, space, &mut HashSet::new())
            .into_resolution()
    }

    /// Resolves one name, guarding the `(module, name)` pair against re-entry.
    fn collect(
        &self,
        module: SourceId,
        name: &EcmaString,
        space: ResolveSpace,
        visited: &mut HashSet<(SourceId, EcmaString)>,
    ) -> ResolutionSet {
        let key = (module, name.clone());
        if !visited.insert(key.clone()) {
            return ResolutionSet::cycle();
        }
        let resolved = self.collect_visible(module, name, space, visited);
        visited.remove(&key);
        resolved
    }

    fn collect_visible(
        &self,
        module: SourceId,
        name: &EcmaString,
        space: ResolveSpace,
        visited: &mut HashSet<(SourceId, EcmaString)>,
    ) -> ResolutionSet {
        let Some(exports) = self.modules.get(&module) else {
            return ResolutionSet::default();
        };
        if let Some(named) = exports.named.get(name)
            && named.flags.visible_in(space)
        {
            return match &named.target {
                ExportTarget::Local { source, symbol } => {
                    ResolutionSet::candidate(ExportCandidate::Symbol {
                        source: *source,
                        symbol: *symbol,
                    })
                }
                ExportTarget::Forward {
                    source,
                    name: forwarded,
                } => self.collect(*source, forwarded, space, visited),
                ExportTarget::StarNamespace { source } => {
                    ResolutionSet::candidate(ExportCandidate::Namespace { source: *source })
                }
            };
        }
        // `export *` never re-exports `default`.
        if name.as_units() == DEFAULT_EXPORT_NAME {
            return ResolutionSet::default();
        }
        let mut candidates = ResolutionSet::default();
        for star in &exports.stars {
            if matches!(star.specifier, SpecifierKind::TypeOnly)
                && !matches!(space, ResolveSpace::Type)
            {
                continue;
            }
            candidates.extend(self.collect(star.target, name, space, visited));
        }
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EXPORT_AMBIGUOUS, EXPORT_CYCLE, ExportTable, ResolveSpace, ResolvedExport, SpecifierKind,
        SymbolFlags, flags_from_kind,
    };
    use crate::checker::{DUPLICATE_DECLARATION, MIXED_EXPORT_ASSIGNMENT, SymbolId, SymbolKind};
    use crate::diagnostic::{Diagnostic, DiagnosticCode};
    use crate::source::{SourceId, TextRange, Utf16Pos};
    use bamts_bytecode::EcmaString;

    fn name(text: &str) -> EcmaString {
        EcmaString::encode(text)
    }

    fn range() -> TextRange {
        TextRange::new(Utf16Pos::ZERO, Utf16Pos::new(1)).expect("endpoints are ordered")
    }

    fn codes(table: &ExportTable) -> Vec<&str> {
        table
            .diagnostics()
            .iter()
            .map(Diagnostic::code)
            .map(DiagnosticCode::as_str)
            .collect()
    }

    fn value_export(table: &mut ExportTable, module: SourceId, text: &str, symbol: u32) {
        table.export_local(
            module,
            name(text),
            module,
            SymbolId::new(symbol),
            flags_from_kind(SymbolKind::Function),
        );
    }

    #[test]
    fn local_export_preserves_symbol_identity_and_flags() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        let symbol = SymbolId::new(7);
        table.export_local(
            module,
            name("Foo"),
            module,
            symbol,
            flags_from_kind(SymbolKind::Class),
        );
        assert_eq!(
            table.resolve_value(module, &name("Foo")),
            ResolvedExport::Symbol {
                source: module,
                symbol
            }
        );
        let flags = table.flags(module, &name("Foo")).expect("Foo is exported");
        assert!(flags.contains(SymbolFlags::EXPORT));
        assert!(flags.intersects(SymbolFlags::CLASS));
        assert_eq!(
            table.exported_names(module).collect::<Vec<_>>(),
            [&name("Foo")]
        );
    }

    #[test]
    fn a_class_export_is_visible_in_both_namespaces() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        let symbol = SymbolId::new(1);
        table.export_local(
            module,
            name("C"),
            module,
            symbol,
            flags_from_kind(SymbolKind::Class),
        );
        let expected = ResolvedExport::Symbol {
            source: module,
            symbol,
        };
        assert_eq!(table.resolve_value(module, &name("C")), expected);
        assert_eq!(table.resolve_type(module, &name("C")), expected);
    }

    #[test]
    fn reexport_chain_resolves_to_the_origin_symbol() {
        let mut table = ExportTable::new();
        let origin = SourceId::new(0);
        let middle = SourceId::new(1);
        let leaf = SourceId::new(2);
        let symbol = SymbolId::new(4);
        value_export(&mut table, origin, "Foo", 4);
        table.export_forward(
            middle,
            name("Foo"),
            origin,
            name("Foo"),
            SpecifierKind::Value,
            middle,
            range(),
        );
        table.export_forward(
            leaf,
            name("Bar"),
            middle,
            name("Foo"),
            SpecifierKind::Value,
            leaf,
            range(),
        );
        assert_eq!(
            table.resolve_value(leaf, &name("Bar")),
            ResolvedExport::Symbol {
                source: origin,
                symbol
            }
        );
        assert!(
            table
                .flags(leaf, &name("Bar"))
                .expect("Bar is exported")
                .contains(SymbolFlags::ALIAS)
        );
        assert!(!table.has_errors());
    }

    #[test]
    fn reexport_cycle_fails_deterministically() {
        let mut table = ExportTable::new();
        let left = SourceId::new(0);
        let right = SourceId::new(1);
        table.export_forward(
            left,
            name("K"),
            right,
            name("K"),
            SpecifierKind::Value,
            left,
            range(),
        );
        table.export_forward(
            right,
            name("K"),
            left,
            name("K"),
            SpecifierKind::Value,
            right,
            range(),
        );
        assert_eq!(table.resolve_value(left, &name("K")), ResolvedExport::Cycle);
        assert_eq!(
            table.resolve_value(right, &name("K")),
            ResolvedExport::Cycle
        );
        assert!(!table.has_errors());
        assert_eq!(
            table.resolve_reported(left, &name("K"), ResolveSpace::Value, left, range()),
            ResolvedExport::Cycle
        );
        assert_eq!(codes(&table), [EXPORT_CYCLE.as_str()]);
    }

    #[test]
    fn export_star_cycle_fails_deterministically() {
        let mut table = ExportTable::new();
        let left = SourceId::new(0);
        let right = SourceId::new(1);
        table.export_star(left, right, SpecifierKind::Value);
        table.export_star(right, left, SpecifierKind::Value);
        assert_eq!(table.resolve_value(left, &name("K")), ResolvedExport::Cycle);
    }

    #[test]
    fn export_star_diamond_over_one_origin_keeps_one_identity() {
        let mut table = ExportTable::new();
        let origin = SourceId::new(0);
        let forwarder = SourceId::new(1);
        let barrel = SourceId::new(2);
        let symbol = SymbolId::new(9);
        table.export_local(
            origin,
            name("K"),
            origin,
            symbol,
            flags_from_kind(SymbolKind::Enum),
        );
        table.export_forward(
            forwarder,
            name("K"),
            origin,
            name("K"),
            SpecifierKind::Value,
            forwarder,
            range(),
        );
        table.export_star(barrel, origin, SpecifierKind::Value);
        table.export_star(barrel, forwarder, SpecifierKind::Value);
        assert_eq!(
            table.resolve_value(barrel, &name("K")),
            ResolvedExport::Symbol {
                source: origin,
                symbol
            }
        );
        assert!(!table.has_errors());
    }

    #[test]
    fn export_star_over_two_origins_is_ambiguous() {
        let mut table = ExportTable::new();
        let left = SourceId::new(0);
        let right = SourceId::new(1);
        let barrel = SourceId::new(2);
        value_export(&mut table, left, "K", 1);
        value_export(&mut table, right, "K", 2);
        table.export_star(barrel, left, SpecifierKind::Value);
        table.export_star(barrel, right, SpecifierKind::Value);
        assert_eq!(
            table.resolve_value(barrel, &name("K")),
            ResolvedExport::Ambiguous
        );
        table.resolve_reported(barrel, &name("K"), ResolveSpace::Value, barrel, range());
        assert_eq!(codes(&table), [EXPORT_AMBIGUOUS.as_str()]);
    }

    #[test]
    fn a_local_named_export_shadows_export_star() {
        let mut table = ExportTable::new();
        let origin = SourceId::new(0);
        let barrel = SourceId::new(1);
        value_export(&mut table, origin, "K", 1);
        value_export(&mut table, barrel, "K", 3);
        table.export_star(barrel, origin, SpecifierKind::Value);
        assert_eq!(
            table.resolve_value(barrel, &name("K")),
            ResolvedExport::Symbol {
                source: barrel,
                symbol: SymbolId::new(3)
            }
        );
    }

    #[test]
    fn an_unresolved_name_is_not_a_cycle() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        value_export(&mut table, module, "Present", 1);
        assert_eq!(
            table.resolve_value(module, &name("Missing")),
            ResolvedExport::Unresolved
        );
        assert_eq!(
            table.resolve_value(SourceId::new(9), &name("Present")),
            ResolvedExport::Unresolved
        );
    }

    #[test]
    fn type_only_export_is_invisible_in_value_space() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        let symbol = SymbolId::new(5);
        table.export_local(
            module,
            name("T"),
            module,
            symbol,
            flags_from_kind(SymbolKind::Interface).union(SymbolFlags::TYPE_ONLY),
        );
        assert_eq!(
            table.resolve_value(module, &name("T")),
            ResolvedExport::Unresolved
        );
        assert_eq!(
            table.resolve_type(module, &name("T")),
            ResolvedExport::Symbol {
                source: module,
                symbol
            }
        );
    }

    #[test]
    fn a_type_only_local_does_not_hide_a_value_star() {
        let mut table = ExportTable::new();
        let origin = SourceId::new(0);
        let barrel = SourceId::new(1);
        value_export(&mut table, origin, "T", 8);
        table.export_local(
            barrel,
            name("T"),
            barrel,
            SymbolId::new(2),
            flags_from_kind(SymbolKind::Interface).union(SymbolFlags::TYPE_ONLY),
        );
        table.export_star(barrel, origin, SpecifierKind::Value);
        assert_eq!(
            table.resolve_value(barrel, &name("T")),
            ResolvedExport::Symbol {
                source: origin,
                symbol: SymbolId::new(8)
            }
        );
        assert_eq!(
            table.resolve_type(barrel, &name("T")),
            ResolvedExport::Symbol {
                source: barrel,
                symbol: SymbolId::new(2)
            }
        );
    }

    #[test]
    fn a_type_only_star_is_invisible_in_value_space() {
        let mut table = ExportTable::new();
        let origin = SourceId::new(0);
        let barrel = SourceId::new(1);
        let symbol = SymbolId::new(3);
        table.export_local(
            origin,
            name("Shape"),
            origin,
            symbol,
            flags_from_kind(SymbolKind::Interface),
        );
        table.export_star(barrel, origin, SpecifierKind::TypeOnly);
        assert_eq!(
            table.resolve_value(barrel, &name("Shape")),
            ResolvedExport::Unresolved
        );
        assert_eq!(
            table.resolve_type(barrel, &name("Shape")),
            ResolvedExport::Symbol {
                source: origin,
                symbol
            }
        );
    }

    #[test]
    fn a_type_only_forward_keeps_the_origin_in_type_space_only() {
        let mut table = ExportTable::new();
        let origin = SourceId::new(0);
        let leaf = SourceId::new(1);
        let symbol = SymbolId::new(6);
        table.export_local(
            origin,
            name("T"),
            origin,
            symbol,
            flags_from_kind(SymbolKind::TypeAlias),
        );
        table.export_forward(
            leaf,
            name("T"),
            origin,
            name("T"),
            SpecifierKind::TypeOnly,
            leaf,
            range(),
        );
        assert_eq!(
            table.resolve_value(leaf, &name("T")),
            ResolvedExport::Unresolved
        );
        assert_eq!(
            table.resolve_type(leaf, &name("T")),
            ResolvedExport::Symbol {
                source: origin,
                symbol
            }
        );
    }

    #[test]
    fn a_type_only_export_of_a_value_kind_is_invisible_in_value_space() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        let symbol = SymbolId::new(3);
        table.export_local(
            module,
            name("Klass"),
            module,
            symbol,
            flags_from_kind(SymbolKind::Class).union(SymbolFlags::TYPE_ONLY),
        );
        assert_eq!(
            table.resolve_value(module, &name("Klass")),
            ResolvedExport::Unresolved
        );
        assert_eq!(
            table.resolve_type(module, &name("Klass")),
            ResolvedExport::Symbol {
                source: module,
                symbol
            }
        );
    }

    #[test]
    fn a_type_only_forward_to_a_value_origin_is_invisible_in_value_space() {
        let mut table = ExportTable::new();
        let origin = SourceId::new(0);
        let leaf = SourceId::new(1);
        let symbol = SymbolId::new(4);
        table.export_local(
            origin,
            name("K"),
            origin,
            symbol,
            flags_from_kind(SymbolKind::Class),
        );
        table.export_forward(
            leaf,
            name("K"),
            origin,
            name("K"),
            SpecifierKind::TypeOnly,
            leaf,
            range(),
        );
        assert_eq!(
            table.resolve_value(leaf, &name("K")),
            ResolvedExport::Unresolved
        );
        assert_eq!(
            table.resolve_type(leaf, &name("K")),
            ResolvedExport::Symbol {
                source: origin,
                symbol
            }
        );
        assert_eq!(
            table.resolve_value(origin, &name("K")),
            ResolvedExport::Symbol {
                source: origin,
                symbol
            }
        );
    }

    #[test]
    fn export_star_does_not_reexport_default() {
        let mut table = ExportTable::new();
        let origin = SourceId::new(0);
        let barrel = SourceId::new(1);
        table.export_default(
            origin,
            origin,
            SymbolId::new(1),
            flags_from_kind(SymbolKind::Function),
        );
        table.export_star(barrel, origin, SpecifierKind::Value);
        assert_eq!(
            table.resolve_value(barrel, &name("default")),
            ResolvedExport::Unresolved
        );
        assert_eq!(
            table.resolve_value(origin, &name("default")),
            ResolvedExport::Symbol {
                source: origin,
                symbol: SymbolId::new(1)
            }
        );
        assert!(
            table
                .flags(origin, &name("default"))
                .expect("default is exported")
                .contains(SymbolFlags::DEFAULT)
        );
    }

    #[test]
    fn export_star_as_resolves_to_a_namespace_whose_members_keep_identity() {
        let mut table = ExportTable::new();
        let origin = SourceId::new(0);
        let barrel = SourceId::new(1);
        table.export_local(
            origin,
            name("K"),
            origin,
            SymbolId::new(1),
            flags_from_kind(SymbolKind::Enum),
        );
        table.export_star_as(barrel, name("Enums"), origin, barrel);
        let resolved = table.resolve_value(barrel, &name("Enums"));
        assert_eq!(resolved, ResolvedExport::Namespace { source: origin });
        let ResolvedExport::Namespace { source } = resolved else {
            panic!("expected a namespace, got {resolved:?}");
        };
        assert_eq!(
            table.resolve_value(source, &name("K")),
            ResolvedExport::Symbol {
                source: origin,
                symbol: SymbolId::new(1)
            }
        );
    }

    #[test]
    fn a_duplicate_exported_name_conflicts_and_keeps_the_first_origin() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        value_export(&mut table, module, "A", 1);
        value_export(&mut table, module, "A", 2);
        assert_eq!(codes(&table), [DUPLICATE_DECLARATION.as_str()]);
        assert_eq!(
            table.resolve_value(module, &name("A")),
            ResolvedExport::Symbol {
                source: module,
                symbol: SymbolId::new(1)
            }
        );
    }

    #[test]
    fn re_recording_the_same_origin_unions_flags() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        let symbol = SymbolId::new(1);
        table.export_local(
            module,
            name("A"),
            module,
            symbol,
            flags_from_kind(SymbolKind::Interface),
        );
        table.export_local(
            module,
            name("A"),
            module,
            symbol,
            flags_from_kind(SymbolKind::Interface).union(SymbolFlags::TYPE_ONLY),
        );
        assert!(!table.has_errors());
        let flags = table.flags(module, &name("A")).expect("A is exported");
        assert!(flags.contains(SymbolFlags::TYPE_ONLY));
        assert!(flags.intersects(SymbolFlags::INTERFACE));
    }

    #[test]
    fn export_assignment_rejects_an_earlier_value_export() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        value_export(&mut table, module, "helper", 1);
        table.export_assignment(module, SymbolId::new(2), module, range());
        assert_eq!(codes(&table), [MIXED_EXPORT_ASSIGNMENT.as_str()]);
        assert!(table.assignment(module).is_none());
    }

    #[test]
    fn export_assignment_rejects_a_later_value_export_once() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        table.export_assignment(module, SymbolId::new(2), module, range());
        value_export(&mut table, module, "helper", 1);
        value_export(&mut table, module, "other", 3);
        assert_eq!(codes(&table), [MIXED_EXPORT_ASSIGNMENT.as_str()]);
        assert_eq!(table.assignment(module), Some(SymbolId::new(2)));
    }

    #[test]
    fn export_assignment_rejects_a_value_star() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        table.export_star(module, SourceId::new(1), SpecifierKind::Value);
        table.export_assignment(module, SymbolId::new(2), module, range());
        assert_eq!(codes(&table), [MIXED_EXPORT_ASSIGNMENT.as_str()]);
        assert!(table.assignment(module).is_none());
    }

    #[test]
    fn export_assignment_accepts_type_only_exports() {
        let mut table = ExportTable::new();
        let module = SourceId::new(0);
        table.export_local(
            module,
            name("Shape"),
            module,
            SymbolId::new(1),
            flags_from_kind(SymbolKind::Interface).union(SymbolFlags::TYPE_ONLY),
        );
        table.export_star(module, SourceId::new(1), SpecifierKind::TypeOnly);
        table.export_assignment(module, SymbolId::new(2), module, range());
        assert!(!table.has_errors());
        assert_eq!(table.assignment(module), Some(SymbolId::new(2)));
    }

    #[test]
    fn kind_flags_select_the_right_namespace() {
        let interface = flags_from_kind(SymbolKind::Interface);
        assert!(interface.visible_in(ResolveSpace::Type));
        assert!(!interface.visible_in(ResolveSpace::Value));

        let function = flags_from_kind(SymbolKind::Function).union(SymbolFlags::EXPORT);
        assert!(function.visible_in(ResolveSpace::Value));
        assert!(!function.visible_in(ResolveSpace::Type));

        let namespace = flags_from_kind(SymbolKind::Namespace);
        assert!(namespace.visible_in(ResolveSpace::Value));
        assert!(namespace.visible_in(ResolveSpace::Type));

        let alias = flags_from_kind(SymbolKind::Import);
        assert!(alias.visible_in(ResolveSpace::Value));
        assert!(alias.visible_in(ResolveSpace::Type));

        assert!(!SymbolFlags::NONE.contains(SymbolFlags::NONE));
        assert_eq!(
            (SymbolFlags::EXPORT | SymbolFlags::ALIAS).bits(),
            SymbolFlags::EXPORT.union(SymbolFlags::ALIAS).bits()
        );
    }
}
