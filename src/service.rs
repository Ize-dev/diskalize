//! The indexing service.
//!
//! Runs as LocalSystem, so `\\.\C:` opens without a UAC prompt — the user
//! approves once at installation and never again. It scans every fixed volume,
//! keeps them current through the USN journal, publishes each index as a shared
//! memory snapshot and streams subsequent changes to connected GUIs.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, NO_ERROR,
};
use windows_sys::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, PIPE_ACCESS_DUPLEX, WRITE_DAC,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};

use crate::index::Index;
use crate::ipc::{self, Change, VolumeMsg};
use crate::scan::{self, usn::LiveWatcher, Progress, Target};
use crate::win::{self, wide};

pub const SERVICE_NAME: &str = "DiskalizeIndex";
pub const DISPLAY_NAME: &str = "Diskalize Index";

// ---- shared state -----------------------------------------------------------

struct Volume {
    key: String,
    title: String,
    index: Arc<RwLock<Index>>,
    section_name: String,
    epoch: u64,
    usn: bool,
    scanning: bool,
    _watcher: Option<LiveWatcher>,
}

#[derive(Default)]
struct Shared {
    vols: Mutex<Vec<Volume>>,
    clients: Mutex<Vec<Sender<Vec<u8>>>>,
    stop: AtomicBool,
    generation: AtomicU64,
}

impl Shared {
    fn volume_list(&self) -> Vec<VolumeMsg> {
        self.vols
            .lock()
            .iter()
            .map(|v| VolumeMsg {
                key: v.key.clone(),
                title: v.title.clone(),
                section: v.section_name.clone(),
                generation: v.index.read().generation,
                usn: v.usn,
                scanning: v.scanning,
            })
            .collect()
    }

    fn broadcast(&self, frame: Vec<u8>) {
        let mut clients = self.clients.lock();
        clients.retain(|tx| tx.send(frame.clone()).is_ok());
    }
}

// ---- indexing ---------------------------------------------------------------

/// Publishes a volume's index into a fresh section and tells clients about it.
///
/// A new epoch each time: growing an existing mapping is not possible, and
/// clients that are mid-read of the old one keep a valid view until they let go.
fn publish(shared: &Arc<Shared>, key: &str) {
    let mut vols = shared.vols.lock();
    let Some(v) = vols.iter_mut().find(|v| v.key == key) else {
        return;
    };
    v.epoch += 1;
    let name = format!("Diskalize_{}_{}", sanitise(&v.key), v.epoch);
    // Moves the columns into shared memory and keeps the index there. The owned
    // buffers go away with the old value, so the service holds one copy.
    let moved = {
        let cur = v.index.read();
        crate::snapshot::create(&name, &cur)
    };
    match moved {
        Some(ix) => {
            v.section_name = ix
                .section
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_default();
            *v.index.write() = ix;
            v.scanning = false;
        }
        None => return,
    }
    drop(vols);
    shared.broadcast(ipc::write_volumes(&shared.volume_list()));
}

