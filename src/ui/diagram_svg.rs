//! Standalone SVG export of a DBML diagram. Mirrors the world-space geometry
//! of `diagram_canvas` (same constants, anchors, and bezier control points) at
//! 1:1 zoom so the exported file matches what the user laid out on screen.

use egui::{Color32, Pos2, Rect, Vec2, pos2, vec2};
use std::fmt::Write as _;

use crate::dbml::DiagramModel;
use crate::ui::diagram_tab::DiagramTab;
use crate::ui::theme;

// Keep in sync with diagram_canvas.
const HEADER_H: f32 = 26.0;
const ROW_H: f32 = 20.0;
const MIN_W: f32 = 160.0;
const GROUP_PAD: f32 = 24.0;
const GROUP_LABEL_H: f32 = 20.0;
const MARGIN: f32 = 40.0;

struct Palette {
    bg: &'static str,
    node_fill: &'static str,
    node_border: &'static str,
    text: &'static str,
    weak: &'static str,
    edge: &'static str,
    header_alpha: f32,
}

const LIGHT: Palette = Palette {
    bg: "#ffffff",
    node_fill: "#ffffff",
    node_border: "#c4c4c4",
    text: "#202020",
    weak: "#808080",
    edge: "#9a9a9a",
    header_alpha: 0.30,
};

const DARK: Palette = Palette {
    bg: "#1b1b1f",
    node_fill: "#26262b",
    node_border: "#45454c",
    text: "#e6e6e6",
    weak: "#9a9aa2",
    edge: "#6a6a72",
    header_alpha: 0.45,
};

