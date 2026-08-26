//! Enum and namespace semantic parity checks.
//!
//! This module extends [`crate::enum_plan`] and [`crate::namespace_plan`]
//! rather than repeating them. Those modules own member evaluation, the scalar
//! value model, container acquisition, and the export tables; the rules here
//! consume those already-derived facts and report the parity errors the plan
//! builders deliberately leave to a semantic pass.
//!
//! Like [`crate::checker::narrowing`], the engine never walks syntax. Callers
//! feed facts plus a [`DeclarationSite`], so ranges stay accurate without this
//! module borrowing the syntax tree.
//!
//! Ownership boundary: [`crate::enum_plan`] already emits `BAMTS-C005` through
//! `BAMTS-C011` for member naming, auto-numbering, constness, self-reference,
//! and merged-first-member conflicts. Nothing here re-reports those.

use std::collections::HashMap;

use bamts_bytecode::EcmaString;

use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::enum_plan::{EnumScalar, EnumValue};
use crate::source::{SourceId, TextRange};

/// Diagnostic emitted when one enum declares the same member name twice.
pub const DUPLICATE_ENUM_MEMBER: DiagnosticCode = DiagnosticCode::new("BAMTS-C060");
/// Diagnostic emitted when a const enum appears outside an access position.
pub const CONST_ENUM_INVALID_USE: DiagnosticCode = DiagnosticCode::new("BAMTS-C061");
/// Diagnostic emitted when a const enum is indexed by a non-literal key.
pub const CONST_ENUM_INVALID_ELEMENT_ACCESS: DiagnosticCode = DiagnosticCode::new("BAMTS-C062");
/// Diagnostic emitted when an ambient const enum is compiled with isolated modules.
pub const AMBIENT_CONST_ENUM_ISOLATED_MODULES: DiagnosticCode = DiagnosticCode::new("BAMTS-C063");
/// Diagnostic emitted when an enum merges with a declaration that is not an enum or namespace.
pub const ENUM_MERGE_INVALID_TARGET: DiagnosticCode = DiagnosticCode::new("BAMTS-C064");
/// Diagnostic emitted when a computed member follows a string-valued member.
pub const COMPUTED_MEMBER_IN_STRING_ENUM: DiagnosticCode = DiagnosticCode::new("BAMTS-C065");
/// Diagnostic emitted when a namespace precedes the class or function it merges with.
pub const NAMESPACE_BEFORE_MERGED_DECLARATION: DiagnosticCode = DiagnosticCode::new("BAMTS-C066");
/// Diagnostic emitted when a non-instantiated namespace is used as a value.
pub const NON_INSTANTIATED_NAMESPACE_VALUE_USE: DiagnosticCode = DiagnosticCode::new("BAMTS-C067");
/// Diagnostic emitted when a qualified name reaches a non-exported member.
pub const NAMESPACE_MEMBER_NOT_EXPORTED: DiagnosticCode = DiagnosticCode::new("BAMTS-C068");

const DUPLICATE_MEMBER_MESSAGE: &str = "Duplicate identifier in enum declaration.";
const CONST_ENUM_USE_MESSAGE: &str = concat!(
    "A 'const' enum can only be used in a property or index access expression, ",
    "on the right hand side of an import declaration or export assignment, or in a type query."
);
const CONST_ENUM_ELEMENT_MESSAGE: &str =
    "A const enum member can only be accessed using a string literal.";
const AMBIENT_CONST_ENUM_MESSAGE: &str =
    "Ambient const enums are not allowed when the '--isolatedModules' flag is provided.";
const ENUM_MERGE_MESSAGE: &str =
    "An enum declaration can only merge with another enum or a namespace.";
const COMPUTED_IN_STRING_ENUM_MESSAGE: &str =
    "Computed values are not permitted in an enum with string valued members.";
const NAMESPACE_ORDER_MESSAGE: &str = concat!(
    "A namespace declaration cannot be located prior to a class or function ",
    "with which it is merged."
);
const NON_INSTANTIATED_MESSAGE: &str =
    "A namespace with only type declarations has no value and cannot be used as one.";
