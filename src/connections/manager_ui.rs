use uuid::Uuid;

use crate::db::DbKind;
use crate::runtime::ConnectionId;
use crate::ui::schema_tree::{self, TreeAction};
use crate::ui::theme;
use crate::ui::ActiveConnection;

use super::ConnectionProfile;

#[derive(Default)]
pub struct ManagerState {
    pub editing: Option<EditState>,
    pub last_test: Option<TestResult>,
    /// Databases highlighted via ctrl+click for a structure compare (max 2,
    /// oldest dropped first).
    pub compare_selection: Vec<ConnectionId>,
}

/// Toggle a database in the compare selection, keeping at most two.
fn toggle_compare_selection(sel: &mut Vec<ConnectionId>, conn: ConnectionId) {
    match sel.iter().position(|c| *c == conn) {
        Some(pos) => {
            sel.remove(pos);
        }
        None => {
            sel.push(conn);
            if sel.len() > 2 {
                sel.remove(0);
            }
        }
    }
}

/// Context-menu entries shared by connection cards and database nodes.
fn compare_menu_items(
    ui: &mut egui::Ui,
    sel: &mut Vec<ConnectionId>,
    conn: ConnectionId,
    tree_action: &mut TreeAction,
) {
    let selected = sel.contains(&conn);
    let label = if selected {
        "Deselect for compare".to_owned()
    } else {
        "Select for compare (Ctrl+click)".to_owned()
    };
    if ui.button(label).clicked() {
        toggle_compare_selection(sel, conn);
        ui.close_menu();
    }
    if sel.len() == 2 && ui.button("Compare selected structures").clicked() {
        *tree_action = TreeAction::CompareSelected(sel[0], sel[1]);
        sel.clear();
        ui.close_menu();
    }
}

pub struct EditState {
    pub profile: ConnectionProfile,
    pub password: String,
    pub is_new: bool,
    pub error: Option<String>,
    pub focus_pending: bool,
}

pub enum TestResult {
    Ok,
    Err(String),
}

pub enum ManagerAction {
    None,
    Connect { profile_id: Uuid },
    Disconnect { profile_id: Uuid },
    Save { profile: ConnectionProfile, password: String },
    Delete { profile_id: Uuid },
    TestConnection { profile: ConnectionProfile, password: String },
    CloseEdit,
    /// Open the database-toggle picker for a connected profile.
    ShowDatabases { profile_id: Uuid },
}

impl ManagerState {
    pub fn start_add(&mut self) {
        self.editing = Some(EditState {
            profile: ConnectionProfile::new(DbKind::Postgres),
            password: String::new(),
            is_new: true,
            error: None,
            focus_pending: true,
        });
        self.last_test = None;
    }

    pub fn start_edit(&mut self, profile: ConnectionProfile, password: String) {
        self.editing = Some(EditState {
            profile,
            password,
            is_new: false,
            error: None,
            focus_pending: true,
        });
        self.last_test = None;
    }

    pub fn close(&mut self) {
        self.editing = None;
        self.last_test = None;
    }
}

