//! Generate and rank completion candidates given context and scope.

use nucleo_matcher::{Matcher, Utf32Str};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};

use crate::db::{DbKind, TableMeta};

use super::context::Context;
use super::funcs::functions;
use super::keywords::{dialect_keywords, AFTER_FROM_FOLLOW, AFTER_SELECT, BOOLEAN_OPS, STATEMENT_START};
use super::scope::{ResolvedRelation, Scope};
use super::schema_cache::SchemaCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Keyword,
    Table,
    Column,
    Alias,
    Cte,
    Function,
    JoinFk,
    Schema,
}

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub insert_text: String,
    pub replace_from_char: usize,
    pub label: String,
    pub detail: String,
    pub kind: ItemKind,
    pub score: i32,
}

pub struct Generator<'a> {
    pub cache: &'a SchemaCache,
    pub dialect: DbKind,
    pub case_upper: bool, // keyword casing
}

impl<'a> Generator<'a> {
    pub fn generate(
        &self,
        ctx: &Context,
        scope: &Scope,
        prefix: &str,
        replace_from: usize,
    ) -> Vec<CompletionItem> {
        let mut items: Vec<CompletionItem> = Vec::new();

        match ctx {
            Context::StatementStart => {
                for &kw in STATEMENT_START {
                    items.push(self.mk_keyword(kw, replace_from));
                }
            }
            Context::AfterSelectList => {
                for &kw in AFTER_SELECT {
                    items.push(self.mk_keyword(kw, replace_from));
                }
                self.push_columns(&mut items, scope, replace_from, /*qualify_when_multi*/ true);
                self.push_functions(&mut items, replace_from);
                // Offer FROM as a follow-up keyword, common flow.
                items.push(self.mk_keyword("FROM", replace_from));
            }
            Context::AfterFromJoin => {
                self.push_tables(&mut items, replace_from, /*include_schema_prefix*/ false);
                self.push_ctes(&mut items, scope, replace_from);
                self.push_schemas(&mut items, replace_from);
            }
            Context::AfterJoinTableNeedOn => {
                // Suggest "ON <fk>" completions for FK links between the last JOIN target
                // and the other in-scope tables.
                items.push(self.mk_keyword("ON", replace_from));
                self.push_fk_joins(&mut items, scope, replace_from);
                // Also offer USING as a keyword for equal-named-column joins.
                items.push(self.mk_keyword("USING", replace_from));
            }
            Context::AfterOn | Context::AfterWhereOrHaving | Context::AfterGroupOrOrderBy => {
                self.push_columns(&mut items, scope, replace_from, true);
                self.push_functions(&mut items, replace_from);
                for &kw in BOOLEAN_OPS {
                    items.push(self.mk_keyword(kw, replace_from));
                }
                for &kw in AFTER_FROM_FOLLOW {
                    items.push(self.mk_keyword(kw, replace_from));
                }
            }
            Context::AfterInsertInto | Context::AfterUpdate | Context::AfterDeleteFrom => {
                self.push_tables(&mut items, replace_from, false);
                self.push_schemas(&mut items, replace_from);
            }
            Context::AfterUpdateSet => {
                self.push_columns(&mut items, scope, replace_from, false);
            }
            Context::InsertColumnList { schema, table } => {
                if let Some(t) = self
                    .cache
                    .resolve_table(schema.as_deref(), table)
                {
                    for col in &t.columns {
                        items.push(CompletionItem {
                            insert_text: quote_ident(&col.name, self.dialect),
                            replace_from_char: replace_from,
                            label: col.name.clone(),
                            detail: format!("column: {}", col.type_name),
                            kind: ItemKind::Column,
                            score: 0,
                        });
                    }
                }
            }
            Context::AfterWith => {
                // Offer `RECURSIVE` after WITH, and a placeholder CTE name.
                items.push(self.mk_keyword("RECURSIVE", replace_from));
            }
            Context::AfterDot { parts } => {
                self.handle_dot(&mut items, parts, scope, replace_from);
            }
            Context::Unknown => {
                self.push_columns(&mut items, scope, replace_from, true);
                self.push_tables(&mut items, replace_from, false);
                self.push_functions(&mut items, replace_from);
            }
        }

        // Dialect-specific extras.
        for &kw in dialect_keywords(self.dialect) {
            items.push(self.mk_keyword(kw, replace_from));
        }

        self.score_filter_and_sort(items, prefix)
    }

