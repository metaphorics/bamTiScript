//! Semantic validation for legacy and stage-3 decorators.
//!
//! The parser and binder own decorator attachment and target-type derivation.
//! This module consumes those facts, checks the invocation contract through the
//! canonical [`TypeTable`], and reports declaration-placement errors. It never
//! walks syntax and does not introduce a second type or symbol model.

use crate::checker::{CONSTRUCTOR_DECORATOR_NOT_SUPPORTED, Type, TypeId, TypeTable};
use crate::diagnostic::{Diagnostic, DiagnosticCode};
use crate::source::{SourceId, TextRange};

/// A decorator expression has no call signature.
pub const DECORATOR_NOT_CALLABLE: DiagnosticCode = DiagnosticCode::new("BAMTS-C070");
/// A decorator's parameters do not accept the values supplied for its target.
pub const DECORATOR_ARGUMENT_MISMATCH: DiagnosticCode = DiagnosticCode::new("BAMTS-C071");
/// A decorator returns a value that cannot replace its target.
pub const DECORATOR_RETURN_MISMATCH: DiagnosticCode = DiagnosticCode::new("BAMTS-C072");
/// Decorators cannot decorate declarations without a runtime body.
pub const DECORATOR_ON_AMBIENT_DECLARATION: DiagnosticCode = DiagnosticCode::new("BAMTS-C073");
/// Only one accessor of a get/set pair may carry decorators in legacy mode.
pub const DECORATOR_ON_BOTH_ACCESSORS: DiagnosticCode = DiagnosticCode::new("BAMTS-C074");
/// Decorators cannot attach to a class static block.
pub const DECORATOR_ON_STATIC_BLOCK: DiagnosticCode = DiagnosticCode::new("BAMTS-C075");
/// Stage-3 decorators do not support parameters.
pub const STAGE3_PARAMETER_DECORATOR: DiagnosticCode = DiagnosticCode::new("BAMTS-C076");
/// A stage-3 class decorator cannot occur on both sides of `export`.
pub const DECORATOR_BOTH_SIDES_OF_EXPORT: DiagnosticCode = DiagnosticCode::new("BAMTS-C077");
/// Metadata emission requires the legacy decorator transform.
pub const METADATA_REQUIRES_EXPERIMENTAL_DECORATORS: DiagnosticCode =
    DiagnosticCode::new("BAMTS-C078");
/// Legacy decorators are disabled unless `experimentalDecorators` is enabled.
pub const LEGACY_DECORATORS_DISABLED: DiagnosticCode = DiagnosticCode::new("BAMTS-C079");

const NOT_CALLABLE_MESSAGE: &str = "This expression is not callable as a decorator.";
const ARGUMENT_MISMATCH_MESSAGE: &str =
    "The decorator signature cannot accept the arguments supplied for this declaration.";
const RETURN_MISMATCH_MESSAGE: &str =
    "The decorator return type cannot replace the decorated declaration.";
const AMBIENT_MESSAGE: &str = "Decorators are not valid on ambient declarations.";
const BOTH_ACCESSORS_MESSAGE: &str =
    "Decorators cannot be applied to both the get and set accessor of the same member.";
const STATIC_BLOCK_MESSAGE: &str = "Decorators are not valid on class static blocks.";
const STAGE3_PARAMETER_MESSAGE: &str = "Decorators are not valid on parameters in stage-3 mode.";
const BOTH_EXPORT_SIDES_MESSAGE: &str =
    "Decorators may appear before or after 'export', but not in both positions.";
const CONSTRUCTOR_MESSAGE: &str = "Constructor decorators are not supported.";
const METADATA_MESSAGE: &str =
    "Option 'emitDecoratorMetadata' cannot be specified without 'experimentalDecorators'.";
const LEGACY_DISABLED_MESSAGE: &str =
    "Legacy decorators require the 'experimentalDecorators' compiler option.";

/// Source location for a decorator or decorated declaration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecoratorSite {
    source: SourceId,
    range: TextRange,
}

impl DecoratorSite {
    /// Anchors a decorator rule at one source range.
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

/// Which decorator proposal supplies the invocation contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecoratorMode {
    /// TypeScript's `experimentalDecorators` transform.
    Legacy,
    /// The standard `(value, context)` decorator proposal.
    Stage3,
}

