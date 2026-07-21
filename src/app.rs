use std::collections::{BTreeMap, BTreeSet, HashMap};

use eframe::CreationContext;
use uuid::Uuid;

use crate::connections::manager_ui::{ManagerAction, ManagerState, TestResult};
use crate::connections::{self, ConnectionProfile};
use crate::runtime::{Command, ConnectionId, Event, RequestId, Runtime};
use crate::ui::ai_panel::{AiPanelAction, AiPanelState};
use crate::ui::auth_dialog::{AuthAction, AuthState};
use crate::ui::import_export::{
    DumpAction, DumpState, ExportAction, ExportSource, ExportState, ImportAction, ImportState,
};
use crate::db::structure::WorkingTable;
use crate::settings::Settings;
use crate::ui::theme;
use crate::ui::{
    ActiveConnection, EditorView, QueryTab, SchemaNode, StructSel, StructureState, Tab, TabId,
    TabStatus, TableEditorTab,
};

const READ_ONLY_MSG: &str =
    "Read-only connection: write refused. Uncheck Read-only in the connection settings to allow \
     writes.";

enum Pending {
    Connect { profile_id: Uuid, database: String, primary: bool },
    TestConnection,
    ListSchemas,
    ListTables,
    ListSchemaObjects,
    ListDatabases,
    RoutineDdl,
    DescribeTable(TabId),
    Query(TabId),
    TableRows(TabId),
    /// Row fetch for the FK-peek popup.
    FkPeek,
    /// Row fetch for the FK hover preview.
    FkHover,
    /// Session list refresh for a sessions-monitor tab.
    SessionsList(TabId),
    /// A cancel/kill fired from a sessions-monitor tab; refresh follows.
    SessionsKill(TabId),
    /// One-cell UPDATE fired from an editable query grid.
    QueryCellEdit(TabId),
    /// Row INSERT fired from an editable query grid's draft form.
    QueryRowInsert(TabId),
    /// Structured EXPLAIN for the visual plan tree.
    ExplainPlan(TabId),
    /// Source-side introspection for a data-sync tab.
    DataSyncMeta(TabId),
    /// Whole-database dump or restore.
    Backup,
    /// A data-sync tab's compare or pull run.
    DataSyncRun(TabId),
    /// Comment introspection for the Comments dialog.
    TableComments,
    /// The Comments dialog's COMMENT ON / ALTER script.
    ApplyComments,
    RowInsert(TabId),
    ApplyChanges(TabId),
    DescribeStructure(TabId),
    ApplyDdl(TabId),
    FetchDbMeta,
    DumpDdl,
    DumpDbml(ConnectionId), // first generation, creates the database's file
    RefreshDbml(TabId),     // merge fresh structure into an open diagram tab
    /// One side of a compare tab's introspection.
    CompareSnapshot { tab: TabId, left: bool, kind: crate::db::DbKind },
    CompareApplyDdl(TabId),
    ImportCsv,
    ExportCsv,
    AuthStatus,
    AuthLogin,
    AuthLogout,
}

pub struct App {
    runtime: Runtime,
    profiles: Vec<ConnectionProfile>,
    manager: ManagerState,
    active: Vec<ActiveConnection>,
    pending: HashMap<RequestId, Pending>,
    tabs: Vec<Tab>,
    active_tab: Option<TabId>,
    status: Option<String>,
    test_probe_conn: Option<ConnectionId>,
    ai_panel: AiPanelState,
    auth: AuthState,
    settings: Settings,
    settings_open: bool,
    /// Whether the OS window had focus this frame (desktop notifications).
    window_focused: bool,
    dump_dialog: Option<DumpState>,
    backup_dialog: Option<crate::ui::import_export::BackupState>,
    import_dialog: Option<ImportState>,
    export_dialog: Option<ExportState>,
    dbml_file_dialog: egui_file_dialog::FileDialog,
    /// File dialog for query-tab .sql open/save, plus which tab it serves.
    sql_file_dialog: egui_file_dialog::FileDialog,
    sql_file_target: Option<(TabId, SqlFileMode)>,
    /// Database-toggle picker, keyed by the profile it is open for.
    db_picker: Option<Uuid>,
    /// Dirty diagram tab whose close button was clicked once; a second click
    /// discards the unsaved changes.
    close_armed: Option<TabId>,
    /// Full-value cell viewer window: (title, content).
    cell_viewer: Option<(String, String)>,
    /// FK "peek" popup: the referenced row(s), without leaving the current tab.
    fk_peek: Option<FkPeek>,
    /// Hover preview over an FK cell; appears after a short dwell.
    fk_hover: Option<FkHover>,
    /// Query history popup state; entries load lazily on first open.
    history_open: bool,
    history_filter: String,
    history: Option<Vec<crate::history::HistoryEntry>>,
    /// egui input time of the current frame (for handlers without ctx).
    last_input_time: f64,
    /// Table/column comments editor (tree context menu → Comments…).
    comments_dialog: Option<CommentsDialog>,
    // Search-everywhere palette (Ctrl+P).
    palette_open: bool,
    palette_query: String,
    palette_index: usize,
    palette_focus: bool,
    /// Parameterized run waiting for values (`:name` / `?` placeholders).
    param_prompt: Option<ParamPrompt>,
    /// Snippets popup: the query tab it inserts into, when open.
    snippets_target: Option<TabId>,
    /// Loaded lazily when the popup first opens.
    snippets: Option<Vec<crate::snippets::Snippet>>,
    snippet_new_name: String,
}

/// A Run paused on its placeholder values.
struct ParamPrompt {
    tab_id: TabId,
    /// The SQL that will run, still holding its placeholders.
    sql: String,
    allow_qmark: bool,
    full_run: bool,
    /// (name, value text) in appearance order.
    values: Vec<(String, String)>,
}

/// State of the table/column Comments dialog.
struct CommentsDialog {
    conn: ConnectionId,
    kind: crate::db::DbKind,
    schema: String,
    table: String,
    loading: bool,
    applying: bool,
    error: Option<String>,
    table_comment: String,
    orig_table_comment: String,
    /// (column, comment, original comment)
    columns: Vec<(String, String, String)>,
}

/// One row of the search-everywhere palette.
#[derive(Clone)]
struct PaletteHit {
    conn: ConnectionId,
    conn_name: String,
    schema: String,
    /// Table, view or routine name.
    name: String,
    /// Set when the hit is a column of `name`.
    column: Option<String>,
    kind: PaletteKind,
}

#[derive(Clone, PartialEq)]
enum PaletteKind {
    Table,
    View,
    Routine(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SqlFileMode {
    Open,
    Save,
}

/// State of the FK-peek window: which row(s) are referenced, and the fetch.
struct FkPeek {
    conn: ConnectionId,
    schema: String,
    table: String,
    filter: String,
    result: Option<crate::db::ResultSet>,
    error: Option<String>,
}

/// State of the FK hover preview: the referenced row's coordinates, the
/// source cell's rect (dismissal test) and the fetch.
struct FkHover {
    conn: ConnectionId,
    schema: String,
    table: String,
    filter: String,
    cell_rect: egui::Rect,
    /// Input time the dwell started; the popup shows after a short delay.
    since: f64,
    req: Option<RequestId>,
    result: Option<crate::db::ResultSet>,
    error: Option<String>,
}

impl App {
    pub fn new(cc: &CreationContext<'_>) -> Self {
        let runtime = Runtime::new(cc.egui_ctx.clone());
        let profiles = connections::load_profiles().unwrap_or_default();
        let settings = crate::settings::load();
        crate::db::set_result_row_cap(settings.max_result_rows);
        theme::install_fonts(&cc.egui_ctx);
        theme::install(&cc.egui_ctx, settings.theme);
        let mut app = Self {
            runtime,
            profiles,
            manager: ManagerState::default(),
            active: Vec::new(),
            pending: HashMap::new(),
            tabs: Vec::new(),
            active_tab: None,
            status: None,
            test_probe_conn: None,
            ai_panel: AiPanelState::default(),
            auth: AuthState::default(),
            settings,
            settings_open: false,
            window_focused: true,
            dump_dialog: None,
            backup_dialog: None,
            import_dialog: None,
            export_dialog: None,
            dbml_file_dialog: egui_file_dialog::FileDialog::new(),
            sql_file_dialog: egui_file_dialog::FileDialog::new(),
            sql_file_target: None,
            db_picker: None,
            close_armed: None,
            cell_viewer: None,
            fk_peek: None,
            fk_hover: None,
            history_open: false,
            history_filter: String::new(),
            history: None,
            last_input_time: 0.0,
            comments_dialog: None,
            palette_open: false,
            palette_query: String::new(),
            palette_index: 0,
            palette_focus: false,
            param_prompt: None,
            snippets_target: None,
            snippets: None,
            snippet_new_name: String::new(),
        };
        // Restore last session's query tabs, detached until their profile
        // connects (Run shows a hint; completion returns once connected).
        let saved = crate::session::load();
        for st in &saved.tabs {
            let mut tab = QueryTab::new(
                st.profile_id,
                Uuid::nil(),
                st.database.clone(),
                st.title.clone(),
                st.sql.clone(),
            );
            tab.file_path = st.file_path.clone();
            tab.status = TabStatus::Info("restored — connect the profile to run".into());
            app.tabs.push(Tab::Query(tab));
        }
        if let Some(i) = saved.active {
            app.active_tab = app.tabs.get(i).map(|t| t.id());
        }
        if app.active_tab.is_none() {
            app.active_tab = app.tabs.first().map(|t| t.id());
        }
        // Probe Claude auth status at startup so the panel populates lazily.
        app.send(Pending::AuthStatus, |req| Command::AuthStatus { req });
        app
    }

    fn send(&mut self, op: Pending, cmd_builder: impl FnOnce(RequestId) -> Command) {
        let req = RequestId::new_v4();
        self.pending.insert(req, op);
        self.runtime.send(cmd_builder(req));
    }

    fn profile_read_only(&self, profile_id: Uuid) -> bool {
        self.profiles
            .iter()
            .find(|p| p.id == profile_id)
            .map(|p| p.read_only)
            .unwrap_or(false)
    }

    fn conn_read_only(&self, conn: ConnectionId) -> bool {
        self.find_active_by_conn(conn)
            .map(|a| self.profile_read_only(a.profile_id))
            .unwrap_or(false)
    }

    /// If the tab's profile is read-only, put an error on the tab and return
    /// true (caller must skip the write).
    fn refuse_readonly_tab(&mut self, tab_id: TabId) -> bool {
        let profile_id = self
            .tabs
            .iter()
            .find(|t| t.id() == tab_id)
            .and_then(|t| t.profile_id());
        let Some(pid) = profile_id else { return false };
        if !self.profile_read_only(pid) {
            return false;
        }
        match self.find_tab_mut(tab_id) {
            Some(Tab::Query(q)) => q.status = TabStatus::Error(READ_ONLY_MSG.into()),
            Some(Tab::TableEditor(t)) => t.status = TabStatus::Error(READ_ONLY_MSG.into()),
            _ => self.status = Some(READ_ONLY_MSG.into()),
        }
        true
    }

    /// The profile's primary connection (its own database), falling back to
    /// any of its database connections.
    fn find_active(&self, profile_id: Uuid) -> Option<&ActiveConnection> {
        self.active
            .iter()
            .find(|a| a.profile_id == profile_id && a.is_primary)
            .or_else(|| self.active.iter().find(|a| a.profile_id == profile_id))
    }

    fn find_active_by_conn(&self, conn: ConnectionId) -> Option<&ActiveConnection> {
        self.active.iter().find(|a| a.conn_id == conn)
    }

    fn find_active_by_conn_mut(&mut self, conn: ConnectionId) -> Option<&mut ActiveConnection> {
        self.active.iter_mut().find(|a| a.conn_id == conn)
    }

    fn find_tab_mut(&mut self, tab_id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id() == tab_id)
    }

    fn handle_manager_action(&mut self, action: ManagerAction) {
        match action {
            ManagerAction::None => {}
            ManagerAction::Connect { profile_id } => self.start_connect(profile_id),
            ManagerAction::Disconnect { profile_id } => self.disconnect_profile(profile_id),
            ManagerAction::CloseEdit => self.manager.close(),
            ManagerAction::Save { mut profile, password } => {
                let id = profile.id;
                let storage_msg = if password.is_empty() {
                    None
                } else {
                    match connections::save_password(&mut profile, &password) {
                        connections::PasswordStore::Keyring => Some("Saved (password in OS keyring).".to_string()),
                        connections::PasswordStore::ProfileFile => Some(format!(
                            "Saved. Keyring unavailable — password stored in plaintext at {}. For better security, run a secret-service daemon.",
                            profile_file_display()
                        )),
                    }
                };
                if let Some(existing) = self.profiles.iter_mut().find(|p| p.id == id) {
                    *existing = profile;
                } else {
                    self.profiles.push(profile);
                }
                if let Err(e) = connections::save_profiles(&self.profiles) {
                    self.status = Some(format!("Failed to save profiles: {e}"));
                } else if let Some(msg) = storage_msg {
                    self.status = Some(msg);
                } else {
                    self.status = Some("Saved.".to_string());
                }
                self.manager.close();
            }
            ManagerAction::Delete { profile_id } => {
                if let Some(p) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
                    connections::delete_password(p);
                }
                self.profiles.retain(|p| p.id != profile_id);
                if let Err(e) = connections::save_profiles(&self.profiles) {
                    self.status = Some(format!("Failed to save profiles: {e}"));
                }
                self.manager.close();
            }
            ManagerAction::TestConnection { profile, password } => {
                let params = profile.to_connect_params(password);
                self.manager.last_test = None;
                self.send(Pending::TestConnection, move |req| Command::Connect { req, params });
            }
            ManagerAction::ShowDatabases { profile_id } => {
                self.db_picker = Some(profile_id);
                // Refresh the server list each time the picker opens.
                if let Some(ac) = self.find_active(profile_id).filter(|a| a.is_primary) {
                    let conn = ac.conn_id;
                    self.send(Pending::ListDatabases, move |req| {
                        Command::ListDatabases { req, conn }
                    });
                }
            }
        }
    }

    fn start_connect(&mut self, profile_id: Uuid) {
        if self.active.iter().any(|a| a.profile_id == profile_id && a.is_primary) {
            self.status = Some("Already connected.".into());
            return;
        }
        let Some(profile) = self.profiles.iter().find(|p| p.id == profile_id).cloned() else {
            return;
        };
        let Some(password) = connections::load_password(&profile) else {
            self.status = Some(format!(
                "No password stored for '{}'. Open Edit to set it.",
                profile.name
            ));
            return;
        };
        let database = profile.database.clone();
        let params = profile.to_connect_params(password);
        self.send(
            Pending::Connect { profile_id, database, primary: true },
            move |req| Command::Connect { req, params },
        );
    }

    /// Open an additional connection to another database on the same server
    /// (a toggled-on database from the picker).
    fn connect_database(&mut self, profile_id: Uuid, database: String) {
        if self
            .active
            .iter()
            .any(|a| a.profile_id == profile_id && a.database == database)
        {
            return;
        }
        let Some(profile) = self.profiles.iter().find(|p| p.id == profile_id).cloned() else {
            return;
        };
        let Some(password) = connections::load_password(&profile) else {
            return;
        };
        let mut params = profile.to_connect_params(password);
        params.database = database.clone();
        self.send(
            Pending::Connect { profile_id, database, primary: false },
            move |req| Command::Connect { req, params },
        );
    }

