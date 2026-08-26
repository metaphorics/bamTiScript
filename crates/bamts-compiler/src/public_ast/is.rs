//! Borrow-only syntax classification helpers.

use super::NodeRef;
use crate::syntax::{NodeKind, SyntaxKind, TokenKind};

#[must_use]
pub const fn is_token(kind: SyntaxKind) -> bool {
    kind.is_token()
}

#[must_use]
pub const fn is_node(kind: SyntaxKind) -> bool {
    kind.is_node()
}

#[must_use]
pub const fn is_keyword(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::Token(token) if is_keyword_token(token))
}

#[must_use]
pub const fn is_keyword_token(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::KwAbstract
            | TokenKind::KwAccessor
            | TokenKind::KwAny
            | TokenKind::KwAs
            | TokenKind::KwAsserts
            | TokenKind::KwAsync
            | TokenKind::KwAwait
            | TokenKind::KwBigint
            | TokenKind::KwBoolean
            | TokenKind::KwBreak
            | TokenKind::KwCase
            | TokenKind::KwCatch
            | TokenKind::KwClass
            | TokenKind::KwConst
            | TokenKind::KwConstructor
            | TokenKind::KwContinue
            | TokenKind::KwDeclare
            | TokenKind::KwDebugger
            | TokenKind::KwDefault
            | TokenKind::KwDelete
            | TokenKind::KwDo
            | TokenKind::KwElse
            | TokenKind::KwEnum
            | TokenKind::KwExport
            | TokenKind::KwExtends
            | TokenKind::KwFalse
            | TokenKind::KwFinally
            | TokenKind::KwFor
            | TokenKind::KwFrom
            | TokenKind::KwFunction
            | TokenKind::KwGet
            | TokenKind::KwIf
            | TokenKind::KwImplements
            | TokenKind::KwImport
            | TokenKind::KwIn
            | TokenKind::KwInfer
            | TokenKind::KwInstanceof
            | TokenKind::KwInterface
            | TokenKind::KwIs
            | TokenKind::KwKeyof
            | TokenKind::KwLet
            | TokenKind::KwNamespace
            | TokenKind::KwNever
            | TokenKind::KwNew
            | TokenKind::KwNull
            | TokenKind::KwNumber
            | TokenKind::KwObject
            | TokenKind::KwOf
            | TokenKind::KwOverride
            | TokenKind::KwPackage
            | TokenKind::KwPrivate
            | TokenKind::KwProtected
            | TokenKind::KwPublic
            | TokenKind::KwReadonly
            | TokenKind::KwReturn
            | TokenKind::KwSatisfies
            | TokenKind::KwSet
            | TokenKind::KwStatic
            | TokenKind::KwString
            | TokenKind::KwSuper
            | TokenKind::KwSwitch
            | TokenKind::KwSymbol
            | TokenKind::KwThis
            | TokenKind::KwThrow
            | TokenKind::KwTrue
            | TokenKind::KwTry
            | TokenKind::KwType
            | TokenKind::KwTypeof
            | TokenKind::KwUndefined
            | TokenKind::KwUnique
            | TokenKind::KwUnknown
            | TokenKind::KwVar
            | TokenKind::KwVoid
            | TokenKind::KwWhile
            | TokenKind::KwWith
            | TokenKind::KwYield
    )
}

#[must_use]
pub const fn is_statement(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::Node(node) if is_statement_kind(node))
}

#[must_use]
pub const fn is_expression(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::Node(node) if is_expression_kind(node))
}

#[must_use]
pub const fn is_type_node(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::Node(node) if is_type_node_kind(node))
}

#[must_use]
pub const fn is_declaration(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::Node(node) if is_declaration_kind(node))
}

#[must_use]
pub const fn is_literal(kind: SyntaxKind) -> bool {
    matches!(
        kind,
        SyntaxKind::Node(
            NodeKind::StringLiteral
                | NodeKind::NumericLiteral
                | NodeKind::BigIntLiteral
                | NodeKind::BooleanLiteral
                | NodeKind::NullLiteral
                | NodeKind::RegexLiteral
                | NodeKind::LiteralExpression
                | NodeKind::LiteralType
        )
    )
}

#[must_use]
pub fn node_is(node: NodeRef<'_>, kind: SyntaxKind) -> bool {
    node.syntax_kind() == kind
}

