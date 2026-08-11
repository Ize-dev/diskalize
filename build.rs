fn main() {
    println!("cargo:rerun-if-changed=assets/diskalize.ico");
    println!("cargo:rerun-if-changed=lang");
    stamp();
    copy_languages();

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/diskalize.ico");
        res.set("ProductName", "Diskalize");
        res.set("FileDescription", "Diskalize — Festplatten-Analyse");
        res.set("LegalCopyright", "");
        // A missing resource compiler must not break the build; the app then
        // falls back to loading assets/diskalize.ico at runtime.
        if let Err(e) = res.compile() {
            println!("cargo:warning=Icon-Ressource nicht eingebettet: {e}");
        }

        // Drop a copy of the .ico next to the binaries. The Explorer context
        // menu points its `Icon` value at that file rather than at an icon
        // resource inside the exe — a plain path is the one form every shell
        // surface resolves.
        if let Ok(out) = std::env::var("OUT_DIR") {
            let target = std::path::Path::new(&out)
                .ancestors()
                .nth(3)
                .map(|p| p.join("diskalize.ico"));
            if let Some(dst) = target {
                let _ = std::fs::copy("assets/diskalize.ico", dst);
            }
        }
    }
}

/// Bakes build date and git revision into the binary for the About page.
///
/// Both are best-effort: a source drop without a `.git` directory still builds,
/// the revision just reads "unbekannt".
fn stamp() {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=DKZ_BUILD_UNIX={secs}");

    let rev = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unbekannt".into());
    println!("cargo:rustc-env=DKZ_GIT_REV={rev}");
    // Without this the stamp would freeze at whatever the first build produced.
    println!("cargo:rerun-if-changed=.git/HEAD");
}

/// Puts the language files next to the binaries.
///
/// They are read at run time rather than compiled in, so a user can add or fix
/// a translation without rebuilding — which is the whole point of keeping them
/// external.
fn copy_languages() {
    let Ok(out) = std::env::var("OUT_DIR") else {
        return;
    };
    let Some(target) = std::path::Path::new(&out).ancestors().nth(3) else {
        return;
    };
    let dst_dir = target.join("lang");
    if std::fs::create_dir_all(&dst_dir).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir("lang") else {
        return;
    };
    for e in entries.flatten() {
        let src = e.path();
        if src.extension().and_then(|s| s.to_str()) != Some("lang") {
            continue;
        }
        if let Some(name) = src.file_name() {
            let _ = std::fs::copy(&src, dst_dir.join(name));
        }
    }
}
