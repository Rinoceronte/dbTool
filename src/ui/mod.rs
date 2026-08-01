pub mod ai_panel;
pub mod auth_dialog;
pub mod compare_tab;
pub mod datasync_tab;
pub mod completion_popup;
pub mod diagram_canvas;
pub mod diagram_svg;
pub mod diagram_tab;
pub mod import_export;
pub mod query_tab;
pub mod results_grid;
pub mod schema_tree;
pub mod sessions_tab;
pub mod settings_dialog;
pub mod table_editor;
pub mod table_structure;
pub mod theme;
pub mod transfer_tab;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;

use crate::db::structure::{ObjId, WorkingTable};
use crate::db::{ResultSet, RowChanges, TableSchema};
use crate::runtime::ConnectionId;
use crate::sql_complete::SchemaCache;

pub type ProfileId = Uuid;
pub type TabId = Uuid;

/// Layout code without wrapping — pair with a horizontal ScrollArea so long
/// lines scroll instead of desyncing a line-number gutter.
pub fn layout_code_no_wrap(ui: &egui::Ui, text: &str) -> Arc<egui::Galley> {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let color = ui.visuals().widgets.inactive.text_color();
    ui.fonts(|f| {
        f.layout_job(egui::text::LayoutJob::simple(
            text.to_owned(),
            font,
            color,
            f32::INFINITY,
        ))
    })
}

/// Keywords worth coloring that the completion list doesn't carry (statement
/// verbs the completer never suggests mid-query).
const HIGHLIGHT_EXTRA_KEYWORDS: &[&str] = &[
    "BEGIN", "COMMIT", "ROLLBACK", "TRANSACTION", "START", "CAST", "ASC", "DESC", "NULLS",
    "FIRST", "LAST", "COLUMN", "ADD", "CONSTRAINT", "UNIQUE", "CHECK", "IF", "REPLACE",
    "TEMPORARY", "TEMP", "FUNCTION", "PROCEDURE", "TRIGGER", "SEQUENCE", "RETURNS", "LANGUAGE",
    "GRANT", "REVOKE", "ANALYZE", "VACUUM", "CONFLICT", "DO", "NOTHING",
];

/// Layout SQL with syntax highlighting and optional find-match backgrounds.
/// No wrapping — pair with a horizontal ScrollArea (gutter alignment).
pub fn layout_sql(
    ui: &egui::Ui,
    text: &str,
    matches: &[(usize, usize)],
    current_match: usize,
) -> Arc<egui::Galley> {
    use crate::sql_complete::{keywords, lexer};
    use egui::text::{LayoutJob, LayoutSection, TextFormat};
    use egui::Color32;

    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let default_color = ui.visuals().widgets.inactive.text_color();
    if text.is_empty() {
        return layout_code_no_wrap(ui, text);
    }
    let dark = ui.visuals().dark_mode;
    let kw_color = if dark { Color32::from_rgb(97, 158, 232) } else { Color32::from_rgb(20, 80, 170) };
    let str_color = if dark { Color32::from_rgb(152, 195, 121) } else { Color32::from_rgb(80, 140, 60) };
    let num_color = if dark { Color32::from_rgb(209, 154, 102) } else { Color32::from_rgb(170, 90, 0) };
    let cmt_color = if dark { Color32::from_rgb(106, 120, 132) } else { Color32::from_rgb(140, 148, 158) };
    let qid_color = if dark { Color32::from_rgb(86, 182, 194) } else { Color32::from_rgb(0, 120, 135) };

    let n = text.chars().count();
    let mut colors = vec![default_color; n];
    for tok in lexer::tokenize(text) {
        let c = match tok.kind {
            lexer::TokKind::Comment => cmt_color,
            lexer::TokKind::String => str_color,
            lexer::TokKind::Number => num_color,
            lexer::TokKind::QuotedIdent => qid_color,
            lexer::TokKind::Word
                if keywords::is_keyword(&tok.raw)
                    || HIGHLIGHT_EXTRA_KEYWORDS
                        .iter()
                        .any(|k| k.eq_ignore_ascii_case(&tok.raw)) =>
            {
                kw_color
            }
            _ => continue,
        };
        colors[tok.start..tok.end.min(n)].fill(c);
    }

    let hl_bg = Color32::from_rgba_unmultiplied(235, 203, 80, 70);
    let cur_bg = Color32::from_rgba_unmultiplied(235, 160, 60, 140);
    let mut bgs = vec![Color32::TRANSPARENT; n];
    for (mi, &(cs, cl)) in matches.iter().enumerate() {
        let bg = if mi == current_match { cur_bg } else { hl_bg };
        bgs[cs.min(n)..(cs + cl).min(n)].fill(bg);
    }

    let mut job = LayoutJob { text: text.to_owned(), ..Default::default() };
    job.wrap.max_width = f32::INFINITY;
    let mut push = |byte_range: std::ops::Range<usize>, attr: (Color32, Color32)| {
        job.sections.push(LayoutSection {
            leading_space: 0.0,
            byte_range,
            format: TextFormat {
                font_id: font.clone(),
                color: attr.0,
                background: attr.1,
                ..Default::default()
            },
        });
    };
    // Coalesce per-char (color, background) runs into byte-ranged sections.
    let mut run_start = 0usize;
    let mut run_attr = (colors[0], bgs[0]);
    for (ci, (b, _)) in text.char_indices().enumerate() {
        let attr = (colors[ci], bgs[ci]);
        if attr != run_attr {
            push(run_start..b, run_attr);
            run_start = b;
            run_attr = attr;
        }
    }
    push(run_start..text.len(), run_attr);
    ui.fonts(|f| f.layout_job(job))
}

