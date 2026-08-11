//! Application shell: state, background jobs and layout.
//!
//! Two rules keep the UI at full framerate:
//!   * Scanning and searching never run on the UI thread — they hand results back
//!     over channels, and the indexes live behind `RwLock`s the UI only ever
//!     read-locks for the few microseconds it takes to paint.
//!   * Nothing expensive is recomputed per frame. Chart layouts, the flattened
//!     tree and search results are all keyed on `Index::generation`.

use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;


use egui::{Align, Layout, RichText, Vec2};
use parking_lot::RwLock;

use crate::client;
use crate::fmt;
use crate::index::{Index, NONE};
use crate::scan::Target;
use crate::search;
use std::cell::RefCell;
use std::rc::Rc;

use crate::i18n::{t, tf};
use crate::shell;
use crate::store::{self, Hit, SortKey, Store, Volume};
use crate::tray::{Tray, TrayEvent};
use crate::ui::{preview, sunburst, theme, tree, treemap};
use crate::win::{self, DriveInfo};
use crate::winshell;

/// A path that names a share directly rather than through a drive letter.
fn is_unc(p: &str) -> bool {
    p.starts_with(r"\\") && !p.starts_with(r"\\?\")
}

/// The sort keys as the interface names them.
fn sort_label(k: SortKey) -> &'static str {
    match k {
        SortKey::Size => t("Größe"),
        SortKey::Name => t("Name"),
        SortKey::Date => t("Geändert"),
        SortKey::Path => t("Pfad"),
    }
}

/// Headed block of related settings.
fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).size(12.5).strong().color(theme::ACCENT));
    ui.add_space(2.0);
    egui::Frame::default()
        .fill(theme::PANEL)
        .corner_radius(6)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        });
}

/// Labelled slider with the value spelled out; returns true when it moved.
fn slider(
    ui: &mut egui::Ui,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    label: &str,
    unit: &str,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(11.5));
        changed = ui
            .add(egui::Slider::new(value, range).show_value(false))
            .changed();
        ui.label(
            RichText::new(format!("{value} {unit}").trim().to_string())
                .size(11.0)
                .color(theme::TEXT_DIM),
        );
    });
    changed
}

#[derive(PartialEq, Clone, Copy)]
enum SettingsTab {
    View,
    Search,
    Media,
    Shell,
    Service,
    About,
}

/// Build metadata, stamped in by `build.rs`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GIT_REV: &str = env!("DKZ_GIT_REV");
pub const BUILD_UNIX: &str = env!("DKZ_BUILD_UNIX");

/// How many frame times the About page keeps for its graph. At 60 Hz that is
/// the last two seconds — long enough to catch a stutter, short enough that the
/// number still reflects what the window is doing right now.
/// What a content search is for: the term, the name query behind it, whether it
/// spans every drive, and which folder it started from. Anything else changing
/// — the index, the sort, another volume finishing — must not restart it.
type FindKey = (String, String, bool, u32);

const FRAME_SAMPLES: usize = 120;

/// Viewport ids have to stay unique for the life of the process.
fn next_window_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Files a single content search will open. Reading is the expensive part, and
/// a question that needs more than this wants narrowing, not patience.
const MAX_CONTENT_FILES: usize = 20_000;

/// Quiet time before a content search starts, so typing does not launch one
/// pass per keystroke.
const FIND_DELAY: std::time::Duration = std::time::Duration::from_millis(350);

const CREDITS: &[&str] = &[
    "egui",
    "eframe",
    "wgpu",
    "rayon",
    "parking_lot",
    "windows-rs",
    "windows-sys",
    "memchr",
    "regex",
    "rfd",
    "libloading",
    "raw-window-handle",
    "winresource",
    "libVLC (VideoLAN)",
];

/// One figure with its caption, for the row of numbers on the About page.
fn stat(ui: &mut egui::Ui, label: &str, value: &str, colour: egui::Color32) {
    ui.vertical(|ui| {
        ui.label(RichText::new(value).size(15.0).color(colour).monospace());
        ui.label(RichText::new(label).size(10.0).color(theme::TEXT_DIM));
    });
    ui.add_space(10.0);
}

fn build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "Debug"
    } else {
        "Release"
    }
}

/// The service binary sits next to the GUI.
fn service_exe() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.with_file_name("diskalize-service.exe");
    p.exists().then_some(p)
}

/// The four peer views of the central area. Details and tiles work with or
/// without an active search — without one they list the current folder.
#[derive(PartialEq, Clone, Copy)]
enum View {
    Sunburst,
    Treemap,
    Details,
    Tiles,
}

impl View {
    fn is_list(self) -> bool {
        matches!(self, View::Details | View::Tiles)
    }
    fn list_mode(self) -> tree::ListMode {
        match self {
            View::Tiles => tree::ListMode::Tiles,
            _ => tree::ListMode::Details,
        }
    }
}

pub struct App {
    drives: Vec<DriveInfo>,
    store: Store,

    /// Connection to the indexing service; all scanning happens over there.
    svc: client::Client,
    svc_volumes: Vec<crate::ipc::VolumeMsg>,
    pending_activate: Option<String>,
    /// Folder handed over by Explorer, applied once its volume is loaded.
    pending_path: Option<String>,
    scanning: Option<String>,
    notice: Option<(String, bool)>,
    service_busy: Option<(String, bool)>,

    view: View,
    chart: sunburst::ChartState,
    map: treemap::MapState,
    view_root: u32,
    history: Vec<u32>,
    /// Places we stepped back from, for the forward button.
    forward: Vec<u32>,
    tree: tree::TreeState,

    query: String,
    query_dirty: bool,
    search_rx: Option<Receiver<(u64, store::Results)>>,
    search_gen: u64,
    global_search: bool,
    results: Vec<Hit>,
    result_info: Option<(usize, u128, bool)>,
    sort: SortKey,
    sort_desc: bool,
    sel_hit: Option<Hit>,
    show_preview: bool,
    show_tree: bool,
    /// View mode plus active volume, watched for a fade when either changes.
    swap_key: (u8, usize),
    swap_t: f32,
    /// Ring of recent frame times in milliseconds, for the About page graph.
    frames: [f32; FRAME_SAMPLES],
    frame_i: usize,
    search_focused: bool,
    focus_search: bool,
    scroll_to: Option<usize>,
    /// Children of the current folder, used when a list view is active without
    /// a search. Rebuilt only when the folder, sort or index generation changes.
    browse: Vec<Hit>,
    browse_key: (u32, u64, usize, bool),

    /// Shared across windows: the same file looks the same in all of them, and
    /// a second cache of 512-pixel textures is real video memory.
    thumbs: Rc<RefCell<preview::Thumbs>>,
    icons: Rc<RefCell<preview::Icons>>,
    media: preview::MediaState,
    tray: Option<Tray>,
    ipc: Receiver<String>,
    icon_set: bool,
    net_path: String,
    show_help: bool,
    /// The first window. Extra ones are viewports inside the same process and
    /// must keep their hands off everything there is only one of: the tray, the
    /// global hotkey, the handoff pipe, the taskbar window itself.
    root: bool,
    /// Extra windows, drawn as immediate viewports. Empty in a non-root window.
    extra: Vec<App>,
    /// Distinguishes this window's viewport from its siblings. Reusing an id
    /// after a window closes would hand the new one the old one's geometry.
    window_id: u64,
    /// Set by the settings button; acted on outside the borrow it happens in.
    open_window: bool,
    /// The active volume was chosen by us, not by the user, so a better
    /// candidate arriving later may still replace it.
    auto_selected: bool,
    /// Windows still to open, from `--windows N`.
    pending_windows: u32,
    /// A UNC share being walked in this process. See `scan_share`.
    share_scan: Option<(Receiver<(String, Result<Index, String>)>, Arc<crate::scan::Progress>)>,
    /// Shares already attempted this session, so a dead server is asked once.
    shares_tried: std::collections::HashSet<String>,
    /// Details-view column widths, dragged by the user.
    columns: tree::Columns,
    /// Text to look for *inside* the files the name query returned.
    find_text: String,
    /// What the running (or finished) content search was started for. Live
    /// index changes arrive constantly on a busy drive and re-run the name
    /// query; without this they would also restart a search that reads
    /// thousands of files, over and over, and the list would never settle.
    find_kicked_for: Option<FindKey>,
    /// What the debounce timer is waiting to start.
    find_pending_for: Option<FindKey>,
    find_rx: Option<Receiver<(u64, Vec<(Hit, String)>)>>,
    find_gen: u64,
    find_progress: Option<Arc<crate::content::Progress>>,
    /// One line of context per hit, keyed by the entry it belongs to.
    find_lines: std::collections::HashMap<Hit, String>,
    /// The same hits in list order. Sorting them costs milliseconds at this
    /// size, which is fine once but ruinous on every frame — so it is done
    /// when the results or the sort change, and never in the draw path.
    find_sorted: Vec<Hit>,
    find_sorted_for: (SortKey, bool, bool, usize),
    /// More candidates than `MAX_CONTENT_FILES`, so only the first were read.
    find_truncated: bool,
    /// When the search may start. Typing retriggers it, and opening thousands
    /// of files per keystroke helps nobody.
    find_after: Option<std::time::Instant>,
    show_settings: bool,
    settings_was_open: bool,
    settings_tab: SettingsTab,
    capturing_hotkey: bool,
    cfg: crate::settings::Settings,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        initial: Option<String>,
        ipc: Receiver<String>,
        // Only the first window owns the notification icon and the hotkey.
        primary: bool,
        // Extra windows to open right away, from `--windows N`.
        extra_windows: u32,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        winshell::init_com();
        shell::repair_defaults();
        shell::repair_autostart();
        let cfg = crate::settings::Settings::load();
        // Before anything draws: every label goes through `t` from here on.
        crate::i18n::set(&cfg.lang);

        // Publish the real window handle for the tray and IPC threads.
        if let Ok(h) = raw_window_handle::HasWindowHandle::window_handle(cc) {
            if let raw_window_handle::RawWindowHandle::Win32(w) = h.as_raw() {
                win::set_main_window(w.hwnd.get() as usize);
            }
        }
        // Playback must not outlive the window being on screen, and neither the
        // render loop nor the message hook can be relied on once it is gone.
        crate::media::watch_window_visibility();

        let tray = primary
            .then(|| {
                Tray::new(
                    "Diskalize",
                    cc.egui_ctx.clone(),
                    cfg.hotkey_enabled
                        .then_some((cfg.hotkey_mods, cfg.hotkey_vk)),
                )
            })
            .flatten();
        let mut app = Self::build(&cc.egui_ctx, cfg, ipc, tray, true);
        app.pending_windows = extra_windows;