/// The unified sidebar list: each connection is one card; a connected card
/// grows its schema tree directly beneath it.
pub fn draw_connection_list(
    ui: &mut egui::Ui,
    state: &mut ManagerState,
    profiles: &[ConnectionProfile],
    active: &mut [ActiveConnection],
) -> (ManagerAction, TreeAction) {
    let mut action = ManagerAction::None;
    let mut tree_action = TreeAction::None;

    // Drop compare selections whose connection has gone away.
    state
        .compare_selection
        .retain(|c| active.iter().any(|a| a.conn_id == *c));

    ui.horizontal(|ui| {
        ui.heading("Connections");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button("+ Add")
                .on_hover_text("Create a new connection")
                .clicked()
            {
                state.start_add();
            }
        });
    });
    ui.add_space(4.0);

    if profiles.is_empty() {
        ui.add_space(8.0);
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new("No connections yet").strong());
            ui.label(
                egui::RichText::new("Click + Add to create one.")
                    .color(ui.visuals().weak_text_color()),
            );
        });
        return (action, tree_action);
    }

    for profile in profiles {
        let primary_conn = active
            .iter()
            .find(|a| a.profile_id == profile.id && a.is_primary)
            .map(|a| a.conn_id);
        draw_conn_row(ui, state, profile, primary_conn, &mut action, &mut tree_action);
        if primary_conn.is_some() {
            let mut conns: Vec<&mut ActiveConnection> = active
                .iter_mut()
                .filter(|a| a.profile_id == profile.id)
                .collect();
            conns.sort_by(|a, b| {
                (!a.is_primary, &a.database).cmp(&(!b.is_primary, &b.database))
            });
            let multi = conns.len() > 1;
            ui.indent(("conn_tree", profile.id), |ui| {
                for conn in conns {
                    // Reserve a paint slot before the node so a DataGrip-style
                    // wash in the database's color can be filled in underneath
                    // the whole subtree (header + expanded schemas/tables).
                    let wash_idx = ui.painter().add(egui::Shape::Noop);
                    let node_top = ui.next_widget_position().y;
                    if multi {
                        // While ctrl is held, freeze the header at its current
                        // open state so ctrl+click only selects for compare
                        // and never expands/collapses. The state id is cached
                        // from last frame's response (the header renders in a
                        // child Ui, so it can't be derived from this ui's id).
                        let hid_key = egui::Id::new(("db_node_hid", conn.conn_id));
                        let frozen_open = if ui.input(|i| i.modifiers.command) {
                            ui.data(|d| d.get_temp::<egui::Id>(hid_key)).map(|hid| {
                                egui::collapsing_header::CollapsingState::load(ui.ctx(), hid)
                                    .map(|st| st.is_open())
                                    .unwrap_or(conn.is_primary)
                            })
                        } else {
                            None
                        };
                        let db_text =
                            egui::RichText::new(format!("🛢 {}", conn.database)).strong();
                        let mut header = egui::CollapsingHeader::new(db_text)
                        .id_source(("db_node", conn.conn_id))
                        .default_open(conn.is_primary);
                        if frozen_open.is_some() {
                            header = header.open(frozen_open);
                        }
                        let resp = header.show(ui, |ui| {
                            let t = schema_tree::draw_tree(ui, conn);
                            if !matches!(t, TreeAction::None) {
                                tree_action = t;
                            }
                        });
                        resp.header_response.context_menu(|ui| {
                            if ui.button("New query tab").clicked() {
                                tree_action = TreeAction::OpenQueryTab(conn.conn_id);
                                ui.close_menu();
                            }
                            if ui.button("View as DBML diagram").clicked() {
                                tree_action = TreeAction::ViewAsDbml(conn.conn_id);
                                ui.close_menu();
                            }
                            if ui.button("Compare structure…").clicked() {
                                tree_action = TreeAction::CompareStructure(conn.conn_id);
                                ui.close_menu();
                            }
                            compare_menu_items(
                                ui,
                                &mut state.compare_selection,
                                conn.conn_id,
                                &mut tree_action,
                            );
                        });
                        ui.data_mut(|d| d.insert_temp(hid_key, resp.header_response.id));
                        if resp.header_response.clicked()
                            && ui.input(|i| i.modifiers.command)
                        {
                            toggle_compare_selection(&mut state.compare_selection, conn.conn_id);
                        }
                        if state.compare_selection.contains(&conn.conn_id) {
                            ui.painter().rect_stroke(
                                resp.header_response.rect.expand(1.0),
                                egui::Rounding::same(4.0),
                                egui::Stroke::new(1.5, theme::ACCENT),
                            );
                        }
                    } else {
                        let t = schema_tree::draw_tree(ui, conn);
                        if !matches!(t, TreeAction::None) {
                            tree_action = t;
                        }
                    }
                    if let Some(c) = profile.database_color(&conn.database) {
                        let rect = egui::Rect::from_min_max(
                            egui::pos2(ui.max_rect().left(), node_top - 1.0),
                            egui::pos2(ui.max_rect().right(), ui.min_rect().bottom() + 1.0),
                        );
                        ui.painter().set(
                            wash_idx,
                            egui::Shape::rect_filled(
                                rect,
                                egui::Rounding::same(4.0),
                                egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], 30),
                            ),
                        );
                    }
                }
            });
        }
        ui.add_space(3.0);
    }

    (action, tree_action)
}

