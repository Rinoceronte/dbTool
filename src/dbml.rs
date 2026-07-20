//! Parse DBML source into an owned diagram model, independent of dbml-rs AST lifetimes.

use std::collections::HashMap;

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
    fn alias_resolves_in_refs_and_groups() {
        let src = "Table users as U {\n  id int [pk]\n}\nTable posts {\n  id int [pk]\n  author int\n}\nRef: posts.author > U.id\nTableGroup g1 {\n  U\n  posts\n}\n";
        let m = parse(src).expect("alias sample should parse");
        assert_eq!(m.refs.len(), 1);
        assert_eq!(m.groups[0].tables.len(), 2);
    }
}
