//! Live index maintenance for volumes without a USN journal.
//!
//! Network shares, exFAT/FAT drives and single-folder scans get a
//! `ReadDirectoryChangesW` watcher instead. It is coarser than the USN journal —
//! the API reports paths, not MFT records, so each change costs a `GetFileAttributesEx`
//! — but it covers everything the fast path cannot reach.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileAttributesExW, GetFileExInfoStandard, ReadDirectoryChangesW,
    FILE_ATTRIBUTE_DIRECTORY, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    WIN32_FILE_ATTRIBUTE_DATA,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::index::{Index, F_DIR, F_INUSE, NONE};
use crate::scan::usn::LiveWatcher;
use crate::win::{self, wide};

const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

const FILE_ACTION_ADDED: u32 = 1;
const FILE_ACTION_REMOVED: u32 = 2;
const FILE_ACTION_MODIFIED: u32 = 3;
const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;

/// Starts watching `root` recursively. Returns immediately.
pub fn spawn(root: String, index: Arc<RwLock<Index>>) -> LiveWatcher {
    let stop = Arc::new(AtomicBool::new(false));
    let events = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new("startet…".to_string()));

    let w = LiveWatcher {
        stop: Arc::clone(&stop),
        events: Arc::clone(&events),
        running: Arc::clone(&running),
        status: Arc::clone(&status),
    };

    std::thread::Builder::new()
        .name("watch".into())
        .spawn(move || unsafe {
            let h = CreateFileW(
                wide(&root).as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE || h.is_null() {
                *status.lock() = "Verzeichnis nicht überwachbar".into();
                return;
            }
            let _guard = win::Handle(h);

            let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            if event.is_null() {
                *status.lock() = "Event konnte nicht angelegt werden".into();
                return;
            }
            let _event_guard = win::Handle(event);

            running.store(true, Ordering::Relaxed);
            *status.lock() = "live (Verzeichnis)".into();

            // 64 KB is the documented ceiling for network shares.
            let mut buf = vec![0u8; 64 * 1024];
            let mut pending: Vec<(String, u32)> = Vec::new();
            let mut last_flush = Instant::now();

            while !stop.load(Ordering::Relaxed) {
                let mut ov: windows_sys::Win32::System::IO::OVERLAPPED = std::mem::zeroed();
                ov.hEvent = event;
                let mut returned: u32 = 0;

                let ok = ReadDirectoryChangesW(
                    h,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                    1, // watch subtree
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_SIZE
                        | FILE_NOTIFY_CHANGE_LAST_WRITE,
                    &mut returned,
                    &mut ov,
                    None,
                );
                if ok == 0 && win::last_error() != ERROR_IO_PENDING {
                    *status.lock() = "Überwachung abgebrochen".into();
                    break;
                }

                // Poll the event so `stop` is honoured promptly.
                loop {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if WaitForSingleObject(event, 250) == WAIT_OBJECT_0 {
                        break;
                    }
                }

                let mut moved: u32 = 0;
                if windows_sys::Win32::System::IO::GetOverlappedResult(h, &ov, &mut moved, 0) == 0 {
                    continue;
                }

                // A zero-length result means the buffer overflowed and changes
                // were dropped; there is nothing to replay, so just carry on.
                let mut off = 0usize;
                while moved > 0 && off + 12 <= moved as usize {
                    let next = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                    let action = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
                    let name_len =
                        u32::from_le_bytes(buf[off + 8..off + 12].try_into().unwrap()) as usize;
                    if off + 12 + name_len > moved as usize {
                        break;
                    }
                    let units: Vec<u16> = buf[off + 12..off + 12 + name_len]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    let rel = String::from_utf16_lossy(&units);
                    pending.push((rel, action));
                    if next == 0 {
                        break;
                    }
                    off += next;
                }

                if !pending.is_empty() && last_flush.elapsed() > Duration::from_millis(400) {
                    events.fetch_add(pending.len() as u64, Ordering::Relaxed);
                    apply(&index, &root, &pending);
                    pending.clear();
                    last_flush = Instant::now();
                }
            }
            running.store(false, Ordering::Relaxed);
            *status.lock() = "gestoppt".into();
        })
        .ok();

    w
}

