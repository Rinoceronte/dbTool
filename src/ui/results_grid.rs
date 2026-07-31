use egui_extras::{Column, TableBuilder};

use super::theme;
use crate::db::{ResultSet, Value};

const INDEX_COL_W: f32 = 44.0;
const DATA_COL_W: f32 = 160.0;

pub enum GridAction {
    None,
    /// Open the full value in the cell viewer window.
    ViewCell { title: String, content: String },
    /// Editable grids: commit this cell's new text as an UPDATE.
    CommitCell { row: usize, col: usize, text: String },
    /// Editable grids: DELETE this row (by its PK values).
    DeleteRow { row: usize },
    /// Open the find-in-results bar (query tabs only; ignored elsewhere).
    OpenFind,
    /// Header click on a sortable grid: sort in memory by this column.
    /// `additive` (shift-click) stacks the column onto the existing sort.
    SortColumn { col: usize, additive: bool },
}

/// Find-in-results state for one frame of grid drawing.
pub struct GridFind {
    /// Lowercased needle; empty disables highlighting.
    pub query_lower: String,
    /// (row, col) of the current match, tinted stronger than the rest.
    pub current: Option<(usize, usize)>,
    /// Scroll this row into view — set only on navigation frames so the
    /// user keeps free scroll the rest of the time.
    pub scroll_to_row: Option<usize>,
}

/// Case-insensitive substring test against the value's display text.
pub fn cell_matches(v: &Value, query_lower: &str) -> bool {
    !matches!(v, Value::Null) && v.display().to_lowercase().contains(query_lower)
}

/// Ascending order across value kinds: numbers numerically, everything else
/// by display text, NULLs last (reversed wholesale for DESC, so NULLs come
/// first there — same as Postgres).
pub fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => Greater,
        (_, Value::Null) => Less,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Equal),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Equal),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Equal),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => a.display().cmp(&b.display()),
    }
}

/// Sort rows in place by the given (column, descending) keys, primary first.
pub fn sort_rows(rows: &mut [Vec<Value>], keys: &[(usize, bool)]) {
    let null = Value::Null;
    rows.sort_by(|ra, rb| {
        for (col, desc) in keys {
            let a = ra.get(*col).unwrap_or(&null);
            let b = rb.get(*col).unwrap_or(&null);
            let ord = value_cmp(a, b);
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// How many bytes of a binary value the hex dump renders.
const HEX_DUMP_CAP: usize = 64 * 1024;

/// Full-value text for the cell viewer: binary becomes a hex dump, JSON is
/// pretty-printed up front, everything else is the display text.
fn viewer_content(v: &Value) -> String {
    match v {
        Value::Bytes(b) => hex_dump(b),
        Value::Json(j) => {
            serde_json::to_string_pretty(j).unwrap_or_else(|_| j.to_string())
        }
        other => other.display(),
    }
}

/// Classic offset · hex · ASCII dump, 16 bytes per line.
fn hex_dump(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let shown = &bytes[..bytes.len().min(HEX_DUMP_CAP)];
    let mut out = String::with_capacity(shown.len() * 4);
    for (li, chunk) in shown.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", li * 16);
        for i in 0..16 {
            match chunk.get(i) {
                Some(b) => {
                    let _ = write!(out, "{b:02x} ");
                }
                None => out.push_str("   "),
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        for b in chunk {
            out.push(if (0x20..0x7f).contains(b) { *b as char } else { '·' });
        }
        out.push('\n');
    }
    if bytes.len() > HEX_DUMP_CAP {
        let _ = write!(
            out,
            "… {} more bytes not shown ({} total)",
            bytes.len() - HEX_DUMP_CAP,
            bytes.len()
        );
    }
    out
}

/// True for value kinds that read better right-aligned (numbers).
fn is_numeric(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_))
}

/// Render a single data value inside a table cell with consistent styling:
/// monospace text, muted italic NULL, right-aligned numbers.
pub fn value_cell(ui: &mut egui::Ui, v: &Value) -> egui::Response {
    if matches!(v, Value::Null) {
        return ui.add(egui::Label::new(
            egui::RichText::new("NULL")
                .monospace()
                .italics()
                .color(theme::null_color(ui)),
        ));
    }
    let full = v.display();
    let preview: String = full.chars().take(200).collect();
    let text = egui::RichText::new(&preview).monospace();
    let label = egui::Label::new(text).truncate();
    if is_numeric(v) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(label).on_hover_text(full)
        })
        .inner
    } else {
        ui.add(label).on_hover_text(full)
    }
}