/// Right-aligned line-number gutter matching a monospace TextEdit's rows;
/// returns its width so callers can budget the editor's desired width.
pub fn line_number_gutter(ui: &mut egui::Ui, text: &str) -> f32 {
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let lines = text.split('\n').count();
    let digits = lines.to_string().len();
    let gutter: String = (1..=lines)
        .map(|i| format!("{i:>digits$}"))
        .collect::<Vec<_>>()
        .join("\n");
    let width = ui.fonts(|f| f.glyph_width(&font, '0')) * digits as f32;
    ui.vertical(|ui| {
        // Match TextEdit's inner top margin so gutter rows line up.
        ui.add_space(2.0);
        ui.add(
            egui::Label::new(egui::RichText::new(gutter).monospace().weak())
                .extend()
                .selectable(false),
        );
    });
    width
}

pub struct ActiveConnection {
    pub profile_id: ProfileId,
    pub conn_id: ConnectionId,
    pub name: String,
    pub kind: crate::db::DbKind,
    /// The database this pool is attached to.
    pub database: String,
    /// The connection opened from the profile itself (as opposed to an extra
    /// toggled-on database). Card-level actions target the primary.
    pub is_primary: bool,
    /// Server database list for the toggle picker; lazily fetched.
    pub server_databases: Option<Vec<String>>,
    pub schemas: Vec<SchemaNode>,
    pub schemas_loaded: bool,
    pub schema_cache: Option<Arc<SchemaCache>>,
}

pub struct SchemaNode {
    pub name: String,
    pub expanded: bool,
    pub tables: Option<Vec<crate::db::TableInfo>>,
    /// Functions, sequences, enums, triggers; loaded with the tables.
    pub objects: Option<crate::db::SchemaObjects>,
}

pub enum Tab {
    Query(QueryTab),
    TableEditor(TableEditorTab),
    Diagram(diagram_tab::DiagramTab),
    Compare(compare_tab::CompareTab),
    Sessions(SessionsTab),
    DataSync(datasync_tab::DataSyncTab),
    Transfer(transfer_tab::TransferTab),
}

impl Tab {
    pub fn id(&self) -> TabId {
        match self {
            Tab::Query(t) => t.id,
            Tab::TableEditor(t) => t.id,
            Tab::Diagram(t) => t.id,
            Tab::Compare(t) => t.id,
            Tab::Sessions(t) => t.id,
            Tab::DataSync(t) => t.id,
            Tab::Transfer(t) => t.id,
        }
    }
    pub fn title(&self) -> String {
        match self {
            Tab::Query(t) => t.title.clone(),
            Tab::TableEditor(t) if t.structure.is_new_table => {
                format!("{}.{} (new)", t.schema, t.table)
            }
            Tab::TableEditor(t) => format!("{}.{}", t.schema, t.table),
            Tab::Diagram(t) => t.title(),
            Tab::Compare(_) => "Compare structures".to_owned(),
            Tab::Sessions(t) => format!("Sessions — {}", t.conn_name),
            Tab::DataSync(_) => "Data sync".to_owned(),
            Tab::Transfer(_) => "Transfer data".to_owned(),
        }
    }
    /// The connection a tab belongs to; `None` for connection-agnostic tabs
    /// (diagrams, compares), which must survive disconnects.
    pub fn profile_id(&self) -> Option<ProfileId> {
        match self {
            Tab::Query(t) => Some(t.profile_id),
            Tab::TableEditor(t) => Some(t.profile_id),
            Tab::Sessions(t) => Some(t.profile_id),
            Tab::Diagram(_) | Tab::Compare(_) | Tab::DataSync(_) | Tab::Transfer(_) => None,
        }
    }
}

