use std::collections::BTreeMap;

use egui_extras::{Column, TableBuilder};

use crate::db::{PkValues, RowChanges, TableSchema, Value};

use super::{CellEdit, TableEditorTab, TabStatus};

pub enum TableEditorAction {
    None,
    Refresh,
    PrevPage,
    NextPage,
    BeginInsert,
    CancelInsert,
    CommitInsert { values: RowChanges },
    CommitPending { updates: Vec<(PkValues, RowChanges)>, deletes: Vec<PkValues> },
}

fn icon_button(
    ui: &mut egui::Ui,
    icon: &str,
    enabled: bool,
    tooltip: impl Into<egui::WidgetText>,
) -> egui::Response {
    let btn = egui::Button::new(egui::RichText::new(icon).size(18.0))
        .min_size(egui::vec2(36.0, 32.0));
    let resp = ui.add_enabled(enabled, btn);
    let tip = tooltip.into();
    if enabled {
        resp.on_hover_text(tip)
    } else {
        resp.on_disabled_hover_text(tip)
    }
}

fn extract_pk(schema: &TableSchema, row: &[Value], col_names: &[&str]) -> Option<PkValues> {
    if !schema.has_primary_key() {
        return None;
    }
    let mut pk = BTreeMap::new();
    for pk_col in &schema.primary_key {
        let idx = col_names.iter().position(|c| *c == pk_col.as_str())?;
        pk.insert(pk_col.clone(), row.get(idx)?.clone());
    }
    Some(pk)
}

fn build_commit_payload(
    tab: &TableEditorTab,
) -> Option<(Vec<(PkValues, RowChanges)>, Vec<PkValues>)> {
    let rs = tab.rows.as_ref()?;
    let ts = tab.table_schema.as_ref()?;
    let col_names: Vec<&str> = rs.columns.iter().map(|c| c.name.as_str()).collect();

    let mut updates: Vec<(PkValues, RowChanges)> = Vec::new();
    for (&row_idx, changes) in &tab.pending_edits {
        if tab.pending_deletes.contains(&row_idx) {
            continue;
        }
        let row = rs.rows.get(row_idx)?;
        let pk = extract_pk(ts, row, &col_names)?;
        updates.push((pk, changes.clone()));
    }

    let mut deletes: Vec<PkValues> = Vec::new();
    for &row_idx in &tab.pending_deletes {
        let row = rs.rows.get(row_idx)?;
        let pk = extract_pk(ts, row, &col_names)?;
        deletes.push(pk);
    }

    Some((updates, deletes))
}

