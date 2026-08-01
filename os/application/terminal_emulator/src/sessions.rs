/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: sessions                                                        ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Descr.: Per-session screen multiplexer for the terminal emulator.       ║
   ║                                                                         ║
   ║ The emulator is a pure renderer/input client of the standalone          ║
   ║ `session_manager`. This module owns, per live session, the endpoints    ║
   ║ (`out` reader, `ctl` reader, lazily-opened `in` writer), a semantic      ║
   ║ terminal model, and a canonical line editor. Every loop it drains        ║
   ║ *every* session's `out` FIFO into that session's model (so background     ║
   ║ apps never block on the bounded pipe), and renders only the active        ║
   ║ session's damage to the framebuffer. Switching tabs presents the target   ║
   ║ session's semantic viewport in one repaint - no history is replayed.     ║
   ║                                                                         ║
   ║ It also speaks the manager protocol: it consumes `created`/`destroyed`  ║
   ║ events, polls the active session id from the manager `ctl` record, and  ║
   ║ forwards UI commands (new/close/next/prev tab) on the `cmd` FIFO.       ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Aymen Sellami                                                   ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use alloc::collections::btree_map::BTreeMap;
use alloc::rc::Rc;

use naming::shared_types::OpenOptions;
use terminal_lib::manager;
use terminal_lib::session::{read_ctl, CtlRecord, Session, WaitState};

use crate::terminal::ansi::SemanticTerminal;
use crate::terminal::lfb_terminal::LFBTerminal;
use crate::util::banner::create_banner_string;
use crate::worker::canonical::{CanonicalAction, CanonicalEditor};

/// Bytes drained from a session `out` pipe per loop iteration.
const OUT_READ_CHUNK: usize = 256;

/// Sentinel "no active session selected yet" so the first manager `ctl` poll
/// always triggers an initial render.
const NO_ACTIVE: u8 = u8::MAX;

/// Per-session emulator state.
struct SessionScreen {
    /// Non-blocking reader on the session `out` FIFO (app stdout).
    out: Option<usize>,
    /// Reader on the session `ctl` record (foreground mode/wait state).
    ctl: Option<usize>,
    /// Lazily-opened writer on the session `in` FIFO (app stdin).
    in_writer: Option<usize>,
    /// Semantic screen state: parser + grid + cursor + colors + scrollback.
    terminal: SemanticTerminal,
    /// Canonical-mode line editor for this session.
    canonical: CanonicalEditor,
}

impl SessionScreen {
    fn new(terminal: SemanticTerminal) -> Self {
        Self {
            out: None,
            ctl: None,
            in_writer: None,
            terminal,
            canonical: CanonicalEditor::new(),
        }
    }
}

pub struct SessionMux {
    terminal: Rc<LFBTerminal>,
    /// Manager `ctl` record reader (pollable active-session state).
    manager_ctl: Option<usize>,
    /// Manager `cmd` FIFO writer (UI commands emulator -> manager).
    manager_cmd: Option<usize>,
    /// Manager `event` FIFO reader (create/destroy notifications).
    manager_event: Option<usize>,
    sessions: BTreeMap<u8, SessionScreen>,
    active: u8,
}

impl SessionMux {
    pub fn new(terminal: Rc<LFBTerminal>) -> Self {
        Self {
            terminal,
            manager_ctl: None,
            manager_cmd: None,
            manager_event: None,
            sessions: BTreeMap::new(),
            active: NO_ACTIVE,
        }
    }

    /// Connect to the manager namespace.
    ///
    /// Order matters for the FIFO rendezvous: open the `event` reader first (so
    /// the manager's `event` writer open can complete), then the `cmd` writer
    /// (blocks until the manager's `cmd` reader is present), then the pollable
    /// `ctl` record.
    pub fn connect(&mut self) {
        self.manager_event = naming::open(
            &manager::event_path(),
            OpenOptions::READONLY | OpenOptions::NONBLOCK,
        )
        .ok();
        self.manager_cmd = naming::open(&manager::cmd_path(), OpenOptions::WRITEONLY).ok();
        self.manager_ctl = naming::open(&manager::ctl_path(), OpenOptions::READWRITE).ok();
    }

    /// Drain manager create/destroy events and update the session set.
    pub fn poll_events(&mut self) {
        let Some(handle) = self.manager_event else {
            return;
        };

        loop {
            let mut frame = [0u8; 2];
            let mut got = naming::read(handle, &mut frame).unwrap_or(0);
            if got == 0 {
                break;
            }
            // Complete a possibly torn 2-byte frame (the manager always writes
            // whole frames, so this stays aligned). We only get here with one
            // byte read, so the tail is a single byte: one more non-blocking
            // read returns 0 (nothing available yet) or 1 (frame completed).
            if got < frame.len() {
                got += naming::read(handle, &mut frame[got..]).unwrap_or(0);
            }

            // Don't act on a frame we couldn't finish reading; parsing a torn
            // frame would consume the first byte and lose sync with the writer.
            if got < frame.len() {
                break;
            }

            match frame[0] {
                manager::event::CREATED => self.add_session(frame[1]),
                manager::event::DESTROYED => self.remove_session(frame[1]),
                _ => {}
            }
        }
    }

