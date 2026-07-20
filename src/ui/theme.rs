//! Central visual theme for dbTool. Installed once at startup (see `App::new`)
//! so widgets stay consistent instead of hand-rolling colors per call site.

use egui::{Color32, Rounding, Stroke};

use crate::db::DbKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

impl ThemeMode {
    pub fn is_dark(self) -> bool {
        matches!(self, ThemeMode::Dark)
    }

    pub fn toggled(self) -> Self {
        match self {
            ThemeMode::Dark => ThemeMode::Light,
            ThemeMode::Light => ThemeMode::Dark,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            ThemeMode::Dark => "☀",
            ThemeMode::Light => "🌙",
        }
    }
}

/// Primary accent used for selection and focus. Legible on both themes.
pub const ACCENT: Color32 = Color32::from_rgb(88, 152, 237);

/// Install the font fallback chain. Hack (the bundled monospace font) covers
/// geometric/arrow glyphs (→ ● ▦ ◈ ⊘ ⇄ ◌) that the default proportional
/// chain (Ubuntu-Light → NotoEmoji → emoji-icon-font) lacks — without this,
/// those icons render as tofu boxes.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some(prop) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        prop.push("Hack".to_owned());
    }
    ctx.set_fonts(fonts);
}

pub fn install(ctx: &egui::Context, mode: ThemeMode) {
    let dark = mode.is_dark();
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    let widget_rounding = Rounding::same(5.0);

    if dark {
        let text = Color32::from_rgb(206, 212, 222);
        let weak = Color32::from_rgb(133, 141, 154);

        visuals.panel_fill = Color32::from_rgb(27, 30, 36);
        visuals.window_fill = Color32::from_rgb(32, 36, 43);
        visuals.extreme_bg_color = Color32::from_rgb(20, 22, 27);
        visuals.faint_bg_color = Color32::from_rgb(37, 41, 49);
        visuals.code_bg_color = Color32::from_rgb(23, 26, 31);
        visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(52, 57, 66));

        visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(32, 36, 43);
        visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(32, 36, 43);
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(48, 53, 62));
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, weak);

        visuals.widgets.inactive.bg_fill = Color32::from_rgb(45, 50, 59);
        visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(40, 44, 52);
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(58, 64, 75));
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, text);

        visuals.widgets.hovered.bg_fill = Color32::from_rgb(55, 62, 73);
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(52, 58, 68);
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(80, 88, 102));
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.5, Color32::from_rgb(230, 234, 240));

        visuals.widgets.active.bg_fill = Color32::from_rgb(64, 72, 86);
        visuals.widgets.active.weak_bg_fill = Color32::from_rgb(60, 68, 80);
        visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
        visuals.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);

        visuals.widgets.open.bg_fill = Color32::from_rgb(45, 50, 59);
        visuals.widgets.open.weak_bg_fill = Color32::from_rgb(40, 44, 52);
        visuals.widgets.open.bg_stroke = Stroke::new(1.0, Color32::from_rgb(58, 64, 75));
        visuals.widgets.open.fg_stroke = Stroke::new(1.0, text);

        visuals.selection.bg_fill = Color32::from_rgb(46, 74, 116);
        visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(210, 224, 245));
        visuals.hyperlink_color = Color32::from_rgb(120, 176, 245);
        visuals.warn_fg_color = Color32::from_rgb(226, 184, 92);
        visuals.error_fg_color = Color32::from_rgb(232, 116, 108);
    } else {
        visuals.panel_fill = Color32::from_rgb(244, 246, 249);
        visuals.window_fill = Color32::from_rgb(252, 253, 255);
        visuals.extreme_bg_color = Color32::from_rgb(255, 255, 255);
        visuals.faint_bg_color = Color32::from_rgb(236, 239, 244);
        visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(210, 216, 224));

        visuals.selection.bg_fill = Color32::from_rgb(200, 222, 252);
        visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(40, 92, 168));
        visuals.hyperlink_color = Color32::from_rgb(30, 108, 210);
        visuals.warn_fg_color = Color32::from_rgb(176, 120, 20);
        visuals.error_fg_color = Color32::from_rgb(196, 64, 56);
    }

    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.rounding = widget_rounding;
    }

    visuals.window_rounding = Rounding::same(9.0);
    visuals.menu_rounding = Rounding::same(6.0);
    visuals.popup_shadow.blur = 12.0;
    visuals.window_shadow.blur = 18.0;

    let mut style = (*ctx.style()).clone();
    style.visuals = visuals;

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(9.0, 4.0);
    style.spacing.window_margin = egui::Margin::same(12.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);
    style.spacing.interact_size.y = 24.0;
    style.spacing.indent = 16.0;
    style.spacing.scroll.bar_width = 10.0;

    use egui::{FontFamily, FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Heading, FontId::new(19.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();

    ctx.set_style(style);
}

/// Brand-ish accent color per driver, for badges and headers.
pub fn driver_color(kind: DbKind) -> Color32 {
    match kind {
        DbKind::Postgres => Color32::from_rgb(70, 125, 195),
        DbKind::MySql => Color32::from_rgb(22, 150, 158),
        DbKind::MsSql => Color32::from_rgb(198, 92, 72),
    }
}

pub fn driver_abbr(kind: DbKind) -> &'static str {
    match kind {
        DbKind::Postgres => "PG",
        DbKind::MySql => "MY",
        DbKind::MsSql => "MS",
    }
}

