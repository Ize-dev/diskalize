//! Persisted preferences, stored as a flat `key=value` file under `%APPDATA%`.
//!
//! Deliberately hand-rolled: a handful of scalars does not justify a serialiser,
//! and a plain text file stays editable and diffable.

use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, PartialEq)]
pub struct Settings {
    /// Allow more than one Diskalize process.
    pub multi_instance: bool,
    /// Register the global show/search hotkey.
    pub hotkey_enabled: bool,
    /// Virtual-key code plus modifier flags for that hotkey.
    pub hotkey_mods: u32,
    pub hotkey_vk: u32,
    /// Index every fixed drive at startup rather than on first use.
    pub index_all_drives: bool,
    /// Let the preview grow past 100% to fill the pane.
    pub preview_upscale: bool,
    pub tile_px: u32,
    /// Start playing as soon as an audio or video file is selected.
    pub autoplay: bool,
    pub loop_media: bool,
    pub volume: i32,
    /// Real shell icons in the lists instead of colour-coded dots.
    pub show_icons: bool,
    /// Sort folders and files as separate groups.
    pub folders_first: bool,
    /// Animate zoom transitions and preview cross-fades.
    pub animate: bool,
    /// Show the head of text files instead of a generic icon.
    pub text_preview: bool,
    /// A search inside a subfolder only covers that subtree.
    pub search_scoped: bool,
    /// Result cap, in thousands.
    pub search_limit_k: u32,
    pub chart_rings: u32,
    pub map_depth: u32,
    /// Closing the window hides it to the notification area.
    pub close_to_tray: bool,
    /// Interface language code, matching a file in `lang/`. "de" is the source
    /// text itself and needs no file.
    pub lang: String,
    /// How lists are sorted when a window opens.
    pub sort_key: crate::store::SortKey,
    pub sort_desc: bool,
    /// Start on the first drive rather than whichever one the service happened
    /// to announce last.
    pub start_first_drive: bool,
    /// Whether the search covers every indexed drive from the start.
    pub search_all_drives: bool,
    /// Details-view column widths, in points.
    pub col_name: f32,
    pub col_size: f32,
    pub col_date: f32,
    /// UNC shares the user added by hand. The service cannot reach them (see
    /// `App::scan_share`), so nothing else remembers they exist.
    pub shares: Vec<String>,
}

// MOD_ALT | MOD_CONTROL, 'D'
const DEFAULT_MODS: u32 = 0x0001 | 0x0002;
const DEFAULT_VK: u32 = 0x44;

impl Default for Settings {
    fn default() -> Self {
        Self {
            multi_instance: false,
            hotkey_enabled: true,
            hotkey_mods: DEFAULT_MODS,
            hotkey_vk: DEFAULT_VK,
            index_all_drives: true,
            preview_upscale: true,
            tile_px: 128,
            autoplay: true,
            loop_media: false,
            volume: 80,
            show_icons: true,
            folders_first: true,
            animate: true,
            text_preview: true,
            search_scoped: true,
            search_limit_k: 200,
            chart_rings: 7,
            map_depth: 5,
            close_to_tray: true,
            lang: crate::i18n::SOURCE_CODE.to_string(),
            sort_key: crate::store::SortKey::Name,
            sort_desc: false,
            start_first_drive: true,
            search_all_drives: false,
            col_name: 300.0,
            col_size: 96.0,
            col_date: 122.0,
            shares: Vec::new(),
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    let dir = PathBuf::from(base).join("Diskalize");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("settings.txt"))
}

impl Settings {
    pub fn load() -> Self {
        let Some(p) = path() else {
            return Settings::default();
        };
        match std::fs::read_to_string(p) {
            Ok(text) => Settings::parse(&text),
            Err(_) => Settings::default(),
        }
    }

