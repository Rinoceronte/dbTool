use uuid::Uuid;

use crate::db::DbKind;
use crate::ui::theme;

use super::ConnectionProfile;

#[derive(Default)]
pub struct ManagerState {
    pub editing: Option<EditState>,
    pub last_test: Option<TestResult>,
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

pub fn draw_connection_list(
    ui: &mut egui::Ui,
    state: &mut ManagerState,
    profiles: &[ConnectionProfile],
    active_profile_ids: &std::collections::HashSet<Uuid>,
) -> ManagerAction {
    let mut action = ManagerAction::None;

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
        return action;
    }

    for profile in profiles {
        let is_active = active_profile_ids.contains(&profile.id);
        draw_conn_row(ui, state, profile, is_active, &mut action);
        ui.add_space(3.0);
    }

    action
}

/// Draws one connection card with a driver badge, connected-state dot, and
/// Connect/Edit affordances.
fn draw_conn_row(
    ui: &mut egui::Ui,
    state: &mut ManagerState,
    profile: &ConnectionProfile,
    is_active: bool,
    action: &mut ManagerAction,
) {
    let accent = theme::driver_color(profile.kind);

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
                if is_active {
                    ui.label(
                        egui::RichText::new("connected")
                            .small()
                            .color(theme::success_color(ui)),
                    );
                } else {
                    ui.label(
                        egui::RichText::new("disconnected")
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                }
            });
        });
    });

    // The whole card is the interaction surface: double-click connects,
    // right-click offers connect/disconnect.
    let resp = inner.response.interact(egui::Sense::click());
    if resp.double_clicked() && !is_active {
        *action = ManagerAction::Connect { profile_id: profile.id };
    }
    resp.context_menu(|ui| {
        ui.set_min_width(120.0);
        if is_active {
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
                            if edit.profile.kind != prev {
                                edit.profile.port = edit.profile.kind.default_port();
                            }
                        });
                    ui.end_row();

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
                });

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
