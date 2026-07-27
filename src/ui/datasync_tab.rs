//! Data compare / sanitized pull tab.
//!
//! Pick a source and a same-dialect target, choose tables, optionally attach
//! masking rules (suggested for the usual PII column names), then:
//! Compare (checksum pass + row diff), open the sync script in an editor
//! against the target, or Pull (sanitized full replace, source → target).

use std::collections::{BTreeMap, BTreeSet};

use crate::db::datasync::{
    FakeKind, MaskMap, MaskStrategy, TableReport, TableSel, infer_fake, suggest_mask,
};
use crate::db::{DbKind, DbMeta};
use crate::runtime::ConnectionId;

use super::{ActiveConnection, TabId, theme};

pub struct DataSyncTab {
    pub id: TabId,
    pub source: Option<ConnectionId>,
    pub target: Option<ConnectionId>,
    /// Source-side meta (tables, columns, PKs, FKs).
    pub meta: Option<DbMeta>,
    pub meta_loading: bool,
    /// Selected "schema.table" keys.
    pub selected: BTreeSet<String>,
    /// Masking rules, "schema.table.column" → strategy.
    pub masks: MaskMap,
    /// Draft text of Fixed-strategy inputs (column key → text).
    pub fixed_drafts: BTreeMap<String, String>,
    /// Expanded tables in the checklist (showing columns).
    pub expanded: BTreeSet<String>,
    pub filter: String,
    /// Optional per-table row cap for Pull (blank = all rows).
    pub row_limit: String,
    pub include_deletes: bool,
    pub reports: Option<Vec<TableReport>>,
    pub running: bool,
    pub progress: String,
    pub status: Option<String>,
    pub error: Option<String>,
    /// Two-step confirmation for Pull.
    pub pull_armed: bool,
    /// Source profile is production-flagged (drives the no-masks warning).
    pub source_is_production: bool,
}

pub enum DataSyncAction {
    None,
    /// Introspect the source (tables/columns/PKs/FKs).
    LoadTables,
    Compare,
    /// Generate DML from the reports and open it in a query tab on target.
    OpenSyncScript,
    Pull,
}

impl DataSyncTab {
    pub fn new(id: TabId, source: Option<ConnectionId>) -> Self {
        Self {
            id,
            source,
            target: None,
            meta: None,
            meta_loading: false,
            selected: BTreeSet::new(),
            masks: MaskMap::new(),
            fixed_drafts: BTreeMap::new(),
            expanded: BTreeSet::new(),
            filter: String::new(),
            row_limit: String::new(),
            include_deletes: true,
            reports: None,
            running: false,
            progress: String::new(),
            status: None,
            error: None,
            pull_armed: false,
            source_is_production: false,
        }
    }

    /// Resolve the current selection into engine TableSels (source meta
    /// order, FK-topo sorted for safe insert order).
    pub fn selections(&self) -> Vec<TableSel> {
        let Some(meta) = &self.meta else { return Vec::new() };
        let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut sels = Vec::new();
        for t in &meta.tables {
            if !matches!(t.kind, crate::db::TableKind::Table) {
                continue;
            }
            let key = format!("{}.{}", t.schema, t.name);
            if !self.selected.contains(&key) {
                continue;
            }
            deps.insert(
                key,
                t.foreign_keys
                    .iter()
                    .map(|f| format!("{}.{}", f.to_schema, f.to_table))
                    .collect(),
            );
            sels.push(TableSel {
                schema: t.schema.clone(),
                table: t.name.clone(),
                columns: t.columns.iter().map(|c| c.name.clone()).collect(),
                pk: t.primary_key.clone(),
            });
        }
        crate::db::datasync::topo_order(sels, &deps)
    }

    pub fn row_limit_value(&self) -> Option<u64> {
        self.row_limit.trim().parse().ok()
    }

    fn report_for(&self, key: &str) -> Option<&TableReport> {
        self.reports
            .as_ref()?
            .iter()
            .find(|r| format!("{}.{}", r.schema, r.table) == key)
    }
}

fn conn_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    label: &str,
    value: &mut Option<ConnectionId>,
    conns: &[&ActiveConnection],
) -> bool {
    let mut changed = false;
    let current = value
        .and_then(|c| conns.iter().find(|a| a.conn_id == c))
        .map(|a| format!("{} · {}", a.name, a.database))
        .unwrap_or_else(|| label.to_string());
    egui::ComboBox::from_id_source(id)
        .selected_text(current)
        .width(190.0)
        .show_ui(ui, |ui| {
            for a in conns {
                let text = format!("{} · {}", a.name, a.database);
                if ui
                    .selectable_label(*value == Some(a.conn_id), text)
                    .clicked()
                {
                    *value = Some(a.conn_id);
                    changed = true;
                }
            }
        });
    changed
}