const fn is_statement_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::ImportDeclaration
            | NodeKind::ImportEqualsDeclaration
            | NodeKind::ExportDeclaration
            | NodeKind::VariableDeclaration
            | NodeKind::FunctionDeclaration
            | NodeKind::ClassDeclaration
            | NodeKind::InterfaceDeclaration
            | NodeKind::TypeAliasDeclaration
            | NodeKind::EnumDeclaration
            | NodeKind::NamespaceDeclaration
            | NodeKind::BlockStatement
            | NodeKind::EmptyStatement
            | NodeKind::ExpressionStatement
            | NodeKind::IfStatement
            | NodeKind::SwitchStatement
            | NodeKind::ForStatement
            | NodeKind::ForInStatement
            | NodeKind::ForOfStatement
            | NodeKind::WhileStatement
            | NodeKind::DoWhileStatement
            | NodeKind::TryStatement
            | NodeKind::WithStatement
            | NodeKind::LabeledStatement
            | NodeKind::BreakStatement
            | NodeKind::ContinueStatement
            | NodeKind::ReturnStatement
            | NodeKind::ThrowStatement
            | NodeKind::DebuggerStatement
            | NodeKind::DeclareStatement
            | NodeKind::MissingStatement
    )
}

const fn is_declaration_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::ImportDeclaration
            | NodeKind::ImportEqualsDeclaration
            | NodeKind::ExportDeclaration
            | NodeKind::VariableDeclaration
            | NodeKind::VariableDeclarator
            | NodeKind::FunctionDeclaration
            | NodeKind::ClassDeclaration
            | NodeKind::InterfaceDeclaration
            | NodeKind::TypeAliasDeclaration
            | NodeKind::EnumDeclaration
            | NodeKind::EnumMember
            | NodeKind::NamespaceDeclaration
    )
}

const fn is_expression_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::IdentifierExpression
            | NodeKind::ThisExpression
            | NodeKind::SuperExpression
            | NodeKind::LiteralExpression
            | NodeKind::ArrayExpression
            | NodeKind::ObjectExpression
            | NodeKind::FunctionExpression
            | NodeKind::ClassExpression
            | NodeKind::ArrowFunction
            | NodeKind::CallExpression
            | NodeKind::MemberExpression
            | NodeKind::NewExpression
            | NodeKind::AwaitExpression
            | NodeKind::YieldExpression
            | NodeKind::UnaryExpression
            | NodeKind::UpdateExpression
            | NodeKind::BinaryExpression
            | NodeKind::LogicalExpression
            | NodeKind::ConditionalExpression
            | NodeKind::AssignmentExpression
            | NodeKind::SequenceExpression
            | NodeKind::ParenthesizedExpression
            | NodeKind::AsExpression
            | NodeKind::SatisfiesExpression
            | NodeKind::TypeAssertionExpression
            | NodeKind::NonNullExpression
            | NodeKind::TaggedTemplateExpression
            | NodeKind::TemplateExpression
            | NodeKind::ImportExpression
            | NodeKind::MetaProperty
            | NodeKind::JsxElement
            | NodeKind::JsxSelfClosingElement
            | NodeKind::JsxFragment
            | NodeKind::MissingExpression
    )
}

const fn is_type_node_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::KeywordType
            | NodeKind::LiteralType
            | NodeKind::TypeReference
            | NodeKind::UnionType
            | NodeKind::IntersectionType
            | NodeKind::ArrayType
            | NodeKind::TupleType
            | NodeKind::ObjectType
            | NodeKind::FunctionType
            | NodeKind::ConstructorType
            | NodeKind::TypeQuery
            | NodeKind::TypeOperator
            | NodeKind::IndexedAccessType
            | NodeKind::ConditionalType
            | NodeKind::MappedType
            | NodeKind::InferType
            | NodeKind::ImportType
            | NodeKind::TemplateLiteralType
            | NodeKind::ParenthesizedType
            | NodeKind::ThisType
            | NodeKind::TypePredicate
            | NodeKind::MissingType
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_reject_adjacent_non_members() {
        assert!(is_keyword(TokenKind::KwClass.into()));
        assert!(!is_keyword(TokenKind::Identifier.into()));
        assert!(is_statement(NodeKind::ReturnStatement.into()));
        assert!(!is_statement(NodeKind::CallExpression.into()));
        assert!(is_expression(NodeKind::CallExpression.into()));
        assert!(!is_expression(NodeKind::TypeReference.into()));
        assert!(is_type_node(NodeKind::TypeReference.into()));
        assert!(!is_type_node(NodeKind::TypeAnnotation.into()));
    }
}
