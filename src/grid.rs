//! A minimal terminal screen model — enough of a VT/ANSI emulator to *replay*
//! captured output (e.g. Claude's) onto a virtual grid and read back the
//! rendered screen. Two purposes:
//!
//!  1. **Instrumentation.** `termtex replay <capture>` renders a captured byte
//!     stream so we can see what a program actually paints (vs. the raw bytes),
//!     and iterate: observe → hypothesize → change → replay → compare.
//!  2. **Foundation.** Reconstructing the screen is what a true in-place inline
//!     renderer needs — detect the equation on the assembled grid, not in the
//!     shredded stream.
//!
//! It is deliberately partial: it handles the sequences Claude actually emits
//! (cursor moves, column-absolute `[NG`, erases, scroll region, line feeds) and
//! ignores styling/mode-set escapes, which don't affect glyph positions.

pub struct Grid {
    rows: usize,
    cols: usize,
    cells: Vec<Vec<char>>,
    row: usize,
    col: usize,
    saved: (usize, usize),
    // Scroll region (DECSTBM), 0-based inclusive.
    top: usize,
    bot: usize,
    state: State,
    params: String,
    /// Replies owed to the child for terminal queries it sent (DA, DSR, …).
    /// Drained by the driver and written back to the child's input, so a program
    /// that blocks on a query (like Claude at startup) doesn't freeze.
    responses: Vec<u8>,
}

#[derive(PartialEq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
}

impl Grid {
    pub fn new(rows: usize, cols: usize) -> Self {
        Grid {
            rows,
            cols,
            cells: vec![vec![' '; cols]; rows],
            row: 0,
            col: 0,
            saved: (0, 0),
            top: 0,
            bot: rows.saturating_sub(1),
            state: State::Ground,
            params: String::new(),
            responses: Vec::new(),
        }
    }

