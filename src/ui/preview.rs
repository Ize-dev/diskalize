//! Preview panel and the thumbnail cache behind it.
//!
//! Thumbnails come from the shell, so whatever Explorer can preview we can too:
//! images, video frames, PDF first pages, Office documents, album art. Requests
//! run on worker threads (each with its own COM apartment) and are uploaded to
//! the GPU on the UI thread, so a slow network file never stalls a frame.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use egui::{Align2, Color32, ColorImage, FontId, RichText, TextureHandle, TextureOptions, Vec2};
use parking_lot::{Condvar, Mutex};

use crate::fmt;
use crate::index::Index;
use crate::ui::theme;
use crate::winshell;

const CACHE_CAP: usize = 900;
const WORKERS: usize = 8;

type Key = (String, u32);

/// Work stack rather than a queue: while scrolling, the newest requests are the
/// ones on screen, so serving them last-in-first-out means the visible tiles
/// fill in immediately instead of waiting behind everything scrolled past.
#[derive(Default)]
struct Jobs {
    stack: Vec<Key>,
    closed: bool,
}

/// How long a freshly decoded thumbnail takes to fade up.
const FADE_SECS: f64 = 0.22;

pub struct Thumbs {
    cache: HashMap<Key, Option<TextureHandle>>,
    /// When each thumbnail landed, so it can fade in rather than pop. Scrolling
    /// a folder of images otherwise looks like the grid is flickering.
    ready: HashMap<Key, f64>,
    order: VecDeque<Key>,
    pending: HashSet<Key>,
    jobs: Arc<(Mutex<Jobs>, Condvar)>,
    res: Receiver<(Key, Option<winshell::Thumb>)>,
    _keep: Sender<(Key, Option<winshell::Thumb>)>,
}

impl Default for Thumbs {
    fn default() -> Self {
        Self::new()
    }
}

/// Requests are rounded to a few sizes so dragging the size slider reuses
/// cached images instead of re-fetching at every intermediate pixel width.
pub fn bucket(px: u32) -> u32 {
    match px {
        0..=48 => 48,
        49..=96 => 96,
        97..=160 => 160,
        161..=256 => 256,
        _ => 512,
    }
}

impl Thumbs {
    pub fn new() -> Self {
        let jobs: Arc<(Mutex<Jobs>, Condvar)> = Arc::default();
        let (res_tx, res_rx) = channel();

        for n in 0..WORKERS {
            let jobs = Arc::clone(&jobs);
            let tx = res_tx.clone();
            std::thread::Builder::new()
                .name(format!("thumb-{n}"))
                .spawn(move || {
                    winshell::init_com();
                    loop {
                        let key = {
                            let (lock, cv) = &*jobs;
                            let mut g = lock.lock();
                            loop {
                                if g.closed {
                                    return;
                                }
                                if let Some(k) = g.stack.pop() {
                                    break k;
                                }
                                cv.wait(&mut g);
                            }
                        };
                        let img = winshell::thumbnail(&key.0, key.1);
                        if tx.send((key, img)).is_err() {
                            return;
                        }
                    }
                })
                .ok();
        }

        Self {
            cache: HashMap::new(),
            ready: HashMap::new(),
            order: VecDeque::new(),
            pending: HashSet::new(),
            jobs,
            res: res_rx,
            _keep: res_tx,
        }
    }

