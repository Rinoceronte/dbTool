//! Structure view of a table tab: object tree on the left, adaptive detail
//! form on the right, generated-SQL preview docked at the bottom.
//!
//! Follows the app's staged-changes idiom: edits only mutate the working
//! model; nothing reaches the database until ⬆ Apply.

use crate::db::DbKind;
use crate::db::structure::{
    self, ChangeState, Danger, DdlStatement, FK_ACTIONS, IdentityKind, ObjId, WorkingTable,
};

use super::{StructSel, TabStatus, TableEditorTab, theme};

pub enum StructureAction {
    None,
    /// First entry into the Structure view — needs `describe_structure`.
    Load,
    Reload,
    Apply { statements: Vec<String> },
    OpenSql { sql: String },
}

/// Deferred mutations collected while the tree is drawn (avoids borrowing
/// the working table mutably inside the render closures).
enum TreeOp {
    Select(StructSel),
    AddColumn,
    AddIndex,
    AddFk,
    AddCheck,
    /// Toggle a column in/out of the primary key (column-form checkbox).
    TogglePkMember(ObjId),
    /// Toggle dropped on an existing object, or remove an added one.
    RemoveSelected,
}

pub fn draw(ui: &mut egui::Ui, tab: &mut TableEditorTab) -> StructureAction {
    let mut action = StructureAction::None;
    let kind = tab.db_kind;

    if tab.structure.working.is_none() {
        if tab.structure.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("introspecting table structure…");
            });
        } else {
            action = StructureAction::Load;
        }
        return action;
    }

    let stmts = structure::generate(tab.structure.working.as_ref().unwrap());
    let changes = tab.structure.working.as_ref().unwrap().change_count();
    let has_danger = stmts.iter().any(|s| s.danger != Danger::Safe);
    let can_apply = !stmts.is_empty() && !tab.structure.applying;

    // ---- toolbar -----------------------------------------------------------
    let mut apply_clicked = false;
    ui.horizontal(|ui| {
        if !tab.structure.is_new_table {
            let reload = ui
                .add_enabled(!tab.structure.applying, egui::Button::new("⟳"))
                .on_hover_text("Reload structure (discards staged changes)");
            if reload.clicked() {
                action = StructureAction::Reload;
            }
        }
        let label = match changes {
            0 => "no changes".to_string(),
            1 => "✎ 1 change".to_string(),
            n => format!("✎ {n} changes"),
        };
        let color = if changes > 0 {
            ui.visuals().warn_fg_color
        } else {
            ui.visuals().weak_text_color()
        };
        ui.label(egui::RichText::new(label).color(color));

        let apply_txt = if tab.structure.is_new_table { "⬆ Create table" } else { "⬆ Apply" };
        let apply_btn = egui::Button::new(
            egui::RichText::new(apply_txt).color(egui::Color32::WHITE),
        )
        .fill(theme::ACCENT);
        if ui
            .add_enabled(can_apply, apply_btn)
            .on_hover_text("Apply staged structure changes (Ctrl+Enter)")
            .clicked()
        {
            apply_clicked = true;
        }
        match &tab.status {
            TabStatus::Idle => {}
            TabStatus::Running(msg) => {
                ui.spinner();
                ui.weak(msg.clone());
            }
            TabStatus::Error(e) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("✖ {e}"));
            }
            TabStatus::Info(i) => {
                ui.weak(i.clone());
            }
        }
    });

    // Ctrl+Enter applies, matching the AI panel submit chord.
    if can_apply
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter))
    {
        apply_clicked = true;
    }

    if apply_clicked {
        if has_danger {
            tab.structure.confirm_open = true;
            tab.structure.confirm_ack = false;
        } else {
            action = StructureAction::Apply { statements: sql_of(&stmts) };
        }
    }

    ui.add_space(2.0);

    // ---- bottom SQL preview -------------------------------------------------
    let preview_action = egui::TopBottomPanel::bottom(egui::Id::new(("structure_sql", tab.id)))
        .resizable(true)
        .default_height(150.0)
        .min_height(60.0)
        .show_inside(ui, |ui| draw_preview(ui, kind, &stmts))
        .inner;
    if let Some(a) = preview_action {
        action = a;
    }

    // ---- left tree ----------------------------------------------------------
    let mut ops: Vec<TreeOp> = Vec::new();
    egui::SidePanel::left(egui::Id::new(("structure_tree", tab.id)))
        .resizable(true)
        .default_width(300.0)
        .min_width(180.0)
        .show_inside(ui, |ui| {
            draw_tree(ui, tab, &mut ops);
        });

    // ---- right detail form --------------------------------------------------
    egui::CentralPanel::default().show_inside(ui, |ui| {
        draw_form(ui, tab, &mut ops);
    });

    apply_tree_ops(tab, ops);

    // ---- destructive confirm dialog ----------------------------------------
    if tab.structure.confirm_open {
        if let Some(a) = draw_confirm(ui.ctx(), tab, &stmts) {
            action = a;
        }
    }

    action
}