/// Draws one connection card with a driver badge, connected-state dot, and
/// Connect/Edit affordances.
fn draw_conn_row(
    ui: &mut egui::Ui,
    state: &mut ManagerState,
    profile: &ConnectionProfile,
    primary_conn: Option<ConnectionId>,
    action: &mut ManagerAction,
    tree_action: &mut TreeAction,
) {
    let is_active = primary_conn.is_some();
    let accent = if profile.production {
        theme::PROD_RED
    } else if let Some(c) = profile.color {
        egui::Color32::from_rgb(c[0], c[1], c[2])
    } else {
        theme::driver_color(profile.kind)
    };

    // Register the card's click surface FIRST (with last frame's rect):
    // egui resolves overlapping click targets to the last-registered widget,
    // so registering the card before its children keeps the buttons inside
    // it (pencil, SQL) clickable.
    let card_id = ui.make_persistent_id(("conn_card", profile.id));
    let prev_rect: Option<egui::Rect> = ui.data(|d| d.get_temp(card_id));
    let pre_resp = prev_rect.map(|r| ui.interact(r, card_id, egui::Sense::click()));

    let fill = if is_active {
        let a = accent.to_array();
        egui::Color32::from_rgba_unmultiplied(a[0], a[1], a[2], 26)
    } else {
        ui.visuals().faint_bg_color
    };
    let stroke = if is_active {
        egui::Stroke::new(1.0, accent)
    } else {
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
    };

    let frame = egui::Frame::none()
        .fill(fill)
        .stroke(stroke)
        .rounding(egui::Rounding::same(7.0))
        .inner_margin(egui::Margin::symmetric(8.0, 7.0));

    let inner = frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            theme::driver_badge(ui, profile.kind);
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                let name = if profile.name.is_empty() {
                    format!("{}:{}", profile.host, profile.port)
                } else {
                    profile.name.clone()
                };
                ui.horizontal(|ui| {
                    theme::status_dot(ui, is_active)
                        .on_hover_text(if is_active { "Connected" } else { "Not connected" });
                    ui.label(egui::RichText::new(name).strong());
                    if profile.production {
                        ui.label(
                            egui::RichText::new("PROD").small().strong().color(theme::PROD_RED),
                        );
                    }
                    if profile.read_only {
                        ui.label(egui::RichText::new("🔒").small())
                            .on_hover_text("Read-only connection");
                    }
                });
                let sub = format!("{}:{} · {}", profile.host, profile.port, sub_db(profile));
                ui.label(
                    egui::RichText::new(sub)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new("✏").frame(false))
                    .on_hover_text("Edit connection")
                    .clicked()
                {
                    let password = crate::connections::load_password(profile).unwrap_or_default();
                    state.start_edit(profile.clone(), password);
                }
                if let Some(conn) = primary_conn
                    && ui
                        .small_button("SQL")
                        .on_hover_text("New query tab")
                        .clicked()
                {
                    *tree_action = TreeAction::OpenQueryTab(conn);
                }
            });
        });
    });
    ui.data_mut(|d| d.insert_temp(card_id, inner.response.rect));

    // The whole card is the interaction surface: double-click connects,
    // right-click opens the menu. Falls back to the frame response on the
    // very first frame, before a rect is cached.
    let resp = pre_resp.unwrap_or_else(|| inner.response.interact(egui::Sense::click()));
    if resp.double_clicked() && !is_active {
        *action = ManagerAction::Connect { profile_id: profile.id };
    }
    if let Some(conn) = primary_conn {
        if resp.clicked() && ui.input(|i| i.modifiers.command) {
            toggle_compare_selection(&mut state.compare_selection, conn);
        }
    }
    resp.context_menu(|ui| {
        ui.set_min_width(150.0);
        if let Some(conn) = primary_conn {
            if ui.button("New query tab").clicked() {
                *tree_action = TreeAction::OpenQueryTab(conn);
                ui.close_menu();
            }
            if ui.button("View as DBML diagram").clicked() {
                *tree_action = TreeAction::ViewAsDbml(conn);
                ui.close_menu();
            }
            if ui.button("Compare structure…").clicked() {
                *tree_action = TreeAction::CompareStructure(conn);
                ui.close_menu();
            }
            if ui
                .button("Compare / pull data…")
                .on_hover_text(
                    "Row-level data compare against another connection, sync scripts, \
                     and sanitized prod → sandbox pulls",
                )
                .clicked()
            {
                *tree_action = TreeAction::DataSync(conn);
                ui.close_menu();
            }
            if ui
                .button("Transfer data…")
                .on_hover_text(
                    "Column-mapped copy of one table into another — any \
                     connection, engines and names may differ",
                )
                .clicked()
            {
                *tree_action = TreeAction::TransferData(conn);
                ui.close_menu();
            }
            compare_menu_items(ui, &mut state.compare_selection, conn, tree_action);
            if profile.kind != DbKind::MySql
                && !profile.kind.is_file_based()
                && ui.button("Databases…").clicked()
            {
                *action = ManagerAction::ShowDatabases { profile_id: profile.id };
                ui.close_menu();
            }
            if ui.button("Dump DDL to folder…").clicked() {
                *tree_action = TreeAction::DumpDdl(conn, None);
                ui.close_menu();
            }
            if profile.kind != DbKind::MsSql {
                if ui
                    .button("Dump database to file…")
                    .on_hover_text("Whole-database backup via pg_dump / mysqldump / file copy")
                    .clicked()
                {
                    *tree_action = TreeAction::DumpDatabase(conn);
                    ui.close_menu();
                }
                if !profile.read_only
                    && ui
                        .button("Restore database from file…")
                        .on_hover_text("Load a dump back in — overwrites the database's data")
                        .clicked()
                {
                    *tree_action = TreeAction::RestoreDatabase(conn);
                    ui.close_menu();
                }
            }
            if !profile.kind.is_file_based() && ui.button("Sessions…").clicked() {
                *tree_action = TreeAction::OpenSessions(conn);
                ui.close_menu();
            }
            if !profile.kind.is_file_based() && ui.button("Users && roles…").clicked() {
                *tree_action = TreeAction::UsersAndRoles(conn);
                ui.close_menu();
            }
            if !profile.kind.is_file_based()
                && ui
                    .button("Top queries…")
                    .on_hover_text(
                        "Statement statistics from the server, heaviest first \
                         (pg_stat_statements / performance_schema / query stats DMVs)",
                    )
                    .clicked()
            {
                *tree_action = TreeAction::TopQueries(conn);
                ui.close_menu();
            }
            ui.separator();
            if ui.button("Disconnect").clicked() {
                *action = ManagerAction::Disconnect { profile_id: profile.id };
                ui.close_menu();
            }
        } else if ui.button("Connect").clicked() {
            *action = ManagerAction::Connect { profile_id: profile.id };
            ui.close_menu();
        }
        if ui.button("Edit…").clicked() {
            let password = crate::connections::load_password(profile).unwrap_or_default();
            state.start_edit(profile.clone(), password);
            ui.close_menu();
        }
    });
    let resp = resp.on_hover_text(if is_active {
        "Connected — right-click to disconnect"
    } else {
        "Double-click to connect"
    });
    if resp.hovered() && !is_active {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_stroke(
            resp.rect,
            egui::Rounding::same(7.0),
            egui::Stroke::new(1.0, accent),
        );
    }
    if primary_conn.is_some_and(|c| state.compare_selection.contains(&c)) {
        ui.painter().rect_stroke(
            resp.rect,
            egui::Rounding::same(7.0),
            egui::Stroke::new(2.0, theme::ACCENT),
        );
    }
}

