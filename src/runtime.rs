use crate::ai_session::{self, ApprovalDecision, SessionEvent, SessionId, SessionInputs};
use crate::claude_auth::{self, AuthStatus};
use crate::db::{
    self, ConnectParams, DbMeta, DynDriver, PkValues, ResultSet, RowChanges, TableInfo, TableSchema,
    structure::TableStructure,
};
use crate::mcp_server::McpServer;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub type ConnectionId = Uuid;
pub type RequestId = Uuid;

#[derive(Debug)]
pub enum Command {
    Connect { req: RequestId, params: ConnectParams },
    ListSchemas { req: RequestId, conn: ConnectionId },
    ListTables { req: RequestId, conn: ConnectionId, schema: String },
    DescribeTable { req: RequestId, conn: ConnectionId, schema: String, table: String },
    Query { req: RequestId, conn: ConnectionId, sql: String },
    FetchTableRows {
        req: RequestId,
        conn: ConnectionId,
        schema: String,
        table: String,
        limit: i64,
        offset: i64,
    },
    InsertRow {
        req: RequestId,
        conn: ConnectionId,
        schema: String,
        table: String,
        values: RowChanges,
    },
    ApplyChanges {
        req: RequestId,
        conn: ConnectionId,
        schema: String,
        table: String,
        updates: Vec<(PkValues, RowChanges)>,
        deletes: Vec<PkValues>,
    },
    DescribeStructure { req: RequestId, conn: ConnectionId, schema: String, table: String },
    /// Execute a DDL batch (transactional where the dialect allows it).
    ApplyDdl { req: RequestId, conn: ConnectionId, statements: Vec<String> },
    FetchDbMeta { req: RequestId, conn: ConnectionId },
    /// Write CREATE statements for every table/view of the given schemas
    /// (or all schemas when empty) as one .sql file per object under `dir`.
    DumpDdl {
        req: RequestId,
        conn: ConnectionId,
        schemas: Vec<String>,
        dir: String,
    },
    /// Introspect every table of every schema for DBML generation.
    DumpDbml { req: RequestId, conn: ConnectionId },
    ImportCsv {
        req: RequestId,
        conn: ConnectionId,
        schema: String,
        table: String,
        path: String,
        options: crate::csv_import::ImportOptions,
    },
    /// Stream the full contents of schema.table to a delimited file.
    ExportCsv {
        req: RequestId,
        conn: ConnectionId,
        schema: String,
        table: String,
        path: String,
        options: crate::csv_export::ExportOptions,
    },
    /// Write an in-memory result set to a delimited file.
    ExportResultCsv {
        req: RequestId,
        result: ResultSet,
        path: String,
        options: crate::csv_export::ExportOptions,
    },
    Disconnect { conn: ConnectionId },
    AiStart {
        session: SessionId,
        prompt: String,
        system: String,
        model: String,
        conn: Option<ConnectionId>,
        allow_writes: bool,
        resume_id: Option<String>,
    },
    AiApprove {
        session: SessionId,
        tool_use_id: String,
        approved: bool,
    },
    AiCancel {
        session: SessionId,
    },
    AuthStatus { req: RequestId },
    AuthLogin { req: RequestId, use_console: bool },
    AuthLogout { req: RequestId },
}

#[derive(Debug)]
pub enum Event {
    Connected { req: RequestId, conn: ConnectionId },
    ConnectFailed { req: RequestId, error: String },
    Schemas { req: RequestId, conn: ConnectionId, schemas: Vec<String> },
    Tables { req: RequestId, conn: ConnectionId, schema: String, tables: Vec<TableInfo> },
    TableDescribed {
        req: RequestId,
        conn: ConnectionId,
        schema: String,
        table: String,
        table_schema: TableSchema,
    },
    QueryResult { req: RequestId, result: ResultSet },
    TableRows {
        req: RequestId,
        conn: ConnectionId,
        schema: String,
        table: String,
        result: ResultSet,
    },
    RowInserted { req: RequestId },
    ChangesApplied { req: RequestId },
    StructureDescribed { req: RequestId, structure: TableStructure },
    DdlApplied {
        req: RequestId,
        applied: usize,
        total: usize,
        error: Option<String>,
    },
    DbMeta { req: RequestId, conn: ConnectionId, meta: DbMeta },
    DdlDumped {
        req: RequestId,
        dir: String,
        files: usize,
        errors: Vec<String>,
    },
    DbmlDumped {
        req: RequestId,
        tables: Vec<crate::db::structure::TableStructure>,
        errors: Vec<String>,
    },
    ImportProgress { req: RequestId, rows: u64 },
    Imported { req: RequestId, rows: u64 },
    ExportProgress { req: RequestId, rows: u64 },
    Exported { req: RequestId, rows: u64 },
    Ai { session: SessionId, event: SessionEvent },
    /// The session task ended (any reason). Emitted after the final SessionEvent.
    AiSessionEnded { session: SessionId },
    AuthStatusResult { req: RequestId, status: AuthStatus },
    AuthLoginResult { req: RequestId, success: bool, output: String },
    AuthLogoutResult { req: RequestId, output: String },
    Error { req: RequestId, error: String },
}

