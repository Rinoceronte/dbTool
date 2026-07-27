use uuid::Uuid;

use crate::ai_session::{SessionEvent, SessionId};
use crate::db::{TableKind, TableMeta};
use crate::ui::ActiveConnection;

const DEFAULT_MODEL: &str = "claude-opus-5";

const MODELS: &[(&str, &str)] = &[
    ("claude-opus-5", "Opus 5 (recommended)"),
    ("claude-fable-5", "Fable 5 (most capable)"),
    ("claude-sonnet-5", "Sonnet 5"),
    ("claude-opus-4-8", "Opus 4.8"),
    ("claude-haiku-4-5", "Haiku 4.5 (fastest)"),
];

fn model_label(id: &str) -> &str {
    MODELS
        .iter()
        .find(|(mid, _)| *mid == id)
        .map(|(_, label)| *label)
        .unwrap_or(id)
}

#[derive(Clone)]
pub enum ChatItem {
    User(String),
    Assistant(String),
    Tool {
        id: String,
        name: String,
        /// Display text: SQL for SQL tools, a change preview for DBML tools.
        sql: Option<String>,
        /// For DBML tools, the target file — the card links to the diagram
        /// instead of the SQL editor.
        dbml_file: Option<String>,
        status: ToolStatus,
        summary: Option<String>,
    },
    Error(String),
    Notice(String),
}

#[derive(Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Streaming,
    AwaitingApproval,
    Running,
    Done { ok: bool },
    Rejected,
}

pub struct AiPanelState {
    pub open: bool,
    pub input: String,
    pub display: Vec<ChatItem>,
    pub selected_profile: Option<Uuid>,
    pub current_session: Option<SessionId>,
    /// The claude CLI session id captured from the last `TurnDone`. Passed as
    /// `--resume` on the next turn so the conversation continues.
    pub claude_session_id: Option<String>,
    pub allow_writes: bool,
    pub model: String,
}

impl Default for AiPanelState {
    fn default() -> Self {
        Self {
            open: false,
            input: String::new(),
            display: Vec::new(),
            selected_profile: None,
            current_session: None,
            claude_session_id: None,
            allow_writes: false,
            model: DEFAULT_MODEL.to_string(),
        }
    }
}

impl AiPanelState {
    pub fn clear(&mut self) {
        self.display.clear();
        self.claude_session_id = None;
    }

    pub fn apply_session_event(&mut self, evt: SessionEvent) {
        match evt {
            SessionEvent::TextStart => {
                self.display.push(ChatItem::Assistant(String::new()));
            }
            SessionEvent::TextDelta { text } => {
                if let Some(ChatItem::Assistant(buf)) = self.display.last_mut() {
                    buf.push_str(&text);
                } else {
                    self.display.push(ChatItem::Assistant(text));
                }
            }
            SessionEvent::ToolStart { tool_use_id, name } => {
                self.display.push(ChatItem::Tool {
                    id: tool_use_id,
                    name,
                    sql: None,
                    dbml_file: None,
                    status: ToolStatus::Streaming,
                    summary: None,
                });
            }
            SessionEvent::ToolReady { tool_use_id, sql, dbml_file, needs_approval, .. } => {
                if let Some(item) = self.find_tool_mut(&tool_use_id) {
                    if let ChatItem::Tool { sql: s, dbml_file: f, status, .. } = item {
                        *s = Some(sql);
                        *f = dbml_file;
                        *status = if needs_approval {
                            ToolStatus::AwaitingApproval
                        } else {
                            ToolStatus::Running
                        };
                    }
                }
            }
            SessionEvent::ToolResult { tool_use_id, ok, summary } => {
                if let Some(item) = self.find_tool_mut(&tool_use_id) {
                    if let ChatItem::Tool { status, summary: s, .. } = item {
                        *status = ToolStatus::Done { ok };
                        *s = Some(summary);
                    }
                }
            }
            // Handled by the app (diagram tab reload / view focus); nothing to
            // show here — the ToolResult card already reports the action.
            SessionEvent::DbmlUpdated { .. } | SessionEvent::DbmlFocus { .. } => {}
            SessionEvent::TurnDone { claude_session_id } => {
                if let Some(id) = claude_session_id {
                    self.claude_session_id = Some(id);
                }
                self.current_session = None;
            }
            SessionEvent::Error { message } => {
                self.display.push(ChatItem::Error(message));
                self.current_session = None;
                for item in &mut self.display {
                    if let ChatItem::Tool { status, .. } = item {
                        if matches!(status, ToolStatus::AwaitingApproval | ToolStatus::Streaming | ToolStatus::Running) {
                            *status = ToolStatus::Rejected;
                        }
                    }
                }
            }
        }
    }

