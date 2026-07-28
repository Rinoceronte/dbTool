//! Cross-connection, column-mapped data transfer tab.
//!
//! Unlike the data-sync tab (same dialect, same schema), this copies data
//! into another connection — engines may differ, names don't have to match.
//! The list is destination-driven: every target table is listed and
//! auto-paired with a same-named source table (snake/camel/kebab-insensitive);
//! tick the pairs to fill, adjust where a pair pulls from with its dropdown,
//! and expand a pair to tune its column mapping (plain column or a raw
//! source-side SQL expression) and per-table WHERE. Unticked target columns
//! are left out of the INSERT so defaults and serials apply. Pairs run in
//! FK-safe order on the target; "delete target rows first" clears them in
//! reverse order.

use std::collections::{BTreeMap, BTreeSet};

use crate::db::datasync::{KeyCapture, SourceExpr, TransferSpec};
use crate::db::{DbMeta, TableMeta};
use crate::runtime::ConnectionId;

use super::{ActiveConnection, TabId, theme};

/// One target table's transfer configuration.
#[derive(Default)]
pub struct PairState {
    /// Chosen source table key ("schema.table"); auto-matched by name.
    pub source: Option<String>,
    pub enabled: bool,
    /// target column → source column or expression feeding it.
    pub mapping: BTreeMap<String, SourceExpr>,
    /// Target columns included in the INSERT.
    pub enabled_cols: BTreeSet<String>,
    /// Per-table raw WHERE body in the source dialect.
    pub filter: String,
    /// Let the target generate its PK and capture an old→new key map that
    /// children's FK columns translate through.
    pub new_pk: bool,
    /// Source column holding the old key (defaults to the source PK).
    pub key_source_col: Option<String>,
}

pub struct TransferTab {
    pub id: TabId,
    pub source: Option<ConnectionId>,
    pub target: Option<ConnectionId>,
    pub source_meta: Option<DbMeta>,
    pub target_meta: Option<DbMeta>,
    pub source_meta_loading: bool,
    pub target_meta_loading: bool,
    /// target table key → its pair config.
    pub pairs: BTreeMap<String, PairState>,
    /// Expanded pairs (showing the column grid).
    pub expanded: BTreeSet<String>,
    pub table_filter: String,
    pub row_limit: String,
    pub delete_first: bool,
    pub running: bool,
    pub progress: String,
    pub status: Option<String>,
    pub error: Option<String>,
    /// Two-step confirmation for Run.
    pub armed: bool,
}

pub enum TransferAction {
    None,
    LoadSourceMeta,
    LoadTargetMeta,
    Run,
    Stop,
}

impl TransferTab {
    pub fn new(id: TabId, source: Option<ConnectionId>) -> Self {
        Self {
            id,
            source,
            target: None,
            source_meta: None,
            target_meta: None,
            source_meta_loading: false,
            target_meta_loading: false,
            pairs: BTreeMap::new(),
            expanded: BTreeSet::new(),
            table_filter: String::new(),
            row_limit: String::new(),
            delete_first: false,
            running: false,
            progress: String::new(),
            status: None,
            error: None,
            armed: false,
        }
    }

    /// Auto-pair every target table with a same-named source table and
    /// auto-map each pair's columns. Called when either meta (re)loads.
    pub fn remap(&mut self) {
        self.pairs.clear();
        self.armed = false;
        let (Some(smeta), Some(tmeta)) = (&self.source_meta, &self.target_meta) else { return };
        for ttm in base_tables(tmeta) {
            let tkey = table_key(ttm);
            let source = base_tables(smeta)
                .find(|stm| squash(&stm.name) == squash(&ttm.name))
                .map(table_key);
            let mut pair = PairState {
                enabled: source.is_some(),
                source,
                ..Default::default()
            };
            self.map_pair_columns(&mut pair, ttm);
            self.pairs.insert(tkey, pair);
        }
    }

    /// Auto-map one pair's columns by name.
    fn map_pair_columns(&self, pair: &mut PairState, ttm: &TableMeta) {
        pair.mapping.clear();
        pair.enabled_cols.clear();
        let Some(stm) = self.source_table_meta(pair) else { return };
        for c in &ttm.columns {
            if let Some(src) = stm.columns.iter().find(|s| squash(&s.name) == squash(&c.name)) {
                pair.mapping.insert(c.name.clone(), SourceExpr::Column(src.name.clone()));
                pair.enabled_cols.insert(c.name.clone());
            }
        }
    }

