//! A minimal terminal screen model — enough of a VT/ANSI emulator to *replay*
//! captured output (e.g. Claude's) onto a virtual grid and read back the
//! rendered screen, **with styling**. Each cell stores its character *and* its
//! SGR attributes (color, bold, …) so the composited output keeps the program's
//! colors verbatim — only equations are re-typeset.
//!
//! Two purposes:
//!  1. **Instrumentation.** `termtex replay <capture>` renders a captured byte
//!     stream so we can see what a program actually paints.
//!  2. **Foundation.** Reconstructing the styled screen is what an in-place
//!     inline renderer needs — detect the equation on the assembled grid and
//!     keep everything else pixel-identical.
//!
//! Partial by design: it handles the sequences Claude emits (cursor moves,
//! column-absolute `[NG`, erases, scroll region, line feeds, SGR colors) and
//! answers the queries a program blocks on; other escapes are ignored.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
        }
    }
}

impl Style {
    /// The SGR sequence (reset-based) that reproduces this style.
    fn sgr(&self) -> String {
        let mut p: Vec<String> = vec!["0".into()];
        if self.bold {
            p.push("1".into());
        }
        if self.dim {
            p.push("2".into());
        }
        if self.italic {
            p.push("3".into());
        }
        if self.underline {
            p.push("4".into());
        }
        if self.reverse {
            p.push("7".into());
        }
        match self.fg {
            Color::Default => {}
            Color::Idx(n) => p.push(format!("38;5;{n}")),
            Color::Rgb(r, g, b) => p.push(format!("38;2;{r};{g};{b}")),
        }
        match self.bg {
            Color::Default => {}
            Color::Idx(n) => p.push(format!("48;5;{n}")),
            Color::Rgb(r, g, b) => p.push(format!("48;2;{r};{g};{b}")),
        }
        format!("\x1b[{}m", p.join(";"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    style: Style,
}

impl Cell {
    fn blank() -> Self {
        Cell {
            ch: ' ',
            style: Style::default(),
        }
    }
    fn is_blank(&self) -> bool {
        self.ch == ' ' && self.style == Style::default()
    }
}

pub struct Grid {
    rows: usize,
    cols: usize,
    cells: Vec<Vec<Cell>>,
    row: usize,
    col: usize,
    cur: Style,
    saved: (usize, usize),
    top: usize,
    bot: usize,
    state: State,
    params: String,
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
            cells: vec![vec![Cell::blank(); cols]; rows],
            row: 0,
            col: 0,
            cur: Style::default(),
            saved: (0, 0),
            top: 0,
            bot: rows.saturating_sub(1),
            state: State::Ground,
            params: String::new(),
            responses: Vec::new(),
        }
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    pub fn feed(&mut self, bytes: &[u8]) {
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
            '\t' => self.col = (((self.col / 8) + 1) * 8).min(self.cols.saturating_sub(1)),
            c if (c as u32) < 0x20 => {}
            c => {
                if self.col >= self.cols {
                    self.col = 0;
                    self.line_feed();
                }
                if self.row < self.rows && self.col < self.cols {
                    self.cells[self.row][self.col] = Cell {
                        ch: c,
                        style: self.cur,
                    };
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
                if self.row == self.top {
                    self.scroll_down();
                } else {
                    self.row = self.row.saturating_sub(1);
                }
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi(&mut self, ch: char) {
        if ('\u{20}'..='\u{3f}').contains(&ch) {
            self.params.push(ch);
            return;
        }
        let p = std::mem::take(&mut self.params);
        self.state = State::Ground;

        if ch == 'm' {
            self.apply_sgr(&p);
            return;
        }
        // Answer queries a program may block on.
        match ch {
            'n' if p == "6" => {
                let r = format!("\x1b[{};{}R", self.row + 1, self.col + 1);
                self.responses.extend_from_slice(r.as_bytes());
            }
            'c' if !p.starts_with('>') && !p.starts_with('=') => {
                self.responses.extend_from_slice(b"\x1b[?6c");
            }
            'p' if p.contains('$') => {
                let mode: String = p.chars().filter(|c| c.is_ascii_digit()).collect();
                let r = format!("\x1b[?{mode};0$y");
                self.responses.extend_from_slice(r.as_bytes());
            }
            'u' if p == "?" => self.responses.extend_from_slice(b"\x1b[?0u"),
            'q' if p.starts_with('>') => {
                self.responses.extend_from_slice(b"\x1bP>|termtex\x1b\\")
            }
            _ => {}
        }

        let private = p.starts_with('?');
        let nums: Vec<usize> = p
            .trim_start_matches('?')
            .split(';')
            .map(|s| s.parse().unwrap_or(0))
            .collect();
        let n = |i: usize, default: usize| nums.get(i).copied().filter(|&v| v != 0).unwrap_or(default);
        if private {
            return;
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
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &str) {
        let codes: Vec<i64> = if params.is_empty() {
            vec![0]
        } else {
            params.split(';').map(|s| s.parse().unwrap_or(0)).collect()
        };
        let mut i = 0;
        while i < codes.len() {
            match codes[i] {
                0 => self.cur = Style::default(),
                1 => self.cur.bold = true,
                2 => self.cur.dim = true,
                3 => self.cur.italic = true,
                4 => self.cur.underline = true,
                7 => self.cur.reverse = true,
                22 => {
                    self.cur.bold = false;
                    self.cur.dim = false;
                }
                23 => self.cur.italic = false,
                24 => self.cur.underline = false,
                27 => self.cur.reverse = false,
                30..=37 => self.cur.fg = Color::Idx((codes[i] - 30) as u8),
                39 => self.cur.fg = Color::Default,
                40..=47 => self.cur.bg = Color::Idx((codes[i] - 40) as u8),
                49 => self.cur.bg = Color::Default,
                90..=97 => self.cur.fg = Color::Idx((codes[i] - 90 + 8) as u8),
                100..=107 => self.cur.bg = Color::Idx((codes[i] - 100 + 8) as u8),
                38 | 48 => {
                    let is_fg = codes[i] == 38;
                    let c = match codes.get(i + 1) {
                        Some(5) => {
                            i += 2;
                            Color::Idx(*codes.get(i).unwrap_or(&0) as u8)
                        }
                        Some(2) => {
                            let r = *codes.get(i + 2).unwrap_or(&0) as u8;
                            let g = *codes.get(i + 3).unwrap_or(&0) as u8;
                            let b = *codes.get(i + 4).unwrap_or(&0) as u8;
                            i += 4;
                            Color::Rgb(r, g, b)
                        }
                        _ => Color::Default,
                    };
                    if is_fg {
                        self.cur.fg = c;
                    } else {
                        self.cur.bg = c;
                    }
                }
                _ => {}
            }
            i += 1;
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
        for r in self.top..self.bot {
            self.cells[r] = self.cells[r + 1].clone();
        }
        self.cells[self.bot] = vec![Cell::blank(); self.cols];
    }

    fn scroll_down(&mut self) {
        for r in (self.top + 1..=self.bot).rev() {
            self.cells[r] = self.cells[r - 1].clone();
        }
        self.cells[self.top] = vec![Cell::blank(); self.cols];
    }

    fn insert_lines(&mut self, n: usize) {
        if self.row < self.top || self.row > self.bot {
            return;
        }
        for _ in 0..n {
            for r in (self.row + 1..=self.bot).rev() {
                self.cells[r] = self.cells[r - 1].clone();
            }
            self.cells[self.row] = vec![Cell::blank(); self.cols];
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
            self.cells[self.bot] = vec![Cell::blank(); self.cols];
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
            self.cells[self.row][c] = Cell::blank();
        }
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            2 | 3 => {
                for r in 0..self.rows {
                    self.cells[r] = vec![Cell::blank(); self.cols];
                }
            }
            1 => {
                for r in 0..self.row {
                    self.cells[r] = vec![Cell::blank(); self.cols];
                }
                self.erase_line(1);
            }
            _ => {
                self.erase_line(0);
                for r in self.row + 1..self.rows {
                    self.cells[r] = vec![Cell::blank(); self.cols];
                }
            }
        }
    }

    /// One row as a styled string (SGR escapes + glyphs), trailing blanks
    /// dropped. Adjacent cells with the same style share one SGR sequence.
    fn styled_row(&self, row: &[Cell]) -> String {
        let last = row
            .iter()
            .rposition(|c| !c.is_blank())
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut s = String::new();
        let mut cur = Style::default();
        for cell in &row[..last] {
            if cell.style != cur {
                s.push_str(&cell.style.sgr());
                cur = cell.style;
            }
            s.push(cell.ch);
        }
        if cur != Style::default() {
            s.push_str("\x1b[0m");
        }
        s
    }

    /// All rows as styled strings (indices line up with the source screen) —
    /// for compositing.
    pub fn rows(&self) -> Vec<String> {
        self.cells.iter().map(|r| self.styled_row(r)).collect()
    }

    /// Styled rows with trailing blank rows trimmed — for display/inspection.
    pub fn render(&self) -> Vec<String> {
        let mut out = self.rows();
        while out.last().map(|l| l.is_empty()).unwrap_or(false) {
            out.pop();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip SGR for plain-text assertions.
    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn plain_text_and_newlines() {
        let mut g = Grid::new(10, 20);
        g.feed(b"hello\r\nworld");
        let r: Vec<String> = g.render().iter().map(|l| plain(l)).collect();
        assert_eq!(r, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn column_absolute_places_tokens() {
        let mut g = Grid::new(4, 40);
        g.feed(b"\x1b[1;1Hmu\x1b[1;7Hnabla\x1b[1;16Hv");
        assert_eq!(plain(&g.render()[0]), "mu    nabla    v");
    }

    #[test]
    fn erase_line_and_repaint() {
        let mut g = Grid::new(4, 20);
        g.feed(b"first draft\r\x1b[Kfinal");
        assert_eq!(plain(&g.render()[0]), "final");
    }

    #[test]
    fn scroll_on_overflow() {
        let mut g = Grid::new(2, 10);
        g.feed(b"a\r\nb\r\nc");
        let r: Vec<String> = g.render().iter().map(|l| plain(l)).collect();
        assert_eq!(r, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn color_is_preserved_in_styled_output() {
        let mut g = Grid::new(2, 20);
        g.feed(b"\x1b[31mred\x1b[0m plain");
        let line = &g.render()[0];
        assert!(line.contains("\x1b["), "carries SGR: {line:?}");
        assert!(line.contains("38;5;1") || line.contains("[31"), "red fg encoded: {line:?}");
        assert_eq!(plain(line), "red plain");
    }
}
