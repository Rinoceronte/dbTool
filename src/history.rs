//! Persistent query history: one JSONL file, newest entries appended at the
//! end. Append is best-effort (history must never break a query run).

use serde::{Deserialize, Serialize};
use std::io::Write as _;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Local wall-clock time, "YYYY-MM-DD HH:MM".
    pub ts: String,
    /// Connection display name the query ran on.
    pub conn: String,
    pub sql: String,
}

fn history_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("dbTool").join("history.jsonl"))
}

pub fn append(conn: &str, sql: &str) {
    let sql = sql.trim();
    if sql.is_empty() {
        return;
    }
    let Some(path) = history_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entry = HistoryEntry {
        ts: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        conn: conn.to_owned(),
        sql: sql.to_owned(),
    };
    let Ok(line) = serde_json::to_string(&entry) else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Load the newest `limit` entries, newest first. Skips corrupt lines.
pub fn load(limit: usize) -> Vec<HistoryEntry> {
    let Some(path) = history_path() else { return Vec::new() };
    let Ok(data) = std::fs::read_to_string(&path) else { return Vec::new() };
    let mut entries: Vec<HistoryEntry> = data
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    entries.reverse();
    entries.truncate(limit);
    entries
}