/// SQL literal for INSERT generation. Strings get single-quote escaping;
/// bytes fall back to NULL (not representable portably).
pub fn sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bytes(_) => "NULL".to_owned(),
        Value::Text(s) | Value::Timestamp(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Json(j) => format!("'{}'", j.to_string().replace('\'', "''")),
    }
}

fn csv_field(v: &Value) -> String {
    let s = match v {
        Value::Null => String::new(),
        other => other.display(),
    };
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

pub fn row_as_csv(columns: &[crate::db::Column], row: &[Value]) -> String {
    let header: Vec<String> = columns.iter().map(|c| csv_field(&Value::Text(c.name.clone()))).collect();
    let vals: Vec<String> = row.iter().map(csv_field).collect();
    format!("{}\n{}", header.join(","), vals.join(","))
}

pub fn row_as_insert(target: &str, columns: &[crate::db::Column], row: &[Value]) -> String {
    let cols: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    let vals: Vec<String> = row.iter().map(sql_literal).collect();
    format!(
        "INSERT INTO {target} ({}) VALUES ({});",
        cols.join(", "),
        vals.join(", ")
    )
}

pub fn rows_as_markdown(columns: &[crate::db::Column], rows: &[Vec<Value>]) -> String {
    let mut out = String::new();
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    out.push_str(&format!("| {} |\n", names.join(" | ")));
    out.push_str(&format!("|{}\n", " --- |".repeat(names.len())));
    for r in rows {
        let vals: Vec<String> = r
            .iter()
            .map(|v| v.display().replace('|', "\\|").replace('\n', " "))
            .collect();
        out.push_str(&format!("| {} |\n", vals.join(" | ")));
    }
    out
}

/// Shared cell context menu. `insert_target` is the table name used in
/// generated INSERTs ("your_table" when unknown).
pub fn cell_context_menu(
    ui: &mut egui::Ui,
    result: &ResultSet,
    row_idx: usize,
    col_idx: usize,
    insert_target: &str,
    action: &mut GridAction,
) {
    cell_context_menu_impl(ui, result, row_idx, col_idx, insert_target, false, action)
}

fn cell_context_menu_impl(
    ui: &mut egui::Ui,
    result: &ResultSet,
    row_idx: usize,
    col_idx: usize,
    insert_target: &str,
    editable: bool,
    action: &mut GridAction,
) {
    let v = &result.rows[row_idx][col_idx];
    let col_name = &result.columns[col_idx].name;
    if ui.button("View cell").clicked() {
        *action = GridAction::ViewCell {
            title: format!("{col_name} (row {})", row_idx + 1),
            content: viewer_content(v),
        };
        ui.close_menu();
    }
    if ui.button("Copy cell").clicked() {
        ui.output_mut(|o| o.copied_text = v.display());
        ui.close_menu();
    }
    if ui
        .button("Find in results…")
        .on_hover_text("Ctrl+Shift+F")
        .clicked()
    {
        *action = GridAction::OpenFind;
        ui.close_menu();
    }
    if editable {
        ui.separator();
        if !matches!(v, Value::Null) && ui.button("Set cell to NULL").clicked() {
            *action = GridAction::CommitCell {
                row: row_idx,
                col: col_idx,
                text: "NULL".to_owned(),
            };
            ui.close_menu();
        }
        if ui
            .button(
                egui::RichText::new("Delete row").color(ui.visuals().error_fg_color),
            )
            .clicked()
        {
            *action = GridAction::DeleteRow { row: row_idx };
            ui.close_menu();
        }
    }
    ui.separator();
    if ui.button("Copy row as CSV").clicked() {
        ui.output_mut(|o| {
            o.copied_text = row_as_csv(&result.columns, &result.rows[row_idx]);
        });
        ui.close_menu();
    }
    if ui.button("Copy row as INSERT").clicked() {
        ui.output_mut(|o| {
            o.copied_text = row_as_insert(insert_target, &result.columns, &result.rows[row_idx]);
        });
        ui.close_menu();
    }
    if ui.button("Copy all rows as CSV").clicked() {
        let mut out = String::new();
        let header: Vec<String> = result
            .columns
            .iter()
            .map(|c| csv_field(&Value::Text(c.name.clone())))
            .collect();
        out.push_str(&header.join(","));
        out.push('\n');
        for r in &result.rows {
            let vals: Vec<String> = r.iter().map(csv_field).collect();
            out.push_str(&vals.join(","));
            out.push('\n');
        }
        ui.output_mut(|o| o.copied_text = out);
        ui.close_menu();
    }
    if ui.button("Copy all rows as Markdown").clicked() {
        ui.output_mut(|o| o.copied_text = rows_as_markdown(&result.columns, &result.rows));
        ui.close_menu();
    }
    if ui.button("Copy all rows as JSON").clicked() {
        let mut out = String::from("[");
        for (i, r) in result.rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n  ");
            out.push_str(&crate::csv_export::row_as_json(&result.columns, r));
        }
        out.push_str("\n]\n");
        ui.output_mut(|o| o.copied_text = out);
        ui.close_menu();
    }
    ui.separator();
    if ui.button("Copy column names").clicked() {
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        ui.output_mut(|o| o.copied_text = names.join(", "));
        ui.close_menu();
    }
}

