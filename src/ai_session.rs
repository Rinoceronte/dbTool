//! Agent session: spawns the `claude` CLI in `--print --output-format stream-json`
//! mode with our in-process MCP server registered, parses the streaming JSON,
//! and emits [`SessionEvent`]s the UI renders.
//!
//! Tool calls (run_select / run_statement) flow through the MCP server, which
//! gates writes on the user's approval. We only parse text deltas + the final
//! result line out of the CLI's stream — tool-related UI events come from the
//! MCP server side.

use anyhow::{Context, Result, bail};
use serde_json::Value as Json;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::mcp_server::McpServer;

pub type SessionId = Uuid;

#[derive(Debug, Clone)]
pub struct ApprovalDecision {
    pub tool_use_id: String,
    pub approved: bool,
}

/// Events a session emits to the UI. Tagged with `SessionId` by the runtime.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// New text content block started.
    TextStart,
    /// Text appended to the most-recent text block.
    TextDelta { text: String },
    /// New tool_use block started — the model is about to call this tool.
    ToolStart { tool_use_id: String, name: String },
    /// Tool input fully assembled and parsed. If `needs_approval` is true the
    /// session is now blocked waiting for an approve/reject decision; otherwise
    /// it executes immediately.
    ToolReady {
        tool_use_id: String,
        name: String,
        sql: String,
        needs_approval: bool,
    },
    /// Tool call resolved (executed or rejected). `summary` is short text for the UI.
    ToolResult { tool_use_id: String, ok: bool, summary: String },
    /// Turn finished cleanly. `claude_session_id` is the ID we'll pass to
    /// `--resume` for follow-up turns to keep conversation continuity.
    TurnDone { claude_session_id: Option<String> },
    /// Session ended with an error.
    Error { message: String },
}

pub struct SessionInputs {
    pub prompt: String,
    pub system: String,
    pub model: String,
    /// If set, the session resumes an existing claude conversation instead of
    /// starting fresh. Captured from the previous turn's `TurnDone`.
    pub resume_id: Option<String>,
}

pub async fn run_session(
    inputs: SessionInputs,
    mcp: Arc<McpServer>,
    events: mpsc::UnboundedSender<SessionEvent>,
    cancel: CancellationToken,
) {
    let result = run_inner(inputs, &mcp, &events, cancel).await;
    match result {
        Ok(claude_session_id) => {
            let _ = events.send(SessionEvent::TurnDone { claude_session_id });
        }
        Err(e) => {
            let _ = events.send(SessionEvent::Error { message: format!("{e:#}") });
        }
    }
}

async fn run_inner(
    inputs: SessionInputs,
    mcp: &Arc<McpServer>,
    events: &mpsc::UnboundedSender<SessionEvent>,
    cancel: CancellationToken,
) -> Result<Option<String>> {
    // Stash the MCP config in a temp file so claude can load it.
    let config_path = write_mcp_config(&mcp.config_json())?;

    let mut cmd = Command::new("claude");
    cmd.arg("-p").arg(&inputs.prompt)
        .arg("--output-format").arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--verbose") // required by stream-json
        .arg("--mcp-config").arg(&config_path)
        .arg("--strict-mcp-config")
        .arg("--allowed-tools").arg(McpServer::allowed_tools_arg())
        .arg("--permission-mode").arg("bypassPermissions")
        .arg("--model").arg(&inputs.model);
    if !inputs.system.is_empty() {
        cmd.arg("--append-system-prompt").arg(&inputs.system);
    }
    if let Some(id) = inputs.resume_id.as_ref() {
        cmd.arg("--resume").arg(id);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("failed to spawn `claude` (is it on PATH?)")?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;

    let mut out_reader = BufReader::new(stdout).lines();
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let stderr_buf_clone = stderr_buf.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = r.next_line().await {
            let mut buf = stderr_buf_clone.lock().await;
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(&line);
        }
    });

    let mut state = ParseState::default();

    loop {
        let line = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                bail!("cancelled");
            }
            r = out_reader.next_line() => r.context("read claude stdout")?,
        };
        let Some(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Err(e) = handle_line(&line, &mut state, events) {
            eprintln!("[ai_session] failed to handle line: {e:#} :: {line}");
        }
    }

    let status = child.wait().await.context("waiting for claude")?;
    if !status.success() {
        let err = stderr_buf.lock().await.clone();
        let trimmed = err.trim();
        if trimmed.is_empty() {
            bail!("claude exited {}", status);
        } else {
            bail!("claude exited {}: {}", status, trimmed);
        }
    }

    Ok(state.session_id)
}

#[derive(Default)]
struct ParseState {
    /// Tracks which content-block index is currently a text block, so we know
    /// whether to start a new TextStart event.
    text_open: bool,
    /// The claude session id, captured from the final `result` line.
    session_id: Option<String>,
}

fn handle_line(
    line: &str,
    state: &mut ParseState,
    events: &mpsc::UnboundedSender<SessionEvent>,
) -> Result<()> {
    let v: Json = serde_json::from_str(line).with_context(|| format!("parse stream-json: {line}"))?;
    let kind = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    match kind {
        // The CLI's wrapper around an Anthropic API SSE event.
        "stream_event" => {
            if let Some(evt) = v.get("event") {
                handle_sse_event(evt, state, events);
            }
        }
        // Final summary line. Carries `session_id` for follow-up `--resume`.
        "result" => {
            if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                state.session_id = Some(sid.to_string());
            }
            if v.get("subtype").and_then(|s| s.as_str()) == Some("error_during_execution") {
                if let Some(msg) = v.get("result").and_then(|s| s.as_str()) {
                    bail!("claude reported error: {msg}");
                }
            }
        }
        // Init / system / user (echo of tool_results) lines: nothing to render
        // — the MCP server already pushed corresponding events for tools, and
        // text was streamed via stream_event/text_delta.
        _ => {}
    }
    Ok(())
}

fn handle_sse_event(
    evt: &Json,
    state: &mut ParseState,
    events: &mpsc::UnboundedSender<SessionEvent>,
) {
    let kind = evt.get("type").and_then(|s| s.as_str()).unwrap_or("");
    match kind {
        "content_block_start" => {
            let bkind = evt
                .get("content_block")
                .and_then(|b| b.get("type"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if bkind == "text" {
                state.text_open = true;
                let _ = events.send(SessionEvent::TextStart);
            } else {
                state.text_open = false;
                // tool_use blocks: MCP server will emit ToolStart/ToolReady
                // when claude actually invokes the tool over HTTP. Skip here
                // to avoid duplicates.
            }
        }
        "content_block_delta" => {
            let delta_kind = evt
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if delta_kind == "text_delta" {
                if !state.text_open {
                    state.text_open = true;
                    let _ = events.send(SessionEvent::TextStart);
                }
                let text = evt
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                if !text.is_empty() {
                    let _ = events.send(SessionEvent::TextDelta { text: text.to_string() });
                }
            }
        }
        "content_block_stop" => {
            state.text_open = false;
        }
        _ => {}
    }
}

fn write_mcp_config(json: &str) -> Result<std::path::PathBuf> {
    use std::io::Write;
    let dir = std::env::temp_dir();
    let path = dir.join(format!("dbtool-mcp-{}.json", Uuid::new_v4()));
    let mut f = std::fs::File::create(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(json.as_bytes())?;
    Ok(path)
}
