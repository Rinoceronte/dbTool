//! Tools the agent can call. Currently:
//!  - `run_select` — read-only SQL, returns columns + rows as JSON.
//!  - `run_statement` — DDL/DML, returns `rows_affected`. Requires approval
//!    unless the user has flipped the "Allow writes" toggle.
//!  - `list_dbml` / `read_dbml` — browse the DBML documents in the dbml dir.
//!  - `update_dbml` — edit or replace a DBML document; the result must parse
//!    before it is written. Approval-gated like `run_statement`.

use serde_json::{Value as Json, json};

use crate::db::{DynDriver, ResultSet, Value};

pub const RUN_SELECT: &str = "run_select";
pub const RUN_STATEMENT: &str = "run_statement";
pub const LIST_DBML: &str = "list_dbml";
pub const READ_DBML: &str = "read_dbml";
pub const UPDATE_DBML: &str = "update_dbml";
pub const FOCUS_TABLE: &str = "focus_table";
pub const READ_EDITOR: &str = "read_editor";
pub const PROPOSE_SQL: &str = "propose_sql";

/// What the user's active query tab holds. Published by the UI thread each
/// frame (same pattern as `claude_auth::CLI_PATH`) so `read_editor` can see
/// it from the MCP server's tokio context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorSnapshot {
    pub connection: String,
    pub title: String,
    pub sql: String,
}

static ACTIVE_EDITOR: std::sync::RwLock<Option<EditorSnapshot>> = std::sync::RwLock::new(None);

