//! Heuristic detection of *bare* display LaTeX — equations written with no
//! delimiters at all, as Claude Code and many LLMs emit them:
//!
//! ```text
//! \rho \left( \frac{\partial \mathbf{u}}{\partial t} \right) = -\nabla p + \mu \nabla^2 \mathbf{u}
//! ```
//!
//! This is opt-in (`--detect-bare`) and best-effort. Two design rules keep it
//! safe:
//!
//! 1. **Never hold the byte stream.** Every input byte is emitted immediately;
//!    detection only *appends* a rendered image after an equation's text. Holding
//!    lines to replace them would stall interactive output and freeze a child's
//!    live TUI (spinner, redraws). So bare math augments (text + image); only
//!    confident delimited math (`$$`) replaces.
//! 2. **Conservative classifier.** A line is treated as a bare equation only when
//!    it has multiple LaTeX commands *and* a math construct (`^`/`_`/`=` or a
//!    known math macro) *and* is not prose. This rejects file paths, code, and
//!    English sentences with the odd backslash.
//!
//! Consecutive equation lines (Claude wraps long equations across terminal
//! lines) are joined into one block and rendered once.

/// What the detector wants the caller to do, in stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BareEvent {
    /// Emit these bytes verbatim (also feed them to the delimiter scanner).
    Pass(Vec<u8>),
    /// Render this joined LaTeX as a display image and emit it here.
    Math(String),
}

/// Streaming bare-LaTeX detector. Feed it the same bytes as the delimiter
/// scanner; it returns [`BareEvent`]s preserving order.
pub struct BareDetector {
    /// Raw bytes accumulated this feed, flushed as `Pass` events.
    pass_buf: Vec<u8>,
    /// Offset in `pass_buf` where the current (in-progress) line begins.
    line_start: usize,
    /// ANSI-stripped text of the current line, for classification.
    clean_line: String,
    /// Whether we are skipping bytes inside an ANSI escape sequence.
    in_escape: EscapeState,
    /// Saw a CR; deciding if it is a "\r\n" line end or a lone-CR line rewrite.
    pending_cr: bool,
    /// Clean text of consecutive equation lines awaiting a flush.
    pending: Vec<String>,
    /// Cap on a joined equation to avoid pathological growth.
    max_bytes: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EscapeState {
    None,
    /// Saw ESC; awaiting the sequence introducer.
    Esc,
    /// Inside a CSI (`ESC [`) sequence; ends on a byte in 0x40..=0x7e.
    Csi,
    /// Inside an OSC (`ESC ]`) sequence; ends on BEL or ST (`ESC \`).
    Osc,
}

impl BareDetector {
    pub fn new(max_bytes: usize) -> Self {
        BareDetector {
            pass_buf: Vec::new(),
            line_start: 0,
            clean_line: String::new(),
            in_escape: EscapeState::None,
            pending: Vec::new(),
            max_bytes,
            pending_cr: false,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<BareEvent> {
        let mut events = Vec::new();
        for &b in chunk {
            self.pass_buf.push(b);
            self.track_clean(b);
            if b == b'\n' {
                self.line_complete(&mut events);
                self.line_start = self.pass_buf.len();
                self.clean_line.clear();
            }
        }
        // Flush this feed's raw bytes; nothing is ever withheld.
        if !self.pass_buf.is_empty() {
            events.push(BareEvent::Pass(std::mem::take(&mut self.pass_buf)));
        }
        self.line_start = 0;
        events
    }

    /// At EOF, classify any trailing partial line and flush pending math.
    pub fn finish(&mut self) -> Vec<BareEvent> {
        let mut events = Vec::new();
        if !self.pass_buf.is_empty() {
            events.push(BareEvent::Pass(std::mem::take(&mut self.pass_buf)));
        }
        let trimmed = self.clean_line.trim();
        if !trimmed.is_empty() && looks_like_bare_math(trimmed) {
            self.pending.push(trimmed.to_string());
        }
        self.flush_pending(&mut events);
        self.clean_line.clear();
        self.line_start = 0;
        events
    }

    /// Update the clean-text accumulator, skipping ANSI escapes and resetting on
    /// a carriage return (a redraw of the current line).
    fn track_clean(&mut self, b: u8) {
        match self.in_escape {
            EscapeState::None => {
                // Resolve a pending CR: "\r\n" is a line end (keep the line for
                // classification); a lone "\r" is a TUI line rewrite (reset).
                // A PTY's ONLCR makes every newline "\r\n", so we must not reset
                // on the CR alone.
                if self.pending_cr {
                    self.pending_cr = false;
                    if b != b'\n' {
                        self.clean_line.clear();
                    }
                }
                match b {
                    0x1b => self.in_escape = EscapeState::Esc,
                    b'\r' => self.pending_cr = true,
                    b'\n' => {}
                    // Keep printable bytes only. C0 control chars (backspaces,
                    // EOT, etc.) are not part of the LaTeX and would break
                    // parsing if they leaked into a detected equation.
                    0x20..=0x7e => self.clean_line.push(b as char),
                    b if b >= 0x80 => self.clean_line.push(b as char),
                    _ => {}
                }
            }
            EscapeState::Esc => {
                self.in_escape = match b {
                    b'[' => EscapeState::Csi,
                    b']' => EscapeState::Osc,
                    _ => EscapeState::None, // simple two-byte escape; consumed
                };
            }
            EscapeState::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    self.in_escape = EscapeState::None;
                }
            }
            EscapeState::Osc => {
                // OSC ends on BEL; ST (ESC \) is handled by the ESC path.
                if b == 0x07 || b == 0x1b {
                    self.in_escape = if b == 0x1b {
                        EscapeState::Esc
                    } else {
                        EscapeState::None
                    };
                }
            }
        }
    }

