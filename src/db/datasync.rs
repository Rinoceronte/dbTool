//! Data compare & sync engine.
//!
//! Two-pass design: a cheap server-side checksum pass finds tables whose
//! data differs, then an ordered merge-join over PK-sorted pages produces
//! row-level differences without materializing whole tables. The same
//! machinery powers the sync-script generator and the sanitized
//! prod→sandbox "pull" (DELETE + batched re-INSERT with in-flight masking —
//! raw values never land on the target).

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{DbKind, DynDriver, Value, quote_ident};

/// One table selected for compare/sync, resolved from the source's meta.
#[derive(Debug, Clone)]
pub struct TableSel {
    pub schema: String,
    pub table: String,
    /// Column names, source order.
    pub columns: Vec<String>,
    /// PK column names (order defines the merge sort key). Empty = no PK.
    pub pk: Vec<String>,
}

impl TableSel {
    pub fn key(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }
}

/// Column masking strategy for the sanitized pull.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaskStrategy {
    /// Replace with NULL.
    Null,
    /// Replace with a fixed value (typed like a parameter: NULL/number/text).
    Fixed(String),
    /// Deterministic hash of the original (preserves joins & uniqueness).
    Hash,
    /// Deterministic fake email derived from the original.
    HashEmail,
}

impl MaskStrategy {
    pub fn label(&self) -> &'static str {
        match self {
            MaskStrategy::Null => "NULL",
            MaskStrategy::Fixed(_) => "fixed",
            MaskStrategy::Hash => "hash",
            MaskStrategy::HashEmail => "email",
        }
    }
}

/// Masks keyed by "schema.table.column".
pub type MaskMap = BTreeMap<String, MaskStrategy>;

/// Row-level outcome of comparing one table.
#[derive(Debug, Clone, Default)]
pub struct TableReport {
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub pk: Vec<String>,
    pub source_count: Option<u64>,
    pub target_count: Option<u64>,
    /// Some(true) = checksums matched (data equal), Some(false) = differ,
    /// None = dialect has no checksum (or it failed) — row pass decides.
    pub checksum_equal: Option<bool>,
    /// Rows on source missing from target (full source rows).
    pub missing: Vec<Vec<Value>>,
    /// Rows on target that source doesn't have.
    pub extra: Vec<Vec<Value>>,
    /// (source row, target row) pairs whose non-PK values differ.
    pub changed: Vec<(Vec<Value>, Vec<Value>)>,
    /// The diff row cap cut collection short.
    pub truncated: bool,
    /// Compare skipped: no primary key to align rows on.
    pub no_pk: bool,
    pub error: Option<String>,
}

impl TableReport {
    pub fn in_sync(&self) -> bool {
        self.error.is_none()
            && !self.no_pk
            && self.missing.is_empty()
            && self.extra.is_empty()
            && self.changed.is_empty()
            && !self.truncated
            && self.checksum_equal != Some(false)
    }

    pub fn diff_count(&self) -> usize {
        self.missing.len() + self.extra.len() + self.changed.len()
    }
}

