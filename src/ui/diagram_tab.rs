//! DBML diagram tab: text editor pane + toolbar; the canvas lives in
//! `diagram_canvas`.

use std::collections::BTreeMap;
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

pub enum DiagramAction {
    None,
    SaveFile,
    Status(String),
}

pub fn draw(ui: &mut egui::Ui, tab: &mut DiagramTab) -> DiagramAction {
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
                egui::ScrollArea::vertical()
                    .id_source(("dbml_editor_scroll", tab.id))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut tab.text)
                                .code_editor()
                                .desired_width(f32::INFINITY)
                                .desired_rows(40),
                        );
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
