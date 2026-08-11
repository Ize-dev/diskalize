//! Holds one index per scanned volume.
//!
//! Keeping every scan around is what makes switching drives instant: a volume
//! that was already read (and is kept current by its USN watcher) never needs a
//! second scan. It also lets search span everything that has been indexed.

use std::sync::Arc;

use parking_lot::RwLock;
use rayon::prelude::*;

use crate::index::Index;
use crate::scan::{usn::LiveWatcher, Target};
use crate::search::{self, Query};

/// A search hit, qualified by which volume it came from.
/// Which volume a fresh window should open, given their keys.
///
/// Drive letters win over anything else: a share restored from the settings is
/// not what "first drive" means, and it was landing there because its walk
/// finished last and grabbed the selection.
pub fn preferred_volume(keys: &[String], first: bool) -> Option<usize> {
    let is_drive = |k: &String| k.len() == 2 && k.as_bytes()[1] == b':';
    let pool: Vec<usize> = (0..keys.len()).filter(|&i| is_drive(&keys[i])).collect();
    let pool = if pool.is_empty() {
        (0..keys.len()).collect()
    } else {
        pool
    };
    if first {
        pool.into_iter().min_by_key(|&i| keys[i].to_ascii_uppercase())
    } else {
        pool.into_iter().max_by_key(|&i| keys[i].to_ascii_uppercase())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Hit {
    pub vol: u16,
    pub idx: u32,
}

pub struct Volume {
    /// Canonical identity, e.g. `C:` or `\\fatboy\share` or a folder path.
    pub key: String,
    pub title: String,
    pub index: Arc<RwLock<Index>>,
    /// Only set when this process owns the watcher; with the service running it
    /// does not, and `live` simply reflects what the service reported.
    pub watcher: Option<LiveWatcher>,
    pub live: bool,
    pub target: Target,
}

impl Volume {
    pub fn live(&self) -> bool {
        self.live
            || self
                .watcher
                .as_ref()
                .is_some_and(|w| w.running.load(std::sync::atomic::Ordering::Relaxed))
    }
}

#[derive(Default)]
pub struct Store {
    pub vols: Vec<Volume>,
    pub active: Option<usize>,
}

impl Store {
    pub fn find(&self, key: &str) -> Option<usize> {
        self.vols.iter().position(|v| v.key.eq_ignore_ascii_case(key))
    }

    /// Inserts or replaces the volume with this key and returns its slot.
    pub fn put(&mut self, vol: Volume) -> usize {
        match self.find(&vol.key) {
            Some(i) => {
                // Drop the old watcher before the old index goes away.
                if let Some(w) = self.vols[i].watcher.take() {
                    w.shutdown();
                }
                self.vols[i] = vol;
                i
            }
            None => {
                self.vols.push(vol);
                self.vols.len() - 1
            }
        }
    }

    pub fn remove(&mut self, i: usize) {
        if i >= self.vols.len() {
            return;
        }
        if let Some(w) = self.vols[i].watcher.take() {
            w.shutdown();
        }
        self.vols.remove(i);
        self.active = match self.active {
            Some(a) if a == i => (!self.vols.is_empty()).then_some(0),
            Some(a) if a > i => Some(a - 1),
            other => other,
        };
    }

    pub fn active_vol(&self) -> Option<&Volume> {
        self.active.and_then(|i| self.vols.get(i))
    }

    pub fn index_of(&self, vol: u16) -> Option<&Arc<RwLock<Index>>> {
        self.vols.get(vol as usize).map(|v| &v.index)
    }

    /// Index handles for background work. A `Store` itself is not `Send`
    /// (the watchers own Win32 handles), but the indexes behind their locks are.
    pub fn snapshot(&self, only: Option<usize>) -> Vec<(u16, Arc<RwLock<Index>>)> {
        self.vols
            .iter()
            .enumerate()
            .filter(|(i, _)| only.is_none_or(|o| o == *i))
            .map(|(i, v)| (i as u16, Arc::clone(&v.index)))
            .collect()
    }

    /// Sum of every indexed volume, for the status line.
    pub fn totals(&self) -> (u64, u64) {
        let mut files = 0;
        let mut dirs = 0;
        for v in &self.vols {
            let ix = v.index.read();
            files += ix.total_files;
            dirs += ix.total_dirs;
        }
        (files, dirs)
    }
}

pub struct Results {
    pub hits: Vec<Hit>,
    pub total: usize,
    pub took_ms: u128,
    pub truncated: bool,
}

impl Default for Results {
    fn default() -> Self {
        Self {
            hits: Vec::new(),
            total: 0,
            took_ms: 0,
            truncated: false,
        }
    }
}

/// Runs `q` over the given volumes and merges the hits, largest first.
/// `scope` restricts the search to one subtree of one volume.
pub fn search(
    vols: &[(u16, Arc<RwLock<Index>>)],
    q: &Query,
    limit: usize,
    scope: Option<Hit>,
) -> Results {
    let t0 = std::time::Instant::now();
    let mut all: Vec<(u64, Hit)> = Vec::new();
    let mut total = 0usize;

    for (slot, index) in vols {
        if let Some(sc) = scope {
            if sc.vol != *slot {
                continue;
            }
        }
        let ix = index.read();
        // Per volume we can afford the full limit; the merge trims afterwards.
        let r = search::run(&ix, q, limit, scope.map(|s| s.idx));
        total += r.total;
        all.extend(r.hits.into_iter().map(|idx| {
            (
                ix.size[idx as usize],
                Hit {
                    vol: *slot,
                    idx,
                },
            )
        }));
    }

    let truncated = total > limit || all.len() > limit;
    if all.len() > limit {
        all.select_nth_unstable_by(limit, |a, b| b.0.cmp(&a.0));
        all.truncate(limit);
    }
    all.par_sort_unstable_by(|a, b| b.0.cmp(&a.0));

    Results {
        hits: all.into_iter().map(|(_, h)| h).collect(),
        total,
        took_ms: t0.elapsed().as_millis(),
        truncated,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Size,
    Name,
    Path,
    Date,
}

impl SortKey {
    /// Name used in the settings file.
    pub fn key(self) -> &'static str {
        match self {
            SortKey::Size => "size",
            SortKey::Name => "name",
            SortKey::Path => "path",
            SortKey::Date => "date",
        }
    }

    pub fn from_key(s: &str) -> Option<Self> {
        match s {
            "size" => Some(SortKey::Size),
            "name" => Some(SortKey::Name),
            "path" => Some(SortKey::Path),
            "date" => Some(SortKey::Date),
            _ => None,
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortVal {
    Num(u64),
    Text(String),
}

/// Sort key with folders kept as their own group.
///
/// Ordering derives field by field, so the leading flag groups directories and
/// files apart before the actual key is compared. Reversing for a descending
/// sort flips the groups too, which is what was asked for: folders lead going
/// up, files lead going down.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Keyed(u8, SortVal);

/// Sorts hits that may span volumes, locking each volume exactly once.
pub fn sort_hits(
    vols: &[(u16, Arc<RwLock<Index>>)],
    hits: &mut [Hit],
    key: SortKey,
    desc: bool,
    // Keep folders and files as separate blocks instead of interleaving them.
    group_dirs: bool,
) {
    if hits.len() < 2 {
        return;
    }
    let mut grouped: Vec<Hit> = hits.to_vec();
    grouped.sort_unstable_by_key(|h| h.vol);

    let mut keyed: Vec<(Keyed, Hit)> = Vec::with_capacity(grouped.len());
    let mut i = 0usize;
    while i < grouped.len() {
        let vol = grouped[i].vol;
        let end = i + grouped[i..].partition_point(|h| h.vol == vol);
        if let Some((_, index)) = vols.iter().find(|(s, _)| *s == vol) {
            let ix = index.read();
            for h in &grouped[i..end] {
                let val = match key {
                    SortKey::Size => SortVal::Num(ix.size[h.idx as usize]),
                    SortKey::Date => SortVal::Num(ix.mtime[h.idx as usize] as u64),
                    SortKey::Name => SortVal::Text(ix.name(h.idx).to_lowercase()),
                    SortKey::Path => SortVal::Text(ix.path_of(h.idx).to_lowercase()),
                };
                // 0 for directories: ascending puts them first, and reversing
                // for a descending sort moves them to the end along with it.
                let group = if group_dirs { u8::from(!ix.is_dir(h.idx)) } else { 0 };
                keyed.push((Keyed(group, val), *h));
            }
        }
        i = end;
    }

    keyed.sort_unstable_by(|a, b| if desc { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) });
    for (dst, (_, h)) in hits.iter_mut().zip(keyed) {
        *dst = h;
    }
}

#[cfg(test)]
mod tests {
    use super::preferred_volume;

    fn keys(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn drive_letters_come_before_shares() {
        // The order they were indexed in is not the order they are offered in.
        let k = keys(&[r"\\fatboy\downloads", "E:", "C:", "D:"]);
        assert_eq!(preferred_volume(&k, true), Some(2), "should pick C:");
        assert_eq!(preferred_volume(&k, false), Some(1), "should pick E:");
    }

    #[test]
    fn a_share_only_wins_when_it_is_all_there_is() {
        let k = keys(&[r"\\fatboy\downloads"]);
        assert_eq!(preferred_volume(&k, true), Some(0));
    }

    #[test]
    fn nothing_indexed_yet() {
        assert_eq!(preferred_volume(&[], true), None);
    }

    #[test]
    fn case_does_not_decide_the_order() {
        let k = keys(&["d:", "C:"]);
        assert_eq!(preferred_volume(&k, true), Some(1));
        assert_eq!(preferred_volume(&k, false), Some(0));
    }
}