    fn handle_dot(
        &self,
        items: &mut Vec<CompletionItem>,
        parts: &[String],
        scope: &Scope,
        replace_from: usize,
    ) {
        match parts.len() {
            1 => {
                let q = &parts[0];
                // 1. Alias of an in-scope relation that resolved to real columns.
                for r in &scope.relations {
                    if r.effective_alias().eq_ignore_ascii_case(q) && !r.columns.is_empty() {
                        for c in &r.columns {
                            items.push(CompletionItem {
                                insert_text: quote_ident(&c.name, self.dialect),
                                replace_from_char: replace_from,
                                label: c.name.clone(),
                                detail: column_detail(c, r),
                                kind: ItemKind::Column,
                                score: 0,
                            });
                        }
                        return;
                    }
                }
                // 2. CTE name → best-effort columns.
                if let Some(cte) = scope.ctes.iter().find(|c| c.name.eq_ignore_ascii_case(q)) {
                    for col in &cte.columns {
                        items.push(CompletionItem {
                            insert_text: quote_ident(col, self.dialect),
                            replace_from_char: replace_from,
                            label: col.clone(),
                            detail: format!("column of CTE {}", cte.name),
                            kind: ItemKind::Column,
                            score: 0,
                        });
                    }
                    return;
                }
                // 3. Schema name → tables in that schema.
                if self
                    .cache
                    .schemas()
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(q))
                {
                    for t in self.cache.tables_in_schema(q) {
                        items.push(self.mk_table(t, replace_from, /*qualify*/ false));
                    }
                    return;
                }
                // 4. Unqualified table name → its columns (even if not in FROM).
                if let Some(t) = self.cache.resolve_table(None, q) {
                    for c in &t.columns {
                        items.push(CompletionItem {
                            insert_text: quote_ident(&c.name, self.dialect),
                            replace_from_char: replace_from,
                            label: c.name.clone(),
                            detail: format!("column: {}", c.type_name),
                            kind: ItemKind::Column,
                            score: 0,
                        });
                    }
                }
            }
            2 => {
                // schema.table.  → columns of that table.
                if let Some(t) = self.cache.resolve_table(Some(&parts[0]), &parts[1]) {
                    for c in &t.columns {
                        items.push(CompletionItem {
                            insert_text: quote_ident(&c.name, self.dialect),
                            replace_from_char: replace_from,
                            label: c.name.clone(),
                            detail: format!("column: {}", c.type_name),
                            kind: ItemKind::Column,
                            score: 0,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    fn push_columns(
        &self,
        items: &mut Vec<CompletionItem>,
        scope: &Scope,
        replace_from: usize,
        qualify_when_multi: bool,
    ) {
        let qualify = qualify_when_multi && scope.relations.len() > 1;
        for r in &scope.relations {
            for c in &r.columns {
                let (label, insert) = if qualify {
                    let alias = r.effective_alias();
                    (
                        format!("{}.{}", alias, c.name),
                        quote_qualified(&[alias, &c.name], self.dialect),
                    )
                } else {
                    (c.name.clone(), quote_ident(&c.name, self.dialect))
                };
                items.push(CompletionItem {
                    insert_text: insert,
                    replace_from_char: replace_from,
                    label,
                    detail: column_detail(c, r),
                    kind: ItemKind::Column,
                    score: 0,
                });
            }
            // The alias itself is useful for typing `u.` later.
            if let Some(a) = &r.alias {
                items.push(CompletionItem {
                    insert_text: quote_ident(a, self.dialect),
                    replace_from_char: replace_from,
                    label: a.clone(),
                    detail: format!("alias for {}", r.name),
                    kind: ItemKind::Alias,
                    score: 0,
                });
            }
        }
    }

    fn push_tables(
        &self,
        items: &mut Vec<CompletionItem>,
        replace_from: usize,
        include_schema_prefix: bool,
    ) {
        // Track duplicates: if a table name appears only in one schema, offer it unqualified.
        let mut names: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for t in self.cache.all_tables() {
            *names.entry(t.name.to_ascii_lowercase()).or_insert(0) += 1;
        }
        for t in self.cache.all_tables() {
            let is_default = t.schema == self.cache.default_schema();
            let unique = names.get(&t.name.to_ascii_lowercase()).copied().unwrap_or(0) == 1;
            if unique || is_default {
                items.push(self.mk_table(t, replace_from, include_schema_prefix));
            }
            if include_schema_prefix || !is_default {
                items.push(self.mk_table(t, replace_from, true));
            }
        }
    }

    fn push_ctes(&self, items: &mut Vec<CompletionItem>, scope: &Scope, replace_from: usize) {
        for cte in &scope.ctes {
            items.push(CompletionItem {
                insert_text: quote_ident(&cte.name, self.dialect),
                replace_from_char: replace_from,
                label: cte.name.clone(),
                detail: format!("CTE ({} cols)", cte.columns.len()),
                kind: ItemKind::Cte,
                score: 0,
            });
        }
    }

    fn push_schemas(&self, items: &mut Vec<CompletionItem>, replace_from: usize) {
        for s in self.cache.schemas() {
            items.push(CompletionItem {
                insert_text: quote_ident(s, self.dialect),
                replace_from_char: replace_from,
                label: s.clone(),
                detail: "schema".to_string(),
                kind: ItemKind::Schema,
                score: 0,
            });
        }
    }

    fn push_functions(&self, items: &mut Vec<CompletionItem>, replace_from: usize) {
        for f in functions(self.dialect) {
            items.push(CompletionItem {
                insert_text: format!("{}(", f.name),
                replace_from_char: replace_from,
                label: f.name.to_string(),
                detail: f.signature.to_string(),
                kind: ItemKind::Function,
                score: 0,
            });
        }
    }

    fn push_fk_joins(
        &self,
        items: &mut Vec<CompletionItem>,
        scope: &Scope,
        replace_from: usize,
    ) {
        // The last added relation is the JOIN target. Use previous relations as the "left".
        let Some(right) = scope.relations.last() else { return };
        let lefts: Vec<&ResolvedRelation> = scope.relations.iter().rev().skip(1).collect();

        let r_schema = right
            .schema
            .clone()
            .unwrap_or_else(|| self.cache.default_schema().to_string());
        let r_name = right.name.clone();

        // Outgoing FKs from the RIGHT table → match against in-scope lefts.
        for fk in self.cache.outgoing_fks(&r_schema, &r_name) {
            for l in &lefts {
                if tables_match(l, &fk.to_schema, &fk.to_table, self.cache.default_schema()) {
                    let label = format!(
                        "ON {l}.{lc} = {r}.{rc}",
                        l = l.effective_alias(),
                        lc = fk.to_column,
                        r = right.effective_alias(),
                        rc = fk.from_column,
                    );
                    let insert = format!(
                        "ON {l} = {r}",
                        l = quote_qualified(&[l.effective_alias(), &fk.to_column], self.dialect),
                        r = quote_qualified(&[right.effective_alias(), &fk.from_column], self.dialect),
                    );
                    items.push(CompletionItem {
                        insert_text: insert,
                        replace_from_char: replace_from,
                        label,
                        detail: format!("FK: {}.{} → {}.{}", r_name, fk.from_column, fk.to_table, fk.to_column),
                        kind: ItemKind::JoinFk,
                        score: 2000,
                    });
                }
            }
        }

        // Incoming FKs into the RIGHT table from any left.
        for (src_tbl, fk) in self.cache.incoming_fks(&r_schema, &r_name) {
            for l in &lefts {
                if tables_match(l, &src_tbl.schema, &src_tbl.name, self.cache.default_schema()) {
                    let label = format!(
                        "ON {l}.{lc} = {r}.{rc}",
                        l = l.effective_alias(),
                        lc = fk.from_column,
                        r = right.effective_alias(),
                        rc = fk.to_column,
                    );
                    let insert = format!(
                        "ON {l} = {r}",
                        l = quote_qualified(&[l.effective_alias(), &fk.from_column], self.dialect),
                        r = quote_qualified(&[right.effective_alias(), &fk.to_column], self.dialect),
                    );
                    items.push(CompletionItem {
                        insert_text: insert,
                        replace_from_char: replace_from,
                        label,
                        detail: format!("FK: {}.{} → {}.{}", src_tbl.name, fk.from_column, r_name, fk.to_column),
                        kind: ItemKind::JoinFk,
                        score: 2000,
                    });
                }
            }
        }
    }

    fn mk_keyword(&self, kw: &str, replace_from: usize) -> CompletionItem {
        let text = if self.case_upper { kw.to_string() } else { kw.to_ascii_lowercase() };
        CompletionItem {
            insert_text: text.clone(),
            replace_from_char: replace_from,
            label: text,
            detail: "keyword".to_string(),
            kind: ItemKind::Keyword,
            score: 0,
        }
    }

    fn mk_table(&self, t: &TableMeta, replace_from: usize, qualify: bool) -> CompletionItem {
        let (insert, label) = if qualify {
            (
                quote_qualified(&[&t.schema, &t.name], self.dialect),
                format!("{}.{}", t.schema, t.name),
            )
        } else {
            (quote_ident(&t.name, self.dialect), t.name.clone())
        };
        CompletionItem {
            insert_text: insert,
            replace_from_char: replace_from,
            label,
            detail: format!(
                "{} in {}",
                match t.kind {
                    crate::db::TableKind::Table => "table",
                    crate::db::TableKind::View => "view",
                },
                t.schema
            ),
            kind: ItemKind::Table,
            score: 0,
        }
    }

    fn score_filter_and_sort(
        &self,
        items: Vec<CompletionItem>,
        prefix: &str,
    ) -> Vec<CompletionItem> {
        let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let out: Vec<CompletionItem> = if prefix.is_empty() {
            // No prefix: keep everything, sort by (kind-priority, label).
            items
                .into_iter()
                .map(|mut it| {
                    it.score += kind_bias(it.kind);
                    it
                })
                .collect()
        } else {
            let pat = Pattern::parse(prefix, CaseMatching::Ignore, Normalization::Smart);
            items
                .into_iter()
                .filter_map(|mut it| {
                    let mut label_buf = Vec::new();
                    let haystack = Utf32Str::new(&it.label, &mut label_buf);
                    let s = pat.score(haystack, &mut matcher);
                    s.map(|raw| {
                        let mut score = raw as i32;
                        score += kind_bias(it.kind);
                        if it.label.to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase()) {
                            score += 500;
                        }
                        it.score = score;
                        it
                    })
                })
                .collect()
        };

        let mut out = out;
        out.sort_by(|a, b| b.score.cmp(&a.score).then(a.label.cmp(&b.label)));
        out.dedup_by(|a, b| a.label == b.label && a.kind == b.kind);
        out.truncate(80);
        out
    }
}

fn column_detail(c: &crate::db::ColumnSchema, r: &ResolvedRelation) -> String {
    let nullable = if c.nullable { "" } else { " NOT NULL" };
    let pk = if c.is_primary_key { " [PK]" } else { "" };
    if c.type_name.is_empty() {
        format!("column of {}{}{}", r.name, nullable, pk)
    } else {
        format!("{} {} {}{}", c.type_name, nullable, r.name, pk)
    }
}

fn tables_match(
    rel: &ResolvedRelation,
    schema: &str,
    name: &str,
    default_schema: &str,
) -> bool {
    let rel_schema = rel
        .schema
        .as_deref()
        .unwrap_or(default_schema)
        .to_ascii_lowercase();
    rel.name.eq_ignore_ascii_case(name) && rel_schema == schema.to_ascii_lowercase()
}

pub fn quote_ident(s: &str, dialect: DbKind) -> String {
    if dialect == DbKind::MsSql {
        return format!("[{}]", s.replace(']', "]]"));
    }
    let q = match dialect {
        DbKind::MySql => '`',
        _ => '"',
    };
    let escaped = s.replace(q, &format!("{0}{0}", q));
    format!("{q}{escaped}{q}")
}

pub fn quote_qualified(parts: &[&str], dialect: DbKind) -> String {
    parts
        .iter()
        .map(|p| quote_ident(p, dialect))
        .collect::<Vec<_>>()
        .join(".")
}

fn kind_bias(k: ItemKind) -> i32 {
    match k {
        ItemKind::JoinFk => 1500,
        ItemKind::Column => 800,
        ItemKind::Alias => 700,
        ItemKind::Cte => 600,
        ItemKind::Table => 500,
        ItemKind::Keyword => 300,
        ItemKind::Function => 200,
        ItemKind::Schema => 100,
    }
}
