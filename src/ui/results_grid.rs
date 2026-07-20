use egui_extras::{Column, TableBuilder};

use super::theme;
use crate::db::{ResultSet, Value};

const INDEX_COL_W: f32 = 44.0;
const DATA_COL_W: f32 = 160.0;

/// True for value kinds that read better right-aligned (numbers).
fn is_numeric(v: &Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_))
}

/// Render a single data value inside a table cell with consistent styling:
/// monospace text, muted italic NULL, right-aligned numbers.
pub fn value_cell(ui: &mut egui::Ui, v: &Value) {
    if matches!(v, Value::Null) {
        ui.add(egui::Label::new(
            egui::RichText::new("NULL")
                .monospace()
                .italics()
                .color(theme::null_color(ui)),
        ));
        return;
    }
    let full = v.display();
    let preview: String = full.chars().take(200).collect();
    let text = egui::RichText::new(&preview).monospace();
    let label = egui::Label::new(text).truncate();
    if is_numeric(v) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(label).on_hover_text(full);
        });
    } else {
        ui.add(label).on_hover_text(full);
    }
}

pub fn draw(ui: &mut egui::Ui, result: &ResultSet) {
    if let Some(n) = result.rows_affected {
        ui.horizontal(|ui| {
            ui.colored_label(theme::success_color(ui), "✔");
            ui.label(format!("{n} row(s) affected"));
        });
        return;
    }
    if result.columns.is_empty() {
        ui.weak("Statement executed — no columns returned.");
        return;
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
    });
    ui.add_space(2.0);

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
                    for c in &result.columns {
                        h.col(|ui| {
                            ui.add(egui::Label::new(egui::RichText::new(&c.name).strong()).truncate())
                                .on_hover_text(format!("{} · {}", c.name, c.type_name));
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
                        for v in &result.rows[i] {
                            row.col(|ui| {
                                value_cell(ui, v);
                            });
                        }
                    });
                });
        });
}
