/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: session_manager                                                 ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Descr.: Standalone userspace owner of terminal sessions/tabs.           ║
   ║                                                                         ║
   ║ The session manager owns session-id allocation, the `/term/<id>/`       ║
   ║ namespace lifecycle, active-session selection, and the policy for       ║
   ║ launching shells/apps into sessions. In terminal-boot mode the kernel   ║
   ║ boots this process; it launches the `terminal_emulator` (renderer) and  ║
   ║ a shell per session, tagging each shell with the `D3OS_TERM_SESSION`    ║
   ║ env descriptor so the shell and its children resolve the right session. ║
   ║                                                                         ║
   ║ It coordinates with the emulator over `/term/manager/{ctl,cmd,event}`:  ║
   ║   - it publishes the active session id on the `ctl` record,             ║
   ║   - it consumes UI commands (new/close/next/prev tab) on `cmd`,         ║
   ║   - it announces session create/destroy on `event`.                     ║
   ║                                                                         ║
   ║ A per-session supervisor thread restarts a session's shell when it      ║
   ║ exits, and stops once the session is closed.                            ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Aymen Sellami                                                   ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
#![no_std]

extern crate alloc;

use alloc::collections::btree_map::BTreeMap;
use alloc::format;
use alloc::vec::Vec;

use concurrent::thread;
use naming::shared_types::OpenOptions;
use spin::Mutex;
use terminal::manager::{self, cmd, event, ManagerCtl};
use terminal::session::{Session, SESSION_DESCRIPTOR_KEY};

#[allow(unused_imports)]
use runtime::*;

/// Maximum number of concurrent sessions.
///
/// Current limit: open handles live in the kernel's single global
/// `OPEN_OBJECTS` table (cap `0x1000`, no per-process binding), and each live
/// session costs persistent emulator endpoints plus the per-app handles that
/// leak on exit. Until the handle table is bound per process, the manager caps
/// concurrent sessions and refuses further `NewTab` requests to stay within the
/// shared budget.
const MAX_SESSIONS: usize = 8;

/// Per-session bookkeeping kept by the manager.
struct SessionEntry {
    /// Thread id of the session's current shell (`0` if not yet running). Used
    /// to terminate the shell when the tab is closed.
    shell_tid: usize,
}

struct ManagerState {
    sessions: BTreeMap<u8, SessionEntry>,
    active: u8,
    next_id: u8,
    generation: u8,
}

static STATE: Mutex<ManagerState> = Mutex::new(ManagerState {
    sessions: BTreeMap::new(),
    active: 0,
    next_id: 0,
    generation: 0,
});

fn is_alive(id: u8) -> bool {
    STATE.lock().sessions.contains_key(&id)
}

fn set_shell_tid(id: u8, tid: usize) {
    if let Some(entry) = STATE.lock().sessions.get_mut(&id) {
        entry.shell_tid = tid;
    }
}

/// Publish the current active session and counters on the manager `ctl` record.
fn publish_ctl(ctl: usize) {
    let (active, count, generation) = {
        let mut state = STATE.lock();
        state.generation = state.generation.wrapping_add(1);
        let count = state.sessions.len() as u8;
        (state.active, count, state.generation)
    };
    let _ = manager::write_ctl(ctl, &ManagerCtl::new(active, count, generation));
}

/// Send a 2-byte `[opcode, id]` event frame to the emulator.
fn emit_event(event_w: usize, opcode: u8, id: u8) {
    let _ = naming::write(event_w, &[opcode, id]);
}

/// Restart loop for a single session's shell.
///
/// Runs in its own thread so the manager's command loop never blocks on
/// `join()`. Restarts the shell when it exits normally (e.g. the user runs the
/// `exit` built-in) and terminates once the session is closed.
fn supervise(id: u8) {
    loop {
        if !is_alive(id) {
            break;
        }

        let descriptor = format!("{}={}", SESSION_DESCRIPTOR_KEY, id);
        let env: Vec<&str> = alloc::vec![descriptor.as_str()];

        match thread::start_application_with_env("shell", Vec::new(), env) {
            Some(shell) => {
                set_shell_tid(id, shell.id());
                let _ = shell.join();
            }
            None => {
                thread::sleep(500);
            }
        }

        // If the session was closed while the shell ran, stop; otherwise the
        // shell exited on its own and we restart it.
        if !is_alive(id) {
            break;
        }
    }
}

