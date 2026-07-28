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
    /// Replace with the empty string (satisfies NOT NULL, unlike Null).
    Empty,
    /// Replace with a fixed value (typed like a parameter: NULL/number/text).
    Fixed(String),
    /// Deterministic hash of the original (preserves joins & uniqueness).
    Hash,
    /// Deterministic fake email derived from the original.
    HashEmail,
    /// Realistic fake data, deterministically derived from the original
    /// (same source value → same fake, so joins and re-pulls stay stable).
    Fake(FakeKind),
    /// Parse the value as JSON and recursively replace leaves under
    /// sensitive-looking keys (per `suggest_mask`) with deterministic fakes,
    /// keeping the structure and everything else intact.
    JsonScrub,
}

/// What kind of realistic fake to generate for a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FakeKind {
    Name,
    FirstName,
    LastName,
    Email,
    Phone,
    Street,
    City,
    Company,
    Ssn,
    BirthDate,
    Zip,
    Age,
    Int,
    Price,
}

impl FakeKind {
    pub const ALL: [FakeKind; 14] = [
        FakeKind::Name,
        FakeKind::FirstName,
        FakeKind::LastName,
        FakeKind::Email,
        FakeKind::Phone,
        FakeKind::Street,
        FakeKind::City,
        FakeKind::Company,
        FakeKind::Ssn,
        FakeKind::BirthDate,
        FakeKind::Zip,
        FakeKind::Age,
        FakeKind::Int,
        FakeKind::Price,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            FakeKind::Name => "fake name",
            FakeKind::FirstName => "fake first name",
            FakeKind::LastName => "fake last name",
            FakeKind::Email => "fake email",
            FakeKind::Phone => "fake phone",
            FakeKind::Street => "fake street",
            FakeKind::City => "fake city",
            FakeKind::Company => "fake company",
            FakeKind::Ssn => "fake SSN",
            FakeKind::BirthDate => "fake birth date",
            FakeKind::Zip => "fake zip",
            FakeKind::Age => "fake age",
            FakeKind::Int => "fake number",
            FakeKind::Price => "fake price",
        }
    }
}

impl MaskStrategy {
    pub fn label(&self) -> &'static str {
        match self {
            MaskStrategy::Null => "NULL",
            MaskStrategy::Empty => "empty",
            MaskStrategy::Fixed(_) => "fixed",
            MaskStrategy::Hash => "hash",
            MaskStrategy::HashEmail => "hash email",
            MaskStrategy::Fake(k) => k.label(),
            MaskStrategy::JsonScrub => "scrub JSON",
        }
    }
}