/// count / nulls / distinct plus sum·avg·min·max for numeric columns —
/// DataGrip's aggregate footer, as an on-demand popup.
fn column_stats(result: &ResultSet, col: usize) -> String {
    let mut count = 0usize;
    let mut nulls = 0usize;
    let mut numeric: Vec<f64> = Vec::new();
    let mut distinct: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut min_text: Option<String> = None;
    let mut max_text: Option<String> = None;
    for r in &result.rows {
        let v = &r[col];
        count += 1;
        match v {
            Value::Null => {
                nulls += 1;
                continue;
            }
            Value::Int(i) => numeric.push(*i as f64),
            Value::Float(f) => numeric.push(*f),
            _ => {}
        }
        let s = v.display();
        match &mut min_text {
            Some(m) if *m <= s => {}
            m => *m = Some(s.clone()),
        }
        match &mut max_text {
            Some(m) if *m >= s => {}
            m => *m = Some(s.clone()),
        }
        distinct.insert(s);
    }
    let mut out = format!(
        "rows:      {count}\nnulls:     {nulls}\ndistinct:  {}\n",
        distinct.len()
    );
    if !numeric.is_empty() {
        let sum: f64 = numeric.iter().sum();
        let avg = sum / numeric.len() as f64;
        let min = numeric.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = numeric.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        out.push_str(&format!(
            "\nsum:       {sum}\navg:       {avg}\nmin:       {min}\nmax:       {max}\n"
        ));
    } else if let (Some(min), Some(max)) = (min_text, max_text) {
        out.push_str(&format!("\nmin:       {min}\nmax:       {max}\n"));
    }
    out
}

pub fn draw(ui: &mut egui::Ui, result: &ResultSet) -> GridAction {
    draw_impl(ui, result, None, None, None)
}

