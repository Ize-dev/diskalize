//! Live search over the index.
//!
//! No inverted index and no lowercase shadow copy — a parallel scan over the
//! name arena with a case-folding matcher is fast enough (a few milliseconds for
//! millions of entries) and costs zero extra memory.

use rayon::prelude::*;
use regex::bytes::Regex;

use crate::fmt;
use crate::index::{contains_ci, fold_at, fold_bytes, wildcard_ci, Index};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

impl Cmp {
    fn test<T: PartialOrd>(&self, a: T, b: T) -> bool {
        match self {
            Cmp::Lt => a < b,
            Cmp::Le => a <= b,
            Cmp::Gt => a > b,
            Cmp::Ge => a >= b,
            Cmp::Eq => a == b,
        }
    }
}

pub enum Matcher {
    Sub(Vec<u8>),
    Glob(Vec<u8>),
}

impl Matcher {
    pub fn new(s: &str) -> Self {
        let f = fold_bytes(s);
        if s.contains('*') || s.contains('?') {
            Matcher::Glob(f)
        } else {
            Matcher::Sub(f)
        }
    }
    #[inline]
    fn test(&self, hay: &[u8]) -> bool {
        match self {
            Matcher::Sub(n) => contains_ci(hay, n),
            Matcher::Glob(p) => wildcard_ci(hay, p),
        }
    }
}

/// A named family of file extensions, e.g. everything that counts as audio.
///
/// The UI offers these as one-click filters and the query language accepts them
/// as `type:<key>`, so both routes end up as the same `Term::Ext`.
pub struct Category {
    /// What `type:` matches on, and what the toggle writes into the query.
    pub key: &'static str,
    pub label: &'static str,
    pub exts: &'static [&'static str],
}

pub const CATEGORIES: &[Category] = &[
    Category {
        key: "audio",
        label: "Audio",
        exts: &[
            "mp3", "flac", "wav", "m4a", "aac", "ogg", "opus", "wma", "aiff", "aif", "alac", "ape",
            "mka", "mid", "midi", "dsf", "wv",
        ],
    },
    Category {
        key: "video",
        label: "Video",
        exts: &[
            "mp4", "mkv", "avi", "mov", "wmv", "m4v", "flv", "webm", "mpg", "mpeg", "m2ts", "ts",
            "vob", "3gp", "ogv", "divx", "rmvb",
        ],
    },
    Category {
        key: "bild",
        label: "Bilder",
        exts: &[
            "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "avif",
            "svg", "ico", "psd", "raw", "cr2", "cr3", "nef", "arw", "dng", "jxl",
        ],
    },
    Category {
        key: "dokument",
        label: "Dokumente",
        exts: &[
            "pdf", "doc", "docx", "odt", "rtf", "txt", "md", "xls", "xlsx", "ods", "csv", "ppt",
            "pptx", "odp", "epub", "mobi", "azw3", "djvu", "pages", "numbers", "key",
        ],
    },
    Category {
        key: "archiv",
        label: "Archive",
        exts: &[
            "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "iso", "cab", "arj", "lzh", "tgz",
            "tbz", "txz", "wim",
        ],
    },
    Category {
        key: "code",
        label: "Code",
        exts: &[
            "rs", "c", "h", "cpp", "hpp", "cs", "java", "py", "js", "mjs", "ts", "tsx", "jsx",
            "go", "rb", "php", "swift", "kt", "lua", "pl", "sh", "bat", "cmd", "ps1", "sql",
            "html", "css", "scss", "json", "xml", "yaml", "yml", "toml", "ini",
        ],
    },
    Category {
        key: "programm",
        label: "Programme",
        exts: &[
            "exe", "dll", "msi", "sys", "bin", "so", "dylib", "appx", "msix", "jar", "apk", "deb",
            "rpm", "dmg", "pkg",
        ],
    },
];

pub fn category(key: &str) -> Option<&'static Category> {
    let k = key.to_ascii_lowercase();
    CATEGORIES.iter().find(|c| {
        c.key == k
            || c.label.eq_ignore_ascii_case(&k)
            // The English names have to work too, since the UI can run in
            // English while the query language stays one language.
            || matches!(
                (c.key, k.as_str()),
                ("bild", "image" | "images" | "picture" | "pictures" | "bilder")
                    | ("dokument", "doc" | "docs" | "document" | "documents" | "dokumente")
                    | ("archiv", "archive" | "archives")
                    | ("programm", "app" | "apps" | "binary" | "executable" | "programme")
            )
    })
}

pub enum Term {
    Name(Matcher),
    Path(Matcher),
    Ext(Vec<Vec<u8>>),
    Size(Cmp, u64),
    Date(Cmp, i64),
    OnlyDirs,
    OnlyFiles,
    Rx(Regex),
}

