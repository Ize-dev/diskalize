//! Content search over a real directory tree.
//!
//! The bug this pins down: browsing a folder lists only its direct children, and
//! the content search used that list as its candidates. A match four levels down
//! — the normal case — was never even opened.

use std::path::PathBuf;
use std::sync::Arc;

use diskalize::content;
use diskalize::index::Index;
use diskalize::scan::{walk, Progress};

/// Each test gets its own directory: they run in parallel, and a shared one
/// meant the first test was reading files the second had already deleted.
fn build_tree(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dkz_content_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    let deep = dir.join("a").join("b").join("c");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(dir.join("top.txt"), "nothing here\n").unwrap();
    std::fs::write(deep.join("buried.csv"), "id;text\n1;needle in here\n").unwrap();
    std::fs::write(deep.join("other.txt"), "unrelated\n").unwrap();
    // A binary file with the term in it must stay out of the results.
    std::fs::write(deep.join("blob.bin"), b"\x00\x00needle\x00").unwrap();
    dir
}

fn index_of(dir: &PathBuf) -> Index {
    walk::scan_path(&dir.to_string_lossy(), &Progress::default()).expect("walk must succeed")
}

/// Files anywhere below `root`, which is what "search in this folder" means.
fn files_below(ix: &Index, root: u32) -> Vec<String> {
    diskalize::search::subtree(ix, root)
        .into_iter()
        .filter(|&i| !ix.is_dir(i))
        .map(|i| ix.path_of(i))
        .collect()
}