pub struct Runtime {
    tx: mpsc::UnboundedSender<Command>,
    rx: mpsc::UnboundedReceiver<Event>,
}

impl Runtime {
    pub fn new(ctx: egui::Context) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<Event>();

        std::thread::Builder::new()
            .name("dbtool-runtime".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(worker_main(cmd_rx, evt_tx, ctx));
            })
            .expect("spawn runtime thread");

        Self { tx: cmd_tx, rx: evt_rx }
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.rx.try_recv() {
            out.push(e);
        }
        out
    }
}

type Connections = Arc<RwLock<HashMap<ConnectionId, DynDriver>>>;

struct SessionHandle {
    cancel: CancellationToken,
    approval_tx: mpsc::UnboundedSender<ApprovalDecision>,
}

type Sessions = Arc<RwLock<HashMap<SessionId, SessionHandle>>>;

async fn worker_main(
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    evt_tx: mpsc::UnboundedSender<Event>,
    ctx: egui::Context,
) {
    let connections: Connections = Arc::new(RwLock::new(HashMap::new()));
    let sessions: Sessions = Arc::new(RwLock::new(HashMap::new()));
    let mcp = match McpServer::spawn().await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[runtime] failed to start MCP server: {e:#}");
            // Continue without an MCP server — agent calls will fail later with a clear error.
            return;
        }
    };

    while let Some(cmd) = cmd_rx.recv().await {
        let evt_tx = evt_tx.clone();
        let ctx = ctx.clone();
        let connections = connections.clone();
        let sessions = sessions.clone();
        let mcp = mcp.clone();
        tokio::spawn(async move {
            handle_command(cmd, connections, sessions, mcp, evt_tx, ctx).await;
        });
    }
}

fn send(evt_tx: &mpsc::UnboundedSender<Event>, ctx: &egui::Context, event: Event) {
    let _ = evt_tx.send(event);
    ctx.request_repaint();
}

async fn get_driver(connections: &Connections, conn: ConnectionId) -> Option<DynDriver> {
    connections.read().await.get(&conn).cloned()
}

