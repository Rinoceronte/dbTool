//! DataGrip-style Import/Export dialogs: dump schema DDL to a folder of
//! .sql files, and import a delimited data file into a table.

use crate::runtime::ConnectionId;

use super::TabId;

pub struct DumpState {
    pub conn: ConnectionId,
    pub conn_name: String,
    /// None = every schema on the connection.
    pub schema: Option<String>,
    pub dir: String,
    pub running: bool,
    pub result: Option<String>,
    pub error: Option<String>,
}

pub enum DumpAction {
    None,
    Start,
    Close,
}

pub fn draw_dump(ctx: &egui::Context, state: &mut DumpState) -> DumpAction {
    let mut action = DumpAction::None;
    let mut open = true;

    let scope = match &state.schema {
        Some(s) => format!("schema '{s}'"),
        None => "all schemas".to_string(),
    };
    egui::Window::new("Dump DDL to folder")
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!(
                "Writes one .sql file per table/view of {scope} on '{}' \
                 (a folder per schema), like a DDL data source.",
                state.conn_name
            ));
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("Target folder:");
                ui.add(
                    egui::TextEdit::singleline(&mut state.dir)
                        .desired_width(f32::INFINITY)
                        .hint_text("~/dbtool-ddl/my-database"),
                );
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let can_start = !state.running && !state.dir.trim().is_empty();
                if ui.add_enabled(can_start, egui::Button::new("Dump")).clicked() {
                    action = DumpAction::Start;
                }
                if state.running {
                    ui.spinner();
                    ui.weak("dumping…");
                }
            });

            if let Some(err) = &state.error {
                ui.add_space(6.0);
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }
            if let Some(res) = &state.result {
                ui.add_space(6.0);
                egui::ScrollArea::vertical().id_source("ie_msg_scroll").max_height(140.0).show(ui, |ui| {
                    ui.label(res);
                });
            }
        });

    if !open {
        action = DumpAction::Close;
    }
    action
}

pub struct ImportState {
    pub conn: ConnectionId,
    pub schema: String,
    pub table: String,
    pub path: String,
    pub file_dialog: egui_file_dialog::FileDialog,
    /// Single-character delimiter as typed; "\t" is accepted for tabs.
    pub delimiter: String,
    pub has_header: bool,
    pub empty_as_null: bool,
    pub running: bool,
    pub progress_rows: u64,
    pub result: Option<String>,
    pub error: Option<String>,
}

pub enum ImportAction {
    None,
    Start,
    Close,
}

impl ImportState {
    pub fn delimiter_byte(&self) -> Option<u8> {
        match self.delimiter.as_str() {
            "\\t" | "\t" => Some(b'\t'),
            s if s.len() == 1 && s.is_ascii() => Some(s.as_bytes()[0]),
            _ => None,
        }
    }
}

pub fn draw_import(ctx: &egui::Context, state: &mut ImportState) -> ImportAction {
    let mut action = ImportAction::None;
    let mut open = true;

    egui::Window::new(format!("Import data into {}.{}", state.schema, state.table))
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("File:");
                if ui.button("Browse…").clicked() {
                    state.file_dialog.select_file();
                }
                ui.add(
                    egui::TextEdit::singleline(&mut state.path)
                        .desired_width(f32::INFINITY)
                        .hint_text("~/data/rows.csv"),
                );
            });

            ui.horizontal(|ui| {
                ui.label("Delimiter:");
                ui.add(egui::TextEdit::singleline(&mut state.delimiter).desired_width(36.0))
                    .on_hover_text("Single character; use \\t for tab");
                ui.add_space(12.0);
                ui.checkbox(&mut state.has_header, "First row is header")
                    .on_hover_text(
                        "Header names are matched to table columns (case-insensitive). \
                         Without a header, fields map to the table's columns in order.",
                    );
            });
            ui.checkbox(&mut state.empty_as_null, "Empty fields as NULL");

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let can_start = !state.running
                    && !state.path.trim().is_empty()
                    && state.delimiter_byte().is_some();
                if ui.add_enabled(can_start, egui::Button::new("Import")).clicked() {
                    action = ImportAction::Start;
                }
                if state.running {
                    ui.spinner();
                    ui.weak(format!("imported {} row(s)…", state.progress_rows));
                }
            });
            if state.delimiter_byte().is_none() {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    "Delimiter must be a single ASCII character (or \\t).",
                );
            }

            if let Some(err) = &state.error {
                ui.add_space(6.0);
                egui::ScrollArea::vertical().id_source("ie_msg_scroll2").max_height(140.0).show(ui, |ui| {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                });
            }
            if let Some(res) = &state.result {
                ui.add_space(6.0);
                ui.label(res);
            }
        });

    state.file_dialog.update(ctx);
    if let Some(picked) = state.file_dialog.take_selected() {
        state.path = picked.to_string_lossy().into_owned();
    }

    if !open {
        action = ImportAction::Close;
    }
    action
}