/// The declaration kind receiving a decorator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecoratorTarget {
    Class,
    Method,
    Getter,
    Setter,
    Field,
    AutoAccessor,
    Parameter,
    Constructor,
    StaticBlock,
}

impl DecoratorTarget {
    const fn expected_arity(self, mode: DecoratorMode) -> Option<usize> {
        match mode {
            DecoratorMode::Legacy => match self {
                Self::Class | Self::Constructor => Some(1),
                Self::Method | Self::Getter | Self::Setter => Some(3),
                Self::Field => Some(2),
                Self::Parameter => Some(3),
                Self::AutoAccessor => Some(3),
                Self::StaticBlock => None,
            },
            DecoratorMode::Stage3 => match self {
                Self::Class
                | Self::Method
                | Self::Getter
                | Self::Setter
                | Self::Field
                | Self::AutoAccessor => Some(2),
                Self::Parameter | Self::Constructor | Self::StaticBlock => None,
            },
        }
    }
}

/// The exact values the transform passes and return types it accepts.
///
/// Callers derive these canonical [`TypeId`]s from the decorated declaration:
/// legacy class decorators receive the constructor; member decorators receive
/// target/key/descriptor as appropriate; stage-3 decorators receive value and
/// the per-kind context object. `allowed_returns` includes `void` and every
/// target replacement type legal for that declaration kind.
#[derive(Clone, Copy, Debug)]
pub struct DecoratorContract<'a> {
    mode: DecoratorMode,
    target: DecoratorTarget,
    arguments: &'a [TypeId],
    allowed_returns: &'a [TypeId],
}

impl<'a> DecoratorContract<'a> {
    /// Creates a contract from binder/checker-derived canonical types.
    #[must_use]
    pub const fn new(
        mode: DecoratorMode,
        target: DecoratorTarget,
        arguments: &'a [TypeId],
        allowed_returns: &'a [TypeId],
    ) -> Self {
        Self {
            mode,
            target,
            arguments,
            allowed_returns,
        }
    }

    /// The selected proposal.
    #[must_use]
    pub const fn mode(&self) -> DecoratorMode {
        self.mode
    }

    /// The decorated declaration kind.
    #[must_use]
    pub const fn target(&self) -> DecoratorTarget {
        self.target
    }
}

/// Placement relative to a class's export modifier in stage-3 syntax.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportDecoratorPlacement {
    None,
    Both,
}

/// Checks one decorator expression against its derived invocation contract.
///
/// A decorator may declare fewer parameters than the transform passes, but not
/// more. Every declared parameter must accept the corresponding generated
/// argument. Union expressions are callable only when every constituent is
/// callable under the same contract. `any` and the recovery `Error` type remain
/// non-cascading escape hatches, matching [`TypeTable::assignable`].
#[must_use]
pub fn check_decorator_expression(
    types: &TypeTable,
    decorator_type: TypeId,
    contract: DecoratorContract<'_>,
    site: DecoratorSite,
) -> Vec<Diagnostic> {
    if contract.target().expected_arity(contract.mode()) != Some(contract.arguments.len()) {
        return vec![error(
            DECORATOR_ARGUMENT_MISMATCH,
            site,
            ARGUMENT_MISMATCH_MESSAGE,
        )];
    }

    match check_callable_type(
        types,
        decorator_type,
        contract.arguments,
        contract.allowed_returns,
    ) {
        CallableCheck::Valid => Vec::new(),
        CallableCheck::NotCallable => {
            vec![error(DECORATOR_NOT_CALLABLE, site, NOT_CALLABLE_MESSAGE)]
        }
        CallableCheck::ArgumentMismatch => vec![error(
            DECORATOR_ARGUMENT_MISMATCH,
            site,
            ARGUMENT_MISMATCH_MESSAGE,
        )],
        CallableCheck::ReturnMismatch => vec![error(
            DECORATOR_RETURN_MISMATCH,
            site,
            RETURN_MISMATCH_MESSAGE,
        )],
    }
}

