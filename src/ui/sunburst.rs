//! Multi-ring sunburst ("Kuchendiagramm") with animated zoom transitions.
//!
//! The layout is rebuilt only when the view root, ring count or index generation
//! changes — never per frame. Each frame emits a single `egui::Mesh` (one draw
//! call), and hit-testing is a binary search over the ring's angle-sorted
//! segments, so hovering stays O(log n) no matter how many files are indexed.

use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, TAU};

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2};

use crate::fmt;
use crate::index::Index;
use crate::ui::theme;

/// Below this angular width a slice is not worth a triangle.
const MIN_ANGLE: f32 = 0.006;
const ANIM_SECS: f32 = 0.28;

pub struct Seg {
    pub idx: u32,
    pub a0: f32,
    pub a1: f32,
    pub ring: u16,
    pub color: Color32,
}

#[derive(Default)]
pub struct Layout {
    pub total: u64,
    pub segs: Vec<Seg>,
    /// `segs` grouped by ring; each range is sorted by `a0`.
    pub ring_ranges: Vec<(usize, usize)>,
    /// node -> position in `segs`, so per-frame highlight lookups stay O(1).
    pub by_idx: HashMap<u32, u32>,
}

impl Layout {
    fn seg_of(&self, idx: u32) -> Option<&Seg> {
        self.by_idx.get(&idx).map(|&i| &self.segs[i as usize])
    }
}

fn seg_color(ix: &Index, idx: u32, hue: f32, ring: usize) -> Color32 {
    let jitter = ((idx.wrapping_mul(2654435761) >> 16) % 1000) as f32 / 1000.0 - 0.5;
    let h = hue + jitter * 0.022;
    let l = (0.40 + ring as f32 * 0.052).min(0.72);
    let s = if ix.is_dir(idx) { 0.58 } else { 0.44 };
    theme::hsl(h, (s - ring as f32 * 0.02).max(0.24), l)
}

