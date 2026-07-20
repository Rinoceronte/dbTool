use crate::db::DbKind;

pub const STATEMENT_START: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "CREATE", "ALTER", "DROP", "EXPLAIN", "TRUNCATE",
];

pub const AFTER_SELECT: &[&str] = &["*", "DISTINCT", "ALL"];

pub const BOOLEAN_OPS: &[&str] = &["AND", "OR", "NOT", "IN", "IS NULL", "IS NOT NULL", "BETWEEN", "LIKE", "EXISTS"];

pub const AFTER_FROM_FOLLOW: &[&str] = &[
    "WHERE", "GROUP BY", "ORDER BY", "LIMIT", "HAVING", "JOIN", "LEFT JOIN", "RIGHT JOIN",
    "INNER JOIN", "FULL JOIN", "CROSS JOIN", "UNION", "UNION ALL", "ON",
];

pub fn dialect_keywords(d: DbKind) -> &'static [&'static str] {
    match d {
        DbKind::Postgres => &["ILIKE", "RETURNING", "OFFSET", "FETCH", "LATERAL", "USING"],
        DbKind::MySql => &["LIMIT", "OFFSET", "USING", "STRAIGHT_JOIN"],
        DbKind::MsSql => &["TOP", "OFFSET", "FETCH", "CROSS APPLY", "OUTER APPLY", "PIVOT", "UNPIVOT", "OUTPUT", "MERGE"],
    }
}

/// Check whether an identifier is a SQL keyword (case-insensitive).
pub fn is_keyword(word: &str) -> bool {
    ALL_KEYWORDS
        .iter()
        .any(|k| k.eq_ignore_ascii_case(word))
}

pub const ALL_KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "GROUP", "BY", "HAVING", "ORDER", "LIMIT", "OFFSET", "JOIN", "LEFT",
    "RIGHT", "INNER", "FULL", "CROSS", "OUTER", "ON", "USING", "AS", "AND", "OR", "NOT", "IN",
    "IS", "NULL", "TRUE", "FALSE", "BETWEEN", "LIKE", "ILIKE", "EXISTS", "WITH", "RECURSIVE",
    "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "ALTER", "DROP", "TABLE",
    "VIEW", "INDEX", "CASE", "WHEN", "THEN", "ELSE", "END", "UNION", "INTERSECT", "EXCEPT",
    "ALL", "DISTINCT", "EXPLAIN", "RETURNING", "LATERAL", "TRUNCATE", "PRIMARY", "KEY", "FOREIGN",
    "REFERENCES", "DEFAULT",
];