pub fn draw(ui: &mut egui::Ui, tab: &mut TableEditorTab) -> TableEditorAction {
    let mut action = TableEditorAction::None;

    let has_pk = tab.table_schema.as_ref().map(|s| s.has_primary_key()).unwrap_or(false);
    let is_inserting = tab.insert_draft.is_some();
    let has_pending = tab.has_pending();

    ui.horizontal(|ui| {
        if icon_button(ui, "⟳", true, "Refresh (discards uncommitted changes)").clicked() {
            action = TableEditorAction::Refresh;
        }
        if !is_inserting {
            let can_insert = tab.table_schema.is_some() && has_pk;
            if icon_button(ui, "➕", can_insert, "Insert row").clicked() {
                action = TableEditorAction::BeginInsert;
            }
        } else {
            if icon_button(ui, "✔", true, "Commit insert").clicked() {
                if let (Some(draft), Some(ts)) = (&tab.insert_draft, &tab.table_schema) {
                    let values = build_changes_from_draft(draft, ts);
                    action = TableEditorAction::CommitInsert { values };
                }
            }
            if icon_button(ui, "✖", true, "Cancel insert").clicked() {
                action = TableEditorAction::CancelInsert;
            }
        }
        let can_delete = has_pk && !tab.selected_rows.is_empty() && !is_inserting;
        let delete_tip = match tab.selected_rows.len() {
            0 => "Delete (select rows first)".to_string(),
            1 => "Stage 1 row for deletion".to_string(),
            n => format!("Stage {n} rows for deletion"),
        };
        if icon_button(ui, "🗑", can_delete, delete_tip).clicked() {
            for idx in std::mem::take(&mut tab.selected_rows) {
                tab.pending_deletes.insert(idx);
                tab.pending_edits.remove(&idx);
            }
            tab.selection_anchor = None;
        }
        let can_commit = has_pending && !is_inserting;
        let commit_tip = {
            let e = tab.pending_edits.len();
            let d = tab.pending_deletes.len();
            match (e, d) {
                (0, 0) => "Commit (nothing staged)".to_string(),
                (e, 0) => format!("Commit {e} edit{}", if e == 1 { "" } else { "s" }),
                (0, d) => format!("Commit {d} delete{}", if d == 1 { "" } else { "s" }),
                (e, d) => format!(
                    "Commit {e} edit{}, {d} delete{}",
                    if e == 1 { "" } else { "s" },
                    if d == 1 { "" } else { "s" },
                ),
            }
        };
        if icon_button(ui, "⬆", can_commit, commit_tip).clicked() {
            if let Some((updates, deletes)) = build_commit_payload(tab) {
                action = TableEditorAction::CommitPending { updates, deletes };
            } else {
                tab.status = TabStatus::Error(
                    "Cannot commit: unable to extract primary key values.".to_string(),
                );
            }
        }
        ui.separator();
        let row_n = tab.rows.as_ref().map(|r| r.rows.len() as i64).unwrap_or(0);
        if ui
            .add_enabled(tab.offset > 0, egui::Button::new("◀ Prev"))
            .clicked()
        {
            action = TableEditorAction::PrevPage;
        }
        let end = tab.offset + row_n;
        ui.label(
            egui::RichText::new(format!("rows {}–{}", tab.offset + 1, end.max(tab.offset)))
                .color(ui.visuals().weak_text_color()),
        );
        if ui
            .add_enabled(row_n >= tab.limit, egui::Button::new("Next ▶"))
            .on_hover_text("Next page")
            .clicked()
        {
            action = TableEditorAction::NextPage;
        }
    });

    match &tab.status {
        TabStatus::Idle => {}
        TabStatus::Running(msg) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(msg);
            });
        }
        TabStatus::Error(e) => {
            ui.colored_label(ui.visuals().error_fg_color, format!("✖ {e}"));
        }
        TabStatus::Info(i) => {
            ui.weak(i);
        }
    }

    if !has_pk && tab.table_schema.is_some() {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "⚠  Table has no primary key — rows are read-only (no edit/insert/delete).",
        );
    }

    let Some(rs) = tab.rows.as_ref() else {
        ui.weak("Loading…");
        return action;
    };

    if rs.columns.is_empty() {
        ui.label("(no columns)");
        return action;
    }

    let col_names: Vec<String> = rs.columns.iter().map(|c| c.name.clone()).collect();

    // Lift out any mutations to after the body closure to avoid double-borrow on tab.
    let mut stage_edit: Option<(usize, String, Value)> = None;
    let mut begin_edit: Option<CellEdit> = None;
    let mut cancel_edit = false;
    let mut click_select: Option<(usize, egui::Modifiers)> = None;

    const INDEX_COL_W: f32 = 50.0;
    const DATA_COL_W: f32 = 140.0;
    let total_w = INDEX_COL_W + DATA_COL_W * rs.columns.len() as f32;

    let pending_bg = super::theme::pending_bg(ui);
    let delete_bg = super::theme::delete_bg(ui);
    let delete_fg = super::theme::delete_fg(ui);
    let null_fg = super::theme::null_color(ui);

    egui::ScrollArea::horizontal()
        .id_source(("table_editor_scroll", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_min_width(total_w);

            let mut builder = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(INDEX_COL_W));
            for _ in &rs.columns {
                builder = builder.column(Column::initial(DATA_COL_W).at_least(60.0).clip(true));
            }

            builder
                .header(22.0, |mut h| {
            h.col(|ui| {
                ui.strong("");
            });
            for c in &rs.columns {
                h.col(|ui| {
                    let is_pk = tab
                        .table_schema
                        .as_ref()
                        .map(|s| s.primary_key.iter().any(|pk| pk == &c.name))
                        .unwrap_or(false);
                    let label = if is_pk {
                        format!("🔑 {}", c.name)
                    } else {
                        c.name.clone()
                    };
                    ui.strong(label).on_hover_text(&c.type_name);
                });
            }
        })
        .body(|mut body| {
            // Insert-draft row at top
            if let Some(draft) = tab.insert_draft.as_mut() {
                body.row(22.0, |mut row| {
                    row.col(|ui| {
                        ui.weak("new");
                    });
                    for c in &rs.columns {
                        row.col(|ui| {
                            let entry = draft.entry(c.name.clone()).or_default();
                            ui.add(egui::TextEdit::singleline(entry).desired_width(f32::INFINITY));
                        });
                    }
                });
            }

            body.rows(20.0, rs.rows.len(), |mut row| {
                let i = row.index();
                let is_deleted = tab.pending_deletes.contains(&i);
                let selected = tab.selected_rows.contains(&i);
                row.set_selected(selected);
                row.col(|ui| {
                    if is_deleted {
                        ui.painter().rect_filled(ui.max_rect(), 0.0, delete_bg);
                    }
                    let text = format!("{}", tab.offset + i as i64 + 1);
                    let label = if is_deleted {
                        egui::Label::new(egui::RichText::new(text).color(delete_fg).strikethrough())
                    } else {
                        egui::Label::new(text)
                    };
                    let resp = ui.add(label.selectable(false).sense(egui::Sense::click()));
                    if resp.clicked() {
                        let mods = ui.input(|inp| inp.modifiers);
                        click_select = Some((i, mods));
                    }
                });
                for (col_idx, original_v) in rs.rows[i].iter().enumerate() {
                    row.col(|ui| {
                        let col_name = &col_names[col_idx];
                        let pending_val = tab
                            .pending_edits
                            .get(&i)
                            .and_then(|m| m.get(col_name));
                        let display_v = pending_val.unwrap_or(original_v);
                        let is_pending = pending_val.is_some();

                        if is_deleted {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, delete_bg);
                        } else if is_pending {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, pending_bg);
                        }

                        let is_editing = tab
                            .edit
                            .as_ref()
                            .map(|e| e.row_index == i && e.col_index == col_idx)
                            .unwrap_or(false);

                        if is_editing {
                            if let Some(edit) = tab.edit.as_mut() {
                                let resp = ui.add_sized(
                                    ui.available_size(),
                                    egui::TextEdit::singleline(&mut edit.text),
                                );
                                if !edit.focus_set {
                                    resp.request_focus();
                                    edit.focus_set = true;
                                }
                                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                                let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
                                let should_commit = enter && (resp.lost_focus() || resp.has_focus());
                                if should_commit {
                                    let new_val = Value::from_text_input(&edit.text, original_v);
                                    stage_edit = Some((i, col_names[col_idx].clone(), new_val));
                                    cancel_edit = true;
                                } else if esc || (resp.lost_focus() && !enter) {
                                    cancel_edit = true;
                                }
                            }
                        } else {
                            let is_null = matches!(display_v, Value::Null);
                            let s = display_v.display();
                            let preview: String = s.chars().take(200).collect();
                            let rich = if is_deleted {
                                egui::RichText::new(preview).monospace().color(delete_fg).strikethrough()
                            } else if is_null {
                                egui::RichText::new(preview).monospace().italics().color(null_fg)
                            } else {
                                egui::RichText::new(preview).monospace()
                            };
                            let resp = ui.add_sized(
                                ui.available_size(),
                                egui::Label::new(rich).selectable(false).sense(egui::Sense::click()),
                            );
                            let resp = resp.on_hover_text(s);
                            if resp.clicked() {
                                let mods = ui.input(|inp| inp.modifiers);
                                click_select = Some((i, mods));
                            }
                            if resp.double_clicked() && has_pk && !is_inserting && !is_deleted {
                                begin_edit = Some(CellEdit {
                                    row_index: i,
                                    col_index: col_idx,
                                    text: display_v.display(),
                                    focus_set: false,
                                });
                            }
                        }
                    });
                }
            });
        });
        });

    if cancel_edit {
        tab.edit = None;
    }
    if let Some(e) = begin_edit {
        tab.edit = Some(e);
    }
    if let Some((idx, mods)) = click_select {
        apply_selection_click(tab, idx, mods);
    }
    if let Some((row_idx, column, new_value)) = stage_edit {
        tab.pending_edits
            .entry(row_idx)
            .or_default()
            .insert(column, new_value);
    }

    action
}