pub fn build(ix: &Index, root: u32, max_rings: usize) -> Layout {
    let total = ix.size[root as usize];
    let mut segs: Vec<Seg> = Vec::with_capacity(1024);
    let mut ring_ranges = Vec::with_capacity(max_rings);
    if total == 0 {
        return Layout {
            total,
            ..Default::default()
        };
    }

    // (node, a0, a1)
    let mut frontier: Vec<(u32, f32, f32)> = vec![(root, 0.0, TAU)];
    for ring in 0..max_rings {
        let start = segs.len();
        let mut next = Vec::new();
        for &(parent, pa0, pa1) in &frontier {
            let span = pa1 - pa0;
            if span <= MIN_ANGLE {
                continue;
            }
            let psize = ix.size[parent as usize];
            if psize == 0 {
                continue;
            }
            let mut a = pa0;
            for c in ix.top_children_by_size(parent, 4096) {
                let sz = ix.size[c as usize];
                if sz == 0 {
                    continue;
                }
                let w = span * (sz as f64 / psize as f64) as f32;
                if w < MIN_ANGLE {
                    break; // children are size-sorted, everything after is smaller
                }
                let (a0, a1) = (a, a + w);
                a = a1;
                // Hue follows the absolute angle, so every subtree keeps a coherent
                // colour family across rings.
                let hue = (a0 + a1) * 0.5 / TAU;
                segs.push(Seg {
                    idx: c,
                    a0,
                    a1,
                    ring: ring as u16,
                    color: seg_color(ix, c, hue, ring),
                });
                next.push((c, a0, a1));
            }
        }
        ring_ranges.push((start, segs.len()));
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    let by_idx = segs
        .iter()
        .enumerate()
        .map(|(i, s)| (s.idx, i as u32))
        .collect();
    Layout {
        total,
        segs,
        ring_ranges,
        by_idx,
    }
}

#[derive(Clone, Copy)]
struct Geo {
    center: Pos2,
    inner: f32,
    ring_w: f32,
    rings: usize,
}

impl Geo {
    fn radii(&self, ring: f32) -> (f32, f32) {
        let r0 = self.inner + ring * self.ring_w;
        (r0, r0 + self.ring_w)
    }
}

pub struct ChartState {
    pub layout: Layout,
    /// Geometry of the previous layout, keyed by node, used to morph on zoom.
    prev: HashMap<u32, (f32, f32, f32)>,
    anim: f32,
    pub rings: usize,
    /// Morph between layouts on zoom instead of jumping.
    pub animate: bool,
    hover: Option<u32>,
    hover_t: f32,
    pub built_for: (u32, u64, usize),
}

impl Default for ChartState {
    fn default() -> Self {
        Self {
            layout: Layout::default(),
            prev: HashMap::new(),
            anim: 1.0,
            rings: 7,
            animate: true,
            hover: None,
            hover_t: 1.0,
            built_for: (u32::MAX, 0, 0),
        }
    }
}

impl ChartState {
    pub fn ensure(&mut self, ix: &Index, root: u32) {
        let key = (root, ix.generation, self.rings);
        if self.built_for == key {
            return;
        }
        let root_changed = self.built_for.0 != root && self.built_for.0 != u32::MAX;
        if root_changed {
            self.prev = self
                .layout
                .segs
                .iter()
                .map(|s| (s.idx, (s.a0, s.a1, s.ring as f32)))
                .collect();
            // The old view root becomes a full circle one ring further in.
            self.prev.insert(root, (0.0, TAU, -1.0));
            self.anim = if self.animate { 0.0 } else { 1.0 };
        }
        self.layout = build(ix, root, self.rings);
        self.built_for = key;
    }

    pub fn invalidate(&mut self) {
        self.built_for = (u32::MAX, 0, 0);
        self.prev.clear();
        self.anim = 1.0;
    }
}

pub struct Interaction {
    pub hovered: Option<u32>,
    pub clicked: Option<u32>,
    pub double: Option<u32>,
    pub context: Option<u32>,
    pub go_up: bool,
}

const HOVER_SECS: f32 = 0.12;

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn add_sector(
    mesh: &mut egui::Mesh,
    c: Pos2,
    r0: f32,
    r1: f32,
    a0: f32,
    a1: f32,
    color: Color32,
) {
    if r1 <= r0 || a1 <= a0 {
        return;
    }
    let span = a1 - a0;
    let steps = ((span / 0.09).ceil() as usize).clamp(1, 128);
    let base = mesh.vertices.len() as u32;
    for k in 0..=steps {
        let a = a0 + span * (k as f32 / steps as f32) - FRAC_PI_2;
        let (s, co) = a.sin_cos();
        mesh.colored_vertex(Pos2::new(c.x + co * r0, c.y + s * r0), color);
        mesh.colored_vertex(Pos2::new(c.x + co * r1, c.y + s * r1), color);
    }
    for k in 0..steps as u32 {
        let i = base + k * 2;
        mesh.add_triangle(i, i + 1, i + 2);
        mesh.add_triangle(i + 1, i + 3, i + 2);
    }
}

fn hit(layout: &Layout, geo: &Geo, p: Pos2) -> Option<u32> {
    let d = p - geo.center;
    let r = d.length();
    if r < geo.inner || r > geo.inner + geo.ring_w * geo.rings as f32 {
        return None;
    }
    let ring = ((r - geo.inner) / geo.ring_w).floor() as usize;
    let &(s, e) = layout.ring_ranges.get(ring)?;
    let mut a = d.y.atan2(d.x) + FRAC_PI_2;
    if a < 0.0 {
        a += TAU;
    }
    let slice = &layout.segs[s..e];
    let i = slice.partition_point(|g| g.a0 <= a);
    if i == 0 {
        return None;
    }
    let g = &slice[i - 1];
    (a < g.a1).then_some(g.idx)
}

pub fn show(ui: &mut Ui, ix: &Index, st: &mut ChartState, view_root: u32) -> Interaction {
    let mut out = Interaction {
        hovered: None,
        clicked: None,
        double: None,
        context: None,
        go_up: false,
    };

    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, Sense::click());
    let painter = ui.painter_at(rect);

    let radius = (rect.width().min(rect.height()) * 0.5) - 6.0;
    if radius < 40.0 {
        return out;
    }
    let rings = st.layout.ring_ranges.len().max(1);
    let inner = (radius * 0.19).max(34.0);
    let geo = Geo {
        center: rect.center(),
        inner,
        ring_w: (radius - inner) / rings as f32,
        rings,
    };

    if st.anim < 1.0 {
        st.anim = (st.anim + ui.input(|i| i.stable_dt) / ANIM_SECS).min(1.0);
        ui.ctx().request_repaint();
    }
    let t = ease_out(st.anim);

    // ---- fills: one mesh, one draw call ----
    let mut mesh = egui::Mesh::default();
    mesh.vertices.reserve(st.layout.segs.len() * 6);
    for seg in &st.layout.segs {
        let (mut a0, mut a1, mut ringf, alpha) = if t >= 1.0 {
            (seg.a0, seg.a1, seg.ring as f32, 1.0)
        } else if let Some(&(pa0, pa1, pr)) = st.prev.get(&seg.idx) {
            (
                pa0 + (seg.a0 - pa0) * t,
                pa1 + (seg.a1 - pa1) * t,
                pr + (seg.ring as f32 - pr) * t,
                1.0,
            )
        } else {
            // Newly revealed: unfurl from the slice's own midpoint.
            let mid = (seg.a0 + seg.a1) * 0.5;
            (
                mid + (seg.a0 - mid) * t,
                mid + (seg.a1 - mid) * t,
                seg.ring as f32,
                t,
            )
        };
        if ringf < -0.5 {
            continue;
        }
        ringf = ringf.max(0.0);
        let (r0, r1) = geo.radii(ringf);
        let mid_r = (r0 + r1) * 0.5;
        // Constant-pixel gaps so thin rings do not turn into mush.
        let gap = (0.9 / mid_r).min((a1 - a0) * 0.22);
        a0 += gap * 0.5;
        a1 -= gap * 0.5;
        let color = if alpha >= 1.0 {
            seg.color
        } else {
            seg.color.gamma_multiply(alpha)
        };
        add_sector(&mut mesh, geo.center, r0, r1 - 1.0, a0, a1, color);
    }
    painter.add(Shape::mesh(mesh));

    // ---- hover ----
    // Taken from the response, not from raw input: `Response::hover_pos` is
    // empty whenever something sits on top — a settings window, a context menu
    // — so the chart no longer lights up and reacts underneath them.
    let pointer = response.hover_pos();
    let hovered = pointer.filter(|p| rect.contains(*p)).and_then(|p| {
        if (p - geo.center).length() < geo.inner {
            None
        } else {
            hit(&st.layout, &geo, p)
        }
    });
    out.hovered = hovered;

    // Moving onto another slice restarts the highlight, so it lifts out of the
    // ring rather than the outline jumping from one wedge to the next.
    if st.hover != hovered {
        st.hover = hovered;
        st.hover_t = if st.animate { 0.0 } else { 1.0 };
    }
    if st.hover_t < 1.0 {
        st.hover_t = (st.hover_t + ui.input(|i| i.stable_dt) / HOVER_SECS).min(1.0);
        ui.ctx().request_repaint();
    }
    let ht = ease_out(st.hover_t);

    if let Some(h) = hovered {
        if let Some(seg) = st.layout.seg_of(h) {
            let (r0, r1) = geo.radii(seg.ring as f32);
            // The slice grows outward as it lights up — the ring it sits in is
            // often only a few pixels thick, so a colour change alone is easy
            // to miss.
            let lift = 3.5 * ht;
            let mut hi = egui::Mesh::default();
            add_sector(
                &mut hi,
                geo.center,
                r0,
                r1 + lift,
                seg.a0,
                seg.a1,
                Color32::from_white_alpha((46.0 * ht) as u8),
            );
            painter.add(Shape::mesh(hi));
            if ht > 0.02 {
                outline_sector(&painter, geo.center, r0, r1 + lift, seg.a0, seg.a1);
            }
        }
        // Trace the path back to the hub so context is obvious.
        let mut cur = ix.parent[h as usize];
        while cur != u32::MAX && cur != view_root {
            if let Some(seg) = st.layout.seg_of(cur) {
                let (r0, r1) = geo.radii(seg.ring as f32);
                let mut hi = egui::Mesh::default();
                add_sector(
                    &mut hi,
                    geo.center,
                    r0,
                    r1,
                    seg.a0,
                    seg.a1,
                    Color32::from_white_alpha((22.0 * ht) as u8),
                );
                painter.add(Shape::mesh(hi));
            }
            cur = ix.parent[cur as usize];
        }
    }

    // ---- labels for slices with room ----
    let mut labelled = 0;
    for seg in &st.layout.segs {
        if labelled >= 48 || t < 1.0 {
            break;
        }
        let (r0, r1) = geo.radii(seg.ring as f32);
        let mid_r = (r0 + r1) * 0.5;
        let arc = (seg.a1 - seg.a0) * mid_r;
        if arc < 46.0 || geo.ring_w < 15.0 {
            continue;
        }
        let a = (seg.a0 + seg.a1) * 0.5 - FRAC_PI_2;
        let (s, c) = a.sin_cos();
        let pos = geo.center + Vec2::new(c * mid_r, s * mid_r);
        let name = ix.name(seg.idx);
        let max_chars = ((arc / 7.0) as usize).min(24);
        let short: String = if name.chars().count() > max_chars && max_chars > 3 {
            name.chars().take(max_chars - 1).collect::<String>() + "…"
        } else {
            name.to_string()
        };
        painter.text(
            pos + Vec2::new(1.0, 1.0),
            Align2::CENTER_CENTER,
            &short,
            FontId::proportional(11.0),
            Color32::from_black_alpha(160),
        );
        painter.text(
            pos,
            Align2::CENTER_CENTER,
            &short,
            FontId::proportional(11.0),
            Color32::from_rgb(0xf2, 0xf5, 0xfa),
        );
        labelled += 1;
    }

    // ---- centre hub ----
    let hub_hover = pointer.is_some_and(|p| (p - geo.center).length() < geo.inner);
    painter.circle_filled(
        geo.center,
        geo.inner - 2.0,
        if hub_hover {
            theme::PANEL_HI
        } else {
            theme::PANEL
        },
    );
    painter.circle_stroke(geo.center, geo.inner - 2.0, Stroke::new(1.0, theme::LINE));

    let can_up = view_root != ix.root;
    let name = ix.name(view_root);
    painter.text(
        geo.center - Vec2::new(0.0, 12.0),
        Align2::CENTER_CENTER,
        if name.len() > 18 { &name[..18] } else { name },
        FontId::proportional(12.0),
        theme::TEXT,
    );
    painter.text(
        geo.center + Vec2::new(0.0, 4.0),
        Align2::CENTER_CENTER,
        fmt::size(ix.size[view_root as usize]),
        FontId::proportional(15.0),
        theme::ACCENT,
    );
    painter.text(
        geo.center + Vec2::new(0.0, 21.0),
        Align2::CENTER_CENTER,
        if can_up {
            "↑ zurück".to_string()
        } else {
            format!("{} Dateien", fmt::count(ix.files[view_root as usize] as u64))
        },
        FontId::proportional(10.0),
        theme::TEXT_DIM,
    );

    // ---- input ----
    if response.clicked() {
        if hub_hover {
            out.go_up = can_up;
        } else if let Some(h) = hovered {
            out.clicked = Some(h);
        }
    }
    if response.double_clicked() {
        out.double = hovered;
    }
    if response.secondary_clicked() {
        out.context = hovered;
    }

    if let (Some(h), Some(p)) = (hovered, pointer) {
        tooltip(&painter, ix, h, p, rect, st.layout.total);
    }

    out
}