/// Active-sessions monitor tab; see [`sessions_tab`].
pub struct SessionsTab {
    pub id: TabId,
    pub profile_id: ProfileId,
    pub conn_id: ConnectionId,
    pub kind: crate::db::DbKind,
    pub conn_name: String,
    pub rows: Option<ResultSet>,
    pub status: TabStatus,
    pub auto_refresh: bool,
    /// egui input time of the last refresh send (auto-refresh pacing).
    pub last_refresh: f64,
}

pub struct QueryTab {
    pub id: TabId,
    pub title: String,
    pub profile_id: ProfileId,
    pub conn_id: ConnectionId,
    /// Database the connection is attached to; used to re-attach restored
    /// tabs when their profile reconnects.
    pub database: String,
    pub sql: String,
    /// One ResultSet per data-producing statement of the last run.
    pub results: Vec<ResultSet>,
    /// Which of `results` the grid shows.
    pub result_idx: usize,
    pub status: TabStatus,
    pub completion: completion_popup::CompletionPopupState,
    pub last_sql: String,
    pub last_cursor_char: usize,
    pub force_reopen: bool,
    pub pending_cursor: Option<usize>,
    /// Backing .sql file, when opened from / saved to disk.
    pub file_path: Option<std::path::PathBuf>,
    /// Request id of the in-flight run, for cancellation.
    pub running_req: Option<crate::runtime::RequestId>,
    /// Editor selection (chars), kept across the focus loss a button click
    /// causes — Run executes just this when present.
    pub selected_sql: Option<String>,
    /// The exact buffer text of the last full-buffer Run; server error
    /// positions only map back to the editor while the text still matches.
    pub last_run_sql: Option<String>,
    /// Select this char range in the editor next frame (find navigation,
    /// error jumps).
    pub pending_selection: Option<(usize, usize)>,
    /// Scroll the editor so this char offset is visible next frame.
    pub scroll_to_char: Option<usize>,
    // Find/replace bar.
    pub find_open: bool,
    pub find_text: String,
    pub replace_text: String,
    /// Index of the current match among all matches.
    pub find_index: usize,
    /// Focus the find field next frame.
    pub find_focus: bool,
    // Manual-commit (transaction) mode.
    /// Runs go through a dedicated transaction session until Commit/Rollback.
    pub manual_commit: bool,
    /// A manual transaction is open on the runtime side.
    pub tx_open: bool,
    /// A begin/commit/rollback is in flight.
    pub tx_busy: bool,
    /// Statements run inside the current transaction.
    pub tx_statements: usize,
    /// SQL waiting for the transaction to open (begin-then-run flow).
    pub pending_tx_sql: Option<String>,
    // Find-in-results bar (Ctrl+Shift+F, over the grid).
    pub grid_find_open: bool,
    pub grid_find_text: String,
    pub grid_find_index: usize,
    /// Focus the grid-find field next frame.
    pub grid_find_focus: bool,
    /// Cached matches: (needle_lower, result_idx) → cell coordinates.
    /// Cleared whenever `results` is replaced.
    pub grid_find_cache: Option<(String, usize, Vec<(usize, usize)>)>,
    /// In-memory sort of each result set (result_idx → sort keys, primary
    /// first). Cleared whenever `results` is replaced.
    pub grid_sort: std::collections::BTreeMap<usize, Vec<(usize, bool)>>,
    /// Set when the last run was a single-table SELECT with its PK in the
    /// projection — the grid then allows in-place cell edits.
    pub editable: Option<EditableMeta>,
    /// In-flight cell edit of the editable grid.
    pub grid_edit: Option<CellEdit>,
    /// The exact SQL of the last run (post EXPLAIN/selection rewriting) —
    /// what "Fetch all rows" re-executes without the row cap.
    pub last_executed_sql: Option<String>,
    /// In-progress "Insert row" form of an editable result (column → text).
    pub insert_draft: Option<std::collections::BTreeMap<String, String>>,
    /// Remembered parameter values (`:name` prompts), per name.
    pub param_values: std::collections::BTreeMap<String, String>,
    /// Visual EXPLAIN tree; shown instead of the grid until closed or rerun.
    pub plan: Option<crate::db::plan::PlanNode>,
    /// Re-run the last query automatically every N seconds.
    pub auto_refresh_secs: Option<u32>,
    /// When the in-flight run started (long-run desktop notification).
    pub run_started: Option<std::time::Instant>,
    /// When the last run finished (auto-refresh pacing).
    pub last_finish: Option<std::time::Instant>,
    /// Fraction of the tab's height the editor occupies (draggable splitter).
    pub editor_frac: f32,
}

/// Where an editable query result's rows live, and how to address them.
#[derive(Clone)]
pub struct EditableMeta {
    pub schema: String,
    pub table: String,
    pub pk: Vec<String>,
}