#[derive(Default)]
pub struct Query {
    pub terms: Vec<(bool, Term)>, // (negated, term)
    pub error: Option<String>,
    pub is_empty: bool,
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn split_cmp(v: &str) -> (Cmp, &str) {
    if let Some(r) = v.strip_prefix(">=") {
        (Cmp::Ge, r)
    } else if let Some(r) = v.strip_prefix("<=") {
        (Cmp::Le, r)
    } else if let Some(r) = v.strip_prefix('>') {
        (Cmp::Gt, r)
    } else if let Some(r) = v.strip_prefix('<') {
        (Cmp::Lt, r)
    } else if let Some(r) = v.strip_prefix('=') {
        (Cmp::Eq, r)
    } else {
        (Cmp::Ge, v)
    }
}

pub fn parse(input: &str) -> Query {
    let mut q = Query {
        is_empty: input.trim().is_empty(),
        ..Default::default()
    };
    if q.is_empty {
        return q;
    }

    for raw in tokenize(input) {
        let (neg, tok) = match raw.strip_prefix('!').or_else(|| raw.strip_prefix('-')) {
            Some(rest) if !rest.is_empty() => (true, rest.to_string()),
            _ => (false, raw),
        };

        let term = match tok.split_once(':') {
            Some(("ext", v)) | Some(("e", v)) => Term::Ext(
                v.split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| fold_bytes(s.trim_start_matches('.')))
                    .collect(),
            ),
            Some(("size", v)) | Some(("s", v)) => {
                let (c, rest) = split_cmp(v);
                match fmt::parse_size(rest) {
                    Some(b) => Term::Size(c, b),
                    None => {
                        q.error = Some(format!("Größe nicht lesbar: '{rest}'"));
                        continue;
                    }
                }
            }
            Some(("date", v)) | Some(("d", v)) => {
                let (c, rest) = split_cmp(v);
                match fmt::parse_date(rest) {
                    Some(t) => Term::Date(c, t),
                    None => {
                        q.error = Some(format!("Datum nicht lesbar: '{rest}'"));
                        continue;
                    }
                }
            }
            Some(("path", v)) | Some(("p", v)) => Term::Path(Matcher::new(v)),
            Some(("re", v)) | Some(("regex", v)) => match Regex::new(&format!("(?i){v}")) {
                Ok(r) => Term::Rx(r),
                Err(e) => {
                    q.error = Some(format!("Regex ungültig: {e}"));
                    continue;
                }
            },
            Some(("is", v)) | Some(("type", v)) | Some(("t", v)) => {
                match v.to_ascii_lowercase().as_str() {
                    "dir" | "folder" | "ordner" => Term::OnlyDirs,
                    "file" | "datei" => Term::OnlyFiles,
                    // A named group expands to the extensions behind it, so
                    // `type:audio` costs exactly what `ext:mp3,flac,…` costs.
                    other => match category(other) {
                        Some(c) => Term::Ext(c.exts.iter().map(|e| fold_bytes(e)).collect()),
                        None => {
                            q.error = Some(format!("unbekannter Typ: '{other}'"));
                            continue;
                        }
                    },
                }
            }
            _ => Term::Name(Matcher::new(&tok)),
        };
        q.terms.push((neg, term));
    }
    q
}

#[inline]
fn ext_of(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(p) if p + 1 < name.len() => &name[p + 1..],
        _ => &[],
    }
}

#[inline]
fn eq_ci(a: &[u8], folded: &[u8]) -> bool {
    a.len() == folded.len() && (0..a.len()).all(|i| fold_at(a, i) == folded[i])
}

fn eval(ix: &Index, i: u32, q: &Query) -> bool {
    let name = ix.name_bytes(i);
    let mut path: Option<Vec<u8>> = None;
    for (neg, t) in &q.terms {
        let hit = match t {
            Term::Name(m) => m.test(name),
            Term::Ext(list) => {
                let e = ext_of(name);
                !e.is_empty() && list.iter().any(|x| eq_ci(e, x))
            }
            Term::Size(c, v) => c.test(ix.size[i as usize], *v),
            Term::Date(c, v) => c.test(ix.mtime[i as usize] as i64, *v),
            Term::OnlyDirs => ix.is_dir(i),
            Term::OnlyFiles => !ix.is_dir(i),
            Term::Rx(rx) => rx.is_match(name),
            Term::Path(m) => {
                let p = path.get_or_insert_with(|| ix.path_of(i).into_bytes());
                m.test(p)
            }
        };
        if hit == *neg {
            return false;
        }
    }
    true
}

pub struct Results {
    pub hits: Vec<u32>,
    pub total: usize,
}