    fn apply_event(&mut self, event: Event) {
        match event {
            Event::Connected { req, conn } => {
                let Some(op) = self.pending.remove(&req) else { return };
                match op {
                    Pending::Connect { profile_id, database, primary } => {
                        if let Some(profile) = self.profiles.iter().find(|p| p.id == profile_id) {
                            let base = if profile.name.is_empty() {
                                format!("{}@{}", profile.username, profile.host)
                            } else {
                                profile.name.clone()
                            };
                            let name = if primary {
                                base
                            } else {
                                format!("{base} / {database}")
                            };
                            let kind = profile.kind;
                            // A freshly connected primary also opens its
                            // remembered toggled-on databases.
                            let enabled = if primary {
                                profile.enabled_databases.clone()
                            } else {
                                Vec::new()
                            };
                            self.active.push(ActiveConnection {
                                profile_id,
                                conn_id: conn,
                                name,
                                kind,
                                database: database.clone(),
                                is_primary: primary,
                                server_databases: None,
                                schemas: Vec::new(),
                                schemas_loaded: false,
                                schema_cache: None,
                            });
                            self.status = Some("Connected.".into());
                            // Re-attach restored (or orphaned) query tabs
                            // that belong to this profile+database.
                            for t in &mut self.tabs {
                                if let Tab::Query(q) = t {
                                    if q.profile_id == profile_id
                                        && q.database == database
                                        && q.conn_id != conn
                                    {
                                        q.conn_id = conn;
                                        if matches!(q.status, TabStatus::Info(_)) {
                                            q.status = TabStatus::Idle;
                                        }
                                    }
                                }
                            }
                            self.send(Pending::ListSchemas, move |req| Command::ListSchemas {
                                req,
                                conn,
                            });
                            self.send(Pending::FetchDbMeta, move |req| {
                                Command::FetchDbMeta { req, conn }
                            });
                            for db in enabled {
                                if db != database {
                                    self.connect_database(profile_id, db);
                                }
                            }
                        }
                    }
                    Pending::TestConnection => {
                        self.manager.last_test = Some(TestResult::Ok);
                        self.test_probe_conn = Some(conn);
                        self.runtime.send(Command::Disconnect { conn });
                    }
                    _ => {}
                }
            }
            Event::ConnectFailed { req, error } => {
                let op = self.pending.remove(&req);
                match op {
                    Some(Pending::TestConnection) => {
                        self.manager.last_test = Some(TestResult::Err(error));
                    }
                    _ => {
                        self.status = Some(format!("Connect failed: {error}"));
                    }
                }
            }
            Event::Schemas { req, conn, schemas } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::ListSchemas = op {
                    if let Some(ac) = self.find_active_by_conn_mut(conn) {
                        ac.schemas = schemas
                            .into_iter()
                            .map(|name| SchemaNode {
                                name,
                                expanded: false,
                                tables: None,
                                objects: None,
                            })
                            .collect();
                        ac.schemas_loaded = true;
                    }
                }
            }
            Event::Databases { req, conn, databases } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::ListDatabases = op {
                    if let Some(ac) = self.find_active_by_conn_mut(conn) {
                        ac.server_databases = Some(databases);
                    }
                }
            }
            Event::Tables { req, conn, schema, tables } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::ListTables = op {
                    if let Some(ac) = self.find_active_by_conn_mut(conn) {
                        if let Some(node) = ac.schemas.iter_mut().find(|n| n.name == schema) {
                            node.tables = Some(tables);
                        }
                    }
                }
            }
            Event::TableDescribed { req, conn: _, schema: _, table: _, table_schema } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::DescribeTable(tab_id) = op {
                    if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                        t.table_schema = Some(table_schema);
                    }
                }
            }
            Event::SchemaObjects { req, conn, schema, objects } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::ListSchemaObjects = op {
                    if let Some(ac) = self.find_active_by_conn_mut(conn) {
                        if let Some(node) = ac.schemas.iter_mut().find(|n| n.name == schema) {
                            node.objects = Some(objects);
                        }
                    }
                }
            }
            Event::RoutineDdl { req, conn, name, ddl } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::RoutineDdl = op {
                    // Make the opened source directly re-runnable per dialect.
                    let kind = self.find_active_by_conn(conn).map(|a| a.kind);
                    let sql = match kind {
                        Some(crate::db::DbKind::MsSql) => {
                            // OBJECT_DEFINITION returns CREATE; 2016+ allows
                            // CREATE OR ALTER for in-place replace.
                            let patched = ddl.trim_start();
                            let upper = patched.to_ascii_uppercase();
                            if upper.starts_with("CREATE ") && !upper.starts_with("CREATE OR ALTER")
                            {
                                format!("-- {name} — edit and Run to replace\nCREATE OR ALTER {}\n", &patched[7..])
                            } else {
                                format!("-- {name}\n{ddl}\n")
                            }
                        }
                        Some(crate::db::DbKind::MySql) => format!(
                            "-- {name} — edit and Run to replace the body.\n\
                             -- MySQL has no CREATE OR REPLACE for routines: run\n\
                             -- DROP FUNCTION/PROCEDURE IF EXISTS `{name}`; first if it exists.\n\
                             {ddl}\n"
                        ),
                        _ => format!("-- {name} — edit and Run to replace\n{ddl}\n"),
                    };
                    self.open_query_tab(conn, sql);
                }
            }
            Event::TableComments { req, table_comment, columns } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::TableComments = op {
                    if let Some(d) = self.comments_dialog.as_mut() {
                        d.loading = false;
                        d.table_comment = table_comment.clone().unwrap_or_default();
                        d.orig_table_comment = d.table_comment.clone();
                        d.columns = columns
                            .into_iter()
                            .map(|(c, v)| {
                                let v = v.unwrap_or_default();
                                (c, v.clone(), v)
                            })
                            .collect();
                    }
                }
            }
            Event::QueryResult { req, results } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::ApplyComments = op {
                    if let Some(d) = self.comments_dialog.take() {
                        self.status = Some(format!("Comments updated on {}.{}", d.schema, d.table));
                        // Refresh the tree so the tooltip shows the new comment.
                        let (conn, schema) = (d.conn, d.schema);
                        self.send(Pending::ListTables, move |req| Command::ListTables {
                            req,
                            conn,
                            schema,
                        });
                    }
                    return;
                }
                if let Pending::SessionsList(tab_id) = op {
                    if let Some(Tab::Sessions(t)) = self.find_tab_mut(tab_id) {
                        t.rows = results.into_iter().next();
                        t.status = TabStatus::Idle;
                    }
                    return;
                }
                if let Pending::SessionsKill(tab_id) = op {
                    let now = self.last_input_time;
                    self.refresh_sessions(tab_id, now);
                    return;
                }
                if let Pending::ExplainPlan(tab_id) = op {
                    let conn = match self.find_tab_mut(tab_id) {
                        Some(Tab::Query(t)) => {
                            t.running_req = None;
                            Some(t.conn_id)
                        }
                        _ => None,
                    };
                    let kind = conn.and_then(|c| self.find_active_by_conn(c).map(|a| a.kind));
                    let parsed = kind.map(|k| crate::db::plan::parse(k, &results));
                    if let Some(Tab::Query(t)) = self.find_tab_mut(tab_id) {
                        match parsed {
                            Some(Ok(plan)) => {
                                t.plan = Some(plan);
                                t.status = TabStatus::Info("execution plan".into());
                            }
                            Some(Err(e)) => {
                                t.status = TabStatus::Error(format!("Plan parse failed: {e}"));
                            }
                            None => t.status = TabStatus::Idle,
                        }
                    }
                    return;
                }
                if let Pending::Query(tab_id) = op {
                    let focused = self.window_focused;
                    if let Some(Tab::Query(t)) = self.find_tab_mut(tab_id) {
                        t.running_req = None;
                        t.plan = None;
                        t.last_finish = Some(std::time::Instant::now());
                        if let Some(started) = t.run_started.take() {
                            if started.elapsed() >= NOTIFY_AFTER && !focused {
                                notify_desktop(
                                    "dbTool — query finished",
                                    &format!(
                                        "{} ({:.0}s)",
                                        t.title,
                                        started.elapsed().as_secs_f32()
                                    ),
                                );
                            }
                        }
                        let info = if results.len() > 1 {
                            let total_rows: usize = results.iter().map(|r| r.rows.len()).sum();
                            let affected: u64 =
                                results.iter().filter_map(|r| r.rows_affected).sum();
                            let mut s = format!("{} result sets", results.len());
                            if total_rows > 0 {
                                s.push_str(&format!(" · {total_rows} row(s)"));
                            }
                            if affected > 0 {
                                s.push_str(&format!(" · {affected} affected"));
                            }
                            s
                        } else {
                            match results.first() {
                                Some(r) if r.rows_affected.is_some() => {
                                    format!("{} row(s) affected", r.rows_affected.unwrap())
                                }
                                Some(r) => format!("{} row(s)", r.rows.len()),
                                None => "done".to_owned(),
                            }
                        };
                        t.result_idx = 0;
                        t.results = results;
                        t.status = TabStatus::Info(info);
                        t.grid_edit = None;
                    }
                    // Single-table SELECT with its PK in the projection →
                    // the grid becomes editable.
                    let probe = match self.find_tab_mut(tab_id) {
                        Some(Tab::Query(t)) if t.results.len() == 1 => t
                            .last_run_sql
                            .clone()
                            .map(|sql| {
                                (
                                    t.conn_id,
                                    sql,
                                    t.results[0]
                                        .columns
                                        .iter()
                                        .map(|c| c.name.clone())
                                        .collect::<Vec<_>>(),
                                )
                            }),
                        _ => None,
                    };
                    let editable = probe
                        .and_then(|(conn, sql, cols)| self.detect_editable(conn, &sql, &cols));
                    if let Some(Tab::Query(t)) = self.find_tab_mut(tab_id) {
                        t.editable = editable;
                    }
                }
            }
            Event::Reconnected { conn } => {
                let name = self
                    .find_active_by_conn(conn)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| "connection".into());
                self.status = Some(format!("⟳ {name}: connection was dead — reconnected."));
            }
            Event::ConnectionLost { conn, error } => {
                let name = self
                    .find_active_by_conn(conn)
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| "connection".into());
                self.status = Some(format!(
                    "⚠ {name}: connection lost, reconnect failed ({error}) — retrying every minute."
                ));
            }
            Event::TableRows { req, conn: _, schema: _, table: _, result } => {
                let Some(op) = self.pending.remove(&req) else { return };
                match op {
                    Pending::TableRows(tab_id) => {
                        if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                            t.rows = Some(result);
                            t.status = TabStatus::Idle;
                        }
                    }
                    Pending::FkPeek => {
                        if let Some(p) = self.fk_peek.as_mut() {
                            p.result = Some(result);
                        }
                    }
                    Pending::FkHover => {
                        if let Some(h) = self.fk_hover.as_mut() {
                            if h.req == Some(req) {
                                h.result = Some(result);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::RowInserted { req } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::RowInsert(tab_id) = op {
                    if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                        t.insert_draft = None;
                    }
                    self.reload_table_tab(tab_id, "Inserted.");
                }
                if let Pending::QueryRowInsert(tab_id) = op {
                    if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
                        q.insert_draft = None;
                    }
                    self.rerun_last_query(tab_id);
                }
            }
            Event::ChangesApplied { req } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::QueryCellEdit(tab_id) = op {
                    self.rerun_last_query(tab_id);
                    return;
                }
                if let Pending::ApplyChanges(tab_id) = op {
                    if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                        t.clear_pending();
                        t.selected_rows.clear();
                        t.selection_anchor = None;
                    }
                    self.reload_table_tab(tab_id, "Changes committed.");
                }
            }
            Event::StructureDescribed { req, structure } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::DescribeStructure(tab_id) = op {
                    if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                        t.structure.loading = false;
                        t.structure.working =
                            Some(WorkingTable::from_structure(t.db_kind, &structure));
                        t.structure.selected = None;
                        t.status = TabStatus::Idle;
                    }
                }
            }
            Event::DdlApplied { req, applied, total, error } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::CompareApplyDdl(tab_id) = op {
                    self.finish_compare_apply(tab_id, applied, total, error);
                    return;
                }
                let Pending::ApplyDdl(tab_id) = op else { return };
                let mut refresh: Option<(ConnectionId, String, String)> = None;
                if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                    t.structure.applying = false;
                    match error {
                        None => {
                            let new_name = t
                                .structure
                                .working
                                .as_ref()
                                .map(|w| w.name.clone())
                                .unwrap_or_else(|| t.table.clone());
                            t.structure.is_new_table = false;
                            t.table = new_name;
                            t.status = TabStatus::Info(format!(
                                "Applied {applied} statement{}.",
                                if applied == 1 { "" } else { "s" }
                            ));
                            // Re-introspect so the view reflects reality.
                            t.structure.working = None;
                            t.structure.loading = true;
                            t.structure.selected = None;
                            refresh = Some((t.conn_id, t.schema.clone(), t.table.clone()));
                        }
                        Some(e) => {
                            if applied > 0 {
                                // MySQL partial apply: the DB moved under us —
                                // re-introspect, staged remainder is discarded.
                                t.status = TabStatus::Error(format!(
                                    "Applied {applied} of {total}, then failed: {e}"
                                ));
                                t.structure.working = None;
                                t.structure.loading = true;
                                t.structure.selected = None;
                                refresh = Some((t.conn_id, t.schema.clone(), t.table.clone()));
                            } else {
                                // Nothing changed on the DB — keep the staged
                                // work so the user can fix and retry.
                                t.status = TabStatus::Error(e);
                            }
                        }
                    }
                }
                if let Some((conn, schema, table)) = refresh {
                    if self.find_active_by_conn(conn).is_some() {
                        let (s, tb) = (schema.clone(), table.clone());
                        self.send(Pending::DescribeStructure(tab_id), move |req| {
                            Command::DescribeStructure { req, conn, schema: s, table: tb }
                        });
                        let (s, tb) = (schema.clone(), table.clone());
                        self.send(Pending::DescribeTable(tab_id), move |req| {
                            Command::DescribeTable { req, conn, schema: s, table: tb }
                        });
                        self.reload_table_tab(tab_id, "Structure changes applied.");
                        // Table list may have gained/lost/renamed entries.
                        self.send(Pending::ListTables, move |req| {
                            Command::ListTables { req, conn, schema }
                        });
                        // Keep autocomplete metadata in sync too.
                        self.send(Pending::FetchDbMeta, move |req| {
                            Command::FetchDbMeta { req, conn }
                        });
                    }
                }
            }
            Event::Ai { session, event } => {
                if self.ai_panel.current_session == Some(session) {
                    self.ai_panel.apply_session_event(event);
                }
            }
            Event::AiSessionEnded { session } => {
                self.ai_panel.session_ended(session);
            }
            Event::AuthStatusResult { req, status } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::AuthStatus = op {
                    self.auth.status = Some(status);
                    self.auth.busy = false;
                    self.auth.last_error = None;
                }
            }
            Event::AuthLoginResult { req, success, output } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::AuthLogin = op {
                    self.auth.busy = false;
                    self.auth.last_output = Some(output.clone());
                    if !success {
                        self.auth.last_error = Some(
                            "Login did not complete. If claude needed an interactive prompt, \
                             try running `claude auth login` in a terminal."
                                .into(),
                        );
                    } else {
                        self.auth.last_error = None;
                    }
                    // Refresh status regardless — best-effort truth.
                    self.send(Pending::AuthStatus, |req| Command::AuthStatus { req });
                }
            }
            Event::AuthLogoutResult { req, output } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::AuthLogout = op {
                    self.auth.busy = false;
                    self.auth.last_output = Some(output);
                    self.auth.last_error = None;
                    self.send(Pending::AuthStatus, |req| Command::AuthStatus { req });
                }
            }
            Event::DbMeta { req, conn, meta } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::FetchDbMeta = op {
                    if let Some(ac) = self.find_active_by_conn_mut(conn) {
                        ac.schema_cache = Some(std::sync::Arc::new(
                            crate::sql_complete::SchemaCache::new(meta),
                        ));
                    }
                    return;
                }
                if let Pending::DataSyncMeta(tab_id) = op {
                    let source_profile = self
                        .find_active_by_conn(conn)
                        .map(|a| a.profile_id);
                    let is_prod = source_profile.is_some_and(|pid| {
                        self.profiles.iter().any(|p| p.id == pid && p.production)
                    });
                    let saved = source_profile.and_then(crate::db::datasync::load_config);
                    if let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) {
                        t.meta_loading = false;
                        t.source_is_production = is_prod;
                        match saved {
                            // A remembered config restores selection & masks.
                            Some(cfg) if t.selected.is_empty() && t.masks.is_empty() => {
                                t.selected = cfg.selected.into_iter().collect();
                                t.masks = cfg.masks;
                                t.include_deletes = cfg.include_deletes;
                                t.row_limit =
                                    cfg.row_limit.map(|n| n.to_string()).unwrap_or_default();
                                for (key, m) in &t.masks {
                                    if let crate::db::datasync::MaskStrategy::Fixed(text) = m {
                                        t.fixed_drafts.insert(key.clone(), text.clone());
                                    }
                                }
                            }
                            _ => {
                                // First time: preselect every base table.
                                if t.selected.is_empty() {
                                    for tm in &meta.tables {
                                        if matches!(tm.kind, crate::db::TableKind::Table) {
                                            t.selected
                                                .insert(format!("{}.{}", tm.schema, tm.name));
                                        }
                                    }
                                }
                            }
                        }
                        t.meta = Some(meta);
                        t.error = None;
                    }
                }
            }
            Event::BackupDone { req, message } => {
                let Some(_) = self.pending.remove(&req) else { return };
                if let Some(d) = self.backup_dialog.as_mut() {
                    d.running = false;
                    d.result = Some(message);
                }
            }
            Event::DataProgress { req, message } => {
                if let Some(Pending::DataSyncRun(tab_id)) = self.pending.get(&req) {
                    let tab_id = *tab_id;
                    if let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) {
                        t.progress = message;
                    }
                }
            }
            Event::DataCompared { req, reports } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::DataSyncRun(tab_id) = op {
                    if let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) {
                        t.running = false;
                        t.progress.clear();
                        let in_sync = reports.iter().filter(|r| r.in_sync()).count();
                        let total = reports.len();
                        t.status = Some(format!(
                            "Compared {total} table(s) — {in_sync} in sync, {} differ.",
                            total - in_sync
                        ));
                        t.error = None;
                        t.reports = Some(reports);
                    }
                }
            }
            Event::DataPulled { req, tables, rows, errors } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::DataSyncRun(tab_id) = op {
                    if let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) {
                        t.running = false;
                        t.progress.clear();
                        t.reports = None;
                        if errors.is_empty() {
                            t.status =
                                Some(format!("Pulled {rows} row(s) across {tables} table(s)."));
                            t.error = None;
                        } else {
                            t.status = Some(format!(
                                "Pulled {rows} row(s) across {tables} table(s), with errors."
                            ));
                            t.error = Some(errors.join("\n"));
                        }
                    }
                }
            }
            Event::DdlDumped { req, dir, files, errors } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::DumpDdl = op {
                    if let Some(d) = self.dump_dialog.as_mut() {
                        d.running = false;
                        let mut msg = format!("Wrote {files} file(s) to {dir}.");
                        if !errors.is_empty() {
                            msg.push_str(&format!("\n{} object(s) failed:\n", errors.len()));
                            msg.push_str(&errors.join("\n"));
                        }
                        d.result = Some(msg);
                        d.error = None;
                    }
                    self.status = Some(format!("DDL dump: {files} file(s) written."));
                }
            }
            Event::DbmlDumped { req, tables, errors } => {
                let Some(op) = self.pending.remove(&req) else { return };
                let skipped = if errors.is_empty() {
                    String::new()
                } else {
                    format!(" — {} object(s) skipped", errors.len())
                };
                match op {
                    // First generation: create the database-owned document.
                    Pending::DumpDbml(conn) => {
                        let Some(ac) = self.find_active_by_conn(conn) else {
                            self.status =
                                Some("View as DBML: the connection was closed".to_owned());
                            return;
                        };
                        let name = ac.name.clone();
                        let source = (ac.profile_id, ac.database.clone());
                        let is_primary = ac.is_primary;
                        if tables.is_empty() {
                            self.status =
                                Some(format!("View as DBML: no tables found in {name}{skipped}"));
                            return;
                        }
                        let text = crate::dbml::generate_document(&tables, &name);
                        let path =
                            self.database_dbml_path(source.0, &source.1, is_primary, &name);
                        if let Err(e) = std::fs::create_dir_all(Self::dbml_dir()) {
                            self.status =
                                Some(format!("Could not create {}: {e}", Self::dbml_dir().display()));
                            return;
                        }
                        if let Err(e) = std::fs::write(&path, &text) {
                            self.status =
                                Some(format!("Could not write {}: {e}", path.display()));
                            return;
                        }
                        self.open_diagram_tab(path, Some(source));
                        self.status = Some(format!(
                            "Generated DBML for {} table(s) from {name}{skipped}",
                            tables.len()
                        ));
                    }
                    Pending::CompareSnapshot { tab, left, kind } => {
                        let err = (!errors.is_empty()).then(|| {
                            format!("{} object(s) skipped during introspection", errors.len())
                        });
                        let Some(Tab::Compare(t)) = self.find_tab_mut(tab) else { return };
                        if left {
                            t.left_snap = Some((kind, tables));
                            t.loading_left = false;
                        } else {
                            t.right_snap = Some((kind, tables));
                            t.loading_right = false;
                        }
                        if let Some(e) = err {
                            t.error = Some(e);
                        }
                        t.recompute();
                    }
                    // Refresh: merge into the open tab's text; the user
                    // persists it with Ctrl+S after reviewing.
                    Pending::RefreshDbml(tab_id) => {
                        let name = self
                            .find_tab_mut(tab_id)
                            .and_then(|t| match t {
                                Tab::Diagram(d) => d.db_source.clone(),
                                _ => None,
                            })
                            .and_then(|(pid, db)| {
                                self.active
                                    .iter()
                                    .find(|a| a.profile_id == pid && a.database == db)
                                    .map(|a| a.name.clone())
                            })
                            .unwrap_or_else(|| "database".to_owned());
                        let Some(Tab::Diagram(d)) = self.find_tab_mut(tab_id) else { return };
                        d.text = crate::dbml::merge_generated(&d.text, &tables, &name);
                        self.status = Some(format!(
                            "Refreshed {} table(s) from {name}{skipped} — Ctrl+S to keep",
                            tables.len()
                        ));
                    }
                    _ => {}
                }
            }
            Event::ImportProgress { req, rows } => {
                if let Some(Pending::ImportCsv) = self.pending.get(&req) {
                    if let Some(d) = self.import_dialog.as_mut() {
                        d.progress_rows = rows;
                    }
                }
            }
            Event::Imported { req, rows } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::ImportCsv = op {
                    if let Some(d) = self.import_dialog.as_mut() {
                        d.running = false;
                        d.progress_rows = rows;
                        d.result = Some(format!("Imported {rows} row(s)."));
                        d.error = None;
                    }
                    self.status = Some(format!("Import finished: {rows} row(s)."));
                }
            }
            Event::ExportProgress { req, rows } => {
                if let Some(Pending::ExportCsv) = self.pending.get(&req) {
                    if let Some(d) = self.export_dialog.as_mut() {
                        d.progress_rows = rows;
                    }
                }
            }
            Event::Exported { req, rows } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::ExportCsv = op {
                    self.export_dialog = None;
                    self.status = Some(format!("Export finished: {rows} row(s)."));
                }
            }
            Event::Error { req, error } => {
                let op = self.pending.remove(&req);
                match op {
                    Some(Pending::Query(tab_id)) => {
                        let focused = self.window_focused;
                        if let Some(Tab::Query(t)) = self.find_tab_mut(tab_id) {
                            t.running_req = None;
                            t.last_finish = Some(std::time::Instant::now());
                            if let Some(started) = t.run_started.take() {
                                if started.elapsed() >= NOTIFY_AFTER && !focused {
                                    notify_desktop(
                                        "dbTool — query failed",
                                        &format!("{}: {error}", t.title),
                                    );
                                }
                            }
                            // Jump the caret to the server-reported position
                            // while the buffer still matches what ran.
                            if t.last_run_sql.as_deref() == Some(t.sql.as_str()) {
                                if let Some(pos) = error_char_position(&error, &t.sql) {
                                    t.pending_selection = Some((pos, pos));
                                    t.scroll_to_char = Some(pos);
                                }
                            }
                            t.status = TabStatus::Error(error);
                        }
                    }
                    Some(Pending::TableRows(tab_id))
                    | Some(Pending::RowInsert(tab_id))
                    | Some(Pending::ApplyChanges(tab_id))
                    | Some(Pending::DescribeTable(tab_id)) => {
                        if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                            t.status = TabStatus::Error(error);
                        }
                    }
                    Some(Pending::DescribeStructure(tab_id)) => {
                        if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                            t.structure.loading = false;
                            t.status = TabStatus::Error(error);
                        }
                    }
                    Some(Pending::ApplyDdl(tab_id)) => {
                        if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                            t.structure.applying = false;
                            t.status = TabStatus::Error(error);
                        }
                    }
                    Some(Pending::AuthStatus)
                    | Some(Pending::AuthLogin)
                    | Some(Pending::AuthLogout) => {
                        self.auth.busy = false;
                        self.auth.last_error = Some(error);
                    }
                    Some(Pending::DumpDdl) => {
                        if let Some(d) = self.dump_dialog.as_mut() {
                            d.running = false;
                            d.error = Some(error);
                        }
                    }
                    Some(Pending::ImportCsv) => {
                        if let Some(d) = self.import_dialog.as_mut() {
                            d.running = false;
                            d.error = Some(error);
                        }
                    }
                    Some(Pending::ExportCsv) => {
                        if let Some(d) = self.export_dialog.as_mut() {
                            d.running = false;
                            d.error = Some(error);
                        }
                    }
                    Some(Pending::CompareSnapshot { tab, left, .. }) => {
                        if let Some(Tab::Compare(t)) = self.find_tab_mut(tab) {
                            if left {
                                t.loading_left = false;
                            } else {
                                t.loading_right = false;
                            }
                            t.error = Some(error);
                        }
                    }
                    Some(Pending::CompareApplyDdl(tab)) => {
                        if let Some(Tab::Compare(t)) = self.find_tab_mut(tab) {
                            t.applying = false;
                            t.error = Some(error);
                        }
                    }
                    Some(Pending::FkPeek) => {
                        if let Some(p) = self.fk_peek.as_mut() {
                            p.error = Some(error);
                        }
                    }
                    Some(Pending::FkHover) => {
                        if let Some(h) = self.fk_hover.as_mut() {
                            h.error = Some(error);
                        }
                    }
                    Some(Pending::SessionsList(tab_id)) | Some(Pending::SessionsKill(tab_id)) => {
                        if let Some(Tab::Sessions(t)) = self.find_tab_mut(tab_id) {
                            t.status = TabStatus::Error(error);
                        }
                    }
                    Some(Pending::QueryCellEdit(tab_id)) => {
                        if let Some(Tab::Query(t)) = self.find_tab_mut(tab_id) {
                            t.status = TabStatus::Error(format!("Update failed: {error}"));
                        }
                    }
                    Some(Pending::ExplainPlan(tab_id)) => {
                        if let Some(Tab::Query(t)) = self.find_tab_mut(tab_id) {
                            t.running_req = None;
                            t.status = TabStatus::Error(format!("Explain failed: {error}"));
                        }
                    }
                    Some(Pending::Backup) => {
                        if let Some(d) = self.backup_dialog.as_mut() {
                            d.running = false;
                            d.error = Some(error);
                        }
                    }
                    Some(Pending::DataSyncMeta(tab_id)) => {
                        if let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) {
                            t.meta_loading = false;
                            t.error = Some(error);
                        }
                    }
                    Some(Pending::DataSyncRun(tab_id)) => {
                        if let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) {
                            t.running = false;
                            t.progress.clear();
                            t.error = Some(error);
                        }
                    }
                    Some(Pending::TableComments) | Some(Pending::ApplyComments) => {
                        if let Some(d) = self.comments_dialog.as_mut() {
                            d.loading = false;
                            d.applying = false;
                            d.error = Some(error);
                        }
                    }
                    _ => {
                        self.status = Some(format!("Error: {error}"));
                    }
                }
            }
        }
    }

    /// Introspect both sides of a compare tab (each via the DBML walk).
    fn start_compare_snapshot(&mut self, tab_id: TabId) {
        let (left, right) = match self.find_tab_mut(tab_id) {
            Some(Tab::Compare(t)) => (t.left, t.right),
            _ => return,
        };
        for (conn_opt, is_left) in [(left, true), (right, false)] {
            let Some(conn) = conn_opt else { continue };
            let Some(kind) = self.find_active_by_conn(conn).map(|a| a.kind) else {
                if let Some(Tab::Compare(t)) = self.find_tab_mut(tab_id) {
                    t.error = Some("A selected connection is no longer active".to_owned());
                }
                continue;
            };
            if let Some(Tab::Compare(t)) = self.find_tab_mut(tab_id) {
                if is_left {
                    t.loading_left = true;
                    t.left_snap = None;
                } else {
                    t.loading_right = true;
                    t.right_snap = None;
                }
                t.diff = None;
            }
            self.send(
                Pending::CompareSnapshot { tab: tab_id, left: is_left, kind },
                move |req| Command::DumpDbml { req, conn },
            );
        }
    }

    fn finish_compare_apply(
        &mut self,
        tab_id: TabId,
        applied: usize,
        total: usize,
        error: Option<String>,
    ) {
        let mut rerun = false;
        if let Some(Tab::Compare(t)) = self.find_tab_mut(tab_id) {
            t.applying = false;
            match &error {
                None => {
                    t.error = None;
                    rerun = true;
                }
                Some(e) => {
                    // A partial apply (MySQL) moved the target — re-compare
                    // so the diff reflects reality either way.
                    rerun = applied > 0;
                    t.error = Some(if applied > 0 {
                        format!("Applied {applied} of {total}, then failed: {e}")
                    } else {
                        format!("Migration failed, nothing applied: {e}")
                    });
                }
            }
            if rerun {
                t.script.clear();
                t.script_dirty = true;
            }
        }
        if error.is_none() {
            self.status = Some(format!(
                "Migration applied: {applied} statement{} — re-comparing…",
                if applied == 1 { "" } else { "s" }
            ));
        }
        if rerun {
            self.start_compare_snapshot(tab_id);
        }
    }

    fn reload_table_tab(&mut self, tab_id: TabId, msg: &str) {
        let (conn_id, schema, table, offset, limit, filter) = match self.find_tab_mut(tab_id) {
            Some(Tab::TableEditor(t)) => {
                t.status = TabStatus::Info(msg.to_string());
                t.applied_filter = t.filter.clone();
                (
                    t.conn_id,
                    t.schema.clone(),
                    t.table.clone(),
                    t.offset,
                    t.limit,
                    table_rows_filter(t),
                )
            }
            _ => return,
        };
        self.send(Pending::TableRows(tab_id), move |req| Command::FetchTableRows {
            req,
            conn: conn_id,
            schema,
            table,
            limit,
            offset,
            filter,
        });
    }

    /// Run (or EXPLAIN) the active SQL of a query tab, recording history and
    /// the request id for cancellation.
    fn run_query_tab(&mut self, tab_id: TabId, explain: bool) {
        let (conn, sql, full_run) = {
            let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) else { return };
            let sql = q.selected_sql.clone().unwrap_or_else(|| q.sql.clone());
            if sql.trim().is_empty() {
                return;
            }
            (q.conn_id, sql, !explain && q.selected_sql.is_none())
        };
        if !self.active.iter().any(|a| a.conn_id == conn) {
            if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
                q.status = TabStatus::Error(
                    "Not connected — double-click the connection profile to connect first.".into(),
                );
            }
            return;
        }
        // Refuse a transaction left open past the end of the script: the
        // connection returns to the pool afterwards, so the open transaction
        // would either leak (idle-in-transaction) or be silently lost.
        if !explain && !crate::db::is_routine_ddl(&sql) && has_dangling_transaction(&sql) {
            if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
                q.status = TabStatus::Error(
                    "BEGIN without COMMIT/ROLLBACK: transactions cannot span separate Runs \
                     (pooled connections) — run BEGIN…COMMIT together in one script."
                        .into(),
                );
            }
            return;
        }
        if self.conn_read_only(conn) {
            if let Some(kw) = crate::db::write_statement_keyword(&sql) {
                if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
                    q.status = TabStatus::Error(format!(
                        "Read-only connection: refusing {kw}. Uncheck Read-only in the \
                         connection settings to allow writes."
                    ));
                }
                return;
            }
        }
        let conn_name = self
            .find_active_by_conn(conn)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let kind = self.find_active_by_conn(conn).map(|a| a.kind);
        let run_sql = if explain {
            match kind {
                Some(crate::db::DbKind::MsSql) => {
                    format!("SET SHOWPLAN_ALL ON;\n{sql};\nSET SHOWPLAN_ALL OFF;")
                }
                _ => format!("EXPLAIN {sql}"),
            }
        } else {
            crate::history::append(&conn_name, &sql);
            self.history = None; // reload lazily next time the popup opens
            sql
        };
        // Placeholder parameters (`:name`, and `?` outside Postgres) prompt
        // for values first; the substituted SQL then goes down the same path.
        let allow_qmark = kind.is_some_and(|k| k != crate::db::DbKind::Postgres);
        let param_names = crate::db::scan_parameters(&run_sql, allow_qmark);
        if !param_names.is_empty() {
            let remembered = match self.find_tab_mut(tab_id) {
                Some(Tab::Query(q)) => q.param_values.clone(),
                _ => Default::default(),
            };
            self.param_prompt = Some(ParamPrompt {
                tab_id,
                sql: run_sql,
                allow_qmark,
                full_run,
                values: param_names
                    .into_iter()
                    .map(|n| {
                        let v = remembered.get(&n).cloned().unwrap_or_default();
                        (n, v)
                    })
                    .collect(),
            });
            return;
        }
        self.dispatch_query(tab_id, conn, run_sql, full_run);
    }

    /// Run the dialect's structured EXPLAIN and show the plan as a tree.
    fn run_explain_visual(&mut self, tab_id: TabId) {
        let (conn, sql) = {
            let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) else { return };
            let sql = q.selected_sql.clone().unwrap_or_else(|| q.sql.clone());
            if sql.trim().is_empty() {
                return;
            }
            (q.conn_id, sql)
        };
        let Some(kind) = self.find_active_by_conn(conn).map(|a| a.kind) else {
            if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
                q.status = TabStatus::Error(
                    "Not connected — double-click the connection profile to connect first.".into(),
                );
            }
            return;
        };
        let run_sql = crate::db::plan::explain_sql(kind, &sql);
        let req = RequestId::new_v4();
        if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
            q.status = TabStatus::Running("explaining…".into());
            q.running_req = Some(req);
        }
        self.pending.insert(req, Pending::ExplainPlan(tab_id));
        self.runtime.send(Command::Query { req, conn, sql: run_sql, unlimited: false });
    }

    /// The tail of a Run: set status, record the request, send the command.
    fn dispatch_query(&mut self, tab_id: TabId, conn: ConnectionId, run_sql: String, full_run: bool) {
        let req = RequestId::new_v4();
        if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
            q.status = TabStatus::Running("running…".into());
            q.running_req = Some(req);
            q.run_started = Some(std::time::Instant::now());
            // Error positions only map back while the buffer is unchanged
            // and the whole buffer was what ran.
            q.last_run_sql = full_run.then(|| q.sql.clone());
            q.last_executed_sql = Some(run_sql.clone());
        }
        self.pending.insert(req, Pending::Query(tab_id));
        self.runtime.send(Command::Query { req, conn, sql: run_sql, unlimited: false });
    }

    /// Modal asking for `:name` / `?` values before a parameterized run.
    fn draw_param_prompt(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.param_prompt.as_mut() else { return };
        let mut open = true;
        let mut run = false;
        let mut cancel = false;
        egui::Window::new("Query parameters")
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        "NULL means null; numbers and true/false go in unquoted, \
                         everything else is quoted.",
                    )
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
                ui.add_space(4.0);
                egui::Grid::new("param_grid")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .min_col_width(80.0)
                    .show(ui, |ui| {
                        for (i, (name, value)) in prompt.values.iter_mut().enumerate() {
                            ui.label(egui::RichText::new(name.as_str()).monospace());
                            let resp = ui.add(
                                egui::TextEdit::singleline(value).desired_width(f32::INFINITY),
                            );
                            if i == 0 && ui.memory(|m| m.focused().is_none()) {
                                resp.request_focus();
                            }
                            ui.end_row();
                        }
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("▶  Run").color(egui::Color32::WHITE),
                            )
                            .fill(theme::ACCENT),
                        )
                        .clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Enter))
                    {
                        run = true;
                    }
                    if ui.button("Cancel").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Escape))
                    {
                        cancel = true;
                    }
                });
            });
        if run {
            self.submit_param_prompt();
        } else if cancel || !open {
            self.param_prompt = None;
        }
    }

    /// Snippets popup: insert saved SQL into the target tab, save the tab's
    /// current SQL under a name, delete entries.
    fn draw_snippets_popup(&mut self, ctx: &egui::Context) {
        let Some(target) = self.snippets_target else { return };
        if self.snippets.is_none() {
            self.snippets = Some(crate::snippets::load());
        }
        let mut open = true;
        let mut insert: Option<String> = None;
        let mut delete: Option<usize> = None;
        let mut save_current = false;
        egui::Window::new("Snippets")
            .collapsible(false)
            .resizable(true)
            .default_width(420.0)
            .open(&mut open)
            .show(ctx, |ui| {
                let snippets = self.snippets.as_ref().unwrap();
                if snippets.is_empty() {
                    ui.weak("No snippets yet — save the current SQL below.");
                } else {
                    egui::ScrollArea::vertical()
                        .id_source("snippets_scroll")
                        .max_height(280.0)
                        .show(ui, |ui| {
                            for (i, s) in snippets.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    if ui
                                        .button(&s.name)
                                        .on_hover_text(format!(
                                            "Insert at cursor:\n{}",
                                            s.sql.chars().take(400).collect::<String>()
                                        ))
                                        .clicked()
                                    {
                                        insert = Some(s.sql.clone());
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button("🗑")
                                                .on_hover_text("Delete snippet")
                                                .clicked()
                                            {
                                                delete = Some(i);
                                            }
                                        },
                                    );
                                });
                            }
                        });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.snippet_new_name)
                            .desired_width(180.0)
                            .hint_text("my snippet"),
                    );
                    let can_save = !self.snippet_new_name.trim().is_empty();
                    if ui
                        .add_enabled(can_save, egui::Button::new("💾 Save current SQL"))
                        .on_hover_text("Saves the tab's selection, or the whole buffer")
                        .clicked()
                    {
                        save_current = true;
                    }
                });
            });

        if let Some(sql) = insert {
            if let Some(Tab::Query(q)) = self.find_tab_mut(target) {
                let at_char = q.last_cursor_char.min(q.sql.chars().count());
                let byte = q
                    .sql
                    .char_indices()
                    .nth(at_char)
                    .map(|(b, _)| b)
                    .unwrap_or(q.sql.len());
                q.sql.insert_str(byte, &sql);
                q.pending_cursor = Some(at_char + sql.chars().count());
            }
            self.snippets_target = None;
            return;
        }
        if save_current {
            let sql = match self.find_tab_mut(target) {
                Some(Tab::Query(q)) => q.selected_sql.clone().unwrap_or_else(|| q.sql.clone()),
                _ => String::new(),
            };
            if !sql.trim().is_empty() {
                let name = std::mem::take(&mut self.snippet_new_name).trim().to_string();
                let list = self.snippets.as_mut().unwrap();
                // Same name replaces the existing snippet.
                list.retain(|s| s.name != name);
                list.push(crate::snippets::Snippet { name, sql });
                list.sort_by(|a, b| a.name.cmp(&b.name));
                if let Err(e) = crate::snippets::save(list) {
                    self.status = Some(format!("Could not save snippets: {e}"));
                }
            }
        }
        if let Some(i) = delete {
            let list = self.snippets.as_mut().unwrap();
            list.remove(i);
            if let Err(e) = crate::snippets::save(list) {
                self.status = Some(format!("Could not save snippets: {e}"));
            }
        }
        if !open {
            self.snippets_target = None;
        }
    }

    /// OK on the parameter prompt: remember values, substitute, dispatch.
    fn submit_param_prompt(&mut self) {
        let Some(p) = self.param_prompt.take() else { return };
        let conn = match self.find_tab_mut(p.tab_id) {
            Some(Tab::Query(q)) => {
                for (name, text) in &p.values {
                    q.param_values.insert(name.clone(), text.clone());
                }
                q.conn_id
            }
            _ => return,
        };
        let literals: std::collections::BTreeMap<String, String> = p
            .values
            .iter()
            .map(|(n, v)| (n.clone(), crate::db::parameter_literal(v)))
            .collect();
        let sql = crate::db::substitute_parameters(&p.sql, &literals, p.allow_qmark);
        self.dispatch_query(p.tab_id, conn, sql, p.full_run);
    }

    /// Re-run the tab's last query with the row cap lifted.
    fn fetch_all_rows(&mut self, tab_id: TabId) {
        let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) else { return };
        let Some(sql) = q.last_executed_sql.clone() else { return };
        let conn = q.conn_id;
        let req = RequestId::new_v4();
        q.status = TabStatus::Running("fetching all rows…".into());
        q.running_req = Some(req);
            q.run_started = Some(std::time::Instant::now());
        self.pending.insert(req, Pending::Query(tab_id));
        self.runtime.send(Command::Query { req, conn, sql, unlimited: true });
    }

    /// Save a query tab's SQL to its file, or ask for a path first.
    fn save_query_file(&mut self, tab_id: TabId) {
        let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) else { return };
        match &q.file_path {
            Some(path) => {
                let msg = match std::fs::write(path, &q.sql) {
                    Ok(()) => format!("Saved {}", path.display()),
                    Err(e) => format!("Could not save {}: {e}", path.display()),
                };
                self.status = Some(msg);
            }
            None => {
                self.sql_file_target = Some((tab_id, SqlFileMode::Save));
                self.sql_file_dialog.save_file();
            }
        }
    }

    fn open_query_tab(&mut self, conn_id: ConnectionId, initial_sql: String) -> Option<TabId> {
        let Some(ac) = self.find_active_by_conn(conn_id) else { return None };
        let tab = QueryTab::new(
            ac.profile_id,
            conn_id,
            ac.database.clone(),
            format!("Query — {}", ac.name),
            initial_sql,
        );
        let id = tab.id;
        self.tabs.push(Tab::Query(tab));
        self.active_tab = Some(id);
        Some(id)
    }

    /// "Users & roles…": a query tab pre-filled with the dialect's
    /// user/role listing, run immediately.
    fn open_users_tab(&mut self, conn_id: ConnectionId) {
        let Some(kind) = self.find_active_by_conn(conn_id).map(|a| a.kind) else { return };
        let sql = match kind {
            crate::db::DbKind::Postgres => {
                "SELECT rolname AS role, rolsuper AS is_superuser, rolcreatedb AS create_db, \
                        rolcreaterole AS create_role, rolcanlogin AS can_login, \
                        rolreplication AS replication, rolconnlimit AS conn_limit, \
                        rolvaliduntil AS valid_until \
                 FROM pg_roles ORDER BY rolname"
            }
            crate::db::DbKind::MySql => {
                "SELECT user, host, account_locked, password_expired, \
                        Super_priv AS super_priv, Create_priv AS create_priv, \
                        Grant_priv AS grant_priv \
                 FROM mysql.user ORDER BY user, host"
            }
            crate::db::DbKind::MsSql => {
                "SELECT name, type_desc, is_disabled, create_date, modify_date, \
                        default_database_name \
                 FROM sys.server_principals \
                 WHERE type IN ('S', 'U', 'G') AND name NOT LIKE '##%' \
                 ORDER BY name"
            }
            crate::db::DbKind::Sqlite => return,
        };
        if let Some(tab_id) = self.open_query_tab(conn_id, sql.to_owned()) {
            self.run_query_tab(tab_id, false);
        }
    }

    fn open_diagram_tab(
        &mut self,
        path: std::path::PathBuf,
        db_source: Option<(Uuid, String)>,
    ) {
        let path = path.canonicalize().unwrap_or(path);
        // Focus an existing tab if one already shows this file.
        if let Some(existing_id) = self
            .tabs
            .iter()
            .find(|t| matches!(t, Tab::Diagram(d) if d.path == path))
            .map(|t| t.id())
        {
            self.active_tab = Some(existing_id);
            return;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.status = Some(format!("Could not read {}: {e}", path.display()));
                return;
            }
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let id = Uuid::new_v4();
        let mut tab = crate::ui::diagram_tab::DiagramTab::open(id, path, text, mtime);
        tab.db_source = db_source;
        self.tabs.push(Tab::Diagram(tab));
        self.active_tab = Some(id);
    }

    fn dbml_dir() -> std::path::PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("dbTool")
            .join("dbml")
    }

    /// The database-owned DBML document. Identified by a short profile-id
    /// suffix (plus the database name for toggled-on databases) so it
    /// survives connection renames; the readable part is just for humans.
    /// The primary database keeps the pre-multi-DB suffix, so existing
    /// documents still open.
    fn database_dbml_path(
        &self,
        profile_id: Uuid,
        database: &str,
        is_primary: bool,
        name: &str,
    ) -> std::path::PathBuf {
        let safe = |s: &str| -> String {
            s.chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                .collect()
        };
        let short = &profile_id.simple().to_string()[..8];
        let suffix = if is_primary {
            format!("-{short}.dbml")
        } else {
            format!("-{short}-{}.dbml", safe(database))
        };
        let dir = Self::dbml_dir();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().ends_with(&suffix) {
                    return e.path();
                }
            }
        }
        dir.join(format!("{}{suffix}", safe(name)))
    }

    fn refresh_diagram_from_db(&mut self, tab_id: TabId) {
        let Some(Tab::Diagram(d)) = self.find_tab_mut(tab_id) else { return };
        let Some((profile_id, database)) = d.db_source.clone() else { return };
        let Some(ac) = self
            .active
            .iter()
            .find(|a| a.profile_id == profile_id && a.database == database)
        else {
            self.status = Some(
                "Refresh needs the owning database to be connected".to_owned(),
            );
            return;
        };
        let conn = ac.conn_id;
        self.status = Some(format!("Refreshing DBML from {}…", ac.name));
        self.send(Pending::RefreshDbml(tab_id), move |req| Command::DumpDbml {
            req,
            conn,
        });
    }

    fn save_diagram_file(&mut self, tab_id: TabId) {
        let Some(Tab::Diagram(d)) = self.find_tab_mut(tab_id) else { return };
        let disk_mtime = std::fs::metadata(&d.path).and_then(|m| m.modified()).ok();
        let changed_externally =
            matches!((disk_mtime, d.file_mtime), (Some(a), Some(b)) if a != b);
        if let Err(e) = std::fs::write(&d.path, &d.text) {
            let msg = format!("Could not save {}: {e}", d.path.display());
            self.status = Some(msg);
            return;
        }
        d.saved_text = d.text.clone();
        d.file_mtime = std::fs::metadata(&d.path).and_then(|m| m.modified()).ok();
        let name = d.path.display().to_string();
        self.status = Some(if changed_externally {
            format!("Saved {name} (overwrote external changes)")
        } else {
            format!("Saved {name}")
        });
    }

    fn open_table_tab(&mut self, conn_id: ConnectionId, schema: String, table: String) {
        self.open_table_tab_full(conn_id, schema, table, EditorView::Data, String::new());
    }

    fn open_table_tab_view(
        &mut self,
        conn_id: ConnectionId,
        schema: String,
        table: String,
        view: EditorView,
    ) {
        self.open_table_tab_full(conn_id, schema, table, view, String::new());
    }

    fn open_table_tab_full(
        &mut self,
        conn_id: ConnectionId,
        schema: String,
        table: String,
        view: EditorView,
        filter: String,
    ) {
        let Some(ac) = self.find_active_by_conn(conn_id) else { return };
        let db_kind = ac.kind;
        let profile_id = ac.profile_id;
        // Outgoing FKs from the connection's completion metadata (loaded at
        // connect), for "go to referenced row".
        let fks = ac
            .schema_cache
            .as_ref()
            .map(|c| c.fks_of(&schema, &table))
            .unwrap_or_default();
        // Focus an existing tab if one already shows this table.
        if let Some(existing_id) = self
            .tabs
            .iter()
            .find(|t| match t {
                Tab::TableEditor(te) => {
                    te.conn_id == conn_id && te.schema == schema && te.table == table
                }
                _ => false,
            })
            .map(|t| t.id())
        {
            self.active_tab = Some(existing_id);
            let mut reload = false;
            if let Some(Tab::TableEditor(te)) = self.find_tab_mut(existing_id) {
                if view == EditorView::Structure {
                    te.view = EditorView::Structure;
                }
                if !filter.is_empty() && te.filter != filter {
                    te.filter = filter;
                    te.offset = 0;
                    reload = true;
                }
            }
            if reload {
                self.reload_table_tab(existing_id, "Filtered.");
            }
            return;
        }
        let id = Uuid::new_v4();
        let tab = TableEditorTab {
            id,
            profile_id,
            conn_id,
            db_kind,
            schema: schema.clone(),
            table: table.clone(),
            view,
            structure: StructureState::default(),
            table_schema: None,
            rows: None,
            offset: 0,
            limit: 1000,
            filter: filter.clone(),
            applied_filter: filter.clone(),
            sort: None,
            col_filters: BTreeMap::new(),
            fks,
            edit: None,
            insert_draft: None,
            selected_rows: BTreeSet::new(),
            selection_anchor: None,
            status: TabStatus::Running("loading…".into()),
            pending_edits: BTreeMap::new(),
            pending_deletes: BTreeSet::new(),
        };
        self.tabs.push(Tab::TableEditor(tab));
        self.active_tab = Some(id);

        // Kick off the describe + fetch.
        let schema_d = schema.clone();
        let table_d = table.clone();
        self.send(Pending::DescribeTable(id), move |req| Command::DescribeTable {
            req,
            conn: conn_id,
            schema: schema_d,
            table: table_d,
        });
        let schema_r = schema.clone();
        let table_r = table.clone();
        let rows_filter = crate::db::RowsFilter {
            where_clause: filter,
            order_col: None,
            order_desc: false,
        };
        self.send(Pending::TableRows(id), move |req| Command::FetchTableRows {
            req,
            conn: conn_id,
            schema: schema_r,
            table: table_r,
            limit: 1000,
            offset: 0,
            filter: rows_filter,
        });
    }

    fn open_new_table_tab(&mut self, conn_id: ConnectionId, schema: String) {
        let Some(ac) = self.find_active_by_conn(conn_id) else { return };
        let id = Uuid::new_v4();
        let profile_id = ac.profile_id;
        let db_kind = ac.kind;
        let working = WorkingTable::new_table(db_kind, schema.clone());
        let table = working.name.clone();
        let structure = StructureState {
            working: Some(working),
            selected: Some(StructSel::Table),
            is_new_table: true,
            ..Default::default()
        };
        self.tabs.push(Tab::TableEditor(TableEditorTab {
            id,
            profile_id,
            conn_id,
            db_kind,
            schema,
            table,
            view: EditorView::Structure,
            structure,
            table_schema: None,
            rows: None,
            offset: 0,
            limit: 1000,
            filter: String::new(),
            applied_filter: String::new(),
            sort: None,
            col_filters: BTreeMap::new(),
            fks: Vec::new(),
            edit: None,
            insert_draft: None,
            selected_rows: BTreeSet::new(),
            selection_anchor: None,
            status: TabStatus::Idle,
            pending_edits: BTreeMap::new(),
            pending_deletes: BTreeSet::new(),
        }));
        self.active_tab = Some(id);
    }

    fn handle_auth_action(&mut self, action: AuthAction) {
        match action {
            AuthAction::None => {}
            AuthAction::Refresh => {
                self.auth.busy = true;
                self.auth.last_error = None;
                self.send(Pending::AuthStatus, |req| Command::AuthStatus { req });
            }
            AuthAction::Login { use_console } => {
                self.auth.busy = true;
                self.auth.last_error = None;
                self.auth.last_output = None;
                self.send(Pending::AuthLogin, move |req| Command::AuthLogin {
                    req,
                    use_console,
                });
            }
            AuthAction::Logout => {
                self.auth.busy = true;
                self.auth.last_error = None;
                self.send(Pending::AuthLogout, |req| Command::AuthLogout { req });
            }
        }
    }

    fn handle_ai_action(&mut self, action: AiPanelAction) {
        match action {
            AiPanelAction::None => {}
            AiPanelAction::Send {
                session,
                profile_id,
                prompt,
                system,
                model,
                allow_writes,
                resume_id,
            } => {
                let conn = profile_id.and_then(|pid| self.find_active(pid)).map(|a| a.conn_id);
                // Read-only profiles never auto-approve writes.
                let allow_writes = allow_writes
                    && !profile_id.map(|p| self.profile_read_only(p)).unwrap_or(false);
                self.runtime.send(Command::AiStart {
                    session,
                    prompt,
                    system,
                    model,
                    conn,
                    allow_writes,
                    resume_id,
                });
            }
            AiPanelAction::Approve { session, tool_use_id, approved } => {
                // A read-only profile can approve reads, never writes.
                let mut approved = approved;
                if approved
                    && self
                        .ai_panel
                        .selected_profile
                        .map(|p| self.profile_read_only(p))
                        .unwrap_or(false)
                {
                    let is_write = self
                        .ai_panel
                        .pending_approval()
                        .map(|(_, sql)| !crate::ai_tools::looks_read_only(&sql))
                        .unwrap_or(false);
                    if is_write {
                        approved = false;
                        self.status = Some(READ_ONLY_MSG.into());
                    }
                }
                self.runtime.send(Command::AiApprove { session, tool_use_id, approved });
            }
            AiPanelAction::Cancel { session } => {
                self.runtime.send(Command::AiCancel { session });
            }
            AiPanelAction::OpenSqlInEditor { profile_id, sql } => {
                let target = profile_id
                    .and_then(|pid| self.find_active(pid))
                    .or_else(|| self.active.first())
                    .map(|a| a.conn_id);
                match target {
                    Some(conn) => {
                        self.open_query_tab(conn, sql);
                    }
                    None => {
                        self.status = Some(
                            "Open a connection before sending AI SQL to the editor.".into(),
                        );
                    }
                }
            }
        }
    }

    fn disconnect_profile(&mut self, profile_id: Uuid) {
        let conn_ids: Vec<ConnectionId> = self
            .active
            .iter()
            .filter(|a| a.profile_id == profile_id)
            .map(|a| a.conn_id)
            .collect();
        for conn in conn_ids {
            self.runtime.send(Command::Disconnect { conn });
        }
        self.active.retain(|a| a.profile_id != profile_id);
        self.tabs.retain(|t| t.profile_id() != Some(profile_id));
        if let Some(id) = self.active_tab {
            if !self.tabs.iter().any(|t| t.id() == id) {
                self.active_tab = None;
            }
        }
    }

    /// Close one toggled-on database connection (never the primary), along
    /// with its tabs.
    fn disconnect_database(&mut self, profile_id: Uuid, database: &str) {
        let Some(pos) = self.active.iter().position(|a| {
            a.profile_id == profile_id && a.database == database && !a.is_primary
        }) else {
            return;
        };
        let conn_id = self.active[pos].conn_id;
        self.runtime.send(Command::Disconnect { conn: conn_id });
        self.active.remove(pos);
        // Query tabs survive a disconnect (detached consoles, re-attached on
        // reconnect); table editors need live schema and close.
        for t in &mut self.tabs {
            if let Tab::Query(q) = t {
                if q.conn_id == conn_id {
                    q.status = TabStatus::Info("disconnected".into());
                    q.running_req = None;
                }
            }
        }
        self.tabs.retain(|t| match t {
            Tab::TableEditor(te) => te.conn_id != conn_id,
            Tab::Sessions(s) => s.conn_id != conn_id,
            _ => true,
        });
        if let Some(id) = self.active_tab {
            if !self.tabs.iter().any(|t| t.id() == id) {
                self.active_tab = self.tabs.first().map(|t| t.id());
            }
        }
    }

    /// Fetch and show the referenced row(s) in a popup, without leaving the
    /// current tab.
    fn open_fk_peek(
        &mut self,
        conn: ConnectionId,
        schema: String,
        table: String,
        filter: String,
    ) {
        self.fk_peek = Some(FkPeek {
            conn,
            schema: schema.clone(),
            table: table.clone(),
            filter: filter.clone(),
            result: None,
            error: None,
        });
        let rows_filter = crate::db::RowsFilter {
            where_clause: filter,
            order_col: None,
            order_desc: false,
        };
        self.send(Pending::FkPeek, move |req| Command::FetchTableRows {
            req,
            conn,
            schema,
            table,
            limit: 50,
            offset: 0,
            filter: rows_filter,
        });
    }

    fn draw_fk_peek(&mut self, ctx: &egui::Context) {
        let Some(p) = &self.fk_peek else { return };
        let title = format!("{}.{} — {}", p.schema, p.table, p.filter);
        let mut open = true;
        let mut open_as_tab = false;
        let mut view_cell: Option<(String, String)> = None;
        egui::Window::new(title)
            .id(egui::Id::new("fk_peek"))
            .open(&mut open)
            .resizable(true)
            .default_size([620.0, 260.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .button("Open as table tab")
                        .on_hover_text("Full editor view with this filter applied")
                        .clicked()
                    {
                        open_as_tab = true;
                    }
                });
                ui.separator();
                if let Some(err) = &p.error {
                    ui.colored_label(ui.visuals().error_fg_color, err);
                    return;
                }
                match &p.result {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("loading…");
                        });
                    }
                    Some(rs) if rs.rows.is_empty() => {
                        ui.weak("No matching row (broken reference?).");
                    }
                    Some(rs) => {
                        let action = ui
                            .push_id("fk_peek_grid", |ui| crate::ui::results_grid::draw(ui, rs))
                            .inner;
                        if let crate::ui::results_grid::GridAction::ViewCell { title, content } =
                            action
                        {
                            view_cell = Some((title, content));
                        }
                    }
                }
            });
        if let Some(v) = view_cell {
            self.cell_viewer = Some(v);
        }
        if open_as_tab {
            if let Some(p) = self.fk_peek.take() {
                self.open_table_tab_full(p.conn, p.schema, p.table, EditorView::Data, p.filter);
            }
        } else if !open {
            self.fk_peek = None;
        }
    }

    /// Can this result be edited in place? Requires a resolvable single-table
    /// SELECT and every PK column present (exactly once) in the projection.
    fn detect_editable(
        &self,
        conn: ConnectionId,
        sql: &str,
        cols: &[String],
    ) -> Option<crate::ui::EditableMeta> {
        let ac = self.find_active_by_conn(conn)?;
        let cache = ac.schema_cache.as_ref()?;
        let (schema, table) = crate::db::single_select_target(sql)?;
        let meta = cache.resolve_table(schema.as_deref(), &table)?;
        if meta.primary_key.is_empty() {
            return None;
        }
        for pk in &meta.primary_key {
            if cols.iter().filter(|c| *c == pk).count() != 1 {
                return None;
            }
        }
        Some(crate::ui::EditableMeta {
            schema: meta.schema.clone(),
            table: meta.name.clone(),
            pk: meta.primary_key.clone(),
        })
    }

    /// UPDATE one cell of an editable query result, then re-run the query.
    fn commit_query_cell(&mut self, tab_id: TabId, row: usize, col: usize, text: String) {
        if self.refuse_readonly_tab(tab_id) {
            return;
        }
        let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) else { return };
        q.grid_edit = None;
        let Some(meta) = q.editable.clone() else { return };
        let Some(rs) = q.results.first() else { return };
        let (Some(r), Some(c)) = (rs.rows.get(row), rs.columns.get(col)) else { return };
        let col_name = c.name.clone();
        let Some(original) = r.get(col) else { return };
        let new_val = crate::db::Value::from_text_input(&text, original);
        let mut pk = crate::db::PkValues::new();
        for pkc in &meta.pk {
            let Some(idx) = rs.columns.iter().position(|c| &c.name == pkc) else { return };
            pk.insert(pkc.clone(), r[idx].clone());
        }
        let mut changes = crate::db::RowChanges::new();
        changes.insert(col_name, new_val);
        q.status = TabStatus::Running("updating…".into());
        let conn = q.conn_id;
        let (schema, table) = (meta.schema, meta.table);
        self.send(Pending::QueryCellEdit(tab_id), move |req| Command::ApplyChanges {
            req,
            conn,
            schema,
            table,
            updates: vec![(pk, changes)],
            deletes: Vec::new(),
        });
    }

    /// INSERT the drafted row of an editable query grid, then refresh.
    fn query_insert_row(&mut self, tab_id: TabId, values: crate::db::RowChanges) {
        if self.refuse_readonly_tab(tab_id) {
            return;
        }
        if values.is_empty() {
            if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
                q.status = TabStatus::Error("Nothing to insert — fill in at least one column.".into());
            }
            return;
        }
        let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) else { return };
        let Some(meta) = q.editable.clone() else { return };
        q.status = TabStatus::Running("inserting…".into());
        let conn = q.conn_id;
        let (schema, table) = (meta.schema, meta.table);
        self.send(Pending::QueryRowInsert(tab_id), move |req| Command::InsertRow {
            req,
            conn,
            schema,
            table,
            values,
        });
    }

    /// DELETE a row of an editable query grid (addressed by PK), then refresh.
    fn query_delete_row(&mut self, tab_id: TabId, row: usize) {
        if self.refuse_readonly_tab(tab_id) {
            return;
        }
        let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) else { return };
        let Some(meta) = q.editable.clone() else { return };
        let Some(rs) = q.results.first() else { return };
        let Some(r) = rs.rows.get(row) else { return };
        let mut pk = crate::db::PkValues::new();
        for pkc in &meta.pk {
            let Some(idx) = rs.columns.iter().position(|c| &c.name == pkc) else { return };
            pk.insert(pkc.clone(), r[idx].clone());
        }
        q.status = TabStatus::Running("deleting…".into());
        let conn = q.conn_id;
        let (schema, table) = (meta.schema, meta.table);
        self.send(Pending::QueryCellEdit(tab_id), move |req| Command::ApplyChanges {
            req,
            conn,
            schema,
            table,
            updates: Vec::new(),
            deletes: vec![pk],
        });
    }

    /// Fire re-runs for query tabs whose auto-refresh interval elapsed.
    /// Returns whether any tab has auto-refresh armed.
    fn tick_auto_refresh(&mut self) -> bool {
        let mut due: Vec<TabId> = Vec::new();
        let mut any_armed = false;
        for t in &self.tabs {
            if let Tab::Query(q) = t {
                let Some(secs) = q.auto_refresh_secs else { continue };
                any_armed = true;
                let elapsed_ok = q
                    .last_finish
                    .map_or(true, |f| f.elapsed().as_secs() >= secs as u64);
                if q.running_req.is_none() && q.last_run_sql.is_some() && elapsed_ok {
                    due.push(q.id);
                }
            }
        }
        for id in due {
            self.rerun_last_query(id);
        }
        any_armed
    }

    /// Re-run the last full-buffer query of a tab (fresh rows after an edit).
    fn rerun_last_query(&mut self, tab_id: TabId) {
        let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) else { return };
        let Some(sql) = q.last_run_sql.clone() else { return };
        let conn = q.conn_id;
        let req = RequestId::new_v4();
        q.status = TabStatus::Running("refreshing…".into());
        q.running_req = Some(req);
            q.run_started = Some(std::time::Instant::now());
        self.pending.insert(req, Pending::Query(tab_id));
        self.runtime.send(Command::Query { req, conn, sql, unlimited: false });
    }

    fn open_comments_dialog(&mut self, conn: ConnectionId, schema: String, table: String) {
        let Some(ac) = self.find_active_by_conn(conn) else { return };
        self.comments_dialog = Some(CommentsDialog {
            conn,
            kind: ac.kind,
            schema: schema.clone(),
            table: table.clone(),
            loading: true,
            applying: false,
            error: None,
            table_comment: String::new(),
            orig_table_comment: String::new(),
            columns: Vec::new(),
        });
        self.send(Pending::TableComments, move |req| Command::TableComments {
            req,
            conn,
            schema,
            table,
        });
    }

    /// Build and run the dialect script for changed comments.
    fn apply_comments(&mut self) {
        let Some(d) = &self.comments_dialog else { return };
        if self.conn_read_only(d.conn) {
            if let Some(d) = self.comments_dialog.as_mut() {
                d.error = Some(READ_ONLY_MSG.into());
            }
            return;
        }
        let kind = d.kind;
        let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let qi = |s: &str| crate::db::quote_ident(kind, s);
        let mut stmts: Vec<String> = Vec::new();
        let tbl = format!("{}.{}", qi(&d.schema), qi(&d.table));
        match kind {
            crate::db::DbKind::Postgres => {
                if d.table_comment != d.orig_table_comment {
                    let v = if d.table_comment.trim().is_empty() {
                        "NULL".to_owned()
                    } else {
                        lit(d.table_comment.trim())
                    };
                    stmts.push(format!("COMMENT ON TABLE {tbl} IS {v}"));
                }
                for (col, val, orig) in &d.columns {
                    if val != orig {
                        let v = if val.trim().is_empty() { "NULL".to_owned() } else { lit(val.trim()) };
                        stmts.push(format!("COMMENT ON COLUMN {tbl}.{} IS {v}", qi(col)));
                    }
                }
            }
            crate::db::DbKind::MySql => {
                if d.table_comment != d.orig_table_comment {
                    stmts.push(format!(
                        "ALTER TABLE {tbl} COMMENT = {}",
                        lit(d.table_comment.trim())
                    ));
                }
                // Column comments need a full column redefinition on MySQL —
                // the dialog disables them there.
            }
            // SQLite has no comments; the dialog is never offered there.
            crate::db::DbKind::Sqlite => {}
            crate::db::DbKind::MsSql => {
                let slit = lit(&d.schema);
                let tlit = lit(&d.table);
                let object = lit(&format!("{}.{}", d.schema, d.table));
                let mut prop = |val: &str, orig: &str, col: Option<&str>| {
                    if val == orig {
                        return;
                    }
                    let level2 = match col {
                        Some(c) => format!(", 'COLUMN', {}", lit(c)),
                        None => String::new(),
                    };
                    let minor = match col {
                        Some(c) => format!(
                            "(SELECT column_id FROM sys.columns WHERE object_id = OBJECT_ID({object}) AND name = {})",
                            lit(c)
                        ),
                        None => "0".to_owned(),
                    };
                    stmts.push(format!(
                        "IF EXISTS (SELECT 1 FROM sys.extended_properties WHERE major_id = OBJECT_ID({object}) AND minor_id = {minor} AND name = 'MS_Description') \
                         EXEC sp_dropextendedproperty 'MS_Description', 'SCHEMA', {slit}, 'TABLE', {tlit}{level2}"
                    ));
                    if !val.trim().is_empty() {
                        stmts.push(format!(
                            "EXEC sp_addextendedproperty 'MS_Description', N{}, 'SCHEMA', {slit}, 'TABLE', {tlit}{level2}",
                            lit(val.trim())
                        ));
                    }
                };
                prop(&d.table_comment, &d.orig_table_comment, None);
                for (col, val, orig) in &d.columns {
                    prop(val, orig, Some(col));
                }
            }
        }
        if stmts.is_empty() {
            self.comments_dialog = None;
            return;
        }
        let conn = d.conn;
        let sql = stmts.join(";\n");
        if let Some(d) = self.comments_dialog.as_mut() {
            d.applying = true;
            d.error = None;
        }
        self.send(Pending::ApplyComments, move |req| Command::Query { req, conn, sql, unlimited: false });
    }

    fn draw_comments_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = self.comments_dialog.as_mut() else { return };
        let mut open = true;
        let mut apply = false;
        egui::Window::new(format!("Comments — {}.{}", d.schema, d.table))
            .id(egui::Id::new("comments_dialog"))
            .open(&mut open)
            .resizable(true)
            .default_width(480.0)
            .show(ctx, |ui| {
                if d.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("loading comments…");
                    });
                    return;
                }
                ui.label(egui::RichText::new("Table comment").strong());
                ui.add(
                    egui::TextEdit::multiline(&mut d.table_comment)
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Column comments").strong());
                let mysql = d.kind == crate::db::DbKind::MySql;
                if mysql {
                    ui.weak(
                        "MySQL column comments require a full column redefinition — edit them \
                         via ALTER TABLE … MODIFY in a query tab.",
                    );
                }
                egui::ScrollArea::vertical()
                    .id_source("comments_scroll")
                    .max_height(260.0)
                    .show(ui, |ui| {
                        egui::Grid::new("comments_grid")
                            .num_columns(2)
                            .spacing([10.0, 4.0])
                            .show(ui, |ui| {
                                for (col, val, _) in d.columns.iter_mut() {
                                    ui.label(egui::RichText::new(col.as_str()).monospace());
                                    ui.add_enabled(
                                        !mysql,
                                        egui::TextEdit::singleline(val)
                                            .desired_width(f32::INFINITY),
                                    );
                                    ui.end_row();
                                }
                            });
                    });
                if let Some(e) = &d.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let dirty = d.table_comment != d.orig_table_comment
                        || d.columns.iter().any(|(_, v, o)| v != o);
                    if ui
                        .add_enabled(dirty && !d.applying, egui::Button::new("Apply"))
                        .clicked()
                    {
                        apply = true;
                    }
                    if d.applying {
                        ui.spinner();
                    }
                });
            });
        if apply {
            self.apply_comments();
        } else if !open {
            self.comments_dialog = None;
        }
    }

    fn open_sessions_tab(&mut self, conn: ConnectionId) {
        // One monitor per connection; focus an existing one.
        if let Some(id) = self
            .tabs
            .iter()
            .find(|t| matches!(t, Tab::Sessions(s) if s.conn_id == conn))
            .map(|t| t.id())
        {
            self.active_tab = Some(id);
            return;
        }
        let Some(ac) = self.find_active_by_conn(conn) else { return };
        let tab = crate::ui::SessionsTab {
            id: Uuid::new_v4(),
            profile_id: ac.profile_id,
            conn_id: conn,
            kind: ac.kind,
            conn_name: ac.name.clone(),
            rows: None,
            status: TabStatus::Idle,
            auto_refresh: true,
            last_refresh: 0.0,
        };
        let id = tab.id;
        self.tabs.push(Tab::Sessions(tab));
        self.active_tab = Some(id);
        self.refresh_sessions(id, 0.0);
    }

    /// Open (or focus) a data-sync tab with the given source connection.
    fn open_datasync_tab(&mut self, source: ConnectionId) {
        let id = Uuid::new_v4();
        let tab = crate::ui::datasync_tab::DataSyncTab::new(id, Some(source));
        self.tabs.push(Tab::DataSync(tab));
        self.active_tab = Some(id);
        self.datasync_load_tables(id);
    }

    fn datasync_load_tables(&mut self, tab_id: TabId) {
        let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) else { return };
        let Some(conn) = t.source else { return };
        t.meta_loading = true;
        t.error = None;
        self.send(Pending::DataSyncMeta(tab_id), move |req| Command::FetchDbMeta {
            req,
            conn,
        });
    }

    /// Persist the tab's selection/masks against its source profile.
    fn datasync_save_config(&mut self, tab_id: TabId) {
        let (source, cfg) = {
            let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) else { return };
            let Some(source) = t.source else { return };
            (
                source,
                crate::db::datasync::SavedSyncConfig {
                    selected: t.selected.iter().cloned().collect(),
                    masks: t.masks.clone(),
                    row_limit: t.row_limit_value(),
                    include_deletes: t.include_deletes,
                },
            )
        };
        if let Some(pid) = self.find_active_by_conn(source).map(|a| a.profile_id) {
            crate::db::datasync::save_config(pid, &cfg);
        }
    }

    fn datasync_compare(&mut self, tab_id: TabId) {
        self.datasync_save_config(tab_id);
        let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) else { return };
        let (Some(source), Some(target)) = (t.source, t.target) else { return };
        let tables = t.selections();
        if tables.is_empty() {
            return;
        }
        t.running = true;
        t.status = None;
        t.error = None;
        t.reports = None;
        self.send(Pending::DataSyncRun(tab_id), move |req| Command::DataCompare {
            req,
            source,
            target,
            tables,
            diff_cap: 1000,
        });
    }

    /// Generate the sync DML (masked) and open it in an editor on the
    /// TARGET connection — reviewed and run by hand, never automatically.
    fn datasync_open_script(&mut self, tab_id: TabId) {
        let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) else { return };
        let Some(target) = t.target else { return };
        let Some(reports) = t.reports.clone() else { return };
        let masks = t.masks.clone();
        let include_deletes = t.include_deletes;
        let Some(kind) = self.find_active_by_conn(target).map(|a| a.kind) else { return };
        let mut script = String::new();
        for r in &reports {
            let part = crate::db::datasync::sync_script(kind, r, &masks, include_deletes);
            if !part.is_empty() {
                script.push_str(&format!("-- {}.{}\n{part}\n", r.schema, r.table));
            }
        }
        if script.is_empty() {
            if let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) {
                t.status = Some("Nothing to sync — no row differences collected.".into());
            }
            return;
        }
        self.open_query_tab(target, script);
    }

    fn datasync_pull(&mut self, tab_id: TabId) {
        self.datasync_save_config(tab_id);
        let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) else { return };
        let (Some(source), Some(target)) = (t.source, t.target) else { return };
        let tables = t.selections();
        let masks = t.masks.clone();
        let row_limit = t.row_limit_value();
        if tables.is_empty() {
            return;
        }
        // Hard rails: never write into read-only or production-flagged targets.
        let target_profile = self.find_active_by_conn(target).map(|a| a.profile_id);
        let blocked = target_profile.is_some_and(|pid| {
            self.profiles
                .iter()
                .any(|p| p.id == pid && (p.read_only || p.production))
        });
        if blocked {
            if let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) {
                t.error = Some(
                    "Refusing to pull into a production or read-only profile — \
                     that direction is locked out by design."
                        .into(),
                );
            }
            return;
        }
        if let Some(Tab::DataSync(t)) = self.find_tab_mut(tab_id) {
            t.running = true;
            t.status = None;
            t.error = None;
            t.reports = None;
        }
        self.send(Pending::DataSyncRun(tab_id), move |req| Command::DataPull {
            req,
            source,
            target,
            tables,
            masks,
            row_limit,
        });
    }

    fn refresh_sessions(&mut self, tab_id: TabId, now: f64) {
        let Some(Tab::Sessions(t)) = self.find_tab_mut(tab_id) else { return };
        let conn = t.conn_id;
        let sql = sessions_query(t.kind).to_owned();
        t.status = TabStatus::Running("refreshing…".into());
        t.last_refresh = now;
        self.send(Pending::SessionsList(tab_id), move |req| Command::Query {
            req,
            conn,
            sql,
            unlimited: false,
        });
    }

    /// Cancel a session's query or kill the session from a monitor tab.
    fn sessions_kill(&mut self, tab_id: TabId, pid: i64, kill: bool) {
        let Some(Tab::Sessions(t)) = self.find_tab_mut(tab_id) else { return };
        let conn = t.conn_id;
        let kind = t.kind;
        if self.conn_read_only(conn) {
            if let Some(Tab::Sessions(t)) = self.find_tab_mut(tab_id) {
                t.status = TabStatus::Error(READ_ONLY_MSG.into());
            }
            return;
        }
        let sql = match (kind, kill) {
            (crate::db::DbKind::Postgres, false) => format!("SELECT pg_cancel_backend({pid})"),
            (crate::db::DbKind::Postgres, true) => format!("SELECT pg_terminate_backend({pid})"),
            (crate::db::DbKind::MySql, false) => format!("KILL QUERY {pid}"),
            (crate::db::DbKind::MySql, true) => format!("KILL {pid}"),
            (crate::db::DbKind::MsSql, _) => format!("KILL {pid}"),
            // No server, no sessions to kill.
            (crate::db::DbKind::Sqlite, _) => return,
        };
        if let Some(Tab::Sessions(t)) = self.find_tab_mut(tab_id) {
            t.status = TabStatus::Running(if kill {
                format!("killing {pid}…")
            } else {
                format!("cancelling {pid}…")
            });
        }
        self.send(Pending::SessionsKill(tab_id), move |req| Command::Query {
            req,
            conn,
            sql,
            unlimited: false,
        });
    }

    /// Everything the palette can jump to that matches the query, best first.
    fn palette_hits(&self) -> Vec<PaletteHit> {
        let q = self.palette_query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let score = |name: &str| -> Option<usize> {
            let n = name.to_lowercase();
            if n == q {
                Some(0)
            } else if n.starts_with(&q) {
                Some(1)
            } else if n.contains(&q) {
                Some(2)
            } else {
                None
            }
        };
        let mut scored: Vec<(usize, usize, PaletteHit)> = Vec::new();
        for ac in &self.active {
            if let Some(cache) = &ac.schema_cache {
                for t in cache.all_tables() {
                    let kind = match t.kind {
                        crate::db::TableKind::View => PaletteKind::View,
                        crate::db::TableKind::Table => PaletteKind::Table,
                    };
                    if let Some(s) = score(&t.name) {
                        scored.push((s, t.name.len(), PaletteHit {
                            conn: ac.conn_id,
                            conn_name: ac.name.clone(),
                            schema: t.schema.clone(),
                            name: t.name.clone(),
                            column: None,
                            kind: kind.clone(),
                        }));
                    }
                    for c in &t.columns {
                        if let Some(s) = score(&c.name) {
                            scored.push((s + 3, c.name.len(), PaletteHit {
                                conn: ac.conn_id,
                                conn_name: ac.name.clone(),
                                schema: t.schema.clone(),
                                name: t.name.clone(),
                                column: Some(c.name.clone()),
                                kind: kind.clone(),
                            }));
                        }
                    }
                }
            }
            // Routines index once their schema has been expanded.
            for node in &ac.schemas {
                if let Some(objs) = &node.objects {
                    for r in &objs.routines {
                        if let Some(s) = score(&r.name) {
                            scored.push((s + 1, r.name.len(), PaletteHit {
                                conn: ac.conn_id,
                                conn_name: ac.name.clone(),
                                schema: node.name.clone(),
                                name: r.name.clone(),
                                column: None,
                                kind: PaletteKind::Routine(r.kind.clone()),
                            }));
                        }
                    }
                }
            }
        }
        scored.sort_by(|a, b| (a.0, a.1, &a.2.name).cmp(&(b.0, b.1, &b.2.name)));
        scored.truncate(50);
        scored.into_iter().map(|x| x.2).collect()
    }

    fn draw_palette(&mut self, ctx: &egui::Context) {
        let hits = self.palette_hits();
        if self.palette_index >= hits.len() {
            self.palette_index = 0;
        }
        let mut activate: Option<PaletteHit> = None;
        let mut close = false;
        // Consume arrows BEFORE the TextEdit sees them so they navigate the
        // list instead of moving the text cursor.
        let down = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown));
        let up = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp));
        if !hits.is_empty() {
            if down {
                self.palette_index = (self.palette_index + 1) % hits.len();
            }
            if up {
                self.palette_index = (self.palette_index + hits.len() - 1) % hits.len();
            }
        }
        egui::Window::new("Search everywhere")
            .id(egui::Id::new("palette"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
            .default_width(620.0)
            .show(ctx, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.palette_query)
                        .hint_text("table, column or function… (Enter opens, Esc closes)")
                        .desired_width(f32::INFINITY),
                );
                if self.palette_focus {
                    resp.request_focus();
                    self.palette_focus = false;
                }
                if resp.changed() {
                    self.palette_index = 0;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    activate = hits.get(self.palette_index).cloned();
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_source("palette_scroll")
                    .max_height(340.0)
                    .show(ui, |ui| {
                        for (i, h) in hits.iter().enumerate() {
                            let icon = match &h.kind {
                                PaletteKind::Table => "▦",
                                PaletteKind::View => "◫",
                                PaletteKind::Routine(_) => "ƒ",
                            };
                            let mut label = format!("{icon} {}.{}", h.schema, h.name);
                            if let Some(c) = &h.column {
                                label.push_str(&format!(" · {c}"));
                            }
                            let row = ui.selectable_label(
                                i == self.palette_index,
                                format!("{label}    —  {}", h.conn_name),
                            );
                            if (down || up) && i == self.palette_index {
                                row.scroll_to_me(None);
                            }
                            if row.clicked() {
                                activate = Some(h.clone());
                            }
                        }
                        if hits.is_empty() && !self.palette_query.trim().is_empty() {
                            ui.weak(
                                "No matches. (Functions index once their schema is expanded.)",
                            );
                        }
                    });
            });
        if let Some(h) = activate {
            self.palette_open = false;
            match h.kind {
                PaletteKind::Routine(kind) => {
                    let (conn, schema, name) = (h.conn, h.schema, h.name);
                    self.status = Some(format!("Fetching source of {schema}.{name}…"));
                    self.send(Pending::RoutineDdl, move |req| Command::RoutineDdl {
                        req,
                        conn,
                        schema,
                        name,
                        kind,
                    });
                }
                _ => {
                    self.open_table_tab_full(
                        h.conn,
                        h.schema,
                        h.name,
                        EditorView::Data,
                        String::new(),
                    );
                }
            }
        } else if close {
            self.palette_open = false;
        }
    }

    /// Track pointer dwell over an FK cell. The same target keeps the popup
    /// alive; a different cell restarts the delay.
    fn note_fk_hover(
        &mut self,
        ctx: &egui::Context,
        conn: ConnectionId,
        kind: crate::db::DbKind,
        cell: crate::ui::FkHoverCell,
    ) {
        let filter = format!(
            "{} = {}",
            crate::db::quote_ident(kind, &cell.column),
            crate::ui::results_grid::sql_literal(&cell.value)
        );
        match self.fk_hover.as_mut() {
            Some(h)
                if h.conn == conn
                    && h.schema == cell.schema
                    && h.table == cell.table
                    && h.filter == filter =>
            {
                h.cell_rect = cell.rect;
            }
            _ => {
                self.fk_hover = Some(FkHover {
                    conn,
                    schema: cell.schema,
                    table: cell.table,
                    filter,
                    cell_rect: cell.rect,
                    since: ctx.input(|i| i.time),
                    req: None,
                    result: None,
                    error: None,
                });
            }
        }
    }

    /// Hover preview of the referenced row; clicking a value opens the table
    /// tab filtered to it. Dismisses once the pointer leaves cell and popup.
    fn draw_fk_hover(&mut self, ctx: &egui::Context) {
        const DELAY: f64 = 0.55;
        let (now, pointer) = ctx.input(|i| (i.time, i.pointer.latest_pos()));
        let Some(h) = &self.fk_hover else { return };
        let over_cell = pointer.is_some_and(|p| h.cell_rect.expand(2.0).contains(p));
        let elapsed = now - h.since;
        if elapsed < DELAY {
            if over_cell {
                // Wake up exactly when the dwell delay elapses.
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(DELAY - elapsed));
            } else {
                self.fk_hover = None;
            }
            return;
        }

        // Fire the row fetch once, when the delay elapses.
        if h.req.is_none() && h.result.is_none() && h.error.is_none() {
            let conn = h.conn;
            let schema = h.schema.clone();
            let table = h.table.clone();
            let filter = crate::db::RowsFilter {
                where_clause: h.filter.clone(),
                order_col: None,
                order_desc: false,
            };
            let mut sent = None;
            self.send(Pending::FkHover, |req| {
                sent = Some(req);
                Command::FetchTableRows { req, conn, schema, table, limit: 4, offset: 0, filter }
            });
            if let Some(h) = self.fk_hover.as_mut() {
                h.req = sent;
            }
        }

        let Some(h) = &self.fk_hover else { return };
        let anchor = egui::pos2(h.cell_rect.left(), h.cell_rect.bottom() + 2.0);
        let mut navigate = false;
        let area = egui::Area::new(egui::Id::new("fk_hover_popup"))
            .fixed_pos(anchor)
            .order(egui::Order::Foreground)
            .constrain(true)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(680.0);
                    match (&h.error, &h.result) {
                        (Some(e), _) => {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                        (None, None) => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.weak("loading…");
                            });
                        }
                        (None, Some(rs)) if rs.rows.is_empty() => {
                            ui.weak("No matching row (broken reference?).");
                        }
                        (None, Some(rs)) => {
                            ui.label(
                                egui::RichText::new(format!("{}.{}", h.schema, h.table))
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                            let null_fg = crate::ui::theme::null_color(ui);
                            egui::ScrollArea::horizontal()
                                .id_source("fk_hover_scroll")
                                .show(ui, |ui| {
                                    egui::Grid::new("fk_hover_grid")
                                        .spacing([14.0, 2.0])
                                        .show(ui, |ui| {
                                            for c in &rs.columns {
                                                ui.label(
                                                    egui::RichText::new(&c.name)
                                                        .small()
                                                        .strong()
                                                        .color(ui.visuals().weak_text_color()),
                                                );
                                            }
                                            ui.end_row();
                                            for (ri, row) in rs.rows.iter().take(3).enumerate() {
                                                // Reserve a background shape so the
                                                // hover highlight paints under the text.
                                                let bg = ui.painter().add(egui::Shape::Noop);
                                                let mut row_rect: Option<egui::Rect> = None;
                                                for v in row {
                                                    let s = v.display();
                                                    let preview: String =
                                                        s.chars().take(60).collect();
                                                    let rich = if matches!(v, crate::db::Value::Null)
                                                    {
                                                        egui::RichText::new(preview)
                                                            .monospace()
                                                            .italics()
                                                            .color(null_fg)
                                                    } else {
                                                        egui::RichText::new(preview).monospace()
                                                    };
                                                    let resp = ui.add(
                                                        egui::Label::new(rich).selectable(false),
                                                    );
                                                    row_rect = Some(row_rect.map_or(
                                                        resp.rect,
                                                        |r: egui::Rect| r.union(resp.rect),
                                                    ));
                                                }
                                                ui.end_row();
                                                // One click/hover target spanning the
                                                // whole row, gaps included.
                                                if let Some(rect) = row_rect {
                                                    let rect =
                                                        rect.expand2(egui::vec2(5.0, 1.0));
                                                    let resp = ui.interact(
                                                        rect,
                                                        ui.id().with(("fk_hover_row", ri)),
                                                        egui::Sense::click(),
                                                    );
                                                    if resp.hovered() {
                                                        ui.painter().set(
                                                            bg,
                                                            egui::Shape::rect_filled(
                                                                rect,
                                                                3.0,
                                                                ui.visuals()
                                                                    .widgets
                                                                    .hovered
                                                                    .weak_bg_fill,
                                                            ),
                                                        );
                                                    }
                                                    if resp
                                                        .on_hover_cursor(
                                                            egui::CursorIcon::PointingHand,
                                                        )
                                                        .clicked()
                                                    {
                                                        navigate = true;
                                                    }
                                                }
                                            }
                                        });
                                });
                            if rs.rows.len() > 3 {
                                ui.weak(format!("… {} more row(s)", rs.rows.len() - 3));
                            }
                            ui.label(
                                egui::RichText::new("click row to open as table tab")
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                        }
                    }
                });
            });

        let over_popup = pointer.is_some_and(|p| area.response.rect.expand(6.0).contains(p));
        if navigate {
            if let Some(h) = self.fk_hover.take() {
                self.open_table_tab_full(h.conn, h.schema, h.table, EditorView::Data, h.filter);
            }
        } else if !over_cell && !over_popup {
            self.fk_hover = None;
        }
    }

    /// Full-value viewer for one cell (JSON pretty-printing on demand).
    fn draw_cell_viewer(&mut self, ctx: &egui::Context) {
        let Some((title, content)) = self.cell_viewer.clone() else { return };
        let mut open = true;
        let mut new_content: Option<String> = None;
        egui::Window::new(format!("Cell — {title}"))
            .open(&mut open)
            .resizable(true)
            .default_size([520.0, 380.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("📋 Copy").clicked() {
                        ui.output_mut(|o| o.copied_text = content.clone());
                    }
                    let as_json = serde_json::from_str::<serde_json::Value>(&content).ok();
                    if let Some(v) = as_json {
                        if ui.button("{ } Pretty JSON").clicked() {
                            if let Ok(pretty) = serde_json::to_string_pretty(&v) {
                                new_content = Some(pretty);
                            }
                        }
                    }
                    ui.weak(format!("{} chars", content.chars().count()));
                });
                ui.separator();
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(&content).monospace()).extend(),
                    );
                });
            });
        if let Some(c) = new_content {
            self.cell_viewer = Some((title, c));
        }
        if !open {
            self.cell_viewer = None;
        }
    }

    /// Query-history popup: searchable, newest first; Insert puts the SQL
    /// into the active (or first) query tab.
    fn draw_history_window(
        &mut self,
        ctx: &egui::Context,
        _pending: &mut Vec<Box<dyn FnOnce(&mut Self)>>,
    ) {
        if !self.history_open {
            return;
        }
        let mut open = true;
        // (sql, run_now): Run executes immediately; double-click just opens
        // the SQL in the editor.
        let mut insert_sql: Option<(String, bool)> = None;
        let entries = self.history.clone().unwrap_or_default();
        let filter = self.history_filter.to_lowercase();
        egui::Window::new("Query history")
            .open(&mut open)
            .resizable(true)
            .default_size([560.0, 420.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.history_filter)
                            .hint_text("search sql or connection")
                            .desired_width(240.0),
                    );
                    ui.weak(format!("{} entr{}", entries.len(), if entries.len() == 1 { "y" } else { "ies" }));
                });
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for e in &entries {
                        if !filter.is_empty()
                            && !e.sql.to_lowercase().contains(&filter)
                            && !e.conn.to_lowercase().contains(&filter)
                        {
                            continue;
                        }
                        let preview: String = e.sql.replace('\n', " ").chars().take(110).collect();
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("{}  {}", e.ts, e.conn))
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button("▶ Run")
                                        .on_hover_text("Run again in the current query tab")
                                        .clicked()
                                    {
                                        insert_sql = Some((e.sql.clone(), true));
                                    }
                                    if ui.small_button("📋 Copy").clicked() {
                                        ui.output_mut(|o| o.copied_text = e.sql.clone());
                                    }
                                },
                            );
                        });
                        let resp = ui.add(
                            egui::Label::new(egui::RichText::new(&preview).monospace())
                                .truncate()
                                .sense(egui::Sense::click()),
                        );
                        if resp
                            .on_hover_text(format!("{}\n\n(double-click to open in editor)", e.sql))
                            .double_clicked()
                        {
                            insert_sql = Some((e.sql.clone(), false));
                        }
                        ui.separator();
                    }
                    if entries.is_empty() {
                        ui.weak("No history yet — run something first.");
                    }
                });
            });
        if !open {
            self.history_open = false;
        }
        if let Some((sql, run_now)) = insert_sql {
            // Prefer the active tab when it's a query tab, else the first one.
            let target = match self.active_tab.and_then(|id| self.find_tab_mut(id)) {
                Some(Tab::Query(q)) => Some(q.id),
                _ => self
                    .tabs
                    .iter()
                    .find_map(|t| match t {
                        Tab::Query(q) => Some(q.id),
                        _ => None,
                    }),
            };
            match target {
                Some(id) => {
                    if let Some(Tab::Query(q)) = self.find_tab_mut(id) {
                        q.sql = sql;
                        q.selected_sql = None;
                        self.active_tab = Some(id);
                        self.history_open = false;
                    }
                    if run_now {
                        self.run_query_tab(id, false);
                    }
                }
                None => self.status = Some("Open a query tab first.".into()),
            }
        }
    }

    fn handle_db_picker_action(&mut self, action: crate::connections::manager_ui::DbPickerAction) {
        use crate::connections::manager_ui::DbPickerAction as P;
        match action {
            P::None => {}
            P::Close => self.db_picker = None,
            P::Enable { profile_id, database } => {
                self.connect_database(profile_id, database.clone());
                if let Some(p) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
                    if !p.enabled_databases.contains(&database) {
                        p.enabled_databases.push(database);
                        p.enabled_databases.sort();
                    }
                }
                if let Err(e) = connections::save_profiles(&self.profiles) {
                    self.status = Some(format!("Failed to save profiles: {e}"));
                }
            }
            P::Disable { profile_id, database } => {
                self.disconnect_database(profile_id, &database);
                if let Some(p) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
                    p.enabled_databases.retain(|d| d != &database);
                }
                if let Err(e) = connections::save_profiles(&self.profiles) {
                    self.status = Some(format!("Failed to save profiles: {e}"));
                }
            }
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let mut saved = crate::session::SavedSession::default();
        for t in &self.tabs {
            if let Tab::Query(q) = t {
                if q.sql.trim().is_empty() {
                    continue;
                }
                if self.active_tab == Some(q.id) {
                    saved.active = Some(saved.tabs.len());
                }
                saved.tabs.push(crate::session::SavedQueryTab {
                    profile_id: q.profile_id,
                    database: q.database.clone(),
                    title: q.title.clone(),
                    sql: q.sql.clone(),
                    file_path: q.file_path.clone(),
                });
            }
        }
        crate::session::save(&saved);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.last_input_time = ctx.input(|i| i.time);
        self.window_focused = ctx.input(|i| i.focused);
        // 1. Drain events.
        let events = self.runtime.drain_events();
        for e in events {
            self.apply_event(e);
        }
        if self.tick_auto_refresh() {
            // Keep frames coming while an auto-refresh interval is armed.
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
        self.draw_param_prompt(ctx);
        self.draw_snippets_popup(ctx);

        // 2. Left sidebar: connection list + schema trees.
        let mut pending_actions: Vec<Box<dyn FnOnce(&mut Self)>> = Vec::new();
        // FK cell the pointer dwells on this frame (drives the hover preview).
        let mut fk_hover_cell: Option<(ConnectionId, crate::db::DbKind, crate::ui::FkHoverCell)> =
            None;

        // Top menu bar (title + AI agent toggle + account).
        egui::TopBottomPanel::top("topbar")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(10.0, 6.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("dbTool").strong().size(15.0).color(theme::ACCENT));
                    ui.separator();

                    if ui
                        .selectable_label(self.ai_panel.open, "💬")
                        .on_hover_text("AI assistant")
                        .clicked()
                    {
                        self.ai_panel.open = !self.ai_panel.open;
                    }

                    if ui
                        .button("◈")
                        .on_hover_text("Open a .dbml file as an editable diagram")
                        .clicked()
                    {
                        self.dbml_file_dialog.select_file();
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .selectable_label(self.settings_open, "⚙")
                            .on_hover_text("Settings")
                            .clicked()
                        {
                            self.settings_open = !self.settings_open;
                        }
                    });
                });
            });

        // AI panel (right side, when open).
        let ai_action = crate::ui::ai_panel::draw(ctx, &mut self.ai_panel, &self.active);
        pending_actions.push(Box::new(move |app: &mut Self| app.handle_ai_action(ai_action)));

        // Settings window (theme, editor toggles, Claude account).
        let (settings_changed, auth_action) = crate::ui::settings_dialog::draw(
            ctx,
            &mut self.settings_open,
            &mut self.settings,
            &mut self.auth,
        );
        if settings_changed {
            theme::install(ctx, self.settings.theme);
            crate::db::set_result_row_cap(self.settings.max_result_rows);
            if let Err(e) = crate::settings::save(&self.settings) {
                self.status = Some(format!("Could not save settings: {e}"));
            }
        }
        pending_actions.push(Box::new(move |app: &mut Self| app.handle_auth_action(auth_action)));

        // Import/Export dialogs.
        if let Some(d) = self.dump_dialog.as_mut() {
            let action = crate::ui::import_export::draw_dump(ctx, d);
            pending_actions.push(Box::new(move |app: &mut Self| app.handle_dump_action(action)));
        }
        if let Some(d) = self.import_dialog.as_mut() {
            let action = crate::ui::import_export::draw_import(ctx, d);
            pending_actions.push(Box::new(move |app: &mut Self| app.handle_import_action(action)));
        }
        if let Some(d) = self.export_dialog.as_mut() {
            let action = crate::ui::import_export::draw_export(ctx, d);
            pending_actions.push(Box::new(move |app: &mut Self| app.handle_export_action(action)));
        }
        if let Some(d) = self.backup_dialog.as_mut() {
            let action = crate::ui::import_export::draw_backup(ctx, d);
            pending_actions.push(Box::new(move |app: &mut Self| app.handle_backup_action(action)));
        }

        // DBML file picker.
        self.dbml_file_dialog.update(ctx);
        if let Some(picked) = self.dbml_file_dialog.take_selected() {
            self.open_diagram_tab(picked, None);
        }

        // Query-tab .sql open/save picker.
        self.sql_file_dialog.update(ctx);
        if let Some(picked) = self.sql_file_dialog.take_selected() {
            if let Some((tab_id, mode)) = self.sql_file_target.take() {
                match mode {
                    SqlFileMode::Open => match std::fs::read_to_string(&picked) {
                        Ok(text) => {
                            if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
                                q.sql = text;
                                q.file_path = Some(picked.clone());
                                let conn_part = q
                                    .title
                                    .rsplit(" — ")
                                    .next()
                                    .unwrap_or("")
                                    .to_owned();
                                q.title = format!(
                                    "{} — {}",
                                    picked
                                        .file_name()
                                        .map(|n| n.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| "query".into()),
                                    conn_part
                                );
                            }
                            self.status = Some(format!("Opened {}", picked.display()));
                        }
                        Err(e) => {
                            self.status = Some(format!("Could not read {}: {e}", picked.display()));
                        }
                    },
                    SqlFileMode::Save => {
                        if let Some(Tab::Query(q)) = self.find_tab_mut(tab_id) {
                            q.file_path = Some(picked.clone());
                        }
                        self.save_query_file(tab_id);
                    }
                }
            }
        }

        // Search everywhere (Ctrl+P).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::P)) {
            self.palette_open = !self.palette_open;
            if self.palette_open {
                self.palette_query.clear();
                self.palette_index = 0;
                self.palette_focus = true;
            }
        }
        if self.palette_open {
            self.draw_palette(ctx);
        }

        self.draw_cell_viewer(ctx);
        self.draw_fk_peek(ctx);
        self.draw_comments_dialog(ctx);
        self.draw_history_window(ctx, &mut pending_actions);

        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_source("sidebar_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let (mgr_action, tree_action) =
                            crate::connections::manager_ui::draw_connection_list(
                                ui,
                                &mut self.manager,
                                &self.profiles,
                                &mut self.active,
                            );
                        pending_actions.push(Box::new(move |app: &mut Self| {
                            app.handle_manager_action(mgr_action)
                        }));
                        pending_actions.push(Box::new(move |app: &mut Self| {
                            app.apply_tree_action(tree_action)
                        }));
                    });
            });

        // 3. Status bar.
        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(10.0, 4.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let conn_n = self.active.len();
                    let conn_txt = format!(
                        "{conn_n} connection{}",
                        if conn_n == 1 { "" } else { "s" }
                    );
                    ui.label(egui::RichText::new(conn_txt).color(ui.visuals().weak_text_color()));
                    if let Some(msg) = &self.status {
                        ui.separator();
                        ui.label(msg);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let pending_n = self.pending.len();
                        if pending_n > 0 {
                            ui.spinner();
                            ui.weak(format!("{pending_n} in flight"));
                        }
                    });
                });
            });

        // 4. Central: tab bar + active tab.
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.tabs.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.label(egui::RichText::new("🗄").size(52.0).color(theme::ACCENT));
                    ui.add_space(8.0);
                    ui.heading("Welcome to dbTool");
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("A lightweight desktop client for PostgreSQL, MySQL and SQL Server.")
                            .color(ui.visuals().weak_text_color()),
                    );
                    ui.add_space(20.0);
                    let dim = ui.visuals().weak_text_color();
                    ui.label(egui::RichText::new("Get started").strong());
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("1.  Add a connection in the sidebar and click Connect").color(dim));
                    ui.label(egui::RichText::new("2.  Expand a schema, then double-click a table to browse it").color(dim));
                    ui.label(egui::RichText::new("3.  Or press SQL on a connection to open a query editor").color(dim));
                });
                return;
            }

            // Tab strip
            let tabbar_frame = egui::Frame::none()
                .fill(ui.visuals().faint_bg_color)
                .inner_margin(egui::Margin::symmetric(4.0, 3.0))
                .rounding(egui::Rounding::same(6.0));
            tabbar_frame.show(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_source("tab_strip_scroll")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let ids_titles: Vec<(TabId, String)> =
                                self.tabs.iter().map(|t| (t.id(), t.title())).collect();
                            let mut to_close: Option<TabId> = None;
                            for (id, title) in ids_titles {
                                let selected = self.active_tab == Some(id);
                                let (fill, text_color) = if selected {
                                    (ui.visuals().selection.bg_fill, ui.visuals().selection.stroke.color)
                                } else {
                                    (egui::Color32::TRANSPARENT, ui.visuals().text_color())
                                };
                                // Title and × are separate, non-overlapping buttons so the
                                // close click can never be swallowed by a pill-wide widget.
                                let inner = egui::Frame::none()
                                    .fill(fill)
                                    .rounding(egui::Rounding::same(5.0))
                                    .inner_margin(egui::Margin::symmetric(4.0, 2.0))
                                    .show(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 2.0;
                                            let select = ui.add(
                                                egui::Button::new(
                                                    egui::RichText::new(&title).color(text_color),
                                                )
                                                .frame(false),
                                            );
                                            if select.clicked() {
                                                self.active_tab = Some(id);
                                            }
                                            if select.middle_clicked() {
                                                to_close = Some(id);
                                            }
                                            // Hand-painted × so it is always dead-center in
                                            // its hit area (text glyphs sit off-baseline).
                                            let (close_rect, close) = ui.allocate_exact_size(
                                                egui::vec2(20.0, 20.0),
                                                egui::Sense::click(),
                                            );
                                            let close =
                                                close.on_hover_text("Close tab (or middle-click)");
                                            if ui.is_rect_visible(close_rect) {
                                                let center = close_rect.center();
                                                if close.hovered() {
                                                    ui.painter().circle_filled(
                                                        center,
                                                        8.0,
                                                        ui.visuals().widgets.hovered.weak_bg_fill,
                                                    );
                                                }
                                                let color = if close.hovered() {
                                                    ui.visuals().strong_text_color()
                                                } else {
                                                    text_color
                                                };
                                                let r = 3.5;
                                                let stroke = egui::Stroke::new(1.4, color);
                                                ui.painter().line_segment(
                                                    [
                                                        center + egui::vec2(-r, -r),
                                                        center + egui::vec2(r, r),
                                                    ],
                                                    stroke,
                                                );
                                                ui.painter().line_segment(
                                                    [
                                                        center + egui::vec2(-r, r),
                                                        center + egui::vec2(r, -r),
                                                    ],
                                                    stroke,
                                                );
                                            }
                                            if close.clicked() || close.middle_clicked() {
                                                to_close = Some(id);
                                            }
                                        });
                                    });
                                let hovered = inner.response.hovered();
                                if hovered {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                    if !selected {
                                        ui.painter().rect_stroke(
                                            inner.response.rect,
                                            egui::Rounding::same(5.0),
                                            egui::Stroke::new(
                                                1.0,
                                                ui.visuals().widgets.hovered.bg_stroke.color,
                                            ),
                                        );
                                    }
                                }
                            }
                            if let Some(id) = to_close {
                                // Dirty diagram tabs need a second click to
                                // discard unsaved DBML changes.
                                let dirty_diagram = self.tabs.iter().any(|t| {
                                    matches!(t, Tab::Diagram(d) if d.id == id && d.is_dirty())
                                });
                                if dirty_diagram && self.close_armed != Some(id) {
                                    self.close_armed = Some(id);
                                    self.status = Some(
                                        "Unsaved .dbml changes — close again to discard, or Ctrl+S to save"
                                            .into(),
                                    );
                                } else {
                                    self.close_armed = None;
                                    self.tabs.retain(|t| t.id() != id);
                                    if self.active_tab == Some(id) {
                                        self.active_tab = self.tabs.first().map(|t| t.id());
                                    }
                                }
                            }
                        });
                    });
            });
            ui.add_space(6.0);

            // Active tab body
            let Some(active_id) = self.active_tab else { return };
            let Some(pos) = self.tabs.iter().position(|t| t.id() == active_id) else { return };

            // Production banner: unmistakable red strip over prod-tab content.
            if let Some(pid) = self.tabs[pos].profile_id() {
                if self.profiles.iter().any(|p| p.id == pid && p.production) {
                    let (bar, _) =
                        ui.allocate_exact_size(egui::vec2(ui.available_width(), 5.0), egui::Sense::hover());
                    ui.painter().rect_filled(bar, 2.0, theme::PROD_RED);
                    ui.add_space(2.0);
                }
            }

            match &mut self.tabs[pos] {
                Tab::DataSync(t) => {
                    let action = crate::ui::datasync_tab::draw(ui, t, &self.active);
                    let tab_id = t.id;
                    match action {
                        crate::ui::datasync_tab::DataSyncAction::None => {}
                        crate::ui::datasync_tab::DataSyncAction::LoadTables => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.datasync_load_tables(tab_id);
                            }));
                        }
                        crate::ui::datasync_tab::DataSyncAction::Compare => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.datasync_compare(tab_id);
                            }));
                        }
                        crate::ui::datasync_tab::DataSyncAction::OpenSyncScript => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.datasync_open_script(tab_id);
                            }));
                        }
                        crate::ui::datasync_tab::DataSyncAction::Pull => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.datasync_pull(tab_id);
                            }));
                        }
                    }
                }
                Tab::Query(q) => {
                    let profile_id = q.profile_id;
                    let ac = self.active.iter().find(|a| a.profile_id == profile_id);
                    let schema_cache = ac.and_then(|a| a.schema_cache.as_ref());
                    let dialect = ac.map(|a| a.kind).unwrap_or(crate::db::DbKind::Postgres);
                    let action = crate::ui::query_tab::draw(
                        ui,
                        q,
                        schema_cache,
                        dialect,
                        self.settings.sql_line_numbers,
                    );
                    match action {
                        crate::ui::query_tab::QueryTabAction::None => {}
                        crate::ui::query_tab::QueryTabAction::Run => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.run_query_tab(tab_id, false);
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::Explain => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.run_query_tab(tab_id, true);
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::ExplainVisual => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.run_explain_visual(tab_id);
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::Cancel => {
                            if let Some(req) = q.running_req {
                                pending_actions.push(Box::new(move |app: &mut Self| {
                                    app.runtime.send(Command::CancelQuery { req });
                                }));
                            }
                        }
                        crate::ui::query_tab::QueryTabAction::OpenFile => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.sql_file_target = Some((tab_id, SqlFileMode::Open));
                                app.sql_file_dialog.select_file();
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::SaveFile => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.save_query_file(tab_id);
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::History => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.history_open = true;
                                if app.history.is_none() {
                                    app.history = Some(crate::history::load(500));
                                }
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::Snippets => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.snippets_target = Some(tab_id);
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::ViewCell { title, content } => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.cell_viewer = Some((title, content));
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::CommitCell { row, col, text } => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.commit_query_cell(tab_id, row, col, text);
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::InsertRow { values } => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.query_insert_row(tab_id, values);
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::DeleteRow { row } => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.query_delete_row(tab_id, row);
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::Export => {
                            let tab_id = q.id;
                            let conn_id = q.conn_id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.open_export_dialog(
                                    conn_id,
                                    ExportSource::QueryResult { tab_id },
                                    "query-result".to_string(),
                                );
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::FetchAll => {
                            let tab_id = q.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.fetch_all_rows(tab_id);
                            }));
                        }
                    }
                }
                Tab::TableEditor(t) => {
                    if !t.structure.is_new_table {
                        ui.horizontal(|ui| {
                            if ui
                                .selectable_label(t.view == EditorView::Data, "Data")
                                .clicked()
                            {
                                t.view = EditorView::Data;
                            }
                            if ui
                                .selectable_label(t.view == EditorView::Structure, "Structure")
                                .clicked()
                            {
                                t.view = EditorView::Structure;
                            }
                        });
                        ui.separator();
                    }
                    match t.view {
                        EditorView::Data => {
                            let mut hover = None;
                            let action = crate::ui::table_editor::draw(ui, t, &mut hover);
                            handle_table_editor_action(action, t, &mut pending_actions);
                            if let Some(cell) = hover {
                                fk_hover_cell = Some((t.conn_id, t.db_kind, cell));
                            }
                        }
                        EditorView::Structure => {
                            let action = crate::ui::table_structure::draw(ui, t);
                            // Keep the tab title in sync while naming a new table.
                            if t.structure.is_new_table {
                                if let Some(w) = &t.structure.working {
                                    t.table = w.name.clone();
                                }
                            }
                            handle_structure_action(action, t, &mut pending_actions);
                        }
                    }
                }
                Tab::Diagram(d) => {
                    use crate::ui::diagram_tab::DiagramAction;
                    match crate::ui::diagram_tab::draw(ui, d, self.settings.dbml_line_numbers) {
                        DiagramAction::None => {}
                        DiagramAction::SaveFile => {
                            let tab_id = d.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.save_diagram_file(tab_id);
                            }));
                        }
                        DiagramAction::RefreshFromDb => {
                            let tab_id = d.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.refresh_diagram_from_db(tab_id);
                            }));
                        }
                        DiagramAction::Status(s) => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.status = Some(s);
                            }));
                        }
                    }
                }
                Tab::Compare(t) => {
                    use crate::ui::compare_tab::CompareAction;
                    match crate::ui::compare_tab::draw(ui, t, &self.active) {
                        CompareAction::None => {}
                        CompareAction::Snapshot => {
                            let tab_id = t.id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.start_compare_snapshot(tab_id);
                            }));
                        }
                        CompareAction::Apply { conn, statements } => {
                            let tab_id = t.id;
                            t.applying = true;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                if app.conn_read_only(conn) {
                                    if let Some(Tab::Compare(t)) = app.find_tab_mut(tab_id) {
                                        t.applying = false;
                                        t.error = Some(READ_ONLY_MSG.into());
                                    }
                                    return;
                                }
                                app.send(Pending::CompareApplyDdl(tab_id), move |req| {
                                    Command::ApplyDdl { req, conn, statements }
                                });
                            }));
                        }
                        CompareAction::OpenScript { conn, sql } => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.open_query_tab(conn, sql);
                            }));
                        }
                    }
                }
                Tab::Sessions(t) => {
                    use crate::ui::sessions_tab::SessionsAction;
                    let now = ui.input(|i| i.time);
                    let tab_id = t.id;
                    let due = t.auto_refresh
                        && now - t.last_refresh > 5.0
                        && !matches!(t.status, TabStatus::Running(_));
                    match crate::ui::sessions_tab::draw(ui, t) {
                        SessionsAction::None => {
                            if due {
                                pending_actions.push(Box::new(move |app: &mut Self| {
                                    app.refresh_sessions(tab_id, now);
                                }));
                            }
                        }
                        SessionsAction::Refresh => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.refresh_sessions(tab_id, now);
                            }));
                        }
                        SessionsAction::CancelQuery(pid) => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.sessions_kill(tab_id, pid, false);
                            }));
                        }
                        SessionsAction::Kill(pid) => {
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.sessions_kill(tab_id, pid, true);
                            }));
                        }
                    }
                    if t.auto_refresh {
                        // Wake up in time for the next auto refresh.
                        ui.ctx().request_repaint_after(std::time::Duration::from_secs(5));
                    }
                }
            }
        });

        // FK hover preview: register this frame's dwell, then draw/dismiss.
        if let Some((conn, kind, cell)) = fk_hover_cell {
            self.note_fk_hover(ctx, conn, kind, cell);
        }
        self.draw_fk_hover(ctx);

        // 5. Edit dialog (modal-ish).
        if self.manager.editing.is_some() {
            let action = crate::connections::manager_ui::draw_edit_dialog(ctx, &mut self.manager);
            pending_actions.push(Box::new(move |app: &mut Self| app.handle_manager_action(action)));
        }

        // Database-toggle picker.
        if let Some(pid) = self.db_picker {
            match self.profiles.iter().find(|p| p.id == pid) {
                Some(profile) => {
                    let action = crate::connections::manager_ui::draw_db_picker(
                        ctx,
                        profile,
                        &self.active,
                    );
                    pending_actions
                        .push(Box::new(move |app: &mut Self| app.handle_db_picker_action(action)));
                }
                None => self.db_picker = None,
            }
        }

        // 6. Apply deferred actions.
        for f in pending_actions {
            f(self);
        }
    }
}

