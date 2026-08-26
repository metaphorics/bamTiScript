//! Shared JSX-to-JavaScript AST desugaring.
//!
//! This module owns JSX call construction for both JavaScript emission and the
//! native lowerer. Generated spellings are carried out-of-band because syntax
//! tokens refer only to immutable source ranges.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

use crate::public_ast::{NodeRef, visitor, visitor::Visitor};
use crate::source::{JsxEmit, NodeIdSource, SourcePositionError, SourceText, TextRange, Utf16Pos};
use crate::syntax::{
    ArrayElement, ArrayLiteral, BooleanLiteral, CallArgument, CallExpression, Expr, Expression,
    Identifier, JsxAttributeInitializer, JsxAttributeItem, JsxAttributeName, JsxChild, JsxElement,
    JsxElementName, JsxFragment, JsxMemberName, JsxSelfClosingElement, Literal, MemberExpression,
    MemberProperty, Node, NodeId, NullLiteral, NumericLiteral, ObjectLiteral, ObjectMember,
    ObjectProperty, PropertyModifier, PropertyName, SourceFile, SpreadElement, StringLiteral,
    Token, TokenKind, UnaryExpression, UnaryOperator,
};

/// How automatic-runtime bindings are introduced by the module consumer.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum JsxRuntimeImportStyle {
    EsModule,
    CommonJs,
}

/// A closed automatic-runtime named export.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum JsxRuntimeBinding {
    Jsx,
    Jsxs,
    JsxDev,
    Fragment,
}

impl JsxRuntimeBinding {
    #[must_use]
    pub(crate) const fn export_name(self) -> &'static str {
        match self {
            Self::Jsx => "jsx",
            Self::Jsxs => "jsxs",
            Self::JsxDev => "jsxDEV",
            Self::Fragment => "Fragment",
        }
    }

    const fn suggested_local_name(self) -> &'static str {
        match self {
            Self::Jsx => "_jsx",
            Self::Jsxs => "_jsxs",
            Self::JsxDev => "_jsxDEV",
            Self::Fragment => "_Fragment",
        }
    }
}

/// Immutable inputs for one JSX transform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JsxEmitOptions {
    pub(crate) emit: JsxEmit,
    /// Classic factory qualified name, for example `React.createElement`.
    pub(crate) factory: Option<Arc<str>>,
    /// Classic fragment qualified name, for example `React.Fragment`.
    pub(crate) fragment_factory: Option<Arc<str>>,
    /// Base package for the automatic runtime. `None` uses TypeScript's `react` default.
    pub(crate) import_source: Option<Arc<str>>,
    pub(crate) import_style: JsxRuntimeImportStyle,
    /// Source-file name embedded in `jsxDEV` metadata.
    pub(crate) file_name: Option<Arc<str>>,
}

/// Runtime and helper requirements accumulated while building one expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JsxRuntimeDemand {
    /// Complete automatic-runtime module specifier, when automatic bindings are used.
    pub(crate) module_specifier: Option<Arc<str>>,
    pub(crate) import_style: JsxRuntimeImportStyle,
    /// Synthetic identifier node to the named runtime export it denotes.
    pub(crate) bindings: BTreeMap<NodeId, JsxRuntimeBinding>,
    /// Classic spread props require the existing `HelperKind::Assign` helper.
    pub(crate) needs_assign: bool,
}

impl JsxRuntimeDemand {
    /// Merges expression-local requirements into a module-level demand.
    pub(crate) fn merge(&mut self, other: Self) -> Result<(), JsxDemandMergeError> {
        match (&self.module_specifier, &other.module_specifier) {
            (Some(left), Some(right)) if left != right => {
                return Err(JsxDemandMergeError::ConflictingModuleSpecifiers {
                    left: Arc::clone(left),
                    right: Arc::clone(right),
                });
            }
            (None, Some(right)) => self.module_specifier = Some(Arc::clone(right)),
            _ => {}
        }
        if !other.bindings.is_empty() && self.import_style != other.import_style {
            return Err(JsxDemandMergeError::ConflictingImportStyles);
        }
        self.bindings.extend(other.bindings);
        self.needs_assign |= other.needs_assign;
        Ok(())
    }
}

/// Exact text for synthesized identifier, string-literal, numeric-literal,
/// boolean-literal, and null-literal leaf nodes, keyed by that leaf's own ID.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct JsxGeneratedText {
    spellings: BTreeMap<NodeId, Arc<str>>,
}

impl JsxGeneratedText {
    #[must_use]
    pub(crate) fn get(&self, node: NodeId) -> Option<&str> {
        self.spellings.get(&node).map(AsRef::as_ref)
    }

    /// Rebinds one generated leaf spelling after module-level collision analysis.
    ///
    /// Runtime consumers use this with the node IDs in
    /// [`JsxRuntimeDemand::bindings`]; it does not change AST identity.
    pub(crate) fn set(&mut self, node: NodeId, spelling: impl Into<Arc<str>>) {
        self.spellings.insert(node, spelling.into());
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.spellings.extend(other.spellings);
    }
}

/// One ordinary JavaScript expression plus everything its consumers must bind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JsxDesugarResult {
    pub(crate) expression: Expr,
    pub(crate) demand: JsxRuntimeDemand,
    pub(crate) generated_text: JsxGeneratedText,
}

/// Desugared replacements and merged demand for every JSX root in one file.
///
/// The plan is allocated once with the consumer's [`NodeIdSource`], which keeps
/// synthesized IDs stable across program linkage and lowering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JsxSourceDesugarPlan {
    /// Original JSX node identity to its one desugared replacement expression.
    pub(crate) expression_desugars: BTreeMap<NodeId, Expr>,
    pub(crate) demand: JsxRuntimeDemand,
    pub(crate) generated_text: JsxGeneratedText,
}

impl JsxSourceDesugarPlan {
    /// Rebinds every demanded local spelling for one runtime export.
    pub(crate) fn rebind_runtime(
        &mut self,
        binding: JsxRuntimeBinding,
        local_name: impl Into<Arc<str>>,
    ) {
        let local_name = local_name.into();
        for (&node, &required) in &self.demand.bindings {
            if required == binding {
                self.generated_text.set(node, Arc::clone(&local_name));
            }
        }
    }

    /// Rebinds all runtime bindings in deterministic export-kind order.
    pub(crate) fn rebind_runtime_names(&mut self, names: &BTreeMap<JsxRuntimeBinding, Arc<str>>) {
        for (binding, name) in names {
            self.rebind_runtime(*binding, Arc::clone(name));
        }
    }
}

