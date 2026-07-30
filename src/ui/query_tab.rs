use std::sync::Arc;

use egui::text::CCursor;
use egui::text_selection::CCursorRange;

use super::completion_popup::{self, PopupAction};
use super::{QueryTab, TabStatus};
use crate::db::DbKind;
use crate::sql_complete::{self, lexer, CompletionItem, CompletionRequest, SchemaCache};

pub enum QueryTabAction {
    None,
    Run,
    /// Abort the in-flight run.
    Cancel,
    /// Run the dialect's EXPLAIN wrapping of the current SQL.
    Explain,
    /// Run the dialect's structured EXPLAIN and show the plan tree.
    ExplainVisual,
    Export,
    OpenFile,
    SaveFile,
    History,
    /// Open the snippets popup targeting this tab.
    Snippets,
    ViewCell { title: String, content: String },
    /// Editable results: run an UPDATE for one cell.
    CommitCell { row: usize, col: usize, text: String },
    /// Editable results: INSERT the drafted row.
    InsertRow { values: crate::db::RowChanges },
    /// Editable results: DELETE the row (by PK).
    DeleteRow { row: usize },
    /// Re-run the last query without the row cap.
    FetchAll,
    /// Commit (true) or roll back (false) the tab's manual transaction.
    TxEnd { commit: bool },
}

