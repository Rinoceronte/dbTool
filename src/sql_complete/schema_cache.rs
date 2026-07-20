use std::collections::BTreeMap;

use crate::db::{DbMeta, ForeignKey, TableMeta};

pub struct SchemaCache {
    meta: DbMeta,
    schemas: Vec<String>,
    by_schema: BTreeMap<String, Vec<usize>>,     // schema -> table indices
    by_name: BTreeMap<String, Vec<usize>>,       // lowercase name -> table indices
    by_full: BTreeMap<(String, String), usize>,  // (lc_schema, lc_name) -> index
}

impl SchemaCache {
    pub fn new(meta: DbMeta) -> Self {
        let mut by_schema: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut by_full: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut schemas: Vec<String> = Vec::new();
        for (i, t) in meta.tables.iter().enumerate() {
            by_schema.entry(t.schema.clone()).or_default().push(i);
            by_name.entry(t.name.to_ascii_lowercase()).or_default().push(i);
            by_full.insert((t.schema.to_ascii_lowercase(), t.name.to_ascii_lowercase()), i);
            if !schemas.contains(&t.schema) {
                schemas.push(t.schema.clone());
            }
        }
        schemas.sort();
        Self { meta, schemas, by_schema, by_name, by_full }
    }

    pub fn default_schema(&self) -> &str {
        &self.meta.default_schema
    }

    pub fn schemas(&self) -> &[String] {
        &self.schemas
    }

    pub fn tables_in_schema(&self, schema: &str) -> Vec<&TableMeta> {
        self.by_schema
            .get(schema)
            .map(|idxs| idxs.iter().map(|&i| &self.meta.tables[i]).collect())
            .unwrap_or_default()
    }

    pub fn all_tables(&self) -> &[TableMeta] {
        &self.meta.tables
    }

    /// Resolve a table by `(schema, name)`. If `schema` is `None`, tries the default schema first,
    /// then falls back to any schema with exactly one match.
    pub fn resolve_table(&self, schema: Option<&str>, name: &str) -> Option<&TableMeta> {
        let lc_name = name.to_ascii_lowercase();
        if let Some(s) = schema {
            return self
                .by_full
                .get(&(s.to_ascii_lowercase(), lc_name))
                .map(|&i| &self.meta.tables[i]);
        }
        // Prefer default schema.
        let default_lc = self.meta.default_schema.to_ascii_lowercase();
        if let Some(&i) = self.by_full.get(&(default_lc, lc_name.clone())) {
            return Some(&self.meta.tables[i]);
        }
        if let Some(idxs) = self.by_name.get(&lc_name) {
            if idxs.len() == 1 {
                return Some(&self.meta.tables[idxs[0]]);
            }
        }
        None
    }

    /// Foreign keys pointing FROM this table.
    pub fn outgoing_fks<'a>(&'a self, schema: &str, table: &str) -> &'a [ForeignKey] {
        match self.resolve_table(Some(schema), table) {
            Some(t) => &t.foreign_keys,
            None => &[],
        }
    }

    /// Foreign keys pointing TO this table (collected by scanning every table).
    pub fn incoming_fks(&self, schema: &str, table: &str) -> Vec<(&TableMeta, &ForeignKey)> {
        let lc_s = schema.to_ascii_lowercase();
        let lc_t = table.to_ascii_lowercase();
        let mut out = Vec::new();
        for t in &self.meta.tables {
            for fk in &t.foreign_keys {
                if fk.to_schema.to_ascii_lowercase() == lc_s
                    && fk.to_table.to_ascii_lowercase() == lc_t
                {
                    out.push((t, fk));
                }
            }
        }
        out
    }
}