/// A failure while planning one file's JSX replacements.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum JsxDesugarPlanError {
    Desugar(JsxDesugarError),
    Merge(JsxDemandMergeError),
}

impl fmt::Display for JsxDesugarPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Desugar(error) => error.fmt(formatter),
            Self::Merge(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for JsxDesugarPlanError {}

impl From<JsxDesugarError> for JsxDesugarPlanError {
    fn from(error: JsxDesugarError) -> Self {
        Self::Desugar(error)
    }
}

impl From<JsxDemandMergeError> for JsxDesugarPlanError {
    fn from(error: JsxDemandMergeError) -> Self {
        Self::Merge(error)
    }
}

/// Finds and desugars every top-level JSX root in `file`, once per root.
///
/// Nested JSX inside an outer element is desugared by recursion in
/// [`desugar_jsx`], so plan consumers can globally substitute each map entry
/// without allocating new IDs or rerunning demand collection.
pub(crate) fn desugar_source_jsx(
    file: &SourceFile,
    source: &SourceText,
    options: &JsxEmitOptions,
    ids: &mut NodeIdSource,
) -> Result<JsxSourceDesugarPlan, JsxDesugarPlanError> {
    let mut collector = JsxRootCollector {
        expressions: Vec::new(),
    };
    let _ = visitor::visit_source_file(file, &mut collector);

    // Direct JSX children and JSX attribute values are folded into their
    // parent's desugar recursively; only JSX reachable through a `{}`
    // container (or standing alone) needs its own plan entry.
    let mut absorbed = HashSet::new();
    for expression in &collector.expressions {
        absorb_direct_jsx(expression, &mut absorbed);
    }

    let mut expression_desugars = BTreeMap::new();
    let mut demand = JsxRuntimeDemand {
        module_specifier: None,
        import_style: options.import_style,
        bindings: BTreeMap::new(),
        needs_assign: false,
    };
    let mut generated_text = JsxGeneratedText::default();

    for root in &collector.expressions {
        if absorbed.contains(&root.id()) || expression_desugars.contains_key(&root.id()) {
            continue;
        }
        let result = desugar_jsx(root, source, options, ids)?;
        expression_desugars.insert(root.id(), result.expression);
        demand.merge(result.demand)?;
        generated_text.merge(result.generated_text);
    }

    Ok(JsxSourceDesugarPlan {
        expression_desugars,
        demand,
        generated_text,
    })
}

/// Records the node IDs that [`desugar_jsx`] folds into `expression`'s own
/// desugar output, mirroring its recursion over JSX children and attribute
/// values so the source-level planner never desugars the same node twice.
fn absorb_direct_jsx(expression: &Expr, absorbed: &mut HashSet<NodeId>) {
    let absorb_child = |child: &JsxChild, absorbed: &mut HashSet<NodeId>| {
        if let JsxChild::Element(element) = child {
            absorbed.insert(element.id());
            absorb_direct_jsx(element, absorbed);
        }
    };
    // JSX inside an expression container is not folded here: it receives its
    // own source-plan entry and is substituted with the containing expression.
    match expression.data() {
        Expression::JsxElement(element) => {
            for child in &element.children {
                absorb_child(child, absorbed);
            }
        }
        Expression::JsxSelfClosingElement(_) => {}
        Expression::JsxFragment(fragment) => {
            for child in &fragment.children {
                absorb_child(child, absorbed);
            }
        }
        _ => {}
    }
}

/// Collects every JSX expression node. Consumers substitute plan entries from
/// innermost to outermost, so JSX nested inside another expression is
/// desugared exactly once at each level.
struct JsxRootCollector<'ast> {
    expressions: Vec<&'ast Expr>,
}

impl<'ast> Visitor<'ast> for JsxRootCollector<'ast> {
    type Break = ();

    fn visit(&mut self, node: NodeRef<'ast>) -> ControlFlow<Self::Break> {
        if let NodeRef::Expression(expression) = node
            && matches!(
                expression.data(),
                Expression::JsxElement(_)
                    | Expression::JsxSelfClosingElement(_)
                    | Expression::JsxFragment(_)
            )
        {
            self.expressions.push(expression);
        }
        ControlFlow::Continue(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum JsxDemandMergeError {
    ConflictingModuleSpecifiers { left: Arc<str>, right: Arc<str> },
    ConflictingImportStyles,
}

impl fmt::Display for JsxDemandMergeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConflictingModuleSpecifiers { left, right } => {
                write!(
                    formatter,
                    "conflicting JSX runtime modules `{left}` and `{right}`"
                )
            }
            Self::ConflictingImportStyles => formatter.write_str("conflicting JSX import styles"),
        }
    }
}

impl std::error::Error for JsxDemandMergeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum JsxDesugarError {
    NotJsx,
    NonExecutableMode(JsxEmit),
    MissingDevelopmentFileName,
    EmptyAttributeExpression,
    InvalidClassicFactory(Arc<str>),
    SourcePosition(SourcePositionError),
}

impl fmt::Display for JsxDesugarError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJsx => formatter.write_str("expression is not JSX"),
            Self::NonExecutableMode(mode) => {
                write!(
                    formatter,
                    "JSX mode `{mode}` does not produce an executable transform"
                )
            }
            Self::MissingDevelopmentFileName => {
                formatter.write_str("react-jsxdev requires a source file name")
            }
            Self::EmptyAttributeExpression => {
                formatter.write_str("JSX attribute expression has no value")
            }
            Self::InvalidClassicFactory(factory) => {
                write!(formatter, "invalid classic JSX factory `{factory}`")
            }
            Self::SourcePosition(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for JsxDesugarError {}

impl From<SourcePositionError> for JsxDesugarError {
    fn from(error: SourcePositionError) -> Self {
        Self::SourcePosition(error)
    }
}

/// Desugars one JSX root into ordinary expression nodes.
///
/// `Preserve` and `ReactNative` are printer modes, not executable transforms.
pub(crate) fn desugar_jsx(
    expression: &Expr,
    source: &SourceText,
    options: &JsxEmitOptions,
    ids: &mut NodeIdSource,
) -> Result<JsxDesugarResult, JsxDesugarError> {
    if matches!(options.emit, JsxEmit::Preserve | JsxEmit::ReactNative) {
        return Err(JsxDesugarError::NonExecutableMode(options.emit));
    }

    let module_specifier = match options.emit {
        JsxEmit::ReactJsx => Some(runtime_module(options, "jsx-runtime")),
        JsxEmit::ReactJsxDev => Some(runtime_module(options, "jsx-dev-runtime")),
        JsxEmit::React => None,
        JsxEmit::Preserve | JsxEmit::ReactNative => unreachable!(),
    };
    let mut context = DesugarContext {
        source,
        options,
        ids,
        demand: JsxRuntimeDemand {
            module_specifier,
            import_style: options.import_style,
            bindings: BTreeMap::new(),
            needs_assign: false,
        },
        generated_text: JsxGeneratedText::default(),
    };
    let expression = context.jsx_expression(expression)?;
    Ok(JsxDesugarResult {
        expression,
        demand: context.demand,
        generated_text: context.generated_text,
    })
}

fn runtime_module(options: &JsxEmitOptions, suffix: &str) -> Arc<str> {
    let base = options.import_source.as_deref().unwrap_or("react");
    Arc::from(format!("{}/{suffix}", base.trim_end_matches('/')))
}

struct DesugarContext<'a> {
    source: &'a SourceText,
    options: &'a JsxEmitOptions,
    ids: &'a mut NodeIdSource,
    demand: JsxRuntimeDemand,
    generated_text: JsxGeneratedText,
}

