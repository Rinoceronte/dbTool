pub mod ai_panel;
pub mod auth_dialog;
pub mod completion_popup;
pub mod import_export;
pub mod query_tab;
pub mod results_grid;
pub mod schema_tree;
pub mod table_editor;
pub mod table_structure;
pub mod theme;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use uuid::Uuid;

use crate::db::structure::{ObjId, WorkingTable};
use crate::db::{ResultSet, RowChanges, TableSchema};
use crate::runtime::ConnectionId;
use crate::sql_complete::SchemaCache;

pub type ProfileId = Uuid;
pub type TabId = Uuid;

pub struct ActiveConnection {
    pub profile_id: ProfileId,
    pub conn_id: ConnectionId,
    pub name: String,
    pub kind: crate::db::DbKind,
    pub schemas: Vec<SchemaNode>,
    pub schemas_loaded: bool,
    pub schema_cache: Option<Arc<SchemaCache>>,
}

pub struct SchemaNode {
    pub name: String,
    pub expanded: bool,
    pub tables: Option<Vec<crate::db::TableInfo>>,
}

pub enum Tab {
    Query(QueryTab),
    TableEditor(TableEditorTab),
}

impl Tab {
    pub fn id(&self) -> TabId {
        match self {
            Tab::Query(t) => t.id,
            Tab::TableEditor(t) => t.id,
        }
    }
    pub fn title(&self) -> String {
        match self {
            Tab::Query(t) => t.title.clone(),
            Tab::TableEditor(t) if t.structure.is_new_table => {
                format!("{}.{} (new)", t.schema, t.table)
            }
            Tab::TableEditor(t) => format!("{}.{}", t.schema, t.table),
        }
    }
    pub fn profile_id(&self) -> ProfileId {
        match self {
            Tab::Query(t) => t.profile_id,
            Tab::TableEditor(t) => t.profile_id,
        }
    }
}

pub struct QueryTab {
    pub id: TabId,
    pub title: String,
    pub profile_id: ProfileId,
    pub conn_id: ConnectionId,
    pub sql: String,
    pub result: Option<ResultSet>,
    pub status: TabStatus,
    pub completion: completion_popup::CompletionPopupState,
    pub last_sql: String,
    pub last_cursor_char: usize,
    pub force_reopen: bool,
    pub pending_cursor: Option<usize>,
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