    /// Poll the manager `ctl` record and switch the rendered session if the
    /// active id changed.
    pub fn poll_active(&mut self) {
        let Some(handle) = self.manager_ctl else {
            return;
        };
        let Ok(record) = manager::read_ctl(handle) else {
            return;
        };
        if !record.is_published() {
            return;
        }
        if record.active_id != self.active && self.sessions.contains_key(&record.active_id) {
            self.switch_to(record.active_id);
        }
    }

    /// Drain every session's output: feed it to that session's semantic model
    /// (so background writers never block on the bounded pipe) and repaint the
    /// active session's damaged rows.
    pub fn drain_outputs(&mut self) {
        let active = self.active;
        let terminal = self.terminal.clone();
        for (id, screen) in self.sessions.iter_mut() {
            let Some(handle) = screen.out else {
                continue;
            };
            let mut buffer = [0u8; OUT_READ_CHUNK];
            let read = naming::read(handle, &mut buffer).unwrap_or(0);
            if read == 0 {
                continue;
            }
            let damage = screen.terminal.feed(&buffer[0..read]);
            if *id == active {
                terminal.present_damage(screen.terminal.model(), damage);
            }
        }
    }

    /// The active session's foreground control record, if a foreground app has
    /// published one.
    pub fn active_ctl(&self) -> Option<CtlRecord> {
        let screen = self.sessions.get(&self.active)?;
        let handle = screen.ctl?;
        match read_ctl(handle) {
            Ok(record) if record.is_published() => Some(record),
            _ => None,
        }
    }

    /// Open the active session's `in` writer once its foreground app is waiting,
    /// so the app's blocking reader `open()` can complete.
    pub fn ensure_active_in_writer(&mut self) {
        let waiting = self
            .active_ctl()
            .is_some_and(|record| record.wait == WaitState::Waiting);
        if !waiting {
            return;
        }
        let active = self.active;
        if let Some(screen) = self.sessions.get_mut(&active) {
            if screen.in_writer.is_none() {
                let session = Session::with_id(active as usize);
                screen.in_writer = naming::open(&session.in_path(), OpenOptions::WRITEONLY).ok();
            }
        }
    }

    /// Deliver a decoded input buffer to the active session's foreground app.
    pub fn deliver(&mut self, bytes: &[u8]) {
        self.ensure_active_in_writer();
        if let Some(screen) = self.sessions.get(&self.active) {
            if let Some(handle) = screen.in_writer {
                let _ = naming::write(handle, bytes);
            }
        }
    }

    /// Apply a canonical-mode edit to the active session: echo into that
    /// session's semantic terminal (repainting it) and, on submit, deliver the
    /// completed line to its foreground app.
    pub fn handle_canonical(&mut self, action: CanonicalAction) {
        let active = self.active;
        let terminal = self.terminal.clone();

        let submit = {
            let Some(screen) = self.sessions.get_mut(&active) else {
                return;
            };
            let effect = screen.canonical.apply(action);
            if !effect.echo.is_empty() {
                let damage = screen.terminal.feed(effect.echo.as_bytes());
                terminal.present_damage(screen.terminal.model(), damage);
            }
            effect.submit
        };

        if let Some(bytes) = submit {
            self.deliver(&bytes);
        }
    }

    /// Forward a one-shot UI command to the manager.
    pub fn send_command(&mut self, command: u8) {
        if let Some(handle) = self.manager_cmd {
            let _ = naming::write(handle, &[command]);
        }
    }

    /// Repaint the active session's full viewport (used after GUI mode returns).
    pub fn present_active(&self) {
        if let Some(screen) = self.sessions.get(&self.active) {
            self.terminal.present_full(screen.terminal.model());
        }
    }

    fn add_session(&mut self, id: u8) {
        if self.sessions.contains_key(&id) {
            return;
        }
        let mut screen = SessionScreen::new(SemanticTerminal::new(self.terminal.screen_size()));
        let session = Session::with_id(id as usize);
        screen.out = naming::open(
            &session.out_path(),
            OpenOptions::READONLY | OpenOptions::NONBLOCK,
        )
        .ok();
        screen.ctl = naming::open(&session.ctl_path(), OpenOptions::READWRITE).ok();
        // Seed the banner so each tab shows it (survives tab switches via the
        // semantic model, not a byte replay ring).
        let _ = screen.terminal.feed(create_banner_string().as_bytes());
        self.sessions.insert(id, screen);

        self.publish_tabs();

        if self.active == id {
            self.present_active();
        }
    }

    fn remove_session(&mut self, id: u8) {
        if let Some(screen) = self.sessions.remove(&id) {
            if let Some(handle) = screen.out {
                let _ = naming::close(handle);
            }
            if let Some(handle) = screen.ctl {
                let _ = naming::close(handle);
            }
            if let Some(handle) = screen.in_writer {
                let _ = naming::close(handle);
            }
        }
        self.publish_tabs();
    }

    fn switch_to(&mut self, id: u8) {
        self.active = id;
        self.publish_tabs();
        self.present_active();
    }

    /// Push the current live session ids and active id into the terminal so the
    /// status bar can render tabs. The `update_tabs` lock is released before any
    /// drawing call, so this must not run while the `display` lock is held.
    fn publish_tabs(&self) {
        let ids: alloc::vec::Vec<u8> = self.sessions.keys().copied().collect();
        self.terminal.update_tabs(&ids, self.active);
    }
}
