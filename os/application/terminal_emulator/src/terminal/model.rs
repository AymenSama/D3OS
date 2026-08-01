/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: model                                                           ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Descr.: Per-session semantic terminal state.                            ║
   ║                                                                         ║
   ║ `TerminalModel` is the framebuffer-independent screen state of a single ║
   ║ terminal session: a character grid, cursor, SGR color state, bounded    ║
   ║ scrollback, and a per-feed damage summary. It implements the            ║
   ║ `anstyle_parse::Perform` backend, so the ANSI behavior that previously  ║
   ║ lived on `LFBTerminal` now mutates pure state instead of drawing.       ║
   ║                                                                         ║
   ║ Coordinate note: row 0 of the grid is reserved for the status bar and   ║
   ║ is never rendered as content or selected by the cursor. Terminal        ║
   ║ content and cursor movement operate on rows 1..rows, and the            ║
   ║ framebuffer renderer draws the status bar over framebuffer row 0.       ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Aymen Sellami                                                   ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use alloc::collections::vec_deque::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use anstyle_parse::{Params, ParamsIter, Perform};
use graphic::{
    ansi::COLOR_TABLE_256,
    color::{self, Color, INVISIBLE},
    lfb,
};

use super::color::ColorState;

const TAB_SPACES: u16 = 4;

/// Byte budget bounding a session's scrollback (excludes the live viewport).
const SCROLLBACK_BYTE_BUDGET: usize = 256 * 1024;

/// Grid dimensions in character cells. `rows` is the full framebuffer row
/// count; row 0 is reserved for the status bar (see module note).
#[derive(Copy, Clone)]
pub struct ScreenSize {
    pub cols: u16,
    pub rows: u16,
}

/// One character cell. `width` is the glyph's column span: `1` for a normal
/// glyph, `2` for a wide glyph's lead cell, and `0` for the continuation cell
/// that follows a wide glyph (the renderer must not draw continuation cells).
#[derive(Copy, Clone)]
pub struct Cell {
    pub value: char,
    pub fg_color: Color,
    pub bg_color: Color,
    pub width: u8,
}

impl Cell {
    fn with(value: char, color: &ColorState) -> Self {
        Cell {
            value,
            fg_color: color.fg_color,
            bg_color: color.bg_color,
            width: 1,
        }
    }
}

/// One grid row. `wrapped` records that the row was terminated by an automatic
/// wrap rather than a newline; it is stored now for future scroll/selection
/// support and is not yet consumed by the renderer.
#[derive(Clone)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    fn blank(cols: u16, color: &ColorState) -> Self {
        Row {
            cells: vec![Cell::with('\0', color); cols as usize],
            wrapped: false,
        }
    }
}

/// Cursor position in grid coordinates (row 0 reserved, so content rows are
/// `1..rows`).
#[derive(Copy, Clone)]
pub struct ModelCursor {
    pub col: u16,
    pub row: u16,
    pub saved_col: u16,
    pub saved_row: u16,
}

impl ModelCursor {
    const fn new() -> Self {
        Self {
            col: 0,
            row: 1,
            saved_col: 0,
            saved_row: 1,
        }
    }
}

/// Summary of what a single `feed()` changed, so the active session can be
/// repainted incrementally. `full` forces a whole-viewport repaint; otherwise
/// `[dirty_min, dirty_max]` is the inclusive range of grid rows whose cells
/// changed. `cursor_changed` requests a cursor-overlay refresh.
#[derive(Copy, Clone)]
pub struct Damage {
    pub full: bool,
    dirty_min: Option<u16>,
    dirty_max: Option<u16>,
    pub cursor_changed: bool,
}

impl Damage {
    const fn none() -> Self {
        Self {
            full: false,
            dirty_min: None,
            dirty_max: None,
            cursor_changed: false,
        }
    }

    fn reset(&mut self) {
        *self = Self::none();
    }

    fn mark_full(&mut self) {
        self.full = true;
    }

    fn mark_cursor(&mut self) {
        self.cursor_changed = true;
    }

    fn mark_row(&mut self, row: u16) {
        self.dirty_min = Some(self.dirty_min.map_or(row, |m| m.min(row)));
        self.dirty_max = Some(self.dirty_max.map_or(row, |m| m.max(row)));
    }

    fn mark_range(&mut self, from: u16, to: u16) {
        self.mark_row(from);
        self.mark_row(to);
    }

    /// Inclusive dirty row range, if any cells changed this feed.
    pub fn dirty_range(&self) -> Option<(u16, u16)> {
        match (self.dirty_min, self.dirty_max) {
            (Some(min), Some(max)) => Some((min, max)),
            _ => None,
        }
    }
}

