//! JSX/TSX checking as an extension of the [`Binder`] expression pass.
//!
//! JSX checking answers three questions for every JSX expression, all on top
//! of the existing scope tree, symbol table, and interned [`TypeTable`]:
//!
//! - **Namespace resolution.** The in-scope `JSX` namespace (value or type
//!   plane, resolved from the element's lexical scope outward) provides the
//!   `IntrinsicElements` map and the `Element` result type. Members are found
//!   through the namespace's export scope first and then through the local
//!   scopes of every merged declaration, so non-exported `JSX` members still
//!   participate. Without a `JSX` namespace, intrinsic checking is inert —
//!   JSX carries no ambient meaning by itself — while value-based tags are
//!   still resolved and checked.
//! - **Element classification.** A lowercase, single-identifier tag is an
//!   *intrinsic* element looked up by name in `JSX.IntrinsicElements`; any
//!   other tag is *value-based* and resolves against the value namespace like
//!   an ordinary expression reference, registering the same `references`
//!   entries an identifier expression would.
//! - **Attribute/children checking.** Attributes fold into one structural
//!   props type — a bare attribute contributes `true`, `name="…"` a string,
//!   `name={expr}` the expression's type, and `{...spread}` members merge in
//!   source order with later members winning. Non-whitespace children
//!   contribute a `children` property typed by the union of all children. The
//!   props object is checked against the `IntrinsicElements` member or the
//!   factory's first parameter with the existing assignability relation.
//!
//! Value-based factories are recovered from the binder's `jsx_callables`
//! side table (function declarations and function/arrow initializers), since
//! function symbols intentionally keep `any` as their symbol type. A generic
//! factory's type arguments are inferred from the synthesized props object
//! with [`InferenceContext`], and the element's result type is the
//! instantiated factory return type. Intrinsic elements and fragments take
//! `JSX.Element`, falling back to `any` when the namespace does not declare
//! one. Result types are recorded in the binder's `jsx_element_types` side
//! table so `type_of_expr` propagates them into surrounding checks such as
//! variable-annotation assignability.
//!
//! Every lookup failure degrades to an `any`-typed element with at most one
//! diagnostic anchored at the responsible tag; recovery never cascades into
//! surrounding expression checking.

use super::binder::{
    Binder, FunctionParameter, PropertyType, ScopeId, ScopeKind, SymbolId, Type, TypeId,
};
use super::inference::{InferenceContext, InferenceParameter};
use super::{
    CANNOT_FIND_NAME, CANNOT_FIND_NAME_MESSAGE, JSX_ATTRIBUTES_NOT_ASSIGNABLE,
    JSX_ATTRIBUTES_NOT_ASSIGNABLE_MESSAGE, JSX_ELEMENT_TYPE_NOT_CALLABLE,
    JSX_ELEMENT_TYPE_NOT_CALLABLE_MESSAGE, JSX_INTRINSIC_ELEMENT_NOT_FOUND,
    JSX_INTRINSIC_ELEMENT_NOT_FOUND_MESSAGE,
};
use crate::source::TextRange;
use crate::syntax::{
    ArrowFunction, Expr, Expression, FunctionLike, JsxAttributeItem, JsxAttributeName, JsxChild,
    JsxElement, JsxElementName, JsxFragment, JsxSelfClosingElement, ParameterNode,
    TypeAnnotationNode, TypeParameterList,
};

