//! Fallback scanner: parallel `FindFirstFileEx` walk.
//!
//! Used for non-NTFS volumes, network shares, single-folder scans and whenever
//! the process is not elevated. Much slower than the MFT path but portable.
//! Work is distributed over a shared directory queue so wide trees saturate all
//! cores instead of serializing on one deep branch.

use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileExW, FindExInfoBasic, FindExSearchNameMatch, FindNextFileW,
    GetDiskFreeSpaceW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, WIN32_FIND_DATAW,
};

use crate::index::{Index, VolumeInfo, F_DIR, F_INUSE, NONE};
use crate::scan::Progress;
use crate::win::{self, wide};

const FIND_FIRST_EX_LARGE_FETCH: u32 = 2;

struct Found {
    name: String,
    is_dir: bool,
    logical: u64,
    alloc: u64,
    mtime: u32,
}

struct Shared {
    queue: Mutex<Vec<(u32, String)>>,
    cv: Condvar,
    active: AtomicUsize,
    ix: Mutex<Index>,
    files: AtomicUsize,
    dirs: AtomicUsize,
}

/// Enumerates a single directory. `path` is already `\\?\`-prefixed.
fn read_dir(path: &str, cluster: u64, out: &mut Vec<Found>) {
    let pattern = if path.ends_with('\\') {
        format!("{path}*")
    } else {
        format!("{path}\\*")
    };
    let wpat = wide(&pattern);
    let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let h = unsafe {
        FindFirstFileExW(
            wpat.as_ptr(),
            FindExInfoBasic,
            &mut fd as *mut _ as *mut c_void,
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return;
    }
    loop {
        let attrs = fd.dwFileAttributes;
        let name = win::from_wide(&fd.cFileName);
        let skip = name.is_empty()
            || name == "."
            || name == ".."
            // Junctions and symlinks would double-count or loop.
            || attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        if !skip {
            let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
            let logical = ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64;
            let alloc = if is_dir {
                0
            } else {
                logical.div_ceil(cluster.max(1)) * cluster
            };
            let ft = ((fd.ftLastWriteTime.dwHighDateTime as u64) << 32)
                | fd.ftLastWriteTime.dwLowDateTime as u64;
            out.push(Found {
                name,
                is_dir,
                logical,
                alloc,
                mtime: win::filetime_to_unix(ft),
            });
        }
        if unsafe { FindNextFileW(h, &mut fd) } == 0 {
            break;
        }
    }
    unsafe { FindClose(h) };
}

fn cluster_size_of(root: &str) -> u64 {
    let (mut spc, mut bps, mut freec, mut totalc) = (0u32, 0u32, 0u32, 0u32);
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide(root).as_ptr(),
            &mut spc,
            &mut bps,
            &mut freec,
            &mut totalc,
        )
    };
    if ok == 0 || spc == 0 || bps == 0 {
        4096
    } else {
        spc as u64 * bps as u64
    }
}

/// Prefixes a path for the extended-length namespace so `MAX_PATH` never bites.
fn long_path(p: &str) -> String {
    if p.starts_with(r"\\?\") {
        p.to_string()
    } else if let Some(rest) = p.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{rest}")
    } else {
        format!(r"\\?\{p}")
    }
}