pub fn set_active_editor(snap: Option<EditorSnapshot>) {
    let mut slot = ACTIVE_EDITOR.write().unwrap();
    if *slot != snap {
        *slot = snap;
    }
}

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

    fn arg(&self, key: &str) -> Option<&str> {
        self.input.get(key).and_then(|v| v.as_str())
    }

    /// The DBML file this call targets, if it is a DBML tool call.
    pub fn dbml_file(&self) -> Option<&str> {
        self.arg("file")
    }

    /// The table a `focus_table` call targets.
    pub fn dbml_table(&self) -> Option<&str> {
        self.arg("table")
    }

    /// Human-readable text shown in the approval / tool card UI: the SQL for
    /// SQL tools, a change preview for `update_dbml`.
    pub fn approval_preview(&self) -> String {
        match self.name.as_str() {
            UPDATE_DBML => {
                let file = self.dbml_file().unwrap_or("?");
                if let Some(content) = self.arg("content") {
                    format!("replace {file} ({} bytes):\n{content}", content.len())
                } else {
                    format!(
                        "edit {file}\n--- find ---\n{}\n--- replace with ---\n{}",
                        self.arg("find").unwrap_or(""),
                        self.arg("replace").unwrap_or("")
                    )
                }
            }
            READ_DBML => format!("read {}", self.dbml_file().unwrap_or("?")),
            LIST_DBML => "list DBML documents".to_string(),
            READ_EDITOR => "read the active query editor".to_string(),
            FOCUS_TABLE => format!(
                "focus diagram on `{}` in {}",
                self.dbml_table().unwrap_or("?"),
                self.dbml_file().unwrap_or("?")
            ),
            _ => self.sql().unwrap_or("").to_string(),
        }
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
            RUN_SELECT | LIST_DBML | READ_DBML | FOCUS_TABLE | READ_EDITOR | PROPOSE_SQL => {
                ToolKind::ReadOnly
            }
            RUN_STATEMENT | UPDATE_DBML => ToolKind::Write,
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
        Tool {
            name: LIST_DBML.into(),
            description: "List the DBML diagram documents dbTool manages (file name and \
                          size). Database-owned documents are generated from a live \
                          schema and carry a profile-id suffix in the name."
                .into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: READ_DBML.into(),
            description: "Read a DBML diagram document by file name (as returned by \
                          list_dbml). Documents have a dbTool-generated region between \
                          marker comments — replaced whenever the user refreshes from \
                          the database — and a user area below it for TableGroups, \
                          notes, colors, and draft tables."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "DBML file name, e.g. `MyDb-1a2b3c4d.dbml`."
                    }
                }
            }),
        },
        Tool {
            name: UPDATE_DBML.into(),
            description: "Edit a DBML diagram document. Provide either `find` + \
                          `replace` (the `find` text must occur exactly once) or \
                          `content` to replace the whole file (also creates a new \
                          file). The result must parse as valid DBML or the write is \
                          rejected with the parse error. Avoid editing inside the \
                          dbTool-generated marker region — those lines are overwritten \
                          on the next refresh from the database; put TableGroups, \
                          notes, and draft tables in the user area below it. The user \
                          is prompted to approve each call unless they have enabled \
                          auto-approve."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "DBML file name to edit or create (must end in .dbml)."
                    },
                    "find": {
                        "type": "string",
                        "description": "Exact text to replace; must occur exactly once in the file."
                    },
                    "replace": {
                        "type": "string",
                        "description": "Replacement text for `find`."
                    },
                    "content": {
                        "type": "string",
                        "description": "Full new file content; mutually exclusive with find/replace."
                    }
                }
            }),
        },
        Tool {
            name: READ_EDITOR.into(),
            description: "Read the SQL currently in the user's active query editor tab. \
                          Use this whenever the user refers to \"this query\", \"my \
                          query\", or the editor. Returns the connection name, tab \
                          title, and SQL text; errors if no query tab is focused."
                .into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        Tool {
            name: PROPOSE_SQL.into(),
            description: "Present SQL to the user without executing it. The SQL appears \
                          as a card with one-click actions to copy it, open it in a new \
                          editor tab, or replace the query in their active editor tab. \
                          Use this to hand back a revised version of the user's query \
                          (after read_editor) or to offer a query you are not meant to \
                          run yourself. Never run a statement merely to display it."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["sql"],
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "The SQL to show the user."
                    }
                }
            }),
        },
        Tool {
            name: FOCUS_TABLE.into(),
            description: "Pan the diagram view to a table so the user can watch where you \
                          are working. Opens the DBML document in a diagram tab if it is \
                          not already open, selects the table, and centers the view on it. \
                          Use this to show the user what you are about to change or just \
                          changed — e.g. focus a table before and after editing its group."
                .into(),
            input_schema: json!({
                "type": "object",
                "required": ["file", "table"],
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "DBML file name, e.g. `open_shop.dbml`."
                    },
                    "table": {
                        "type": "string",
                        "description": "Table name, optionally schema-qualified (`public.orders` or `orders`)."
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

/// Execute a tool call. SQL tools need the active driver; DBML tools work on
/// the dbml directory and run without a connection. The agent loop is
/// responsible for gating writes on user approval before invoking this.
pub async fn execute(call: &ToolCall, driver: Option<&DynDriver>) -> ToolOutcome {
    match call.name.as_str() {
        RUN_SELECT | RUN_STATEMENT => {
            let Some(driver) = driver else {
                return ToolOutcome::err(
                    "no database connection is selected; pick one in the AI panel",
                );
            };
            let Some(sql) = call.sql() else {
                return ToolOutcome::err("missing 'sql' parameter");
            };
            if call.name == RUN_SELECT && !looks_read_only(sql) {
                return ToolOutcome::err(
                    "run_select only accepts read-only statements; use run_statement for writes",
                );
            }
            run_query(driver, sql).await
        }
        LIST_DBML => list_dbml(),
        READ_DBML => read_dbml(call),
        UPDATE_DBML => update_dbml(call),
        FOCUS_TABLE => focus_table(call),
        READ_EDITOR => match ACTIVE_EDITOR.read().unwrap().clone() {
            Some(snap) => ToolOutcome::ok(json!({
                "connection": snap.connection,
                "title": snap.title,
                "sql": snap.sql,
            })),
            None => ToolOutcome::err(
                "no query editor tab is currently active; ask the user to focus one",
            ),
        },
        PROPOSE_SQL => {
            if call.sql().is_none_or(|s| s.trim().is_empty()) {
                return ToolOutcome::err("missing 'sql' parameter");
            }
            ToolOutcome::ok(json!({
                "shown": true,
                "note": "the SQL is displayed with copy / open-in-editor / replace-query actions"
            }))
        }
        other => ToolOutcome::err(format!("unknown tool: {other}")),
    }
}

/// Validate a `focus_table` call. The actual view change happens on the UI
/// thread — the MCP server forwards it as a `DbmlFocus` session event after
/// this returns success.
fn focus_table(call: &ToolCall) -> ToolOutcome {
    let path = match dbml_path(call) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(table) = call.dbml_table() else {
        return ToolOutcome::err("missing 'table' parameter");
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return ToolOutcome::err(format!("could not read {}: {e}", path.display())),
    };
    let model = match crate::dbml::parse(&text) {
        Ok(m) => m,
        Err(e) => {
            return ToolOutcome::err(format!(
                "the document does not currently parse ({}); fix it before focusing",
                e.message
            ));
        }
    };
    let want = table.to_lowercase();
    let found = model.tables.iter().any(|t| {
        t.key.to_lowercase() == want
            || t.name.to_lowercase() == want
            || crate::dbml::bare_table_name(&t.key).to_lowercase() == want
    });
    if !found {
        let mut names: Vec<&str> = model.tables.iter().map(|t| t.key.as_str()).collect();
        names.sort();
        return ToolOutcome::err(format!(
            "table `{table}` not found in {}; tables: {}",
            call.dbml_file().unwrap_or("?"),
            names.join(", ")
        ));
    }
    ToolOutcome::ok(json!({
        "focused": table,
        "note": "the diagram tab is now centered on this table"
    }))
}

/// Resolve a `file` argument to a path inside the dbml dir, rejecting
/// anything that could escape it.
fn dbml_path(call: &ToolCall) -> Result<std::path::PathBuf, ToolOutcome> {
    let Some(file) = call.dbml_file() else {
        return Err(ToolOutcome::err("missing 'file' parameter"));
    };
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return Err(ToolOutcome::err("'file' must be a bare file name, not a path"));
    }
    if !file.ends_with(".dbml") {
        return Err(ToolOutcome::err("'file' must end in .dbml"));
    }
    Ok(crate::dbml::dbml_dir().join(file))
}

fn list_dbml() -> ToolOutcome {
    let dir = crate::dbml::dbml_dir();
    let mut files: Vec<Json> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".dbml") {
                continue;
            }
            let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(json!({ "file": name, "bytes": bytes }));
        }
    }
    files.sort_by(|a, b| a["file"].as_str().cmp(&b["file"].as_str()));
    ToolOutcome::ok(json!({ "dir": dir.display().to_string(), "files": files }))
}

