//! Squarified treemap — the "every file as a rectangle" view.
//!
//! Same contract as the sunburst: layout is cached and only recomputed when the
//! view root, depth or index generation changes; drawing is a single mesh plus
//! borders, and hit-testing walks the cached rectangles back-to-front.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2};

use std::collections::HashSet;

use crate::index::Index;
use crate::ui::theme;

pub struct Tile {
    pub idx: u32,
    pub rect: Rect,
    pub color: Color32,
}

#[derive(Default)]
pub struct Layout {
    pub tiles: Vec<Tile>,
    pub total: u64,
}

/// Classic squarified layout (Bruls/Huizing/van Wijk): fill the shorter side with
/// a row whose worst aspect ratio stops improving, then recurse on the rest.
fn squarify(ix: &Index, nodes: &[u32], mut area: Rect, out: &mut Vec<(u32, Rect)>) {
    let mut items: Vec<(u32, f64)> = nodes
        .iter()
        .map(|&n| (n, ix.size[n as usize] as f64))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    if items.is_empty() {
        return;
    }
    let total: f64 = items.iter().map(|(_, s)| *s).sum();
    if total <= 0.0 {
        return;
    }
    let scale = (area.width() as f64 * area.height() as f64) / total;
    for it in items.iter_mut() {
        it.1 *= scale;
    }

    let mut i = 0usize;
    while i < items.len() {
        let short = area.width().min(area.height()) as f64;
        if short < 1.0 {
            break;
        }
        // Grow the row while the worst aspect ratio keeps improving.
        let mut sum = 0.0f64;
        let mut min = f64::MAX;
        let mut max = 0.0f64;
        let mut end = i;
        let mut best = f64::MAX;
        while end < items.len() {
            let a = items[end].1;
            let (ns, nmin, nmax) = (sum + a, min.min(a), max.max(a));
            let worst = worst_ratio(ns, nmin, nmax, short);
            if end > i && worst > best {
                break;
            }
            sum = ns;
            min = nmin;
            max = nmax;
            best = worst;
            end += 1;
        }
        if end == i {
            end = i + 1;
            sum = items[i].1;
        }

        let thick = (sum / short) as f32;
        let horizontal = area.width() <= area.height();
        let row = if horizontal {
            Rect::from_min_size(area.min, Vec2::new(area.width(), thick))
        } else {
            Rect::from_min_size(area.min, Vec2::new(thick, area.height()))
        };

        let mut p = if horizontal { row.left() } else { row.top() };
        for it in &items[i..end] {
            let len = (it.1 / thick.max(0.0001) as f64) as f32;
            let r = if horizontal {
                Rect::from_min_size(Pos2::new(p, row.top()), Vec2::new(len, row.height()))
            } else {
                Rect::from_min_size(Pos2::new(row.left(), p), Vec2::new(row.width(), len))
            };
            out.push((it.0, r));
            p += len;
        }

        area = if horizontal {
            Rect::from_min_max(Pos2::new(area.left(), row.bottom()), area.max)
        } else {
            Rect::from_min_max(Pos2::new(row.right(), area.top()), area.max)
        };
        i = end;
    }
}

fn worst_ratio(sum: f64, min: f64, max: f64, short: f64) -> f64 {
    if sum <= 0.0 || min <= 0.0 {
        return f64::MAX;
    }
    let s2 = sum * sum;
    let w2 = short * short;
    ((w2 * max) / s2).max(s2 / (w2 * min))
}