/// Whole-database dump/restore via the engine's native tool.
pub struct BackupState {
    pub conn: ConnectionId,
    pub conn_name: String,
    pub kind: crate::db::DbKind,
    /// true = restore INTO the database, false = dump FROM it.
    pub restore: bool,
    pub path: String,
    pub running: bool,
    pub result: Option<String>,
    pub error: Option<String>,
    /// Two-step confirmation for restores (they overwrite data).
    pub armed: bool,
    pub file_dialog: egui_file_dialog::FileDialog,
}

pub enum BackupAction {
    None,
    Start,
    Close,
}

pub fn draw_backup(ctx: &egui::Context, state: &mut BackupState) -> BackupAction {
    let mut action = BackupAction::None;
    let mut open = true;

    let title = if state.restore {
        format!("Restore database into '{}'", state.conn_name)
    } else {
        format!("Dump database '{}'", state.conn_name)
    };
    let tool = match (state.kind, state.restore) {
        (crate::db::DbKind::Postgres, false) => "pg_dump -Fc",
        (crate::db::DbKind::Postgres, true) => "pg_restore --clean --if-exists",
        (crate::db::DbKind::MySql, false) => "mysqldump --single-transaction",
        (crate::db::DbKind::MySql, true) => "mysql < file",
        (crate::db::DbKind::Sqlite, _) => "file copy",
        (crate::db::DbKind::MsSql, _) => "not supported for SQL Server",
    };
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!("Uses {tool}."));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("File:");
                if ui.button("Browse…").clicked() {
                    if state.restore {
                        state.file_dialog.select_file();
                    } else {
                        state.file_dialog.save_file();
                    }
                }
                ui.add(
                    egui::TextEdit::singleline(&mut state.path)
                        .desired_width(f32::INFINITY),
                );
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let can_start = !state.running && !state.path.trim().is_empty();
                if state.restore && !state.armed {
                    if ui
                        .add_enabled(can_start, egui::Button::new("Restore…"))
                        .clicked()
                    {
                        state.armed = true;
                    }
                } else if state.restore {
                    ui.label(
                        egui::RichText::new("Overwrites the database's data. Sure?")
                            .color(ui.visuals().error_fg_color),
                    );
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Yes, restore").color(egui::Color32::WHITE),
                            )
                            .fill(ui.visuals().error_fg_color),
                        )
                        .clicked()
                    {
                        state.armed = false;
                        action = BackupAction::Start;
                    }
                    if ui.button("Cancel").clicked() {
                        state.armed = false;
                    }
                } else if ui.add_enabled(can_start, egui::Button::new("Dump")).clicked() {
                    action = BackupAction::Start;
                }
                if state.running {
                    ui.spinner();
                    ui.weak("working…");
                }
            });
            if let Some(err) = &state.error {
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .id_source("backup_err")
                    .max_height(120.0)
                    .show(ui, |ui| {
                        ui.colored_label(ui.visuals().error_fg_color, err);
                    });
            }
            if let Some(res) = &state.result {
                ui.add_space(6.0);
                ui.colored_label(super::theme::success_color(ui), res);
            }
        });

    state.file_dialog.update(ctx);
    if let Some(picked) = state.file_dialog.take_selected() {
        state.path = picked.to_string_lossy().into_owned();
    }
    if !open {
        action = BackupAction::Close;
    }
    action
}