    pub fn session_ended(&mut self, session: SessionId) {
        if self.current_session == Some(session) {
            self.current_session = None;
        }
    }

    fn find_tool_mut(&mut self, id: &str) -> Option<&mut ChatItem> {
        self.display.iter_mut().rev().find(|i| matches!(i, ChatItem::Tool { id: x, .. } if x == id))
    }

    pub fn pending_approval(&self) -> Option<(String, String)> {
        self.display.iter().rev().find_map(|i| match i {
            ChatItem::Tool { id, sql, status: ToolStatus::AwaitingApproval, .. } => {
                Some((id.clone(), sql.clone().unwrap_or_default()))
            }
            _ => None,
        })
    }
}

pub enum AiPanelAction {
    None,
    Send {
        session: SessionId,
        profile_id: Option<Uuid>,
        prompt: String,
        system: String,
        model: String,
        allow_writes: bool,
        resume_id: Option<String>,
    },
    Approve {
        session: SessionId,
        tool_use_id: String,
        approved: bool,
    },
    Cancel {
        session: SessionId,
    },
    OpenSqlInEditor {
        profile_id: Option<Uuid>,
        sql: String,
    },
    OpenDbmlDiagram {
        file: String,
    },
    /// User clicked "Replace query" on a SQL card: put the SQL into the
    /// active query tab's buffer.
    ReplaceSqlInActiveTab {
        sql: String,
    },
}

/// What a tool card's buttons asked for.
enum CardAction {
    OpenSql(String),
    ReplaceSql(String),
    OpenDbml(String),
}

