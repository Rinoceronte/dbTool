//! Pannable/zoomable canvas for the DBML diagram tab: draggable table nodes,
//! bezier ref curves, and TableGroup containers that drag as a unit.

use egui::{
    Align2, Color32, CursorIcon, FontId, Pos2, Rect, Rounding, Sense, Stroke, Vec2, pos2, vec2,
};

use crate::dbml::DiagramModel;
use crate::ui::diagram_tab::{DiagramTab, DragKind, TablePos};
use crate::ui::theme;

const HEADER_H: f32 = 26.0;
const ROW_H: f32 = 20.0;
const MIN_W: f32 = 160.0;
const MAX_W: f32 = 340.0;
const GROUP_PAD: f32 = 24.0;
const GROUP_LABEL_H: f32 = 20.0;
const ZOOM_MIN: f32 = 0.15;
const ZOOM_MAX: f32 = 3.0;
/// Seconds of pan/zoom quiet before the layout sidecar is flushed.
const LAYOUT_FLUSH_AFTER: f64 = 0.7;

pub fn draw(ui: &mut egui::Ui, tab: &mut DiagramTab) {
    let canvas_rect = ui.available_rect_before_wrap();
    let bg = ui.allocate_rect(canvas_rect, Sense::click_and_drag());

    let Some(model) = tab.model.clone() else {
        ui.painter().text(
            canvas_rect.center(),
            Align2::CENTER_CENTER,
            "Fix the DBML source to see the diagram",
            FontId::proportional(14.0),
            ui.visuals().weak_text_color(),
        );
        return;
    };

    ensure_node_sizes(ui, tab, &model);
    place_missing_tables(tab, &model);

    let now = ui.input(|i| i.time);

    // --- Requested view changes (Fit / 1:1) --------------------------------
    if tab.want_fit {
        tab.want_fit = false;
        if let Some(bounds) = world_bounds(tab, &model) {
            let bounds = bounds.expand(GROUP_PAD + 16.0);
            let avail = canvas_rect.size();
            let zoom = (avail.x / bounds.width())
                .min(avail.y / bounds.height())
                .clamp(ZOOM_MIN, 1.5)
                * 0.92;
            tab.layout.zoom = zoom;
            let pan = canvas_rect.center() - canvas_rect.min - bounds.center().to_vec2() * zoom;
            tab.layout.pan = (pan.x, pan.y);
            tab.mark_layout_dirty(now);
        }
    }
    if tab.want_reset_zoom {
        tab.want_reset_zoom = false;
        let center = canvas_rect.center() - canvas_rect.min;
        let pan = vec2(tab.layout.pan.0, tab.layout.pan.1);
        let world_center = (center - pan) / tab.layout.zoom;
        tab.layout.zoom = 1.0;
        let pan = center - world_center;
        tab.layout.pan = (pan.x, pan.y);
        tab.mark_layout_dirty(now);
    }

    let zoom = tab.layout.zoom;
    let pan = vec2(tab.layout.pan.0, tab.layout.pan.1);
    let to_screen = |p: Pos2| canvas_rect.min + pan + p.to_vec2() * zoom;

    // --- Geometry -----------------------------------------------------------
    let node_rects: Vec<Rect> = model
        .tables
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let pos = tab
                .layout
                .tables
                .get(&t.key)
                .map(|p| pos2(p.x, p.y))
                .unwrap_or(pos2(40.0, 40.0));
            let mut size = tab.node_sizes.get(i).copied().unwrap_or(vec2(MIN_W, HEADER_H));
            if collapsed(tab, &t.key) {
                size.y = HEADER_H;
            }
            Rect::from_min_size(to_screen(pos), size * zoom)
        })
        .collect();

    let group_rects: Vec<Rect> = model
        .groups
        .iter()
        .map(|g| {
            let mut r: Option<Rect> = None;
            for &ti in &g.tables {
                r = Some(match r {
                    None => node_rects[ti],
                    Some(acc) => acc.union(node_rects[ti]),
                });
            }
            r.map(|r| {
                let mut r = r.expand(GROUP_PAD * zoom);
                r.min.y -= GROUP_LABEL_H * zoom;
                r
            })
            .unwrap_or(Rect::NOTHING)
        })
        .collect();

    // --- Interaction (registration order = hover priority; nodes on top) ---
    let mut moved: Option<(DragKind, Vec2)> = None;

    if bg.dragged() && (tab.drag.is_none() || tab.drag == Some(DragKind::Pan)) {
        tab.drag = Some(DragKind::Pan);
        let pan = pan + bg.drag_delta();
        tab.layout.pan = (pan.x, pan.y);
        tab.mark_layout_dirty(now);
        ui.ctx().request_repaint();
    }
    if bg.clicked() {
        tab.selected = None;
    }

    for (gi, rect) in group_rects.iter().enumerate() {
        if rect.width() <= 0.0 {
            continue;
        }
        let resp = ui.interact(
            rect.intersect(canvas_rect),
            ui.id().with(("dbml_group", tab.id, gi)),
            Sense::click_and_drag(),
        );
        if resp.drag_started() {
            tab.drag = Some(DragKind::Group(gi));
        }
        if resp.dragged() && tab.drag == Some(DragKind::Group(gi)) {
            moved = Some((DragKind::Group(gi), resp.drag_delta() / zoom));
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
    }

    for (ti, rect) in node_rects.iter().enumerate() {
        let resp = ui.interact(
            rect.intersect(canvas_rect),
            ui.id().with(("dbml_node", tab.id, ti)),
            Sense::click_and_drag(),
        );
        if resp.drag_started() {
            tab.drag = Some(DragKind::Node(ti));
        }
        if resp.dragged() && tab.drag == Some(DragKind::Node(ti)) {
            moved = Some((DragKind::Node(ti), resp.drag_delta() / zoom));
        }
        if resp.clicked() {
            tab.selected = Some(ti);
        }
        if resp.double_clicked() {
            let key = model.tables[ti].key.clone();
            let entry = ensure_pos(tab, &key);
            entry.collapsed = !entry.collapsed;
            tab.sizes_stale = true;
            tab.save_layout();
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Grab);
        }
    }

    if let Some((kind, delta)) = moved {
        let indices: Vec<usize> = match kind {
            DragKind::Node(ti) => vec![ti],
            DragKind::Group(gi) => model.groups[gi].tables.clone(),
            DragKind::Pan => vec![],
        };
        for ti in indices {
            let key = model.tables[ti].key.clone();
            let entry = ensure_pos(tab, &key);
            entry.x += delta.x;
            entry.y += delta.y;
        }
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        ui.ctx().request_repaint();
    }

    let drag_active = ui.input(|i| i.pointer.any_down());
    if !drag_active && tab.drag.is_some() {
        let was = tab.drag.take();
        if was != Some(DragKind::Pan) {
            tab.save_layout();
        }
    }

    // --- Zoom at cursor -----------------------------------------------------
    if ui.rect_contains_pointer(canvas_rect) {
        let (scroll, zoom_delta) = ui.input(|i| (i.raw_scroll_delta.y, i.zoom_delta()));
        let factor = if zoom_delta != 1.0 { zoom_delta } else { (scroll * 0.003).exp() };
        if factor != 1.0
            && let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
        {
            let mouse = pointer - canvas_rect.min;
            let pan = vec2(tab.layout.pan.0, tab.layout.pan.1);
            let world = (mouse - pan) / tab.layout.zoom;
            tab.layout.zoom = (tab.layout.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
            let pan = mouse - world * tab.layout.zoom;
            tab.layout.pan = (pan.x, pan.y);
            tab.mark_layout_dirty(now);
            ui.ctx().request_repaint();
        }
    }

    // Debounced sidecar flush for pan/zoom.
    if tab.layout_dirty {
        if now - tab.layout_dirty_at > LAYOUT_FLUSH_AFTER {
            tab.save_layout();
        } else {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(750));
        }
    }

    // --- Painting (call order = z-order) ------------------------------------
    let painter = ui.painter_at(canvas_rect);
    let visuals = ui.visuals().clone();

    // Recompute rects after drag so painting has no one-frame lag.
    let zoom = tab.layout.zoom;
    let pan = vec2(tab.layout.pan.0, tab.layout.pan.1);
    let to_screen = |p: Pos2| canvas_rect.min + pan + p.to_vec2() * zoom;
    let node_rects: Vec<Rect> = model
        .tables
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let pos = tab
                .layout
                .tables
                .get(&t.key)
                .map(|p| pos2(p.x, p.y))
                .unwrap_or(pos2(40.0, 40.0));
            let mut size = tab.node_sizes.get(i).copied().unwrap_or(vec2(MIN_W, HEADER_H));
            if collapsed(tab, &t.key) {
                size.y = HEADER_H;
            }
            Rect::from_min_size(to_screen(pos), size * zoom)
        })
        .collect();

    // Groups.
    for (gi, g) in model.groups.iter().enumerate() {
        if g.tables.is_empty() {
            continue;
        }
        let mut r = node_rects[g.tables[0]];
        for &ti in &g.tables[1..] {
            r = r.union(node_rects[ti]);
        }
        let mut r = r.expand(GROUP_PAD * zoom);
        r.min.y -= GROUP_LABEL_H * zoom;
        let color = g.color.unwrap_or(theme::GROUP_COLORS[gi % theme::GROUP_COLORS.len()]);
        painter.rect_filled(r, Rounding::same(8.0 * zoom), color.gamma_multiply(0.10));
        painter.rect_stroke(
            r,
            Rounding::same(8.0 * zoom),
            Stroke::new(1.0, color.gamma_multiply(0.5)),
        );
        if GROUP_LABEL_H * zoom > 8.0 {
            painter.text(
                r.min + vec2(10.0 * zoom, 4.0 * zoom),
                Align2::LEFT_TOP,
                &g.name,
                FontId::proportional(12.0 * zoom),
                color.gamma_multiply(0.9),
            );
        }
    }

    // Edges.
    let edge_color = visuals.weak_text_color().gamma_multiply(0.8);
    for r in &model.refs {
        let (ft, fc) = r.from;
        let (tt, tc) = r.to;
        let (fr, tr) = (node_rects[ft], node_rects[tt]);
        let from_right = tr.center().x > fr.center().x;
        let a = edge_anchor(tab, &model, ft, fc, fr, from_right, zoom);
        let b = edge_anchor(tab, &model, tt, tc, tr, !from_right, zoom);
        let dx = ((a.x - b.x).abs() * 0.5).clamp(30.0 * zoom, 120.0 * zoom);
        let c1 = a + vec2(if from_right { dx } else { -dx }, 0.0);
        let c2 = b + vec2(if from_right { -dx } else { dx }, 0.0);
        let highlighted = tab.selected == Some(ft) || tab.selected == Some(tt);
        let stroke = if highlighted {
            Stroke::new(2.0, theme::ACCENT)
        } else {
            Stroke::new(1.3, edge_color)
        };
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            [a, c1, c2, b],
            false,
            Color32::TRANSPARENT,
            stroke,
        ));
        painter.circle_filled(a, 2.8 * zoom.min(1.4), stroke.color);
        painter.circle_filled(b, 2.8 * zoom.min(1.4), stroke.color);
    }

    // Nodes.
    let show_text = ROW_H * zoom >= 6.0;
    for (ti, t) in model.tables.iter().enumerate() {
        let rect = node_rects[ti];
        if !canvas_rect.intersects(rect) {
            continue;
        }
        let is_collapsed = collapsed(tab, &t.key);
        let selected = tab.selected == Some(ti);
        let rounding = Rounding::same(6.0 * zoom.min(1.2));
        let group_color = t
            .group
            .map(|gi| {
                model.groups[gi]
                    .color
                    .unwrap_or(theme::GROUP_COLORS[gi % theme::GROUP_COLORS.len()])
            })
            .unwrap_or(theme::ACCENT);
        let header_color = t.header_color.unwrap_or(group_color);

        painter.rect_filled(rect, rounding, visuals.window_fill());
        let header_rect =
            Rect::from_min_size(rect.min, vec2(rect.width(), HEADER_H * zoom));
        painter.rect_filled(
            header_rect,
            Rounding { nw: rounding.nw, ne: rounding.ne, sw: 0.0, se: 0.0 },
            header_color.gamma_multiply(if visuals.dark_mode { 0.45 } else { 0.30 }),
        );
        let border = if selected {
            Stroke::new(1.6, theme::ACCENT)
        } else {
            Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color)
        };
        painter.rect_stroke(rect, rounding, border);

        if !show_text {
            continue;
        }
        let np = ui.painter_at(rect.intersect(canvas_rect));
        let title = if is_collapsed {
            format!("{}  ({})", t.name, t.columns.len())
        } else {
            t.name.clone()
        };
        np.text(
            header_rect.left_center() + vec2(9.0 * zoom, 0.0),
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(13.0 * zoom),
            visuals.strong_text_color(),
        );

        if is_collapsed {
            continue;
        }
        for (ci, col) in t.columns.iter().enumerate() {
            let y = rect.min.y + (HEADER_H + ci as f32 * ROW_H + ROW_H * 0.5) * zoom;
            let name_color = if col.is_pk {
                theme::ACCENT
            } else {
                visuals.text_color()
            };
            let marker = if col.is_pk {
                "●"
            } else if col.is_fk {
                "→"
            } else {
                ""
            };
            if !marker.is_empty() {
                np.text(
                    pos2(rect.min.x + 8.0 * zoom, y),
                    Align2::LEFT_CENTER,
                    marker,
                    FontId::monospace(9.0 * zoom),
                    name_color.gamma_multiply(0.9),
                );
            }
            np.text(
                pos2(rect.min.x + 22.0 * zoom, y),
                Align2::LEFT_CENTER,
                &col.name,
                FontId::proportional(12.5 * zoom),
                name_color,
            );
            np.text(
                pos2(rect.max.x - 9.0 * zoom, y),
                Align2::RIGHT_CENTER,
                &col.ty,
                FontId::monospace(10.5 * zoom),
                visuals.weak_text_color(),
            );
        }
    }
}

