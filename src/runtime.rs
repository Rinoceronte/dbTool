use crate::ai_session::{self, ApprovalDecision, SessionEvent, SessionId, SessionInputs};
use crate::claude_auth::{self, AuthStatus};
use crate::db::{
    self, ConnectParams, DbMeta, DynDriver, PkValues, ResultSet, RowChanges, RowsFilter,
    SchemaObjects, TableInfo, TableSchema, structure::TableStructure,
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
    /// List server databases/catalogs (for the per-connection database toggles).
    ListDatabases { req: RequestId, conn: ConnectionId },
    ListTables { req: RequestId, conn: ConnectionId, schema: String },
    /// Functions, sequences, enums, triggers of one schema.
    ListSchemaObjects { req: RequestId, conn: ConnectionId, schema: String },
    /// Fetch a routine's source for viewing.
    RoutineDdl { req: RequestId, conn: ConnectionId, schema: String, name: String, kind: String },
    TableComments { req: RequestId, conn: ConnectionId, schema: String, table: String },
    DescribeTable { req: RequestId, conn: ConnectionId, schema: String, table: String },
    Query {
        req: RequestId,
        conn: ConnectionId,
        sql: String,
        /// Lift the result row cap for this run ("Fetch all rows").
        unlimited: bool,
    },
    /// Abort a running Query task by its request id.
    CancelQuery { req: RequestId },
    FetchTableRows {
        req: RequestId,
        conn: ConnectionId,
        schema: String,
        table: String,
        limit: i64,
        offset: i64,
        filter: RowsFilter,
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
    /// Row-level data compare of the selected tables across two connections.
    DataCompare {
        req: RequestId,
        source: ConnectionId,
        target: ConnectionId,
        tables: Vec<crate::db::datasync::TableSel>,
        diff_cap: usize,
    },
    /// Sanitized full replace: DELETE + re-INSERT each table on the target,
    /// masking in flight. Tables must already be in FK-safe order.
    DataPull {
        req: RequestId,
        source: ConnectionId,
        target: ConnectionId,
        tables: Vec<crate::db::datasync::TableSel>,
        masks: crate::db::datasync::MaskMap,
        row_limit: Option<u64>,
    },
    /// Column-mapped copy of tables across connections (engines may differ).
    /// Specs arrive in FK-safe insert order (parents first).
    DataTransfer {
        req: RequestId,
        source: ConnectionId,
        target: ConnectionId,
        specs: Vec<crate::db::datasync::TransferSpec>,
        /// Clear every included target table (children first) before inserting.
        delete_first: bool,
    },
    /// Stop a running DataTransfer after its current batch; rows already
    /// copied stay in the target.
    CancelTransfer { req: RequestId },
    /// Whole-database dump via the engine's native tool (pg_dump/mysqldump;
    /// SQLite = file copy). Tunnelled profiles get a fresh tunnel.
    DumpDatabase { req: RequestId, conn: ConnectionId, path: String },
    /// Restore a dump written by DumpDatabase into this connection's database.
    RestoreDatabase { req: RequestId, conn: ConnectionId, path: String },
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
    /// Ask GitHub for the latest release. Manual checks (from Settings) get
    /// explicit "up to date" / failure events; the startup check stays silent.
    CheckForUpdate { manual: bool },
    /// Download the Windows installer and run it silently.
    ApplyUpdate { url: String, version: String },
}

#[derive(Debug)]
pub enum Event {
    Connected { req: RequestId, conn: ConnectionId },
    ConnectFailed { req: RequestId, error: String },
    Schemas { req: RequestId, conn: ConnectionId, schemas: Vec<String> },
    Databases { req: RequestId, conn: ConnectionId, databases: Vec<String> },
    Tables { req: RequestId, conn: ConnectionId, schema: String, tables: Vec<TableInfo> },
    SchemaObjects { req: RequestId, conn: ConnectionId, schema: String, objects: SchemaObjects },
    RoutineDdl { req: RequestId, conn: ConnectionId, name: String, ddl: String },
    TableComments {
        req: RequestId,
        table_comment: Option<String>,
        columns: Vec<(String, Option<String>)>,
    },
    TableDescribed {
        req: RequestId,
        conn: ConnectionId,
        schema: String,
        table: String,
        table_schema: TableSchema,
    },
    QueryResult { req: RequestId, results: Vec<ResultSet> },
    /// Keepalive rebuilt a dead connection under the same id.
    Reconnected { conn: ConnectionId },
    /// Keepalive found the connection dead and could not rebuild it (yet).
    ConnectionLost { conn: ConnectionId, error: String },
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
    /// Free-form progress line for long data compare/pull runs.
    DataProgress { req: RequestId, message: String },
    /// A dump or restore finished; message is the human summary.
    BackupDone { req: RequestId, message: String },
    DataCompared {
        req: RequestId,
        reports: Vec<crate::db::datasync::TableReport>,
    },
    DataPulled {
        req: RequestId,
        tables: usize,
        rows: u64,
        errors: Vec<String>,
    },
    DataTransferred {
        req: RequestId,
        tables: usize,
        rows: u64,
        errors: Vec<String>,
    },
    ImportProgress { req: RequestId, rows: u64 },
    Imported { req: RequestId, rows: u64 },
    ExportProgress { req: RequestId, rows: u64 },
    Exported { req: RequestId, rows: u64 },
    Ai { session: SessionId, event: SessionEvent },
    /// The session task ended (any reason). Emitted after the final SessionEvent.
    AiSessionEnded { session: SessionId },
    /// A newer release exists on GitHub.
    UpdateAvailable { info: crate::update::UpdateInfo },
    /// A manual check found no newer release.
    UpdateUpToDate,
    /// A manual check failed (offline, rate limit, …).
    UpdateCheckFailed { error: String },
    /// The Windows installer was downloaded and launched; the app should exit
    /// so the installer can replace it (it relaunches dbTool when done).
    UpdateInstallerLaunched,
    UpdateFailed { error: String },
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

/// One live connection: the driver plus, when tunnelled, the ssh child whose
/// drop (kill_on_drop) tears the tunnel down with the connection. Params are
/// kept so the keepalive loop can rebuild a dead connection in place.
struct ConnEntry {
    driver: DynDriver,
    _tunnel: Option<tokio::process::Child>,
    params: db::ConnectParams,
}

type Connections = Arc<RwLock<HashMap<ConnectionId, ConnEntry>>>;

struct SessionHandle {
    cancel: CancellationToken,
    approval_tx: mpsc::UnboundedSender<ApprovalDecision>,
}

type Sessions = Arc<RwLock<HashMap<SessionId, SessionHandle>>>;

/// Cancellation tokens of in-flight editor queries, keyed by request id.
/// Cancelling one lets the driver abort SERVER-side (pg_cancel_backend /
/// KILL QUERY) before the task returns.
type QueryCancels = Arc<RwLock<HashMap<RequestId, CancellationToken>>>;

async fn worker_main(
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    evt_tx: mpsc::UnboundedSender<Event>,
    ctx: egui::Context,
) {
    let connections: Connections = Arc::new(RwLock::new(HashMap::new()));
    let sessions: Sessions = Arc::new(RwLock::new(HashMap::new()));
    let cancels: QueryCancels = Arc::new(RwLock::new(HashMap::new()));
    let mcp = match McpServer::spawn().await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[runtime] failed to start MCP server: {e:#}");
            // Continue without an MCP server — agent calls will fail later with a clear error.
            return;
        }
    };

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::CancelQuery { req } | Command::CancelTransfer { req } => {
                if let Some(t) = cancels.write().await.remove(&req) {
                    t.cancel();
                    // The query/transfer task reports its own cancellation.
                }
                continue;
            }
            // Intercepted here (not handle_command) to wire a cancel token.
            Command::DataTransfer { req, source, target, specs, delete_first } => {
                let token = CancellationToken::new();
                cancels.write().await.insert(req, token.clone());
                let evt_tx = evt_tx.clone();
                let ctx = ctx.clone();
                let connections = connections.clone();
                let cancels = cancels.clone();
                tokio::spawn(async move {
                    run_transfer(
                        req, source, target, specs, delete_first, &connections, &token,
                        &evt_tx, &ctx,
                    )
                    .await;
                    cancels.write().await.remove(&req);
                });
                continue;
            }
            Command::Query { req, conn, sql, unlimited } => {
                let token = CancellationToken::new();
                cancels.write().await.insert(req, token.clone());
                let evt_tx = evt_tx.clone();
                let ctx = ctx.clone();
                let connections = connections.clone();
                let cancels = cancels.clone();
                let cap_override = unlimited.then_some(usize::MAX);
                tokio::spawn(crate::db::RESULT_CAP_OVERRIDE.scope(cap_override, async move {
                    let Some(driver) = get_driver(&connections, conn).await else {
                        cancels.write().await.remove(&req);
                        return send(
                            &evt_tx,
                            &ctx,
                            Event::Error { req, error: "connection not found".into() },
                        );
                    };
                    let result = driver.query_script(&sql, token).await;
                    cancels.write().await.remove(&req);
                    match result {
                        Ok(results) => send(&evt_tx, &ctx, Event::QueryResult { req, results }),
                        Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
                    }
                }));
                continue;
            }
            _ => {}
        }
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

/// A DataTransfer run: bulk clears, then each spec in FK-safe order, sharing
/// one key map. Stops between batches when `cancel` fires; copied rows stay.
#[allow(clippy::too_many_arguments)]
async fn run_transfer(
    req: RequestId,
    source: ConnectionId,
    target: ConnectionId,
    specs: Vec<crate::db::datasync::TransferSpec>,
    delete_first: bool,
    connections: &Connections,
    cancel: &CancellationToken,
    evt_tx: &mpsc::UnboundedSender<Event>,
    ctx: &egui::Context,
) {
    let (Some(src), Some(tgt)) = (
        get_driver(connections, source).await,
        get_driver(connections, target).await,
    ) else {
        return send(evt_tx, ctx, Event::Error { req, error: "connection not found".into() });
    };
    let mut rows = 0u64;
    let mut done = 0usize;
    let mut errors = Vec::new();
    if delete_first {
        // Children first (specs are parents-first insert order).
        for spec in specs.iter().rev() {
            let tbl = format!(
                "{}.{}",
                crate::db::quote_ident(tgt.kind(), &spec.target_schema),
                crate::db::quote_ident(tgt.kind(), &spec.target_table)
            );
            if let Err(e) = tgt.query(&format!("DELETE FROM {tbl}")).await {
                errors.push(format!(
                    "{}.{}: clear failed: {e:#}",
                    spec.target_schema, spec.target_table
                ));
            }
        }
    }
    let mut key_maps = crate::db::datasync::KeyMaps::default();
    for spec in &specs {
        if cancel.is_cancelled() {
            errors.push("stopped by user — remaining tables skipped".into());
            break;
        }
        let progress = |message: String| {
            send(evt_tx, ctx, Event::DataProgress { req, message });
        };
        let masks = crate::db::datasync::MaskMap::new();
        match crate::db::datasync::transfer_table(
            &src, &tgt, spec, &masks, &mut key_maps, cancel, &progress,
        )
        .await
        {
            Ok(n) => {
                rows += n;
                done += 1;
            }
            Err(e) => errors.push(format!(
                "{}.{}: {e:#}",
                spec.source_schema, spec.source_table
            )),
        }
    }
    send(evt_tx, ctx, Event::DataTransferred { req, tables: done, rows, errors });
}

/// Ping every minute; on failure, rebuild the connection (and tunnel) IN
/// PLACE under the same ConnectionId so open tabs keep working. Exits when
/// the connection is removed (user disconnect).
fn spawn_keepalive(
    conn: ConnectionId,
    connections: Connections,
    evt_tx: mpsc::UnboundedSender<Event>,
    ctx: egui::Context,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let Some(driver) = get_driver(&connections, conn).await else { break };
            if driver.ping().await.is_ok() {
                continue;
            }
            let params = match connections.read().await.get(&conn) {
                Some(e) => e.params.clone(),
                None => break,
            };
            match db::connect(&params).await {
                Ok((driver, tunnel)) => {
                    let mut map = connections.write().await;
                    match map.get_mut(&conn) {
                        Some(e) => {
                            e.driver = driver;
                            e._tunnel = tunnel;
                            send(&evt_tx, &ctx, Event::Reconnected { conn });
                        }
                        None => break,
                    }
                }
                Err(e) => {
                    send(
                        &evt_tx,
                        &ctx,
                        Event::ConnectionLost { conn, error: format!("{e:#}") },
                    );
                    // Keep trying on the next tick.
                }
            }
        }
    });
}

