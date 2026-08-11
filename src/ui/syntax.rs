//! Lightweight syntax colouring for the text preview.
//!
//! Deliberately not `syntect`: it brings a regex engine and megabytes of syntax
//! definitions, and a disk analyser that exists to stay small and instant is
//! the wrong place for that. A preview pane does not need a parser — comments,
//! strings, numbers and keywords carry almost all of the readability, and a
//! single pass over the text gets them.
//!
//! Everything here is deliberately approximate. A nested template literal or a
//! regex containing a quote will occasionally be shaded wrongly; nothing breaks
//! when that happens, and the alternative costs far more than it is worth here.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

use crate::ui::theme;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lang {
    /// C, C++, C#, Java, JavaScript, Go, Swift, Kotlin, …
    CLike,
    Rust,
    Python,
    Shell,
    /// HTML, XML, SVG.
    Markup,
    /// JSON, YAML, TOML — quoted keys and scalars, `#` or `//` comments.
    Data,
    Ini,
    Sql,
    Plain,
}

pub fn lang_of(name: &str) -> Lang {
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Lang::Rust,
        "c" | "h" | "cpp" | "hpp" | "cc" | "cs" | "java" | "js" | "mjs" | "cjs" | "ts" | "tsx"
        | "jsx" | "go" | "swift" | "kt" | "kts" | "php" | "scala" | "dart" | "glsl" | "hlsl" => {
            Lang::CLike
        }
        "py" | "pyw" | "rb" | "pl" | "lua" | "r" => Lang::Python,
        "sh" | "bash" | "zsh" | "bat" | "cmd" | "ps1" | "psm1" => Lang::Shell,
        "html" | "htm" | "xml" | "svg" | "xhtml" | "xaml" | "vue" | "xsl" => Lang::Markup,
        "json" | "yaml" | "yml" | "toml" | "jsonc" | "css" | "scss" | "less" => Lang::Data,
        "ini" | "cfg" | "conf" | "properties" | "reg" | "desktop" => Lang::Ini,
        "sql" => Lang::Sql,
        _ => Lang::Plain,
    }
}

/// Colours, kept close to the rest of the interface rather than importing a
/// scheme that would look pasted in.
mod hue {
    use egui::Color32;
    pub const COMMENT: Color32 = Color32::from_rgb(0x6b, 0x77, 0x86);
    pub const STRING: Color32 = Color32::from_rgb(0x9c, 0xd6, 0x7f);
    pub const NUMBER: Color32 = Color32::from_rgb(0xe8, 0xb4, 0x6b);
    pub const KEYWORD: Color32 = Color32::from_rgb(0x7f, 0xb3, 0xff);
    pub const TAG: Color32 = Color32::from_rgb(0xff, 0x9a, 0x8c);
    pub const ATTR: Color32 = Color32::from_rgb(0xd8, 0xa8, 0xff);
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tok {
    Text,
    Comment,
    Str,
    Num,
    Keyword,
    Tag,
    Attr,
}

/// `.nfo` art is drawn at a size where an 80-column frame fits a normal pane.
pub const ART_SIZE: f32 = 12.0;

fn colour(t: Tok) -> Color32 {
    match t {
        Tok::Text => theme::TEXT,
        Tok::Comment => hue::COMMENT,
        Tok::Str => hue::STRING,
        Tok::Num => hue::NUMBER,
        Tok::Keyword => hue::KEYWORD,
        Tok::Tag => hue::TAG,
        Tok::Attr => hue::ATTR,
    }
}

const KW_COMMON: &[&str] = &[
    "if", "else", "for", "while", "do", "switch", "case", "default", "break", "continue",
    "return", "new", "delete", "class", "struct", "enum", "interface", "extends", "implements",
    "public", "private", "protected", "static", "const", "let", "var", "function", "void", "int",
    "float", "double", "bool", "char", "true", "false", "null", "this", "try", "catch", "finally",
    "throw", "import", "export", "from", "package", "namespace", "using", "typedef", "template",
    "async", "await", "yield", "typeof", "instanceof", "in", "of", "type",
];

const KW_RUST: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "usize", "isize", "u8", "u16", "u32", "u64", "i8", "i16",
    "i32", "i64", "f32", "f64", "bool", "str", "String", "Vec", "Option", "Some", "None", "Result",
    "Ok", "Err",
];

