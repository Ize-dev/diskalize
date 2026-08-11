//! Direct NTFS Master File Table reader.
//!
//! Instead of walking directories through the Win32 API (one syscall per entry),
//! this opens the volume as a raw block device, follows the `$MFT` data runs and
//! streams the whole table in 16 MB unbuffered chunks, parsing records across all
//! cores. A full 1 TB SSD with ~2 million files lands in the low single-digit
//! seconds. Requires administrator rights — `\\.\C:` is privileged.

use std::io;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;

use rayon::prelude::*;

use crate::index::{Index, F_DIR, F_INUSE, NONE};
use crate::scan::Progress;
use crate::win::{self, AlignedBuf, Handle};

const CHUNK: usize = 16 * 1024 * 1024;
const RECS_PER_TASK: usize = 256;
const REF_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

pub const ROOT_REC: u32 = 5;

// ---- little-endian readers ---------------------------------------------------

#[inline]
fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
#[inline]
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}

// ---- volume geometry ---------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Geom {
    pub bytes_per_sector: u32,
    pub cluster_size: u32,
    pub rec_size: u32,
    pub mft_lcn: u64,
}

/// How a given volume tolerates raw reads.
///
/// Not every volume driver accepts the fast combination. `ERROR_NOT_SUPPORTED`
/// on the very first read is common enough that the scanner probes for a
/// working mode instead of giving up and falling back to the slow walker.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ReadMode {
    /// Unbuffered, positioned by OVERLAPPED — fastest.
    Direct,
    /// Buffered, positioned by an explicit seek.
    Seek,
}

impl ReadMode {
    pub fn read(self, h: &Handle, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ReadMode::Direct => win::read_at(h.raw(), off, buf),
            ReadMode::Seek => win::read_at_seek(h.raw(), off, buf),
        }
    }
}

/// Opens `letter` and finds a read mode that actually works on it.
pub fn open_readable(letter: char) -> io::Result<(Handle, ReadMode)> {
    let mut why = String::new();
    for (unbuffered, mode) in [(true, ReadMode::Direct), (false, ReadMode::Seek)] {
        match win::open_volume(letter, unbuffered) {
            Ok(h) => {
                let mut probe = AlignedBuf::new(4096);
                match mode.read(&h, 0, &mut probe.as_mut()[..4096]) {
                    Ok(n) if n == 4096 => return Ok((h, mode)),
                    Ok(n) => why += &format!("{mode:?}: nur {n} Bytes; "),
                    Err(e) => why += &format!("{mode:?} read: {e}; "),
                }
            }
            Err(e) => why += &format!("{mode:?} open: {e}; "),
        }
    }
    Err(io::Error::other(why))
}

pub fn read_geom(h: &Handle, mode: ReadMode) -> io::Result<Geom> {
    let mut buf = AlignedBuf::new(4096);
    mode.read(h, 0, &mut buf.as_mut()[..4096])?;
    let b = buf.as_slice();

    if &b[3..7] != b"NTFS" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an NTFS volume",
        ));
    }

    let bytes_per_sector = u16le(b, 0x0B) as u32;
    let spc_raw = b[0x0D] as i8;
    let sectors_per_cluster: u32 = if spc_raw >= 0 {
        spc_raw as u32
    } else {
        1u32 << ((-spc_raw) as u32)
    };
    if bytes_per_sector == 0 || sectors_per_cluster == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad boot sector"));
    }
    let cluster_size = bytes_per_sector * sectors_per_cluster;

    let mft_lcn = u64le(b, 0x30);

    let cpr = b[0x40] as i8;
    let rec_size: u32 = if cpr >= 0 {
        cpr as u32 * cluster_size
    } else {
        1u32 << ((-cpr) as u32)
    };
    if rec_size == 0 || rec_size > 64 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad MFT record size",
        ));
    }

    Ok(Geom {
        bytes_per_sector,
        cluster_size,
        rec_size,
        mft_lcn,
    })
}