    /// Uploads whatever the workers finished since the last frame.
    pub fn pump(&mut self, ctx: &egui::Context) {
        let mut got = false;
        let now = ctx.input(|i| i.time);
        while let Ok((key, img)) = self.res.try_recv() {
            self.pending.remove(&key);
            let tex = img.map(|t| {
                let px: Vec<Color32> = t
                    .rgba
                    .chunks_exact(4)
                    .map(|c| Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3]))
                    .collect();
                let image = ColorImage {
                    size: [t.w as usize, t.h as usize],
                    source_size: Vec2::new(t.w as f32, t.h as f32),
                    pixels: px,
                };
                ctx.load_texture(format!("thumb:{}:{}", key.0, key.1), image, TextureOptions::LINEAR)
            });
            self.ready.insert(key.clone(), now);
            if self.cache.insert(key.clone(), tex).is_none() {
                self.order.push_back(key);
            }
            got = true;
        }
        while self.order.len() > CACHE_CAP {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
                self.ready.remove(&old);
            }
        }
        if got {
            ctx.request_repaint();
        }
    }

    /// Cached texture for `path`, queueing a fetch on first miss.
    pub fn get(&mut self, path: &str, px: u32) -> Option<&TextureHandle> {
        let key = (path.to_string(), bucket(px));
        if !self.cache.contains_key(&key) && self.pending.insert(key.clone()) {
            let (lock, cv) = &*self.jobs;
            let mut g = lock.lock();
            // Bound the backlog: anything this old is far off screen by now.
            if g.stack.len() > 4096 {
                g.stack.drain(..2048);
            }
            g.stack.push(key.clone());
            cv.notify_one();
        }
        self.cache.get(&key).and_then(|t| t.as_ref())
    }

    /// How far along a thumbnail's fade-in is, 0..=1. Anything cached from an
    /// earlier frame is already at 1, so only newly arrived images animate.
    pub fn fade(&self, path: &str, px: u32, now: f64) -> f32 {
        match self.ready.get(&(path.to_string(), bucket(px))) {
            Some(t0) => (((now - t0) / FADE_SECS) as f32).clamp(0.0, 1.0),
            None => 1.0,
        }
    }
}

impl Drop for Thumbs {
    fn drop(&mut self) {
        let (lock, cv) = &*self.jobs;
        lock.lock().closed = true;
        cv.notify_all();
    }
}

/// Shell icons, cached per file type.
///
/// Keyed by extension rather than path: every `.mp4` looks the same, so a list
/// of a hundred thousand rows needs a handful of lookups. Cheap enough to fill
/// synchronously — no worker threads, no disk access.
#[derive(Default)]
pub struct Icons {
    cache: HashMap<(String, bool), Option<TextureHandle>>,
}

impl Icons {
    pub fn get(
        &mut self,
        ctx: &egui::Context,
        name: &[u8],
        is_dir: bool,
    ) -> Option<&TextureHandle> {
        let ext = if is_dir {
            String::new()
        } else {
            match name.iter().rposition(|&b| b == b'.') {
                Some(p) if p + 1 < name.len() => String::from_utf8_lossy(&name[p + 1..])
                    .to_ascii_lowercase(),
                _ => String::new(),
            }
        };
        let key = (ext, is_dir);
        if !self.cache.contains_key(&key) {
            let tex = crate::winshell::file_icon(&key.0, is_dir, true).map(|t| {
                let px: Vec<Color32> = t
                    .rgba
                    .chunks_exact(4)
                    .map(|c| Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3]))
                    .collect();
                let image = ColorImage {
                    size: [t.w as usize, t.h as usize],
                    source_size: Vec2::new(t.w as f32, t.h as f32),
                    pixels: px,
                };
                ctx.load_texture(
                    format!("icon:{}:{}", key.0, is_dir),
                    image,
                    TextureOptions::LINEAR,
                )
            });
            self.cache.insert(key.clone(), tex);
        }
        self.cache.get(&key).and_then(|t| t.as_ref())
    }
}

const TEXT_EXT: &[&str] = &[
    "txt", "log", "md", "json", "xml", "csv", "tsv", "ini", "cfg", "conf", "toml", "yaml", "yml",
    "rs", "py", "js", "mjs", "ts", "tsx", "jsx", "html", "htm", "css", "scss", "c", "h", "cpp",
    "hpp", "cs", "java", "go", "rb", "php", "sh", "bat", "cmd", "ps1", "sql", "srt", "vtt", "lrc",
    "nfo", "diz", "asc", "ans", "gitignore", "env", "properties", "gradle", "lua", "kt", "swift",
    "r", "m", "vb",
];

