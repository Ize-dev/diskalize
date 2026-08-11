//! Notification-area icon.
//!
//! Runs on its own thread with a message-only window, so the tray never shares a
//! message pump with the renderer. Closing the main window hides it instead of
//! exiting, which keeps the indexes warm and the USN watchers running.

use std::ffi::c_void;
use std::sync::mpsc::{channel, Receiver, Sender};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, LoadImageW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, HMENU,
    IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE, LR_LOADFROMFILE, MF_SEPARATOR, MF_STRING, MSG,
    TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_DESTROY,
    WM_HOTKEY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_POPUP,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};

static HOTKEY_OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

use crate::win::wide;

const WM_TRAY: u32 = WM_APP + 17;
const ID_SHOW: usize = 1;
const ID_QUIT: usize = 2;

const HOTKEY_ID: i32 = 1;
/// Sent to the tray window to swap the global hotkey without a restart.
const WM_SETHOTKEY: u32 = WM_APP + 18;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrayEvent {
    Show,
    /// Raise the window *and* put the caret in the search box.
    Search,
    Quit,
}

pub struct Tray {
    hwnd: usize,
    pub events: Receiver<TrayEvent>,
}

impl Tray {
    /// False when the current global hotkey is already taken by another app.
    pub fn hotkey_ok(&self) -> bool {
        HOTKEY_OK.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Re-registers the global hotkey. `None` unregisters it.
    pub fn set_hotkey(&self, combo: Option<(u32, u32)>) {
        let (mods, vk) = combo.unwrap_or((0, 0));
        unsafe {
            PostMessageW(self.hwnd as HWND, WM_SETHOTKEY, mods as usize, vk as isize);
        }
    }
}

static mut SENDER: Option<Sender<TrayEvent>> = None;
static mut WAKE: Option<egui::Context> = None;
static mut ICON: Option<NOTIFYICONDATAW> = None;

/// Takes the icon out of the notification area. Called before we exit, so the
/// shell does not leave a ghost behind.
fn remove_icon() {
    let p = &raw const ICON;
    if let Some(nid) = unsafe { (*p).as_ref() } {
        unsafe { Shell_NotifyIconW(NIM_DELETE, nid) };
    }
}

/// The app icon: embedded resource first, then a sibling .ico, then the stock one.
pub fn load_app_icon() -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
    unsafe {
        let inst = GetModuleHandleW(std::ptr::null());
        let icon = LoadIconW(inst as _, 1 as *const u16);
        if !icon.is_null() {
            return icon;
        }
        if let Ok(exe) = std::env::current_exe() {
            let ico = exe.with_extension("ico");
            if ico.exists() {
                let h = LoadImageW(
                    std::ptr::null_mut(),
                    wide(&ico.to_string_lossy()).as_ptr(),
                    IMAGE_ICON,
                    0,
                    0,
                    LR_LOADFROMFILE | LR_DEFAULTSIZE,
                );
                if !h.is_null() {
                    return h as _;
                }
            }
        }
        LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // Acted on here rather than in the UI thread: a window hidden to the tray
    // renders no frames, so an event parked in a channel would never be seen.
    let send = |e: TrayEvent| {
        match e {
            TrayEvent::Show | TrayEvent::Search => crate::win::show_main_window(),
            TrayEvent::Quit => {
                remove_icon();
                std::process::exit(0);
            }
        }
        let s = &raw const SENDER;
        if let Some(tx) = unsafe { (*s).as_ref() } {
            let _ = tx.send(e);
        }
        let c = &raw const WAKE;
        if let Some(ctx) = unsafe { (*c).as_ref() } {
            ctx.request_repaint();
        }
    };
    match msg {
        WM_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => send(TrayEvent::Show),
                WM_RBUTTONUP => unsafe {
                    let menu: HMENU = CreatePopupMenu();
                    if !menu.is_null() {
                        AppendMenuW(menu, MF_STRING, ID_SHOW, wide("Diskalize öffnen").as_ptr());
                        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                        AppendMenuW(menu, MF_STRING, ID_QUIT, wide("Beenden").as_ptr());
                        let mut pt = POINT { x: 0, y: 0 };
                        GetCursorPos(&mut pt);
                        // The documented dance for tray menus: take the
                        // foreground first, then post a dummy message so the
                        // menu dismisses correctly when focus moves away.
                        SetForegroundWindow(hwnd);
                        let picked = TrackPopupMenu(
                            menu,
                            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
                            pt.x,
                            pt.y,
                            0,
                            hwnd,
                            std::ptr::null(),
                        );
                        PostMessageW(hwnd, WM_NULL, 0, 0);
                        DestroyMenu(menu);
                        match picked as usize {
                            ID_SHOW => send(TrayEvent::Show),
                            ID_QUIT => send(TrayEvent::Quit),
                            _ => {}
                        }
                    }
                },
                _ => {}
            }
            0
        }
        WM_HOTKEY if wp as i32 == HOTKEY_ID => {
            send(TrayEvent::Search);
            0
        }
        WM_SETHOTKEY => unsafe {
            UnregisterHotKey(hwnd, HOTKEY_ID);
            let (mods, vk) = (wp as u32, lp as u32);
            let ok = mods == 0 && vk == 0
                || RegisterHotKey(hwnd, HOTKEY_ID, mods as HOT_KEY_MODIFIERS, vk) != 0;
            HOTKEY_OK.store(ok, std::sync::atomic::Ordering::Relaxed);
            0
        },
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

impl Tray {
    pub fn new(
        tooltip: &str,
        wake: egui::Context,
        hotkey: Option<(u32, u32)>,
    ) -> Option<Tray> {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel::<(usize, bool)>();
        let tip = tooltip.to_string();

        std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || unsafe {
                let s = &raw mut SENDER;
                *s = Some(tx);
                let w = &raw mut WAKE;
                *w = Some(wake);

                let inst = GetModuleHandleW(std::ptr::null());
                let class = wide("DiskalizeTray");
                let wc = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: Some(wndproc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: inst as _,
                    hIcon: std::ptr::null_mut(),
                    hCursor: std::ptr::null_mut(),
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: class.as_ptr(),
                };
                RegisterClassW(&wc);

                // A real (never shown) top-level window, not HWND_MESSAGE:
                // message-only windows cannot become foreground, and without
                // that the popup menu never commits a selection.
                // Distinct title: `win::main_window` looks the real window up by
                // name, and two windows called "Diskalize" would race.
                let hwnd = CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    class.as_ptr(),
                    wide("DiskalizeTrayHost").as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    inst as _,
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    let _ = ready_tx.send((0, false));
                    return;
                }

                // The hotkey belongs to this thread's message queue, which is why
                // it is registered here rather than on the UI thread.
                let hotkey_ok = match hotkey {
                    Some((mods, vk)) => {
                        RegisterHotKey(hwnd, HOTKEY_ID, mods as HOT_KEY_MODIFIERS, vk) != 0
                    }
                    None => true,
                };
                HOTKEY_OK.store(hotkey_ok, std::sync::atomic::Ordering::Relaxed);

                let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
                nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                nid.hWnd = hwnd;
                nid.uID = 1;
                nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
                nid.uCallbackMessage = WM_TRAY;
                nid.hIcon = load_app_icon();
                for (i, c) in wide(&tip).iter().take(127).enumerate() {
                    nid.szTip[i] = *c;
                }
                Shell_NotifyIconW(NIM_ADD, &nid);
                let slot = &raw mut ICON;
                *slot = Some(nid);

                let _ = ready_tx.send((hwnd as usize, hotkey_ok));

                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                remove_icon();
                DestroyWindow(hwnd);
            })
            .ok()?;

        let (hwnd, _hotkey_ok) = ready_rx.recv().ok()?;
        (hwnd != 0).then_some(Tray { hwnd, events: rx })
    }

    pub fn try_recv(&self) -> Option<TrayEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
    }
}

const _: Option<*const c_void> = None;
