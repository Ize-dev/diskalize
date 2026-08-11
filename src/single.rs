//! Single-instance guard.
//!
//! A named mutex decides who is primary. Anyone else hands their path argument
//! to the running instance over a named pipe and exits, so double-clicking the
//! Explorer entry twice raises the existing window instead of starting a second
//! full scan.

use std::ffi::c_void;
use std::sync::mpsc::{channel, Receiver, Sender};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::CreateMutexW;

use crate::win::{last_error, wide};

const MUTEX: &str = "Local\\DiskalizeSingleInstance";
const PIPE: &str = r"\\.\pipe\DiskalizeIPC";

pub enum Instance {
    /// We own the app. Paths sent by later launches arrive on this channel.
    Primary(Receiver<String>),
    /// Another instance is already running and has been handed our argument.
    Secondary,
}

/// Claims the instance slot. `arg` is forwarded when we are not the first.
pub fn acquire(arg: Option<&str>) -> Instance {
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide(MUTEX).as_ptr()) };
    let already = last_error() == ERROR_ALREADY_EXISTS;

    if handle.is_null() {
        // Without the mutex we cannot arbitrate; behave like a normal launch.
        return Instance::Primary(channel().1);
    }
    if already {
        unsafe { CloseHandle(handle) };
        send(arg.unwrap_or(""));
        return Instance::Secondary;
    }
    // The handle is deliberately never closed: the mutex must be held for the
    // lifetime of the process so later launches see it.

    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("ipc".into())
        .spawn(move || serve(tx))
        .ok();
    Instance::Primary(rx)
}

fn send(arg: &str) {
    unsafe {
        let h = CreateFileW(
            wide(PIPE).as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return;
        }
        let bytes = arg.as_bytes();
        let mut written = 0u32;
        WriteFile(
            h,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        );
        CloseHandle(h);
    }
}

fn serve(tx: Sender<String>) {
    loop {
        let pipe: HANDLE = unsafe {
            CreateNamedPipeW(
                wide(PIPE).as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                255,
                0,
                4096,
                0,
                std::ptr::null(),
            )
        };
        if pipe == INVALID_HANDLE_VALUE || pipe.is_null() {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }

        let connected = unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } != 0
            || last_error() == ERROR_PIPE_CONNECTED;
        if connected {
            // The connection itself is the signal to come forward — a launch
            // with no path argument writes nothing, and a zero-byte write does
            // not wake `ReadFile`, so waiting for a payload first would leave
            // the window sitting in the tray. Doing it here also covers the
            // case where no frames are running to notice a message at all.
            crate::win::show_main_window();

            let mut buf = [0u8; 4096];
            let mut read = 0u32;
            let ok = unsafe {
                ReadFile(
                    pipe,
                    buf.as_mut_ptr() as *mut c_void as *mut u8,
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            // Sent even when empty: a launch with no path still means "come
            // forward", and after an autostart there is no window yet — the
            // channel is the only way the waiting process hears about it.
            let msg = if ok != 0 && read > 0 {
                String::from_utf8_lossy(&buf[..read as usize]).into_owned()
            } else {
                String::new()
            };
            if tx.send(msg).is_err() {
                unsafe {
                    DisconnectNamedPipe(pipe);
                    CloseHandle(pipe);
                }
                return;
            }
            unsafe { DisconnectNamedPipe(pipe) };
        }
        unsafe { CloseHandle(pipe) };
    }
}

/// Whether this process is the first Diskalize window.
///
/// Used when several windows are allowed: they all run, but only the first
/// claims the shared, process-wide things — the notification icon, the global
/// hotkey and the handoff pipe. The mutex is kept for the lifetime of the
/// process so later launches see it.
pub fn is_first() -> bool {
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide(MUTEX).as_ptr()) };
    if handle.is_null() {
        return true;
    }
    if last_error() == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(handle) };
        return false;
    }
    true
}
