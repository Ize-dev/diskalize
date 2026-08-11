//! GUI side of the service connection.
//!
//! Snapshots arrive through shared memory and become ordinary local `Index`
//! values, so everything above this layer — chart, tree, search, preview — keeps
//! working on plain in-process data at full speed. Only the *changes* travel
//! over the pipe, and they are applied with the same code the service ran.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use parking_lot::RwLock;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::{SetNamedPipeHandleState, PIPE_READMODE_MESSAGE};

use std::collections::HashMap;

use crate::index::Index;
use crate::ipc::{self, VolumeMsg};
use crate::win::wide;

/// What the client knows about one volume.
///
/// The index handle exists as soon as the service mentions the volume, but it
/// stays empty until something asks for the data. A volume is hundreds of
/// megabytes and most sessions only ever look at one, so mapping all of them up
/// front was simply wasted memory.
struct VolState {
    index: Arc<RwLock<Index>>,
    /// Section the service last announced.
    section: String,
    /// Section actually mapped into `index`, if any.
    mapped: Option<String>,
    wanted: bool,
}

/// What the connection thread reports back to the UI.
pub enum Event {
    Connected,
    Disconnected,
    /// A volume's snapshot was (re)loaded.
    Volume {
        key: String,
        title: String,
        index: Arc<RwLock<Index>>,
        usn: bool,
    },
    /// Volume list changed without new data, e.g. a scan started.
    Status(Vec<VolumeMsg>),
    /// Live changes were folded into an existing index.
    Changed(String),
}

pub enum Cmd {
    Rescan(String),
    AddPath(String),
    Forget(String),
    /// Map this volume's snapshot now. Loading is deferred until something
    /// actually needs the data — a whole volume is hundreds of megabytes, and
    /// most sessions only ever look at one.
    Load(String),
    /// Map every known volume, for a search across all of them.
    LoadAll,
}

pub struct Client {
    pub events: Receiver<Event>,
    cmds: Sender<Cmd>,
    connected: Arc<AtomicBool>,
}

impl Client {
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmds.send(cmd);
    }
}

