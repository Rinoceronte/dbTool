use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AuthStatus {
    #[serde(rename = "loggedIn", default)]
    pub logged_in: bool,
    #[serde(rename = "authMethod", default)]
    pub auth_method: Option<String>,
    #[serde(rename = "apiProvider", default)]
    pub api_provider: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(rename = "orgName", default)]
    pub org_name: Option<String>,
    #[serde(rename = "subscriptionType", default)]
    pub subscription_type: Option<String>,
}

/// Run `claude auth status --json` and parse the result.
pub async fn probe_status() -> anyhow::Result<AuthStatus> {
    use tokio::process::Command;
    let output = Command::new("claude")
        .arg("auth")
        .arg("status")
        .arg("--json")
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to spawn `claude` (is it on PATH?): {e}"))?;
    // Exit code is non-zero when logged out on some builds; trust stdout regardless.
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude auth status produced no output: {}", stderr.trim());
    }
    let status: AuthStatus = serde_json::from_str(stdout.trim())
        .map_err(|e| anyhow::anyhow!("failed to parse auth status: {e}; raw: {stdout}"))?;
    Ok(status)
}

pub struct LoginOutcome {
    pub success: bool,
    pub output: String,
}

/// Run `claude auth login`, optionally with `--console` (API-console billing)
/// or `--claudeai` (default, claude.ai subscription).
pub async fn run_login(use_console: bool) -> anyhow::Result<LoginOutcome> {
    use std::process::Stdio;
    use tokio::process::Command;
    let mut cmd = Command::new("claude");
    cmd.arg("auth").arg("login");
    if use_console {
        cmd.arg("--console");
    } else {
        cmd.arg("--claudeai");
    }
    // Close stdin so the process can't block waiting for input; if it needs a
    // prompt we want it to error quickly rather than deadlock the UI.
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to spawn `claude auth login`: {e}"))?;
    let mut merged = String::new();
    merged.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        if !merged.is_empty() && !merged.ends_with('\n') {
            merged.push('\n');
        }
        merged.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok(LoginOutcome {
        success: output.status.success(),
        output: merged.trim().to_string(),
    })
}

pub async fn run_logout() -> anyhow::Result<String> {
    use std::process::Stdio;
    use tokio::process::Command;
    let output = Command::new("claude")
        .arg("auth")
        .arg("logout")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to spawn `claude auth logout`: {e}"))?;
    let mut merged = String::new();
    merged.push_str(&String::from_utf8_lossy(&output.stdout));
    if !output.stderr.is_empty() {
        if !merged.is_empty() && !merged.ends_with('\n') {
            merged.push('\n');
        }
        merged.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    if !output.status.success() {
        anyhow::bail!("claude auth logout failed: {}", merged.trim());
    }
    Ok(merged.trim().to_string())
}