#[test]
fn a_match_several_levels_down_is_found() {
    let dir = build_tree("deep");
    let ix = index_of(&dir);

    let candidates = files_below(&ix, ix.root);
    assert!(
        candidates.len() >= 4,
        "the subtree walk missed files: {candidates:?}"
    );
    assert!(
        candidates.iter().any(|p| p.ends_with("buried.csv")),
        "the deep file is not among the candidates: {candidates:?}"
    );

    let hits = content::search(&candidates, "needle", 100, &Arc::new(content::Progress::default()));
    let names: Vec<&str> = hits
        .iter()
        .map(|h| candidates[h.which].as_str())
        .collect();
    assert_eq!(hits.len(), 1, "expected only the csv, got {names:?}");
    assert!(names[0].ends_with("buried.csv"));
    assert_eq!(hits[0].line, "1;needle in here");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Only the direct children — the set the search used to run over — contain no
/// match at all. This is the shape of the report, kept as its own assertion so
/// a regression cannot hide behind the test above.
#[test]
fn the_direct_children_alone_would_find_nothing() {
    let dir = build_tree("shallow");
    let ix = index_of(&dir);

    let mut children = Vec::new();
    let mut c = ix.first_child[ix.root as usize];
    while c != diskalize::index::NONE {
        if !ix.is_dir(c) {
            children.push(ix.path_of(c));
        }
        c = ix.next_sib[c as usize];
    }

    let hits = content::search(&children, "needle", 100, &Arc::new(content::Progress::default()));
    assert!(
        hits.is_empty(),
        "the fixture no longer reproduces the reported case"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The case from the report, against the real folder.
/// `cargo test --test content_search -- --ignored --nocapture`
#[test]
#[ignore = "needs that game installed"]
fn the_reported_folder() {
    let root = std::env::var("DKZ_TEST_DIR")
        .unwrap_or_else(|_| r"D:\Games\Kingpin Reloaded".to_string());
    let needle = std::env::var("DKZ_TEST_TEXT").unwrap_or_else(|_| "fuck".to_string());
    let ix = walk::scan_path(&root, &Progress::default()).expect("walk must succeed");
    let candidates = files_below(&ix, ix.root);
    println!("{} files under {root}", candidates.len());

    let progress = Arc::new(content::Progress::default());
    let t0 = std::time::Instant::now();
    let hits = content::search(&candidates, &needle, 1000, &progress);
    println!(
        "{} files contain {needle:?}, in {} ms",
        hits.len(),
        t0.elapsed().as_millis()
    );
    for h in hits.iter().take(5) {
        println!("   {}\n      {}", candidates[h.which], h.line);
    }
    assert!(!hits.is_empty(), "nothing found");
}

/// How long the UI thread used to sit building paths for a large result set.
/// `cargo test --test content_search -- --ignored path_cost --nocapture`
#[test]
#[ignore = "needs a large indexed folder"]
fn path_cost_for_many_hits() {
    let root = std::env::var("DKZ_TEST_DIR").unwrap_or_else(|_| r"C:\Windows\System32".to_string());
    let ix = walk::scan_path(&root, &Progress::default()).expect("walk must succeed");
    let nodes: Vec<u32> = diskalize::search::subtree(&ix, ix.root)
        .into_iter()
        .filter(|&i| !ix.is_dir(i))
        .collect();
    println!("{} files", nodes.len());

    let t0 = std::time::Instant::now();
    let paths: Vec<String> = nodes.iter().map(|&i| ix.path_of(i)).collect();
    let ms = t0.elapsed().as_millis();
    println!(
        "building {} paths took {ms} ms ({} MB of strings)",
        paths.len(),
        paths.iter().map(|p| p.len()).sum::<usize>() / (1024 * 1024)
    );
}

/// What a common term over a large candidate set actually costs.
/// `cargo test --test content_search -- --ignored stress --nocapture`
#[test]
#[ignore = "reads thousands of files"]
fn stress_a_common_term() {
    let root = std::env::var("DKZ_TEST_DIR").unwrap_or_else(|_| r"C:\Windows\System32".to_string());
    let needle = std::env::var("DKZ_TEST_TEXT").unwrap_or_else(|_| "e".to_string());
    let ix = walk::scan_path(&root, &Progress::default()).expect("walk must succeed");
    let candidates = files_below(&ix, ix.root);
    println!("{} candidate files", candidates.len());

    let before = diskalize::win::process_memory(0).unwrap_or_default();
    let progress = Arc::new(content::Progress::default());
    let t0 = std::time::Instant::now();
    let hits = content::search(&candidates, &needle, 200_000, &progress);
    let after = diskalize::win::process_memory(0).unwrap_or_default();
    println!(
        "{} hits in {} ms, private {} -> {} MB",
        hits.len(),
        t0.elapsed().as_millis(),
        before.private / (1024 * 1024),
        after.private / (1024 * 1024),
    );
}

/// `apply_find` used to re-sort the whole content-hit set on every frame.
/// This measures one such sort; at 60 fps the per-frame cost is that times 60.
/// `cargo test --test content_search --release -- --ignored sort_cost --nocapture`
#[test]
#[ignore = "timing measurement"]
fn sort_cost_for_many_hits() {
    use diskalize::store::{Hit, SortKey};
    let root = std::env::var("DKZ_TEST_DIR").unwrap_or_else(|_| r"C:\Windows\System32".to_string());
    let ix = walk::scan_path(&root, &Progress::default()).expect("walk must succeed");
    let hits: Vec<Hit> = diskalize::search::subtree(&ix, ix.root)
        .into_iter()
        .filter(|&i| !ix.is_dir(i))
        .map(|idx| Hit { vol: 0, idx })
        .collect();
    let vols = vec![(0u16, std::sync::Arc::new(parking_lot::RwLock::new(ix)))];

    for (name, key) in [("size", SortKey::Size), ("name", SortKey::Name), ("path", SortKey::Path)] {
        let mut h = hits.clone();
        let t0 = std::time::Instant::now();
        diskalize::store::sort_hits(&vols, &mut h, key, true, true);
        println!(
            "{name}: {} hits sorted in {} µs",
            h.len(),
            t0.elapsed().as_micros()
        );
    }
}

/// The reported case: the name query matches a *folder*, and the text sits in a
/// file several levels below it. Searching the matched entries themselves finds
/// nothing — a folder cannot be read — so folder hits have to be expanded.
#[test]
fn a_folder_name_match_searches_the_files_under_it() {
    let dir = build_tree("byfolder");
    let ix = index_of(&dir);

    // What a name query for the folder would return.
    let folder = diskalize::search::subtree(&ix, ix.root)
        .into_iter()
        .find(|&i| ix.is_dir(i) && ix.name(i) == "c")
        .expect("fixture folder missing");

    let as_named: Vec<String> = vec![ix.path_of(folder)];
    let direct = content::search(
        &as_named,
        "needle",
        100,
        &Arc::new(content::Progress::default()),
    );
    assert!(
        direct.is_empty(),
        "reading the folder itself must not produce a hit"
    );

    let expanded: Vec<String> = diskalize::search::subtree(&ix, folder)
        .into_iter()
        .filter(|&i| !ix.is_dir(i))
        .map(|i| ix.path_of(i))
        .collect();
    let hits = content::search(
        &expanded,
        "needle",
        100,
        &Arc::new(content::Progress::default()),
    );
    assert_eq!(hits.len(), 1, "expanding the folder must find the file");
    assert!(expanded[hits[0].which].ends_with("buried.csv"));

    let _ = std::fs::remove_dir_all(&dir);
}
