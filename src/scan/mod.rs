pub mod ntfs;
pub mod usn;
pub mod walk;
pub mod watch;

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::index::Index;
use crate::scan::ntfs::MftMap;
use crate::win::{self, DriveInfo};

pub struct Progress {
    pub phase: Mutex<String>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub cancel: Arc<AtomicBool>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: Mutex::new(String::new()),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Progress {
    pub fn set_phase(&self, s: &str) {
        *self.phase.lock() = s.to_string();
    }
    pub fn fraction(&self) -> Option<f32> {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 {
            return None;
        }
        Some((self.done.load(Ordering::Relaxed) as f64 / t as f64).clamp(0.0, 1.0) as f32)
    }
}

#[derive(Clone, Debug)]
pub enum Target {
    Drive(DriveInfo),
    Path(String),
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Drive(d) => format!("{}:\\", d.letter),
            Target::Path(p) => p.clone(),
        }
    }

    /// Identity used to look a volume up in the store.
    pub fn key(&self) -> String {
        match self {
            Target::Drive(d) => format!("{}:", d.letter.to_ascii_uppercase()),
            Target::Path(p) => {
                let t = p.trim_end_matches('\\');
                if t.is_empty() {
                    p.clone()
                } else {
                    t.to_string()
                }
            }
        }
    }

    /// Short label for the drive strip.
    pub fn title(&self) -> String {
        match self {
            Target::Drive(d) if d.label.is_empty() => format!("{}:", d.letter),
            Target::Drive(d) => format!("{}: {}", d.letter, d.label),
            Target::Path(p) => {
                let t = p.trim_end_matches('\\');
                t.rsplit('\\').next().filter(|s| !s.is_empty()).unwrap_or(t).to_string()
            }
        }
    }
}

pub struct ScanResult {
    pub index: Index,
    /// Present only when the MFT path was used — enables the live USN watcher.
    pub mft: Option<(Arc<MftMap>, char)>,
    pub fallback_reason: Option<String>,
}

pub fn run(target: Target, progress: &Progress) -> io::Result<ScanResult> {
    let t0 = Instant::now();
    let mut fallback_reason = None;

    let result = match &target {
        Target::Drive(d) if d.is_ntfs() && d.kind != win::DRIVE_REMOTE => {
            if !win::is_elevated() {
                fallback_reason =
                    Some("MFT-Zugriff braucht Administratorrechte — nutze Verzeichnis-Scan".into());
                None
            } else {
                progress.set_phase("MFT wird gelesen…");
                match ntfs::scan(d, progress) {
                    Ok(s) => Some(s),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
                    Err(e) => {
                        fallback_reason = Some(format!("MFT-Scan fehlgeschlagen ({e}) — Fallback"));
                        None
                    }
                }
            }
        }
        Target::Drive(d) => {
            fallback_reason = Some(format!(
                "{} ({}) unterstützt keinen MFT-Scan",
                d.fs,
                d.kind_name()
            ));
            None
        }
        Target::Path(_) => None,
    };

    let mut out = match result {
        Some(s) => {
            let letter = match &target {
                Target::Drive(d) => d.letter,
                Target::Path(p) => p.chars().next().unwrap_or('C'),
            };
            ScanResult {
                index: s.index,
                mft: Some((s.map, letter)),
                fallback_reason: None,
            }
        }
        None => {
            progress.set_phase("Verzeichnisse werden durchlaufen…");
            let path = match &target {
                Target::Drive(d) => format!("{}:\\", d.letter),
                Target::Path(p) => p.clone(),
            };
            ScanResult {
                index: walk::scan_path(&path, progress)?,
                mft: None,
                fallback_reason,
            }
        }
    };

    out.index.vol.scan_ms = t0.elapsed().as_millis();
    progress.set_phase("fertig");
    Ok(out)
}