    /// Take any pending query replies (to write back to the child's input).
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // Treat the stream as UTF-8 for printable glyphs but dispatch control
        // bytes individually. Decode lazily: collect a char from valid UTF-8.
        let text = String::from_utf8_lossy(bytes);
        for ch in text.chars() {
            self.step(ch);
        }
    }

    fn step(&mut self, ch: char) {
        match self.state {
            State::Ground => self.ground(ch),
            State::Esc => self.esc(ch),
            State::Csi => self.csi(ch),
            State::Osc => {
                // Ends on BEL or ST (ESC \). Approximate: end on BEL or ESC.
                if ch == '\u{7}' {
                    self.state = State::Ground;
                } else if ch == '\u{1b}' {
                    self.state = State::Esc;
                }
            }
        }
    }

    fn ground(&mut self, ch: char) {
        match ch {
            '\u{1b}' => self.state = State::Esc,
            '\r' => self.col = 0,
            '\n' => self.line_feed(),
            '\u{8}' => self.col = self.col.saturating_sub(1),
            '\t' => self.col = ((self.col / 8) + 1) * 8,
            c if (c as u32) < 0x20 => {} // other C0 controls: ignore
            c => {
                if self.col >= self.cols {
                    self.col = 0;
                    self.line_feed();
                }
                if self.row < self.rows && self.col < self.cols {
                    self.cells[self.row][self.col] = c;
                }
                self.col += 1;
            }
        }
    }

    fn esc(&mut self, ch: char) {
        match ch {
            '[' => {
                self.params.clear();
                self.state = State::Csi;
            }
            ']' => self.state = State::Osc,
            '7' => {
                self.saved = (self.row, self.col);
                self.state = State::Ground;
            }
            '8' => {
                (self.row, self.col) = self.saved;
                self.state = State::Ground;
            }
            'M' => {
                // Reverse index: move up, scroll down at top.
                if self.row == self.top {
                    self.scroll_down();
                } else {
                    self.row = self.row.saturating_sub(1);
                }
                self.state = State::Ground;
            }
            _ => self.state = State::Ground, // other 2-byte escapes: ignore
        }
    }

    fn csi(&mut self, ch: char) {
        // Collect intermediate/parameter bytes until a final byte (0x40..0x7e).
        if ('\u{20}'..='\u{3f}').contains(&ch) {
            self.params.push(ch);
            return;
        }
        let p = std::mem::take(&mut self.params);
        self.state = State::Ground;
        // Answer the queries a program may block on, so it doesn't hang under us.
        match ch {
            'n' if p == "6" => {
                // Device Status Report → cursor position.
                let r = format!("\x1b[{};{}R", self.row + 1, self.col + 1);
                self.responses.extend_from_slice(r.as_bytes());
            }
            'c' if !p.starts_with('>') && !p.starts_with('=') => {
                // Primary Device Attributes → "VT102".
                self.responses.extend_from_slice(b"\x1b[?6c");
            }
            'p' if p.contains('$') => {
                // DECRQM mode query → report "not recognised" for the asked mode.
                let mode: String = p.chars().filter(|c| c.is_ascii_digit()).collect();
                let r = format!("\x1b[?{mode};0$y");
                self.responses.extend_from_slice(r.as_bytes());
            }
            'u' if p == "?" => {
                // Kitty keyboard flags query → none enabled.
                self.responses.extend_from_slice(b"\x1b[?0u");
            }
            'q' if p.starts_with('>') => {
                // XTVERSION → name ourselves.
                self.responses.extend_from_slice(b"\x1bP>|termtex\x1b\\");
            }
            _ => {}
        }
        let private = p.starts_with('?');
        let nums: Vec<usize> = p
            .trim_start_matches('?')
            .split(';')
            .map(|s| s.parse().unwrap_or(0))
            .collect();
        let n = |i: usize, default: usize| {
            nums.get(i).copied().filter(|&v| v != 0).unwrap_or(default)
        };
        if private {
            return; // DEC private mode sets (cursor visibility, sync update, …)
        }
        match ch {
            'H' | 'f' => {
                self.row = (n(0, 1) - 1).min(self.rows.saturating_sub(1));
                self.col = (n(1, 1) - 1).min(self.cols.saturating_sub(1));
            }
            'A' => self.row = self.row.saturating_sub(n(0, 1)),
            'B' => self.row = (self.row + n(0, 1)).min(self.rows.saturating_sub(1)),
            'C' => self.col = (self.col + n(0, 1)).min(self.cols.saturating_sub(1)),
            'D' => self.col = self.col.saturating_sub(n(0, 1)),
            'G' => self.col = (n(0, 1) - 1).min(self.cols.saturating_sub(1)),
            'd' => self.row = (n(0, 1) - 1).min(self.rows.saturating_sub(1)),
            'E' => {
                self.row = (self.row + n(0, 1)).min(self.rows.saturating_sub(1));
                self.col = 0;
            }
            'K' => self.erase_line(nums.first().copied().unwrap_or(0)),
            'J' => self.erase_display(nums.first().copied().unwrap_or(0)),
            'L' => self.insert_lines(n(0, 1)),
            'M' => self.delete_lines(n(0, 1)),
            'r' => {
                self.top = (n(0, 1) - 1).min(self.rows.saturating_sub(1));
                self.bot = (n(1, self.rows) - 1).min(self.rows.saturating_sub(1));
            }
            _ => {} // SGR ('m'), DSR, etc.: no effect on glyph positions
        }
    }

    fn line_feed(&mut self) {
        if self.row == self.bot {
            self.scroll_up();
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    fn scroll_up(&mut self) {
        // Region [top..=bot] scrolls up by one; bottom line cleared.
        for r in self.top..self.bot {
            self.cells[r] = self.cells[r + 1].clone();
        }
        self.cells[self.bot] = vec![' '; self.cols];
    }

    fn scroll_down(&mut self) {
        for r in (self.top + 1..=self.bot).rev() {
            self.cells[r] = self.cells[r - 1].clone();
        }
        self.cells[self.top] = vec![' '; self.cols];
    }

    fn insert_lines(&mut self, n: usize) {
        if self.row < self.top || self.row > self.bot {
            return;
        }
        for _ in 0..n {
            for r in (self.row + 1..=self.bot).rev() {
                self.cells[r] = self.cells[r - 1].clone();
            }
            self.cells[self.row] = vec![' '; self.cols];
        }
    }

    fn delete_lines(&mut self, n: usize) {
        if self.row < self.top || self.row > self.bot {
            return;
        }
        for _ in 0..n {
            for r in self.row..self.bot {
                self.cells[r] = self.cells[r + 1].clone();
            }
            self.cells[self.bot] = vec![' '; self.cols];
        }
    }

    fn erase_line(&mut self, mode: usize) {
        if self.row >= self.rows {
            return;
        }
        let (a, b) = match mode {
            1 => (0, self.col + 1),
            2 => (0, self.cols),
            _ => (self.col, self.cols),
        };
        for c in a..b.min(self.cols) {
            self.cells[self.row][c] = ' ';
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            2 | 3 => {
                for r in 0..self.rows {
                    self.cells[r] = vec![' '; self.cols];
                }
            }
            1 => {
                for r in 0..self.row {
                    self.cells[r] = vec![' '; self.cols];
                }
                self.erase_line(1);
            }
            _ => {
                self.erase_line(0);
                for r in self.row + 1..self.rows {
                    self.cells[r] = vec![' '; self.cols];
                }
            }
        }
    }

    /// Current cursor position (row, col), 0-based.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// All rows as strings (right-trimmed), without dropping trailing blanks —
    /// for compositing, where row indices must line up with the source screen.
    pub fn rows(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect()
    }

    /// The rendered screen as lines, trailing blank rows and trailing spaces
    /// trimmed.
    pub fn render(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .cells
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect();
        while out.last().map(|l| l.is_empty()).unwrap_or(false) {
            out.pop();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_and_newlines() {
        let mut g = Grid::new(10, 20);
        g.feed(b"hello\r\nworld");
        let r = g.render();
        assert_eq!(r, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn column_absolute_places_tokens() {
        // The cursor-painting pattern Claude uses: write, then jump to a column.
        let mut g = Grid::new(4, 40);
        g.feed(b"\x1b[1;1Hmu\x1b[1;7Hnabla\x1b[1;16Hv");
        let r = g.render();
        assert_eq!(r[0], "mu    nabla    v");
    }

    #[test]
    fn erase_line_and_repaint() {
        let mut g = Grid::new(4, 20);
        g.feed(b"first draft\r\x1b[Kfinal");
        assert_eq!(g.render(), vec!["final".to_string()]);
    }

    #[test]
    fn scroll_on_overflow() {
        let mut g = Grid::new(2, 10);
        g.feed(b"a\r\nb\r\nc");
        assert_eq!(g.render(), vec!["b".to_string(), "c".to_string()]);
    }
}