const NOT_EXPORTED_MESSAGE: &str = "Property is not exported from this namespace.";

/// A source-anchored declaration or reference position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeclarationSite {
    source: SourceId,
    range: TextRange,
}

impl DeclarationSite {
    /// Anchors a rule at one source range.
    #[must_use]
    pub const fn new(source: SourceId, range: TextRange) -> Self {
        Self { source, range }
    }

    /// The owning source file.
    #[must_use]
    pub const fn source(&self) -> SourceId {
        self.source
    }

    /// The reported range.
    #[must_use]
    pub const fn range(&self) -> TextRange {
        self.range
    }
}

/// One already-evaluated enum member, keyed by its cooked property name.
///
/// The value uses [`EnumValue`] so the scalar model stays owned by
/// [`crate::enum_plan`].
#[derive(Clone, Debug)]
pub struct EnumMemberFact {
    name: EcmaString,
    value: EnumValue,
    site: DeclarationSite,
}

impl EnumMemberFact {
    /// Records one checked member.
    #[must_use]
    pub const fn new(name: EcmaString, value: EnumValue, site: DeclarationSite) -> Self {
        Self { name, value, site }
    }

    /// The cooked member name.
    #[must_use]
    pub const fn name(&self) -> &EcmaString {
        &self.name
    }

    /// Where the member is reported.
    #[must_use]
    pub const fn site(&self) -> DeclarationSite {
        self.site
    }

    fn is_string_valued(&self) -> bool {
        matches!(self.value.constant(), Some(EnumScalar::String(_)))
    }

    fn is_computed(&self) -> bool {
        self.value.constant().is_none()
    }
}

/// How a const enum reference appears in source.
///
/// Only access, alias, and type positions preserve a const enum's erasure; any
/// other position would require a runtime object that is never emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConstEnumUse {
    /// `E.Member`
    PropertyAccess,
    /// `E["Member"]`
    ElementAccessWithStringLiteral,
    /// `E[expr]` where `expr` is not a string literal.
    ElementAccessComputed,
    /// The right hand side of `import X = E`.
    ImportEqualsTarget,
    /// The right hand side of `export = E`.
    ExportAssignment,
    /// A `typeof E` query or other type position.
    TypeQuery,
    /// A bare value reference such as passing `E` as an argument.
    ValueReference,
}

impl ConstEnumUse {
    const fn diagnostic_code(self) -> Option<(DiagnosticCode, &'static str)> {
        match self {
            Self::PropertyAccess
            | Self::ElementAccessWithStringLiteral
            | Self::ImportEqualsTarget
            | Self::ExportAssignment
            | Self::TypeQuery => None,
            Self::ElementAccessComputed => Some((
                CONST_ENUM_INVALID_ELEMENT_ACCESS,
                CONST_ENUM_ELEMENT_MESSAGE,
            )),
            Self::ValueReference => Some((CONST_ENUM_INVALID_USE, CONST_ENUM_USE_MESSAGE)),
        }
    }
}

/// The kind of a declaration participating in a merge group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MergeKind {
    /// An `enum` or `const enum` declaration.
    Enum,
    /// A `namespace` or `module` declaration.
    Namespace,
    /// A `class` declaration.
    Class,
    /// A `function` declaration.
    Function,
    /// An `interface` declaration.
    Interface,
    /// A `var`, `let`, or `const` declaration.
    Variable,
    /// A `type` alias declaration.
    TypeAlias,
}

impl MergeKind {
    const fn merges_with_enum(self) -> bool {
        matches!(self, Self::Enum | Self::Namespace)
    }

    const fn requires_namespace_after(self) -> bool {
        matches!(self, Self::Class | Self::Function)
    }
}

/// One declaration in a merge group, ordered by source position.
#[derive(Clone, Copy, Debug)]
pub struct MergeDeclaration {
    kind: MergeKind,
    order: u32,
    site: DeclarationSite,
}

impl MergeDeclaration {
    /// Records one merge participant. `order` is the declaration's source rank.
    #[must_use]
    pub const fn new(kind: MergeKind, order: u32, site: DeclarationSite) -> Self {
        Self { kind, order, site }
    }