/// Deterministic fake: the original value's hash seeds the RNG, so identical
/// inputs always map to the identical fake across tables and pulls. Numeric
/// kinds return typed values so they insert into numeric columns unquoted.
fn fake_value(kind: FakeKind, original: &str) -> Value {
    use fake::Fake;
    use fake::faker::address::en::{BuildingNumber, CityName, StreetName};
    use fake::faker::company::en::CompanyName;
    use fake::faker::name::en::{FirstName, LastName, Name};
    use rand::SeedableRng;

    let seed = fnv64(original);
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    // Count of leading integer digits — keeps fake numbers in the same
    // order of magnitude as the original so distributions stay plausible.
    let int_digits = |cap: u32| -> u32 {
        (original
            .trim()
            .trim_start_matches('-')
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .count() as u32)
            .clamp(1, cap)
    };
    let text = |s: String| Value::Text(s);
    match kind {
        FakeKind::Name => text(Name().fake_with_rng(&mut rng)),
        FakeKind::FirstName => text(FirstName().fake_with_rng(&mut rng)),
        FakeKind::LastName => text(LastName().fake_with_rng(&mut rng)),
        // Wordlists are small; a short hash suffix keeps UNIQUE columns safe.
        FakeKind::Email => {
            let first: String = FirstName().fake_with_rng(&mut rng);
            let last: String = LastName().fake_with_rng(&mut rng);
            text(format!(
                "{}.{}.{:06x}@example.test",
                first.to_lowercase(),
                last.to_lowercase(),
                seed & 0xff_ffff
            ))
        }
        FakeKind::Phone => {
            text(format!("({:03}) 555-{:04}", 200 + seed % 800, (seed >> 16) % 10000))
        }
        FakeKind::Street => {
            let number: String = BuildingNumber().fake_with_rng(&mut rng);
            let street: String = StreetName().fake_with_rng(&mut rng);
            text(format!("{number} {street}"))
        }
        FakeKind::City => text(CityName().fake_with_rng(&mut rng)),
        FakeKind::Company => text(CompanyName().fake_with_rng(&mut rng)),
        // 9xx area numbers are never issued, so these can't hit a real SSN.
        FakeKind::Ssn => text(format!(
            "9{:02}-{:02}-{:04}",
            (seed >> 4) % 100,
            (seed >> 12) % 100,
            (seed >> 20) % 10000
        )),
        // ISO date so it inserts cleanly into DATE-typed columns.
        FakeKind::BirthDate => text(format!(
            "{}-{:02}-{:02}",
            1950 + seed % 50,
            1 + (seed >> 8) % 12,
            1 + (seed >> 16) % 28
        )),
        FakeKind::Zip => text(format!("{:05}", seed % 100_000)),
        FakeKind::Age => Value::Int((18 + seed % 73) as i64),
        FakeKind::Int => {
            let digits = int_digits(12);
            let lo = 10i64.pow(digits - 1);
            let hi = 10i64.pow(digits);
            let sign = if original.trim_start().starts_with('-') { -1 } else { 1 };
            Value::Int(sign * (lo + (seed % (hi - lo) as u64) as i64))
        }
        FakeKind::Price => {
            let digits = int_digits(9);
            let lo = 10u64.pow(digits - 1);
            let hi = 10u64.pow(digits);
            let whole = lo + seed % (hi - lo);
            let cents = (seed >> 24) % 100;
            Value::Float((whole * 100 + cents) as f64 / 100.0)
        }
    }
}

