//! Settings window: appearance, editor toggles, and the Claude account.

use crate::settings::Settings;
use crate::ui::auth_dialog::{self, AuthAction, AuthState};
use crate::ui::theme::ThemeMode;

/// Returns whether any setting changed (the caller re-installs the theme and
/// persists), plus any account action.
pub fn draw(
    ctx: &egui::Context,
    open: &mut bool,
    settings: &mut Settings,
    auth: &mut AuthState,
) -> (bool, AuthAction) {
    let mut changed = false;
    let mut auth_action = AuthAction::None;
    if !*open {
        return (changed, auth_action);
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
            ui.label(egui::RichText::new("Claude account").strong());
            ui.add_space(2.0);
            auth_action = auth_dialog::draw_body(ui, auth);
        });
    *open = still_open;
    (changed, auth_action)
}