const KW_PYTHON: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield", "self", "end", "do", "then", "function", "local", "nil", "elseif", "require",
];

const KW_SHELL: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until", "case",
    "esac", "function", "return", "exit", "export", "local", "echo", "set", "unset", "source",
    "param", "begin", "end", "foreach", "try", "catch", "throw",
];

const KW_SQL: &[&str] = &[
    "select", "from", "where", "insert", "into", "values", "update", "set", "delete", "create",
    "table", "drop", "alter", "index", "join", "inner", "left", "right", "outer", "on", "group",
    "by", "order", "having", "limit", "offset", "distinct", "as", "and", "or", "not", "null",
    "primary", "key", "foreign", "references", "unique", "default", "case", "when", "then",
    "else", "end", "union", "all", "exists", "between", "like", "in",
];

fn keywords(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => KW_RUST,
        Lang::Python => KW_PYTHON,
        Lang::Shell => KW_SHELL,
        Lang::Sql => KW_SQL,
        Lang::CLike => KW_COMMON,
        _ => &[],
    }
}

/// Which comment openers apply. Returning them per language beats a single
/// table because `#` starts a comment in Python and a preprocessor line in C.
fn line_comments(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust | Lang::CLike => &["//"],
        Lang::Python | Lang::Shell => &["#"],
        Lang::Data => &["#", "//"],
        Lang::Ini => &[";", "#"],
        Lang::Sql => &["--"],
        _ => &[],
    }
}

fn has_block_comments(lang: Lang) -> bool {
    matches!(lang, Lang::Rust | Lang::CLike | Lang::Data)
}