fn connect() -> Option<HANDLE> {
    let h = unsafe {
        CreateFileW(
            wide(ipc::PIPE).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return None;
    }
    let mut mode = PIPE_READMODE_MESSAGE;
    unsafe {
        SetNamedPipeHandleState(h, &mut mode, std::ptr::null_mut(), std::ptr::null_mut());
    }
    Some(h)
}

fn request(pipe: HANDLE, frame: &[u8]) -> bool {
    let mut written = 0u32;
    unsafe {
        WriteFile(
            pipe,
            frame.as_ptr(),
            frame.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        ) != 0
    }
}

/// Maps a section into the volume's existing index handle, in place, so every
/// reference the UI already holds keeps pointing at the right data.
fn load_into(state: &Arc<parking_lot::Mutex<HashMap<String, VolState>>>, key: &str, section: &str) {
    let Some(ix) = crate::snapshot::attach(section) else {
        return;
    };
    let map = state.lock();
    let Some(entry) = map.get(key) else { return };
    let handle = Arc::clone(&entry.index);
    drop(map);
    *handle.write() = ix;
    if let Some(entry) = state.lock().get_mut(key) {
        entry.mapped = Some(section.to_string());
    }
}

pub fn spawn(wake: egui::Context) -> Client {
    let (ev_tx, ev_rx) = channel();
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let connected = Arc::new(AtomicBool::new(false));
    let state: Arc<parking_lot::Mutex<HashMap<String, VolState>>> = Arc::default();

    // Commands get their own short-lived connection each, so the long-lived
    // push stream is only ever read from. Mixing both directions on one
    // synchronous handle deadlocks: a blocked ReadFile stalls every WriteFile.
    let cmd_state = Arc::clone(&state);
    std::thread::Builder::new()
        .name("svc-cmd".into())
        .spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                match &cmd {
                    Cmd::Load(k) => {
                        if let Some(e) = cmd_state.lock().get_mut(k) {
                            e.wanted = true;
                        }
                    }
                    Cmd::LoadAll => {
                        for e in cmd_state.lock().values_mut() {
                            e.wanted = true;
                        }
                    }
                    _ => {}
                }
                let frame = match cmd {
                    // Handled by the reader thread; nothing to send.
                    // Answered by re-publishing: a client that loads late must
                    // not inherit a snapshot from before the last changes.
                    Cmd::Load(k) => {
                        let mut w = ipc::Writer::new(ipc::REQ_PUBLISH);
                        w.str(&k);
                        w.finish()
                    }
                    Cmd::LoadAll => {
                        let mut w = ipc::Writer::new(ipc::REQ_PUBLISH);
                        w.str("");
                        w.finish()
                    }
                    Cmd::Rescan(k) => {
                        let mut w = ipc::Writer::new(ipc::REQ_RESCAN);
                        w.str(&k);
                        w.finish()
                    }
                    Cmd::AddPath(p) => {
                        let mut w = ipc::Writer::new(ipc::REQ_ADD_PATH);
                        w.str(&p);
                        w.finish()
                    }
                    Cmd::Forget(k) => {
                        let mut w = ipc::Writer::new(ipc::REQ_FORGET);
                        w.str(&k);
                        w.finish()
                    }
                };
                if let Some(pipe) = connect() {
                    request(pipe, &frame);
                    unsafe { CloseHandle(pipe) };
                }
            }
        })
        .ok();

    let flag = Arc::clone(&connected);
    std::thread::Builder::new()
        .name("svc-client".into())
        .spawn(move || {

            loop {
                let Some(pipe) = connect() else {
                    if flag.swap(false, Ordering::Relaxed) {
                        let _ = ev_tx.send(Event::Disconnected);
                        wake.request_repaint();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(
                        if crate::win::last_error() == ERROR_PIPE_BUSY {
                            200
                        } else {
                            1500
                        },
                    ));
                    continue;
                };
                flag.store(true, Ordering::Relaxed);
                let _ = ev_tx.send(Event::Connected);
                wake.request_repaint();

                let mut hello = ipc::Writer::new(ipc::REQ_HELLO);
                hello.u32(ipc::PROTOCOL);
                if !request(pipe, &hello.finish()) {
                    unsafe { CloseHandle(pipe) };
                    continue;
                }

                let mut buf = vec![0u8; 1 << 20];
                loop {
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
                        break;
                    }
                    let body = &buf[4..read as usize];
                    let mut r = ipc::Reader::new(&body[1..]);
                    match body[0] {
                        ipc::MSG_VOLUMES | ipc::MSG_VOLUME_UPDATED => {
                            let (_, vols) = ipc::read_volumes(&mut r);
                            for v in &vols {
                                let mut map = state.lock();
                                let entry = map.entry(v.key.clone()).or_insert_with(|| {
                                    // Announced but not read yet: an empty index
                                    // so the UI has something to hold on to.
                                    VolState {
                                        index: Arc::new(RwLock::new(Index::default())),
                                        section: String::new(),
                                        mapped: None,
                                        wanted: false,
                                    }
                                });
                                let first_sight = entry.section.is_empty();
                                if !v.section.is_empty() {
                                    entry.section = v.section.clone();
                                }
                                let handle = Arc::clone(&entry.index);
                                let need_load = entry.wanted
                                    && !entry.section.is_empty()
                                    && entry.mapped.as_deref() != Some(entry.section.as_str());
                                let section = entry.section.clone();
                                drop(map);

                                if first_sight {
                                    let _ = ev_tx.send(Event::Volume {
                                        key: v.key.clone(),
                                        title: v.title.clone(),
                                        index: handle,
                                        usn: v.usn,
                                    });
                                }
                                if need_load {
                                    load_into(&state, &v.key, &section);
                                    let _ = ev_tx.send(Event::Changed(v.key.clone()));
                                }
                            }
                            let _ = ev_tx.send(Event::Status(vols));
                            wake.request_repaint();
                        }
                        ipc::MSG_DELTA => {
                            // Nothing to apply: the service edits the shared
                            // pages we are reading. This is only the nudge that
                            // says the generation moved on.
                            let (key, _) = ipc::read_delta(&mut r);
                            let handle = state
                                .lock()
                                .get(&key)
                                .filter(|e| e.mapped.is_some())
                                .map(|e| Arc::clone(&e.index));
                            if let Some(ix) = handle {
                                if crate::snapshot::refresh(&mut ix.write()) {
                                    let _ = ev_tx.send(Event::Changed(key));
                                    wake.request_repaint();
                                }
                            }
                        }
                        _ => {}
                    }
                }

                unsafe { CloseHandle(pipe) };
                flag.store(false, Ordering::Relaxed);
                let _ = ev_tx.send(Event::Disconnected);
                wake.request_repaint();
                // The next connection re-announces every section; anything we
                // already mapped stays valid until it does.
                std::thread::sleep(std::time::Duration::from_millis(800));
            }
        })
        .ok();

    Client {
        events: ev_rx,
        cmds: cmd_tx,
        connected,
    }
}