fn collapsed(tab: &DiagramTab, key: &str) -> bool {
    tab.layout.tables.get(key).is_some_and(|p| p.collapsed)
}

fn ensure_pos<'a>(tab: &'a mut DiagramTab, key: &str) -> &'a mut TablePos {
    tab.layout
        .tables
        .entry(key.to_owned())
        .or_insert(TablePos { x: 40.0, y: 40.0, collapsed: false })
}

fn edge_anchor(
    tab: &DiagramTab,
    model: &DiagramModel,
    ti: usize,
    ci: Option<usize>,
    rect: Rect,
    right: bool,
    zoom: f32,
) -> Pos2 {
    let x = if right { rect.max.x } else { rect.min.x };
    let y = match ci {
        Some(ci) if !collapsed(tab, &model.tables[ti].key) => {
            rect.min.y + (HEADER_H + ci as f32 * ROW_H + ROW_H * 0.5) * zoom
        }
        _ => rect.min.y + HEADER_H * 0.5 * zoom,
    };
    pos2(x, y.clamp(rect.min.y, rect.max.y))
}

/// Measure world-space node sizes at base font sizes (zoom-independent).
fn ensure_node_sizes(ui: &egui::Ui, tab: &mut DiagramTab, model: &DiagramModel) {
    if !tab.sizes_stale && tab.node_sizes.len() == model.tables.len() {
        return;
    }
    tab.sizes_stale = false;
    let measure = |text: &str, font: FontId| -> f32 {
        ui.fonts(|f| f.layout_no_wrap(text.to_owned(), font, Color32::WHITE).size().x)
    };
    tab.node_sizes = model
        .tables
        .iter()
        .map(|t| {
            let mut w = measure(&t.name, FontId::proportional(13.0)) + 40.0;
            for c in &t.columns {
                let row = measure(&c.name, FontId::proportional(12.5))
                    + measure(&c.ty, FontId::monospace(10.5))
                    + 22.0  // marker gutter
                    + 26.0; // inner padding + name/type gap
                w = w.max(row);
            }
            vec2(
                w.clamp(MIN_W, MAX_W),
                HEADER_H + t.columns.len() as f32 * ROW_H,
            )
        })
        .collect();
}

