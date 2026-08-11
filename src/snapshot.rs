//! One index, one copy, shared by every process.
//!
//! The service builds a volume's index into a named section and then keeps
//! working *in* it — the USN watcher writes straight into the shared pages.
//! Clients map the same section read-only and point their `Index` columns at
//! it. A volume therefore exists once in physical memory however many windows
//! are open, instead of once per process plus a published copy.
//!
//! Layout: a small mutable header, the volume strings, then the columns sized
//! to `entry_cap` — reserved headroom so files can appear without reallocating
//! anything. Running out of headroom is not an error; the service notices and
//! republishes into a larger section under a new epoch.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ,
    FILE_MAP_WRITE, PAGE_READWRITE,
};

use crate::index::{Arr, Index, VolumeInfo, NONE};
use crate::win::wide;

const MAGIC: u32 = 0x325A_4B44; // "DKZ2"
const HEADER: usize = 256;

/// Spare room for entries and names, as a fraction of what the scan produced.
/// Enough that ordinary churn never forces a republish.
const HEADROOM: usize = 4; // one quarter

// ---- header accessors --------------------------------------------------------

macro_rules! hdr {
    ($t:ty, $get:ident, $set:ident, $off:expr) => {
        #[inline]
        fn $get(base: *const u8) -> $t {
            unsafe { std::ptr::read_unaligned(base.add($off) as *const $t) }
        }
        #[inline]
        fn $set(base: *mut u8, v: $t) {
            unsafe { std::ptr::write_unaligned(base.add($off) as *mut $t, v) }
        }
    };
}

hdr!(u32, get_magic, set_magic, 0);
hdr!(u32, get_entry_cap, set_entry_cap, 4);
hdr!(u32, get_entry_len, set_entry_len, 8);
hdr!(u32, get_root, set_root, 12);
hdr!(u64, get_names_cap, set_names_cap, 16);
hdr!(u64, get_names_len, set_names_len, 24);
hdr!(u64, get_generation, set_generation, 32);
hdr!(u64, get_total_files, set_total_files, 40);
hdr!(u64, get_total_dirs, set_total_dirs, 48);
hdr!(u64, get_vol_total, set_vol_total, 56);
hdr!(u64, get_vol_free, set_vol_free, 64);
hdr!(u64, get_scan_ms, set_scan_ms, 72);
hdr!(u32, get_cluster, set_cluster, 80);
hdr!(u32, get_mft, set_mft, 84);
hdr!(u32, get_arrays, set_arrays, 88);

#[inline]
fn align8(x: usize) -> usize {
    (x + 7) & !7
}

/// Byte offsets of each column, given the reserved capacities.
struct Layout {
    arrays: usize,
    size: usize,
    own: usize,
    logical: usize,
    name_off: usize,
    parent: usize,
    first_child: usize,
    next_sib: usize,
    files: usize,
    mtime: usize,
    name_len: usize,
    flags: usize,
    names: usize,
    total: usize,
}

/// Widest alignment first, so every column starts correctly aligned given a
/// page-aligned mapping base and an 8-aligned array block.
fn layout(entry_cap: usize, names_cap: usize, strings: usize) -> Layout {
    let arrays = align8(HEADER + strings);
    let mut o = arrays;
    let mut take = |n: usize| {
        let at = o;
        o += n;
        at
    };
    let size = take(entry_cap * 8);
    let own = take(entry_cap * 8);
    let logical = take(entry_cap * 8);
    let name_off = take(entry_cap * 4);
    let parent = take(entry_cap * 4);
    let first_child = take(entry_cap * 4);
    let next_sib = take(entry_cap * 4);
    let files = take(entry_cap * 4);
    let mtime = take(entry_cap * 4);
    let name_len = take(entry_cap * 2);
    let flags = take(entry_cap);
    let names = take(names_cap);
    Layout {
        arrays,
        size,
        own,
        logical,
        name_off,
        parent,
        first_child,
        next_sib,
        files,
        mtime,
        name_len,
        flags,
        names,
        total: align8(o),
    }
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}

fn take_str(b: &[u8], at: &mut usize) -> String {
    if *at + 4 > b.len() {
        return String::new();
    }
    let n = u32::from_le_bytes(b[*at..*at + 4].try_into().unwrap()) as usize;
    *at += 4;
    if *at + n > b.len() {
        return String::new();
    }
    let s = String::from_utf8_lossy(&b[*at..*at + n]).into_owned();
    *at += n;
    s
}

// ---- section handle ----------------------------------------------------------

pub struct Section {
    handle: HANDLE,
    view: *mut c_void,
    pub name: String,
    pub writable: bool,
}

unsafe impl Send for Section {}
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS;
        unsafe {
            if !self.view.is_null() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view });
            }
            if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
                CloseHandle(self.handle);
            }
        }
    }
}