fn sanitise(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn index_target(shared: &Arc<Shared>, target: Target) {
    // A mapped network drive root is never worth indexing: as LocalSystem the
    // mapping does not even exist, and raw access is impossible either way.
    // Shares reached by UNC path are a different matter and stay allowed.
    if let Target::Drive(d) = &target {
        if d.kind == win::DRIVE_REMOTE {
            eprintln!("[{}:] Netzlaufwerk — übersprungen", d.letter);
            return;
        }
    }
    let key = target.key();
    let title = target.title();
    {
        let mut vols = shared.vols.lock();
        if let Some(v) = vols.iter_mut().find(|v| v.key == key) {
            v.scanning = true;
        } else {
            vols.push(Volume {
                key: key.clone(),
                title: title.clone(),
                index: Arc::new(RwLock::new(Index::default())),
                section_name: String::new(),
                epoch: 0,
                usn: false,
                scanning: true,
                _watcher: None,
            });
        }
    }
    shared.broadcast(ipc::write_volumes(&shared.volume_list()));

    let progress = Arc::new(Progress::default());
    // Watchdog: a share that stops answering must not park the indexing thread
    // forever. The walker checks the cancel flag between directories, so it
    // unwinds as soon as the current call returns.
    {
        let p = Arc::clone(&progress);
        let key = key.clone();
        std::thread::spawn(move || {
            let mut last = 0u64;
            let mut stalled = 0u32;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                if p.cancel.load(Ordering::Relaxed) {
                    return;
                }
                let now = p.done.load(Ordering::Relaxed);
                if now == last {
                    stalled += 1;
                    // A minute without a single new entry means it is wedged.
                    if stalled >= 12 {
                        eprintln!("[{key}] reagiert nicht mehr — abgebrochen");
                        p.cancel.store(true, Ordering::Relaxed);
                        return;
                    }
                } else {
                    stalled = 0;
                    last = now;
                }
            }
        });
    }
    let scanned = scan::run(target, &progress);
    progress.cancel.store(true, Ordering::Relaxed); // retire the watchdog
    if let Ok(r) = &scanned {
        if let Some(reason) = &r.fallback_reason {
            eprintln!("[{key}] {reason}");
        }
    }
    let Ok(result) = scanned else {
        let mut vols = shared.vols.lock();
        if let Some(v) = vols.iter_mut().find(|v| v.key == key) {
            v.scanning = false;
        }
        return;
    };

    let index = Arc::new(RwLock::new(result.index));
    let watcher = match result.mft {
        Some((map, letter)) => {
            let sh = Arc::clone(shared);
            let k = key.clone();
            let idx = Arc::clone(&index);
            let sink: crate::scan::usn::ChangeSink = Arc::new(move |_batch: &[Change]| {
                // Readers share these pages, so only the header needs updating;
                // the payload is already in front of them.
                let exhausted = {
                    let ix = idx.read();
                    crate::snapshot::commit(&ix);
                    ix.exhausted
                };
                if exhausted {
                    // Out of reserved room — a bigger section, new epoch.
                    publish(&sh, &k);
                } else {
                    sh.broadcast(ipc::write_delta(&k, &[]));
                }
                sh.generation.fetch_add(1, Ordering::Relaxed);
            });
            Some(scan::usn::spawn(
                letter,
                map,
                Arc::clone(&index),
                Some(sink),
            ))
        }
        None => {
            // No journal: the directory watcher edits the index in place, so
            // clients get a fresh snapshot rather than deltas.
            let root = index.read().vol.root_path.clone();
            (!root.is_empty()).then(|| scan::watch::spawn(root, Arc::clone(&index)))
        }
    };
    let usn = result_is_usn(&watcher, &index);

    {
        let mut vols = shared.vols.lock();
        if let Some(v) = vols.iter_mut().find(|v| v.key == key) {
            v.index = index;
            v._watcher = watcher;
            v.usn = usn;
        }
    }
    publish(shared, &key);
}

fn result_is_usn(watcher: &Option<LiveWatcher>, index: &Arc<RwLock<Index>>) -> bool {
    watcher.is_some() && index.read().vol.method_mft
}

fn worker(shared: Arc<Shared>, jobs: Receiver<Target>) {
    // Fixed drives first, so a fresh install has the common case ready quickly.
    // Never probe network drives here: an unreachable share would block the
    // whole indexing thread inside GetVolumeInformationW.
    for d in win::list_drives_detailed(false) {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        if d.kind == win::DRIVE_FIXED {
            index_target(&shared, Target::Drive(d));
        }
    }
    while let Ok(t) = jobs.recv() {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        index_target(&shared, t);
    }
}