/// Undoes the update-sequence-array fixups NTFS applies to every sector of a record.
fn apply_fixups(rec: &mut [u8], bytes_per_sector: usize) -> bool {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return false;
    }
    let usa_off = u16le(rec, 4) as usize;
    let usa_cnt = u16le(rec, 6) as usize;
    if usa_cnt == 0 || usa_off + usa_cnt * 2 > rec.len() {
        return false;
    }
    let sig = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_cnt {
        let end = i * bytes_per_sector;
        if end < 2 || end > rec.len() {
            return false;
        }
        if rec[end - 2] != sig[0] || rec[end - 1] != sig[1] {
            return false; // torn write / not our record
        }
        rec[end - 2] = rec[usa_off + i * 2];
        rec[end - 1] = rec[usa_off + i * 2 + 1];
    }
    true
}

/// Decodes an NTFS data-run list into `(lcn, cluster_count)` pairs.
/// An `lcn` of -1 marks a sparse run.
fn parse_runs(data: &[u8]) -> Vec<(i64, u64)> {
    let mut runs = Vec::new();
    let mut lcn: i64 = 0;
    let mut i = 0usize;
    while i < data.len() && data[i] != 0 {
        let hdr = data[i];
        let len_sz = (hdr & 0x0F) as usize;
        let off_sz = (hdr >> 4) as usize;
        i += 1;
        if len_sz == 0 || len_sz > 8 || off_sz > 8 || i + len_sz + off_sz > data.len() {
            break;
        }
        let mut len: u64 = 0;
        for k in 0..len_sz {
            len |= (data[i + k] as u64) << (k * 8);
        }
        i += len_sz;

        if off_sz == 0 {
            runs.push((-1, len));
            continue;
        }
        let mut off: i64 = 0;
        for k in 0..off_sz {
            off |= (data[i + k] as i64) << (k * 8);
        }
        let shift = 64 - off_sz * 8;
        off = (off << shift) >> shift; // sign extend
        i += off_sz;
        lcn += off;
        if lcn < 0 {
            break;
        }
        runs.push((lcn, len));
    }
    runs
}

/// Walks a run list without allocating, returning `(mapped_clusters, has_holes)`.
///
/// Needed because `allocated_size` in the attribute header counts unmapped
/// clusters as well. `$BadClus:$Bad` is the extreme case: a single hole spanning
/// the entire volume, which would otherwise show up as terabytes of usage.
fn runs_mapped(data: &[u8]) -> (u64, bool) {
    let mut mapped = 0u64;
    let mut holes = false;
    let mut i = 0usize;
    while i < data.len() && data[i] != 0 {
        let hdr = data[i];
        let len_sz = (hdr & 0x0F) as usize;
        let off_sz = (hdr >> 4) as usize;
        i += 1;
        if len_sz == 0 || len_sz > 8 || off_sz > 8 || i + len_sz + off_sz > data.len() {
            break;
        }
        let mut len: u64 = 0;
        for k in 0..len_sz {
            len |= (data[i + k] as u64) << (k * 8);
        }
        i += len_sz + off_sz;
        if off_sz == 0 {
            holes = true;
        } else {
            mapped += len;
        }
    }
    (mapped, holes)
}

/// On-disk bytes actually occupied by one non-resident attribute fragment.
#[inline]
fn nonresident_alloc(r: &[u8], o: usize, alen: usize, cluster: u64) -> u64 {
    if o + 0x22 > r.len() {
        return 0;
    }
    let run_off = u16le(r, o + 0x20) as usize;
    if run_off == 0 || run_off >= alen || o + alen > r.len() {
        return 0;
    }
    let (mapped, holes) = runs_mapped(&r[o + run_off..o + alen]);
    if holes {
        // Sparse. `allocated_size` counts the holes too, so trust the runs and
        // add up every fragment individually.
        return mapped.saturating_mul(cluster);
    }
    // Fully mapped: the first fragment's header already states the total across
    // all fragments, so continuation fragments in extension records contribute
    // nothing — adding their runs on top would double-count fragmented files.
    let start_vcn = u64le(r, o + 0x10);
    if start_vcn == 0 && o + 0x30 <= r.len() {
        u64le(r, o + 0x28)
    } else {
        0
    }
}