pub enum DbPickerAction {
    None,
    Close,
    Enable { profile_id: Uuid, database: String },
    Disable { profile_id: Uuid, database: String },
    /// Set (Some) or clear (None) a database's sidebar accent color.
    SetColor { profile_id: Uuid, database: String, color: Option<[u8; 3]> },
    /// Make this database the profile's primary and reconnect.
    SetPrimary { profile_id: Uuid, database: String },
}

/// DataGrip-style database toggles: checked databases open their own
/// connection and appear as nodes in the sidebar tree.
pub fn draw_db_picker(
    ctx: &egui::Context,
    profile: &ConnectionProfile,
    active: &[ActiveConnection],
) -> DbPickerAction {
    let mut action = DbPickerAction::None;
    let mut open = true;
    let primary = active
        .iter()
        .find(|a| a.profile_id == profile.id && a.is_primary);
    let title = if profile.name.is_empty() {
        format!("Databases — {}:{}", profile.host, profile.port)
    } else {
        format!("Databases — {}", profile.name)
    };

    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .default_width(340.0)
        .open(&mut open)
        .show(ctx, |ui| {
            let Some(primary) = primary else {
                ui.label("Connection is no longer active.");
                return;
            };
            match &primary.server_databases {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("listing databases…");
                    });
                }
                Some(dbs) if dbs.is_empty() => {
                    ui.weak("No databases listed (insufficient privileges?).");
                }
                Some(dbs) => {
                    ui.label(
                        egui::RichText::new(
                            "Checked databases open their own connection and \
                             show up under this connection in the sidebar.",
                        )
                        .small()
                        .color(ui.visuals().weak_text_color()),
                    );
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .id_source("db_picker_scroll")
                        .max_height(320.0)
                        .show(ui, |ui| {
                            for db in dbs {
                                let is_own = *db == primary.database;
                                let mut on = is_own
                                    || active.iter().any(|a| {
                                        a.profile_id == profile.id && &a.database == db
                                    });
                                ui.horizontal(|ui| {
                                    let cb = ui.add_enabled(
                                        !is_own,
                                        egui::Checkbox::new(&mut on, db.as_str()),
                                    );
                                    if is_own {
                                        cb.on_hover_text("The connection's own database");
                                    } else if cb.clicked() {
                                        action = if on {
                                            DbPickerAction::Enable {
                                                profile_id: profile.id,
                                                database: db.clone(),
                                            }
                                        } else {
                                            DbPickerAction::Disable {
                                                profile_id: profile.id,
                                                database: db.clone(),
                                            }
                                        };
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let saved = profile.db_colors.get(db).copied();
                                            let mut rgb = saved
                                                .or(profile.color)
                                                .unwrap_or([110, 110, 110]);
                                            let sw = ui.color_edit_button_srgb(&mut rgb);
                                            if sw.changed() {
                                                action = DbPickerAction::SetColor {
                                                    profile_id: profile.id,
                                                    database: db.clone(),
                                                    color: Some(rgb),
                                                };
                                            }
                                            sw.on_hover_text(
                                                "Accent color for this database — overrides \
                                                 the connection's color",
                                            );
                                            if saved.is_some()
                                                && ui
                                                    .small_button("🗙")
                                                    .on_hover_text(
                                                        "Clear override (inherit the \
                                                         connection color)",
                                                    )
                                                    .clicked()
                                            {
                                                action = DbPickerAction::SetColor {
                                                    profile_id: profile.id,
                                                    database: db.clone(),
                                                    color: None,
                                                };
                                            }
                                            if is_own {
                                                ui.label(
                                                    egui::RichText::new("primary")
                                                        .small()
                                                        .color(ui.visuals().weak_text_color()),
                                                );
                                            } else if ui
                                                .small_button("★")
                                                .on_hover_text(
                                                    "Make this the primary database \
                                                     (reconnects; the current primary \
                                                     stays toggled on)",
                                                )
                                                .clicked()
                                            {
                                                action = DbPickerAction::SetPrimary {
                                                    profile_id: profile.id,
                                                    database: db.clone(),
                                                };
                                            }
                                        },
                                    );
                                });
                            }
                        });
                }
            }
        });

    if !open {
        action = DbPickerAction::Close;
    }
    action
}