/// Grid with in-place cell editing (single-table SELECTs whose PK is in the
/// projection): double-click a cell, Enter commits, Esc cancels.
pub fn draw_editable(
    ui: &mut egui::Ui,
    result: &ResultSet,
    edit: &mut Option<super::CellEdit>,
) -> GridAction {
    draw_impl(ui, result, Some(edit), None, None)
}

/// Grid with optional editing, find-in-results highlighting, and — when
/// `sort` is Some — click-to-sort headers showing the active sort keys.
pub fn draw_with_find(
    ui: &mut egui::Ui,
    result: &ResultSet,
    edit: Option<&mut Option<super::CellEdit>>,
    find: Option<&GridFind>,
    sort: Option<&[(usize, bool)]>,
) -> GridAction {
    draw_impl(ui, result, edit, find, sort)
}

fn draw_impl(
    ui: &mut egui::Ui,
    result: &ResultSet,
    mut edit: Option<&mut Option<super::CellEdit>>,
    find: Option<&GridFind>,
    sort: Option<&[(usize, bool)]>,
) -> GridAction {
    let mut action = GridAction::None;
    if let Some(n) = result.rows_affected {
        ui.horizontal(|ui| {
            ui.colored_label(theme::success_color(ui), "✔");
            ui.label(format!("{n} row(s) affected"));
        });
        return action;
    }
    if result.columns.is_empty() {
        ui.weak("Statement executed — no columns returned.");
        return action;
    }

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!(
                "{} row{} × {} column{}",
                result.rows.len(),
                if result.rows.len() == 1 { "" } else { "s" },
                result.columns.len(),
                if result.columns.len() == 1 { "" } else { "s" },
            ))
            .small()
            .color(ui.visuals().weak_text_color()),
        );
        if edit.is_some() {
            ui.label(
                egui::RichText::new("✏ double-click a cell to edit")
                    .small()
                    .color(ui.visuals().weak_text_color()),
            );
        }
    });
    ui.add_space(2.0);

    let editing_cell = edit
        .as_deref()
        .and_then(|o| o.as_ref())
        .map(|e| (e.row_index, e.col_index));
    let mut finish_edit = false;
    let mut begin_edit: Option<super::CellEdit> = None;

    let total_w = INDEX_COL_W + DATA_COL_W * result.columns.len() as f32;
    egui::ScrollArea::horizontal()
        .id_source("results_grid_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_min_width(total_w);

            let mut builder = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(INDEX_COL_W));
            if let Some(r) = find.and_then(|f| f.scroll_to_row) {
                builder = builder.scroll_to_row(r, Some(egui::Align::Center));
            }
            for _ in &result.columns {
                builder = builder.column(Column::initial(DATA_COL_W).at_least(40.0).clip(true));
            }

            builder
                .header(24.0, |mut h| {
                    h.col(|ui| {
                        ui.add(egui::Label::new(
                            egui::RichText::new("#")
                                .small()
                                .color(ui.visuals().weak_text_color()),
                        ));
                    });
                    for (ci, c) in result.columns.iter().enumerate() {
                        h.col(|ui| {
                            let mut label = c.name.clone();
                            let keys = sort.unwrap_or(&[]);
                            if let Some(pos) = keys.iter().position(|(col, _)| *col == ci) {
                                let arrow = if keys[pos].1 { "▼" } else { "▲" };
                                label.push_str(&format!(" {arrow}"));
                                if keys.len() > 1 {
                                    label.push_str(&(pos + 1).to_string());
                                }
                            }
                            let hover = if sort.is_some() {
                                format!(
                                    "{} · {} — click to sort, shift-click to add \
                                     to sort, right-click for column stats",
                                    c.name, c.type_name
                                )
                            } else {
                                format!(
                                    "{} · {} — right-click for column stats",
                                    c.name, c.type_name
                                )
                            };
                            let resp = ui
                                .add(
                                    egui::Label::new(egui::RichText::new(label).strong())
                                        .truncate()
                                        .sense(egui::Sense::click()),
                                )
                                .on_hover_text(hover);
                            if sort.is_some() && resp.clicked() {
                                let additive = ui.input(|i| i.modifiers.shift);
                                action = GridAction::SortColumn { col: ci, additive };
                            }
                            resp.context_menu(|ui| {
                                if ui.button("Column stats").clicked() {
                                    action = GridAction::ViewCell {
                                        title: format!("Stats · {}", c.name),
                                        content: column_stats(result, ci),
                                    };
                                    ui.close_menu();
                                }
                                if ui.button("Copy column values").clicked() {
                                    let vals: Vec<String> = result
                                        .rows
                                        .iter()
                                        .map(|r| r[ci].display())
                                        .collect();
                                    ui.output_mut(|o| o.copied_text = vals.join("\n"));
                                    ui.close_menu();
                                }
                            });
                        });
                    }
                })
                .body(|body| {
                    body.rows(20.0, result.rows.len(), |mut row| {
                        let i = row.index();
                        row.col(|ui| {
                            ui.add(egui::Label::new(
                                egui::RichText::new((i + 1).to_string())
                                    .monospace()
                                    .color(ui.visuals().weak_text_color()),
                            ));
                        });
                        for (ci, v) in result.rows[i].iter().enumerate() {
                            row.col(|ui| {
                                if editing_cell == Some((i, ci)) {
                                    if let Some(e) =
                                        edit.as_deref_mut().and_then(|o| o.as_mut())
                                    {
                                        let resp = ui.add_sized(
                                            ui.available_size(),
                                            egui::TextEdit::singleline(&mut e.text),
                                        );
                                        if !e.focus_set {
                                            resp.request_focus();
                                            e.focus_set = true;
                                        }
                                        let enter =
                                            ui.input(|inp| inp.key_pressed(egui::Key::Enter));
                                        let esc =
                                            ui.input(|inp| inp.key_pressed(egui::Key::Escape));
                                        if enter && (resp.lost_focus() || resp.has_focus()) {
                                            action = GridAction::CommitCell {
                                                row: i,
                                                col: ci,
                                                text: e.text.clone(),
                                            };
                                            finish_edit = true;
                                        } else if esc || (resp.lost_focus() && !enter) {
                                            finish_edit = true;
                                        }
                                    }
                                    return;
                                }
                                let resp = value_cell(ui, v);
                                if let Some(f) = find {
                                    if !f.query_lower.is_empty()
                                        && cell_matches(v, &f.query_lower)
                                    {
                                        let rect = ui.max_rect();
                                        if f.current == Some((i, ci)) {
                                            ui.painter().rect(
                                                rect,
                                                2.0,
                                                egui::Color32::from_rgba_unmultiplied(
                                                    255, 170, 0, 46,
                                                ),
                                                egui::Stroke::new(1.0, theme::ACCENT),
                                            );
                                        } else {
                                            ui.painter().rect_filled(
                                                rect,
                                                2.0,
                                                egui::Color32::from_rgba_unmultiplied(
                                                    255, 213, 0, 28,
                                                ),
                                            );
                                        }
                                    }
                                }
                                let resp = if edit.is_some() {
                                    resp.interact(egui::Sense::click())
                                } else {
                                    resp
                                };
                                if edit.is_some() && resp.double_clicked() {
                                    begin_edit = Some(super::CellEdit {
                                        row_index: i,
                                        col_index: ci,
                                        text: v.display(),
                                        focus_set: false,
                                    });
                                }
                                let editable = edit.is_some();
                                resp.context_menu(|ui| {
                                    cell_context_menu_impl(
                                        ui, result, i, ci, "your_table", editable, &mut action,
                                    );
                                });
                            });
                        }
                    });
                });
        });
    if let Some(o) = edit.as_deref_mut() {
        if finish_edit {
            *o = None;
        }
        if let Some(b) = begin_edit {
            *o = Some(b);
        }
    }
    action
}