pub fn scan_path(root_path: &str, progress: &Progress) -> io::Result<Index> {
    let root_path = root_path.trim_end_matches('\\').to_string();
    let root_path = if root_path.len() == 2 && root_path.ends_with(':') {
        format!("{root_path}\\")
    } else {
        root_path
    };

    let vol_root = if root_path.len() >= 2 && root_path.as_bytes()[1] == b':' {
        format!("{}\\", &root_path[..2])
    } else {
        root_path.clone()
    };
    let cluster = cluster_size_of(&vol_root);

    let mut ix = Index::with_capacity(4096);
    ix.flags.clear();
    ix.name_off.clear();
    ix.name_len.clear();
    ix.parent.clear();
    ix.first_child.clear();
    ix.next_sib.clear();
    ix.size.clear();
    ix.own.clear();
    ix.logical.clear();
    ix.files.clear();
    ix.mtime.clear();

    let root = ix.push_entry();
    ix.set_name(root, root_path.trim_end_matches('\\'));
    ix.flags[root as usize] = F_INUSE | F_DIR;
    ix.parent[root as usize] = NONE;
    ix.root = root;

    let shared = Arc::new(Shared {
        queue: Mutex::new(vec![(root, long_path(&root_path))]),
        cv: Condvar::new(),
        active: AtomicUsize::new(1),
        ix: Mutex::new(ix),
        files: AtomicUsize::new(0),
        dirs: AtomicUsize::new(0),
    });

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 16);

    std::thread::scope(|scope| {
    for _ in 0..threads {
        let sh = Arc::clone(&shared);
        let cancel = progress.cancel.clone();
        scope.spawn(move || {
            let mut found: Vec<Found> = Vec::with_capacity(256);
            loop {
                let item = {
                    let mut q = sh.queue.lock();
                    loop {
                        if let Some(it) = q.pop() {
                            break Some(it);
                        }
                        if sh.active.load(Ordering::Acquire) == 0 {
                            break None;
                        }
                        sh.cv.wait(&mut q);
                    }
                };
                let Some((parent, path)) = item else { break };
                if cancel.load(Ordering::Relaxed) {
                    sh.active.fetch_sub(1, Ordering::Release);
                    sh.cv.notify_all();
                    break;
                }

                found.clear();
                read_dir(&path, cluster, &mut found);

                let mut new_dirs: Vec<(u32, String)> = Vec::new();
                {
                    let mut ix = sh.ix.lock();
                    for f in found.iter() {
                        let i = ix.push_entry();
                        ix.set_name(i, &f.name);
                        ix.flags[i as usize] = F_INUSE | if f.is_dir { F_DIR } else { 0 };
                        ix.own[i as usize] = f.alloc;
                        ix.logical[i as usize] = f.logical;
                        ix.mtime[i as usize] = f.mtime;
                        ix.parent[i as usize] = parent;
                        if f.is_dir {
                            new_dirs.push((i, format!("{}\\{}", path.trim_end_matches('\\'), f.name)));
                        }
                    }
                }
                let (nd, nf) = (
                    found.iter().filter(|f| f.is_dir).count(),
                    found.iter().filter(|f| !f.is_dir).count(),
                );
                sh.dirs.fetch_add(nd, Ordering::Relaxed);
                sh.files.fetch_add(nf, Ordering::Relaxed);
                progress.done.store(
                    (sh.files.load(Ordering::Relaxed) + sh.dirs.load(Ordering::Relaxed)) as u64,
                    Ordering::Relaxed,
                );

                if !new_dirs.is_empty() {
                    sh.active.fetch_add(new_dirs.len(), Ordering::Release);
                    let mut q = sh.queue.lock();
                    q.extend(new_dirs);
                }
                sh.active.fetch_sub(1, Ordering::Release);
                sh.cv.notify_all();
            }
        });
    }
    });

    // Every worker has been joined, so this is the last reference.
    let shared = Arc::try_unwrap(shared)
        .ok()
        .expect("workers still hold the shared state");
    let mut ix = shared.ix.into_inner();

    if progress.cancel.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }

    // A drive letter can borrow the figures already collected for that drive.
    // A UNC share has no letter, so it has to be asked directly — otherwise it
    // would report a capacity of zero everywhere it is shown.
    let by_letter = (root_path.as_bytes().get(1) == Some(&b':'))
        .then(|| {
            let c = root_path.chars().next()?;
            win::list_drives()
                .into_iter()
                .find(|d| d.letter.eq_ignore_ascii_case(&c))
        })
        .flatten();
    ix.vol = match by_letter {
        Some(d) => VolumeInfo {
            root_path: root_path.clone(),
            label: d.label,
            fs: d.fs,
            total: d.total,
            free: d.free,
            cluster: cluster as u32,
            scan_ms: 0,
            method_mft: false,
        },
        None => {
            let (total, free) = win::space_of(&vol_root);
            VolumeInfo {
                root_path: root_path.clone(),
                label: String::new(),
                fs: win::filesystem_of(&vol_root),
                total,
                free,
                cluster: cluster as u32,
                scan_ms: 0,
                method_mft: false,
            }
        }
    };

    ix.build_tree();
    Ok(ix)
}

#[cfg(test)]
mod tests {
    /// Walks a real UNC share. Ignored by default — it needs a reachable server
    /// and the caller's network credentials, so it only runs on request:
    /// `cargo test --lib -- --ignored unc --nocapture`
    #[test]
    #[ignore = "needs a reachable share"]
    fn walks_a_unc_share() {
        let path = std::env::var("DKZ_TEST_SHARE").unwrap_or(r"\\fatboy\downloads".into());
        let progress = crate::scan::Progress::default();
        let ix = super::scan_path(&path, &progress).expect("share must be walkable");
        println!(
            "{path}: {} Einträge, {} Dateien, {} Ordner",
            ix.len(),
            ix.total_files,
            ix.total_dirs
        );
        assert!(ix.total_files + ix.total_dirs > 0);
        assert!(ix.is_ready());
    }
}
