//! Live index maintenance via the NTFS USN change journal.
//!
//! The journal reports every touched file with its record number, parent, name
//! and attributes. Size and timestamp come from `OpenFileById`, which goes
//! through the filesystem cache — deliberately *not* by re-reading the MFT
//! record from the raw volume. The journal fires the instant a file appears,
//! while the on-disk MFT record still holds its previous, unused state for a
//! while; reading it raw made every creation look like a deletion.
//!
//! Nothing is rescanned: changes are patched in and size deltas propagate up
//! the parent chain in O(depth).

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::index::{Index, F_DIR, F_INUSE, NONE};
use crate::ipc::Change;
use crate::scan::ntfs::MftMap;
use crate::win::{self, Handle};

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;
const FSCTL_CREATE_USN_JOURNAL: u32 = 0x0009_00E7;

const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct UsnJournalData {
    journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ReadUsnJournalData {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    journal_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CreateUsnJournalData {
    maximum_size: u64,
    allocation_delta: u64,
}

fn ioctl(h: &Handle, code: u32, inp: Option<&[u8]>, out: &mut [u8]) -> Option<usize> {
    let mut returned: u32 = 0;
    let (ip, il) = match inp {
        Some(b) => (b.as_ptr() as *const c_void, b.len() as u32),
        None => (std::ptr::null(), 0),
    };
    let ok = unsafe {
        DeviceIoControl(
            h.raw(),
            code,
            ip,
            il,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        None
    } else {
        Some(returned as usize)
    }
}

fn query_journal(h: &Handle) -> Option<UsnJournalData> {
    let mut out = [0u8; std::mem::size_of::<UsnJournalData>()];
    ioctl(h, FSCTL_QUERY_USN_JOURNAL, None, &mut out)?;
    Some(unsafe { std::ptr::read_unaligned(out.as_ptr() as *const UsnJournalData) })
}

fn create_journal(h: &Handle) -> bool {
    let data = CreateUsnJournalData {
        maximum_size: 32 * 1024 * 1024,
        allocation_delta: 4 * 1024 * 1024,
    };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            &data as *const _ as *const u8,
            std::mem::size_of::<CreateUsnJournalData>(),
        )
    };
    let mut out = [0u8; 8];
    ioctl(h, FSCTL_CREATE_USN_JOURNAL, Some(bytes), &mut out).is_some()
}

pub struct LiveWatcher {
    pub stop: Arc<AtomicBool>,
    /// Number of filesystem events folded into the index so far.
    pub events: Arc<AtomicU64>,
    pub running: Arc<AtomicBool>,
    pub status: Arc<Mutex<String>>,
}

impl LiveWatcher {
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for LiveWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Called with every batch the watcher applied, so the service can forward it.
pub type ChangeSink = Arc<dyn Fn(&[Change]) + Send + Sync>;

/// Starts the background watcher for `letter`. Returns immediately.
///
/// `_map` is kept so the caller's plumbing stays uniform; the watcher no longer
/// reads MFT records itself.
pub fn spawn(
    letter: char,
    _map: Arc<MftMap>,
    index: Arc<RwLock<Index>>,
    sink: Option<ChangeSink>,
) -> LiveWatcher {
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
        .name(format!("usn-{letter}"))
        .spawn(move || {
            let h = match win::open_volume_buffered(letter) {
                Ok(h) => h,
                Err(e) => {
                    *status.lock() = format!("kein Zugriff: {e}");
                    return;
                }
            };
            let jd = match query_journal(&h) {
                Some(j) => Some(j),
                None => {
                    if create_journal(&h) {
                        query_journal(&h)
                    } else {
                        None
                    }
                }
            };
            let Some(jd) = jd else {
                *status.lock() = "USN-Journal nicht verfügbar".into();
                return;
            };

            // Hint handle for OpenFileById: must be a file on the volume, so the
            // root directory rather than the `\\.\X:` device.
            let root_dir = match win::open_dir(&format!("{letter}:\\")) {
                Ok(d) => d,
                Err(e) => {
                    *status.lock() = format!("Wurzelverzeichnis nicht lesbar: {e}");
                    return;
                }
            };

            running.store(true, Ordering::Relaxed);
            *status.lock() = "live".into();

            let trace = std::env::var_os("DISKALIZE_TRACE_USN").is_some();
            let mut next_usn = jd.next_usn;
            let mut buf = vec![0u8; 128 * 1024];
            let mut changed: HashMap<u32, Entry> = HashMap::new();

            while !stop.load(Ordering::Relaxed) {
                let req = ReadUsnJournalData {
                    start_usn: next_usn,
                    reason_mask: u32::MAX,
                    return_only_on_close: 0,
                    timeout: 0,
                    bytes_to_wait_for: 0,
                    journal_id: jd.journal_id,
                };
                let inp = unsafe {
                    std::slice::from_raw_parts(
                        &req as *const _ as *const u8,
                        std::mem::size_of::<ReadUsnJournalData>(),
                    )
                };
                let Some(n) = ioctl(&h, FSCTL_READ_USN_JOURNAL, Some(inp), &mut buf) else {
                    *status.lock() = "Journal-Lesefehler".into();
                    std::thread::sleep(Duration::from_millis(1000));
                    continue;
                };
                if n < 8 {
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                next_usn = i64::from_le_bytes(buf[0..8].try_into().unwrap());

                // USN_RECORD_V2: name at +60, attributes at +52, reason at +40.
                changed.clear();
                let mut o = 8usize;
                while o + 60 <= n {
                    let rec_len = u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()) as usize;
                    if rec_len < 60 || o + rec_len > n {
                        break;
                    }
                    let file_ref = u64::from_le_bytes(buf[o + 8..o + 16].try_into().unwrap());
                    let parent_ref = u64::from_le_bytes(buf[o + 16..o + 24].try_into().unwrap());
                    let reason = u32::from_le_bytes(buf[o + 40..o + 44].try_into().unwrap());
                    let attrs = u32::from_le_bytes(buf[o + 52..o + 56].try_into().unwrap());
                    let name_len = u16::from_le_bytes(buf[o + 56..o + 58].try_into().unwrap()) as usize;
                    let name_off = u16::from_le_bytes(buf[o + 58..o + 60].try_into().unwrap()) as usize;

                    let name = if name_off + name_len <= rec_len && o + name_off + name_len <= n {
                        let units: Vec<u16> = buf[o + name_off..o + name_off + name_len]
                            .chunks_exact(2)
                            .map(|c| u16::from_le_bytes([c[0], c[1]]))
                            .collect();
                        String::from_utf16_lossy(&units)
                    } else {
                        String::new()
                    };

                    // One entry per record: later journal entries win, which is
                    // what we want for a rename followed by a write.
                    let rec = (file_ref & REF_MASK) as u32;
                    changed.insert(
                        rec,
                        Entry {
                            rec,
                            file_ref,
                            parent: (parent_ref & REF_MASK) as u32,
                            attrs,
                            reason,
                            name,
                        },
                    );
                    o += rec_len;
                }

                if changed.is_empty() {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }

                events.fetch_add(changed.len() as u64, Ordering::Relaxed);
                let entries: Vec<Entry> = changed.drain().map(|(_, e)| e).collect();
                // Query outside the write lock so readers never wait on I/O.
                let batch = collect(&root_dir, &entries);
                if trace {
                    for c in &batch {
                        eprintln!(
                            "  usn {} rec={} parent={} alive={} dir={} size={} '{}'",
                            letter,
                            c.rec,
                            c.parent,
                            c.alive,
                            c.flags & F_DIR != 0,
                            c.alloc,
                            c.name
                        );
                    }
                }
                apply_changes(&mut index.write(), &batch);
                if let Some(sink) = &sink {
                    sink(&batch);
                }
            }
            running.store(false, Ordering::Relaxed);
            *status.lock() = "gestoppt".into();
        })
        .ok();

    w
}

/// What one USN journal entry tells us before we look anything up.
struct Entry {
    /// MFT record number — the index key.
    rec: u32,
    /// The complete 64-bit file reference. `OpenFileById` needs this one: the
    /// sequence number lives in the high bits and a masked-off value is rejected.
    file_ref: u64,
    parent: u32,
    attrs: u32,
    reason: u32,
    name: String,
}

#[repr(C, align(8))]
struct FileIdDesc {
    size: u32,
    kind: u32,
    id: [u8; 16],
}

#[repr(C)]
#[derive(Default)]
struct StandardInfo {
    allocation: i64,
    end_of_file: i64,
    links: u32,
    delete_pending: u8,
    directory: u8,
}

#[repr(C)]
#[derive(Default)]
struct BasicInfo {
    creation: i64,
    last_access: i64,
    last_write: i64,
    change: i64,
    attributes: u32,
}

/// Turns journal entries into index changes.
///
/// Deliberately *not* by re-reading the MFT record from the raw volume: the
/// journal fires the moment a file appears, but the on-disk MFT record still
/// holds its previous, unused state for a while — reading it raw made every
/// creation look like a deletion. The journal entry already carries name,
/// parent and attributes; size and timestamp come from `OpenFileById`, which
/// goes through the filesystem cache and is therefore always current.
fn collect(vol_root: &Handle, entries: &[Entry]) -> Vec<Change> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandleEx, OpenFileById, FileBasicInfo, FileStandardInfo,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let is_dir = e.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        let mut change = Change {
            rec: e.rec,
            alive: false,
            parent: e.parent,
            flags: F_INUSE | if is_dir { F_DIR } else { 0 },
            alloc: 0,
            logical: 0,
            mtime: 0,
            name: e.name.clone(),
        };

        if e.reason & USN_REASON_FILE_DELETE != 0 {
            out.push(change);
            continue;
        }

        let mut desc = FileIdDesc {
            size: std::mem::size_of::<FileIdDesc>() as u32,
            kind: 0, // FileIdType
            id: [0; 16],
        };
        desc.id[..8].copy_from_slice(&e.file_ref.to_le_bytes());

        let h = unsafe {
            OpenFileById(
                vol_root.raw(),
                &desc as *const _ as *const _,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        };
        if h.is_null() || h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            // Gone by the time we looked — treat it as a deletion.
            out.push(change);
            continue;
        }
        let owned = Handle(h);

        let mut std_info = StandardInfo::default();
        let mut basic = BasicInfo::default();
        let ok_std = unsafe {
            GetFileInformationByHandleEx(
                owned.raw(),
                FileStandardInfo,
                &mut std_info as *mut _ as *mut _,
                std::mem::size_of::<StandardInfo>() as u32,
            )
        } != 0;
        let ok_basic = unsafe {
            GetFileInformationByHandleEx(
                owned.raw(),
                FileBasicInfo,
                &mut basic as *mut _ as *mut _,
                std::mem::size_of::<BasicInfo>() as u32,
            )
        } != 0;

        if ok_std {
            change.alive = true;
            change.alloc = std_info.allocation.max(0) as u64;
            change.logical = std_info.end_of_file.max(0) as u64;
            if std_info.directory != 0 {
                change.flags = F_INUSE | F_DIR;
                // A directory's own size is its index, not its contents.
                change.alloc = 0;
            }
        }
        if ok_basic {
            change.mtime = win::filetime_to_unix(basic.last_write.max(0) as u64);
        }
        out.push(change);
    }
    out
}