fn stat(path: &str) -> Option<(bool, u64, u32)> {
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileAttributesExW(
            wide(path).as_ptr(),
            GetFileExInfoStandard,
            &mut data as *mut _ as *mut c_void,
        )
    };
    if ok == 0 {
        return None;
    }
    let size = ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64;
    let ft = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32)
        | data.ftLastWriteTime.dwLowDateTime as u64;
    Some((
        data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        size,
        win::filetime_to_unix(ft),
    ))
}

/// Applies a batch of relative-path changes to the index.
fn apply(index: &Arc<RwLock<Index>>, root: &str, batch: &[(String, u32)]) {
    // Stat outside the lock so the UI never waits on (possibly network) I/O.
    let root_trim = root.trim_end_matches('\\');
    let mut facts: Vec<(&str, u32, Option<(bool, u64, u32)>)> = Vec::with_capacity(batch.len());
    for (rel, action) in batch {
        let full = format!("{root_trim}\\{rel}");
        let info = if *action == FILE_ACTION_REMOVED || *action == FILE_ACTION_RENAMED_OLD_NAME {
            None
        } else {
            stat(&full)
        };
        facts.push((rel.as_str(), *action, info));
    }

    let mut ix = index.write();
    let cluster = ix.vol.cluster.max(1) as u64;
    let mut cache: HashMap<String, u32> = HashMap::new();

    for (rel, action, info) in facts {
        let Some(node) = resolve(&mut ix, rel, &mut cache, info.is_some()) else {
            continue;
        };
        match (action, info) {
            (FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME, _) | (_, None) => {
                let (s, l, f) = (
                    ix.size[node as usize],
                    ix.logical[node as usize],
                    ix.files[node as usize],
                );
                ix.propagate(node, -(s as i64), -(l as i64), -(f as i64));
                ix.unlink_child(node);
                ix.flags[node as usize] = 0;
                ix.size[node as usize] = 0;
                ix.own[node as usize] = 0;
                ix.logical[node as usize] = 0;
                ix.files[node as usize] = 0;
            }
            (FILE_ACTION_ADDED | FILE_ACTION_MODIFIED | FILE_ACTION_RENAMED_NEW_NAME, Some((is_dir, size, mtime))) => {
                let alloc = if is_dir { 0 } else { size.div_ceil(cluster) * cluster };
                let old = ix.own[node as usize];
                ix.mtime[node as usize] = mtime;
                if is_dir {
                    continue; // directories carry only their children's totals here
                }
                ix.own[node as usize] = alloc;
                ix.size[node as usize] = alloc;
                let dlog = size as i64 - ix.logical[node as usize] as i64;
                ix.logical[node as usize] = size;
                ix.propagate(node, alloc as i64 - old as i64, dlog, 0);
            }
            _ => {}
        }
    }
    ix.generation += 1;
}

/// Finds (or creates) the node for a path relative to the watched root.
fn resolve(
    ix: &mut Index,
    rel: &str,
    cache: &mut HashMap<String, u32>,
    create: bool,
) -> Option<u32> {
    let mut node = ix.root;
    if node == NONE {
        return None;
    }
    let mut walked = String::new();
    let parts: Vec<&str> = rel.split('\\').filter(|s| !s.is_empty()).collect();
    let last = parts.len().saturating_sub(1);

    for (n, part) in parts.iter().enumerate() {
        if !walked.is_empty() {
            walked.push('\\');
        }
        walked.push_str(part);
        if let Some(&hit) = cache.get(&walked) {
            node = hit;
            continue;
        }
        let found = ix
            .children(node)
            .find(|&c| ix.name(c).eq_ignore_ascii_case(part));
        node = match found {
            Some(c) => c,
            None if create => {
                let i = ix.push_entry();
                ix.set_name(i, part);
                // Anything that is not the final component must be a directory.
                ix.flags[i as usize] = F_INUSE | if n < last { F_DIR } else { 0 };
                ix.link_child(i, node);
                i
            }
            None => return None,
        };
        cache.insert(walked.clone(), node);
    }
    Some(node)
}
