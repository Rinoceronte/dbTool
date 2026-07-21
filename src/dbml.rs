//! Parse DBML source into an owned diagram model, independent of dbml-rs AST lifetimes.

use std::collections::{BTreeMap, HashMap, HashSet};

use dbml_rs::ast::{
    Attribute, Nullable, RefIdent, Relation, TableBlock, TopLevelBlock, Value,
};
use pest::error::LineColLocation;

#[derive(Debug, Clone)]
pub struct DiagramModel {
    pub tables: Vec<DTable>,
    pub refs: Vec<DRef>,
    pub groups: Vec<DGroup>,
}

#[derive(Debug, Clone)]
pub struct DTable {
    /// Stable layout key: "schema.name".
    pub key: String,
    /// Display name (schema prefix omitted for the default schema).
    pub name: String,
    pub header_color: Option<egui::Color32>,
    pub group: Option<usize>,
    pub columns: Vec<DColumn>,
}

#[derive(Debug, Clone)]
pub struct DColumn {
    pub name: String,
    pub ty: String,
    pub is_pk: bool,
    pub is_fk: bool,
    pub not_null: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DRel {
    One2One,
    One2Many,
    Many2One,
    Many2Many,
}

#[derive(Debug, Clone, Copy)]
pub struct DRef {
    pub from: (usize, Option<usize>),
    pub to: (usize, Option<usize>),
    pub rel: DRel,
}

#[derive(Debug, Clone)]
pub struct DGroup {
    pub name: String,
    pub color: Option<egui::Color32>,
    pub tables: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct DbmlError {
    pub message: String,
    pub line: Option<usize>,
}

pub fn parse(input: &str) -> Result<DiagramModel, DbmlError> {
    // Syntax-only parse: dbml-rs's semantic analyzer rejects valid dbdiagram
    // constructs (e.g. composite PKs as multiple [pk] columns). We resolve
    // refs/groups leniently ourselves, which suits a live editor better.
    let ast = dbml_rs::parse_dbml_unchecked(input).map_err(|e| {
        let line = match e.line_col {
            LineColLocation::Pos((l, _)) | LineColLocation::Span((l, _), _) => Some(l),
        };
        DbmlError { message: compact_pest_message(&e.to_string()), line }
    })?;

    let table_blocks: Vec<&TableBlock> = ast
        .blocks
        .iter()
        .filter_map(|b| match b {
            TopLevelBlock::Table(t) => Some(t),
            _ => None,
        })
        .collect();

    // Lookup: "schema.name" plus bare name and alias for unqualified references.
    let mut lookup: HashMap<String, usize> = HashMap::new();
    let mut tables: Vec<DTable> = Vec::with_capacity(table_blocks.len());
    for (i, t) in table_blocks.iter().enumerate() {
        let schema = t
            .ident
            .schema
            .as_ref()
            .map(|s| s.to_string.clone())
            .unwrap_or_else(|| dbml_rs::DEFAULT_SCHEMA.to_owned());
        let name = t.ident.name.to_string.clone();
        lookup.insert(format!("{schema}.{name}"), i);
        lookup.entry(name.clone()).or_insert(i);
        if let Some(alias) = &t.ident.alias {
            lookup.insert(alias.to_string.clone(), i);
        }

        let columns = t
            .cols
            .iter()
            .map(|c| {
                let s = c.settings.as_ref();
                let is_pk = s.is_some_and(|s| s.is_pk);
                DColumn {
                    name: c.name.to_string.clone(),
                    ty: c.r#type.raw.clone(),
                    is_pk,
                    is_fk: false,
                    not_null: is_pk
                        || s.is_some_and(|s| s.nullable == Some(Nullable::NotNull)),
                }
            })
            .collect();

        tables.push(DTable {
            key: format!("{schema}.{name}"),
            name: if schema == dbml_rs::DEFAULT_SCHEMA {
                name
            } else {
                format!("{schema}.{name}")
            },
            header_color: t
                .settings
                .as_ref()
                .and_then(|s| attr_color(&s.attributes, "headercolor")),
            group: None,
            columns,
        });
    }

    let resolve = |ident: &RefIdent| -> Option<usize> {
        let name = &ident.table.to_string;
        match &ident.schema {
            Some(s) => lookup.get(&format!("{}.{}", s.to_string, name)).copied(),
            None => lookup.get(name).copied(),
        }
    };

    let mut refs: Vec<DRef> = Vec::new();
    let mut add_ref = |tables: &mut Vec<DTable>,
                       from: (usize, Option<usize>),
                       to: (usize, Option<usize>),
                       rel: DRel| {
        for (ti, ci) in [from, to] {
            if let Some(ci) = ci
                && let Some(col) = tables[ti].columns.get_mut(ci)
            {
                col.is_fk = true;
            }
        }
        refs.push(DRef { from, to, rel });
    };

    let col_index = |tables: &[DTable], ti: usize, col: &str| -> Option<usize> {
        tables[ti].columns.iter().position(|c| c.name == col)
    };

    // Standalone Ref blocks (composite refs expand pairwise).
    for b in &ast.blocks {
        let TopLevelBlock::Ref(r) = b else { continue };
        let (Some(li), Some(ri)) = (resolve(&r.lhs), resolve(&r.rhs)) else {
            continue;
        };
        let rel = drel(&r.rel);
        let pairs = r.lhs.compositions.iter().zip(r.rhs.compositions.iter());
        let mut any = false;
        for (lc, rc) in pairs {
            any = true;
            let fc = col_index(&tables, li, &lc.to_string);
            let tc = col_index(&tables, ri, &rc.to_string);
            add_ref(&mut tables, (li, fc), (ri, tc), rel);
        }
        if !any {
            add_ref(&mut tables, (li, None), (ri, None), rel);
        }
    }

    // Inline column refs: lhs is the enclosing column.
    for (ti, t) in table_blocks.iter().enumerate() {
        for (ci, c) in t.cols.iter().enumerate() {
            let Some(settings) = &c.settings else { continue };
            for inline in &settings.refs {
                let Some(ri) = resolve(&inline.rhs) else { continue };
                let tc = inline
                    .rhs
                    .compositions
                    .first()
                    .and_then(|c| col_index(&tables, ri, &c.to_string));
                add_ref(&mut tables, (ti, Some(ci)), (ri, tc), drel(&inline.rel));
            }
        }
    }

    let mut groups: Vec<DGroup> = Vec::new();
    for b in &ast.blocks {
        let TopLevelBlock::TableGroup(g) = b else { continue };
        let gi = groups.len();
        let mut members = Vec::new();
        for item in &g.items {
            let ident = RefIdent {
                span_range: item.span_range.clone(),
                schema: item.schema.clone(),
                table: item.ident_alias.clone(),
                compositions: Vec::new(),
            };
            if let Some(ti) = resolve(&ident) {
                members.push(ti);
                tables[ti].group = Some(gi);
            }
        }
        groups.push(DGroup {
            name: g.ident.to_string.clone(),
            color: g
                .settings
                .as_ref()
                .and_then(|s| attr_color(&s.attributes, "color")),
            tables: members,
        });
    }

    Ok(DiagramModel { tables, refs, groups })
}

fn drel(rel: &Relation) -> DRel {
    match rel {
        Relation::One2One | Relation::Undef => DRel::One2One,
        Relation::One2Many => DRel::One2Many,
        Relation::Many2One => DRel::Many2One,
        Relation::Many2Many => DRel::Many2Many,
    }
}

fn attr_color(attributes: &[Attribute], key: &str) -> Option<egui::Color32> {
    attributes.iter().find_map(|a| {
        if !a.key.to_string.eq_ignore_ascii_case(key) {
            return None;
        }
        match &a.value.as_ref()?.value {
            Value::HexColor(hex) => parse_hex_color(hex),
            _ => None,
        }
    })
}

fn parse_hex_color(hex: &str) -> Option<egui::Color32> {
    let h = hex.trim_start_matches('#');
    let (r, g, b) = match h.len() {
        3 => {
            let v = u16::from_str_radix(h, 16).ok()? as u32;
            let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
            ((r * 17) as u8, (g * 17) as u8, (b * 17) as u8)
        }
        6 => {
            let v = u32::from_str_radix(h, 16).ok()?;
            (((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8)
        }
        _ => return None,
    };
    Some(egui::Color32::from_rgb(r, g, b))
}

// ---------------------------------------------------------------------------
// DBML generation from live-database introspection ("View as DBML").
// ---------------------------------------------------------------------------

use crate::db::structure::{IdentityKind, TableStructure};

/// Markers delimiting the machine-owned region of a connection-backed DBML
/// document. Refresh replaces everything between them; everything outside
/// (TableGroups, notes, draft tables) belongs to the user and is preserved.
pub const GEN_BEGIN: &str = "//==== dbTool generated — replaced on refresh, do not edit ====";
pub const GEN_END: &str = "//==== end generated — everything below is yours ====";

fn table_schemas(tables: &[TableStructure]) -> Vec<&str> {
    let mut schemas: Vec<&str> = tables.iter().map(|t| t.schema.as_str()).collect();
    schemas.sort_unstable();
    schemas.dedup();
    schemas
}

/// The marker-wrapped generated region: table definitions + FK refs.
fn generated_region(tables: &[TableStructure], source_name: &str) -> String {
    use std::fmt::Write;

    let qualified = table_schemas(tables).len() > 1;

    let table_ident = |t: &TableStructure| -> String {
        if qualified {
            format!("{}.{}", quote_ident(&t.schema), quote_ident(&t.name))
        } else {
            quote_ident(&t.name)
        }
    };

    let mut out = String::new();
    let _ = writeln!(out, "{GEN_BEGIN}");
    let _ = writeln!(
        out,
        "// {} table(s) introspected from {source_name}\n",
        tables.len()
    );

    for t in tables {
        let pk_cols: &[String] = t.primary_key.as_ref().map(|k| k.columns.as_slice()).unwrap_or(&[]);
        let _ = writeln!(out, "Table {} {{", table_ident(t));
        for c in &t.columns {
            let mut attrs: Vec<String> = Vec::new();
            let is_pk = pk_cols.contains(&c.name);
            if is_pk {
                attrs.push("pk".into());
            }
            if c.identity != IdentityKind::None {
                attrs.push("increment".into());
            }
            if c.not_null && !is_pk {
                attrs.push("not null".into());
            }
            if let Some(d) = default_attr(c.default.as_deref(), c.identity) {
                attrs.push(d);
            }
            let attrs = if attrs.is_empty() {
                String::new()
            } else {
                format!(" [{}]", attrs.join(", "))
            };
            let _ = writeln!(
                out,
                "  {} {}{attrs}",
                quote_ident(&c.name),
                quote_type(&c.type_name)
            );
        }
        let _ = writeln!(out, "}}\n");
    }

    let mut wrote_ref = false;
    for t in tables {
        for fk in &t.foreign_keys {
            if fk.columns.is_empty() || fk.columns.len() != fk.ref_columns.len() {
                continue;
            }
            let target = if qualified {
                format!("{}.{}", quote_ident(&fk.ref_schema), quote_ident(&fk.ref_table))
            } else {
                quote_ident(&fk.ref_table)
            };
            let _ = writeln!(
                out,
                "Ref: {}.{} > {}.{}",
                table_ident(t),
                composition(&fk.columns),
                target,
                composition(&fk.ref_columns)
            );
            wrote_ref = true;
        }
    }
    if wrote_ref {
        out.push('\n');
    }
    let _ = writeln!(out, "{GEN_END}");
    out
}

/// Starter content for the user-owned area of a fresh document.
fn seed_user_area(tables: &[TableStructure]) -> String {
    use std::fmt::Write;

    let schemas = table_schemas(tables);
    let qualified = schemas.len() > 1;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n// Your area — TableGroups, notes, and draft tables here survive refreshes."
    );
    if qualified {
        out.push('\n');
        for schema in &schemas {
            let _ = writeln!(out, "TableGroup {} {{", quote_ident(schema));
            for t in tables.iter().filter(|t| t.schema == *schema) {
                let _ = writeln!(
                    out,
                    "  {}.{}",
                    quote_ident(&t.schema),
                    quote_ident(&t.name)
                );
            }
            let _ = writeln!(out, "}}\n");
        }
    } else {
        let by_name: HashMap<&str, usize> =
            tables.iter().enumerate().map(|(i, t)| (t.name.as_str(), i)).collect();
        let names: Vec<String> = tables.iter().map(|t| t.name.clone()).collect();
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for (i, t) in tables.iter().enumerate() {
            for fk in &t.foreign_keys {
                if let Some(&j) = by_name.get(fk.ref_table.as_str())
                    && j != i
                {
                    edges.push((i, j));
                }
            }
        }
        let groups = suggest_groups(&names, &edges);
        if groups.is_empty() {
            let _ = writeln!(out, "// Example:");
            let _ = writeln!(out, "// TableGroup billing [color: #1E69FD] {{");
            let _ = writeln!(out, "//   invoice");
            let _ = writeln!(out, "//   payment");
            let _ = writeln!(out, "// }}");
        } else {
            out.push('\n');
            let mut taken = HashSet::new();
            out.push_str(&render_table_groups(
                &groups,
                |i| quote_ident(&tables[i].name),
                &mut taken,
            ));
        }
    }
    out
}

/// A complete fresh connection-backed document: generated region + seed
/// user area (one TableGroup per schema when several schemas exist).
pub fn generate_document(tables: &[TableStructure], source_name: &str) -> String {
    format!(
        "{}{}",
        generated_region(tables, source_name),
        seed_user_area(tables)
    )
}

/// Refresh `existing` with newly introspected structure: replace the region
/// between GEN_BEGIN/GEN_END, preserving everything before and after it
/// byte-for-byte. A document without markers (or with them out of order) is
/// treated entirely as user content and kept below a fresh region.
pub fn merge_generated(
    existing: &str,
    tables: &[TableStructure],
    source_name: &str,
) -> String {
    let region = generated_region(tables, source_name);
    let begin = existing.lines().position(|l| l.trim() == GEN_BEGIN);
    let end = existing.lines().position(|l| l.trim() == GEN_END);
    match (begin, end) {
        (Some(b), Some(e)) if b <= e => {
            let lines: Vec<&str> = existing.lines().collect();
            let before = lines[..b].join("\n");
            let after = lines[e + 1..].join("\n");
            let mut out = String::new();
            if !before.trim().is_empty() {
                out.push_str(&before);
                out.push('\n');
            }
            out.push_str(&region);
            out.push_str(&after);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out
        }
        _ => format!("{region}\n{existing}"),
    }
}

// ---------------------------------------------------------------------------
// TableGroup suggestion: name-prefix families + FK skeleton attachment.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct SuggestedGroup {
    /// Bare name of the seed table, doubling as the group name.
    pub name: String,
    /// Indices into the input `names` slice, seed first.
    pub tables: Vec<usize>,
}

/// Lowercased word tokens of a table name (splits snake_case, kebab-case
/// and camelCase).
fn name_tokens(name: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    for ch in name.chars() {
        if matches!(ch, '_' | '-' | ' ' | '.') {
            if !cur.is_empty() {
                toks.push(std::mem::take(&mut cur).to_lowercase());
            }
        } else if ch.is_uppercase() && cur.chars().last().is_some_and(char::is_lowercase) {
            toks.push(std::mem::take(&mut cur).to_lowercase());
            cur.push(ch);
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        toks.push(cur.to_lowercase());
    }
    toks
}

/// Cluster tables into suggested groups. Two signals:
///
/// 1. Name prefixes seed families: `order_item` (and `orderItemTax`, via the
///    longest matching prefix) belong to `order`.
/// 2. FK attachment: a still-loose table joins the family holding a strict
///    majority of its FK edges, or a plurality when it also shares a name
///    token with the family's seed. Runs a few passes so attachments chain.
///
/// Two loose tables joined only by a FK never form a group on their own —
/// that keeps the whole schema from collapsing into one blob. Tables that
/// never cluster are simply absent from the result.
pub fn suggest_groups(names: &[String], edges: &[(usize, usize)]) -> Vec<SuggestedGroup> {
    let n = names.len();
    let toks: Vec<Vec<String>> = names.iter().map(|s| name_tokens(s)).collect();

    // Longest strict token-prefix parent, resolved to its root seed
    // (token count strictly decreases along parents, so this terminates).
    let mut parent: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        for j in 0..n {
            if i != j
                && !toks[j].is_empty()
                && toks[i].len() > toks[j].len()
                && toks[i][..toks[j].len()] == toks[j][..]
                && parent[i].is_none_or(|p| toks[j].len() > toks[p].len())
            {
                parent[i] = Some(j);
            }
        }
    }
    let root = |mut i: usize| -> usize {
        while let Some(p) = parent[i] {
            i = p;
        }
        i
    };
    let mut family: Vec<Option<usize>> = (0..n).map(|i| parent[i].map(|_| root(i))).collect();

    let is_seed = |family: &[Option<usize>], i: usize| family.iter().any(|f| *f == Some(i));

    for _ in 0..3 {
        let mut changed = false;
        for i in 0..n {
            if family[i].is_some() || is_seed(&family, i) {
                continue;
            }
            let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
            let mut total = 0usize;
            for &(a, b) in edges {
                let other = match (a == i, b == i) {
                    (true, false) => b,
                    (false, true) => a,
                    _ => continue,
                };
                total += 1;
                if let Some(r) = family[other] {
                    *counts.entry(r).or_default() += 1;
                } else if is_seed(&family, other) {
                    *counts.entry(other).or_default() += 1;
                }
            }
            // Ties keep the smallest root: deterministic output.
            let mut best: Option<(usize, usize)> = None; // (count, root)
            for (&r, &c) in &counts {
                if best.is_none_or(|(bc, _)| c > bc) {
                    best = Some((c, r));
                }
            }
            let Some((cnt, r)) = best else { continue };
            let similar = toks[i].iter().any(|t| toks[r].contains(t));
            if cnt * 2 > total || similar {
                family[i] = Some(r);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut seeds: Vec<usize> = family.iter().flatten().copied().collect();
    seeds.sort_unstable();
    seeds.dedup();
    seeds
        .into_iter()
        .map(|s| {
            let mut tables = vec![s];
            tables.extend((0..n).filter(|&i| family[i] == Some(s)));
            SuggestedGroup { name: names[s].clone(), tables }
        })
        .collect()
}

/// The table part of a "schema.name" layout key.
pub fn bare_table_name(key: &str) -> &str {
    key.split_once('.').map_or(key, |(_, n)| n)
}

/// A `TableGroup` member reference for a layout key: bare for the default
/// schema, schema-qualified otherwise.
pub fn table_ref_from_key(key: &str) -> String {
    match key.split_once('.') {
        Some((schema, name)) if schema == dbml_rs::DEFAULT_SCHEMA => quote_ident(name),
        Some((schema, name)) => format!("{}.{}", quote_ident(schema), quote_ident(name)),
        None => quote_ident(key),
    }
}

/// Render suggested groups as TableGroup blocks. `ident` turns a table index
/// into its DBML reference; `taken` holds names already in use (suffixed on
/// collision) and is updated with the names actually emitted.
pub fn render_table_groups(
    groups: &[SuggestedGroup],
    ident: impl Fn(usize) -> String,
    taken: &mut HashSet<String>,
) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    for g in groups {
        let mut name = g.name.clone();
        let mut k = 2;
        while !taken.insert(name.clone()) {
            name = format!("{}_{k}", g.name);
            k += 1;
        }
        let _ = writeln!(out, "TableGroup {} {{", quote_ident(&name));
        for &ti in &g.tables {
            let _ = writeln!(out, "  {}", ident(ti));
        }
        let _ = writeln!(out, "}}\n");
    }
    out
}

fn composition(cols: &[String]) -> String {
    if cols.len() == 1 {
        quote_ident(&cols[0])
    } else {
        let quoted: Vec<String> = cols.iter().map(|c| quote_ident(c)).collect();
        format!("({})", quoted.join(", "))
    }
}

fn quote_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && !name.chars().next().unwrap().is_ascii_digit()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain { name.to_owned() } else { format!("\"{name}\"") }
}

/// Compact verbose SQL type names; quote anything the DBML grammar can't
/// take bare (spaces require the quoted-type form).
fn quote_type(ty: &str) -> String {
    let lower = ty.to_ascii_lowercase();
    let (base, args) = match lower.split_once('(') {
        Some((b, rest)) => (b.trim_end().to_owned(), Some(rest)),
        None => (lower.clone(), None),
    };
    let compact = match base.as_str() {
        "character varying" => Some("varchar"),
        "character" => Some("char"),
        "timestamp without time zone" => Some("timestamp"),
        "timestamp with time zone" => Some("timestamptz"),
        "time without time zone" => Some("time"),
        "time with time zone" => Some("timetz"),
        "double precision" => Some("float8"),
        "bit varying" => Some("varbit"),
        _ => None,
    };
    let rebuilt = match (compact, args) {
        (Some(c), Some(rest)) => format!("{c}({rest}"),
        (Some(c), None) => c.to_owned(),
        (None, _) => ty.to_owned(),
    };
    if rebuilt.contains(' ') { format!("\"{rebuilt}\"") } else { rebuilt }
}

fn default_attr(default: Option<&str>, identity: IdentityKind) -> Option<String> {
    if identity != IdentityKind::None {
        return None;
    }
    let d = default?.trim();
    if d.is_empty() || d.len() > 60 || d.contains('`') {
        return None;
    }
    if d.parse::<f64>().is_ok() || matches!(d.to_ascii_lowercase().as_str(), "true" | "false" | "null") {
        Some(format!("default: {}", d.to_ascii_lowercase()))
    } else {
        Some(format!("default: `{d}`"))
    }
}

/// Pest's Display is a multiline caret diagram; keep the final "= reason" line.
fn compact_pest_message(full: &str) -> String {
    full.lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("= "))
        .map(str::to_owned)
        .unwrap_or_else(|| full.lines().next().unwrap_or("parse error").to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../examples/sample.dbml");

    #[test]
    fn parses_sample() {
        let m = parse(SAMPLE).expect("sample should parse");
        assert_eq!(m.tables.len(), 5);
        assert_eq!(m.groups.len(), 2);
        // 1 inline + composite (2 pairs) + 2 plain standalone = 5 edges
        assert_eq!(m.refs.len(), 5);
    }

    #[test]
    fn group_membership_and_color() {
        let m = parse(SAMPLE).unwrap();
        let commerce = &m.groups[0];
        assert_eq!(commerce.name, "commerce");
        assert_eq!(commerce.color, Some(egui::Color32::from_rgb(0x1e, 0x69, 0xfd)));
        assert_eq!(commerce.tables.len(), 3);
        let identity = &m.groups[1];
        assert_eq!(identity.color, None);
        let users = m.tables.iter().find(|t| t.name == "users").unwrap();
        assert_eq!(users.group, Some(1));
        assert_eq!(users.key, "public.users");
    }

    #[test]
    fn composite_ref_expands_pairwise() {
        let m = parse(SAMPLE).unwrap();
        let orders = m.tables.iter().position(|t| t.name == "orders").unwrap();
        let users = m.tables.iter().position(|t| t.name == "users").unwrap();
        let pairs: Vec<_> = m
            .refs
            .iter()
            .filter(|r| r.from.0 == orders && r.to.0 == users)
            .collect();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.iter().all(|r| r.from.1.is_some() && r.to.1.is_some()));
    }

    #[test]
    fn column_flags() {
        let m = parse(SAMPLE).unwrap();
        let users = m.tables.iter().find(|t| t.name == "users").unwrap();
        let id = users.columns.iter().find(|c| c.name == "id").unwrap();
        assert!(id.is_pk && id.not_null);
        let region = users.columns.iter().find(|c| c.name == "region").unwrap();
        assert!(!region.is_pk && region.not_null);
        let orders = m.tables.iter().find(|t| t.name == "orders").unwrap();
        let user_id = orders.columns.iter().find(|c| c.name == "user_id").unwrap();
        assert!(user_id.is_fk);
    }

    #[test]
    fn error_reports_line() {
        let e = parse("Table users {\n  id int [pk\n}").unwrap_err();
        assert!(e.line.is_some());
        assert!(!e.message.is_empty());
    }

    #[test]
    fn generated_dbml_round_trips_through_parse() {
        use crate::db::structure::{ColumnInfo, FkInfo, KeyInfo};

        let col = |name: &str, ty: &str, not_null: bool| ColumnInfo {
            name: name.into(),
            type_name: ty.into(),
            not_null,
            default: None,
            identity: IdentityKind::None,
            generated: false,
            default_constraint: None,
        };
        let users = TableStructure {
            schema: "auth".into(),
            name: "users".into(),
            columns: vec![
                col("id", "integer", true),
                col("email", "character varying(255)", true),
                col("joined at", "timestamp without time zone", false),
            ],
            primary_key: Some(KeyInfo { name: None, columns: vec!["id".into()] }),
            foreign_keys: vec![],
            indexes: vec![],
            checks: vec![],
        };
        let orders = TableStructure {
            schema: "shop".into(),
            name: "orders".into(),
            columns: vec![col("id", "integer", true), col("user_id", "integer", true)],
            primary_key: Some(KeyInfo { name: None, columns: vec!["id".into()] }),
            foreign_keys: vec![FkInfo {
                name: "fk_user".into(),
                columns: vec!["user_id".into()],
                ref_schema: "auth".into(),
                ref_table: "users".into(),
                ref_columns: vec!["id".into()],
                on_delete: String::new(),
                on_update: String::new(),
            }],
            indexes: vec![],
            checks: vec![],
        };

        let src = generate_document(&[users.clone(), orders.clone()], "test-db");
        let m = parse(&src).expect("generated DBML should parse");
        assert_eq!(m.tables.len(), 2);
        assert_eq!(m.refs.len(), 1);
        assert_eq!(m.groups.len(), 2, "one TableGroup per schema");

        // Refresh merge: user edits below the marker survive, region is replaced.
        let edited = format!(
            "{src}\nTableGroup mine [color: #ff0000] {{\n  \"auth\".users\n}}\n\nTable draft_notes {{\n  id integer [pk]\n}}\n"
        );
        let merged = merge_generated(&edited, &[users, orders], "test-db");
        assert!(merged.contains("TableGroup mine"), "user group preserved");
        assert!(merged.contains("draft_notes"), "draft table preserved");
        assert_eq!(
            merged.matches(GEN_BEGIN).count(),
            1,
            "exactly one generated region after merge"
        );
        let m2 = parse(&merged).expect("merged DBML should parse");
        assert_eq!(m2.tables.len(), 3, "2 real + 1 draft");
        assert!(m2.groups.iter().any(|g| g.name == "mine"));

        // No markers → whole document treated as user content, region prepended.
        let merged3 = merge_generated("TableGroup g { users }\n", &[], "db");
        assert!(merged3.contains("TableGroup g"));
        assert!(merged3.contains(GEN_BEGIN));
        // Quoted ident and verbose type survived the trip.
        let u = m.tables.iter().find(|t| t.name.contains("users")).unwrap();
        assert!(u.columns.iter().any(|c| c.name == "joined at"));
        assert!(u.columns.iter().any(|c| c.ty.contains("varchar")));
        // The ref resolved to real column indices.
        assert!(m.refs[0].from.1.is_some() && m.refs[0].to.1.is_some());
    }

    #[test]
    fn generate_single_schema_stays_unqualified() {
        use crate::db::structure::KeyInfo;
        let t = TableStructure {
            schema: "public".into(),
            name: "users".into(),
            columns: vec![],
            primary_key: Some(KeyInfo { name: None, columns: vec![] }),
            foreign_keys: vec![],
            indexes: vec![],
            checks: vec![],
        };
        let src = generate_document(&[t], "db");
        assert!(src.contains("Table users {"));
        let m = parse(&src).unwrap();
        assert!(m.groups.is_empty(), "single schema seeds no real groups");
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn suggest_groups_prefix_families() {
        let n = names(&["order", "order_item", "orderItemTax", "customer"]);
        let groups = suggest_groups(&n, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "order");
        assert_eq!(groups[0].tables, vec![0, 1, 2]);
    }

    #[test]
    fn suggest_groups_fk_majority_attaches() {
        // shipment's only FK points into the order family → it joins;
        // customer has no edges and stays loose.
        let n = names(&["order", "order_item", "shipment", "customer"]);
        let groups = suggest_groups(&n, &[(1, 0), (2, 0)]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tables, vec![0, 1, 2]);
    }

    #[test]
    fn suggest_groups_no_majority_no_name_tie_stays_loose() {
        // audit references both families evenly and shares no tokens.
        let n = names(&["order", "order_item", "user", "user_role", "audit"]);
        let groups = suggest_groups(&n, &[(4, 0), (4, 2)]);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| !g.tables.contains(&4)));
    }

