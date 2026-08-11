//! Shell integration through COM: the real Explorer context menu and the
//! Windows thumbnail/icon provider.
//!
//! `IContextMenu` gives us exactly the menu Explorer shows — including entries
//! installed by other programs — instead of a hand-written imitation.
//! `IShellItemImageFactory` gives us the same previews Explorer renders, which
//! covers images, video frames, PDF first pages and Office documents without
//! shipping a single decoder.

use std::cell::RefCell;
use std::ffi::c_void;

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDIBits, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DefSubclassProc, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder,
    IShellItemImageFactory, RemoveWindowSubclass, SHBindToParent, SHCreateItemFromParsingName,
    SHParseDisplayName, SetWindowSubclass, CMF_EXPLORE, CMF_NORMAL, CMINVOKECOMMANDINFOEX,
    SIIGBF_RESIZETOFIT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetForegroundWindow, GetWindowThreadProcessId, TrackPopupMenuEx,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};

const ID_FIRST: u32 = 1;
const ID_LAST: u32 = 0x7FFF;
const SUBCLASS_ID: usize = 0xD15CA;

/// `None` for the optional `IBindCtx` parameters, spelled out so type inference
/// has something to chew on.
const NO_BIND_CTX: Option<&windows::Win32::System::Com::IBindCtx> = None;

pub fn init_com() {
    unsafe {
        // Already-initialised (a different mode) is fine — winit does its own.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    }
}

/// Our top-level window, used as the menu owner. At the moment of a right-click
/// our window has focus, so the foreground window is the right one; the process
/// check guards against the rare case where it is not.
pub fn owner_hwnd() -> HWND {
    unsafe {
        let h = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == std::process::id() {
            h
        } else {
            HWND(std::ptr::null_mut())
        }
    }
}

struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0 as *const c_void)) };
        }
    }
}

fn parse(path: &str) -> Option<Pidl> {
    let w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe {
        SHParseDisplayName(PCWSTR(w.as_ptr()), NO_BIND_CTX, &mut pidl, 0, None).ok()?;
    }
    (!pidl.is_null()).then_some(Pidl(pidl))
}

// The menu owner has to forward a few messages to the shell extension that
// drew them, otherwise submenus like "Öffnen mit" come up empty.
thread_local! {
    static ACTIVE_MENU: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let handled = ACTIVE_MENU.with(|slot| {
            let borrow = slot.borrow();
            let cm = borrow.as_ref()?;
            unsafe {
                if let Ok(cm3) = cm.cast::<IContextMenu3>() {
                    let mut result = LRESULT(0);
                    if cm3
                        .HandleMenuMsg2(msg, wparam, lparam, Some(&mut result))
                        .is_ok()
                    {
                        return Some(result);
                    }
                }
                if let Ok(cm2) = cm.cast::<IContextMenu2>() {
                    if cm2.HandleMenuMsg(msg, wparam, lparam).is_ok() {
                        return Some(LRESULT(0));
                    }
                }
            }
            None
        });
        if let Some(r) = handled {
            return r;
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Same as [`context_menu`], positioned where the cursor actually is.
pub fn context_menu_at_cursor(path: &str) {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
    }
    context_menu(path, pt.x, pt.y);
}

/// Shows the genuine Explorer context menu for `path` at the given screen
/// position and invokes whatever the user picks. Blocks until the menu closes,
/// exactly like Explorer does.
pub fn context_menu(path: &str, x: i32, y: i32) {
    let owner = owner_hwnd();
    let Some(pidl) = parse(path) else { return };

    unsafe {
        let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
        let Ok(folder) = SHBindToParent::<IShellFolder>(pidl.0, Some(&mut child)) else {
            return;
        };
        if child.is_null() {
            return;
        }

        let Ok(cm) =
            folder.GetUIObjectOf::<IContextMenu>(owner, &[child as *const ITEMIDLIST], None)
        else {
            return;
        };

        let Ok(hmenu) = CreatePopupMenu() else { return };
        if cm
            .QueryContextMenu(hmenu, 0, ID_FIRST, ID_LAST, CMF_NORMAL | CMF_EXPLORE)
            .is_err()
        {
            let _ = DestroyMenu(hmenu);
            return;
        }

        ACTIVE_MENU.with(|s| *s.borrow_mut() = Some(cm.clone()));
        let _ = SetWindowSubclass(owner, Some(subclass_proc), SUBCLASS_ID, 0);

        let picked = TrackPopupMenuEx(hmenu, TPM_RETURNCMD.0 | TPM_RIGHTBUTTON.0, x, y, owner, None);

        let _ = RemoveWindowSubclass(owner, Some(subclass_proc), SUBCLASS_ID);
        ACTIVE_MENU.with(|s| *s.borrow_mut() = None);

        if picked.0 != 0 {
            let verb = (picked.0 as u32 - ID_FIRST) as usize;
            let mut info = CMINVOKECOMMANDINFOEX {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
                hwnd: owner,
                lpVerb: windows::core::PCSTR(verb as *const u8),
                nShow: windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL.0,
                ptInvoke: POINT { x, y },
                ..Default::default()
            };
            let _ = cm.InvokeCommand(&mut info as *mut _ as *const _);
        }
        let _ = DestroyMenu(hmenu);
    }
}

const CLOSE_SUBCLASS_ID: usize = 0xD15CB;

unsafe extern "system" fn close_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{
        ShowWindow, SC_CLOSE, SC_MINIMIZE, SW_HIDE, WM_CLOSE, WM_SYSCOMMAND,
    };


    // Minimising leaves the window alive but off screen, and stops egui frames
    // just like hiding does. The visibility guard would catch it within a fifth
    // of a second; doing it here makes it instant.
    if msg == WM_SYSCOMMAND && (wparam.0 & 0xFFF0) == SC_MINIMIZE as usize {
        crate::media::stop_all();
    }

    let closing = msg == WM_CLOSE
        || (msg == WM_SYSCOMMAND && (wparam.0 & 0xFFF0) == SC_CLOSE as usize);
    if closing && CLOSE_HIDES.load(std::sync::atomic::Ordering::Relaxed) {
        // No egui frame follows a hide, so the preview pane never gets the
        // chance to stop playback — audio would keep going from the tray.
        crate::media::stop_all();
        // Hide instead of exiting; the tray menu's "Beenden" ends the process.
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        };
        return LRESULT(0);
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Whether the close button hides the window. Read inside the window procedure,
/// which runs on Windows' own callback and cannot reach the settings struct.
static CLOSE_HIDES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);


