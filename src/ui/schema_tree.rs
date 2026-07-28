use crate::runtime::ConnectionId;

use super::ActiveConnection;

/// Tree/database-level actions carry the ConnectionId of the specific
/// database connection they belong to.
pub enum TreeAction {
    None,
    ExpandSchemas(ConnectionId),
    ExpandTables(ConnectionId, String),
    OpenTable(ConnectionId, String, String),
    OpenQueryTab(ConnectionId),
    /// Dump DDL files for one schema, or the whole database when None.
    DumpDdl(ConnectionId, Option<String>),
    /// Introspect this database and open it as a DBML diagram.
    ViewAsDbml(ConnectionId),
    /// Open a structure-compare tab with this database preselected.
    CompareStructure(ConnectionId),
    /// Open the active-sessions monitor for this connection.
    OpenSessions(ConnectionId),
    /// Open a query tab listing the server's users/roles.
    UsersAndRoles(ConnectionId),
    /// Open the data compare / sanitized pull tab, sourced from this conn.
    DataSync(ConnectionId),
    /// Open the column-mapped cross-connection transfer tab.
    TransferData(ConnectionId),
    /// Whole-database dump via the native tool (pg_dump/mysqldump/copy).
    DumpDatabase(ConnectionId),
    /// Restore a dump into this connection's database.
    RestoreDatabase(ConnectionId),
    /// Edit table/column comments of schema.table.
    EditComments(ConnectionId, String, String),
    /// Compare the two ctrl+click-selected databases right away.
    CompareSelected(ConnectionId, ConnectionId),
    /// Import a data file into schema.table.
    ImportInto(ConnectionId, String, String),
    /// Export schema.table's data to a delimited file.
    ExportFrom(ConnectionId, String, String),
    /// Open the table's Structure view.
    ModifyTable(ConnectionId, String, String),
    /// Create a new table in the schema via the Structure view.
    NewTable(ConnectionId, String),
    /// Open a routine's source in a query tab (schema, name, kind).
    RoutineDdl(ConnectionId, String, String, String),
    /// Open a view's CREATE statement in a query tab (schema, view).
    ViewDefinition(ConnectionId, String, String),
}

