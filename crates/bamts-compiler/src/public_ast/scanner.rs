//! Public scanner projection.
//!
//! The scanner, scanned product, and whole-source driver are the canonical
//! compiler values. Re-exporting them here keeps lexical state and recovery
//! diagnostics identical across compiler and package consumers.

pub use crate::scanner::{ScannedSource, Scanner, scan};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::source::{ScriptKind, SourceId, SourceText};
    use crate::syntax::TokenKind;

    #[test]
    fn scanner_projection_preserves_canonical_token_order_and_text() {
        let source = Arc::new(
            SourceText::new("let answer = 42;").expect("test source fits the per-file budget"),
        );
        let recovered = scan(
            SourceId::new(7),
            ScriptKind::TypeScript,
            Arc::clone(&source),
        );
        let scanned = recovered.product();
        let kinds: Vec<_> = scanned.tokens().iter().map(|token| token.kind()).collect();
        assert_eq!(
            kinds,
            [
                TokenKind::KwLet,
                TokenKind::Whitespace,
                TokenKind::Identifier,
                TokenKind::Whitespace,
                TokenKind::Eq,
                TokenKind::Whitespace,
                TokenKind::NumericLiteral,
                TokenKind::Semicolon,
            ]
        );
        assert_eq!(scanned.eof().kind(), TokenKind::EndOfFile);
        let identifier = &scanned.tokens()[2];
        let text = scanned.token_text(identifier).unwrap();
        assert_eq!(text, "answer");
        assert_eq!(text.as_ptr(), source.as_str()[4..10].as_ptr());
    }

    #[test]
    fn grammar_directed_rescan_uses_canonical_scanner_state() {
        let source = SourceText::new("/answer/g").expect("test source fits the per-file budget");
        let mut scanner = Scanner::new(SourceId::new(8), ScriptKind::TypeScript, &source);
        assert_eq!(scanner.next_token().kind(), TokenKind::Slash);
        let regex = scanner.rescan_regex();
        assert_eq!(regex.kind(), TokenKind::RegularExpressionLiteral);
        assert_eq!(regex.range().start().get(), 0);
        assert_eq!(regex.range().end().get(), 9);
    }
}