pub fn draw(
    ui: &mut egui::Ui,
    tab: &mut QueryTab,
    schema_cache: Option<&Arc<SchemaCache>>,
    dialect: DbKind,
    line_numbers: bool,
    tx_supported: bool,
) -> QueryTabAction {
    let mut action = QueryTabAction::None;

    let running = matches!(tab.status, TabStatus::Running(_));
    ui.horizontal(|ui| {
        if running {
            let stop = ui.add(
                egui::Button::new(egui::RichText::new("⏹  Stop").color(egui::Color32::WHITE))
                    .fill(ui.visuals().error_fg_color),
            );
            if stop.on_hover_text("Cancel the running query").clicked() {
                action = QueryTabAction::Cancel;
            }
        } else {
            let label = if tab.selected_sql.is_some() {
                "▶  Run selection"
            } else {
                "▶  Run"
            };
            let run = ui.add(
                egui::Button::new(egui::RichText::new(label).color(egui::Color32::WHITE))
                    .fill(super::theme::ACCENT),
            );
            if run
                .on_hover_text("Run query (Ctrl+Enter) — runs only the selection when text is selected")
                .clicked()
            {
                action = QueryTabAction::Run;
            }
        }
        if ui
            .add_enabled(!running, egui::Button::new("Explain"))
            .on_hover_text("Show the execution plan as text")
            .clicked()
        {
            action = QueryTabAction::Explain;
        }
        if ui
            .add_enabled(!running, egui::Button::new("🌳 Plan"))
            .on_hover_text("Show the execution plan as a tree")
            .clicked()
        {
            action = QueryTabAction::ExplainVisual;
        }
        let has_result = tab
            .current_result()
            .map(|r| !r.columns.is_empty())
            .unwrap_or(false);
        if ui
            .add_enabled(has_result, egui::Button::new("Export…"))
            .on_hover_text("Export this result to a delimited file")
            .clicked()
        {
            action = QueryTabAction::Export;
        }
        if tx_supported {
            ui.separator();
            if ui
                .add_enabled(
                    !tab.tx_open && !tab.tx_busy,
                    egui::SelectableLabel::new(tab.manual_commit, "🔒 manual"),
                )
                .on_hover_text(
                    "Manual commit: every Run goes into one transaction on a \
                     dedicated connection until you Commit or Rollback",
                )
                .clicked()
            {
                tab.manual_commit = !tab.manual_commit;
            }
            if tab.tx_busy {
                ui.spinner();
            } else if tab.tx_open {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!(
                        "⛁ tx · {} stmt{}",
                        tab.tx_statements,
                        if tab.tx_statements == 1 { "" } else { "s" }
                    ),
                );
                if ui
                    .add_enabled(
                        !running,
                        egui::Button::new(
                            egui::RichText::new("Commit")
                                .color(super::theme::success_color(ui)),
                        ),
                    )
                    .clicked()
                {
                    action = QueryTabAction::TxEnd { commit: true };
                }
                if ui
                    .add_enabled(
                        !running,
                        egui::Button::new(
                            egui::RichText::new("Rollback")
                                .color(ui.visuals().error_fg_color),
                        ),
                    )
                    .clicked()
                {
                    action = QueryTabAction::TxEnd { commit: false };
                }
            }
        }
        ui.separator();
        if ui.button("📂").on_hover_text("Open .sql file…").clicked() {
            action = QueryTabAction::OpenFile;
        }
        let save_tip = match &tab.file_path {
            Some(p) => format!("Save to {} (Ctrl+S)", p.display()),
            None => "Save as .sql file… (Ctrl+S)".to_owned(),
        };
        if ui.button("💾").on_hover_text(save_tip).clicked() {
            action = QueryTabAction::SaveFile;
        }
        if ui.button("🕓").on_hover_text("Query history").clicked() {
            action = QueryTabAction::History;
        }
        if ui.button("📋").on_hover_text("Snippets — saved SQL").clicked() {
            action = QueryTabAction::Snippets;
        }
        let refresh_label = match tab.auto_refresh_secs {
            None => "⟳ auto".to_owned(),
            Some(s) => format!("⟳ {s}s"),
        };
        egui::ComboBox::from_id_source(("auto_refresh", tab.id))
            .selected_text(refresh_label)
            .width(76.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut tab.auto_refresh_secs, None, "Off");
                for s in [5u32, 15, 30, 60] {
                    ui.selectable_value(&mut tab.auto_refresh_secs, Some(s), format!("every {s}s"));
                }
            })
            .response
            .on_hover_text("Automatically re-run the last query on an interval");
        if ui
            .button("¶")
            .on_hover_text("Format SQL (whole buffer)")
            .clicked()
        {
            format_sql(tab);
        }
        if ui.button("🔍").on_hover_text("Find / replace (Ctrl+F)").clicked() {
            tab.find_open = !tab.find_open;
            tab.find_focus = tab.find_open;
        }
        ui.label(
            egui::RichText::new("Ctrl+Enter to run · Ctrl+Space for completions")
                .small()
                .color(ui.visuals().weak_text_color()),
        );

        // Status pinned to the right so it never pushes the editor around.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match &tab.status {
                TabStatus::Idle => {}
                TabStatus::Running(msg) => {
                    ui.weak(msg);
                    ui.spinner();
                }
                TabStatus::Error(e) => {
                    ui.add(egui::Label::new(
                        egui::RichText::new(format!("✖ {e}")).color(ui.visuals().error_fg_color),
                    ).truncate())
                    .on_hover_text(e);
                }
                TabStatus::Info(i) => {
                    ui.add(egui::Label::new(
                        egui::RichText::new(i).color(ui.visuals().weak_text_color()),
                    ).truncate());
                }
            }
        });
    });
    ui.add_space(4.0);

    // Handle completion popup FIRST so its key consumption precedes the TextEdit.
    // Re-draw is triggered each frame anyway.
    let popup_action = completion_popup::draw(ui.ctx(), &mut tab.completion);

    // Editor.
    let editor_height = (ui.available_height() * 0.4).max(120.0);
    let editor_id = egui::Id::new(("query_editor", tab.id));

    // Pre-typing: detect Ctrl+Space for forced popup open.
    let ctrl_space = ui.ctx().input_mut(|i| {
        i.consume_key(egui::Modifiers::CTRL, egui::Key::Space)
    });
    if ctrl_space {
        tab.force_reopen = true;
    }

    // Ctrl+Shift+F opens find-in-results over the grid (checked before the
    // plain Ctrl+F editor find so the combos don't shadow each other).
    if ui.ctx().input_mut(|i| {
        i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::F)
    }) {
        tab.grid_find_open = true;
        tab.grid_find_focus = true;
    }

    // Ctrl+F toggles find/replace, seeded from a single-line selection.
    if ui
        .ctx()
        .input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::F))
    {
        tab.find_open = true;
        tab.find_focus = true;
        if let Some(sel) = &tab.selected_sql {
            if !sel.contains('\n') {
                tab.find_text = sel.clone();
            }
        }
    }
    // Grid-find Escape is consumed so it doesn't also close the editor find.
    if tab.grid_find_open
        && !tab.completion.open
        && ui
            .ctx()
            .input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        tab.grid_find_open = false;
    }
    if tab.find_open && !tab.completion.open && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        tab.find_open = false;
    }
    if tab.find_open {
        draw_find_bar(ui, tab);
        ui.add_space(4.0);
    }

    // If we previously accepted a completion and scheduled a new cursor position,
    // push it into the TextEdit state before drawing.
    if let Some(pos) = tab.pending_cursor.take() {
        if let Some(mut state) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id)
        {
            state.cursor.set_char_range(Some(CCursorRange {
                primary: CCursor::new(pos),
                secondary: CCursor::new(pos),
            }));
            state.store(ui.ctx(), editor_id);
        }
    }
    // Scheduled selection (find navigation, error jumps).
    if let Some((a, b)) = tab.pending_selection.take() {
        if let Some(mut state) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), editor_id)
        {
            state.cursor.set_char_range(Some(CCursorRange {
                primary: CCursor::new(b),
                secondary: CCursor::new(a),
            }));
            state.store(ui.ctx(), editor_id);
        }
    }

    let mut editor_output = None;
    let panel_width = ui.available_width();
    // Find-match highlights, painted by the layouter so they are visible even
    // while the find field (not the editor) holds focus.
    let highlights: Vec<(usize, usize)> = if tab.find_open && !tab.find_text.is_empty() {
        find_matches(&tab.sql, &tab.find_text)
    } else {
        Vec::new()
    };
    let current_match = tab.find_index;
    egui::ScrollArea::both()
        .id_source(("editor_scroll", tab.id))
        .max_height(editor_height)
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let gutter_width = if line_numbers {
                    super::line_number_gutter(ui, &tab.sql)
                } else {
                    0.0
                };
                // No-wrap keeps rows 1:1 with source lines (gutter alignment);
                // long lines scroll horizontally.
                let mut layouter = |ui: &egui::Ui, text: &str, _wrap_width: f32| {
                    if highlights.is_empty() {
                        super::layout_code_no_wrap(ui, text)
                    } else {
                        layout_with_matches(ui, text, &highlights, current_match)
                    }
                };
                let editor_width =
                    (panel_width - gutter_width - ui.spacing().item_spacing.x * 2.0).max(200.0);
                let editor = egui::TextEdit::multiline(&mut tab.sql)
                    .id(editor_id)
                    .desired_width(editor_width)
                    .desired_rows(12)
                    .code_editor()
                    .font(egui::TextStyle::Monospace)
                    .layouter(&mut layouter);
                let output = editor.show(ui);
                if let Some(pos) = tab.scroll_to_char.take() {
                    let cur = output.galley.from_ccursor(CCursor::new(pos));
                    let r = output
                        .galley
                        .pos_from_cursor(&cur)
                        .translate(output.galley_pos.to_vec2());
                    ui.scroll_to_rect(r.expand2(egui::vec2(0.0, 60.0)), Some(egui::Align::Center));
                }
                editor_output = Some(output);
            });
        });

    // Ctrl+Enter runs the query; Ctrl+S saves the file.
    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Enter)) && !running {
        action = QueryTabAction::Run;
    }
    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::S)) {
        action = QueryTabAction::SaveFile;
    }

    if let Some(output) = editor_output {
        let cursor_char = output
            .cursor_range
            .map(|r| r.primary.ccursor.index)
            .unwrap_or(tab.last_cursor_char);
        let has_focus = output.response.has_focus();

        // Track the selection while the editor has focus; keep the last one
        // when focus moves to a toolbar button so Run can use it.
        if has_focus {
            tab.selected_sql = output.cursor_range.and_then(|r| {
                let (a, b) = (r.primary.ccursor.index, r.secondary.ccursor.index);
                if a == b {
                    return None;
                }
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                let sel: String = tab.sql.chars().skip(lo).take(hi - lo).collect();
                (!sel.trim().is_empty()).then_some(sel)
            });
        }
        let sql_changed = tab.sql != tab.last_sql;
        let cursor_moved = cursor_char != tab.last_cursor_char;

        // Anchor: one line below the cursor in screen coords.
        let anchor = if let Some(range) = output.cursor_range {
            let rect = output.galley.pos_from_cursor(&range.primary);
            egui::pos2(
                output.galley_pos.x + rect.min.x,
                output.galley_pos.y + rect.max.y + 2.0,
            )
        } else {
            output.galley_pos
        };

        // Auto-open requires *something* to complete: a non-empty word prefix,
        // or the cursor sitting just after a `.` (dot-completion). Ctrl+Space
        // bypasses this so the user can always force the popup.
        let (prefix_at_cursor, _) = lexer::word_at_cursor(&tab.sql, cursor_char);
        let after_dot = {
            let chars_count = tab.sql.chars().count();
            cursor_char > 0
                && cursor_char <= chars_count
                && tab.sql.chars().nth(cursor_char - 1) == Some('.')
        };
        let has_trigger = !prefix_at_cursor.is_empty() || after_dot;

        // Triggers:
        //   - Ctrl+Space: force open.
        //   - SQL changed while editor has focus and a trigger is present.
        //   - Cursor moved while popup is already open and a trigger is present.
        let should_run_completion = schema_cache.is_some()
            && has_focus
            && (tab.force_reopen
                || ((sql_changed || (tab.completion.open && cursor_moved)) && has_trigger));

        if should_run_completion {
            let cache = schema_cache.unwrap();
            let items = sql_complete::complete(CompletionRequest {
                sql: &tab.sql,
                cursor_char,
                cache: cache.as_ref(),
                dialect,
            });
            if items.is_empty() {
                tab.completion.close();
            } else {
                tab.completion.refresh(items, anchor);
            }
        } else if !has_focus && !tab.force_reopen {
            // Editor lost focus → dismiss.
            tab.completion.close();
        } else if tab.completion.open && !has_trigger && !tab.force_reopen {
            // Cursor moved off the word being completed (e.g. into whitespace).
            tab.completion.close();
        }

        tab.force_reopen = false;
        tab.last_sql = tab.sql.clone();
        tab.last_cursor_char = cursor_char;

        // Handle popup action AFTER refresh so Accept uses the latest items.
        match popup_action {
            PopupAction::None => {}
            PopupAction::Dismiss => {
                tab.completion.close();
            }
            PopupAction::Accept(item) => {
                apply_completion(tab, &item);
                tab.completion.close();
            }
        }
    }

    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);

    // Result-set picker for scripts that returned several.
    if tab.results.len() > 1 {
        ui.horizontal_wrapped(|ui| {
            for (i, rs) in tab.results.iter().enumerate() {
                let label = if rs.columns.is_empty() {
                    match rs.rows_affected {
                        Some(n) => format!("{} · {n} affected", i + 1),
                        None => format!("{}", i + 1),
                    }
                } else {
                    format!("{} · {} row(s)", i + 1, rs.rows.len())
                };
                if ui.selectable_label(tab.result_idx == i, label).clicked() {
                    tab.result_idx = i;
                }
            }
        });
        ui.add_space(2.0);
    }

    // Visual plan takes over the results area until closed or a new run.
    if let Some(plan) = tab.plan.clone() {
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Execution plan").strong());
            if ui.small_button("✖").on_hover_text("Back to results").clicked() {
                close = true;
            }
        });
        ui.add_space(2.0);
        let max_cost = plan.max_cost().max(f64::MIN_POSITIVE);
        egui::ScrollArea::both()
            .id_source(("plan_scroll", tab.id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut path = 0usize;
                draw_plan_node(ui, &plan, max_cost, tab.id, &mut path);
            });
        if close {
            tab.plan = None;
        }
        return action;
    }

    if !tab.results.is_empty() {
        let idx = tab.result_idx.min(tab.results.len() - 1);
        if tab.results[idx].truncated {
            ui.horizontal(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!(
                        "⚠ Showing the first {} rows — the result was truncated.",
                        tab.results[idx].rows.len()
                    ),
                );
                if ui
                    .small_button("Fetch all rows")
                    .on_hover_text("Re-runs the query without the row cap")
                    .clicked()
                {
                    action = QueryTabAction::FetchAll;
                }
            });
            ui.add_space(2.0);
        }
        // Cell edits are only unambiguous when the run produced one set.
        let can_edit = tab.editable.is_some() && tab.results.len() == 1;
        if can_edit {
            draw_insert_row_controls(ui, tab, &mut action);
        }
        let find = draw_grid_find_bar(ui, tab, idx);
        // Salt with the tab id AND result index so each set's ScrollArea/Table
        // state stays independent.
        let grid_action = ui
            .push_id(("results_grid", tab.id, idx), |ui| {
                let edit = if can_edit { Some(&mut tab.grid_edit) } else { None };
                super::results_grid::draw_with_find(ui, &tab.results[idx], edit, find.as_ref())
            })
            .inner;
        match grid_action {
            super::results_grid::GridAction::ViewCell { title, content } => {
                action = QueryTabAction::ViewCell { title, content };
            }
            super::results_grid::GridAction::CommitCell { row, col, text } => {
                action = QueryTabAction::CommitCell { row, col, text };
            }
            super::results_grid::GridAction::DeleteRow { row } => {
                action = QueryTabAction::DeleteRow { row };
            }
            super::results_grid::GridAction::OpenFind => {
                tab.grid_find_open = true;
                tab.grid_find_focus = true;
            }
            super::results_grid::GridAction::None => {}
        }
    } else {
        ui.vertical_centered(|ui| {
            ui.add_space(20.0);
            ui.label(
                egui::RichText::new("Run a query to see results here.")
                    .color(ui.visuals().weak_text_color()),
            );
        });
    }

    action
}