pub struct TerminalModel {
    pub(crate) size: ScreenSize,
    pub(crate) grid: Vec<Row>,
    pub(crate) scrollback: VecDeque<Row>,
    max_scrollback_rows: usize,
    pub(crate) cursor: ModelCursor,
    color: ColorState,
    pub(crate) damage: Damage,
}

impl TerminalModel {
    pub fn new(size: ScreenSize) -> Self {
        let color = ColorState::new();
        let grid = (0..size.rows)
            .map(|_| Row::blank(size.cols, &color))
            .collect();
        let row_bytes = (size.cols as usize).max(1) * core::mem::size_of::<Cell>();
        // Keep at least one scrollback row, even if one row exceeds the budget.
        let max_scrollback_rows = (SCROLLBACK_BYTE_BUDGET / row_bytes.max(1)).max(1);

        Self {
            size,
            grid,
            scrollback: VecDeque::new(),
            max_scrollback_rows,
            cursor: ModelCursor::new(),
            color,
            damage: Damage::none(),
        }
    }

    /// Reset the damage summary before a feed accumulates a new one.
    pub(crate) fn reset_damage(&mut self) {
        self.damage.reset();
    }

    fn set_cell(&mut self, col: u16, row: u16, cell: Cell) {
        if (row as usize) < self.grid.len() && col < self.size.cols {
            self.grid[row as usize].cells[col as usize] = cell;
        }
    }

    fn print_char(&mut self, c: char) {
        if c == '\n' {
            self.clear_line_from_cursor();
            self.cursor.col = 0;
            self.cursor.row += 1;
        } else if c == 0x08 as char {
            // backspace: move left (saturating) and blank the vacated cell
            self.cursor.col = self.cursor.col.saturating_sub(1);
            let (col, row) = (self.cursor.col, self.cursor.row);
            self.set_cell(col, row, Cell::with(' ', &self.color));
            self.damage.mark_row(row);
        } else {
            let width = char_columns(c);
            if width > 0 {
                let (col, row) = (self.cursor.col, self.cursor.row);
                let lead = Cell {
                    value: c,
                    fg_color: self.color.fg_color,
                    bg_color: self.color.bg_color,
                    width: width as u8,
                };
                self.set_cell(col, row, lead);

                // Null out following continuation cells for wide glyphs.
                for i in 1..width {
                    if col + i >= self.size.cols {
                        break;
                    }
                    self.set_cell(
                        col + i,
                        row,
                        Cell {
                            value: '\0',
                            fg_color: INVISIBLE,
                            bg_color: INVISIBLE,
                            width: 0,
                        },
                    );
                }
                self.damage.mark_row(row);

                if col + width >= self.size.cols {
                    self.grid[row as usize].wrapped = true;
                    self.position((0, row + 1));
                } else {
                    self.cursor.col += width;
                }
            }
        }

        if self.cursor.col >= self.size.cols {
            self.cursor.row += 1;
            self.cursor.col = 0;
        }

        if self.cursor.row >= self.size.rows {
            self.scroll_up();
            self.cursor.col = 0;
            self.cursor.row = self.size.rows - 1;
        }

        self.damage.mark_cursor();
    }

    /// Set the cursor position, mapping the reserved row 0 to the first content
    /// row and scrolling when the target overflows the grid.
    fn position(&mut self, pos: (u16, u16)) {
        if pos.1 == 0 {
            self.cursor.col = pos.0;
            self.cursor.row = 1;
        } else {
            self.cursor.col = pos.0;
            self.cursor.row = pos.1;
        }

        while self.cursor.row >= self.size.rows {
            self.cursor.row -= 1;
            self.scroll_up();
        }

        self.damage.mark_cursor();
    }

    fn scroll_up(&mut self) {
        let rows = self.size.rows as usize;
        if rows <= 1 {
            return;
        }

        // The topmost content row (index 1) scrolls off into scrollback.
        let scrolled = self.grid[1].clone();
        self.scrollback.push_back(scrolled);
        while self.scrollback.len() > self.max_scrollback_rows {
            self.scrollback.pop_front();
        }

        for r in 1..rows - 1 {
            let next = self.grid[r + 1].clone();
            self.grid[r] = next;
        }
        self.grid[rows - 1] = Row::blank(self.size.cols, &self.color);

        self.damage.mark_full();
    }

    fn handle_tab(&mut self) {
        if self.cursor.col + TAB_SPACES >= self.size.cols {
            self.position((0, self.cursor.row + 1));
        } else {
            self.position((
                ((self.cursor.col + TAB_SPACES) / TAB_SPACES) * TAB_SPACES,
                self.cursor.row,
            ));
        }
    }

    fn fill_cells<F>(&mut self, keep: F, value: char)
    where
        F: Fn(usize) -> bool,
    {
        let cols = self.size.cols as usize;
        for r in 0..self.grid.len() {
            for c in 0..cols {
                if keep(r * cols + c) {
                    self.grid[r].cells[c] = Cell::with(value, &self.color);
                }
            }
        }
    }