fn sql_of(stmts: &[DdlStatement]) -> Vec<String> {
    stmts.iter().map(|s| s.sql.clone()).collect()
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

fn draw_tree(ui: &mut egui::Ui, tab: &mut TableEditorTab, ops: &mut Vec<TreeOp>) {
    let st = &tab.structure;
    let Some(wt) = st.working.as_ref() else { return };
    let selected = st.selected;

    // Toolbar: remove only — adding lives on each group header's ＋.
    ui.horizontal(|ui| {
        let removable = matches!(
            selected,
            Some(StructSel::Column(_) | StructSel::Pk | StructSel::Fk(_) | StructSel::Index(_) | StructSel::Check(_))
        );
        let remove = ui
            .add_enabled(removable, egui::Button::new("- Remove"))
            .on_hover_text("Stage drop (again to unstage); removes never-applied objects");
        if remove.clicked() {
            ops.push(TreeOp::RemoveSelected);
        }
    });
    ui.separator();

    // `row` and `group` closures coexist and both queue ops — collect via
    // RefCell and drain into `ops` at the end.
    let pending: std::cell::RefCell<Vec<TreeOp>> = std::cell::RefCell::new(Vec::new());

    let add_fg = theme::add_fg(ui);
    let del_fg = theme::delete_fg(ui);
    let warn_fg = ui.visuals().warn_fg_color;

    // One selectable, state-tinted row in the object tree.
    let row = |ui: &mut egui::Ui,
                   sel: StructSel,
                   state: ChangeState,
                   label: String,
                   detail: String| {
        let is_sel = selected == Some(sel);
        let (marker, fg) = match state {
            ChangeState::Added => ("+ ", Some(add_fg)),
            ChangeState::Modified => ("● ", Some(warn_fg)),
            ChangeState::Dropped => ("⊘ ", Some(del_fg)),
            ChangeState::Unchanged => ("", None),
        };
        let mut text = egui::RichText::new(format!("{marker}{label}"));
        if let Some(c) = fg {
            text = text.color(c);
        }
        if state == ChangeState::Dropped {
            text = text.strikethrough();
        }
        let bg = match state {
            ChangeState::Added => Some(theme::add_bg(ui)),
            ChangeState::Modified => Some(theme::pending_bg(ui)),
            ChangeState::Dropped => Some(theme::delete_bg(ui)),
            ChangeState::Unchanged => None,
        };
        // Reserve a paint slot first so the tint renders BEHIND the row text.
        let bg_slot = bg.map(|_| ui.painter().add(egui::Shape::Noop));
        let inner = ui.horizontal(|ui| {
            let r = ui.selectable_label(is_sel, text);
            if !detail.is_empty() {
                ui.label(
                    egui::RichText::new(detail)
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
            }
            r
        });
        if let (Some(bg), Some(slot)) = (bg, bg_slot) {
            ui.painter().set(
                slot,
                egui::Shape::rect_filled(
                    inner.response.rect.expand2(egui::vec2(2.0, 1.0)),
                    2.0,
                    bg,
                ),
            );
        }
        if inner.inner.clicked() {
            pending.borrow_mut().push(TreeOp::Select(sel));
        }
    };

    egui::ScrollArea::vertical()
        .id_source(("structure_tree_scroll", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Table root.
            {
                let state = if wt.original_name.is_none() {
                    ChangeState::Added
                } else if wt.table_renamed() {
                    ChangeState::Modified
                } else {
                    ChangeState::Unchanged
                };
                row(ui, StructSel::Table, state, format!("▦ {}", wt.name), wt.schema.clone());
            }

            // Collapsible group with the count in the header and an optional
            // ＋ button on the right — the natural "add here" affordance.
            let group = |ui: &mut egui::Ui,
                             key: &str,
                             title: String,
                             default_open: bool,
                             add: Option<(TreeOp, &str)>,
                             body: &mut dyn FnMut(&mut egui::Ui)| {
                let state = egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    ui.make_persistent_id((key, tab.id)),
                    default_open,
                );
                let mut add_clicked = false;
                let (_, header, body_resp) = state
                    .show_header(ui, |ui| {
                        ui.label(egui::RichText::new(title).strong());
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if let Some((_, tip)) = &add {
                                    if ui
                                        .add(egui::Button::new("+").small().frame(false))
                                        .on_hover_text(*tip)
                                        .clicked()
                                    {
                                        add_clicked = true;
                                    }
                                }
                            },
                        );
                    })
                    .body(|ui| body(ui));
                let _ = (header, body_resp);
                if add_clicked {
                    if let Some((op, _)) = add {
                        pending.borrow_mut().push(op);
                    }
                }
            };
            let live_cols = wt.columns.iter().filter(|c| !c.dropped).count();
            group(
                ui,
                "cols",
                format!("Columns ({live_cols})"),
                true,
                Some((TreeOp::AddColumn, "Add column")),
                &mut |ui| {
                    for c in &wt.columns {
                        let pk_mark = if wt.is_pk_member(c.id) { "🔑 " } else { "" };
                        row(
                            ui,
                            StructSel::Column(c.id),
                            c.state(),
                            format!("{pk_mark}{}", c.name),
                            c.type_name.clone(),
                        );
                    }
                },
            );

            group(
                ui,
                "keys",
                format!("Keys ({})", if wt.pk.present { 1 } else { 0 }),
                true,
                None,
                &mut |ui| {
                    if wt.pk.present || wt.pk.origin.is_some() {
                        let cols = wt
                            .pk
                            .column_ids
                            .iter()
                            .filter_map(|id| wt.col_name(*id))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let label = if wt.pk.name.is_empty() {
                            "PRIMARY KEY".to_string()
                        } else {
                            wt.pk.name.clone()
                        };
                        row(ui, StructSel::Pk, wt.pk.state(), format!("🔑 {label}"), format!("({cols})"));
                    } else {
                        ui.weak("(none — tick “Primary key” on a column)");
                    }
                },
            );

            let live_fks = wt.foreign_keys.iter().filter(|f| !f.dropped).count();
            group(
                ui,
                "fks",
                format!("Foreign keys ({live_fks})"),
                true,
                Some((TreeOp::AddFk, "Add foreign key")),
                &mut |ui| {
                    if wt.foreign_keys.is_empty() {
                        ui.weak("(none)");
                    }
                    for fk in &wt.foreign_keys {
                        row(
                            ui,
                            StructSel::Fk(fk.id),
                            fk.state(),
                            format!("⇄ {}", fk.name),
                            format!("→ {}", fk.ref_table),
                        );
                    }
                },
            );

            let live_ix = wt.indexes.iter().filter(|i| !i.dropped).count();
            group(
                ui,
                "idx",
                format!("Indexes ({live_ix})"),
                true,
                Some((TreeOp::AddIndex, "Add index")),
                &mut |ui| {
                    if wt.indexes.is_empty() {
                        ui.weak("(none)");
                    }
                    for ix in &wt.indexes {
                        let cols = ix
                            .column_ids
                            .iter()
                            .filter_map(|id| wt.col_name(*id))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let detail = if ix.unique { format!("({cols}) UNIQUE") } else { format!("({cols})") };
                        row(ui, StructSel::Index(ix.id), ix.state(), format!("◌ {}", ix.name), detail);
                    }
                },
            );

            let live_ck = wt.checks.iter().filter(|c| !c.dropped).count();
            group(
                ui,
                "checks",
                format!("Checks ({live_ck})"),
                false,
                Some((TreeOp::AddCheck, "Add check constraint")),
                &mut |ui| {
                    if wt.checks.is_empty() {
                        ui.weak("(none)");
                    }
                    for ck in &wt.checks {
                        row(ui, StructSel::Check(ck.id), ck.state(), format!("✓ {}", ck.name), String::new());
                    }
                },
            );
        });

    ops.extend(pending.into_inner());
}