fn is_texty(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, ext)) => TEXT_EXT.iter().any(|e| e.eq_ignore_ascii_case(ext)),
        None => false,
    }
}

fn kind_label(name: &str, is_dir: bool) -> String {
    if is_dir {
        return crate::i18n::t("Ordner").into();
    }
    match name.rsplit_once('.') {
        Some((_, ext)) => crate::i18n::tf("{0}-Datei", &[&ext.to_uppercase()]),
        None => crate::i18n::t("Datei").into(),
    }
}

/// Enough to put playback back exactly where it was.
pub struct Resume {
    path: String,
    position: f32,
    was_playing: bool,
}

/// Playback state that survives between frames.
#[derive(Default)]
pub struct MediaState {
    player: Option<crate::media::Player_>,
    tex: Option<TextureHandle>,
    /// Seek/pause that could not be applied yet — libVLC ignores both until the
    /// media is opened and running.
    pending_seek: Option<f32>,
    pending_pause: bool,
    /// File that was silenced because the window left the screen. Autoplay skips
    /// it until the user asks again — bringing the window back should not blast
    /// the video at you from the start.
    suppressed: Option<String>,
    /// Previous preview, kept just long enough to fade out under the new one.
    fade_from: Option<TextureHandle>,
    fade_path: String,
    fade: f32,
    pub autoplay: bool,
    pub looping: bool,
    pub volume: i32,
    /// Cross-fade between previews instead of switching instantly.
    pub animate: bool,
    /// The content-search term, highlighted in the text preview.
    pub find: String,
}

impl MediaState {
    pub fn new(autoplay: bool, looping: bool, volume: i32) -> Self {
        Self {
            player: None,
            tex: None,
            pending_seek: None,
            pending_pause: false,
            suppressed: None,
            fade_from: None,
            fade_path: String::new(),
            fade: 1.0,
            autoplay,
            looping,
            volume,
            animate: true,
            find: String::new(),
        }
    }

    /// Releases a player that was already stopped from outside, and remembers
    /// not to start it again on its own.
    pub fn silence(&mut self) {
        if let Some(p) = &self.player {
            self.suppressed = Some(p.path.clone());
        }
        self.stop();
    }
    pub fn stop(&mut self) {
        self.player = None;
        self.tex = None;
        self.pending_seek = None;
        self.pending_pause = false;
    }
    /// Pushes `volume` to a player that is already running.
    pub fn apply_volume(&mut self) {
        if let Some(p) = &self.player {
            p.set_volume(self.volume);
        }
    }

    fn open(&mut self, path: &str) {
        self.player = crate::media::Player_::open(path, self.looping, self.volume);
        self.tex = None;
    }
    fn is(&self, path: &str) -> bool {
        self.player.as_ref().is_some_and(|p| p.path == path)
    }

    /// Lets go of `path` so the shell can act on it.
    ///
    /// A playing file is an open handle, and Windows will not delete, move or
    /// rename underneath one. Returns what is needed to pick up again if the
    /// operation turns out to be cancelled.
    pub fn release_for(&mut self, path: &str) -> Option<Resume> {
        let p = self.player.as_ref()?;
        if p.path != path {
            return None;
        }
        let resume = Resume {
            path: p.path.clone(),
            position: p.position(),
            was_playing: p.playing(),
        };
        self.stop();
        Some(resume)
    }

    /// Resumes what [`release_for`] gave up, at the same spot.
    pub fn restore(&mut self, resume: Option<Resume>) {
        let Some(r) = resume else { return };
        self.open(&r.path);
        if self.player.is_some() {
            self.pending_seek = Some(r.position);
            self.pending_pause = !r.was_playing;
        }
    }