/// Re-publishes volumes whose watcher edited the index without emitting deltas.
fn republisher(shared: Arc<Shared>) {
    let mut seen: Vec<(String, u64)> = Vec::new();
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let stale: Vec<String> = {
            let vols = shared.vols.lock();
            vols.iter()
                .filter(|v| !v.usn && !v.scanning && !v.section_name.is_empty())
                .filter(|v| {
                    let g = v.index.read().generation;
                    match seen.iter_mut().find(|(k, _)| *k == v.key) {
                        Some(e) if e.1 == g => false,
                        Some(e) => {
                            e.1 = g;
                            true
                        }
                        None => {
                            seen.push((v.key.clone(), g));
                            false
                        }
                    }
                })
                .map(|v| v.key.clone())
                .collect()
        };
        for k in stale {
            publish(&shared, &k);
        }
    }
}

// ---- pipe server ------------------------------------------------------------

fn create_pipe() -> HANDLE {
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    // The service is LocalSystem and clients are ordinary users, so the pipe
    // needs a DACL that lets authenticated users read and write.
    let mut sd: *mut c_void = std::ptr::null_mut();
    let sddl = wide("D:(A;;GRGW;;;AU)(A;;GA;;;SY)(A;;GA;;;BA)");
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut sd,
            std::ptr::null_mut(),
        )
    };
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let sa_ptr = if ok != 0 {
        &mut sa as *mut SECURITY_ATTRIBUTES
    } else {
        std::ptr::null_mut()
    };

    unsafe {
        CreateNamedPipeW(
            wide(ipc::PIPE).as_ptr(),
            PIPE_ACCESS_DUPLEX | WRITE_DAC,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            32,
            1 << 20,
            1 << 16,
            0,
            sa_ptr,
        )
    }
}

