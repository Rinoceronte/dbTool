//! ResultSet → delimited file rendering for "Export data".

use crate::db::Value;

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub delimiter: u8,
    pub include_header: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self { delimiter: b',', include_header: true }
    }
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
