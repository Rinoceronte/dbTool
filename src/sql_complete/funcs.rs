use crate::db::DbKind;

pub struct FuncSig {
    pub name: &'static str,
    pub signature: &'static str,
}

const COMMON: &[FuncSig] = &[
    FuncSig { name: "COUNT",    signature: "COUNT(expr) → bigint" },
    FuncSig { name: "SUM",      signature: "SUM(expr) → numeric" },
    FuncSig { name: "AVG",      signature: "AVG(expr) → numeric" },
    FuncSig { name: "MIN",      signature: "MIN(expr) → any" },
    FuncSig { name: "MAX",      signature: "MAX(expr) → any" },
    FuncSig { name: "COALESCE", signature: "COALESCE(a, b, …) → any" },
    FuncSig { name: "NULLIF",   signature: "NULLIF(a, b) → any" },
    FuncSig { name: "CASE",     signature: "CASE WHEN cond THEN x ELSE y END" },
    FuncSig { name: "CAST",     signature: "CAST(expr AS type)" },
    FuncSig { name: "LOWER",    signature: "LOWER(text) → text" },
    FuncSig { name: "UPPER",    signature: "UPPER(text) → text" },
    FuncSig { name: "LENGTH",   signature: "LENGTH(text) → integer" },
    FuncSig { name: "TRIM",     signature: "TRIM(text) → text" },
    FuncSig { name: "SUBSTRING",signature: "SUBSTRING(text FROM n FOR m) → text" },
    FuncSig { name: "CONCAT",   signature: "CONCAT(a, b, …) → text" },
    FuncSig { name: "ABS",      signature: "ABS(n) → numeric" },
    FuncSig { name: "ROUND",    signature: "ROUND(n, d?) → numeric" },
    FuncSig { name: "NOW",      signature: "NOW() → timestamp" },
    FuncSig { name: "CURRENT_DATE",      signature: "CURRENT_DATE → date" },
    FuncSig { name: "CURRENT_TIMESTAMP", signature: "CURRENT_TIMESTAMP → timestamp" },
];

const POSTGRES: &[FuncSig] = &[
    FuncSig { name: "GENERATE_SERIES", signature: "GENERATE_SERIES(start, stop, step?) → setof" },
    FuncSig { name: "ARRAY_AGG",       signature: "ARRAY_AGG(expr) → array" },
    FuncSig { name: "STRING_AGG",      signature: "STRING_AGG(expr, sep) → text" },
    FuncSig { name: "TO_CHAR",         signature: "TO_CHAR(val, fmt) → text" },
    FuncSig { name: "TO_DATE",         signature: "TO_DATE(text, fmt) → date" },
    FuncSig { name: "DATE_TRUNC",      signature: "DATE_TRUNC(unit, ts) → timestamp" },
    FuncSig { name: "EXTRACT",         signature: "EXTRACT(unit FROM ts) → numeric" },
];

const MYSQL: &[FuncSig] = &[
    FuncSig { name: "IFNULL",   signature: "IFNULL(a, b) → any" },
    FuncSig { name: "IF",       signature: "IF(cond, x, y) → any" },
    FuncSig { name: "GROUP_CONCAT", signature: "GROUP_CONCAT(expr) → text" },
    FuncSig { name: "DATE_FORMAT",  signature: "DATE_FORMAT(d, fmt) → text" },
    FuncSig { name: "STR_TO_DATE",  signature: "STR_TO_DATE(s, fmt) → date" },
    FuncSig { name: "UNIX_TIMESTAMP", signature: "UNIX_TIMESTAMP(d?) → bigint" },
];

const SQLITE: &[FuncSig] = &[
    FuncSig { name: "IFNULL",        signature: "IFNULL(a, b) → any" },
    FuncSig { name: "IIF",           signature: "IIF(cond, x, y) → any" },
    FuncSig { name: "GROUP_CONCAT",  signature: "GROUP_CONCAT(expr, sep?) → text" },
    FuncSig { name: "STRFTIME",      signature: "STRFTIME(fmt, ts, mod…) → text" },
    FuncSig { name: "DATETIME",      signature: "DATETIME(ts, mod…) → text" },
    FuncSig { name: "DATE",          signature: "DATE(ts, mod…) → text" },
    FuncSig { name: "JSON_EXTRACT",  signature: "JSON_EXTRACT(json, path) → any" },
    FuncSig { name: "TYPEOF",        signature: "TYPEOF(expr) → text" },
    FuncSig { name: "RANDOM",        signature: "RANDOM() → integer" },
    FuncSig { name: "LAST_INSERT_ROWID", signature: "LAST_INSERT_ROWID() → integer" },
];

const MSSQL: &[FuncSig] = &[
    FuncSig { name: "ISNULL",     signature: "ISNULL(a, b) → any" },
    FuncSig { name: "IIF",        signature: "IIF(cond, x, y) → any" },
    FuncSig { name: "LEN",        signature: "LEN(text) → integer" },
    FuncSig { name: "CHARINDEX",  signature: "CHARINDEX(find, text, start?) → integer" },
    FuncSig { name: "GETDATE",    signature: "GETDATE() → datetime" },
    FuncSig { name: "DATEADD",    signature: "DATEADD(unit, n, date) → datetime" },
    FuncSig { name: "DATEDIFF",   signature: "DATEDIFF(unit, start, end) → integer" },
    FuncSig { name: "FORMAT",     signature: "FORMAT(val, fmt) → text" },
    FuncSig { name: "CONVERT",    signature: "CONVERT(type, expr, style?) → any" },
    FuncSig { name: "STRING_AGG", signature: "STRING_AGG(expr, sep) → text" },
];

pub fn functions(dialect: DbKind) -> impl Iterator<Item = &'static FuncSig> {
    let extra = match dialect {
        DbKind::Postgres => POSTGRES,
        DbKind::MySql => MYSQL,
        DbKind::MsSql => MSSQL,
        DbKind::Sqlite => SQLITE,
    };
    COMMON.iter().chain(extra.iter())
}
