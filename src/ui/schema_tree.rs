use super::theme;
use super::{ActiveConnection, ProfileId};

pub enum TreeAction {
    None,
    ExpandSchemas(ProfileId),
    ExpandTables(ProfileId, String),
    OpenTable(ProfileId, String, String),
    OpenQueryTab(ProfileId),
    Disconnect(ProfileId),
    /// Dump DDL files for one schema, or the whole connection when None.
    DumpDdl(ProfileId, Option<String>),
    /// Import a data file into schema.table.
    ImportInto(ProfileId, String, String),
    /// Export schema.table's data to a delimited file.
    ExportFrom(ProfileId, String, String),
    /// Open the table's Structure view.
    ModifyTable(ProfileId, String, String),
    /// Create a new table in the schema via the Structure view.
    NewTable(ProfileId, String),
}

pub fn draw(ui: &mut egui::Ui, active: &mut [ActiveConnection]) -> TreeAction {
    let mut action = TreeAction::None;

    if active.is_empty() {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Not connected")
                .color(ui.visuals().weak_text_color()),
        );
        ui.label(
            egui::RichText::new("Connect above to browse schemas and tables.")
                .small()
                .color(ui.visuals().weak_text_color()),
        );
        return action;
    }

    for conn in active.iter_mut() {
        ui.horizontal(|ui| {
            theme::driver_badge(ui, conn.kind);
            let name_resp = ui.add(
                egui::Label::new(egui::RichText::new(&conn.name).strong())
                    .truncate()
                    .sense(egui::Sense::click()),
            );
            name_resp.context_menu(|ui| {
                if ui.button("New query tab").clicked() {
                    action = TreeAction::OpenQueryTab(conn.profile_id);
                    ui.close_menu();
                }
                if ui.button("Dump DDL to folder…").clicked() {
                    action = TreeAction::DumpDdl(conn.profile_id, None);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Disconnect").clicked() {
                    action = TreeAction::Disconnect(conn.profile_id);
                    ui.close_menu();
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new("✖").frame(false))
                    .on_hover_text("Disconnect")
                    .clicked()
                {
                    action = TreeAction::Disconnect(conn.profile_id);
                }
                if ui
                    .small_button("SQL")
                    .on_hover_text("New query tab")
                    .clicked()
                {
                    action = TreeAction::OpenQueryTab(conn.profile_id);
                }
            });
        });

        if !conn.schemas_loaded {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("loading schemas…");
            });
            if matches!(action, TreeAction::None) {
                action = TreeAction::ExpandSchemas(conn.profile_id);
            }
            ui.add_space(6.0);
            continue;
        }

        if conn.schemas.is_empty() {
            ui.weak("(no schemas)");
        }

        for schema in conn.schemas.iter_mut() {
            let id = ui.make_persistent_id(format!("schema_{}_{}", conn.profile_id, schema.name));
            let count_hint = match &schema.tables {
                Some(t) => format!(" ({})", t.len()),
                None => String::new(),
            };
            let header_text = egui::RichText::new(format!("🗀 {}{}", schema.name, count_hint));
            let header = egui::CollapsingHeader::new(header_text).id_source(id);
            let resp = header.show(ui, |ui| match &schema.tables {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("loading tables…");
                    });
                }
                Some(tables) if tables.is_empty() => {
                    ui.weak("(no tables)");
                }
                Some(tables) => {
                    for t in tables {
                        let (icon, hover) = match t.kind {
                            crate::db::TableKind::Table => ("▦", "Table — double-click to open"),
                            crate::db::TableKind::View => ("◈", "View — double-click to open"),
                        };
                        let is_view = matches!(t.kind, crate::db::TableKind::View);
                        let text = if is_view {
                            egui::RichText::new(format!("{icon}  {}", t.name))
                                .italics()
                                .color(ui.visuals().weak_text_color())
                        } else {
                            egui::RichText::new(format!("{icon}  {}", t.name))
                        };
                        let resp = ui.selectable_label(false, text).on_hover_text(hover);
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.double_clicked() {
                            action = TreeAction::OpenTable(
                                conn.profile_id,
                                schema.name.clone(),
                                t.name.clone(),
                            );
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Open").clicked() {
                                action = TreeAction::OpenTable(
                                    conn.profile_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                            if !is_view && ui.button("Modify structure").clicked() {
                                action = TreeAction::ModifyTable(
                                    conn.profile_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                            if !is_view
                                && ui.button("Import data from file…").clicked()
                            {
                                action = TreeAction::ImportInto(
                                    conn.profile_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                            if ui.button("Export data to file…").clicked() {
                                action = TreeAction::ExportFrom(
                                    conn.profile_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                        });
                    }
                }
            });
            resp.header_response.context_menu(|ui| {
                if ui.button("New table…").clicked() {
                    action = TreeAction::NewTable(conn.profile_id, schema.name.clone());
                    ui.close_menu();
                }
                if ui.button("Dump schema DDL to folder…").clicked() {
                    action = TreeAction::DumpDdl(conn.profile_id, Some(schema.name.clone()));
                    ui.close_menu();
                }
            });
            let opened_now = resp.fully_open();
            if opened_now && !schema.expanded {
                schema.expanded = true;
                if schema.tables.is_none() && matches!(action, TreeAction::None) {
                    action = TreeAction::ExpandTables(conn.profile_id, schema.name.clone());
                }
            }
            if !opened_now {
                schema.expanded = false;
            }
        }
        ui.add_space(6.0);
    }

    action
}