    pub fn target_table_meta(&self, tkey: &str) -> Option<&TableMeta> {
        self.target_meta.as_ref()?.tables.iter().find(|t| table_key(t) == tkey)
    }

    pub fn source_table_meta(&self, pair: &PairState) -> Option<&TableMeta> {
        let key = pair.source.as_deref()?;
        self.source_meta.as_ref()?.tables.iter().find(|t| table_key(t) == key)
    }

    /// The target's generated-PK column for a pair, when "new ids" can work
    /// (exactly one PK column).
    pub fn single_target_pk(&self, tkey: &str) -> Option<String> {
        let ttm = self.target_table_meta(tkey)?;
        match ttm.primary_key.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    /// The old-key source column for a pair: explicit choice, else source PK.
    pub fn key_source_col(&self, pair: &PairState) -> Option<String> {
        pair.key_source_col.clone().or_else(|| {
            match self.source_table_meta(pair)?.primary_key.as_slice() {
                [one] => Some(one.clone()),
                _ => None,
            }
        })
    }

    /// Toggling "new ids" on the pair keyed by `parent_tkey`: convert
    /// children's plain FK column mappings into key lookups (or back),
    /// driven by the source's FK metadata. The lookup names the parent's
    /// SOURCE table — that is what keys the runtime's captured id maps.
    pub fn propagate_key_capture(&mut self, parent_tkey: &str, on: bool) {
        let Some(parent_src) =
            self.pairs.get(parent_tkey).and_then(|p| p.source.clone())
        else {
            return;
        };
        let Some(smeta) = &self.source_meta else { return };
        // (child source key, source FK column) referencing the parent source.
        let edges: Vec<(String, String)> = smeta
            .tables
            .iter()
            .flat_map(|t| {
                let child = table_key(t);
                t.foreign_keys
                    .iter()
                    .filter(|f| format!("{}.{}", f.to_schema, f.to_table) == parent_src)
                    .map(move |f| (child.clone(), f.from_column.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (child_src, from_col) in edges {
            for pair in self.pairs.values_mut() {
                if pair.source.as_deref() != Some(child_src.as_str()) {
                    continue;
                }
                for expr in pair.mapping.values_mut() {
                    if on {
                        if matches!(expr, SourceExpr::Column(c) if *c == from_col) {
                            *expr = SourceExpr::KeyLookup {
                                table: parent_src.clone(),
                                column: from_col.clone(),
                            };
                        }
                    } else if matches!(expr, SourceExpr::KeyLookup { table, .. } if *table == parent_src)
                    {
                        *expr = SourceExpr::Column(from_col.clone());
                    }
                }
            }
        }
    }

    /// Resolve all enabled pairs into engine specs, FK-topo-ordered on the
    /// target side (parents first).
    pub fn build_specs(&self) -> Vec<TransferSpec> {
        let Some(tmeta) = &self.target_meta else { return Vec::new() };
        let row_limit = self.row_limit.trim().parse().ok();
        let mut specs = Vec::new();
        for (tkey, pair) in &self.pairs {
            if !pair.enabled {
                continue;
            }
            let (Some(stm), Some(ttm)) =
                (self.source_table_meta(pair), self.target_table_meta(tkey))
            else {
                continue;
            };
            // With "new ids" the generated PK never travels; it comes from
            // the key capture instead.
            let key_capture = if pair.new_pk {
                let (Some(pk), Some(src_col)) =
                    (self.single_target_pk(tkey), self.key_source_col(pair))
                else {
                    continue;
                };
                Some(KeyCapture { source_column: src_col, target_pk: pk })
            } else {
                None
            };
            let generated_pk = key_capture.as_ref().map(|kc| kc.target_pk.as_str());
            let columns: Vec<(SourceExpr, String)> = ttm
                .columns
                .iter()
                .filter(|c| pair.enabled_cols.contains(&c.name))
                .filter(|c| Some(c.name.as_str()) != generated_pk)
                .filter_map(|c| pair.mapping.get(&c.name).map(|s| (s.clone(), c.name.clone())))
                .filter(|(s, _)| !matches!(s, SourceExpr::Expr(e) if e.trim().is_empty()))
                .collect();
            if columns.is_empty() {
                continue;
            }
            specs.push(TransferSpec {
                source_schema: stm.schema.clone(),
                source_table: stm.name.clone(),
                target_schema: ttm.schema.clone(),
                target_table: ttm.name.clone(),
                columns,
                order_by: stm.primary_key.clone(),
                filter: (!pair.filter.trim().is_empty()).then(|| pair.filter.trim().to_owned()),
                row_limit,
                delete_first: false, // bulk clear is handled by the runtime
                key_capture,
            });
        }
        topo_by_target(specs, tmeta)
    }
}

/// Order specs so FK-referenced target tables are inserted first.
fn topo_by_target(mut specs: Vec<TransferSpec>, tmeta: &DbMeta) -> Vec<TransferSpec> {
    let key_of = |s: &TransferSpec| format!("{}.{}", s.target_schema, s.target_table);
    let deps_of = |s: &TransferSpec| -> Vec<String> {
        tmeta
            .tables
            .iter()
            .find(|t| t.schema == s.target_schema && t.name == s.target_table)
            .map(|t| {
                t.foreign_keys
                    .iter()
                    .map(|f| format!("{}.{}", f.to_schema, f.to_table))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out: Vec<TransferSpec> = Vec::with_capacity(specs.len());
    let mut placed: BTreeSet<String> = BTreeSet::new();
    while !specs.is_empty() {
        let included: BTreeSet<String> = specs.iter().map(&key_of).collect();
        let idx = specs.iter().position(|s| {
            let k = key_of(s);
            deps_of(s)
                .iter()
                .all(|d| *d == k || placed.contains(d) || !included.contains(d))
        });
        // Cycle: emit the rest in listed order rather than looping forever.
        let idx = idx.unwrap_or(0);
        let s = specs.remove(idx);
        placed.insert(key_of(&s));
        out.push(s);
    }
    out
}

/// "first_name" / "firstName" / "first-name" → "firstname".
fn squash(s: &str) -> String {
    s.to_ascii_lowercase().chars().filter(|c| *c != '_' && *c != '-').collect()
}

pub fn table_key(t: &TableMeta) -> String {
    format!("{}.{}", t.schema, t.name)
}

fn base_tables(meta: &DbMeta) -> impl Iterator<Item = &TableMeta> {
    meta.tables.iter().filter(|t| matches!(t.kind, crate::db::TableKind::Table))
}

fn conn_combo(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    label: &str,
    value: &mut Option<ConnectionId>,
    conns: &[ActiveConnection],
) -> bool {
    let mut changed = false;
    let current = value
        .and_then(|c| conns.iter().find(|a| a.conn_id == c))
        .map(|a| format!("{} · {}", a.name, a.database))
        .unwrap_or_else(|| label.to_string());
    egui::ComboBox::from_id_source(id)
        .selected_text(current)
        .width(180.0)
        .show_ui(ui, |ui| {
            for a in conns {
                let text = format!("{} · {}", a.name, a.database);
                if ui.selectable_label(*value == Some(a.conn_id), text).clicked() {
                    *value = Some(a.conn_id);
                    changed = true;
                }
            }
        });
    changed
}

pub fn draw(
    ui: &mut egui::Ui,
    tab: &mut TransferTab,
    active: &[ActiveConnection],
) -> TransferAction {
    let mut action = TransferAction::None;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Source").strong());
        if conn_combo(ui, ("tr_src", tab.id), "pick source…", &mut tab.source, active) {
            tab.source_meta = None;
            tab.remap();
            action = TransferAction::LoadSourceMeta;
        }
        ui.label("→");
        ui.label(egui::RichText::new("Target").strong());
        if conn_combo(ui, ("tr_tgt", tab.id), "pick target…", &mut tab.target, active) {
            tab.target_meta = None;
            tab.remap();
            action = TransferAction::LoadTargetMeta;
        }
        if tab.source_meta_loading || tab.target_meta_loading || tab.running {
            ui.spinner();
            if tab.running
                && ui
                    .small_button("⏹ Stop")
                    .on_hover_text(
                        "Stop after the current batch — rows already copied \
                         stay in the target",
                    )
                    .clicked()
            {
                action = TransferAction::Stop;
            }
            if !tab.progress.is_empty() {
                ui.weak(&tab.progress);
            }
        }
    });

    let specs = tab.build_specs();
    ui.horizontal(|ui| {
        ui.label("Row limit:");
        ui.add(
            egui::TextEdit::singleline(&mut tab.row_limit)
                .desired_width(70.0)
                .hint_text("all"),
        )
        .on_hover_text("Per-table cap on copied rows");
        ui.checkbox(&mut tab.delete_first, "delete target rows first")
            .on_hover_text(
                "Default appends; tick to clear every included target table \
                 (children first) before inserting",
            );
        ui.separator();
        let ready = !specs.is_empty() && !tab.running;
        if !tab.armed {
            if ui
                .add_enabled(ready, egui::Button::new("⬇ Transfer…"))
                .on_hover_text("Copy the mapped columns of every ticked pair")
                .clicked()
            {
                tab.armed = true;
            }
        } else {
            let target_label = tab
                .target
                .and_then(|c| active.iter().find(|a| a.conn_id == c))
                .map(|a| format!("{} · {}", a.name, a.database))
                .unwrap_or_else(|| "?".into());
            let text = egui::RichText::new(format!(
                "{}{} table(s) into {target_label}. Sure?",
                if tab.delete_first {
                    "Deletes the target rows of, then refills, "
                } else {
                    "Appends into "
                },
                specs.len(),
            ))
            .color(ui.visuals().error_fg_color);
            ui.scope(|ui| {
                let reserve = 190.0; // "Yes, transfer" + "Cancel"
                ui.set_max_width((ui.available_width() - reserve).max(220.0));
                ui.add(egui::Label::new(text).wrap());
            });
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Yes, transfer").color(egui::Color32::WHITE),
                    )
                    .fill(ui.visuals().error_fg_color),
                )
                .clicked()
            {
                tab.armed = false;
                action = TransferAction::Run;
            }
            if ui.button("Cancel").clicked() {
                tab.armed = false;
            }
        }
    });

    if let Some(err) = &tab.error {
        ui.colored_label(ui.visuals().error_fg_color, err);
    }
    if let Some(status) = &tab.status {
        ui.colored_label(theme::success_color(ui), status);
    }
    ui.add_space(4.0);
    ui.separator();

    let (Some(smeta), Some(tmeta)) = (tab.source_meta.clone(), tab.target_meta.clone()) else {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(
                egui::RichText::new(
                    "Pick source and target connections — every target table is \
                     listed and auto-paired with a same-named source table.",
                )
                .color(ui.visuals().weak_text_color()),
            );
        });
        return action;
    };
    let source_keys: Vec<String> = base_tables(&smeta).map(table_key).collect();

    // Pair-list toolbar.
    ui.horizontal(|ui| {
        let paired = tab.pairs.values().filter(|p| p.enabled).count();
        ui.label(
            egui::RichText::new(format!(
                "{paired} of {} table(s) selected",
                tab.pairs.len()
            ))
            .strong(),
        );
        if ui.small_button("All matched").on_hover_text("Tick every auto-paired table").clicked()
        {
            for p in tab.pairs.values_mut() {
                p.enabled = p.source.is_some();
            }
            tab.armed = false;
        }
        if ui.small_button("None").clicked() {
            for p in tab.pairs.values_mut() {
                p.enabled = false;
            }
            tab.armed = false;
        }
        if ui
            .small_button("Auto-map")
            .on_hover_text("Re-pair tables and re-match all columns by name")
            .clicked()
        {
            tab.remap();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut tab.table_filter)
                    .desired_width(160.0)
                    .hint_text("filter tables…"),
            );
        });
    });
    ui.add_space(2.0);

    let source_kind = tab
        .source
        .and_then(|c| active.iter().find(|a| a.conn_id == c))
        .map(|a| a.kind);
    let filter = tab.table_filter.trim().to_ascii_lowercase();
    egui::ScrollArea::vertical()
        .id_source(("tr_pairs", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for ttm in base_tables(&tmeta) {
                let tkey = table_key(ttm);
                if !filter.is_empty() && !tkey.to_ascii_lowercase().contains(&filter) {
                    continue;
                }
                // Pair header row: the target table fills from a chosen source.
                let mut retmap = false;
                {
                    let Some(pair) = tab.pairs.get_mut(&tkey) else { continue };
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut pair.enabled, "").changed() {
                            tab.armed = false;
                        }
                        let expanded = tab.expanded.contains(&tkey);
                        if ui
                            .small_button(if expanded { "▾" } else { "▸" })
                            .on_hover_text("Columns & mapping")
                            .clicked()
                        {
                            if expanded {
                                tab.expanded.remove(&tkey);
                            } else {
                                tab.expanded.insert(tkey.clone());
                            }
                        }
                        ui.add_sized(
                            [220.0, 18.0],
                            egui::Label::new(egui::RichText::new(&tkey).monospace().small()),
                        );
                        ui.label("←");
                        let selected = pair.source.clone().unwrap_or_else(|| "(none)".into());
                        egui::ComboBox::from_id_source(("tr_pair_src", tab.id, &tkey))
                            .selected_text(selected)
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(pair.source.is_none(), "(none)")
                                    .clicked()
                                {
                                    pair.source = None;
                                    pair.enabled = false;
                                    retmap = true;
                                }
                                for skey in &source_keys {
                                    if ui
                                        .selectable_label(
                                            pair.source.as_deref() == Some(skey),
                                            skey,
                                        )
                                        .clicked()
                                    {
                                        pair.source = Some(skey.clone());
                                        pair.enabled = true;
                                        retmap = true;
                                    }
                                }
                            });
                        let mapped = pair
                            .enabled_cols
                            .iter()
                            .filter(|c| pair.mapping.contains_key(*c))
                            .count();
                        if pair.source.is_some() {
                            ui.weak(format!("{mapped} col(s)"));
                        } else {
                            ui.weak("unmatched");
                        }
                    });
                }
                if retmap {
                    // Source changed: re-match this pair's columns by name.
                    let mut pair = tab.pairs.remove(&tkey).unwrap_or_default();
                    tab.map_pair_columns(&mut pair, ttm);
                    tab.pairs.insert(tkey.clone(), pair);
                    tab.armed = false;
                }

                // Expanded: per-pair options + column grid.
                if tab.expanded.contains(&tkey) {
                    let tab_id = tab.id;
                    let stm = tab
                        .pairs
                        .get(&tkey)
                        .and_then(|p| tab.source_table_meta(p))
                        .cloned();
                    let target_pk = tab.single_target_pk(&tkey);
                    let key_src_default =
                        tab.pairs.get(&tkey).and_then(|p| tab.key_source_col(p));
                    // Key-lookup options for this table: source FKs whose
                    // referenced source table feeds a pair capturing new ids
                    // (a pair never looks up through its own source).
                    let own_source = tab.pairs.get(&tkey).and_then(|p| p.source.clone());
                    let captured: BTreeSet<String> = tab
                        .pairs
                        .values()
                        .filter(|p| p.new_pk && p.enabled)
                        .filter_map(|p| p.source.clone())
                        .filter(|s| Some(s) != own_source.as_ref())
                        .collect();
                    let lookup_opts: Vec<(String, String)> = stm
                        .as_ref()
                        .map(|stm| {
                            stm.foreign_keys
                                .iter()
                                .filter(|f| {
                                    captured
                                        .contains(&format!("{}.{}", f.to_schema, f.to_table))
                                })
                                .map(|f| {
                                    (
                                        format!("{}.{}", f.to_schema, f.to_table),
                                        f.from_column.clone(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut toggled: Option<bool> = None;
                    {
                        let Some(pair) = tab.pairs.get_mut(&tkey) else { continue };
                        ui.indent(("tr_pair_body", &tkey), |ui| {
                            let Some(stm) = stm else {
                                ui.weak("Pick a source table to map columns.");
                                return;
                            };
                            let source_cols: Vec<String> =
                                stm.columns.iter().map(|c| c.name.clone()).collect();
                            ui.horizontal(|ui| {
                                match &target_pk {
                                    Some(pk) => {
                                        if ui
                                            .checkbox(&mut pair.new_pk, "new ids")
                                            .on_hover_text(format!(
                                                "The target generates `{pk}` instead of \
                                                 copying it; old→new ids are captured so \
                                                 FK columns of child tables translate \
                                                 through the key map"
                                            ))
                                            .changed()
                                        {
                                            toggled = Some(pair.new_pk);
                                        }
                                        if pair.new_pk {
                                            ui.label("old key:");
                                            let current = pair
                                                .key_source_col
                                                .clone()
                                                .or_else(|| key_src_default.clone())
                                                .unwrap_or_else(|| "pick…".into());
                                            egui::ComboBox::from_id_source((
                                                "tr_keysrc", tab_id, &tkey,
                                            ))
                                            .selected_text(current.clone())
                                            .width(160.0)
                                            .show_ui(ui, |ui| {
                                                for s in &source_cols {
                                                    if ui
                                                        .selectable_label(&current == s, s)
                                                        .clicked()
                                                    {
                                                        pair.key_source_col = Some(s.clone());
                                                    }
                                                }
                                            });
                                        }
                                    }
                                    None => {
                                        let mut off = false;
                                        ui.add_enabled(
                                            false,
                                            egui::Checkbox::new(&mut off, "new ids"),
                                        )
                                        .on_disabled_hover_text(
                                            "Needs a single-column primary key on the \
                                             target table",
                                        );
                                    }
                                }
                                ui.separator();
                                ui.label("WHERE");
                                ui.add(
                                    egui::TextEdit::singleline(&mut pair.filter)
                                        .desired_width(240.0)
                                        .font(egui::TextStyle::Monospace)
                                        .hint_text("source filter, optional"),
                                )
                                .on_hover_text(
                                    "Raw WHERE body in the SOURCE dialect, \
                                     e.g. status = 'active'",
                                );
                            });
                            let generated_pk = pair
                                .new_pk
                                .then(|| target_pk.clone())
                                .flatten();
                            for c in &ttm.columns {
                                draw_column_row(
                                    ui,
                                    tab_id,
                                    &tkey,
                                    pair,
                                    c,
                                    &source_cols,
                                    generated_pk.as_deref(),
                                    &lookup_opts,
                                    &captured,
                                    source_kind,
                                );
                            }
                        });
                    }
                    if let Some(on) = toggled {
                        tab.propagate_key_capture(&tkey, on);
                        tab.armed = false;
                    }
                }
            }
        });

    action
}

#[allow(clippy::too_many_arguments)]
fn draw_column_row(
    ui: &mut egui::Ui,
    tab_id: TabId,
    tkey: &str,
    pair: &mut PairState,
    c: &crate::db::ColumnSchema,
    source_cols: &[String],
    generated_pk: Option<&str>,
    lookup_opts: &[(String, String)],
    captured: &BTreeSet<String>,
    source_kind: Option<crate::db::DbKind>,
) {
    ui.horizontal(|ui| {
        // The generated PK never travels; it comes from the database.
        if Some(c.name.as_str()) == generated_pk {
            ui.add_space(22.0);
            ui.add_sized(
                [260.0, 18.0],
                egui::Label::new(
                    egui::RichText::new(format!("{} · {}  🔑", c.name, c.type_name))
                        .monospace()
                        .small(),
                ),
            );
            ui.weak("⚙ generated (new ids)");
            return;
        }
        let mut on = pair.enabled_cols.contains(&c.name);
        if ui.checkbox(&mut on, "").changed() {
            if on {
                pair.enabled_cols.insert(c.name.clone());
            } else {
                pair.enabled_cols.remove(&c.name);
            }
        }
        let mut label = format!("{} · {}", c.name, c.type_name);
        if c.is_primary_key {
            label.push_str("  🔑");
        } else if !c.nullable {
            label.push_str("  (not null)");
        }
        ui.add_sized(
            [260.0, 18.0],
            egui::Label::new(egui::RichText::new(label).monospace().small()),
        );
        ui.label("←");
        let current = pair.mapping.get(&c.name).cloned();
        let selected_text = match &current {
            None => "(none)".to_owned(),
            Some(SourceExpr::Column(s)) => s.clone(),
            Some(SourceExpr::Expr(_)) => "ƒ expression".to_owned(),
            Some(SourceExpr::KeyLookup { table, column }) => {
                format!("↪ {column} via {}", table.rsplit('.').next().unwrap_or(table))
            }
        };
        egui::ComboBox::from_id_source(("tr_map", tab_id, tkey, &c.name))
            .selected_text(selected_text)
            .width(200.0)
            .show_ui(ui, |ui| {
                if ui.selectable_label(current.is_none(), "(none)").clicked() {
                    pair.mapping.remove(&c.name);
                }
                for s in source_cols {
                    let sel = matches!(&current, Some(SourceExpr::Column(cur)) if cur == s);
                    if ui.selectable_label(sel, s).clicked() {
                        pair.mapping.insert(c.name.clone(), SourceExpr::Column(s.clone()));
                        pair.enabled_cols.insert(c.name.clone());
                    }
                }
                for (parent, from_col) in lookup_opts {
                    let opt = SourceExpr::KeyLookup {
                        table: parent.clone(),
                        column: from_col.clone(),
                    };
                    let sel = current.as_ref() == Some(&opt);
                    let parent_short = parent.rsplit('.').next().unwrap_or(parent);
                    if ui
                        .selectable_label(sel, format!("↪ {from_col} via {parent_short}"))
                        .on_hover_text(
                            "Translate the old key through the parent's \
                             captured new-id map",
                        )
                        .clicked()
                    {
                        pair.mapping.insert(c.name.clone(), opt);
                        pair.enabled_cols.insert(c.name.clone());
                    }
                }
                // Sources without declared FKs (common in reporting DBs)
                // still deserve lookups: offer the currently-mapped column
                // against every capturing parent.
                let manual_col = match &current {
                    Some(SourceExpr::Column(c)) => Some(c.clone()),
                    Some(SourceExpr::KeyLookup { column, .. }) => Some(column.clone()),
                    _ => None,
                };
                if let Some(cur) = manual_col {
                    for parent in captured {
                        if lookup_opts.iter().any(|(p, f)| p == parent && *f == cur) {
                            continue;
                        }
                        let opt = SourceExpr::KeyLookup {
                            table: parent.clone(),
                            column: cur.clone(),
                        };
                        let sel = current.as_ref() == Some(&opt);
                        let parent_short = parent.rsplit('.').next().unwrap_or(parent);
                        if ui
                            .selectable_label(sel, format!("↪ {cur} via {parent_short}"))
                            .on_hover_text(
                                "No FK is declared between these tables — \
                                 translate this column through the parent's \
                                 captured new-id map anyway",
                            )
                            .clicked()
                        {
                            pair.mapping.insert(c.name.clone(), opt);
                            pair.enabled_cols.insert(c.name.clone());
                        }
                    }
                }
                let is_expr = matches!(&current, Some(SourceExpr::Expr(_)));
                if ui
                    .selectable_label(is_expr, "ƒ expression…")
                    .on_hover_text(
                        "Raw SQL evaluated on the SOURCE per row — concat columns, \
                         build JSON, cast, …",
                    )
                    .clicked()
                    && !is_expr
                {
                    pair.mapping.insert(c.name.clone(), SourceExpr::Expr(String::new()));
                    pair.enabled_cols.insert(c.name.clone());
                }
            });
        if let Some(SourceExpr::Expr(_)) = &current {
            if let Some(SourceExpr::Expr(text)) = pair.mapping.get_mut(&c.name) {
                // The expression runs on the SOURCE — hint in its dialect.
                let hint = match source_kind {
                    Some(crate::db::DbKind::MsSql | crate::db::DbKind::MySql) => {
                        "CONCAT(first_name, ' ', last_name)"
                    }
                    _ => "first_name || ' ' || last_name",
                };
                ui.add(
                    egui::TextEdit::singleline(text)
                        .desired_width(260.0)
                        .font(egui::TextStyle::Monospace)
                        .hint_text(hint),
                );
            }
        }
        let unmapped = match pair.mapping.get(&c.name) {
            None => true,
            Some(SourceExpr::Expr(e)) => e.trim().is_empty(),
            Some(SourceExpr::Column(_) | SourceExpr::KeyLookup { .. }) => false,
        };
        if pair.enabled_cols.contains(&c.name) && unmapped {
            ui.colored_label(ui.visuals().warn_fg_color, "no source column");
        } else if !pair.enabled_cols.contains(&c.name) && !c.nullable && !c.is_primary_key {
            ui.weak("needs a default").on_hover_text(
                "NOT NULL and excluded from the insert — the transfer fails \
                 unless the column has a database default",
            );
        }
    });
}