    /// The declared kind.
    #[must_use]
    pub const fn kind(&self) -> MergeKind {
        self.kind
    }

    /// The source rank used for ordering rules.
    #[must_use]
    pub const fn order(&self) -> u32 {
        self.order
    }

    /// Where the declaration is reported.
    #[must_use]
    pub const fn site(&self) -> DeclarationSite {
        self.site
    }
}

/// Reports duplicate member names and computed members in string enums.
///
/// Members must arrive in source order. Heterogeneous numeric/string enums stay
/// legal; only a *computed* member alongside a string-valued member is an error.
#[must_use]
pub fn check_enum_members(members: &[EnumMemberFact]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut seen: HashMap<&EcmaString, ()> = HashMap::new();
    let has_string_member = members.iter().any(EnumMemberFact::is_string_valued);

    for member in members {
        if seen.insert(member.name(), ()).is_some() {
            diagnostics.push(error(
                DUPLICATE_ENUM_MEMBER,
                member.site(),
                DUPLICATE_MEMBER_MESSAGE,
            ));
        }
        if has_string_member && member.is_computed() {
            diagnostics.push(error(
                COMPUTED_MEMBER_IN_STRING_ENUM,
                member.site(),
                COMPUTED_IN_STRING_ENUM_MESSAGE,
            ));
        }
    }
    diagnostics
}

/// Reports a const enum reference that cannot survive erasure.
#[must_use]
pub fn check_const_enum_use(use_kind: ConstEnumUse, site: DeclarationSite) -> Option<Diagnostic> {
    use_kind
        .diagnostic_code()
        .map(|(code, message)| error(code, site, message))
}

/// Reports an ambient const enum that cannot be erased under isolated modules.
#[must_use]
pub fn check_ambient_const_enum(
    is_ambient: bool,
    is_const: bool,
    isolated_modules: bool,
    site: DeclarationSite,
) -> Option<Diagnostic> {
    if is_ambient && is_const && isolated_modules {
        Some(error(
            AMBIENT_CONST_ENUM_ISOLATED_MODULES,
            site,
            AMBIENT_CONST_ENUM_MESSAGE,
        ))
    } else {
        None
    }
}

/// Reports merge partners an enum cannot legally merge with.
///
/// Constness agreement across a merged enum is already reported by
/// [`crate::enum_plan`]; this rule only rejects illegal partner *kinds*.
#[must_use]
pub fn check_enum_merge_group(declarations: &[MergeDeclaration]) -> Vec<Diagnostic> {
    declarations
        .iter()
        .filter(|declaration| !declaration.kind().merges_with_enum())
        .map(|declaration| {
            error(
                ENUM_MERGE_INVALID_TARGET,
                declaration.site(),
                ENUM_MERGE_MESSAGE,
            )
        })
        .collect()
}

/// Reports a namespace declared before a class or function it merges with.
#[must_use]
pub fn check_namespace_merge_order(
    namespace: MergeDeclaration,
    partners: &[MergeDeclaration],
) -> Vec<Diagnostic> {
    partners
        .iter()
        .filter(|partner| {
            partner.kind().requires_namespace_after() && partner.order() > namespace.order()
        })
        .map(|_| {
            error(
                NAMESPACE_BEFORE_MERGED_DECLARATION,
                namespace.site(),
                NAMESPACE_ORDER_MESSAGE,
            )
        })
        .collect()
}

/// Reports a namespace used as a value when it declares no runtime member.
///
/// `is_value_bearing` comes from
/// [`crate::namespace_plan::NamespacePlan::is_value_bearing`].
#[must_use]
pub fn check_namespace_value_use(
    is_value_bearing: bool,
    site: DeclarationSite,
) -> Option<Diagnostic> {
    if is_value_bearing {
        None
    } else {
        Some(error(
            NON_INSTANTIATED_NAMESPACE_VALUE_USE,
            site,
            NON_INSTANTIATED_MESSAGE,
        ))
    }
}