/// Every node below `scope`, collected with an explicit stack.
///
/// A scoped search walks this instead of the whole index. Filtering a folder
/// with a few thousand entries then costs a few thousand comparisons rather
/// than several million, which is what made switching folders feel sluggish
/// while a filter was active.
/// Every entry below `scope`, files and folders alike.
///
/// Public because the content search needs the same set: browsing a folder
/// lists only its direct children, but "find this text under here" plainly
/// means the whole tree.
pub fn subtree(ix: &Index, scope: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(1024);
    let mut stack = vec![scope];
    while let Some(i) = stack.pop() {
        let mut c = ix.first_child[i as usize];
        while c != crate::index::NONE {
            out.push(c);
            if ix.flags[c as usize] & crate::index::F_DIR != 0 {
                stack.push(c);
            }
            c = ix.next_sib[c as usize];
        }
    }
    out
}

/// Runs `q` across the index in parallel, returning the `limit` largest hits.
/// `scope` restricts the result to one subtree.
pub fn run(ix: &Index, q: &Query, limit: usize, scope: Option<u32>) -> Results {
    if q.is_empty || q.terms.is_empty() {
        return Results {
            hits: Vec::new(),
            total: 0,
        };
    }

    let n = ix.len() as u32;
    let root = ix.root;
    let keep =
        |i: u32| i != root && ix.live(i) && ix.name_len[i as usize] > 0 && eval(ix, i, q);

    let mut hits: Vec<u32> = match scope.filter(|s| *s != root) {
        Some(s) => subtree(ix, s)
            .into_par_iter()
            .filter(|&i| keep(i))
            .collect(),
        None => (0..n).into_par_iter().filter(|&i| keep(i)).collect(),
    };

    let total = hits.len();
    if total > limit {
        hits.select_nth_unstable_by(limit, |&a, &b| {
            ix.size[b as usize].cmp(&ix.size[a as usize])
        });
        hits.truncate(limit);
    }
    hits.par_sort_unstable_by(|&a, &b| {
        ix.size[b as usize]
            .cmp(&ix.size[a as usize])
            .then_with(|| ix.name_bytes(a).cmp(ix.name_bytes(b)))
    });

    Results { hits, total }
}

/// The syntax cheat sheet, translated as one block.
///
/// It stays whole rather than being split per line: the columns are aligned by
/// hand for a monospaced font, and only a translator looking at the whole thing
/// can keep that alignment in another language.
pub fn syntax_help() -> &'static str {
    // Written out here rather than behind a constant so `extract_lang.py` can
    // see the literal — it only picks up text passed directly to `t`.
    crate::i18n::t(
        "Suche\n\
         \x20 foo bar          alle Begriffe müssen vorkommen (UND)\n\
         \x20 \"mein film\"      Anführungszeichen für Leerzeichen\n\
         \x20 *.mp4  bild_??    Wildcards * und ?\n\
         \x20 !temp  -cache     Begriff ausschließen\n\
         \n\
         Filter\n\
         \x20 ext:mp4,mkv       Dateiendung\n\
         \x20 type:audio        Dateiart — audio, video, bild, dokument,\n\
         \x20                   archiv, code, programm\n\
         \x20 size:>100mb       Größe  (>, >=, <, <=, =; k/m/g/t)\n\
         \x20 date:>2024-01-01  Änderungsdatum\n\
         \x20 path:\\Users\\      im vollständigen Pfad suchen\n\
         \x20 is:folder         nur Ordner   (is:file für Dateien)\n\
         \x20 re:^\\d{4}_.*      regulärer Ausdruck",
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn categories_expand_to_extensions() {
        let q = super::parse("type:audio");
        assert!(q.error.is_none());
        match q.terms.as_slice() {
            [(false, super::Term::Ext(list))] => {
                assert!(list.iter().any(|e| e == b"flac"));
                assert!(!list.iter().any(|e| e == b"mp4"));
            }
            _ => panic!("expected a single extension term"),
        }
    }

    #[test]
    fn category_lookup_takes_both_languages() {
        for name in ["bild", "Bilder", "image", "PICTURES"] {
            assert_eq!(super::category(name).map(|c| c.key), Some("bild"), "{name}");
        }
        assert!(super::category("nonsense").is_none());
    }

    /// Extensions may sit in two groups only where the extension itself is
    /// genuinely ambiguous. Anything else is a slip in the table.
    #[test]
    fn categories_only_overlap_where_intended() {
        // `.ts` is both an MPEG transport stream and a TypeScript source file,
        // and there is no way to tell from the name alone.
        const ALLOWED: &[(&str, &str, &str)] = &[("video", "code", "ts")];
        for (i, a) in super::CATEGORIES.iter().enumerate() {
            for b in &super::CATEGORIES[i + 1..] {
                for e in a.exts.iter().filter(|e| b.exts.contains(e)) {
                    assert!(
                        ALLOWED.contains(&(a.key, b.key, e)),
                        "{} and {} both claim .{e}",
                        a.key,
                        b.key
                    );
                }
            }
        }
    }

    #[test]
    fn is_folder_still_works() {
        assert!(matches!(
            super::parse("is:folder").terms.as_slice(),
            [(false, super::Term::OnlyDirs)]
        ));
        assert!(super::parse("type:quatsch").error.is_some());
    }
}