/// Recursively mask a JSON tree in place: leaves under sensitive-looking
/// keys get the matching deterministic fake; JSON nulls and everything
/// under unsuspicious keys stay as-is.
fn scrub_json(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if val.is_null() {
                    continue;
                }
                match suggest_mask(key) {
                    Some(strategy) => {
                        let original = match &*val {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        let masked = mask_value(&strategy, &Value::Text(original));
                        *val = match masked {
                            Value::Int(i) => serde_json::Value::from(i),
                            Value::Float(f) => serde_json::json!(f),
                            other => serde_json::Value::String(other.display()),
                        };
                    }
                    None => scrub_json(val),
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(scrub_json),
        _ => {}
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
        MaskStrategy::Empty => Value::Text(String::new()),
        MaskStrategy::Fixed(text) => typed_fixed(text),
        MaskStrategy::Hash => Value::Text(format!("{:016x}", fnv64(&v.display()))),
        MaskStrategy::HashEmail => {
            Value::Text(format!("user_{:012x}@example.test", fnv64(&v.display()) & 0xffff_ffff_ffff))
        }
        MaskStrategy::Fake(kind) => fake_value(*kind, &v.display()),
        MaskStrategy::JsonScrub => {
            let parsed = match v {
                Value::Json(j) => Some(j.clone()),
                Value::Text(s) => serde_json::from_str(s).ok(),
                _ => None,
            };
            match parsed {
                Some(mut j) => {
                    scrub_json(&mut j);
                    Value::Json(j)
                }
                // Not valid JSON: never pass the original through unmasked.
                None => Value::Json(serde_json::Value::String("<masked>".into())),
            }
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
/// Recognizable shapes (names, emails, phones, addresses) get realistic
/// fakes; secrets and identifiers fall back to the opaque hash.
pub fn suggest_mask(column: &str) -> Option<MaskStrategy> {
    // Squash separators so snake_case, camelCase and kebab-case all match
    // the same keywords ("first_name", "firstName", "first-name" → "firstname").
    let c: String = column
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| *ch != '_' && *ch != '-')
        .collect();
    if c.contains("email") {
        return Some(MaskStrategy::Fake(FakeKind::Email));
    }
    const FAKED: &[(&str, FakeKind)] = &[
        ("firstname", FakeKind::FirstName),
        ("lastname", FakeKind::LastName),
        ("surname", FakeKind::LastName),
        ("fullname", FakeKind::Name),
        ("phone", FakeKind::Phone),
        ("mobile", FakeKind::Phone),
        ("address", FakeKind::Street),
        ("street", FakeKind::Street),
        ("city", FakeKind::City),
        ("company", FakeKind::Company),
        ("employer", FakeKind::Company),
        ("ssn", FakeKind::Ssn),
        ("socialsecurity", FakeKind::Ssn),
        ("birthdate", FakeKind::BirthDate),
        ("dob", FakeKind::BirthDate),
        ("zip", FakeKind::Zip),
        ("postal", FakeKind::Zip),
        ("price", FakeKind::Price),
        ("cost", FakeKind::Price),
        ("amount", FakeKind::Price),
        ("salary", FakeKind::Price),
        ("balance", FakeKind::Price),
        ("quantity", FakeKind::Int),
        ("qty", FakeKind::Int),
    ];
    if let Some((_, kind)) = FAKED.iter().find(|(k, _)| c.contains(k)) {
        return Some(MaskStrategy::Fake(*kind));
    }
    // Exact match only: "age" as a substring is far too common (page, usage…).
    if c == "age" {
        return Some(MaskStrategy::Fake(FakeKind::Age));
    }
    const HASHED: &[&str] = &[
        "password", "passwd", "secret", "token", "apikey",
        "creditcard", "cardnumber", "iban", "license",
    ];
    if HASHED.iter().any(|k| c.contains(k)) {
        return Some(MaskStrategy::Hash);
    }
    None
}

/// Best fake strategy for a column, inferred from its type and name. The UI
/// offers one "fake" option backed by this; None disables it. Broader than
/// `suggest_mask` (bare "name" columns count here but are too ambiguous to
/// auto-suggest).
pub fn infer_fake(column: &str, type_name: &str) -> Option<MaskStrategy> {
    let ty = type_name.to_ascii_lowercase();
    if ty.contains("json") {
        return Some(MaskStrategy::JsonScrub);
    }
    if let Some(s @ MaskStrategy::Fake(_)) = suggest_mask(column) {
        return Some(s);
    }
    let c = column.to_ascii_lowercase();
    if c == "name" || c.ends_with("_name") || column.ends_with("Name") {
        return Some(MaskStrategy::Fake(FakeKind::Name));
    }
    // Type-based fallback for numeric columns the name says nothing about.
    let inty = ty.contains("int") && !ty.contains("interval") && !ty.contains("point");
    if inty || ty.contains("serial") {
        return Some(MaskStrategy::Fake(FakeKind::Int));
    }
    if ["numeric", "decimal", "double", "float", "real", "money"]
        .iter()
        .any(|k| ty.contains(k))
    {
        return Some(MaskStrategy::Fake(FakeKind::Price));
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

/// PK-ordered page of a table's rows; `filter` is a raw WHERE body in the
/// source dialect.
fn page_sql(
    kind: DbKind,
    sel: &TableSel,
    filter: Option<&str>,
    limit: usize,
    offset: u64,
) -> String {
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
    let where_sql = filter
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .map(|f| format!(" WHERE {f}"))
        .unwrap_or_default();
    match kind {
        DbKind::MsSql => format!(
            "SELECT {cols} FROM {tbl}{where_sql} ORDER BY {order} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
        ),
        _ => format!(
            "SELECT {cols} FROM {tbl}{where_sql} ORDER BY {order} LIMIT {limit} OFFSET {offset}"
        ),
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
            let sql = page_sql(self.kind, self.sel, None, PAGE_ROWS, self.offset);
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
        let page = page_sql(source.kind(), &order_sel, None, PAGE_ROWS, offset);
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

/// What feeds one target column of a transfer: a plain source column, a raw
/// SQL expression evaluated on the source (concat, JSON building, …), or a
/// key-map lookup that translates an old foreign-key value into the id newly
/// generated for the parent row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceExpr {
    Column(String),
    Expr(String),
    /// Read source column `column` and replace its value through the key map
    /// captured for the parent pair whose source table is `table`
    /// ("schema.table").
    KeyLookup { table: String, column: String },
}

impl SourceExpr {
    fn select_sql(&self, kind: DbKind) -> String {
        match self {
            SourceExpr::Column(c) => quote_ident(kind, c),
            SourceExpr::Expr(e) => format!("({})", e.trim()),
            SourceExpr::KeyLookup { column, .. } => quote_ident(kind, column),
        }
    }
}

/// Old→new id maps captured during a multi-table transfer, keyed by the
/// parent pair's source table ("schema.table"). Old keys are normalized to
/// their display string.
pub type KeyMaps = std::collections::HashMap<String, std::collections::HashMap<String, Value>>;

/// Generate the target PK instead of copying it, capturing old→new ids.
#[derive(Debug, Clone)]
pub struct KeyCapture {
    /// Source column holding the old key (e.g. `CustomerID`).
    pub source_column: String,
    /// The target's generated primary-key column (e.g. `id`).
    pub target_pk: String,
}

/// A cross-connection, column-mapped copy: which source columns/expressions
/// feed which target columns. Connections may be different engines — paging
/// runs in the source dialect, INSERT literals render in the target dialect.
#[derive(Debug, Clone)]
pub struct TransferSpec {
    pub source_schema: String,
    pub source_table: String,
    pub target_schema: String,
    pub target_table: String,
    /// (source column/expression, target column), in insert order.
    pub columns: Vec<(SourceExpr, String)>,
    /// Source-side ORDER BY key for stable paging (source PK when known).
    pub order_by: Vec<String>,
    /// Raw WHERE body in the source dialect (no keyword), if any.
    pub filter: Option<String>,
    pub row_limit: Option<u64>,
    /// DELETE the target table's rows before inserting (default appends).
    pub delete_first: bool,
    /// When set, the target generates its PK and old→new ids are captured
    /// into [`KeyMaps`] for children's `KeyLookup` mappings.
    pub key_capture: Option<KeyCapture>,
}

/// Copy mapped columns from one table to another across connections.
/// Crude window-function sniff: the keyword `OVER` followed by `(`. Quoted
/// identifiers can false-positive; that only costs OFFSET-paging speed,
/// never correctness.
fn contains_window_fn(expr: &str) -> bool {
    let l = expr.to_ascii_lowercase();
    let b = l.as_bytes();
    let mut from = 0;
    while let Some(pos) = l[from..].find("over").map(|p| p + from) {
        let boundary_before = pos == 0 || {
            let c = b[pos - 1] as char;
            !c.is_alphanumeric() && c != '_'
        };
        if boundary_before && l[pos + 4..].trim_start().starts_with('(') {
            return true;
        }
        from = pos + 4;
    }
    false
}

/// `key_maps` accumulates old→new ids for [`KeyCapture`] pairs and resolves
/// [`SourceExpr::KeyLookup`] mappings; pass one map across an ordered run.
pub async fn transfer_table(
    source: &DynDriver,
    target: &DynDriver,
    spec: &TransferSpec,
    masks: &MaskMap,
    key_maps: &mut KeyMaps,
    cancel: &tokio_util::sync::CancellationToken,
    progress: &(impl Fn(String) + Sync),
) -> Result<u64> {
    anyhow::ensure!(!spec.columns.is_empty(), "no columns mapped");
    let (skind, tkind) = (source.kind(), target.kind());
    // Mask keys use the plain source column name; expressions key by the
    // target name (the only stable identity they have).
    let mask_sel = TableSel {
        schema: spec.source_schema.clone(),
        table: spec.source_table.clone(),
        columns: spec
            .columns
            .iter()
            .map(|(s, t)| match s {
                SourceExpr::Column(c) | SourceExpr::KeyLookup { column: c, .. } => c.clone(),
                SourceExpr::Expr(_) => t.clone(),
            })
            .collect(),
        pk: Vec::new(),
    };
    let stbl = qtable(skind, &mask_sel);
    // Column positions that translate through a parent's key map.
    let lookups: Vec<(usize, &str)> = spec
        .columns
        .iter()
        .enumerate()
        .filter_map(|(i, (s, _))| match s {
            SourceExpr::KeyLookup { table, .. } => Some((i, table.as_str())),
            _ => None,
        })
        .collect();
    for (_, parent) in &lookups {
        anyhow::ensure!(
            key_maps.contains_key(*parent),
            "key lookup needs {parent} transferred first with \"new ids\" enabled"
        );
    }
    let source_key = format!("{}.{}", spec.source_schema, spec.source_table);
    if spec.key_capture.is_some() {
        // Present even if the table is empty, so children with all-NULL FKs
        // don't fail the "parent transferred?" precheck.
        key_maps.entry(source_key.clone()).or_default();
    }
    let target_sel = TableSel {
        schema: spec.target_schema.clone(),
        table: spec.target_table.clone(),
        columns: spec.columns.iter().map(|(_, t)| t.clone()).collect(),
        pk: Vec::new(),
    };
    let ttbl = qtable(tkind, &target_sel);
    if spec.delete_first {
        target.query(&format!("DELETE FROM {ttbl}")).await?;
    }
    // With key capture the old key rides along as an extra trailing column;
    // a single-column paging order rides along too, for keyset paging.
    let mut select_items: Vec<String> =
        spec.columns.iter().map(|(s, _)| s.select_sql(skind)).collect();
    if let Some(kc) = &spec.key_capture {
        select_items.push(quote_ident(skind, &kc.source_column));
    }
    // Window functions see only the WHERE-filtered set, so a keyset
    // predicate would change their result per page (e.g. ROW_NUMBER
    // restarting) — those tables page by OFFSET, where the window always
    // spans the whole table.
    let windowed = spec
        .columns
        .iter()
        .any(|(s, _)| matches!(s, SourceExpr::Expr(e) if contains_window_fn(e)));
    let keyset_col: Option<String> = match spec.order_by.as_slice() {
        [one] if !windowed => Some(one.clone()),
        _ => None,
    };
    if let Some(c) = &keyset_col {
        select_items.push(quote_ident(skind, c));
    }
    let select_list = select_items.join(", ");
    // Stable paging order: source PK, else the first select item by ordinal.
    // Table-qualified so it binds to the input column even when the same
    // column also appears in the select list (capture/keyset duplicates
    // would otherwise make `ORDER BY col` ambiguous).
    let src_tbl_q = quote_ident(skind, &spec.source_table);
    let order = if spec.order_by.is_empty() {
        "1".to_owned()
    } else {
        spec.order_by
            .iter()
            .map(|c| format!("{src_tbl_q}.{}", quote_ident(skind, c)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let where_sql = spec
        .filter
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|f| format!(" WHERE {f}"))
        .unwrap_or_default();
    let tcols = target_sel
        .columns
        .iter()
        .map(|c| quote_ident(tkind, c))
        .collect::<Vec<_>>()
        .join(", ");

    let mut written: u64 = 0;
    let mut offset: u64 = 0;
    // Keyset paging (WHERE pk > last-seen) stays O(1) per page where OFFSET
    // re-skips every prior row; falls back to OFFSET on NULL keys.
    let mut keyset_active = keyset_col.is_some();
    let mut last_key: Option<Value> = None;
    let ncols = spec.columns.len();
    let key_idx = ncols + usize::from(spec.key_capture.is_some());
    loop {
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "stopped by user after {written} row(s) — copied rows stay in the target"
        );
        let page = if keyset_active {
            let kq = format!(
                "{src_tbl_q}.{}",
                quote_ident(skind, keyset_col.as_deref().unwrap_or_default())
            );
            let mut conds: Vec<String> = Vec::new();
            if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty())
            {
                conds.push(format!("({f})"));
            }
            if let Some(v) = &last_key {
                conds.push(format!("{kq} > {}", value_literal(skind, v)));
            }
            let where_clause = if conds.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", conds.join(" AND "))
            };
            match skind {
                DbKind::MsSql => format!(
                    "SELECT TOP {PAGE_ROWS} {select_list} FROM {stbl}{where_clause} \
                     ORDER BY {kq}"
                ),
                _ => format!(
                    "SELECT {select_list} FROM {stbl}{where_clause} ORDER BY {kq} \
                     LIMIT {PAGE_ROWS}"
                ),
            }
        } else {
            match skind {
                DbKind::MsSql => format!(
                    "SELECT {select_list} FROM {stbl}{where_sql} ORDER BY {order} \
                     OFFSET {offset} ROWS FETCH NEXT {PAGE_ROWS} ROWS ONLY"
                ),
                _ => format!(
                    "SELECT {select_list} FROM {stbl}{where_sql} ORDER BY {order} \
                     LIMIT {PAGE_ROWS} OFFSET {offset}"
                ),
            }
        };
        let rs = source.query(&page).await?;
        let n = rs.rows.len();
        offset += n as u64;
        let mut rows = rs.rows;
        if keyset_active {
            match rows.last().and_then(|r| r.get(key_idx)) {
                Some(v) if !matches!(v, Value::Null) => last_key = Some(v.clone()),
                // NULL / missing key: can't seek past it — OFFSET from here.
                _ => keyset_active = false,
            }
        }
        if let Some(limit) = spec.row_limit {
            if written + rows.len() as u64 > limit {
                rows.truncate((limit - written) as usize);
            }
        }
        // Batched RETURNING follows VALUES order on Postgres; other engines
        // give no order guarantee, so key capture there goes row-at-a-time.
        let batch = match (&spec.key_capture, tkind) {
            (Some(_), DbKind::Postgres) | (None, _) => INSERT_BATCH,
            (Some(_), _) => 1,
        };
        for chunk in rows.chunks(batch) {
            let mut values = Vec::with_capacity(chunk.len());
            let mut old_keys = Vec::with_capacity(chunk.len());
            for row in chunk {
                let mut masked: Vec<Value> = row.get(..ncols).unwrap_or(row).to_vec();
                if spec.key_capture.is_some() {
                    old_keys.push(row.get(ncols).cloned().unwrap_or(Value::Null));
                }
                for (i, parent) in &lookups {
                    let old = &masked[*i];
                    if matches!(old, Value::Null) {
                        continue;
                    }
                    match key_maps.get(*parent).and_then(|m| m.get(&old.display())) {
                        Some(new_id) => masked[*i] = new_id.clone(),
                        None => anyhow::bail!(
                            "{source_key}: no new id for {} = {} in the key map of \
                             {parent} — was that parent row filtered out?",
                            spec.columns[*i].1,
                            old.display()
                        ),
                    }
                }
                mask_row(&mask_sel, masks, &mut masked);
                values.push(format!(
                    "({})",
                    masked
                        .iter()
                        .map(|v| value_literal(tkind, v))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if values.is_empty() {
                continue;
            }
            match &spec.key_capture {
                None => {
                    let sql =
                        format!("INSERT INTO {ttbl} ({tcols}) VALUES {}", values.join(", "));
                    target.query(&sql).await?;
                }
                Some(kc) => {
                    let pk = quote_ident(tkind, &kc.target_pk);
                    let first_col = |rs: crate::db::ResultSet| -> Vec<Value> {
                        rs.rows
                            .into_iter()
                            .filter_map(|mut r| (!r.is_empty()).then(|| r.remove(0)))
                            .collect()
                    };
                    // Phrased so each driver's keyword classifier takes its
                    // row-returning path — a statement led by INSERT is
                    // execute()d and its result rows dropped.
                    let new_ids: Vec<Value> = match tkind {
                        DbKind::Postgres => {
                            let sql = format!(
                                "WITH ins AS (INSERT INTO {ttbl} ({tcols}) VALUES {} \
                                 RETURNING {pk}) SELECT {pk} FROM ins",
                                values.join(", ")
                            );
                            first_col(target.query(&sql).await?)
                        }
                        DbKind::MsSql => {
                            // The SET makes this a script; scripts run whole
                            // via simple_query, which surfaces OUTPUT rows.
                            let sql = format!(
                                "SET NOCOUNT ON; INSERT INTO {ttbl} ({tcols}) \
                                 OUTPUT INSERTED.{pk} VALUES {}",
                                values.join(", ")
                            );
                            first_col(target.query(&sql).await?)
                        }
                        DbKind::MySql | DbKind::Sqlite => {
                            // One script = one session, so the id call can't
                            // land on a different pooled connection.
                            let id_fn = if tkind == DbKind::MySql {
                                "LAST_INSERT_ID()"
                            } else {
                                "last_insert_rowid()"
                            };
                            let sql = format!(
                                "INSERT INTO {ttbl} ({tcols}) VALUES {};\nSELECT {id_fn}",
                                values.join(", ")
                            );
                            first_col(target.query(&sql).await?)
                        }
                    };
                    anyhow::ensure!(
                        new_ids.len() == old_keys.len(),
                        "captured {} generated id(s) for {} inserted row(s)",
                        new_ids.len(),
                        old_keys.len()
                    );
                    let map = key_maps.entry(source_key.clone()).or_default();
                    for (old, new) in old_keys.iter().zip(new_ids) {
                        map.insert(old.display(), new);
                    }
                }
            }
            written += chunk.len() as u64;
        }
        progress(format!(
            "{} → {}: {written} row(s)",
            mask_sel.key(),
            target_sel.key()
        ));
        if n < PAGE_ROWS || spec.row_limit.is_some_and(|l| written >= l) {
            break;
        }
    }

    // Postgres target: bump serial sequences past any copied ids (no-op and
    // ignored for columns without a sequence).
    if tkind == DbKind::Postgres {
        for col in &target_sel.columns {
            let seq_sql = format!(
                "SELECT setval(s::regclass, GREATEST((SELECT COALESCE(MAX({col_q}), 1) FROM {ttbl}), 1)) \
                 FROM pg_get_serial_sequence('{schema}.{table}', '{col_e}') s WHERE s IS NOT NULL",
                col_q = quote_ident(tkind, col),
                schema = spec.target_schema.replace('\'', "''"),
                table = spec.target_table.replace('\'', "''"),
                col_e = col.replace('\'', "''"),
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
    fn window_fn_sniff() {
        assert!(contains_window_fn("ROW_NUMBER() OVER (PARTITION BY Email)"));
        assert!(contains_window_fn("count(*) over(order by id)"));
        assert!(contains_window_fn("LOWER(CASE WHEN ROW_NUMBER() OVER (PARTITION BY e) = 1 THEN e END)"));
        assert!(!contains_window_fn("CONCAT(FirstName, ' ', LastName)"));
        assert!(!contains_window_fn("rollover + 1"));
        assert!(!contains_window_fn("turnover_rate * 2"));
    }

    #[test]
    fn fake_masks_are_deterministic_and_shaped() {
        let v = Value::Text("John Smith".into());
        for kind in FakeKind::ALL {
            let a = mask_value(&MaskStrategy::Fake(kind), &v);
            let b = mask_value(&MaskStrategy::Fake(kind), &v);
            assert_eq!(a.display(), b.display(), "{kind:?} not deterministic");
            assert_ne!(a.display(), "John Smith", "{kind:?} left value unmasked");
            assert!(!a.display().is_empty());
            // NULL passes through untouched.
            assert!(matches!(mask_value(&MaskStrategy::Fake(kind), &Value::Null), Value::Null));
        }
        // Different inputs diverge (same seed would defeat the purpose).
        let other = Value::Text("Jane Doe".into());
        assert_ne!(
            mask_value(&MaskStrategy::Fake(FakeKind::Name), &v).display(),
            mask_value(&MaskStrategy::Fake(FakeKind::Name), &other).display()
        );
        let email = mask_value(&MaskStrategy::Fake(FakeKind::Email), &v).display();
        assert!(email.ends_with("@example.test"), "got {email}");
        assert!(!email.contains(' '), "got {email}");
        // Numeric fakes are typed and magnitude-preserving.
        let age = mask_value(&MaskStrategy::Fake(FakeKind::Age), &Value::Int(37));
        assert!(matches!(age, Value::Int(n) if (18..=90).contains(&n)));
        let qty = mask_value(&MaskStrategy::Fake(FakeKind::Int), &Value::Int(742));
        assert!(matches!(qty, Value::Int(n) if (100..1000).contains(&n)), "got {qty:?}");
        let neg = mask_value(&MaskStrategy::Fake(FakeKind::Int), &Value::Int(-5));
        assert!(matches!(neg, Value::Int(n) if (-9..0).contains(&n)), "got {neg:?}");
        let price = mask_value(&MaskStrategy::Fake(FakeKind::Price), &Value::Float(19.99));
        assert!(matches!(price, Value::Float(f) if (10.0..100.0).contains(&f)), "got {price:?}");
    }

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
        assert_eq!(suggest_mask("primary_email"), Some(MaskStrategy::Fake(FakeKind::Email)));
        assert!(matches!(suggest_mask("password_hash"), Some(MaskStrategy::Hash)));
        assert_eq!(suggest_mask("phone_number"), Some(MaskStrategy::Fake(FakeKind::Phone)));
        assert_eq!(suggest_mask("ssn"), Some(MaskStrategy::Fake(FakeKind::Ssn)));
        assert_eq!(suggest_mask("date_of_birth"), None); // only birthdate/dob spellings
        assert_eq!(suggest_mask("dob"), Some(MaskStrategy::Fake(FakeKind::BirthDate)));
        assert_eq!(suggest_mask("created_at"), None);
        assert_eq!(suggest_mask("id"), None);
    }

    #[test]
    fn fake_kind_inference() {
        assert_eq!(infer_fake("shipping_address", "jsonb"), Some(MaskStrategy::JsonScrub));
        assert_eq!(infer_fake("email", "varchar"), Some(MaskStrategy::Fake(FakeKind::Email)));
        assert_eq!(infer_fake("first_name", "text"), Some(MaskStrategy::Fake(FakeKind::FirstName)));
        // Bare name columns fake as a person name (manual option only).
        assert_eq!(infer_fake("name", "text"), Some(MaskStrategy::Fake(FakeKind::Name)));
        assert_eq!(infer_fake("display_name", "text"), Some(MaskStrategy::Fake(FakeKind::Name)));
        assert_eq!(infer_fake("total", "numeric"), Some(MaskStrategy::Fake(FakeKind::Price)));
        assert_eq!(infer_fake("visits", "integer"), Some(MaskStrategy::Fake(FakeKind::Int)));
        assert_eq!(infer_fake("age", "integer"), Some(MaskStrategy::Fake(FakeKind::Age)));
        assert_eq!(infer_fake("notes", "text"), None);
        assert_eq!(infer_fake("password", "text"), None); // hash, not fake
        // camelCase / kebab-case columns match the same keywords.
        assert_eq!(suggest_mask("firstName"), Some(MaskStrategy::Fake(FakeKind::FirstName)));
        assert_eq!(suggest_mask("socialSecurity"), Some(MaskStrategy::Fake(FakeKind::Ssn)));
        assert_eq!(suggest_mask("credit-card"), Some(MaskStrategy::Hash));
        assert_eq!(infer_fake("displayName", "text"), Some(MaskStrategy::Fake(FakeKind::Name)));
        // Empty is a real value, not NULL.
        assert!(matches!(
            mask_value(&MaskStrategy::Empty, &Value::Text("x".into())),
            Value::Text(s) if s.is_empty()
        ));
    }

    #[test]
    fn json_scrub_masks_sensitive_keys_only() {
        let v = Value::Json(serde_json::json!({
            "street": "1 Real Street",
            "city": "Realville",
            "note": "keep me",
            "nested": { "phone": "555-REAL", "count": 3 },
            "email": null
        }));
        let Value::Json(out) = mask_value(&MaskStrategy::JsonScrub, &v) else {
            panic!("expected JSON out");
        };
        assert_ne!(out["street"], "1 Real Street");
        assert_ne!(out["city"], "Realville");
        assert_eq!(out["note"], "keep me");
        assert_ne!(out["nested"]["phone"], "555-REAL");
        assert_eq!(out["nested"]["count"], 3);
        assert!(out["email"].is_null(), "JSON nulls stay null");
        // Deterministic and structure-preserving.
        let Value::Json(again) = mask_value(&MaskStrategy::JsonScrub, &v) else {
            panic!("expected JSON out");
        };
        assert_eq!(out, again);
        // Garbage never passes through unmasked.
        let junk = mask_value(&MaskStrategy::JsonScrub, &Value::Text("not json".into()));
        assert_eq!(junk.display(), "\"<masked>\"");
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
