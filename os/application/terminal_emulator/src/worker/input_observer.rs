use core::cell::RefCell;

use alloc::{format, rc::Rc, string::String, vec::Vec};
use globals::hotkeys::{
    HKEY_CLOSE_TAB, HKEY_NEW_TAB, HKEY_NEXT_TAB, HKEY_PREV_TAB, HKEY_TOGGLE_TERMINAL_WINDOW,
};
use pc_keyboard::{DecodedKey, EventDecoder, HandleControl, KeyCode, KeyEvent};
use pc_keyboard::layouts::{AnyLayout, De105Key};
use stream::{event_to_u16, OutputStream, RawInputStream};
use terminal_lib::manager::cmd;
use terminal_lib::session::{CtlRecord, WaitState};
use terminal_lib::{DecodedKeyType, TerminalMode};

use crate::{
    event_handler::{Event, EventHandler},
    sessions::SessionMux,
    terminal::lfb_terminal::LFBTerminal,
};

use super::worker::Worker;

const BUFFER_SIZE: usize = 256;

struct Canonical {
    cursor_pos: usize,
    buffer: String,
}

impl Canonical {
    const fn new() -> Self {
        Self {
            cursor_pos: 0,
            buffer: String::new(),
        }
    }

    fn submit(&mut self) -> Vec<u8> {
        let buffer = self.buffer.clone();
        self.cursor_pos = 0;
        self.buffer.clear();
        buffer.into()
    }

    fn remove_at_cursor(&mut self) -> Result<(), ()> {
        if self.cursor_pos >= self.buffer.len() || self.buffer.is_empty() {
            return Err(());
        }
        self.buffer.remove(self.cursor_pos);
        Ok(())
    }

    fn remove_before_cursor(&mut self) -> Result<(), ()> {
        if self.cursor_pos <= 0 {
            return Err(());
        }
        self.buffer.remove(self.cursor_pos - 1);
        self.cursor_pos -= 1;
        Ok(())
    }

    fn add_at_cursor(&mut self, ch: char) -> Result<(), ()> {
        if self.buffer.len() >= BUFFER_SIZE {
            return Err(());
        }
        self.buffer.insert(self.cursor_pos, ch);
        self.cursor_pos += 1;
        Ok(())
    }

    fn move_cursor_to_start(&mut self) -> Result<usize, ()> {
        self.cursor_pos = 0;
        Ok(self.cursor_pos)
    }

    fn move_cursor_to_end(&mut self) -> Result<usize, ()> {
        self.cursor_pos = self.buffer.len();
        Ok(self.buffer.len() - self.cursor_pos)
    }

    fn move_cursor_left(&mut self) -> Result<(), ()> {
        if self.cursor_pos <= 0 {
            return Err(());
        }
        self.cursor_pos -= 1;
        Ok(())
    }

    fn move_cursor_right(&mut self) -> Result<(), ()> {
        if self.cursor_pos >= self.buffer.len() {
            return Err(());
        }
        self.cursor_pos += 1;
        Ok(())
    }
}

pub struct InputObserver {
    terminal: Rc<LFBTerminal>,
    event_handler: Rc<RefCell<EventHandler>>,
    decoder: EventDecoder<AnyLayout>,
    mode: TerminalMode,
    canonical: Canonical,
    /// Shared session multiplexer: owns per-session endpoints and routes input
    /// to the active session's foreground app.
    mux: Rc<RefCell<SessionMux>>,
}

impl InputObserver {
    pub fn new(
        terminal: Rc<LFBTerminal>,
        event_handler: Rc<RefCell<EventHandler>>,
        mux: Rc<RefCell<SessionMux>>,
    ) -> Self {
        Self {
            terminal,
            event_handler,
            decoder: EventDecoder::new(
                AnyLayout::De105Key(De105Key),
                HandleControl::Ignore,
            ),
            mode: TerminalMode::Raw,
            canonical: Canonical::new(),
            mux,
        }
    }

    /// Resolve the current foreground input mode from the control record.
    /// `None` means no foreground app is currently waiting for input.
    fn resolve_mode(&self, record: Option<CtlRecord>) -> Option<TerminalMode> {
        match record {
            Some(record) if record.wait == WaitState::Waiting => Some(record.mode),
            _ => None,
        }
    }

    /// Deliver a decoded input buffer to the active session's foreground app.
    fn deliver(&mut self, buffer: &[u8]) {
        self.mux.borrow_mut().deliver(buffer);
    }
}