    /// Applies a pending seek/pause once playback has actually started.
    fn settle(&mut self) {
        let Some(p) = self.player.as_ref() else { return };
        if !p.playing() {
            return;
        }
        if let Some(pos) = self.pending_seek.take() {
            p.seek(pos);
        }
        if std::mem::take(&mut self.pending_pause) {
            p.toggle();
        }
    }
}

/// Right-hand detail pane for one entry.
pub fn show(
    ui: &mut egui::Ui,
    ix: &Index,
    idx: u32,
    thumbs: &mut Thumbs,
    upscale: &mut bool,
    media: &mut MediaState,
    // Render text files as text rather than as a blown-up generic icon.
    show_text: bool,
) {
    let name = ix.name(idx).to_string();
    let path = ix.path_of(idx);
    let is_dir = ix.is_dir(idx);

    ui.add_space(4.0);
    ui.label(RichText::new(&name).size(13.5).strong());
    ui.label(RichText::new(&path).size(10.5).color(theme::TEXT_DIM));
    ui.add_space(6.0);

    let is_text = show_text && !is_dir && is_texty(&name);
    let kind = if is_dir {
        crate::media::Kind::Other
    } else {
        crate::media::kind_of(&name)
    };
    let playable = kind != crate::media::Kind::Other && crate::media::available();

    media.settle();

    // Picking a different file clears the suppression — it only ever applied to
    // the one that was silenced.
    if media.suppressed.as_deref() != Some(path.as_str()) {
        media.suppressed = None;
    }

    // Selecting something else must not leave the previous file playing.
    if !playable || !media.is(&path) {
        if media.player.is_some() && !media.is(&path) {
            media.stop();
        }
        let hold_off = media.suppressed.as_deref() == Some(path.as_str());
        if playable && media.autoplay && !hold_off && media.player.is_none() {
            media.open(&path);
        }
    }

    // Metadata and actions dock to the bottom edge, so the preview above them
    // gets every remaining pixel instead of stopping halfway down the panel.
    egui::Panel::bottom("preview_meta")
        .frame(egui::Frame::default().inner_margin(egui::Margin {
            left: 0,
            right: 0,
            top: 6,
            bottom: 0,
        }))
        .show(ui, |ui| {
            egui::Grid::new("preview_meta_grid")
                .num_columns(2)
                .spacing([10.0, 3.0])
                .show(ui, |ui| {
                    let mut row = |k: &str, v: String| {
                        ui.label(RichText::new(k).size(11.0).color(theme::TEXT_DIM));
                        ui.label(RichText::new(v).size(11.0));
                        ui.end_row();
                    };
                    row(crate::i18n::t("Typ"), kind_label(&name, is_dir));
                    row(crate::i18n::t("Größe"), fmt::size(ix.size[idx as usize]));
                    if !is_dir {
                        row(
                            crate::i18n::t("Logisch"),
                            fmt::size(ix.logical[idx as usize]),
                        );
                    } else {
                        row(
                            crate::i18n::t("Dateien"),
                            fmt::count(ix.files[idx as usize] as u64),
                        );
                    }
                    row(
                        crate::i18n::t("Geändert"),
                        fmt::timestamp(ix.mtime[idx as usize]),
                    );
                });
            if playable {
                transport(ui, media, &path);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(crate::i18n::t("Öffnen")).clicked() {
                    crate::shell::open(&path);
                }
                if ui.button(crate::i18n::t("Im Explorer")).clicked() {
                    crate::shell::reveal(&path);
                }
                if !is_text {
                    ui.checkbox(upscale, crate::i18n::t("Füllen")).on_hover_text(
                        "Kleine Bilder auf die Panelgröße vergrößern (Seitenverhältnis bleibt)",
                    );
                }
            });
            ui.add_space(2.0);
        });

    // A text file's content is the preview. Showing a blown-up generic file
    // icon instead was pointless.
    if is_text {
        let head = read_head(&path, &name, 64 * 1024);
        let art = crate::ui::cp437::is_dos_art(&name);
        // Art is laid out by column, so it must not be highlighted (colouring
        // "keywords" inside a picture is nonsense) and must not be wrapped.
        let lang = if art {
            crate::ui::syntax::Lang::Plain
        } else {
            crate::ui::syntax::lang_of(&name)
        };
        // Where the content search matched, so the hit is visible without
        // hunting for it. Empty for everything else.
        let marks = crate::content::ranges(&head, &media.find);
        let mut job = crate::ui::syntax::layout(
            &head,
            lang,
            if art { crate::ui::syntax::ART_SIZE } else { 11.0 },
            &marks,
        );
        // No wrapping either way: a wrapped line of box drawing is unreadable,
        // and code reads better with a horizontal scrollbar than with reflow.
        job.wrap.max_width = f32::INFINITY;
        egui::Frame::default()
            .fill(if art { theme::BG } else { theme::PANEL })
            .corner_radius(6)
            .inner_margin(8)
            .show(ui, |ui| {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(egui::Label::new(job));
                    });
            });
        return;
    }

    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, egui::CornerRadius::same(6), theme::PANEL);

    if is_dir {
        p.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Ordner",
            FontId::proportional(13.0),
            theme::TEXT_DIM,
        );
        return;
    }

    // A running player owns the preview area; the shell's still frame is only
    // the stand-in until the first decoded frame arrives.
    if media.player.is_some() {
        let fresh = media.player.as_mut().and_then(|mp| mp.take_frame());
        if let Some((w, h, rgba)) = fresh {
            let image = ColorImage {
                size: [w as usize, h as usize],
                source_size: Vec2::new(w as f32, h as f32),
                pixels: rgba
                    .chunks_exact(4)
                    .map(|c| Color32::from_rgba_premultiplied(c[0], c[1], c[2], c[3]))
                    .collect(),
            };
            media.tex = Some(ui.ctx().load_texture("video", image, TextureOptions::LINEAR));
        }
        if let Some(tex) = &media.tex {
            let ts = tex.size_vec2();
            let fit = (rect.width() / ts.x).min(rect.height() / ts.y);
            p.image(
                tex.id(),
                egui::Rect::from_center_size(rect.center(), ts * fit),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            // Decoding runs on VLC's threads, so keep asking for frames.
            ui.ctx().request_repaint();
            return;
        }
        // Audio, or video that has not produced a frame yet.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(120));
    }

    // Switching files cross-fades rather than cutting: the outgoing image is
    // held for one short transition so the pane does not flicker to empty.
    if media.fade_path != path {
        media.fade_from = thumbs.get(&media.fade_path.clone(), want_px(rect)).cloned();
        media.fade_path = path.clone();
        media.fade = if media.animate { 0.0 } else { 1.0 };
    }
    if media.fade < 1.0 {
        media.fade = (media.fade + ui.input(|i| i.stable_dt) / 0.18).min(1.0);
        ui.ctx().request_repaint();
    }

    let fit_into = |ts: Vec2| {
        let fit = (rect.width() / ts.x).min(rect.height() / ts.y);
        let scale = if *upscale { fit } else { fit.min(1.0) };
        egui::Rect::from_center_size(rect.center(), ts * scale)
    };
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

    if media.fade < 1.0 {
        if let Some(old) = &media.fade_from {
            p.image(
                old.id(),
                fit_into(old.size_vec2()),
                uv,
                Color32::WHITE.gamma_multiply(1.0 - media.fade),
            );
        }
    }

    match thumbs.get(&path, want_px(rect)) {
        Some(tex) => {
            // Fit inside the pane; only shrink unless upscaling is allowed, and
            // never distort — the file's aspect ratio survives either way.
            p.image(
                tex.id(),
                fit_into(tex.size_vec2()),
                uv,
                Color32::WHITE.gamma_multiply(media.fade),
            );
        }
        None => {
            p.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "…",
                FontId::proportional(16.0),
                theme::TEXT_DIM,
            );
        }
    }
}

