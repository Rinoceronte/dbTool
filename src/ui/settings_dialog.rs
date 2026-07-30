//! Settings window: appearance, editor toggles, and the Claude account.

use crate::settings::Settings;
use crate::ui::auth_dialog::{self, AuthAction, AuthState};
use crate::ui::theme::{self, ThemeMode};
use crate::update::UpdateInfo;

/// Result of the last manual update check, shown inline in the dialog.
#[derive(Default)]
pub struct UpdateUi {
    pub checking: bool,
    pub up_to_date: bool,
    pub error: Option<String>,
}

impl UpdateUi {
    /// A manual check is starting; clear stale results.
    pub fn begin(&mut self) {
        self.checking = true;
        self.up_to_date = false;
        self.error = None;
    }
}

pub enum UpdateAction {
    None,
    Check,
    Install { url: String, version: String },
}

/// Returns whether any setting changed (the caller re-installs the theme and
/// persists), plus any account/update action.
pub fn draw(
    ctx: &egui::Context,
    open: &mut bool,
    settings: &mut Settings,
    auth: &mut AuthState,
    update_ui: &UpdateUi,
    update_available: Option<&UpdateInfo>,
    update_downloading: bool,
) -> (bool, AuthAction, UpdateAction) {
    let mut changed = false;
    let mut auth_action = AuthAction::None;
    let mut update_action = UpdateAction::None;
    if !*open {
        return (changed, auth_action, update_action);
    }

    let mut still_open = true;
    egui::Window::new("Settings")
        .collapsible(false)
        .resizable(false)
        .default_width(460.0)
        .open(&mut still_open)
        .show(ctx, |ui| {
            ui.label(egui::RichText::new("Appearance").strong());
            ui.horizontal(|ui| {
                ui.label("Theme:");
                changed |= ui
                    .selectable_value(&mut settings.theme, ThemeMode::Dark, "🌙 Dark")
                    .clicked();
                changed |= ui
                    .selectable_value(&mut settings.theme, ThemeMode::Light, "☀ Light")
                    .clicked();
            });

            ui.add_space(6.0);
            ui.separator();
            ui.label(egui::RichText::new("Editors").strong());
            changed |= ui
                .checkbox(&mut settings.sql_line_numbers, "Line numbers in SQL editors")
                .changed();
            changed |= ui
                .checkbox(
                    &mut settings.dbml_line_numbers,
                    "Line numbers in the DBML source pane",
                )
                .changed();

            ui.add_space(6.0);
            ui.separator();
            ui.label(egui::RichText::new("Results").strong());
            ui.horizontal(|ui| {
                ui.label("Max rows per result:");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut settings.max_result_rows)
                            .range(50..=1_000_000)
                            .speed(100),
                    )
                    .on_hover_text(
                        "Queries stop materializing rows past this cap; \
                         the grid offers \"Fetch all rows\" to lift it per query",
                    )
                    .changed();
            });

            ui.add_space(6.0);
            ui.separator();
            ui.label(egui::RichText::new("Updates").strong());
            ui.horizontal(|ui| {
                ui.label(format!("dbTool v{}", env!("CARGO_PKG_VERSION")));
                if update_downloading {
                    ui.spinner();
                    ui.weak("Downloading update…");
                } else if let Some(info) = update_available {
                    if cfg!(windows) && info.installer_url.is_some() {
                        if ui
                            .button(
                                egui::RichText::new(format!("⬆ Update to v{}", info.version))
                                    .color(theme::ACCENT),
                            )
                            .on_hover_text(
                                "Download and install the update; \
                                 dbTool restarts automatically",
                            )
                            .clicked()
                        {
                            update_action = UpdateAction::Install {
                                url: info.installer_url.clone().unwrap(),
                                version: info.version.clone(),
                            };
                        }
                    } else if ui
                        .button(
                            egui::RichText::new(format!("⬆ v{} available", info.version))
                                .color(theme::ACCENT),
                        )
                        .on_hover_text("Open the release download page")
                        .clicked()
                    {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&info.release_url));
                    }
                } else if update_ui.checking {
                    ui.spinner();
                    ui.weak("Checking…");
                } else {
                    if ui.button("Check for updates").clicked() {
                        update_action = UpdateAction::Check;
                    }
                    if update_ui.up_to_date {
                        ui.weak("You're up to date.");
                    }
                }
            });
            if let Some(err) = &update_ui.error {
                ui.colored_label(ui.visuals().warn_fg_color, format!("Check failed: {err}"));
            }

            ui.add_space(6.0);
            ui.separator();
            ui.label(egui::RichText::new("Claude account").strong());
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label("CLI path:");
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut settings.claude_cli_path)
                            .hint_text("claude (from PATH)")
                            .desired_width(280.0),
                    )
                    .on_hover_text(
                        "Full path to the `claude` executable. \
                         Leave empty to use `claude` from PATH.",
                    )
                    .changed();
            });
            ui.add_space(4.0);
            auth_action = auth_dialog::draw_body(ui, auth);
        });
    *open = still_open;
    (changed, auth_action, update_action)
}