fn handle_structure_action(
    action: crate::ui::table_structure::StructureAction,
    tab: &mut TableEditorTab,
    pending_actions: &mut Vec<Box<dyn FnOnce(&mut App)>>,
) {
    use crate::ui::table_structure::StructureAction as A;
    match action {
        A::None => {}
        A::Load | A::Reload => {
            tab.structure.loading = true;
            let tab_id = tab.id;
            let conn = tab.conn_id;
            let schema = tab.schema.clone();
            let table = tab
                .structure
                .working
                .as_ref()
                .and_then(|w| w.original_name.clone())
                .unwrap_or_else(|| tab.table.clone());
            if matches!(action, A::Reload) {
                tab.structure.working = None;
                tab.structure.selected = None;
            }
            tab.status = TabStatus::Running("introspecting…".into());
            pending_actions.push(Box::new(move |app: &mut App| {
                app.send(Pending::DescribeStructure(tab_id), move |req| {
                    Command::DescribeStructure { req, conn, schema, table }
                });
            }));
        }
        A::Apply { statements } => {
            if statements.is_empty() {
                return;
            }
            tab.structure.applying = true;
            let n = statements.len();
            tab.status = TabStatus::Running(format!(
                "applying {n} statement{}…",
                if n == 1 { "" } else { "s" }
            ));
            let tab_id = tab.id;
            let conn = tab.conn_id;
            pending_actions.push(Box::new(move |app: &mut App| {
                if app.refuse_readonly_tab(tab_id) {
                    if let Some(Tab::TableEditor(t)) = app.find_tab_mut(tab_id) {
                        t.structure.applying = false;
                    }
                    return;
                }
                app.send(Pending::ApplyDdl(tab_id), move |req| Command::ApplyDdl {
                    req,
                    conn,
                    statements,
                });
            }));
        }
        A::OpenSql { sql } => {
            let conn_id = tab.conn_id;
            pending_actions.push(Box::new(move |app: &mut App| {
                app.open_query_tab(conn_id, sql);
            }));
        }
    }
}

