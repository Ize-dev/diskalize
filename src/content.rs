//! Searching inside files.
//!
//! The name index answers in milliseconds because it never leaves memory.
//! Content search cannot: it has to read the files. What it *can* do is read as
//! few of them as possible, so the name query runs first and this only ever
//! looks at what came back — `type:code fehler` reads the code files, not the
//! disk.
//!
//! Reading is bounded on purpose. Files past `MAX_BYTES` are searched only up
//! to that point, and anything that looks binary is skipped after the first
//! block, because scanning a 4 GB disk image for a word is never what was
//! meant.

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use rayon::prelude::*;

/// Most text worth searching is far smaller; this is a backstop against a log
/// file that grew to gigabytes.
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// How much has to be read before a file can be called binary.
const SNIFF: usize = 4096;

pub struct Hit {
    /// Index of the candidate this hit belongs to, as handed in.
    pub which: usize,
    /// Byte offset of the first match inside the file.
    pub at: usize,
    /// The line the match sits on, trimmed for display.
    pub line: String,
    pub matches: usize,
}

/// Progress and cancellation for a running search.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub cancel: AtomicBool,
}

/// A file that holds no text is not worth reading further.
///
/// A NUL byte is the giveaway — no text encoding we can display produces one,
/// and every binary format has them within the first few kilobytes.
fn looks_binary(buf: &[u8]) -> bool {
    buf.contains(&0)
}

/// Case-insensitive substring search over bytes, ASCII-folded.
///
/// Deliberately not a regex: the term comes from a search box, and folding both
/// sides while comparing costs nothing next to reading the file.
fn find_all(hay: &[u8], needle: &[u8], cap: usize) -> Vec<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return Vec::new();
    }
    let first = needle[0].to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        // memchr over the folded first byte would need a folded copy of the
        // whole file; scanning for either case is cheaper.
        match memchr::memchr2(first, first.to_ascii_uppercase(), &hay[i..]) {
            None => break,
            Some(p) => {
                let at = i + p;
                if at + needle.len() <= hay.len()
                    && hay[at..at + needle.len()]
                        .iter()
                        .zip(needle)
                        .all(|(h, n)| h.to_ascii_lowercase() == n.to_ascii_lowercase())
                {
                    out.push(at);
                    if out.len() >= cap {
                        return out;
                    }
                    i = at + needle.len();
                } else {
                    i = at + 1;
                }
            }
        }
    }
    out
}

/// The line containing `at`, trimmed and shortened for a result row.
fn line_at(buf: &[u8], at: usize) -> String {
    let start = buf[..at].iter().rposition(|&c| c == b'\n').map_or(0, |p| p + 1);
    let end = buf[at..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(buf.len(), |p| at + p);
    let raw = String::from_utf8_lossy(&buf[start..end]);
    if raw.chars().count() <= 200 {
        return raw.trim().to_string();
    }
    // A minified line can be the whole file. Window it around the match rather
    // than showing its first 200 characters, which would never contain the hit.
    // Lossy conversion can shift offsets, so clamp before slicing.
    let rel = (at - start).min(raw.len());
    let mut from = rel.saturating_sub(60);
    while from > 0 && !raw.is_char_boundary(from) {
        from -= 1;
    }
    let window: String = raw[from..].chars().take(200).collect();
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.push_str(window.trim_end());
    out
}

/// Searches one file. `None` when it holds no match, cannot be read, or is not
/// text at all.
pub fn search_file(path: &str, needle: &[u8]) -> Option<(usize, String, usize)> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(SNIFF);
    // Read the head first: most files are ruled out by it, and a binary one is
    // dropped before its whole body is pulled in.
    buf.resize(SNIFF, 0);
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    if n == 0 || looks_binary(&buf) {
        return None;
    }
    if n == SNIFF {
        // More to come: cap the total and read the rest.
        let mut rest = Vec::new();
        f.take((MAX_BYTES - SNIFF) as u64).read_to_end(&mut rest).ok()?;
        if looks_binary(&rest) {
            return None;
        }
        buf.extend_from_slice(&rest);
    }
    let hits = find_all(&buf, needle, 500);
    let first = *hits.first()?;
    Some((first, line_at(&buf, first), hits.len()))
}