pub fn draw(
    ctx: &egui::Context,
    state: &mut AiPanelState,
    active: &[ActiveConnection],
) -> AiPanelAction {
    if !state.open {
        return AiPanelAction::None;
    }

    let mut action = AiPanelAction::None;

    // Keep selection valid; default to first active connection if any.
    if let Some(sel) = state.selected_profile {
        if !active.iter().any(|a| a.profile_id == sel) {
            state.selected_profile = None;
        }
    }
    if state.selected_profile.is_none() {
        state.selected_profile = active.first().map(|a| a.profile_id);
    }

    egui::SidePanel::right("ai_panel")
        .resizable(true)
        .default_width(400.0)
        .min_width(300.0)
        .show(ctx, |ui| {
            // Header
            ui.horizontal(|ui| {
                ui.heading("AI agent");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✖").clicked() {
                        state.open = false;
                    }
                    if ui.add_enabled(state.current_session.is_none(), egui::Button::new("Clear").small()).clicked() {
                        state.clear();
                    }
                });
            });

            // Connection picker
            ui.horizontal(|ui| {
                ui.label("Context:");
                let current_label = state
                    .selected_profile
                    .and_then(|id| active.iter().find(|a| a.profile_id == id))
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| "<none>".to_string());
                let prior = state.selected_profile;
                egui::ComboBox::from_id_source("ai_panel_target")
                    .selected_text(current_label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut state.selected_profile, None, "<none>");
                        // Selection is profile-keyed; extra per-database
                        // connections would be duplicate entries here.
                        for ac in active.iter().filter(|a| a.is_primary) {
                            ui.selectable_value(
                                &mut state.selected_profile,
                                Some(ac.profile_id),
                                &ac.name,
                            );
                        }
                    });
                if state.selected_profile != prior && !state.display.is_empty() {
                    state.clear();
                    state.display.push(ChatItem::Notice(
                        "Connection changed — chat reset.".into(),
                    ));
                }
            });

            ui.horizontal(|ui| {
                ui.checkbox(&mut state.allow_writes, "Auto-approve writes");
                ui.weak("(off → modal asks before each INSERT/UPDATE/DDL)");
            });

            ui.horizontal(|ui| {
                ui.label("Model:");
                egui::ComboBox::from_id_source("ai_panel_model")
                    .selected_text(model_label(&state.model).to_string())
                    .show_ui(ui, |ui| {
                        for (id, label) in MODELS {
                            ui.selectable_value(&mut state.model, (*id).into(), *label);
                        }
                    });
            });

            ui.separator();

            // History scrollback
            let history_height = ui.available_height() - 140.0;
            let mut card_action: Option<CardAction> = None;
            egui::ScrollArea::vertical()
                .id_source("ai_history_scroll")
                .auto_shrink([false, false])
                .max_height(history_height.max(80.0))
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    // Salt each item's scope: repeated widgets ("result"
                    // collapsers, buttons) would otherwise clash ids.
                    for (idx, item) in state.display.iter().enumerate() {
                        let act = ui.push_id(("ai_item", idx), |ui| render_item(ui, item)).inner;
                        if let Some(act) = act {
                            card_action = Some(act);
                        }
                        ui.add_space(6.0);
                    }
                    if state.current_session.is_some() && state.pending_approval().is_none() {
                        ui.weak("…thinking");
                    }
                });
            match card_action {
                Some(CardAction::OpenSql(sql)) => {
                    action = AiPanelAction::OpenSqlInEditor {
                        profile_id: state.selected_profile,
                        sql,
                    };
                }
                Some(CardAction::ReplaceSql(sql)) => {
                    action = AiPanelAction::ReplaceSqlInActiveTab { sql };
                }
                Some(CardAction::OpenDbml(file)) => {
                    action = AiPanelAction::OpenDbmlDiagram { file };
                }
                None => {}
            }

            ui.separator();

            // Input + Send/Cancel. Plain Enter sends (consumed before the
            // TextEdit can insert a newline); Shift+Enter inserts a newline;
            // ⌘/Ctrl+Enter still sends for muscle memory.
            let busy = state.current_session.is_some();
            let edit_id = egui::Id::new("ai_panel_input");
            let has_focus = ui.memory(|m| m.has_focus(edit_id));
            let submit = !busy
                && ((has_focus
                    && ui.input_mut(|i| {
                        i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    }))
                    || ui.input(|i| {
                        i.key_pressed(egui::Key::Enter) && i.modifiers.command_only()
                    }));
            ui.add(
                egui::TextEdit::multiline(&mut state.input)
                    .id(edit_id)
                    .desired_rows(3)
                    .desired_width(f32::INFINITY)
                    .hint_text("Ask about the DB… (Enter sends, Shift+Enter = newline)"),
            );

            ui.horizontal(|ui| {
                if busy {
                    if ui.button("Cancel").clicked() {
                        if let Some(s) = state.current_session {
                            action = AiPanelAction::Cancel { session: s };
                        }
                    }
                    ui.weak("session running");
                } else {
                    let can_send = !state.input.trim().is_empty();
                    let send_clicked =
                        ui.add_enabled(can_send, egui::Button::new("Send")).clicked();
                    if send_clicked || (submit && can_send) {
                        let prompt = std::mem::take(&mut state.input);
                        state.display.push(ChatItem::User(prompt.clone()));
                        let session = SessionId::new_v4();
                        state.current_session = Some(session);
                        let system = build_system_prompt(state.selected_profile, active);
                        action = AiPanelAction::Send {
                            session,
                            profile_id: state.selected_profile,
                            prompt,
                            system,
                            model: state.model.clone(),
                            allow_writes: state.allow_writes,
                            resume_id: state.claude_session_id.clone(),
                        };
                    }
                }
            });
        });

    // Approval modal
    if let Some((tool_use_id, sql)) = state.pending_approval() {
        let mut keep_open = true;
        let session = state.current_session;
        egui::Window::new("Approve SQL statement")
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .open(&mut keep_open)
            .show(ctx, |ui| {
                ui.label("The agent wants to run this against your database:");
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .id_source("ai_approval_sql_scroll")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut sql.as_str().to_string())
                                .code_editor()
                                .desired_width(f32::INFINITY)
                                .interactive(false),
                        );
                    });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("Approve & run").color(egui::Color32::BLACK),
                        ).fill(egui::Color32::LIGHT_GREEN))
                        .clicked()
                    {
                        if let Some(s) = session {
                            action = AiPanelAction::Approve {
                                session: s,
                                tool_use_id: tool_use_id.clone(),
                                approved: true,
                            };
                        }
                    }
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new("Reject").color(egui::Color32::BLACK),
                        ).fill(egui::Color32::LIGHT_RED))
                        .clicked()
                    {
                        if let Some(s) = session {
                            action = AiPanelAction::Approve {
                                session: s,
                                tool_use_id: tool_use_id.clone(),
                                approved: false,
                            };
                        }
                    }
                });
            });
        if !keep_open {
            if let Some(s) = session {
                action = AiPanelAction::Approve {
                    session: s,
                    tool_use_id,
                    approved: false,
                };
            }
        }
    }

    action
}

