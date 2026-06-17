//! Streaming LaTeX delimiter scanner.
//!
//! Output from the child arrives in arbitrary chunks; a single math block may
//! span several reads. This is a byte-oriented state machine that partitions the
//! stream into [`Output::Passthrough`] runs (copied verbatim) and
//! [`Output::Math`] blocks (handed to the renderer).
//!
//! Chunk-boundary safety comes from keeping *every* ambiguous byte in explicit
//! state: a lone trailing `$` or `\` is held in the state enum, and in-progress
//! math content lives in `math_buf`. Nothing is ever "peeked then dropped", so
//! splitting the input at any byte offset yields identical results to feeding it
//! whole. The unit tests below assert exactly that.
//!
//! Detect-only consumers can ignore the parsed `latex`/`display` fields and
//! re-emit [`Output::Math::raw`], which holds the original bytes (delimiters
//! included), making the scanner a lossless passthrough until rendering is wired
//! in. The round-trip property is covered by `reconstruct_*` tests.

/// Default safety valve: an unterminated block longer than this is given up on
/// and flushed verbatim, so a stray `$$` can never swallow the session.
pub const DEFAULT_MAX_MATH_BYTES: usize = 4096;

/// One unit of scanner output, emitted in stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Bytes to copy to the real terminal unchanged.
    Passthrough(Vec<u8>),
    /// A completed math block.
    Math {
        /// Inner LaTeX source, delimiters excluded (lossy UTF-8).
        latex: String,
        /// True for block/display delimiters (`$$`, `\[`), false for inline.
        display: bool,
        /// Exact original bytes including delimiters, for verbatim re-emission.
        raw: Vec<u8>,
    },
}

/// Which delimiter pair opened the current block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    DoubleDollar,
    Bracket,
    SingleDollar,
    Paren,
}

impl Kind {
    fn opener(self) -> &'static [u8] {
        match self {
            Kind::DoubleDollar => b"$$",
            Kind::Bracket => b"\\[",
            Kind::SingleDollar => b"$",
            Kind::Paren => b"\\(",
        }
    }

    fn closer(self) -> &'static [u8] {
        match self {
            Kind::DoubleDollar => b"$$",
            Kind::Bracket => b"\\]",
            Kind::SingleDollar => b"$",
            Kind::Paren => b"\\)",
        }
    }

    fn display(self) -> bool {
        matches!(self, Kind::DoubleDollar | Kind::Bracket)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Outside math, normal passthrough.
    Pass,
    /// Saw a single `$` in passthrough; undecided until the next byte.
    PassDollar,
    /// Saw a single `\` in passthrough; undecided until the next byte.
    PassBackslash,
    /// Inside a math block of the given kind.
    Math(Kind),
    /// Inside a `$$` block, saw one `$`; closes if the next byte is `$`.
    MathDollar(Kind),
    /// Inside a `\[`/`\(` block, saw one `\`; closes if next byte is `]`/`)`.
    MathBackslash(Kind),
}

/// The streaming scanner. Feed it chunks with [`Scanner::feed`] and call
/// [`Scanner::finish`] at EOF to flush any buffered tail.
pub struct Scanner {
    state: State,
    kind: Option<Kind>,
    math_buf: Vec<u8>,
    inline: bool,
    max_math_bytes: usize,
}

impl Scanner {
    /// Block-only scanner (`$$`, `\[`) with the default safety valve.
    pub fn new() -> Self {
        Self::with_config(false, DEFAULT_MAX_MATH_BYTES)
    }

    /// `inline` also recognizes `$...$` and `\(...\)`.
    pub fn with_config(inline: bool, max_math_bytes: usize) -> Self {
        Self {
            state: State::Pass,
            kind: None,
            math_buf: Vec::new(),
            inline,
            max_math_bytes,
        }
    }

