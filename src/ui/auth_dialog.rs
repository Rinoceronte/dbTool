use crate::claude_auth::AuthStatus;

#[derive(Default)]
pub struct AuthState {
    pub status: Option<AuthStatus>,
    pub last_output: Option<String>,
    pub last_error: Option<String>,
    pub busy: bool,
    /// When true, Login button uses `--console` (API billing) instead of claude.ai.
    pub use_console: bool,
}

pub enum AuthAction {
    None,
    Refresh,
    Login { use_console: bool },
    Logout,
}

/// The Claude account section body; embedded in the Settings window.
pub fn draw_body(ui: &mut egui::Ui, state: &mut AuthState) -> AuthAction {
    let mut action = AuthAction::None;

    ui.label(
        "dbTool's AI agent runs through your local `claude` CLI, \
         which authenticates against your Claude.ai subscription \
         (or an Anthropic Console API key). No separate key needed here.",
    );
    ui.add_space(8.0);

    match &state.status {
        Some(s) if s.logged_in => {
            ui.colored_label(egui::Color32::LIGHT_GREEN, "● Signed in");
            egui::Grid::new("auth_status_grid")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    if let Some(email) = &s.email {
                        ui.label("Email:");
                        ui.label(email);
                        ui.end_row();
                    }
                    if let Some(org) = &s.org_name {
                        ui.label("Organization:");
                        ui.label(org);
                        ui.end_row();
                    }
                    if let Some(sub) = &s.subscription_type {
                        ui.label("Plan:");
                        ui.label(sub);
                        ui.end_row();
                    }
                    if let Some(method) = &s.auth_method {
                        ui.label("Auth method:");
                        ui.label(method);
                        ui.end_row();
                    }
                });
        }
        Some(_) => {
            ui.colored_label(egui::Color32::LIGHT_RED, "● Not signed in");
        }
        None => {
            ui.weak("Status unknown — click Refresh.");
        }
    }

    ui.add_space(8.0);

    ui.horizontal(|ui| {
        ui.label("Login method:");
        ui.radio_value(&mut state.use_console, false, "Claude.ai subscription");
        ui.radio_value(&mut state.use_console, true, "Anthropic Console (API)");
    });

    ui.add_space(4.0);

    ui.horizontal(|ui| {
        let signed_in = state.status.as_ref().map(|s| s.logged_in).unwrap_or(false);
        if ui
            .add_enabled(!state.busy, egui::Button::new("Log in"))
            .on_hover_text(
                "Runs `claude auth login`. A browser window should open; \
                 complete the OAuth flow there.",
            )
            .clicked()
        {
            action = AuthAction::Login { use_console: state.use_console };
        }
        if ui
            .add_enabled(!state.busy && signed_in, egui::Button::new("Log out"))
            .clicked()
        {
            action = AuthAction::Logout;
        }
        if ui
            .add_enabled(!state.busy, egui::Button::new("Refresh"))
            .clicked()
        {
            action = AuthAction::Refresh;
        }
        if state.busy {
            ui.add_space(8.0);
            ui.weak("working…");
        }
    });

    if let Some(err) = &state.last_error {
        ui.add_space(6.0);
        ui.colored_label(egui::Color32::LIGHT_RED, err);
    }
    if let Some(out) = &state.last_output {
        if !out.is_empty() {
            ui.add_space(6.0);
            ui.group(|ui| {
                egui::ScrollArea::vertical()
                    .id_source("auth_output_scroll")
                    .max_height(140.0)
                    .show(ui, |ui| {
                        ui.monospace(out);
                    });
            });
        }
    }

    action
}