/// Handles one connection.
///
/// A synchronous pipe handle serialises operations, so a thread parked in
/// `ReadFile` would block every `WriteFile` on the same handle — the push
/// stream would deadlock. Each connection therefore travels in one direction
/// only: `Hello` turns it into a push stream we never read from again, and
/// commands arrive on their own short-lived connections.
fn serve_client(shared: Arc<Shared>, pipe: HANDLE, jobs: Sender<Target>) {
    let mut buf = vec![0u8; 1 << 16];
    let mut read = 0u32;
    let ok = unsafe {
        ReadFile(
            pipe,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut read,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || read < 5 {
        unsafe {
            DisconnectNamedPipe(pipe);
            CloseHandle(pipe);
        }
        return;
    }

    let mut r = ipc::Reader::new(&buf[4..read as usize]);
    let tag = r.u8();
    if tag == ipc::REQ_HELLO {
        let (tx, rx) = channel::<Vec<u8>>();
        let _ = tx.send(ipc::write_volumes(&shared.volume_list()));
        shared.clients.lock().push(tx);
        while let Ok(frame) = rx.recv() {
            let mut written = 0u32;
            let ok = unsafe {
                WriteFile(
                    pipe,
                    frame.as_ptr(),
                    frame.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                break;
            }
        }
    } else {
        match tag {
            ipc::REQ_RESCAN => {
                if let Some(t) = target_for_key(&r.str()) {
                    let _ = jobs.send(t);
                }
            }
            ipc::REQ_ADD_PATH => {
                let p = r.str();
                if !p.is_empty() {
                    let _ = jobs.send(Target::Path(p));
                }
            }
            ipc::REQ_PUBLISH => {
                // An empty key means "all of them" — a client turning on the
                // search across every volume.
                let key = r.str();
                if key.is_empty() {
                    let all: Vec<String> =
                        shared.vols.lock().iter().map(|v| v.key.clone()).collect();
                    for k in all {
                        publish(&shared, &k);
                    }
                } else {
                    publish(&shared, &key);
                }
            }
            ipc::REQ_FORGET => {
                let key = r.str();
                shared.vols.lock().retain(|v| v.key != key);
                shared.broadcast(ipc::write_volumes(&shared.volume_list()));
            }
            _ => {}
        }
    }

    unsafe {
        DisconnectNamedPipe(pipe);
        CloseHandle(pipe);
    }
}

fn target_for_key(key: &str) -> Option<Target> {
    let t = key.trim_end_matches('\\');
    if t.len() == 2 && t.as_bytes()[1] == b':' {
        let letter = t.as_bytes()[0].to_ascii_uppercase() as char;
        if let Some(d) = win::list_drives_detailed(false)
            .into_iter()
            .find(|d| d.letter == letter)
        {
            return Some(Target::Drive(d));
        }
    }
    (!t.is_empty()).then(|| Target::Path(t.to_string()))
}

fn pipe_server(shared: Arc<Shared>, jobs: Sender<Target>) {
    while !shared.stop.load(Ordering::Relaxed) {
        let pipe = create_pipe();
        if pipe == INVALID_HANDLE_VALUE || pipe.is_null() {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        let connected = unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } != 0
            || win::last_error() == ERROR_PIPE_CONNECTED;
        if !connected {
            unsafe { CloseHandle(pipe) };
            continue;
        }
        let sh = Arc::clone(&shared);
        let jb = jobs.clone();
        let h = pipe as usize;
        std::thread::spawn(move || serve_client(sh, h as HANDLE, jb));
    }
}

// ---- service control --------------------------------------------------------

static STATUS_HANDLE: AtomicU64 = AtomicU64::new(0);
static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

fn set_status(state: u32, checkpoint: u32) {
    use windows_sys::Win32::System::Services::{
        SetServiceStatus, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_STATUS,
        SERVICE_WIN32_OWN_PROCESS,
    };
    let h = STATUS_HANDLE.load(Ordering::Relaxed);
    if h == 0 {
        return;
    }
    const RUNNING: u32 = 4;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: NO_ERROR,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: 30_000,
    };
    unsafe { SetServiceStatus(h as _, &status) };
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _ctx: *mut c_void,
) -> u32 {
    const SERVICE_CONTROL_STOP: u32 = 1;
    const SERVICE_CONTROL_SHUTDOWN: u32 = 5;
    const STOP_PENDING: u32 = 3;
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        set_status(STOP_PENDING, 1);
        if let Some(s) = SHARED.lock().as_ref() {
            s.stop.store(true, Ordering::Relaxed);
        }
    }
    NO_ERROR
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    use windows_sys::Win32::System::Services::RegisterServiceCtrlHandlerExW;
    const START_PENDING: u32 = 2;
    const RUNNING: u32 = 4;
    const STOPPED: u32 = 1;

    let h = unsafe {
        RegisterServiceCtrlHandlerExW(
            wide(SERVICE_NAME).as_ptr(),
            Some(control_handler),
            std::ptr::null_mut(),
        )
    };
    if h.is_null() {
        return;
    }
    STATUS_HANDLE.store(h as u64, Ordering::Relaxed);
    set_status(START_PENDING, 1);

    let shared = run_core();
    set_status(RUNNING, 0);

    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    set_status(STOPPED, 0);
}

/// Starts everything and returns the shared state; used by both the service and
/// the console mode that makes debugging bearable.
fn run_core() -> Arc<Shared> {
    let shared = Arc::new(Shared::default());
    *SHARED.lock() = Some(Arc::clone(&shared));

    let (tx, rx) = channel::<Target>();
    {
        let sh = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("index".into())
            .spawn(move || worker(sh, rx))
            .ok();
    }
    {
        let sh = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("republish".into())
            .spawn(move || republisher(sh))
            .ok();
    }
    {
        let sh = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("pipe".into())
            .spawn(move || pipe_server(sh, tx))
            .ok();
    }
    shared
}

// ---- installation -----------------------------------------------------------

pub fn install() -> Result<(), String> {
    use windows_sys::Win32::System::Services::{
        ChangeServiceConfig2W, CloseServiceHandle, CreateServiceW, OpenSCManagerW,
        SC_MANAGER_CREATE_SERVICE, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
        SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_WIN32_OWN_PROCESS,
    };

    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .into_owned();
    let cmd = format!("\"{exe}\"");

    unsafe {
        let scm = OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CREATE_SERVICE,
        );
        if scm.is_null() {
            return Err("Dienststeuerung nicht erreichbar (Adminrechte nötig)".into());
        }
        let svc = CreateServiceW(
            scm,
            wide(SERVICE_NAME).as_ptr(),
            wide(DISPLAY_NAME).as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            wide(&cmd).as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(), // LocalSystem
            std::ptr::null(),
        );
        if svc.is_null() {
            let code = win::last_error();
            CloseServiceHandle(scm);
            // ERROR_SERVICE_EXISTS
            if code == 1073 {
                return start();
            }
            return Err(format!("Dienst konnte nicht angelegt werden (Code {code})"));
        }
        // Bound to a named local: inlining `wide(..)` would free the buffer
        // before ChangeServiceConfig2W ever reads it.
        let mut text = wide(
            "Indiziert die Datenträger für Diskalize und hält den Index über das \
             NTFS-USN-Journal aktuell.",
        );
        let mut desc = SERVICE_DESCRIPTIONW {
            lpDescription: text.as_mut_ptr(),
        };
        ChangeServiceConfig2W(
            svc,
            SERVICE_CONFIG_DESCRIPTION,
            &mut desc as *mut _ as *mut c_void,
        );
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
    }
    start()
}

