/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: session                                                         ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Descr.: Userspace terminal session model built on `naming`.             ║
   ║                                                                         ║
   ║ A terminal session is a directory namespace `/term/<id>/` holding three ║
   ║ named objects:                                                          ║
   ║   - `in`  : FIFO carrying application stdin  (emulator -> app)          ║
   ║   - `out` : FIFO carrying application stdout (app -> emulator)          ║
   ║   - `ctl` : a regular file holding a fixed-layout control record        ║
   ║                                                                         ║
   ║ stdin/stdout stay plain byte streams. All terminal-mode and input-wait  ║
   ║ metadata lives on the `ctl` record, never inside the data streams.      ║
   ║                                                                         ║
   ║ Discovery is centralized here: `Session::current()` is the single       ║
   ║ resolver every application goes through. Phase 1 resolves a fixed       ║
   ║ default session by well-known path; later phases can swap the body of   ║
   ║ `current()` for a launch-time descriptor without touching callers.      ║
   ║                                                                         ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Aymen Sellami                                                   ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use alloc::format;
use alloc::string::String;

use naming::shared_types::SeekOrigin;
use syscall::return_vals::Errno;

use crate::TerminalMode;

/// Root of the terminal session namespace.
pub const SESSION_ROOT: &str = "/term";

/// Phase 1 ships a single foreground session with this id. The protocol is
/// already keyed by id so that multiple sessions/tabs only require allocating
/// further ids, not a redesign.
pub const DEFAULT_SESSION_ID: usize = 0;

/// Argv/environment key under which a launch-time session descriptor will be
/// passed to child processes once multi-session support lands.
///
/// The descriptor is the session id as a decimal string, never a raw open
/// handle: handles are indices into the kernel's global open-object table and
/// have no per-process meaning, so they cannot be inherited. A child re-opens
/// its endpoints by path from the id. Until launch-time passing is wired
/// through process launch (`Thread::copy_args` / `runtime::env`), `current()`
/// resolves the well-known default session by path.
pub const SESSION_DESCRIPTOR_KEY: &str = "D3OS_TERM_SESSION";

/// Size of the fixed-layout control record stored in `ctl`. The record never
/// grows; it is always read/written in full at offset 0.
pub const CTL_RECORD_SIZE: usize = 32;

/// Current control-record layout version. A `version` of 0 means "no record
/// has been published yet" (a freshly `touch`ed, empty file decodes to this).
pub const CTL_VERSION: u8 = 1;

// Byte offsets within the control record.
const OFF_VERSION: usize = 0;
const OFF_MODE: usize = 1;
const OFF_WAIT: usize = 2;
// byte 3 reserved (flags)
const OFF_ROWS: usize = 4; // u16, little endian (reserved for resize)
const OFF_COLS: usize = 6; // u16, little endian (reserved for resize)
// bytes 8..24 reserved for a future foreground-process id (u128)
// bytes 24..32 reserved

/// Input-wait flag carried on the control plane.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum WaitState {
    /// The foreground app is not currently waiting for input.
    Idle,
    /// The foreground app is blocked waiting for input in the recorded mode.
    Waiting,
}

impl WaitState {
    fn to_byte(self) -> u8 {
        match self {
            WaitState::Idle => 0,
            WaitState::Waiting => 1,
        }
    }

    fn from_byte(b: u8) -> Self {
        match b {
            1 => WaitState::Waiting,
            _ => WaitState::Idle,
        }
    }
}

/// The pollable control record exchanged on the `ctl` plane.
#[derive(Debug, Clone, Copy)]
pub struct CtlRecord {
    version: u8,
    pub mode: TerminalMode,
    pub wait: WaitState,
    pub rows: u16,
    pub cols: u16,
}

impl CtlRecord {
    /// A zeroed/unpublished record. `is_published()` is `false` for this value.
    pub fn empty() -> Self {
        Self {
            version: 0,
            mode: TerminalMode::Canonical,
            wait: WaitState::Idle,
            rows: 0,
            cols: 0,
        }
    }

    /// A published record describing the app's requested mode and wait-state.
    pub fn new(mode: TerminalMode, wait: WaitState) -> Self {
        Self {
            version: CTL_VERSION,
            mode,
            wait,
            rows: 0,
            cols: 0,
        }
    }

    /// `true` once a process has actually written a record.
    pub fn is_published(&self) -> bool {
        self.version != 0
    }

