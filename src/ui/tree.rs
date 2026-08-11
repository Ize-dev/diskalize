//! Virtualised directory tree and search-result list.
//!
//! Both render only the rows the scroll area actually shows, so a folder with a
//! million children costs the same per frame as one with ten.

use std::collections::HashSet;

use egui::{Align2, FontId, Rect, Sense, Stroke, Ui, Vec2};

use crate::fmt;
use crate::index::Index;
use crate::ui::theme;

const ROW_H: f32 = 21.0;

#[derive(Default)]
pub struct TreeState {
    pub expanded: HashSet<u32>,
    /// Flattened visible rows: (node, depth).
    flat: Vec<(u32, u16)>,
    built_for: (u32, u64, u64),
    dirty: u64,
    pub selected: Option<u32>,
}

impl TreeState {
    pub fn toggle(&mut self, idx: u32) {
        if !self.expanded.remove(&idx) {
            self.expanded.insert(idx);
        }
        self.dirty += 1;
    }

    pub fn expand_to(&mut self, ix: &Index, idx: u32) {
        let mut cur = ix.parent[idx as usize];
        while cur != u32::MAX {
            self.expanded.insert(cur);
            if cur == ix.root {
                break;
            }
            cur = ix.parent[cur as usize];
        }
        self.dirty += 1;
    }

    pub fn reset(&mut self) {
        self.expanded.clear();
        self.flat.clear();
        self.selected = None;
        self.built_for = (u32::MAX, 0, 0);
        self.dirty += 1;
    }

    fn rebuild(&mut self, ix: &Index, root: u32) {
        let key = (root, ix.generation, self.dirty);
        if self.built_for == key {
            return;
        }
        self.built_for = key;
        self.flat.clear();
        if root == u32::MAX || root as usize >= ix.len() {
            return;
        }
        // Explicit stack; recursion would blow up on pathological trees.
        let mut stack: Vec<(u32, u16)> = vec![(root, 0)];
        while let Some((node, depth)) = stack.pop() {
            self.flat.push((node, depth));
            if !self.expanded.contains(&node) {
                continue;
            }
            let mut kids = ix.top_children_by_size(node, 20_000);
            kids.retain(|&c| ix.is_dir(c));
            for &c in kids.iter().rev() {
                stack.push((c, depth + 1));
            }
        }
    }
}

pub struct TreeAction {
    pub focus: Option<u32>,
    pub context: Option<u32>,
}