fn hex(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Render the diagram to an SVG document string. `dark` picks the palette so
/// the export matches the theme the user is looking at.
pub fn export_svg(tab: &DiagramTab, model: &DiagramModel, dark: bool) -> String {
    let p = if dark { &DARK } else { &LIGHT };

    let collapsed = |key: &str| tab.layout.tables.get(key).is_some_and(|t| t.collapsed);
    let node_rect = |i: usize| -> Rect {
        let t = &model.tables[i];
        let pos = tab
            .layout
            .tables
            .get(&t.key)
            .map(|tp| pos2(tp.x, tp.y))
            .unwrap_or(pos2(40.0, 40.0));
        let mut size: Vec2 =
            tab.node_sizes.get(i).copied().unwrap_or(vec2(MIN_W, HEADER_H));
        if collapsed(&t.key) {
            size.y = HEADER_H;
        }
        Rect::from_min_size(pos, size)
    };

    let rects: Vec<Rect> = (0..model.tables.len()).map(node_rect).collect();

    // Document bounds: nodes plus group padding.
    let mut bounds: Option<Rect> = None;
    for r in &rects {
        bounds = Some(bounds.map_or(*r, |b| b.union(*r)));
    }
    let bounds = bounds
        .unwrap_or(Rect::from_min_size(Pos2::ZERO, vec2(200.0, 100.0)))
        .expand(GROUP_PAD + MARGIN);
    let offset = bounds.min.to_vec2();
    let x = |v: f32| v - offset.x;
    let y = |v: f32| v - offset.y;

    let mut out = String::new();
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}" font-family="-apple-system, 'Segoe UI', sans-serif">"#,
        w = bounds.width(),
        h = bounds.height(),
    );
    out.push('\n');
    let _ = writeln!(
        out,
        r#"<rect width="100%" height="100%" fill="{}"/>"#,
        p.bg
    );

    // Groups (under everything, like the canvas z-order).
    for (gi, g) in model.groups.iter().enumerate() {
        if g.tables.is_empty() {
            continue;
        }
        let mut r = rects[g.tables[0]];
        for &ti in &g.tables[1..] {
            r = r.union(rects[ti]);
        }
        let mut r = r.expand(GROUP_PAD);
        r.min.y -= GROUP_LABEL_H;
        let color = g.color.unwrap_or(theme::GROUP_COLORS[gi % theme::GROUP_COLORS.len()]);
        let c = hex(color);
        let _ = writeln!(
            out,
            r#"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" rx="8" fill="{c}" fill-opacity="0.10" stroke="{c}" stroke-opacity="0.5"/>"#,
            x(r.min.x),
            y(r.min.y),
            r.width(),
            r.height(),
        );
        let _ = writeln!(
            out,
            r#"<text x="{:.1}" y="{:.1}" font-size="12" fill="{c}" fill-opacity="0.9">{}</text>"#,
            x(r.min.x) + 10.0,
            y(r.min.y) + 14.0,
            esc(&g.name),
        );
    }

    // Edges.
    let edge_anchor = |ti: usize, ci: Option<usize>, rect: Rect, right: bool| -> Pos2 {
        let ax = if right { rect.max.x } else { rect.min.x };
        let ay = match ci {
            Some(ci) if !collapsed(&model.tables[ti].key) => {
                rect.min.y + HEADER_H + ci as f32 * ROW_H + ROW_H * 0.5
            }
            _ => rect.min.y + HEADER_H * 0.5,
        };
        pos2(ax, ay.clamp(rect.min.y, rect.max.y))
    };
    for r in &model.refs {
        let (ft, fc) = r.from;
        let (tt, tc) = r.to;
        let (fr, tr) = (rects[ft], rects[tt]);
        let from_right = tr.center().x > fr.center().x;
        let a = edge_anchor(ft, fc, fr, from_right);
        let b = edge_anchor(tt, tc, tr, !from_right);
        let dx = ((a.x - b.x).abs() * 0.5).clamp(30.0, 120.0);
        let c1 = a + vec2(if from_right { dx } else { -dx }, 0.0);
        let c2 = b + vec2(if from_right { -dx } else { dx }, 0.0);
        let _ = writeln!(
            out,
            r#"<path d="M {:.1} {:.1} C {:.1} {:.1}, {:.1} {:.1}, {:.1} {:.1}" fill="none" stroke="{}" stroke-width="1.3"/>"#,
            x(a.x), y(a.y), x(c1.x), y(c1.y), x(c2.x), y(c2.y), x(b.x), y(b.y), p.edge,
        );
        for pt in [a, b] {
            let _ = writeln!(
                out,
                r#"<circle cx="{:.1}" cy="{:.1}" r="2.8" fill="{}"/>"#,
                x(pt.x),
                y(pt.y),
                p.edge,
            );
        }
    }

    // Nodes.
    for (ti, t) in model.tables.iter().enumerate() {
        let rect = rects[ti];
        let is_collapsed = collapsed(&t.key);
        let group_color = t
            .group
            .map(|gi| {
                model.groups[gi]
                    .color
                    .unwrap_or(theme::GROUP_COLORS[gi % theme::GROUP_COLORS.len()])
            })
            .unwrap_or(theme::ACCENT);
        let header_color = t.header_color.unwrap_or(group_color);

        let _ = writeln!(
            out,
            r#"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" rx="6" fill="{}" stroke="{}"/>"#,
            x(rect.min.x),
            y(rect.min.y),
            rect.width(),
            rect.height(),
            p.node_fill,
            p.node_border,
        );
        // Header band (clipped to the rounded top by drawing a rounded rect
        // and covering its lower rounding with a square patch).
        let hc = hex(header_color);
        let _ = writeln!(
            out,
            r#"<path d="M {x0:.1} {ky:.1} q 0 -6 6 -6 h {wq:.1} q 6 0 6 6 v {rest:.1} h -{w:.1} z" fill="{hc}" fill-opacity="{a}"/>"#,
            x0 = x(rect.min.x),
            ky = y(rect.min.y) + 6.0,
            wq = rect.width() - 12.0,
            rest = HEADER_H - 6.0,
            w = rect.width(),
            a = p.header_alpha,
        );
        let title = if is_collapsed {
            format!("{}  ({})", t.name, t.columns.len())
        } else {
            t.name.clone()
        };
        let _ = writeln!(
            out,
            r#"<text x="{:.1}" y="{:.1}" font-size="13" font-weight="600" fill="{}">{}</text>"#,
            x(rect.min.x) + 9.0,
            y(rect.min.y) + HEADER_H * 0.5 + 4.5,
            p.text,
            esc(&title),
        );
        if is_collapsed {
            continue;
        }
        for (ci, col) in t.columns.iter().enumerate() {
            let cy = y(rect.min.y) + HEADER_H + ci as f32 * ROW_H + ROW_H * 0.5 + 4.0;
            let name_color = if col.is_pk { hex(theme::ACCENT) } else { p.text.to_owned() };
            let marker = if col.is_pk {
                "●"
            } else if col.is_fk {
                "→"
            } else {
                ""
            };
            if !marker.is_empty() {
                let _ = writeln!(
                    out,
                    r#"<text x="{:.1}" y="{cy:.1}" font-size="9" font-family="monospace" fill="{name_color}">{marker}</text>"#,
                    x(rect.min.x) + 8.0,
                );
            }
            let _ = writeln!(
                out,
                r#"<text x="{:.1}" y="{cy:.1}" font-size="12.5" fill="{name_color}">{}</text>"#,
                x(rect.min.x) + 22.0,
                esc(&col.name),
            );
            let _ = writeln!(
                out,
                r#"<text x="{:.1}" y="{cy:.1}" font-size="10.5" font-family="monospace" fill="{}" text-anchor="end">{}</text>"#,
                x(rect.max.x) - 9.0,
                p.weak,
                esc(&col.ty),
            );
        }
    }

    out.push_str("</svg>\n");
    out
}
