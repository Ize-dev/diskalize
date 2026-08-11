//! The in-memory file index.
//!
//! Struct-of-arrays layout, indexed by MFT record number when scanning NTFS
//! directly (which makes parent lookup a single array access) or by insertion
//! order for the fallback walker. ~51 bytes per entry plus the name arena.

pub const NONE: u32 = u32::MAX;

/// Storage for one index column: either owned, or a window into shared memory.
///
/// The service builds an index into a named section and keeps working in it;
/// every client maps the same section instead of copying it out. That way a
/// volume's index exists once in physical memory no matter how many processes
/// are looking at it, rather than once per process.
pub enum Arr<T> {
    Owned(Vec<T>),
    /// Points into a mapped section. `cap` is the room reserved there, which is
    /// what allows the index to grow in place as files appear.
    Shared { ptr: *mut T, len: usize, cap: usize },
}

// The pointer refers to a mapping that outlives the `Index` (the section handle
// is held alongside it), and access is serialised by the lock around the index.
unsafe impl<T: Send> Send for Arr<T> {}
unsafe impl<T: Sync> Sync for Arr<T> {}

impl<T> Default for Arr<T> {
    fn default() -> Self {
        Arr::Owned(Vec::new())
    }
}

impl<T: Copy + Default> Arr<T> {
    pub fn capacity(&self) -> usize {
        match self {
            Arr::Owned(v) => v.capacity(),
            Arr::Shared { cap, .. } => *cap,
        }
    }

    /// Grows to `n`, filling with the default. Returns false when a shared
    /// window is out of reserved room — the caller then needs a bigger section.
    pub fn resize_to(&mut self, n: usize) -> bool {
        match self {
            Arr::Owned(v) => {
                v.resize(n, T::default());
                true
            }
            Arr::Shared { ptr, len, cap } => {
                if n > *cap {
                    return false;
                }
                if n > *len {
                    unsafe {
                        std::ptr::write_bytes(ptr.add(*len), 0, n - *len);
                    }
                }
                *len = n;
                true
            }
        }
    }

    pub fn push(&mut self, value: T) -> bool {
        match self {
            Arr::Owned(v) => {
                v.push(value);
                true
            }
            Arr::Shared { ptr, len, cap } => {
                if *len >= *cap {
                    return false;
                }
                unsafe { std::ptr::write(ptr.add(*len), value) };
                *len += 1;
                true
            }
        }
    }

    pub fn extend_from(&mut self, src: &[T]) -> bool {
        match self {
            Arr::Owned(v) => {
                v.extend_from_slice(src);
                true
            }
            Arr::Shared { ptr, len, cap } => {
                if *len + src.len() > *cap {
                    return false;
                }
                unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), ptr.add(*len), src.len()) };
                *len += src.len();
                true
            }
        }
    }
}

impl<T> std::ops::Deref for Arr<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Arr::Owned(v) => v,
            Arr::Shared { ptr, len, .. } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
        }
    }
}

impl<T> std::ops::DerefMut for Arr<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            Arr::Owned(v) => v,
            Arr::Shared { ptr, len, .. } => unsafe { std::slice::from_raw_parts_mut(*ptr, *len) },
        }
    }
}

impl<T> Arr<T> {
    pub fn clear(&mut self) {
        match self {
            Arr::Owned(v) => v.clear(),
            Arr::Shared { len, .. } => *len = 0,
        }
    }
}

impl<T: Copy> From<Vec<T>> for Arr<T> {
    fn from(v: Vec<T>) -> Self {
        Arr::Owned(v)
    }
}

pub const F_INUSE: u8 = 1 << 0;
pub const F_DIR: u8 = 1 << 1;

#[derive(Clone, Debug, Default)]
pub struct VolumeInfo {
    pub root_path: String,
    pub label: String,
    pub fs: String,
    pub total: u64,
    pub free: u64,
    pub cluster: u32,
    pub scan_ms: u128,
    pub method_mft: bool,
}

#[derive(Default)]
pub struct Index {
    pub names: Arr<u8>,
    pub name_off: Arr<u32>,
    pub name_len: Arr<u16>,

    pub parent: Arr<u32>,
    pub first_child: Arr<u32>,
    pub next_sib: Arr<u32>,

    /// Aggregated allocated size (own + all descendants).
    pub size: Arr<u64>,
    /// This entry's own allocated bytes, excluding children.
    pub own: Arr<u64>,
    /// Aggregated logical size.
    pub logical: Arr<u64>,
    /// Recursive file count.
    pub files: Arr<u32>,
    pub flags: Arr<u8>,
    pub mtime: Arr<u32>,

    /// Keeps the mapping alive while the columns point into it.
    pub section: Option<std::sync::Arc<crate::snapshot::Section>>,