fn handle_table_editor_action(
    action: crate::ui::table_editor::TableEditorAction,
    tab: &mut TableEditorTab,
    pending_actions: &mut Vec<Box<dyn FnOnce(&mut App)>>,
) {
    use crate::ui::table_editor::TableEditorAction as A;
    match action {
        A::None => {}
        A::Refresh => {
            tab.clear_pending();
            tab.selected_rows.clear();
            tab.selection_anchor = None;
            let tab_id = tab.id;
            let conn = tab.conn_id;
            let schema = tab.schema.clone();
            let table = tab.table.clone();
            let offset = tab.offset;
            let limit = tab.limit;
            let filter = table_rows_filter(tab);
            tab.status = TabStatus::Running("refreshing…".into());
            pending_actions.push(Box::new(move |app: &mut App| {
                app.send(Pending::TableRows(tab_id), move |req| Command::FetchTableRows {
                    req,
                    conn,
                    schema,
                    table,
                    limit,
                    offset,
                    filter,
                });
            }));
        }
        A::PrevPage => {
            tab.clear_pending();
            tab.selected_rows.clear();
            tab.selection_anchor = None;
            tab.offset = (tab.offset - tab.limit).max(0);
            let tab_id = tab.id;
            let conn = tab.conn_id;
            let schema = tab.schema.clone();
            let table = tab.table.clone();
            let offset = tab.offset;
            let limit = tab.limit;
            let filter = table_rows_filter(tab);
            tab.status = TabStatus::Running("loading…".into());
            pending_actions.push(Box::new(move |app: &mut App| {
                app.send(Pending::TableRows(tab_id), move |req| Command::FetchTableRows {
                    req,
                    conn,
                    schema,
                    table,
                    limit,
                    offset,
                    filter,
                });
            }));
        }
        A::NextPage => {
            tab.clear_pending();
            tab.selected_rows.clear();
            tab.selection_anchor = None;
            tab.offset += tab.limit;
            let tab_id = tab.id;
            let conn = tab.conn_id;
            let schema = tab.schema.clone();
            let table = tab.table.clone();
            let offset = tab.offset;
            let limit = tab.limit;
            let filter = table_rows_filter(tab);
            tab.status = TabStatus::Running("loading…".into());
            pending_actions.push(Box::new(move |app: &mut App| {
                app.send(Pending::TableRows(tab_id), move |req| Command::FetchTableRows {
                    req,
                    conn,
                    schema,
                    table,
                    limit,
                    offset,
                    filter,
                });
            }));
        }
        A::BeginInsert => {
            tab.insert_draft = Some(BTreeMap::new());
        }
        A::CancelInsert => {
            tab.insert_draft = None;
        }
        A::CommitInsert { values } => {
            let tab_id = tab.id;
            let conn = tab.conn_id;
            let schema = tab.schema.clone();
            let table = tab.table.clone();
            tab.status = TabStatus::Running("inserting…".into());
            pending_actions.push(Box::new(move |app: &mut App| {
                if app.refuse_readonly_tab(tab_id) {
                    return;
                }
                app.send(Pending::RowInsert(tab_id), move |req| Command::InsertRow {
                    req,
                    conn,
                    schema,
                    table,
                    values,
                });
            }));
        }
        A::CommitPending { updates, deletes } => {
            let tab_id = tab.id;
            let conn = tab.conn_id;
            let schema = tab.schema.clone();
            let table = tab.table.clone();
            let n_u = updates.len();
            let n_d = deletes.len();
            tab.status = TabStatus::Running(format!(
                "committing {} edit(s), {} delete(s)…",
                n_u, n_d
            ));
            pending_actions.push(Box::new(move |app: &mut App| {
                if app.refuse_readonly_tab(tab_id) {
                    return;
                }
                app.send(Pending::ApplyChanges(tab_id), move |req| Command::ApplyChanges {
                    req,
                    conn,
                    schema,
                    table,
                    updates,
                    deletes,
                });
            }));
        }
        A::ApplyFilter => {
            tab.clear_pending();
            tab.selected_rows.clear();
            tab.selection_anchor = None;
            tab.offset = 0;
            let tab_id = tab.id;
            pending_actions.push(Box::new(move |app: &mut App| {
                app.reload_table_tab(tab_id, "Filtered.");
            }));
        }
        A::GoToFk { schema, table, column, value } => {
            let conn = tab.conn_id;
            let filter = format!(
                "{} = {}",
                crate::db::quote_ident(tab.db_kind, &column),
                crate::ui::results_grid::sql_literal(&value)
            );
            pending_actions.push(Box::new(move |app: &mut App| {
                app.open_fk_peek(conn, schema, table, filter);
            }));
        }
        A::ViewCell { title, content } => {
            pending_actions.push(Box::new(move |app: &mut App| {
                app.cell_viewer = Some((title, content));
            }));
        }
    }
}