/// Splits `text` into coloured runs. Byte ranges, so the caller can slice.
fn spans(text: &str, lang: Lang) -> Vec<(usize, usize, Tok)> {
    let b = text.as_bytes();
    let n = b.len();
    let mut out: Vec<(usize, usize, Tok)> = Vec::with_capacity(256);
    let mut i = 0;
    let kw = keywords(lang);
    let line_cmt = line_comments(lang);
    // Markup alternates between text and the inside of a tag; nothing else
    // needs state that survives a token.
    let mut in_tag = false;

    let push = |out: &mut Vec<(usize, usize, Tok)>, s: usize, e: usize, t: Tok| {
        if s >= e {
            return;
        }
        // Merging touching runs of the same colour keeps the layout job small.
        if let Some(last) = out.last_mut() {
            if last.1 == s && last.2 == t {
                last.1 = e;
                return;
            }
        }
        out.push((s, e, t));
    };

    while i < n {
        let c = b[i];

        if lang == Lang::Markup {
            if !in_tag {
                if c == b'<' {
                    if text[i..].starts_with("<!--") {
                        let end = text[i..].find("-->").map(|p| i + p + 3).unwrap_or(n);
                        push(&mut out, i, end, Tok::Comment);
                        i = end;
                        continue;
                    }
                    // `<tag` up to the first space or `>`.
                    let mut j = i + 1;
                    while j < n && b[j] != b'>' && !b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    push(&mut out, i, j, Tok::Tag);
                    i = j;
                    in_tag = true;
                    continue;
                }
                let mut j = i;
                while j < n && b[j] != b'<' {
                    j += 1;
                }
                push(&mut out, i, j, Tok::Text);
                i = j;
                continue;
            }
            match c {
                b'>' => {
                    push(&mut out, i, i + 1, Tok::Tag);
                    i += 1;
                    in_tag = false;
                    continue;
                }
                b'"' | b'\'' => {
                    let end = string_end(b, i, c, false);
                    push(&mut out, i, end, Tok::Str);
                    i = end;
                    continue;
                }
                c if c.is_ascii_alphabetic() || c == b'_' || c == b'-' || c == b':' => {
                    let mut j = i;
                    while j < n
                        && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'_' | b'-' | b':'))
                    {
                        j += 1;
                    }
                    push(&mut out, i, j, Tok::Attr);
                    i = j;
                    continue;
                }
                _ => {
                    push(&mut out, i, i + 1, Tok::Text);
                    i += 1;
                    continue;
                }
            }
        }

        // Line comments.
        if let Some(open) = line_cmt.iter().find(|o| text[i..].starts_with(**o)) {
            let _ = open;
            let end = text[i..].find('\n').map(|p| i + p).unwrap_or(n);
            push(&mut out, i, end, Tok::Comment);
            i = end;
            continue;
        }
        // Block comments.
        if has_block_comments(lang) && text[i..].starts_with("/*") {
            let end = text[i + 2..].find("*/").map(|p| i + 2 + p + 2).unwrap_or(n);
            push(&mut out, i, end, Tok::Comment);
            i = end;
            continue;
        }
        // Strings. A single quote in shell and Python is a string; in C-like
        // languages it is a character literal, which colours the same anyway.
        if c == b'"' || c == b'\'' || (c == b'`' && matches!(lang, Lang::CLike | Lang::Shell)) {
            let raw = lang == Lang::Shell && c == b'\'';
            let end = string_end(b, i, c, raw);
            push(&mut out, i, end, Tok::Str);
            i = end;
            continue;
        }
        // Numbers, including hex and floats. A digit directly after a letter is
        // part of an identifier, not a number.
        if c.is_ascii_digit() && (i == 0 || !is_word(b[i - 1])) {
            let mut j = i;
            while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'.' || b[j] == b'_') {
                j += 1;
            }
            push(&mut out, i, j, Tok::Num);
            i = j;
            continue;
        }
        // Identifiers and keywords.
        if is_word_start(c) {
            let mut j = i;
            while j < n && is_word(b[j]) {
                j += 1;
            }
            let word = &text[i..j];
            let t = if kw.contains(&word) {
                Tok::Keyword
            } else {
                Tok::Text
            };
            push(&mut out, i, j, t);
            i = j;
            continue;
        }

        push(&mut out, i, i + 1, Tok::Text);
        i += 1;
    }
    out
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// Index just past the closing quote, or the end of the line if there is none —
/// an unterminated quote must not paint the whole rest of the file.
fn string_end(b: &[u8], start: usize, quote: u8, raw: bool) -> usize {
    let n = b.len();
    let mut i = start + 1;
    while i < n {
        match b[i] {
            b'\\' if !raw => i += 2,
            b'\n' => return i,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    n
}

/// Builds the coloured text egui will draw.
///
/// `highlight_ranges` are byte ranges to mark — the content-search hit, which
/// has to win over whatever colour the token would otherwise get.
pub fn layout(
    text: &str,
    lang: Lang,
    size: f32,
    highlight_ranges: &[(usize, usize)],
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let font = FontId::monospace(size);
    let runs = if lang == Lang::Plain {
        vec![(0, text.len(), Tok::Text)]
    } else {
        spans(text, lang)
    };

    for (s, e, t) in runs {
        // Split the run wherever a search hit overlaps it.
        let mut cur = s;
        while cur < e {
            let hit = highlight_ranges
                .iter()
                .find(|(hs, he)| *hs < e && *he > cur && *he > *hs);
            match hit {
                Some(&(hs, _)) if hs.max(cur) > cur => {
                    append(&mut job, text, cur, hs.max(cur), &font, colour(t), false);
                    cur = hs.max(cur);
                }
                Some(&(_, he)) => {
                    let end = he.min(e);
                    append(&mut job, text, cur, end, &font, colour(t), true);
                    cur = end;
                }
                None => {
                    append(&mut job, text, cur, e, &font, colour(t), false);
                    cur = e;
                }
            }
        }
    }
    job
}

fn append(
    job: &mut LayoutJob,
    text: &str,
    s: usize,
    e: usize,
    font: &FontId,
    colour: Color32,
    marked: bool,
) {
    if s >= e || e > text.len() || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
        return;
    }
    job.append(
        &text[s..e],
        0.0,
        TextFormat {
            font_id: font.clone(),
            color: if marked { theme::BG } else { colour },
            background: if marked {
                theme::ACCENT
            } else {
                Color32::TRANSPARENT
            },
            ..Default::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{lang_of, spans, Lang, Tok};

    /// Everything the spans cover, in order, as (text, kind) — easier to assert
    /// on than byte offsets.
    fn toks(text: &str, lang: Lang) -> Vec<(&str, Tok)> {
        spans(text, lang)
            .into_iter()
            .map(|(s, e, t)| (&text[s..e], t))
            .collect()
    }

    #[test]
    fn extensions_map_to_languages() {
        assert_eq!(lang_of("main.rs"), Lang::Rust);
        assert_eq!(lang_of("App.TSX"), Lang::CLike);
        assert_eq!(lang_of("setup.py"), Lang::Python);
        assert_eq!(lang_of("index.html"), Lang::Markup);
        assert_eq!(lang_of("Cargo.toml"), Lang::Data);
        assert_eq!(lang_of("notes.txt"), Lang::Plain);
    }

    #[test]
    fn comments_strings_numbers_and_keywords() {
        let t = toks("let x = 42; // note\n", Lang::Rust);
        assert!(t.contains(&("let", Tok::Keyword)));
        assert!(t.contains(&("42", Tok::Num)));
        assert!(t.contains(&("// note", Tok::Comment)));
        assert!(!t.iter().any(|(s, k)| *s == "x" && *k == Tok::Keyword));
    }

    /// An unterminated quote must not swallow the rest of the file.
    #[test]
    fn an_unclosed_string_stops_at_the_line_end() {
        let t = toks("a = \"oops\nlet b = 1\n", Lang::Rust);
        assert!(t.contains(&("\"oops", Tok::Str)));
        assert!(t.contains(&("let", Tok::Keyword)), "{t:?}");
    }

    #[test]
    fn escapes_do_not_end_a_string() {
        let t = toks(r#"s = "a\"b" ;"#, Lang::Rust);
        assert!(t.contains(&(r#""a\"b""#, Tok::Str)), "{t:?}");
    }

    #[test]
    fn markup_separates_tags_attributes_and_values() {
        let t = toks(r#"<a href="x">hi</a>"#, Lang::Markup);
        assert!(t.contains(&("<a", Tok::Tag)));
        assert!(t.contains(&("href", Tok::Attr)));
        assert!(t.contains(&("\"x\"", Tok::Str)));
        assert!(t.contains(&("hi", Tok::Text)));
    }

    #[test]
    fn a_hash_is_a_comment_in_python_but_not_in_c() {
        assert!(toks("# hi\n", Lang::Python).contains(&("# hi", Tok::Comment)));
        assert!(!toks("#include <a>\n", Lang::CLike)
            .iter()
            .any(|(_, k)| *k == Tok::Comment));
    }

    #[test]
    fn sql_uses_double_dash_comments() {
        let t = toks("select 1 -- why\n", Lang::Sql);
        assert!(t.contains(&("select", Tok::Keyword)));
        assert!(t.contains(&("-- why", Tok::Comment)), "{t:?}");
    }

    /// A digit inside a name is part of the name, not a number of its own.
    /// Touching runs of one colour are merged, so `x2` shows up inside a plain
    /// text run rather than as a token by itself.
    #[test]
    fn identifiers_containing_digits_stay_whole() {
        let t = toks("let x2 = 1;", Lang::Rust);
        assert!(!t.iter().any(|(s, k)| *k == Tok::Num && s.contains('x')), "{t:?}");
        assert!(t.iter().any(|(s, k)| *k == Tok::Text && s.contains("x2")), "{t:?}");
        assert!(t.contains(&("1", Tok::Num)), "{t:?}");
    }

    /// The spans must tile the input exactly — no gap, no overlap — or the
    /// layout would silently drop or duplicate characters.
    #[test]
    fn spans_cover_the_input_exactly() {
        let samples = [
            ("fn main() { let s = \"hi\"; /* c */ }", Lang::Rust),
            ("<div class='a'>text<!-- c --></div>", Lang::Markup),
            ("def f(): # c\n  return 'x'\n", Lang::Python),
            ("key = 1 ; comment\n[section]\n", Lang::Ini),
            ("", Lang::Rust),
            ("äöü ✓ 日本語", Lang::Rust),
        ];
        for (text, lang) in samples {
            let mut at = 0;
            for (s, e, _) in spans(text, lang) {
                assert_eq!(s, at, "gap or overlap in {text:?}");
                assert!(e > s && e <= text.len());
                at = e;
            }
            assert_eq!(at, text.len(), "spans stop short of the end in {text:?}");
        }
    }

    /// Every boundary has to land on a character boundary, or slicing panics.
    #[test]
    fn spans_never_split_a_character() {
        let text = "let s = \"日本語\"; // ökonomisch\n";
        for (s, e, _) in spans(text, Lang::Rust) {
            assert!(text.is_char_boundary(s) && text.is_char_boundary(e), "{s}..{e}");
        }
    }
}