pub fn build(ix: &Index, root: u32, area: Rect, max_depth: usize) -> Layout {
    let mut tiles = Vec::with_capacity(2048);
    let total = ix.size[root as usize];
    let mut frontier = vec![(root, area, 0usize)];

    while let Some((node, rect, depth)) = frontier.pop() {
        if depth >= max_depth || rect.width() < 4.0 || rect.height() < 4.0 {
            continue;
        }
        let children = ix.top_children_by_size(node, 4096);
        if children.is_empty() {
            continue;
        }
        // Leave a header strip for the folder name on the outer levels.
        let pad = if depth == 0 { 0.0 } else { 2.0 };
        let head = if depth > 0 && rect.height() > 26.0 && rect.width() > 60.0 {
            13.0
        } else {
            0.0
        };
        let inner = Rect::from_min_max(
            rect.min + Vec2::new(pad, pad + head),
            rect.max - Vec2::new(pad, pad),
        );
        if inner.width() < 3.0 || inner.height() < 3.0 {
            continue;
        }

        let mut placed = Vec::with_capacity(children.len());
        squarify(ix, &children, inner, &mut placed);
        for (idx, r) in placed {
            if r.width() < 1.0 || r.height() < 1.0 {
                continue;
            }
            let hue = if ix.is_dir(idx) {
                theme::ext_hue(ix.name_bytes(idx))
            } else {
                theme::ext_hue(ext_of(ix.name_bytes(idx)))
            };
            let color = theme::hsl(
                hue,
                if ix.is_dir(idx) { 0.34 } else { 0.5 },
                (0.36 + depth as f32 * 0.045).min(0.62),
            );
            tiles.push(Tile {
                idx,
                rect: r,
                color,
            });
            if ix.is_dir(idx) {
                frontier.push((idx, r, depth + 1));
            }
        }
    }

    Layout { tiles, total }
}

fn ext_of(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(p) if p + 1 < name.len() => &name[p + 1..],
        _ => b"",
    }
}

pub struct MapState {
    pub layout: Layout,
    pub depth: usize,
    built_for: (u32, u64, u32, u32, usize),
    /// Morph between layouts on zoom instead of jumping.
    pub animate: bool,
    /// Where the whole drawing area starts out at `anim == 0`. Zooming in means
    /// the new layout begins squeezed into the tile that was clicked; zooming
    /// out means it begins blown up so the folder you left still fills the view.
    from: Rect,
    anim: f32,
    /// Nodes that were on screen before the zoom, so only genuinely new tiles
    /// fade in rather than the whole map flashing.
    carried: HashSet<u32>,
    hover: Option<u32>,
    hover_t: f32,
}