fn render_item(ui: &mut egui::Ui, item: &ChatItem) -> Option<CardAction> {
    let mut card_action: Option<CardAction> = None;
    match item {
        ChatItem::User(text) => {
            ui.colored_label(egui::Color32::LIGHT_BLUE, "You");
            ui.label(text);
        }
        ChatItem::Assistant(text) => {
            ui.colored_label(egui::Color32::LIGHT_GREEN, "Claude");
            ui.label(text);
        }
        ChatItem::Tool { name, sql, dbml_file, status, summary, .. } => {
            let (icon, color) = match status {
                ToolStatus::Streaming => ("◌", egui::Color32::GRAY),
                ToolStatus::AwaitingApproval => ("?", egui::Color32::YELLOW),
                ToolStatus::Running => ("▶", egui::Color32::LIGHT_BLUE),
                ToolStatus::Done { ok: true } => ("✔", egui::Color32::LIGHT_GREEN),
                ToolStatus::Done { ok: false } => ("✖", egui::Color32::LIGHT_RED),
                ToolStatus::Rejected => ("⊘", egui::Color32::LIGHT_RED),
            };
            ui.horizontal(|ui| {
                ui.colored_label(color, icon);
                ui.weak(name);
            });
            if let Some(sql) = sql {
                ui.code(sql);
                let is_sql_tool = matches!(
                    name.as_str(),
                    crate::ai_tools::RUN_SELECT
                        | crate::ai_tools::RUN_STATEMENT
                        | crate::ai_tools::PROPOSE_SQL
                );
                ui.horizontal(|ui| {
                    if ui.small_button("Copy").clicked() {
                        ui.output_mut(|o| o.copied_text = sql.clone());
                    }
                    if let Some(file) = dbml_file {
                        if ui.small_button("Open diagram").clicked() {
                            card_action = Some(CardAction::OpenDbml(file.clone()));
                        }
                    } else if is_sql_tool {
                        if ui.small_button("Open in editor").clicked() {
                            card_action = Some(CardAction::OpenSql(sql.clone()));
                        }
                        if ui
                            .small_button("Replace query")
                            .on_hover_text("Replace the active query tab's SQL with this")
                            .clicked()
                        {
                            card_action = Some(CardAction::ReplaceSql(sql.clone()));
                        }
                    }
                });
            }
            if let Some(s) = summary {
                ui.collapsing("result", |ui| {
                    egui::ScrollArea::horizontal()
                        .id_source(("ai_tool_result", s.as_ptr() as usize))
                        .max_height(140.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(s).monospace())
                                    .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        });
                });
            }
        }
        ChatItem::Error(msg) => {
            ui.colored_label(egui::Color32::LIGHT_RED, "Error");
            ui.label(msg);
        }
        ChatItem::Notice(msg) => {
            ui.weak(msg);
        }
    }
    card_action
}

