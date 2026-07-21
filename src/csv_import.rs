//! CSV → INSERT statement building for "Import from data file".
//!
//! Values are rendered as quoted SQL literals and coerced by the server to the
//! target column types, which behaves the same across Postgres, MySQL and
//! SQL Server for text/number/date/bool columns.

use crate::db::DbKind;

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub delimiter: u8,
    pub has_header: bool,
    /// Treat empty fields as SQL NULL instead of empty strings.
    pub empty_as_null: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self { delimiter: b',', has_header: true, empty_as_null: true }
    }
}

pub fn quote_ident(kind: DbKind, name: &str) -> String {
    match kind {
        DbKind::Postgres | DbKind::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
        DbKind::MySql => format!("`{}`", name.replace('`', "``")),
        DbKind::MsSql => format!("[{}]", name.replace(']', "]]")),
    }
}

pub fn quote_literal(kind: DbKind, s: &str) -> String {
    match kind {
        // MySQL treats backslash as an escape character by default.
        DbKind::MySql => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
        DbKind::Postgres | DbKind::MsSql | DbKind::Sqlite => {
            format!("'{}'", s.replace('\'', "''"))
        }
    }
}

/// Build one multi-row INSERT statement for a batch of rows.
/// `None` fields become NULL; every row must have exactly `columns.len()` fields.
pub fn build_insert(
    kind: DbKind,
    schema: &str,
    table: &str,
    columns: &[String],
    rows: &[Vec<Option<String>>],
) -> String {
    let cols: Vec<String> = columns.iter().map(|c| quote_ident(kind, c)).collect();
    let mut sql = format!(
        "INSERT INTO {}.{} ({}) VALUES\n",
        quote_ident(kind, schema),
        quote_ident(kind, table),
        cols.join(", ")
    );
    let tuples: Vec<String> = rows
        .iter()
        .map(|row| {
            let vals: Vec<String> = row
                .iter()
                .map(|v| match v {
                    None => "NULL".to_string(),
                    Some(s) => quote_literal(kind, s),
                })
                .collect();
            format!("({})", vals.join(", "))
        })
        .collect();
    sql.push_str(&tuples.join(",\n"));
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_escape_quotes() {
        assert_eq!(quote_literal(DbKind::Postgres, "O'Brien"), "'O''Brien'");
        assert_eq!(quote_literal(DbKind::MsSql, "O'Brien"), "'O''Brien'");
        assert_eq!(quote_literal(DbKind::MySql, r"a\'b"), r"'a\\''b'");
    }

    #[test]
    fn idents_escape_per_dialect() {
        assert_eq!(quote_ident(DbKind::Postgres, "we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_ident(DbKind::MySql, "we`ird"), "`we``ird`");
        assert_eq!(quote_ident(DbKind::MsSql, "we]ird"), "[we]]ird]");
    }

    #[test]
    fn builds_multi_row_insert_with_nulls() {
        let sql = build_insert(
            DbKind::Postgres,
            "public",
            "t",
            &["a".into(), "b".into()],
            &[
                vec![Some("1".into()), None],
                vec![Some("x'y".into()), Some("2".into())],
            ],
        );
        assert_eq!(
            sql,
            "INSERT INTO \"public\".\"t\" (\"a\", \"b\") VALUES\n('1', NULL),\n('x''y', '2')"
        );
    }
}
