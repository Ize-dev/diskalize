//! Checks the shipped language files against the source they translate.
//!
//! Translations drift silently: a label gets reworded, the file still carries
//! the old key, and the interface quietly falls back to German for that one
//! string. These tests turn that into a build failure instead.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Same escaping rules the loader uses; see `i18n::split_pair`.
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

fn entries(path: &Path) -> (Option<String>, HashMap<String, String>) {
    let text = std::fs::read_to_string(path).expect("language file must be readable");
    let mut name = None;
    let mut map = HashMap::new();
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
        map.insert(k.to_string(), v.to_string());
    }
    (name, map)
}

fn lang_files() -> Vec<PathBuf> {
    let dir = repo_root().join("lang");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("lang/ must exist")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("lang"))
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no language files found in {}", dir.display());
    out
}

/// Placeholders are the contract between code and translation: dropping one
/// leaves a value out of the sentence, inventing one prints nothing.
fn placeholders(s: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] != '{' {
            i += 1;
            continue;
        }
        if b.get(i + 1) == Some(&'{') {
            i += 2;
            continue;
        }
        let mut j = i + 1;
        let mut digits = String::new();
        while j < b.len() && b[j].is_ascii_digit() {
            digits.push(b[j]);
            j += 1;
        }
        if !digits.is_empty() && b.get(j) == Some(&'}') {
            out.insert(digits);
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

#[test]
fn every_file_declares_a_name() {
    for path in lang_files() {
        let (name, _) = entries(&path);
        let name = name.unwrap_or_else(|| panic!("{} has no @name", path.display()));
        assert!(
            !name.is_empty() && name != "LANGUAGE NAME HERE" || path.ends_with("_template.lang"),
            "{} still carries the placeholder name",
            path.display()
        );
    }
}

#[test]
fn translations_keep_every_placeholder() {
    for path in lang_files() {
        let (_, map) = entries(&path);
        for (key, value) in &map {
            if value.is_empty() {
                continue;
            }
            assert_eq!(
                placeholders(key),
                placeholders(value),
                "{}: placeholders differ for {key:?}",
                path.display()
            );
        }
    }
}

/// The template is regenerated from the source, so it is the list of strings
/// the program actually asks for. Any shipped language missing one of them
/// would show German in that spot.
#[test]
fn shipped_languages_are_complete() {
    let (_, template) = entries(&repo_root().join("lang/_template.lang"));
    assert!(
        template.len() > 100,
        "template looks truncated ({} entries) — rerun tools/extract_lang.py",
        template.len()
    );
    for path in lang_files() {
        if path.file_name().and_then(|s| s.to_str()) == Some("_template.lang") {
            continue;
        }
        let (_, map) = entries(&path);
        let missing: Vec<&String> = template
            .keys()
            .filter(|k| map.get(*k).is_none_or(|v| v.is_empty()))
            .collect();
        assert!(
            missing.is_empty(),
            "{} is missing {} string(s), first: {:?}",
            path.display(),
            missing.len(),
            missing.first()
        );

        let stale: Vec<&String> = map.keys().filter(|k| !template.contains_key(*k)).collect();
        assert!(
            stale.is_empty(),
            "{} translates {} string(s) the code no longer uses, first: {:?}",
            path.display(),
            stale.len(),
            stale.first()
        );
    }
}

/// End to end through the real loader.
///
/// One test rather than several: the selected language is process-wide state,
/// and Rust runs tests in parallel, so two of these would race each other.
#[test]
fn the_loader_switches_language() {
    // Tests run with the manifest directory as the working directory, which is
    // one of the places the loader looks for `lang/`.
    assert_eq!(diskalize::i18n::t("Einstellungen"), "Einstellungen");

    assert!(diskalize::i18n::set("en"), "en.lang must load");
    assert_eq!(diskalize::i18n::current(), "en");
    assert_eq!(diskalize::i18n::t("Einstellungen"), "Settings");
    assert_eq!(
        diskalize::i18n::tf("{0} Dateien · {1} Ordner", &["3", "4"]),
        "3 files · 4 folders"
    );

    // The pieces the first translation pass missed, pinned here so a
    // regression shows up in a test rather than by eye in the running app.
    for (german, english) in [
        ("MFT-Direktzugriff", "MFT direct read"),
        ("Verzeichnis-Scan", "Directory walk"),
        ("Bilder", "Images"),
        ("Dokumente", "Documents"),
        ("Archive", "Archives"),
        ("Programme", "Programs"),
    ] {
        assert_eq!(diskalize::i18n::t(german), english, "{german}");
    }
    assert_eq!(
        diskalize::i18n::tf("{0}   ·   {1} des Ausschnitts", &["1 GB", "12 %"]),
        "1 GB   ·   12 % of the view"
    );

    // The whole cheat sheet, not just its first line.
    let help = diskalize::search::syntax_help();
    assert!(help.starts_with("Search"), "help not translated: {help:.40}");
    assert!(help.contains("regular expression"), "help truncated");
    assert!(help.contains(r"re:^\d{4}_.*"), "regex example mangled");

    // Back to the source language, and the file is no longer consulted.
    assert!(diskalize::i18n::set("de"));
    assert_eq!(diskalize::i18n::t("Einstellungen"), "Einstellungen");
    assert_eq!(diskalize::search::syntax_help().lines().next(), Some("Suche"));

    // A language with no file leaves the current one alone.
    assert!(!diskalize::i18n::set("xx"));
    assert_eq!(diskalize::i18n::current(), "de");
}
