//! ResultSet → delimited file rendering for "Export data".

use crate::db::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    /// One JSON array of objects (column name → value).
    Json,
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub format: ExportFormat,
    pub delimiter: u8,
    pub include_header: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self { format: ExportFormat::Csv, delimiter: b',', include_header: true }
    }
}

/// Render one row as a JSON object string keyed by column names.
pub fn row_as_json(columns: &[crate::db::Column], row: &[Value]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = columns
        .iter()
        .zip(row.iter())
        .map(|(c, v)| (c.name.clone(), v.to_json()))
        .collect();
    serde_json::Value::Object(map).to_string()
}

/// Render one value as a CSV field. NULL becomes an empty field; bytes are
/// hex-encoded (Postgres `\x` style) so binary data survives the round trip.
pub fn field_text(v: &Value) -> String {
    use std::fmt::Write as _;
    match v {
        Value::Null => String::new(),
        Value::Bytes(b) => {
            let mut s = String::with_capacity(2 + b.len() * 2);
            s.push_str("\\x");
            for byte in b {
                let _ = write!(s, "{byte:02x}");
            }
            s
        }
        other => other.display(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_is_empty_field() {
        assert_eq!(field_text(&Value::Null), "");
    }

    #[test]
    fn bytes_hex_encode() {
        assert_eq!(field_text(&Value::Bytes(vec![0xde, 0xad, 0x01])), "\\xdead01");
    }

    #[test]
    fn scalars_use_display() {
        assert_eq!(field_text(&Value::Int(42)), "42");
        assert_eq!(field_text(&Value::Text("a,b".into())), "a,b");
    }
}
