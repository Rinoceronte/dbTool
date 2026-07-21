use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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
use crate::ui::theme::{self, ThemeMode};
use crate::ui::{
    ActiveConnection, EditorView, QueryTab, SchemaNode, StructSel, StructureState, Tab, TabId,
    TabStatus, TableEditorTab,
};

enum Pending {
    Connect(Uuid), // profile id
    TestConnection,
    ListSchemas(Uuid),
    ListTables(Uuid),
    DescribeTable(TabId),
    Query(TabId),
    TableRows(TabId),
    RowInsert(TabId),
    ApplyChanges(TabId),
    DescribeStructure(TabId),
    ApplyDdl(TabId),
    FetchDbMeta(Uuid),
    DumpDdl,
    DumpDbml(Uuid),     // profile id — first generation, creates the file
    RefreshDbml(TabId), // merge fresh structure into an open diagram tab
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
    theme: ThemeMode,
    dump_dialog: Option<DumpState>,
    import_dialog: Option<ImportState>,
    export_dialog: Option<ExportState>,
    dbml_file_dialog: egui_file_dialog::FileDialog,
    /// Dirty diagram tab whose close button was clicked once; a second click
    /// discards the unsaved changes.
    close_armed: Option<TabId>,
}

impl App {
    pub fn new(cc: &CreationContext<'_>) -> Self {
        let runtime = Runtime::new(cc.egui_ctx.clone());
        let profiles = connections::load_profiles().unwrap_or_default();
        let theme = ThemeMode::Dark;
        theme::install_fonts(&cc.egui_ctx);
        theme::install(&cc.egui_ctx, theme);
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
            theme,
            dump_dialog: None,
            import_dialog: None,
            export_dialog: None,
            dbml_file_dialog: egui_file_dialog::FileDialog::new(),
            close_armed: None,
        };
        // Probe Claude auth status at startup so the panel populates lazily.
        app.send(Pending::AuthStatus, |req| Command::AuthStatus { req });
        app
    }

    fn send(&mut self, op: Pending, cmd_builder: impl FnOnce(RequestId) -> Command) {
        let req = RequestId::new_v4();
        self.pending.insert(req, op);
        self.runtime.send(cmd_builder(req));
    }

    fn find_active(&self, profile_id: Uuid) -> Option<&ActiveConnection> {
        self.active.iter().find(|a| a.profile_id == profile_id)
    }

    fn find_active_mut(&mut self, profile_id: Uuid) -> Option<&mut ActiveConnection> {
        self.active.iter_mut().find(|a| a.profile_id == profile_id)
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
        }
    }

    fn start_connect(&mut self, profile_id: Uuid) {
        if self.active.iter().any(|a| a.profile_id == profile_id) {
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
        let params = profile.to_connect_params(password);
        self.send(Pending::Connect(profile_id), move |req| Command::Connect { req, params });
    }

    fn apply_event(&mut self, event: Event) {
        match event {
            Event::Connected { req, conn } => {
                let Some(op) = self.pending.remove(&req) else { return };
                match op {
                    Pending::Connect(profile_id) => {
                        if let Some(profile) = self.profiles.iter().find(|p| p.id == profile_id) {
                            self.active.push(ActiveConnection {
                                profile_id,
                                conn_id: conn,
                                name: if profile.name.is_empty() {
                                    format!("{}@{}", profile.username, profile.host)
                                } else {
                                    profile.name.clone()
                                },
                                kind: profile.kind,
                                schemas: Vec::new(),
                                schemas_loaded: false,
                                schema_cache: None,
                            });
                            self.status = Some("Connected.".into());
                            self.send(Pending::ListSchemas(profile_id), move |req| Command::ListSchemas {
                                req,
                                conn,
                            });
                            self.send(Pending::FetchDbMeta(profile_id), move |req| {
                                Command::FetchDbMeta { req, conn }
                            });
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
            Event::Schemas { req, conn: _, schemas } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::ListSchemas(profile_id) = op {
                    if let Some(ac) = self.find_active_mut(profile_id) {
                        ac.schemas = schemas
                            .into_iter()
                            .map(|name| SchemaNode { name, expanded: false, tables: None })
                            .collect();
                        ac.schemas_loaded = true;
                    }
                }
            }
            Event::Tables { req, conn: _, schema, tables } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::ListTables(profile_id) = op {
                    if let Some(ac) = self.find_active_mut(profile_id) {
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
            Event::QueryResult { req, result } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::Query(tab_id) = op {
                    if let Some(Tab::Query(t)) = self.find_tab_mut(tab_id) {
                        let info = match (result.rows_affected, result.columns.len()) {
                            (Some(n), _) => format!("{n} row(s) affected"),
                            (None, _) => format!("{} row(s)", result.rows.len()),
                        };
                        t.result = Some(result);
                        t.status = TabStatus::Info(info);
                    }
                }
            }
            Event::TableRows { req, conn: _, schema: _, table: _, result } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::TableRows(tab_id) = op {
                    if let Some(Tab::TableEditor(t)) = self.find_tab_mut(tab_id) {
                        t.rows = Some(result);
                        t.status = TabStatus::Idle;
                    }
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
            }
            Event::ChangesApplied { req } => {
                let Some(op) = self.pending.remove(&req) else { return };
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
                let Pending::ApplyDdl(tab_id) = op else { return };
                let mut refresh: Option<(Uuid, String, String)> = None;
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
                            refresh = Some((t.profile_id, t.schema.clone(), t.table.clone()));
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
                                refresh = Some((t.profile_id, t.schema.clone(), t.table.clone()));
                            } else {
                                // Nothing changed on the DB — keep the staged
                                // work so the user can fix and retry.
                                t.status = TabStatus::Error(e);
                            }
                        }
                    }
                }
                if let Some((profile_id, schema, table)) = refresh {
                    let conn = self.find_active(profile_id).map(|a| a.conn_id);
                    if let Some(conn) = conn {
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
                        self.send(Pending::ListTables(profile_id), move |req| {
                            Command::ListTables { req, conn, schema }
                        });
                        // Keep autocomplete metadata in sync too.
                        self.send(Pending::FetchDbMeta(profile_id), move |req| {
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
            Event::DbMeta { req, conn: _, meta } => {
                let Some(op) = self.pending.remove(&req) else { return };
                if let Pending::FetchDbMeta(profile_id) = op {
                    if let Some(ac) = self.find_active_mut(profile_id) {
                        ac.schema_cache = Some(std::sync::Arc::new(
                            crate::sql_complete::SchemaCache::new(meta),
                        ));
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
                    // First generation: create the connection-owned document.
                    Pending::DumpDbml(profile_id) => {
                        let name = self
                            .find_active(profile_id)
                            .map(|a| a.name.clone())
                            .unwrap_or_else(|| "database".to_owned());
                        if tables.is_empty() {
                            self.status =
                                Some(format!("View as DBML: no tables found in {name}{skipped}"));
                            return;
                        }
                        let text = crate::dbml::generate_document(&tables, &name);
                        let path = self.connection_dbml_path(profile_id, &name);
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
                        self.open_diagram_tab(path, Some(profile_id));
                        self.status = Some(format!(
                            "Generated DBML for {} table(s) from {name}{skipped}",
                            tables.len()
                        ));
                    }
                    // Refresh: merge into the open tab's text; the user
                    // persists it with Ctrl+S after reviewing.
                    Pending::RefreshDbml(tab_id) => {
                        let name = self
                            .find_tab_mut(tab_id)
                            .and_then(|t| match t {
                                Tab::Diagram(d) => d.profile_id,
                                _ => None,
                            })
                            .and_then(|pid| self.find_active(pid).map(|a| a.name.clone()))
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
                        if let Some(Tab::Query(t)) = self.find_tab_mut(tab_id) {
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
                    _ => {
                        self.status = Some(format!("Error: {error}"));
                    }
                }
            }
        }
    }

    fn reload_table_tab(&mut self, tab_id: TabId, msg: &str) {
        let (conn_id, schema, table, offset, limit) = match self.find_tab_mut(tab_id) {
            Some(Tab::TableEditor(t)) => {
                t.status = TabStatus::Info(msg.to_string());
                (t.conn_id, t.schema.clone(), t.table.clone(), t.offset, t.limit)
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
        });
    }

    fn open_query_tab(&mut self, profile_id: Uuid, initial_sql: String) {
        let Some(ac) = self.find_active(profile_id) else { return };
        let id = Uuid::new_v4();
        let title = format!("Query — {}", ac.name);
        let conn_id = ac.conn_id;
        self.tabs.push(Tab::Query(QueryTab {
            id,
            title,
            profile_id,
            conn_id,
            sql: initial_sql,
            result: None,
            status: TabStatus::Idle,
            completion: Default::default(),
            last_sql: String::new(),
            last_cursor_char: 0,
            force_reopen: false,
            pending_cursor: None,
        }));
        self.active_tab = Some(id);
    }

    fn open_diagram_tab(&mut self, path: std::path::PathBuf, profile_id: Option<Uuid>) {
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
        tab.profile_id = profile_id;
        self.tabs.push(Tab::Diagram(tab));
        self.active_tab = Some(id);
    }

    fn dbml_dir() -> std::path::PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("dbTool")
            .join("dbml")
    }

    /// The connection-owned DBML document. Identified by a short id suffix so
    /// it survives connection renames; the readable part is just for humans.
    fn connection_dbml_path(&self, profile_id: Uuid, name: &str) -> std::path::PathBuf {
        let short = &profile_id.simple().to_string()[..8];
        let dir = Self::dbml_dir();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let suffix = format!("-{short}.dbml");
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().ends_with(&suffix) {
                    return e.path();
                }
            }
        }
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        dir.join(format!("{safe}-{short}.dbml"))
    }

    fn refresh_diagram_from_db(&mut self, tab_id: TabId) {
        let Some(Tab::Diagram(d)) = self.find_tab_mut(tab_id) else { return };
        let Some(profile_id) = d.profile_id else { return };
        let Some(ac) = self.find_active(profile_id) else {
            self.status = Some(
                "Refresh needs the owning connection to be connected".to_owned(),
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

    fn open_table_tab(&mut self, profile_id: Uuid, schema: String, table: String) {
        self.open_table_tab_view(profile_id, schema, table, EditorView::Data);
    }

    fn open_table_tab_view(
        &mut self,
        profile_id: Uuid,
        schema: String,
        table: String,
        view: EditorView,
    ) {
        let Some(ac) = self.find_active(profile_id) else { return };
        let db_kind = ac.kind;
        let conn_id = ac.conn_id;
        // Focus an existing tab if one already shows this table.
        if let Some(existing_id) = self
            .tabs
            .iter()
            .find(|t| match t {
                Tab::TableEditor(te) => {
                    te.profile_id == profile_id && te.schema == schema && te.table == table
                }
                _ => false,
            })
            .map(|t| t.id())
        {
            self.active_tab = Some(existing_id);
            if view == EditorView::Structure {
                if let Some(Tab::TableEditor(te)) = self.find_tab_mut(existing_id) {
                    te.view = EditorView::Structure;
                }
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
        self.send(Pending::TableRows(id), move |req| Command::FetchTableRows {
            req,
            conn: conn_id,
            schema: schema_r,
            table: table_r,
            limit: 1000,
            offset: 0,
        });
    }

    fn open_new_table_tab(&mut self, profile_id: Uuid, schema: String) {
        let Some(ac) = self.find_active(profile_id) else { return };
        let id = Uuid::new_v4();
        let conn_id = ac.conn_id;
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
                self.runtime.send(Command::AiApprove { session, tool_use_id, approved });
            }
            AiPanelAction::Cancel { session } => {
                self.runtime.send(Command::AiCancel { session });
            }
            AiPanelAction::OpenSqlInEditor { profile_id, sql } => {
                let target = profile_id
                    .filter(|pid| self.find_active(*pid).is_some())
                    .or_else(|| self.active.first().map(|a| a.profile_id));
                match target {
                    Some(pid) => self.open_query_tab(pid, sql),
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
        let conn_id = {
            let Some(ac) = self.find_active(profile_id) else { return };
            ac.conn_id
        };
        self.runtime.send(Command::Disconnect { conn: conn_id });
        self.active.retain(|a| a.profile_id != profile_id);
        self.tabs.retain(|t| t.profile_id() != Some(profile_id));
        if let Some(id) = self.active_tab {
            if !self.tabs.iter().any(|t| t.id() == id) {
                self.active_tab = None;
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Drain events.
        let events = self.runtime.drain_events();
        for e in events {
            self.apply_event(e);
        }

        // 2. Left sidebar: connection list + schema trees.
        let mut pending_actions: Vec<Box<dyn FnOnce(&mut Self)>> = Vec::new();

        // Top menu bar (title + AI agent toggle + account).
        egui::TopBottomPanel::top("topbar")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(10.0, 6.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("dbTool").strong().size(15.0).color(theme::ACCENT));
                    ui.separator();

                    let ai_label = if self.ai_panel.open { "💬 Hide AI" } else { "💬 AI assistant" };
                    if ui
                        .selectable_label(self.ai_panel.open, ai_label)
                        .on_hover_text("Toggle the AI assistant panel")
                        .clicked()
                    {
                        self.ai_panel.open = !self.ai_panel.open;
                    }

                    if ui
                        .button("◈ Open .dbml…")
                        .on_hover_text("Open a DBML file as an editable diagram")
                        .clicked()
                    {
                        self.dbml_file_dialog.select_file();
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let signed_in = matches!(self.auth.status.as_ref(), Some(s) if s.logged_in);
                        let account_label = match self.auth.status.as_ref() {
                            Some(s) if s.logged_in => {
                                let who = s.email.as_deref().unwrap_or("signed in");
                                format!("● {who}")
                            }
                            Some(_) => "○ Signed out".to_string(),
                            None => "Account…".to_string(),
                        };
                        let account_color = if signed_in {
                            theme::success_color(ui)
                        } else {
                            ui.visuals().weak_text_color()
                        };
                        if ui
                            .button(egui::RichText::new(account_label).color(account_color))
                            .on_hover_text("Claude account for the AI assistant")
                            .clicked()
                        {
                            self.auth.open = true;
                        }
                        if ui
                            .button(self.theme.icon())
                            .on_hover_text("Toggle light / dark theme")
                            .clicked()
                        {
                            self.theme = self.theme.toggled();
                            theme::install(ctx, self.theme);
                        }
                    });
                });
            });

        // AI panel (right side, when open).
        let ai_action = crate::ui::ai_panel::draw(ctx, &mut self.ai_panel, &self.active);
        pending_actions.push(Box::new(move |app: &mut Self| app.handle_ai_action(ai_action)));

        // Auth dialog (modal).
        let auth_action = crate::ui::auth_dialog::draw(ctx, &mut self.auth);
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

        // DBML file picker.
        self.dbml_file_dialog.update(ctx);
        if let Some(picked) = self.dbml_file_dialog.take_selected() {
            self.open_diagram_tab(picked, None);
        }

        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                let active_ids: HashSet<Uuid> =
                    self.active.iter().map(|a| a.profile_id).collect();
                let mgr_action = crate::connections::manager_ui::draw_connection_list(
                    ui,
                    &mut self.manager,
                    &self.profiles,
                    &active_ids,
                );
                pending_actions.push(Box::new(move |app: &mut Self| app.handle_manager_action(mgr_action)));

                ui.separator();

                let tree_action = crate::ui::schema_tree::draw(ui, &mut self.active);
                pending_actions.push(Box::new(move |app: &mut Self| app.apply_tree_action(tree_action)));
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

            match &mut self.tabs[pos] {
                Tab::Query(q) => {
                    let profile_id = q.profile_id;
                    let ac = self.active.iter().find(|a| a.profile_id == profile_id);
                    let schema_cache = ac.and_then(|a| a.schema_cache.as_ref());
                    let dialect = ac.map(|a| a.kind).unwrap_or(crate::db::DbKind::Postgres);
                    let action = crate::ui::query_tab::draw(ui, q, schema_cache, dialect);
                    match action {
                        crate::ui::query_tab::QueryTabAction::None => {}
                        crate::ui::query_tab::QueryTabAction::Run => {
                            let sql = q.sql.clone();
                            let conn = q.conn_id;
                            let tab_id = q.id;
                            q.status = TabStatus::Running("running…".into());
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.send(Pending::Query(tab_id), move |req| Command::Query {
                                    req,
                                    conn,
                                    sql,
                                });
                            }));
                        }
                        crate::ui::query_tab::QueryTabAction::Export => {
                            let tab_id = q.id;
                            let profile_id = q.profile_id;
                            pending_actions.push(Box::new(move |app: &mut Self| {
                                app.open_export_dialog(
                                    profile_id,
                                    ExportSource::QueryResult { tab_id },
                                    "query-result".to_string(),
                                );
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
                            let action = crate::ui::table_editor::draw(ui, t);
                            handle_table_editor_action(action, t, &mut pending_actions);
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
                    match crate::ui::diagram_tab::draw(ui, d) {
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
            }
        });

        // 5. Edit dialog (modal-ish).
        if self.manager.editing.is_some() {
            let action = crate::connections::manager_ui::draw_edit_dialog(ctx, &mut self.manager);
            pending_actions.push(Box::new(move |app: &mut Self| app.handle_manager_action(action)));
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
                app.send(Pending::ApplyDdl(tab_id), move |req| Command::ApplyDdl {
                    req,
                    conn,
                    statements,
                });
            }));
        }
        A::OpenSql { sql } => {
            let profile_id = tab.profile_id;
            pending_actions.push(Box::new(move |app: &mut App| {
                app.open_query_tab(profile_id, sql);
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
            tab.status = TabStatus::Running("refreshing…".into());
            pending_actions.push(Box::new(move |app: &mut App| {
                app.send(Pending::TableRows(tab_id), move |req| Command::FetchTableRows {
                    req,
                    conn,
                    schema,
                    table,
                    limit,
                    offset,
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
            tab.status = TabStatus::Running("loading…".into());
            pending_actions.push(Box::new(move |app: &mut App| {
                app.send(Pending::TableRows(tab_id), move |req| Command::FetchTableRows {
                    req,
                    conn,
                    schema,
                    table,
                    limit,
                    offset,
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
            tab.status = TabStatus::Running("loading…".into());
            pending_actions.push(Box::new(move |app: &mut App| {
                app.send(Pending::TableRows(tab_id), move |req| Command::FetchTableRows {
                    req,
                    conn,
                    schema,
                    table,
                    limit,
                    offset,
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
    }
}

impl App {
    fn apply_tree_action(&mut self, action: crate::ui::schema_tree::TreeAction) {
        use crate::ui::schema_tree::TreeAction as T;
        match action {
            T::None => {}
            T::ExpandSchemas(profile_id) => {
                let Some(ac) = self.find_active(profile_id) else { return };
                let conn = ac.conn_id;
                self.send(Pending::ListSchemas(profile_id), move |req| Command::ListSchemas {
                    req,
                    conn,
                });
            }
            T::ExpandTables(profile_id, schema) => {
                let Some(ac) = self.find_active(profile_id) else { return };
                let conn = ac.conn_id;
                self.send(Pending::ListTables(profile_id), move |req| {
                    Command::ListTables { req, conn, schema }
                });
            }
            T::OpenTable(profile_id, schema, table) => {
                self.open_table_tab(profile_id, schema, table);
            }
            T::ModifyTable(profile_id, schema, table) => {
                self.open_table_tab_view(profile_id, schema, table, EditorView::Structure);
            }
            T::NewTable(profile_id, schema) => {
                self.open_new_table_tab(profile_id, schema);
            }
            T::OpenQueryTab(profile_id) => {
                self.open_query_tab(profile_id, String::new());
            }
            T::Disconnect(profile_id) => {
                self.disconnect_profile(profile_id);
            }
            T::ViewAsDbml(profile_id) => {
                let Some(ac) = self.find_active(profile_id) else { return };
                let conn = ac.conn_id;
                let name = ac.name.clone();
                // The connection's document persists across sessions: open it
                // if it exists (keeping the user's groups and edits); only
                // introspect when there is nothing yet.
                let path = self.connection_dbml_path(profile_id, &name);
                if path.exists() {
                    self.open_diagram_tab(path, Some(profile_id));
                    return;
                }
                self.status = Some(format!("Introspecting {name} for DBML…"));
                self.send(Pending::DumpDbml(profile_id), move |req| Command::DumpDbml {
                    req,
                    conn,
                });
            }
            T::DumpDdl(profile_id, schema) => {
                let Some(ac) = self.find_active(profile_id) else { return };
                let conn_name = ac.name.clone();
                let default_dir = format!("~/dbtool-ddl/{}", safe_dir_name(&conn_name));
                self.dump_dialog = Some(DumpState {
                    profile_id,
                    conn_name,
                    schema,
                    dir: default_dir,
                    running: false,
                    result: None,
                    error: None,
                });
            }
            T::ImportInto(profile_id, schema, table) => {
                if self.find_active(profile_id).is_none() {
                    return;
                }
                self.import_dialog = Some(ImportState {
                    profile_id,
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
            T::ExportFrom(profile_id, schema, table) => {
                if self.find_active(profile_id).is_none() {
                    return;
                }
                let file_stem = table.clone();
                self.open_export_dialog(
                    profile_id,
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
                let Some(ac) = self.active.iter().find(|a| a.profile_id == d.profile_id) else {
                    d.error = Some("Connection is no longer active.".into());
                    return;
                };
                let conn = ac.conn_id;
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

    fn open_export_dialog(&mut self, profile_id: Uuid, source: ExportSource, file_stem: String) {
        self.export_dialog = Some(ExportState {
            profile_id,
            source,
            path: format!("~/{}.csv", safe_dir_name(&file_stem)),
            delimiter: ",".to_string(),
            include_header: true,
            running: false,
            progress_rows: 0,
            error: None,
            file_dialog: egui_file_dialog::FileDialog::new(),
        });
    }

    fn handle_export_action(&mut self, action: ExportAction) {
        match action {
            ExportAction::None => {}
            ExportAction::Close => self.export_dialog = None,
            ExportAction::Start => {
                let (delimiter, path, include_header, profile_id, source) = {
                    let Some(d) = self.export_dialog.as_ref() else { return };
                    let Some(delimiter) = d.delimiter_byte() else { return };
                    (
                        delimiter,
                        d.path.trim().to_string(),
                        d.include_header,
                        d.profile_id,
                        d.source.clone(),
                    )
                };
                let options = crate::csv_export::ExportOptions { delimiter, include_header };
                match source {
                    ExportSource::Table { schema, table } => {
                        let Some(ac) = self.active.iter().find(|a| a.profile_id == profile_id)
                        else {
                            if let Some(d) = self.export_dialog.as_mut() {
                                d.error = Some("Connection is no longer active.".into());
                            }
                            return;
                        };
                        let conn = ac.conn_id;
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
                            Some(Tab::Query(q)) => q.result.clone(),
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
                let Some(d) = self.import_dialog.as_mut() else { return };
                let Some(delimiter) = d.delimiter_byte() else { return };
                let Some(ac) = self.active.iter().find(|a| a.profile_id == d.profile_id) else {
                    d.error = Some("Connection is no longer active.".into());
                    return;
                };
                let conn = ac.conn_id;
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