/// One plan node: header (label + cost + rows, tinted by relative cost)
/// with details and children nested underneath.
fn draw_plan_node(
    ui: &mut egui::Ui,
    node: &crate::db::plan::PlanNode,
    max_cost: f64,
    tab_id: super::TabId,
    path: &mut usize,
) {
    *path += 1;
    let id = (tab_id, "plan_node", *path);

    let mut header = node.label.clone();
    if let Some(cost) = node.cost {
        header.push_str(&format!("  ·  cost {cost:.2}"));
    }
    if let Some(rows) = node.rows {
        header.push_str(&format!("  ·  ~{rows:.0} rows"));
    }
    let fraction = node.cost.map(|c| (c / max_cost) as f32).unwrap_or(0.0);
    let base = ui.visuals().text_color();
    let hot = egui::Color32::from_rgb(224, 90, 70);
    let color = egui::Color32::from_rgb(
        (base.r() as f32 + (hot.r() as f32 - base.r() as f32) * fraction) as u8,
        (base.g() as f32 + (hot.g() as f32 - base.g() as f32) * fraction) as u8,
        (base.b() as f32 + (hot.b() as f32 - base.b() as f32) * fraction) as u8,
    );
    let text = egui::RichText::new(header).monospace().color(color);

    if node.children.is_empty() && node.detail.is_empty() {
        ui.label(text);
        return;
    }
    egui::CollapsingHeader::new(text)
        .id_source(id)
        .default_open(true)
        .show(ui, |ui| {
            for (k, v) in &node.detail {
                ui.label(
                    egui::RichText::new(format!("{k}: {v}"))
                        .small()
                        .monospace()
                        .color(ui.visuals().weak_text_color()),
                );
            }
            for child in &node.children {
                draw_plan_node(ui, child, max_cost, tab_id, path);
            }
        });
}

