//! SQL completion popup widget. Rendered in an overlay `Area` anchored below the
//! text cursor; captures Up/Down/Tab/Enter/Esc before the TextEdit sees them.

use crate::sql_complete::{CompletionItem, ItemKind};

#[derive(Default)]
pub struct CompletionPopupState {
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub anchor: egui::Pos2,
    pub open: bool,
    /// Set true when selection changes via keyboard or refresh; consumed by the
    /// next render so we only auto-scroll on real selection changes — otherwise
    /// the user's manual scroll position would snap back every frame.
    pub scroll_to_selected: bool,
}

pub enum PopupAction {
    None,
    Accept(CompletionItem),
    Dismiss,
}

impl CompletionPopupState {
    pub fn refresh(&mut self, items: Vec<CompletionItem>, anchor: egui::Pos2) {
        // Try to keep the selection on the same label across refreshes.
        let prev_label = self
            .items
            .get(self.selected)
            .map(|i| i.label.clone());
        let was_open = self.open;
        let prev_selected = self.selected;
        self.items = items;
        self.anchor = anchor;
        self.selected = prev_label
            .and_then(|l| self.items.iter().position(|i| i.label == l))
            .unwrap_or(0);
        self.open = !self.items.is_empty();
        // Only force a scroll on (re)open or when the kept-label landed at a
        // different index. If the user was scrolling around an unchanged list,
        // leave their scroll position alone.
        if !was_open || self.selected != prev_selected {
            self.scroll_to_selected = true;
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.items.clear();
        self.selected = 0;
        self.scroll_to_selected = false;
    }
}

pub fn draw(ctx: &egui::Context, state: &mut CompletionPopupState) -> PopupAction {
    if !state.open || state.items.is_empty() {
        return PopupAction::None;
    }

    // Capture keys BEFORE the TextEdit renders next frame. Using `consume_key`
    // atomically checks-and-removes the press so the editor below won't also act on it.
    let mut action = PopupAction::None;
    ctx.input_mut(|i| {
        if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
            action = PopupAction::Dismiss;
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
            || i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
        {
            if let Some(item) = state.items.get(state.selected).cloned() {
                action = PopupAction::Accept(item);
            }
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
            state.selected = (state.selected + 1).min(state.items.len().saturating_sub(1));
            state.scroll_to_selected = true;
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
            state.selected = state.selected.saturating_sub(1);
            state.scroll_to_selected = true;
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown) {
            state.selected = (state.selected + 8).min(state.items.len().saturating_sub(1));
            state.scroll_to_selected = true;
        } else if i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp) {
            state.selected = state.selected.saturating_sub(8);
            state.scroll_to_selected = true;
        }
    });

    // Render popup. Foreground so it sits above the TextEdit.
    let area = egui::Area::new(egui::Id::new("sql_complete_popup"))
        .order(egui::Order::Foreground)
        .fixed_pos(state.anchor)
        .constrain(true);

    area.show(ctx, |ui| {
        egui::Frame::popup(ui.style())
            .inner_margin(egui::Margin::same(4.0))
            .show(ui, |ui| {
                ui.set_min_width(340.0);
                ui.set_max_width(520.0);
                egui::ScrollArea::vertical()
                    .id_source("completion_popup_scroll")
                    .max_height(240.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let scroll_now = state.scroll_to_selected;
                        for (idx, it) in state.items.iter().enumerate() {
                            let selected = idx == state.selected;
                            let row_resp = draw_row(ui, it, selected);
                            if row_resp.clicked() {
                                if let Some(item) = state.items.get(idx).cloned() {
                                    action = PopupAction::Accept(item);
                                }
                            }
                            if selected && scroll_now {
                                row_resp.scroll_to_me(None);
                            }
                        }
                        state.scroll_to_selected = false;
                    });
            });
    });

    action
}

fn draw_row(ui: &mut egui::Ui, item: &CompletionItem, selected: bool) -> egui::Response {
    let icon = kind_icon(item.kind);
    let bg = if selected {
        ui.visuals().selection.bg_fill
    } else {
        egui::Color32::TRANSPARENT
    };
    let fg = if selected {
        ui.visuals().selection.stroke.color
    } else {
        ui.visuals().text_color()
    };

    let desired = egui::vec2(ui.available_width(), 20.0);
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());

    ui.painter().rect_filled(rect, 2.0, bg);

    let mut x = rect.left() + 6.0;
    // Icon
    let icon_galley = ui.painter().layout_no_wrap(
        icon.to_string(),
        egui::FontId::monospace(12.0),
        kind_color(item.kind),
    );
    ui.painter().galley(egui::pos2(x, rect.top() + 3.0), icon_galley, fg);
    x += 18.0;

    // Label
    let label_galley = ui.painter().layout_no_wrap(
        item.label.clone(),
        egui::FontId::monospace(13.0),
        fg,
    );
    let label_w = label_galley.rect.width();
    ui.painter().galley(egui::pos2(x, rect.top() + 2.0), label_galley, fg);
    x += label_w + 12.0;

    // Detail (dim, right side)
    let detail_col = if selected {
        fg
    } else {
        ui.visuals().weak_text_color()
    };
    let detail_galley = ui.painter().layout_no_wrap(
        item.detail.clone(),
        egui::FontId::proportional(11.0),
        detail_col,
    );
    let detail_w = detail_galley.rect.width();
    let detail_x = (rect.right() - detail_w - 6.0).max(x + 8.0);
    ui.painter().galley(egui::pos2(detail_x, rect.top() + 4.0), detail_galley, detail_col);

    resp
}

fn kind_icon(k: ItemKind) -> &'static str {
    match k {
        ItemKind::Keyword => "K",
        ItemKind::Table => "T",
        ItemKind::Column => "c",
        ItemKind::Alias => "a",
        ItemKind::Cte => "W",
        ItemKind::Function => "ƒ",
        ItemKind::JoinFk => "⇄",
        ItemKind::Schema => "S",
    }
}

fn kind_color(k: ItemKind) -> egui::Color32 {
    match k {
        ItemKind::Keyword => egui::Color32::from_rgb(220, 160, 80),
        ItemKind::Table => egui::Color32::from_rgb(140, 200, 220),
        ItemKind::Column => egui::Color32::from_rgb(180, 220, 180),
        ItemKind::Alias => egui::Color32::from_rgb(200, 200, 240),
        ItemKind::Cte => egui::Color32::from_rgb(220, 180, 220),
        ItemKind::Function => egui::Color32::from_rgb(220, 220, 140),
        ItemKind::JoinFk => egui::Color32::from_rgb(240, 140, 140),
        ItemKind::Schema => egui::Color32::from_rgb(160, 160, 160),
    }
}