    #[test]
    fn suggest_groups_fk_pair_alone_is_not_a_group() {
        let n = names(&["invoice", "customer"]);
        assert!(suggest_groups(&n, &[(0, 1)]).is_empty());
    }

    #[test]
    fn render_table_groups_suffixes_taken_names() {
        let groups = vec![SuggestedGroup { name: "order".into(), tables: vec![0, 1] }];
        let mut taken: HashSet<String> = ["order".to_owned()].into();
        let n = names(&["order", "order_item"]);
        let out = render_table_groups(&groups, |i| n[i].clone(), &mut taken);
        assert!(out.contains("TableGroup order_2 {"));
        assert!(out.contains("  order_item"));
    }

    #[test]
    fn seed_user_area_pregroups_single_schema() {
        use crate::db::structure::{ColumnInfo, FkInfo, KeyInfo};
        let table = |name: &str, fks: Vec<FkInfo>| TableStructure {
            schema: "public".into(),
            name: name.into(),
            columns: vec![ColumnInfo {
                name: "id".into(),
                type_name: "integer".into(),
                not_null: true,
                default: None,
                identity: IdentityKind::None,
                generated: false,
                default_constraint: None,
            }],
            primary_key: Some(KeyInfo { name: None, columns: vec!["id".into()] }),
            foreign_keys: fks,
            indexes: vec![],
            checks: vec![],
        };
        let fk = |to: &str| FkInfo {
            name: String::new(),
            columns: vec!["id".into()],
            ref_schema: "public".into(),
            ref_table: to.into(),
            ref_columns: vec!["id".into()],
            on_delete: String::new(),
            on_update: String::new(),
        };
        let src = generate_document(
            &[
                table("order", vec![]),
                table("order_item", vec![fk("order")]),
                table("shipment", vec![fk("order")]),
            ],
            "db",
        );
        let m = parse(&src).expect("seeded document should parse");
        assert_eq!(m.groups.len(), 1);
        assert_eq!(m.groups[0].name, "order");
        assert_eq!(m.groups[0].tables.len(), 3);
    }

    #[test]
    fn table_ref_from_key_qualifies_non_default_schema() {
        assert_eq!(table_ref_from_key("public.users"), "users");
        assert_eq!(table_ref_from_key("auth.users"), "auth.users");
        assert_eq!(table_ref_from_key("auth.user table"), "auth.\"user table\"");
    }

    #[test]
    fn alias_resolves_in_refs_and_groups() {
        let src = "Table users as U {\n  id int [pk]\n}\nTable posts {\n  id int [pk]\n  author int\n}\nRef: posts.author > U.id\nTableGroup g1 {\n  U\n  posts\n}\n";
        let m = parse(src).expect("alias sample should parse");
        assert_eq!(m.refs.len(), 1);
        assert_eq!(m.groups[0].tables.len(), 2);
    }
}