/// Checks target-level restrictions shared by all decorators on a declaration.
#[must_use]
pub fn check_decorator_target(
    mode: DecoratorMode,
    target: DecoratorTarget,
    is_ambient: bool,
    both_accessors_decorated: bool,
    site: DecoratorSite,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if is_ambient {
        diagnostics.push(error(
            DECORATOR_ON_AMBIENT_DECLARATION,
            site,
            AMBIENT_MESSAGE,
        ));
    }
    if target == DecoratorTarget::StaticBlock {
        diagnostics.push(error(DECORATOR_ON_STATIC_BLOCK, site, STATIC_BLOCK_MESSAGE));
    }
    if target == DecoratorTarget::Constructor {
        diagnostics.push(error(
            CONSTRUCTOR_DECORATOR_NOT_SUPPORTED,
            site,
            CONSTRUCTOR_MESSAGE,
        ));
    }
    if mode == DecoratorMode::Stage3 && target == DecoratorTarget::Parameter {
        diagnostics.push(error(
            STAGE3_PARAMETER_DECORATOR,
            site,
            STAGE3_PARAMETER_MESSAGE,
        ));
    }
    if mode == DecoratorMode::Legacy
        && matches!(target, DecoratorTarget::Getter | DecoratorTarget::Setter)
        && both_accessors_decorated
    {
        diagnostics.push(error(
            DECORATOR_ON_BOTH_ACCESSORS,
            site,
            BOTH_ACCESSORS_MESSAGE,
        ));
    }
    diagnostics
}

/// Checks whether decorators are enabled consistently with compiler options.
#[must_use]
pub fn check_decorator_options(
    mode: DecoratorMode,
    experimental_decorators: bool,
    emit_decorator_metadata: bool,
    has_decorators: bool,
    site: DecoratorSite,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    if emit_decorator_metadata && !experimental_decorators {
        diagnostics.push(error(
            METADATA_REQUIRES_EXPERIMENTAL_DECORATORS,
            site,
            METADATA_MESSAGE,
        ));
    }
    if mode == DecoratorMode::Legacy && has_decorators && !experimental_decorators {
        diagnostics.push(error(
            LEGACY_DECORATORS_DISABLED,
            site,
            LEGACY_DISABLED_MESSAGE,
        ));
    }
    diagnostics
}

