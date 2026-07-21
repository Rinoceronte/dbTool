//! DBML diagram tab: text editor pane + toolbar; the canvas lives in
//! `diagram_canvas`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::dbml::{DbmlError, DiagramModel};
use crate::ui::TabId;

pub const LAYOUT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TablePos {
    pub x: f32,
    pub y: f32,
    #[serde(default)]
    pub collapsed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagramLayout {
    pub version: u32,
    pub pan: (f32, f32),
    pub zoom: f32,
    pub editor_open: bool,
    pub tables: BTreeMap<String, TablePos>,
}

impl Default for DiagramLayout {
    fn default() -> Self {
        Self {
            version: LAYOUT_VERSION,
            pan: (0.0, 0.0),
            zoom: 1.0,
            editor_open: true,
            tables: BTreeMap::new(),
        }
    }
}

impl DiagramLayout {
    pub fn sidecar_path(dbml_path: &Path) -> PathBuf {
        let mut s = dbml_path.as_os_str().to_owned();
        s.push(".layout.json");
        PathBuf::from(s)
    }

    pub fn load(dbml_path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(Self::sidecar_path(dbml_path)).ok()?;
        let layout: Self = serde_json::from_str(&text).ok()?;
        (layout.version == LAYOUT_VERSION).then_some(layout)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    Node(usize),
    Group(usize),
    Pan,
}

pub struct DiagramTab {
    pub id: TabId,
    /// Set when this diagram is owned by one database of a connection
    /// ("View as DBML"): (profile, database name). Enables the
    /// Refresh-from-database action.
    pub db_source: Option<(crate::ui::ProfileId, String)>,
    pub path: PathBuf,
    pub text: String,
    pub saved_text: String,
    pub file_mtime: Option<SystemTime>,
    /// Last successfully parsed model; kept while the text has errors.
    pub model: Option<DiagramModel>,
    pub parse_error: Option<DbmlError>,
    /// Re-parse guard (same idiom as QueryTab::last_sql).
    pub last_parsed: String,
    pub layout: DiagramLayout,
    pub layout_dirty: bool,
    pub layout_dirty_at: f64,
    /// World-space node sizes, index-aligned with model.tables.
    pub node_sizes: Vec<egui::Vec2>,
    pub sizes_stale: bool,
    pub drag: Option<DragKind>,
    pub selected: Option<usize>,
    pub want_fit: bool,
    pub want_reset_zoom: bool,
    /// Status text produced inside the canvas (e.g. sidecar write errors),
    /// drained by draw() into a DiagramAction.
    pub pending_status: Option<String>,
}

impl DiagramTab {
    pub fn open(id: TabId, path: PathBuf, text: String, mtime: Option<SystemTime>) -> Self {
        let (model, parse_error) = match crate::dbml::parse(&text) {
            Ok(m) => (Some(m), None),
            Err(e) => (None, Some(e)),
        };
        let stored = DiagramLayout::load(&path);
        let want_fit = stored.is_none();
        Self {
            id,
            db_source: None,
            path,
            saved_text: text.clone(),
            last_parsed: text.clone(),
            text,
            file_mtime: mtime,
            model,
            parse_error,
            layout: stored.unwrap_or_default(),
            layout_dirty: false,
            layout_dirty_at: 0.0,
            node_sizes: Vec::new(),
            sizes_stale: true,
            drag: None,
            selected: None,
            want_fit,
            want_reset_zoom: false,
            pending_status: None,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.text != self.saved_text
    }

    pub fn title(&self) -> String {
        let stem = self
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "diagram".to_owned());
        if self.is_dirty() { format!("{stem} ●") } else { stem }
    }

    /// Write the layout sidecar; failures land in `pending_status`, never fatal.
    pub fn save_layout(&mut self) {
        self.layout_dirty = false;
        let path = DiagramLayout::sidecar_path(&self.path);
        let json = match serde_json::to_string_pretty(&self.layout) {
            Ok(j) => j,
            Err(e) => {
                self.pending_status = Some(format!("Layout serialize failed: {e}"));
                return;
            }
        };
        if let Err(e) = std::fs::write(&path, json) {
            self.pending_status = Some(format!("Could not write {}: {e}", path.display()));
        }
    }

    pub fn mark_layout_dirty(&mut self, now: f64) {
        self.layout_dirty = true;
        self.layout_dirty_at = now;
    }
}

/// TableGroup blocks suggested for tables not already in a group; None when
/// nothing clusters.
fn suggest_groups_block(tab: &DiagramTab) -> Option<String> {
    let m = tab.model.as_ref()?;
    let loose: Vec<usize> =
        (0..m.tables.len()).filter(|&i| m.tables[i].group.is_none()).collect();
    let names: Vec<String> = loose
        .iter()
        .map(|&i| crate::dbml::bare_table_name(&m.tables[i].key).to_owned())
        .collect();
    let index_of: HashMap<usize, usize> =
        loose.iter().enumerate().map(|(li, &i)| (i, li)).collect();
    let edges: Vec<(usize, usize)> = m
        .refs
        .iter()
        .filter_map(|r| Some((*index_of.get(&r.from.0)?, *index_of.get(&r.to.0)?)))
        .collect();
    let groups = crate::dbml::suggest_groups(&names, &edges);
    if groups.is_empty() {
        return None;
    }
    let mut taken: HashSet<String> = m.groups.iter().map(|g| g.name.clone()).collect();
    Some(crate::dbml::render_table_groups(
        &groups,
        |li| crate::dbml::table_ref_from_key(&m.tables[loose[li]].key),
        &mut taken,
    ))
}

pub enum DiagramAction {
    None,
    SaveFile,
    /// Re-introspect the owning connection and merge into the source text.
    RefreshFromDb,
    Status(String),
}

pub fn draw(ui: &mut egui::Ui, tab: &mut DiagramTab, line_numbers: bool) -> DiagramAction {
    let mut action = DiagramAction::None;

    // Ctrl+S anywhere in the tab saves the .dbml file.
    if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
        action = DiagramAction::SaveFile;
    }

    // Toolbar.
    ui.horizontal(|ui| {
        if ui
            .selectable_label(tab.layout.editor_open, "⌨ DBML")
            .on_hover_text("Toggle the DBML source pane")
            .clicked()
        {
            tab.layout.editor_open = !tab.layout.editor_open;
            tab.save_layout();
        }
        ui.separator();
        if ui.button("Fit").on_hover_text("Zoom to fit the whole diagram").clicked() {
            tab.want_fit = true;
        }
        if ui.button("1:1").on_hover_text("Reset zoom to 100%").clicked() {
            tab.want_reset_zoom = true;
        }
        if ui
            .button("Auto layout")
            .on_hover_text("Discard saved positions and re-place all tables")
            .clicked()
        {
            tab.layout.tables.clear();
            tab.want_fit = true;
            tab.save_layout();
        }
        if ui
            .button("Suggest groups")
            .on_hover_text(
                "Append TableGroup suggestions for tables not yet in a group, \
                 clustered by name prefixes and foreign keys. Plain text — \
                 edit or delete the blocks freely; nothing is written until \
                 you Ctrl+S.",
            )
            .clicked()
        {
            match suggest_groups_block(tab) {
                Some(block) => {
                    if !tab.text.ends_with('\n') {
                        tab.text.push('\n');
                    }
                    tab.text.push('\n');
                    tab.text.push_str(&block);
                }
                None => {
                    action = DiagramAction::Status(
                        "No group suggestions — ungrouped tables don't cluster by \
                         name or foreign keys"
                            .into(),
                    );
                }
            }
        }
        if tab.db_source.is_some() {
            ui.separator();
            if ui
                .button("⟳ Refresh from database")
                .on_hover_text(
                    "Re-introspect the connection and update the generated region. \
                     Your TableGroups and edits below the marker are preserved; \
                     nothing is written until you Ctrl+S.",
                )
                .clicked()
            {
                action = DiagramAction::RefreshFromDb;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match (&tab.parse_error, &tab.model) {
                (Some(e), _) => {
                    let line = e.line.map(|l| format!("line {l}: ")).unwrap_or_default();
                    ui.colored_label(
                        crate::ui::theme::delete_fg(ui),
                        format!("✗ {line}{}", e.message),
                    );
                }
                (None, Some(m)) => {
                    ui.weak(format!(
                        "{} tables · {} refs · {} groups",
                        m.tables.len(),
                        m.refs.len(),
                        m.groups.len()
                    ));
                }
                (None, None) => {}
            }
        });
    });
    ui.separator();

    // DBML source pane.
    if tab.layout.editor_open {
        egui::SidePanel::left(egui::Id::new(("dbml_editor", tab.id)))
            .resizable(true)
            .default_width(380.0)
            .show_inside(ui, |ui| {
                let panel_width = ui.available_width();
                egui::ScrollArea::both()
                    .id_source(("dbml_editor_scroll", tab.id))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.horizontal_top(|ui| {
                            let gutter_width = if line_numbers {
                                crate::ui::line_number_gutter(ui, &tab.text)
                            } else {
                                0.0
                            };
                            // No-wrap layouter: keeps source lines and visual
                            // rows 1:1 so the gutter stays aligned; long lines
                            // scroll horizontally instead.
                            let mut layouter = |ui: &egui::Ui, text: &str, _wrap_width: f32| {
                                crate::ui::layout_code_no_wrap(ui, text)
                            };
                            let editor_width = (panel_width
                                - gutter_width
                                - ui.spacing().item_spacing.x * 2.0)
                                .max(100.0);
                            ui.add(
                                egui::TextEdit::multiline(&mut tab.text)
                                    .code_editor()
                                    .desired_width(editor_width)
                                    .desired_rows(40)
                                    .layouter(&mut layouter),
                            );
                        });
                    });
            });
    }

    // Live re-parse (per keystroke; DBML is KB-scale, pest handles it in <1ms).
    if tab.text != tab.last_parsed {
        tab.last_parsed = tab.text.clone();
        match crate::dbml::parse(&tab.text) {
            Ok(m) => {
                tab.model = Some(m);
                tab.parse_error = None;
                tab.sizes_stale = true;
                tab.selected = None;
            }
            Err(e) => tab.parse_error = Some(e),
        }
    }

    // Canvas fills the remainder.
    egui::CentralPanel::default()
        .frame(egui::Frame::none())
        .show_inside(ui, |ui| {
            crate::ui::diagram_canvas::draw(ui, tab);
        });

    if let Some(s) = tab.pending_status.take()
        && matches!(action, DiagramAction::None)
    {
        action = DiagramAction::Status(s);
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_path_appends_suffix() {
        let p = DiagramLayout::sidecar_path(Path::new("/x/schema.dbml"));
        assert_eq!(p, PathBuf::from("/x/schema.dbml.layout.json"));
    }

    #[test]
    fn layout_round_trips() {
        let mut layout = DiagramLayout::default();
        layout.tables.insert(
            "public.users".into(),
            TablePos { x: 12.5, y: -4.0, collapsed: true },
        );
        layout.pan = (100.0, 50.0);
        layout.zoom = 0.8;
        let json = serde_json::to_string_pretty(&layout).unwrap();
        let back: DiagramLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, LAYOUT_VERSION);
        assert_eq!(back.pan, (100.0, 50.0));
        assert!(back.tables["public.users"].collapsed);
    }
}