    /// Reads settings out of the file's text. Separate from `load` so it can be
    /// exercised without a `%APPDATA%` to write to.
    pub fn parse(text: &str) -> Self {
        let mut s = Settings::default();
        let map: HashMap<&str, &str> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();

        let flag = |k: &str, cur: bool| map.get(k).map_or(cur, |v| *v == "1" || *v == "true");
        let num = |k: &str, cur: u32| map.get(k).and_then(|v| v.parse().ok()).unwrap_or(cur);

        s.multi_instance = flag("multi_instance", s.multi_instance);
        s.hotkey_enabled = flag("hotkey_enabled", s.hotkey_enabled);
        s.hotkey_mods = num("hotkey_mods", s.hotkey_mods);
        s.hotkey_vk = num("hotkey_vk", s.hotkey_vk);
        s.index_all_drives = flag("index_all_drives", s.index_all_drives);
        s.preview_upscale = flag("preview_upscale", s.preview_upscale);
        s.tile_px = num("tile_px", s.tile_px).clamp(48, 320);
        s.autoplay = flag("autoplay", s.autoplay);
        s.loop_media = flag("loop_media", s.loop_media);
        s.volume = num("volume", s.volume as u32).min(100) as i32;
        s.show_icons = flag("show_icons", s.show_icons);
        s.folders_first = flag("folders_first", s.folders_first);
        s.animate = flag("animate", s.animate);
        s.text_preview = flag("text_preview", s.text_preview);
        s.search_scoped = flag("search_scoped", s.search_scoped);
        s.search_limit_k = num("search_limit_k", s.search_limit_k).clamp(10, 500);
        s.chart_rings = num("chart_rings", s.chart_rings).clamp(3, 12);
        s.map_depth = num("map_depth", s.map_depth).clamp(1, 8);
        s.close_to_tray = flag("close_to_tray", s.close_to_tray);
        let f = |k: &str, cur: f32| map.get(k).and_then(|v| v.parse().ok()).unwrap_or(cur);
        if let Some(k) = map.get("sort_key").and_then(|v| crate::store::SortKey::from_key(v)) {
            s.sort_key = k;
        }
        s.sort_desc = flag("sort_desc", s.sort_desc);
        s.start_first_drive = flag("start_first_drive", s.start_first_drive);
        s.search_all_drives = flag("search_all_drives", s.search_all_drives);
        s.col_name = f("col_name", s.col_name);
        s.col_size = f("col_size", s.col_size);
        s.col_date = f("col_date", s.col_date);
        if let Some(v) = map.get("lang").filter(|v| !v.is_empty()) {
            s.lang = v.to_string();
        }
        // One `share=` line per entry, so a path containing any separator we
        // might have picked stays intact.
        s.shares = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .filter(|(k, _)| k.trim() == "share")
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        let _ = std::fs::write(p, self.render());
    }

    /// The file's text. Split out of `save` so a round trip can be tested.
    pub fn render(&self) -> String {
        let b = |v: bool| if v { "1" } else { "0" };
        let text = format!(
            "multi_instance={}\nhotkey_enabled={}\nhotkey_mods={}\nhotkey_vk={}\n\
             index_all_drives={}\npreview_upscale={}\ntile_px={}\n\
             autoplay={}\nloop_media={}\nvolume={}\nshow_icons={}\n\
             folders_first={}\nanimate={}\ntext_preview={}\nsearch_scoped={}\n\
             search_limit_k={}\nchart_rings={}\nmap_depth={}\nclose_to_tray={}\n\
             lang={}\ncol_name={}\ncol_size={}\ncol_date={}\n\
             sort_key={}\nsort_desc={}\nstart_first_drive={}\nsearch_all_drives={}\n",
            b(self.multi_instance),
            b(self.hotkey_enabled),
            self.hotkey_mods,
            self.hotkey_vk,
            b(self.index_all_drives),
            b(self.preview_upscale),
            self.tile_px,
            b(self.autoplay),
            b(self.loop_media),
            self.volume,
            b(self.show_icons),
            b(self.folders_first),
            b(self.animate),
            b(self.text_preview),
            b(self.search_scoped),
            self.search_limit_k,
            self.chart_rings,
            self.map_depth,
            b(self.close_to_tray),
            self.lang,
            self.col_name,
            self.col_size,
            self.col_date,
            self.sort_key.key(),
            b(self.sort_desc),
            b(self.start_first_drive),
            b(self.search_all_drives)
        );
        let mut text = text;
        for s in &self.shares {
            text.push_str("share=");
            text.push_str(s);
            text.push('\n');
        }
        text
    }