pub fn draw(
    ui: &mut egui::Ui,
    tab: &mut DataSyncTab,
    active: &[ActiveConnection],
) -> DataSyncAction {
    let mut action = DataSyncAction::None;
    let all: Vec<&ActiveConnection> = active.iter().collect();
    let source_kind: Option<DbKind> = tab
        .source
        .and_then(|c| active.iter().find(|a| a.conn_id == c))
        .map(|a| a.kind);
    // Compare/pull require the same dialect (literals & checksums differ).
    let targets: Vec<&ActiveConnection> = active
        .iter()
        .filter(|a| Some(a.conn_id) != tab.source && Some(a.kind) == source_kind)
        .collect();

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Source").strong());
        if conn_combo(ui, ("ds_src", tab.id), "pick source…", &mut tab.source, &all) {
            tab.meta = None;
            tab.reports = None;
            tab.selected.clear();
            action = DataSyncAction::LoadTables;
        }
        ui.label("→");
        ui.label(egui::RichText::new("Target").strong());
        if conn_combo(ui, ("ds_tgt", tab.id), "pick target…", &mut tab.target, &targets) {
            tab.reports = None;
            tab.pull_armed = false;
        }
        if tab.source.is_some() && tab.meta.is_none() && !tab.meta_loading {
            if ui.button("Load tables").clicked() {
                action = DataSyncAction::LoadTables;
            }
        }
        if tab.meta_loading || tab.running {
            ui.spinner();
            if !tab.progress.is_empty() {
                ui.weak(&tab.progress);
            }
        }
    });

    let ready = tab.meta.is_some()
        && tab.target.is_some()
        && !tab.selected.is_empty()
        && !tab.running;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(ready, egui::Button::new("⚖ Compare"))
            .on_hover_text("Server-side checksums first, then a row-level diff of differing tables")
            .clicked()
        {
            action = DataSyncAction::Compare;
        }
        let has_diffs = tab
            .reports
            .as_ref()
            .is_some_and(|rs| rs.iter().any(|r| r.diff_count() > 0));
        if ui
            .add_enabled(has_diffs && !tab.running, egui::Button::new("📝 Sync script…"))
            .on_hover_text(
                "Generate INSERT/UPDATE/DELETE making the target match the source \
                 (masking applied) and open it in an editor on the target — nothing runs yet",
            )
            .clicked()
        {
            action = DataSyncAction::OpenSyncScript;
        }
        ui.checkbox(&mut tab.include_deletes, "script deletes target-only rows");
        ui.separator();
        if !tab.pull_armed {
            if ui
                .add_enabled(ready, egui::Button::new("⬇ Pull data…"))
                .on_hover_text(
                    "Full replace: DELETE the selected tables on the target and re-copy \
                     every row from the source, masked in flight",
                )
                .clicked()
            {
                tab.pull_armed = true;
            }
        } else {
            let conn_label = |id: Option<crate::runtime::ConnectionId>| {
                id.and_then(|c| active.iter().find(|a| a.conn_id == c))
                    .map(|a| format!("{} · {}", a.name, a.database))
                    .unwrap_or_else(|| "?".into())
            };
            // Wrap the warning at a width that always leaves the yes/no
            // buttons visible on the row.
            let text = egui::RichText::new(format!(
                "Deletes ALL rows of {} table(s) on {} and refills them \
                 from {}. Sure?",
                tab.selected.len(),
                conn_label(tab.target),
                conn_label(tab.source),
            ))
            .color(ui.visuals().error_fg_color);
            ui.scope(|ui| {
                let reserve = 170.0; // "Yes, pull" + "Cancel"
                ui.set_max_width((ui.available_width() - reserve).max(220.0));
                ui.add(egui::Label::new(text).wrap());
            });
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("Yes, pull").color(egui::Color32::WHITE))
                        .fill(ui.visuals().error_fg_color),
                )
                .clicked()
            {
                tab.pull_armed = false;
                action = DataSyncAction::Pull;
            }
            if ui.button("Cancel").clicked() {
                tab.pull_armed = false;
            }
        }
        ui.label("Row limit:");
        ui.add(
            egui::TextEdit::singleline(&mut tab.row_limit)
                .desired_width(70.0)
                .hint_text("all"),
        )
        .on_hover_text("Pull at most this many rows per table (sampled sandboxes)");
    });

    if tab.source_is_production && tab.masks.is_empty() && tab.meta.is_some() {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "⚠ Source is flagged production and NO masking rules are set — \
             a pull would copy raw production data. Consider \"Suggest masks\".",
        );
    }
    if let Some(err) = &tab.error {
        ui.colored_label(ui.visuals().error_fg_color, err);
    }
    if let Some(status) = &tab.status {
        ui.colored_label(theme::success_color(ui), status);
    }
    ui.add_space(4.0);
    ui.separator();

    let Some(meta) = tab.meta.clone() else {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(
                egui::RichText::new("Pick a source connection to list its tables.")
                    .color(ui.visuals().weak_text_color()),
            );
        });
        return action;
    };

    // Selection toolbar.
    ui.horizontal(|ui| {
        if ui.small_button("All").clicked() {
            for t in &meta.tables {
                if matches!(t.kind, crate::db::TableKind::Table) {
                    tab.selected.insert(format!("{}.{}", t.schema, t.name));
                }
            }
        }
        if ui.small_button("None").clicked() {
            tab.selected.clear();
        }
        if ui
            .small_button("🛡 Suggest masks")
            .on_hover_text(
                "Adds hash/email rules for columns that look sensitive \
                 (email, password, phone, names, …)",
            )
            .clicked()
        {
            for t in &meta.tables {
                for c in &t.columns {
                    // JSON columns get the scrub (it only touches
                    // sensitive-looking keys, so it's always safe).
                    let s = if c.type_name.to_ascii_lowercase().contains("json") {
                        Some(MaskStrategy::JsonScrub)
                    } else {
                        suggest_mask(&c.name)
                    };
                    if let Some(s) = s {
                        tab.masks
                            .entry(format!("{}.{}.{}", t.schema, t.name, c.name))
                            .or_insert(s);
                    }
                }
            }
        }
        if !tab.masks.is_empty() {
            ui.weak(format!("{} mask rule(s)", tab.masks.len()));
            if ui.small_button("clear masks").clicked() {
                tab.masks.clear();
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut tab.filter)
                    .desired_width(160.0)
                    .hint_text("filter tables…"),
            );
        });
    });
    ui.add_space(2.0);

    let filter = tab.filter.trim().to_ascii_lowercase();
    egui::ScrollArea::vertical()
        .id_source(("ds_tables", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for t in &meta.tables {
                if !matches!(t.kind, crate::db::TableKind::Table) {
                    continue;
                }
                let key = format!("{}.{}", t.schema, t.name);
                if !filter.is_empty() && !key.to_ascii_lowercase().contains(&filter) {
                    continue;
                }
                ui.horizontal(|ui| {
                    let mut on = tab.selected.contains(&key);
                    if ui.checkbox(&mut on, "").changed() {
                        if on {
                            tab.selected.insert(key.clone());
                        } else {
                            tab.selected.remove(&key);
                        }
                    }
                    let expanded = tab.expanded.contains(&key);
                    if ui
                        .small_button(if expanded { "▾" } else { "▸" })
                        .on_hover_text("Columns & masking")
                        .clicked()
                    {
                        if expanded {
                            tab.expanded.remove(&key);
                        } else {
                            tab.expanded.insert(key.clone());
                        }
                    }
                    let masked_cols = t
                        .columns
                        .iter()
                        .filter(|c| tab.masks.contains_key(&format!("{key}.{}", c.name)))
                        .count();
                    let mut label = key.clone();
                    if t.primary_key.is_empty() {
                        label.push_str("  (no PK)");
                    }
                    ui.label(label);
                    if masked_cols > 0 {
                        ui.label(
                            egui::RichText::new(format!("🛡 {masked_cols}"))
                                .small()
                                .color(theme::ACCENT),
                        );
                    }
                    if let Some(r) = tab.report_for(&key) {
                        draw_report_summary(ui, r);
                    }
                });
                if tab.expanded.contains(&key) {
                    ui.indent(("ds_cols", &key), |ui| {
                        for c in &t.columns {
                            let col_key = format!("{key}.{}", c.name);
                            ui.horizontal(|ui| {
                                let name_label = ui.label(
                                    egui::RichText::new(format!("{} · {}", c.name, c.type_name))
                                        .monospace()
                                        .small(),
                                );
                                if !c.nullable {
                                    name_label.on_hover_text(
                                        "NOT NULL — the NULL mask is not offered",
                                    );
                                }
                                let current = tab.masks.get(&col_key).cloned();
                                let label = current
                                    .as_ref()
                                    .map(|m| m.label())
                                    .unwrap_or("keep");
                                egui::ComboBox::from_id_source(("ds_mask", &col_key))
                                    .selected_text(label)
                                    .width(80.0)
                                    .show_ui(ui, |ui| {
                                        if ui.selectable_label(current.is_none(), "keep").clicked() {
                                            tab.masks.remove(&col_key);
                                        }
                                        let opts = if c.nullable {
                                            &[
                                                MaskStrategy::Null,
                                                MaskStrategy::Empty,
                                                MaskStrategy::Hash,
                                            ][..]
                                        } else {
                                            // NULL would violate the column's
                                            // NOT NULL constraint mid-pull.
                                            &[MaskStrategy::Empty, MaskStrategy::Hash][..]
                                        };
                                        for opt in opts.iter().cloned() {
                                            let sel = current.as_ref() == Some(&opt);
                                            if ui.selectable_label(sel, opt.label()).clicked() {
                                                tab.masks.insert(col_key.clone(), opt.clone());
                                            }
                                        }
                                        // One "fake" entry; the kind comes from
                                        // the column's name and type.
                                        match infer_fake(&c.name, &c.type_name) {
                                            Some(f) => {
                                                let sel = current.as_ref() == Some(&f);
                                                if ui
                                                    .selectable_label(sel, f.label())
                                                    .on_hover_text(
                                                        "Deterministic realistic fake — the \
                                                         kind is inferred from the column",
                                                    )
                                                    .clicked()
                                                {
                                                    tab.masks.insert(col_key.clone(), f.clone());
                                                }
                                            }
                                            // Nothing inferable: offer every
                                            // fake kind so it stays possible.
                                            None => {
                                                ui.separator();
                                                for kind in FakeKind::ALL {
                                                    let opt = MaskStrategy::Fake(kind);
                                                    let sel = current.as_ref() == Some(&opt);
                                                    if ui
                                                        .selectable_label(sel, kind.label())
                                                        .clicked()
                                                    {
                                                        tab.masks.insert(col_key.clone(), opt);
                                                    }
                                                }
                                                let scrub =
                                                    current == Some(MaskStrategy::JsonScrub);
                                                if ui
                                                    .selectable_label(scrub, "scrub JSON")
                                                    .clicked()
                                                {
                                                    tab.masks.insert(
                                                        col_key.clone(),
                                                        MaskStrategy::JsonScrub,
                                                    );
                                                }
                                            }
                                        }
                                        let is_fixed =
                                            matches!(current, Some(MaskStrategy::Fixed(_)));
                                        if ui.selectable_label(is_fixed, "fixed").clicked() {
                                            let text = tab
                                                .fixed_drafts
                                                .get(&col_key)
                                                .cloned()
                                                .unwrap_or_default();
                                            tab.masks
                                                .insert(col_key.clone(), MaskStrategy::Fixed(text));
                                        }
                                    });
                                if let Some(MaskStrategy::Fixed(_)) = tab.masks.get(&col_key) {
                                    let draft =
                                        tab.fixed_drafts.entry(col_key.clone()).or_default();
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(draft)
                                                .desired_width(120.0)
                                                .hint_text("value (NULL/number/text)"),
                                        )
                                        .changed()
                                    {
                                        tab.masks.insert(
                                            col_key.clone(),
                                            MaskStrategy::Fixed(draft.clone()),
                                        );
                                    }
                                }
                            });
                        }
                    });
                }
            }
        });

    action
}