fn want_px(rect: egui::Rect) -> u32 {
    rect.width().max(rect.height()).max(256.0) as u32
}

/// Play/pause, seek bar, loop, autoplay and volume.
fn transport(ui: &mut egui::Ui, media: &mut MediaState, path: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let running = media.player.as_ref().is_some_and(|p| p.playing());
        let label = if media.player.is_none() {
            "▶"
        } else if running {
            "⏸"
        } else {
            "▶"
        };
        if ui.add_sized([30.0, 22.0], egui::Button::new(label)).clicked() {
            // An explicit press overrides the hold-off from being silenced.
            media.suppressed = None;
            match &media.player {
                None => media.open(path),
                Some(p) => p.toggle(),
            }
        }
        if ui
            .add_sized([30.0, 22.0], egui::Button::new("⏹"))
            .on_hover_text(crate::i18n::t("Stopp"))
            .clicked()
        {
            media.stop();
        }

        let (cur, total) = media
            .player
            .as_ref()
            .map_or((0, -1), |p| p.times());
        ui.label(
            RichText::new(format!(
                "{} / {}",
                crate::media::fmt_time(cur),
                crate::media::fmt_time(total)
            ))
            .size(10.5)
            .color(theme::TEXT_DIM),
        );

        let mut pos = media.player.as_ref().map_or(0.0, |p| p.position());
        let slider = ui.add_enabled(
            media.player.is_some(),
            egui::Slider::new(&mut pos, 0.0..=1.0).show_value(false),
        );
        if slider.changed() {
            if let Some(p) = &media.player {
                p.seek(pos);
            }
        }
    });
    ui.horizontal(|ui| {
        if ui
            .checkbox(&mut media.looping, crate::i18n::t("Endlos"))
            .on_hover_text(crate::i18n::t("Gilt ab dem nächsten Start"))
            .changed()
        {
            // libVLC takes the repeat count as a media option, so a running
            // player has to be reopened for the change to take.
            if media.player.is_some() {
                media.open(path);
            }
        }
        ui.checkbox(&mut media.autoplay, crate::i18n::t("Autoplay"))
            .on_hover_text(crate::i18n::t("Ausgewählte Medien sofort abspielen"));
        ui.label(RichText::new("🔊").size(11.0));
        let mut vol = media.volume;
        if ui
            .add(egui::Slider::new(&mut vol, 0..=100).show_value(false))
            .changed()
        {
            media.volume = vol;
            if let Some(p) = &media.player {
                p.set_volume(vol);
            }
        }
    });
}

