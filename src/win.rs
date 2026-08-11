//! Thin, hand-rolled Win32 layer: raw volume access, positioned reads, elevation,
//! drive enumeration and sector-aligned buffers for unbuffered I/O.

use std::alloc::{alloc, dealloc, Layout};
use std::ffi::{c_void, OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    ReadFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
pub const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

pub const DRIVE_REMOVABLE: u32 = 2;
pub const DRIVE_FIXED: u32 = 3;
pub const DRIVE_REMOTE: u32 = 4;
pub const DRIVE_CDROM: u32 = 5;
pub const DRIVE_RAMDISK: u32 = 6;

pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end]).to_string_lossy().into_owned()
}

fn last_err() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

pub fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// Owned Win32 HANDLE that closes on drop.
pub struct Handle(pub HANDLE);

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

impl Handle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

/// Opens `\\.\X:` for raw sector reads. Requires an elevated process.
pub fn open_volume(letter: char, unbuffered: bool) -> io::Result<Handle> {
    let path = format!(r"\\.\{}:", letter.to_ascii_uppercase());
    let flags = if unbuffered {
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN
    } else {
        FILE_FLAG_SEQUENTIAL_SCAN
    };
    let h = unsafe {
        CreateFileW(
            wide(&path).as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(last_err());
    }
    Ok(Handle(h))
}

/// Opens `\\.\X:` with buffered access — used by the USN journal ioctls.
pub fn open_volume_buffered(letter: char) -> io::Result<Handle> {
    open_volume(letter, false)
}

/// Positioned read. Uses OVERLAPPED so the shared file pointer is never touched.
pub fn read_at(h: HANDLE, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0usize;
    while total < buf.len() {
        let mut ov: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
        let pos = offset + total as u64;
        ov.Anonymous.Anonymous.Offset = (pos & 0xFFFF_FFFF) as u32;
        ov.Anonymous.Anonymous.OffsetHigh = (pos >> 32) as u32;

        let want = (buf.len() - total).min(32 * 1024 * 1024) as u32;
        let mut got: u32 = 0;
        let ok = unsafe {
            ReadFile(
                h,
                buf[total..].as_mut_ptr(),
                want,
                &mut got,
                &mut ov as *mut _,
            )
        };
        if ok == 0 {
            return Err(last_err());
        }
        if got == 0 {
            break;
        }
        total += got as usize;
    }
    Ok(total)
}

/// Opens a directory (needs `FILE_FLAG_BACKUP_SEMANTICS`).
///
/// Used as the volume hint for `OpenFileById`, which wants a handle to a *file*
/// on the volume — a raw `\\.\C:` device handle is not one and is rejected.
pub fn open_dir(path: &str) -> io::Result<Handle> {
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_SHARE_DELETE: u32 = 4;
    let h = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(last_err());
    }
    Ok(Handle(h))
}

/// Positioned read via an explicit seek.
///
/// Some volume drivers reject `ReadFile` with an `OVERLAPPED` offset on a
/// handle that was not opened overlapped, answering `ERROR_NOT_SUPPORTED`.
/// Seeking first works everywhere; it is only safe because a single thread owns
/// the handle for the duration of a scan.
pub fn read_at_seek(h: HANDLE, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    use windows_sys::Win32::Storage::FileSystem::{SetFilePointerEx, FILE_BEGIN};

    let mut newpos: i64 = 0;
    if unsafe { SetFilePointerEx(h, offset as i64, &mut newpos, FILE_BEGIN) } == 0 {
        return Err(last_err());
    }
    let mut total = 0usize;
    while total < buf.len() {
        let want = (buf.len() - total).min(32 * 1024 * 1024) as u32;
        let mut got: u32 = 0;
        let ok = unsafe {
            ReadFile(
                h,
                buf[total..].as_mut_ptr(),
                want,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_err());
        }
        if got == 0 {
            break;
        }
        total += got as usize;
    }
    Ok(total)
}

/// Page-aligned heap buffer, required for `FILE_FLAG_NO_BUFFERING` reads.
pub struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
    layout: Layout,
}

unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        let layout = Layout::from_size_align(len, 4096).expect("bad layout");
        let ptr = unsafe { alloc(layout) };
        assert!(!ptr.is_null(), "out of memory allocating {len} bytes");
        Self { ptr, len, layout }
    }

    pub fn as_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut el: TOKEN_ELEVATION = std::mem::zeroed();
        let mut ret: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut el as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        );
        CloseHandle(token);
        ok != 0 && el.TokenIsElevated != 0
    }
}