/// "➕ Insert row" toggle plus the draft form for editable query results.
/// Columns come from the result projection; omitted/empty fields fall back
/// to the database's defaults.
fn draw_insert_row_controls(ui: &mut egui::Ui, tab: &mut QueryTab, action: &mut QueryTabAction) {
    let Some(meta) = tab.editable.clone() else { return };
    let columns: Vec<(String, String)> = tab.results[0]
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.type_name.clone()))
        .collect();

    match &mut tab.insert_draft {
        None => {
            if ui
                .small_button("➕ Insert row…")
                .on_hover_text(format!("INSERT INTO {}.{}", meta.schema, meta.table))
                .clicked()
            {
                tab.insert_draft = Some(
                    columns
                        .iter()
                        .map(|(n, _)| (n.clone(), String::new()))
                        .collect(),
                );
            }
        }
        Some(draft) => {
            let mut submit = false;
            let mut cancel = false;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Insert into {}.{}  —  empty = default, NULL = null",
                        meta.schema, meta.table
                    ))
                    .small()
                    .color(ui.visuals().weak_text_color()),
                );
                egui::ScrollArea::vertical()
                    .id_source(("insert_draft", tab.id))
                    .max_height(180.0)
                    .show(ui, |ui| {
                        egui::Grid::new(("insert_grid", tab.id))
                            .num_columns(2)
                            .spacing([10.0, 4.0])
                            .min_col_width(120.0)
                            .show(ui, |ui| {
                                for (name, ty) in &columns {
                                    ui.label(format!("{name} · {ty}"));
                                    if let Some(text) = draft.get_mut(name) {
                                        ui.add(
                                            egui::TextEdit::singleline(text)
                                                .desired_width(f32::INFINITY),
                                        );
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                ui.horizontal(|ui| {
                    if ui.button("✔ Insert").clicked() {
                        submit = true;
                    }
                    if ui.button("✖ Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
            if submit {
                let mut values = crate::db::RowChanges::new();
                for (name, ty) in &columns {
                    let Some(input) = draft.get(name) else { continue };
                    if input.is_empty() {
                        continue;
                    }
                    if input == "NULL" {
                        values.insert(name.clone(), crate::db::Value::Null);
                        continue;
                    }
                    let original = super::table_editor::guess_from_type(ty)
                        .unwrap_or(crate::db::Value::Text(String::new()));
                    values.insert(
                        name.clone(),
                        crate::db::Value::from_text_input(input, &original),
                    );
                }
                *action = QueryTabAction::InsertRow { values };
            }
            if cancel {
                tab.insert_draft = None;
            }
        }
    }
    ui.add_space(2.0);
}

/// Case-insensitive matches of `needle` in `hay`, as (char_start, char_len).
fn find_matches(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let h: Vec<char> = hay.chars().flat_map(|c| c.to_lowercase()).collect();
    let n: Vec<char> = needle.chars().flat_map(|c| c.to_lowercase()).collect();
    let mut out = Vec::new();
    if n.len() > h.len() {
        return out;
    }
    let mut i = 0;
    while i + n.len() <= h.len() {
        if h[i..i + n.len()] == n[..] {
            out.push((i, n.len()));
            i += n.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Replace `len` chars starting at char `start` with `rep`.
fn replace_chars(s: &str, start: usize, len: usize, rep: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out: String = chars[..start.min(chars.len())].iter().collect();
    out.push_str(rep);
    out.extend(chars[(start + len).min(chars.len())..].iter());
    out
}

/// Monospace layout with find-match backgrounds; the current match is louder.
fn layout_with_matches(
    ui: &egui::Ui,
    text: &str,
    matches: &[(usize, usize)],
    current: usize,
) -> Arc<egui::Galley> {
    use egui::text::{LayoutJob, LayoutSection, TextFormat};
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    let color = ui.visuals().widgets.inactive.text_color();
    let normal = TextFormat { font_id: font.clone(), color, ..Default::default() };
    let hl_bg = egui::Color32::from_rgba_unmultiplied(235, 203, 80, 70);
    let cur_bg = egui::Color32::from_rgba_unmultiplied(235, 160, 60, 140);

    // Char offsets → byte offsets in one pass.
    let mut byte_at: Vec<usize> = Vec::with_capacity(text.chars().count() + 1);
    for (b, _) in text.char_indices() {
        byte_at.push(b);
    }
    byte_at.push(text.len());

    let mut job = LayoutJob {
        text: text.to_owned(),
        ..Default::default()
    };
    job.wrap.max_width = f32::INFINITY;
    let mut push = |range: std::ops::Range<usize>, format: TextFormat| {
        if !range.is_empty() {
            job.sections.push(LayoutSection { leading_space: 0.0, byte_range: range, format });
        }
    };
    let mut pos = 0usize; // bytes
    for (mi, &(cs, cl)) in matches.iter().enumerate() {
        let (Some(&bs), Some(&be)) = (byte_at.get(cs), byte_at.get(cs + cl)) else { continue };
        if bs < pos {
            continue;
        }
        push(pos..bs, normal.clone());
        let mut fmt = normal.clone();
        fmt.background = if mi == current { cur_bg } else { hl_bg };
        push(bs..be, fmt);
        pos = be;
    }
    push(pos..text.len(), normal);
    ui.fonts(|f| f.layout_job(job))
}

/// Reformat the whole buffer with sqlformat.
fn format_sql(tab: &mut QueryTab) {
    let formatted = sqlformat::format(
        &tab.sql,
        &sqlformat::QueryParams::None,
        &sqlformat::FormatOptions {
            indent: sqlformat::Indent::Spaces(2),
            uppercase: Some(true),
            lines_between_queries: 2,
            ..Default::default()
        },
    );
    if !formatted.trim().is_empty() && formatted != tab.sql {
        tab.sql = formatted;
        tab.last_sql = tab.sql.clone();
        tab.selected_sql = None;
    }
}

fn draw_find_bar(ui: &mut egui::Ui, tab: &mut QueryTab) {
    let matches = find_matches(&tab.sql, &tab.find_text);
    let n = matches.len();
    if tab.find_index >= n {
        tab.find_index = 0;
    }
    let mut goto: Option<usize> = None;
    ui.horizontal(|ui| {
        let resp = ui.add(
            egui::TextEdit::singleline(&mut tab.find_text)
                .id(egui::Id::new(("find_field", tab.id)))
                .hint_text("find (case-insensitive)")
                .desired_width(190.0),
        );
        if tab.find_focus {
            resp.request_focus();
            tab.find_focus = false;
        }
        if resp.changed() {
            tab.find_index = 0;
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && n > 0 {
            tab.find_index = (tab.find_index + 1) % n;
            goto = Some(tab.find_index);
            tab.find_focus = true; // Enter keeps cycling
        }
        if ui.add_enabled(n > 0, egui::Button::new("◀").small()).clicked() {
            tab.find_index = (tab.find_index + n - 1) % n;
            goto = Some(tab.find_index);
        }
        if ui.add_enabled(n > 0, egui::Button::new("▶").small()).clicked() {
            tab.find_index = (tab.find_index + 1) % n;
            goto = Some(tab.find_index);
        }
        if !tab.find_text.is_empty() {
            let count = if n == 0 {
                "0 matches".to_owned()
            } else {
                format!("{}/{}", tab.find_index + 1, n)
            };
            ui.label(egui::RichText::new(count).small().color(ui.visuals().weak_text_color()));
        }
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut tab.replace_text)
                .id(egui::Id::new(("replace_field", tab.id)))
                .hint_text("replace with")
                .desired_width(160.0),
        );
        if ui.add_enabled(n > 0, egui::Button::new("Replace")).clicked() {
            if let Some(&(s, l)) = matches.get(tab.find_index) {
                tab.sql = replace_chars(&tab.sql, s, l, &tab.replace_text.clone());
                tab.last_sql = tab.sql.clone();
            }
        }
        if ui
            .add_enabled(n > 0, egui::Button::new("All"))
            .on_hover_text(format!("Replace all {n} matches"))
            .clicked()
        {
            let mut s = tab.sql.clone();
            for &(st, l) in matches.iter().rev() {
                s = replace_chars(&s, st, l, &tab.replace_text);
            }
            tab.sql = s;
            tab.last_sql = tab.sql.clone();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("✖").on_hover_text("Close (Esc)").clicked() {
                tab.find_open = false;
            }
        });
    });
    if let Some(g) = goto {
        if let Some(&(s, l)) = matches.get(g) {
            tab.pending_selection = Some((s, s + l));
            tab.scroll_to_char = Some(s);
        }
    }
}

/// Find-in-results bar over the grid. Returns the per-frame find spec the
/// grid uses for highlighting and scroll-to-match.
fn draw_grid_find_bar(
    ui: &mut egui::Ui,
    tab: &mut QueryTab,
    idx: usize,
) -> Option<super::results_grid::GridFind> {
    if !tab.grid_find_open {
        return None;
    }
    let just_opened = tab.grid_find_focus;
    let mut navigated = false;
    let mut changed = false;

    ui.horizontal(|ui| {
        ui.label("🔍");
        let resp = ui.add(
            egui::TextEdit::singleline(&mut tab.grid_find_text)
                .id(egui::Id::new(("grid_find_field", tab.id)))
                .hint_text("find in results")
                .desired_width(220.0),
        );
        if tab.grid_find_focus {
            resp.request_focus();
            tab.grid_find_focus = false;
        }
        changed = resp.changed();

        // (Re)compute matches when the needle or the shown result changed.
        let needle = tab.grid_find_text.to_lowercase();
        let stale = match &tab.grid_find_cache {
            Some((n, i, _)) => *n != needle || *i != idx,
            None => true,
        };
        if stale {
            let mut matches = Vec::new();
            if !needle.is_empty() {
                for (ri, row) in tab.results[idx].rows.iter().enumerate() {
                    for (ci, v) in row.iter().enumerate() {
                        if super::results_grid::cell_matches(v, &needle) {
                            matches.push((ri, ci));
                        }
                    }
                }
            }
            tab.grid_find_cache = Some((needle, idx, matches));
            tab.grid_find_index = 0;
        }
        let n = tab.grid_find_cache.as_ref().map_or(0, |(_, _, m)| m.len());
        if tab.grid_find_index >= n {
            tab.grid_find_index = 0;
        }

        // Enter cycles forward, Shift+Enter backward, while the field is focused.
        let (enter, shift) =
            ui.input(|i| (i.key_pressed(egui::Key::Enter), i.modifiers.shift));
        if (resp.lost_focus() || resp.has_focus()) && enter && n > 0 {
            tab.grid_find_index = if shift {
                (tab.grid_find_index + n - 1) % n
            } else {
                (tab.grid_find_index + 1) % n
            };
            navigated = true;
            tab.grid_find_focus = true; // Enter keeps cycling
        }
        if ui.add_enabled(n > 0, egui::Button::new("◀").small()).clicked() {
            tab.grid_find_index = (tab.grid_find_index + n - 1) % n;
            navigated = true;
        }
        if ui.add_enabled(n > 0, egui::Button::new("▶").small()).clicked() {
            tab.grid_find_index = (tab.grid_find_index + 1) % n;
            navigated = true;
        }
        if !tab.grid_find_text.is_empty() {
            let count = if n == 0 {
                "0 matches".to_owned()
            } else {
                format!("{}/{}", tab.grid_find_index + 1, n)
            };
            ui.label(egui::RichText::new(count).small().color(ui.visuals().weak_text_color()));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("✖").on_hover_text("Close (Esc)").clicked() {
                tab.grid_find_open = false;
            }
        });
    });
    ui.add_space(2.0);

    let (needle, matches) = match &tab.grid_find_cache {
        Some((n, _, m)) => (n.clone(), m),
        None => return None,
    };
    let current = matches.get(tab.grid_find_index).copied();
    let scroll_to_row = if changed || navigated || just_opened {
        current.map(|(r, _)| r)
    } else {
        None
    };
    Some(super::results_grid::GridFind { query_lower: needle, current, scroll_to_row })
}

fn apply_completion(tab: &mut QueryTab, item: &CompletionItem) {
    let chars: Vec<char> = tab.sql.chars().collect();
    let mut start = item.replace_from_char.min(chars.len());
    let cursor = tab.last_cursor_char.min(chars.len());
    let mut tail_start = cursor.max(start);

    // If insert_text begins with a quote that the user already typed (e.g. user
    // typed `"p`, suggestion is `"people"`), consume that leading quote so we
    // don't end up with `""people"`. Same on the trailing side if the cursor
    // sits just before a matching closing quote.
    let insert_chars: Vec<char> = item.insert_text.chars().collect();
    if let (Some(&first), Some(&last)) = (insert_chars.first(), insert_chars.last()) {
        if first == last && (first == '"' || first == '`') {
            if start > 0 && chars[start - 1] == first {
                start -= 1;
            }
            if tail_start < chars.len() && chars[tail_start] == last {
                tail_start += 1;
            }
        }
    }

    let (head, _) = chars.split_at(start);
    let tail = if tail_start <= chars.len() {
        &chars[tail_start..]
    } else {
        &[]
    };
    let new_sql: String = head
        .iter()
        .chain(insert_chars.iter())
        .chain(tail.iter())
        .collect();
    let new_cursor = start + insert_chars.len();
    tab.sql = new_sql;
    tab.pending_cursor = Some(new_cursor);
    tab.last_sql = tab.sql.clone();
    tab.last_cursor_char = new_cursor;
}

