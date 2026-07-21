use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Json(serde_json::Value),
    Timestamp(String),
}

impl Value {
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Text(s) => s.clone(),
            Value::Bytes(b) => format!("<{} bytes>", b.len()),
            Value::Json(j) => j.to_string(),
            Value::Timestamp(s) => s.clone(),
        }
    }

    /// JSON rendering for exports. Bytes become a hex string; floats that
    /// aren't finite fall back to their text form.
    pub fn to_json(&self) -> serde_json::Value {
        use std::fmt::Write as _;
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::Number((*i).into()),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| serde_json::Value::String(f.to_string())),
            Value::Text(s) | Value::Timestamp(s) => serde_json::Value::String(s.clone()),
            Value::Bytes(b) => {
                let mut s = String::with_capacity(2 + b.len() * 2);
                s.push_str("\\x");
                for byte in b {
                    let _ = write!(s, "{byte:02x}");
                }
                serde_json::Value::String(s)
            }
            Value::Json(j) => j.clone(),
        }
    }

    pub fn from_text_input(input: &str, original: &Value) -> Value {
        if input.is_empty() && matches!(original, Value::Null) {
            return Value::Null;
        }
        if input == "NULL" {
            return Value::Null;
        }
        match original {
            Value::Null => Value::Text(input.to_string()),
            Value::Bool(_) => match input.to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" => Value::Bool(true),
                "false" | "f" | "0" | "no" => Value::Bool(false),
                _ => Value::Text(input.to_string()),
            },
            Value::Int(_) => input.parse::<i64>().map(Value::Int).unwrap_or(Value::Text(input.to_string())),
            Value::Float(_) => input.parse::<f64>().map(Value::Float).unwrap_or(Value::Text(input.to_string())),
            _ => Value::Text(input.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

#[derive(Debug, Clone)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub rows_affected: Option<u64>,
    /// The row cap cut this result short; the full set is larger.
    pub truncated: bool,
}

impl ResultSet {
    pub fn empty() -> Self {
        Self { columns: vec![], rows: vec![], rows_affected: None, truncated: false }
    }

    /// Enforce the row cap on an already-materialized set (drivers that
    /// can't stream row-by-row use this after the fact).
    pub fn apply_cap(&mut self, cap: usize) {
        if self.rows.len() > cap {
            self.rows.truncate(cap);
            self.truncated = true;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    View,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
    /// Table comment, when the dialect has one and it is set.
    pub comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ColumnSchema {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub is_primary_key: bool,
}

#[derive(Debug, Clone)]
pub struct TableSchema {
    pub columns: Vec<ColumnSchema>,
    pub primary_key: Vec<String>,
}

impl TableSchema {
    pub fn has_primary_key(&self) -> bool {
        !self.primary_key.is_empty()
    }
}

pub type PkValues = BTreeMap<String, Value>;
pub type RowChanges = BTreeMap<String, Value>;

/// User-driven narrowing of a table data page: an optional raw WHERE clause
/// (written by the user, passed through verbatim) and an optional sort column.
#[derive(Debug, Clone, Default)]
pub struct RowsFilter {
    pub where_clause: String,
    pub order_col: Option<String>,
    pub order_desc: bool,
}

impl RowsFilter {
    pub fn is_empty(&self) -> bool {
        self.where_clause.trim().is_empty() && self.order_col.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct RoutineInfo {
    pub name: String,
    /// "function" | "procedure"
    pub kind: String,
    /// Signature / return info, dialect-dependent; display only.
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct EnumInfo {
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TriggerInfo {
    pub name: String,
    pub table: String,
    /// e.g. "BEFORE INSERT OR UPDATE"; display only.
    pub detail: String,
}

/// Non-table objects of one schema, for the sidebar tree.
#[derive(Debug, Clone, Default)]
pub struct SchemaObjects {
    pub routines: Vec<RoutineInfo>,
    pub sequences: Vec<String>,
    pub enums: Vec<EnumInfo>,
    pub triggers: Vec<TriggerInfo>,
}

impl SchemaObjects {
    pub fn is_empty(&self) -> bool {
        self.routines.is_empty()
            && self.sequences.is_empty()
            && self.enums.is_empty()
            && self.triggers.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ForeignKey {
    pub from_column: String,
    pub to_schema: String,
    pub to_table: String,
    pub to_column: String,
}

#[derive(Debug, Clone)]
pub struct TableMeta {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
    pub columns: Vec<ColumnSchema>,
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, Clone)]
pub struct DbMeta {
    pub tables: Vec<TableMeta>,
    pub default_schema: String,
}