/// Maps MFT record numbers to absolute byte offsets on the volume.
pub struct MftMap {
    pub runs: Vec<(i64, u64)>,
    /// Read mode that was found to work on this volume.
    pub mode: ReadMode,
    pub cluster_size: u64,
    pub rec_size: u64,
    pub bytes_per_sector: usize,
    pub record_count: u64,
}

impl MftMap {
    pub fn offset_of(&self, rec: u64) -> Option<u64> {
        let want = rec * self.rec_size;
        let mut seen = 0u64;
        for &(lcn, clusters) in &self.runs {
            let bytes = clusters * self.cluster_size;
            if want < seen + bytes {
                if lcn < 0 {
                    return None; // sparse
                }
                return Some(lcn as u64 * self.cluster_size + (want - seen));
            }
            seen += bytes;
        }
        None
    }
}

/// Reads MFT record 0 and extracts the run list describing the MFT itself.
pub fn read_mft_map(h: &Handle, g: Geom, mode: ReadMode) -> io::Result<MftMap> {
    let size = (g.rec_size as usize).max(4096);
    let mut buf = AlignedBuf::new(size);
    let off = g.mft_lcn * g.cluster_size as u64;
    mode.read(h, off, buf.as_mut())?;

    let rec = &mut buf.as_mut()[..g.rec_size as usize];
    if !apply_fixups(rec, g.bytes_per_sector as usize) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "$MFT record 0 is unreadable",
        ));
    }

    let attrs_off = u16le(rec, 0x14) as usize;
    let mut o = attrs_off;
    while o + 8 <= rec.len() {
        let atype = u32le(rec, o);
        if atype == 0xFFFF_FFFF {
            break;
        }
        let alen = u32le(rec, o + 4) as usize;
        if alen < 24 || o + alen > rec.len() {
            break;
        }
        let non_res = rec[o + 8];
        let name_len = rec[o + 9] as usize;
        if atype == 0x80 && non_res == 1 && name_len == 0 {
            let run_off = u16le(rec, o + 0x20) as usize;
            if o + run_off > rec.len() {
                break;
            }
            let runs = parse_runs(&rec[o + run_off..o + alen]);
            let total_clusters: u64 = runs.iter().map(|r| r.1).sum();
            let bytes = total_clusters * g.cluster_size as u64;
            return Ok(MftMap {
                runs,
                mode,
                cluster_size: g.cluster_size as u64,
                rec_size: g.rec_size as u64,
                bytes_per_sector: g.bytes_per_sector as usize,
                record_count: bytes / g.rec_size as u64,
            });
        }
        o += alen;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "$MFT has no unnamed non-resident $DATA attribute",
    ))
}

// ---- record parsing ----------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Rec {
    pub rec: u32,
    pub base: u32,
    pub parent: u32,
    pub noff: u32,
    pub nlen: u16,
    pub flags: u8,
    /// Quality of the chosen `$FILE_NAME` namespace; -1 when the record has none.
    /// A DOS 8.3 alias must never win over the Win32 name, even when the two live
    /// in different MFT records because of an `$ATTRIBUTE_LIST`.
    pub rank: i8,
    pub alloc: u64,
    pub logical: u64,
    pub mtime: u32,
}

#[derive(Default)]
struct ChunkOut {
    names: Vec<u8>,
    recs: Vec<Rec>,
}

