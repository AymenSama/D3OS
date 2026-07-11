/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: manager                                                         ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Descr.: Coordination protocol between the standalone `session_manager`  ║
   ║         and the `terminal_emulator`, built on `naming`.                 ║
   ║                                                                         ║
   ║ The manager owns a small namespace `/term/manager/` holding three       ║
   ║ objects:                                                                ║
   ║   - `ctl`   : a fixed-layout tmpfs control record (pollable state):     ║
   ║               the active session id, live session count, generation.    ║
   ║   - `cmd`   : a FIFO carrying one-shot UI commands emulator -> manager  ║
   ║               (new/close/next/prev tab).                                ║
   ║   - `event` : a FIFO carrying one-shot notifications manager -> emulator║
   ║               (session created/destroyed).                              ║
   ║                                                                         ║
   ║ Pollable state (which session is active) lives on the `ctl` record so   ║
   ║ the emulator can read it repeatedly without consuming it. One-shot      ║
   ║ transitions (a tab was created/destroyed, the user pressed a tab key)   ║
   ║ travel on the FIFOs so they are consumed exactly once.                  ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Aymen Sellami                                                   ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use alloc::format;
use alloc::string::String;

use naming::shared_types::SeekOrigin;
use syscall::return_vals::Errno;

use crate::session::TERM_ROOT;

/// Root of the manager coordination namespace.
pub const MANAGER_ROOT: &str = "/term/manager";

/// One-shot UI commands sent emulator -> manager on the `cmd` FIFO, one byte
/// per command.
pub mod cmd {
    /// Create a new session/tab and make it active.
    pub const NEW_TAB: u8 = 1;
    /// Close the currently active session/tab.
    pub const CLOSE_ACTIVE: u8 = 2;
    /// Switch to the next live session/tab.
    pub const NEXT_TAB: u8 = 3;
    /// Switch to the previous live session/tab.
    pub const PREV_TAB: u8 = 4;
}

/// One-shot notifications sent manager -> emulator on the `event` FIFO, encoded
/// as a 2-byte frame `[opcode, id]`.
pub mod event {
    /// A session with the given id was created; the emulator should open its
    /// endpoints and allocate a screen buffer.
    pub const CREATED: u8 = 1;
    /// A session with the given id was destroyed; the emulator should close its
    /// endpoints and drop the screen buffer.
    pub const DESTROYED: u8 = 2;
}

/// Size of the fixed-layout manager control record. Always read/written in full
/// at offset 0, like the per-session `ctl` record.
pub const MANAGER_CTL_SIZE: usize = 16;

/// Current manager control-record version. `0` means "not published yet".
pub const MANAGER_CTL_VERSION: u8 = 1;

const OFF_VERSION: usize = 0;
const OFF_ACTIVE: usize = 1; // active session id (u8)
const OFF_COUNT: usize = 2; // live session count (u8)
const OFF_GENERATION: usize = 3; // bumped on every change (u8)
// bytes 4..16 reserved

/// The pollable manager control record.
#[derive(Debug, Clone, Copy)]
pub struct ManagerCtl {
    version: u8,
    pub active_id: u8,
    pub session_count: u8,
    pub generation: u8,
}

impl ManagerCtl {
    /// An unpublished record. `is_published()` is `false` for this value.
    pub fn empty() -> Self {
        Self {
            version: 0,
            active_id: 0,
            session_count: 0,
            generation: 0,
        }
    }

    /// A published record describing the current active session and counters.
    pub fn new(active_id: u8, session_count: u8, generation: u8) -> Self {
        Self {
            version: MANAGER_CTL_VERSION,
            active_id,
            session_count,
            generation,
        }
    }

    /// `true` once the manager has actually written a record.
    pub fn is_published(&self) -> bool {
        self.version != 0
    }

    fn encode(&self) -> [u8; MANAGER_CTL_SIZE] {
        let mut buf = [0u8; MANAGER_CTL_SIZE];
        buf[OFF_VERSION] = self.version;
        buf[OFF_ACTIVE] = self.active_id;
        buf[OFF_COUNT] = self.session_count;
        buf[OFF_GENERATION] = self.generation;
        buf
    }

    fn decode(buf: &[u8]) -> Self {
        if buf.len() < MANAGER_CTL_SIZE || buf[OFF_VERSION] == 0 {
            return Self::empty();
        }
        Self {
            version: buf[OFF_VERSION],
            active_id: buf[OFF_ACTIVE],
            session_count: buf[OFF_COUNT],
            generation: buf[OFF_GENERATION],
        }
    }
}

pub fn ctl_path() -> String {
    format!("{}/ctl", MANAGER_ROOT)
}

pub fn cmd_path() -> String {
    format!("{}/cmd", MANAGER_ROOT)
}

pub fn event_path() -> String {
    format!("{}/event", MANAGER_ROOT)
}

/// Create the manager namespace and its three objects.
///
/// Best-effort and intended to be called once by the manager at startup. Errors
/// on already-existing components are ignored so a restart does not fail.
pub fn create_namespace() {
    let _ = naming::mkdir(TERM_ROOT);
    let _ = naming::mkdir(MANAGER_ROOT);
    let _ = naming::touch(&ctl_path());
    let _ = naming::mkfifo(&cmd_path());
    let _ = naming::mkfifo(&event_path());
}

/// Read the full manager control record from an open `ctl` handle (offset is
/// reset to 0 first, so the same handle can be polled repeatedly).
pub fn read_ctl(handle: usize) -> Result<ManagerCtl, Errno> {
    naming::seek(handle, 0, SeekOrigin::Start)?;
    let mut buf = [0u8; MANAGER_CTL_SIZE];
    let n = naming::read(handle, &mut buf)?;
    if n < MANAGER_CTL_SIZE {
        return Ok(ManagerCtl::empty());
    }
    Ok(ManagerCtl::decode(&buf))
}

/// Write the full manager control record to an open `ctl` handle at offset 0.
pub fn write_ctl(handle: usize, record: &ManagerCtl) -> Result<(), Errno> {
    naming::seek(handle, 0, SeekOrigin::Start)?;
    let buf = record.encode();
    naming::write(handle, &buf)?;
    Ok(())
}