    /// Translates a pressed egui key plus modifiers into Win32 hotkey values.
    /// Returns `None` for keys Windows will not accept on their own.
    pub fn combo_from_egui(key: egui::Key, m: egui::Modifiers) -> Option<(u32, u32)> {
        use egui::Key::*;
        let vk = match key {
            A => 0x41, B => 0x42, C => 0x43, D => 0x44, E => 0x45, F => 0x46,
            G => 0x47, H => 0x48, I => 0x49, J => 0x4A, K => 0x4B, L => 0x4C,
            M => 0x4D, N => 0x4E, O => 0x4F, P => 0x50, Q => 0x51, R => 0x52,
            S => 0x53, T => 0x54, U => 0x55, V => 0x56, W => 0x57, X => 0x58,
            Y => 0x59, Z => 0x5A,
            Num0 => 0x30, Num1 => 0x31, Num2 => 0x32, Num3 => 0x33, Num4 => 0x34,
            Num5 => 0x35, Num6 => 0x36, Num7 => 0x37, Num8 => 0x38, Num9 => 0x39,
            F1 => 0x70, F2 => 0x71, F3 => 0x72, F4 => 0x73, F5 => 0x74, F6 => 0x75,
            F7 => 0x76, F8 => 0x77, F9 => 0x78, F10 => 0x79, F11 => 0x7A, F12 => 0x7B,
            Space => 0x20,
            _ => return None,
        };
        let mut mods = 0u32;
        if m.ctrl || m.command {
            mods |= 0x0002;
        }
        if m.alt {
            mods |= 0x0001;
        }
        if m.shift {
            mods |= 0x0004;
        }
        // A bare letter would swallow that key system-wide.
        let function_key = (0x70..=0x7B).contains(&vk);
        if mods == 0 && !function_key {
            return None;
        }
        Some((mods, vk))
    }

    /// Human-readable hotkey, e.g. "Strg + Alt + D".
    pub fn hotkey_label(&self) -> String {
        let mut parts = Vec::new();
        if self.hotkey_mods & 0x0002 != 0 {
            parts.push("Strg".to_string());
        }
        if self.hotkey_mods & 0x0001 != 0 {
            parts.push("Alt".to_string());
        }
        if self.hotkey_mods & 0x0004 != 0 {
            parts.push("Umschalt".to_string());
        }
        if self.hotkey_mods & 0x0008 != 0 {
            parts.push("Win".to_string());
        }
        parts.push(match self.hotkey_vk {
            0x30..=0x39 => ((b'0' + (self.hotkey_vk - 0x30) as u8) as char).to_string(),
            0x41..=0x5A => ((b'A' + (self.hotkey_vk - 0x41) as u8) as char).to_string(),
            0x70..=0x87 => format!("F{}", self.hotkey_vk - 0x6F),
            other => format!("VK{other:#04X}"),
        });
        parts.join(" + ")
    }
}

#[cfg(test)]
mod tests {
    use super::Settings;

    /// The shares list is the only field that is not a scalar, and it is what
    /// keeps a hand-added UNC path alive across restarts — so it has to survive
    /// a write/read round trip exactly, separators and all.
    #[test]
    fn shares_survive_the_file_format() {
        let mut s = Settings::default();
        s.shares = vec![
            r"\\fatboy\downloads".into(),
            r"\\server\share with spaces".into(),
            // An `=` in the value must not be mistaken for the separator.
            r"\\server\odd=name".into(),
        ];
        s.lang = "en".into();
        s.tile_px = 96;
        s.close_to_tray = false;

        let back = Settings::parse(&s.render());
        assert_eq!(back.shares, s.shares);
        assert_eq!(back.lang, "en");
        assert_eq!(back.tile_px, 96);
        assert!(!back.close_to_tray);
    }

    #[test]
    fn a_file_without_shares_yields_none() {
        let s = Settings::parse("lang=de\ntile_px=128\n");
        assert!(s.shares.is_empty());
        assert_eq!(s.tile_px, 128);
    }

    /// Every field has to come back, not just the ones a test happens to name.
    #[test]
    fn a_full_round_trip_changes_nothing() {
        let mut s = Settings::default();
        s.shares = vec![r"\\a\b".into()];
        s.lang = "en".into();
        s.hotkey_vk = 0x71;
        s.search_limit_k = 350;
        s.volume = 42;
        // Dragged column widths have to come back too, or the details view
        // resets its layout on every launch.
        s.sort_key = crate::store::SortKey::Path;
        s.sort_desc = true;
        s.start_first_drive = false;
        s.search_all_drives = true;
        s.col_name = 412.5;
        s.col_size = 77.0;
        s.col_date = 143.25;
        assert!(Settings::parse(&s.render()) == s);
    }
}