        // The service indexes every fixed drive on its own; a path handed over
        // by Explorer only says where to go once that drive's snapshot lands.
        if let Some(p) = initial {
            app.request_path(p);
        }
        app
    }

    /// Everything a window needs, root or not.
    ///
    /// Extra windows are viewports in this same process, so they share the
    /// graphics device, the font atlas and — through `spawn_window` — the
    /// thumbnail cache. What stays private is only what makes a window a
    /// separate view: where it is looking, what it has selected, what it is
    /// searching for.
    fn build(
        ctx: &egui::Context,
        cfg: crate::settings::Settings,
        ipc: Receiver<String>,
        tray: Option<Tray>,
        root: bool,
    ) -> Self {
        Self {
            drives: win::list_drives(),
            store: Store::default(),
            svc: client::spawn(ctx.clone()),
            svc_volumes: Vec::new(),
            pending_activate: None,
            pending_path: None,
            scanning: None,
            notice: None,
            service_busy: None,
            view: View::Sunburst,
            chart: sunburst::ChartState::default(),
            map: treemap::MapState::default(),
            view_root: NONE,
            history: Vec::new(),
            forward: Vec::new(),
            tree: tree::TreeState::default(),
            query: String::new(),
            query_dirty: false,
            search_rx: None,
            search_gen: 0,
            global_search: cfg.search_all_drives,
            results: Vec::new(),
            result_info: None,
            sort: cfg.sort_key,
            sort_desc: cfg.sort_desc,
            sel_hit: None,
            show_preview: true,
            show_tree: true,
            swap_key: (u8::MAX, usize::MAX),
            swap_t: 1.0,
            frames: [0.0; FRAME_SAMPLES],
            frame_i: 0,
            search_focused: false,
            focus_search: true,
            scroll_to: None,
            browse: Vec::new(),
            browse_key: (u32::MAX, 0, 0, false),
            thumbs: Rc::new(RefCell::new(preview::Thumbs::new())),
            icons: Rc::new(RefCell::new(preview::Icons::default())),
            media: preview::MediaState::new(cfg.autoplay, cfg.loop_media, cfg.volume),
            tray,
            ipc,
            icon_set: false,
            net_path: String::new(),
            show_help: false,
            root,
            extra: Vec::new(),
            window_id: next_window_id(),
            open_window: false,
            auto_selected: true,
            pending_windows: 0,
            share_scan: None,
            shares_tried: std::collections::HashSet::new(),
            columns: tree::Columns {
                name: cfg.col_name,
                size: cfg.col_size,
                date: cfg.col_date,
            },
            find_text: String::new(),
            find_kicked_for: None,
            find_pending_for: None,
            find_rx: None,
            find_gen: 0,
            find_progress: None,
            find_lines: std::collections::HashMap::new(),
            find_sorted: Vec::new(),
            find_sorted_for: (SortKey::Size, false, false, usize::MAX),
            find_truncated: false,
            find_after: None,
            show_settings: false,
            settings_was_open: false,
            settings_tab: SettingsTab::View,
            capturing_hotkey: false,
            cfg,
        }
    }

    /// Navigates to `path`, waiting for its volume if the index is not in yet.
    ///
    /// A folder is *not* a scan target: it already lives inside its drive's
    /// index. Treating it as one is what made the first launch from the context
    /// menu show whatever was open last instead of the folder that was clicked.
    fn request_path(&mut self, path: String) {
        let p = path.trim().trim_matches('"').to_string();
        if p.is_empty() {
            return;
        }
        // UNC paths have no drive letter and are indexed in their own right.
        if p.starts_with(r"\\") {
            let t = self.target_for(&p);
            self.pending_activate = Some(t.key());
            self.open_target(t);
            return;
        }
        let Some(letter) = p.chars().next().filter(|c| c.is_ascii_alphabetic()) else {
            return;
        };
        let key = format!("{}:", letter.to_ascii_uppercase());
        self.pending_path = Some(p);
        self.pending_activate = Some(key.clone());
        if let Some(slot) = self.store.find(&key) {
            self.activate(slot);
            self.apply_pending_path();
        }
    }

    /// Jumps to the folder a pending path points at, once its volume is loaded.
    fn apply_pending_path(&mut self) {
        let Some(p) = self.pending_path.clone() else {
            return;
        };
        let Some(index) = self.active_index().cloned() else {
            return;
        };
        let node = index.read().node_for_path(&p);
        let Some(node) = node else { return };
        self.pending_path = None;

        let is_dir = index.read().is_dir(node);
        self.history.clear();
        self.forward.clear();
        if is_dir {
            self.view_root = node;
        } else {
            // A file: show its folder and select it.
            let parent = index.read().parent[node as usize];
            if parent != NONE {
                self.view_root = parent;
            }
            if let Some(slot) = self.store.active {
                self.sel_hit = Some(Hit {
                    vol: slot as u16,
                    idx: node,
                });
            }
        }
        self.tree.expand_to(&index.read(), node);
        self.chart.invalidate();
        self.map.invalidate();
        self.browse_key = (u32::MAX, 0, 0, false);
    }

    /// A drive root from the Explorer context menu should take the fast MFT path,
    /// not the generic directory walker.
    fn target_for(&self, path: &str) -> Target {
        let trimmed = path.trim_end_matches('\\');
        if trimmed.len() == 2 && trimmed.as_bytes()[1] == b':' {
            let letter = trimmed.as_bytes()[0].to_ascii_uppercase() as char;
            if let Some(d) = self.drives.iter().find(|d| d.letter == letter) {
                return Target::Drive(d.clone());
            }
        }
        Target::Path(path.to_string())
    }

    // ---- background jobs -----------------------------------------------------

    /// Switches to an already-indexed volume, or scans it if we have not seen it.
    fn open_target(&mut self, target: Target) {
        // From here on the user has chosen; stop second-guessing them when
        // another volume finishes loading.
        self.auto_selected = false;
        if let Target::Path(p) = &target {
            // `\\SERVER` is a computer, not something with a filesystem — the
            // shares below it are what can actually be walked.
            let rest = p.trim_start_matches('\\').trim_end_matches('\\');
            if p.starts_with(r"\\") && !rest.contains('\\') {
                self.notice = Some((
                    tf("„{0}“ ist ein Server. Bitte eine Freigabe wählen, z. B. {0}\\daten", &[p]),
                    true,
                ));
                return;
            }
        }
        match self.store.find(&target.key()) {
            Some(i) => self.activate(i),
            None => self.start_scan(target),
        }
    }

    fn activate(&mut self, slot: usize) {
        if self.store.active == Some(slot) {
            return;
        }
        self.store.active = Some(slot);
        // Volumes arrive as empty placeholders; the data is fetched on demand.
        let key = self.store.vols[slot].key.clone();
        self.svc.send(client::Cmd::Load(key));
        let ix = self.store.vols[slot].index.read();
        let root = if ix.is_ready() { ix.root } else { NONE };
        drop(ix);
        self.view_root = root;
        self.history.clear();
        self.tree.reset();
        self.tree.expanded.insert(root);
        self.chart.invalidate();
        self.map.invalidate();
        if !self.query.trim().is_empty() && !self.global_search {
            self.query_dirty = true;
        }
    }

    /// Scanning normally lives in the service. UNC shares are the exception —
    /// see `scan_share`.
    fn start_scan(&mut self, target: Target) {
        self.notice = None;
        match &target {
            Target::Drive(d) => self.svc.send(client::Cmd::Rescan(format!("{}:", d.letter))),
            Target::Path(p) if is_unc(p) => {
                self.scan_share(p.clone());
                self.scanning = Some(target.label());
                return;
            }
            Target::Path(p) => self.svc.send(client::Cmd::AddPath(p.clone())),
        }
        self.scanning = Some(target.label());
    }

    /// Re-walks the shares the user added in an earlier session.
    ///
    /// They cannot live in the service — it has no credentials for them — so
    /// without this they would silently disappear on every restart. Only one
    /// walk runs at a time; the rest follow as each finishes.
    fn restore_shares(&mut self) {
        if self.share_scan.is_some() {
            return;
        }
        // Once per session, whatever the outcome. Retrying on failure would
        // hammer an unreachable server on every single frame, and the entry
        // stays in the settings either way so the next launch tries again.
        let next = self
            .cfg
            .shares
            .iter()
            .find(|p| {
                !self.shares_tried.contains(*p)
                    && self.store.find(&Target::Path((*p).clone()).key()).is_none()
            })
            .cloned();
        if let Some(p) = next {
            self.shares_tried.insert(p.clone());
            self.scan_share(p);
        }
    }

    /// Decides whether a content search should start, and when.
    ///
    /// Two things it deliberately does not react to: a name query re-running
    /// because the live index changed, and every keystroke. The first happens
    /// constantly on a drive that is being written to, the second happens while
    /// the term is still half typed — and each one would mean opening thousands
    /// of files again.
    fn maybe_kick_find(&mut self, ctx: &egui::Context) {
        let needle = self.find_text.trim().to_string();
        if needle.is_empty() {
            if self.find_kicked_for.is_some() || self.find_pending_for.is_some() {
                self.clear_find();
            }
            return;
        }
        let want: FindKey = (
            needle,
            self.query.trim().to_string(),
            self.global_search,
            self.view_root,
        );
        if self.find_kicked_for.as_ref() == Some(&want) {
            return;
        }
        // A name query still running is about to change the candidate set.
        if self.search_rx.is_some() {
            ctx.request_repaint_after(FIND_DELAY);
            return;
        }
        if self.find_pending_for.as_ref() != Some(&want) {
            self.find_pending_for = Some(want);
            self.find_after = Some(std::time::Instant::now() + FIND_DELAY);
            ctx.request_repaint_after(FIND_DELAY);
            return;
        }
        if self.find_after.is_some_and(|t| std::time::Instant::now() >= t) {
            self.find_after = None;
            self.find_kicked_for = self.find_pending_for.take();
            self.kick_find();
        } else {
            ctx.request_repaint_after(FIND_DELAY);
        }
    }

    /// Drops everything a content search produced and stops one in flight.
    fn clear_find(&mut self) {
        if let Some(p) = self.find_progress.take() {
            p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.find_rx = None;
        self.find_lines.clear();
        self.find_sorted.clear();
        self.find_sorted_for = (SortKey::Size, false, false, usize::MAX);
        self.find_kicked_for = None;
        self.find_pending_for = None;
        self.find_after = None;
        self.find_truncated = false;
        self.media.find.clear();
    }

    /// Starts a content search over whatever the name query produced.
    ///
    /// Reading files is orders of magnitude slower than scanning the name
    /// index, so this never walks the disk itself: it only opens the entries
    /// already on screen. Narrowing with `type:code` or a folder first is what
    /// makes it quick, and it is also what the user was going to do anyway.
    fn kick_find(&mut self) {
        // Retire whatever was running; its results are for an older query.
        if let Some(p) = self.find_progress.take() {
            p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.find_rx = None;
        let needle = self.find_text.trim().to_string();
        self.media.find = needle.clone();
        if needle.is_empty() {
            return;
        }

        let mut hits = self.candidate_hits();
        if hits.is_empty() {
            return;
        }
        // Every candidate is a file that has to be opened and read. Past a
        // point that is minutes of disk work for a question the user can ask
        // more precisely, so stop and say so rather than grind.
        self.find_truncated = hits.len() > MAX_CONTENT_FILES;
        hits.truncate(MAX_CONTENT_FILES);
        let vols = self.store.snapshot(None);

        self.find_gen += 1;
        let generation = self.find_gen;
        let progress = Arc::new(crate::content::Progress::default());
        self.find_progress = Some(Arc::clone(&progress));
        let limit = self.cfg.search_limit_k as usize * 1000;
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("content-search".into())
            .spawn(move || {
                // Resolving paths belongs here, not on the UI thread: it walks
                // the tree once per entry and holds a read lock while it does.
                let paths: Vec<String> = hits
                    .iter()
                    .map(|h| {
                        vols.iter()
                            .find(|(s, _)| *s == h.vol)
                            .map(|(_, ix)| ix.read().path_of(h.idx))
                            .unwrap_or_default()
                    })
                    .collect();
                let found = crate::content::search(&paths, &needle, limit, &progress);
                let out: Vec<(Hit, String)> = found
                    .into_iter()
                    .filter_map(|f| hits.get(f.which).map(|h| (*h, f.line)))
                    .collect();
                let _ = tx.send((generation, out));
            })
            .ok();
        self.find_rx = Some(rx);
    }

    /// The rows currently listed, whether they came from a search or from
    /// browsing a folder.
    fn candidate_hits(&self) -> Vec<Hit> {
        let vols = self.store.snapshot(if self.global_search {
            None
        } else {
            self.store.active
        });
        // Where to start: a name query gives its hits, otherwise the folder on
        // screen. Either way folders are starting points, not candidates —
        // searching "kingpin" matches the *folder*, and the text being looked
        // for is in the files below it.
        let roots: Vec<Hit> = if self.query.trim().is_empty() {
            let Some(slot) = self.store.active else {
                return Vec::new();
            };
            let Some(index) = self.active_index() else {
                return Vec::new();
            };
            let ix = index.read();
            if !ix.is_ready() {
                return Vec::new();
            }
            let root = if self.view_root == NONE || self.view_root as usize >= ix.len() {
                ix.root
            } else {
                self.view_root
            };
            vec![Hit {
                vol: slot as u16,
                idx: root,
            }]
        } else {
            self.results.clone()
        };

        let mut out: Vec<Hit> = Vec::new();
        let mut seen: std::collections::HashSet<Hit> = std::collections::HashSet::new();
        for (vol, index) in &vols {
            let ix = index.read();
            if !ix.is_ready() {
                continue;
            }
            for h in roots.iter().filter(|h| h.vol == *vol) {
                if (h.idx as usize) >= ix.len() {
                    continue;
                }
                if ix.is_dir(h.idx) {
                    for idx in search::subtree(&ix, h.idx) {
                        if !ix.is_dir(idx) && ix.live(idx) {
                            let hit = Hit { vol: *vol, idx };
                            // A file can be reached both directly and through a
                            // folder above it; reading it twice would be waste.
                            if seen.insert(hit) {
                                out.push(hit);
                            }
                        }
                    }
                } else if ix.live(h.idx) {
                    let hit = Hit {
                        vol: *vol,
                        idx: h.idx,
                    };
                    if seen.insert(hit) {
                        out.push(hit);
                    }
                }
                if out.len() > MAX_CONTENT_FILES {
                    return out;
                }
            }
        }
        out
    }

    /// Puts the content hits into the list's order. Called when the results
    /// or the sort change — never from the draw path.
    fn rebuild_find_order(&mut self) {
        let mut hits: Vec<Hit> = self.find_lines.keys().copied().collect();
        let vols = self.store.snapshot(None);
        store::sort_hits(
            &vols,
            &mut hits,
            self.sort,
            self.sort_desc,
            self.cfg.folders_first,
        );
        self.find_sorted = hits;
        self.find_sorted_for = (
            self.sort,
            self.sort_desc,
            self.cfg.folders_first,
            self.find_lines.len(),
        );
    }

    fn poll_find(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.find_rx else {
            if self.find_progress.is_some() {
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            }
            return;
        };
        if let Ok((generation, hits)) = rx.try_recv() {
            self.find_rx = None;
            self.find_progress = None;
            if generation == self.find_gen {
                self.find_lines = hits.into_iter().collect();
                self.rebuild_find_order();
                // The selection only moves if what it pointed at really left
                // the list — and then to the first row rather than to nothing,
                // so the preview does not blink out.
                if self.sel_hit.is_some_and(|h| !self.find_lines.contains_key(&h)) {
                    self.sel_hit = self.find_sorted.first().copied();
                }
            }
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
    }

    /// Keeps only the entries whose contents matched, once a content search has
    /// produced results.
    fn apply_find(&self, hits: &mut Vec<Hit>) {
        if self.find_text.trim().is_empty() {
            return;
        }
        if self.query.trim().is_empty() {
            // The search ranged over the whole subtree, so its hits *are* the
            // list — filtering the folder's own children would drop everything
            // that was actually found.
            hits.clear();
            hits.extend_from_slice(&self.find_sorted);
        } else {
            hits.retain(|h| self.find_lines.contains_key(h));
        }
    }

    /// Walks a UNC share here rather than in the service.
    ///
    /// The service runs as LocalSystem, which on a machine that is not domain
    /// joined reaches the network as the computer account — it has none of the
    /// credentials that made `\server\share` work in Explorer. This process is
    /// the user, so the share opens. The result stays local to the window:
    /// there is no MFT and therefore no USN journal to keep it live either way.
    fn scan_share(&mut self, path: String) {
        if self.share_scan.is_some() {
            self.notice = Some((t("Es läuft bereits ein Freigaben-Scan").into(), true));
            return;
        }
        let (tx, rx) = channel();
        let progress = Arc::new(crate::scan::Progress::default());
        let p = Arc::clone(&progress);
        std::thread::Builder::new()
            .name("share-scan".into())
            .spawn(move || {
                let target = Target::Path(path.clone());
                let r = crate::scan::run(target, &p).map(|s| s.index).map_err(|e| {
                    tf("„{0}“ konnte nicht gelesen werden: {1}", &[&path, &e.to_string()])
                });
                let _ = tx.send((path, r));
            })
            .ok();
        self.share_scan = Some((rx, progress));
    }

    /// Folds a finished share walk into the store.
    fn poll_share_scan(&mut self, ctx: &egui::Context) {
        let Some((rx, _)) = &self.share_scan else {
            return;
        };
        match rx.try_recv() {
            Ok((path, Ok(index))) => {
                self.share_scan = None;
                self.scanning = None;
                if !self.cfg.shares.iter().any(|s| s.eq_ignore_ascii_case(&path)) {
                    self.cfg.shares.push(path.clone());
                    self.cfg.save();
                }
                let target = Target::Path(path);
                let slot = self.store.put(Volume {
                    key: target.key(),
                    title: target.title(),
                    index: Arc::new(RwLock::new(index)),
                    watcher: None,
                    live: false,
                    target,
                });
                self.store.active = None; // force `activate` to do its work
                self.activate(slot);
            }
            Ok((_, Err(e))) => {
                self.share_scan = None;
                self.scanning = None;
                self.notice = Some((e, true));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.share_scan = None;
                self.scanning = None;
            }
        }
    }

    /// Folds everything the service reported into the local store.
    fn poll_service(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.svc.events.try_recv() {
            match ev {
                client::Event::Connected => {
                    self.notice = None;
                }
                client::Event::Disconnected => {
                    self.notice = Some((t("Verbindung zum Dienst verloren").into(), true));
                }
                client::Event::Volume {
                    key,
                    title,
                    index,
                    usn,
                } => {
                    let target = self.target_for(&key);
                    let slot = self.store.put(Volume {
                        key: key.clone(),
                        title,
                        index,
                        // The service owns the watchers; the client only mirrors.
                        watcher: None,
                        live: usn,
                        target,
                    });
                    let asked_for = self.pending_activate.as_deref() == Some(&key);
                    // Volumes arrive in whatever order the service finishes
                    // them, so taking the first to appear meant a different
                    // drive each launch. Keep re-picking the preferred one
                    // until the user chooses for themselves.
                    if asked_for || (self.auto_selected && self.pending_path.is_none()) {
                        let want = if asked_for {
                            Some(slot)
                        } else {
                            self.preferred_slot()
                        };
                        if let Some(want) = want {
                            self.pending_activate = None;
                            self.store.active = None;
                            self.activate(want);
                            self.auto_selected = !asked_for;
                            self.apply_pending_path();
                        }
                    } else {
                        // A refreshed snapshot replaced the arrays under us.
                        self.chart.invalidate();
                        self.map.invalidate();
                        self.browse_key = (u32::MAX, 0, 0, false);
                    }
                    self.scanning = None;
                    if !self.query.trim().is_empty() {
                        self.query_dirty = true;
                    }
                }
                client::Event::Status(vols) => {
                    self.scanning = vols
                        .iter()
                        .find(|v| v.scanning)
                        .map(|v| format!("{} …", v.title));
                    self.svc_volumes = vols;
                }
                client::Event::Changed(key) => {
                    // A volume that just finished loading has a root now.
                    let is_active = self
                        .store
                        .active
                        .and_then(|i| self.store.vols.get(i))
                        .is_some_and(|v| v.key == key);
                    if is_active && self.view_root == NONE {
                        let root = self
                            .active_index()
                            .cloned()
                            .map(|ix| {
                                let g = ix.read();
                                if g.is_ready() { g.root } else { NONE }
                            })
                            .unwrap_or(NONE);
                        if root != NONE {
                            self.view_root = root;
                            self.tree.reset();
                            self.tree.expanded.insert(root);
                            self.apply_pending_path();
                        }
                    }
                    self.chart.invalidate();
                    self.map.invalidate();
                    self.browse_key = (u32::MAX, 0, 0, false);
                    if !self.query.trim().is_empty() {
                        self.query_dirty = true;
                    }
                }
            }
            ctx.request_repaint();
        }
    }

    fn kick_search(&mut self) {
        self.search_gen += 1;
        let gen = self.search_gen;
        if self.query.trim().is_empty() {
            self.results.clear();
            self.result_info = None;
            self.search_rx = None;
            self.sel_hit = None;
            return;
        }
        let vols = self
            .store
            .snapshot(if self.global_search { None } else { self.store.active });
        // A search while a subfolder is open is scoped to that subfolder — the
        // filter narrows what you are already looking at.
        let scope = (!self.global_search && self.cfg.search_scoped)
            .then(|| {
                let vol = self.store.active? as u16;
                let root = self.active_index()?.read().root;
                (self.view_root != root && self.view_root != NONE).then_some(Hit {
                    vol,
                    idx: self.view_root,
                })
            })
            .flatten();

        let (tx, rx) = channel();
        let text = self.query.clone();
        let (sort, desc) = (self.sort, self.sort_desc);
        let limit = self.cfg.search_limit_k as usize * 1000;
        let group = self.cfg.folders_first;
        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                let q = search::parse(&text);
                let mut r = store::search(&vols, &q, limit, scope);
                if sort != SortKey::Size || !desc {
                    store::sort_hits(&vols, &mut r.hits, sort, desc, group);
                }
                let _ = tx.send((gen, r));
            })
            .ok();
        self.search_rx = Some(rx);
    }

    /// The rows the central list shows: search hits, or the current folder.
    fn list_hits(&mut self) -> &[Hit] {
        if !self.query.trim().is_empty() {
            return &self.results;
        }
        let Some(slot) = self.store.active else {
            self.browse.clear();
            return &self.browse;
        };
        let gen = self.store.vols[slot].index.read().generation;
        let key = (self.view_root, gen, slot, self.sort_desc);
        if self.browse_key != key || self.browse.is_empty() {
            self.browse_key = key;
            let vols = self.store.snapshot(Some(slot));
            self.browse = self.store.vols[slot]
                .index
                .read()
                .children(self.view_root)
                .map(|idx| Hit {
                    vol: slot as u16,
                    idx,
                })
                .collect();
            store::sort_hits(&vols, &mut self.browse, self.sort, self.sort_desc, self.cfg.folders_first);
        }
        &self.browse
    }

    fn poll_search(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.search_rx else { return };
        match rx.try_recv() {
            Ok((gen, r)) => {
                if gen == self.search_gen {
                    self.results = r.hits;
                    self.result_info = Some((r.total, r.took_ms, r.truncated));
                    if !self.results.iter().any(|h| Some(*h) == self.sel_hit) {
                        self.sel_hit = self.results.first().copied();
                    }
                }
                self.search_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(30));
            }
            Err(_) => self.search_rx = None,
        }
    }

    // ---- helpers -------------------------------------------------------------

    fn active_index(&self) -> Option<&Arc<RwLock<Index>>> {
        self.store.active_vol().map(|v| &v.index)
    }

    /// True when the active volume's snapshot has actually been mapped.
    fn active_ready(&self) -> bool {
        self.active_index()
            .is_some_and(|ix| ix.read().is_ready())
    }

    /// Keeps the current position inside the active index.
    ///
    /// The index behind a volume is replaced wholesale when its snapshot is
    /// mapped or re-published, and the node we were sitting on may not exist in
    /// the new one. Every frame starts by checking that, because the breadcrumb
    /// and the child lists index the arrays directly.
    fn normalise_view_root(&mut self) {
        let Some(index) = self.active_index().cloned() else {
            self.view_root = NONE;
            return;
        };
        let ix = index.read();
        if !ix.is_ready() {
            self.view_root = NONE;
            return;
        }
        if (self.view_root as usize) >= ix.len() || !ix.live(self.view_root) {
            let root = ix.root;
            drop(ix);
            self.view_root = root;
            self.history.clear();
            self.forward.clear();
            self.tree.reset();
            self.tree.expanded.insert(root);
            self.chart.invalidate();
            self.map.invalidate();
            self.browse_key = (u32::MAX, 0, 0, false);
        }
    }

    fn path_of(&self, hit: Hit) -> Option<String> {
        self.store
            .index_of(hit.vol)
            .map(|ix| ix.read().path_of(hit.idx))
    }

    fn zoom_to(&mut self, idx: u32) {
        let Some(index) = self.active_index().cloned() else {
            return;
        };
        {
            let ix = index.read();
            if idx as usize >= ix.len() || !ix.is_dir(idx) || idx == self.view_root {
                return;
            }
        }
        self.history.push(self.view_root);
        self.forward.clear();
        self.view_root = idx;
        self.tree.expanded.insert(idx);
        let ix = index.read();
        self.tree.expand_to(&ix, idx);
        drop(ix);
        self.map.invalidate();
    }

    fn go_up(&mut self) {
        if let Some(prev) = self.history.pop() {
            self.forward.push(self.view_root);
            self.view_root = prev;
        } else if let Some(index) = self.active_index().cloned() {
            let p = index.read().parent[self.view_root as usize];
            if p != NONE {
                self.forward.push(self.view_root);
                self.view_root = p;
            }
        }
        self.map.invalidate();
    }

    /// Forward again after going back, the way a browser or Explorer does.
    fn go_forward(&mut self) {
        if let Some(next) = self.forward.pop() {
            self.history.push(self.view_root);
            self.view_root = next;
            self.map.invalidate();
        }
    }

    /// Back and forward on the thumb buttons, plus Alt+Arrow.
    fn navigation_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() && self.search_focused {
            return;
        }
        let (back, fwd) = ctx.input(|i| {
            (
                i.pointer.button_clicked(egui::PointerButton::Extra1)
                    || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowLeft)),
                i.pointer.button_clicked(egui::PointerButton::Extra2)
                    || (i.modifiers.alt && i.key_pressed(egui::Key::ArrowRight)),
            )
        });
        if back {
            self.go_up();
        } else if fwd {
            self.go_forward();
        }
    }

    /// The genuine Explorer menu, at the cursor.
    ///
    /// Playback is released first: the menu may well contain Delete, Rename or
    /// Move, none of which work while we hold the file open. The call blocks
    /// until the chosen command has run, so afterwards the file's continued
    /// existence tells us whether to pick playback back up.
    fn shell_menu(&mut self, path: &str) {
        let resume = self.media.release_for(path);
        winshell::context_menu_at_cursor(path);
        if std::path::Path::new(path).exists() {
            self.media.restore(resume);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;

        // egui only repaints on demand, so the gap across an idle window is not
        // a dropped frame — recording it would make the graph invent stutters
        // that never happened. Anything past a quarter second was the window
        // sitting still and is left out of the ring entirely.
        let dt = ctx.input(|i| i.unstable_dt) * 1000.0;
        self.frames[self.frame_i] = if dt > 250.0 { 0.0 } else { dt };
        self.frame_i = (self.frame_i + 1) % FRAME_SAMPLES;

        // Settings that live in other components are pushed once per frame.
        // The view states cache by their own detail level, so plain assignment
        // only re-layouts when a value actually moved.
        self.chart.rings = self.cfg.chart_rings as usize;
        self.chart.animate = self.cfg.animate;
        self.media.animate = self.cfg.animate;
        self.map.animate = self.cfg.animate;
        if self.map.depth != self.cfg.map_depth as usize {
            self.map.depth = self.cfg.map_depth as usize;
            self.map.invalidate();
        }
        if self.root {
            winshell::set_close_to_tray(self.cfg.close_to_tray);
    
            if !self.icon_set {
                let hwnd = win::main_window();
                if !hwnd.is_null() {
                    self.icon_set = true;
                    win::set_window_icon();
                    if self.tray.is_some() {
                        winshell::install_close_to_tray(hwnd as *mut std::ffi::c_void);
                    }
                }
            }

            // Playback was cut short while the window was off screen; now that
            // we have a frame again, release the player for real — and do not
            // let autoplay start it over the moment the window reappears.
            if crate::media::take_silenced() {
                self.media.silence();
            }
            self.thumbs.borrow_mut().pump(ctx);
        }

        self.poll_service(ctx);
        self.poll_search(ctx);
        self.handle_tray(ctx);
        self.handle_ipc(ctx);
        self.capture_hotkey(ctx);
        self.navigation_keys(ctx);

        self.poll_share_scan(ctx);
        self.restore_shares();
        self.normalise_view_root();

        if self.query_dirty {
            self.query_dirty = false;
            self.kick_search();
        }
        self.poll_find(ctx);
        self.maybe_kick_find(ctx);
        if !self.find_lines.is_empty()
            && self.find_sorted_for
                != (
                    self.sort,
                    self.sort_desc,
                    self.cfg.folders_first,
                    self.find_lines.len(),
                )
        {
            self.rebuild_find_order();
        }

        self.top_bar(ui);
        self.status_bar(ui);
        self.side_panel(ui);
        // The detail pane belongs to both list views, with or without a search.
        // It is always created and only its expansion changes, so it slides in
        // and out instead of the layout jumping. Playback has to be stopped
        // from here: the player lives in the pane's state, and once the pane is
        // collapsed nothing inside it runs any more.
        let want_preview = self.show_preview
            && (self.view.is_list() || !self.query.trim().is_empty())
            && self.sel_hit.is_some();
        self.preview_panel(ui, want_preview);
        self.central(ui);
        self.dialogs(ctx);

        if self.pending_windows > 0 {
            self.pending_windows -= 1;
            self.open_window = true;
        }
        if self.open_window {
            self.open_window = false;
            let w = self.spawn_window(ctx);
            self.extra.push(w);
        }
        self.extra_windows(ctx, _frame);
    }
}

