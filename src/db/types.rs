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
}

impl ResultSet {
    pub fn empty() -> Self {
        Self { columns: vec![], rows: vec![], rows_affected: None }
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
