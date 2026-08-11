//! Audio and video playback through libVLC.
//!
//! libVLC is loaded at runtime rather than linked, so Diskalize builds and runs
//! on machines without VLC — the preview simply falls back to the still frame
//! the shell provides.
//!
//! Video never touches a child window: VLC renders into our own buffer through
//! its video callbacks and the frames become an egui texture. That keeps the
//! player inside the normal paint order, so it scrolls, resizes and layers like
//! any other widget instead of floating above everything.

use std::cell::UnsafeCell;
use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use libloading::{Library, Symbol};
use parking_lot::Mutex;

type Inst = *mut c_void;
type Media = *mut c_void;
type Player = *mut c_void;

type FnNew = unsafe extern "C" fn(c_int, *const *const c_char) -> Inst;
type FnRelease = unsafe extern "C" fn(*mut c_void);
type FnMediaNewPath = unsafe extern "C" fn(Inst, *const c_char) -> Media;
type FnPlayerFromMedia = unsafe extern "C" fn(Media) -> Player;
type FnPlayerVoid = unsafe extern "C" fn(Player);
type FnPlayerInt = unsafe extern "C" fn(Player) -> c_int;
type FnSetVolume = unsafe extern "C" fn(Player, c_int) -> c_int;
type FnGetPosition = unsafe extern "C" fn(Player) -> f32;
type FnSetPosition = unsafe extern "C" fn(Player, f32);
type FnGetLength = unsafe extern "C" fn(Player) -> i64;
type FnGetTime = unsafe extern "C" fn(Player) -> i64;
type FnMediaAddOption = unsafe extern "C" fn(Media, *const c_char);

type LockCb = unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> *mut c_void;
type UnlockCb = unsafe extern "C" fn(*mut c_void, *mut c_void, *const *mut c_void);
type DisplayCb = unsafe extern "C" fn(*mut c_void, *mut c_void);
type SetupCb = unsafe extern "C" fn(
    *mut *mut c_void,
    *mut c_char,
    *mut c_uint,
    *mut c_uint,
    *mut c_uint,
    *mut c_uint,
) -> c_uint;
type CleanupCb = unsafe extern "C" fn(*mut c_void);

type FnVideoSetCallbacks =
    unsafe extern "C" fn(Player, Option<LockCb>, Option<UnlockCb>, Option<DisplayCb>, *mut c_void);
type FnVideoSetFormatCallbacks = unsafe extern "C" fn(Player, Option<SetupCb>, Option<CleanupCb>);