/// Folds a batch of changes into the index.
///
/// Pass A detaches the old state (subtracting aggregates from every ancestor) and
/// writes the new per-entry fields without linking. Pass B links and adds the new
/// aggregates back. Splitting it this way means a batch containing both a new
/// directory and its new children resolves correctly regardless of order.
///
/// Pure: no I/O, so the service and every connected client run the identical
/// code over the identical records and stay bit-for-bit in step.
pub fn apply_changes(ix: &mut Index, changes: &[Change]) {
    let root = ix.root;
    if root == NONE {
        return;
    }

    // ---- pass A: detach + write new leaf state ----
    let mut pending: Vec<(u32, u32)> = Vec::with_capacity(changes.len()); // (idx, new parent)
    for c in changes {
        let i = c.rec as usize;
        if i >= ix.len() {
            if c.alive {
                ix.grow_to(i + 1);
            } else {
                continue;
            }
        }
        if c.rec == root {
            continue;
        }

        let existed = ix.flags[i] & F_INUSE != 0;
        let (old_size, old_own, old_log, old_files) =
            (ix.size[i], ix.own[i], ix.logical[i], ix.files[i]);

        if existed {
            ix.propagate(
                c.rec,
                -(old_size as i64),
                -(old_log as i64),
                -(old_files as i64),
            );
            ix.unlink_child(c.rec);
            ix.parent[i] = NONE;
        }

        if !c.alive {
            ix.flags[i] = 0;
            ix.size[i] = 0;
            ix.own[i] = 0;
            ix.logical[i] = 0;
            ix.files[i] = 0;
            continue;
        }
        let is_dir = c.flags & F_DIR != 0;

        if !c.name.is_empty() {
            ix.set_name(c.rec, &c.name);
        }
        ix.flags[i] = c.flags;
        ix.own[i] = c.alloc;
        ix.mtime[i] = c.mtime;

        if is_dir {
            // Children are untouched, so carry their aggregate over.
            let children = old_size.saturating_sub(old_own);
            ix.size[i] = c.alloc.saturating_add(children);
            ix.logical[i] = if existed { old_log } else { 0 };
            ix.files[i] = if existed { old_files } else { 0 };
        } else {
            ix.size[i] = c.alloc;
            ix.logical[i] = c.logical;
            ix.files[i] = 1;
        }
        pending.push((c.rec, c.parent));
    }

    // ---- pass B: re-attach and propagate ----
    for (i, want_parent) in pending {
        let p = if want_parent != NONE
            && (want_parent as usize) < ix.len()
            && ix.flags[want_parent as usize] & F_INUSE != 0
            && ix.flags[want_parent as usize] & F_DIR != 0
            && want_parent != i
        {
            want_parent
        } else {
            root
        };
        ix.link_child(i, p);
        let (s, l, f) = (
            ix.size[i as usize],
            ix.logical[i as usize],
            ix.files[i as usize],
        );
        ix.propagate(i, s as i64, l as i64, f as i64);
    }

    ix.generation += 1;
}