fn world_bounds(tab: &DiagramTab, model: &DiagramModel) -> Option<Rect> {
    let mut bounds: Option<Rect> = None;
    for (i, t) in model.tables.iter().enumerate() {
        let Some(p) = tab.layout.tables.get(&t.key) else { continue };
        let size = tab.node_sizes.get(i).copied().unwrap_or(vec2(MIN_W, HEADER_H));
        let r = Rect::from_min_size(pos2(p.x, p.y), size);
        bounds = Some(match bounds {
            None => r,
            Some(b) => b.union(r),
        });
    }
    bounds
}

/// Deterministic grid placement, clustered by group, for tables that have no
/// saved position. New tables (added while editing) flow in below the
/// existing diagram instead of on top of it.
fn place_missing_tables(tab: &mut DiagramTab, model: &DiagramModel) {
    let missing: Vec<usize> = model
        .tables
        .iter()
        .enumerate()
        .filter(|(_, t)| !tab.layout.tables.contains_key(&t.key))
        .map(|(i, _)| i)
        .collect();
    if missing.is_empty() {
        return;
    }

    let origin = world_bounds(tab, model)
        .map(|b| pos2(40.0, b.max.y + 120.0))
        .unwrap_or(pos2(40.0, 40.0));

    // Clusters: groups in declaration order, then ungrouped tables.
    let mut clusters: Vec<Vec<usize>> = model
        .groups
        .iter()
        .map(|g| {
            g.tables
                .iter()
                .copied()
                .filter(|ti| missing.contains(ti))
                .collect::<Vec<_>>()
        })
        .collect();
    clusters.push(
        missing
            .iter()
            .copied()
            .filter(|ti| model.tables[*ti].group.is_none())
            .collect(),
    );

    let size_of = |ti: usize| {
        tab.node_sizes
            .get(ti)
            .copied()
            .unwrap_or(vec2(MIN_W, HEADER_H + 4.0 * ROW_H))
    };

    let mut cursor_x = origin.x;
    let mut cursor_y = origin.y;
    let mut band_h: f32 = 0.0;
    let mut band_count = 0;
    for cluster in clusters.iter().filter(|c| !c.is_empty()) {
        let cell_w = cluster.iter().map(|&ti| size_of(ti).x).fold(0.0, f32::max) + 60.0;
        let cell_h = cluster.iter().map(|&ti| size_of(ti).y).fold(0.0, f32::max) + 40.0;
        let cols = (cluster.len() as f32).sqrt().ceil() as usize;
        let rows = cluster.len().div_ceil(cols);
        for (i, &ti) in cluster.iter().enumerate() {
            let (col, row) = (i % cols, i / cols);
            tab.layout.tables.insert(
                model.tables[ti].key.clone(),
                TablePos {
                    x: cursor_x + col as f32 * cell_w,
                    y: cursor_y + row as f32 * cell_h,
                    collapsed: false,
                },
            );
        }
        band_h = band_h.max(rows as f32 * cell_h + GROUP_PAD * 2.0);
        cursor_x += cols as f32 * cell_w + 120.0;
        band_count += 1;
        if band_count % 3 == 0 {
            cursor_x = origin.x;
            cursor_y += band_h + 60.0;
            band_h = 0.0;
        }
    }
    tab.save_layout();
}