impl Section {
    pub fn base(&self) -> *mut u8 {
        self.view as *mut u8
    }
    pub fn generation(&self) -> u64 {
        get_generation(self.base())
    }
}

/// Grants read access to authenticated users; the service runs as LocalSystem
/// and its default DACL would shut everyone else out.
fn security() -> (*mut c_void, SECURITY_ATTRIBUTES) {
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    let mut sd: *mut c_void = std::ptr::null_mut();
    let sddl = wide("D:(A;;GRGX;;;AU)(A;;GA;;;SY)(A;;GA;;;BA)");
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: if ok != 0 { sd } else { std::ptr::null_mut() },
        bInheritHandle: 0,
    };
    (sd, sa)
}

/// Builds `ix` into a fresh shared section and returns an index that lives in
/// it. The passed-in owned columns are dropped afterwards, so the service holds
/// the data exactly once.
pub fn create(name: &str, ix: &Index) -> Option<Index> {
    let n = ix.len();
    let entry_cap = n + n / HEADROOM + 4096;
    let names_cap = ix.names.len() + ix.names.len() / HEADROOM + 64 * 1024;

    let mut strings: Vec<u8> = Vec::new();
    put_str(&mut strings, &ix.vol.root_path);
    put_str(&mut strings, &ix.vol.label);
    put_str(&mut strings, &ix.vol.fs);

    let lay = layout(entry_cap, names_cap, strings.len());
    let full = format!("Global\\{name}");
    let (_sd, mut sa) = security();

    let handle = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            &mut sa,
            PAGE_READWRITE,
            (lay.total >> 32) as u32,
            (lay.total & 0xFFFF_FFFF) as u32,
            wide(&full).as_ptr(),
        )
    };
    if handle.is_null() {
        return None;
    }
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, lay.total) };
    if view.Value.is_null() {
        unsafe { CloseHandle(handle) };
        return None;
    }
    let base = view.Value as *mut u8;

    set_magic(base, MAGIC);
    set_entry_cap(base, entry_cap as u32);
    set_entry_len(base, n as u32);
    set_root(base, ix.root);
    set_names_cap(base, names_cap as u64);
    set_names_len(base, ix.names.len() as u64);
    set_generation(base, ix.generation);
    set_total_files(base, ix.total_files);
    set_total_dirs(base, ix.total_dirs);
    set_vol_total(base, ix.vol.total);
    set_vol_free(base, ix.vol.free);
    set_scan_ms(base, ix.vol.scan_ms as u64);
    set_cluster(base, ix.vol.cluster);
    set_mft(base, u32::from(ix.vol.method_mft));
    set_arrays(base, lay.arrays as u32);
    unsafe {
        std::ptr::copy_nonoverlapping(strings.as_ptr(), base.add(HEADER), strings.len());
    }

    unsafe fn fill<T: Copy>(base: *mut u8, off: usize, src: &[T]) {
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), base.add(off) as *mut T, src.len());
        }
    }
    unsafe {
        fill(base, lay.size, &ix.size);
        fill(base, lay.own, &ix.own);
        fill(base, lay.logical, &ix.logical);
        fill(base, lay.name_off, &ix.name_off);
        fill(base, lay.parent, &ix.parent);
        fill(base, lay.first_child, &ix.first_child);
        fill(base, lay.next_sib, &ix.next_sib);
        fill(base, lay.files, &ix.files);
        fill(base, lay.mtime, &ix.mtime);
        fill(base, lay.name_len, &ix.name_len);
        fill(base, lay.flags, &ix.flags);
        fill(base, lay.names, &ix.names);
    }

    let section = std::sync::Arc::new(Section {
        handle,
        view: view.Value,
        name: full,
        writable: true,
    });
    Some(bind(section, ix.vol.clone()))
}

/// Opens a section the service published, read-only.
pub fn attach(name: &str) -> Option<Index> {
    let handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide(name).as_ptr()) };
    if handle.is_null() {
        return None;
    }
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
    if view.Value.is_null() {
        unsafe { CloseHandle(handle) };
        return None;
    }
    let base = view.Value as *mut u8;
    if get_magic(base) != MAGIC {
        unsafe {
            UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: view.Value,
            });
            CloseHandle(handle);
        }
        return None;
    }

    let strings_len = get_arrays(base) as usize - HEADER;
    let strings = unsafe { std::slice::from_raw_parts(base.add(HEADER), strings_len) };
    let mut at = 0usize;
    let vol = VolumeInfo {
        root_path: take_str(strings, &mut at),
        label: take_str(strings, &mut at),
        fs: take_str(strings, &mut at),
        total: get_vol_total(base),
        free: get_vol_free(base),
        cluster: get_cluster(base),
        scan_ms: get_scan_ms(base) as u128,
        method_mft: get_mft(base) != 0,
    };

    let section = std::sync::Arc::new(Section {
        handle,
        view: view.Value,
        name: name.to_string(),
        writable: false,
    });
    Some(bind(section, vol))
}