    pub root: u32,
    pub vol: VolumeInfo,

    pub total_files: u64,
    pub total_dirs: u64,
    /// Bumped on every mutation so caches know to invalidate.
    pub generation: u64,
    /// Set when a shared window ran out of reserved room. The service notices
    /// and republishes into a larger section.
    pub exhausted: bool,
}

impl Index {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            names: Vec::with_capacity(n * 14).into(),
            name_off: vec![0; n].into(),
            name_len: vec![0; n].into(),
            parent: vec![NONE; n].into(),
            first_child: vec![NONE; n].into(),
            next_sib: vec![NONE; n].into(),
            size: vec![0; n].into(),
            own: vec![0; n].into(),
            logical: vec![0; n].into(),
            files: vec![0; n].into(),
            flags: vec![0; n].into(),
            mtime: vec![0; n].into(),
            section: None,
            root: NONE,
            vol: VolumeInfo::default(),
            total_files: 0,
            total_dirs: 0,
            generation: 1,
            exhausted: false,
        }
    }

    pub fn len(&self) -> usize {
        self.flags.len()
    }

    /// True once there is real data behind this index.
    ///
    /// A volume the service has announced but whose snapshot has not been mapped
    /// yet is an empty placeholder — and `Default` leaves `root` at 0, which
    /// indexes nothing. Everything that dereferences the root must check first.
    pub fn is_ready(&self) -> bool {
        (self.root as usize) < self.flags.len()
    }

    #[inline]
    pub fn live(&self, i: u32) -> bool {
        (i as usize) < self.flags.len() && self.flags[i as usize] & F_INUSE != 0
    }

    #[inline]
    pub fn is_dir(&self, i: u32) -> bool {
        self.flags[i as usize] & F_DIR != 0
    }

    #[inline]
    pub fn name_bytes(&self, i: u32) -> &[u8] {
        let i = i as usize;
        if i >= self.name_off.len() || i >= self.name_len.len() {
            return &[];
        }
        let o = self.name_off[i] as usize;
        let l = self.name_len[i] as usize;
        // Reading while the service appends: a stale offset must not index past
        // the arena.
        match self.names.get(o..o + l) {
            Some(s) => s,
            None => &[],
        }
    }

    #[inline]
    pub fn name(&self, i: u32) -> &str {
        // The arena only ever receives valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(self.name_bytes(i)) }
    }

    pub fn push_name(&mut self, s: &str) -> (u32, u16) {
        let off = self.names.len() as u32;
        if !self.names.extend_from(s.as_bytes()) {
            self.exhausted = true;
            return (off, 0);
        }
        (off, s.len().min(u16::MAX as usize) as u16)
    }

    pub fn set_name(&mut self, i: u32, s: &str) {
        let (o, l) = self.push_name(s);
        self.name_off[i as usize] = o;
        self.name_len[i as usize] = l;
    }

    /// Full display path, e.g. `C:\Users\Marco\Desktop`.
    pub fn path_of(&self, i: u32) -> String {
        let mut parts: Vec<u32> = Vec::with_capacity(16);
        let mut cur = i;
        while cur != NONE {
            parts.push(cur);
            if cur == self.root {
                break;
            }
            let p = self.parent[cur as usize];
            if p == cur {
                break;
            }
            cur = p;
        }
        let mut s = String::with_capacity(96);
        for (n, &idx) in parts.iter().rev().enumerate() {
            let name = self.name(idx);
            if n == 0 {
                s.push_str(name);
                if !name.ends_with('\\') {
                    s.push('\\');
                }
            } else {
                s.push_str(name);
                if n + 1 < parts.len() {
                    s.push('\\');
                }
            }
        }
        if s.is_empty() {
            s.push_str(&self.vol.root_path);
        }
        s
    }

    /// Finds the node for a full path by walking down from the root.
    ///
    /// Lets a folder handed over by Explorer be shown inside the volume that
    /// already contains it, instead of being indexed a second time as if it
    /// were a volume of its own.
    pub fn node_for_path(&self, path: &str) -> Option<u32> {
        if self.root == NONE {
            return None;
        }
        let root_name = self.name(self.root).trim_end_matches('\\').to_string();
        let rest = path
            .trim_end_matches('\\')
            .strip_prefix(&root_name)
            .or_else(|| {
                // Case-insensitive retry: drive letters vary in case.
                path.get(..root_name.len())
                    .filter(|p| p.eq_ignore_ascii_case(&root_name))
                    .map(|_| &path[root_name.len()..])
            })?;

        let mut node = self.root;
        for part in rest.split('\\').filter(|s| !s.is_empty()) {
            node = self
                .children(node)
                .find(|&c| self.name(c).eq_ignore_ascii_case(part))?;
        }
        Some(node)
    }

    pub fn children(&self, i: u32) -> ChildIter<'_> {
        ChildIter {
            idx: self.index_first_child(i),
            steps: 0,
            ix: self,
        }
    }

    #[inline]
    fn index_first_child(&self, i: u32) -> u32 {
        if (i as usize) < self.first_child.len() {
            self.first_child[i as usize]
        } else {
            NONE
        }
    }

    /// The `k` largest children, sorted descending. Uses partial selection for
    /// huge directories so a folder with 500k entries stays cheap to visualise.
    pub fn top_children_by_size(&self, i: u32, k: usize) -> Vec<u32> {
        let mut v: Vec<u32> = self.children(i).collect();
        if v.len() > k {
            v.select_nth_unstable_by(k, |&a, &b| {
                self.size[b as usize].cmp(&self.size[a as usize])
            });
            v.truncate(k);
        }
        v.sort_unstable_by(|&a, &b| {
            self.size[b as usize]
                .cmp(&self.size[a as usize])
                .then_with(|| self.name_bytes(a).cmp(self.name_bytes(b)))
        });
        v
    }

    // ---- structural mutation -------------------------------------------------

    pub fn link_child(&mut self, child: u32, parent: u32) {
        self.parent[child as usize] = parent;
        self.next_sib[child as usize] = self.first_child[parent as usize];
        self.first_child[parent as usize] = child;
    }

    pub fn unlink_child(&mut self, child: u32) {
        let p = self.parent[child as usize];
        if p == NONE || p as usize >= self.first_child.len() {
            return;
        }
        let mut cur = self.first_child[p as usize];
        if cur == child {
            self.first_child[p as usize] = self.next_sib[child as usize];
            self.next_sib[child as usize] = NONE;
            return;
        }
        while cur != NONE {
            let nxt = self.next_sib[cur as usize];
            if nxt == child {
                self.next_sib[cur as usize] = self.next_sib[child as usize];
                self.next_sib[child as usize] = NONE;
                return;
            }
            cur = nxt;
        }
    }

    /// Adds the given deltas to every ancestor of `i` (exclusive of `i`).
    pub fn propagate(&mut self, i: u32, dsize: i64, dlog: i64, dfiles: i64) {
        let mut cur = self.parent[i as usize];
        let mut guard = 0;
        while cur != NONE && guard < 512 {
            let s = &mut self.size[cur as usize];
            *s = (*s as i64 + dsize).max(0) as u64;
            let l = &mut self.logical[cur as usize];
            *l = (*l as i64 + dlog).max(0) as u64;
            let f = &mut self.files[cur as usize];
            *f = (*f as i64 + dfiles).max(0) as u32;
            if cur == self.root {
                break;
            }
            let p = self.parent[cur as usize];
            if p == cur {
                break;
            }
            cur = p;
            guard += 1;
        }
    }

    pub fn grow_to(&mut self, n: usize) {
        if n <= self.flags.len() {
            return;
        }
        // Shared columns are zero-filled, so the sentinel has to be written
        // explicitly for the link fields.
        let old = self.flags.len();
        let ok = self.name_off.resize_to(n)
            & self.name_len.resize_to(n)
            & self.parent.resize_to(n)
            & self.first_child.resize_to(n)
            & self.next_sib.resize_to(n)
            & self.size.resize_to(n)
            & self.own.resize_to(n)
            & self.logical.resize_to(n)
            & self.files.resize_to(n)
            & self.mtime.resize_to(n)
            & self.flags.resize_to(n);
        if !ok {
            // Out of reserved room; the service republishes into a bigger
            // section, so leaving the index as-is is the safe outcome.
            self.exhausted = true;
            return;
        }
        for i in old..n {
            self.parent[i] = NONE;
            self.first_child[i] = NONE;
            self.next_sib[i] = NONE;
        }
    }

    pub fn push_entry(&mut self) -> u32 {
        let i = self.flags.len();
        self.grow_to(i + 1);
        i as u32
    }

    // ---- tree construction ---------------------------------------------------

    /// Rebuilds sibling lists from `parent`, then aggregates sizes bottom-up.
    /// Entries unreachable from the root are reparented to the root so no space
    /// silently disappears (this also breaks any parent cycles).
    pub fn build_tree(&mut self) {
        let n = self.len();
        let root = self.root;

        self.relink();
        let mut order = self.preorder();

        // Rescue orphans / cycles.
        let mut seen = vec![false; n];
        for &i in &order {
            seen[i as usize] = true;
        }
        let mut rescued = 0usize;
        for i in 0..n {
            if self.flags[i] & F_INUSE != 0 && !seen[i] && i as u32 != root {
                self.parent[i] = root;
                rescued += 1;
            }
        }
        if rescued > 0 {
            self.relink();
            order = self.preorder();
        }

        // Reset aggregates to own values, then fold children into parents.
        for &i in &order {
            let i = i as usize;
            self.size[i] = self.own[i];
            if self.flags[i] & F_DIR == 0 {
                self.files[i] = 1;
            } else {
                self.files[i] = 0;
                self.logical[i] = 0;
            }
        }
        let mut dirs = 0u64;
        let mut files = 0u64;
        for &i in order.iter().rev() {
            let i = i as usize;
            if self.flags[i] & F_DIR != 0 {
                dirs += 1;
            } else {
                files += 1;
            }
            let p = self.parent[i];
            if p == NONE || p as usize == i {
                continue;
            }
            let (s, l, f) = (self.size[i], self.logical[i], self.files[i]);
            let p = p as usize;
            self.size[p] += s;
            self.logical[p] += l;
            self.files[p] += f;
        }
        self.total_dirs = dirs.saturating_sub(1);
        self.total_files = files;
        self.generation += 1;
    }

    fn relink(&mut self) {
        for v in self.first_child.iter_mut() {
            *v = NONE;
        }
        for v in self.next_sib.iter_mut() {
            *v = NONE;
        }
        let root = self.root;
        // Descending so the resulting head-insert list ends up ascending.
        for i in (0..self.len()).rev() {
            if self.flags[i] & F_INUSE == 0 || i as u32 == root {
                continue;
            }
            let p = self.parent[i];
            if p == NONE || p as usize >= self.len() || p == i as u32 {
                continue;
            }
            if self.flags[p as usize] & F_INUSE == 0 {
                continue;
            }
            self.next_sib[i] = self.first_child[p as usize];
            self.first_child[p as usize] = i as u32;
        }
    }

    /// Iterative DFS pre-order over everything reachable from the root.
    pub fn preorder(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.len());
        if self.root == NONE || self.root as usize >= self.len() {
            return out;
        }
        let mut stack = vec![self.root];
        while let Some(i) = stack.pop() {
            out.push(i);
            let mut c = self.first_child[i as usize];
            while c != NONE {
                stack.push(c);
                c = self.next_sib[c as usize];
            }
        }
        out
    }

}