// ---------------------------------------------------------------------------
// Deterministic masking
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit — dependency-free deterministic hash for pseudonymization.
pub fn fnv64(input: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in input.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Parse a fixed-mask input the way parameter values are parsed.
fn typed_fixed(input: &str) -> Value {
    let t = input.trim();
    if t.eq_ignore_ascii_case("null") {
        return Value::Null;
    }
    if let Ok(i) = t.parse::<i64>() {
        return Value::Int(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Value::Float(f);
    }
    match t {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => Value::Text(t.to_string()),
    }
}

/// Apply a mask to one value. NULLs stay NULL (there is nothing to hide and
/// hashing them would break nullability semantics).
pub fn mask_value(strategy: &MaskStrategy, v: &Value) -> Value {
    if matches!(v, Value::Null) && !matches!(strategy, MaskStrategy::Fixed(_)) {
        return Value::Null;
    }
    match strategy {
        MaskStrategy::Null => Value::Null,
        MaskStrategy::Fixed(text) => typed_fixed(text),
        MaskStrategy::Hash => Value::Text(format!("{:016x}", fnv64(&v.display()))),
        MaskStrategy::HashEmail => {
            Value::Text(format!("user_{:012x}@example.test", fnv64(&v.display()) & 0xffff_ffff_ffff))
        }
    }
}

/// Mask a whole row in place, per the "schema.table.column" rules.
pub fn mask_row(sel: &TableSel, masks: &MaskMap, row: &mut [Value]) {
    for (i, col) in sel.columns.iter().enumerate() {
        if let Some(strategy) = masks.get(&format!("{}.{}", sel.key(), col)) {
            if let Some(v) = row.get_mut(i) {
                *v = mask_value(strategy, v);
            }
        }
    }
}

/// Column names that likely hold personal data → suggested strategy.
pub fn suggest_mask(column: &str) -> Option<MaskStrategy> {
    let c = column.to_ascii_lowercase();
    if c.contains("email") {
        return Some(MaskStrategy::HashEmail);
    }
    const HASHED: &[&str] = &[
        "password", "passwd", "secret", "token", "api_key", "apikey", "ssn",
        "social_security", "credit_card", "card_number", "iban", "phone",
        "mobile", "first_name", "last_name", "full_name", "surname",
        "birthdate", "birth_date", "dob", "address", "street", "zip",
        "postal", "salary", "iban", "license",
    ];
    if HASHED.iter().any(|k| c.contains(k)) {
        return Some(MaskStrategy::Hash);
    }
    None
}

// ---------------------------------------------------------------------------
// SQL rendering
// ---------------------------------------------------------------------------

/// Dialect SQL literal for a value (INSERT/UPDATE/DELETE generation).
pub fn value_literal(kind: DbKind, v: &Value) -> String {
    use std::fmt::Write as _;
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Bool(b) => match kind {
            DbKind::MsSql => if *b { "1" } else { "0" }.to_owned(),
            _ => b.to_string(),
        },
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.is_finite() {
                f.to_string()
            } else {
                "NULL".to_owned()
            }
        }
        Value::Text(s) | Value::Timestamp(s) => crate::csv_import::quote_literal(kind, s),
        Value::Json(j) => crate::csv_import::quote_literal(kind, &j.to_string()),
        Value::Bytes(b) => {
            let mut hex = String::with_capacity(b.len() * 2);
            for byte in b {
                let _ = write!(hex, "{byte:02x}");
            }
            match kind {
                DbKind::Postgres => format!("'\\x{hex}'"),
                DbKind::MySql | DbKind::Sqlite => format!("X'{hex}'"),
                DbKind::MsSql => format!("0x{hex}"),
            }
        }
    }
}

fn qtable(kind: DbKind, sel: &TableSel) -> String {
    format!("{}.{}", quote_ident(kind, &sel.schema), quote_ident(kind, &sel.table))
}

/// COUNT + order-independent content checksum, where the dialect can.
/// Returns (sql, has_checksum).
fn checksum_sql(kind: DbKind, sel: &TableSel) -> (String, bool) {
    let tbl = qtable(kind, sel);
    match kind {
        DbKind::Postgres => (
            format!(
                "SELECT count(*)::bigint, \
                        COALESCE(sum(('x' || substr(md5(t::text), 1, 8))::bit(32)::bigint), 0)::text \
                 FROM {tbl} t"
            ),
            true,
        ),
        DbKind::MySql => {
            let concat = sel
                .columns
                .iter()
                .map(|c| format!("IFNULL(CAST({} AS CHAR), '\u{2400}')", quote_ident(kind, c)))
                .collect::<Vec<_>>()
                .join(", ");
            (
                format!(
                    "SELECT COUNT(*), CAST(COALESCE(BIT_XOR(CRC32(CONCAT_WS('|', {concat}))), 0) AS CHAR) \
                     FROM {tbl}"
                ),
                true,
            )
        }
        DbKind::MsSql => (
            format!(
                "SELECT COUNT_BIG(*), CAST(COALESCE(CHECKSUM_AGG(BINARY_CHECKSUM(*)), 0) AS varchar(32)) \
                 FROM {tbl}"
            ),
            true,
        ),
        // SQLite has no built-in hash; counts only, rows decide the rest.
        DbKind::Sqlite => (format!("SELECT count(*), '' FROM {tbl}"), false),
    }
}

/// PK-ordered page of a table's rows.
fn page_sql(kind: DbKind, sel: &TableSel, limit: usize, offset: u64) -> String {
    let cols = sel
        .columns
        .iter()
        .map(|c| quote_ident(kind, c))
        .collect::<Vec<_>>()
        .join(", ");
    let order = sel
        .pk
        .iter()
        .map(|c| quote_ident(kind, c))
        .collect::<Vec<_>>()
        .join(", ");
    let tbl = qtable(kind, sel);
    match kind {
        DbKind::MsSql => format!(
            "SELECT {cols} FROM {tbl} ORDER BY {order} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
        ),
        _ => format!("SELECT {cols} FROM {tbl} ORDER BY {order} LIMIT {limit} OFFSET {offset}"),
    }
}