    fn encode(&self) -> [u8; CTL_RECORD_SIZE] {
        let mut buf = [0u8; CTL_RECORD_SIZE];
        buf[OFF_VERSION] = self.version;
        buf[OFF_MODE] = usize::from(self.mode) as u8;
        buf[OFF_WAIT] = self.wait.to_byte();
        buf[OFF_ROWS..OFF_ROWS + 2].copy_from_slice(&self.rows.to_le_bytes());
        buf[OFF_COLS..OFF_COLS + 2].copy_from_slice(&self.cols.to_le_bytes());
        buf
    }

    fn decode(buf: &[u8]) -> Self {
        if buf.len() < CTL_RECORD_SIZE || buf[OFF_VERSION] == 0 {
            return Self::empty();
        }
        let mode = TerminalMode::from(buf[OFF_MODE] as usize);
        let wait = WaitState::from_byte(buf[OFF_WAIT]);
        let rows = u16::from_le_bytes([buf[OFF_ROWS], buf[OFF_ROWS + 1]]);
        let cols = u16::from_le_bytes([buf[OFF_COLS], buf[OFF_COLS + 1]]);
        Self {
            version: buf[OFF_VERSION],
            mode,
            wait,
            rows,
            cols,
        }
    }
}

/// A resolved terminal session: just the absolute paths of its objects. All
/// path knowledge is confined to this type.
pub struct Session {
    base: String,
}

impl Session {
    /// Resolve the session the current process belongs to.
    ///
    /// Phase 1: every process shares the single foreground session at a
    /// well-known path. Future phases replace this body with a launch-time
    /// descriptor lookup; callers do not change.
    pub fn current() -> Self {
        Self::with_id(DEFAULT_SESSION_ID)
    }

    /// Resolve a session by explicit id (used by the owner that allocates ids).
    pub fn with_id(id: usize) -> Self {
        Session {
            base: format!("{}/{}", SESSION_ROOT, id),
        }
    }

    /// Resolve a session from a launch-time descriptor string (the decimal
    /// session id). Reserved for the multi-session phase: once descriptors are
    /// carried through process launch, `current()` will parse the descriptor
    /// (see [`SESSION_DESCRIPTOR_KEY`]) and delegate here. Centralizing the
    /// shape now means switching from the fixed default to passed ids touches
    /// only this module, never the applications.
    pub fn from_descriptor(descriptor: &str) -> Option<Self> {
        descriptor.trim().parse::<usize>().ok().map(Self::with_id)
    }

    pub fn in_path(&self) -> String {
        format!("{}/in", self.base)
    }

    pub fn out_path(&self) -> String {
        format!("{}/out", self.base)
    }

    pub fn ctl_path(&self) -> String {
        format!("{}/ctl", self.base)
    }

    /// Create the session namespace and its three objects.
    ///
    /// Best-effort and intended to be called once by the session owner (the
    /// emulator in Phase 1). Errors on already-existing components are ignored
    /// so a restart does not fail. The `ctl` file is left empty, which decodes
    /// to an *unpublished* record.
    pub fn create(&self) -> Result<(), Errno> {
        let _ = naming::mkdir(SESSION_ROOT);
        let _ = naming::mkdir(&self.base);
        let _ = naming::mkfifo(&self.in_path());
        let _ = naming::mkfifo(&self.out_path());
        let _ = naming::touch(&self.ctl_path());
        Ok(())
    }

}

/// Read the full control record from an open `ctl` handle (offset is reset to
/// 0 first, so the same handle can be polled repeatedly).
pub fn read_ctl(handle: usize) -> Result<CtlRecord, Errno> {
    naming::seek(handle, 0, SeekOrigin::Start)?;
    let mut buf = [0u8; CTL_RECORD_SIZE];
    let n = naming::read(handle, &mut buf)?;
    // Incomplete control record is treated as unpublished.
    if n < CTL_RECORD_SIZE {
        return Ok(CtlRecord::empty());
    }
    Ok(CtlRecord::decode(&buf))
}

/// Write the full control record to an open `ctl` handle at offset 0.
pub fn write_ctl(handle: usize, record: &CtlRecord) -> Result<(), Errno> {
    naming::seek(handle, 0, SeekOrigin::Start)?;
    let buf = record.encode();
    naming::write(handle, &buf)?;
    Ok(())
}