impl DesugarContext<'_> {
    fn jsx_expression(&mut self, expression: &Expr) -> Result<Expr, JsxDesugarError> {
        match expression.data() {
            Expression::JsxElement(element) => self.element(expression.range(), element),
            Expression::JsxSelfClosingElement(element) => {
                self.self_closing(expression.range(), element)
            }
            Expression::JsxFragment(fragment) => self.fragment(expression.range(), fragment),
            _ => Err(JsxDesugarError::NotJsx),
        }
    }

    fn element(&mut self, range: TextRange, element: &JsxElement) -> Result<Expr, JsxDesugarError> {
        let opening = element.opening.data();
        self.with_tag_attributes_children(
            range,
            &opening.name,
            &opening.attributes,
            element.opening.range(),
            &element.children,
        )
    }

    fn self_closing(
        &mut self,
        range: TextRange,
        element: &JsxSelfClosingElement,
    ) -> Result<Expr, JsxDesugarError> {
        self.with_tag_attributes_children(range, &element.name, &element.attributes, range, &[])
    }

    fn fragment(
        &mut self,
        range: TextRange,
        fragment: &JsxFragment,
    ) -> Result<Expr, JsxDesugarError> {
        let children = self.children(&fragment.children)?;
        match self.options.emit {
            JsxEmit::React => {
                let tag = self.classic_factory(range, true)?;
                let callee = self.classic_factory(range, false)?;
                self.classic_call(range, callee, tag, None, children)
            }
            JsxEmit::ReactJsx | JsxEmit::ReactJsxDev => {
                let tag = self.runtime_identifier(range, JsxRuntimeBinding::Fragment);
                self.automatic_call(range, tag, Vec::new(), None, children)
            }
            JsxEmit::Preserve | JsxEmit::ReactNative => unreachable!(),
        }
    }

    fn with_tag_attributes_children(
        &mut self,
        range: TextRange,
        tag_name: &JsxElementName,
        attributes: &[JsxAttributeItem],
        attributes_range: TextRange,
        children: &[JsxChild],
    ) -> Result<Expr, JsxDesugarError> {
        let tag = self.tag(tag_name)?;
        let children = self.children(children)?;
        match self.options.emit {
            JsxEmit::React => {
                let callee = self.classic_factory(range, false)?;
                let props = self.classic_props(attributes, attributes_range)?;
                self.classic_call(range, callee, tag, props, children)
            }
            JsxEmit::ReactJsx | JsxEmit::ReactJsxDev => {
                let (members, key) = self.automatic_props(attributes)?;
                self.automatic_call(range, tag, members, key, children)
            }
            JsxEmit::Preserve | JsxEmit::ReactNative => unreachable!(),
        }
    }

    fn tag(&mut self, tag_name: &JsxElementName) -> Result<Expr, JsxDesugarError> {
        match tag_name {
            JsxElementName::Identifier(identifier) => {
                let spelling = self.token_text(identifier.data().token())?.to_owned();
                if is_intrinsic_tag(&spelling) {
                    Ok(self.string_expr(identifier.range(), spelling))
                } else {
                    Ok(self.node_expr(
                        identifier.range(),
                        Expression::Identifier(identifier.clone()),
                    ))
                }
            }
            JsxElementName::Member(member) => self.member_tag(member),
            JsxElementName::Namespace(name) => {
                let namespace = self.token_text(name.namespace.data().token())?.to_owned();
                let local = self.token_text(name.name.data().token())?.to_owned();
                let range =
                    TextRange::new(name.namespace.range().start(), name.name.range().end())?;
                Ok(self.string_expr(range, format!("{namespace}:{local}")))
            }
        }
    }

    fn member_tag(&mut self, member: &JsxMemberName) -> Result<Expr, JsxDesugarError> {
        let object = self.element_name_expression(&member.object)?;
        let range = TextRange::new(object.range().start(), member.property.range().end())?;
        Ok(self.node_expr(
            range,
            Expression::Member(MemberExpression {
                object: Box::new(object),
                property: MemberProperty::Named(member.property.clone()),
                optional: false,
            }),
        ))
    }

    fn element_name_expression(&mut self, name: &JsxElementName) -> Result<Expr, JsxDesugarError> {
        match name {
            JsxElementName::Identifier(identifier) => Ok(self.node_expr(
                identifier.range(),
                Expression::Identifier(identifier.clone()),
            )),
            JsxElementName::Member(member) => self.member_tag(member),
            JsxElementName::Namespace(name) => {
                let namespace = self.token_text(name.namespace.data().token())?.to_owned();
                let local = self.token_text(name.name.data().token())?.to_owned();
                let range =
                    TextRange::new(name.namespace.range().start(), name.name.range().end())?;
                Ok(self.string_expr(range, format!("{namespace}:{local}")))
            }
        }
    }

    fn classic_factory(
        &mut self,
        range: TextRange,
        fragment: bool,
    ) -> Result<Expr, JsxDesugarError> {
        // Unset classic factories use the TypeScript defaults.
        let factory = if fragment {
            self.options
                .fragment_factory
                .as_deref()
                .unwrap_or("React.Fragment")
        } else {
            self.options
                .factory
                .as_deref()
                .unwrap_or("React.createElement")
        };

        let mut parts = factory.split('.');
        let Some(first) = parts.next().filter(|part| is_identifier_name(part)) else {
            return Err(JsxDesugarError::InvalidClassicFactory(factory.into()));
        };
        let mut expression = self.generated_identifier_expr(range, first);
        for part in parts {
            if !is_identifier_name(part) {
                return Err(JsxDesugarError::InvalidClassicFactory(factory.into()));
            }
            let token = self.generated_token(TokenKind::Identifier, range.start());
            let property = self.generated_leaf(range, Identifier::new(token), part);
            expression = self.node_expr(
                range,
                Expression::Member(MemberExpression {
                    object: Box::new(expression),
                    property: MemberProperty::Named(property),
                    optional: false,
                }),
            );
        }
        Ok(expression)
    }

    fn classic_props(
        &mut self,
        attributes: &[JsxAttributeItem],
        range: TextRange,
    ) -> Result<Option<Expr>, JsxDesugarError> {
        if attributes.is_empty() {
            return Ok(None);
        }
        let has_spread = attributes
            .iter()
            .any(|entry| matches!(entry, JsxAttributeItem::Spread(_)));
        if !has_spread {
            let mut members = Vec::with_capacity(attributes.len());
            for entry in attributes {
                let JsxAttributeItem::Attribute(attribute) = entry else {
                    unreachable!();
                };
                members.push(self.attribute_member(attribute)?);
            }
            return Ok(Some(self.object_expr(range, members)));
        }

        self.demand.needs_assign = true;
        let empty = self.object_expr(range, Vec::new());
        let mut arguments = vec![self.call_argument(empty)];
        let mut bucket = Vec::new();
        for entry in attributes {
            match entry {
                JsxAttributeItem::Attribute(attribute) => {
                    bucket.push(self.attribute_member(attribute)?);
                }
                JsxAttributeItem::Spread(spread) => {
                    if !bucket.is_empty() {
                        let members = std::mem::take(&mut bucket);
                        let object = self.object_expr(range, members);
                        arguments.push(self.call_argument(object));
                    }
                    arguments.push(self.call_argument((*spread.data().expression).clone()));
                }
            }
        }
        if !bucket.is_empty() {
            let object = self.object_expr(range, bucket);
            arguments.push(self.call_argument(object));
        }
        let callee = self.generated_identifier_expr(range, "__assign");
        Ok(Some(self.call_expr(range, callee, arguments)))
    }

    fn automatic_props(
        &mut self,
        attributes: &[JsxAttributeItem],
    ) -> Result<(Vec<ObjectMember>, Option<Expr>), JsxDesugarError> {
        let extractable_key = attributes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, entry)| match entry {
                JsxAttributeItem::Attribute(attribute) if self.is_key(attribute) => Some(index),
                _ => None,
            })
            .filter(|key_index| {
                !attributes[*key_index + 1..]
                    .iter()
                    .any(|entry| matches!(entry, JsxAttributeItem::Spread(_)))
            });

        let mut key = None;
        let mut members = Vec::with_capacity(attributes.len());
        for (index, entry) in attributes.iter().enumerate() {
            match entry {
                JsxAttributeItem::Attribute(attribute) if Some(index) == extractable_key => {
                    key = Some(self.attribute_value(attribute)?);
                }
                JsxAttributeItem::Attribute(attribute) => {
                    members.push(self.attribute_member(attribute)?.into_data());
                }
                JsxAttributeItem::Spread(spread) => {
                    members.push(ObjectMember::Spread(SpreadElement {
                        argument: Box::new((*spread.data().expression).clone()),
                    }))
                }
            }
        }
        Ok((members, key))
    }

    fn is_key(&self, attribute: &Node<crate::syntax::JsxAttribute>) -> bool {
        let JsxAttributeName::Identifier(identifier) = &attribute.data().name else {
            return false;
        };
        self.token_text(identifier.data().token()) == Ok("key")
    }

    fn attribute_member(
        &mut self,
        attribute: &Node<crate::syntax::JsxAttribute>,
    ) -> Result<Node<ObjectMember>, JsxDesugarError> {
        let name = self.attribute_name(&attribute.data().name)?;
        let value = self.attribute_value(attribute)?;
        Ok(self.node(
            attribute.range(),
            ObjectMember::Property(ObjectProperty {
                name,
                value: Box::new(value),
                modifier: PropertyModifier::None,
                shorthand: false,
            }),
        ))
    }

    fn attribute_name(&mut self, name: &JsxAttributeName) -> Result<PropertyName, JsxDesugarError> {
        let (range, spelling) = match name {
            JsxAttributeName::Identifier(identifier) => (
                identifier.range(),
                self.token_text(identifier.data().token())?.to_owned(),
            ),
            JsxAttributeName::Namespace(name) => {
                let namespace = self.token_text(name.namespace.data().token())?;
                let local = self.token_text(name.name.data().token())?;
                let range =
                    TextRange::new(name.namespace.range().start(), name.name.range().end())?;
                (range, format!("{namespace}:{local}"))
            }
        };
        Ok(PropertyName::String(self.string_leaf(range, spelling)))
    }

    fn attribute_value(
        &mut self,
        attribute: &Node<crate::syntax::JsxAttribute>,
    ) -> Result<Expr, JsxDesugarError> {
        let Some(value) = &attribute.data().initializer else {
            return Ok(self.boolean_expr(attribute.range(), true));
        };
        match value {
            JsxAttributeInitializer::String(string) => {
                let raw = self.token_text(string.data().token())?;
                let unquoted = match raw.as_bytes() {
                    [quote @ (b'\'' | b'"'), middle @ .., end] if quote == end => {
                        std::str::from_utf8(middle).expect("subslice of valid UTF-8")
                    }
                    _ => raw,
                };
                Ok(self.string_expr(string.range(), decode_entities(unquoted)))
            }
            JsxAttributeInitializer::Expression(expression) => expression
                .data()
                .expression
                .as_deref()
                .cloned()
                .ok_or(JsxDesugarError::EmptyAttributeExpression),
        }
    }

    fn children(&mut self, children: &[JsxChild]) -> Result<Vec<Expr>, JsxDesugarError> {
        let mut lowered = Vec::new();
        for child in children {
            match child {
                JsxChild::Text(text) => {
                    let raw = self.token_text(text.data().token())?;
                    let cooked = cook_jsx_text(raw);
                    if !cooked.is_empty() {
                        lowered.push(self.string_expr(text.range(), cooked));
                    }
                }
                JsxChild::ExpressionContainer(expression) => {
                    if let Some(value) = expression.data().expression.as_deref() {
                        lowered.push(value.clone());
                    }
                }
                JsxChild::Spread(spread) => {
                    lowered.push((*spread.data().expression).clone());
                }
                JsxChild::Element(expression) => lowered.push(self.jsx_expression(expression)?),
            }
        }
        Ok(lowered)
    }

    fn classic_call(
        &mut self,
        range: TextRange,
        callee: Expr,
        tag: Expr,
        props: Option<Expr>,
        children: Vec<Expr>,
    ) -> Result<Expr, JsxDesugarError> {
        let mut arguments = Vec::with_capacity(children.len() + 2);
        arguments.push(self.call_argument(tag));
        let props = props.unwrap_or_else(|| self.null_expr(range));
        arguments.push(self.call_argument(props));
        arguments.extend(children.into_iter().map(|child| self.call_argument(child)));
        Ok(self.call_expr(range, callee, arguments))
    }

    fn automatic_call(
        &mut self,
        range: TextRange,
        tag: Expr,
        mut members: Vec<ObjectMember>,
        key: Option<Expr>,
        children: Vec<Expr>,
    ) -> Result<Expr, JsxDesugarError> {
        let static_children = children.len() > 1;
        if children.len() == 1 {
            let child = children.into_iter().next().expect("one child");
            members.push(self.property(range, "children", child).into_data());
        } else if !children.is_empty() {
            let elements = children
                .into_iter()
                .map(|child| ArrayElement::Expression(Box::new(child)))
                .collect();
            let array = self.node_expr(range, Expression::Array(ArrayLiteral { elements }));
            members.push(self.property(range, "children", array).into_data());
        }
        let mut property_nodes = Vec::with_capacity(members.len());
        for member in members {
            property_nodes.push(self.node(range, member));
        }
        let props = self.object_expr(range, property_nodes);

        if self.options.emit == JsxEmit::ReactJsxDev {
            let callee = self.runtime_identifier(range, JsxRuntimeBinding::JsxDev);
            let key = key.unwrap_or_else(|| self.undefined_expr(range));
            let metadata = self.development_metadata(range)?;
            let static_children = self.boolean_expr(range, static_children);
            let this = self.node_expr(range, Expression::This);
            let arguments = vec![
                self.call_argument(tag),
                self.call_argument(props),
                self.call_argument(key),
                self.call_argument(static_children),
                self.call_argument(metadata),
                self.call_argument(this),
            ];
            return Ok(self.call_expr(range, callee, arguments));
        }

        let binding = if static_children {
            JsxRuntimeBinding::Jsxs
        } else {
            JsxRuntimeBinding::Jsx
        };
        let callee = self.runtime_identifier(range, binding);
        let mut arguments = vec![self.call_argument(tag), self.call_argument(props)];
        if let Some(key) = key {
            arguments.push(self.call_argument(key));
        }
        Ok(self.call_expr(range, callee, arguments))
    }

    fn development_metadata(&mut self, range: TextRange) -> Result<Expr, JsxDesugarError> {
        let file_name = Arc::clone(
            self.options
                .file_name
                .as_ref()
                .ok_or(JsxDesugarError::MissingDevelopmentFileName)?,
        );
        let (line, column) = self.source.line_column(range.start())?;
        let file_name = self.string_expr(range, file_name);
        let line = self.number_expr(range, (line + 1).to_string());
        let column = self.number_expr(range, (column + 1).to_string());
        let members = vec![
            self.property(range, "fileName", file_name),
            self.property(range, "lineNumber", line),
            self.property(range, "columnNumber", column),
        ];
        Ok(self.object_expr(range, members))
    }

    fn property(&mut self, range: TextRange, name: &str, value: Expr) -> Node<ObjectMember> {
        let token = self.generated_token(TokenKind::Identifier, range.start());
        let name =
            PropertyName::Identifier(self.generated_leaf(range, Identifier::new(token), name));
        self.node(
            range,
            ObjectMember::Property(ObjectProperty {
                name,
                value: Box::new(value),
                modifier: PropertyModifier::None,
                shorthand: false,
            }),
        )
    }

    fn runtime_identifier(&mut self, range: TextRange, binding: JsxRuntimeBinding) -> Expr {
        let expression = self.generated_identifier_expr(range, binding.suggested_local_name());
        let Expression::Identifier(identifier) = expression.data() else {
            unreachable!();
        };
        self.demand.bindings.insert(identifier.id(), binding);
        expression
    }

    fn generated_identifier_expr(&mut self, range: TextRange, spelling: &str) -> Expr {
        let token = self.generated_token(TokenKind::Identifier, range.start());
        let identifier = self.generated_leaf(range, Identifier::new(token), spelling);
        self.node_expr(range, Expression::Identifier(identifier))
    }

    fn string_expr(&mut self, range: TextRange, value: impl Into<Arc<str>>) -> Expr {
        let literal = self.string_leaf(range, value);
        self.node_expr(range, Expression::Literal(Literal::String(literal)))
    }

    fn string_leaf(&mut self, range: TextRange, value: impl Into<Arc<str>>) -> Node<StringLiteral> {
        let token = self.generated_token(TokenKind::StringLiteral, range.start());
        self.generated_leaf(range, StringLiteral::new(token), value)
    }

    fn boolean_expr(&mut self, range: TextRange, value: bool) -> Expr {
        let spelling = if value { "true" } else { "false" };
        let token = self.generated_token(
            if value {
                TokenKind::KwTrue
            } else {
                TokenKind::KwFalse
            },
            range.start(),
        );
        let literal = self.generated_leaf(range, BooleanLiteral::new(token), spelling);
        self.node_expr(range, Expression::Literal(Literal::Boolean(literal)))
    }

    fn number_expr(&mut self, range: TextRange, value: impl Into<Arc<str>>) -> Expr {
        let token = self.generated_token(TokenKind::NumericLiteral, range.start());
        let literal = self.generated_leaf(range, NumericLiteral::new(token), value);
        self.node_expr(range, Expression::Literal(Literal::Number(literal)))
    }

    fn null_expr(&mut self, range: TextRange) -> Expr {
        let token = self.generated_token(TokenKind::KwNull, range.start());
        let literal = self.generated_leaf(range, NullLiteral::new(token), "null");
        self.node_expr(range, Expression::Literal(Literal::Null(literal)))
    }

    fn undefined_expr(&mut self, range: TextRange) -> Expr {
        let zero = self.number_expr(range, "0");
        self.node_expr(
            range,
            Expression::Unary(UnaryExpression {
                operator: UnaryOperator::Void,
                argument: Box::new(zero),
            }),
        )
    }

    fn object_expr(&mut self, range: TextRange, members: Vec<Node<ObjectMember>>) -> Expr {
        self.node_expr(range, Expression::Object(ObjectLiteral { members }))
    }

    fn call_expr(&mut self, range: TextRange, callee: Expr, arguments: Vec<CallArgument>) -> Expr {
        self.node_expr(
            range,
            Expression::Call(CallExpression {
                callee: Box::new(callee),
                optional: false,
                type_arguments: None,
                arguments,
            }),
        )
    }

    fn call_argument(&self, expression: Expr) -> CallArgument {
        CallArgument::Expression(Box::new(expression))
    }

    fn node_expr(&mut self, range: TextRange, data: Expression) -> Expr {
        self.node(range, data)
    }

    fn node<T>(&mut self, range: TextRange, data: T) -> Node<T> {
        Node::new(self.ids.fresh(), range, data)
    }

    fn generated_leaf<T>(
        &mut self,
        range: TextRange,
        data: T,
        spelling: impl Into<Arc<str>>,
    ) -> Node<T> {
        let node = self.node(range, data);
        self.generated_text
            .spellings
            .insert(node.id(), spelling.into());
        node
    }

    fn generated_token(&self, kind: TokenKind, anchor: Utf16Pos) -> Token {
        let range = TextRange::new(anchor, anchor).expect("equal JSX token anchors are ordered");
        Token::new(kind, range)
    }

    fn token_text(&self, token: &Token) -> Result<&str, JsxDesugarError> {
        let start = self.source.utf16_to_byte(token.range().start())?;
        let end = self.source.utf16_to_byte(token.range().end())?;
        Ok(&self.source.as_str()[start..end])
    }
}