impl App {
    /// The volume a fresh window should show: the first drive by letter, or the
    /// last, as configured. Volumes that carry no drive letter — an added share
    /// — never win it by default.
    fn preferred_slot(&self) -> Option<usize> {
        let lettered: Vec<usize> = self
            .store
            .vols
            .iter()
            .enumerate()
            .filter(|(_, v)| v.key.len() == 2 && v.key.as_bytes()[1] == b':')
            .map(|(i, _)| i)
            .collect();
        let pool: Vec<usize> = if lettered.is_empty() {
            (0..self.store.vols.len()).collect()
        } else {
            lettered
        };
        if self.cfg.start_first_drive {
            pool.into_iter().min_by_key(|&i| self.store.vols[i].key.clone())
        } else {
            pool.into_iter().max_by_key(|&i| self.store.vols[i].key.clone())
        }
    }

    /// An egui id that no other window can collide with.
    ///
    /// Viewports share a single `Context`, so a literal id is shared by every
    /// window that uses it. For a `TextEdit` that means one cursor for all of
    /// them, and typing in the second window inserts every character at the
    /// position the first one left behind — which reads back as the text
    /// reversed.
    fn id_of(&self, what: &str) -> egui::Id {
        egui::Id::new((what, self.window_id))
    }

    /// A second window: its own view, selection and search, sharing everything
    /// that costs memory.
    ///
    /// Separate processes were the obvious way to do this and the wrong one —
    /// each brought its own graphics device and committed close to 400 MB for
    /// what is really just another view of the same index.
    fn spawn_window(&self, ctx: &egui::Context) -> App {
        let mut w = App::build(ctx, self.cfg.clone(), channel().1, None, false);
        w.thumbs = Rc::clone(&self.thumbs);
        w.icons = Rc::clone(&self.icons);
        w.drives = self.drives.clone();
        w
    }