fn apply_tree_ops(tab: &mut TableEditorTab, ops: Vec<TreeOp>) {
    let st = &mut tab.structure;
    let Some(wt) = st.working.as_mut() else { return };
    for op in ops {
        match op {
            TreeOp::Select(sel) => st.selected = Some(sel),
            TreeOp::AddColumn => {
                let id = wt.add_column();
                st.selected = Some(StructSel::Column(id));
                st.focus_col = Some(id);
            }
            TreeOp::AddIndex => {
                let id = wt.add_index();
                st.selected = Some(StructSel::Index(id));
            }
            TreeOp::AddFk => {
                let id = wt.add_fk();
                st.selected = Some(StructSel::Fk(id));
            }
            TreeOp::AddCheck => {
                let id = wt.add_check();
                st.selected = Some(StructSel::Check(id));
            }
            TreeOp::TogglePkMember(id) => {
                if wt.pk.column_ids.contains(&id) {
                    wt.pk.column_ids.retain(|cid| *cid != id);
                    if wt.pk.column_ids.is_empty() {
                        wt.pk.present = false;
                    }
                } else {
                    wt.pk.present = true;
                    wt.pk.column_ids.push(id);
                }
            }
            TreeOp::RemoveSelected => match st.selected {
                Some(StructSel::Column(id)) => {
                    if let Some(pos) = wt.columns.iter().position(|c| c.id == id) {
                        if wt.columns[pos].origin.is_none() {
                            wt.columns.remove(pos);
                            wt.pk.column_ids.retain(|cid| *cid != id);
                            for ix in &mut wt.indexes {
                                ix.column_ids.retain(|cid| *cid != id);
                            }
                            for fk in &mut wt.foreign_keys {
                                fk.column_ids.retain(|cid| *cid != id);
                            }
                            st.selected = None;
                        } else {
                            wt.columns[pos].dropped = !wt.columns[pos].dropped;
                        }
                    }
                }
                Some(StructSel::Pk) => {
                    if wt.pk.origin.is_none() {
                        wt.pk.present = false;
                        wt.pk.column_ids.clear();
                        st.selected = None;
                    } else {
                        wt.pk.present = !wt.pk.present;
                    }
                }
                Some(StructSel::Fk(id)) => {
                    if let Some(pos) = wt.foreign_keys.iter().position(|f| f.id == id) {
                        if wt.foreign_keys[pos].origin.is_none() {
                            wt.foreign_keys.remove(pos);
                            st.selected = None;
                        } else {
                            wt.foreign_keys[pos].dropped = !wt.foreign_keys[pos].dropped;
                        }
                    }
                }
                Some(StructSel::Index(id)) => {
                    if let Some(pos) = wt.indexes.iter().position(|i| i.id == id) {
                        if wt.indexes[pos].origin.is_none() {
                            wt.indexes.remove(pos);
                            st.selected = None;
                        } else {
                            wt.indexes[pos].dropped = !wt.indexes[pos].dropped;
                        }
                    }
                }
                Some(StructSel::Check(id)) => {
                    if let Some(pos) = wt.checks.iter().position(|c| c.id == id) {
                        if wt.checks[pos].origin.is_none() {
                            wt.checks.remove(pos);
                            st.selected = None;
                        } else {
                            wt.checks[pos].dropped = !wt.checks[pos].dropped;
                        }
                    }
                }
                _ => {}
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Detail form
// ---------------------------------------------------------------------------

fn draw_form(ui: &mut egui::Ui, tab: &mut TableEditorTab, ops: &mut Vec<TreeOp>) {
    let kind = tab.db_kind;
    let focus_col = tab.structure.focus_col.take();
    let mut ac_idx = tab.structure.type_ac_index;
    let st = &mut tab.structure;
    let Some(wt) = st.working.as_mut() else { return };

    let Some(sel) = st.selected else {
        ui.add_space(12.0);
        ui.weak("Select an object on the left, or add one with + on a group header.");
        return;
    };

    egui::ScrollArea::vertical()
        .id_source(("structure_form_scroll", tab.id))
        .auto_shrink([false, false])
        .show(ui, |ui| match sel {
            StructSel::Table => {
                ui.strong("TABLE");
                ui.add_space(4.0);
                egui::Grid::new(("tbl_form", tab.id))
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Schema");
                        ui.label(egui::RichText::new(&wt.schema).monospace());
                        ui.end_row();
                        ui.label("Name");
                        ui.add(egui::TextEdit::singleline(&mut wt.name).desired_width(260.0));
                        ui.end_row();
                    });
                if wt.table_renamed() {
                    ui.add_space(4.0);
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("Will be renamed from “{}”.", wt.original_name.as_deref().unwrap_or_default()),
                    );
                }
            }

            StructSel::Column(id) => {
                // Names of other live columns, for uniqueness hints if wanted later.
                let Some(pos) = wt.columns.iter().position(|c| c.id == id) else { return };
                let is_pk_member = wt.is_pk_member(id);
                let c = &mut wt.columns[pos];
                let editable = !c.dropped && !c.generated;
                let is_new = c.origin.is_none();

                ui.horizontal(|ui| {
                    ui.strong("COLUMN");
                    if is_pk_member {
                        ui.label("🔑 primary key member");
                    }
                    if c.generated {
                        ui.colored_label(ui.visuals().warn_fg_color, "generated — read-only");
                    }
                    if c.dropped {
                        ui.colored_label(theme::delete_fg(ui), "staged for drop");
                    }
                });
                ui.add_space(4.0);

                let mut spawn_next = false;
                let mut toggle_pk = false;
                egui::Grid::new(("col_form", tab.id, id))
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        let name_edit = ui.add_enabled(
                            editable,
                            egui::TextEdit::singleline(&mut c.name).desired_width(260.0),
                        );
                        if focus_col == Some(id) {
                            name_edit.request_focus();
                        }
                        // Enter in a NEW column's name commits it and spawns
                        // the next one — the fast add-many-columns flow.
                        if is_new
                            && name_edit.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            spawn_next = true;
                        }
                        ui.end_row();

                        // Type is edited as base + optional length so users
                        // type "varchar" and "255", not "varchar(255)".
                        let (mut base, mut params) = structure::split_type(&c.type_name);
                        ui.label("Type");
                        {
                            let type_edit = ui.add_enabled(
                                editable,
                                egui::TextEdit::singleline(&mut base)
                                    .hint_text("start typing…")
                                    .desired_width(200.0),
                            );
                            let popup_id = ui.make_persistent_id(("type_ac", tab.id, id));
                            if type_edit.changed() {
                                ac_idx = 0;
                                ui.memory_mut(|m| m.open_popup(popup_id));
                            }
                            // Substring match ("int" finds bigint), prefix
                            // matches ranked first.
                            let needle = base.trim().to_ascii_lowercase();
                            let mut matches: Vec<&str> = structure::base_type_suggestions(kind)
                                .iter()
                                .filter(|t| needle.is_empty() || t.contains(needle.as_str()))
                                .copied()
                                .collect();
                            matches.sort_by_key(|t| (!t.starts_with(needle.as_str()), *t));
                            let mut accepted: Option<&str> = None;
                            if !matches.is_empty() && matches != [base.as_str()] {
                                let popup_open = ui.memory(|m| m.is_popup_open(popup_id));
                                if popup_open && type_edit.has_focus() {
                                    ui.input_mut(|i| {
                                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                                            ac_idx = (ac_idx + 1).min(matches.len() - 1);
                                        }
                                        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                                            ac_idx = ac_idx.saturating_sub(1);
                                        }
                                    });
                                }
                                ac_idx = ac_idx.min(matches.len() - 1);
                                egui::popup_below_widget(
                                    ui,
                                    popup_id,
                                    &type_edit,
                                    egui::PopupCloseBehavior::CloseOnClick,
                                    |ui: &mut egui::Ui| {
                                    ui.set_min_width(200.0);
                                    for (i, m) in matches.iter().enumerate() {
                                        if ui.selectable_label(i == ac_idx, *m).clicked() {
                                            accepted = Some(m);
                                        }
                                    }
                                });

                                // Tab or Enter completes to the highlighted
                                // match — Tab additionally moves focus on to
                                // the next field, which is usually what you
                                // want (type, Tab, length).
                                if type_edit.lost_focus()
                                    && popup_open
                                    && ui.input(|i| {
                                        i.key_pressed(egui::Key::Enter)
                                            || i.key_pressed(egui::Key::Tab)
                                    })
                                {
                                    accepted = Some(matches[ac_idx]);
                                }
                            }
                            if let Some(a) = accepted {
                                base = a.to_string();
                                if structure::type_params_label(&base).is_none() {
                                    params.clear();
                                }
                                ui.memory_mut(|m| m.close_popup());
                            }
                        }
                        ui.end_row();

                        let params_label = structure::type_params_label(&base);
                        if params_label.is_some() || !params.is_empty() {
                            ui.label(params_label.unwrap_or("Length"));
                            ui.add_enabled(
                                editable,
                                egui::TextEdit::singleline(&mut params)
                                    .hint_text(match params_label {
                                        Some("Precision, scale") => "e.g. 12,2",
                                        _ => "e.g. 255",
                                    })
                                    .desired_width(100.0),
                            );
                            ui.end_row();
                        }
                        c.type_name = structure::join_type(&base, &params);

                        ui.label("");
                        ui.add_enabled_ui(editable, |ui| {
                            ui.checkbox(&mut c.not_null, "Not Null");
                        });
                        ui.end_row();

                        ui.label("");
                        let mut pk_checked = is_pk_member;
                        if ui
                            .add_enabled(editable, egui::Checkbox::new(&mut pk_checked, "Primary key"))
                            .changed()
                        {
                            toggle_pk = true;
                        }
                        ui.end_row();

                        ui.label("");
                        let auto_label = match kind {
                            DbKind::Postgres => "Auto increment (identity)",
                            DbKind::MySql => "Auto increment (AUTO_INCREMENT)",
                            DbKind::MsSql => "Auto increment (IDENTITY)",
                            DbKind::Sqlite => "Auto increment (AUTOINCREMENT)",
                        };
                        let mut auto = c.identity != IdentityKind::None;
                        let auto_resp = ui.add_enabled(
                            editable && is_new,
                            egui::Checkbox::new(&mut auto, auto_label),
                        );
                        if !is_new {
                            auto_resp.on_disabled_hover_text(
                                "Changing auto-increment on an existing column requires a manual migration",
                            );
                        } else if auto_resp.changed() {
                            c.identity = if auto {
                                structure::default_identity(kind)
                            } else {
                                IdentityKind::None
                            };
                        }
                        ui.end_row();

                        ui.label("Default");
                        if c.identity == IdentityKind::None {
                            ui.add_enabled(
                                editable,
                                egui::TextEdit::singleline(&mut c.default)
                                    .hint_text("expression, e.g. now() or 'ACTIVE'")
                                    .desired_width(260.0),
                            );
                        } else {
                            ui.weak("(managed by auto increment)");
                        }
                        ui.end_row();
                    });

                if toggle_pk {
                    ops.push(TreeOp::TogglePkMember(id));
                }
                if spawn_next {
                    ops.push(TreeOp::AddColumn);
                }
            }

            StructSel::Pk => {
                ui.strong("PRIMARY KEY");
                ui.add_space(4.0);
                if !wt.pk.present {
                    ui.colored_label(theme::delete_fg(ui), "staged for drop");
                    return;
                }
                if kind != DbKind::MySql {
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        ui.add(
                            egui::TextEdit::singleline(&mut wt.pk.name)
                                .hint_text("(default)")
                                .desired_width(240.0),
                        );
                    });
                }
                let mut ids = wt.pk.column_ids.clone();
                column_picker(ui, ("pk_cols", tab.id), wt, &mut ids);
                wt.pk.column_ids = ids;
            }

            StructSel::Fk(id) => {
                let col_choices: Vec<(ObjId, String)> = wt
                    .columns
                    .iter()
                    .filter(|c| !c.dropped)
                    .map(|c| (c.id, c.name.clone()))
                    .collect();
                let Some(fk) = wt.foreign_keys.iter_mut().find(|f| f.id == id) else { return };
                ui.horizontal(|ui| {
                    ui.strong("FOREIGN KEY");
                    if fk.dropped {
                        ui.colored_label(theme::delete_fg(ui), "staged for drop");
                    }
                });
                ui.add_space(4.0);
                let editable = !fk.dropped;
                egui::Grid::new(("fk_form", tab.id, id))
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.add_enabled(
                            editable,
                            egui::TextEdit::singleline(&mut fk.name).desired_width(260.0),
                        );
                        ui.end_row();

                        ui.label("Ref schema");
                        ui.add_enabled(
                            editable,
                            egui::TextEdit::singleline(&mut fk.ref_schema).desired_width(200.0),
                        );
                        ui.end_row();

                        ui.label("Ref table");
                        ui.add_enabled(
                            editable,
                            egui::TextEdit::singleline(&mut fk.ref_table).desired_width(200.0),
                        );
                        ui.end_row();

                        ui.label("Ref columns");
                        ui.add_enabled(
                            editable,
                            egui::TextEdit::singleline(&mut fk.ref_columns)
                                .hint_text("id  (comma-separated for composite)")
                                .desired_width(260.0),
                        );
                        ui.end_row();

                        ui.label("On delete");
                        egui::ComboBox::from_id_source(("fk_del", tab.id, id))
                            .selected_text(&fk.on_delete)
                            .show_ui(ui, |ui| {
                                for a in FK_ACTIONS {
                                    ui.selectable_value(&mut fk.on_delete, a.to_string(), *a);
                                }
                            });
                        ui.end_row();

                        ui.label("On update");
                        egui::ComboBox::from_id_source(("fk_upd", tab.id, id))
                            .selected_text(&fk.on_update)
                            .show_ui(ui, |ui| {
                                for a in FK_ACTIONS {
                                    ui.selectable_value(&mut fk.on_update, a.to_string(), *a);
                                }
                            });
                        ui.end_row();
                    });
                ui.add_space(4.0);
                ui.label("Local columns");
                let mut ids = fk.column_ids.clone();
                column_picker_with(ui, ("fk_cols", tab.id, id), &col_choices, &mut ids);
                fk.column_ids = ids;
            }

            StructSel::Index(id) => {
                let col_choices: Vec<(ObjId, String)> = wt
                    .columns
                    .iter()
                    .filter(|c| !c.dropped)
                    .map(|c| (c.id, c.name.clone()))
                    .collect();
                let Some(ix) = wt.indexes.iter_mut().find(|i| i.id == id) else { return };
                ui.horizontal(|ui| {
                    ui.strong("INDEX");
                    if ix.dropped {
                        ui.colored_label(theme::delete_fg(ui), "staged for drop");
                    }
                });
                ui.add_space(4.0);
                let editable = !ix.dropped;
                egui::Grid::new(("ix_form", tab.id, id))
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.add_enabled(
                            editable,
                            egui::TextEdit::singleline(&mut ix.name).desired_width(260.0),
                        );
                        ui.end_row();
                        ui.label("");
                        ui.add_enabled_ui(editable, |ui| {
                            ui.checkbox(&mut ix.unique, "Unique");
                        });
                        ui.end_row();
                    });
                ui.add_space(4.0);
                ui.label("Columns (in order)");
                let mut ids = ix.column_ids.clone();
                column_picker_with(ui, ("ix_cols", tab.id, id), &col_choices, &mut ids);
                ix.column_ids = ids;
            }

            StructSel::Check(id) => {
                let Some(ck) = wt.checks.iter_mut().find(|c| c.id == id) else { return };
                ui.horizontal(|ui| {
                    ui.strong("CHECK CONSTRAINT");
                    if ck.dropped {
                        ui.colored_label(theme::delete_fg(ui), "staged for drop");
                    }
                });
                ui.add_space(4.0);
                let editable = !ck.dropped;
                egui::Grid::new(("ck_form", tab.id, id))
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.add_enabled(
                            editable,
                            egui::TextEdit::singleline(&mut ck.name).desired_width(260.0),
                        );
                        ui.end_row();
                        ui.label("Expression");
                        ui.add_enabled(
                            editable,
                            egui::TextEdit::singleline(&mut ck.expression)
                                .hint_text("e.g. price > 0")
                                .desired_width(320.0),
                        );
                        ui.end_row();
                    });
            }
        });

    tab.structure.type_ac_index = ac_idx;
}

