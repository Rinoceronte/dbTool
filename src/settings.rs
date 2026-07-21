//! Persisted application settings (`config_dir/dbTool/settings.json`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::ui::theme::ThemeMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeMode,
    /// Line-number gutter in SQL query editors.
    pub sql_line_numbers: bool,
    /// Line-number gutter in the DBML source pane.
    pub dbml_line_numbers: bool,
    /// Editor results stop materializing rows past this cap (min 50); the
    /// grid offers "Fetch all rows" to lift it per query.
    pub max_result_rows: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Dark,
            sql_line_numbers: true,
            dbml_line_numbers: true,
            max_result_rows: 5000,
        }
    }
}

fn settings_path() -> Result<std::path::PathBuf> {
    let base = dirs::config_dir().context("no OS config dir")?;
    Ok(base.join("dbTool").join("settings.json"))
}

/// Missing or unreadable settings fall back to defaults; never fatal.
pub fn load() -> Settings {
    settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) -> Result<()> {
    let path = settings_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(settings)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_defaults() {
        let s = Settings { theme: ThemeMode::Light, sql_line_numbers: false, ..Default::default() };
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.theme, ThemeMode::Light);
        assert!(!back.sql_line_numbers);
        assert!(back.dbml_line_numbers);
        // Unknown/empty documents fall back to defaults per field.
        let empty: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.theme, ThemeMode::Dark);
    }
}