fn is_intrinsic_tag(name: &str) -> bool {
    name.starts_with(|character: char| character.is_ascii_lowercase()) || name.contains('-')
}

fn is_identifier_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(first) if first == '_' || first == '$' || unicode_id_start::is_id_start(first))
        && characters.all(|character| {
            character == '_'
                || character == '$'
                || unicode_id_start::is_id_continue(character)
                || character == '\u{200c}'
                || character == '\u{200d}'
        })
}

fn cook_jsx_text(raw: &str) -> String {
    let decoded = decode_entities(raw);
    let lines: Vec<&str> = decoded.split(['\r', '\n']).collect();
    let last_non_empty = lines.iter().rposition(|line| !line.trim().is_empty());
    let Some(last_non_empty) = last_non_empty else {
        return String::new();
    };

    let mut cooked = String::new();
    for (index, line) in lines.iter().enumerate() {
        let line = line.replace('\t', " ");
        let line = if index == 0 {
            line.as_str()
        } else {
            line.trim_start_matches(' ')
        };
        let line = if index == lines.len() - 1 {
            line
        } else {
            line.trim_end_matches(' ')
        };
        if line.is_empty() {
            continue;
        }
        cooked.push_str(line);
        if index != last_non_empty {
            cooked.push(' ');
        }
    }
    cooked
}