/// Ordered column list editor: chips with ✖ plus an "add column…" picker.
fn column_picker(ui: &mut egui::Ui, id_src: impl std::hash::Hash, wt: &WorkingTable, ids: &mut Vec<ObjId>) {
    let choices: Vec<(ObjId, String)> = wt
        .columns
        .iter()
        .filter(|c| !c.dropped)
        .map(|c| (c.id, c.name.clone()))
        .collect();
    column_picker_with(ui, id_src, &choices, ids);
}

fn column_picker_with(
    ui: &mut egui::Ui,
    id_src: impl std::hash::Hash,
    choices: &[(ObjId, String)],
    ids: &mut Vec<ObjId>,
) {
    let mut remove: Option<usize> = None;
    ui.horizontal_wrapped(|ui| {
        for (i, cid) in ids.iter().enumerate() {
            let name = choices
                .iter()
                .find(|(id, _)| id == cid)
                .map(|(_, n)| n.as_str())
                .unwrap_or("?");
            if ui
                .button(format!("{name} ✖"))
                .on_hover_text("Remove from list")
                .clicked()
            {
                remove = Some(i);
            }
        }
        let available: Vec<&(ObjId, String)> =
            choices.iter().filter(|(id, _)| !ids.contains(id)).collect();
        if !available.is_empty() {
            ui.menu_button("+ column…", |ui| {
                for (cid, name) in available {
                    if ui.button(name).clicked() {
                        ids.push(*cid);
                        ui.close_menu();
                    }
                }
            });
        }
    });
    if let Some(i) = remove {
        ids.remove(i);
    }
    let _ = id_src;
}