struct Api {
    _lib: Library,
    new: FnNew,
    release: FnRelease,
    media_new_path: FnMediaNewPath,
    media_add_option: FnMediaAddOption,
    media_release: FnRelease,
    player_from_media: FnPlayerFromMedia,
    player_release: FnRelease,
    play: FnPlayerInt,
    pause: FnPlayerVoid,
    stop: FnPlayerVoid,
    is_playing: FnPlayerInt,
    set_volume: FnSetVolume,
    get_position: FnGetPosition,
    set_position: FnSetPosition,
    get_length: FnGetLength,
    get_time: FnGetTime,
    video_set_callbacks: FnVideoSetCallbacks,
    video_set_format_callbacks: FnVideoSetFormatCallbacks,
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// Standard install locations, then whatever is on PATH.
fn libvlc_dir() -> Option<std::path::PathBuf> {
    for base in [
        r"C:\Program Files\VideoLAN\VLC",
        r"C:\Program Files (x86)\VideoLAN\VLC",
    ] {
        let p = std::path::Path::new(base);
        if p.join("libvlc.dll").exists() {
            return Some(p.to_path_buf());
        }
    }
    None
}

fn load() -> Option<Api> {
    let dir = libvlc_dir()?;
    // libVLC finds its codecs through this; without it `libvlc_new` returns null.
    unsafe { std::env::set_var("VLC_PLUGIN_PATH", dir.join("plugins")) };
    // libvlccore.dll sits next to it and must resolve first.
    let _core = unsafe { Library::new(dir.join("libvlccore.dll")) }.ok();
    let lib = unsafe { Library::new(dir.join("libvlc.dll")) }.ok()?;

    macro_rules! sym {
        ($n:literal, $t:ty) => {{
            let s: Symbol<$t> = unsafe { lib.get($n) }.ok()?;
            *s
        }};
    }
    let api = Api {
        new: sym!(b"libvlc_new\0", FnNew),
        release: sym!(b"libvlc_release\0", FnRelease),
        media_new_path: sym!(b"libvlc_media_new_path\0", FnMediaNewPath),
        media_add_option: sym!(b"libvlc_media_add_option\0", FnMediaAddOption),
        media_release: sym!(b"libvlc_media_release\0", FnRelease),
        player_from_media: sym!(b"libvlc_media_player_new_from_media\0", FnPlayerFromMedia),
        player_release: sym!(b"libvlc_media_player_release\0", FnRelease),
        play: sym!(b"libvlc_media_player_play\0", FnPlayerInt),
        pause: sym!(b"libvlc_media_player_pause\0", FnPlayerVoid),
        stop: sym!(b"libvlc_media_player_stop\0", FnPlayerVoid),
        is_playing: sym!(b"libvlc_media_player_is_playing\0", FnPlayerInt),
        set_volume: sym!(b"libvlc_audio_set_volume\0", FnSetVolume),
        get_position: sym!(b"libvlc_media_player_get_position\0", FnGetPosition),
        set_position: sym!(b"libvlc_media_player_set_position\0", FnSetPosition),
        get_length: sym!(b"libvlc_media_player_get_length\0", FnGetLength),
        get_time: sym!(b"libvlc_media_player_get_time\0", FnGetTime),
        video_set_callbacks: sym!(b"libvlc_video_set_callbacks\0", FnVideoSetCallbacks),
        video_set_format_callbacks: sym!(
            b"libvlc_video_set_format_callbacks\0",
            FnVideoSetFormatCallbacks
        ),
        _lib: lib,
    };
    Some(api)
}

static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();

fn api() -> Option<&'static Api> {
    API.get_or_init(load).as_ref()
}

pub fn available() -> bool {
    api().is_some()
}

/// The player currently running, as a raw pointer, plus a flag saying it was
/// silenced from outside the UI thread.
///
/// Hiding the window to the tray happens in a window procedure, where no egui
/// frame follows — so the usual "stop when the pane goes away" path never runs.
/// This lets any thread stop playback immediately; the UI then releases the
/// player properly the next time it does get a frame.
static ACTIVE: Mutex<usize> = Mutex::new(0);
static SILENCED: AtomicBool = AtomicBool::new(false);

pub fn stop_all() {
    let Some(a) = api() else { return };
    let guard = ACTIVE.lock();
    if *guard != 0 {
        unsafe { (a.stop)(*guard as Player) };
        SILENCED.store(true, Ordering::Release);
    }
}

/// True once, if playback was stopped from outside.
pub fn take_silenced() -> bool {
    SILENCED.swap(false, Ordering::AcqRel)
}

/// Watches whether the window is still on screen and silences playback if not.
///
/// Belt and braces on purpose. Hiding to the tray and minimising both stop egui
/// frames, and relying on a window-procedure hook alone proved fragile — this
/// thread owes nothing to the message loop or the render loop and catches every
/// way the window can leave the screen, including ones we did not think of.
pub fn watch_window_visibility() {
    std::thread::Builder::new()
        .name("media-guard".into())
        .spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let playing = *ACTIVE.lock() != 0;
            if playing && !crate::win::main_window_on_screen() {
                stop_all();
            }
        })
        .ok();
}