impl Worker for InputObserver {
    fn run(&mut self) {
        self.mux.borrow_mut().ensure_active_in_writer();

        let Some(key_event) = self.terminal.read_event_nb() else { return };

        let record = self.mux.borrow().active_ctl();
        let use_pipe = record.is_some();
        let mode = self.resolve_mode(record);

        if let Some(mode) = mode {
            self.mode = mode;
        }

        // Process key event into decoded key (unicode char or raw keycode)
        let Some(decoded_key) = self.decoder.process_keyevent(key_event.clone()) else {
            // This returns none if the key event was a key release
            // In this case, we only process the byte if the terminal is in raw mode
            if use_pipe && self.mode == TerminalMode::Raw {
                if let Some(buffer) = self.buffer_raw(key_event) {
                    self.deliver(&buffer);
                }
            }

            return;
        };

        // Handle reserved keys (e.g. hotkeys)
        let Some(decoded_key) = self.try_intercept_reserved_key(decoded_key) else {
            return;
        };

        // Buffer the decoded key based on the terminal input state
        let buffer = match mode {
            Some(TerminalMode::Canonical) => self.buffer_canonical(decoded_key),
            Some(TerminalMode::Fluid) => self.buffer_fluid(decoded_key),
            Some(TerminalMode::Raw) => self.buffer_raw(key_event),
            None => return,
        };
        let Some(buffer) = buffer else {
            return;
        };

        // Reaching here means a foreground app is actively waiting for input.
        self.deliver(&buffer);
    }
}

impl InputObserver {
    fn try_intercept_reserved_key(&self, key: DecodedKey) -> Option<DecodedKey> {
        match key {
            DecodedKey::RawKey(HKEY_TOGGLE_TERMINAL_WINDOW) => {
                self.event_handler.borrow_mut().trigger(Event::EnterGuiMode);
                None
            }
            DecodedKey::RawKey(HKEY_NEW_TAB) => {
                self.mux.borrow_mut().send_command(cmd::NEW_TAB);
                None
            }
            DecodedKey::RawKey(HKEY_CLOSE_TAB) => {
                self.mux.borrow_mut().send_command(cmd::CLOSE_ACTIVE);
                None
            }
            DecodedKey::RawKey(HKEY_NEXT_TAB) => {
                self.mux.borrow_mut().send_command(cmd::NEXT_TAB);
                None
            }
            DecodedKey::RawKey(HKEY_PREV_TAB) => {
                self.mux.borrow_mut().send_command(cmd::PREV_TAB);
                None
            }
            key => Some(key),
        }
    }

    fn buffer_raw(&self, event: KeyEvent) -> Option<Vec<u8>> {
        let raw = event_to_u16(event);
        Some(raw.to_ne_bytes().to_vec())
    }

    fn buffer_fluid(&self, key: DecodedKey) -> Option<Vec<u8>> {
        match key {
            DecodedKey::Unicode(key) => Some([DecodedKeyType::Unicode as u8, key as u8].to_vec()),
            DecodedKey::RawKey(key) => Some([DecodedKeyType::RawKey as u8, key as u8].to_vec()),
        }
    }

    fn buffer_canonical(&mut self, key: DecodedKey) -> Option<Vec<u8>> {
        match key {
            DecodedKey::RawKey(KeyCode::ArrowLeft) => {
                if self.canonical.move_cursor_left().is_ok() {
                    self.terminal.write_str("\x1b[1D");
                }
            }
            DecodedKey::RawKey(KeyCode::ArrowRight) => {
                if self.canonical.move_cursor_right().is_ok() {
                    self.terminal.write_str("\x1b[1C");
                }
            }
            DecodedKey::RawKey(KeyCode::Home) => {
                if let Ok(steps) = self.canonical.move_cursor_to_start() {
                    self.terminal.write_str(&format!("\x1b[{}D", steps));
                }
            }
            DecodedKey::RawKey(KeyCode::End) => {
                if let Ok(steps) = self.canonical.move_cursor_to_end() {
                    self.terminal.write_str(&format!("\x1b[{}C", steps));
                }
            }
            DecodedKey::RawKey(_) => return None,

            DecodedKey::Unicode('\x1B') => return None,
            DecodedKey::Unicode('\n') => {
                let offset = self.canonical.buffer.len() - self.canonical.cursor_pos;
                if offset > 0 {
                    self.terminal.write_str(&format!("\x1B[{}C\n", offset));
                } else {
                    self.terminal.write_byte(b'\n');
                }
                return Some(self.canonical.submit());
            }
            DecodedKey::Unicode('\x08') => {
                if self.canonical.remove_before_cursor().is_ok() {
                    self.terminal
                        .write_str(&format!("\x1B[1D \x1B[1D{}", self.redraw_canonical_content()));
                }
            }
            DecodedKey::Unicode('\x7F') => {
                if self.canonical.remove_at_cursor().is_ok() {
                    self.terminal
                        .write_str(&format!(" \x1B[1D{}", self.redraw_canonical_content()));
                }
            }
            DecodedKey::Unicode(ch) => {
                if self.canonical.add_at_cursor(ch).is_ok() {
                    self.terminal
                        .write_str(&format!("{}{}", ch, self.redraw_canonical_content()));
                }
            }
        };
        None
    }

    fn redraw_canonical_content(&self) -> String {
        let content = &self.canonical.buffer[self.canonical.cursor_pos..];
        if content.is_empty() {
            String::new()
        } else {
            format!("\x1b[0K{}\x1B[{}D", content, content.len())
        }
    }
}