pub fn show(ui: &mut Ui, ix: &Index, st: &mut TreeState, root: u32, view_root: u32) -> TreeAction {
    let mut act = TreeAction {
        focus: None,
        context: None,
    };
    st.rebuild(ix, root);
    let total = ix.size[root as usize].max(1);

    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, st.flat.len(), |ui, range| {
            let width = ui.available_width().max(260.0);
            for row in range {
                let (idx, depth) = st.flat[row];
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
                let p = ui.painter();
                let hovered = resp.hovered();
                let selected = st.selected == Some(idx);
                let is_view = idx == view_root;

                if selected || is_view {
                    p.rect_filled(
                        rect,
                        egui::CornerRadius::same(4),
                        theme::ACCENT.gamma_multiply(if selected { 0.28 } else { 0.14 }),
                    );
                } else if hovered {
                    p.rect_filled(rect, egui::CornerRadius::same(4), theme::PANEL_HI);
                }

                let indent = 6.0 + depth as f32 * 13.0;
                let has_kids = ix.children(idx).any(|c| ix.is_dir(c));

                // Expander triangle.
                let tri = Rect::from_center_size(
                    egui::Pos2::new(rect.left() + indent + 5.0, rect.center().y),
                    Vec2::splat(14.0),
                );
                if has_kids {
                    let open = st.expanded.contains(&idx);
                    p.text(
                        tri.center(),
                        Align2::CENTER_CENTER,
                        if open { "▾" } else { "▸" },
                        FontId::proportional(10.0),
                        theme::TEXT_DIM,
                    );
                }

                // Proportional bar behind the size column.
                let frac = ix.size[idx as usize] as f64 / total as f64;
                let bar_w = 54.0;
                let bar = Rect::from_min_size(
                    egui::Pos2::new(rect.right() - bar_w - 84.0, rect.center().y - 4.0),
                    Vec2::new(bar_w, 8.0),
                );
                p.rect_filled(bar, egui::CornerRadius::same(2), theme::PANEL_HI);
                p.rect_filled(
                    Rect::from_min_size(
                        bar.min,
                        Vec2::new((bar.width() * frac as f32).max(1.0), bar.height()),
                    ),
                    egui::CornerRadius::same(2),
                    theme::ACCENT,
                );

                let name_w = bar.left() - rect.left() - indent - 24.0;
                p.text(
                    egui::Pos2::new(rect.left() + indent + 18.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    super::treemap::elide(ix.name(idx), (name_w / 6.6).max(4.0) as usize),
                    FontId::proportional(12.0),
                    theme::TEXT,
                );
                p.text(
                    egui::Pos2::new(rect.right() - 8.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    fmt::size(ix.size[idx as usize]),
                    FontId::proportional(11.5),
                    theme::TEXT,
                );

                if resp.clicked() {
                    if has_kids && resp.interact_pointer_pos().is_some_and(|pp| tri.contains(pp)) {
                        st.toggle(idx);
                    } else {
                        st.selected = Some(idx);
                        act.focus = Some(idx);
                    }
                }
                // Explorer's tree pane toggles the node on double-click.
                if resp.double_clicked() && has_kids {
                    st.toggle(idx);
                }
                if resp.secondary_clicked() {
                    st.selected = Some(idx);
                    act.context = Some(idx);
                }
            }
        });
    act
}

// ---- search results ----------------------------------------------------------

pub use crate::store::SortKey;
use crate::store::Hit;
use crate::ui::preview::Thumbs;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ListMode {
    Details,
    Tiles,
}

/// How far a key press moves the selection.
#[derive(Clone, Copy)]
pub enum Step {
    /// One row up/down (a whole grid row in tile view).
    Prev,
    Next,
    /// One entry, used by left/right in the grid.
    Back,
    Forward,
    PageUp,
    PageDown,
    First,
    Last,
}

pub struct ListAction {
    pub focus: Option<Hit>,
    pub open: Option<Hit>,
    pub context: Option<Hit>,
    pub sort: Option<SortKey>,
}

/// One volume's index plus the path prefix used when showing hits from it.
pub struct HitCtx<'a> {
    pub indexes: &'a [(u16, parking_lot::RwLockReadGuard<'a, Index>)],
}

impl<'a> HitCtx<'a> {
    fn ix(&self, vol: u16) -> Option<&Index> {
        self.indexes
            .iter()
            .find(|(v, _)| *v == vol)
            .map(|(_, g)| &**g)
    }
}

/// Applies a keyboard step to the selection and reports the new row so the
/// caller can scroll it into view.
pub fn step_selection(
    hits: &[Hit],
    selected: &mut Option<Hit>,
    step: Step,
    per_row: usize,
    page: usize,
) -> Option<usize> {
    if hits.is_empty() {
        return None;
    }
    let cur = selected
        .and_then(|s| hits.iter().position(|h| *h == s))
        .unwrap_or(0) as isize;
    let n = hits.len() as isize;
    let row = per_row.max(1) as isize;
    let next = match step {
        Step::Prev => cur - row,
        Step::Next => cur + row,
        Step::Back => cur - 1,
        Step::Forward => cur + 1,
        Step::PageUp => cur - (page.max(1) as isize) * row,
        Step::PageDown => cur + (page.max(1) as isize) * row,
        Step::First => 0,
        Step::Last => n - 1,
    }
    .clamp(0, n - 1) as usize;
    *selected = Some(hits[next]);
    Some(next)
}

pub struct ListParams {
    pub sort: SortKey,
    pub desc: bool,
    pub mode: ListMode,
    pub tile_px: u32,
    /// Fade freshly decoded thumbnails in.
    pub animate: bool,
    /// Row (or grid row) to bring into view this frame.
    pub scroll_to: Option<usize>,
    /// Real shell icons instead of the colour-coded dot.
    pub icons: bool,
}