fn build_system_prompt(profile: Option<Uuid>, active: &[ActiveConnection]) -> String {
    let base = "You are an AI assistant embedded in dbTool, a desktop database GUI. \
                You can call the `run_select` and `run_statement` MCP tools to inspect \
                and modify the user's currently selected database. Prefer running queries \
                directly when a question can be answered from the data; explain results \
                concisely. For any statement that modifies data or schema, the user will \
                be prompted to approve it — keep your statements minimal and idempotent \
                where possible.\n\
                \n\
                You can also work with the user's DBML diagram documents via `list_dbml`, \
                `read_dbml`, and `update_dbml` (edits also require approval). When the \
                user talks about \"the DBML\", the diagram, groups, or notes, use these \
                tools — not SQL. Each database-owned document has a dbTool-generated \
                region between marker comments — never edit inside it; it is overwritten \
                when the user refreshes from the database. Below the markers is the \
                user's area for `TableGroup` blocks, notes, colors, and draft tables — \
                make organizational edits there. Prefer small `find`/`replace` edits over \
                rewriting whole files. The user can see the diagram live: `focus_table` \
                pans their view to a table, so use it to show where you are working — \
                e.g. focus a table right before or after you change it.\n\
                \n\
                When the user refers to \"this query\", \"my query\", or their editor, \
                call `read_editor` to see the SQL in their active query tab. To hand \
                back a revised or suggested query without running it, call `propose_sql` \
                — the user gets one-click actions to copy it, open it in a new tab, or \
                replace their current query. You cannot write into their editor \
                directly; propose and let them apply.";

    let Some(profile) = profile else {
        return format!("{base}\n\nNo database connection is currently selected — your tools will fail until the user picks one.");
    };
    let Some(ac) = active.iter().find(|a| a.profile_id == profile) else {
        return base.to_string();
    };
    let Some(cache) = ac.schema_cache.as_ref() else {
        return format!(
            "{base}\n\nConnected to `{}` ({:?}). Schema metadata is still loading; you can still call tools but may need to discover structure via SELECTs.",
            ac.name, ac.kind
        );
    };
    let mut out = String::new();
    out.push_str(base);
    out.push_str(&format!(
        "\n\nConnected to `{}` (dialect: {:?}). Default schema: `{}`.\n",
        ac.name,
        ac.kind,
        cache.default_schema()
    ));
    let dbml_path =
        crate::dbml::database_dbml_path(ac.profile_id, &ac.database, ac.is_primary);
    if let Some(file) = dbml_path.file_name().map(|f| f.to_string_lossy().to_string()) {
        if dbml_path.exists() {
            out.push_str(&format!("DBML document for this connection: `{file}`.\n"));
        } else {
            out.push_str(
                "No DBML document exists for this connection yet (the user can generate \
                 one via View as DBML).\n",
            );
        }
    }
    out.push('\n');
    out.push_str("Schema reference:\n");
    let mut by_schema: std::collections::BTreeMap<&str, Vec<&TableMeta>> =
        std::collections::BTreeMap::new();
    for t in cache.all_tables() {
        by_schema.entry(t.schema.as_str()).or_default().push(t);
    }
    for (schema, tables) in by_schema {
        out.push_str(&format!("\nSchema `{schema}`:\n"));
        for t in tables {
            let kind_str = match t.kind {
                TableKind::Table => "table",
                TableKind::View => "view",
            };
            out.push_str(&format!("  {} `{}`:\n", kind_str, t.name));
            for c in &t.columns {
                let pk = if c.is_primary_key { " PK" } else { "" };
                let null = if c.nullable { "" } else { " NOT NULL" };
                out.push_str(&format!("    - {} {}{}{}\n", c.name, c.type_name, null, pk));
            }
            for fk in &t.foreign_keys {
                out.push_str(&format!(
                    "    FK: {} -> {}.{}.{}\n",
                    fk.from_column, fk.to_schema, fk.to_table, fk.to_column
                ));
            }
        }
    }
    out
}
