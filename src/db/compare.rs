//! Structure comparison between two introspected databases.
//!
//! Both sides are plain `Vec<TableStructure>` snapshots (the same walk that
//! feeds DBML generation), so any two connections can be compared — different
//! servers, different databases on one server, even different drivers (though
//! cross-dialect type names make that a noisy, view-only affair).
//!
//! Display and scripting are split: [`diff_snapshots`] produces the
//! human-readable difference list, while [`migration_statements`] reuses the
//! Structure editor's `WorkingTable` diff → DDL pipeline by loading the target
//! table as the origin snapshot and overlaying the desired state on top.

use std::collections::BTreeMap;

use super::DbKind;
use super::structure::{
    CheckInfo, ColumnInfo, DdlStatement, FkInfo, IdentityKind, IndexInfo, TableStructure,
    WorkingTable, drop_table_stmt, generate,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    OnlyLeft(String),
    OnlyRight(String),
    Differs { left: String, right: String },
}

#[derive(Debug, Clone)]
pub struct DiffEntry {
    /// "column" / "primary key" / "index" / "foreign key" / "check".
    pub kind: &'static str,
    pub name: String,
    pub change: Change,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableStatus {
    OnlyLeft,
    OnlyRight,
    Different,
    Identical,
}

#[derive(Debug, Clone)]
pub struct TableDiff {
    /// Schema/name for display and script comments (left side wins when both
    /// exist; they only differ under name-only matching).
    pub schema: String,
    pub name: String,
    pub status: TableStatus,
    pub entries: Vec<DiffEntry>,
    /// Indexes into the snapshot Vecs this diff was computed from.
    pub left_idx: Option<usize>,
    pub right_idx: Option<usize>,
}

/// Compare two snapshots. Tables match on (schema, name), or on the bare
/// table name when `ignore_schema` is set (useful across dialects where the
/// schema concept differs).
pub fn diff_snapshots(
    left: &[TableStructure],
    right: &[TableStructure],
    ignore_schema: bool,
) -> Vec<TableDiff> {
    let key = |t: &TableStructure| -> String {
        if ignore_schema {
            t.name.clone()
        } else {
            format!("{}.{}", t.schema, t.name)
        }
    };
    let mut map: BTreeMap<String, (Option<usize>, Option<usize>)> = BTreeMap::new();
    for (i, t) in left.iter().enumerate() {
        map.entry(key(t)).or_insert((None, None)).0.get_or_insert(i);
    }
    for (i, t) in right.iter().enumerate() {
        map.entry(key(t)).or_insert((None, None)).1.get_or_insert(i);
    }

    map.into_values()
        .filter_map(|(li, ri)| {
            let src = li.map(|i| &left[i]).or(ri.map(|i| &right[i]))?;
            let (status, entries) = match (li, ri) {
                (Some(l), Some(r)) => {
                    let entries = diff_tables(&left[l], &right[r]);
                    let status = if entries.is_empty() {
                        TableStatus::Identical
                    } else {
                        TableStatus::Different
                    };
                    (status, entries)
                }
                (Some(_), None) => (TableStatus::OnlyLeft, Vec::new()),
                (None, Some(_)) => (TableStatus::OnlyRight, Vec::new()),
                (None, None) => return None,
            };
            Some(TableDiff {
                schema: src.schema.clone(),
                name: src.name.clone(),
                status,
                entries,
                left_idx: li,
                right_idx: ri,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Per-object descriptions (what the UI shows on each side of a difference)
// ---------------------------------------------------------------------------

fn column_desc(c: &ColumnInfo) -> String {
    let mut s = c.type_name.clone();
    if c.not_null {
        s.push_str(" NOT NULL");
    }
    if let Some(d) = c.default.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        s.push_str(&format!(" DEFAULT {d}"));
    }
    if c.identity != IdentityKind::None {
        s.push_str(&format!(" {}", c.identity.label()));
    }
    if c.generated {
        s.push_str(" (generated)");
    }
    s
}

fn pk_desc(k: &super::structure::KeyInfo) -> String {
    match &k.name {
        Some(n) => format!("{n} ({})", k.columns.join(", ")),
        None => format!("({})", k.columns.join(", ")),
    }
}

fn index_desc(ix: &IndexInfo) -> String {
    let unique = if ix.unique { "UNIQUE " } else { "" };
    format!("{} {unique}({})", ix.name, ix.columns.join(", "))
}

fn fk_desc(fk: &FkInfo) -> String {
    let mut s = format!(
        "{} ({}) → {}.{} ({})",
        fk.name,
        fk.columns.join(", "),
        fk.ref_schema,
        fk.ref_table,
        fk.ref_columns.join(", ")
    );
    if fk.on_delete != "NO ACTION" {
        s.push_str(&format!(" ON DELETE {}", fk.on_delete));
    }
    if fk.on_update != "NO ACTION" {
        s.push_str(&format!(" ON UPDATE {}", fk.on_update));
    }
    s
}

fn norm_expr(e: &str) -> String {
    e.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn check_desc(ck: &CheckInfo) -> String {
    format!("{} CHECK ({})", ck.name, norm_expr(&ck.expression))
}

/// Structural differences between two same-named tables, as display entries.
pub fn diff_tables(l: &TableStructure, r: &TableStructure) -> Vec<DiffEntry> {
    let mut out = Vec::new();

    // Columns, matched by name; left order first, then right-only extras.
    for lc in &l.columns {
        match r.columns.iter().find(|rc| rc.name == lc.name) {
            Some(rc) => {
                let (ld, rd) = (column_desc(lc), column_desc(rc));
                if ld != rd {
                    out.push(DiffEntry {
                        kind: "column",
                        name: lc.name.clone(),
                        change: Change::Differs { left: ld, right: rd },
                    });
                }
            }
            None => out.push(DiffEntry {
                kind: "column",
                name: lc.name.clone(),
                change: Change::OnlyLeft(column_desc(lc)),
            }),
        }
    }
    for rc in &r.columns {
        if !l.columns.iter().any(|lc| lc.name == rc.name) {
            out.push(DiffEntry {
                kind: "column",
                name: rc.name.clone(),
                change: Change::OnlyRight(column_desc(rc)),
            });
        }
    }

    // Primary key: compare column lists; names only when both sides name it.
    match (&l.primary_key, &r.primary_key) {
        (None, None) => {}
        (Some(k), None) => out.push(DiffEntry {
            kind: "primary key",
            name: k.name.clone().unwrap_or_default(),
            change: Change::OnlyLeft(pk_desc(k)),
        }),
        (None, Some(k)) => out.push(DiffEntry {
            kind: "primary key",
            name: k.name.clone().unwrap_or_default(),
            change: Change::OnlyRight(pk_desc(k)),
        }),
        (Some(a), Some(b)) => {
            let names_differ =
                matches!((&a.name, &b.name), (Some(x), Some(y)) if x != y);
            if a.columns != b.columns || names_differ {
                out.push(DiffEntry {
                    kind: "primary key",
                    name: a.name.clone().unwrap_or_default(),
                    change: Change::Differs { left: pk_desc(a), right: pk_desc(b) },
                });
            }
        }
    }

    // Indexes / FKs / checks: match by name first, then structurally (same
    // definition under a different name — common when constraint names were
    // auto-generated), so renamed-but-equal objects show as one line.
    diff_named(
        &mut out,
        "index",
        &l.indexes,
        &r.indexes,
        |ix| ix.name.clone(),
        |a, b| a.columns == b.columns && a.unique == b.unique,
        index_desc,
    );
    diff_named(
        &mut out,
        "foreign key",
        &l.foreign_keys,
        &r.foreign_keys,
        |fk| fk.name.clone(),
        |a, b| {
            a.columns == b.columns
                && a.ref_table == b.ref_table
                && a.ref_columns == b.ref_columns
                && a.on_delete == b.on_delete
                && a.on_update == b.on_update
        },
        fk_desc,
    );
    diff_named(
        &mut out,
        "check",
        &l.checks,
        &r.checks,
        |ck| ck.name.clone(),
        |a, b| norm_expr(&a.expression) == norm_expr(&b.expression),
        check_desc,
    );

    out
}

/// Generic name-then-structure matcher for indexes, FKs and checks.
fn diff_named<T>(
    out: &mut Vec<DiffEntry>,
    kind: &'static str,
    left: &[T],
    right: &[T],
    name: impl Fn(&T) -> String,
    equivalent: impl Fn(&T, &T) -> bool,
    desc: impl Fn(&T) -> String,
) {
    let mut r_used = vec![false; right.len()];
    let mut l_unmatched: Vec<&T> = Vec::new();

    for li in left {
        match right.iter().position(|ri| name(ri) == name(li)) {
            Some(pos) => {
                r_used[pos] = true;
                if !equivalent(li, &right[pos]) {
                    out.push(DiffEntry {
                        kind,
                        name: name(li),
                        change: Change::Differs {
                            left: desc(li),
                            right: desc(&right[pos]),
                        },
                    });
                }
            }
            None => l_unmatched.push(li),
        }
    }
    for li in l_unmatched {
        let structural = right
            .iter()
            .enumerate()
            .find(|(i, ri)| !r_used[*i] && equivalent(li, ri));
        match structural {
            Some((i, ri)) => {
                r_used[i] = true;
                out.push(DiffEntry {
                    kind,
                    name: name(li),
                    change: Change::Differs { left: desc(li), right: desc(ri) },
                });
            }
            None => out.push(DiffEntry {
                kind,
                name: name(li),
                change: Change::OnlyLeft(desc(li)),
            }),
        }
    }
    for (i, ri) in right.iter().enumerate() {
        if !r_used[i] {
            out.push(DiffEntry {
                kind,
                name: name(ri),
                change: Change::OnlyRight(desc(ri)),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Migration scripting
// ---------------------------------------------------------------------------

/// Statements that make `target` match `desired` (both in `kind`'s dialect).
/// `desired` without `target` → CREATE; `target` without `desired` → DROP.
pub fn migration_statements(
    kind: DbKind,
    desired: Option<&TableStructure>,
    target: Option<&TableStructure>,
) -> Vec<DdlStatement> {
    match (desired, target) {
        (None, None) => Vec::new(),
        (Some(d), None) => {
            let mut wt = WorkingTable::from_structure(kind, d);
            wt.original_name = None; // force CREATE-path generation
            generate(&wt)
        }
        (None, Some(t)) => vec![drop_table_stmt(kind, &t.schema, &t.name)],
        (Some(d), Some(t)) => {
            let mut wt = WorkingTable::from_structure(kind, t);
            overlay(&mut wt, d);
            generate(&wt)
        }
    }
}

/// Mutate a target-loaded `WorkingTable` so its working state equals
/// `desired`; `generate()` then emits the ALTERs. Objects are matched like
/// the display diff: by name, then structurally (keeping the target's name
/// so equivalently-defined objects aren't dropped and recreated).
fn overlay(wt: &mut WorkingTable, desired: &TableStructure) {
    // Columns by name.
    for dc in &desired.columns {
        match wt.columns.iter_mut().find(|c| c.name == dc.name) {
            Some(c) => {
                c.type_name = dc.type_name.clone();
                c.not_null = dc.not_null;
                c.default = dc.default.clone().unwrap_or_default();
                c.identity = dc.identity;
            }
            None => {
                let id = wt.add_column();
                let c = wt.columns.iter_mut().find(|c| c.id == id).unwrap();
                c.name = dc.name.clone();
                c.type_name = dc.type_name.clone();
                c.not_null = dc.not_null;
                c.default = dc.default.clone().unwrap_or_default();
                c.identity = dc.identity;
                c.generated = dc.generated;
            }
        }
    }
    for c in wt.columns.iter_mut() {
        if !desired.columns.iter().any(|dc| dc.name == c.name) {
            c.dropped = true;
        }
    }
    let id_of = |wt: &WorkingTable, name: &str| {
        wt.columns
            .iter()
            .find(|c| !c.dropped && c.name == name)
            .map(|c| c.id)
    };

    // Primary key.
    match &desired.primary_key {
        Some(k) => {
            wt.pk.present = true;
            wt.pk.column_ids = k.columns.iter().filter_map(|n| id_of(wt, n)).collect();
            // Only adopt the desired name when the target didn't have a PK;
            // renaming an existing equal PK is churn, not drift.
            if wt.pk.origin.is_none() {
                wt.pk.name = k.name.clone().unwrap_or_default();
            }
        }
        None => wt.pk.present = false,
    }

    // Indexes: name match → update in place; structural match → keep as-is
    // under the target's name; leftovers are added/dropped.
    let mut used = vec![false; wt.indexes.len()];
    let mut new_indexes: Vec<&IndexInfo> = Vec::new();
    for dix in &desired.indexes {
        if let Some(pos) = wt.indexes.iter().position(|ix| ix.name == dix.name) {
            used[pos] = true;
            let ids: Vec<_> = dix.columns.iter().filter_map(|n| id_of(wt, n)).collect();
            let ix = &mut wt.indexes[pos];
            ix.unique = dix.unique;
            ix.column_ids = ids;
        } else {
            new_indexes.push(dix);
        }
    }
    for dix in new_indexes {
        let ids: Vec<_> = dix.columns.iter().filter_map(|n| id_of(wt, n)).collect();
        let structural = wt
            .indexes
            .iter()
            .enumerate()
            .find(|(i, ix)| !used[*i] && ix.unique == dix.unique && ix.column_ids == ids)
            .map(|(i, _)| i);
        match structural {
            Some(pos) => used[pos] = true,
            None => {
                let id = wt.add_index();
                used.push(true);
                let ix = wt.indexes.iter_mut().find(|ix| ix.id == id).unwrap();
                ix.name = dix.name.clone();
                ix.unique = dix.unique;
                ix.column_ids = ids;
            }
        }
    }
    for (i, ix) in wt.indexes.iter_mut().enumerate() {
        if !used.get(i).copied().unwrap_or(true) {
            ix.dropped = true;
        }
    }

    // Foreign keys, same scheme; structural identity ignores the name and the
    // referenced schema (which differs across databases by construction).
    let fk_matches = |wt: &WorkingTable, i: usize, dfk: &FkInfo| {
        let fk = &wt.foreign_keys[i];
        let ids: Vec<_> = dfk.columns.iter().filter_map(|n| id_of(wt, n)).collect();
        fk.column_ids == ids
            && fk.ref_table == dfk.ref_table
            && super::structure::split_cols(&fk.ref_columns) == dfk.ref_columns
    };
    let mut used = vec![false; wt.foreign_keys.len()];
    let mut new_fks: Vec<&FkInfo> = Vec::new();
    for dfk in &desired.foreign_keys {
        if let Some(pos) = wt.foreign_keys.iter().position(|fk| fk.name == dfk.name) {
            used[pos] = true;
            let ids: Vec<_> = dfk.columns.iter().filter_map(|n| id_of(wt, n)).collect();
            let fk = &mut wt.foreign_keys[pos];
            fk.column_ids = ids;
            fk.ref_table = dfk.ref_table.clone();
            fk.ref_columns = dfk.ref_columns.join(", ");
            fk.on_delete = dfk.on_delete.clone();
            fk.on_update = dfk.on_update.clone();
        } else {
            new_fks.push(dfk);
        }
    }
    for dfk in new_fks {
        let structural = (0..wt.foreign_keys.len())
            .find(|&i| !used[i] && fk_matches(wt, i, dfk));
        match structural {
            Some(pos) => {
                used[pos] = true;
                let fk = &mut wt.foreign_keys[pos];
                fk.on_delete = dfk.on_delete.clone();
                fk.on_update = dfk.on_update.clone();
            }
            None => {
                let ids: Vec<_> = dfk.columns.iter().filter_map(|n| id_of(wt, n)).collect();
                let id = wt.add_fk();
                used.push(true);
                let ref_schema = wt.schema.clone();
                let fk = wt.foreign_keys.iter_mut().find(|fk| fk.id == id).unwrap();
                fk.name = dfk.name.clone();
                fk.column_ids = ids;
                // Point at the target database's own schema: the referenced
                // table lives there once the whole script has run.
                fk.ref_schema = ref_schema;
                fk.ref_table = dfk.ref_table.clone();
                fk.ref_columns = dfk.ref_columns.join(", ");
                fk.on_delete = dfk.on_delete.clone();
                fk.on_update = dfk.on_update.clone();
            }
        }
    }
    for (i, fk) in wt.foreign_keys.iter_mut().enumerate() {
        if !used.get(i).copied().unwrap_or(true) {
            fk.dropped = true;
        }
    }

    // Checks.
    let mut used = vec![false; wt.checks.len()];
    let mut new_checks: Vec<&CheckInfo> = Vec::new();
    for dck in &desired.checks {
        if let Some(pos) = wt.checks.iter().position(|ck| ck.name == dck.name) {
            used[pos] = true;
            wt.checks[pos].expression = dck.expression.clone();
        } else {
            new_checks.push(dck);
        }
    }
    for dck in new_checks {
        let structural = (0..wt.checks.len()).find(|&i| {
            !used[i] && norm_expr(&wt.checks[i].expression) == norm_expr(&dck.expression)
        });
        match structural {
            Some(pos) => used[pos] = true,
            None => {
                let id = wt.add_check();
                used.push(true);
                let ck = wt.checks.iter_mut().find(|ck| ck.id == id).unwrap();
                ck.name = dck.name.clone();
                ck.expression = dck.expression.clone();
            }
        }
    }
    for (i, ck) in wt.checks.iter_mut().enumerate() {
        if !used.get(i).copied().unwrap_or(true) {
            ck.dropped = true;
        }
    }
}

/// Stable key for a table diff (checkbox bookkeeping in the UI).
pub fn table_key(d: &TableDiff) -> String {
    format!("{}.{}", d.schema, d.name)
}

/// Stable key for one diff entry within its table.
pub fn entry_key(e: &DiffEntry) -> String {
    format!("{}:{}", e.kind, e.name)
}

/// The desired state of `target` when only `entries` — a subset of the diff
/// between `origin` and `target` — should be applied. Each entry copies,
/// inserts, or removes the one object it describes; everything else keeps the
/// target's current shape.
pub fn selective_desired(
    origin: &TableStructure,
    target: &TableStructure,
    entries: &[&DiffEntry],
) -> TableStructure {
    fn sync_by_name<T: Clone>(
        dst: &mut Vec<T>,
        src: &[T],
        name: &str,
        equivalent: impl Fn(&T, &T) -> bool,
        name_of: impl Fn(&T) -> &str,
    ) {
        match src.iter().find(|o| name_of(o) == name) {
            // Origin has the object: replace the target's pairing (same name,
            // or the structurally-equivalent one a renamed pair matched with).
            Some(o) => {
                if let Some(pos) = dst.iter().position(|t| name_of(t) == name) {
                    dst[pos] = o.clone();
                } else if let Some(pos) = dst.iter().position(|t| equivalent(t, o)) {
                    dst[pos] = o.clone();
                } else {
                    dst.push(o.clone());
                }
            }
            // Target-only object: applying the entry means dropping it.
            None => dst.retain(|t| name_of(t) != name),
        }
    }

    let mut d = target.clone();
    for e in entries {
        match e.kind {
            "column" => match origin.columns.iter().find(|c| c.name == e.name) {
                Some(oc) => match d.columns.iter_mut().find(|c| c.name == e.name) {
                    Some(dc) => *dc = oc.clone(),
                    None => d.columns.push(oc.clone()),
                },
                None => d.columns.retain(|c| c.name != e.name),
            },
            "primary key" => d.primary_key = origin.primary_key.clone(),
            "index" => sync_by_name(
                &mut d.indexes,
                &origin.indexes,
                &e.name,
                |a, b| a.columns == b.columns && a.unique == b.unique,
                |ix| &ix.name,
            ),
            "foreign key" => sync_by_name(
                &mut d.foreign_keys,
                &origin.foreign_keys,
                &e.name,
                |a, b| {
                    a.columns == b.columns
                        && a.ref_table == b.ref_table
                        && a.ref_columns == b.ref_columns
                },
                |fk| &fk.name,
            ),
            "check" => sync_by_name(
                &mut d.checks,
                &origin.checks,
                &e.name,
                |a, b| norm_expr(&a.expression) == norm_expr(&b.expression),
                |ck| &ck.name,
            ),
            _ => {}
        }
    }
    d
}

/// Migration batches that make the right side (target) match the left
/// (origin), honoring the caller's per-table / per-entry inclusion choices.
/// `included(diff, None)` gates whole-table creates/drops; `(diff, Some(e))`
/// gates individual entries of a differing table.
pub fn selective_migration(
    kind: DbKind,
    diffs: &[TableDiff],
    left: &[TableStructure],
    right: &[TableStructure],
    included: &dyn Fn(&TableDiff, Option<&DiffEntry>) -> bool,
) -> Vec<(String, Vec<DdlStatement>)> {
    let mut out = Vec::new();
    for d in diffs {
        let stmts = match d.status {
            TableStatus::Identical => continue,
            TableStatus::OnlyLeft => {
                if !included(d, None) {
                    continue;
                }
                migration_statements(kind, d.left_idx.map(|i| &left[i]), None)
            }
            TableStatus::OnlyRight => {
                if !included(d, None) {
                    continue;
                }
                migration_statements(kind, None, d.right_idx.map(|i| &right[i]))
            }
            TableStatus::Different => {
                let sel: Vec<&DiffEntry> =
                    d.entries.iter().filter(|e| included(d, Some(e))).collect();
                if sel.is_empty() {
                    continue;
                }
                let (Some(li), Some(ri)) = (d.left_idx, d.right_idx) else { continue };
                let (origin, target) = (&left[li], &right[ri]);
                if sel.len() == d.entries.len() {
                    migration_statements(kind, Some(origin), Some(target))
                } else {
                    let desired = selective_desired(origin, target, &sel);
                    migration_statements(kind, Some(&desired), Some(target))
                }
            }
        };
        if !stmts.is_empty() {
            out.push((format!("{}.{}", d.schema, d.name), stmts));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::structure::{IndexInfo, KeyInfo};

    fn col(name: &str, ty: &str, not_null: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            type_name: ty.into(),
            not_null,
            default: None,
            identity: IdentityKind::None,
            generated: false,
            default_constraint: None,
        }
    }

    fn table(schema: &str, name: &str, columns: Vec<ColumnInfo>) -> TableStructure {
        TableStructure {
            schema: schema.into(),
            name: name.into(),
            columns,
            primary_key: None,
            foreign_keys: vec![],
            indexes: vec![],
            checks: vec![],
        }
    }

    #[test]
    fn snapshot_diff_classifies_tables() {
        let left = vec![
            table("public", "users", vec![col("id", "bigint", true)]),
            table("public", "orders", vec![col("id", "bigint", true)]),
        ];
        let right = vec![
            table("public", "users", vec![col("id", "bigint", true)]),
            table("public", "invoices", vec![col("id", "bigint", true)]),
        ];
        let diffs = diff_snapshots(&left, &right, false);
        let status_of = |n: &str| diffs.iter().find(|d| d.name == n).unwrap().status;
        assert_eq!(status_of("users"), TableStatus::Identical);
        assert_eq!(status_of("orders"), TableStatus::OnlyLeft);
        assert_eq!(status_of("invoices"), TableStatus::OnlyRight);
    }

    #[test]
    fn column_and_type_differences_are_reported() {
        let l = table(
            "public",
            "users",
            vec![col("id", "bigint", true), col("email", "varchar(100)", true)],
        );
        let r = table(
            "public",
            "users",
            vec![col("id", "bigint", true), col("email", "varchar(50)", false), col("age", "int", false)],
        );
        let entries = diff_tables(&l, &r);
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[0].change,
            Change::Differs { left, right }
                if left == "varchar(100) NOT NULL" && right == "varchar(50)"
        ));
        assert!(matches!(&entries[1].change, Change::OnlyRight(_)));
        assert_eq!(entries[1].name, "age");
    }

    #[test]
    fn renamed_but_equal_index_matches_structurally() {
        let mut l = table("public", "users", vec![col("email", "text", false)]);
        let mut r = l.clone();
        l.indexes.push(IndexInfo {
            name: "users_email_idx".into(),
            columns: vec!["email".into()],
            unique: true,
        });
        r.indexes.push(IndexInfo {
            name: "ix_users_email".into(),
            columns: vec!["email".into()],
            unique: true,
        });
        let entries = diff_tables(&l, &r);
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0].change, Change::Differs { .. }),
            "renamed index should pair up as one Differs entry, got {:?}",
            entries[0].change
        );
    }

    #[test]
    fn migration_creates_missing_table() {
        let mut t = table("public", "tags", vec![col("id", "bigint", true)]);
        t.primary_key = Some(KeyInfo { name: None, columns: vec!["id".into()] });
        let stmts = migration_statements(DbKind::Postgres, Some(&t), None);
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].sql.starts_with("CREATE TABLE \"public\".\"tags\""));
        assert!(stmts[0].sql.contains("PRIMARY KEY (\"id\")"));
    }

    #[test]
    fn migration_drops_extra_table() {
        let t = table("public", "legacy", vec![col("id", "bigint", true)]);
        let stmts = migration_statements(DbKind::Postgres, None, Some(&t));
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].sql, r#"DROP TABLE "public"."legacy""#);
    }

    #[test]
    fn migration_alters_to_match_desired() {
        let desired = table(
            "public",
            "users",
            vec![col("id", "bigint", true), col("email", "varchar(100)", true)],
        );
        let target = table(
            "public",
            "users",
            vec![col("id", "bigint", true), col("age", "int", false)],
        );
        let stmts = migration_statements(DbKind::Postgres, Some(&desired), Some(&target));
        let sql: Vec<&str> = stmts.iter().map(|s| s.sql.as_str()).collect();
        assert!(sql.iter().any(|s| s.contains("DROP COLUMN \"age\"")));
        assert!(sql.iter().any(|s| s.contains(r#"ADD COLUMN "email" varchar(100) NOT NULL"#)));
    }

    #[test]
    fn migration_keeps_structurally_equal_renamed_index() {
        let mut desired = table("public", "users", vec![col("email", "text", false)]);
        let mut target = desired.clone();
        desired.indexes.push(IndexInfo {
            name: "users_email_idx".into(),
            columns: vec!["email".into()],
            unique: false,
        });
        target.indexes.push(IndexInfo {
            name: "ix_users_email".into(),
            columns: vec!["email".into()],
            unique: false,
        });
        let stmts = migration_statements(DbKind::Postgres, Some(&desired), Some(&target));
        assert!(
            stmts.is_empty(),
            "equal index under a different name should not churn: {:?}",
            stmts.iter().map(|s| &s.sql).collect::<Vec<_>>()
        );
    }

    #[test]
    fn selective_migration_honors_checkboxes() {
        let origin = table(
            "public",
            "users",
            vec![col("id", "bigint", true), col("email", "varchar(100)", true)],
        );
        let target = table(
            "public",
            "users",
            vec![col("id", "bigint", true), col("age", "int", false)],
        );
        let diffs = diff_snapshots(
            std::slice::from_ref(&origin),
            std::slice::from_ref(&target),
            false,
        );
        assert_eq!(diffs[0].entries.len(), 2); // add email, drop age

        // Only the "email" entry selected: age must survive.
        let only_email = |_d: &TableDiff, e: Option<&DiffEntry>| {
            e.is_some_and(|e| e.name == "email")
        };
        let batches = selective_migration(
            DbKind::Postgres,
            &diffs,
            std::slice::from_ref(&origin),
            std::slice::from_ref(&target),
            &only_email,
        );
        let sql: Vec<&str> = batches
            .iter()
            .flat_map(|(_, s)| s.iter().map(|st| st.sql.as_str()))
            .collect();
        assert_eq!(sql.len(), 1);
        assert!(sql[0].contains(r#"ADD COLUMN "email""#));

        // Nothing selected → empty script.
        let none = |_d: &TableDiff, _e: Option<&DiffEntry>| false;
        assert!(
            selective_migration(
                DbKind::Postgres,
                &diffs,
                std::slice::from_ref(&origin),
                std::slice::from_ref(&target),
                &none,
            )
            .is_empty()
        );
    }

    #[test]
    fn name_only_matching_pairs_across_schemas() {
        let left = vec![table("public", "users", vec![col("id", "bigint", true)])];
        let right = vec![table("shop", "users", vec![col("id", "bigint", true)])];
        assert_eq!(diff_snapshots(&left, &right, false).len(), 2);
        let merged = diff_snapshots(&left, &right, true);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].status, TableStatus::Identical);
    }
}