// ---------------------------------------------------------------------------
// Value ordering (merge join)
// ---------------------------------------------------------------------------

/// Total order over values of the same underlying column type.
fn cmp_value(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => Less,
        (_, Value::Null) => Greater,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Equal),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Equal),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Equal),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => a.display().cmp(&b.display()),
    }
}

fn cmp_key(a: &[Value], b: &[Value], pk_idx: &[usize]) -> std::cmp::Ordering {
    for &i in pk_idx {
        let ord = cmp_value(&a[i], &b[i]);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

fn rows_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| cmp_value(x, y) == std::cmp::Ordering::Equal && matches!(x, Value::Null) == matches!(y, Value::Null))
}

// ---------------------------------------------------------------------------
// Compare
// ---------------------------------------------------------------------------

const PAGE_ROWS: usize = 1000;

/// PK-ordered row pager over one side.
struct Pager<'a> {
    driver: &'a DynDriver,
    kind: DbKind,
    sel: &'a TableSel,
    buf: std::collections::VecDeque<Vec<Value>>,
    offset: u64,
    done: bool,
}

impl<'a> Pager<'a> {
    fn new(driver: &'a DynDriver, sel: &'a TableSel) -> Self {
        Self { driver, kind: driver.kind(), sel, buf: Default::default(), offset: 0, done: false }
    }

    async fn peek(&mut self) -> Result<Option<&Vec<Value>>> {
        if self.buf.is_empty() && !self.done {
            let sql = page_sql(self.kind, self.sel, PAGE_ROWS, self.offset);
            let rs = self.driver.query(&sql).await?;
            self.offset += rs.rows.len() as u64;
            if rs.rows.len() < PAGE_ROWS {
                self.done = true;
            }
            self.buf.extend(rs.rows);
        }
        Ok(self.buf.front())
    }

    async fn next(&mut self) -> Result<Option<Vec<Value>>> {
        self.peek().await?;
        Ok(self.buf.pop_front())
    }
}