fn spawn_supervisor(id: u8) {
    thread::create(move || supervise(id));
}

fn handle_command(command: u8, ctl: usize, event_w: usize) {
    match command {
        cmd::NEW_TAB => new_tab(ctl, event_w),
        cmd::CLOSE_ACTIVE => close_active(ctl, event_w),
        cmd::NEXT_TAB => switch_relative(ctl, 1),
        cmd::PREV_TAB => switch_relative(ctl, -1),
        _ => {}
    }
}

fn new_tab(ctl: usize, event_w: usize) {
    let id = {
        let mut state = STATE.lock();
        let live = state.sessions.len();
        if live >= MAX_SESSIONS {
            return; // Refuse gracefully; see MAX_SESSIONS.
        }
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state.sessions.insert(id, SessionEntry { shell_tid: 0 });
        state.active = id;
        id
    };

    let _ = Session::with_id(id as usize).create();
    emit_event(event_w, event::CREATED, id);
    spawn_supervisor(id);
    publish_ctl(ctl);
}

fn close_active(ctl: usize, event_w: usize) {
    let (closed, shell_tid) = {
        let mut state = STATE.lock();
        // Always keep at least one session alive.
        if state.sessions.len() <= 1 {
            return;
        }

        let id = state.active;
        let shell_tid = state.sessions.get(&id).map(|entry| entry.shell_tid).unwrap_or(0);
        state.sessions.remove(&id);

        // Pick the lowest remaining session id as the new active session.
        if let Some(&next) = state.sessions.keys().next() {
            state.active = next;
        }
        (id, shell_tid)
    };

    // Terminate the closed session's shell so its supervisor's join() returns
    // and the supervisor stops (the session is no longer alive).
    if shell_tid != 0 {
        thread::kill(shell_tid);
    }
    emit_event(event_w, event::DESTROYED, closed);
    publish_ctl(ctl);
}

fn switch_relative(ctl: usize, direction: i32) {
    {
        let mut state = STATE.lock();
        let live: Vec<u8> = state
            .sessions
            .iter()
            .map(|(id, _)| *id)
            .collect();
        if live.len() <= 1 {
            return;
        }

        let position = live.iter().position(|&id| id == state.active).unwrap_or(0) as i32;
        let len = live.len() as i32;
        let next = (((position + direction) % len) + len) % len;
        state.active = live[next as usize];
    }
    publish_ctl(ctl);
}

#[unsafe(no_mangle)]
pub fn main() {
    manager::create_namespace();

    let ctl = naming::open(&manager::ctl_path(), OpenOptions::READWRITE)
        .expect("failed to open manager ctl");

    // Create the initial session 0 and record it.
    Session::with_id(0).create();
    {
        let mut state = STATE.lock();
        state.sessions.insert(0, SessionEntry { shell_tid: 0 });
        state.active = 0;
        state.next_id = 1;
    }
    publish_ctl(ctl);

    // Launch the renderer. It connects back to the manager namespace and opens
    // its endpoints for each announced session.
    thread::start_application("terminal_emulator", Vec::new())
        .expect("failed to start terminal_emulator");

    // Open the coordination FIFOs. The event writer rendezvous with the
    // emulator's event reader; the cmd reader is non-blocking so the command
    // loop doesn't stall when no key has been pressed.
    let event_w = naming::open(&manager::event_path(), OpenOptions::WRITEONLY)
        .expect("failed to open manager event");
    let cmd_r = naming::open(&manager::cmd_path(), OpenOptions::READONLY | OpenOptions::NONBLOCK)
        .expect("failed to open manager cmd");

    // Announce the initial session and start its shell supervisor.
    emit_event(event_w, event::CREATED, 0);
    spawn_supervisor(0);

    // Command loop: react to UI commands from the emulator.
    loop {
        let mut byte = [0u8; 1];
        if let Ok(1) = naming::read(cmd_r, &mut byte) {
            handle_command(byte[0], ctl, event_w);
        }
        thread::sleep(20);
    }
}