/// Runs an executable elevated. This is the single UAC prompt in the product:
/// the service takes over privileged work afterwards.
pub fn run_elevated(exe: &str, args: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let r = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            wide("runas").as_ptr(),
            wide(exe).as_ptr(),
            if args.is_empty() {
                ptr::null()
            } else {
                wide(args).as_ptr()
            },
            ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL as i32,
        )
    };
    (r as isize) > 32
}

/// Relaunches the current executable through the UAC "runas" verb.
pub fn relaunch_elevated(args: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => return false,
    };
    let r = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            wide("runas").as_ptr(),
            wide(&exe).as_ptr(),
            if args.is_empty() {
                ptr::null()
            } else {
                wide(args).as_ptr()
            },
            ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL as i32,
        )
    };
    (r as isize) > 32
}

#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub letter: char,
    pub label: String,
    pub fs: String,
    pub kind: u32,
    pub total: u64,
    pub free: u64,
}

impl DriveInfo {
    pub fn is_ntfs(&self) -> bool {
        self.fs.eq_ignore_ascii_case("NTFS")
    }
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            DRIVE_REMOVABLE => "Removable",
            DRIVE_FIXED => "Fixed",
            DRIVE_REMOTE => "Network",
            DRIVE_CDROM => "Optical",
            DRIVE_RAMDISK => "RAM disk",
            _ => "Unknown",
        }
    }
}

pub fn list_drives() -> Vec<DriveInfo> {
    list_drives_detailed(true)
}

/// Enumerates drives.
///
/// `probe_remote` decides whether network drives are queried for label, file
/// system and capacity. Those calls go over SMB and block for a long time when
/// the share is unreachable — long enough to freeze whatever thread asked. The
/// service passes `false`, since it only ever indexes fixed volumes anyway.
pub fn list_drives_detailed(probe_remote: bool) -> Vec<DriveInfo> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let wroot = wide(&root);
        let kind = unsafe { GetDriveTypeW(wroot.as_ptr()) };
        if kind == DRIVE_CDROM || kind < DRIVE_REMOVABLE {
            continue;
        }

        if kind == DRIVE_REMOTE && !probe_remote {
            out.push(DriveInfo {
                letter,
                label: String::new(),
                fs: "NTFS".into(),
                kind,
                total: 0,
                free: 0,
            });
            continue;
        }

        let mut label = [0u16; 261];
        let mut fs = [0u16; 32];
        let ok = unsafe {
            GetVolumeInformationW(
                wroot.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            continue; // e.g. empty card reader slot
        }

        let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
        unsafe {
            GetDiskFreeSpaceExW(wroot.as_ptr(), &mut free, &mut total, &mut total_free);
        }

        out.push(DriveInfo {
            letter,
            label: from_wide(&label),
            fs: from_wide(&fs),
            kind,
            total,
            free: total_free,
        });
    }
    out
}

/// The main window handle, published once at startup.
///
/// Taken straight from eframe's `CreationContext` rather than guessed by
/// enumerating windows — the tray and IPC threads need it too, so it lives in a
/// global instead of being threaded through every call.
static MAIN_HWND: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn set_main_window(hwnd: usize) {
    MAIN_HWND.store(hwnd, std::sync::atomic::Ordering::Relaxed);
}

pub fn main_window() -> windows_sys::Win32::Foundation::HWND {
    MAIN_HWND.load(std::sync::atomic::Ordering::Relaxed)
        as windows_sys::Win32::Foundation::HWND
}

/// True while the main window is actually on screen — neither hidden to the
/// tray nor minimised.
pub fn main_window_on_screen() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
    let h = main_window();
    if h.is_null() {
        return true; // not up yet; do not act on it
    }
    unsafe { IsWindowVisible(h) != 0 && IsIconic(h) == 0 }
}

/// Brings the main window back, restoring it if minimised.
pub fn show_main_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };
    let h = main_window();
    if h.is_null() {
        return;
    }
    unsafe {
        ShowWindow(h, SW_SHOW);
        if IsIconic(h) != 0 {
            ShowWindow(h, SW_RESTORE);
        }
        SetForegroundWindow(h);
    }
}