/// Parses one MFT record in place. `names` is a scratch arena for this task.
fn parse_record(
    r: &mut [u8],
    recno: u32,
    bps: usize,
    cluster: u64,
    names: &mut Vec<u8>,
) -> Option<Rec> {
    if !apply_fixups(r, bps) {
        return None;
    }
    let hdr_flags = u16le(r, 0x16);
    let in_use = hdr_flags & 1 != 0;
    let is_dir = hdr_flags & 2 != 0;
    let base_ref = u64le(r, 0x20) & REF_MASK;

    if !in_use && base_ref == 0 {
        return None;
    }

    let attrs_off = u16le(r, 0x14) as usize;
    if attrs_off >= r.len() {
        return None;
    }

    let mut parent = NONE;
    let mut name_rank = -1i32;
    let mut name_range: Option<(usize, usize)> = None; // (offset, utf16 len)
    let mut alloc: u64 = 0;
    let mut logical: u64 = 0;
    let mut mtime: u32 = 0;
    let mut have_data = false;

    let mut o = attrs_off;
    while o + 8 <= r.len() {
        let atype = u32le(r, o);
        if atype == 0xFFFF_FFFF {
            break;
        }
        if o + 16 > r.len() {
            break;
        }
        let alen = u32le(r, o + 4) as usize;
        if alen < 24 || o.saturating_add(alen) > r.len() {
            break;
        }
        let non_res = r[o + 8];
        let name_len = r[o + 9] as usize;

        match atype {
            // $STANDARD_INFORMATION
            0x10 if non_res == 0 => {
                let vo = u16le(r, o + 0x14) as usize;
                let c = o + vo;
                if c + 0x10 <= r.len() {
                    mtime = win::filetime_to_unix(u64le(r, c + 8));
                }
            }
            // $FILE_NAME
            0x30 if non_res == 0 => {
                let vo = u16le(r, o + 0x14) as usize;
                let c = o + vo;
                if c + 0x42 <= r.len() {
                    let nl = r[c + 0x40] as usize;
                    let ns = r[c + 0x41];
                    if c + 0x42 + nl * 2 <= r.len() && nl > 0 {
                        // Prefer Win32 names; a pure DOS 8.3 alias is the last resort.
                        let rank = match ns {
                            3 => 4, // Win32 & DOS
                            1 => 3, // Win32
                            0 => 2, // POSIX
                            2 => 1, // DOS 8.3
                            _ => 0,
                        };
                        if rank > name_rank {
                            name_rank = rank;
                            name_range = Some((c + 0x42, nl));
                            parent = (u64le(r, c) & REF_MASK) as u32;
                        }
                    }
                }
            }
            // $DATA
            0x80 => {
                if non_res == 0 {
                    // Resident: the bytes live inside this MFT record, so they cost
                    // no extra clusters. $MFT itself already accounts for them.
                    if name_len == 0 {
                        logical = u32le(r, o + 0x10) as u64;
                        have_data = true;
                    }
                } else {
                    let start_vcn = u64le(r, o + 0x10);
                    let comp_unit = u16le(r, o + 0x22);
                    if comp_unit != 0 {
                        // Compressed: the header carries the true on-disk size.
                        if start_vcn == 0 && o + 0x48 <= r.len() {
                            alloc = alloc.saturating_add(u64le(r, o + 0x40));
                        }
                    } else {
                        alloc = alloc.saturating_add(nonresident_alloc(r, o, alen, cluster));
                    }
                    if start_vcn == 0 && name_len == 0 && o + 0x38 <= r.len() {
                        logical = u64le(r, o + 0x30);
                        have_data = true;
                    }
                }
            }
            // $INDEX_ALLOCATION — a directory's own B-tree storage
            0xA0 if non_res == 1 => {
                alloc = alloc.saturating_add(nonresident_alloc(r, o, alen, cluster));
            }
            _ => {}
        }
        o += alen;
    }

    if !have_data && !is_dir && alloc == 0 && name_range.is_none() {
        return None;
    }

    let (noff, nlen) = match name_range {
        Some((start, nl)) => {
            let off = names.len() as u32;
            let mut u16buf = [0u16; 255];
            let n = nl.min(255);
            for k in 0..n {
                u16buf[k] = u16le(r, start + k * 2);
            }
            let mut tmp = [0u8; 4];
            for ch in char::decode_utf16(u16buf[..n].iter().copied()) {
                let ch = ch.unwrap_or('\u{FFFD}');
                names.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
            }
            let len = names.len() as u32 - off;
            (off, len.min(u16::MAX as u32) as u16)
        }
        None => (names.len() as u32, 0),
    };

    Some(Rec {
        rec: recno,
        base: if base_ref == 0 { NONE } else { base_ref as u32 },
        parent,
        noff,
        nlen,
        flags: if in_use { F_INUSE } else { 0 } | if is_dir { F_DIR } else { 0 },
        rank: name_rank as i8,
        alloc,
        logical,
        mtime,
    })
}