/// The schema/table tree for one connected connection; the connection card
/// above it is the header (name, badge, actions).
pub fn draw_tree(ui: &mut egui::Ui, conn: &mut ActiveConnection) -> TreeAction {
    let mut action = TreeAction::None;

    {
        if !conn.schemas_loaded {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("loading schemas…");
            });
            if matches!(action, TreeAction::None) {
                action = TreeAction::ExpandSchemas(conn.conn_id);
            }
            return action;
        }

        if conn.schemas.is_empty() {
            ui.weak("(no schemas)");
        }

        for schema in conn.schemas.iter_mut() {
            let id = ui.make_persistent_id(format!("schema_{}_{}", conn.conn_id, schema.name));
            let count_hint = match &schema.tables {
                Some(t) => format!(" ({})", t.len()),
                None => String::new(),
            };
            let header_text = egui::RichText::new(format!("🗀 {}{}", schema.name, count_hint));
            let header = egui::CollapsingHeader::new(header_text).id_source(id);
            let resp = header.show(ui, |ui| {
                match &schema.tables {
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
                        let hover_text = match &t.comment {
                            Some(c) => format!("{hover}\n💬 {c}"),
                            None => hover.to_owned(),
                        };
                        let resp = ui.selectable_label(false, text).on_hover_text(hover_text);
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if resp.double_clicked() {
                            action = TreeAction::OpenTable(
                                conn.conn_id,
                                schema.name.clone(),
                                t.name.clone(),
                            );
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Open").clicked() {
                                action = TreeAction::OpenTable(
                                    conn.conn_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                            if is_view && ui.button("Show definition").clicked() {
                                action = TreeAction::ViewDefinition(
                                    conn.conn_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                            if !is_view && ui.button("Modify structure").clicked() {
                                action = TreeAction::ModifyTable(
                                    conn.conn_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                            if !is_view
                                && ui.button("Import data from file…").clicked()
                            {
                                action = TreeAction::ImportInto(
                                    conn.conn_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                            if ui.button("Export data to file…").clicked() {
                                action = TreeAction::ExportFrom(
                                    conn.conn_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                            if !is_view
                                && conn.kind != crate::db::DbKind::Sqlite
                                && ui.button("Comments…").clicked()
                            {
                                action = TreeAction::EditComments(
                                    conn.conn_id,
                                    schema.name.clone(),
                                    t.name.clone(),
                                );
                                ui.close_menu();
                            }
                        });
                    }
                }
                }
                draw_schema_objects(
                    ui,
                    conn.conn_id,
                    &schema.name,
                    schema.objects.as_ref(),
                    &mut action,
                );
            });
            resp.header_response.context_menu(|ui| {
                if ui.button("New table…").clicked() {
                    action = TreeAction::NewTable(conn.conn_id, schema.name.clone());
                    ui.close_menu();
                }
                if ui.button("Dump schema DDL to folder…").clicked() {
                    action = TreeAction::DumpDdl(conn.conn_id, Some(schema.name.clone()));
                    ui.close_menu();
                }
            });
            let opened_now = resp.fully_open();
            if opened_now && !schema.expanded {
                schema.expanded = true;
                if schema.tables.is_none() && matches!(action, TreeAction::None) {
                    action = TreeAction::ExpandTables(conn.conn_id, schema.name.clone());
                }
            }
            if !opened_now {
                schema.expanded = false;
            }
        }
    }

    action
}

/// Functions / sequences / enums / triggers sections beneath a schema's
/// tables. Only non-empty sections are drawn.
fn draw_schema_objects(
    ui: &mut egui::Ui,
    conn_id: ConnectionId,
    schema: &str,
    objects: Option<&crate::db::SchemaObjects>,
    action: &mut TreeAction,
) {
    let Some(objects) = objects else { return };
    if objects.is_empty() {
        return;
    }
    let weak = ui.visuals().weak_text_color();

    if !objects.routines.is_empty() {
        egui::CollapsingHeader::new(
            egui::RichText::new(format!("ƒ functions ({})", objects.routines.len())).color(weak),
        )
        .id_source(("routines", conn_id, schema))
        .show(ui, |ui| {
            for r in &objects.routines {
                let label = if r.detail.is_empty() {
                    r.name.clone()
                } else {
                    format!("{}{}", r.name, r.detail)
                };
                let resp = ui
                    .selectable_label(false, egui::RichText::new(label).italics())
                    .on_hover_text(format!("{} — double-click for source", r.kind));
                let open = resp.double_clicked();
                resp.context_menu(|ui| {
                    if ui.button("Show source").clicked() {
                        *action = TreeAction::RoutineDdl(
                            conn_id,
                            schema.to_owned(),
                            r.name.clone(),
                            r.kind.clone(),
                        );
                        ui.close_menu();
                    }
                });
                if open {
                    *action = TreeAction::RoutineDdl(
                        conn_id,
                        schema.to_owned(),
                        r.name.clone(),
                        r.kind.clone(),
                    );
                }
            }
        });
    }

    if !objects.enums.is_empty() {
        egui::CollapsingHeader::new(
            egui::RichText::new(format!("☰ enums ({})", objects.enums.len())).color(weak),
        )
        .id_source(("enums", conn_id, schema))
        .show(ui, |ui| {
            for e in &objects.enums {
                ui.label(egui::RichText::new(&e.name).monospace())
                    .on_hover_text(e.values.join(", "));
            }
        });
    }

    if !objects.sequences.is_empty() {
        egui::CollapsingHeader::new(
            egui::RichText::new(format!("№ sequences ({})", objects.sequences.len())).color(weak),
        )
        .id_source(("sequences", conn_id, schema))
        .show(ui, |ui| {
            for s in &objects.sequences {
                ui.label(egui::RichText::new(s).monospace());
            }
        });
    }

    if !objects.triggers.is_empty() {
        egui::CollapsingHeader::new(
            egui::RichText::new(format!("⚡ triggers ({})", objects.triggers.len())).color(weak),
        )
        .id_source(("triggers", conn_id, schema))
        .show(ui, |ui| {
            for t in &objects.triggers {
                ui.label(egui::RichText::new(&t.name).monospace())
                    .on_hover_text(format!("{} on {}", t.detail, t.table));
            }
        });
    }
}
