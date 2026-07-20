//! Context-aware SQL completion for the query editor.

pub mod candidates;
pub mod context;
pub mod funcs;
pub mod keywords;
pub mod lexer;
pub mod schema_cache;
pub mod scope;

pub use candidates::{CompletionItem, ItemKind};
pub use schema_cache::SchemaCache;

use crate::db::DbKind;

pub struct CompletionRequest<'a> {
    pub sql: &'a str,
    pub cursor_char: usize,
    pub cache: &'a SchemaCache,
    pub dialect: DbKind,
}

pub fn complete(req: CompletionRequest<'_>) -> Vec<CompletionItem> {
    let tokens = lexer::tokenize(req.sql);

    // Don't open the popup inside a string literal or comment.
    if lexer::cursor_in_dead_zone(&tokens, req.cursor_char) {
        return Vec::new();
    }

    let (prefix, replace_from) = lexer::word_at_cursor(req.sql, req.cursor_char);
    let ctx = context::detect(req.sql, &tokens, req.cursor_char);
    let scope = scope::resolve(&tokens, req.cursor_char, req.cache);

    // Detect keyword casing from the cursor's prefix: upper if any upper char present,
    // lowercase only if prefix is non-empty and all alphabetic chars are lowercase.
    // Empty / punctuation-only prefix defaults to uppercase (SQL convention).
    let has_upper = prefix.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = prefix.chars().any(|c| c.is_ascii_lowercase());
    let case_upper = has_upper || !has_lower;

    let generator = candidates::Generator {
        cache: req.cache,
        dialect: req.dialect,
        case_upper,
    };
    generator.generate(&ctx, &scope, &prefix, replace_from)
}