fn read_head(path: &str, name: &str, max: usize) -> String {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return crate::i18n::t("(nicht lesbar)").into();
    };
    let mut buf = vec![0u8; max];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    // Drop a UTF-16 BOM's worth of noise rather than showing mojibake.
    if buf.starts_with(&[0xFF, 0xFE]) || buf.starts_with(&[0xFE, 0xFF]) {
        return crate::i18n::t("(UTF-16-Datei)").into();
    }
    if crate::ui::cp437::is_dos_art(name) {
        // Not UTF-8 at all: every byte is one code page 437 character.
        return crate::ui::cp437::decode(&buf).replace('\r', "");
    }
    // A UTF-8 BOM would otherwise show up as a stray character on line one.
    let body = buf.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&buf);
    String::from_utf8_lossy(body).replace('\r', "")
}

#[cfg(test)]
mod tests {
    /// The DOS-art extensions have to reach the text branch at all, or the
    /// code page handling below it never runs.
    #[test]
    fn dos_art_counts_as_text() {
        for name in ["release.nfo", "file_id.diz", "art.ans", "logo.asc"] {
            assert!(super::is_texty(name), "{name}");
            assert!(crate::ui::cp437::is_dos_art(name), "{name}");
        }
        assert!(super::is_texty("notes.txt"));
        assert!(!crate::ui::cp437::is_dos_art("notes.txt"));
    }
}
