//! Structure-compare tab, DataGrip-migration style: origin on the left,
//! target on the right, a checkbox per difference deciding what goes into
//! the migration, and a live script preview underneath. The script always
//! makes the target match the origin; the swap button flips direction.

use std::collections::BTreeSet;

use crate::db::DbKind;
use crate::db::compare::{self, Change, DiffEntry, TableDiff, TableStatus};
use crate::db::structure::{Danger, DdlStatement, TableStructure};
use crate::runtime::ConnectionId;

use super::{ActiveConnection, TabId};

const CHECK_COL_W: f32 = 26.0;

pub struct CompareTab {
    pub id: TabId,
    /// Origin (left) — the shape to replicate.
    pub left: Option<ConnectionId>,
    /// Target (right) — where the migration would run.
    pub right: Option<ConnectionId>,
    pub left_snap: Option<(DbKind, Vec<TableStructure>)>,
    pub right_snap: Option<(DbKind, Vec<TableStructure>)>,
    pub loading_left: bool,
    pub loading_right: bool,
    pub error: Option<String>,
    pub diff: Option<Vec<TableDiff>>,
    pub show_identical: bool,
    pub ignore_schema: bool,
    pub filter: String,
    /// (table key, entry key or "" for whole-table create/drop) → include in
    /// the migration. Reset to everything on each fresh compare.
    pub included: BTreeSet<(String, String)>,
    /// Expanded differing tables in the tree.
    pub expanded: BTreeSet<String>,
    /// Cached migration batches (origin → target) for the current selection.
    pub script: Vec<(String, Vec<DdlStatement>)>,
    pub script_dirty: bool,
    /// Two-step confirmation when the script contains destructive statements.
    pub execute_armed: bool,
    pub applying: bool,
}

impl CompareTab {
    pub fn new(id: TabId, left: Option<ConnectionId>) -> Self {
        Self {
            id,
            left,
            right: None,
            left_snap: None,
            right_snap: None,
            loading_left: false,
            loading_right: false,
            error: None,
            diff: None,
            show_identical: false,
            ignore_schema: false,
            filter: String::new(),
            included: BTreeSet::new(),
            expanded: BTreeSet::new(),
            script: Vec::new(),
            script_dirty: false,
            execute_armed: false,
            applying: false,
        }
    }

    pub fn loading(&self) -> bool {
        self.loading_left || self.loading_right
    }

    pub fn recompute(&mut self) {
        self.diff = match (&self.left_snap, &self.right_snap) {
            (Some((_, l)), Some((_, r))) => {
                Some(compare::diff_snapshots(l, r, self.ignore_schema))
            }
            _ => None,
        };
        // Fresh compare: include every difference, expand the changed tables.
        self.included.clear();
        self.expanded.clear();
        if let Some(diff) = &self.diff {
            for d in diff {
                let key = compare::table_key(d);
                match d.status {
                    TableStatus::OnlyLeft | TableStatus::OnlyRight => {
                        self.included.insert((key, String::new()));
                    }
                    TableStatus::Different => {
                        for e in &d.entries {
                            self.included.insert((key.clone(), compare::entry_key(e)));
                        }
                        self.expanded.insert(key);
                    }
                    TableStatus::Identical => {}
                }
            }
        }
        self.script.clear();
        self.script_dirty = true;
        self.execute_armed = false;
    }

    fn swap_sides(&mut self) {
        std::mem::swap(&mut self.left, &mut self.right);
        std::mem::swap(&mut self.left_snap, &mut self.right_snap);
        std::mem::swap(&mut self.loading_left, &mut self.loading_right);
        self.recompute();
    }

    /// All checkbox keys for the current diff (for the master checkbox).
    fn all_keys(diff: &[TableDiff]) -> BTreeSet<(String, String)> {
        let mut out = BTreeSet::new();
        for d in diff {
            let key = compare::table_key(d);
            match d.status {
                TableStatus::OnlyLeft | TableStatus::OnlyRight => {
                    out.insert((key, String::new()));
                }
                TableStatus::Different => {
                    for e in &d.entries {
                        out.insert((key.clone(), compare::entry_key(e)));
                    }
                }
                TableStatus::Identical => {}
            }
        }
        out
    }
}