    fn clear_screen(&mut self) {
        self.fill_cells(|_| true, '\0');
        self.damage.mark_full();
    }

    fn clear_screen_to_cursor(&mut self) {
        let limit = self.cursor.row as usize * self.size.cols as usize + self.cursor.col as usize;
        self.fill_cells(|i| i < limit, '\0');
        self.damage.mark_range(1, self.cursor.row);
    }

    fn clear_screen_from_cursor(&mut self) {
        let start = self.cursor.row as usize * self.size.cols as usize + self.cursor.col as usize;
        self.fill_cells(|i| i >= start, '\0');
        self.damage.mark_range(self.cursor.row, self.size.rows.saturating_sub(1));
    }

    fn clear_line_range(&mut self, from: u16, to: u16, value: char) {
        let row = self.cursor.row;
        if (row as usize) >= self.grid.len() {
            return;
        }
        for c in from..to {
            if c >= self.size.cols {
                break;
            }
            self.grid[row as usize].cells[c as usize] = Cell::with(value, &self.color);
        }
        self.damage.mark_row(row);
    }

    fn clear_line(&mut self) {
        // Whole-line clear historically used a space glyph, not the null spacer.
        self.clear_line_range(0, self.size.cols, ' ');
    }

    fn clear_line_to_cursor(&mut self) {
        let col = self.cursor.col;
        self.clear_line_range(0, col, '\0');
    }

    fn clear_line_from_cursor(&mut self) {
        let col = self.cursor.col;
        self.clear_line_range(col, self.size.cols, '\0');
    }

    fn handle_ansi_color(&mut self, params: &Params) {
        let color = &mut self.color;
        let mut iter = params.iter();
        while let Some(param) = iter.next() {
            let code = param[0];

            match code {
                0..=29 => {
                    Self::handle_ansi_graphic_rendition(color, code);
                }
                30..=39 => {
                    if let Some(col) = ansi_color(code - 30, &mut iter) {
                        color.fg_base_color = col;
                        color.fg_bright = false;
                    }
                }
                40..=49 => {
                    if let Some(col) = ansi_color(code - 40, &mut iter) {
                        color.bg_base_color = col;
                        color.bg_bright = false;
                    }
                }
                90..=97 => {
                    if let Some(col) = ansi_color(code - 90, &mut iter) {
                        color.fg_base_color = col;
                        color.fg_bright = true;
                    }
                }
                100..=107 => {
                    if let Some(col) = ansi_color(code - 100, &mut iter) {
                        color.bg_base_color = col;
                        color.bg_bright = true;
                    }
                }
                _ => {}
            }
        }

        let mut fg_self = color.fg_base_color;
        let mut bg_self = color.bg_base_color;

        if color.invert {
            core::mem::swap(&mut fg_self, &mut bg_self);
        }

        if color.bright || color.fg_bright {
            fg_self = fg_self.bright();
        }

        if color.dim {
            fg_self = fg_self.dim();
        }

        if color.bg_bright {
            bg_self = bg_self.bright();
        }

        color.fg_color = fg_self;
        color.bg_color = bg_self;
    }

    fn handle_ansi_graphic_rendition(color: &mut ColorState, code: u16) {
        match code {
            0 => {
                color.fg_base_color = color::WHITE;
                color.bg_base_color = color::BLACK;
                color.fg_color = color::WHITE;
                color.bg_color = color::BLACK;
                color.fg_bright = false;
                color.bg_bright = false;
                color.invert = false;
                color.bright = false;
                color.dim = false;
            }
            1 => color.bright = true,
            2 => color.dim = true,
            7 => color.invert = true,
            22 => {
                color.bright = false;
                color.dim = false;
            }
            27 => color.invert = false,
            _ => {}
        }
    }

