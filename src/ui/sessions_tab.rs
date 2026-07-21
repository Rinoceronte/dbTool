//! Active-sessions monitor: who is connected, what runs, and kill switches.
//! One tab per connection; rows come from pg_stat_activity / PROCESSLIST /
//! sys.dm_exec_sessions via the normal query path.

use egui_extras::{Column, TableBuilder};

use crate::db::{DbKind, Value};

use super::{SessionsTab, TabStatus};

pub enum SessionsAction {
    None,
    Refresh,
    /// Soft-cancel the running statement (PG/MySQL).
    CancelQuery(i64),
    /// Terminate the whole session.
    Kill(i64),
}

pub fn draw(ui: &mut egui::Ui, tab: &mut SessionsTab) -> SessionsAction {
    let mut action = SessionsAction::None;

    ui.horizontal(|ui| {
        if ui.button("⟳ Refresh").clicked() {
            action = SessionsAction::Refresh;
        }
        ui.checkbox(&mut tab.auto_refresh, "auto (5s)")
            .on_hover_text("Refresh every 5 seconds while this tab is visible");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match &tab.status {
                TabStatus::Running(m) => {
                    ui.weak(m.as_str());
                    ui.spinner();
                }
                TabStatus::Error(e) => {
                    ui.colored_label(ui.visuals().error_fg_color, format!("✖ {e}"));
                }
                _ => {
                    if let Some(rs) = &tab.rows {
                        ui.weak(format!("{} session(s)", rs.rows.len()));
                    }
                }
            }
        });
    });
    ui.add_space(4.0);

    let Some(rs) = &tab.rows else {
        ui.weak("Loading…");
        return action;
    };
    if rs.columns.is_empty() {
        ui.weak("No data.");
        return action;
    }

    let can_cancel = matches!(tab.kind, DbKind::Postgres | DbKind::MySql);
    const ACTION_W: f32 = 64.0;
    const COL_W: f32 = 130.0;

    egui::ScrollArea::horizontal()
        .id_source(("sessions_scroll", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut builder = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(ACTION_W));
            for c in &rs.columns {
                let w = if c.name.eq_ignore_ascii_case("query") { 420.0 } else { COL_W };
                builder = builder.column(Column::initial(w).at_least(50.0).clip(true));
            }
            builder
                .header(22.0, |mut h| {
                    h.col(|_| {});
                    for c in &rs.columns {
                        h.col(|ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(&c.name).strong())
                                    .selectable(false)
                                    .truncate(),
                            );
                        });
                    }
                })
                .body(|mut body| {
                    for row in &rs.rows {
                        let pid = row.first().and_then(|v| match v {
                            Value::Int(n) => Some(*n),
                            Value::Text(s) => s.parse().ok(),
                            _ => None,
                        });
                        body.row(20.0, |mut r| {
                            r.col(|ui| {
                                if let Some(pid) = pid {
                                    if can_cancel
                                        && ui
                                            .small_button("✋")
                                            .on_hover_text("Cancel the running query")
                                            .clicked()
                                    {
                                        action = SessionsAction::CancelQuery(pid);
                                    }
                                    if ui
                                        .small_button("🗙")
                                        .on_hover_text("Kill the session")
                                        .clicked()
                                    {
                                        action = SessionsAction::Kill(pid);
                                    }
                                }
                            });
                            for v in row {
                                r.col(|ui| {
                                    let s = v.display();
                                    let preview: String = s.chars().take(200).collect();
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(preview).monospace(),
                                        )
                                        .selectable(false)
                                        .truncate(),
                                    )
                                    .on_hover_text(s);
                                });
                            }
                        });
                    }
                });
        });

    action
}
