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
    /// Additional server databases toggled on for this connection; each opens
    /// its own connection and appears as a node in the sidebar tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_databases: Vec<String>,
    // SSH tunnel (system ssh; keys/agent/~/.ssh/config apply as usual).
    #[serde(default)]
    pub ssh_enabled: bool,
    #[serde(default)]
    pub ssh_host: String,
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    #[serde(default)]
    pub ssh_user: String,
    /// Optional identity file; empty = ssh defaults.
    #[serde(default)]
    pub ssh_key: String,
    /// Tint this connection red everywhere so prod is unmistakable.
    #[serde(default)]
    pub production: bool,
    /// Refuse statements that can write, plus all editor/DDL commits.
    #[serde(default)]
    pub read_only: bool,
}

fn default_ssh_port() -> u16 {
    22
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
            enabled_databases: Vec::new(),
            ssh_enabled: false,
            ssh_host: String::new(),
            ssh_port: 22,
            ssh_user: String::new(),
            ssh_key: String::new(),
            production: false,
            read_only: false,
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
            ssh: self.ssh_enabled.then(|| crate::db::SshTunnelParams {
                host: self.ssh_host.clone(),
                port: self.ssh_port,
                user: self.ssh_user.clone(),
                key_path: self.ssh_key.clone(),
            }),
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