fn sub_db(profile: &ConnectionProfile) -> String {
    if profile.database.is_empty() {
        profile.kind.display().to_string()
    } else {
        profile.database.clone()
    }
}

pub fn draw_edit_dialog(ctx: &egui::Context, state: &mut ManagerState) -> ManagerAction {
    let mut action = ManagerAction::None;
    let Some(edit) = state.editing.as_mut() else {
        return action;
    };

    let title = if edit.is_new { "New Connection" } else { "Edit Connection" };
    let mut open = true;
    let mut save_clicked = false;
    egui::Window::new(title)
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(440.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let (enter, esc) = ui.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
                    i.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
                )
            });
            if enter {
                save_clicked = true;
            }
            if esc {
                action = ManagerAction::CloseEdit;
            }

            ui.horizontal(|ui| {
                theme::driver_badge(ui, edit.profile.kind);
                ui.label(
                    egui::RichText::new(edit.profile.kind.display())
                        .strong()
                        .color(theme::driver_color(edit.profile.kind)),
                );
            });
            ui.add_space(6.0);

            egui::Grid::new("edit_conn_grid")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .min_col_width(90.0)
                .show(ui, |ui| {
                    ui.label("Name");
                    let name_resp = ui.add(
                        egui::TextEdit::singleline(&mut edit.profile.name)
                            .hint_text("My database")
                            .desired_width(f32::INFINITY),
                    );
                    if edit.focus_pending {
                        name_resp.request_focus();
                        edit.focus_pending = false;
                    }
                    ui.end_row();

                    ui.label("Driver");
                    egui::ComboBox::from_id_source("driver")
                        .selected_text(edit.profile.kind.display())
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            let prev = edit.profile.kind;
                            ui.selectable_value(&mut edit.profile.kind, DbKind::Postgres, "PostgreSQL");
                            ui.selectable_value(&mut edit.profile.kind, DbKind::MySql, "MySQL / MariaDB");
                            ui.selectable_value(&mut edit.profile.kind, DbKind::MsSql, "Microsoft SQL Server");
                            ui.selectable_value(&mut edit.profile.kind, DbKind::Sqlite, "SQLite");
                            if edit.profile.kind != prev {
                                edit.profile.port = edit.profile.kind.default_port();
                            }
                        });
                    ui.end_row();

                    if edit.profile.kind.is_file_based() {
                        ui.label("File");
                        ui.add(
                            egui::TextEdit::singleline(&mut edit.profile.database)
                                .hint_text("~/data/app.db (created if missing)")
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                    } else {
                    ui.label("Host");
                    ui.add(
                        egui::TextEdit::singleline(&mut edit.profile.host)
                            .hint_text("localhost")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Port");
                    let mut port_str = edit.profile.port.to_string();
                    if ui
                        .add(egui::TextEdit::singleline(&mut port_str).desired_width(90.0))
                        .changed()
                    {
                        if let Ok(p) = port_str.parse::<u16>() {
                            edit.profile.port = p;
                        }
                    }
                    ui.end_row();

                    ui.label("Database");
                    ui.add(
                        egui::TextEdit::singleline(&mut edit.profile.database)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Username");
                    ui.add(
                        egui::TextEdit::singleline(&mut edit.profile.username)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Password");
                    ui.add(
                        egui::TextEdit::singleline(&mut edit.password)
                            .password(true)
                            .hint_text("leave blank to keep existing")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("");
                    ui.checkbox(&mut edit.profile.require_ssl, "Require SSL");
                    ui.end_row();
                    }

                    ui.label("Color");
                    ui.horizontal(|ui| {
                        let mut rgb = edit.profile.color.unwrap_or([110, 110, 110]);
                        if ui
                            .color_edit_button_srgb(&mut rgb)
                            .on_hover_text(
                                "Accent color for this connection — cascades to all \
                                 its databases unless overridden per database",
                            )
                            .changed()
                        {
                            edit.profile.color = Some(rgb);
                        }
                        if edit.profile.color.is_some() {
                            if ui.small_button("🗙").on_hover_text("Clear color").clicked() {
                                edit.profile.color = None;
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("none")
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                        }
                    });
                    ui.end_row();

                    ui.label("");
                    ui.checkbox(&mut edit.profile.production, "Production")
                        .on_hover_text("Tints this connection red everywhere");
                    ui.end_row();

                    ui.label("");
                    ui.checkbox(&mut edit.profile.read_only, "Read-only")
                        .on_hover_text(
                            "Refuses INSERT/UPDATE/DELETE/DDL and all editor commits",
                        );
                    ui.end_row();
                });

            ui.add_space(4.0);
            if !edit.profile.kind.is_file_based() {
            egui::CollapsingHeader::new("SSH tunnel")
                .id_source(("ssh_section", edit.profile.id))
                .default_open(edit.profile.ssh_enabled)
                .show(ui, |ui| {
                    ui.checkbox(
                        &mut edit.profile.ssh_enabled,
                        "Connect through an SSH tunnel",
                    )
                    .on_hover_text(
                        "Uses the system ssh — your keys, agent and ~/.ssh/config apply",
                    );
                    ui.add_enabled_ui(edit.profile.ssh_enabled, |ui| {
                        egui::Grid::new("ssh_grid")
                            .num_columns(2)
                            .spacing([8.0, 6.0])
                            .show(ui, |ui| {
                                ui.label("SSH host");
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut edit.profile.ssh_host)
                                            .hint_text("server.example.com")
                                            .desired_width(180.0),
                                    );
                                    ui.label(":");
                                    let mut port = edit.profile.ssh_port.to_string();
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(&mut port)
                                                .desired_width(52.0),
                                        )
                                        .changed()
                                    {
                                        if let Ok(p) = port.trim().parse::<u16>() {
                                            edit.profile.ssh_port = p;
                                        }
                                    }
                                });
                                ui.end_row();

                                ui.label("SSH user");
                                ui.add(
                                    egui::TextEdit::singleline(&mut edit.profile.ssh_user)
                                        .desired_width(f32::INFINITY),
                                );
                                ui.end_row();

                                ui.label("Identity file");
                                ui.add(
                                    egui::TextEdit::singleline(&mut edit.profile.ssh_key)
                                        .hint_text("blank = ssh defaults / agent")
                                        .desired_width(f32::INFINITY),
                                );
                                ui.end_row();
                            });
                        ui.label(
                            egui::RichText::new(
                                "The database host/port above are then reached from the SSH server.",
                            )
                            .small()
                            .color(ui.visuals().weak_text_color()),
                        );
                    });
                });
            }

            if let Some(test) = &state.last_test {
                ui.add_space(6.0);
                match test {
                    TestResult::Ok => {
                        ui.colored_label(theme::success_color(ui), "✔ Connection OK");
                    }
                    TestResult::Err(e) => {
                        ui.colored_label(
                            ui.visuals().error_fg_color,
                            format!("✖ Connection failed: {e}"),
                        );
                    }
                }
            }

            if let Some(err) = &edit.error {
                ui.add_space(6.0);
                ui.colored_label(ui.visuals().error_fg_color, err);
            }

            ui.add_space(4.0);
            ui.separator();
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if ui.button("Test Connection").clicked() {
                    action = ManagerAction::TestConnection {
                        profile: edit.profile.clone(),
                        password: edit.password.clone(),
                    };
                }
                if !edit.is_new
                    && ui
                        .button(egui::RichText::new("Delete").color(ui.visuals().error_fg_color))
                        .clicked()
                {
                    action = ManagerAction::Delete { profile_id: edit.profile.id };
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let save_label = if edit.is_new { "Create" } else { "Save" };
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new(save_label).color(egui::Color32::WHITE))
                                .fill(theme::ACCENT),
                        )
                        .on_hover_text("Save (Enter)")
                        .clicked()
                    {
                        save_clicked = true;
                    }
                    if ui.button("Cancel").on_hover_text("Cancel (Esc)").clicked() {
                        action = ManagerAction::CloseEdit;
                    }
                });
            });
        });

    if save_clicked {
        if let Some(edit) = state.editing.as_ref() {
            action = ManagerAction::Save {
                profile: edit.profile.clone(),
                password: edit.password.clone(),
            };
        }
    }
    if !open {
        action = ManagerAction::CloseEdit;
    }

    action
}