pub enum CompareAction {
    None,
    /// Introspect both selected connections.
    Snapshot,
    /// Run the reviewed script on its target connection.
    Apply { conn: ConnectionId, statements: Vec<String> },
    /// Hand the generated script to an editable query tab on the target.
    OpenScript { conn: ConnectionId, sql: String },
}

fn conn_name(active: &[ActiveConnection], conn: Option<ConnectionId>) -> String {
    match conn {
        None => "select…".to_owned(),
        Some(c) => active
            .iter()
            .find(|a| a.conn_id == c)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "(disconnected)".to_owned()),
    }
}

fn conn_kind(active: &[ActiveConnection], conn: Option<ConnectionId>) -> Option<DbKind> {
    conn.and_then(|c| active.iter().find(|a| a.conn_id == c)).map(|a| a.kind)
}

/// Combo of all active connections; returns true when the selection changed.
fn conn_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    sel: &mut Option<ConnectionId>,
    active: &[ActiveConnection],
) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_source(id)
        .width(170.0)
        .selected_text(conn_name(active, *sel))
        .show_ui(ui, |ui| {
            for a in active {
                if ui
                    .selectable_label(*sel == Some(a.conn_id), &a.name)
                    .clicked()
                    && *sel != Some(a.conn_id)
                {
                    *sel = Some(a.conn_id);
                    changed = true;
                }
            }
        });
    changed
}

fn status_color(ui: &egui::Ui, status: TableStatus) -> egui::Color32 {
    let dark = ui.visuals().dark_mode;
    match status {
        TableStatus::OnlyLeft => {
            if dark { egui::Color32::from_rgb(120, 200, 120) } else { egui::Color32::from_rgb(20, 120, 20) }
        }
        TableStatus::OnlyRight => {
            if dark { egui::Color32::from_rgb(230, 120, 120) } else { egui::Color32::from_rgb(170, 30, 30) }
        }
        TableStatus::Different => {
            if dark { egui::Color32::from_rgb(230, 180, 90) } else { egui::Color32::from_rgb(170, 110, 0) }
        }
        TableStatus::Identical => ui.visuals().weak_text_color(),
    }
}

/// One aligned row: origin cell | checkbox cell | target cell.
fn three_cols(
    ui: &mut egui::Ui,
    side_w: f32,
    left: impl FnOnce(&mut egui::Ui),
    mid: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui),
) {
    let h = ui.spacing().interact_size.y;
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(side_w, h),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(side_w);
                ui.set_max_width(side_w);
                left(ui);
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(CHECK_COL_W, h),
            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
            mid,
        );
        ui.allocate_ui_with_layout(
            egui::vec2(side_w, h),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_min_width(side_w);
                ui.set_max_width(side_w);
                right(ui);
            },
        );
    });
}

fn side_label(ui: &mut egui::Ui, text: &str, color: Option<egui::Color32>) {
    let mut rt = egui::RichText::new(text);
    if let Some(c) = color {
        rt = rt.color(c);
    }
    ui.add(egui::Label::new(rt).truncate());
}

fn dash(ui: &mut egui::Ui) {
    ui.weak("—");
}

/// The two sides of one diff entry, as (origin, target) display texts.
fn entry_sides(e: &DiffEntry) -> (Option<String>, Option<String>) {
    let label = |desc: &str| format!("{} {}: {}", e.kind, e.name, desc);
    match &e.change {
        Change::OnlyLeft(desc) => (Some(label(desc)), None),
        Change::OnlyRight(desc) => (None, Some(label(desc))),
        Change::Differs { left, right } => (Some(label(left)), Some(label(right))),
    }
}