/// Draws the leading marker for a row: the shell icon, or a dot tinted by file
/// type when icons are switched off.
fn row_marker(
    ui: &Ui,
    icons: &mut crate::ui::preview::Icons,
    use_icons: bool,
    ix: &Index,
    idx: u32,
    centre: egui::Pos2,
    size: f32,
) {
    if use_icons {
        if let Some(tex) =
            icons.get(ui.ctx(), ix.name_bytes(idx), ix.is_dir(idx))
        {
            ui.painter().image(
                tex.id(),
                Rect::from_center_size(centre, Vec2::splat(size)),
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            return;
        }
    }
    let col = if ix.is_dir(idx) {
        theme::WARN
    } else {
        theme::hsl(theme::ext_hue(ext_of(ix.name_bytes(idx))), 0.5, 0.6)
    };
    ui.painter().rect_filled(
        Rect::from_center_size(centre, Vec2::splat(6.0)),
        egui::CornerRadius::same(2),
        col,
    );
}

/// Column widths of the details list, in points.
///
/// Kept as real widths rather than fractions: a column sized to fit a date
/// should stay that size when the window grows, and only the last column —
/// the path — takes up the slack.
#[derive(Clone, Copy, PartialEq)]
pub struct Columns {
    pub name: f32,
    pub size: f32,
    pub date: f32,
}

impl Default for Columns {
    fn default() -> Self {
        Self {
            name: 300.0,
            size: 96.0,
            date: 122.0,
        }
    }
}

impl Columns {
    /// Clamps against the space actually available, so a narrow window cannot
    /// leave the path column with negative width.
    fn fitted(self, full: f32) -> Self {
        let name = self.name.clamp(80.0, (full - 200.0).max(80.0));
        let size = self.size.clamp(50.0, 200.0);
        let date = self.date.clamp(60.0, 260.0);
        Self { name, size, date }
    }
}

/// Draggable boundary between two columns.
///
/// Drawn as a hairline in the header and widened to a comfortable grab area;
/// a 1 px target would be unusable.
fn column_handle(ui: &mut Ui, hrect: Rect, x: f32, salt: usize, width: &mut f32) -> bool {
    let grab = Rect::from_min_size(
        egui::Pos2::new(hrect.left() + x - 3.0, hrect.top()),
        Vec2::new(6.0, hrect.height()),
    );
    let resp = ui.interact(
        grab,
        ui.id().with(("col", salt)),
        Sense::click_and_drag(),
    );
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    let dragged = resp.dragged();
    if dragged {
        *width += resp.drag_delta().x;
    }
    let line = if resp.hovered() || dragged {
        theme::ACCENT
    } else {
        theme::LINE
    };
    ui.painter().line_segment(
        [
            egui::Pos2::new(hrect.left() + x, hrect.top() + 4.0),
            egui::Pos2::new(hrect.left() + x, hrect.bottom() - 4.0),
        ],
        Stroke::new(1.0, line),
    );
    dragged || resp.hovered()
}

pub fn results(
    ui: &mut Ui,
    ctx: &HitCtx<'_>,
    hits: &[Hit],
    selected: &mut Option<Hit>,
    params: &ListParams,
    thumbs: &mut Thumbs,
    icons: &mut crate::ui::preview::Icons,
    cols: &mut Columns,
) -> ListAction {
    let (sort, desc) = (params.sort, params.desc);
    let mut act = ListAction {
        focus: None,
        open: None,
        context: None,
        sort: None,
    };
    if params.mode != ListMode::Details {
        grid(ui, ctx, hits, selected, params, thumbs, icons, &mut act);
        return act;
    }

    let full = ui.available_width();
    let fitted = cols.fitted(full);
    let (name_w, size_w, date_w) = (fitted.name, fitted.size, fitted.date);

    // Header
    let (hrect, _) = ui.allocate_exact_size(Vec2::new(full, 24.0), Sense::hover());

    // Boundaries first: a click that lands on a grab area is a resize, not a
    // request to sort by that column.
    let mut on_handle = false;
    let mut widths = [fitted.name, fitted.size, fitted.date];
    for (n, x) in [name_w, name_w + size_w, name_w + size_w + date_w]
        .into_iter()
        .enumerate()
    {
        if column_handle(ui, hrect, x, n, &mut widths[n]) {
            on_handle = true;
        }
    }
    *cols = Columns {
        name: widths[0],
        size: widths[1],
        date: widths[2],
    }
    .fitted(full);

    {
        let p = ui.painter();
        p.rect_filled(hrect, egui::CornerRadius::same(4), theme::PANEL);
        let hdr = |x: f32, w: f32, label: &str, key: SortKey, right: bool| {
            let r = Rect::from_min_size(
                egui::Pos2::new(hrect.left() + x, hrect.top()),
                Vec2::new(w, hrect.height()),
            );
            let txt = if sort == key {
                format!("{label} {}", if desc { "▾" } else { "▴" })
            } else {
                label.to_string()
            };
            p.text(
                if right {
                    egui::Pos2::new(r.right() - 8.0, r.center().y)
                } else {
                    egui::Pos2::new(r.left() + 8.0, r.center().y)
                },
                if right {
                    Align2::RIGHT_CENTER
                } else {
                    Align2::LEFT_CENTER
                },
                txt,
                FontId::proportional(11.0),
                if sort == key {
                    theme::ACCENT
                } else {
                    theme::TEXT_DIM
                },
            );
            r
        };
        let rects = [
            (hdr(0.0, name_w, crate::i18n::t("Name"), SortKey::Name, false), SortKey::Name),
            (
                hdr(name_w, size_w, crate::i18n::t("Größe"), SortKey::Size, true),
                SortKey::Size,
            ),
            (
                hdr(name_w + size_w, date_w, crate::i18n::t("Geändert"), SortKey::Date, false),
                SortKey::Date,
            ),
            (
                hdr(
                    name_w + size_w + date_w,
                    full - name_w - size_w - date_w,
                    crate::i18n::t("Pfad"),
                    SortKey::Path,
                    false,
                ),
                SortKey::Path,
            ),
        ];
        if !on_handle && ui.input(|i| i.pointer.primary_clicked()) {
            if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
                for (r, k) in rects {
                    if r.contains(pos) {
                        act.sort = Some(k);
                    }
                }
            }
        }
    }

    let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if let Some(row) = params.scroll_to {
        // Keep the keyboard selection visible without yanking the view around.
        let y = row as f32 * ROW_H;
        area = area.vertical_scroll_offset(
            (y - ui.available_height() * 0.5).max(0.0),
        );
    }
    area.show_rows(ui, ROW_H, hits.len(), |ui, range| {
            for row in range {
                let hit = hits[row];
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(full, ROW_H), Sense::click());
                let Some(ix) = ctx.ix(hit.vol) else { continue };
                let idx = hit.idx;
                let p = ui.painter();
                if *selected == Some(hit) {
                    p.rect_filled(
                        rect,
                        egui::CornerRadius::same(4),
                        theme::ACCENT.gamma_multiply(0.28),
                    );
                } else if resp.hovered() {
                    p.rect_filled(rect, egui::CornerRadius::same(4), theme::PANEL_HI);
                } else if row % 2 == 1 {
                    p.rect_filled(rect, egui::CornerRadius::ZERO, theme::PANEL.gamma_multiply(0.5));
                }

                row_marker(
                    ui,
                    icons,
                    params.icons,
                    ix,
                    idx,
                    egui::Pos2::new(rect.left() + 11.0, rect.center().y),
                    16.0,
                );

                let p = ui.painter();
                p.text(
                    egui::Pos2::new(rect.left() + 24.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    super::treemap::elide(ix.name(idx), ((name_w - 26.0) / 6.6) as usize),
                    FontId::proportional(12.0),
                    theme::TEXT,
                );
                p.text(
                    egui::Pos2::new(rect.left() + name_w + size_w - 8.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    fmt::size(ix.size[idx as usize]),
                    FontId::proportional(11.5),
                    theme::TEXT,
                );
                p.text(
                    egui::Pos2::new(rect.left() + name_w + size_w + 8.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    fmt::timestamp(ix.mtime[idx as usize]),
                    FontId::proportional(11.0),
                    theme::TEXT_DIM,
                );
                let parent = ix.parent[idx as usize];
                let path = if parent == u32::MAX {
                    String::new()
                } else {
                    ix.path_of(parent)
                };
                let px = rect.left() + name_w + size_w + date_w + 8.0;
                p.text(
                    egui::Pos2::new(px, rect.center().y),
                    Align2::LEFT_CENTER,
                    super::treemap::elide(&path, ((rect.right() - px - 8.0) / 6.4) as usize),
                    FontId::proportional(11.0),
                    theme::TEXT_DIM,
                );

                if resp.clicked() {
                    *selected = Some(hit);
                    act.focus = Some(hit);
                }
                if resp.double_clicked() {
                    act.open = Some(hit);
                }
                if resp.secondary_clicked() {
                    *selected = Some(hit);
                    act.context = Some(hit);
                }
            }
        });
    act
}

/// Tile layout: a virtualised grid of shell thumbnails, sized by the slider.
fn grid(
    ui: &mut Ui,
    ctx: &HitCtx<'_>,
    hits: &[Hit],
    selected: &mut Option<Hit>,
    params: &ListParams,
    thumbs: &mut Thumbs,
    icons: &mut crate::ui::preview::Icons,
    act: &mut ListAction,
) {
    let thumb_px = params.tile_px;
    let now = ui.input(|i| i.time);
    let mut fading = false;
    let cell = Vec2::new(thumb_px as f32 + 26.0, thumb_px as f32 + 40.0);
    let avail = ui.available_width();
    let cols = ((avail / cell.x).floor() as usize).max(1);
    let rows = hits.len().div_ceil(cols);

    let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if let Some(i) = params.scroll_to {
        let y = (i / cols) as f32 * (cell.y + 6.0);
        area = area.vertical_scroll_offset((y - ui.available_height() * 0.5).max(0.0));
    }
    area.show_rows(ui, cell.y + 6.0, rows, |ui, range| {
            for row in range {
                ui.horizontal(|ui| {
                    for col in 0..cols {
                        let Some(&hit) = hits.get(row * cols + col) else {
                            break;
                        };
                        let Some(ix) = ctx.ix(hit.vol) else { continue };
                        let (rect, resp) = ui.allocate_exact_size(cell, Sense::click());
                        let is_sel = *selected == Some(hit);
                        let p = ui.painter();
                        if is_sel {
                            p.rect_filled(
                                rect,
                                egui::CornerRadius::same(6),
                                theme::ACCENT.gamma_multiply(0.28),
                            );
                        } else if resp.hovered() {
                            p.rect_filled(rect, egui::CornerRadius::same(6), theme::PANEL_HI);
                        }

                        let path = ix.path_of(hit.idx);
                        let name = ix.name(hit.idx);
                        let size = fmt::size(ix.size[hit.idx as usize]);

                        let img_box = Rect::from_min_size(
                            rect.min + Vec2::new(6.0, 6.0),
                            Vec2::new(cell.x - 12.0, cell.y - 34.0),
                        );
                        let fade = if params.animate {
                            thumbs.fade(&path, thumb_px, now)
                        } else {
                            1.0
                        };
                        if fade < 1.0 {
                            fading = true;
                        }
                        match thumbs.get(&path, thumb_px) {
                            Some(tex) => {
                                let ts = tex.size_vec2();
                                let s = (img_box.width() / ts.x)
                                    .min(img_box.height() / ts.y)
                                    .min(1.0);
                                // A thumbnail that just decoded rises into place
                                // instead of appearing at full size and opacity.
                                let grow = 0.94 + 0.06 * fade;
                                p.image(
                                    tex.id(),
                                    Rect::from_center_size(img_box.center(), ts * s * grow),
                                    Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                    egui::Color32::WHITE.gamma_multiply(fade),
                                );
                            }
                            None => {
                                p.rect_filled(img_box, egui::CornerRadius::same(4), theme::PANEL);
                                if params.icons {
                                    if let Some(tex) =
                                        icons.get(ui.ctx(), ix.name_bytes(hit.idx), ix.is_dir(hit.idx))
                                    {
                                        let s = (img_box.width() * 0.45).min(48.0);
                                        p.image(
                                            tex.id(),
                                            Rect::from_center_size(img_box.center(), Vec2::splat(s)),
                                            Rect::from_min_max(
                                                egui::pos2(0.0, 0.0),
                                                egui::pos2(1.0, 1.0),
                                            ),
                                            egui::Color32::WHITE,
                                        );
                                    }
                                }
                            }
                        }

                        p.text(
                            egui::Pos2::new(rect.center().x, rect.bottom() - 22.0),
                            Align2::CENTER_CENTER,
                            super::treemap::elide(name, ((cell.x - 10.0) / 6.2) as usize),
                            FontId::proportional(11.0),
                            theme::TEXT,
                        );
                        p.text(
                            egui::Pos2::new(rect.center().x, rect.bottom() - 8.0),
                            Align2::CENTER_CENTER,
                            size,
                            FontId::proportional(10.0),
                            theme::TEXT_DIM,
                        );

                        if resp.clicked() {
                            *selected = Some(hit);
                            act.focus = Some(hit);
                        }
                        if resp.double_clicked() {
                            act.open = Some(hit);
                        }
                        if resp.secondary_clicked() {
                            *selected = Some(hit);
                            act.context = Some(hit);
                        }
                    }
                });
            }
        });
    // Thumbnails arrive on worker threads, so without this the fade would stall
    // on whatever frame happened to be the last one requested.
    if fading {
        ui.ctx().request_repaint();
    }
}

fn ext_of(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(p) if p + 1 < name.len() => &name[p + 1..],
        _ => b"",
    }
}

/// Centred placeholder for the "nothing to show yet" states.
pub fn empty_hint(ui: &mut Ui, text: &str) {
    let rect = ui.available_rect_before_wrap();
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(13.0),
        theme::TEXT_DIM,
    );
    ui.allocate_rect(rect, Sense::hover());
}