/// Maps `area` onto `to`, and every rectangle inside it along with it.
fn remap(r: Rect, area: Rect, to: Rect) -> Rect {
    let kx = to.width() / area.width().max(1.0);
    let ky = to.height() / area.height().max(1.0);
    Rect::from_min_size(
        Pos2::new(
            to.min.x + (r.min.x - area.min.x) * kx,
            to.min.y + (r.min.y - area.min.y) * ky,
        ),
        Vec2::new(r.width() * kx, r.height() * ky),
    )
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    Rect::from_min_max(
        Pos2::new(
            a.min.x + (b.min.x - a.min.x) * t,
            a.min.y + (b.min.y - a.min.y) * t,
        ),
        Pos2::new(
            a.max.x + (b.max.x - a.max.x) * t,
            a.max.y + (b.max.y - a.max.y) * t,
        ),
    )
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

const ANIM_SECS: f32 = 0.26;

impl Default for MapState {
    fn default() -> Self {
        Self {
            layout: Layout::default(),
            depth: 5,
            built_for: (u32::MAX, 0, 0, 0, 0),
            animate: true,
            from: Rect::ZERO,
            anim: 1.0,
            carried: HashSet::new(),
            hover: None,
            hover_t: 1.0,
        }
    }
}

impl MapState {
    pub fn invalidate(&mut self) {
        self.built_for = (u32::MAX, 0, 0, 0, 0);
        self.anim = 1.0;
        self.carried.clear();
    }
}

pub struct Interaction {
    pub hovered: Option<u32>,
    pub clicked: Option<u32>,
    pub double: Option<u32>,
    pub context: Option<u32>,
}

pub fn show(ui: &mut Ui, ix: &Index, st: &mut MapState, view_root: u32) -> Interaction {
    let mut out = Interaction {
        hovered: None,
        clicked: None,
        double: None,
        context: None,
    };
    let rect = ui.available_rect_before_wrap().shrink(2.0);
    let response = ui.allocate_rect(rect, Sense::click());
    let painter = ui.painter_at(rect);
    if rect.width() < 40.0 || rect.height() < 40.0 {
        return out;
    }
    if st.depth == 0 {
        st.depth = 5;
    }

    let key = (
        view_root,
        ix.generation,
        rect.width() as u32,
        rect.height() as u32,
        st.depth,
    );
    if st.built_for != key {
        // Only a changed view root is a zoom; a re-layout after a resize or an
        // index update should land where it lands, without sliding.
        let old_root = st.built_for.0;
        let zoom = st.animate && old_root != view_root && old_root != u32::MAX;
        // Rect the clicked folder occupied before it became the root.
        let entered = zoom
            .then(|| st.layout.tiles.iter().find(|t| t.idx == view_root))
            .flatten()
            .map(|t| t.rect);
        let carried: HashSet<u32> = if zoom {
            st.layout.tiles.iter().map(|t| t.idx).collect()
        } else {
            HashSet::new()
        };

        st.layout = build(ix, view_root, rect, st.depth);
        st.built_for = key;

        let from = entered.or_else(|| {
            // Going up: the folder we came from is a tile in the new layout, and
            // it is the one that has to grow out to cover the whole area.
            let r = st.layout.tiles.iter().find(|t| t.idx == old_root)?.rect;
            // Guard the divisor: a sliver of a tile would send the start rect
            // off to infinity and the first frames would be pure noise.
            let kx = (rect.width() / r.width().max(1.0)).min(24.0);
            let ky = (rect.height() / r.height().max(1.0)).min(24.0);
            Some(Rect::from_min_size(
                Pos2::new(
                    rect.min.x - (r.min.x - rect.min.x) * kx,
                    rect.min.y - (r.min.y - rect.min.y) * ky,
                ),
                Vec2::new(rect.width() * kx, rect.height() * ky),
            ))
        });
        match from.filter(|_| zoom) {
            Some(f) => {
                st.from = f;
                st.anim = 0.0;
                st.carried = carried;
            }
            None => {
                st.anim = 1.0;
                st.carried.clear();
            }
        }
    }

    if st.anim < 1.0 {
        st.anim = (st.anim + ui.input(|i| i.stable_dt) / ANIM_SECS).min(1.0);
        ui.ctx().request_repaint();
    }
    let t = ease_out(st.anim);
    let zooming = t < 1.0;
    // Everything below draws through this: identity once the zoom has settled,
    // so the steady state costs nothing.
    let stage = if zooming {
        lerp_rect(st.from, rect, t)
    } else {
        rect
    };
    let place = |r: Rect| if zooming { remap(r, rect, stage) } else { r };

    let mut mesh = egui::Mesh::default();
    for tile in &st.layout.tiles {
        let r = place(tile.rect);
        // A tile that was already on screen travels with the zoom; one that the
        // new depth just revealed fades up instead of snapping in.
        let color = if zooming && !st.carried.contains(&tile.idx) {
            tile.color.gamma_multiply(t)
        } else {
            tile.color
        };
        let i = mesh.vertices.len() as u32;
        mesh.colored_vertex(r.left_top(), color);
        mesh.colored_vertex(r.right_top(), color);
        mesh.colored_vertex(r.left_bottom(), color);
        mesh.colored_vertex(r.right_bottom(), color);
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 3, i + 2);
    }
    painter.add(Shape::mesh(mesh));

    // Every tile gets an outline, not just the folders — without one the blocks
    // of a densely packed directory run into each other and the map turns to
    // mush. Folders get a stronger line so the nesting still reads.
    for tile in &st.layout.tiles {
        let r = place(tile.rect);
        if r.width() < 2.5 || r.height() < 2.5 {
            continue;
        }
        let dir = ix.is_dir(tile.idx);
        painter.rect_stroke(
            r,
            egui::CornerRadius::ZERO,
            Stroke::new(
                if dir { 1.5 } else { 1.0 },
                if dir {
                    Color32::from_black_alpha(170)
                } else {
                    Color32::from_black_alpha(85)
                },
            ),
            egui::StrokeKind::Inside,
        );
        // A highlight along the top edge lifts each block off its neighbour.
        if r.width() > 6.0 && r.height() > 6.0 {
            painter.line_segment(
                [
                    r.left_top() + Vec2::new(1.0, 1.0),
                    r.right_top() + Vec2::new(-1.0, 1.0),
                ],
                Stroke::new(1.0, Color32::from_white_alpha(if dir { 40 } else { 22 })),
            );
        }
    }

    // Labels sit out the zoom: text sliding and rescaling across the screen
    // reads as a glitch, while the blocks moving underneath reads as motion.
    let labels = !zooming;
    // Folder headers.
    for t in &st.layout.tiles {
        if !labels || !ix.is_dir(t.idx) || t.rect.width() < 60.0 || t.rect.height() < 26.0 {
            continue;
        }
        painter.text(
            t.rect.min + Vec2::new(4.0, 2.0),
            Align2::LEFT_TOP,
            elide(ix.name(t.idx), (t.rect.width() / 6.5) as usize),
            FontId::proportional(10.5),
            Color32::from_white_alpha(215),
        );
    }
    // File labels where there is room.
    for t in &st.layout.tiles {
        if !labels || ix.is_dir(t.idx) || t.rect.width() < 54.0 || t.rect.height() < 16.0 {
            continue;
        }
        painter.text(
            t.rect.center(),
            Align2::CENTER_CENTER,
            elide(ix.name(t.idx), (t.rect.width() / 6.0) as usize),
            FontId::proportional(10.0),
            Color32::from_white_alpha(225),
        );
    }

    // Taken from the response, not from raw input: `Response::hover_pos` is
    // empty whenever something sits on top — a settings window, a context menu
    // — so the chart no longer lights up and reacts underneath them.
    let pointer = response.hover_pos();
    // Deepest tile wins; tiles are pushed parent-before-child. Mid-zoom the
    // rectangles under the cursor are still moving, so hit-testing them would
    // pick a different tile every frame.
    let hovered = (!zooming)
        .then(|| {
            pointer.filter(|p| rect.contains(*p)).and_then(|p| {
                st.layout
                    .tiles
                    .iter()
                    .rev()
                    .find(|t| t.rect.contains(p))
                    .map(|t| t.idx)
            })
        })
        .flatten();
    out.hovered = hovered;

    // Moving to a different block restarts the highlight, so it reads as a
    // deliberate pop rather than a rectangle teleporting around the map.
    if st.hover != hovered {
        st.hover = hovered;
        st.hover_t = if st.animate { 0.0 } else { 1.0 };
    }
    if st.hover_t < 1.0 {
        st.hover_t = (st.hover_t + ui.input(|i| i.stable_dt) / 0.12).min(1.0);
        ui.ctx().request_repaint();
    }

    if let Some(h) = hovered {
        let ht = ease_out(st.hover_t);
        if let Some(t) = st.layout.tiles.iter().find(|t| t.idx == h) {
            painter.rect_filled(
                t.rect,
                egui::CornerRadius::ZERO,
                Color32::from_white_alpha((40.0 * ht) as u8),
            );
            painter.rect_stroke(
                t.rect,
                egui::CornerRadius::ZERO,
                Stroke::new(1.5, Color32::WHITE.gamma_multiply(ht)),
                egui::StrokeKind::Inside,
            );
        }
        if let Some(p) = pointer {
            super::sunburst_tooltip(&painter, ix, h, p, rect, st.layout.total);
        }
    }

    if response.clicked() {
        out.clicked = hovered;
    }
    if response.double_clicked() {
        out.double = hovered;
    }
    if response.secondary_clicked() {
        out.context = hovered;
    }
    out
}

pub fn elide(s: &str, max: usize) -> String {
    if max < 2 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}