/// Reports a qualified name that reaches a member the namespace never exports.
///
/// Ambient namespace members are implicitly exported, so `is_ambient` suppresses
/// the rule.
#[must_use]
pub fn check_namespace_member_access(
    is_exported: bool,
    is_ambient: bool,
    site: DeclarationSite,
) -> Option<Diagnostic> {
    if is_exported || is_ambient {
        None
    } else {
        Some(error(
            NAMESPACE_MEMBER_NOT_EXPORTED,
            site,
            NOT_EXPORTED_MESSAGE,
        ))
    }
}

fn error(code: DiagnosticCode, site: DeclarationSite, message: &'static str) -> Diagnostic {
    Diagnostic::error(code, site.source(), site.range(), message)
}

#[cfg(test)]
mod tests {
    use super::{
        AMBIENT_CONST_ENUM_ISOLATED_MODULES, COMPUTED_MEMBER_IN_STRING_ENUM,
        CONST_ENUM_INVALID_ELEMENT_ACCESS, CONST_ENUM_INVALID_USE, ConstEnumUse,
        DUPLICATE_ENUM_MEMBER, DeclarationSite, ENUM_MERGE_INVALID_TARGET, EnumMemberFact,
        MergeDeclaration, MergeKind, NAMESPACE_BEFORE_MERGED_DECLARATION,
        NAMESPACE_MEMBER_NOT_EXPORTED, NON_INSTANTIATED_NAMESPACE_VALUE_USE,
        check_ambient_const_enum, check_const_enum_use, check_enum_members, check_enum_merge_group,
        check_namespace_member_access, check_namespace_merge_order, check_namespace_value_use,
    };
    use crate::diagnostic::Diagnostic;
    use crate::enum_plan::{EnumScalar, EnumValue};
    use crate::source::{SourceId, TextRange, Utf16Pos};
    use bamts_bytecode::{EcmaString, NumberBits};

    fn site() -> DeclarationSite {
        DeclarationSite::new(SourceId::new(0), range())
    }

    fn range() -> TextRange {
        TextRange::new(Utf16Pos::ZERO, Utf16Pos::new(1)).expect("ordered")
    }