/// Draw a small colored driver badge (e.g. `PG`) and return its response so
/// callers can attach a hover tooltip.
pub fn driver_badge(ui: &mut egui::Ui, kind: DbKind) -> egui::Response {
    let color = driver_color(kind);
    let font = egui::FontId::new(10.5, egui::FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(driver_abbr(kind).to_string(), font, Color32::WHITE);
    let pad = egui::vec2(6.0, 2.5);
    let size = galley.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, Rounding::same(4.0), color);
    let text_pos = rect.center() - galley.size() * 0.5;
    painter.galley(text_pos, galley, Color32::WHITE);
    resp.on_hover_text(kind.display())
}

/// A small filled/hollow status dot for connected state.
pub fn status_dot(ui: &mut egui::Ui, on: bool) -> egui::Response {
    let color = if on {
        success_color(ui)
    } else {
        ui.visuals().weak_text_color()
    };
    let size = egui::vec2(11.0, 11.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let c = rect.center();
    let r = 4.0;
    if on {
        ui.painter().circle_filled(c, r, color);
    } else {
        ui.painter().circle_stroke(c, r, Stroke::new(1.5, color));
    }
    resp
}

pub fn success_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(112, 192, 120)
    } else {
        Color32::from_rgb(38, 140, 58)
    }
}

/// Muted color for NULL cells and other absent values.
pub fn null_color(ui: &egui::Ui) -> Color32 {
    ui.visuals().weak_text_color()
}

/// Background tint for rows staged with pending edits.
pub fn pending_bg(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgba_unmultiplied(210, 176, 80, 42)
    } else {
        Color32::from_rgba_unmultiplied(230, 200, 90, 70)
    }
}

/// Background tint for objects staged for creation (Structure view).
pub fn add_bg(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgba_unmultiplied(96, 190, 110, 40)
    } else {
        Color32::from_rgba_unmultiplied(110, 205, 130, 66)
    }
}

pub fn add_fg(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(130, 205, 140)
    } else {
        Color32::from_rgb(34, 128, 54)
    }
}

/// Background tint for rows staged for deletion.
pub fn delete_bg(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgba_unmultiplied(214, 84, 80, 52)
    } else {
        Color32::from_rgba_unmultiplied(232, 120, 116, 70)
    }
}

pub fn delete_fg(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(228, 150, 148)
    } else {
        Color32::from_rgb(176, 52, 48)
    }
}