fn apply_selection_click(tab: &mut TableEditorTab, idx: usize, mods: egui::Modifiers) {
    if mods.shift {
        if let Some(anchor) = tab.selection_anchor {
            let (lo, hi) = if anchor <= idx { (anchor, idx) } else { (idx, anchor) };
            tab.selected_rows.clear();
            tab.selected_rows.extend(lo..=hi);
        } else {
            tab.selected_rows.clear();
            tab.selected_rows.insert(idx);
            tab.selection_anchor = Some(idx);
        }
    } else if mods.command || mods.ctrl {
        if !tab.selected_rows.insert(idx) {
            tab.selected_rows.remove(&idx);
        }
        tab.selection_anchor = Some(idx);
    } else {
        tab.selected_rows.clear();
        tab.selected_rows.insert(idx);
        tab.selection_anchor = Some(idx);
    }
}

fn build_changes_from_draft(draft: &BTreeMap<String, String>, schema: &TableSchema) -> RowChanges {
    let mut changes = RowChanges::new();
    for col in &schema.columns {
        let Some(input) = draft.get(&col.name) else { continue };
        if input.is_empty() {
            if col.nullable {
                // leave absent so DB default or NULL applies
                continue;
            }
            if input.is_empty() {
                continue;
            }
        }
        if input == "NULL" {
            changes.insert(col.name.clone(), Value::Null);
            continue;
        }
        let placeholder = Value::Text(String::new());
        let original = guess_from_type(&col.type_name).unwrap_or(placeholder);
        changes.insert(col.name.clone(), Value::from_text_input(input, &original));
    }
    changes
}

fn guess_from_type(type_name: &str) -> Option<Value> {
    let t = type_name.to_ascii_lowercase();
    if t.contains("int") {
        Some(Value::Int(0))
    } else if t.contains("bool") {
        Some(Value::Bool(false))
    } else if t.contains("float") || t.contains("double") || t.contains("numeric") || t.contains("decimal") {
        Some(Value::Float(0.0))
    } else if t.contains("json") {
        Some(Value::Json(serde_json::Value::Null))
    } else {
        None
    }
}