/// Queries finishing after at least this long, while the window is
/// unfocused, raise a desktop notification.
const NOTIFY_AFTER: std::time::Duration = std::time::Duration::from_secs(8);

/// Fire-and-forget desktop notification; failures (no notification daemon,
/// e.g. bare WSL) are silently ignored.
fn notify_desktop(title: &str, body: &str) {
    let title = title.to_owned();
    let body = body.to_owned();
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .appname("dbTool")
            .summary(&title)
            .body(&body)
            .show();
    });
}

/// Dialect query behind the sessions-monitor tab. First column MUST be the
/// numeric session id — the kill buttons read it.
fn sessions_query(kind: crate::db::DbKind) -> &'static str {
    match kind {
        crate::db::DbKind::Postgres => {
            "SELECT pid, usename AS user, datname AS database, state, \
                    COALESCE(EXTRACT(EPOCH FROM (now() - query_start))::int, 0) AS seconds, \
                    LEFT(query, 300) AS query \
             FROM pg_stat_activity \
             WHERE pid <> pg_backend_pid() AND backend_type = 'client backend' \
             ORDER BY query_start DESC NULLS LAST"
        }
        crate::db::DbKind::MySql => {
            "SELECT id AS pid, user, db AS `database`, command AS state, \
                    time AS seconds, LEFT(COALESCE(info, ''), 300) AS query \
             FROM information_schema.processlist \
             WHERE id <> CONNECTION_ID() \
             ORDER BY time DESC"
        }
        crate::db::DbKind::MsSql => {
            "SELECT s.session_id AS pid, s.login_name AS [user], \
                    DB_NAME(s.database_id) AS [database], \
                    COALESCE(r.status, s.status) AS state, \
                    COALESCE(r.total_elapsed_time, 0) / 1000 AS seconds, \
                    LEFT(COALESCE(t.text, ''), 300) AS query \
             FROM sys.dm_exec_sessions s \
             LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
             OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t \
             WHERE s.is_user_process = 1 AND s.session_id <> @@SPID \
             ORDER BY s.session_id"
        }
        // No server sessions; the Sessions tab is never offered for SQLite.
        crate::db::DbKind::Sqlite => {
            "SELECT 0 AS pid, '' AS user, '' AS database, '' AS state, \
                    0 AS seconds, '' AS query LIMIT 0"
        }
    }
}

