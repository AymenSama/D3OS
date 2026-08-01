/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: canonical                                                       ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Descr.: Per-session canonical (line-edited) input buffer.               ║
   ║                                                                         ║
   ║ Canonical local echo is emulator-owned: as the user edits a line, the   ║
   ║ editor produces ANSI echo bytes that are fed back into that session's   ║
   ║ semantic terminal (so the edit survives tab switches), and on submit it ║
   ║ yields the completed line for delivery to the app's stdin.              ║
   ║                                                                         ║
   ║ Each session owns its own `CanonicalEditor`, so an in-progress line in  ║
   ║ one tab never leaks into another.                                       ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Aymen Sellami                                                   ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use alloc::{format, string::String, vec::Vec};

const BUFFER_SIZE: usize = 256;

/// A single canonical-mode edit request derived from a decoded key.
pub enum CanonicalAction {
    Insert(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Submit,
}

/// The result of applying a `CanonicalAction`: echo bytes to render into the
/// session's semantic terminal, and, on submit, the completed line bytes.
pub struct CanonicalEffect {
    pub echo: String,
    pub submit: Option<Vec<u8>>,
}

impl CanonicalEffect {
    fn none() -> Self {
        Self {
            echo: String::new(),
            submit: None,
        }
    }

    fn echo(echo: String) -> Self {
        Self { echo, submit: None }
    }
}

pub struct CanonicalEditor {
    cursor_pos: usize,
    buffer: String,
}

impl CanonicalEditor {
    pub const fn new() -> Self {
        Self {
            cursor_pos: 0,
            buffer: String::new(),
        }
    }

    pub fn apply(&mut self, action: CanonicalAction) -> CanonicalEffect {
        match action {
            CanonicalAction::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    CanonicalEffect::echo("\x1b[1D".into())
                } else {
                    CanonicalEffect::none()
                }
            }
            CanonicalAction::Right => {
                if self.cursor_pos < self.buffer.len() {
                    self.cursor_pos += 1;
                    CanonicalEffect::echo("\x1b[1C".into())
                } else {
                    CanonicalEffect::none()
                }
            }
            CanonicalAction::Home => {
                let steps = self.cursor_pos;
                self.cursor_pos = 0;
                if steps > 0 {
                    CanonicalEffect::echo(format!("\x1b[{}D", steps))
                } else {
                    CanonicalEffect::none()
                }
            }
            CanonicalAction::End => {
                let steps = self.buffer.len() - self.cursor_pos;
                self.cursor_pos = self.buffer.len();
                if steps > 0 {
                    CanonicalEffect::echo(format!("\x1b[{}C", steps))
                } else {
                    CanonicalEffect::none()
                }
            }
            CanonicalAction::Backspace => {
                if self.cursor_pos > 0 {
                    self.buffer.remove(self.cursor_pos - 1);
                    self.cursor_pos -= 1;
                    CanonicalEffect::echo(format!("\x1B[1D \x1B[1D{}", self.redraw_tail()))
                } else {
                    CanonicalEffect::none()
                }
            }
            CanonicalAction::Delete => {
                if self.cursor_pos < self.buffer.len() && !self.buffer.is_empty() {
                    self.buffer.remove(self.cursor_pos);
                    CanonicalEffect::echo(format!(" \x1B[1D{}", self.redraw_tail()))
                } else {
                    CanonicalEffect::none()
                }
            }
            CanonicalAction::Insert(ch) => {
                if self.buffer.len() < BUFFER_SIZE {
                    self.buffer.insert(self.cursor_pos, ch);
                    self.cursor_pos += 1;
                    CanonicalEffect::echo(format!("{}{}", ch, self.redraw_tail()))
                } else {
                    CanonicalEffect::none()
                }
            }
            CanonicalAction::Submit => {
                let offset = self.buffer.len() - self.cursor_pos;
                let echo = if offset > 0 {
                    format!("\x1B[{}C\n", offset)
                } else {
                    "\n".into()
                };
                let line = self.buffer.clone().into_bytes();
                self.buffer.clear();
                self.cursor_pos = 0;
                CanonicalEffect {
                    echo,
                    submit: Some(line),
                }
            }
        }
    }

    /// Echo needed to redraw the buffer tail after the cursor when characters
    /// are inserted or removed mid-line, leaving the cursor in place.
    fn redraw_tail(&self) -> String {
        let content = &self.buffer[self.cursor_pos..];
        if content.is_empty() {
            String::new()
        } else {
            format!("\x1b[0K{}\x1B[{}D", content, content.len())
        }
    }
}