/// Applies the embedded application icon to the window and taskbar. Called once
/// on the first frame, when the window definitely exists.
pub fn set_window_icon() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, ICON_BIG, ICON_SMALL, WM_SETICON};

    let hwnd = main_window();
    if hwnd.is_null() {
        return;
    }
    let icon = crate::tray::load_app_icon();
    if icon.is_null() {
        return;
    }
    unsafe {
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, icon as isize);
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, icon as isize);
    }
}

/// Windows FILETIME (100 ns since 1601) -> unix seconds, clamped to u32.
pub fn filetime_to_unix(ft: u64) -> u32 {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    if ft == 0 {
        return 0;
    }
    let secs = ft / 10_000_000;
    secs.saturating_sub(EPOCH_DIFF).min(u32::MAX as u64) as u32
}

/// Working set and private (commit) bytes of a process.
///
/// The two differ enough to be worth showing both: the shared index lives in a
/// section, so it counts once in the working set of every process that maps it
/// but only against the private bytes of whoever actually wrote it.
#[derive(Clone, Copy, Default)]
pub struct Mem {
    pub working_set: u64,
    pub private: u64,
}

pub fn process_memory(pid: u32) -> Option<Mem> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    let (h, owned) = if pid == 0 {
        (unsafe { GetCurrentProcess() }, false)
    } else {
        let h = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                0,
                pid,
            )
        };
        if h.is_null() {
            return None;
        }
        (h, true)
    };

    let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let ok = unsafe {
        GetProcessMemoryInfo(
            h,
            (&mut c as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            c.cb,
        )
    };
    if owned {
        unsafe { CloseHandle(h) };
    }
    (ok != 0).then_some(Mem {
        working_set: c.WorkingSetSize as u64,
        private: c.PrivateUsage as u64,
    })
}

/// First process with this executable name, or `None` if it is not running.
pub fn find_process(exe_name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE || snap.is_null() {
        return None;
    }
    let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut found = None;
    if unsafe { Process32FirstW(snap, &mut e) } != 0 {
        loop {
            let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            if from_wide(&e.szExeFile[..n]).eq_ignore_ascii_case(exe_name) {
                found = Some(e.th32ProcessID);
                break;
            }
            if unsafe { Process32NextW(snap, &mut e) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snap) };
    found
}

/// Current window rectangle as (x, y, width, height).

/// Moves the window without resizing, raising or activating it.


#[cfg(test)]
mod tests {
    #[test]
    fn own_memory_is_readable() {
        let m = super::process_memory(0).expect("own process must report memory");
        assert!(m.working_set > 0 && m.private > 0);
    }

    #[test]
    fn finds_a_running_process_by_name() {
        // Every Windows session has one, so this needs no fixture.
        assert!(super::find_process("explorer.exe").is_some());
        assert!(super::find_process("definitely-not-running-xyz.exe").is_none());
    }
}


/// Puts the window away without closing it. Used on the first frame of an
/// autostart run: eframe brings the window up during initialisation even when
/// it was built invisible, so hiding has to happen once egui is running.
pub fn hide_main_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    let h = main_window();
    if !h.is_null() {
        unsafe { ShowWindow(h, SW_HIDE) };
    }
}

/// Total and free bytes for any path a filesystem answers for, including a UNC
/// share. `GetDiskFreeSpaceExW` takes a directory, not just a volume root.
pub fn space_of(path: &str) -> (u64, u64) {
    let mut free_to_caller = 0u64;
    let mut total = 0u64;
    let mut free = 0u64;
    let p = if path.ends_with('\\') {
        path.to_string()
    } else {
        format!("{path}\\")
    };
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide(&p).as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut free,
        )
    };
    if ok == 0 {
        (0, 0)
    } else {
        (total, free)
    }
}

/// Filesystem name behind a path, e.g. "NTFS". Empty when it cannot be asked.
pub fn filesystem_of(path: &str) -> String {
    let p = if path.ends_with('\\') {
        path.to_string()
    } else {
        format!("{path}\\")
    };
    let mut fs = [0u16; 32];
    let ok = unsafe {
        GetVolumeInformationW(
            wide(&p).as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        String::new()
    } else {
        let n = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
        from_wide(&fs[..n])
    }
}
