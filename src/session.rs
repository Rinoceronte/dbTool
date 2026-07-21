//! Session persistence: open query tabs survive app restarts, DataGrip-style
//! durable consoles. Saved on exit, restored (detached) on launch, and
//! re-attached to their connection when the profile connects.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Clone)]
pub struct SavedQueryTab {
    pub profile_id: Uuid,
    /// Database the tab's connection was attached to (profiles can have
    /// several database connections open).
    pub database: String,
    pub title: String,
    pub sql: String,
    #[serde(default)]
    pub file_path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct SavedSession {
    pub tabs: Vec<SavedQueryTab>,
    /// Index into `tabs` of the tab that was active.
    #[serde(default)]
    pub active: Option<usize>,
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("dbTool").join("session.json"))
}

pub fn save(session: &SavedSession) {
    let Some(p) = path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(data) = serde_json::to_string_pretty(session) {
        let _ = std::fs::write(p, data);
    }
}

pub fn load() -> SavedSession {
    let Some(p) = path() else { return SavedSession::default() };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}