    /// Draws the extra windows. Each is an ordinary `App` in a viewport of its
    /// own, so nothing in the drawing code has to know about them.
    fn extra_windows(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if self.extra.is_empty() {
            return;
        }
        // Taken out for the duration: the closure needs one window mutably
        // while `self` still owns the rest.
        let mut extra = std::mem::take(&mut self.extra);
        let mut closed = Vec::new();
        for (n, w) in extra.iter_mut().enumerate() {
            let id = egui::ViewportId::from_hash_of(("diskalize-window", w.window_id));
            let builder = egui::ViewportBuilder::default()
                .with_title(t("Diskalize"))
                .with_inner_size([1200.0, 800.0])
                .with_min_inner_size([880.0, 560.0]);
            ctx.show_viewport_immediate(id, builder, |ui, _class| {
                <App as eframe::App>::ui(w, ui, frame);
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    closed.push(n);
                }
            });
        }
        for n in closed.into_iter().rev() {
            extra.remove(n);
        }
        self.extra = extra;
    }

    /// A second launch handed us its path instead of starting another process.
    fn handle_ipc(&mut self, ctx: &egui::Context) {
        let paths: Vec<String> = std::iter::from_fn(|| self.ipc.try_recv().ok()).collect();
        for p in paths {
            win::show_main_window();
            ctx.request_repaint();
            // A launch with no path only means "come to the front", which the
            // line above has already done.
            if !p.trim().is_empty() {
                self.request_path(p);
            }
        }
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        // The close button is intercepted at the window level (see
        // `winshell::install_close_to_tray`), so nothing to do here.
        // Drain first: handling Quit drops the tray, which would invalidate a
        // borrow held across the loop.
        let events: Vec<TrayEvent> = match &self.tray {
            Some(t) => std::iter::from_fn(|| t.try_recv()).collect(),
            None => return,
        };
        // The tray thread already performed the window action; this is only so
        // the next frame is drawn with the window back on screen.
        for ev in events {
            if ev == TrayEvent::Search {
                self.focus_search = true;
            }
            ctx.request_repaint();
        }
    }

    fn top_bar(&mut self, host: &mut egui::Ui) {
        egui::Panel::top("top")
            .frame(
                egui::Frame::default()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 8)),
            )
            .show(host, |ui| {
                ui.horizontal(|ui| {
                    // Network drive roots are left out: raw access does not work
                    // on them and a walk of the whole share is not something to
                    // start by accident. Individual shares can still be added as
                    // UNC paths in the settings.
                    let drives: Vec<DriveInfo> = self
                        .drives
                        .iter()
                        .filter(|d| d.kind != win::DRIVE_REMOTE)
                        .cloned()
                        .collect();
                    let active_key = self.store.active_vol().map(|v| v.key.clone());
                    for d in &drives {
                        let key = format!("{}:", d.letter);
                        let indexed = self.store.find(&key).is_some();
                        let active = active_key.as_deref() == Some(key.as_str());
                        if self.drive_chip(ui, d, active, indexed).clicked() {
                            self.open_target(Target::Drive(d.clone()));
                        }
                    }

                    // Folder and network scans that are not drive roots.
                    let extra: Vec<(usize, String)> = self
                        .store
                        .vols
                        .iter()
                        .enumerate()
                        .filter(|(_, v)| !matches!(v.target, Target::Drive(_)))
                        .map(|(i, v)| (i, v.title.clone()))
                        .collect();
                    for (slot, title) in extra {
                        let active = self.store.active == Some(slot);
                        let r = ui.selectable_label(active, format!("📁 {title}"));
                        if r.clicked() {
                            self.activate(slot);
                        }
                        if r.secondary_clicked() {
                            self.store.remove(slot);
                        }
                    }

                    if ui
                        .button(t("Ordner…"))
                        .on_hover_text(t("Einen einzelnen Ordner analysieren"))
                        .clicked()
                    {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.open_target(Target::Path(p.to_string_lossy().into_owned()));
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("⚙").on_hover_text(t("Einstellungen")).clicked() {
                            self.show_settings = !self.show_settings;
                        }
                        if ui.button("?").on_hover_text(t("Suchsyntax")).clicked() {
                            self.show_help = !self.show_help;
                        }
                        ui.separator();
                        match self.view {
                            // The toolbar sliders edit the persisted settings
                            // directly, so a value set here survives a restart
                            // and the settings dialog shows the same number.
                            View::Sunburst => {
                                let mut r = self.cfg.chart_rings as usize;
                                if ui
                                    .add(egui::Slider::new(&mut r, 3..=12).text(t("Ringe")))
                                    .changed()
                                {
                                    self.cfg.chart_rings = r as u32;
                                    self.cfg.save();
                                }
                            }
                            View::Treemap => {
                                let mut d = self.cfg.map_depth as usize;
                                if ui
                                    .add(egui::Slider::new(&mut d, 1..=8).text(t("Tiefe")))
                                    .changed()
                                {
                                    self.cfg.map_depth = d as u32;
                                    self.cfg.save();
                                }
                            }
                            View::Tiles => {
                                let mut px = self.cfg.tile_px;
                                if ui
                                    .add(egui::Slider::new(&mut px, 48..=320).text(t("Größe")))
                                    .changed()
                                {
                                    self.cfg.tile_px = px;
                                    self.cfg.save();
                                }
                            }
                            View::Details => {}
                        }
                        ui.separator();
                        ui.selectable_value(&mut self.view, View::Tiles, t("Kacheln"));
                        ui.selectable_value(&mut self.view, View::Details, t("Details"));
                        ui.selectable_value(&mut self.view, View::Treemap, t("Treemap"));
                        ui.selectable_value(&mut self.view, View::Sunburst, t("Kuchen"));
                    });
                });

                ui.add_space(6.0);
                // Right-aligned group first, then the search field fills whatever
                // is left — otherwise the two sides overlap on narrow windows.
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some((total, ms, trunc)) = self.result_info {
                            ui.label(
                                RichText::new(tf(
                                    "{0}{1} Treffer · {2} ms",
                                    &[
                                        &fmt::count(total as u64),
                                        if trunc { "+" } else { "" },
                                        &ms.to_string(),
                                    ],
                                ))
                                .size(11.0)
                                .color(theme::TEXT_DIM),
                            );
                        } else if self.search_rx.is_some() {
                            ui.spinner();
                        }

                        ui.separator();
                        ui.toggle_value(&mut self.show_tree, "☰")
                            .on_hover_text(t("Ordnerbaum ein-/ausblenden"));
                        if self.view.is_list() {
                            ui.toggle_value(&mut self.show_preview, t("Detailbereich"));
                            // The Details header is clickable, but tiles have no
                            // header — so the sort controls live here for both.
                            if ui
                                .add(egui::Button::new(if self.sort_desc { "▼" } else { "▲" }))
                                .on_hover_text(t("Sortierrichtung"))
                                .clicked()
                            {
                                self.sort_desc = !self.sort_desc;
                                self.resort();
                            }
                            let mut sort = self.sort;
                            egui::ComboBox::from_id_salt(self.id_of("sortkey"))
                                .selected_text(match sort {
                                    SortKey::Size => t("Größe"),
                                    SortKey::Name => t("Name"),
                                    SortKey::Date => t("Geändert"),
                                    SortKey::Path => t("Pfad"),
                                })
                                .width(94.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut sort, SortKey::Size, t("Größe"));
                                    ui.selectable_value(&mut sort, SortKey::Name, t("Name"));
                                    ui.selectable_value(&mut sort, SortKey::Date, t("Geändert"));
                                    ui.selectable_value(&mut sort, SortKey::Path, t("Pfad"));
                                });
                            if sort != self.sort {
                                self.sort = sort;
                                self.resort();
                            }
                            ui.separator();
                        }
                        if ui
                            .selectable_label(self.global_search, t("Alle Laufwerke"))
                            .on_hover_text(
                                t("Über jedes bereits indizierte Laufwerk suchen statt nur über das aktive"),
                            )
                            .clicked()
                        {
                            self.global_search = !self.global_search;
                            if self.global_search {
                                self.svc.send(client::Cmd::LoadAll);
                            }
                            self.query_dirty = !self.query.trim().is_empty();
                        }
                        if !self.query.is_empty() && ui.button("✕").clicked() {
                            self.query.clear();
                            self.query_dirty = true;
                        }

                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.label("🔍");
                            let hint =
                                t("Suche: *.mp4   ext:iso   size:>1gb   !cache   path:\\Users\\");
                            // Fixed Id: without it, typing the first character
                            // changes the surrounding layout, egui hands the box a
                            // new auto-Id, and the caret jumps out of the field.
                            let field_id = self.id_of("search");
                            let resp = ui.add_sized(
                                Vec2::new(ui.available_width(), 26.0),
                                egui::TextEdit::singleline(&mut self.query)
                                    .id(field_id)
                                    .hint_text(hint)
                                    .desired_width(f32::INFINITY),
                            );
                            if resp.changed() {
                                self.query_dirty = true;
                            }
                            if std::mem::take(&mut self.focus_search) {
                                resp.request_focus();
                            }
                            self.search_focused = resp.has_focus();
                        });
                    });
                });

                ui.add_space(4.0);
                self.type_filters(ui);
                self.content_field(ui);
            });
    }

    /// The second search box: text to look for inside the listed files.
    ///
    /// Separate from the name query on purpose. The name query is what makes
    /// this affordable — it decides how many files get opened — and mixing the
    /// two into one box would hide that relationship.
    fn content_field(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(t("Inhalt")).size(11.0).color(theme::TEXT_DIM));
            let field_id = self.id_of("content");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.find_text)
                    .id(field_id)
                    .hint_text(t("Text in den gelisteten Dateien suchen"))
                    .desired_width(260.0),
            );
            let _ = resp;
            if !self.find_text.is_empty() && ui.small_button("✕").clicked() {
                self.find_text.clear();
            }

            match &self.find_progress {
                Some(p) => {
                    let done = p.done.load(std::sync::atomic::Ordering::Relaxed);
                    let total = p.total.load(std::sync::atomic::Ordering::Relaxed).max(1);
                    ui.add(egui::ProgressBar::new(done as f32 / total as f32).desired_width(90.0));
                    ui.label(
                        RichText::new(tf(
                            "{0} von {1} Dateien",
                            &[&fmt::count(done as u64), &fmt::count(total as u64)],
                        ))
                        .size(10.5)
                        .color(theme::TEXT_DIM),
                    );
                }
                None if !self.find_text.trim().is_empty() => {
                    ui.label(
                        RichText::new(tf(
                            "{0} Dateien enthalten den Text",
                            &[&fmt::count(self.find_lines.len() as u64)],
                        ))
                        .size(10.5)
                        .color(theme::TEXT_DIM),
                    );
                    if self.find_truncated {
                        ui.label(
                            RichText::new(tf(
                                "nur die ersten {0} Dateien gelesen — weiter eingrenzen",
                                &[&fmt::count(MAX_CONTENT_FILES as u64)],
                            ))
                            .size(10.5)
                            .color(theme::WARN),
                        );
                    }
                }
                None => {
                    ui.label(
                        RichText::new(t(
                            "Liest nur, was oben gelistet ist — erst eingrenzen, dann suchen",
                        ))
                        .size(10.5)
                        .color(theme::TEXT_DIM),
                    );
                }
            }
        });
    }

    /// One-click filters for the common file families.
    ///
    /// They are nothing but a `type:` term written into the search box, so a
    /// chip and a typed query are the same thing — the chip lights up when the
    /// term is there, whoever put it there.
    fn type_filters(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(t("Typ"))
                    .size(11.0)
                    .color(theme::TEXT_DIM),
            );
            let mut toggled: Option<(&'static str, bool)> = None;
            for c in search::CATEGORIES {
                let on = self.has_type_term(c.key);
                if ui
                    .selectable_label(on, RichText::new(crate::i18n::t(c.label)).size(11.0))
                    .on_hover_text(format!("type:{} — {}", c.key, c.exts.join(", ")))
                    .clicked()
                {
                    toggled = Some((c.key, on));
                }
            }
            let only_dirs = self.has_type_term("ordner");
            if ui
                .selectable_label(only_dirs, RichText::new(t("Nur Ordner")).size(11.0))
                .clicked()
            {
                toggled = Some(("ordner", only_dirs));
            }
            if let Some((key, was_on)) = toggled {
                self.set_type_term(key, !was_on);
            }
        });
    }

    fn type_token(key: &str) -> String {
        format!("type:{key}")
    }

    fn has_type_term(&self, key: &str) -> bool {
        let token = Self::type_token(key);
        self.query
            .split_whitespace()
            .any(|t| t.eq_ignore_ascii_case(&token))
    }

    /// Adds or removes one `type:` token, leaving the rest of the query alone.
    fn set_type_term(&mut self, key: &str, on: bool) {
        let token = Self::type_token(key);
        // Terms are ANDed, so two of these would always return nothing: no file
        // is both audio and video, and none is both a folder and a document.
        // Switching one on therefore replaces whichever was set.
        let mut parts: Vec<String> = self
            .query
            .split_whitespace()
            .filter(|t| {
                !t.eq_ignore_ascii_case(&token)
                    && !(on && t.len() > 5 && t[..5].eq_ignore_ascii_case("type:"))
            })
            .map(str::to_string)
            .collect();
        if on {
            parts.push(token);
        }
        self.query = parts.join(" ");
        self.query_dirty = true;
    }

    fn drive_chip(
        &self,
        ui: &mut egui::Ui,
        d: &DriveInfo,
        active: bool,
        indexed: bool,
    ) -> egui::Response {
        let w = 74.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), egui::Sense::click());
        let p = ui.painter();
        let bg = if active {
            theme::ACCENT.gamma_multiply(0.30)
        } else if resp.hovered() {
            theme::PANEL_HI
        } else {
            theme::BG
        };
        p.rect_filled(rect, egui::CornerRadius::same(6), bg);
        p.rect_stroke(
            rect,
            egui::CornerRadius::same(6),
            egui::Stroke::new(1.0, if active { theme::ACCENT } else { theme::LINE }),
            egui::StrokeKind::Inside,
        );
        p.text(
            egui::Pos2::new(rect.left() + 8.0, rect.top() + 9.0),
            egui::Align2::LEFT_CENTER,
            format!("{}:", d.letter),
            egui::FontId::proportional(12.5),
            theme::TEXT,
        );
        p.text(
            egui::Pos2::new(rect.right() - 8.0, rect.top() + 9.0),
            egui::Align2::RIGHT_CENTER,
            fmt::size(d.total),
            egui::FontId::proportional(9.5),
            theme::TEXT_DIM,
        );
        let bar = egui::Rect::from_min_size(
            egui::Pos2::new(rect.left() + 8.0, rect.bottom() - 11.0),
            Vec2::new(w - 16.0, 5.0),
        );
        p.rect_filled(bar, egui::CornerRadius::same(2), theme::LINE);
        let frac = if d.total > 0 {
            d.used() as f32 / d.total as f32
        } else {
            0.0
        };
        p.rect_filled(
            egui::Rect::from_min_size(bar.min, Vec2::new(bar.width() * frac, bar.height())),
            egui::CornerRadius::same(2),
            if frac > 0.9 { theme::WARN } else { theme::ACCENT },
        );
        // A dot marks volumes that are already in memory.
        if indexed && !active {
            p.circle_filled(
                egui::Pos2::new(rect.right() - 6.0, rect.top() + 5.0),
                2.5,
                theme::GOOD,
            );
        }
        resp.on_hover_text(tf(
            "{0} {1}\n{2} · {3} frei von {4}\n{5}",
            &[
                &d.label,
                d.kind_name(),
                &d.fs,
                &fmt::size(d.free),
                &fmt::size(d.total),
                if indexed {
                    t("bereits indiziert — Klick wechselt sofort")
                } else {
                    t("Klick startet den Scan")
                },
            ],
        ))
    }

    fn status_bar(&mut self, host: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::default()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 5)),
            )
            .show(host, |ui| {
                ui.horizontal(|ui| {
                    if let Some(label) = self.scanning.clone() {
                        ui.spinner();
                        ui.label(tf("Dienst indiziert {0}", &[&label]));
                    } else if let Some(v) = self.store.active_vol() {
                        let ix = v.index.read();
                        ui.label(
                            RichText::new(tf(
                                "{0} Dateien · {1} Ordner",
                                &[&fmt::count(ix.total_files), &fmt::count(ix.total_dirs)],
                            ))
                            .size(11.5),
                        );
                        ui.separator();
                        ui.label(
                            RichText::new(tf(
                                "{0} in {1}",
                                &[
                                    if ix.vol.method_mft {
                                        t("MFT-Direktzugriff")
                                    } else {
                                        t("Verzeichnis-Scan")
                                    },
                                    &fmt::duration(ix.vol.scan_ms),
                                ],
                            ))
                            .size(11.5)
                            .color(theme::TEXT_DIM),
                        );
                        drop(ix);
                        ui.separator();
                        // The service owns the watchers now, so `live` comes from
                        // what it reported — not from a local watcher we no
                        // longer have.
                        if v.live() {
                            ui.label(
                                RichText::new(t("● live · Dienst hält den Index aktuell"))
                                    .size(11.5)
                                    .color(theme::GOOD),
                            );
                        } else {
                            ui.label(
                                RichText::new(t("○ keine Live-Indizierung für dieses Volume"))
                                    .size(11.5)
                                    .color(theme::TEXT_DIM),
                            );
                        }
                        if self.store.vols.len() > 1 {
                            ui.separator();
                            let (f, _) = self.store.totals();
                            ui.label(
                                RichText::new(tf(
                                    "{0} Volumes indiziert · {1} Dateien gesamt",
                                    &[&self.store.vols.len().to_string(), &fmt::count(f)],
                                ))
                                .size(11.5)
                                .color(theme::TEXT_DIM),
                            );
                        }
                    } else {
                        ui.label(RichText::new(t("Kein Index")).color(theme::TEXT_DIM));
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // Version in the corner doubles as the way into the
                        // About page — nothing else in the window mentions it.
                        if ui
                            .add(
                                egui::Label::new(
                                    RichText::new(format!("v{VERSION}"))
                                        .size(10.5)
                                        .color(theme::TEXT_DIM),
                                )
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_text(t("Über Diskalize"))
                            .clicked()
                        {
                            self.settings_tab = SettingsTab::About;
                            self.show_settings = true;
                        }
                        ui.separator();
                        // Messages appear and vanish on their own schedule, so
                        // they fade rather than blink in and out of the bar.
                        let shown = ui.ctx().animate_bool_with_time(
                            self.id_of("notice_fade"),
                            self.notice.is_some(),
                            if self.cfg.animate { 0.25 } else { 0.0 },
                        );
                        if let (Some((msg, is_err)), true) = (&self.notice, shown > 0.0) {
                            let base = if *is_err { theme::WARN } else { theme::TEXT_DIM };
                            ui.label(
                                RichText::new(msg)
                                    .size(11.0)
                                    .color(base.gamma_multiply(shown)),
                            );
                        }
                    });
                });
            });
    }

    fn side_panel(&mut self, host: &mut egui::Ui) {
        let mut open = self.show_tree;
        // Widths follow the window. With absolute limits, shrinking a large
        // window left the side panels at full size and the file list vanished
        // between them.
        let w = host.ctx().content_rect().width();
        egui::Panel::left("tree")
            .resizable(true)
            .default_size((w * 0.22).clamp(200.0, 320.0))
            .size_range(egui::Rangef::new(
                160.0f32.min(w * 0.3),
                (w * 0.34).clamp(200.0, 620.0),
            ))
            .frame(
                egui::Frame::default()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(6, 6)),
            )
            .show_collapsible(host, &mut open, |ui| {
                let Some(index) = self
                    .active_index()
                    .filter(|ix| ix.read().is_ready())
                    .cloned()
                else {
                    tree::empty_hint(ui, t("Laufwerk oben auswählen"));
                    return;
                };
                let (act, root) = {
                    let ix = index.read();
                    let root = ix.root;
                    (
                        tree::show(ui, &ix, &mut self.tree, root, self.view_root),
                        root,
                    )
                };
                let _ = root;
                if let Some(f) = act.focus {
                    let is_dir = index.read().is_dir(f);
                    if is_dir {
                        self.view_root = f;
                        self.history.clear();
                        self.map.invalidate();
                    }
                }
                if let Some(c) = act.context {
                    let path = index.read().path_of(c);
                    self.shell_menu(&path);
                }
            });
        self.show_tree = open;
    }

    fn preview_panel(&mut self, host: &mut egui::Ui, wanted: bool) {
        // Every way out of here means the pane shows nothing, so playback has to
        // go with it — otherwise changing the search leaves audio running.
        let target = self
            .sel_hit
            .filter(|_| wanted)
            .and_then(|hit| Some((hit, self.store.index_of(hit.vol).cloned()?)));
        if target.is_none() {
            self.media.stop();
        }
        let mut open = target.is_some();
        let w = host.ctx().content_rect().width();
        egui::Panel::right("preview")
            .resizable(true)
            .default_size((w * 0.26).clamp(240.0, 420.0))
            .size_range(egui::Rangef::new(
                200.0f32.min(w * 0.35),
                (w * 0.45).clamp(240.0, 1100.0),
            ))
            .drag_to_open(false)
            .frame(
                egui::Frame::default()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show_collapsible(host, &mut open, |ui| {
                let Some((hit, index)) = target else { return };
                let ix = index.read();
                if (hit.idx as usize) < ix.len() && ix.live(hit.idx) {
                    let mut upscale = self.cfg.preview_upscale;
                    preview::show(
                        ui,
                        &ix,
                        hit.idx,
                        &mut self.thumbs.borrow_mut(),
                        &mut upscale,
                        &mut self.media,
                        self.cfg.text_preview,
                    );
                    let changed = upscale != self.cfg.preview_upscale
                        || self.media.autoplay != self.cfg.autoplay
                        || self.media.looping != self.cfg.loop_media
                        || self.media.volume != self.cfg.volume;
                    if changed {
                        self.cfg.preview_upscale = upscale;
                        self.cfg.autoplay = self.media.autoplay;
                        self.cfg.loop_media = self.media.looping;
                        self.cfg.volume = self.media.volume;
                        self.cfg.save();
                    }
                } else {
                    self.media.stop();
                    tree::empty_hint(ui, t("Eintrag nicht mehr vorhanden"));
                }
            });
        // Dragging the pane shut past its minimum width means the same thing as
        // switching the toggle off.
        if wanted && !open {
            self.show_preview = false;
        }
    }

    fn central(&mut self, host: &mut egui::Ui) {
        // Switching view mode or drive replaces everything in here at once. The
        // charts morph on their own when you drill down, so this only covers the
        // swaps they cannot: a different renderer, or a different volume.
        let key = (self.view as u8, self.store.active.unwrap_or(usize::MAX));
        if self.swap_key != key {
            self.swap_key = key;
            self.swap_t = if self.cfg.animate { 0.0 } else { 1.0 };
        }
        if self.swap_t < 1.0 {
            self.swap_t = (self.swap_t + host.input(|i| i.stable_dt) / 0.16).min(1.0);
            host.ctx().request_repaint();
        }

        let inner = egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show(host, |ui| {
                if self.store.active.is_none() || !self.active_ready() {
                    self.service_screen(ui);
                    return;
                }
                // A search always lands in a list; without one the chosen view wins.
                if self.view.is_list() || !self.query.trim().is_empty() {
                    if !self.query.trim().is_empty() && !self.view.is_list() {
                        self.view = View::Details;
                    }
                    self.results_view(ui);
                    return;
                }
                self.chart_view(ui);
            });

        if self.swap_t < 1.0 {
            // A veil in the panel colour, thinning out. Drawn above the panel
            // but below tooltips and popups, so a menu opened mid-transition is
            // not dimmed along with it.
            let painter = host.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Middle,
                self.id_of("view_swap"),
            ));
            painter.rect_filled(
                inner.response.rect,
                egui::CornerRadius::ZERO,
                theme::BG.gamma_multiply(1.0 - self.swap_t),
            );
        }
    }

    /// Shown until the service hands us a first snapshot.
    fn service_screen(&mut self, ui: &mut egui::Ui) {
        let installed = crate::service::is_installed();
        let running = installed && crate::service::is_running();
        let connected = self.svc.connected();

        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.24);
            ui.label(RichText::new(t("Diskalize")).size(26.0).color(theme::ACCENT));
            ui.add_space(6.0);

            if connected {
                ui.label(
                    RichText::new(t("Der Dienst indiziert gerade die Laufwerke …"))
                        .size(13.0)
                        .color(theme::TEXT_DIM),
                );
                ui.add_space(10.0);
                ui.spinner();
                for v in &self.svc_volumes {
                    ui.label(
                        RichText::new(tf(
                            "{0}  {1}",
                            &[
                                &v.title,
                                if v.scanning {
                                    t("wird gelesen")
                                } else {
                                    t("bereit")
                                },
                            ],
                        ))
                        .size(11.5)
                        .color(theme::TEXT_DIM),
                    );
                }
                return;
            }

            let msg = if !installed {
                t("Diskalize liest die NTFS-Master-File-Table direkt. Das braucht \
                   Systemrechte, deshalb erledigt ein Hintergrunddienst die Indizierung.\n\
                   Einmal installieren — danach startet Diskalize ohne jede Rückfrage.")
            } else if !running {
                t("Der Dienst ist installiert, läuft aber nicht.")
            } else {
                t("Der Dienst läuft, die Verbindung steht noch nicht.")
            };
            ui.label(RichText::new(msg).size(13.0).color(theme::TEXT_DIM));
            ui.add_space(14.0);

            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 260.0).max(0.0) * 0.5);
                if !installed {
                    if ui
                        .add_sized([200.0, 32.0], egui::Button::new(t("Dienst installieren")))
                        .clicked()
                    {
                        self.run_service_verb("--install");
                    }
                } else if !running && ui
                    .add_sized([200.0, 32.0], egui::Button::new(t("Dienst starten")))
                    .clicked()
                {
                    self.run_service_verb("--install");
                }
            });

            if let Some((msg, err)) = &self.service_busy {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(msg)
                        .size(11.5)
                        .color(if *err { theme::WARN } else { theme::GOOD }),
                );
            }
        });
        ui.allocate_space(ui.available_size());
    }

    /// Runs the service binary elevated for install/uninstall.
    fn run_service_verb(&mut self, verb: &str) {
        let Some(exe) = service_exe() else {
            self.service_busy = Some((t("diskalize-service.exe nicht gefunden").into(), true));
            return;
        };
        // The UAC prompt happens here and only here.
        if win::run_elevated(&exe.to_string_lossy(), verb) {
            self.service_busy = Some((
                "Dienst wird eingerichtet — die Laufwerke erscheinen gleich.".into(),
                false,
            ));
        } else {
            self.service_busy = Some(("Abgebrochen oder fehlgeschlagen.".into(), true));
        }
    }

    fn chart_view(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self.active_index().cloned() else {
            return;
        };
        self.breadcrumb(ui, &index);
        ui.add_space(4.0);

        let (clicked, double, context, go_up, clicked_is_dir) = {
            let ix = index.read();
            if self.view_root == NONE || self.view_root as usize >= ix.len() {
                self.view_root = ix.root;
            }
            let vr = self.view_root;
            let (clicked, double, context, go_up) = match self.view {
                View::Treemap => {
                    let r = treemap::show(ui, &ix, &mut self.map, vr);
                    (r.clicked, r.double, r.context, false)
                }
                // Only the chart views reach here; list views take the other path.
                _ => {
                    self.chart.ensure(&ix, vr);
                    let r = sunburst::show(ui, &ix, &mut self.chart, vr);
                    (r.clicked, r.double, r.context, r.go_up)
                }
            };
            let is_dir = clicked.map(|c| ix.is_dir(c)).unwrap_or(false);
            (clicked, double, context, go_up, is_dir)
        };

        if go_up {
            self.go_up();
        }
        if let Some(c) = context {
            let path = index.read().path_of(c);
            self.shell_menu(&path);
        } else if let Some(d) = double {
            let (is_dir, path) = {
                let ix = index.read();
                (ix.is_dir(d), ix.path_of(d))
            };
            if !is_dir {
                shell::open(&path);
            }
        } else if let Some(c) = clicked {
            if clicked_is_dir {
                self.zoom_to(c);
            }
        }
    }

    fn breadcrumb(&mut self, ui: &mut egui::Ui, index: &Arc<RwLock<Index>>) {
        let (names, size, files, depth) = {
            let ix = index.read();
            if !ix.is_ready() || (self.view_root as usize) >= ix.len() {
                return;
            }
            let mut chain = Vec::new();
            let mut cur = self.view_root;
            while cur != NONE {
                chain.push(cur);
                if cur == ix.root {
                    break;
                }
                cur = ix.parent[cur as usize];
            }
            chain.reverse();
            let names: Vec<(u32, String)> =
                chain.iter().map(|&i| (i, ix.name(i).to_string())).collect();
            (
                names,
                ix.size[self.view_root as usize],
                ix.files[self.view_root as usize],
                chain.len(),
            )
        };

        let mut jump = None;
        let mut up = false;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(depth > 1, egui::Button::new("↑"))
                .on_hover_text(t("Eine Ebene höher (Rücktaste)"))
                .clicked()
            {
                up = true;
            }
            egui::ScrollArea::horizontal()
                .max_width((ui.available_width() - 220.0).max(120.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (n, (idx, name)) in names.iter().enumerate() {
                            if n > 0 {
                                ui.label(RichText::new("›").color(theme::TEXT_DIM));
                            }
                            let last = n + 1 == names.len();
                            let txt = RichText::new(name).size(12.5).color(if last {
                                theme::TEXT
                            } else {
                                theme::ACCENT
                            });
                            if ui.add(egui::Link::new(txt)).clicked() && !last {
                                jump = Some(*idx);
                            }
                        }
                    });
                });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(tf(
                        "{0} · {1} Dateien",
                        &[&fmt::size(size), &fmt::count(files as u64)],
                    ))
                    .size(12.0)
                    .color(theme::TEXT_DIM),
                );
            });
        });

        if up {
            self.go_up();
        } else if let Some(idx) = jump {
            self.history.clear();
            self.view_root = idx;
            self.map.invalidate();
        }
    }

    /// Arrow keys move the selection; the list scrolls to follow. Only active
    /// while the search box does not have focus, so typing still works.
    fn results_keys(&mut self, ui: &egui::Ui) {
        // Any focused text field owns the keyboard, not just the name search:
        // Delete in the content box has to erase a character, not a file.
        // Asking egui covers every field there is, including ones added later.
        if ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let hits: Vec<Hit> = self.list_hits().to_vec();
        if hits.is_empty() {
            return;
        }
        use egui::Key;
        let grid = self.view == View::Tiles;
        let step = ui.input(|i| {
            if i.key_pressed(Key::ArrowRight) && grid {
                Some(tree::Step::Forward)
            } else if i.key_pressed(Key::ArrowLeft) && grid {
                Some(tree::Step::Back)
            } else if i.key_pressed(Key::ArrowDown) {
                Some(tree::Step::Next)
            } else if i.key_pressed(Key::ArrowUp) {
                Some(tree::Step::Prev)
            } else if i.key_pressed(Key::PageDown) {
                Some(tree::Step::PageDown)
            } else if i.key_pressed(Key::PageUp) {
                Some(tree::Step::PageUp)
            } else if i.key_pressed(Key::Home) {
                Some(tree::Step::First)
            } else if i.key_pressed(Key::End) {
                Some(tree::Step::Last)
            } else {
                None
            }
        });

        // In the grid, up/down must move a whole row.
        let per_row = if grid {
            ((ui.available_width() / (self.cfg.tile_px as f32 + 26.0)).floor() as usize).max(1)
        } else {
            1
        };
        let page = if grid {
            ((ui.available_height() / (self.cfg.tile_px as f32 + 46.0)).floor() as usize).max(1)
        } else {
            (ui.available_height() / 21.0).floor() as usize
        };

        if let Some(step) = step {
            let mut sel = self.sel_hit;
            self.scroll_to = tree::step_selection(&hits, &mut sel, step, per_row, page);
            self.sel_hit = sel;
        }
        if ui.input(|i| i.key_pressed(Key::Enter)) {
            if let Some(h) = self.sel_hit {
                self.open_hit(h);
            }
        }
        if ui.input(|i| i.key_pressed(Key::Delete)) {
            if let Some(h) = self.sel_hit {
                if let Some(p) = self.path_of(h) {
                    // A file we are playing is an open handle, and Windows will
                    // not delete underneath one — so let go first.
                    let resume = self.media.release_for(&p);
                    // Handed to the shell so the recycle bin, the confirmation
                    // prompt and the undo record all behave as usual.
                    winshell::delete_items(&[p.clone()]);
                    if std::path::Path::new(&p).exists() {
                        // Declined at the prompt: carry on exactly where we were.
                        self.media.restore(resume);
                    } else {
                        self.after_delete(h, &hits);
                    }
                }
            }
        }
    }

    /// Settings, grouped into tabs.
    ///
    /// Everything writes through to disk the moment it changes — no apply
    /// button, no way to lose a change by closing the window.
    fn settings_body(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (tab, label) in [
                (SettingsTab::View, t("Ansicht")),
                (SettingsTab::Search, t("Suche & Index")),
                (SettingsTab::Media, t("Medien")),
                (SettingsTab::Shell, t("Integration")),
                (SettingsTab::Service, t("Dienst")),
                (SettingsTab::About, t("Über")),
            ] {
                ui.selectable_value(&mut self.settings_tab, tab, label);
            }
        });
        ui.separator();

        let before = self.cfg.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| match self.settings_tab {
                SettingsTab::View => self.settings_view(ui),
                SettingsTab::Search => self.settings_search(ui),
                SettingsTab::Media => self.settings_media(ui),
                SettingsTab::Shell => self.settings_shell(ui),
                SettingsTab::Service => self.settings_service(ui),
                SettingsTab::About => self.settings_about(ui),
            });
        if self.cfg != before {
            self.cfg.save();
        }
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        let sort_id = self.id_of("cfg_sort");
        section(ui, t("Sprache"), |ui| {
            let langs = crate::i18n::available();
            let current = crate::i18n::current();
            let selected = langs
                .iter()
                .find(|l| l.code == current)
                .map(|l| l.name.clone())
                .unwrap_or_else(|| current.clone());
            let mut pick = current.clone();
            egui::ComboBox::from_id_salt(self.id_of("lang"))
                .selected_text(selected)
                .width(200.0)
                .show_ui(ui, |ui| {
                    for l in &langs {
                        ui.selectable_value(&mut pick, l.code.clone(), &l.name);
                    }
                });
            if pick != current && crate::i18n::set(&pick) {
                self.cfg.lang = pick;
            }
            ui.label(
                RichText::new(t(
                    "Weitere Sprachen: eine Datei nach lang/ legen, z. B. lang/fr.lang. \
                     Was darin fehlt, bleibt deutsch.",
                ))
                .size(10.5)
                .color(theme::TEXT_DIM),
            );
        });
        section(ui, t("Beim Start"), |ui| {
            let mut key = self.cfg.sort_key;
            ui.horizontal(|ui| {
                ui.label(RichText::new(t("Sortierung")).size(11.5));
                egui::ComboBox::from_id_salt(sort_id)
                    .selected_text(sort_label(key))
                    .width(120.0)
                    .show_ui(ui, |ui| {
                        for k in [SortKey::Name, SortKey::Size, SortKey::Date, SortKey::Path] {
                            ui.selectable_value(&mut key, k, sort_label(k));
                        }
                    });
                let mut desc = self.cfg.sort_desc;
                if ui
                    .selectable_label(desc, if desc { "▼" } else { "▲" })
                    .on_hover_text(t("Absteigend"))
                    .clicked()
                {
                    desc = !desc;
                }
                self.cfg.sort_desc = desc;
            });
            self.cfg.sort_key = key;

            let mut first = self.cfg.start_first_drive;
            ui.horizontal(|ui| {
                ui.label(RichText::new(t("Laufwerk")).size(11.5));
                ui.selectable_value(&mut first, true, t("erstes"));
                ui.selectable_value(&mut first, false, t("letztes"));
            });
            self.cfg.start_first_drive = first;
            ui.label(
                RichText::new(t("Gilt für neu geöffnete Fenster"))
                    .size(10.5)
                    .color(theme::TEXT_DIM),
            );
        });
        section(ui, t("Listen"), |ui| {
            ui.checkbox(&mut self.cfg.show_icons, t("Echte Dateisymbole"))
                .on_hover_text(t("Aus: farbige Punkte nach Dateityp, ohne Shell-Abfragen"));
            ui.checkbox(&mut self.cfg.folders_first, t("Ordner gruppieren"))
                .on_hover_text(t(
                    "Ordner und Dateien getrennt sortieren — aufsteigend führen die Ordner",
                ));
            slider(ui, &mut self.cfg.tile_px, 48..=320, t("Kachelgröße"), "px");
        });
        section(ui, t("Diagramm"), |ui| {
            slider(ui, &mut self.cfg.chart_rings, 3..=12, t("Ringe im Kuchen"), "");
            slider(ui, &mut self.cfg.map_depth, 1..=8, t("Ebenen in der Treemap"), "");
            ui.checkbox(&mut self.cfg.animate, t("Übergänge animieren"))
                .on_hover_text(t("Aufblenden beim Hineinzoomen und Überblenden der Vorschau"));
        });
        section(ui, t("Vorschau"), |ui| {
            ui.checkbox(&mut self.cfg.preview_upscale, t("Kleine Bilder füllend skalieren"))
                .on_hover_text(t("Seitenverhältnis bleibt erhalten"));
            ui.checkbox(&mut self.cfg.text_preview, t("Textdateien im Klartext zeigen"));
        });
    }

    fn settings_search(&mut self, ui: &mut egui::Ui) {
        section(ui, t("Verhalten"), |ui| {
            ui.checkbox(&mut self.cfg.index_all_drives,
                t("Alle festen Laufwerke indizieren"),
            )
            .on_hover_text(t("Sonst nur die, die man tatsächlich öffnet"));
            ui.checkbox(&mut self.cfg.search_scoped,
                t("In einem Unterordner nur darunter suchen"),
            );
            ui.checkbox(
                &mut self.cfg.search_all_drives,
                t("Neue Fenster durchsuchen alle Laufwerke"),
            )
            .on_hover_text(t("Der Schalter in der Suchzeile gilt weiter für dieses Fenster"));
            slider(
                ui,
                &mut self.cfg.search_limit_k,
                10..=500,
                t("Treffer höchstens"),
                t("tausend"),
            );
        });

        section(ui, t("Indizierte Volumes"), |ui| {
            let rows: Vec<(usize, String, bool, u64, String)> = self
                .store
                .vols
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let ix = v.index.read();
                    let detail = if ix.vol.total > 0 {
                        tf(
                            "{0}{1} · {2} von {3} belegt",
                            &[
                                &ix.vol.fs,
                                &if ix.vol.label.is_empty() {
                                    String::new()
                                } else {
                                    format!(" „{}“", ix.vol.label)
                                },
                                &fmt::size(ix.vol.total.saturating_sub(ix.vol.free)),
                                &fmt::size(ix.vol.total),
                            ],
                        )
                    } else {
                        ix.vol.root_path.clone()
                    };
                    (i, v.title.clone(), v.live(), ix.total_files, detail)
                })
                .collect();
            let mut rescan = None;
            let mut drop_slot = None;
            egui::Grid::new(self.id_of("vols"))
                .num_columns(5)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for (i, title, live, files, detail) in rows {
                        ui.label(RichText::new(title).size(11.5));
                        ui.label(RichText::new(detail).size(11.0).color(theme::TEXT_DIM));
                        ui.label(
                            RichText::new(tf("{0} Dateien", &[&fmt::count(files)]))
                                .size(11.0)
                                .color(theme::TEXT_DIM),
                        );
                        ui.label(
                            RichText::new(if live {
                                t("● live")
                            } else {
                                t("○ statisch")
                            })
                                .size(11.0)
                                .color(if live { theme::GOOD } else { theme::TEXT_DIM }),
                        );
                        ui.horizontal(|ui| {
                            if ui.small_button(t("Neu scannen")).clicked() {
                                rescan = Some(i);
                            }
                            if ui.small_button(t("Entfernen")).clicked() {
                                drop_slot = Some(i);
                            }
                        });
                        ui.end_row();
                    }
                });
            if let Some(i) = rescan {
                let t = self.store.vols[i].target.clone();
                self.start_scan(t);
            }
            if let Some(i) = drop_slot {
                let key = self.store.vols[i].key.clone();
                let before = self.cfg.shares.len();
                self.cfg.shares.retain(|s| !s.eq_ignore_ascii_case(&key));
                if self.cfg.shares.len() != before {
                    self.cfg.save();
                }
                self.svc.send(client::Cmd::Forget(key));
                self.store.remove(i);
                self.chart.invalidate();
                self.map.invalidate();
            }
        });

        section(ui, t("Netzwerkpfad hinzufügen"), |ui| {
            ui.label(
                RichText::new(t("UNC-Freigaben lassen sich direkt indizieren, z. B. \\\\fatboy\\daten."))
                    .size(11.5)
                    .color(theme::TEXT_DIM),
            );
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.net_path)
                        .hint_text(t("\\\\server\\freigabe"))
                        .desired_width(260.0),
                );
                let ok = self.net_path.trim().len() > 3;
                if ui.add_enabled(ok, egui::Button::new(t("Indizieren"))).clicked() {
                    let p = self.net_path.trim().to_string();
                    self.net_path.clear();
                    self.open_target(Target::Path(p));
                }
            });
        });
    }

    fn settings_media(&mut self, ui: &mut egui::Ui) {
        let available = crate::media::available();
        section(ui, t("Wiedergabe"), |ui| {
            if !available {
                ui.label(
                    RichText::new(
                        t("libVLC nicht gefunden — es bleibt beim Standbild. VLC installieren \
                           schaltet die Wiedergabe frei."),
                    )
                    .size(11.5)
                    .color(theme::WARN),
                );
            }
            ui.add_enabled_ui(available, |ui| {
                ui.checkbox(&mut self.cfg.autoplay, t("Automatisch abspielen"))
                    .on_hover_text(t("Sobald eine Audio- oder Videodatei ausgewählt wird"));
                ui.checkbox(&mut self.cfg.loop_media, t("Endlos wiederholen"));
                let mut vol = self.cfg.volume.clamp(0, 100) as u32;
                if slider(ui, &mut vol, 0..=100, t("Lautstärke"), "%") {
                    self.cfg.volume = vol as i32;
                    self.media.volume = vol as i32;
                    self.media.apply_volume();
                }
            });
        });
        section(ui, t("Vorschaubilder"), |ui| {
            ui.label(
                RichText::new(
                    t("Kommen vom Shell-Anbieter — dieselben, die der Explorer zeigt: Bilder, \
                       Videobilder, PDF-Erstseiten, Cover."),
                )
                .size(11.5)
                .color(theme::TEXT_DIM),
            );
            if ui.button(t("Zwischenspeicher leeren")).clicked() {
                *self.thumbs.borrow_mut() = preview::Thumbs::new();
                *self.icons.borrow_mut() = preview::Icons::default();
            }
        });
    }

    fn settings_shell(&mut self, ui: &mut egui::Ui) {
        section(ui, t("Explorer-Kontextmenü"), |ui| {
            ui.label(
                RichText::new(
                    t("Fügt „Mit Diskalize öffnen“ beim Rechtsklick auf Ordner, Laufwerke und \
                     Netzwerkorte hinzu — nur für dich, ohne Adminrechte."),
                )
                .size(11.5)
                .color(theme::TEXT_DIM),
            );
            let installed = shell::is_installed();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!installed, egui::Button::new(t("Eintragen")))
                    .clicked()
                {
                    self.notice = match shell::install() {
                        Ok(()) => Some((t("Kontextmenü eingetragen").into(), false)),
                        Err(e) => Some((e, true)),
                    };
                }
                if ui
                    .add_enabled(installed, egui::Button::new(t("Entfernen")))
                    .clicked()
                {
                    self.notice = match shell::uninstall() {
                        Ok(()) => Some((t("Kontextmenü entfernt").into(), false)),
                        Err(e) => Some((e, true)),
                    };
                }
                ui.label(
                    RichText::new(if installed {
                        t("✔ aktiv")
                    } else {
                        t("nicht aktiv")
                    })
                        .size(11.5)
                        .color(if installed { theme::GOOD } else { theme::TEXT_DIM }),
                );
            });
        });

        section(ui, t("Fenster"), |ui| {
            let mut autostart = shell::autostart_enabled();
            if ui
                .checkbox(&mut autostart, t("Mit Windows starten"))
                .on_hover_text(t("Nur die Oberfläche — der Dienst startet ohnehin mit Windows"))
                .changed()
            {
                self.notice = shell::set_autostart(autostart).err().map(|e| (e, true));
            }
            ui.checkbox(&mut self.cfg.multi_instance,
                t("Mehrere Fenster erlauben"),
            )
            .on_hover_text(
                t("Aus: ein weiterer Start reicht seinen Pfad an das laufende Fenster durch"),
            );
            ui.checkbox(&mut self.cfg.close_to_tray,
                t("Schließen legt ins Infobereich-Symbol"),
            );
            ui.add_enabled_ui(self.root, |ui| {
                if ui
                    .button(t("Neues Fenster öffnen"))
                    .on_hover_text(t(
                        "Im selben Prozess — teilt Index, Vorschaubilder und Grafikgerät",
                    ))
                    .clicked()
                {
                    self.open_window = true;
                }
            });
        });

        section(ui, t("Globales Tastenkürzel"), |ui| {
            ui.checkbox(&mut self.cfg.hotkey_enabled, t("Aktiv"))
                .on_hover_text(t("Holt Diskalize nach vorn und setzt den Cursor ins Suchfeld"));
            ui.horizontal(|ui| {
                let enabled = self.cfg.hotkey_enabled;
                ui.add_enabled_ui(enabled, |ui| {
                    if self.capturing_hotkey {
                        ui.label(
                            RichText::new(t("Tastenkombination drücken…"))
                                .size(11.5)
                                .color(theme::ACCENT),
                        );
                        if ui.small_button(t("Abbrechen")).clicked() {
                            self.capturing_hotkey = false;
                        }
                    } else {
                        ui.label(RichText::new(self.cfg.hotkey_label()).size(11.5).monospace());
                        if ui.small_button(t("Ändern")).clicked() {
                            self.capturing_hotkey = true;
                        }
                    }
                });
            });
            if self.cfg.hotkey_enabled && self.tray.as_ref().is_some_and(|t| !t.hotkey_ok()) {
                ui.label(
                    RichText::new(t("Das Kürzel ist von einem anderen Programm belegt."))
                        .size(11.0)
                        .color(theme::WARN),
                );
            }
        });
    }

    fn settings_service(&mut self, ui: &mut egui::Ui) {
        let installed = crate::service::is_installed();
        let running = installed && crate::service::is_running();
        let connected = self.svc.connected();

        section(ui, t("Status"), |ui| {
            let (text, colour) = match (installed, running, connected) {
                (false, ..) => (t("Nicht installiert."), theme::WARN),
                (_, false, _) => (t("Installiert, aber gestoppt."), theme::WARN),
                (_, _, false) => (t("Läuft, Verbindung wird aufgebaut …"), theme::WARN),
                _ => (
                    t("Läuft und ist verbunden. Die Oberfläche braucht keine Sonderrechte."),
                    theme::GOOD,
                ),
            };
            ui.label(RichText::new(text).size(11.5).color(colour));
            ui.horizontal(|ui| {
                if ui.button(t("Installieren / starten")).clicked() {
                    self.run_service_verb("--install");
                }
                if ui
                    .add_enabled(installed, egui::Button::new(t("Entfernen")))
                    .clicked()
                {
                    self.run_service_verb("--uninstall");
                }
            });
            if let Some((msg, err)) = &self.service_busy {
                ui.label(
                    RichText::new(msg)
                        .size(11.0)
                        .color(if *err { theme::WARN } else { theme::TEXT_DIM }),
                );
            }
        });

        section(ui, t("Speicher"), |ui| {
            let entries: usize = self.store.vols.iter().map(|v| v.index.read().len()).sum();
            ui.label(
                RichText::new(tf(
                    "{0} Einträge in {1} Volume(s).",
                    &[
                        &fmt::count(entries as u64),
                        &self.store.vols.len().to_string(),
                    ],
                ))
                .size(11.5),
            );
            ui.label(
                RichText::new(
                    t("Jeder Index liegt einmal im gemeinsamen Speicher. Der Dienst schreibt \
                       hinein, die Oberflächen lesen dieselben Seiten — mehrere Fenster kosten \
                       also keinen zusätzlichen Index."),
                )
                .size(11.0)
                .color(theme::TEXT_DIM),
            );
        });
    }

    /// Version, the numbers worth knowing, and who wrote what.
    fn settings_about(&mut self, ui: &mut egui::Ui) {
        let built = BUILD_UNIX.parse::<u32>().unwrap_or(0);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(t("Diskalize")).size(19.0).color(theme::ACCENT));
            ui.label(
                RichText::new(format!(
                    "v{VERSION} · {} · {} · {GIT_REV}",
                    build_profile(),
                    fmt::timestamp(built)
                ))
                .size(10.5)
                .color(theme::TEXT_DIM),
            );
        });

        // Everything measurable in one row: how fast it draws, what it holds,
        // what it has indexed.
        let (avg, worst) = self.frame_stats();
        let me = win::process_memory(0).unwrap_or_default();
        let svc = win::find_process("diskalize-service.exe")
            .and_then(win::process_memory)
            .unwrap_or_default();
        let (files, dirs) = self.store.totals();
        let entries: u64 = self.store.vols.iter().map(|v| v.index.read().len() as u64).sum();

        ui.add_space(6.0);
        egui::Frame::default()
            .fill(theme::PANEL)
            .corner_radius(6)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // Continuous frames while the graph is on screen, otherwise it
                // only fills in when something else happens to redraw.
                ui.ctx().request_repaint();
                ui.horizontal(|ui| {
                    let col = if worst > 33.0 {
                        theme::WARN
                    } else if worst > 20.0 {
                        theme::TEXT
                    } else {
                        theme::GOOD
                    };
                    stat(ui, t("Bildzeit"), &format!("{avg:.1} ms"), col);
                    stat(
                        ui,
                        t("Spitze"),
                        &format!("{worst:.1} ms"),
                        theme::TEXT_DIM,
                    );
                    stat(ui, t("Oberfläche"), &fmt::size(me.working_set), theme::TEXT);
                    stat(ui, t("Dienst"), &fmt::size(svc.working_set), theme::TEXT);
                });
                self.frame_graph(ui);
            });

        ui.add_space(6.0);
        egui::Frame::default()
            .fill(theme::PANEL)
            .corner_radius(6)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(tf(
                        "{0} Volumes · {1} Einträge · {2} Dateien · {3} Ordner",
                        &[
                            &self.store.vols.len().to_string(),
                            &fmt::count(entries),
                            &fmt::count(files),
                            &fmt::count(dirs),
                        ],
                    ))
                    .size(11.5),
                );
                for v in &self.store.vols {
                    let ix = v.index.read();
                    ui.label(
                        RichText::new(tf(
                            "{0} — {1} Dateien, {2}{3}{4}",
                            &[
                                &v.title,
                                &fmt::count(ix.total_files),
                                if ix.vol.method_mft {
                                    "MFT"
                                } else {
                                    t("Verzeichnislauf")
                                },
                                &if ix.vol.scan_ms > 0 {
                                    format!(" {}", tf("in {0}", &[&fmt::duration(ix.vol.scan_ms)]))
                                } else {
                                    String::new()
                                },
                                if v.live() { t(", live") } else { "" },
                            ],
                        ))
                        .size(10.5)
                        .color(theme::TEXT_DIM),
                    );
                }
            });

        ui.add_space(10.0);
        ui.label(RichText::new(t("Idee und Realisation")).size(11.0).color(theme::TEXT_DIM));
        ui.label(RichText::new(t("Ize")).size(14.0).strong().color(theme::ACCENT));

        ui.add_space(8.0);
        ui.label(RichText::new(t("Freie Software")).size(11.0).color(theme::TEXT_DIM));
        ui.add_space(2.0);
        ui.label(
            RichText::new(CREDITS.join(" · "))
                .size(11.0)
                .color(theme::TEXT),
        );

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui.button(t("Diagnose kopieren")).clicked() {
                let text = self.diagnostics();
                ui.ctx().copy_text(text);
                self.notice = Some((t("Diagnose in der Zwischenablage").into(), false));
            }
            if ui.button(t("Einstellungsordner")).clicked() {
                if let Some(dir) = std::env::var_os("APPDATA") {
                    let p = std::path::Path::new(&dir).join("Diskalize");
                    let _ = std::process::Command::new("explorer.exe").arg(p).spawn();
                }
            }
        });
        ui.add_space(6.0);
    }

    /// Mean and worst frame time over the ring, ignoring slots never written.
    fn frame_stats(&self) -> (f32, f32) {
        let used: Vec<f32> = self.frames.iter().copied().filter(|v| *v > 0.0).collect();
        if used.is_empty() {
            return (0.0, 0.0);
        }
        let avg = used.iter().sum::<f32>() / used.len() as f32;
        let worst = used.iter().copied().fold(0.0f32, f32::max);
        (avg, worst)
    }

    fn frame_graph(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width().min(420.0), 42.0),
            egui::Sense::hover(),
        );
        let p = ui.painter_at(rect);
        p.rect_filled(rect, egui::CornerRadius::same(4), theme::BG);
        // Fixed 40 ms ceiling: a scale that follows the data would make every
        // graph look the same, which is the opposite of the point.
        let ceiling = 40.0f32;
        let w = rect.width() / FRAME_SAMPLES as f32;
        for k in 0..FRAME_SAMPLES {
            // Oldest first, so the newest frame sits at the right edge.
            let i = (self.frame_i + k) % FRAME_SAMPLES;
            let ms = self.frames[i];
            if ms <= 0.0 {
                continue;
            }
            let h = (ms / ceiling).min(1.0) * (rect.height() - 4.0);
            let x = rect.left() + k as f32 * w;
            p.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x, rect.bottom() - 2.0 - h),
                    egui::pos2(x + w.max(1.0) - 0.5, rect.bottom() - 2.0),
                ),
                egui::CornerRadius::ZERO,
                if ms > 33.0 {
                    theme::WARN
                } else if ms > 20.0 {
                    theme::ACCENT.gamma_multiply(0.55)
                } else {
                    theme::ACCENT
                },
            );
        }
        let y = rect.bottom() - 2.0 - (16.7 / ceiling) * (rect.height() - 4.0);
        p.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(1.0, theme::LINE),
        );
    }

    /// One pasteable block for bug reports.
    fn diagnostics(&self) -> String {
        let (avg, worst) = self.frame_stats();
        let me = win::process_memory(0).unwrap_or_default();
        let svc = win::find_process("diskalize-service.exe")
            .and_then(win::process_memory)
            .unwrap_or_default();
        let mut out = format!(
            "Diskalize {VERSION} ({}) rev {GIT_REV}\n\
             Bild: {avg:.1} ms Mittel, {worst:.1} ms längstes\n\
             Oberfläche: {} / {} privat\n\
             Dienst: {} / {} privat · installiert={} läuft={} verbunden={}\n",
            build_profile(),
            fmt::size(me.working_set),
            fmt::size(me.private),
            fmt::size(svc.working_set),
            fmt::size(svc.private),
            crate::service::is_installed(),
            crate::service::is_running(),
            self.svc.connected(),
        );
        for v in &self.store.vols {
            let ix = v.index.read();
            out.push_str(&format!(
                "{}: {} Einträge, {} Dateien, {}, {} in {}, live={}\n",
                v.title,
                fmt::count(ix.len() as u64),
                fmt::count(ix.total_files),
                if ix.is_ready() {
                    fmt::size(ix.size[ix.root as usize])
                } else {
                    "—".into()
                },
                if ix.vol.method_mft { "MFT" } else { "Walker" },
                fmt::duration(ix.vol.scan_ms),
                v.live(),
            ));
        }
        out
    }

    fn apply_hotkey(&self) {
        if let Some(t) = &self.tray {
            t.set_hotkey(
                self.cfg
                    .hotkey_enabled
                    .then_some((self.cfg.hotkey_mods, self.cfg.hotkey_vk)),
            );
        }
    }

    /// While recording, the next real key press becomes the global hotkey.
    fn capture_hotkey(&mut self, ctx: &egui::Context) {
        if !self.capturing_hotkey {
            return;
        }
        let pressed = ctx.input(|i| {
            let m = i.modifiers;
            i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key, pressed: true, ..
                } => Some((*key, m)),
                _ => None,
            })
        });
        let Some((key, mods)) = pressed else { return };
        if key == egui::Key::Escape {
            self.capturing_hotkey = false;
            return;
        }
        if let Some((m, vk)) = crate::settings::Settings::combo_from_egui(key, mods) {
            self.capturing_hotkey = false;
            self.cfg.hotkey_mods = m;
            self.cfg.hotkey_vk = vk;
            self.cfg.hotkey_enabled = true;
            self.cfg.save();
            self.apply_hotkey();
        }
    }

    fn resort(&mut self) {
        let vols = self.store.snapshot(None);
        store::sort_hits(&vols, &mut self.results, self.sort, self.sort_desc, self.cfg.folders_first);
        self.browse_key = (u32::MAX, 0, 0, false);
        if !self.find_lines.is_empty() {
            self.rebuild_find_order();
        }
    }

    /// Breadcrumb variant shown above search results: where the search applies.
    fn breadcrumb_for_search(&mut self, ui: &mut egui::Ui) {
        let scope = self.store.active.and_then(|slot| {
            let ix = self.store.vols[slot].index.read();
            (self.view_root != ix.root && self.view_root != NONE)
                .then(|| ix.path_of(self.view_root))
        });
        let mut widen = false;
        ui.horizontal(|ui| match (&scope, self.global_search) {
            (_, true) => {
                ui.label(
                    RichText::new(t("Suche über alle indizierten Laufwerke"))
                        .size(12.0)
                        .color(theme::TEXT_DIM),
                );
            }
            (Some(p), false) => {
                ui.label(RichText::new(t("Suche in")).size(12.0).color(theme::TEXT_DIM));
                ui.label(RichText::new(p).size(12.0).color(theme::ACCENT));
                if ui.small_button(t("ganzes Laufwerk")).clicked() {
                    widen = true;
                }
            }
            (None, false) => {
                ui.label(
                    RichText::new(t("Suche im aktiven Laufwerk"))
                        .size(12.0)
                        .color(theme::TEXT_DIM),
                );
            }
        });
        if widen {
            if let Some(index) = self.active_index().cloned() {
                let root = index.read().root;
                self.view_root = root;
                self.history.clear();
                self.forward.clear();
                self.query_dirty = true;
            }
        }
        ui.add_space(2.0);
    }

    /// Drops a deleted entry from the visible lists.
    ///
    /// The index itself is shared memory owned by the service and read-only
    /// here; its USN watcher removes the entry within a fraction of a second.
    /// This only keeps the selection and the current lists from pointing at
    /// something that is already gone.
    fn after_delete(&mut self, hit: Hit, hits: &[Hit]) {
        let next = hits
            .iter()
            .position(|h| *h == hit)
            .and_then(|p| {
                hits.get(p + 1)
                    .or_else(|| p.checked_sub(1).and_then(|q| hits.get(q)))
            })
            .copied();
        self.sel_hit = next;
        self.results.retain(|h| *h != hit);
        self.browse.retain(|h| *h != hit);
        self.chart.invalidate();
        self.map.invalidate();
    }

    fn open_hit(&mut self, h: Hit) {
        let is_dir = self
            .store
            .index_of(h.vol)
            .map(|ix| ix.read().is_dir(h.idx))
            .unwrap_or(false);
        if is_dir {
            // Opening a folder means navigating to it, the same way Explorer
            // moves into the folder you double-click.
            if self.store.active != Some(h.vol as usize) {
                self.activate(h.vol as usize);
            }
            self.query.clear();
            self.query_dirty = true;
            self.zoom_to(h.idx);
        } else if let Some(p) = self.path_of(h) {
            shell::open(&p);
        }
    }

    fn results_view(&mut self, ui: &mut egui::Ui) {
        if !self.query.trim().is_empty() {
            self.breadcrumb_for_search(ui);
        } else {
            let index = self.active_index().cloned();
            if let Some(ix) = index {
                self.breadcrumb(ui, &ix);
            }
            ui.add_space(2.0);
        }

        self.results_keys(ui);
        let mut hits: Vec<Hit> = self.list_hits().to_vec();
        self.apply_find(&mut hits);
        if hits.is_empty() {
            tree::empty_hint(
                ui,
                if self.search_rx.is_some() || self.find_progress.is_some() {
                    t("suche…")
                } else if !self.find_text.trim().is_empty() {
                    t("kein Treffer im Dateiinhalt")
                } else if self.query.trim().is_empty() {
                    t("Ordner ist leer")
                } else {
                    t("keine Treffer")
                },
            );
            return;
        }

        // Read-lock every volume the hits reference, once, for the whole list.
        let vols: Vec<(u16, Arc<RwLock<Index>>)> = self.store.snapshot(None);
        let guards: Vec<(u16, parking_lot::RwLockReadGuard<'_, Index>)> =
            vols.iter().map(|(s, ix)| (*s, ix.read())).collect();
        let hctx = tree::HitCtx { indexes: &guards };

        // Nothing picked yet (fresh folder, or the old pick is not in this list):
        // select the first row so the detail pane has something to show.
        let mut sel = self.sel_hit.filter(|h| hits.contains(h));
        if sel.is_none() {
            sel = hits.first().copied();
        }
        let params = tree::ListParams {
            sort: self.sort,
            desc: self.sort_desc,
            mode: self.view.list_mode(),
            tile_px: self.cfg.tile_px,
            scroll_to: self.scroll_to.take(),
            icons: self.cfg.show_icons,
            animate: self.cfg.animate,
        };
        let act = tree::results(
            ui,
            &hctx,
            &hits,
            &mut sel,
            &params,
            &mut self.thumbs.borrow_mut(),
            &mut self.icons.borrow_mut(),
            &mut self.columns,
        );
        // Dragged column widths belong in the settings file, not just this run.
        if self.columns.name != self.cfg.col_name
            || self.columns.size != self.cfg.col_size
            || self.columns.date != self.cfg.col_date
        {
            self.cfg.col_name = self.columns.name;
            self.cfg.col_size = self.columns.size;
            self.cfg.col_date = self.columns.date;
            self.cfg.save();
        }
        drop(guards);
        self.sel_hit = sel;

        if let Some(k) = act.sort {
            if self.sort == k {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort = k;
                self.sort_desc = k == SortKey::Size || k == SortKey::Date;
            }
            store::sort_hits(&vols, &mut self.results, self.sort, self.sort_desc, self.cfg.folders_first);
            self.browse_key = (u32::MAX, 0, 0, false);
        }
        if let Some(c) = act.context {
            if let Some(p) = self.path_of(c) {
                self.shell_menu(&p);
            }
        }
        if let Some(h) = act.open {
            self.open_hit(h);
        }
        let _ = act.focus;
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if !ctx.egui_wants_keyboard_input()
            && ctx.input(|i| i.key_pressed(egui::Key::Backspace))
            && self.store.active.is_some()
            && self.query.trim().is_empty()
        {
            self.go_up();
        }

        if self.show_help {
            egui::Window::new(t("Suchsyntax"))
                .id(self.id_of("help_window"))
                .collapsible(false)
                .default_width(440.0)
                .open(&mut self.show_help)
                .show(ctx, |ui| {
                    ui.label(RichText::new(search::syntax_help()).monospace().size(12.0));
                    ui.separator();
                    ui.label(
                        RichText::new(
                            t("Diagramm: Klick = hineinzoomen · Mitte oder Rücktaste = zurück · \
                               Doppelklick auf eine Datei öffnet sie\n\
                               Baum: Doppelklick klappt auf und zu\n\
                               Rechtsklick zeigt überall das Explorer-Kontextmenü"),
                        )
                        .size(11.5)
                        .color(theme::TEXT_DIM),
                    );
                });
        }

        if self.show_settings {
            let mut open = true;
            let screen = ctx.content_rect();
            let mut win = egui::Window::new(t("Einstellungen"))
                .id(self.id_of("settings_window"))
                .collapsible(false)
                .default_width(560.0)
                .max_height((screen.height() - 80.0).max(320.0))
                .pivot(egui::Align2::CENTER_CENTER)
                .open(&mut open);
            // Pin it to the middle on the frame it opens. `default_pos` only
            // applies the very first time, so without this it reappears
            // wherever it was last dragged — or offset by egui's cascade.
            if !self.settings_was_open {
                win = win.current_pos(screen.center());
            }
            win.show(ctx, |ui| self.settings_body(ui));
            self.show_settings = open;
        }
        self.settings_was_open = self.show_settings;
    }
}

#[cfg(test)]
mod tests {
    /// The About page shows these verbatim; a missing stamp would quietly turn
    /// the build date into 1970.
    #[test]
    fn build_stamp_is_present() {
        let unix: u32 = super::BUILD_UNIX.parse().expect("build stamp must be a number");
        assert!(unix > 1_700_000_000, "stamp looks unset: {unix}");
        assert!(!super::GIT_REV.is_empty());
        assert!(!super::VERSION.is_empty());
    }

}