pub fn set_close_to_tray(on: bool) {
    CLOSE_HIDES.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Makes the window's close button hide to the notification area.
///
/// Done at the Win32 level rather than through egui's close-request plumbing,
/// which does not reliably surface the event for a window that then stays alive.
pub fn install_close_to_tray(hwnd_raw: *mut std::ffi::c_void) -> bool {
    if hwnd_raw.is_null() {
        return false;
    }
    unsafe { SetWindowSubclass(HWND(hwnd_raw), Some(close_proc), CLOSE_SUBCLASS_ID, 0).as_bool() }
}

/// Hands a selection to the shell's own delete operation, so the user gets the
/// familiar recycle-bin behaviour, confirmation prompt and progress dialog
/// rather than something we reimplemented.
pub fn delete_items(paths: &[String]) {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::UI::Shell::{
        FileOperation, IFileOperation, IShellItem, FOFX_ADDUNDORECORD, FOF_ALLOWUNDO,
    };

    if paths.is_empty() {
        return;
    }
    unsafe {
        let Ok(op) = CoCreateInstance::<_, IFileOperation>(&FileOperation, None, CLSCTX_ALL) else {
            return;
        };
        let _ = op.SetOwnerWindow(owner_hwnd());
        let _ = op.SetOperationFlags(FOF_ALLOWUNDO | FOFX_ADDUNDORECORD);

        let mut queued = 0;
        for p in paths {
            let w: Vec<u16> = p.encode_utf16().chain(std::iter::once(0)).collect();
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                PCWSTR(w.as_ptr()),
                NO_BIND_CTX,
            ) {
                if op.DeleteItem(&item, None).is_ok() {
                    queued += 1;
                }
            }
        }
        if queued > 0 {
            let _ = op.PerformOperations();
        }
    }
}

/// The shell's icon for a file type, without touching the disk.
///
/// `SHGFI_USEFILEATTRIBUTES` means the path is treated as a mere name, so a
/// made-up `x.mp4` yields the registered icon for that extension. Callers cache
/// per extension — every `.mp4` shares one icon, so a list of 200 000 rows needs
/// a handful of lookups rather than one per row.
pub fn file_icon(ext: &str, is_dir: bool, large: bool) -> Option<Thumb> {
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON,
        SHGFI_USEFILEATTRIBUTES,
    };
    use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;

    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

    let name = if is_dir {
        "folder".to_string()
    } else if ext.is_empty() {
        "file".to_string()
    } else {
        format!("file.{ext}")
    };
    let w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let mut info = SHFILEINFOW::default();
        let flags = SHGFI_ICON
            | SHGFI_USEFILEATTRIBUTES
            | if large { SHGFI_LARGEICON } else { SHGFI_SMALLICON };
        let attrs = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        let ok = SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(attrs),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if ok == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let out = icon_to_rgba(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        out
    }
}