const SERVICE_ALL_ACCESS: u32 = 0xF01FF;
const SC_MANAGER_CONNECT: u32 = 0x0001;

pub fn start() -> Result<(), String> {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, StartServiceW,
    };
    unsafe {
        // SCM handles take SC_MANAGER_* rights; SERVICE_ALL_ACCESS is for the
        // service handle and is rejected here.
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err("Dienststeuerung nicht erreichbar".into());
        }
        let svc = OpenServiceW(scm, wide(SERVICE_NAME).as_ptr(), SERVICE_ALL_ACCESS);
        if svc.is_null() {
            CloseServiceHandle(scm);
            return Err("Dienst nicht gefunden".into());
        }
        StartServiceW(svc, 0, std::ptr::null());
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
    }
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, ControlService, DeleteService, OpenSCManagerW, OpenServiceW,
        SERVICE_STATUS,
    };
    unsafe {
        // SCM handles take SC_MANAGER_* rights; SERVICE_ALL_ACCESS is for the
        // service handle and is rejected here.
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return Err("Dienststeuerung nicht erreichbar (Adminrechte nötig)".into());
        }
        let svc = OpenServiceW(scm, wide(SERVICE_NAME).as_ptr(), SERVICE_ALL_ACCESS);
        if svc.is_null() {
            CloseServiceHandle(scm);
            return Ok(()); // already gone
        }
        let mut st: SERVICE_STATUS = std::mem::zeroed();
        ControlService(svc, 1 /* STOP */, &mut st);
        let ok = DeleteService(svc) != 0;
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
        if ok {
            Ok(())
        } else {
            Err("Dienst konnte nicht entfernt werden".into())
        }
    }
}

/// True when the service is registered and currently running.
pub fn is_running() -> bool {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SERVICE_QUERY_STATUS,
        SERVICE_STATUS,
    };
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return false;
        }
        let svc = OpenServiceW(scm, wide(SERVICE_NAME).as_ptr(), SERVICE_QUERY_STATUS);
        if svc.is_null() {
            CloseServiceHandle(scm);
            return false;
        }
        let mut st: SERVICE_STATUS = std::mem::zeroed();
        let ok = QueryServiceStatus(svc, &mut st) != 0 && st.dwCurrentState == 4;
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
        ok
    }
}

pub fn is_installed() -> bool {
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, SERVICE_QUERY_STATUS,
    };
    unsafe {
        let scm = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if scm.is_null() {
            return false;
        }
        let svc = OpenServiceW(scm, wide(SERVICE_NAME).as_ptr(), SERVICE_QUERY_STATUS);
        let found = !svc.is_null();
        if found {
            CloseServiceHandle(svc);
        }
        CloseServiceHandle(scm);
        found
    }
}