/// Frame buffers shared with VLC's decoder threads.
struct Frames {
    /// Written by VLC between `lock` and `unlock`.
    back: UnsafeCell<Vec<u8>>,
    /// Handed to the UI; swapped in on `display`.
    front: Mutex<Vec<u8>>,
    /// Width in the low 32 bits, height in the high 32.
    dims: AtomicU64,
    seq: AtomicU64,
}

unsafe impl Send for Frames {}
unsafe impl Sync for Frames {}

unsafe extern "C" fn cb_setup(
    opaque: *mut *mut c_void,
    chroma: *mut c_char,
    width: *mut c_uint,
    height: *mut c_uint,
    pitches: *mut c_uint,
    lines: *mut c_uint,
) -> c_uint {
    unsafe {
        let f = &*((*opaque) as *const Frames);
        let (w, h) = (*width, *height);
        // BGRA, one plane; egui wants straight RGBA so the swap happens on upload.
        std::ptr::copy_nonoverlapping(b"RV32".as_ptr() as *const c_char, chroma, 4);
        *pitches = w * 4;
        *lines = h;
        let need = (w as usize) * (h as usize) * 4;
        (*f.back.get()).resize(need, 0);
        f.front.lock().resize(need, 0);
        f.dims
            .store((w as u64) | ((h as u64) << 32), Ordering::Release);
        1
    }
}

unsafe extern "C" fn cb_cleanup(_opaque: *mut c_void) {}

unsafe extern "C" fn cb_lock(opaque: *mut c_void, planes: *mut *mut c_void) -> *mut c_void {
    unsafe {
        let f = &*(opaque as *const Frames);
        *planes = (*f.back.get()).as_mut_ptr() as *mut c_void;
    }
    std::ptr::null_mut()
}

unsafe extern "C" fn cb_unlock(_o: *mut c_void, _pic: *mut c_void, _planes: *const *mut c_void) {}

unsafe extern "C" fn cb_display(opaque: *mut c_void, _pic: *mut c_void) {
    unsafe {
        let f = &*(opaque as *const Frames);
        // Safe to swap here: VLC only writes between lock and unlock, and
        // display always follows unlock for the same picture.
        let mut front = f.front.lock();
        std::mem::swap(&mut *front, &mut *f.back.get());
        f.seq.fetch_add(1, Ordering::Release);
    }
}

pub struct Player_ {
    inst: Inst,
    media: Media,
    player: Player,
    frames: Arc<Frames>,
    last_seq: u64,
    pub path: String,
    pub looping: Arc<AtomicBool>,
    pub has_video: bool,
}

impl Drop for Player_ {
    fn drop(&mut self) {
        if let Some(a) = api() {
            // Same lock `stop_all` takes, so another thread cannot be stopping
            // this player while it is being torn down.
            let mut guard = ACTIVE.lock();
            if *guard == self.player as usize {
                *guard = 0;
            }
            unsafe {
                (a.stop)(self.player);
                (a.player_release)(self.player);
                (a.media_release)(self.media);
                (a.release)(self.inst);
            }
        }
    }
}