/// Points an `Index`'s columns at a mapped section.
fn bind(section: std::sync::Arc<Section>, vol: VolumeInfo) -> Index {
    let base = section.base();
    let entry_cap = get_entry_cap(base) as usize;
    let entry_len = get_entry_len(base) as usize;
    let names_cap = get_names_cap(base) as usize;
    let names_len = get_names_len(base) as usize;
    let strings = get_arrays(base) as usize - HEADER;
    let lay = layout(entry_cap, names_cap, strings);

    unsafe fn col<T>(base: *mut u8, off: usize, len: usize, cap: usize) -> Arr<T> {
        Arr::Shared {
            ptr: unsafe { base.add(off) } as *mut T,
            len,
            cap,
        }
    }
    let (l, c) = (entry_len, entry_cap);
    let mut ix = Index {
        size: unsafe { col(base, lay.size, l, c) },
        own: unsafe { col(base, lay.own, l, c) },
        logical: unsafe { col(base, lay.logical, l, c) },
        name_off: unsafe { col(base, lay.name_off, l, c) },
        parent: unsafe { col(base, lay.parent, l, c) },
        first_child: unsafe { col(base, lay.first_child, l, c) },
        next_sib: unsafe { col(base, lay.next_sib, l, c) },
        files: unsafe { col(base, lay.files, l, c) },
        mtime: unsafe { col(base, lay.mtime, l, c) },
        name_len: unsafe { col(base, lay.name_len, l, c) },
        flags: unsafe { col(base, lay.flags, l, c) },
        names: unsafe { col(base, lay.names, names_len, names_cap) },
        root: get_root(base),
        vol,
        total_files: get_total_files(base),
        total_dirs: get_total_dirs(base),
        generation: get_generation(base),
        exhausted: false,
        section: Some(section),
    };
    ix.root = get_root(base);
    ix
}

/// Writes the mutable header fields back. Called by the service after it edits
/// the index in place, so every reader sees the new lengths and generation.
pub fn commit(ix: &Index) {
    let Some(sec) = &ix.section else { return };
    if !sec.writable {
        return;
    }
    let base = sec.base();
    set_entry_len(base, ix.len() as u32);
    set_names_len(base, ix.names.len() as u64);
    set_total_files(base, ix.total_files);
    set_total_dirs(base, ix.total_dirs);
    set_root(base, ix.root);
    // Generation last: a reader that sees the new value knows the rest landed.
    set_generation(base, ix.generation);
}

/// Re-reads the mutable header into a read-only view. Cheap enough per frame.
/// Returns true when anything changed.
pub fn refresh(ix: &mut Index) -> bool {
    let Some(sec) = ix.section.clone() else {
        return false;
    };
    let base = sec.base();
    let gen = get_generation(base);
    if gen == ix.generation {
        return false;
    }
    let entry_len = get_entry_len(base) as usize;
    let names_len = get_names_len(base) as usize;
    if let Arr::Shared { len, cap, .. } = &mut ix.names {
        *len = names_len.min(*cap);
    }
    for len in [
        arr_len(&mut ix.size),
        arr_len(&mut ix.own),
        arr_len(&mut ix.logical),
    ] {
        *len = entry_len;
    }
    set_len_u32(&mut ix.name_off, entry_len);
    set_len_u32(&mut ix.parent, entry_len);
    set_len_u32(&mut ix.first_child, entry_len);
    set_len_u32(&mut ix.next_sib, entry_len);
    set_len_u32(&mut ix.files, entry_len);
    set_len_u32(&mut ix.mtime, entry_len);
    if let Arr::Shared { len, cap, .. } = &mut ix.name_len {
        *len = entry_len.min(*cap);
    }
    if let Arr::Shared { len, cap, .. } = &mut ix.flags {
        *len = entry_len.min(*cap);
    }
    ix.root = get_root(base);
    ix.total_files = get_total_files(base);
    ix.total_dirs = get_total_dirs(base);
    ix.generation = gen;
    true
}

fn arr_len(a: &mut Arr<u64>) -> &mut usize {
    match a {
        Arr::Shared { len, .. } => len,
        Arr::Owned(_) => unreachable!("refresh only runs on shared views"),
    }
}

fn set_len_u32(a: &mut Arr<u32>, n: usize) {
    if let Arr::Shared { len, cap, .. } = a {
        *len = n.min(*cap);
    }
}

const _: Option<u32> = Some(NONE);
