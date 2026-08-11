//! Wire protocol between the service and the GUI.
//!
//! Length-framed messages over a named pipe. Bulk index data never travels here
//! — that goes through shared memory (see [`crate::snapshot`]). What does travel
//! is the volume list and, afterwards, the individual filesystem changes, so
//! both sides can keep their copy of an index in step by running the same
//! `apply_changes` over the same records.

pub const PIPE: &str = r"\\.\pipe\DiskalizeService";
pub const PROTOCOL: u32 = 1;

// client -> service
pub const REQ_HELLO: u8 = 0x01;
pub const REQ_RESCAN: u8 = 0x02;
pub const REQ_ADD_PATH: u8 = 0x03;
pub const REQ_FORGET: u8 = 0x04;
/// Re-publish a volume's snapshot so a client that loads it late gets current
/// data rather than whatever the last scan left behind.
pub const REQ_PUBLISH: u8 = 0x05;

// service -> client
pub const MSG_VOLUMES: u8 = 0x81;
pub const MSG_DELTA: u8 = 0x82;
pub const MSG_VOLUME_UPDATED: u8 = 0x83;
pub const MSG_STATUS: u8 = 0x84;

/// One filesystem change, in the form both sides can apply to an index.
#[derive(Clone, Debug)]
pub struct Change {
    pub rec: u32,
    /// False when the entry is gone.
    pub alive: bool,
    pub parent: u32,
    pub flags: u8,
    pub alloc: u64,
    pub logical: u64,
    pub mtime: u32,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct VolumeMsg {
    pub key: String,
    pub title: String,
    /// Shared-memory section holding the snapshot.
    pub section: String,
    pub generation: u64,
    /// True when the volume is kept live by the USN journal, i.e. deltas follow.
    pub usn: bool,
    pub scanning: bool,
}

// ---- encoding ---------------------------------------------------------------

#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn new(tag: u8) -> Self {
        // Four bytes reserved for the frame length, patched in `finish`.
        Writer(vec![0, 0, 0, 0, tag])
    }
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
    pub fn finish(mut self) -> Vec<u8> {
        let len = (self.0.len() - 4) as u32;
        self.0[..4].copy_from_slice(&len.to_le_bytes());
        self.0
    }
}

pub struct Reader<'a> {
    b: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }
    pub fn u8(&mut self) -> u8 {
        let v = self.b.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        v
    }
    pub fn u32(&mut self) -> u32 {
        if self.pos + 4 > self.b.len() {
            self.pos = self.b.len();
            return 0;
        }
        let v = u32::from_le_bytes(self.b[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        v
    }
    pub fn u64(&mut self) -> u64 {
        if self.pos + 8 > self.b.len() {
            self.pos = self.b.len();
            return 0;
        }
        let v = u64::from_le_bytes(self.b[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        v
    }
    pub fn str(&mut self) -> String {
        let n = self.u32() as usize;
        if self.pos + n > self.b.len() {
            self.pos = self.b.len();
            return String::new();
        }
        let s = String::from_utf8_lossy(&self.b[self.pos..self.pos + n]).into_owned();
        self.pos += n;
        s
    }
    pub fn left(&self) -> usize {
        self.b.len().saturating_sub(self.pos)
    }
}

pub fn write_volumes(vols: &[VolumeMsg]) -> Vec<u8> {
    let mut w = Writer::new(MSG_VOLUMES);
    w.u32(PROTOCOL);
    w.u32(vols.len() as u32);
    for v in vols {
        w.str(&v.key);
        w.str(&v.title);
        w.str(&v.section);
        w.u64(v.generation);
        w.u8(u8::from(v.usn));
        w.u8(u8::from(v.scanning));
    }
    w.finish()
}

pub fn read_volumes(r: &mut Reader<'_>) -> (u32, Vec<VolumeMsg>) {
    let proto = r.u32();
    let n = r.u32() as usize;
    let mut out = Vec::with_capacity(n.min(64));
    for _ in 0..n.min(256) {
        out.push(VolumeMsg {
            key: r.str(),
            title: r.str(),
            section: r.str(),
            generation: r.u64(),
            usn: r.u8() != 0,
            scanning: r.u8() != 0,
        });
    }
    (proto, out)
}

pub fn write_delta(key: &str, changes: &[Change]) -> Vec<u8> {
    let mut w = Writer::new(MSG_DELTA);
    w.str(key);
    w.u32(changes.len() as u32);
    for c in changes {
        w.u32(c.rec);
        w.u8(u8::from(c.alive));
        if c.alive {
            w.u32(c.parent);
            w.u8(c.flags);
            w.u64(c.alloc);
            w.u64(c.logical);
            w.u32(c.mtime);
            w.str(&c.name);
        }
    }
    w.finish()
}

pub fn read_delta(r: &mut Reader<'_>) -> (String, Vec<Change>) {
    let key = r.str();
    let n = r.u32() as usize;
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        if r.left() == 0 {
            break;
        }
        let rec = r.u32();
        let alive = r.u8() != 0;
        if !alive {
            out.push(Change {
                rec,
                alive: false,
                parent: 0,
                flags: 0,
                alloc: 0,
                logical: 0,
                mtime: 0,
                name: String::new(),
            });
            continue;
        }
        out.push(Change {
            rec,
            alive: true,
            parent: r.u32(),
            flags: r.u8(),
            alloc: r.u64(),
            logical: r.u64(),
            mtime: r.u32(),
            name: r.str(),
        });
    }
    (key, out)
}