/// Checks the stage-3 class decorator/export placement grammar.
#[must_use]
pub fn check_export_placement(
    mode: DecoratorMode,
    placement: ExportDecoratorPlacement,
    site: DecoratorSite,
) -> Option<Diagnostic> {
    if mode == DecoratorMode::Stage3 && placement == ExportDecoratorPlacement::Both {
        Some(error(
            DECORATOR_BOTH_SIDES_OF_EXPORT,
            site,
            BOTH_EXPORT_SIDES_MESSAGE,
        ))
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallableCheck {
    Valid,
    NotCallable,
    ArgumentMismatch,
    ReturnMismatch,
}

fn check_callable_type(
    types: &TypeTable,
    decorator_type: TypeId,
    arguments: &[TypeId],
    allowed_returns: &[TypeId],
) -> CallableCheck {
    match types.get(decorator_type) {
        Type::Any | Type::Error => CallableCheck::Valid,
        Type::Function(signature) => {
            if signature.parameters().len() > arguments.len()
                || signature
                    .parameters()
                    .iter()
                    .zip(arguments)
                    .any(|(parameter, argument)| !types.assignable(*argument, parameter.type_id()))
            {
                CallableCheck::ArgumentMismatch
            } else if allowed_returns
                .iter()
                .any(|allowed| types.assignable(signature.return_type(), *allowed))
            {
                CallableCheck::Valid
            } else {
                CallableCheck::ReturnMismatch
            }
        }
        Type::Union(members) => {
            let mut result = CallableCheck::Valid;
            for member in members {
                let member_result = check_callable_type(types, *member, arguments, allowed_returns);
                if member_result != CallableCheck::Valid {
                    result = member_result;
                    break;
                }
            }
            result
        }
        _ => CallableCheck::NotCallable,
    }
}

fn error(code: DiagnosticCode, site: DecoratorSite, message: &'static str) -> Diagnostic {
    Diagnostic::error(code, site.source(), site.range(), message)
}

#[cfg(test)]
mod tests {
    use super::{
        DECORATOR_ARGUMENT_MISMATCH, DECORATOR_BOTH_SIDES_OF_EXPORT, DECORATOR_NOT_CALLABLE,
        DECORATOR_ON_AMBIENT_DECLARATION, DECORATOR_ON_BOTH_ACCESSORS, DECORATOR_ON_STATIC_BLOCK,
        DECORATOR_RETURN_MISMATCH, DecoratorContract, DecoratorMode, DecoratorSite,
        DecoratorTarget, ExportDecoratorPlacement, LEGACY_DECORATORS_DISABLED,
        METADATA_REQUIRES_EXPERIMENTAL_DECORATORS, STAGE3_PARAMETER_DECORATOR,
        check_decorator_expression, check_decorator_options, check_decorator_target,
        check_export_placement,
    };
    use crate::checker::TypeTable;
    use crate::diagnostic::Diagnostic;
    use crate::source::{SourceId, TextRange, Utf16Pos};

    fn site() -> DecoratorSite {
        DecoratorSite::new(
            SourceId::new(0),
            TextRange::new(Utf16Pos::ZERO, Utf16Pos::new(1)).expect("ordered"),
        )
    }

    fn codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect()
    }

    #[test]
    fn legacy_class_decorator_accepts_constructor_and_void_return() {
        let mut types = TypeTable::new();
        let constructor = types.function(Vec::new(), types.object());
        let decorator = types.function(vec![constructor], types.void());
        let arguments = [constructor];
        let returns = [types.void(), constructor];
        let contract = DecoratorContract::new(
            DecoratorMode::Legacy,
            DecoratorTarget::Class,
            &arguments,
            &returns,
        );
        assert!(check_decorator_expression(&types, decorator, contract, site()).is_empty());
    }

    #[test]
    fn legacy_method_decorator_accepts_three_argument_contract() {
        let mut types = TypeTable::new();
        let arguments = [types.object(), types.string(), types.object()];
        let decorator = types.function(arguments.to_vec(), types.void());
        let returns = [types.void(), types.object()];
        let contract = DecoratorContract::new(
            DecoratorMode::Legacy,
            DecoratorTarget::Method,
            &arguments,
            &returns,
        );
        assert!(check_decorator_expression(&types, decorator, contract, site()).is_empty());
    }

    #[test]
    fn legacy_parameter_contract_is_accepted() {
        let mut types = TypeTable::new();
        let arguments = vec![types.object(), types.string(), types.number()];
        let decorator = types.function(arguments.clone(), types.void());
        let returns = [types.void()];
        let contract = DecoratorContract::new(
            DecoratorMode::Legacy,
            DecoratorTarget::Parameter,
            &arguments,
            &returns,
        );
        assert!(check_decorator_expression(&types, decorator, contract, site()).is_empty());
    }

    #[test]
    fn non_callable_decorator_is_rejected() {
        let types = TypeTable::new();
        let arguments = [types.object()];
        let returns = [types.void()];
        let contract = DecoratorContract::new(
            DecoratorMode::Legacy,
            DecoratorTarget::Class,
            &arguments,
            &returns,
        );
        assert_eq!(
            codes(&check_decorator_expression(
                &types,
                types.number(),
                contract,
                site()
            )),
            [DECORATOR_NOT_CALLABLE.as_str()]
        );
    }

    #[test]
    fn decorator_argument_mismatch_is_rejected() {
        let mut types = TypeTable::new();
        let arguments = [types.object()];
        let decorator = types.function(vec![types.number()], types.void());
        let returns = [types.void()];
        let contract = DecoratorContract::new(
            DecoratorMode::Legacy,
            DecoratorTarget::Class,
            &arguments,
            &returns,
        );
        assert_eq!(
            codes(&check_decorator_expression(
                &types,
                decorator,
                contract,
                site()
            )),
            [DECORATOR_ARGUMENT_MISMATCH.as_str()]
        );
    }

    #[test]
    fn decorator_return_mismatch_is_rejected() {
        let mut types = TypeTable::new();
        let arguments = [types.object()];
        let decorator = types.function(vec![types.object()], types.number());
        let returns = [types.void(), types.object()];
        let contract = DecoratorContract::new(
            DecoratorMode::Legacy,
            DecoratorTarget::Class,
            &arguments,
            &returns,
        );
        assert_eq!(
            codes(&check_decorator_expression(
                &types,
                decorator,
                contract,
                site()
            )),
            [DECORATOR_RETURN_MISMATCH.as_str()]
        );
    }

    #[test]
    fn ambient_and_static_block_targets_are_rejected_without_over_rejecting_methods() {
        assert!(
            check_decorator_target(
                DecoratorMode::Stage3,
                DecoratorTarget::Method,
                false,
                false,
                site()
            )
            .is_empty()
        );
        assert_eq!(
            codes(&check_decorator_target(
                DecoratorMode::Stage3,
                DecoratorTarget::Class,
                true,
                false,
                site()
            )),
            [DECORATOR_ON_AMBIENT_DECLARATION.as_str()]
        );
        assert_eq!(
            codes(&check_decorator_target(
                DecoratorMode::Stage3,
                DecoratorTarget::StaticBlock,
                false,
                false,
                site()
            )),
            [DECORATOR_ON_STATIC_BLOCK.as_str()]
        );
    }

    #[test]
    fn constructor_target_is_rejected_without_rejecting_class_target() {
        assert!(
            check_decorator_target(
                DecoratorMode::Legacy,
                DecoratorTarget::Class,
                false,
                false,
                site()
            )
            .is_empty()
        );
        assert_eq!(
            codes(&check_decorator_target(
                DecoratorMode::Legacy,
                DecoratorTarget::Constructor,
                false,
                false,
                site()
            )),
            [crate::checker::CONSTRUCTOR_DECORATOR_NOT_SUPPORTED.as_str()]
        );
    }

    #[test]
    fn legacy_rejects_both_accessors_but_accepts_one() {
        assert!(
            check_decorator_target(
                DecoratorMode::Legacy,
                DecoratorTarget::Getter,
                false,
                false,
                site()
            )
            .is_empty()
        );
        assert_eq!(
            codes(&check_decorator_target(
                DecoratorMode::Legacy,
                DecoratorTarget::Setter,
                false,
                true,
                site()
            )),
            [DECORATOR_ON_BOTH_ACCESSORS.as_str()]
        );
    }

    #[test]
    fn stage3_parameter_is_rejected_but_auto_accessor_is_accepted() {
        assert!(
            check_decorator_target(
                DecoratorMode::Stage3,
                DecoratorTarget::AutoAccessor,
                false,
                false,
                site()
            )
            .is_empty()
        );
        assert_eq!(
            codes(&check_decorator_target(
                DecoratorMode::Stage3,
                DecoratorTarget::Parameter,
                false,
                false,
                site()
            )),
            [STAGE3_PARAMETER_DECORATOR.as_str()]
        );
    }

    #[test]
    fn stage3_method_uses_value_and_context_contract() {
        let mut types = TypeTable::new();
        let method = types.function(Vec::new(), types.void());
        let context = types.object();
        let arguments = [method, context];
        let decorator = types.function(arguments.to_vec(), method);
        let returns = [types.void(), method];
        let contract = DecoratorContract::new(
            DecoratorMode::Stage3,
            DecoratorTarget::Method,
            &arguments,
            &returns,
        );
        assert!(check_decorator_expression(&types, decorator, contract, site()).is_empty());
    }

    #[test]
    fn stage3_decorators_may_be_on_one_export_side_not_both() {
        assert!(
            check_export_placement(
                DecoratorMode::Stage3,
                ExportDecoratorPlacement::None,
                site()
            )
            .is_none()
        );
        let diagnostic = check_export_placement(
            DecoratorMode::Stage3,
            ExportDecoratorPlacement::Both,
            site(),
        )
        .expect("rejected");
        assert_eq!(diagnostic.code(), DECORATOR_BOTH_SIDES_OF_EXPORT);
    }

    #[test]
    fn legacy_export_placement_is_not_subject_to_stage3_rule() {
        assert!(
            check_export_placement(
                DecoratorMode::Legacy,
                ExportDecoratorPlacement::Both,
                site()
            )
            .is_none()
        );
    }

    #[test]
    fn metadata_requires_experimental_decorators() {
        assert!(
            check_decorator_options(DecoratorMode::Legacy, true, true, true, site()).is_empty()
        );
        assert_eq!(
            codes(&check_decorator_options(
                DecoratorMode::Stage3,
                false,
                true,
                false,
                site()
            )),
            [METADATA_REQUIRES_EXPERIMENTAL_DECORATORS.as_str()]
        );
    }

    #[test]
    fn legacy_decorators_require_option_but_stage3_does_not() {
        assert!(
            check_decorator_options(DecoratorMode::Stage3, false, false, true, site()).is_empty()
        );
        assert_eq!(
            codes(&check_decorator_options(
                DecoratorMode::Legacy,
                false,
                false,
                true,
                site()
            )),
            [LEGACY_DECORATORS_DISABLED.as_str()]
        );
    }
}