fn decode_entities(raw: &str) -> String {
    let mut decoded = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(ampersand) = rest.find('&') {
        decoded.push_str(&rest[..ampersand]);
        rest = &rest[ampersand..];
        let Some(semicolon) = rest.find(';') else {
            decoded.push_str(rest);
            return decoded;
        };
        let entity = &rest[1..semicolon];
        if let Some(character) = decode_entity(entity) {
            decoded.push(character);
            rest = &rest[semicolon + 1..];
        } else {
            decoded.push('&');
            rest = &rest[1..];
        }
    }
    decoded.push_str(rest);
    decoded
}

fn decode_entity(entity: &str) -> Option<char> {
    if let Some(decimal) = entity.strip_prefix('#') {
        let value = if let Some(hex) = decimal.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            decimal.parse().ok()?
        };
        return char::from_u32(value);
    }
    Some(match entity {
        "amp" => '&',
        "apos" => '\'',
        "gt" => '>',
        "lt" => '<',
        "nbsp" => '\u{00a0}',
        "quot" => '"',
        "copy" => '\u{00a9}',
        "reg" => '\u{00ae}',
        "trade" => '\u{2122}',
        "cent" => '\u{00a2}',
        "pound" => '\u{00a3}',
        "yen" => '\u{00a5}',
        "euro" => '\u{20ac}',
        "sect" => '\u{00a7}',
        "para" => '\u{00b6}',
        "middot" => '\u{00b7}',
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "hellip" => '\u{2026}',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{ScriptKind, SourceId};
    use crate::syntax::{
        ExpressionStatement, IdentifierNode, JsxAttribute, JsxClosingElement,
        JsxExpressionContainer, JsxNamespacedName, JsxOpeningElement, JsxSpreadAttribute, JsxText,
        SequenceExpression, Statement,
    };

    fn pos(offset: usize) -> Utf16Pos {
        Utf16Pos::new(offset)
    }

    fn span(start: usize, end: usize) -> TextRange {
        TextRange::new(pos(start), pos(end)).expect("test range")
    }

    fn ident(id: u32, start: usize, end: usize) -> IdentifierNode {
        Node::new(
            NodeId::new(id),
            span(start, end),
            Identifier::new(Token::new(TokenKind::Identifier, span(start, end))),
        )
    }

    fn ident_expr(id: u32, start: usize, end: usize) -> Expr {
        let identifier = ident(id, start, end);
        Node::new(
            identifier.id(),
            identifier.range(),
            Expression::Identifier(identifier),
        )
    }

    fn string_value(start: usize, end: usize) -> JsxAttributeInitializer {
        JsxAttributeInitializer::String(Node::new(
            NodeId::new(9000),
            span(start, end),
            StringLiteral::new(Token::new(TokenKind::StringLiteral, span(start, end))),
        ))
    }

    fn named_attribute(
        id: u32,
        name_span: TextRange,
        initializer: Option<JsxAttributeInitializer>,
    ) -> JsxAttributeItem {
        JsxAttributeItem::Attribute(Node::new(
            NodeId::new(id),
            name_span,
            JsxAttribute {
                name: JsxAttributeName::Identifier(ident(
                    id + 200,
                    name_span.start().get(),
                    name_span.end().get(),
                )),
                initializer,
            },
        ))
    }

    fn spread_attribute(id: u32, range: TextRange, expression_id: u32) -> JsxAttributeItem {
        JsxAttributeItem::Spread(Node::new(
            NodeId::new(id),
            range,
            JsxSpreadAttribute {
                expression: Box::new(ident_expr(
                    expression_id,
                    range.start().get(),
                    range.start().get() + 5,
                )),
            },
        ))
    }

    fn self_closing(
        id: u32,
        range: TextRange,
        name: JsxElementName,
        attributes: Vec<JsxAttributeItem>,
    ) -> Expr {
        Node::new(
            NodeId::new(id),
            range,
            Expression::JsxSelfClosingElement(JsxSelfClosingElement { name, attributes }),
        )
    }

    fn classic_options() -> JsxEmitOptions {
        JsxEmitOptions {
            emit: JsxEmit::React,
            factory: Some(Arc::from("React.createElement")),
            fragment_factory: Some(Arc::from("React.Fragment")),
            import_source: None,
            import_style: JsxRuntimeImportStyle::EsModule,
            file_name: Some(Arc::from("app.tsx")),
        }
    }

    fn automatic_options(emit: JsxEmit) -> JsxEmitOptions {
        JsxEmitOptions {
            emit,
            factory: None,
            fragment_factory: None,
            import_source: None,
            import_style: JsxRuntimeImportStyle::EsModule,
            file_name: Some(Arc::from("app.tsx")),
        }
    }

    fn fresh_ids() -> NodeIdSource {
        NodeIdSource::after(NodeId::new(5000))
    }

    #[test]
    fn cook_jsx_text_trims_interior_lines_and_keeps_outer_whitespace() {
        assert_eq!(cook_jsx_text("  A  \n  B  "), "  A B  ");
        assert_eq!(cook_jsx_text("A\r\nB"), "A B");
        assert_eq!(cook_jsx_text("   "), "");
        assert_eq!(cook_jsx_text("a\tb"), "a b");
    }

    #[test]
    fn decode_entities_handles_named_and_numeric_forms() {
        assert_eq!(decode_entities("&lt;&amp;&gt;"), "<&>");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("&unknowable;"), "&unknowable;");
        assert_eq!(decode_entities("no entities"), "no entities");
    }

    #[test]
    fn classic_intrinsic_tag_becomes_string_first_argument() {
        let text =
            SourceText::new("<div id=\"a\" />").expect("test source fits the per-file budget");
        let attrs = vec![named_attribute(3, span(6, 8), Some(string_value(9, 12)))];
        let element = self_closing(
            4,
            span(0, 14),
            JsxElementName::Identifier(ident(1, 1, 4)),
            attrs,
        );
        let result =
            desugar_jsx(&element, &text, &classic_options(), &mut fresh_ids()).expect("desugars");

        let Expression::Call(call) = result.expression.data() else {
            panic!("classic element must lower to a call");
        };
        assert_eq!(call.arguments.len(), 2);
        assert!(!result.demand.needs_assign);
        assert!(result.demand.module_specifier.is_none());

        let CallArgument::Expression(first) = &call.arguments[0] else {
            panic!("tag argument");
        };
        let Expression::Literal(Literal::String(tag_name)) = first.data() else {
            panic!("intrinsic tag must become a string literal");
        };
        assert_eq!(result.generated_text.get(tag_name.id()), Some("div"));
    }

    #[test]
    fn classic_spread_switches_props_to_assign_call() {
        let text = SourceText::new("<div {...props} id=\"a\" />")
            .expect("test source fits the per-file budget");
        let attrs = vec![
            spread_attribute(7, span(6, 14), 8),
            named_attribute(3, span(15, 17), Some(string_value(18, 21))),
        ];
        let element = self_closing(
            4,
            span(0, 25),
            JsxElementName::Identifier(ident(1, 1, 4)),
            attrs,
        );
        let result =
            desugar_jsx(&element, &text, &classic_options(), &mut fresh_ids()).expect("desugars");

        assert!(result.demand.needs_assign);
        let Expression::Call(call) = result.expression.data() else {
            panic!("classic element must lower to a call");
        };
        let CallArgument::Expression(second) = &call.arguments[1] else {
            panic!("props argument");
        };
        assert!(
            matches!(second.data(), Expression::Call(_)),
            "spread props must go through __assign"
        );
    }

    #[test]
    fn automatic_key_extraction_respects_spread_order() {
        let text = SourceText::new("<Foo key=\"k\" {...rest} />")
            .expect("test source fits the per-file budget");
        let attrs = vec![
            named_attribute(3, span(6, 9), Some(string_value(10, 13))),
            spread_attribute(7, span(14, 22), 8),
        ];
        let element = self_closing(
            4,
            span(0, 25),
            JsxElementName::Identifier(ident(1, 1, 4)),
            attrs,
        );
        let result = desugar_jsx(
            &element,
            &text,
            &automatic_options(JsxEmit::ReactJsx),
            &mut fresh_ids(),
        )
        .expect("desugars");

        let Expression::Call(call) = result.expression.data() else {
            panic!("automatic element must lower to a call");
        };
        assert_eq!(
            call.arguments.len(),
            2,
            "key behind a spread is not extracted"
        );
        assert!(matches!(
            result.demand.bindings.values().next(),
            Some(JsxRuntimeBinding::Jsx)
        ));
        assert_eq!(
            result.demand.module_specifier.as_deref(),
            Some("react/jsx-runtime")
        );

        let text = SourceText::new("<Foo {...rest} key=\"k\" />")
            .expect("test source fits the per-file budget");
        let attrs = vec![
            spread_attribute(17, span(6, 14), 18),
            named_attribute(13, span(15, 18), Some(string_value(19, 22))),
        ];
        let element = self_closing(
            14,
            span(0, 25),
            JsxElementName::Identifier(ident(11, 1, 4)),
            attrs,
        );
        let extracted = desugar_jsx(
            &element,
            &text,
            &automatic_options(JsxEmit::ReactJsx),
            &mut fresh_ids(),
        )
        .expect("desugars key after spread");
        let Expression::Call(call) = extracted.expression.data() else {
            panic!("automatic element must lower to a call");
        };
        assert_eq!(
            call.arguments.len(),
            3,
            "key after the final spread is extracted"
        );
    }

    #[test]
    fn automatic_two_static_children_use_jsxs_and_extract_key() {
        let text = SourceText::new("<Foo key=\"k\">a{b}</Foo>")
            .expect("test source fits the per-file budget");
        let attrs = vec![named_attribute(3, span(5, 8), Some(string_value(10, 13)))];
        let opening = Node::new(
            NodeId::new(20),
            span(0, 14),
            JsxOpeningElement {
                name: JsxElementName::Identifier(ident(1, 1, 4)),
                attributes: attrs,
            },
        );
        let closing = Node::new(
            NodeId::new(21),
            span(22, 28),
            JsxClosingElement {
                name: JsxElementName::Identifier(ident(22, 24, 27)),
            },
        );
        let text_child = Node::new(
            NodeId::new(30),
            span(15, 16),
            JsxText::new(Token::new(TokenKind::StringLiteral, span(15, 16))),
        );
        let container = Node::new(
            NodeId::new(31),
            span(16, 19),
            JsxExpressionContainer {
                expression: Some(Box::new(ident_expr(32, 17, 18))),
            },
        );
        let element = Node::new(
            NodeId::new(4),
            span(0, 28),
            Expression::JsxElement(JsxElement {
                opening,
                children: vec![
                    JsxChild::Text(text_child),
                    JsxChild::ExpressionContainer(container),
                ],
                closing,
            }),
        );
        let result = desugar_jsx(
            &element,
            &text,
            &automatic_options(JsxEmit::ReactJsx),
            &mut fresh_ids(),
        )
        .expect("desugars");

        let Expression::Call(call) = result.expression.data() else {
            panic!("automatic element must lower to a call");
        };
        assert_eq!(
            call.arguments.len(),
            3,
            "static key is extracted as argument three"
        );
        assert!(matches!(
            result.demand.bindings.values().next(),
            Some(JsxRuntimeBinding::Jsxs)
        ));
    }

    #[test]
    fn dev_mode_reports_one_based_utf16_line_and_column() {
        let text = SourceText::new("const A = \u{2028}\u{e9}<X />")
            .expect("test source fits the per-file budget");
        let element = self_closing(
            4,
            span(12, 17),
            JsxElementName::Identifier(ident(1, 13, 14)),
            Vec::new(),
        );
        let result = desugar_jsx(
            &element,
            &text,
            &automatic_options(JsxEmit::ReactJsxDev),
            &mut fresh_ids(),
        )
        .expect("desugars");

        let Expression::Call(call) = result.expression.data() else {
            panic!("dev element must lower to a call");
        };
        assert_eq!(call.arguments.len(), 6);
        assert!(matches!(
            result.demand.bindings.values().next(),
            Some(JsxRuntimeBinding::JsxDev)
        ));
        assert_eq!(
            result.demand.module_specifier.as_deref(),
            Some("react/jsx-dev-runtime")
        );
        let CallArgument::Expression(metadata) = &call.arguments[4] else {
            panic!("jsxDEV metadata argument");
        };
        let Expression::Object(metadata) = metadata.data() else {
            panic!("jsxDEV metadata must be an object");
        };
        assert_eq!(metadata.members.len(), 3);
        for (index, expected) in [(0, "app.tsx"), (1, "2"), (2, "2")] {
            let ObjectMember::Property(property) = metadata.members[index].data() else {
                panic!("metadata member must be a property");
            };
            let Expression::Literal(literal) = property.value.data() else {
                panic!("metadata value must be a literal");
            };
            let node_id = match literal {
                Literal::String(value) => value.id(),
                Literal::Number(value) => value.id(),
                _ => panic!("unexpected metadata literal"),
            };
            assert_eq!(result.generated_text.get(node_id), Some(expected));
        }
        let (line, column) = text.line_column(element.range().start()).expect("position");
        assert_eq!(
            (line, column),
            (1, 1),
            "\u{2028} advances a line; \u{e9} keeps one UTF-16 unit of width"
        );
    }

    #[test]
    fn preserve_and_react_native_are_rejected() {
        let text = SourceText::new("<X />").expect("test source fits the per-file budget");
        let element = self_closing(
            4,
            span(0, 5),
            JsxElementName::Identifier(ident(1, 1, 2)),
            Vec::new(),
        );
        for emit in [JsxEmit::Preserve, JsxEmit::ReactNative] {
            let mut options = automatic_options(emit);
            options.emit = emit;
            assert!(matches!(
                desugar_jsx(&element, &text, &options, &mut fresh_ids()),
                Err(JsxDesugarError::NonExecutableMode(_))
            ));
        }
    }

    #[test]
    fn synthetic_ids_are_unique_and_above_source_ids() {
        let text = SourceText::new("<X />").expect("test source fits the per-file budget");
        let element = self_closing(
            4,
            span(0, 5),
            JsxElementName::Identifier(ident(1, 1, 2)),
            Vec::new(),
        );
        let result =
            desugar_jsx(&element, &text, &classic_options(), &mut fresh_ids()).expect("desugars");

        assert!(result.expression.id().get() > 5000);
        assert_eq!(
            result.generated_text.spellings.len(),
            3,
            "React, createElement, and null each need a distinct generated leaf"
        );
        assert!(
            result
                .generated_text
                .spellings
                .keys()
                .all(|node| node.get() > 5000),
            "every generated leaf id must come from the caller's fresh-id source"
        );
    }

    #[test]
    fn namespaced_tags_render_as_colon_separated_strings() {
        let text = SourceText::new("<svg:rect />").expect("test source fits the per-file budget");
        let namespaced = JsxNamespacedName {
            namespace: ident(51, 1, 4),
            name: ident(52, 5, 9),
        };
        let element = self_closing(
            4,
            span(0, 11),
            JsxElementName::Namespace(namespaced),
            Vec::new(),
        );
        let result =
            desugar_jsx(&element, &text, &classic_options(), &mut fresh_ids()).expect("desugars");

        let Expression::Call(call) = result.expression.data() else {
            panic!("classic element must lower to a call");
        };
        let CallArgument::Expression(first) = &call.arguments[0] else {
            panic!("tag argument");
        };
        let Expression::Literal(Literal::String(tag_name)) = first.data() else {
            panic!("namespaced tag must become a string literal");
        };
        assert_eq!(result.generated_text.get(tag_name.id()), Some("svg:rect"));
    }

    #[test]
    fn plan_desugars_each_outermost_jsx_once() {
        let text =
            Arc::new(SourceText::new("<A /><B />").expect("test source fits the per-file budget"));
        let first = self_closing(
            4,
            span(0, 5),
            JsxElementName::Identifier(ident(1, 1, 2)),
            Vec::new(),
        );
        let second = self_closing(
            14,
            span(5, 10),
            JsxElementName::Identifier(ident(11, 6, 7)),
            Vec::new(),
        );
        let statement = Node::new(
            NodeId::new(90),
            span(0, 10),
            Statement::Expression(ExpressionStatement {
                expression: Box::new(Node::new(
                    NodeId::new(91),
                    span(0, 10),
                    Expression::Sequence(SequenceExpression {
                        expressions: vec![first, second],
                    }),
                )),
            }),
        );
        let file = SourceFile::new(
            NodeId::new(0),
            SourceId::new(0),
            ScriptKind::TypeScriptReact,
            span(0, 10),
            Arc::clone(&text),
            Vec::new(),
            vec![statement],
            Token::new(TokenKind::EndOfFile, span(10, 10)),
            Vec::new(),
        );
        let plan = desugar_source_jsx(
            &file,
            &text,
            &classic_options(),
            &mut NodeIdSource::after(NodeId::new(5000)),
        )
        .expect("plans");

        assert_eq!(plan.expression_desugars.len(), 2);
        assert!(plan.expression_desugars.contains_key(&NodeId::new(4)));
        assert!(plan.expression_desugars.contains_key(&NodeId::new(14)));
        assert!(
            plan.demand.bindings.is_empty(),
            "classic mode demands no runtime imports"
        );
    }
}