fn read_dbml(call: &ToolCall) -> ToolOutcome {
    let path = match dbml_path(call) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => ToolOutcome::ok(json!({ "file": call.dbml_file(), "content": content })),
        Err(e) => ToolOutcome::err(format!("could not read {}: {e}", path.display())),
    }
}

fn update_dbml(call: &ToolCall) -> ToolOutcome {
    let path = match dbml_path(call) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = call.input.get("content").and_then(|v| v.as_str());
    let find = call.input.get("find").and_then(|v| v.as_str());
    let replace = call.input.get("replace").and_then(|v| v.as_str());

    let new_text = match (content, find, replace) {
        (Some(c), None, None) => c.to_string(),
        (None, Some(f), Some(r)) => {
            let old = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    return ToolOutcome::err(format!(
                        "could not read {}: {e}; to create a new file pass 'content'",
                        path.display()
                    ));
                }
            };
            if f.is_empty() {
                return ToolOutcome::err("'find' must not be empty");
            }
            match old.matches(f).count() {
                1 => old.replacen(f, r, 1),
                0 => return ToolOutcome::err("'find' text not found in the file"),
                n => {
                    return ToolOutcome::err(format!(
                        "'find' text occurs {n} times; provide a longer unique snippet"
                    ));
                }
            }
        }
        _ => {
            return ToolOutcome::err(
                "provide either 'content' alone or both 'find' and 'replace'",
            );
        }
    };

    // Refuse to write anything the diagram can't render.
    if let Err(e) = crate::dbml::parse(&new_text) {
        let line = e.line.map(|l| format!(" (line {l})")).unwrap_or_default();
        return ToolOutcome::err(format!(
            "rejected: result is not valid DBML{line}: {}",
            e.message
        ));
    }
    if let Err(e) = std::fs::create_dir_all(crate::dbml::dbml_dir()) {
        return ToolOutcome::err(format!("could not create dbml dir: {e}"));
    }
    match std::fs::write(&path, &new_text) {
        Ok(()) => ToolOutcome::ok(json!({
            "file": call.dbml_file(),
            "bytes": new_text.len(),
            "note": "written; any open diagram tab without unsaved changes reloads automatically"
        })),
        Err(e) => ToolOutcome::err(format!("could not write {}: {e}", path.display())),
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