// ---------------------------------------------------------------------------
// SQL preview
// ---------------------------------------------------------------------------

fn draw_preview(ui: &mut egui::Ui, kind: DbKind, stmts: &[DdlStatement]) -> Option<StructureAction> {
    let mut action = None;
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.strong("SQL preview");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let enabled = !stmts.is_empty();
            if ui
                .add_enabled(enabled, egui::Button::new("Open in SQL tab"))
                .on_hover_text("Edit the script by hand in a query tab")
                .clicked()
            {
                action = Some(StructureAction::OpenSql { sql: structure::script(stmts) });
            }
            if ui
                .add_enabled(enabled, egui::Button::new("Copy"))
                .clicked()
            {
                ui.output_mut(|o| o.copied_text = structure::script(stmts));
            }
        });
    });

    if stmts.len() > 1 {
        let (icon, msg, warn) = match kind {
            DbKind::MySql => (
                "⚠",
                "MySQL applies each statement separately and cannot roll back — if one fails, earlier ones stay applied.",
                true,
            ),
            _ => ("ⓘ", "Applied atomically in a transaction.", false),
        };
        let color = if warn { ui.visuals().warn_fg_color } else { ui.visuals().weak_text_color() };
        ui.colored_label(color, format!("{icon} {msg}"));
    }

    egui::ScrollArea::both()
        .id_source("structure_sql_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if stmts.is_empty() {
                ui.weak("No pending changes.");
                return;
            }
            for s in stmts {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(format!("{};", s.sql)).monospace());
                    match s.danger {
                        Danger::Safe => {}
                        Danger::Lossy => {
                            ui.colored_label(
                                ui.visuals().warn_fg_color,
                                format!("⚠ {}", s.note.unwrap_or("may lose data")),
                            );
                        }
                        Danger::Destructive => {
                            ui.colored_label(
                                ui.visuals().error_fg_color,
                                format!("⚠ {}", s.note.unwrap_or("destructive")),
                            );
                        }
                    }
                });
            }
        });
    action
}