/// Char offset a server error message points at, if it carries one.
/// Postgres: our drivers annotate `[position:N]` (1-based chars). MySQL:
/// "… at line N" — best effort, the start of that line.
fn error_char_position(error: &str, sql: &str) -> Option<usize> {
    if let Some(i) = error.find("[position:") {
        let digits: String = error[i + 10..].chars().take_while(|c| c.is_ascii_digit()).collect();
        let n: usize = digits.parse().ok()?;
        let pos = n.saturating_sub(1);
        return (pos <= sql.chars().count()).then_some(pos);
    }
    if let Some(i) = error.rfind(" at line ") {
        let digits: String = error[i + 9..].chars().take_while(|c| c.is_ascii_digit()).collect();
        let n: usize = digits.parse().ok()?;
        let mut chars = 0usize;
        for (li, line) in sql.split('\n').enumerate() {
            if li + 1 == n {
                return Some(chars);
            }
            chars += line.chars().count() + 1;
        }
    }
    None
}

/// Does the script open a transaction it never closes?
fn has_dangling_transaction(sql: &str) -> bool {
    let mut opens = 0i32;
    let mut closes = 0i32;
    for stmt in crate::db::split_statements(sql) {
        let s = stmt.trim_start().to_ascii_uppercase();
        if s == "BEGIN"
            || s.starts_with("BEGIN TRAN")
            || s.starts_with("BEGIN TRANSACTION")
            || s.starts_with("BEGIN WORK")
            || s.starts_with("BEGIN ISOLATION")
            || s.starts_with("START TRANSACTION")
        {
            opens += 1;
        } else if s.starts_with("COMMIT")
            || (s.starts_with("ROLLBACK") && !s.starts_with("ROLLBACK TO"))
        {
            closes += 1;
        }
    }
    opens > closes
}