/// Searches many files in parallel.
///
/// `paths` is whatever the name query produced, so the caller decides the scope
/// — one folder, one drive, or everything.
pub fn search(paths: &[String], needle: &str, limit: usize, progress: &Arc<Progress>) -> Vec<Hit> {
    let needle = needle.as_bytes().to_vec();
    if needle.is_empty() {
        return Vec::new();
    }
    progress.total.store(paths.len(), Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);

    let found = AtomicUsize::new(0);
    let mut hits: Vec<Hit> = paths
        .par_iter()
        .enumerate()
        .filter_map(|(which, p)| {
            if progress.cancel.load(Ordering::Relaxed) || found.load(Ordering::Relaxed) >= limit {
                return None;
            }
            let r = search_file(p, &needle);
            progress.done.fetch_add(1, Ordering::Relaxed);
            let (at, line, matches) = r?;
            found.fetch_add(1, Ordering::Relaxed);
            Some(Hit {
                which,
                at,
                line,
                matches,
            })
        })
        .collect();
    // Back into the order the caller handed them in, which is the order the
    // list is already sorted by.
    hits.sort_unstable_by_key(|h| h.which);
    hits.truncate(limit);
    hits
}

/// Byte ranges of every match in `text`, for highlighting a preview.
pub fn ranges(text: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    find_all(text.as_bytes(), needle.as_bytes(), 5000)
        .into_iter()
        .map(|s| (s, s + needle.len()))
        // A match that straddles a character boundary would panic on slicing.
        .filter(|(s, e)| text.is_char_boundary(*s) && text.is_char_boundary(*e))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_every_occurrence_ignoring_case() {
        let hay = b"Fehler hier, fehler dort, FEHLER";
        assert_eq!(find_all(hay, b"fehler", 100), vec![0, 13, 26]);
        assert_eq!(find_all(hay, b"FEHLER", 100), vec![0, 13, 26]);
        assert!(find_all(hay, b"nichts", 100).is_empty());
    }

    #[test]
    fn overlapping_needles_advance_past_the_match() {
        // "aaa" in "aaaa" starts at 0; the next search resumes after it, so the
        // result is the non-overlapping set rather than an infinite walk.
        assert_eq!(find_all(b"aaaa", b"aaa", 100), vec![0]);
        assert_eq!(find_all(b"abab", b"ab", 100), vec![0, 2]);
    }

    #[test]
    fn the_cap_stops_the_scan() {
        let hay = vec![b'x'; 1000];
        assert_eq!(find_all(&hay, b"x", 7).len(), 7);
    }

    #[test]
    fn an_empty_needle_matches_nothing() {
        assert!(find_all(b"abc", b"", 10).is_empty());
        assert!(ranges("abc", "").is_empty());
    }

    #[test]
    fn nul_bytes_mark_a_file_as_binary() {
        assert!(looks_binary(b"MZ\0\0"));
        assert!(!looks_binary(b"plain text\n"));
    }

    #[test]
    fn the_reported_line_is_the_one_holding_the_match() {
        let buf = b"first\nsecond has it\nthird\n";
        let at = find_all(buf, b"has", 1)[0];
        assert_eq!(line_at(buf, at), "second has it");
    }

    #[test]
    fn a_very_long_line_is_cut_around_the_match() {
        let mut buf = vec![b'a'; 500];
        buf.extend_from_slice(b"needle");
        buf.extend(std::iter::repeat_n(b'b', 500));
        let at = find_all(&buf, b"needle", 1)[0];
        let line = line_at(&buf, at);
        assert!(line.starts_with('…'), "{line:.20}");
        assert!(line.contains("needle"), "match cut out of its own line");
        assert!(line.chars().count() <= 201);
    }

    #[test]
    fn ranges_never_split_a_character() {
        // The needle appears at a byte offset inside a multi-byte character's
        // neighbourhood; anything that would slice one is dropped.
        let text = "äöü needle üöä";
        let r = ranges(text, "needle");
        assert_eq!(r.len(), 1);
        let (s, e) = r[0];
        assert_eq!(&text[s..e], "needle");
    }

    #[test]
    fn searching_real_files() {
        let dir = std::env::temp_dir().join("dkz_content_test");
        let _ = std::fs::create_dir_all(&dir);
        let text = dir.join("a.txt");
        let bin = dir.join("b.bin");
        std::fs::write(&text, "hello\nfindme here\nbye\n").unwrap();
        std::fs::write(&bin, [0x00, 0x66, 0x69, 0x6e, 0x64, 0x6d, 0x65]).unwrap();

        let paths = vec![
            text.to_string_lossy().into_owned(),
            bin.to_string_lossy().into_owned(),
            dir.join("missing.txt").to_string_lossy().into_owned(),
        ];
        let progress = Arc::new(Progress::default());
        let hits = search(&paths, "findme", 100, &progress);

        assert_eq!(hits.len(), 1, "binary and missing files must be skipped");
        assert_eq!(hits[0].which, 0);
        assert_eq!(hits[0].line, "findme here");
        assert_eq!(hits[0].matches, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_stops_the_search() {
        let progress = Arc::new(Progress::default());
        progress.cancel.store(true, Ordering::Relaxed);
        let paths: Vec<String> = (0..50).map(|i| format!("nonexistent-{i}")).collect();
        assert!(search(&paths, "x", 100, &progress).is_empty());
    }
}