async fn count_and_checksum(
    driver: &DynDriver,
    sel: &TableSel,
) -> Result<(u64, Option<String>)> {
    let (sql, has_checksum) = checksum_sql(driver.kind(), sel);
    let rs = driver.query(&sql).await?;
    let row = rs.rows.first().ok_or_else(|| anyhow::anyhow!("empty checksum result"))?;
    let count = match row.first() {
        Some(Value::Int(i)) => *i as u64,
        Some(Value::Text(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Float(f)) => *f as u64,
        _ => 0,
    };
    let checksum = if has_checksum { row.get(1).map(|v| v.display()) } else { None };
    Ok((count, checksum))
}

/// Compare one table across two same-dialect connections. `diff_cap` bounds
/// how many row differences are collected.
pub async fn compare_table(
    source: &DynDriver,
    target: &DynDriver,
    sel: &TableSel,
    diff_cap: usize,
) -> TableReport {
    let mut report = TableReport {
        schema: sel.schema.clone(),
        table: sel.table.clone(),
        columns: sel.columns.clone(),
        pk: sel.pk.clone(),
        ..Default::default()
    };

    let src = match count_and_checksum(source, sel).await {
        Ok(v) => v,
        Err(e) => {
            report.error = Some(format!("source: {e:#}"));
            return report;
        }
    };
    let tgt = match count_and_checksum(target, sel).await {
        Ok(v) => v,
        Err(e) => {
            report.error = Some(format!("target: {e:#}"));
            return report;
        }
    };
    report.source_count = Some(src.0);
    report.target_count = Some(tgt.0);
    if let (Some(a), Some(b)) = (&src.1, &tgt.1) {
        let equal = a == b && src.0 == tgt.0;
        report.checksum_equal = Some(equal);
        if equal {
            return report;
        }
    } else if src.0 == 0 && tgt.0 == 0 {
        return report;
    }

    if sel.pk.is_empty() {
        report.no_pk = true;
        return report;
    }
    let pk_idx: Vec<usize> = sel
        .pk
        .iter()
        .filter_map(|p| sel.columns.iter().position(|c| c == p))
        .collect();
    if pk_idx.len() != sel.pk.len() {
        report.no_pk = true;
        return report;
    }

    // Merge join the two PK-ordered streams.
    let mut sp = Pager::new(source, sel);
    let mut tp = Pager::new(target, sel);
    loop {
        if report.diff_count() >= diff_cap {
            report.truncated = true;
            break;
        }
        let step = async {
            let has_s = sp.peek().await?.is_some();
            let has_t = tp.peek().await?.is_some();
            anyhow::Ok(match (has_s, has_t) {
                (false, false) => 0u8,
                (true, false) => 1,
                (false, true) => 2,
                (true, true) => {
                    let ord = cmp_key(
                        sp.peek().await?.unwrap(),
                        tp.peek().await?.unwrap(),
                        &pk_idx,
                    );
                    match ord {
                        std::cmp::Ordering::Less => 1,
                        std::cmp::Ordering::Greater => 2,
                        std::cmp::Ordering::Equal => 3,
                    }
                }
            })
        };
        match step.await {
            Ok(0) => break,
            Ok(1) => {
                if let Ok(Some(row)) = sp.next().await {
                    report.missing.push(row);
                }
            }
            Ok(2) => {
                if let Ok(Some(row)) = tp.next().await {
                    report.extra.push(row);
                }
            }
            Ok(3) => {
                let s = sp.next().await.ok().flatten();
                let t = tp.next().await.ok().flatten();
                if let (Some(s), Some(t)) = (s, t) {
                    if !rows_equal(&s, &t) {
                        report.changed.push((s, t));
                    }
                }
            }
            Ok(_) => unreachable!(),
            Err(e) => {
                report.error = Some(format!("{e:#}"));
                break;
            }
        }
    }
    report
}

// ---------------------------------------------------------------------------
// Sync script
// ---------------------------------------------------------------------------

/// DML that makes the target's data match the source, from a compare report.
/// Masks apply to values written by INSERTs/UPDATEs.
pub fn sync_script(
    kind: DbKind,
    report: &TableReport,
    masks: &MaskMap,
    include_deletes: bool,
) -> String {
    let sel = TableSel {
        schema: report.schema.clone(),
        table: report.table.clone(),
        columns: report.columns.clone(),
        pk: report.pk.clone(),
    };
    let tbl = qtable(kind, &sel);
    let mut out = String::new();
    let pk_idx: Vec<usize> = sel
        .pk
        .iter()
        .filter_map(|p| sel.columns.iter().position(|c| c == p))
        .collect();

    let where_pk = |row: &[Value]| -> String {
        pk_idx
            .iter()
            .map(|&i| {
                let v = &row[i];
                if matches!(v, Value::Null) {
                    format!("{} IS NULL", quote_ident(kind, &sel.columns[i]))
                } else {
                    format!("{} = {}", quote_ident(kind, &sel.columns[i]), value_literal(kind, v))
                }
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    };

    if !report.missing.is_empty() {
        let cols = sel
            .columns
            .iter()
            .map(|c| quote_ident(kind, c))
            .collect::<Vec<_>>()
            .join(", ");
        for row in &report.missing {
            let mut masked = row.clone();
            mask_row(&sel, masks, &mut masked);
            let vals = masked
                .iter()
                .map(|v| value_literal(kind, v))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("INSERT INTO {tbl} ({cols}) VALUES ({vals});\n"));
        }
    }
    for (src, tgt) in &report.changed {
        let mut masked = src.clone();
        mask_row(&sel, masks, &mut masked);
        let sets: Vec<String> = sel
            .columns
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                !pk_idx.contains(i)
                    && !(cmp_value(&masked[*i], &tgt[*i]) == std::cmp::Ordering::Equal
                        && matches!(masked[*i], Value::Null) == matches!(tgt[*i], Value::Null))
            })
            .map(|(i, c)| format!("{} = {}", quote_ident(kind, c), value_literal(kind, &masked[i])))
            .collect();
        if sets.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "UPDATE {tbl} SET {} WHERE {};\n",
            sets.join(", "),
            where_pk(src)
        ));
    }
    if include_deletes {
        for row in &report.extra {
            out.push_str(&format!("DELETE FROM {tbl} WHERE {};\n", where_pk(row)));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Pull (full replace with in-flight masking)
// ---------------------------------------------------------------------------

const INSERT_BATCH: usize = 400;

/// DELETE everything on the target, then stream the source's rows across in
/// batched INSERTs, masking in flight. Returns rows written.
pub async fn pull_table(
    source: &DynDriver,
    target: &DynDriver,
    sel: &TableSel,
    masks: &MaskMap,
    row_limit: Option<u64>,
    progress: &(impl Fn(String) + Sync),
) -> Result<u64> {
    let kind = target.kind();
    let tbl = qtable(kind, sel);
    target.query(&format!("DELETE FROM {tbl}")).await?;

    let cols = sel
        .columns
        .iter()
        .map(|c| quote_ident(kind, c))
        .collect::<Vec<_>>()
        .join(", ");
    // Ordered paging needs a sort key; PK when present, else first column.
    let mut order_sel = sel.clone();
    if order_sel.pk.is_empty() {
        order_sel.pk = vec![sel.columns.first().cloned().unwrap_or_default()];
    }

    let mut written: u64 = 0;
    let mut offset: u64 = 0;
    loop {
        let page = page_sql(source.kind(), &order_sel, PAGE_ROWS, offset);
        let rs = source.query(&page).await?;
        let n = rs.rows.len();
        offset += n as u64;
        let mut rows = rs.rows;
        if let Some(limit) = row_limit {
            if written + rows.len() as u64 > limit {
                rows.truncate((limit - written) as usize);
            }
        }
        for chunk in rows.chunks(INSERT_BATCH) {
            let mut values = Vec::with_capacity(chunk.len());
            for row in chunk {
                let mut masked = row.clone();
                mask_row(sel, masks, &mut masked);
                values.push(format!(
                    "({})",
                    masked
                        .iter()
                        .map(|v| value_literal(kind, v))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if values.is_empty() {
                continue;
            }
            let sql = format!("INSERT INTO {tbl} ({cols}) VALUES {}", values.join(", "));
            target.query(&sql).await?;
            written += chunk.len() as u64;
        }
        progress(format!("{}: {written} row(s)", sel.key()));
        if n < PAGE_ROWS || row_limit.is_some_and(|l| written >= l) {
            break;
        }
    }

    // Postgres: bump sequences past the copied ids so future inserts don't
    // collide. Errors (no sequence) are expected and ignored.
    if kind == DbKind::Postgres {
        for pk in &sel.pk {
            let seq_sql = format!(
                "SELECT setval(s::regclass, GREATEST((SELECT COALESCE(MAX({pk_q}), 1) FROM {tbl}), 1)) \
                 FROM pg_get_serial_sequence('{schema}.{table}', '{pk_e}') s WHERE s IS NOT NULL",
                pk_q = quote_ident(kind, pk),
                schema = sel.schema.replace('\'', "''"),
                table = sel.table.replace('\'', "''"),
                pk_e = pk.replace('\'', "''"),
            );
            let _ = target.query(&seq_sql).await;
        }
    }
    Ok(written)
}

/// Order tables so FK-referenced tables come first (safe insert order).
/// `deps` maps "schema.table" → referenced "schema.table"s.
pub fn topo_order(
    mut tables: Vec<TableSel>,
    deps: &BTreeMap<String, Vec<String>>,
) -> Vec<TableSel> {
    let mut out: Vec<TableSel> = Vec::with_capacity(tables.len());
    let mut placed: std::collections::HashSet<String> = Default::default();
    let selected: std::collections::HashSet<String> =
        tables.iter().map(|t| t.key()).collect();
    let mut progressed = true;
    while progressed && !tables.is_empty() {
        progressed = false;
        let mut i = 0;
        while i < tables.len() {
            let key = tables[i].key();
            let ready = deps
                .get(&key)
                .map(|ds| {
                    ds.iter().all(|d| {
                        d == &key || placed.contains(d) || !selected.contains(d)
                    })
                })
                .unwrap_or(true);
            if ready {
                placed.insert(key);
                out.push(tables.remove(i));
                progressed = true;
            } else {
                i += 1;
            }
        }
    }
    // Cycles: append the rest in given order.
    out.extend(tables);
    out
}

// ---------------------------------------------------------------------------
// Saved per-source-profile config
// ---------------------------------------------------------------------------

/// Masking rules and table selection remembered per SOURCE profile, so
/// "pull down prod" keeps its sanitization set across sessions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSyncConfig {
    pub selected: Vec<String>,
    pub masks: MaskMap,
    pub row_limit: Option<u64>,
    pub include_deletes: bool,
}

fn config_path(profile_id: uuid::Uuid) -> Option<std::path::PathBuf> {
    Some(
        dirs::config_dir()?
            .join("dbTool")
            .join("datasync")
            .join(format!("{profile_id}.json")),
    )
}

pub fn load_config(profile_id: uuid::Uuid) -> Option<SavedSyncConfig> {
    let path = config_path(profile_id)?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_config(profile_id: uuid::Uuid, config: &SavedSyncConfig) {
    let Some(path) = config_path(profile_id) else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_is_deterministic_and_null_safe() {
        let email = Value::Text("thomas@example.com".into());
        let a = mask_value(&MaskStrategy::HashEmail, &email);
        let b = mask_value(&MaskStrategy::HashEmail, &email);
        assert_eq!(a.display(), b.display());
        assert!(a.display().ends_with("@example.test"));
        // NULL stays NULL under hashing strategies.
        assert!(matches!(mask_value(&MaskStrategy::Hash, &Value::Null), Value::Null));
        // Fixed can resurrect NULLs, typed.
        assert!(matches!(mask_value(&MaskStrategy::Fixed("42".into()), &Value::Null), Value::Int(42)));
    }

    #[test]
    fn suggestions_catch_the_usual_suspects() {
        assert_eq!(suggest_mask("primary_email"), Some(MaskStrategy::HashEmail));
        assert!(matches!(suggest_mask("password_hash"), Some(MaskStrategy::Hash)));
        assert!(matches!(suggest_mask("phone_number"), Some(MaskStrategy::Hash)));
        assert_eq!(suggest_mask("created_at"), None);
        assert_eq!(suggest_mask("id"), None);
    }

    #[test]
    fn literals_are_dialect_correct() {
        assert_eq!(value_literal(DbKind::Postgres, &Value::Text("it's".into())), "'it''s'");
        assert_eq!(value_literal(DbKind::MsSql, &Value::Bool(true)), "1");
        assert_eq!(value_literal(DbKind::Postgres, &Value::Bytes(vec![0xab])), "'\\xab'");
        assert_eq!(value_literal(DbKind::MySql, &Value::Bytes(vec![0xab])), "X'ab'");
        assert_eq!(value_literal(DbKind::Postgres, &Value::Null), "NULL");
    }

    #[test]
    fn topo_orders_referenced_tables_first() {
        let t = |name: &str| TableSel {
            schema: "public".into(),
            table: name.into(),
            columns: vec!["id".into()],
            pk: vec!["id".into()],
        };
        let mut deps = BTreeMap::new();
        // order → customer, order_item → order + product
        deps.insert("public.order".to_string(), vec!["public.customer".to_string()]);
        deps.insert(
            "public.order_item".to_string(),
            vec!["public.order".to_string(), "public.product".to_string()],
        );
        let sorted = topo_order(
            vec![t("order_item"), t("order"), t("customer"), t("product")],
            &deps,
        );
        let names: Vec<&str> = sorted.iter().map(|s| s.table.as_str()).collect();
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("customer") < pos("order"));
        assert!(pos("order") < pos("order_item"));
        assert!(pos("product") < pos("order_item"));
    }

    #[test]
    fn sync_script_generates_masked_dml() {
        let report = TableReport {
            schema: "public".into(),
            table: "users".into(),
            columns: vec!["id".into(), "email".into()],
            pk: vec!["id".into()],
            missing: vec![vec![Value::Int(7), Value::Text("x@y.com".into())]],
            extra: vec![vec![Value::Int(9), Value::Text("gone@y.com".into())]],
            changed: vec![(
                vec![Value::Int(1), Value::Text("new@y.com".into())],
                vec![Value::Int(1), Value::Text("old@y.com".into())],
            )],
            ..Default::default()
        };
        let mut masks = MaskMap::new();
        masks.insert("public.users.email".into(), MaskStrategy::HashEmail);
        let script = sync_script(DbKind::Postgres, &report, &masks, true);
        assert!(script.contains("INSERT INTO \"public\".\"users\""));
        assert!(script.contains("@example.test"));
        assert!(!script.contains("x@y.com"), "raw value must not appear");
        assert!(script.contains("DELETE FROM \"public\".\"users\" WHERE \"id\" = 9"));
        assert!(script.contains("UPDATE \"public\".\"users\" SET \"email\" ="));
    }
}