/// Binary entry point: handles the maintenance verbs, otherwise hands over to
/// the service control manager.
pub fn entry() {
    use windows_sys::Win32::System::Services::{
        StartServiceCtrlDispatcherW, SERVICE_TABLE_ENTRYW,
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("--install") => {
            match install() {
                Ok(()) => println!("Dienst installiert und gestartet."),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--uninstall") => {
            match uninstall() {
                Ok(()) => println!("Dienst entfernt."),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        Some("--check") => {
            // Exercises the client path exactly as the GUI does, so a broken
            // snapshot or protocol shows up here rather than as an empty window.
            let client = crate::client::spawn(egui::Context::default());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
            let mut seen = 0;
            let mut vols: Vec<(
                String,
                std::sync::Arc<parking_lot::RwLock<crate::index::Index>>,
                bool,
            )> = Vec::new();
            while std::time::Instant::now() < deadline {
                match client.events.recv_timeout(std::time::Duration::from_millis(500)) {
                    Ok(crate::client::Event::Connected) => println!("verbunden"),
                    Ok(crate::client::Event::Volume { key, index, usn, .. }) => {
                        // Volumes arrive as empty placeholders; ask for the data.
                        client.send(crate::client::Cmd::Load(key.clone()));
                        vols.push((key, index, usn));
                    }
                    Ok(crate::client::Event::Changed(key)) => {
                        let Some((_, index, usn)) = vols.iter().find(|(k, ..)| *k == key) else {
                            continue;
                        };
                        let ix = index.read();
                        if !ix.is_ready() {
                            continue;
                        }
                        println!(
                            "  {key:<6} {} Dateien, {} Ordner, {} gesamt, {} in {}, usn={usn}",
                            crate::fmt::count(ix.total_files),
                            crate::fmt::count(ix.total_dirs),
                            crate::fmt::size(ix.size[ix.root as usize]),
                            if ix.vol.method_mft { "MFT" } else { "Walker" },
                            crate::fmt::duration(ix.vol.scan_ms),
                        );
                        seen += 1;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        if seen > 0 {
                            break;
                        }
                    }
                }
            }
            println!("{seen} Volume(s) über den Dienst geladen");
            println!(
                "libVLC: {}",
                if crate::media::available() {
                    "geladen — Medienvorschau verfügbar"
                } else {
                    "nicht gefunden — nur Standbilder"
                }
            );
            std::process::exit(if seen > 0 { 0 } else { 1 });
        }
        Some("--watch") => {
            // End-to-end delta check: does a file created now show up in the
            // client's copy of the index without a rescan?
            let needle = args.get(1).cloned().unwrap_or_else(|| ".mp4".into());
            let client = crate::client::spawn(egui::Context::default());
            let mut vols: Vec<(String, std::sync::Arc<parking_lot::RwLock<crate::index::Index>>)> =
                Vec::new();
            let mut deltas = 0u64;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            let mut last_report = std::time::Instant::now();
            println!("beobachte '{needle}' — jetzt eine Datei anlegen/löschen");
            while std::time::Instant::now() < deadline {
                while let Ok(ev) = client
                    .events
                    .recv_timeout(std::time::Duration::from_millis(200))
                {
                    match ev {
                        crate::client::Event::Volume { key, index, .. } => {
                            vols.retain(|(k, _)| *k != key);
                            vols.push((key, index));
                        }
                        crate::client::Event::Changed(_) => deltas += 1,
                        _ => {}
                    }
                }
                if last_report.elapsed() >= std::time::Duration::from_secs(2) {
                    last_report = std::time::Instant::now();
                    let q = crate::search::parse(&needle);
                    let counts: Vec<String> = vols
                        .iter()
                        .map(|(k, ix)| {
                            let g = ix.read();
                            let r = crate::search::run(&g, &q, 1_000_000, None);
                            format!("{k} {} (gen {})", r.total, g.generation)
                        })
                        .collect();
                    println!("  {deltas} Deltas · {}", counts.join(" · "));
                }
            }
            std::process::exit(0);
        }
        Some("--visguard") => {
            // Mimics the real thing: a genuine top-level window registered as
            // the main window, playback running, then minimise and hide it.
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, RegisterClassW, ShowWindow, SW_HIDE, SW_MINIMIZE,
                SW_RESTORE, SW_SHOW, WNDCLASSW, WS_OVERLAPPEDWINDOW,
            };
            let Some(path) = args.get(1) else {
                eprintln!("Nutzung: --visguard <datei>");
                std::process::exit(2);
            };
            let hwnd = unsafe {
                let cls = crate::win::wide("DiskalizeVisTest");
                let mut wc: WNDCLASSW = std::mem::zeroed();
                wc.lpfnWndProc = Some(DefWindowProcW);
                wc.lpszClassName = cls.as_ptr();
                RegisterClassW(&wc);
                let h = CreateWindowExW(
                    0,
                    cls.as_ptr(),
                    crate::win::wide("VisTest").as_ptr(),
                    WS_OVERLAPPEDWINDOW,
                    10,
                    10,
                    320,
                    200,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                ShowWindow(h, SW_SHOW);
                h
            };
            crate::win::set_main_window(hwnd as usize);
            crate::media::watch_window_visibility();

            let mut fail = false;
            for (label, cmd) in [("minimiert", SW_MINIMIZE), ("versteckt", SW_HIDE)] {
                unsafe { ShowWindow(hwnd, SW_RESTORE) };
                unsafe { ShowWindow(hwnd, SW_SHOW) };
                let _ = crate::media::take_silenced();
                let Some(mut p) = crate::media::Player_::open(path, false, 0) else {
                    eprintln!("konnte nicht geöffnet werden");
                    std::process::exit(1);
                };
                for _ in 0..60 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    let _ = p.take_frame();
                    if p.playing() {
                        break;
                    }
                }
                let before = p.playing();
                unsafe { ShowWindow(hwnd, cmd) };
                std::thread::sleep(std::time::Duration::from_millis(900));
                let after = p.playing();
                println!("Fenster {label}: lief vorher={before}, läuft danach={after}");
                if !before || after {
                    fail = true;
                }
                drop(p);
            }
            std::process::exit(if fail { 1 } else { 0 });
        }
        Some("--stopcheck") => {
            // The window procedure that hides to tray runs on another thread and
            // has no access to the player, so it calls the global stop. This
            // checks that path: does playback really end from over there?
            let Some(path) = args.get(1) else {
                eprintln!("Nutzung: --stopcheck <datei>");
                std::process::exit(2);
            };
            let Some(mut p) = crate::media::Player_::open(path, false, 0) else {
                eprintln!("konnte nicht geöffnet werden");
                std::process::exit(1);
            };
            for _ in 0..60 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let _ = p.take_frame();
                if p.playing() {
                    break;
                }
            }
            println!("läuft: {}", p.playing());
            std::thread::spawn(crate::media::stop_all).join().ok();
            std::thread::sleep(std::time::Duration::from_millis(500));
            let after = p.playing();
            let flagged = crate::media::take_silenced();
            println!("nach stop_all von fremdem Thread: läuft={after}, gemeldet={flagged}");
            std::process::exit(if !after && flagged { 0 } else { 1 });
        }
        Some("--resume") => {
            // The other half of the cancel path: reopening and seeking back.
            // libVLC ignores set_position until playback is actually running,
            // which is why the UI defers the seek instead of doing it inline.
            let Some(path) = args.get(1) else {
                eprintln!("Nutzung: --resume <datei>");
                std::process::exit(2);
            };
            let Some(mut p) = crate::media::Player_::open(path, false, 0) else {
                eprintln!("konnte nicht geöffnet werden");
                std::process::exit(1);
            };
            for _ in 0..60 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let _ = p.take_frame();
            }
            let saved = p.position();
            println!("Position vor dem Freigeben: {saved:.3}");
            drop(p);

            let Some(mut p2) = crate::media::Player_::open(path, false, 0) else {
                eprintln!("erneutes Öffnen fehlgeschlagen");
                std::process::exit(1);
            };
            let mut sought = false;
            let mut landed = -1.0f32;
            for _ in 0..80 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let _ = p2.take_frame();
                if !sought && p2.playing() {
                    p2.seek(saved);
                    sought = true;
                } else if sought && landed < 0.0 {
                    // Measured right after the seek lands; letting it run on
                    // would only show normal playback progress.
                    landed = p2.position();
                }
            }
            let drift = (landed - saved).abs();
            println!(
                "Position direkt nach dem Zurückspringen: {landed:.3} (Abweichung {drift:.3}), \
                 danach weitergelaufen bis {:.3}",
                p2.position()
            );
            std::process::exit(if sought && drift < 0.05 { 0 } else { 1 });
        }
        Some("--lockcheck") => {
            // Proves the release-before-delete mechanic: a playing file cannot
            // be deleted, and dropping the player must make it deletable again.
            let Some(path) = args.get(1) else {
                eprintln!("Nutzung: --lockcheck <datei>");
                std::process::exit(2);
            };
            let Some(mut p) = crate::media::Player_::open(path, false, 0) else {
                eprintln!("konnte nicht geöffnet werden");
                std::process::exit(1);
            };
            // Let playback actually take hold of the file.
            for _ in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let _ = p.take_frame();
                if p.playing() {
                    break;
                }
            }
            let while_open = std::fs::remove_file(path);
            println!(
                "während der Wiedergabe löschen: {}",
                match &while_open {
                    Ok(()) => "GELUNGEN (Datei war nicht gesperrt)".to_string(),
                    Err(e) => format!("abgewiesen — {e}"),
                }
            );
            let pos = p.position();
            drop(p);
            std::thread::sleep(std::time::Duration::from_millis(400));
            let after = std::fs::remove_file(path);
            println!(
                "nach dem Freigeben (Position war {pos:.3}): {}",
                match &after {
                    Ok(()) => "gelöscht".to_string(),
                    Err(e) => format!("FEHLGESCHLAGEN — {e}"),
                }
            );
            std::process::exit(if while_open.is_err() && after.is_ok() { 0 } else { 1 });
        }
        Some("--media") => {
            // Headless playback check: does libVLC actually decode into our buffers?
            let Some(path) = args.get(1) else {
                eprintln!("Nutzung: --media <datei>");
                std::process::exit(2);
            };
            if !crate::media::available() {
                eprintln!("libVLC nicht gefunden");
                std::process::exit(1);
            }
            let Some(mut p) = crate::media::Player_::open(path, false, 0) else {
                eprintln!("konnte nicht geöffnet werden");
                std::process::exit(1);
            };
            let mut frames = 0;
            let mut dims = (0, 0);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if let Some((w, h, buf)) = p.take_frame() {
                    frames += 1;
                    dims = (w, h);
                    if frames == 1 {
                        let lit = buf.chunks_exact(4).filter(|c| c[0] | c[1] | c[2] != 0).count();
                        println!("erstes Bild {w}x{h}, {lit} von {} Pixeln nicht schwarz", buf.len() / 4);
                    }
                    if frames >= 20 {
                        break;
                    }
                }
            }
            let (cur, total) = p.times();
            println!(
                "{frames} Bilder ({}x{}), Position {} von {}",
                dims.0,
                dims.1,
                crate::media::fmt_time(cur),
                crate::media::fmt_time(total)
            );
            // Audio has no video track, so a known duration is the proof there.
            std::process::exit(if frames > 0 || total > 0 { 0 } else { 1 });
        }
        Some("--console") => {
            // Foreground mode: same code path, visible output, Ctrl-C to stop.
            println!("Diskalize-Dienst im Vordergrund. Strg+C beendet.");
            let shared = run_core();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let vols = shared.volume_list();
                for v in &vols {
                    println!(
                        "  {:<12} gen={} usn={} scan={} section={}",
                        v.title, v.generation, v.usn, v.scanning, v.section
                    );
                }
            }
        }
        _ => {}
    }

    let name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_ptr() as *mut u16,
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
}
