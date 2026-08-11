use egui::{Color32, Context, CornerRadius, Stroke, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x14, 0x16, 0x1a);
pub const PANEL: Color32 = Color32::from_rgb(0x1a, 0x1d, 0x23);
pub const PANEL_HI: Color32 = Color32::from_rgb(0x22, 0x26, 0x2e);
pub const LINE: Color32 = Color32::from_rgb(0x2d, 0x32, 0x3c);
pub const TEXT: Color32 = Color32::from_rgb(0xdd, 0xe1, 0xe8);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa1);
pub const ACCENT: Color32 = Color32::from_rgb(0x4d, 0x9b, 0xff);
pub const WARN: Color32 = Color32::from_rgb(0xff, 0xb1, 0x4d);
pub const GOOD: Color32 = Color32::from_rgb(0x5c, 0xd6, 0x8a);

/// egui's bundled fonts have no geometric shapes or arrows, so `▾ ▸ ↑ ›` render
/// as tofu boxes. Pulling in the Windows system faces fixes that and makes the
/// app look native at the same time. Each is optional — if a file is missing we
/// silently keep whatever egui shipped.
fn install_fonts(ctx: &Context) {
    use std::sync::Arc;

    let mut fonts = egui::FontDefinitions::default();
    let mut proportional: Vec<String> = Vec::new();
    let mut monospace: Vec<String> = Vec::new();

    let mut load = |name: &str, path: &str| -> bool {
        match std::fs::read(path) {
            Ok(bytes) => {
                fonts.font_data.insert(
                    name.to_owned(),
                    Arc::new(egui::FontData::from_owned(bytes)),
                );
                true
            }
            Err(_) => false,
        }
    };

    if load("segoe", r"C:\Windows\Fonts\segoeui.ttf") {
        proportional.push("segoe".into());
    }
    // Symbol face second: egui falls back per glyph, so text keeps the UI face
    // and only the shapes come from here.
    if load("segoe_sym", r"C:\Windows\Fonts\seguisym.ttf") {
        proportional.push("segoe_sym".into());
        monospace.push("segoe_sym".into());
    }
    if load("consola", r"C:\Windows\Fonts\consola.ttf") {
        monospace.insert(0, "consola".into());
    }

    if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        for (i, name) in proportional.into_iter().enumerate() {
            f.insert(i, name);
        }
    }
    if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        for (i, name) in monospace.into_iter().enumerate() {
            f.insert(i, name);
        }
    }
    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &Context) {
    install_fonts(ctx);
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(0x10, 0x12, 0x16);
    v.faint_bg_color = PANEL_HI;
    v.override_text_color = Some(TEXT);
    v.window_stroke = Stroke::new(1.0, LINE);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    v.widgets.inactive.bg_fill = PANEL_HI;
    v.widgets.inactive.weak_bg_fill = PANEL_HI;
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x2e, 0x34, 0x3f);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2e, 0x34, 0x3f);
    v.widgets.active.bg_fill = ACCENT.gamma_multiply(0.6);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    let r = CornerRadius::same(6);
    v.widgets.noninteractive.corner_radius = r;
    v.widgets.inactive.corner_radius = r;
    v.widgets.hovered.corner_radius = r;
    v.widgets.active.corner_radius = r;
    ctx.options_mut(|o| o.theme_preference = egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.scroll.bar_width = 10.0;
    });
}

/// HSL -> sRGB. `h` in turns [0,1), `s`/`l` in [0,1].
pub fn hsl(h: f32, s: f32, l: f32) -> Color32 {
    let h = h.rem_euclid(1.0) * 6.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color32::from_rgb(
        ((r + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).clamp(0.0, 255.0) as u8,
    )
}

/// Stable hue for a file extension, used to colour the treemap/list by type.
pub fn ext_hue(ext: &[u8]) -> f32 {
    let mut h: u32 = 2166136261;
    for &b in ext {
        h ^= b.to_ascii_lowercase() as u32;
        h = h.wrapping_mul(16777619);
    }
    (h % 1000) as f32 / 1000.0
}