/// A callable declaration usable as a JSX factory: a function declaration or
/// function expression, or an arrow function.
#[derive(Clone, Copy)]
pub(crate) enum JsxCallable<'src> {
    Function(&'src FunctionLike),
    Arrow(&'src ArrowFunction),
}

impl<'src> JsxCallable<'src> {
    /// The signature-defining parts shared by both callable forms.
    pub(crate) fn parts(
        self,
    ) -> (
        Option<&'src TypeParameterList>,
        &'src [ParameterNode],
        Option<&'src TypeAnnotationNode>,
    ) {
        match self {
            Self::Function(function) => (
                function.type_parameters.as_ref(),
                &function.parameters,
                function.return_type.as_ref(),
            ),
            Self::Arrow(arrow) => (
                arrow.type_parameters.as_ref(),
                &arrow.parameters,
                arrow.return_type.as_ref(),
            ),
        }
    }
}

/// A declaration signature resolved once and instantiated independently for
/// each JSX use.
pub(crate) struct JsxFactorySignature {
    inference_parameters: Vec<InferenceParameter>,
    parameters: Vec<TypeId>,
    return_type: TypeId,
}

impl<'src> Binder<'src> {
    /// Checks a balanced JSX element `<name attrs>children</name>` and
    /// returns its result type.
    pub(crate) fn check_jsx_element(
        &mut self,
        expression: &'src Expr,
        element: &'src JsxElement,
        scope: ScopeId,
    ) -> TypeId {
        let opening = element.opening.data();
        self.resolve_jsx_attributes(&opening.attributes, scope);
        let children = self.check_jsx_children(&element.children, scope);
        let result = self.check_jsx_opening(
            &opening.name,
            &opening.attributes,
            children,
            expression.range(),
            scope,
        );
        self.record_jsx_element_type(expression, scope, result)
    }

    /// Checks a self-closing JSX element `<name attrs />` and returns its
    /// result type.
    pub(crate) fn check_jsx_self_closing_element(
        &mut self,
        expression: &'src Expr,
        element: &'src JsxSelfClosingElement,
        scope: ScopeId,
    ) -> TypeId {
        self.resolve_jsx_attributes(&element.attributes, scope);
        let result = self.check_jsx_opening(
            &element.name,
            &element.attributes,
            None,
            expression.range(),
            scope,
        );
        self.record_jsx_element_type(expression, scope, result)
    }

    /// Checks a JSX fragment `<>children</>` and returns its result type.
    pub(crate) fn check_jsx_fragment(
        &mut self,
        expression: &'src Expr,
        fragment: &'src JsxFragment,
        scope: ScopeId,
    ) -> TypeId {
        let _ = self.check_jsx_children(&fragment.children, scope);
        self.record_jsx_element_type(expression, scope, None)
    }

    // -- namespace resolution ---------------------------------------------------

    /// Resolves the in-scope `JSX` namespace symbol from `scope`, trying the
    /// value plane then the type plane.
    fn jsx_namespace_symbol(&self, scope: ScopeId) -> Option<SymbolId> {
        self.lookup_value(scope, "JSX")
            .or_else(|| self.lookup_type(scope, "JSX"))
    }

    /// Resolves a named member (`Element`, `IntrinsicElements`) of the `JSX`
    /// namespace: the export scope first, then the local scopes of every
    /// merged declaration, so non-exported members still participate.
    fn jsx_namespace_member(&self, scope: ScopeId, member: &str) -> Option<SymbolId> {
        let namespace = self.jsx_namespace_symbol(scope)?;
        let member_in = |scope: ScopeId| {
            let scope = &self.scopes[scope.get() as usize];
            scope.type_binding(member).or_else(|| scope.value(member))
        };
        if let Some(export_scope) = self.container_member_scope(namespace)
            && let Some(found) = member_in(export_scope)
        {
            return Some(found);
        }
        self.namespace_declarations
            .iter()
            .filter(|binding| binding.symbol == namespace)
            .filter_map(|binding| self.namespace_local_scopes.get(&binding.declaration_id))
            .find_map(|local_scope| member_in(*local_scope))
    }

    /// Returns the declared `JSX.Element` result type, or `any` when the
    /// namespace does not declare one.
    fn jsx_element_type(&mut self, scope: ScopeId) -> TypeId {
        match self.jsx_namespace_member(scope, "Element") {
            Some(symbol) => self.resolve_type_symbol(symbol),
            None => self.types.any(),
        }
    }

    // -- element classification ---------------------------------------------------

    /// Dispatches an opening tag to intrinsic or value-based checking and
    /// returns the element's result type when the tag is value-based with a
    /// known factory return type.
    fn check_jsx_opening(
        &mut self,
        name: &JsxElementName,
        attributes: &'src [JsxAttributeItem],
        children: Option<TypeId>,
        range: TextRange,
        scope: ScopeId,
    ) -> Option<TypeId> {
        match name {
            JsxElementName::Identifier(identifier)
                if is_intrinsic_tag(&self.identifier_text(identifier)) =>
            {
                self.check_intrinsic_tag(name, attributes, children, range, scope);
                None
            }
            _ => self.check_value_tag(name, attributes, children, range, scope),
        }
    }

    /// Checks an intrinsic tag against `JSX.IntrinsicElements`.
    fn check_intrinsic_tag(
        &mut self,
        name: &JsxElementName,
        attributes: &'src [JsxAttributeItem],
        children: Option<TypeId>,
        range: TextRange,
        scope: ScopeId,
    ) {
        let JsxElementName::Identifier(tag) = name else {
            return;
        };
        let tag_name = self.identifier_text(tag).into_owned();
        let Some(intrinsics_symbol) = self.jsx_namespace_member(scope, "IntrinsicElements") else {
            // With no `JSX` namespace, intrinsic checking is inert: JSX has
            // no ambient meaning on its own.
            return;
        };
        let intrinsics = self.resolve_type_symbol(intrinsics_symbol);
        let target = match self.types.get(intrinsics) {
            Type::ObjectType(object) => object
                .properties
                .iter()
                .find(|member| member.name() == tag_name)
                .map(|member| member.type_id()),
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
            | Type::Array(_)
            | Type::Tuple(_)
            | Type::Union(_)
            | Type::Function(_)
            | Type::Named(_)
            | Type::NumericEnum(_) => None,
        };
        let Some(target) = target else {
            self.emit(
                JSX_INTRINSIC_ELEMENT_NOT_FOUND,
                tag.range(),
                JSX_INTRINSIC_ELEMENT_NOT_FOUND_MESSAGE,
            );
            return;
        };
        let props = self.jsx_props_type(attributes, children, scope);
        self.check_jsx_props_assignable(range, props, target);
    }

    /// Checks a value-based tag: resolves the tag value and matches the props
    /// object against the factory's first parameter, returning the factory's
    /// return type as the element's result type.
    fn check_value_tag(
        &mut self,
        name: &JsxElementName,
        attributes: &'src [JsxAttributeItem],
        children: Option<TypeId>,
        range: TextRange,
        scope: ScopeId,
    ) -> Option<TypeId> {
        let symbol = self.resolve_jsx_tag_value(name, scope)?;
        let props = self.jsx_props_type(attributes, children, scope);
        // A declared factory wins over the symbol's type: function symbols
        // intentionally keep `any` as their symbol type.
        if let Some(callable) = self.jsx_callables.get(&symbol).copied() {
            let declaration_scope = self.symbols[symbol.get() as usize].scope();
            return Some(self.check_factory_callable(
                symbol,
                callable,
                props,
                range,
                declaration_scope,
            ));
        }
        let callee = self.symbol_types[symbol.get() as usize];
        match self.types.get(callee) {
            Type::Function(signature) => {
                let signature = signature.clone();
                if let Some(target) = signature
                    .parameters()
                    .first()
                    .map(FunctionParameter::type_id)
                {
                    self.check_jsx_props_assignable(range, props, target);
                }
                Some(signature.return_type())
            }
            // Opaque recovery types must not cascade; nominal types (classes,
            // type parameters) have no visible construct/call side in this
            // type space and are accepted unchecked.
            Type::Any
            | Type::Error
            | Type::Intersection(_)
            | Type::Unknown
            | Type::Named(_)
            | Type::Union(_) => None,
            Type::Never
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
            | Type::Array(_)
            | Type::Tuple(_)
            | Type::ObjectType(_)
            | Type::NumericEnum(_) => {
                self.emit(
                    JSX_ELEMENT_TYPE_NOT_CALLABLE,
                    range,
                    JSX_ELEMENT_TYPE_NOT_CALLABLE_MESSAGE,
                );
                None
            }
        }
    }

    /// Checks a factory recovered from its declaration. Declaration annotations
    /// resolve once; each JSX use gets a fresh inference context.
    fn check_factory_callable(
        &mut self,
        symbol: SymbolId,
        callable: JsxCallable<'src>,
        props: TypeId,
        range: TextRange,
        declaration_scope: ScopeId,
    ) -> TypeId {
        if !self.jsx_factory_signatures.contains_key(&symbol) {
            let signature = self.resolve_jsx_factory_signature(callable, declaration_scope);
            self.jsx_factory_signatures.insert(symbol, signature);
        }

        let (target, result) = {
            let signature = self
                .jsx_factory_signatures
                .get(&symbol)
                .expect("JSX factory signature was cached");
            if signature.inference_parameters.is_empty() {
                (signature.parameters.first().copied(), signature.return_type)
            } else {
                let mut context =
                    InferenceContext::new(&mut self.types, &signature.inference_parameters);
                if let Some(first) = signature.parameters.first().copied() {
                    context.infer_from_argument(first, props, 0);
                }
                let inferred = context.resolve();
                let target = signature
                    .parameters
                    .first()
                    .copied()
                    .map(|first| inferred.instantiate(&mut self.types, first));
                let result = inferred.instantiate(&mut self.types, signature.return_type);
                (target, result)
            }
        };
        if let Some(target) = target {
            self.check_jsx_props_assignable(range, props, target);
        }
        result
    }

    fn resolve_jsx_factory_signature(
        &mut self,
        callable: JsxCallable<'src>,
        declaration_scope: ScopeId,
    ) -> JsxFactorySignature {
        let (type_parameters, parameters, return_type) = callable.parts();
        let type_parameters = type_parameters.filter(|list| !list.parameters.is_empty());
        let scope = match type_parameters {
            Some(list) => {
                let scope = self.new_scope(ScopeKind::Function, Some(declaration_scope));
                self.bind_type_parameters(Some(list), scope);
                scope
            }
            None => declaration_scope,
        };
        let parameters = parameters
            .iter()
            .map(|parameter| match &parameter.data().type_annotation {
                Some(annotation) => self.resolve_type(&annotation.data().type_node, scope),
                None => self.types.any(),
            })
            .collect();
        let return_type = match return_type {
            Some(annotation) => self.resolve_type(&annotation.data().type_node, scope),
            None => self.types.any(),
        };
        let inference_parameters = type_parameters.map_or_else(Vec::new, |list| {
            let mut resolved = Vec::with_capacity(list.parameters.len());
            for parameter in &list.parameters {
                let data = parameter.data();
                let name = self.identifier_text(&data.name).into_owned();
                let Some(symbol) = self.scopes[scope.get() as usize].type_binding(&name) else {
                    self.emit(
                        CANNOT_FIND_NAME,
                        data.name.range(),
                        CANNOT_FIND_NAME_MESSAGE,
                    );
                    continue;
                };
                let mut inference_parameter = InferenceParameter::new(symbol);
                if let Some(constraint) = &data.constraint {
                    inference_parameter =
                        inference_parameter.with_constraint(self.resolve_type(constraint, scope));
                }
                if let Some(default) = &data.default {
                    inference_parameter =
                        inference_parameter.with_default(self.resolve_type(default, scope));
                }
                resolved.push(inference_parameter);
            }
            resolved
        });
        JsxFactorySignature {
            inference_parameters,
            parameters,
            return_type,
        }
    }

    /// Resolves a JSX tag name through the value namespace, following dotted
    /// member chains through container scopes and registering `references`
    /// entries like ordinary identifier resolution.
    fn resolve_jsx_tag_value(&mut self, name: &JsxElementName, scope: ScopeId) -> Option<SymbolId> {
        match name {
            JsxElementName::Identifier(identifier) => {
                let text = self.identifier_text(identifier).into_owned();
                match self.lookup_value(scope, &text) {
                    Some(symbol) => {
                        self.references.insert(identifier.id(), symbol);
                        Some(symbol)
                    }
                    None => {
                        if !self.suppresses_unresolved_value(scope) {
                            self.emit(
                                CANNOT_FIND_NAME,
                                identifier.range(),
                                CANNOT_FIND_NAME_MESSAGE,
                            );
                        }
                        None
                    }
                }
            }
            JsxElementName::Member(member) => {
                let object = self.resolve_jsx_tag_value(&member.object, scope)?;
                self.resolve_tag_member(object, &member.property)
            }
            JsxElementName::Namespace(namespaced) => {
                let text = self.identifier_text(&namespaced.namespace).into_owned();
                let Some(namespace) = self.lookup_value(scope, &text) else {
                    if !self.suppresses_unresolved_value(scope) {
                        self.emit(
                            CANNOT_FIND_NAME,
                            namespaced.namespace.range(),
                            CANNOT_FIND_NAME_MESSAGE,
                        );
                    }
                    return None;
                };
                self.resolve_tag_member(namespace, &namespaced.name)
            }
        }
    }

    /// Resolves one member step of a dotted tag through the container's
    /// member scope. A missing container scope is recovery, not an error.
    fn resolve_tag_member(
        &mut self,
        container: SymbolId,
        property: &crate::syntax::IdentifierNode,
    ) -> Option<SymbolId> {
        let member_scope = self.container_member_scope(container)?;
        let text = self.identifier_text(property).into_owned();
        match self.scopes[member_scope.get() as usize].value(&text) {
            Some(symbol) => {
                self.references.insert(property.id(), symbol);
                Some(symbol)
            }
            None => {
                self.emit(CANNOT_FIND_NAME, property.range(), CANNOT_FIND_NAME_MESSAGE);
                None
            }
        }
    }

    // -- props synthesis ---------------------------------------------------------

    /// Resolves every attribute value expression so reference and type
    /// information exists before props synthesis.
    fn resolve_jsx_attributes(&mut self, attributes: &'src [JsxAttributeItem], scope: ScopeId) {
        for attribute in attributes {
            match attribute {
                JsxAttributeItem::Attribute(attribute) => {
                    if let Some(crate::syntax::JsxAttributeInitializer::Expression(container)) =
                        &attribute.data().initializer
                        && let Some(expression) = &container.data().expression
                    {
                        self.resolve_expr(expression, scope);
                    }
                }
                JsxAttributeItem::Spread(spread) => {
                    self.resolve_expr(&spread.data().expression, scope);
                }
            }
        }
    }

    /// Folds the attribute list and children into one structural props type.
    /// Spread members merge in source order; later members win.
    fn jsx_props_type(
        &mut self,
        attributes: &'src [JsxAttributeItem],
        children: Option<TypeId>,
        scope: ScopeId,
    ) -> TypeId {
        let mut properties: Vec<PropertyType> = Vec::new();
        let mut has_opaque_spread = false;
        for attribute in attributes {
            match attribute {
                JsxAttributeItem::Attribute(attribute) => {
                    let data = attribute.data();
                    let Some(name) = jsx_attribute_key(self, &data.name) else {
                        continue;
                    };
                    let value = match &data.initializer {
                        None => self.types.boolean_literal(true),
                        Some(crate::syntax::JsxAttributeInitializer::String(_)) => {
                            self.types.string()
                        }
                        Some(crate::syntax::JsxAttributeInitializer::Expression(container)) => {
                            match &container.data().expression {
                                Some(expression) => self.type_of_expr(expression, scope),
                                None => self.types.any(),
                            }
                        }
                    };
                    upsert_property(&mut properties, PropertyType::new(name, false, value));
                }
                JsxAttributeItem::Spread(spread) => {
                    let spread_type = self.type_of_expr(&spread.data().expression, scope);
                    if let Type::ObjectType(object) = self.types.get(spread_type) {
                        let spread_properties = object.properties.clone();
                        for property in spread_properties {
                            upsert_property(&mut properties, property);
                        }
                    } else {
                        has_opaque_spread = true;
                    }
                }
            }
        }
        if let Some(children) = children {
            upsert_property(
                &mut properties,
                PropertyType::new("children", false, children),
            );
        }
        if has_opaque_spread {
            self.types.any()
        } else {
            self.types.object_type(properties)
        }
    }

    /// Checks the synthesized props object against the element's expected
    /// props type. Opaque targets absorb everything.
    fn check_jsx_props_assignable(&mut self, range: TextRange, props: TypeId, target: TypeId) {
        if matches!(
            self.types.get(target),
            Type::Any | Type::Unknown | Type::Error
        ) {
            return;
        }
        if !self.types.assignable(props, target) {
            self.emit(
                JSX_ATTRIBUTES_NOT_ASSIGNABLE,
                range,
                JSX_ATTRIBUTES_NOT_ASSIGNABLE_MESSAGE,
            );
        }
    }

    // -- children -------------------------------------------------------------------

    /// Resolves every child, checking nested elements recursively, and
    /// returns the union of all non-whitespace child types. Whitespace-only
    /// text contributes nothing.
    fn check_jsx_children(&mut self, children: &'src [JsxChild], scope: ScopeId) -> Option<TypeId> {
        let mut child_types: Vec<TypeId> = Vec::new();
        for child in children {
            match child {
                JsxChild::Text(text) => {
                    let raw = self.text(text.data().token());
                    if raw.trim().is_empty() {
                        continue;
                    }
                    child_types.push(self.types.string());
                }
                JsxChild::ExpressionContainer(container) => {
                    if let Some(expression) = &container.data().expression {
                        self.resolve_expr(expression, scope);
                        child_types.push(self.type_of_expr(expression, scope));
                    }
                }
                JsxChild::Spread(spread) => {
                    self.resolve_expr(&spread.data().expression, scope);
                    let spread_type = self.type_of_expr(&spread.data().expression, scope);
                    let child_type = match self.types.get(spread_type) {
                        Type::Array(element) => *element,
                        _ => self.types.any(),
                    };
                    child_types.push(child_type);
                }
                JsxChild::Element(expression) => {
                    child_types.push(self.check_jsx_nested(expression, scope));
                }
            }
        }
        if child_types.is_empty() {
            None
        } else {
            Some(self.types.union(&child_types))
        }
    }

    /// Checks one nested JSX child expression, returning its result type.
    fn check_jsx_nested(&mut self, expression: &'src Expr, scope: ScopeId) -> TypeId {
        match expression.data() {
            Expression::JsxElement(element) => self.check_jsx_element(expression, element, scope),
            Expression::JsxSelfClosingElement(element) => {
                self.check_jsx_self_closing_element(expression, element, scope)
            }
            Expression::JsxFragment(fragment) => {
                self.check_jsx_fragment(expression, fragment, scope)
            }
            _ => {
                self.resolve_expr(expression, scope);
                self.type_of_expr(expression, scope)
            }
        }
    }

    /// Records the element's result type, defaulting to `JSX.Element` when
    /// the tag did not yield a factory return type.
    fn record_jsx_element_type(
        &mut self,
        expression: &'src Expr,
        scope: ScopeId,
        result: Option<TypeId>,
    ) -> TypeId {
        let element_type = match result {
            Some(result) => result,
            None => self.jsx_element_type(scope),
        };
        self.jsx_element_types.insert(expression.id(), element_type);
        element_type
    }
}

/// Returns whether `tag` is a lowercase intrinsic-style JSX tag such as
/// `div`. Member (`A.B`) and namespaced (`a:b`) tags never classify here;
/// they resolve through the value namespace instead.
fn is_intrinsic_tag(tag: &str) -> bool {
    tag.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
}

/// Returns the props member key for an attribute name. Namespaced attribute
/// names (`xml:lang`) have no props-type key in this type space and are
/// accepted unchecked.
fn jsx_attribute_key(binder: &Binder<'_>, name: &JsxAttributeName) -> Option<String> {
    match name {
        JsxAttributeName::Identifier(identifier) => {
            Some(binder.identifier_text(identifier).into_owned())
        }
        JsxAttributeName::Namespace(_) => None,
    }
}

/// Inserts `property` into `properties`, replacing any earlier member with
/// the same name so later attributes and spreads win in source order.
fn upsert_property(properties: &mut Vec<PropertyType>, property: PropertyType) {
    if let Some(existing) = properties
        .iter_mut()
        .find(|existing| existing.name() == property.name())
    {
        *existing = property;
    } else {
        properties.push(property);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::binder::{SemanticModel, bind_source};
    use super::super::{
        CANNOT_FIND_NAME, JSX_ATTRIBUTES_NOT_ASSIGNABLE, JSX_ELEMENT_TYPE_NOT_CALLABLE,
        JSX_INTRINSIC_ELEMENT_NOT_FOUND, TYPE_NOT_ASSIGNABLE,
    };
    use crate::diagnostic::Diagnostic;
    use crate::source::{ScriptKind, SourceId, SourceText};
    use crate::{parser, scanner};

    fn bound(text: &str) -> (SemanticModel, Vec<Diagnostic>) {
        let parsed = parser::parse(scanner::scan(
            SourceId::new(0),
            ScriptKind::TypeScriptReact,
            Arc::new(SourceText::new(text).expect("test source fits the per-file budget")),
        ));
        assert!(
            parsed.diagnostics().is_empty(),
            "unexpected parse diagnostics: {:?}",
            parsed.diagnostics()
        );
        bind_source(parsed.product())
    }

    fn codes(text: &str) -> Vec<&'static str> {
        let (_model, diagnostics) = bound(text);
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code().as_str())
            .collect()
    }

    #[track_caller]
    fn assert_clean(codes: Vec<&'static str>) {
        assert!(codes.is_empty(), "unexpected diagnostics: {codes:?}");
    }

    /// A `JSX` namespace with an empty `Element` type and one intrinsic `div`
    /// taking `{ id?: string, children?: string }`.
    const JSX_PREAMBLE: &str = "namespace JSX { \
        interface Element {} \
        interface IntrinsicElements { div: { id?: string; children?: string } } \
    } ";

    // -- intrinsic vs value-based elements ---------------------------------------

    #[test]
    fn known_intrinsic_element_with_valid_attributes_is_clean() {
        assert_clean(codes(&format!(
            "{JSX_PREAMBLE} const x = <div id=\"a\" />;"
        )));
    }

    #[test]
    fn unknown_intrinsic_element_reports_intrinsic_element_not_found() {
        assert_eq!(
            codes(&format!("{JSX_PREAMBLE} const x = <span />;")),
            [JSX_INTRINSIC_ELEMENT_NOT_FOUND.as_str()]
        );
    }

    #[test]
    fn value_based_element_resolves_its_factory() {
        let source = format!(
            "{JSX_PREAMBLE} function Comp(props: {{ id?: string }}) {{ return null; }} \
             const x = <Comp id=\"a\" />;"
        );
        assert_clean(codes(&source));
    }

    #[test]
    fn unknown_value_based_tag_reports_cannot_find_name() {
        assert_eq!(
            codes(&format!("{JSX_PREAMBLE} const x = <Missing />;")),
            [CANNOT_FIND_NAME.as_str()]
        );
    }

    #[test]
    fn unresolved_tag_names_report_cannot_find_name_in_every_name_shape() {
        for source in [
            format!("{JSX_PREAMBLE} const x = <Missing />;"),
            format!("{JSX_PREAMBLE} const x = <Missing.name />;"),
            format!("{JSX_PREAMBLE} const x = <Missing:name />;"),
        ] {
            assert_eq!(codes(&source), [CANNOT_FIND_NAME.as_str()], "{source}");
        }
    }

    #[test]
    fn non_callable_value_tag_reports_not_callable() {
        assert_eq!(
            codes(&format!("{JSX_PREAMBLE} const C = 42; const x = <C />;")),
            [JSX_ELEMENT_TYPE_NOT_CALLABLE.as_str()]
        );
    }

    #[test]
    fn dotted_tag_resolves_through_namespace_member_scopes() {
        let source = format!(
            "{JSX_PREAMBLE} namespace UI {{ \
                export function Button(props: {{ label: string }}) {{ return null; }} \
            }} \
            const ok = <UI.Button label=\"x\" />; \
            const bad = <UI.Button label={{1}} />;"
        );
        assert_eq!(codes(&source), [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]);
    }

    // -- attribute type errors -----------------------------------------------------

    #[test]
    fn mistyped_intrinsic_attribute_reports_attributes_not_assignable() {
        assert_eq!(
            codes(&format!("{JSX_PREAMBLE} const x = <div id={{1}} />;")),
            [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]
        );
    }

    #[test]
    fn missing_required_intrinsic_attribute_is_an_error() {
        let source = "namespace JSX { \
            interface Element {} \
            interface IntrinsicElements { div: { id: string } } \
        } \
        const x = <div />;";
        assert_eq!(codes(source), [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]);
    }

    #[test]
    fn mistyped_factory_prop_reports_attributes_not_assignable() {
        let source = format!(
            "{JSX_PREAMBLE} function Comp(props: {{ id: string }}) {{ return null; }} \
             const x = <Comp id={{1}} />;"
        );
        assert_eq!(codes(&source), [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]);
    }

    #[test]
    fn bare_attribute_contributes_true() {
        let source = "namespace JSX { \
            interface Element {} \
            interface IntrinsicElements { div: { hidden?: boolean } } \
        } \
        const x = <div hidden />; \
        const y = <div hidden=\"yes\" />;";
        assert_eq!(codes(source), [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]);
    }

    // -- children type checking -----------------------------------------------------

    #[test]
    fn text_children_check_against_the_children_prop() {
        let good = format!("{JSX_PREAMBLE} const x = <div>hello</div>;");
        assert_clean(codes(&good));

        let bad = "namespace JSX { \
            interface Element {} \
            interface IntrinsicElements { div: { children: number } } \
        } \
        const x = <div>hello</div>;";
        assert_eq!(codes(bad), [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]);
    }

    #[test]
    fn expression_children_union_into_the_children_prop() {
        let bad = format!("{JSX_PREAMBLE} const n = 1; const x = <div>{{n}}</div>;");
        assert_eq!(codes(&bad), [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]);
    }

    #[test]
    fn whitespace_only_children_contribute_no_children_prop() {
        let source = "namespace JSX { \
            interface Element {} \
            interface IntrinsicElements { div: { id?: string } } \
        } \
        const x = <div>   </div>;";
        assert_clean(codes(source));
    }

    #[test]
    fn spread_children_contribute_array_elements_and_keep_other_values_opaque() {
        let strings = format!(
            "{JSX_PREAMBLE} const items: string[] = []; const x = <div>{{...items}}</div>;"
        );
        assert_clean(codes(&strings));

        let numbers = format!(
            "{JSX_PREAMBLE} const items: number[] = []; const x = <div>{{...items}}</div>;"
        );
        assert_eq!(codes(&numbers), [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]);

        let opaque = format!(
            "{JSX_PREAMBLE} const items: unknown = null; const x = <div>{{...items}}</div>;"
        );
        assert_clean(codes(&opaque));
    }

    // -- spread attributes -------------------------------------------------------------

    #[test]
    fn spread_attributes_merge_into_the_props_object() {
        let good = format!(
            "{JSX_PREAMBLE} const extra = {{ id: \"x\" }}; const x = <div {{...extra}} />;"
        );
        assert_clean(codes(&good));

        let bad =
            format!("{JSX_PREAMBLE} const wrong = {{ id: 1 }}; const x = <div {{...wrong}} />;");
        assert_eq!(codes(&bad), [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]);
    }

    #[test]
    fn later_spread_members_override_earlier_attributes() {
        let source = format!(
            "{JSX_PREAMBLE} const fix = {{ id: \"s\" }}; const x = <div id={{1}} {{...fix}} />;"
        );
        assert_clean(codes(&source));
    }

    #[test]
    fn opaque_spread_skips_element_props_assignability() {
        let source = "namespace JSX { \
            interface Element {} \
            interface IntrinsicElements { div: { id: string } } \
        } \
        const opaque: any = {}; \
        const element = <div {...opaque} />;";
        assert_clean(codes(source));
    }

    // -- factory function inference ----------------------------------------------------

    #[test]
    fn element_result_type_flows_from_the_factory_return_type() {
        let good = format!(
            "{JSX_PREAMBLE} function Comp(props: {{ id?: string }}): string {{ return \"x\"; }} \
             const x: string = <Comp />;"
        );
        assert_clean(codes(&good));

        let bad = format!(
            "{JSX_PREAMBLE} function Comp(props: {{ id?: string }}): string {{ return \"x\"; }} \
             const x: number = <Comp />;"
        );
        assert_eq!(codes(&bad), [TYPE_NOT_ASSIGNABLE.as_str()]);
    }

    #[test]
    fn intrinsic_element_result_takes_the_jsx_element_type() {
        let bad = format!("{JSX_PREAMBLE} const x: number = <div />;");
        assert_eq!(codes(&bad), [TYPE_NOT_ASSIGNABLE.as_str()]);
    }

    #[test]
    fn generic_factory_infers_type_arguments_from_the_props() {
        let source = format!(
            "{JSX_PREAMBLE} \
             function Comp<T>(props: {{ value: T }}): T {{ return props.value; }} \
             const ok: number = <Comp value={{1}} />; \
             const bad: string = <Comp value={{1}} />;"
        );
        assert_eq!(codes(&source), [TYPE_NOT_ASSIGNABLE.as_str()]);
    }

    #[test]
    fn generic_factory_signature_resolution_is_cached_per_declaration() {
        let one_use = format!(
            "{JSX_PREAMBLE} \
             function Comp<T>(props: {{ value: T }}): T {{ return props.value; }} \
             const number_value: number = <Comp value={{1}} />;"
        );
        let two_uses = format!(
            "{one_use} \
             const string_value: string = <Comp value={{\"value\"}} />;"
        );

        let (one_model, one_diagnostics) = bound(&one_use);
        let (two_model, two_diagnostics) = bound(&two_uses);
        assert!(one_diagnostics.is_empty(), "{one_diagnostics:?}");
        assert!(two_diagnostics.is_empty(), "{two_diagnostics:?}");
        assert_eq!(
            one_model.scopes().len(),
            two_model.scopes().len(),
            "a second JSX use must reuse the resolved declaration signature"
        );
    }

    #[test]
    fn generic_factory_respects_its_constraint() {
        let source = format!(
            "{JSX_PREAMBLE} \
             function Comp<T extends string>(props: {{ value: T }}) {{ return null; }} \
             const bad = <Comp value={{1}} />;"
        );
        assert_eq!(codes(&source), [JSX_ATTRIBUTES_NOT_ASSIGNABLE.as_str()]);
    }

    // -- malformed type parameters --------------------------------------------------------

    /// A generic JSX factory with a duplicate type parameter name must not
    /// panic the compiler. The binder reports the duplicate; the factory
    /// check degrades to a diagnostic instead of crashing on the type
    /// parameter lookup.
    #[test]
    fn generic_factory_with_duplicate_type_parameter_does_not_panic() {
        use super::super::DUPLICATE_DECLARATION;
        let source = format!(
            "{JSX_PREAMBLE} \
             function Comp<T, T>(props: {{ value: T }}) {{ return props.value; }} \
             const x = <Comp value={{1}} />;"
        );
        let diagnostics = codes(&source);
        // The compiler must not panic; it reports the duplicate declaration.
        assert!(
            diagnostics.contains(&DUPLICATE_DECLARATION.as_str()),
            "expected DUPLICATE_DECLARATION in {diagnostics:?}"
        );
    }

    // -- namespace resolution degradation ------------------------------------------------

    #[test]
    fn intrinsic_elements_are_inert_without_a_jsx_namespace() {
        assert_clean(codes("const x = <div id={1} />;"));
    }

    #[test]
    fn nested_elements_and_fragments_check_recursively() {
        // `unknown` children keep the outer element clean so the nested
        // element's own diagnostic is the only one observed.
        let bad = "namespace JSX { \
            interface Element {} \
            interface IntrinsicElements { div: { children?: unknown } } \
        } \
        const x = <div><section /></div>;";
        assert_eq!(codes(bad), [JSX_INTRINSIC_ELEMENT_NOT_FOUND.as_str()]);

        let fragment = format!("{JSX_PREAMBLE} const x = <><div>text</div></>;");
        assert_clean(codes(&fragment));
    }
}
