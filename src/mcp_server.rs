//! Tiny in-process HTTP MCP server. Exposes `run_select` and `run_statement`
//! to the `claude` CLI so the agent can act on the user's selected database
//! connection. Approval gating happens here: `tools/call` blocks the JSON-RPC
//! response until the user approves (or rejects) writes.
//!
//! Protocol: MCP "Streamable HTTP" transport, single POST endpoint, JSON-RPC 2.0.
//! We implement the minimum: `initialize`, `tools/list`, `tools/call`, plus
//! empty stubs for `resources/list` / `prompts/list` and a no-op `ping`.

use anyhow::Result;
use axum::{
    Json as AxumJson, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value as Json, json};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::ai_session::{ApprovalDecision, SessionEvent};
use crate::ai_tools::{self, RUN_SELECT, RUN_STATEMENT, ToolCall, ToolKind};
use crate::db::DynDriver;

const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Clone)]
pub struct McpServer {
    pub port: u16,
    state: Arc<ServerState>,
}

struct ServerState {
    current: Mutex<Option<SessionContext>>,
}

struct SessionContext {
    driver: Option<DynDriver>,
    allow_writes: bool,
    events_tx: mpsc::UnboundedSender<SessionEvent>,
    approvals_rx: Arc<Mutex<mpsc::UnboundedReceiver<ApprovalDecision>>>,
    cancel: CancellationToken,
}

impl McpServer {
    /// Bind to 127.0.0.1:0 and start serving in a background tokio task.
    pub async fn spawn() -> Result<Self> {
        let state = Arc::new(ServerState {
            current: Mutex::new(None),
        });
        let app = Router::new()
            .route("/mcp", post(handle_rpc))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        Ok(Self { port, state })
    }

    /// Install a session context. The next claude invocation that hits this
    /// MCP server will dispatch tool calls against `driver`.
    pub async fn set_session(
        &self,
        driver: Option<DynDriver>,
        allow_writes: bool,
        events_tx: mpsc::UnboundedSender<SessionEvent>,
        approvals_rx: mpsc::UnboundedReceiver<ApprovalDecision>,
        cancel: CancellationToken,
    ) {
        let mut slot = self.state.current.lock().await;
        *slot = Some(SessionContext {
            driver,
            allow_writes,
            events_tx,
            approvals_rx: Arc::new(Mutex::new(approvals_rx)),
            cancel,
        });
    }

    pub async fn clear_session(&self) {
        let mut slot = self.state.current.lock().await;
        *slot = None;
    }

    /// JSON to write to a temp file and pass to `claude --mcp-config`.
    pub fn config_json(&self) -> String {
        json!({
            "mcpServers": {
                "dbtool": {
                    "type": "http",
                    "url": format!("http://127.0.0.1:{}/mcp", self.port)
                }
            }
        })
        .to_string()
    }

    /// MCP tool names as they appear in `--allowed-tools`.
    pub fn allowed_tools_arg() -> String {
        format!("mcp__dbtool__{RUN_SELECT},mcp__dbtool__{RUN_STATEMENT}")
    }
}

async fn handle_rpc(
    State(state): State<Arc<ServerState>>,
    AxumJson(body): AxumJson<Json>,
) -> Response {
    let id = body.get("id").cloned().unwrap_or(Json::Null);
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = body.get("params").cloned().unwrap_or(Json::Null);

    match method {
        "initialize" => AxumJson(json_ok(id, json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "dbtool", "version": env!("CARGO_PKG_VERSION") }
        }))).into_response(),
        "notifications/initialized" | "notifications/cancelled" => {
            // Notifications get no response (HTTP 202 with empty body).
            (StatusCode::ACCEPTED, "").into_response()
        }
        "ping" => AxumJson(json_ok(id, json!({}))).into_response(),
        "tools/list" => AxumJson(json_ok(id, json!({
            "tools": tool_specs()
        }))).into_response(),
        "resources/list" => AxumJson(json_ok(id, json!({ "resources": [] }))).into_response(),
        "prompts/list" => AxumJson(json_ok(id, json!({ "prompts": [] }))).into_response(),
        "tools/call" => {
            let resp = match call_tool(&state, params).await {
                Ok(content) => json_ok(id, content),
                Err(e) => json_ok(
                    id,
                    json!({
                        "isError": true,
                        "content": [{ "type": "text", "text": format!("{e:#}") }]
                    }),
                ),
            };
            AxumJson(resp).into_response()
        }
        _ => AxumJson(json_err(id, -32601, format!("method not found: {method}"))).into_response(),
    }
}

