//! Interface language, loaded from plain text files next to the executable.
//!
//! The German source text is its own lookup key. A translation file maps that
//! text to another language; anything it does not mention simply stays German.
//! That has two consequences worth the slightly bulky keys: German needs no
//! file at all and can never go missing, and a half-finished translation shows
//! real sentences rather than `settings.view.icons.label`.
//!
//! Files live in `lang/` beside the binary (a `lang/` in the working directory
//! is checked too, which is where `cargo run` finds them). Adding a language is
//! dropping in one more file — nothing here enumerates them.

use std::collections::HashMap;
use std::path::PathBuf;

use parking_lot::RwLock;

/// The language a file describes, taken from its header.
#[derive(Clone, PartialEq)]
pub struct LangInfo {
    /// File stem, e.g. `en`. Also what gets written to the settings file.
    pub code: String,
    /// Endonym for the picker, e.g. "English".
    pub name: String,
}

/// German is not a file — it is what the source already says.
pub const SOURCE_CODE: &str = "de";
pub const SOURCE_NAME: &str = "Deutsch";

struct State {
    code: String,
    /// Leaked so lookups can hand out `&'static str` and callers stay free of
    /// lifetimes. Bounded by the size of one language file, and only ever grows
    /// when the user switches language.
    map: HashMap<&'static str, &'static str>,
}

static STATE: RwLock<Option<State>> = RwLock::new(None);

/// Directories searched for `lang/`, nearest first.
fn search_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("lang"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push(cwd.join("lang"));
    }
    out
}

fn file_for(code: &str) -> Option<PathBuf> {
    // Reject anything that could climb out of the directory: the code comes
    // from a settings file the user can edit by hand.
    if code.is_empty()
        || code.starts_with('_')
        || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    search_dirs()
        .into_iter()
        .map(|d| d.join(format!("{code}.lang")))
        .find(|p| p.is_file())
}

/// Splits a language file into its header value and its key/value pairs.
///
/// Format is `original = translation`, one per line. `#` starts a comment.
/// `\n` in a value becomes a line break so multi-line UI text stays on one line
/// in the file.
fn parse(text: &str) -> (Option<String>, Vec<(String, String)>) {
    let mut name = None;
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = split_pair(line) else {
            continue;
        };
        if k.eq_ignore_ascii_case("@name") {
            name = Some(v.to_string());
            continue;
        }
        if k.is_empty() || v.is_empty() {
            continue;
        }
        pairs.push((unescape(k), unescape(v)));
    }
    (name, pairs)
}

/// Splits a line at the first `=` that is not escaped.
///
/// Interface text contains equals signs of its own — "Klick = hineinzoomen" —
/// so a naive split on the first one would cut a key in half and the entry
/// would never be found again.
fn split_pair(line: &str) -> Option<(&str, &str)> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'=' => return Some((line[..i].trim(), line[i + 1..].trim())),
            _ => i += 1,
        }
    }
    None
}

fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('=') => out.push('='),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Every language that can be selected: German plus one entry per file found.
pub fn available() -> Vec<LangInfo> {
    let mut out = vec![LangInfo {
        code: SOURCE_CODE.into(),
        name: SOURCE_NAME.into(),
    }];
    for dir in search_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("lang") {
                continue;
            }
            let Some(code) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // `_template.lang` ships as a starting point for translators; it
            // is not a language anyone should be able to switch to.
            if code.starts_with('_') || code == SOURCE_CODE || out.iter().any(|l| l.code == code) {
                continue;
            }
            let name = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| parse(&t).0)
                .unwrap_or_else(|| code.to_uppercase());
            out.push(LangInfo {
                code: code.to_string(),
                name,
            });
        }
    }
    out
}