    fn handle_ansi_cursor_sequence(&mut self, code: u8, params: &Params) {
        let mut iter = params.iter();
        match code {
            0x41 => {
                // Cursor up
                if let Some(p) = iter.next() {
                    let y_move = p[0];
                    let row = self.cursor.row - if y_move == 0 { 1 } else { y_move };
                    self.position((self.cursor.col, if row > 0 { row } else { 0 }));
                }
            }
            0x42 => {
                // Cursor down
                if let Some(p) = iter.next() {
                    let y_move = p[0];
                    let row = self.cursor.row + if y_move == 0 { 1 } else { y_move };
                    self.position((
                        self.cursor.col,
                        if row < self.size.rows { row } else { self.size.rows - 1 },
                    ));
                }
            }
            0x43 => {
                // Cursor right
                if let Some(p) = iter.next() {
                    let x_move = p[0];
                    let column = self.cursor.col + if x_move == 0 { 1 } else { x_move };
                    self.position((
                        if column < self.size.cols { column } else { self.size.cols - 1 },
                        self.cursor.row,
                    ));
                }
            }
            0x44 => {
                // Cursor left
                if let Some(p) = iter.next() {
                    let x_move = p[0];
                    let column = self.cursor.col - if x_move == 0 { 1 } else { x_move };
                    self.position((if column > 0 { column } else { 0 }, self.cursor.row));
                }
            }
            0x45 => {
                // Cursor to start of next line(s)
                if let Some(p) = iter.next() {
                    let row = self.cursor.row + p[0] + 1;
                    self.position((0, if row < self.size.rows { row } else { self.size.rows - 1 }));
                }
            }
            0x46 => {
                // Cursor to start of previous line(s)
                if let Some(p) = iter.next() {
                    let row = self.cursor.row - p[0] - 1;
                    self.position((0, if row > 0 { row } else { 0 }));
                }
            }
            0x47 => {
                // Cursor to column
                if let Some(p) = iter.next() {
                    let column = p[0];
                    self.position((
                        if column < self.size.cols { column } else { self.size.cols - 1 },
                        self.cursor.row,
                    ));
                }
            }
            0x48 | 0x66 => {
                // Set cursor position. Historically param 1 is column, param 2 row.
                let param1 = iter.next();
                let param2 = iter.next();

                if let Some(p1) = param1
                    && let Some(p2) = param2
                {
                    let column = p1[0];
                    let row = p2[0];
                    self.position((
                        if column > self.size.cols { self.size.cols - 1 } else { column },
                        if row > self.size.rows { self.size.rows - 1 } else { row },
                    ));
                } else {
                    self.position((0, 0));
                }
            }
            0x73 => {
                // Save cursor position
                self.cursor.saved_col = self.cursor.col;
                self.cursor.saved_row = self.cursor.row;
            }
            0x75 => {
                // Restore cursor position
                self.position((self.cursor.saved_col, self.cursor.saved_row));
            }
            _ => {}
        }
    }

    fn handle_ansi_erase_sequence(&mut self, code: u8, params: &Params) {
        let mut iter = params.iter();
        let erase_code = iter.next().map_or(0, |p| p[0]);

        match code {
            0x4a => match erase_code {
                0 => self.clear_screen_from_cursor(),
                1 => self.clear_screen_to_cursor(),
                2 => {
                    self.clear_screen();
                    self.position((0, 0));
                }
                _ => {}
            },
            0x4b => match erase_code {
                0 => self.clear_line_from_cursor(),
                1 => self.clear_line_to_cursor(),
                2 => self.clear_line(),
                _ => {}
            },
            _ => {}
        }
    }
}

impl Perform for TerminalModel {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => {} // bell: no-op (speaker access disabled)
            0x09 => self.handle_tab(),
            0x0a => self.print_char('\n'),
            _ => {}
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: u8) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: u8) {
        match action {
            0x41..=0x48 | 0x66 | 0x6e | 0x73 | 0x75 => {
                self.handle_ansi_cursor_sequence(action, params)
            }
            0x4a | 0x4b => self.handle_ansi_erase_sequence(action, params),
            0x6d => self.handle_ansi_color(params),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

/// Number of columns a glyph occupies, matching `graphic::lfb::draw_char`
/// (`0` for glyphs Unifont cannot render, which are not printed).
fn char_columns(c: char) -> u16 {
    match unifont::get_glyph(c) {
        Some(glyph) => {
            let width = glyph.get_width() as u32;
            if width == 0 {
                return 0;
            }
            (width / lfb::DEFAULT_CHAR_WIDTH
                + if width % lfb::DEFAULT_CHAR_WIDTH == 0 { 0 } else { 1 }) as u16
        }
        None => 0,
    }
}

fn ansi_color(code: u16, iter: &mut ParamsIter) -> Option<Color> {
    match code {
        0 => Some(color::BLACK),
        1 => Some(color::RED),
        2 => Some(color::GREEN),
        3 => Some(color::YELLOW),
        4 => Some(color::BLUE),
        5 => Some(color::MAGENTA),
        6 => Some(color::CYAN),
        7 | 9 => Some(color::WHITE),
        8 => parse_complex_color(iter),
        _ => None,
    }
}

fn parse_complex_color(iter: &mut ParamsIter) -> Option<Color> {
    let mode = iter.next()?[0];

    match mode {
        2 => {
            let red = iter.next()?[0] as u8;
            let green = iter.next()?[0] as u8;
            let blue = iter.next()?[0] as u8;
            Some(Color {
                red,
                green,
                blue,
                alpha: 255,
            })
        }
        5 => {
            let index = iter.next()?[0] as usize;
            if index <= 255 {
                Some(COLOR_TABLE_256[index])
            } else {
                None
            }
        }
        _ => None,
    }
}
