pub mod compare;
pub mod datasync;
pub mod mssql;
pub mod mysql;
pub mod plan;
pub mod postgres;
pub mod sqlite;
pub mod structure;
pub mod types;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub use types::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbKind {
    Postgres,
    MySql,
    MsSql,
    Sqlite,
}

impl DbKind {
    pub fn display(&self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::MySql => "MySQL / MariaDB",
            DbKind::MsSql => "Microsoft SQL Server",
            DbKind::Sqlite => "SQLite",
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            DbKind::Postgres => 5432,
            DbKind::MySql => 3306,
            DbKind::MsSql => 1433,
            DbKind::Sqlite => 0,
        }
    }

    /// File-based engines have no host/port/credentials; the profile's
    /// `database` field is the file path.
    pub fn is_file_based(&self) -> bool {
        matches!(self, DbKind::Sqlite)
    }
}

#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DbKind;
    async fn list_schemas(&self) -> Result<Vec<String>>;
    /// Databases/catalogs visible on the server. Empty where the concept
    /// doesn't add anything (MySQL: databases ARE the listed schemas).
    async fn list_databases(&self) -> Result<Vec<String>>;
    async fn list_tables(&self, schema: &str) -> Result<Vec<TableInfo>>;
    /// Non-table objects (functions, sequences, enums, triggers) of a schema.
    async fn list_schema_objects(&self, schema: &str) -> Result<SchemaObjects>;
    /// Source of a function/procedure, for viewing in a query tab.
    async fn routine_ddl(&self, schema: &str, name: &str, kind: &str) -> Result<String>;
    /// Table comment plus per-column comments (Comments dialog).
    async fn table_comments(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<(Option<String>, Vec<(String, Option<String>)>)>;
    async fn describe_table(&self, schema: &str, table: &str) -> Result<TableSchema>;
    /// Cheap liveness probe for the keepalive loop.
    async fn ping(&self) -> Result<()> {
        self.query("SELECT 1").await.map(|_| ())
    }
    async fn query(&self, sql: &str) -> Result<ResultSet>;
    /// The editor query path: cancellable mid-flight, returning EVERY result
    /// set the script produces (shown as selectable results in the UI).
    /// Drivers that can should cancel SERVER-side (pg_cancel_backend /
    /// KILL QUERY) — merely dropping the client future leaves the statement
    /// running on the server. This default only stops waiting and returns a
    /// single set.
    async fn query_script(
        &self,
        sql: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Vec<ResultSet>> {
        tokio::select! {
            r = self.query(sql) => r.map(|rs| vec![rs]),
            _ = cancel.cancelled() => Err(anyhow::anyhow!(
                "Query cancelled (the server may still finish the statement)"
            )),
        }
    }
    async fn fetch_table_rows(
        &self,
        schema: &str,
        table: &str,
        limit: i64,
        offset: i64,
        filter: &RowsFilter,
    ) -> Result<ResultSet>;
    async fn update_row(
        &self,
        schema: &str,
        table: &str,
        pk: &PkValues,
        changes: &RowChanges,
    ) -> Result<()>;
    async fn insert_row(
        &self,
        schema: &str,
        table: &str,
        values: &RowChanges,
    ) -> Result<()>;
    async fn delete_row(
        &self,
        schema: &str,
        table: &str,
        pk: &PkValues,
    ) -> Result<()>;
    async fn apply_changes(
        &self,
        schema: &str,
        table: &str,
        updates: &[(PkValues, RowChanges)],
        deletes: &[PkValues],
    ) -> Result<()>;
    async fn fetch_db_meta(&self) -> Result<DbMeta>;
    /// Generate a `CREATE TABLE` / `CREATE VIEW` statement for one object,
    /// suitable for writing to a DDL file.
    async fn table_ddl(&self, schema: &str, table: &str, kind: TableKind) -> Result<String>;
    /// Full structural introspection (columns with defaults/identity, PK,
    /// FKs, secondary indexes, checks) for the Structure editor.
    async fn describe_structure(&self, schema: &str, table: &str) -> Result<structure::TableStructure>;
    /// Execute a DDL batch: transactionally where the dialect supports it
    /// (Postgres, SQL Server), sequentially with stop-on-error otherwise
    /// (MySQL). Returns how many statements actually took effect.
    async fn apply_ddl(&self, statements: &[String]) -> structure::DdlOutcome;
}

pub type DynDriver = Arc<dyn Driver>;

/// Editor queries stop materializing rows past this cap (the UI shows a
/// "truncated" banner with a fetch-all escape hatch). Settings-backed.
static RESULT_ROW_CAP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(5000);

tokio::task_local! {
    /// Per-task cap override — set by the runtime for "fetch all" re-runs.
    pub static RESULT_CAP_OVERRIDE: Option<usize>;
}

pub fn set_result_row_cap(cap: usize) {
    RESULT_ROW_CAP.store(cap.max(50), std::sync::atomic::Ordering::Relaxed);
}

/// The cap the current query task should honor.
pub fn effective_row_cap() -> usize {
    RESULT_CAP_OVERRIDE
        .try_with(|v| *v)
        .ok()
        .flatten()
        .unwrap_or_else(|| RESULT_ROW_CAP.load(std::sync::atomic::Ordering::Relaxed))
}

/// Dialect-correct identifier quoting (case-sensitive names stay intact).
pub fn quote_ident(kind: DbKind, name: &str) -> String {
    match kind {
        DbKind::Postgres | DbKind::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
        DbKind::MySql => format!("`{}`", name.replace('`', "``")),
        DbKind::MsSql => format!("[{}]", name.replace(']', "]]")),
    }
}

/// Statement-boundary scanner: is there more than one statement in `sql`?
/// Tracks single/double/backtick quotes, dollar-quoted strings, `--` and
/// `/* */` comments, so semicolons inside strings don't count.
pub fn is_multi_statement(sql: &str) -> bool {
    split_statements(sql).len() > 1
}

/// Does a fragment contain anything besides whitespace and comments?
fn has_sql_content(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() {
                if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else {
            return true;
        }
    }
    false
}

/// Split a script into individual statements (trailing semicolons stripped,
/// comment-only fragments dropped). Same quoting rules as
/// [`is_multi_statement`].
pub fn split_statements(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' | '"' | '`' => {
                let quote = c;
                cur.push(c);
                i += 1;
                while i < chars.len() {
                    cur.push(chars[i]);
                    if chars[i] == quote {
                        // Doubled quote = escaped quote inside the literal.
                        if chars.get(i + 1) == Some(&quote) {
                            cur.push(quote);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            '$' => {
                // Postgres dollar-quoting: $tag$ ... $tag$
                let mut j = i + 1;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if chars.get(j) == Some(&'$') {
                    let tag: String = chars[i..=j].iter().collect();
                    let rest: String = chars[j + 1..].iter().collect();
                    match rest.find(&tag) {
                        Some(end) => {
                            let body_end = j + 1 + rest[..end].chars().count() + tag.chars().count();
                            cur.extend(&chars[i..body_end]);
                            i = body_end;
                            continue;
                        }
                        None => {
                            cur.extend(&chars[i..]);
                            break;
                        }
                    }
                }
            }
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    cur.push(chars[i]);
                    i += 1;
                }
                continue;
            }
            '/' if next == Some('*') => {
                cur.push_str("/*");
                i += 2;
                while i < chars.len() {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        cur.push_str("*/");
                        i += 2;
                        break;
                    }
                    cur.push(chars[i]);
                    i += 1;
                }
                continue;
            }
            ';' => {
                if has_sql_content(&cur) {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    if has_sql_content(&cur) {
        out.push(cur.trim().to_string());
    }
    out
}

/// Parameter placeholders found in a statement, in order of appearance.
/// Named `:name` params are deduplicated; bare `?` become "?1", "?2", ….
/// Skips strings, comments, dollar-quoted bodies, `::` casts and `:=`.
/// `allow_qmark` should be false for Postgres, where `?`/`?|`/`?&` are
/// JSON operators.
pub fn scan_parameters(sql: &str, allow_qmark: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut qmark = 0usize;
    scan_params_impl(sql, allow_qmark, |p| {
        let name = match p {
            ParamTok::Named(n) => n,
            ParamTok::Positional => {
                qmark += 1;
                format!("?{qmark}")
            }
        };
        if !out.contains(&name) {
            out.push(name);
        }
    });
    out
}

/// Replace parameter placeholders with the literal texts in `values`
/// (already rendered as SQL — quoted/NULL/numeric). Missing names are left
/// untouched.
pub fn substitute_parameters(
    sql: &str,
    values: &std::collections::BTreeMap<String, String>,
    allow_qmark: bool,
) -> String {
    let mut result = String::with_capacity(sql.len());
    let mut qmark = 0usize;
    let mut last = 0usize;
    let mut spans: Vec<(usize, usize, String)> = Vec::new();
    scan_params_spans(sql, allow_qmark, |start, end, p| {
        let name = match p {
            ParamTok::Named(n) => n,
            ParamTok::Positional => {
                qmark += 1;
                format!("?{qmark}")
            }
        };
        if let Some(v) = values.get(&name) {
            spans.push((start, end, v.clone()));
        }
    });
    for (start, end, v) in spans {
        result.push_str(&sql[last..start]);
        result.push_str(&v);
        last = end;
    }
    result.push_str(&sql[last..]);
    result
}

enum ParamTok {
    Named(String),
    Positional,
}

fn scan_params_impl(sql: &str, allow_qmark: bool, mut f: impl FnMut(ParamTok)) {
    scan_params_spans(sql, allow_qmark, |_, _, p| f(p));
}

/// Core scanner: reports each placeholder with its byte span.
fn scan_params_spans(
    sql: &str,
    allow_qmark: bool,
    mut f: impl FnMut(usize, usize, ParamTok),
) {
    let bytes = sql.as_bytes();
    let chars: Vec<(usize, char)> = sql.char_indices().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let (pos, c) = chars[i];
        let next = chars.get(i + 1).map(|&(_, c)| c);
        match c {
            '\'' | '"' | '`' | '[' => {
                // Quoted literal / identifier (or bracketed segment) — skip
                // to the closing delimiter so its contents can't look like
                // parameters.
                if c == '[' {
                    // MSSQL bracket ident; no escaping beyond ]].
                    i += 1;
                    while i < chars.len() {
                        if chars[i].1 == ']' {
                            if chars.get(i + 1).map(|&(_, c)| c) == Some(']') {
                                i += 2;
                                continue;
                            }
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                let quote = c;
                i += 1;
                while i < chars.len() {
                    if chars[i].1 == quote {
                        if chars.get(i + 1).map(|&(_, c)| c) == Some(quote) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i].1 != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                i += 2;
                while i < chars.len() {
                    if chars[i].1 == '*' && chars.get(i + 1).map(|&(_, c)| c) == Some('/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            '$' => {
                // Dollar-quoted body: skip whole.
                let mut j = i + 1;
                while j < chars.len() && (chars[j].1.is_alphanumeric() || chars[j].1 == '_') {
                    j += 1;
                }
                if chars.get(j).map(|&(_, c)| c) == Some('$') {
                    let tag: String = chars[i..=j].iter().map(|&(_, c)| c).collect();
                    let rest_start = chars[j].0 + 1;
                    match sql[rest_start..].find(&tag) {
                        Some(off) => {
                            let end_byte = rest_start + off + tag.len();
                            while i < chars.len() && chars[i].0 < end_byte {
                                i += 1;
                            }
                            continue;
                        }
                        None => return,
                    }
                }
                i += 1;
            }
            ':' => {
                // '::' cast, ':=' assignment → skip both chars.
                if matches!(next, Some(':') | Some('=')) {
                    i += 2;
                    continue;
                }
                // Only ident-start characters begin a named parameter.
                if matches!(next, Some(c2) if c2.is_alphabetic() || c2 == '_') {
                    let start = pos;
                    let mut j = i + 1;
                    let mut name = String::new();
                    while j < chars.len()
                        && (chars[j].1.is_alphanumeric() || chars[j].1 == '_')
                    {
                        name.push(chars[j].1);
                        j += 1;
                    }
                    let end = chars.get(j).map(|&(p, _)| p).unwrap_or(bytes.len());
                    f(start, end, ParamTok::Named(name));
                    i = j;
                    continue;
                }
                i += 1;
            }
            '?' if allow_qmark => {
                f(pos, pos + 1, ParamTok::Positional);
                i += 1;
            }
            _ => i += 1,
        }
    }
}

/// Render a user-typed parameter value as a SQL literal: NULL stays NULL,
/// numbers and booleans go bare, everything else is single-quoted.
pub fn parameter_literal(input: &str) -> String {
    let t = input.trim();
    if t.eq_ignore_ascii_case("null") {
        return "NULL".to_owned();
    }
    if t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false") {
        return t.to_ascii_lowercase();
    }
    if t.parse::<f64>().is_ok() {
        return t.to_owned();
    }
    format!("'{}'", t.replace('\'', "''"))
}

/// Reduce a script's result sets to one for callers that only want the
/// "main" result: the last data-bearing set, else the summary.
pub fn collapse_sets(sets: Vec<ResultSet>) -> ResultSet {
    let mut summary = None;
    let mut last_data = None;
    for s in sets {
        if s.columns.is_empty() {
            summary = Some(s);
        } else {
            last_data = Some(s);
        }
    }
    last_data.or(summary).unwrap_or(ResultSet {
        columns: vec![],
        rows: vec![],
        rows_affected: None,
        truncated: false,
    })
}

/// Keywords of a statement, lowercased, in order — strings and comments
/// skipped. Used by the read-only classifier.
fn statement_keywords(stmt: &str) -> Vec<String> {
    let chars: Vec<char> = stmt.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' || c == '`' {
            let quote = c;
            i += 1;
            while i < chars.len() {
                if chars[i] == quote {
                    if chars.get(i + 1) == Some(&quote) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() {
                if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else if c.is_alphabetic() || c == '_' {
            let mut w = String::new();
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                w.push(chars[i].to_ascii_lowercase());
                i += 1;
            }
            out.push(w);
        } else {
            i += 1;
        }
    }
    out
}

/// Lexical tokens for the single-select detector.
enum SelTok {
    /// Bare word, lowercased (keywords compare against this).
    Word(String),
    /// Quoted identifier, unescaped, case preserved.
    Quoted(String),
    Punct(char),
}

fn select_tokens(stmt: &str) -> Vec<SelTok> {
    let chars: Vec<char> = stmt.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        if chars.get(i + 1) == Some(&'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '"' | '`' => {
                let quote = c;
                let mut name = String::new();
                i += 1;
                while i < chars.len() {
                    if chars[i] == quote {
                        if chars.get(i + 1) == Some(&quote) {
                            name.push(quote);
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    name.push(chars[i]);
                    i += 1;
                }
                out.push(SelTok::Quoted(name));
            }
            '[' => {
                let mut name = String::new();
                i += 1;
                while i < chars.len() {
                    if chars[i] == ']' {
                        if chars.get(i + 1) == Some(&']') {
                            name.push(']');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    name.push(chars[i]);
                    i += 1;
                }
                out.push(SelTok::Quoted(name));
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut w = String::new();
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    w.push(chars[i]);
                    i += 1;
                }
                out.push(SelTok::Word(w));
            }
            '(' | ')' | ',' | '.' | '*' => {
                out.push(SelTok::Punct(c));
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// If `sql` is a single plain SELECT from exactly one table (no joins,
/// grouping, distinct or unions at the top level), return its
/// (schema, table). Powers editable query results.
pub fn single_select_target(sql: &str) -> Option<(Option<String>, String)> {
    let stmts = split_statements(sql);
    let [stmt] = &stmts[..] else { return None };
    let toks = select_tokens(stmt);
    match toks.first() {
        Some(SelTok::Word(w)) if w.eq_ignore_ascii_case("select") => {}
        _ => return None,
    }
    // Scan top-level (paren depth 0) for disqualifiers and locate FROM.
    let mut depth = 0i32;
    let mut from_idx = None;
    for (i, t) in toks.iter().enumerate() {
        match t {
            SelTok::Punct('(') => depth += 1,
            SelTok::Punct(')') => depth -= 1,
            SelTok::Word(w) if depth == 0 => {
                let lw = w.to_ascii_lowercase();
                match lw.as_str() {
                    "join" | "union" | "intersect" | "except" | "group" | "having"
                    | "distinct" | "into" => return None,
                    "from" if from_idx.is_none() => from_idx = Some(i),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let fi = from_idx?;
    // Read `ident (. ident)*` after FROM.
    let mut parts: Vec<String> = Vec::new();
    let mut j = fi + 1;
    loop {
        match toks.get(j) {
            Some(SelTok::Word(w)) => parts.push(w.clone()),
            Some(SelTok::Quoted(q)) => parts.push(q.clone()),
            _ => return None,
        }
        j += 1;
        if matches!(toks.get(j), Some(SelTok::Punct('.'))) {
            j += 1;
            continue;
        }
        break;
    }
    // A comma right after the target means an implicit join.
    if matches!(toks.get(j), Some(SelTok::Punct(','))) {
        return None;
    }
    match parts.len() {
        1 => Some((None, parts.pop().unwrap())),
        2 => {
            let table = parts.pop().unwrap();
            Some((Some(parts.pop().unwrap()), table))
        }
        _ => None,
    }
}

/// Is the script one CREATE FUNCTION/PROCEDURE/TRIGGER/EVENT statement?
/// Their bodies contain semicolons, so statement-splitting must not apply
/// (MySQL runs these whole; Postgres $$-quoting already protects them).
pub fn is_routine_ddl(sql: &str) -> bool {
    let kws = statement_keywords(sql);
    if kws.first().map(String::as_str) != Some("create") {
        return false;
    }
    kws.iter()
        .take(6)
        .any(|k| matches!(k.as_str(), "function" | "procedure" | "trigger" | "event"))
}

/// Lexical read-only guard: the first write-capable keyword found in the
/// script, if any. First-keyword allowlist per statement; `WITH`/`EXPLAIN`
/// statements (which can wrap writes) are scanned in full. Conservative by
/// design — a read-only connection would rather refuse an exotic read than
/// permit a write.
pub fn write_statement_keyword(sql: &str) -> Option<String> {
    const READ_STARTERS: &[&str] = &[
        "select", "show", "describe", "desc", "table", "values", "use", "set",
        "begin", "start", "commit", "rollback", "checkpoint", "pragma",
    ];
    const WRITES: &[&str] = &[
        "insert", "update", "delete", "merge", "truncate", "drop", "alter",
        "create", "grant", "revoke", "vacuum", "reindex", "copy", "call",
        "exec", "execute", "replace", "rename", "comment", "kill", "load",
    ];
    for stmt in split_statements(sql) {
        let kws = statement_keywords(&stmt);
        let Some(first) = kws.first() else { continue };
        if READ_STARTERS.contains(&first.as_str()) {
            continue;
        }
        if first == "with" || first == "explain" || first == "analyze" {
            // These can wrap a write (CTE writes, EXPLAIN ANALYZE UPDATE …).
            if let Some(w) = kws.iter().find(|k| WRITES.contains(&k.as_str())) {
                return Some(w.to_uppercase());
            }
            continue;
        }
        return Some(first.to_uppercase());
    }
    None
}

#[derive(Debug, Clone)]
pub struct SshTunnelParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Optional identity file; empty = ssh defaults / agent.
    pub key_path: String,
}

#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    pub require_ssl: bool,
    /// When set, the database is reached through a local `ssh -L` forward.
    pub ssh: Option<SshTunnelParams>,
}

impl ConnectParams {
    pub fn to_url(&self) -> String {
        if self.kind == DbKind::Sqlite {
            // `database` is the file path; created if missing (mode=rwc).
            return format!("sqlite://{}?mode=rwc", self.database);
        }
        let scheme = match self.kind {
            DbKind::Postgres => "postgres",
            DbKind::MySql => "mysql",
            DbKind::MsSql => "mssql",
            DbKind::Sqlite => unreachable!(),
        };
        let user = urlencoding::encode(&self.username);
        let pw = urlencoding::encode(&self.password);
        let db = urlencoding::encode(&self.database);
        let mut url = format!(
            "{scheme}://{user}:{pw}@{host}:{port}/{db}",
            host = self.host,
            port = self.port,
        );
        if self.require_ssl {
            let sep = if url.contains('?') { '&' } else { '?' };
            match self.kind {
                DbKind::Postgres => url.push_str(&format!("{sep}sslmode=require")),
                DbKind::MySql => url.push_str(&format!("{sep}ssl-mode=REQUIRED")),
                DbKind::MsSql => url.push_str(&format!("{sep}encrypt=true")),
                DbKind::Sqlite => {}
            }
        }
        url
    }
}

/// Connect, opening an SSH tunnel first when the profile asks for one.
/// The returned child is the tunnel process; dropping it closes the tunnel
/// (`kill_on_drop`), so it must be kept alive alongside the driver.
pub async fn connect(
    params: &ConnectParams,
) -> Result<(DynDriver, Option<tokio::process::Child>)> {
    let mut effective = params.clone();
    let tunnel = match &params.ssh {
        Some(ssh) => {
            let (child, local_port) = open_ssh_tunnel(ssh, &params.host, params.port).await?;
            effective.host = "127.0.0.1".to_owned();
            effective.port = local_port;
            Some(child)
        }
        None => None,
    };
    let driver: DynDriver = match effective.kind {
        DbKind::Postgres => Arc::new(postgres::PostgresDriver::connect(&effective).await?),
        DbKind::MySql => Arc::new(mysql::MySqlDriver::connect(&effective).await?),
        DbKind::MsSql => Arc::new(mssql::MsSqlDriver::connect(&effective).await?),
        DbKind::Sqlite => Arc::new(sqlite::SqliteDriver::connect(&effective).await?),
    };
    Ok((driver, tunnel))
}

/// Spawn `ssh -N -L <free_port>:<db_host>:<db_port>` and wait until the local
/// end accepts connections. Uses the system ssh so keys, agents and
/// ~/.ssh/config all work as usual.
pub async fn open_ssh_tunnel(
    ssh: &SshTunnelParams,
    db_host: &str,
    db_port: u16,
) -> Result<(tokio::process::Child, u16)> {
    use anyhow::Context as _;

    // Grab a free local port; a tiny race against other processes, but fine.
    let local_port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .context("finding a free local port for the SSH tunnel")?;

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.arg("-N")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-L")
        .arg(format!("127.0.0.1:{local_port}:{db_host}:{db_port}"))
        .arg("-p")
        .arg(ssh.port.to_string());
    if !ssh.key_path.trim().is_empty() {
        cmd.arg("-i").arg(ssh.key_path.trim());
    }
    let target = if ssh.user.trim().is_empty() {
        ssh.host.trim().to_owned()
    } else {
        format!("{}@{}", ssh.user.trim(), ssh.host.trim())
    };
    cmd.arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("spawning ssh (is it installed?)")?;

    // Wait for the forward to come up: either the local port accepts, or ssh
    // exits (auth failure etc. — report its stderr).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                use tokio::io::AsyncReadExt as _;
                let _ = stderr.read_to_string(&mut err).await;
            }
            anyhow::bail!(
                "ssh tunnel exited ({status}): {}",
                if err.trim().is_empty() { "no error output" } else { err.trim() }
            );
        }
        match tokio::net::TcpStream::connect(("127.0.0.1", local_port)).await {
            Ok(_) => break,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => {
                let _ = child.kill().await;
                anyhow::bail!("ssh tunnel did not come up within 15s: {e}");
            }
        }
    }
    Ok((child, local_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_statement_is_not_multi() {
        assert!(!is_multi_statement("SELECT 1"));
        assert!(!is_multi_statement("SELECT 1;"));
        assert!(!is_multi_statement("SELECT 1;\n-- trailing comment"));
    }

    #[test]
    fn semicolons_in_strings_and_comments_do_not_split() {
        assert!(!is_multi_statement("SELECT 'a;b'"));
        assert!(!is_multi_statement("SELECT 1 -- not; two\n"));
        assert!(!is_multi_statement("SELECT 1 /* not; two */"));
        assert!(!is_multi_statement("SELECT \"col;on\" FROM t"));
    }

    #[test]
    fn scripts_split_correctly() {
        let stmts = split_statements("BEGIN; UPDATE t SET x = 1; COMMIT;");
        assert_eq!(stmts, vec!["BEGIN", "UPDATE t SET x = 1", "COMMIT"]);
    }

    #[test]
    fn dollar_quoted_bodies_stay_whole() {
        let sql = "CREATE FUNCTION f() RETURNS void AS $$ BEGIN; SELECT 1; END $$ LANGUAGE plpgsql";
        assert!(!is_multi_statement(sql));
        let two = format!("{sql}; SELECT 2");
        assert_eq!(split_statements(&two).len(), 2);
    }

    #[test]
    fn escaped_quotes_inside_literals() {
        assert!(!is_multi_statement("SELECT 'it''s; fine'"));
        assert!(is_multi_statement("SELECT 'a'; SELECT 'b'"));
    }

    #[test]
    fn single_select_target_detection() {
        assert_eq!(
            single_select_target("SELECT * FROM people"),
            Some((None, "people".into()))
        );
        assert_eq!(
            single_select_target("SELECT id, name FROM public.people WHERE age > 21 ORDER BY id"),
            Some((Some("public".into()), "people".into()))
        );
        assert_eq!(
            single_select_target("select * from \"Order Items\" limit 5"),
            Some((None, "Order Items".into()))
        );
        // Subquery in WHERE is fine; the outer target still wins.
        assert_eq!(
            single_select_target(
                "SELECT * FROM t WHERE id IN (SELECT tid FROM other JOIN x ON 1=1)"
            ),
            Some((None, "t".into()))
        );
        // Disqualified shapes.
        assert_eq!(single_select_target("SELECT * FROM a JOIN b ON a.id = b.id"), None);
        assert_eq!(single_select_target("SELECT * FROM a, b"), None);
        assert_eq!(single_select_target("SELECT DISTINCT x FROM t"), None);
        assert_eq!(single_select_target("SELECT x FROM t GROUP BY x"), None);
        assert_eq!(single_select_target("SELECT 1; SELECT * FROM t"), None);
        assert_eq!(single_select_target("UPDATE t SET x = 1"), None);
    }

    #[test]
    fn parameter_scanning() {
        assert_eq!(
            scan_parameters("SELECT * FROM t WHERE id = :id AND name = :name", false),
            vec!["id", "name"]
        );
        // Duplicates collapse; casts and assignments are not parameters.
        assert_eq!(
            scan_parameters("SELECT :a::int, :a, x := 1", false),
            vec!["a"]
        );
        // Strings, comments and dollar bodies are opaque.
        assert_eq!(
            scan_parameters("SELECT ':no' -- :no\n /* :no */ , $$ :no $$, :yes", false),
            vec!["yes"]
        );
        // Question marks only when the dialect allows them.
        assert_eq!(scan_parameters("SELECT ? , ?", true), vec!["?1", "?2"]);
        assert_eq!(scan_parameters("SELECT '{}'::jsonb ? 'k'", false), Vec::<String>::new());
    }

    #[test]
    fn parameter_substitution() {
        let mut vals = std::collections::BTreeMap::new();
        vals.insert("id".to_string(), "42".to_string());
        vals.insert("name".to_string(), "'O''Brien'".to_string());
        assert_eq!(
            substitute_parameters("SELECT * FROM t WHERE id = :id AND name = :name", &vals, false),
            "SELECT * FROM t WHERE id = 42 AND name = 'O''Brien'"
        );
        let mut q = std::collections::BTreeMap::new();
        q.insert("?1".to_string(), "1".to_string());
        q.insert("?2".to_string(), "2".to_string());
        assert_eq!(substitute_parameters("SELECT ?, ?", &q, true), "SELECT 1, 2");
    }

    #[test]
    fn parameter_literals() {
        assert_eq!(parameter_literal("null"), "NULL");
        assert_eq!(parameter_literal("42"), "42");
        assert_eq!(parameter_literal("4.5"), "4.5");
        assert_eq!(parameter_literal("true"), "true");
        assert_eq!(parameter_literal("it's"), "'it''s'");
    }

    #[test]
    fn read_only_classifier_permits_reads() {
        assert_eq!(write_statement_keyword("SELECT * FROM t"), None);
        assert_eq!(write_statement_keyword("EXPLAIN SELECT 1"), None);
        assert_eq!(write_statement_keyword("SHOW TABLES; SELECT 2"), None);
        // Write keywords inside strings/comments don't count.
        assert_eq!(write_statement_keyword("SELECT 'drop table users'"), None);
        assert_eq!(write_statement_keyword("SELECT 1 -- update later\n"), None);
        // A column merely *named* update is a false positive we accept only
        // in WITH/EXPLAIN scans; as a plain select it is fine.
        assert_eq!(write_statement_keyword("SELECT updated_at FROM t"), None);
    }

    #[test]
    fn read_only_classifier_refuses_writes() {
        assert_eq!(
            write_statement_keyword("UPDATE t SET x = 1").as_deref(),
            Some("UPDATE")
        );
        assert_eq!(
            write_statement_keyword("SELECT 1; DROP TABLE t").as_deref(),
            Some("DROP")
        );
        assert_eq!(
            write_statement_keyword("WITH d AS (SELECT 1) DELETE FROM t").as_deref(),
            Some("DELETE")
        );
        assert_eq!(
            write_statement_keyword("EXPLAIN ANALYZE UPDATE t SET x = 1").as_deref(),
            Some("UPDATE")
        );
        assert_eq!(
            write_statement_keyword("BEGIN; INSERT INTO t VALUES (1); COMMIT").as_deref(),
            Some("INSERT")
        );
    }
}