async fn get_driver(connections: &Connections, conn: ConnectionId) -> Option<DynDriver> {
    connections.read().await.get(&conn).map(|e| e.driver.clone())
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
            Ok((driver, tunnel)) => {
                let id = ConnectionId::new_v4();
                connections
                    .write()
                    .await
                    .insert(id, ConnEntry { driver, _tunnel: tunnel, params });
                spawn_keepalive(id, connections.clone(), evt_tx.clone(), ctx.clone());
                send(&evt_tx, &ctx, Event::Connected { req, conn: id });
            }
            Err(e) => send(&evt_tx, &ctx, Event::ConnectFailed { req, error: format!("{e:#}") }),
        },

        Command::Disconnect { conn } => {
            connections.write().await.remove(&conn);
        }

        Command::DumpDatabase { req, conn, path } => {
            let params = match connections.read().await.get(&conn) {
                Some(e) => e.params.clone(),
                None => {
                    return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
                }
            };
            match backup_database(&params, &path, false).await {
                Ok(message) => send(&evt_tx, &ctx, Event::BackupDone { req, message }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::RestoreDatabase { req, conn, path } => {
            let params = match connections.read().await.get(&conn) {
                Some(e) => e.params.clone(),
                None => {
                    return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
                }
            };
            match backup_database(&params, &path, true).await {
                Ok(message) => send(&evt_tx, &ctx, Event::BackupDone { req, message }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::DataCompare { req, source, target, tables, diff_cap } => {
            let (Some(src), Some(tgt)) = (
                get_driver(&connections, source).await,
                get_driver(&connections, target).await,
            ) else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            let mut reports = Vec::with_capacity(tables.len());
            for (i, sel) in tables.iter().enumerate() {
                send(
                    &evt_tx,
                    &ctx,
                    Event::DataProgress {
                        req,
                        message: format!("comparing {} ({}/{})", sel.key(), i + 1, tables.len()),
                    },
                );
                reports.push(crate::db::datasync::compare_table(&src, &tgt, sel, diff_cap).await);
            }
            send(&evt_tx, &ctx, Event::DataCompared { req, reports });
        }

        Command::DataPull { req, source, target, tables, masks, row_limit } => {
            let (Some(src), Some(tgt)) = (
                get_driver(&connections, source).await,
                get_driver(&connections, target).await,
            ) else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            let mut rows = 0u64;
            let mut done = 0usize;
            let mut errors = Vec::new();
            // Deletes must run children-first (reverse FK order), inserts
            // parents-first. Clearing everything up front keeps both happy.
            for sel in tables.iter().rev() {
                let tbl = format!(
                    "{}.{}",
                    crate::db::quote_ident(tgt.kind(), &sel.schema),
                    crate::db::quote_ident(tgt.kind(), &sel.table)
                );
                if let Err(e) = tgt.query(&format!("DELETE FROM {tbl}")).await {
                    errors.push(format!("{}: clear failed: {e:#}", sel.key()));
                }
            }
            for sel in &tables {
                let progress = |message: String| {
                    send(&evt_tx, &ctx, Event::DataProgress { req, message });
                };
                match crate::db::datasync::pull_table(&src, &tgt, sel, &masks, row_limit, &progress)
                    .await
                {
                    Ok(n) => {
                        rows += n;
                        done += 1;
                    }
                    Err(e) => errors.push(format!("{}: {e:#}", sel.key())),
                }
            }
            send(&evt_tx, &ctx, Event::DataPulled { req, tables: done, rows, errors });
        }

        // DataTransfer/CancelTransfer are intercepted in worker_main so a
        // cancel token can be registered; they never reach here.
        Command::DataTransfer { .. } | Command::CancelTransfer { .. } => {}

        Command::ListSchemas { req, conn } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.list_schemas().await {
                Ok(schemas) => send(&evt_tx, &ctx, Event::Schemas { req, conn, schemas }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::ListDatabases { req, conn } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.list_databases().await {
                Ok(databases) => send(&evt_tx, &ctx, Event::Databases { req, conn, databases }),
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

        Command::Query { req, conn, sql, .. } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.query(&sql).await {
                Ok(result) => {
                    send(&evt_tx, &ctx, Event::QueryResult { req, results: vec![result] })
                }
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::CancelQuery { .. } => {
            // Handled inline in worker_main; unreachable here.
        }

        Command::ListSchemaObjects { req, conn, schema } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.list_schema_objects(&schema).await {
                Ok(objects) => send(
                    &evt_tx,
                    &ctx,
                    Event::SchemaObjects { req, conn, schema, objects },
                ),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::RoutineDdl { req, conn, schema, name, kind } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            // "view" rides the same pipe: its CREATE comes from table_ddl.
            let result = if kind == "view" {
                driver.table_ddl(&schema, &name, crate::db::TableKind::View).await
            } else {
                driver.routine_ddl(&schema, &name, &kind).await
            };
            match result {
                Ok(ddl) => send(&evt_tx, &ctx, Event::RoutineDdl { req, conn, name, ddl }),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::TableComments { req, conn, schema, table } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.table_comments(&schema, &table).await {
                Ok((table_comment, columns)) => send(
                    &evt_tx,
                    &ctx,
                    Event::TableComments { req, table_comment, columns },
                ),
                Err(e) => send(&evt_tx, &ctx, Event::Error { req, error: format!("{e:#}") }),
            }
        }

        Command::FetchTableRows { req, conn, schema, table, limit, offset, filter } => {
            let Some(driver) = get_driver(&connections, conn).await else {
                return send(&evt_tx, &ctx, Event::Error { req, error: "connection not found".into() });
            };
            match driver.fetch_table_rows(&schema, &table, limit, offset, &filter).await {
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

        Command::CheckForUpdate { manual } => match crate::update::check().await {
            Ok(Some(info)) => send(&evt_tx, &ctx, Event::UpdateAvailable { info }),
            Ok(None) if manual => send(&evt_tx, &ctx, Event::UpdateUpToDate),
            Ok(None) => {}
            Err(e) if manual => {
                send(&evt_tx, &ctx, Event::UpdateCheckFailed { error: format!("{e:#}") })
            }
            // Startup check stays silent on failure (offline, rate limit, …).
            Err(e) => log::warn!("update check failed: {e:#}"),
        },

        Command::ApplyUpdate { url, version } => {
            let result = async {
                let path = crate::update::download_installer(&url, &version).await?;
                crate::update::launch_installer(&path)
            }
            .await;
            match result {
                Ok(()) => send(&evt_tx, &ctx, Event::UpdateInstallerLaunched),
                Err(e) => send(&evt_tx, &ctx, Event::UpdateFailed { error: format!("{e:#}") }),
            }
        }

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

/// Dump or restore a whole database with the engine's native tool.
/// Tunnelled profiles get their own tunnel for the run. Postgres uses the
/// custom format (`pg_dump -Fc` / `pg_restore --clean --if-exists`);
/// MySQL plain SQL (`mysqldump` / `mysql < file`); SQLite copies the file.
async fn backup_database(
    params: &db::ConnectParams,
    path: &str,
    restore: bool,
) -> anyhow::Result<String> {
    use anyhow::Context as _;

    let file = expand_home(path);
    if restore && !file.exists() {
        anyhow::bail!("{} does not exist", file.display());
    }
    if !restore {
        if let Some(parent) = file.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
        }
    }

    if params.kind == db::DbKind::Sqlite {
        // The database IS a file; a copy is a full dump.
        let db_file = std::path::PathBuf::from(&params.database);
        if restore {
            std::fs::copy(&file, &db_file)
                .with_context(|| format!("copy {} → {}", file.display(), db_file.display()))?;
            return Ok(format!("Restored {} from {}.", db_file.display(), file.display()));
        }
        std::fs::copy(&db_file, &file)
            .with_context(|| format!("copy {} → {}", db_file.display(), file.display()))?;
        return Ok(format!("Dumped {} to {}.", db_file.display(), file.display()));
    }

    // Reach tunnelled servers through a fresh forward for the run.
    let (mut host, mut port) = (params.host.clone(), params.port);
    let _tunnel = match &params.ssh {
        Some(ssh) => {
            let (child, local_port) = db::open_ssh_tunnel(ssh, &params.host, params.port).await?;
            host = "127.0.0.1".into();
            port = local_port;
            Some(child)
        }
        None => None,
    };

    let mut cmd = match (params.kind, restore) {
        (db::DbKind::Postgres, false) => {
            let mut c = tokio::process::Command::new("pg_dump");
            c.arg("-h").arg(&host)
                .arg("-p").arg(port.to_string())
                .arg("-U").arg(&params.username)
                .arg("-d").arg(&params.database)
                .arg("-Fc")
                .arg("-f").arg(&file)
                .env("PGPASSWORD", &params.password);
            c
        }
        (db::DbKind::Postgres, true) => {
            let mut c = tokio::process::Command::new("pg_restore");
            c.arg("-h").arg(&host)
                .arg("-p").arg(port.to_string())
                .arg("-U").arg(&params.username)
                .arg("-d").arg(&params.database)
                .arg("--clean")
                .arg("--if-exists")
                .arg("--no-owner")
                .arg(&file)
                .env("PGPASSWORD", &params.password);
            c
        }
        (db::DbKind::MySql, false) => {
            let mut c = tokio::process::Command::new("mysqldump");
            c.arg("-h").arg(&host)
                .arg("-P").arg(port.to_string())
                .arg("-u").arg(&params.username)
                .arg("--single-transaction")
                .arg("--routines")
                .arg("--result-file").arg(&file)
                .arg(&params.database)
                .env("MYSQL_PWD", &params.password);
            c
        }
        (db::DbKind::MySql, true) => {
            let mut c = tokio::process::Command::new("mysql");
            c.arg("-h").arg(&host)
                .arg("-P").arg(port.to_string())
                .arg("-u").arg(&params.username)
                .arg(&params.database)
                .env("MYSQL_PWD", &params.password)
                .stdin(std::fs::File::open(&file).with_context(|| format!("open {}", file.display()))?);
            c
        }
        (db::DbKind::MsSql, _) => {
            anyhow::bail!(
                "SQL Server has no portable CLI dump — use BACKUP DATABASE on the server \
                 or SSMS/sqlpackage."
            );
        }
        (db::DbKind::Sqlite, _) => unreachable!(),
    };
    let tool = format!("{:?}", cmd.as_std().get_program());
    let output = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawn {tool} (is it installed and on PATH?)"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "{tool} exited with {}: {}",
            output.status,
            err.trim().chars().take(2000).collect::<String>()
        );
    }
    Ok(if restore {
        format!("Restored '{}' from {}.", params.database, file.display())
    } else {
        format!("Dumped '{}' to {}.", params.database, file.display())
    })
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

/// Incremental writer for a JSON array of row objects.
struct JsonArrayWriter {
    out: std::io::BufWriter<std::fs::File>,
    any: bool,
}

impl JsonArrayWriter {
    fn create(path: &str) -> anyhow::Result<Self> {
        use anyhow::Context as _;
        use std::io::Write as _;
        let file_path = expand_home(path);
        if let Some(parent) = file_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
        }
        let file = std::fs::File::create(&file_path)
            .with_context(|| format!("create {}", file_path.display()))?;
        let mut out = std::io::BufWriter::new(file);
        out.write_all(b"[").context("write JSON opening")?;
        Ok(Self { out, any: false })
    }

    fn write_row(
        &mut self,
        columns: &[crate::db::Column],
        row: &[crate::db::Value],
    ) -> anyhow::Result<()> {
        use std::io::Write as _;
        if self.any {
            self.out.write_all(b",")?;
        }
        self.out.write_all(b"\n  ")?;
        self.out
            .write_all(crate::csv_export::row_as_json(columns, row).as_bytes())?;
        self.any = true;
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<()> {
        use std::io::Write as _;
        self.out.write_all(b"\n]\n")?;
        self.out.flush()?;
        Ok(())
    }
}

/// Stream every row of `schema.table` into a file (CSV or JSON), paging
/// through the table so large tables never sit in memory all at once.
pub async fn export_table_csv(
    driver: DynDriver,
    schema: &str,
    table: &str,
    path: &str,
    options: &crate::csv_export::ExportOptions,
    progress: impl Fn(u64),
) -> anyhow::Result<u64> {
    use anyhow::Context as _;
    use crate::csv_export::ExportFormat;

    let mut csv_writer = match options.format {
        ExportFormat::Csv => Some(open_export_writer(path, options.delimiter)?),
        ExportFormat::Json => None,
    };
    let mut json_writer = match options.format {
        ExportFormat::Json => Some(JsonArrayWriter::create(path)?),
        ExportFormat::Csv => None,
    };
    let mut total: u64 = 0;
    let mut offset: i64 = 0;
    let mut first_page = true;
    loop {
        let rs = driver
            .fetch_table_rows(schema, table, EXPORT_PAGE_ROWS, offset, &RowsFilter::default())
            .await
            .with_context(|| format!("fetch rows at offset {offset}"))?;
        if first_page {
            if options.include_header {
                if let Some(w) = csv_writer.as_mut() {
                    w.write_record(rs.columns.iter().map(|c| c.name.as_str()))
                        .context("write header")?;
                }
            }
            first_page = false;
        }
        let page_rows = rs.rows.len();
        for row in &rs.rows {
            if let Some(w) = csv_writer.as_mut() {
                w.write_record(row.iter().map(crate::csv_export::field_text))
                    .with_context(|| format!("write row {}", total + 1))?;
            }
            if let Some(w) = json_writer.as_mut() {
                w.write_row(&rs.columns, row)
                    .with_context(|| format!("write row {}", total + 1))?;
            }
            total += 1;
        }
        progress(total);
        if (page_rows as i64) < EXPORT_PAGE_ROWS {
            break;
        }
        offset += EXPORT_PAGE_ROWS;
    }
    if let Some(mut w) = csv_writer {
        w.flush().context("flush export file")?;
    }
    if let Some(w) = json_writer {
        w.finish().context("finish JSON export")?;
    }
    Ok(total)
}

/// Write an already-fetched result set to a file (CSV or JSON).
pub fn export_result_csv(
    result: &ResultSet,
    path: &str,
    options: &crate::csv_export::ExportOptions,
) -> anyhow::Result<u64> {
    use anyhow::Context as _;
    use crate::csv_export::ExportFormat;

    match options.format {
        ExportFormat::Csv => {
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
        }
        ExportFormat::Json => {
            let mut writer = JsonArrayWriter::create(path)?;
            for (i, row) in result.rows.iter().enumerate() {
                writer
                    .write_row(&result.columns, row)
                    .with_context(|| format!("write row {}", i + 1))?;
            }
            writer.finish().context("finish JSON export")?;
        }
    }
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
