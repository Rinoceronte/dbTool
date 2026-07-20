pub mod keyring;
pub mod manager_ui;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

use crate::db::{ConnectParams, DbKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: Uuid,
    pub name: String,
    pub kind: DbKind,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    #[serde(default)]
    pub require_ssl: bool,
    /// Plaintext password fallback — written only when the OS keyring
    /// isn't available (e.g. a WSL session with no secret-service daemon).
    /// Skipped from the JSON output when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_fallback: Option<String>,
}

impl ConnectionProfile {
    pub fn new(kind: DbKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: String::new(),
            kind,
            host: "localhost".to_string(),
            port: kind.default_port(),
            database: String::new(),
            username: String::new(),
            require_ssl: false,
            password_fallback: None,
        }
    }

    pub fn to_connect_params(&self, password: String) -> ConnectParams {
        ConnectParams {
            kind: self.kind,
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            username: self.username.clone(),
            password,
            require_ssl: self.require_ssl,
        }
    }
}

pub enum PasswordStore {
    Keyring,
    ProfileFile,
}

/// Save a password, preferring the OS keyring. If the keyring fails
/// (missing daemon, etc.), store it in the profile as a plaintext fallback.
/// Caller should persist `profiles` afterwards if `ProfileFile` is returned.
pub fn save_password(profile: &mut ConnectionProfile, password: &str) -> PasswordStore {
    if password.is_empty() {
        return PasswordStore::Keyring;
    }
    match keyring::set_password(profile.id, password) {
        Ok(()) => {
            profile.password_fallback = None;
            PasswordStore::Keyring
        }
        Err(_) => {
            profile.password_fallback = Some(password.to_string());
            PasswordStore::ProfileFile
        }
    }
}

/// Load a password: keyring first, profile fallback second.
pub fn load_password(profile: &ConnectionProfile) -> Option<String> {
    if let Some(pw) = keyring::get_password(profile.id) {
        return Some(pw);
    }
    profile.password_fallback.clone()
}

pub fn delete_password(profile: &mut ConnectionProfile) {
    let _ = keyring::delete_password(profile.id);
    profile.password_fallback = None;
}

fn config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("no OS config dir")?;
    Ok(base.join("dbTool").join("connections.json"))
}

pub fn load_profiles() -> Result<Vec<ConnectionProfile>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let data = fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let profiles: Vec<ConnectionProfile> = serde_json::from_str(&data)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(profiles)
}

pub fn save_profiles(profiles: &[ConnectionProfile]) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(profiles)?;
    fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