pub fn draw(
    ui: &mut egui::Ui,
    tab: &mut CompareTab,
    active: &[ActiveConnection],
) -> CompareAction {
    let mut action = CompareAction::None;

    // Selection + controls row.
    ui.horizontal(|ui| {
        ui.label("Origin:");
        if conn_combo(ui, ("cmp_left", tab.id), &mut tab.left, active) {
            tab.left_snap = None;
            tab.recompute();
        }
        if ui.button("⇆").on_hover_text("Swap origin and target").clicked() {
            tab.swap_sides();
        }
        ui.label("Target:");
        if conn_combo(ui, ("cmp_right", tab.id), &mut tab.right, active) {
            tab.right_snap = None;
            tab.recompute();
        }
        ui.add_space(6.0);
        let both = tab.left.is_some() && tab.right.is_some() && tab.left != tab.right;
        if tab.loading() {
            ui.spinner();
            ui.weak("introspecting…");
        } else if ui
            .add_enabled(both, egui::Button::new("Compare"))
            .on_hover_text("Introspect both databases and diff their structures")
            .clicked()
        {
            tab.error = None;
            action = CompareAction::Snapshot;
        }
        if tab.left.is_some() && tab.left == tab.right {
            ui.weak("pick two different connections");
        }
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut tab.show_identical, "Show identical");
        if ui
            .checkbox(&mut tab.ignore_schema, "Match by table name only")
            .on_hover_text("Pair tables across differently-named schemas")
            .changed()
        {
            tab.recompute();
        }
        ui.add_space(6.0);
        ui.label("🔍");
        ui.add(
            egui::TextEdit::singleline(&mut tab.filter)
                .hint_text("filter tables")
                .desired_width(160.0),
        );
    });
    if let Some(err) = &tab.error {
        ui.colored_label(ui.visuals().warn_fg_color, err);
    }
    ui.separator();

    let left_name = conn_name(active, tab.left);
    let right_name = conn_name(active, tab.right);
    let left_kind = conn_kind(active, tab.left);
    let right_kind = conn_kind(active, tab.right);
    let same_kind = matches!((left_kind, right_kind), (Some(a), Some(b)) if a == b);

    if tab.diff.is_none() {
        ui.weak("Pick an origin and a target, then hit Compare.");
        return action;
    }

    // Rebuild the script when selection or diff changed.
    if tab.script_dirty {
        let script = match (&tab.diff, &tab.left_snap, &tab.right_snap, right_kind) {
            (Some(diff), Some((_, l)), Some((_, r)), Some(kind)) if same_kind => {
                let included = &tab.included;
                compare::selective_migration(kind, diff, l, r, &|d, e| {
                    let key = compare::table_key(d);
                    let entry = e.map(compare::entry_key).unwrap_or_default();
                    included.contains(&(key, entry))
                })
            }
            _ => Vec::new(),
        };
        tab.script = script;
        tab.script_dirty = false;
        tab.execute_armed = false;
    }

    let diff = tab.diff.as_ref().unwrap();

    // Summary.
    let count = |s: TableStatus| diff.iter().filter(|d| d.status == s).count();
    ui.label(format!(
        "{} only in {left_name} · {} only in {right_name} · {} different · {} identical",
        count(TableStatus::OnlyLeft),
        count(TableStatus::OnlyRight),
        count(TableStatus::Different),
        count(TableStatus::Identical),
    ));
    ui.add_space(2.0);

    // Split the remaining height: diff tree on top, script preview below.
    let avail_h = ui.available_height();
    let script_h = (avail_h * 0.38).clamp(110.0, 280.0);
    let tree_h = (avail_h - script_h - 24.0).max(80.0);

    let side_w =
        ((ui.available_width() - CHECK_COL_W - ui.spacing().item_spacing.x * 2.0) / 2.0)
            .max(120.0);

    // Header row with the master checkbox.
    let all_keys = CompareTab::all_keys(diff);
    let strong = ui.visuals().strong_text_color();
    let mut toggle_all: Option<bool> = None;
    three_cols(
        ui,
        side_w,
        |ui| side_label(ui, &format!("Origin: {left_name}"), Some(strong)),
        |ui| {
            let mut all = !all_keys.is_empty() && all_keys.is_subset(&tab.included);
            if !all_keys.is_empty() && ui.checkbox(&mut all, "").changed() {
                toggle_all = Some(all);
            }
        },
        |ui| side_label(ui, &format!("Target: {right_name}"), Some(strong)),
    );
    if let Some(on) = toggle_all {
        if on {
            tab.included = all_keys.clone();
        } else {
            tab.included.clear();
        }
        tab.script_dirty = true;
    }
    ui.separator();

    // Diff tree.
    let filter = tab.filter.trim().to_lowercase();
    let mut any_change = false;
    egui::ScrollArea::vertical()
        .id_source(("cmp_tree", tab.id))
        .max_height(tree_h)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for d in diff {
                if d.status == TableStatus::Identical && !tab.show_identical {
                    continue;
                }
                let key = compare::table_key(d);
                if !filter.is_empty() && !key.to_lowercase().contains(&filter) {
                    continue;
                }
                let color = status_color(ui, d.status);

                match d.status {
                    TableStatus::Identical => {
                        three_cols(
                            ui,
                            side_w,
                            |ui| side_label(ui, &key, Some(color)),
                            |_ui| {},
                            |ui| side_label(ui, &key, Some(color)),
                        );
                    }
                    TableStatus::OnlyLeft | TableStatus::OnlyRight => {
                        let k = (key.clone(), String::new());
                        let mut on = tab.included.contains(&k);
                        let mut changed = false;
                        let only_left = d.status == TableStatus::OnlyLeft;
                        three_cols(
                            ui,
                            side_w,
                            |ui| {
                                if only_left {
                                    side_label(ui, &key, Some(color));
                                } else {
                                    dash(ui);
                                }
                            },
                            |ui| {
                                let hover = if only_left {
                                    format!("Create on {right_name}")
                                } else {
                                    format!("Drop from {right_name}")
                                };
                                changed =
                                    ui.checkbox(&mut on, "").on_hover_text(hover).changed();
                            },
                            |ui| {
                                if only_left {
                                    dash(ui);
                                } else {
                                    side_label(ui, &key, Some(color));
                                }
                            },
                        );
                        if changed {
                            if on {
                                tab.included.insert(k);
                            } else {
                                tab.included.remove(&k);
                            }
                            any_change = true;
                        }
                    }
                    TableStatus::Different => {
                        let open = tab.expanded.contains(&key);
                        let entry_keys: Vec<(String, String)> = d
                            .entries
                            .iter()
                            .map(|e| (key.clone(), compare::entry_key(e)))
                            .collect();
                        let mut all_on =
                            entry_keys.iter().all(|k| tab.included.contains(k));
                        let mut toggled = false;
                        let mut toggle_open = false;
                        three_cols(
                            ui,
                            side_w,
                            |ui| {
                                let arrow = if open { "⏷" } else { "⏵" };
                                if ui
                                    .add(egui::Button::new(arrow).small().frame(false))
                                    .clicked()
                                {
                                    toggle_open = true;
                                }
                                side_label(ui, &key, Some(color));
                            },
                            |ui| {
                                toggled = ui
                                    .checkbox(&mut all_on, "")
                                    .on_hover_text("Include all of this table's changes")
                                    .changed();
                            },
                            |ui| {
                                ui.add_space(18.0);
                                side_label(ui, &key, Some(color));
                            },
                        );
                        if toggle_open {
                            if open {
                                tab.expanded.remove(&key);
                            } else {
                                tab.expanded.insert(key.clone());
                            }
                        }
                        if toggled {
                            for k in &entry_keys {
                                if all_on {
                                    tab.included.insert(k.clone());
                                } else {
                                    tab.included.remove(k);
                                }
                            }
                            any_change = true;
                        }
                        if open {
                            for (e, k) in d.entries.iter().zip(&entry_keys) {
                                let (l_txt, r_txt) = entry_sides(e);
                                let e_color = match &e.change {
                                    Change::OnlyLeft(_) => {
                                        status_color(ui, TableStatus::OnlyLeft)
                                    }
                                    Change::OnlyRight(_) => {
                                        status_color(ui, TableStatus::OnlyRight)
                                    }
                                    Change::Differs { .. } => {
                                        status_color(ui, TableStatus::Different)
                                    }
                                };
                                let mut on = tab.included.contains(k);
                                let mut changed = false;
                                three_cols(
                                    ui,
                                    side_w,
                                    |ui| {
                                        ui.add_space(22.0);
                                        match &l_txt {
                                            Some(t) => side_label(ui, t, Some(e_color)),
                                            None => dash(ui),
                                        }
                                    },
                                    |ui| {
                                        changed = ui.checkbox(&mut on, "").changed();
                                    },
                                    |ui| {
                                        ui.add_space(22.0);
                                        match &r_txt {
                                            Some(t) => side_label(ui, t, Some(e_color)),
                                            None => dash(ui),
                                        }
                                    },
                                );
                                if changed {
                                    if on {
                                        tab.included.insert(k.clone());
                                    } else {
                                        tab.included.remove(k);
                                    }
                                    any_change = true;
                                }
                            }
                        }
                    }
                }
            }
        });
    if any_change {
        tab.script_dirty = true;
    }

    ui.separator();

    // Script preview + actions.
    if !same_kind {
        ui.weak("Cross-DBMS pair — script generation is disabled; use the diff above as a reference.");
        return action;
    }

    let text: String = tab
        .script
        .iter()
        .map(|(table, stmts)| {
            let body: String = stmts
                .iter()
                .map(|s| match s.note {
                    Some(n) => format!("{}; -- {n}\n", s.sql),
                    None => format!("{};\n", s.sql),
                })
                .collect();
            format!("-- {table}\n{body}\n")
        })
        .collect();
    let stmt_count: usize = tab.script.iter().map(|(_, s)| s.len()).sum();
    let destructive = tab
        .script
        .iter()
        .flat_map(|(_, s)| s)
        .filter(|s| s.danger == Danger::Destructive)
        .count();
    let lossy = tab
        .script
        .iter()
        .flat_map(|(_, s)| s)
        .filter(|s| s.danger == Danger::Lossy)
        .count();

    let target_active = tab
        .right
        .is_some_and(|c| active.iter().any(|a| a.conn_id == c));
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Script preview").strong());
        ui.weak(format!("({stmt_count} statement{})", if stmt_count == 1 { "" } else { "s" }));
        if destructive + lossy > 0 {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("⚠ {destructive} destructive · {lossy} possibly lossy"),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let can_run = stmt_count > 0 && target_active && !tab.applying;
            let exec_label = if tab.execute_armed {
                format!("⚠ Really execute ({destructive} destructive)")
            } else {
                format!("▶ Execute on {right_name}")
            };
            if ui
                .add_enabled(can_run, egui::Button::new(exec_label))
                .on_hover_text("Runs transactionally where the DBMS supports transactional DDL")
                .clicked()
            {
                if destructive > 0 && !tab.execute_armed {
                    tab.execute_armed = true;
                } else {
                    tab.execute_armed = false;
                    let statements: Vec<String> = tab
                        .script
                        .iter()
                        .flat_map(|(_, s)| s.iter().map(|st| st.sql.clone()))
                        .collect();
                    action = CompareAction::Apply {
                        conn: tab.right.unwrap(),
                        statements,
                    };
                }
            }
            if tab.applying {
                ui.spinner();
            }
            if ui
                .add_enabled(can_run, egui::Button::new("Open in query tab"))
                .on_hover_text("Edit the script before running it elsewhere")
                .clicked()
            {
                action = CompareAction::OpenScript {
                    conn: tab.right.unwrap(),
                    sql: text.clone(),
                };
            }
            if ui
                .add_enabled(stmt_count > 0, egui::Button::new("📋 Copy"))
                .clicked()
            {
                ui.output_mut(|o| o.copied_text = text.clone());
            }
        });
    });
    egui::ScrollArea::both()
        .id_source(("cmp_script", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if stmt_count == 0 {
                ui.weak("Nothing selected — tick changes above to build the migration.");
            } else {
                ui.add(egui::Label::new(egui::RichText::new(&text).monospace()).extend());
            }
        });

    action
}