/// What an export dialog writes: a whole table, or a query tab's current result.
#[derive(Clone)]
pub enum ExportSource {
    Table { schema: String, table: String },
    QueryResult { tab_id: TabId },
}

pub struct ExportState {
    pub conn: ConnectionId,
    pub source: ExportSource,
    pub path: String,
    pub format: crate::csv_export::ExportFormat,
    /// Single-character delimiter as typed; "\t" is accepted for tabs.
    pub delimiter: String,
    pub include_header: bool,
    pub running: bool,
    pub progress_rows: u64,
    pub error: Option<String>,
    pub file_dialog: egui_file_dialog::FileDialog,
}

pub enum ExportAction {
    None,
    Start,
    Close,
}

impl ExportState {
    pub fn delimiter_byte(&self) -> Option<u8> {
        match self.delimiter.as_str() {
            "\\t" | "\t" => Some(b'\t'),
            s if s.len() == 1 && s.is_ascii() => Some(s.as_bytes()[0]),
            _ => None,
        }
    }
}

pub fn draw_export(ctx: &egui::Context, state: &mut ExportState) -> ExportAction {
    let mut action = ExportAction::None;
    let mut open = true;

    let title = match &state.source {
        ExportSource::Table { schema, table } => format!("Export {schema}.{table} to file"),
        ExportSource::QueryResult { .. } => "Export query result to file".to_string(),
    };
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("File:");
                if ui.button("Browse…").clicked() {
                    state.file_dialog.save_file();
                }
                ui.add(
                    egui::TextEdit::singleline(&mut state.path)
                        .desired_width(f32::INFINITY)
                        .hint_text("~/data/rows.csv"),
                );
            });

            use crate::csv_export::ExportFormat;
            ui.horizontal(|ui| {
                ui.label("Format:");
                let prev = state.format;
                ui.selectable_value(&mut state.format, ExportFormat::Csv, "CSV");
                ui.selectable_value(&mut state.format, ExportFormat::Json, "JSON");
                if state.format != prev {
                    // Keep the file extension in step with the format.
                    let (from, to) = match state.format {
                        ExportFormat::Csv => (".json", ".csv"),
                        ExportFormat::Json => (".csv", ".json"),
                    };
                    if let Some(stripped) = state.path.strip_suffix(from) {
                        state.path = format!("{stripped}{to}");
                    }
                }
            });
            if state.format == ExportFormat::Csv {
                ui.horizontal(|ui| {
                    ui.label("Delimiter:");
                    ui.add(egui::TextEdit::singleline(&mut state.delimiter).desired_width(36.0))
                        .on_hover_text("Single character; use \\t for tab");
                    ui.add_space(12.0);
                    ui.checkbox(&mut state.include_header, "First row is header");
                });
            }

            ui.add_space(8.0);
            let delimiter_ok =
                state.format == ExportFormat::Json || state.delimiter_byte().is_some();
            ui.horizontal(|ui| {
                let can_start = !state.running && !state.path.trim().is_empty() && delimiter_ok;
                if ui.add_enabled(can_start, egui::Button::new("Export")).clicked() {
                    action = ExportAction::Start;
                }
                if state.running {
                    ui.spinner();
                    ui.weak(format!("exported {} row(s)…", state.progress_rows));
                }
            });
            if !delimiter_ok {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    "Delimiter must be a single ASCII character (or \\t).",
                );
            }

            if let Some(err) = &state.error {
                ui.add_space(6.0);
                egui::ScrollArea::vertical().id_source("ie_msg_scroll3").max_height(140.0).show(ui, |ui| {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                });
            }
        });

    state.file_dialog.update(ctx);
    if let Some(picked) = state.file_dialog.take_selected() {
        state.path = picked.to_string_lossy().into_owned();
    }

    if !open {
        action = ExportAction::Close;
    }
    action
}