fn outline_sector(painter: &egui::Painter, c: Pos2, r0: f32, r1: f32, a0: f32, a1: f32) {
    let span = a1 - a0;
    let steps = ((span / 0.09).ceil() as usize).clamp(1, 128);
    let mut pts = Vec::with_capacity(steps * 2 + 2);
    for k in 0..=steps {
        let a = a0 + span * (k as f32 / steps as f32) - FRAC_PI_2;
        let (s, co) = a.sin_cos();
        pts.push(Pos2::new(c.x + co * r1, c.y + s * r1));
    }
    for k in (0..=steps).rev() {
        let a = a0 + span * (k as f32 / steps as f32) - FRAC_PI_2;
        let (s, co) = a.sin_cos();
        pts.push(Pos2::new(c.x + co * r0, c.y + s * r0));
    }
    pts.push(pts[0]);
    painter.add(Shape::line(
        pts,
        Stroke::new(1.5, Color32::from_white_alpha(190)),
    ));
}

pub fn tooltip(painter: &egui::Painter, ix: &Index, idx: u32, p: Pos2, bounds: Rect, total: u64) {
    let path = ix.path_of(idx);
    let lines = [
        path,
        crate::i18n::tf(
            "{0}   ·   {1} des Ausschnitts",
            &[
                &fmt::size(ix.size[idx as usize]),
                &fmt::percent(ix.size[idx as usize], total),
            ],
        ),
        if ix.is_dir(idx) {
            crate::i18n::tf(
                "{0} Dateien   ·   logisch {1}",
                &[
                    &fmt::count(ix.files[idx as usize] as u64),
                    &fmt::size(ix.logical[idx as usize]),
                ],
            )
        } else {
            crate::i18n::tf(
                "logisch {0}   ·   geändert {1}",
                &[
                    &fmt::size(ix.logical[idx as usize]),
                    &fmt::timestamp(ix.mtime[idx as usize]),
                ],
            )
        },
    ];

    let font = FontId::proportional(11.5);
    let mut w: f32 = 0.0;
    let galleys: Vec<_> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let g = painter.layout_no_wrap(
                l.clone(),
                if i == 0 {
                    FontId::proportional(12.5)
                } else {
                    font.clone()
                },
                if i == 0 { theme::TEXT } else { theme::TEXT_DIM },
            );
            w = w.max(g.size().x);
            g
        })
        .collect();
    let h: f32 = galleys.iter().map(|g| g.size().y + 2.0).sum::<f32>() + 10.0;
    let size = Vec2::new(w + 20.0, h);

    let mut pos = p + Vec2::new(16.0, 16.0);
    if pos.x + size.x > bounds.right() {
        pos.x = p.x - size.x - 16.0;
    }
    if pos.y + size.y > bounds.bottom() {
        pos.y = p.y - size.y - 16.0;
    }
    let r = Rect::from_min_size(pos, size);
    painter.rect_filled(
        r,
        egui::CornerRadius::same(6),
        Color32::from_rgba_unmultiplied(0x10, 0x12, 0x17, 244),
    );
    painter.rect_stroke(
        r,
        egui::CornerRadius::same(6),
        Stroke::new(1.0, theme::LINE),
        egui::StrokeKind::Inside,
    );
    let mut y = r.top() + 5.0;
    for g in galleys {
        let sz = g.size();
        painter.galley(Pos2::new(r.left() + 10.0, y), g, theme::TEXT);
        y += sz.y + 2.0;
    }
}