fn json_ok(id: Json, result: Json) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn json_err(id: Json, code: i64, message: String) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_specs() -> Vec<Json> {
    ai_tools::tool_defs()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })
        })
        .collect()
}

async fn call_tool(state: &Arc<ServerState>, params: Json) -> anyhow::Result<Json> {
    let name = params.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
    let arguments = params.get("arguments").cloned().unwrap_or(Json::Object(Default::default()));

    // Snapshot what we need from the session, then drop the lock so the
    // approval wait doesn't keep it held.
    let (driver, allow_writes, events_tx, approvals_rx, cancel) = {
        let slot = state.current.lock().await;
        let Some(ctx) = slot.as_ref() else {
            anyhow::bail!("no active dbTool session");
        };
        (
            ctx.driver.clone(),
            ctx.allow_writes,
            ctx.events_tx.clone(),
            ctx.approvals_rx.clone(),
            ctx.cancel.clone(),
        )
    };

    let call = ToolCall {
        id: format!("mcp-{}", uuid::Uuid::new_v4()),
        name,
        input: arguments,
    };

    let kind = call.kind();
    let needs_approval = matches!(kind, ToolKind::Write) && !allow_writes;
    let sql = call.sql().unwrap_or("").to_string();

    let _ = events_tx.send(SessionEvent::ToolStart {
        tool_use_id: call.id.clone(),
        name: call.name.clone(),
    });
    let _ = events_tx.send(SessionEvent::ToolReady {
        tool_use_id: call.id.clone(),
        name: call.name.clone(),
        sql,
        needs_approval,
    });

    if needs_approval {
        let mut rx = approvals_rx.lock().await;
        let decision = loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => anyhow::bail!("cancelled"),
                d = rx.recv() => d,
            };
            let Some(d) = next else {
                anyhow::bail!("approval channel closed");
            };
            if d.tool_use_id == call.id {
                break d;
            }
        };
        if !decision.approved {
            let _ = events_tx.send(SessionEvent::ToolResult {
                tool_use_id: call.id.clone(),
                ok: false,
                summary: "user rejected".into(),
            });
            return Ok(json!({
                "isError": true,
                "content": [{ "type": "text", "text": "user rejected the statement" }]
            }));
        }
    }

    let Some(driver) = driver else {
        let _ = events_tx.send(SessionEvent::ToolResult {
            tool_use_id: call.id.clone(),
            ok: false,
            summary: "no connection".into(),
        });
        return Ok(json!({
            "isError": true,
            "content": [{ "type": "text", "text": "no database connection is selected; pick one in the AI panel" }]
        }));
    };

    let outcome = ai_tools::execute(&call, &driver).await;
    let _ = events_tx.send(SessionEvent::ToolResult {
        tool_use_id: call.id.clone(),
        ok: !outcome.is_error,
        summary: short_summary(&outcome.content),
    });
    Ok(json!({
        "isError": outcome.is_error,
        "content": [{ "type": "text", "text": outcome.content }]
    }))
}

fn short_summary(content: &str) -> String {
    const N: usize = 200;
    if content.len() <= N {
        return content.to_string();
    }
    let mut end = N;
    while end < content.len() && !content.is_char_boundary(end) {
        end += 1;
    }
    format!("{}…", &content[..end])
}