    /// Feed one chunk; returns the outputs it completes, in stream order.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Output> {
        let mut pass = Vec::new();
        let mut events = Vec::new();
        for &b in chunk {
            self.step(b, &mut pass, &mut events);
        }
        if !pass.is_empty() {
            events.push(Output::Passthrough(pass));
        }
        events
    }

    /// Flush at EOF. Any unterminated block or held delimiter byte is emitted
    /// verbatim as passthrough.
    pub fn finish(&mut self) -> Vec<Output> {
        let mut pass = Vec::new();
        match self.state {
            State::Pass => {}
            State::PassDollar => pass.push(b'$'),
            State::PassBackslash => pass.push(b'\\'),
            State::Math(_) => self.abort_math(&mut pass),
            State::MathDollar(_) => {
                self.math_buf.push(b'$');
                self.abort_math(&mut pass);
            }
            State::MathBackslash(_) => {
                self.math_buf.push(b'\\');
                self.abort_math(&mut pass);
            }
        }
        self.state = State::Pass;
        if pass.is_empty() {
            Vec::new()
        } else {
            vec![Output::Passthrough(pass)]
        }
    }

    fn step(&mut self, b: u8, pass: &mut Vec<u8>, events: &mut Vec<Output>) {
        match self.state {
            State::Pass => self.step_pass(b, pass),
            State::PassDollar => {
                if b == b'$' {
                    self.open(Kind::DoubleDollar);
                } else if self.inline {
                    // Lone `$` opens inline math; `b` is its first content byte.
                    self.open(Kind::SingleDollar);
                    self.step(b, pass, events);
                } else {
                    pass.push(b'$');
                    self.state = State::Pass;
                    self.step(b, pass, events);
                }
            }
            State::PassBackslash => {
                if b == b'[' {
                    self.open(Kind::Bracket);
                } else if b == b'(' && self.inline {
                    self.open(Kind::Paren);
                } else {
                    pass.push(b'\\');
                    self.state = State::Pass;
                    self.step(b, pass, events);
                }
            }
            State::Math(_) | State::MathDollar(_) | State::MathBackslash(_) => {
                self.step_math(b, pass, events)
            }
        }
    }

    fn step_pass(&mut self, b: u8, pass: &mut Vec<u8>) {
        match b {
            b'$' => self.state = State::PassDollar,
            b'\\' => self.state = State::PassBackslash,
            _ => pass.push(b),
        }
    }

    fn step_math(&mut self, b: u8, pass: &mut Vec<u8>, events: &mut Vec<Output>) {
        // An ESC (start of a terminal control sequence) can never appear inside
        // real LaTeX math. If we see one while buffering a candidate block, the
        // opener (`$`, `\[`, …) was not math after all — abort and reprocess the
        // byte verbatim. This is critical for interactive TUIs: without it, a
        // stray `$` would swallow every following byte up to `max_math_bytes`,
        // including the startup query escapes a TUI blocks on, freezing it.
        if b == 0x1b {
            self.abort_math(pass);
            self.step(b, pass, events);
            return;
        }
        match self.state {
            State::Math(kind) => match kind {
                Kind::SingleDollar => {
                    if b == b'$' {
                        self.close(kind, pass, events);
                    } else {
                        self.push_math(b, pass);
                    }
                }
                Kind::DoubleDollar => {
                    if b == b'$' {
                        self.state = State::MathDollar(kind);
                    } else {
                        self.push_math(b, pass);
                    }
                }
                Kind::Bracket | Kind::Paren => {
                    if b == b'\\' {
                        self.state = State::MathBackslash(kind);
                    } else {
                        self.push_math(b, pass);
                    }
                }
            },
            State::MathDollar(kind) => {
                if b == b'$' {
                    self.close(kind, pass, events);
                } else {
                    // The held `$` was content, not a closer.
                    self.state = State::Math(kind);
                    self.push_math(b'$', pass);
                    self.step(b, pass, events);
                }
            }
            State::MathBackslash(kind) => {
                let closer = if kind == Kind::Bracket { b']' } else { b')' };
                if b == closer {
                    self.close(kind, pass, events);
                } else {
                    // The held `\` was content, not a closer.
                    self.state = State::Math(kind);
                    self.push_math(b'\\', pass);
                    self.step(b, pass, events);
                }
            }
            State::Pass | State::PassDollar | State::PassBackslash => unreachable!(),
        }
    }

    fn open(&mut self, kind: Kind) {
        self.math_buf.clear();
        self.kind = Some(kind);
        self.state = State::Math(kind);
    }

    fn push_math(&mut self, b: u8, pass: &mut Vec<u8>) {
        self.math_buf.push(b);
        if self.math_buf.len() > self.max_math_bytes {
            self.abort_math(pass);
        }
    }

    /// Give up on an unterminated/oversized block: flush opener + buffered
    /// content verbatim and return to passthrough.
    fn abort_math(&mut self, pass: &mut Vec<u8>) {
        if let Some(kind) = self.kind {
            pass.extend_from_slice(kind.opener());
            pass.append(&mut self.math_buf);
        }
        self.math_buf.clear();
        self.kind = None;
        self.state = State::Pass;
    }

    fn close(&mut self, kind: Kind, pass: &mut Vec<u8>, events: &mut Vec<Output>) {
        if !pass.is_empty() {
            events.push(Output::Passthrough(std::mem::take(pass)));
        }
        let latex = String::from_utf8_lossy(&self.math_buf).into_owned();
        let mut raw = kind.opener().to_vec();
        raw.extend_from_slice(&self.math_buf);
        raw.extend_from_slice(kind.closer());
        events.push(Output::Math {
            latex,
            display: kind.display(),
            raw,
        });
        self.math_buf.clear();
        self.kind = None;
        self.state = State::Pass;
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- helpers -----------------------------------------------------------

    /// Feed the whole input as one chunk, then finish.
    fn scan_whole(inline: bool, max: usize, input: &[u8]) -> Vec<Output> {
        let mut s = Scanner::with_config(inline, max);
        let mut out = s.feed(input);
        out.extend(s.finish());
        out
    }

    /// Feed the input split at every `chunk` boundary, then finish.
    fn scan_chunked(inline: bool, max: usize, input: &[u8], chunk: usize) -> Vec<Output> {
        let mut s = Scanner::with_config(inline, max);
        let mut out = Vec::new();
        for piece in input.chunks(chunk.max(1)) {
            out.extend(s.feed(piece));
        }
        out.extend(s.finish());
        out
    }

    /// Concatenate Passthrough bytes + Math.raw bytes — must equal the input.
    fn reconstruct(events: &[Output]) -> Vec<u8> {
        let mut v = Vec::new();
        for e in events {
            match e {
                Output::Passthrough(b) => v.extend_from_slice(b),
                Output::Math { raw, .. } => v.extend_from_slice(raw),
            }
        }
        v
    }

    fn math_blocks(events: &[Output]) -> Vec<(String, bool)> {
        events
            .iter()
            .filter_map(|e| match e {
                Output::Math { latex, display, .. } => Some((latex.clone(), *display)),
                _ => None,
            })
            .collect()
    }

    fn passthrough(events: &[Output]) -> Vec<u8> {
        let mut v = Vec::new();
        for e in events {
            if let Output::Passthrough(b) = e {
                v.extend_from_slice(b);
            }
        }
        v
    }

    // --- core cases --------------------------------------------------------

    #[test]
    fn plain_text_passes_through() {
        let ev = scan_whole(false, 4096, b"hello world\n");
        assert_eq!(passthrough(&ev), b"hello world\n");
        assert!(math_blocks(&ev).is_empty());
    }

    #[test]
    fn simple_block_dollar() {
        let ev = scan_whole(false, 4096, b"a $$x+1$$ b");
        assert_eq!(passthrough(&ev), b"a  b");
        assert_eq!(math_blocks(&ev), vec![("x+1".to_string(), true)]);
    }

    #[test]
    fn simple_block_bracket() {
        let ev = scan_whole(false, 4096, b"a \\[x+1\\] b");
        assert_eq!(passthrough(&ev), b"a  b");
        assert_eq!(math_blocks(&ev), vec![("x+1".to_string(), true)]);
    }

    #[test]
    fn block_at_stream_start_and_eof() {
        let ev = scan_whole(false, 4096, b"$$E=mc^2$$");
        assert_eq!(math_blocks(&ev), vec![("E=mc^2".to_string(), true)]);
        assert_eq!(passthrough(&ev), b"");
    }

    #[test]
    fn back_to_back_blocks() {
        let ev = scan_whole(false, 4096, b"$$a$$$$b$$");
        assert_eq!(
            math_blocks(&ev),
            vec![("a".to_string(), true), ("b".to_string(), true)]
        );
    }

    #[test]
    fn internal_single_dollar_preserved_in_block() {
        // A single `$` inside a `$$` block is content, not a close.
        let ev = scan_whole(false, 4096, b"$$a$b$$");
        assert_eq!(math_blocks(&ev), vec![("a$b".to_string(), true)]);
    }

    #[test]
    fn stray_single_dollar_is_passthrough() {
        let ev = scan_whole(false, 4096, b"the cost is $5 today");
        assert_eq!(passthrough(&ev), b"the cost is $5 today");
        assert!(math_blocks(&ev).is_empty());
    }

    #[test]
    fn lone_backslash_sequences_pass_through() {
        // `\n`, `\\`, `\t` etc. must not be eaten as `\[` openers.
        let input = b"path C:\\\\temp and \\n newline and trailing \\";
        let ev = scan_whole(false, 4096, input);
        assert!(math_blocks(&ev).is_empty());
        assert_eq!(reconstruct(&ev), input);
    }

    #[test]
    fn inline_stray_dollar_releases_control_sequences_immediately() {
        // In --inline mode a lone `$` opens a candidate inline block. A following
        // ESC sequence (e.g. an interactive TUI's startup cursor-position query)
        // must abort the block and pass through in the SAME feed — not be held in
        // the math buffer until EOF. Otherwise a TUI that blocks on the query
        // response freezes with a blank screen until `max_math_bytes` or Ctrl-C.
        let mut s = Scanner::with_config(true, 4096); // inline = true
        let ev = s.feed(b"prompt $ \x1b[6n more");
        assert!(math_blocks(&ev).is_empty(), "no math should be produced");
        let pass = passthrough(&ev);
        assert!(
            pass.windows(4).any(|w| w == b"\x1b[6n"),
            "the query escape must pass through in this feed, not be trapped: {pass:?}"
        );
        assert_eq!(reconstruct(&ev), b"prompt $ \x1b[6n more");
    }

    #[test]
    fn ansi_escapes_with_dollar_are_not_math() {
        // CSI color sequences (ESC = 0x1b) plus a stray `$` must pass through
        // untouched and produce no math in block-only mode.
        let input = b"\x1b[31mred$text\x1b[0m and \x1b[1mbold\x1b[0m";
        let ev = scan_whole(false, 4096, input);
        assert!(math_blocks(&ev).is_empty());
        assert_eq!(reconstruct(&ev), input);
        assert_eq!(passthrough(&ev), input);
    }

    // --- safety valve ------------------------------------------------------

    #[test]
    fn unterminated_block_hits_max_and_flushes_verbatim() {
        let max = 8;
        let mut input = b"x $$".to_vec();
        input.extend(std::iter::repeat(b'a').take(max + 20));
        input.extend_from_slice(b" y");
        let ev = scan_whole(false, max, &input);
        assert!(
            math_blocks(&ev).is_empty(),
            "oversized block must not produce a Math event"
        );
        assert_eq!(reconstruct(&ev), input, "all bytes must survive verbatim");
    }

    #[test]
    fn recovers_after_giving_up_on_oversized_block() {
        let max = 4;
        // First `$$...$$` is too long and is abandoned; a later short one works
        // only if it appears after the abort returns us to passthrough.
        let input = b"$$abcdefgh$$ then $$ok$$";
        let ev = scan_whole(false, max, input);
        assert_eq!(reconstruct(&ev), input);
        // The short block opens fresh after the abort flushes the long one.
        assert_eq!(math_blocks(&ev), vec![("ok".to_string(), true)]);
    }

    #[test]
    fn unterminated_at_eof_flushes_verbatim() {
        let input = b"text $$x+1 and no close";
        let ev = scan_whole(false, 4096, input);
        assert!(math_blocks(&ev).is_empty());
        assert_eq!(reconstruct(&ev), input);
    }

    #[test]
    fn trailing_dollar_at_eof() {
        let ev = scan_whole(false, 4096, b"price$");
        assert_eq!(reconstruct(&ev), b"price$");
        assert!(math_blocks(&ev).is_empty());
    }

    // --- inline mode -------------------------------------------------------

    #[test]
    fn inline_dollar_when_enabled() {
        let ev = scan_whole(true, 4096, b"let $x$ be");
        assert_eq!(math_blocks(&ev), vec![("x".to_string(), false)]);
        assert_eq!(passthrough(&ev), b"let  be");
    }

    #[test]
    fn inline_paren_when_enabled() {
        let ev = scan_whole(true, 4096, b"let \\(y\\) be");
        assert_eq!(math_blocks(&ev), vec![("y".to_string(), false)]);
    }

    #[test]
    fn inline_disabled_keeps_single_dollar_as_text() {
        let ev = scan_whole(false, 4096, b"let $x$ be");
        assert!(math_blocks(&ev).is_empty());
        assert_eq!(passthrough(&ev), b"let $x$ be");
    }

    #[test]
    fn block_still_works_in_inline_mode() {
        let ev = scan_whole(true, 4096, b"$$x$$ and $y$");
        assert_eq!(
            math_blocks(&ev),
            vec![("x".to_string(), true), ("y".to_string(), false)]
        );
    }

    // --- chunk-boundary invariance ----------------------------------------

    #[test]
    fn chunking_is_invariant_to_split_size() {
        let inputs: &[&[u8]] = &[
            b"a $$x+1$$ b $$y$$ c",
            b"\\[ frac{a}{b} \\] tail",
            b"mix $$a$b$$ and text $ and \\[c\\]",
            b"\x1b[31m$$z^2$$\x1b[0m",
            b"$$split across many tiny chunks$$",
            b"no math here at all, just $ and \\ and stuff",
        ];
        for input in inputs {
            let whole = scan_whole(false, 4096, input);
            // Reconstruction must always equal the input, regardless of split.
            assert_eq!(reconstruct(&whole), *input, "whole reconstruct: {input:?}");
            for chunk in 1..=7 {
                let chunked = scan_chunked(false, 4096, input, chunk);
                assert_eq!(
                    reconstruct(&chunked),
                    *input,
                    "reconstruct mismatch at chunk={chunk} for {input:?}"
                );
                assert_eq!(
                    math_blocks(&chunked),
                    math_blocks(&whole),
                    "math blocks differ at chunk={chunk} for {input:?}"
                );
            }
        }
    }

    #[test]
    fn delimiter_split_exactly_at_boundary() {
        // Opener `$$` split between two feeds, closer `$$` split too.
        let mut s = Scanner::with_config(false, 4096);
        let mut ev = s.feed(b"pre $");
        ev.extend(s.feed(b"$inner$"));
        ev.extend(s.feed(b"$post"));
        ev.extend(s.finish());
        assert_eq!(math_blocks(&ev), vec![("inner".to_string(), true)]);
        assert_eq!(passthrough(&ev), b"pre post");
    }

    #[test]
    fn reconstruct_always_lossless_for_random_ish_inputs() {
        // A grab-bag of adversarial byte patterns; reconstruction must be exact
        // at every chunk size.
        let inputs: &[&[u8]] = &[
            b"$",
            b"$$",
            b"$$$",
            b"$$$$",
            b"\\",
            b"\\[",
            b"\\]",
            b"\\[\\]",
            b"$$\\[$$",
            b"a$$b\\[c\\]d$$e",
        ];
        for input in inputs {
            for chunk in 1..=4 {
                let ev = scan_chunked(false, 8, input, chunk);
                assert_eq!(reconstruct(&ev), *input, "lossless {input:?} chunk={chunk}");
            }
        }
    }
}