    fn codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect()
    }

    fn numeric(name: &str, value: f64) -> EnumMemberFact {
        EnumMemberFact::new(
            EcmaString::encode(name),
            EnumValue::Constant(EnumScalar::Number(NumberBits::from_f64(value))),
            site(),
        )
    }

    fn string_member(name: &str, value: &str) -> EnumMemberFact {
        EnumMemberFact::new(
            EcmaString::encode(name),
            EnumValue::Constant(EnumScalar::String(EcmaString::encode(value))),
            site(),
        )
    }

    fn computed(name: &str) -> EnumMemberFact {
        EnumMemberFact::new(EcmaString::encode(name), EnumValue::Runtime, site())
    }

    #[test]
    fn distinct_numeric_members_are_accepted() {
        let members = [numeric("A", 0.0), numeric("B", 1.0)];
        assert!(check_enum_members(&members).is_empty());
    }

    #[test]
    fn heterogeneous_numeric_and_string_members_stay_legal() {
        let members = [numeric("A", 0.0), string_member("B", "b")];
        assert!(check_enum_members(&members).is_empty());
    }

    #[test]
    fn repeated_member_name_reports_once_on_the_later_member() {
        let members = [numeric("A", 0.0), numeric("A", 1.0)];
        assert_eq!(
            codes(&check_enum_members(&members)),
            [DUPLICATE_ENUM_MEMBER.as_str()]
        );
    }

    #[test]
    fn computed_member_beside_string_member_is_rejected() {
        let members = [string_member("A", "a"), computed("B")];
        assert_eq!(
            codes(&check_enum_members(&members)),
            [COMPUTED_MEMBER_IN_STRING_ENUM.as_str()]
        );
    }

    #[test]
    fn computed_member_in_numeric_enum_is_accepted() {
        let members = [numeric("A", 0.0), computed("B")];
        assert!(check_enum_members(&members).is_empty());
    }

    #[test]
    fn const_enum_access_and_alias_positions_are_accepted() {
        for use_kind in [
            ConstEnumUse::PropertyAccess,
            ConstEnumUse::ElementAccessWithStringLiteral,
            ConstEnumUse::ImportEqualsTarget,
            ConstEnumUse::ExportAssignment,
            ConstEnumUse::TypeQuery,
        ] {
            assert!(
                check_const_enum_use(use_kind, site()).is_none(),
                "{use_kind:?} must be accepted"
            );
        }
    }

    #[test]
    fn const_enum_value_reference_is_rejected() {
        let diagnostic =
            check_const_enum_use(ConstEnumUse::ValueReference, site()).expect("rejected");
        assert_eq!(diagnostic.code(), CONST_ENUM_INVALID_USE);
    }

    #[test]
    fn const_enum_computed_element_access_is_rejected() {
        let diagnostic =
            check_const_enum_use(ConstEnumUse::ElementAccessComputed, site()).expect("rejected");
        assert_eq!(diagnostic.code(), CONST_ENUM_INVALID_ELEMENT_ACCESS);
    }

    #[test]
    fn ambient_const_enum_is_rejected_only_under_isolated_modules() {
        assert!(check_ambient_const_enum(true, true, false, site()).is_none());
        assert!(check_ambient_const_enum(true, false, true, site()).is_none());
        assert!(check_ambient_const_enum(false, true, true, site()).is_none());
        let diagnostic = check_ambient_const_enum(true, true, true, site()).expect("rejected");
        assert_eq!(diagnostic.code(), AMBIENT_CONST_ENUM_ISOLATED_MODULES);
    }

    #[test]
    fn enum_merges_with_enum_and_namespace_only() {
        let accepted = [
            MergeDeclaration::new(MergeKind::Enum, 0, site()),
            MergeDeclaration::new(MergeKind::Namespace, 1, site()),
        ];
        assert!(check_enum_merge_group(&accepted).is_empty());

        let rejected = [MergeDeclaration::new(MergeKind::Class, 0, site())];
        assert_eq!(
            codes(&check_enum_merge_group(&rejected)),
            [ENUM_MERGE_INVALID_TARGET.as_str()]
        );
    }

    #[test]
    fn namespace_before_merged_class_is_rejected() {
        let namespace = MergeDeclaration::new(MergeKind::Namespace, 0, site());
        let partners = [MergeDeclaration::new(MergeKind::Class, 1, site())];
        assert_eq!(
            codes(&check_namespace_merge_order(namespace, &partners)),
            [NAMESPACE_BEFORE_MERGED_DECLARATION.as_str()]
        );
    }

    #[test]
    fn namespace_after_merged_class_is_accepted() {
        let namespace = MergeDeclaration::new(MergeKind::Namespace, 1, site());
        let partners = [MergeDeclaration::new(MergeKind::Class, 0, site())];
        assert!(check_namespace_merge_order(namespace, &partners).is_empty());
    }

    #[test]
    fn namespace_before_merged_interface_is_accepted() {
        let namespace = MergeDeclaration::new(MergeKind::Namespace, 0, site());
        let partners = [MergeDeclaration::new(MergeKind::Interface, 1, site())];
        assert!(check_namespace_merge_order(namespace, &partners).is_empty());
    }

    #[test]
    fn non_instantiated_namespace_cannot_be_a_value() {
        assert!(check_namespace_value_use(true, site()).is_none());
        let diagnostic = check_namespace_value_use(false, site()).expect("rejected");
        assert_eq!(diagnostic.code(), NON_INSTANTIATED_NAMESPACE_VALUE_USE);
    }

    #[test]
    fn non_exported_member_is_rejected_unless_ambient() {
        assert!(check_namespace_member_access(true, false, site()).is_none());
        assert!(check_namespace_member_access(false, true, site()).is_none());
        let diagnostic = check_namespace_member_access(false, false, site()).expect("rejected");
        assert_eq!(diagnostic.code(), NAMESPACE_MEMBER_NOT_EXPORTED);
    }
}