impl Player_ {
    pub fn open(path: &str, looping: bool, volume: i32) -> Option<Player_> {
        let a = api()?;
        let args: [&[u8]; 3] = [b"--no-xlib\0", b"--quiet\0", b"--no-video-title-show\0"];
        let argv: Vec<*const c_char> = args.iter().map(|s| s.as_ptr() as *const c_char).collect();
        let inst = unsafe { (a.new)(argv.len() as c_int, argv.as_ptr()) };
        if inst.is_null() {
            return None;
        }
        let cpath = CString::new(path).ok()?;
        let media = unsafe { (a.media_new_path)(inst, cpath.as_ptr()) };
        if media.is_null() {
            unsafe { (a.release)(inst) };
            return None;
        }
        if looping {
            // VLC has no per-player loop flag; the media option is the way.
            let opt = CString::new("input-repeat=65535").unwrap();
            unsafe { (a.media_add_option)(media, opt.as_ptr()) };
        }
        let player = unsafe { (a.player_from_media)(media) };
        if player.is_null() {
            unsafe {
                (a.media_release)(media);
                (a.release)(inst);
            }
            return None;
        }

        let frames = Arc::new(Frames {
            back: UnsafeCell::new(Vec::new()),
            front: Mutex::new(Vec::new()),
            dims: AtomicU64::new(0),
            seq: AtomicU64::new(0),
        });
        let opaque = Arc::as_ptr(&frames) as *mut c_void;
        unsafe {
            (a.video_set_format_callbacks)(player, Some(cb_setup), Some(cb_cleanup));
            (a.video_set_callbacks)(
                player,
                Some(cb_lock),
                Some(cb_unlock),
                Some(cb_display),
                opaque,
            );
            (a.set_volume)(player, volume.clamp(0, 100));
            (a.play)(player);
        }
        *ACTIVE.lock() = player as usize;

        Some(Player_ {
            inst,
            media,
            player,
            frames,
            last_seq: 0,
            path: path.to_string(),
            looping: Arc::new(AtomicBool::new(looping)),
            has_video: false,
        })
    }

    /// Latest decoded frame as straight RGBA, or `None` if nothing is new.
    pub fn take_frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        let seq = self.frames.seq.load(Ordering::Acquire);
        if seq == self.last_seq {
            return None;
        }
        self.last_seq = seq;
        let d = self.frames.dims.load(Ordering::Acquire);
        let (w, h) = ((d & 0xFFFF_FFFF) as u32, (d >> 32) as u32);
        if w == 0 || h == 0 {
            return None;
        }
        let mut buf = self.frames.front.lock().clone();
        if buf.len() < (w as usize) * (h as usize) * 4 {
            return None;
        }
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2); // BGRA -> RGBA
            px[3] = 255; // RV32 leaves alpha undefined
        }
        self.has_video = true;
        Some((w, h, buf))
    }

    pub fn playing(&self) -> bool {
        api().is_some_and(|a| unsafe { (a.is_playing)(self.player) } != 0)
    }
    pub fn toggle(&self) {
        if let Some(a) = api() {
            unsafe { (a.pause)(self.player) };
        }
    }
    pub fn set_volume(&self, v: i32) {
        if let Some(a) = api() {
            unsafe { (a.set_volume)(self.player, v.clamp(0, 100)) };
        }
    }
    pub fn position(&self) -> f32 {
        api().map_or(0.0, |a| unsafe { (a.get_position)(self.player) })
    }
    pub fn seek(&self, p: f32) {
        if let Some(a) = api() {
            unsafe { (a.set_position)(self.player, p.clamp(0.0, 1.0)) };
        }
    }
    /// `(elapsed, total)` in milliseconds; total is -1 while unknown.
    pub fn times(&self) -> (i64, i64) {
        api().map_or((0, -1), |a| unsafe {
            ((a.get_time)(self.player), (a.get_length)(self.player))
        })
    }
}

const AUDIO_EXT: &[&str] = &[
    "mp3", "flac", "wav", "m4a", "aac", "ogg", "opus", "wma", "aiff", "ape", "alac", "mid",
];
const VIDEO_EXT: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "webm", "flv", "m4v", "mpg", "mpeg", "ts", "m2ts", "vob",
    "ogv", "3gp", "divx",
];

#[derive(PartialEq, Clone, Copy)]
pub enum Kind {
    Audio,
    Video,
    Other,
}

pub fn kind_of(name: &str) -> Kind {
    let Some((_, ext)) = name.rsplit_once('.') else {
        return Kind::Other;
    };
    let e = ext.to_ascii_lowercase();
    if VIDEO_EXT.contains(&e.as_str()) {
        Kind::Video
    } else if AUDIO_EXT.contains(&e.as_str()) {
        Kind::Audio
    } else {
        Kind::Other
    }
}

pub fn fmt_time(ms: i64) -> String {
    if ms < 0 {
        return "--:--".into();
    }
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}