fn parse_chunk(
    buf: &mut [u8],
    start_rec: u64,
    rec_size: usize,
    bps: usize,
    cluster: u64,
) -> Vec<ChunkOut> {
    buf.par_chunks_mut(rec_size * RECS_PER_TASK)
        .enumerate()
        .map(|(ci, sub)| {
            let base = start_rec + (ci * RECS_PER_TASK) as u64;
            let mut out = ChunkOut {
                names: Vec::with_capacity(sub.len() / 16),
                recs: Vec::with_capacity(RECS_PER_TASK),
            };
            for (k, r) in sub.chunks_mut(rec_size).enumerate() {
                if r.len() < rec_size {
                    break;
                }
                let recno = base + k as u64;
                if recno > u32::MAX as u64 {
                    break;
                }
                if let Some(p) = parse_record(r, recno as u32, bps, cluster, &mut out.names) {
                    out.recs.push(p);
                }
            }
            out
        })
        .collect()
}

// ---- full scan ---------------------------------------------------------------

pub struct MftScan {
    pub index: Index,
    pub map: Arc<MftMap>,
}

pub fn scan(drive: &win::DriveInfo, progress: &Progress) -> io::Result<MftScan> {
    let (handle, mode) = open_readable(drive.letter)?;
    let h = Arc::new(handle);
    let geom = read_geom(&h, mode)?;
    let map = Arc::new(read_mft_map(&h, geom, mode)?);

    let total_bytes: u64 = map.runs.iter().filter(|r| r.0 >= 0).map(|r| r.1).sum::<u64>()
        * geom.cluster_size as u64;
    progress.total.store(total_bytes, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    let n = (map.record_count as usize).max(64);
    let mut ix = Index::with_capacity(n);
    ix.vol.cluster = geom.cluster_size;

    // Records with an $ATTRIBUTE_LIST push attributes into extension records.
    // Those carry $DATA runs and sometimes the Win32 $FILE_NAME, both of which
    // belong to the base record. Collected here and folded in after the main pass,
    // when every base record is known.
    struct Ext {
        base: u32,
        alloc: u64,
        logical: u64,
        rank: i8,
        noff: u32,
        nlen: u16,
        parent: u32,
    }
    let mut extensions: Vec<Ext> = Vec::new();
    let mut ranks: Vec<i8> = vec![-1; n];

    let (tx_full, rx_full) = mpsc::sync_channel::<(u64, AlignedBuf, usize)>(2);
    let (tx_free, rx_free) = mpsc::channel::<AlignedBuf>();
    for _ in 0..3 {
        let _ = tx_free.send(AlignedBuf::new(CHUNK));
    }

    let reader = {
        let h = Arc::clone(&h);
        let map = Arc::clone(&map);
        let cancel = progress.cancel.clone();
        std::thread::spawn(move || -> io::Result<()> {
            let mut consumed: u64 = 0;
            for &(lcn, clusters) in &map.runs {
                let bytes = clusters * map.cluster_size;
                if lcn < 0 {
                    consumed += bytes;
                    continue;
                }
                let mut off = lcn as u64 * map.cluster_size;
                let mut left = bytes;
                while left > 0 {
                    if cancel.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    let Ok(mut buf) = rx_free.recv() else {
                        return Ok(());
                    };
                    let want = left.min(CHUNK as u64) as usize;
                    let got = mode.read(&h, off, &mut buf.as_mut()[..want])?;
                    if got == 0 {
                        return Ok(());
                    }
                    let start_rec = consumed / map.rec_size;
                    if tx_full.send((start_rec, buf, got)).is_err() {
                        return Ok(());
                    }
                    consumed += got as u64;
                    off += got as u64;
                    left -= got as u64;
                }
            }
            Ok(())
        })
    };

    let mut done: u64 = 0;
    while let Ok((start_rec, mut buf, len)) = rx_full.recv() {
        let outs = parse_chunk(
            &mut buf.as_mut()[..len],
            start_rec,
            geom.rec_size as usize,
            geom.bytes_per_sector as usize,
            geom.cluster_size as u64,
        );
        for out in &outs {
            let base_off = ix.names.len() as u32;
            ix.names.extend_from(&out.names);
            for r in &out.recs {
                let i = r.rec as usize;
                if i >= ix.len() {
                    ix.grow_to(i + 1);
                    ranks.resize(ix.len(), -1);
                }
                if r.base != NONE {
                    if r.alloc > 0 || r.rank >= 0 {
                        extensions.push(Ext {
                            base: r.base,
                            alloc: r.alloc,
                            logical: r.logical,
                            rank: r.rank,
                            noff: base_off + r.noff,
                            nlen: r.nlen,
                            parent: r.parent,
                        });
                    }
                    continue;
                }
                if r.flags & F_INUSE == 0 {
                    continue;
                }
                ranks[i] = r.rank;
                ix.name_off[i] = base_off + r.noff;
                ix.name_len[i] = r.nlen;
                ix.parent[i] = r.parent;
                ix.own[i] = r.alloc;
                ix.logical[i] = r.logical;
                ix.mtime[i] = r.mtime;
                ix.flags[i] = r.flags;
            }
        }
        done += len as u64;
        progress.done.store(done, Ordering::Relaxed);
        let _ = tx_free.send(buf);
    }
    drop(tx_free);
    let _ = reader.join();

    if progress.cancel.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }

    ranks.resize(ix.len(), -1);
    for e in extensions {
        let i = e.base as usize;
        if i >= ix.len() || ix.flags[i] & F_INUSE == 0 {
            continue;
        }
        ix.own[i] = ix.own[i].saturating_add(e.alloc);
        if ix.logical[i] == 0 {
            ix.logical[i] = e.logical;
        }
        if e.rank > ranks[i] && e.nlen > 0 {
            ranks[i] = e.rank;
            ix.name_off[i] = e.noff;
            ix.name_len[i] = e.nlen;
            ix.parent[i] = e.parent;
        }
    }

    // Root directory is always record 5 and is its own parent on disk.
    let root = ROOT_REC;
    if root as usize >= ix.len() || ix.flags[root as usize] & F_INUSE == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "root directory record missing",
        ));
    }
    ix.root = root;
    ix.parent[root as usize] = NONE;
    ix.set_name(root, &format!("{}:", drive.letter));

    ix.vol = crate::index::VolumeInfo {
        root_path: format!("{}:\\", drive.letter),
        label: drive.label.clone(),
        fs: drive.fs.clone(),
        total: drive.total,
        free: drive.free,
        cluster: geom.cluster_size,
        scan_ms: 0,
        method_mft: true,
    };

    ix.build_tree();

    Ok(MftScan { index: ix, map })
}

/// Reads and parses a single MFT record — used by the live USN updater.
pub fn read_one(
    h: &Handle,
    map: &MftMap,
    mode: ReadMode,
    recno: u64,
    scratch: &mut Vec<u8>,
) -> Option<Rec> {
    let off = map.offset_of(recno)?;
    let rs = map.rec_size as usize;
    let mut buf = vec![0u8; rs];
    mode.read(h, off, &mut buf).ok()?;
    scratch.clear();
    parse_record(
        &mut buf,
        recno as u32,
        map.bytes_per_sector,
        map.cluster_size,
        scratch,
    )
}