/// The current filter/sort of a table tab as driver parameters: the free-form
/// WHERE bar AND all per-column quick filters.
fn table_rows_filter(tab: &TableEditorTab) -> crate::db::RowsFilter {
    let mut conds: Vec<String> = Vec::new();
    let top = tab.filter.trim();
    if !top.is_empty() {
        conds.push(format!("({top})"));
    }
    for (col, val) in &tab.col_filters {
        let v = val.trim();
        if !v.is_empty() {
            conds.push(column_condition(tab.db_kind, col, v));
        }
    }
    crate::db::RowsFilter {
        where_clause: conds.join(" AND "),
        order_col: tab.sort.as_ref().map(|(c, _)| c.clone()),
        order_desc: tab.sort.as_ref().map(|(_, d)| *d).unwrap_or(false),
    }
}

/// One per-column quick filter as SQL, with the identifier quoted so
/// case-sensitive column names just work. A bare value means equality
/// (numbers unquoted, strings quoted, NULL → IS NULL); anything starting
/// with an operator or keyword is appended as written.
fn column_condition(kind: crate::db::DbKind, col: &str, v: &str) -> String {
    let ident = crate::db::quote_ident(kind, col);
    let upper = v.to_ascii_uppercase();
    let is_expr = ["=", "!", "<", ">"].iter().any(|p| upper.starts_with(p))
        || ["IS ", "LIKE ", "ILIKE ", "IN ", "NOT ", "BETWEEN "]
            .iter()
            .any(|p| upper.starts_with(p));
    if is_expr {
        format!("{ident} {v}")
    } else if upper == "NULL" {
        format!("{ident} IS NULL")
    } else if v.parse::<f64>().is_ok() {
        format!("{ident} = {v}")
    } else {
        format!("{ident} = '{}'", v.replace('\'', "''"))
    }
}


