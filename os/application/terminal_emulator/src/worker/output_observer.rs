use alloc::{rc::Rc, vec};
use naming::shared_types::OpenOptions;
use stream::OutputStream;
use terminal_lib::session::Session;

use crate::terminal::lfb_terminal::LFBTerminal;

use super::worker::Worker;

const BUFFER_SIZE: usize = 128;

pub struct OutputObserver {
    terminal: Rc<LFBTerminal>,
    /// Lazily opened, non-blocking reader on the session `out` FIFO. Held open
    /// for the whole session lifetime so the pipe buffer is never reset between
    /// foreground applications.
    out: Option<usize>,
}

impl OutputObserver {
    pub const fn new(terminal: Rc<LFBTerminal>) -> Self {
        Self { terminal, out: None }
    }

    fn handle(&mut self) -> Option<usize> {
        if self.out.is_none() {
            // The emulator keeps stdout reads non-blocking so its single-threaded
            // event loop can continue servicing input, cursor, and status work.
            self.out = naming::open(
                &Session::current().out_path(),
                OpenOptions::READONLY | OpenOptions::NONBLOCK,
            ).ok();
        }
        self.out
    }
}

impl Worker for OutputObserver {
    fn run(&mut self) {
        let Some(handle) = self.handle() else {
            return;
        };

        let mut buffer = vec![0u8; BUFFER_SIZE];

        // Non-blocking read: returns 0 when nothing is currently buffered
        let read_bytes = match naming::read(handle, &mut buffer) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };

        for byte in &mut buffer[0..read_bytes] {
            self.terminal.write_byte(*byte);
            *byte = 0;
        }
    }
}