    fn line_complete(&mut self, events: &mut Vec<BareEvent>) {
        let trimmed = self.clean_line.trim();
        if !trimmed.is_empty() && looks_like_bare_math(trimmed) {
            self.pending.push(trimmed.to_string());
        } else {
            // A non-math (or blank) line ends any equation run. Insert the image
            // before this line's bytes: flush everything up to line_start, emit
            // the joined Math, then keep the rest of pass_buf.
            if !self.pending.is_empty() {
                let head: Vec<u8> = self.pass_buf.drain(..self.line_start).collect();
                if !head.is_empty() {
                    events.push(BareEvent::Pass(head));
                }
                self.line_start = 0;
                self.flush_pending(events);
            }
        }
    }

    fn flush_pending(&mut self, events: &mut Vec<BareEvent>) {
        if self.pending.is_empty() {
            return;
        }
        let joined = self.pending.join(" ");
        self.pending.clear();
        if joined.len() <= self.max_bytes {
            events.push(BareEvent::Math(joined));
        }
        // If oversized, drop the math event; the source text already passed
        // through, so nothing is lost.
    }
}

/// Known math macros that signal an equation even without `^`/`_`/`=`.
const MATH_MACROS: &[&str] = &[
    "frac", "sqrt", "sum", "int", "prod", "lim", "nabla", "partial", "infty", "alpha", "beta",
    "gamma", "delta", "epsilon", "varepsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda",
    "mu", "nu", "xi", "pi", "rho", "sigma", "tau", "phi", "varphi", "chi", "psi", "omega", "pm",
    "mp", "cdot", "times", "div", "leq", "geq", "neq", "approx", "equiv", "propto", "mathbf",
    "mathrm", "mathbb", "mathcal", "boldsymbol", "hat", "vec", "bar", "dot", "ddot", "tilde",
    "overline", "underline", "langle", "rangle", "otimes", "oplus", "forall", "exists", "subset",
    "subseteq", "supset", "cup", "cap", "to", "rightarrow", "leftarrow", "mapsto", "binom",
    "begin", "end", "left", "right", "sin", "cos", "tan", "log", "ln", "exp", "det", "operatorname",
];

/// Conservatively decide whether an ANSI-stripped, trimmed line is a bare
/// display equation. See module docs for the rationale.
pub fn looks_like_bare_math(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }

    // Count LaTeX command tokens (`\` + letters) and whether any is a math macro.
    let mut commands = 0usize;
    let mut has_math_macro = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                j += 1;
            }
            if j > start {
                commands += 1;
                let name = &line[start..j];
                if MATH_MACROS.contains(&name) {
                    has_math_macro = true;
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }

    if commands < 2 {
        return false;
    }

    // Require a math construct so command-heavy prose (\textbf, \emph) and file
    // paths (\Users\Foo) don't qualify.
    let has_op = line.contains('^') || line.contains('_') || line.contains('=');
    if !has_op && !has_math_macro {
        return false;
    }

    // Reject prose: three or more consecutive plain alphabetic words.
    if max_consecutive_prose_words(line) >= 3 {
        return false;
    }

    true
}