async fn handle_command(
    cmd: Command,
    connections: Connections,
    sessions: Sessions,
    mcp: Arc<McpServer>,
    evt_tx: mpsc::UnboundedSender<Event>,
    ctx: egui::Context,
) {
    match cmd {
        Command::Connect { req, params } => match db::connect(&params).await {
            Ok(driver) => {
                let id = ConnectionId::new_v4();
                connections.write().await.insert(id, driver);
                send(&evt_tx, &ctx, Event::Connected { req, conn: id });
            }
            Err(e) => send(&evt_tx, &ctx, Event::ConnectFailed { req, error: format!("{e:#}") }),
        },

        Command::Disconnect { conn } => {
            connections.write().await.remove(&conn);
        }

        Command::ListSchemas { req, conn } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.list_schemas().await {
                Ok(schemas) => send(&evt_tx, &ctx, Event::Schemas { req, conn, schemas }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::ListTables { req, conn, schema } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.list_tables(&schema).await {
                Ok(tables) => send(&evt_tx, &ctx, Event::Tables { req, conn, schema, tables }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::DescribeTable { req, conn, schema, table } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.describe_table(&schema, &table).await {
                Ok(ts) => send(
                    &evt_tx,
                    &ctx,
                    Event::TableDescribed { req, conn, schema, table, table_schema: ts },
                ),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::Query { req, conn, sql } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.query(&sql).await {
                Ok(result) => send(&evt_tx, &ctx, Event::QueryResult { req, result }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::FetchTableRows { req, conn, schema, table, limit, offset } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.fetch_table_rows(&schema, &table, limit, offset).await {
                Ok(result) => send(
                    &evt_tx,
                    &ctx,
                    Event::TableRows { req, conn, schema, table, result },
                ),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::InsertRow { req, conn, schema, table, values } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.insert_row(&schema, &table, &values).await {
                Ok(()) => send(&evt_tx, &ctx, Event::RowInserted { req }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::ApplyChanges { req, conn, schema, table, updates, deletes } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.apply_changes(&schema, &table, &updates, &deletes).await {
                Ok(()) => send(&evt_tx, &ctx, Event::ChangesApplied { req }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::AiStart { session, prompt, system, model, conn, allow_writes, resume_id } => {
            let driver = match conn {
                Some(c) => get_driver(&connections, c).await,
                None => None,
            };
            let cancel = CancellationToken::new();
            let (approval_tx, approval_rx) = mpsc::unbounded_channel::<ApprovalDecision>();
            sessions.write().await.insert(
                session,
                SessionHandle { cancel: cancel.clone(), approval_tx },
            );

            let (sess_tx, mut sess_rx) = mpsc::unbounded_channel::<SessionEvent>();
            let sess_id = session;
            let evt_tx_for_pump = evt_tx.clone();
            let ctx_for_pump = ctx.clone();
            let pump = tokio::spawn(async move {
                while let Some(e) = sess_rx.recv().await {
                    send(&evt_tx_for_pump, &ctx_for_pump, Event::Ai { session: sess_id, event: e });
                }
            });

            // Wire the MCP server to this session so tool calls reach the right driver.
            mcp.set_session(driver, allow_writes, sess_tx.clone(), approval_rx, cancel.clone()).await;

            let inputs = SessionInputs { prompt, system, model, resume_id };
            ai_session::run_session(inputs, mcp.clone(), sess_tx, cancel).await;

            mcp.clear_session().await;
            let _ = pump.await;
            sessions.write().await.remove(&session);
            send(&evt_tx, &ctx, Event::AiSessionEnded { session });
        }

        Command::AiApprove { session, tool_use_id, approved } => {
            if let Some(handle) = sessions.read().await.get(&session) {
                let _ = handle.approval_tx.send(ApprovalDecision { tool_use_id, approved });
            }
        }

        Command::AiCancel { session } => {
            if let Some(handle) = sessions.read().await.get(&session) {
                handle.cancel.cancel();
            }
        }

        Command::AuthStatus { req } => match claude_auth::probe_status().await {
            Ok(status) => send(&evt_tx, &ctx, Event::AuthStatusResult { req, status }),
            Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
        },

        Command::AuthLogin { req, use_console } => match claude_auth::run_login(use_console).await {
            Ok(outcome) => send(
                &evt_tx,
                &ctx,
                Event::AuthLoginResult {
                    req,
                    success: outcome.success,
                    output: outcome.output,
                },
            ),
            Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
        },

        Command::AuthLogout { req } => match claude_auth::run_logout().await {
            Ok(output) => send(&evt_tx, &ctx, Event::AuthLogoutResult { req, output }),
            Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
        },

        Command::DumpDdl { req, conn, schemas, dir } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match dump_ddl(driver, schemas, &dir).await {
                Ok((files, errors)) => {
                    send(&evt_tx, &ctx, Event::DdlDumped { req, dir, files, errors })
                }
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::DumpDbml { req, conn } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match dump_dbml(driver).await {
                Ok((tables, errors)) => {
                    send(&evt_tx, &ctx, Event::DbmlDumped { req, tables, errors })
                }
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::ImportCsv { req, conn, schema, table, path, options } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            let progress = {
                let evt_tx = evt_tx.clone();
                let ctx = ctx.clone();
                move |rows: u64| send(&evt_tx, &ctx, Event::ImportProgress { req, rows })
            };
            match import_csv(driver, &schema, &table, &path, &options, progress).await {
                Ok(rows) => send(&evt_tx, &ctx, Event::Imported { req, rows }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::ExportCsv { req, conn, schema, table, path, options } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            let progress = {
                let evt_tx = evt_tx.clone();
                let ctx = ctx.clone();
                move |rows: u64| send(&evt_tx, &ctx, Event::ExportProgress { req, rows })
            };
            match export_table_csv(driver, &schema, &table, &path, &options, progress).await {
                Ok(rows) => send(&evt_tx, &ctx, Event::Exported { req, rows }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::ExportResultCsv { req, result, path, options } => {
            match export_result_csv(&result, &path, &options) {
                Ok(rows) => send(&evt_tx, &ctx, Event::Exported { req, rows }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::DescribeStructure { req, conn, schema, table } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.describe_structure(&schema, &table).await {
                Ok(structure) => send(&evt_tx, &ctx, Event::StructureDescribed { req, structure }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::ApplyDdl { req, conn, statements } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            let total = statements.len();
            let outcome = driver.apply_ddl(&statements).await;
            send(
                &evt_tx,
                &ctx,
                Event::DdlApplied { req, applied: outcome.applied, total, error: outcome.error },
            );
        }

        Command::FetchDbMeta { req, conn } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.fetch_db_meta().await {
                Ok(meta) => send(&evt_tx, &ctx, Event::DbMeta { req, conn, meta }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }
    }
}

/// Replace path-hostile characters so schema/table names are safe file names.
fn safe_file_component(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect()
}

fn expand_home(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

/// Dump DDL of every table/view in `schemas` (all schemas when empty) to
/// `<dir>/<schema>/<object>.sql`. Returns (files written, per-object errors).
/// Introspect every base table (views excluded — DBML has no view concept)
/// across all schemas. Per-table failures are collected, not fatal.
pub async fn dump_dbml(
    driver: DynDriver,
) -> anyhow::Result<(Vec<crate::db::structure::TableStructure>, Vec<String>)> {
    let schemas = driver.list_schemas().await?;
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for schema in schemas {
        let tables = match driver.list_tables(&schema).await {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{schema}: list tables failed: {e:#}"));
                continue;
            }
        };
        for t in tables.iter().filter(|t| matches!(t.kind, crate::db::TableKind::Table)) {
            match driver.describe_structure(&schema, &t.name).await {
                Ok(s) => out.push(s),
                Err(e) => errors.push(format!("{schema}.{}: {e:#}", t.name)),
            }
        }
    }
    Ok((out, errors))
}

pub async fn dump_ddl(
    driver: DynDriver,
    schemas: Vec<String>,
    dir: &str,
) -> anyhow::Result<(usize, Vec<String>)> {
    let root = expand_home(dir);
    let schemas = if schemas.is_empty() { driver.list_schemas().await? } else { schemas };

    let mut files = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for schema in schemas {
        let tables = match driver.list_tables(&schema).await {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{schema}: list tables failed: {e:#}"));
                continue;
            }
        };
        if tables.is_empty() {
            continue;
        }
        let schema_dir = root.join(safe_file_component(&schema));
        std::fs::create_dir_all(&schema_dir)
            .map_err(|e| anyhow::anyhow!("create {}: {e}", schema_dir.display()))?;
        for t in tables {
            match driver.table_ddl(&schema, &t.name, t.kind).await {
                Ok(ddl) => {
                    let path = schema_dir.join(format!("{}.sql", safe_file_component(&t.name)));
                    match std::fs::write(&path, ddl) {
                        Ok(()) => files += 1,
                        Err(e) => errors.push(format!("{}: write failed: {e}", path.display())),
                    }
                }
                Err(e) => errors.push(format!("{schema}.{}: {e:#}", t.name)),
            }
        }
    }
    Ok((files, errors))
}

const IMPORT_BATCH_ROWS: usize = 500;
const EXPORT_PAGE_ROWS: i64 = 2000;

fn open_export_writer(
    path: &str,
    delimiter: u8,
) -> anyhow::Result<csv::Writer<std::fs::File>> {
    use anyhow::Context as _;
    let file_path = expand_home(path);
    if let Some(parent) = file_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
    }
    let file = std::fs::File::create(&file_path)
        .with_context(|| format!("create {}", file_path.display()))?;
    Ok(csv::WriterBuilder::new().delimiter(delimiter).from_writer(file))
}

/// Stream every row of `schema.table` into a delimited file, paging through
/// the table so large tables never sit in memory all at once.
pub async fn export_table_csv(
    driver: DynDriver,
    schema: &str,
    table: &str,
    path: &str,
    options: &crate::csv_export::ExportOptions,
    progress: impl Fn(u64),
) -> anyhow::Result<u64> {
    use anyhow::Context as _;

    let mut writer = open_export_writer(path, options.delimiter)?;
    let mut total: u64 = 0;
    let mut offset: i64 = 0;
    let mut first_page = true;
    loop {
        let rs = driver
            .fetch_table_rows(schema, table, EXPORT_PAGE_ROWS, offset)
            .await
            .with_context(|| format!("fetch rows at offset {offset}"))?;
        if first_page {
            if options.include_header {
                writer
                    .write_record(rs.columns.iter().map(|c| c.name.as_str()))
                    .context("write header")?;
            }
            first_page = false;
        }
        let page_rows = rs.rows.len();
        for row in &rs.rows {
            writer
                .write_record(row.iter().map(crate::csv_export::field_text))
                .with_context(|| format!("write row {}", total + 1))?;
            total += 1;
        }
        progress(total);
        if (page_rows as i64) < EXPORT_PAGE_ROWS {
            break;
        }
        offset += EXPORT_PAGE_ROWS;
    }
    writer.flush().context("flush export file")?;
    Ok(total)
}

/// Write an already-fetched result set to a delimited file.
pub fn export_result_csv(
    result: &ResultSet,
    path: &str,
    options: &crate::csv_export::ExportOptions,
) -> anyhow::Result<u64> {
    use anyhow::Context as _;

    let mut writer = open_export_writer(path, options.delimiter)?;
    if options.include_header {
        writer
            .write_record(result.columns.iter().map(|c| c.name.as_str()))
            .context("write header")?;
    }
    for (i, row) in result.rows.iter().enumerate() {
        writer
            .write_record(row.iter().map(crate::csv_export::field_text))
            .with_context(|| format!("write row {}", i + 1))?;
    }
    writer.flush().context("flush export file")?;
    Ok(result.rows.len() as u64)
}

/// Stream a delimited file into `schema.table` as batched multi-row INSERTs.
pub async fn import_csv(
    driver: DynDriver,
    schema: &str,
    table: &str,
    path: &str,
    options: &crate::csv_import::ImportOptions,
    progress: impl Fn(u64),
) -> anyhow::Result<u64> {
    use anyhow::Context as _;

    let kind = driver.kind();
    let file_path = expand_home(path);
    let file = std::fs::File::open(&file_path)
        .with_context(|| format!("open {}", file_path.display()))?;
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(options.delimiter)
        .has_headers(options.has_header)
        .flexible(true)
        .from_reader(file);

    let table_schema = driver
        .describe_table(schema, table)
        .await
        .with_context(|| format!("describe {schema}.{table}"))?;

    // Column list: header names matched (case-insensitively) to real columns,
    // or the table's columns in ordinal order when there is no header row.
    let columns: Vec<String> = if options.has_header {
        let headers = reader.headers().context("read header row")?.clone();
        let mut cols = Vec::with_capacity(headers.len());
        let mut unknown: Vec<String> = Vec::new();
        for h in headers.iter() {
            let name = h.trim();
            match table_schema
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
            {
                Some(c) => cols.push(c.name.clone()),
                None => unknown.push(name.to_string()),
            }
        }
        if !unknown.is_empty() {
            anyhow::bail!(
                "header column(s) not found in {schema}.{table}: {}",
                unknown.join(", ")
            );
        }
        cols
    } else {
        table_schema.columns.iter().map(|c| c.name.clone()).collect()
    };
    if columns.is_empty() {
        anyhow::bail!("no columns to import");
    }

    let mut total: u64 = 0;
    let mut batch: Vec<Vec<Option<String>>> = Vec::with_capacity(IMPORT_BATCH_ROWS);
    let mut line = if options.has_header { 1u64 } else { 0u64 };
    for record in reader.records() {
        line += 1;
        let record = record.with_context(|| format!("parse error at line {line}"))?;
        if record.len() > columns.len() {
            anyhow::bail!(
                "line {line} has {} fields but only {} target column(s)",
                record.len(),
                columns.len()
            );
        }
        let mut row: Vec<Option<String>> = Vec::with_capacity(columns.len());
        for i in 0..columns.len() {
            let field = record.get(i).unwrap_or("");
            if field.is_empty() && options.empty_as_null {
                row.push(None);
            } else {
                row.push(Some(field.to_string()));
            }
        }
        batch.push(row);
        if batch.len() >= IMPORT_BATCH_ROWS {
            let sql = crate::csv_import::build_insert(kind, schema, table, &columns, &batch);
            driver
                .query(&sql)
                .await
                .with_context(|| format!("insert failed near line {line} ({total} row(s) already imported)"))?;
            total += batch.len() as u64;
            batch.clear();
            progress(total);
        }
    }
    if !batch.is_empty() {
        let sql = crate::csv_import::build_insert(kind, schema, table, &columns, &batch);
        driver
            .query(&sql)
            .await
            .with_context(|| format!("insert failed in final batch ({total} row(s) already imported)"))?;
        total += batch.len() as u64;
    }
    Ok(total)
}