// ---------------------------------------------------------------------------
// Destructive-change confirmation
// ---------------------------------------------------------------------------

fn draw_confirm(
    ctx: &egui::Context,
    tab: &mut TableEditorTab,
    stmts: &[DdlStatement],
) -> Option<StructureAction> {
    let mut action = None;
    let mut open = true;
    let dangerous: Vec<&DdlStatement> =
        stmts.iter().filter(|s| s.danger != Danger::Safe).collect();
    let n_destructive = dangerous.iter().filter(|s| s.danger == Danger::Destructive).count();
    let n_lossy = dangerous.len() - n_destructive;

    egui::Window::new("Confirm structure changes")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label("These statements can destroy or lose data:");
            ui.add_space(4.0);
            for s in &dangerous {
                let color = match s.danger {
                    Danger::Destructive => ui.visuals().error_fg_color,
                    _ => ui.visuals().warn_fg_color,
                };
                ui.colored_label(color, egui::RichText::new(format!("{};", s.sql)).monospace());
                if let Some(note) = s.note {
                    ui.weak(format!("   {note}"));
                }
            }
            ui.add_space(8.0);
            let ack_text = match (n_destructive, n_lossy) {
                (0, _) => format!("I understand {n_lossy} change(s) may fail or lose data"),
                (_, 0) => format!("I understand {n_destructive} object(s) will be permanently dropped"),
                _ => format!(
                    "I understand {n_destructive} object(s) will be dropped and {n_lossy} change(s) may lose data"
                ),
            };
            ui.checkbox(&mut tab.structure.confirm_ack, ack_text);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let apply = egui::Button::new(
                    egui::RichText::new("Apply changes").color(egui::Color32::WHITE),
                )
                .fill(ui.visuals().error_fg_color);
                if ui.add_enabled(tab.structure.confirm_ack, apply).clicked() {
                    tab.structure.confirm_open = false;
                    action = Some(StructureAction::Apply { statements: sql_of(stmts) });
                }
                if ui.button("Cancel").clicked() {
                    tab.structure.confirm_open = false;
                }
            });
        });
    if !open {
        tab.structure.confirm_open = false;
    }
    action
}