impl QueryTab {
    pub fn new(
        profile_id: ProfileId,
        conn_id: ConnectionId,
        database: String,
        title: String,
        sql: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            title,
            profile_id,
            conn_id,
            database,
            sql,
            results: Vec::new(),
            result_idx: 0,
            status: TabStatus::Idle,
            completion: Default::default(),
            last_sql: String::new(),
            last_cursor_char: 0,
            force_reopen: false,
            pending_cursor: None,
            file_path: None,
            running_req: None,
            selected_sql: None,
            last_run_sql: None,
            pending_selection: None,
            scroll_to_char: None,
            find_open: false,
            find_text: String::new(),
            replace_text: String::new(),
            find_index: 0,
            find_focus: false,
            manual_commit: false,
            tx_open: false,
            tx_busy: false,
            tx_statements: 0,
            pending_tx_sql: None,
            grid_find_open: false,
            grid_find_text: String::new(),
            grid_find_index: 0,
            grid_find_focus: false,
            grid_find_cache: None,
            grid_sort: Default::default(),
            editable: None,
            grid_edit: None,
            last_executed_sql: None,
            insert_draft: None,
            param_values: Default::default(),
            plan: None,
            auto_refresh_secs: None,
            run_started: None,
            last_finish: None,
            editor_frac: crate::session::default_editor_frac(),
        }
    }

    pub fn current_result(&self) -> Option<&ResultSet> {
        self.results.get(self.result_idx.min(self.results.len().saturating_sub(1)))
    }
}

/// Which face of a table tab is showing: the data grid or the structure editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorView {
    #[default]
    Data,
    Structure,
}

/// Selection inside the Structure view's object tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructSel {
    Table,
    Column(ObjId),
    Pk,
    Fk(ObjId),
    Index(ObjId),
    Check(ObjId),
}

#[derive(Default)]
pub struct StructureState {
    pub working: Option<WorkingTable>,
    pub selected: Option<StructSel>,
    pub is_new_table: bool,
    pub loading: bool,
    pub applying: bool,
    /// Destructive-change confirmation dialog.
    pub confirm_open: bool,
    pub confirm_ack: bool,
    /// Column whose name field should grab focus next frame.
    pub focus_col: Option<ObjId>,
    /// Highlighted entry in the type-autocomplete popup.
    pub type_ac_index: usize,
}

pub struct TableEditorTab {
    pub id: TabId,
    pub profile_id: ProfileId,
    pub conn_id: ConnectionId,
    pub db_kind: crate::db::DbKind,
    pub schema: String,
    pub table: String,
    pub view: EditorView,
    pub structure: StructureState,
    pub table_schema: Option<TableSchema>,
    pub rows: Option<ResultSet>,
    pub offset: i64,
    pub limit: i64,
    /// User-typed WHERE clause applied to the data page.
    pub filter: String,
    /// The filter text last applied (so the UI can show dirty state).
    pub applied_filter: String,
    /// Per-column quick filters (header row); identifiers are auto-quoted.
    pub col_filters: BTreeMap<String, String>,
    /// Active sorts, primary first: (column, descending).
    pub sort: Vec<(String, bool)>,
    /// COUNT(*) matching the applied filter, for "rows X–Y of N". None while
    /// unknown or being recounted.
    pub total_rows: Option<i64>,
    /// Outgoing FKs of this table (from connection metadata), for
    /// "go to referenced row" navigation.
    pub fks: Vec<crate::db::ForeignKey>,
    pub edit: Option<CellEdit>,
    pub insert_draft: Option<BTreeMap<String, String>>,
    pub selected_rows: BTreeSet<usize>,
    pub selection_anchor: Option<usize>,
    pub status: TabStatus,
    // Keyed by row index on the currently loaded page. Cleared whenever the page reloads.
    pub pending_edits: BTreeMap<usize, RowChanges>,
    pub pending_deletes: BTreeSet<usize>,
}

impl TableEditorTab {
    pub fn has_pending(&self) -> bool {
        !self.pending_edits.is_empty() || !self.pending_deletes.is_empty()
    }

    pub fn clear_pending(&mut self) {
        self.pending_edits.clear();
        self.pending_deletes.clear();
    }
}

/// An FK cell the pointer is dwelling on: the referenced row's coordinates
/// plus the cell's screen rect (popup anchor + dismissal test).
pub struct FkHoverCell {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub value: crate::db::Value,
    pub rect: egui::Rect,
}

pub struct CellEdit {
    pub row_index: usize,
    pub col_index: usize,
    pub text: String,
    pub focus_set: bool,
}

#[derive(Default, Clone)]
pub enum TabStatus {
    #[default]
    Idle,
    Running(String),
    Error(String),
    Info(String),
}
