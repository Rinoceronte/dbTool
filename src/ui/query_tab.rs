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
    Export,
}

pub fn draw(
    ui: &mut egui::Ui,
    tab: &mut QueryTab,
    schema_cache: Option<&Arc<SchemaCache>>,
    dialect: DbKind,
) -> QueryTabAction {
    let mut action = QueryTabAction::None;

    ui.horizontal(|ui| {
        let run = ui.add(
            egui::Button::new(egui::RichText::new("▶  Run").color(egui::Color32::WHITE))
                .fill(super::theme::ACCENT),
        );
        if run.on_hover_text("Run query (Ctrl+Enter)").clicked() {
            action = QueryTabAction::Run;
        }
        let has_result = tab
            .result
            .as_ref()
            .map(|r| !r.columns.is_empty())
            .unwrap_or(false);
        if ui
            .add_enabled(has_result, egui::Button::new("Export…"))
            .on_hover_text("Export this result to a delimited file")
            .clicked()
        {
            action = QueryTabAction::Export;
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

    let mut editor_output = None;
    egui::ScrollArea::vertical()
        .id_source(("editor_scroll", tab.id))
        .max_height(editor_height)
        .show(ui, |ui| {
            let editor = egui::TextEdit::multiline(&mut tab.sql)
                .id(editor_id)
                .desired_width(f32::INFINITY)
                .desired_rows(12)
                .code_editor()
                .font(egui::TextStyle::Monospace);
            let output = editor.show(ui);
            editor_output = Some(output);
        });

    // Ctrl+Enter runs the query.
    if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Enter)) {
        action = QueryTabAction::Run;
    }

    if let Some(output) = editor_output {
        let cursor_char = output
            .cursor_range
            .map(|r| r.primary.ccursor.index)
            .unwrap_or(tab.last_cursor_char);
        let has_focus = output.response.has_focus();
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

    if let Some(result) = &tab.result {
        // Salt with the tab id so the grid's ScrollArea/Table ids can never
        // collide with other scroll areas in the same panel (or other tabs).
        ui.push_id(("results_grid", tab.id), |ui| {
            super::results_grid::draw(ui, result);
        });
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