impl App {
    fn apply_tree_action(&mut self, action: crate::ui::schema_tree::TreeAction) {
        use crate::ui::schema_tree::TreeAction as T;
        match action {
            T::None => {}
            T::ExpandSchemas(conn) => {
                self.send(Pending::ListSchemas, move |req| Command::ListSchemas {
                    req,
                    conn,
                });
            }
            T::ExpandTables(conn, schema) => {
                let schema_o = schema.clone();
                self.send(Pending::ListTables, move |req| {
                    Command::ListTables { req, conn, schema }
                });
                self.send(Pending::ListSchemaObjects, move |req| {
                    Command::ListSchemaObjects { req, conn, schema: schema_o }
                });
            }
            T::OpenTable(conn, schema, table) => {
                self.open_table_tab(conn, schema, table);
            }
            T::ModifyTable(conn, schema, table) => {
                self.open_table_tab_view(conn, schema, table, EditorView::Structure);
            }
            T::NewTable(conn, schema) => {
                self.open_new_table_tab(conn, schema);
            }
            T::OpenQueryTab(conn) => {
                self.open_query_tab(conn, String::new());
            }
            T::ViewAsDbml(conn) => {
                let Some(ac) = self.find_active_by_conn(conn) else { return };
                let name = ac.name.clone();
                let source = (ac.profile_id, ac.database.clone());
                let is_primary = ac.is_primary;
                // The database's document persists across sessions: open it
                // if it exists (keeping the user's groups and edits); only
                // introspect when there is nothing yet.
                let path = self.database_dbml_path(source.0, &source.1, is_primary, &name);
                if path.exists() {
                    self.open_diagram_tab(path, Some(source));
                    return;
                }
                self.status = Some(format!("Introspecting {name} for DBML…"));
                self.send(Pending::DumpDbml(conn), move |req| Command::DumpDbml {
                    req,
                    conn,
                });
            }
            T::OpenSessions(conn) => {
                self.open_sessions_tab(conn);
            }
            T::UsersAndRoles(conn) => {
                self.open_users_tab(conn);
            }
            T::DataSync(conn) => {
                self.open_datasync_tab(conn);
            }
            T::DumpDatabase(conn) => {
                self.open_backup_dialog(conn, false);
            }
            T::RestoreDatabase(conn) => {
                self.open_backup_dialog(conn, true);
            }
            T::EditComments(conn, schema, table) => {
                self.open_comments_dialog(conn, schema, table);
            }
            T::CompareStructure(conn) => {
                let id = Uuid::new_v4();
                self.tabs.push(Tab::Compare(crate::ui::compare_tab::CompareTab::new(
                    id,
                    Some(conn),
                )));
                self.active_tab = Some(id);
            }
            T::RoutineDdl(conn, schema, name, kind) => {
                self.status = Some(format!("Fetching source of {schema}.{name}…"));
                self.send(Pending::RoutineDdl, move |req| Command::RoutineDdl {
                    req,
                    conn,
                    schema,
                    name,
                    kind,
                });
            }
            T::ViewDefinition(conn, schema, view) => {
                self.status = Some(format!("Fetching definition of {schema}.{view}…"));
                self.send(Pending::RoutineDdl, move |req| Command::RoutineDdl {
                    req,
                    conn,
                    schema,
                    name: view,
                    kind: "view".to_owned(),
                });
            }
            T::CompareSelected(a, b) => {
                let id = Uuid::new_v4();
                let mut tab = crate::ui::compare_tab::CompareTab::new(id, Some(a));
                tab.right = Some(b);
                self.tabs.push(Tab::Compare(tab));
                self.active_tab = Some(id);
                self.start_compare_snapshot(id);
            }
            T::DumpDdl(conn, schema) => {
                let Some(ac) = self.find_active_by_conn(conn) else { return };
                let conn_name = ac.name.clone();
                let default_dir = format!("~/dbtool-ddl/{}", safe_dir_name(&conn_name));
                self.dump_dialog = Some(DumpState {
                    conn,
                    conn_name,
                    schema,
                    dir: default_dir,
                    running: false,
                    result: None,
                    error: None,
                });
            }
            T::ImportInto(conn, schema, table) => {
                if self.find_active_by_conn(conn).is_none() {
                    return;
                }
                self.import_dialog = Some(ImportState {
                    conn,
                    schema,
                    table,
                    path: String::new(),
                    file_dialog: egui_file_dialog::FileDialog::new(),
                    delimiter: ",".to_string(),
                    has_header: true,
                    empty_as_null: true,
                    running: false,
                    progress_rows: 0,
                    result: None,
                    error: None,
                });
            }
            T::ExportFrom(conn, schema, table) => {
                if self.find_active_by_conn(conn).is_none() {
                    return;
                }
                let file_stem = table.clone();
                self.open_export_dialog(
                    conn,
                    ExportSource::Table { schema, table },
                    file_stem,
                );
            }
        }
    }

    fn handle_dump_action(&mut self, action: DumpAction) {
        match action {
            DumpAction::None => {}
            DumpAction::Close => self.dump_dialog = None,
            DumpAction::Start => {
                let Some(d) = self.dump_dialog.as_mut() else { return };
                if !self.active.iter().any(|a| a.conn_id == d.conn) {
                    d.error = Some("Connection is no longer active.".into());
                    return;
                }
                let conn = d.conn;
                let schemas = d.schema.clone().into_iter().collect::<Vec<_>>();
                let dir = d.dir.trim().to_string();
                d.running = true;
                d.result = None;
                d.error = None;
                self.send(Pending::DumpDdl, move |req| Command::DumpDdl {
                    req,
                    conn,
                    schemas,
                    dir,
                });
            }
        }
    }

    fn open_export_dialog(&mut self, conn: ConnectionId, source: ExportSource, file_stem: String) {
        self.export_dialog = Some(ExportState {
            conn,
            source,
            path: format!("~/{}.csv", safe_dir_name(&file_stem)),
            format: crate::csv_export::ExportFormat::Csv,
            delimiter: ",".to_string(),
            include_header: true,
            running: false,
            progress_rows: 0,
            error: None,
            file_dialog: egui_file_dialog::FileDialog::new(),
        });
    }

    /// Open the dump/restore dialog for a connection.
    fn open_backup_dialog(&mut self, conn: ConnectionId, restore: bool) {
        let Some(ac) = self.find_active_by_conn(conn) else { return };
        if restore && self.conn_read_only(conn) {
            self.status = Some(READ_ONLY_MSG.into());
            return;
        }
        let stem = safe_dir_name(if ac.database.is_empty() { &ac.name } else { &ac.database });
        let ext = match ac.kind {
            crate::db::DbKind::Postgres => "dump",
            crate::db::DbKind::MySql => "sql",
            _ => "db",
        };
        let date = chrono::Local::now().format("%Y-%m-%d");
        self.backup_dialog = Some(crate::ui::import_export::BackupState {
            conn,
            conn_name: ac.name.clone(),
            kind: ac.kind,
            restore,
            path: format!("~/dbtool-backups/{stem}-{date}.{ext}"),
            running: false,
            result: None,
            error: None,
            armed: false,
            file_dialog: egui_file_dialog::FileDialog::new(),
        });
    }

    fn handle_backup_action(&mut self, action: crate::ui::import_export::BackupAction) {
        use crate::ui::import_export::BackupAction as A;
        match action {
            A::None => {}
            A::Close => self.backup_dialog = None,
            A::Start => {
                let Some(d) = self.backup_dialog.as_mut() else { return };
                let (conn, path, restore) = (d.conn, d.path.trim().to_string(), d.restore);
                d.running = true;
                d.result = None;
                d.error = None;
                if restore {
                    self.send(Pending::Backup, move |req| Command::RestoreDatabase {
                        req,
                        conn,
                        path,
                    });
                } else {
                    self.send(Pending::Backup, move |req| Command::DumpDatabase {
                        req,
                        conn,
                        path,
                    });
                }
            }
        }
    }

    fn handle_export_action(&mut self, action: ExportAction) {
        match action {
            ExportAction::None => {}
            ExportAction::Close => self.export_dialog = None,
            ExportAction::Start => {
                let (format, delimiter, path, include_header, conn, source) = {
                    let Some(d) = self.export_dialog.as_ref() else { return };
                    let delimiter = match d.format {
                        crate::csv_export::ExportFormat::Json => b',',
                        crate::csv_export::ExportFormat::Csv => {
                            let Some(delim) = d.delimiter_byte() else { return };
                            delim
                        }
                    };
                    (
                        d.format,
                        delimiter,
                        d.path.trim().to_string(),
                        d.include_header,
                        d.conn,
                        d.source.clone(),
                    )
                };
                let options = crate::csv_export::ExportOptions { format, delimiter, include_header };
                match source {
                    ExportSource::Table { schema, table } => {
                        if !self.active.iter().any(|a| a.conn_id == conn) {
                            if let Some(d) = self.export_dialog.as_mut() {
                                d.error = Some("Connection is no longer active.".into());
                            }
                            return;
                        }
                        if let Some(d) = self.export_dialog.as_mut() {
                            d.running = true;
                            d.progress_rows = 0;
                            d.error = None;
                        }
                        self.send(Pending::ExportCsv, move |req| Command::ExportCsv {
                            req,
                            conn,
                            schema,
                            table,
                            path,
                            options,
                        });
                    }
                    ExportSource::QueryResult { tab_id } => {
                        let result = match self.find_tab_mut(tab_id) {
                            Some(Tab::Query(q)) => q.current_result().cloned(),
                            _ => None,
                        };
                        let Some(result) = result else {
                            if let Some(d) = self.export_dialog.as_mut() {
                                d.error = Some(
                                    "The query tab (or its result) is gone — run the query again."
                                        .into(),
                                );
                            }
                            return;
                        };
                        if let Some(d) = self.export_dialog.as_mut() {
                            d.running = true;
                            d.progress_rows = 0;
                            d.error = None;
                        }
                        self.send(Pending::ExportCsv, move |req| Command::ExportResultCsv {
                            req,
                            result,
                            path,
                            options,
                        });
                    }
                }
            }
        }
    }

    fn handle_import_action(&mut self, action: ImportAction) {
        match action {
            ImportAction::None => {}
            ImportAction::Close => self.import_dialog = None,
            ImportAction::Start => {
                let read_only = self
                    .import_dialog
                    .as_ref()
                    .map(|d| self.conn_read_only(d.conn))
                    .unwrap_or(false);
                let Some(d) = self.import_dialog.as_mut() else { return };
                let Some(delimiter) = d.delimiter_byte() else { return };
                if !self.active.iter().any(|a| a.conn_id == d.conn) {
                    d.error = Some("Connection is no longer active.".into());
                    return;
                }
                if read_only {
                    d.error = Some(READ_ONLY_MSG.into());
                    return;
                }
                let conn = d.conn;
                let schema = d.schema.clone();
                let table = d.table.clone();
                let path = d.path.trim().to_string();
                let options = crate::csv_import::ImportOptions {
                    delimiter,
                    has_header: d.has_header,
                    empty_as_null: d.empty_as_null,
                };
                d.running = true;
                d.progress_rows = 0;
                d.result = None;
                d.error = None;
                self.send(Pending::ImportCsv, move |req| Command::ImportCsv {
                    req,
                    conn,
                    schema,
                    table,
                    path,
                    options,
                });
            }
        }
    }
}

fn safe_dir_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if cleaned.is_empty() { "connection".to_string() } else { cleaned }
}

fn profile_file_display() -> String {
    dirs::config_dir()
        .map(|p| p.join("dbTool").join("connections.json").display().to_string())
        .unwrap_or_else(|| "~/.config/dbTool/connections.json".to_string())
}