unsafe fn icon_to_rgba(icon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<Thumb> {
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    let mut ii = ICONINFO::default();
    if unsafe { GetIconInfo(icon, &mut ii) }.is_err() {
        return None;
    }
    let out = unsafe { hbitmap_to_rgba(ii.hbmColor) };
    unsafe {
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmColor.0));
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(ii.hbmMask.0));
        }
    }
    out
}

pub struct Thumb {
    pub w: u32,
    pub h: u32,
    /// Premultiplied RGBA, top-down.
    pub rgba: Vec<u8>,
}

/// Asks the shell for a preview of `path` at up to `px` pixels. Falls back to
/// the file-type icon when no real thumbnail exists.
pub fn thumbnail(path: &str, px: u32) -> Option<Thumb> {
    // PDFs first: without a registered thumbnail handler the shell answers with
    // the generic file icon and reports success, so there is no failure to fall
    // back from. Windows can render the page itself — see `crate::pdf`.
    if crate::pdf::is_pdf(path) {
        if let Some(t) = crate::pdf::first_page(path, px) {
            return Some(t);
        }
    }
    let w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), NO_BIND_CTX).ok()?;
        let bmp: HBITMAP = factory
            .GetImage(
                windows::Win32::Foundation::SIZE {
                    cx: px as i32,
                    cy: px as i32,
                },
                SIIGBF_RESIZETOFIT,
            )
            .ok()?;
        let out = hbitmap_to_rgba(bmp);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        out
    }
}

unsafe fn hbitmap_to_rgba(bmp: HBITMAP) -> Option<Thumb> {
    use windows::Win32::Graphics::Gdi::{GetObjectW, BITMAP};

    let mut info = BITMAP::default();
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(bmp.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut info as *mut _ as *mut c_void),
        )
    };
    if got == 0 || info.bmWidth <= 0 || info.bmHeight <= 0 {
        return None;
    }
    let (w, h) = (info.bmWidth as u32, info.bmHeight as u32);

    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            // Negative height requests a top-down buffer.
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut buf = vec![0u8; (w * h * 4) as usize];
    let hdc = unsafe { GetDC(None) };
    let lines = unsafe {
        GetDIBits(
            hdc,
            bmp,
            0,
            h,
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut bi,
            DIB_RGB_COLORS,
        )
    };
    unsafe { ReleaseDC(None, hdc) };
    if lines == 0 {
        return None;
    }

    // GDI hands back BGRA; egui wants RGBA.
    let mut opaque = true;
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
        if px[3] != 255 {
            opaque = false;
        }
    }
    // Some providers return a zeroed alpha channel for opaque images.
    if !opaque && buf.chunks_exact(4).all(|p| p[3] == 0) {
        for px in buf.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }

    Some(Thumb { w, h, rgba: buf })
}

const _: Option<RECT> = None;

/// Reports what the shell can actually produce for a path.
///
/// `GetImage` without `SIIGBF_THUMBNAILONLY` happily falls back to the file
/// type's icon, which looks like a working preview but carries no information
/// about the file. Asking both ways separates "the shell rendered this file"
/// from "the shell handed us the generic icon".
pub fn thumbnail_kind(path: &str, px: u32) -> (bool, bool) {
    use windows::Win32::UI::Shell::SIIGBF_THUMBNAILONLY;
    let w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let Ok(factory) =
            SHCreateItemFromParsingName::<_, _, IShellItemImageFactory>(PCWSTR(w.as_ptr()), NO_BIND_CTX)
        else {
            return (false, false);
        };
        let size = windows::Win32::Foundation::SIZE {
            cx: px as i32,
            cy: px as i32,
        };
        let real = factory
            .GetImage(size, SIIGBF_RESIZETOFIT | SIIGBF_THUMBNAILONLY)
            .map(|b| {
                let _ = DeleteObject(HGDIOBJ(b.0));
            })
            .is_ok();
        let any = factory
            .GetImage(size, SIIGBF_RESIZETOFIT)
            .map(|b| {
                let _ = DeleteObject(HGDIOBJ(b.0));
            })
            .is_ok();
        (real, any)
    }
}

#[cfg(test)]
mod tests {
    /// Diagnostic rather than an assertion about this machine: prints whether
    /// the shell has a real thumbnail for each sample, or only an icon.
    /// `cargo test --lib -- --ignored thumbnail_kinds --nocapture`
    #[test]
    #[ignore = "depends on which thumbnail providers are installed"]
    fn thumbnail_kinds() {
        super::init_com();
        let samples: Vec<String> = std::env::var("DKZ_TEST_FILES")
            .map(|v| v.split(';').map(str::to_string).collect())
            .unwrap_or_default();
        assert!(!samples.is_empty(), "set DKZ_TEST_FILES=a.pdf;b.jpg");
        for s in samples {
            let (real, any) = super::thumbnail_kind(&s, 256);
            println!(
                "{s}\n   real thumbnail: {real}   anything at all: {any}"
            );
        }
    }
}
