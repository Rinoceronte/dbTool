//! Saved SQL snippets (`config_dir/dbTool/snippets.json`) — named bits of
//! SQL insertable into any query tab.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snippet {
    pub name: String,
    pub sql: String,
}

fn path() -> Result<std::path::PathBuf> {
    let base = dirs::config_dir().context("no OS config dir")?;
    Ok(base.join("dbTool").join("snippets.json"))
}

/// Missing or unreadable files yield an empty list; never fatal.
pub fn load() -> Vec<Snippet> {
    path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(snippets: &[Snippet]) -> Result<()> {
    let path = path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(snippets)?)?;
    Ok(())
}