/// Upper bound on a sibling chain. Far beyond any real directory, but finite.
const MAX_CHILDREN: u32 = 8_000_000;

pub struct ChildIter<'a> {
    idx: u32,
    steps: u32,
    ix: &'a Index,
}

impl<'a> Iterator for ChildIter<'a> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        // The service writes these links while we read them, so a value can be
        // caught mid-update. Bounds and a step budget turn what would be an
        // out-of-range access or an endless loop into a short list.
        if self.idx == NONE || (self.idx as usize) >= self.ix.next_sib.len() {
            return None;
        }
        self.steps += 1;
        if self.steps > MAX_CHILDREN {
            return None;
        }
        let cur = self.idx;
        self.idx = self.ix.next_sib[cur as usize];
        Some(cur)
    }
}

/// Case-insensitive byte fold covering ASCII and the Latin-1 block in UTF-8
/// (where the uppercase/lowercase pair also differs by 0x20 in the trail byte).
#[inline]
pub fn fold_at(s: &[u8], i: usize) -> u8 {
    let b = s[i];
    if b.is_ascii_uppercase() {
        b + 32
    } else if (0x80..=0x9E).contains(&b) && i > 0 && s[i - 1] == 0xC3 {
        b + 32
    } else {
        b
    }
}

pub fn fold_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    (0..b.len()).map(|i| fold_at(b, i)).collect()
}

/// Case-insensitive substring test. `needle` must already be folded.
pub fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    let n0 = needle[0];
    let n0u = n0.to_ascii_uppercase();
    let last = hay.len() - needle.len();
    let mut i = 0usize;
    while i <= last {
        let window = &hay[i..=last];
        let pos = if n0 == n0u {
            memchr::memchr(n0, window)
        } else {
            memchr::memchr2(n0, n0u, window)
        };
        let Some(p) = pos else { return false };
        let s = i + p;
        if (0..needle.len()).all(|k| fold_at(hay, s + k) == needle[k]) {
            return true;
        }
        i = s + 1;
    }
    false
}

/// Glob matcher supporting `*` and `?`. `pat` must already be folded.
pub fn wildcard_ci(hay: &[u8], pat: &[u8]) -> bool {
    let (mut h, mut p) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while h < hay.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == fold_at(hay, h)) {
            h += 1;
            p += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = p;
            mark = h;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            mark += 1;
            h = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}