/// Switches language. Returns false when the file is missing or unreadable, in
/// which case the previous language stays in force.
pub fn set(code: &str) -> bool {
    if code.eq_ignore_ascii_case(SOURCE_CODE) {
        *STATE.write() = None;
        return true;
    }
    let Some(path) = file_for(code) else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let (_, pairs) = parse(&text);
    let mut map = HashMap::with_capacity(pairs.len());
    for (k, v) in pairs {
        // Identical entries carry no information and would only cost memory.
        if k == v {
            continue;
        }
        map.insert(&*Box::leak(k.into_boxed_str()), &*Box::leak(v.into_boxed_str()));
    }
    *STATE.write() = Some(State {
        code: code.to_string(),
        map,
    });
    true
}

pub fn current() -> String {
    STATE
        .read()
        .as_ref()
        .map(|s| s.code.clone())
        .unwrap_or_else(|| SOURCE_CODE.to_string())
}

/// Translates one piece of interface text.
///
/// The argument is the German original. Untranslated text passes straight
/// through, which is why a missing or partial file is never a broken UI.
pub fn t(source: &'static str) -> &'static str {
    match &*STATE.read() {
        Some(s) => s.map.get(source).copied().unwrap_or(source),
        None => source,
    }
}

/// Translates text carrying `{0}`, `{1}`, … placeholders and fills them in.
///
/// Positional rather than inline, because word order differs between languages
/// and a translator has to be able to move the values around. `{{` and `}}`
/// produce literal braces, as in `format!` — there is otherwise no way to tell
/// `{4}` in a regular expression from a reference to the fifth argument.
pub fn tf(source: &'static str, args: &[&str]) -> String {
    let pattern = t(source);
    let mut out = String::with_capacity(pattern.len() + args.len() * 8);
    let mut rest = pattern;
    while let Some(open) = rest.find(['{', '}']) {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let brace = rest.as_bytes()[open];
        if after.as_bytes().first() == Some(&brace) {
            out.push(brace as char);
            rest = &after[1..];
            continue;
        }
        // A lone `}` is not a placeholder; pass it along.
        let Some(close) = (brace == b'{').then(|| after.find('}')).flatten() else {
            out.push(brace as char);
            rest = after;
            continue;
        };
        match after[..close].parse::<usize>() {
            Ok(n) => out.push_str(args.get(n).copied().unwrap_or("")),
            // Not a number, so not ours — leave it exactly as written.
            Err(_) => {
                out.push('{');
                out.push_str(&after[..close]);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn untranslated_text_passes_through() {
        assert_eq!(super::t("Einstellungen"), "Einstellungen");
    }

    #[test]
    fn placeholders_can_be_reordered() {
        // The whole point of positional arguments: a translation may put the
        // unit before the number, or drop one entirely.
        assert_eq!(super::tf("{0} von {1}", &["3", "9"]), "3 von 9");
        assert_eq!(super::tf("{1}/{0}", &["a", "b"]), "b/a");
        assert_eq!(super::tf("{0} {5}", &["x"]), "x ");
    }

    #[test]
    fn braces_that_are_not_placeholders_survive() {
        // Named braces are left alone; numeric ones have to be escaped, since
        // `{4}` is indistinguishable from a reference to the fifth argument.
        assert_eq!(super::tf("{name} = {0}", &["x"]), "{name} = x");
        assert_eq!(super::tf("re:^\\d{{4}}_ {0}", &["ok"]), "re:^\\d{4}_ ok");
        assert_eq!(super::tf("100 % }", &[]), "100 % }");
    }

    #[test]
    fn parser_reads_header_comments_and_escapes() {
        let (name, pairs) = super::parse(
            "# a comment\n@name = English\nHallo = Hello\nZeile\\nUmbruch = Line\\nbreak\n\nleer =\n",
        );
        assert_eq!(name.as_deref(), Some("English"));
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[1].0, "Zeile\nUmbruch");
        assert_eq!(pairs[1].1, "Line\nbreak");
    }

    #[test]
    fn a_language_code_cannot_escape_the_directory() {
        assert!(super::file_for("../../etc/passwd").is_none());
        assert!(super::file_for("").is_none());
    }
}
