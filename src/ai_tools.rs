//! Tools the agent can call. Currently:
//!  - `run_select` — read-only SQL, returns columns + rows as JSON.
//!  - `run_statement` — DDL/DML, returns `rows_affected`. Requires approval
//!    unless the user has flipped the "Allow writes" toggle.

use serde_json::{Value as Json, json};

use crate::db::{DynDriver, ResultSet, Value};

pub const RUN_SELECT: &str = "run_select";
pub const RUN_STATEMENT: &str = "run_statement";

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Json,
}

/// A tool invocation routed to [`execute`].
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Json,
}

impl ToolCall {
    pub fn sql(&self) -> Option<&str> {
        self.input.get("sql").and_then(|v| v.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    ReadOnly,
    Write,
    Unknown,
}

impl ToolCall {
    pub fn kind(&self) -> ToolKind {
        match self.name.as_str() {
            RUN_SELECT => ToolKind::ReadOnly,
            RUN_STATEMENT => ToolKind::Write,
            _ => ToolKind::Unknown,
        }
    }
}

/// Tool definitions sent to the API.
pub fn tool_defs() -> Vec<Tool> {
    vec![
        Tool {
            name: RUN_SELECT.into(),
            description: "Run a read-only SQL query (SELECT, WITH, EXPLAIN, SHOW, DESCRIBE) \
                          against the user's currently selected database connection. \
                          Returns columns and up to 100 rows as JSON. Use this to inspect \
                          data; do not use it for INSERT/UPDATE/DELETE/DDL."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["sql"],
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "A single read-only SQL statement."
                    }
                }
            }),
        },
        Tool {
            name: RUN_STATEMENT.into(),
            description: "Execute a single SQL statement that modifies data or schema \
                          (INSERT, UPDATE, DELETE, CREATE, ALTER, DROP, TRUNCATE, etc.) \
                          against the user's currently selected database connection. \
                          The user is prompted to approve each call unless they have \
                          enabled auto-approve. Returns rows_affected on success."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["sql"],
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "A single DDL or DML SQL statement."
                    }
                }
            }),
        },
    ]
}

/// Outcome of executing a tool call. `content` is the JSON-encoded payload
/// returned to the model via the MCP server's `tools/call` response.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(payload: Json) -> Self {
        Self {
            content: payload.to_string(),
            is_error: false,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            content: json!({ "error": msg.into() }).to_string(),
            is_error: true,
        }
    }
}

/// Execute a tool call against the active driver. The agent loop is
/// responsible for gating writes on user approval before invoking this.
pub async fn execute(call: &ToolCall, driver: &DynDriver) -> ToolOutcome {
    match call.name.as_str() {
        RUN_SELECT => {
            let Some(sql) = call.sql() else {
                return ToolOutcome::err("missing 'sql' parameter");
            };
            if !looks_read_only(sql) {
                return ToolOutcome::err(
                    "run_select only accepts read-only statements; use run_statement for writes",
                );
            }
            run_query(driver, sql).await
        }
        RUN_STATEMENT => {
            let Some(sql) = call.sql() else {
                return ToolOutcome::err("missing 'sql' parameter");
            };
            run_query(driver, sql).await
        }
        other => ToolOutcome::err(format!("unknown tool: {other}")),
    }
}

async fn run_query(driver: &DynDriver, sql: &str) -> ToolOutcome {
    match driver.query(sql).await {
        Ok(rs) => ToolOutcome::ok(encode_result(&rs)),
        Err(e) => ToolOutcome::err(format!("{e:#}")),
    }
}

const MAX_ROWS: usize = 100;
const MAX_PAYLOAD_BYTES: usize = 50_000;

fn encode_result(rs: &ResultSet) -> Json {
    let truncated = rs.rows.len() > MAX_ROWS;
    let cols: Vec<Json> = rs
        .columns
        .iter()
        .map(|c| json!({ "name": c.name, "type": c.type_name }))
        .collect();

    let mut rows_out: Vec<Json> = Vec::with_capacity(rs.rows.len().min(MAX_ROWS));
    for row in rs.rows.iter().take(MAX_ROWS) {
        let row_obj: serde_json::Map<String, Json> = rs
            .columns
            .iter()
            .zip(row.iter())
            .map(|(c, v)| (c.name.clone(), value_to_json(v)))
            .collect();
        rows_out.push(Json::Object(row_obj));
    }

    let mut payload = json!({
        "columns": cols,
        "rows": rows_out,
        "row_count": rs.rows.len(),
        "truncated": truncated,
    });
    if let Some(n) = rs.rows_affected {
        payload["rows_affected"] = json!(n);
    }

    // Hard cap on payload size to keep token use sane.
    let s = payload.to_string();
    if s.len() > MAX_PAYLOAD_BYTES {
        return json!({
            "columns": cols,
            "row_count": rs.rows.len(),
            "truncated": true,
            "rows_omitted_reason": format!(
                "result body {} bytes exceeds {} byte cap; refine the query",
                s.len(), MAX_PAYLOAD_BYTES
            ),
        });
    }
    payload
}

fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Text(s) => Json::String(s.clone()),
        Value::Bytes(b) => Json::String(format!("<{} bytes>", b.len())),
        Value::Json(j) => j.clone(),
        Value::Timestamp(s) => Json::String(s.clone()),
    }
}

/// Best-effort check: does this SQL look like a read-only statement?
/// Conservative — anything not on the allow-list is rejected. Uses the same
/// keyword detection as the drivers so the two layers can't disagree.
pub fn looks_read_only(sql: &str) -> bool {
    matches!(
        crate::db::leading_keyword(sql).as_str(),
        "SELECT" | "WITH" | "EXPLAIN" | "SHOW" | "DESCRIBE" | "DESC"
    )
}