/// Longest run of consecutive whitespace-separated tokens that are plain
/// alphabetic words (length >= 2, no LaTeX/symbols). A long run means prose.
fn max_consecutive_prose_words(line: &str) -> usize {
    let mut max = 0usize;
    let mut run = 0usize;
    for tok in line.split_whitespace() {
        let is_word = tok.len() >= 2
            && tok.chars().all(|c| c.is_ascii_alphabetic())
            && !MATH_MACROS.contains(&tok);
        if is_word {
            run += 1;
            max = max.max(run);
        } else {
            run = 0;
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- classifier: positives -------------------------------------------

    #[test]
    fn detects_claude_navier_stokes_lines() {
        assert!(looks_like_bare_math(
            "\\rho \\left( \\frac{\\partial \\mathbf{u}}{\\partial t} + \\mathbf{u} \\cdot \\nabla"
        ));
        assert!(looks_like_bare_math(
            "\\mathbf{u} \\right) = -\\nabla p + \\mu \\nabla^2 \\mathbf{u} + \\mathbf{f}"
        ));
        assert!(looks_like_bare_math("\\nabla \\cdot \\mathbf{u} = 0"));
    }

    #[test]
    fn detects_common_bare_equations() {
        assert!(looks_like_bare_math(
            "\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}"
        ));
        assert!(looks_like_bare_math(
            "\\int_0^\\infty e^{-x^2}\\,dx = \\frac{\\sqrt{\\pi}}{2}"
        ));
    }

    // --- classifier: negatives (must not false-positive) -----------------

    #[test]
    fn rejects_prose() {
        assert!(!looks_like_bare_math(
            "The incompressible Navier-Stokes momentum equation:"
        ));
        assert!(!looks_like_bare_math(
            "Paired with the incompressibility constraint:"
        ));
        assert!(!looks_like_bare_math("Where:"));
        assert!(!looks_like_bare_math("Want me to drop one of these into a file?"));
    }

    #[test]
    fn rejects_command_heavy_prose_without_math() {
        // Two LaTeX commands but no math construct -> prose/markup, not an equation.
        assert!(!looks_like_bare_math(
            "Use \\textbf{bold} and \\emph{italic} for emphasis here"
        ));
    }

    #[test]
    fn rejects_file_paths_and_code() {
        assert!(!looks_like_bare_math("C:\\Users\\Foo\\Bar"));
        assert!(!looks_like_bare_math("let x = 5; // assign"));
        assert!(!looks_like_bare_math("if a == b { return c; }"));
        assert!(!looks_like_bare_math("run cargo build to compile the crate"));
    }

    #[test]
    fn rejects_inline_math_inside_prose() {
        // A bullet describing a symbol: inline math handled separately, the whole
        // line is not a display equation.
        assert!(!looks_like_bare_math("- $\\mathbf{u}$ is the velocity field"));
    }

    #[test]
    fn rejects_bare_arithmetic_without_commands() {
        // No LaTeX commands -> indistinguishable from an assignment; left alone.
        assert!(!looks_like_bare_math("x = 5"));
        assert!(!looks_like_bare_math("total = price + tax"));
    }

    // --- streaming behavior ----------------------------------------------

    /// Collect events from feeding `input` in one chunk + finish.
    fn run(input: &[u8]) -> Vec<BareEvent> {
        let mut d = BareDetector::new(4096);
        let mut ev = d.feed(input);
        ev.extend(d.finish());
        ev
    }

    fn passthrough(ev: &[BareEvent]) -> Vec<u8> {
        let mut v = Vec::new();
        for e in ev {
            if let BareEvent::Pass(b) = e {
                v.extend_from_slice(b);
            }
        }
        v
    }

    fn math(ev: &[BareEvent]) -> Vec<String> {
        ev.iter()
            .filter_map(|e| match e {
                BareEvent::Math(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn all_bytes_pass_through_unchanged() {
        // Augment, never replace: the source text must always survive verbatim.
        let input = b"intro:\n\\nabla \\cdot \\mathbf{u} = 0\ndone\n";
        let ev = run(input);
        assert_eq!(passthrough(&ev), input, "every byte passes through");
    }

    #[test]
    fn renders_a_bare_equation_block() {
        let input = b"The constraint:\n\\nabla \\cdot \\mathbf{u} = 0\nThat is it.\n";
        let ev = run(input);
        assert_eq!(math(&ev), vec!["\\nabla \\cdot \\mathbf{u} = 0".to_string()]);
    }

    #[test]
    fn joins_wrapped_equation_lines() {
        // Claude wraps one equation across two terminal lines; they should join.
        let input =
            b"eq:\n\\rho \\frac{\\partial \\mathbf{u}}{\\partial t} = -\\nabla p\n+ \\mu \\nabla^2 \\mathbf{u} + \\mathbf{f}\n\nnext\n";
        let ev = run(input);
        let m = math(&ev);
        assert_eq!(m.len(), 1, "wrapped lines join into one equation");
        assert!(m[0].contains("\\rho") && m[0].contains("\\mathbf{f}"));
    }

    #[test]
    fn separate_equations_are_distinct() {
        let input = b"\\nabla \\cdot \\mathbf{u} = 0\nprose here now\n\\rho = \\frac{m}{V}\n";
        let ev = run(input);
        assert_eq!(math(&ev).len(), 2, "prose between equations splits them");
    }

    #[test]
    fn image_event_precedes_following_nonmath_text() {
        // The Math event must come before the text of the line that ended the run.
        let input = b"\\nabla \\cdot \\mathbf{u} = 0\nAFTER\n";
        let mut d = BareDetector::new(4096);
        let mut ev = d.feed(input);
        ev.extend(d.finish());
        let math_idx = ev.iter().position(|e| matches!(e, BareEvent::Math(_)));
        assert!(math_idx.is_some());
        // Reconstruct bytes emitted after the Math event; must contain AFTER.
        let after: Vec<u8> = ev[math_idx.unwrap() + 1..]
            .iter()
            .flat_map(|e| match e {
                BareEvent::Pass(b) => b.clone(),
                _ => Vec::new(),
            })
            .collect();
        assert!(
            String::from_utf8_lossy(&after).contains("AFTER"),
            "the following line's text comes after the image"
        );
    }

    #[test]
    fn ansi_color_codes_are_stripped_for_classification() {
        // The same equation wrapped in SGR color codes must still be detected,
        // and every byte (codes included) still passes through.
        let input = b"\x1b[36m\\nabla \\cdot \\mathbf{u} = 0\x1b[0m\nx\n";
        let ev = run(input);
        assert_eq!(passthrough(&ev), input);
        assert_eq!(math(&ev).len(), 1, "ansi-wrapped equation detected");
    }

    #[test]
    fn chunk_boundaries_do_not_change_results() {
        let input =
            b"intro\n\\rho = \\frac{m}{V} \\cdot \\nabla x\nmore prose words here\n\\nabla^2 \\phi = 0\n";
        let whole = run(input);
        for size in 1..=9 {
            let mut d = BareDetector::new(4096);
            let mut ev = Vec::new();
            for piece in input.chunks(size) {
                ev.extend(d.feed(piece));
            }
            ev.extend(d.finish());
            assert_eq!(passthrough(&ev), input, "bytes preserved at chunk={size}");
            assert_eq!(
                math(&ev),
                math(&whole),
                "same equations detected at chunk={size}"
            );
        }
    }

    #[test]
    fn control_characters_are_stripped_from_classification() {
        // Stray control bytes (e.g. an EOT/backspace echo) must not leak into
        // the LaTeX or they would break parsing.
        let input = b"\x04\x08\x08\\nabla \\cdot \\mathbf{u} = 0\nx\n";
        let ev = run(input);
        assert_eq!(passthrough(&ev), input, "control bytes still pass through");
        assert_eq!(
            math(&ev),
            vec!["\\nabla \\cdot \\mathbf{u} = 0".to_string()],
            "detected LaTeX is clean of control chars"
        );
    }

    #[test]
    fn lone_cr_rewrite_resets_the_line() {
        // A TUI redraws a line with a lone CR (no LF): the earlier content is
        // overwritten. Only the final state should be classified.
        let input = b"\\foo partial\r\\nabla \\cdot \\mathbf{u} = 0\nafter\n";
        let ev = run(input);
        assert_eq!(passthrough(&ev), input);
        assert_eq!(
            math(&ev),
            vec!["\\nabla \\cdot \\mathbf{u} = 0".to_string()],
            "lone CR discards the overwritten prefix"
        );
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        // A PTY emits "\r\n"; the CR must not wipe the line before classification.
        let input = b"intro\r\n\\nabla \\cdot \\mathbf{u} = 0\r\nafter\r\n";
        let ev = run(input);
        assert_eq!(passthrough(&ev), input, "every byte (incl CR) passes through");
        assert_eq!(math(&ev), vec!["\\nabla \\cdot \\mathbf{u} = 0".to_string()]);
    }

    #[test]
    fn no_math_means_pure_passthrough() {
        let input = b"just some normal text\nwith two lines\n";
        let ev = run(input);
        assert_eq!(passthrough(&ev), input);
        assert!(math(&ev).is_empty());
    }
}