fn draw_report_summary(ui: &mut egui::Ui, r: &TableReport) {
    if let Some(err) = &r.error {
        ui.label(
            egui::RichText::new(format!("✖ {err}"))
                .small()
                .color(ui.visuals().error_fg_color),
        );
        return;
    }
    if r.no_pk {
        ui.label(
            egui::RichText::new(format!(
                "counts {} vs {} — no PK, rows not compared",
                r.source_count.unwrap_or(0),
                r.target_count.unwrap_or(0)
            ))
            .small()
            .color(ui.visuals().warn_fg_color),
        );
        return;
    }
    if r.in_sync() {
        ui.label(
            egui::RichText::new(format!("✔ in sync ({} rows)", r.source_count.unwrap_or(0)))
                .small()
                .color(theme::success_color(ui)),
        );
        return;
    }
    let mut parts = Vec::new();
    if !r.missing.is_empty() {
        parts.push(format!("{} missing", r.missing.len()));
    }
    if !r.extra.is_empty() {
        parts.push(format!("{} extra", r.extra.len()));
    }
    if !r.changed.is_empty() {
        parts.push(format!("{} changed", r.changed.len()));
    }
    if parts.is_empty() {
        parts.push("differs (checksum)".to_string());
    }
    let mut text = parts.join(", ");
    if r.truncated {
        text.push_str(" (truncated)");
    }
    ui.label(
        egui::RichText::new(format!("≠ {text}"))
            .small()
            .color(ui.visuals().warn_fg_color),
    );
}
