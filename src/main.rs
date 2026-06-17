//! termtex — render LaTeX math in your terminal.
//!
//! Wraps a child program in a PTY (so it still believes it is on a real
//! terminal), scans its stdout for LaTeX blocks, and renders each in place. By
//! default a block becomes a multi-line 2-D text layout (see `layout`); with
//! `--image` it becomes a typeset image via the Kitty graphics protocol. All
//! non-LaTeX output passes through byte-for-byte.

mod bare;
mod compositor;
mod config;
mod grid;
mod kitty;
mod pty;
mod render;
mod scanner;
mod strip;
mod layout;
mod unicode;
mod watch;

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty};
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;

use bare::{BareDetector, BareEvent};
use config::{Config, GraphicsMode, ParseOutcome};
use pty::{terminal_size, RawModeGuard};
use render::{CachingRenderer, MathRenderer, RatexRenderer};
use scanner::{Output, Scanner};
use strip::Strip;

/// LRU capacity for rendered equations.
const CACHE_CAPACITY: usize = 256;

/// stdin file descriptor of the real (controlling) terminal.
const STDIN_FD: i32 = libc::STDIN_FILENO;

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    // Subcommands that render Claude Code's transcript (clean markdown) rather
    // than wrapping a child: `termtex watch [file]` tails the live session and
    // renders math as responses land; `termtex render [file]` renders past
    // responses once and exits.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(sub) = argv.first().map(String::as_str) {
        if sub == "watch" || sub == "render" {
            let cols = terminal_size(STDIN_FD).cols.max(1) as usize;
            let arg = argv.get(1).cloned();
            return match sub {
                "watch" => watch::run_watch(arg, cols),
                _ => watch::run_render(arg, cols),
            };
        }
        if sub == "replay" {
            return run_replay(&argv[1..]);
        }
    }

    let cfg = match config::parse(std::env::args().skip(1)) {
        ParseOutcome::Run(cfg) => cfg,
        ParseOutcome::Exit(msg) => {
            print!("{msg}");
            return 0;
        }
        ParseOutcome::Error(msg) => {
            eprintln!("termtex: {msg}");
            return 2;
        }
    };

    // Diagnostic: emit a hardcoded image via the Kitty protocol and exit, to
    // confirm the terminal renders inline graphics at all (independent of the
    // PTY proxy and LaTeX pipeline).
    if cfg.selftest_image {
        return run_selftest_image();
    }

    // Resolve the child command: args after `--` (or a bare command), else
    // wrap the user's $SHELL as an interactive session.
    let command = if cfg.command.is_empty() {
        match std::env::var("SHELL") {
            Ok(shell) => vec![shell],
            Err(_) => {
                eprintln!("termtex: no command and $SHELL is unset");
                return 1;
            }
        }
    } else {
        cfg.command.clone()
    };

    // --- Allocate the PTY ---------------------------------------------------
    let pty_system = native_pty_system();
    let size = terminal_size(STDIN_FD);
    // Cell pixel size (for fitting images to text rows), if the terminal reports
    // pixel dimensions; None means we fall back to natural image size.
    let cell = cell_pixels(&size);

    // Resolve graphics up front (gates the image path; the strip can carry the
    // text layout even without graphics).
    let graphics_on = match cfg.graphics {
        GraphicsMode::Force => true,
        GraphicsMode::Off => false,
        GraphicsMode::Auto => terminal_supports_graphics(),
    };
    let strip = cfg
        .strip
        .then(|| Strip::new(size.rows, size.cols, cfg.strip_rows, cell));

    // In strip mode the child gets a terminal that is `strip_rows` shorter.
    let mut child_size = size;
    if let Some(s) = &strip {
        child_size.rows = s.child_rows();
    }
    let pair = match pty_system.openpty(child_size) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("termtex: failed to open pty: {e}");
            return 1;
        }
    };

    // Build the child command, inheriting the parent environment and cwd so the
    // child sees the same TERM, PATH, etc. as if launched directly.
    let mut builder = CommandBuilder::new(&command[0]);
    builder.args(&command[1..]);
    for (key, value) in std::env::vars() {
        builder.env(key, value);
    }
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }

    let mut child = match pair.slave.spawn_command(builder) {
        Ok(child) => child,
        Err(e) => {
            eprintln!("termtex: failed to spawn {:?}: {e}", command[0]);
            return 1;
        }
    };

    // Take the reader/writer before moving the master behind a mutex. Drop the
    // slave in the parent so the master sees EOF once the child exits.
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    // Shared so both the stdin thread (forwarding keystrokes) and the compositor
    // (answering the child's terminal queries) can write to the child's input.
    let writer: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(pair.master.take_writer().expect("take pty writer")));
    drop(pair.slave);
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    // --- Enter raw mode (restored on drop, including on panic) --------------
    let _raw_guard = match RawModeGuard::new(STDIN_FD) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("termtex: failed to enter raw mode: {e}");
            return 1;
        }
    };

    // --- SIGWINCH -> propagate new size to the child PTY --------------------
    let reserved_rows = strip.as_ref().map(|_| cfg.strip_rows).unwrap_or(0);
    spawn_resize_handler(Arc::clone(&master), reserved_rows);

    // --- Thread B: real stdin -> child PTY ----------------------------------
    // Detached: it blocks on stdin.read and is reaped when the process exits.
    let debug_stdin = std::env::var_os("MT_DEBUG").is_some();
    let stdin_writer = Arc::clone(&writer);
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => {
                    if debug_stdin {
                        eprintln!("[mt] stdin EOF");
                    }
                    break;
                }
                Ok(n) => {
                    let mut w = stdin_writer.lock().unwrap();
                    if w.write_all(&buf[..n]).is_err() || w.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // --- Compositor mode: own the display ----------------------------------
    // Reconstruct the child's screen on a virtual grid, render a composited
    // version (equations typeset in place) to the alternate screen, and answer
    // the child's terminal queries so it runs cleanly under us.
    if cfg.compose {
        let rows = size.rows.max(1) as usize;
        let cols = size.cols.max(1) as usize;
        {
            let mut o = std::io::stdout().lock();
            let _ = o.write_all(b"\x1b[?1049h\x1b[2J\x1b[H");
            let _ = o.flush();
        }
        let mut g = grid::Grid::new(rows, cols);
        let mut shown: Vec<String> = vec![String::new(); rows];
        // Optional: tee the child's raw output for offline replay/debugging.
        let mut capture = cfg
            .capture
            .as_ref()
            .and_then(|path| std::fs::File::create(path).ok());
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(f) = capture.as_mut() {
                        let _ = f.write_all(&buf[..n]);
                    }
                    g.feed(&buf[..n]);
                    let resp = g.take_responses();
                    if !resp.is_empty() {
                        if let Ok(mut w) = writer.lock() {
                            let _ = w.write_all(&resp);
                            let _ = w.flush();
                        }
                    }
                    let (display, map) = compositor::compose_mapped(&g.rows(), cols);
                    let top = display.len().saturating_sub(rows);
                    let mut o = std::io::stdout().lock();
                    let _ = o.write_all(b"\x1b[?25l"); // hide cursor while repainting
                    for vr in 0..rows {
                        // Styled lines carry SGR escapes (not visible columns), and
                        // the grid already bounds them to the width, so emit whole.
                        let line = display.get(top + vr).map(String::as_str).unwrap_or("");
                        if shown[vr] != line {
                            // Reset before erasing so a prior bg colour can't bleed.
                            let _ = write!(o, "\x1b[{};1H\x1b[0m\x1b[K{line}", vr + 1);
                            shown[vr] = line.to_string();
                        }
                    }
                    let (cr, cc) = g.cursor();
                    let drow = map.get(cr).copied().unwrap_or(0);
                    let vrow = drow.saturating_sub(top).min(rows - 1);
                    let _ = write!(o, "\x1b[{};{}H\x1b[?25h", vrow + 1, cc + 1);
                    let _ = o.flush();
                }
                Err(_) => break,
            }
        }
        {
            let mut o = std::io::stdout().lock();
            let _ = o.write_all(b"\x1b[?25h\x1b[?1049l");
            let _ = o.flush();
        }
        return match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => 1,
        };
    }

    // --- Thread A (main): child PTY -> scanner -> render -> real stdout -----
    // The scanner partitions the byte stream; passthrough bytes are copied
    // verbatim and completed math blocks are rendered to images and emitted via
    // the Kitty protocol. Rendering happens synchronously at block close: it is
    // ~2-5ms and math blocks are occasional, so it never meaningfully stalls the
    // passthrough path, and emitting in place keeps output ordering correct. On
    // any render failure or a non-graphics terminal, the original LaTeX bytes
    // are emitted verbatim (never crash, never corrupt).
    // Companion mode: follow Claude's transcript in a thread and print pretty
    // equations as responses land. The child's own output passes through
    // verbatim, so its TUI is never disturbed. stdout is locked per write (not
    // held for the whole run) so the companion thread can interleave cleanly.
    let companion = cfg.companion;
    if companion {
        let cols = size.cols.max(1) as usize;
        thread::spawn(move || watch::companion(cols));
    }
    let mut scanner = Scanner::with_config(cfg.inline, cfg.max_math_bytes);
    // Optional bare-LaTeX detector sits in front of the delimiter scanner: it
    // passes every byte through (to the scanner) and additionally emits images
    // for delimiter-less equations. Off unless --detect-bare.
    let mut bare = cfg
        .detect_bare
        .then(|| BareDetector::new(cfg.max_math_bytes));
    let sink = Sink::new(&cfg, cell, graphics_on, strip.clone(), size.cols);
    // Reserve the strip region (scroll region + clear) before output begins.
    if let Some(s) = &strip {
        let mut out = std::io::stdout().lock();
        let _ = s.setup(&mut out);
    }
    // Diagnostic: tee the child's raw output to a file before scanning, to
    // characterize a program's stream (e.g. a TUI's cursor-control escapes).
    let mut capture = cfg
        .capture
        .as_ref()
        .and_then(|path| std::fs::File::create(path).ok());
    let mut buf = [0u8; 8192];
    let mut broken = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // child closed the PTY (exited)
            Ok(n) => {
                if let Some(file) = capture.as_mut() {
                    let _ = file.write_all(&buf[..n]);
                }
                let mut out = std::io::stdout().lock();
                if companion {
                    // Pass the child through untouched; equations come from the
                    // transcript via the companion thread.
                    if out.write_all(&buf[..n]).is_err() {
                        break;
                    }
                } else {
                    broken = feed_output(&mut out, &sink, &mut scanner, bare.as_mut(), &buf[..n]);
                }
                if broken || out.flush().is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    // Flush anything buffered at EOF (scanner tail + any pending bare equation).
    if !broken && !companion {
        let mut out = std::io::stdout().lock();
        if let Some(bare) = bare.as_mut() {
            for event in bare.finish() {
                if dispatch_bare(&mut out, &sink, &mut scanner, event).is_err() {
                    broken = true;
                    break;
                }
            }
        }
        if !broken {
            for event in scanner.finish() {
                if sink.emit(&mut out, event).is_err() {
                    break;
                }
            }
        }
        let _ = out.flush();
    }

    // Release the reserved strip (reset scroll region, clear) on exit.
    if let Some(s) = &strip {
        let mut out = std::io::stdout().lock();
        let _ = s.teardown(&mut out);
    }

    // --- Exit with the child's status ---------------------------------------
    match child.wait() {
        Ok(status) => status.exit_code() as i32,
        Err(_) => 1,
    }
}

/// Process one chunk of child output. With a bare detector, every byte still
/// reaches the delimiter scanner; the detector only adds images for bare
/// equations. Returns true if the output pipe broke.
fn feed_output(
    stdout: &mut impl Write,
    sink: &Sink,
    scanner: &mut Scanner,
    bare: Option<&mut BareDetector>,
    chunk: &[u8],
) -> bool {
    match bare {
        None => {
            for event in scanner.feed(chunk) {
                if sink.emit(stdout, event).is_err() {
                    return true;
                }
            }
            false
        }
        Some(bare) => {
            for event in bare.feed(chunk) {
                if dispatch_bare(stdout, sink, scanner, event).is_err() {
                    return true;
                }
            }
            false
        }
    }
}

/// Route a bare-detector event: pass-through bytes go to the delimiter scanner
/// (so `$$`/`$` still work inside them); a detected equation is rendered.
fn dispatch_bare(
    stdout: &mut impl Write,
    sink: &Sink,
    scanner: &mut Scanner,
    event: BareEvent,
) -> std::io::Result<()> {
    match event {
        BareEvent::Pass(bytes) => {
            for out in scanner.feed(&bytes) {
                sink.emit(stdout, out)?;
            }
            Ok(())
        }
        BareEvent::Math(latex) => sink.emit_bare_math(stdout, &latex),
    }
}

/// `termtex replay <capture> [--rows N] [--cols N]` — render a captured byte
/// stream onto the screen model and print the result, so we can see what a
/// program actually paints (not the raw escape soup). Instrument for the
/// observe → hypothesize → change → replay loop.
fn run_replay(args: &[String]) -> i32 {
    let mut path: Option<&str> = None;
    let mut rows = 50usize;
    let mut cols = 120usize;
    let mut render = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rows" => rows = it.next().and_then(|s| s.parse().ok()).unwrap_or(rows),
            "--cols" => cols = it.next().and_then(|s| s.parse().ok()).unwrap_or(cols),
            // After reconstructing the screen, run it through the equation
            // renderer — what an in-place inline pass would produce.
            "--render" => render = true,
            other => path = Some(other),
        }
    }
    let Some(path) = path else {
        eprintln!("usage: termtex replay <capture-file> [--rows N] [--cols N]");
        return 2;
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("termtex replay: cannot read {path}: {e}");
            return 1;
        }
    };
    let mut g = grid::Grid::new(rows, cols);
    g.feed(&bytes);
    let screen = g.render();
    let display = if render {
        compositor::compose(&screen, cols)
    } else {
        screen
    };
    for line in display {
        println!("{line}");
    }
    0
}

/// Emit a hardcoded test image inline via the Kitty graphics protocol.
fn run_selftest_image() -> i32 {
    let png = kitty::selftest_png();
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(b"termtex Kitty graphics self-test:\r\n");
    if kitty::emit_png(&mut stdout, &png, None, None).is_err() {
        return 1;
    }
    let _ = stdout
        .write_all(b"\r\n^ if you see a blue box with a white border, inline graphics work.\r\n");
    let _ = stdout.flush();
    0
}

/// Consumes scanner [`Output`]s and writes them to the terminal: passthrough
/// bytes verbatim, math blocks as inline Kitty images (falling back to the raw
/// LaTeX bytes when the terminal lacks graphics support or a render fails).
struct Sink {
    renderer: Box<dyn MathRenderer>,
    graphics: bool,
    /// (cell_width_px, cell_height_px) of the terminal, when known.
    cell: Option<(u32, u32)>,
    /// Cap an image to this many rows (0 = natural).
    max_rows: u32,
    /// When set, equations are drawn into a reserved bottom strip instead of
    /// inline (for self-repainting TUIs like interactive Claude Code).
    strip: Option<Strip>,
    /// Render detected math as single-line Unicode text.
    unicode: bool,
    /// Render detected math as a Kitty-graphics image (opt-in via `--image`).
    /// When false (the default), math renders as a 2-D text layout.
    image: bool,
    /// Terminal width in columns (0 = unknown), used to wrap wide 2-D layouts.
    term_cols: usize,
    detect_log: Mutex<Option<std::fs::File>>,
}

impl Sink {
    fn new(
        cfg: &Config,
        cell: Option<(u32, u32)>,
        graphics: bool,
        strip: Option<Strip>,
        term_cols: u16,
    ) -> Self {
        let [r, g, b] = cfg.color;
        let base = RatexRenderer::new(cfg.font_px()).with_color(r, g, b);
        let renderer: Box<dyn MathRenderer> = if cfg.no_cache {
            Box::new(base)
        } else {
            Box::new(CachingRenderer::new(base, CACHE_CAPACITY))
        };
        Sink {
            renderer,
            graphics,
            cell,
            max_rows: cfg.max_rows,
            strip,
            unicode: cfg.unicode,
            // Image is opt-in (`--image`); `--pretty` forces text back over it.
            // Independent of placement: an equation renders as image or text and
            // is then placed inline or into the strip.
            image: cfg.image && !cfg.pretty,
            term_cols: term_cols as usize,
            detect_log: Mutex::new(open_detect_log()),
        }
    }

    /// Compute the Kitty `c=`/`r=` cell box for an image: cap its height at
    /// `max_rows` (scaling to preserve aspect) when the cell size is known.
    /// Returns `(None, None)` to use the image's natural size.
    fn fit(&self, width_px: u32, height_px: u32) -> (Option<u32>, Option<u32>) {
        let (cell_w, cell_h) = match self.cell {
            Some(c) => c,
            None => return (None, None),
        };
        if self.max_rows == 0 || cell_w == 0 || cell_h == 0 {
            return (None, None);
        }
        let nat_rows = height_px.div_ceil(cell_h).max(1);
        let nat_cols = width_px.div_ceil(cell_w).max(1);
        if nat_rows <= self.max_rows {
            return (Some(nat_cols), Some(nat_rows));
        }
        // Scale down proportionally so height == max_rows.
        let rows = self.max_rows;
        let cols = ((nat_cols as u64 * rows as u64) / nat_rows as u64).max(1) as u32;
        (Some(cols), Some(rows))
    }

    fn emit(&self, stdout: &mut impl Write, event: Output) -> std::io::Result<()> {
        match event {
            Output::Passthrough(bytes) => stdout.write_all(&bytes),
            Output::Math {
                latex,
                display,
                raw,
            } => {
                self.log_detection(&latex, display, raw.len());
                // Unicode mode: replace the (delimiter-stripped) math with its
                // Unicode text form. Pure text → safe in any terminal, including
                // self-repainting TUIs. Independent of graphics support.
                if self.unicode {
                    let text = unicode::latex_to_unicode(&latex);
                    return stdout.write_all(text.as_bytes());
                }
                // Strip placement (for self-repainting TUIs): the original LaTeX
                // flows through inline so the child's layout is untouched, and the
                // rendered equation — image or 2-D text — goes to the reserved
                // bottom strip, which termtex owns.
                if let Some(strip) = &self.strip {
                    stdout.write_all(&raw)?;
                    if self.image && self.graphics {
                        if let Ok(img) = self.renderer.render(&latex, display) {
                            strip.draw(stdout, &img.png, img.width_px, img.height_px)?;
                        }
                    } else {
                        strip.draw_text(stdout, &layout::latex_to_lines(&latex))?;
                    }
                    return Ok(());
                }
                // Inline image (opt-in via --image) needs a graphics-capable
                // terminal; otherwise we fall through to the default text layout.
                if self.image && self.graphics {
                    match self.renderer.render(&latex, display) {
                        Ok(img) => {
                            let (cols, rows) = self.fit(img.width_px, img.height_px);
                            // Display math sits on its own line; inline in place.
                            if display {
                                stdout.write_all(b"\r\n")?;
                            }
                            kitty::emit_png(stdout, &img.png, cols, rows)?;
                            if display {
                                stdout.write_all(b"\r\n")?;
                            }
                            return Ok(());
                        }
                        // Never crash on bad LaTeX: fall back to the text layout.
                        Err(_) => return self.emit_pretty(stdout, &latex, display),
                    }
                }
                // Default: a multi-line 2-D text layout (miniature TeX). Display
                // math goes on its own lines; inline still renders as a block
                // (a 2-D layout can't sit mid-line).
                self.emit_pretty(stdout, &latex, display)
            }
        }
    }

    /// Render a heuristically-detected bare equation and emit it on its own
    /// line. The source text has already passed through (augment, not replace),
    /// so on any failure we simply emit nothing extra.
    fn emit_bare_math(&self, stdout: &mut impl Write, latex: &str) -> std::io::Result<()> {
        self.log_detection(latex, true, latex.len());
        // Unicode mode: the raw LaTeX line already passed through; show the
        // Unicode rendering just below it (text, so TUI-safe; no graphics needed).
        if self.unicode {
            let text = unicode::latex_to_unicode(latex);
            return write!(stdout, "\r\n{text}\r\n");
        }
        // Strip placement: the raw line already passed through; draw the render
        // (image or 2-D text) into the reserved strip.
        if let Some(strip) = &self.strip {
            if self.image && self.graphics {
                if let Ok(img) = self.renderer.render(latex, true) {
                    strip.draw(stdout, &img.png, img.width_px, img.height_px)?;
                }
            } else {
                strip.draw_text(stdout, &layout::latex_to_lines(latex))?;
            }
            return Ok(());
        }
        // Inline image (opt-in): show the rendered image below the line.
        if self.image && self.graphics {
            if let Ok(img) = self.renderer.render(latex, true) {
                let (cols, rows) = self.fit(img.width_px, img.height_px);
                stdout.write_all(b"\r\n")?;
                kitty::emit_png(stdout, &img.png, cols, rows)?;
                stdout.write_all(b"\r\n")?;
            }
            return Ok(());
        }
        // Default: show the 2-D text layout just below the passed-through line.
        self.emit_pretty(stdout, latex, true)
    }

    /// Emit a math fragment as a multi-line 2-D text block. Each row is
    /// terminated with CRLF (the PTY is in raw mode). For display math the block
    /// is set off with blank lines above and below; inline math gets the block
    /// in place (a 2-D layout cannot sit within a line).
    fn emit_pretty(&self, stdout: &mut impl Write, latex: &str, display: bool) -> std::io::Result<()> {
        // Re-flow to the terminal width so a wide equation breaks into indented
        // panels instead of being hard-wrapped (and misaligned) by the terminal.
        let lines = layout::latex_to_lines_wrapped(latex, self.term_cols);
        // Inline math (`$…$`) that renders to a single line stays in place with no
        // surrounding newlines, so it flows within the sentence. Display math
        // always gets its own line(s).
        if !display && lines.len() <= 1 {
            return stdout.write_all(lines.first().map(String::as_str).unwrap_or("").as_bytes());
        }
        // A multi-row 2-D block gets its own lines; display math is set off with
        // blank lines above and below.
        if display {
            stdout.write_all(b"\r\n")?;
        }
        for line in &lines {
            stdout.write_all(line.as_bytes())?;
            stdout.write_all(b"\r\n")?;
        }
        if display {
            stdout.write_all(b"\r\n")?;
        }
        Ok(())
    }

    fn log_detection(&self, latex: &str, display: bool, raw_len: usize) {
        if let Ok(mut guard) = self.detect_log.lock() {
            if let Some(log) = guard.as_mut() {
                let kind = if display { "block" } else { "inline" };
                let _ = writeln!(log, "[detect] {kind} ({raw_len} bytes): {latex}");
                let _ = log.flush();
            }
        }
    }
}

/// Open the detection log file when `MT_DEBUG` is set; otherwise `None`.
fn open_detect_log() -> Option<std::fs::File> {
    if std::env::var_os("MT_DEBUG").is_none() {
        return None;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("termtex-detect.log")
        .ok()
}

/// Per-cell pixel size from the terminal's reported pixel dimensions, used to
/// fit images to text rows. `None` if the terminal reports no pixel size.
fn cell_pixels(size: &portable_pty::PtySize) -> Option<(u32, u32)> {
    if size.pixel_width == 0 || size.pixel_height == 0 || size.cols == 0 || size.rows == 0 {
        return None;
    }
    Some((
        (size.pixel_width / size.cols) as u32,
        (size.pixel_height / size.rows) as u32,
    ))
}

/// Heuristic capability check for the Kitty graphics protocol: recognize the
/// terminals known to implement it. A runtime query (sending the protocol's
/// detection escape and reading the reply) is more robust but races with the
/// child in raw mode; `--force-graphics`/`--no-graphics` override this. Errs
/// toward enabling only for known-good terminals.
fn terminal_supports_graphics() -> bool {
    if std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some()
        || std::env::var_os("GHOSTTY_BIN_DIR").is_some()
    {
        return true;
    }
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    if term.contains("kitty") || term.contains("ghostty") {
        return true;
    }
    let prog = std::env::var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    prog.contains("ghostty") || prog.contains("wezterm")
}

/// Spawn a thread that resizes the child PTY whenever the real terminal resizes.
fn spawn_resize_handler(master: Arc<Mutex<Box<dyn MasterPty + Send>>>, reserved_rows: u16) {
    let mut signals = match Signals::new([SIGWINCH]) {
        Ok(s) => s,
        Err(_) => return, // resize is a nicety; failing to register isn't fatal
    };
    thread::spawn(move || {
        for _ in signals.forever() {
            let mut size = terminal_size(STDIN_FD);
            // Keep the child's view shorter by the reserved strip height.
            size.rows = size.rows.saturating_sub(reserved_rows).max(1);
            if let Ok(master) = master.lock() {
                let _ = master.resize(size);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink_with_cell(cell: Option<(u32, u32)>, max_rows: u32) -> Sink {
        let mut cfg = Config::default();
        cfg.max_rows = max_rows;
        Sink::new(&cfg, cell, true, None, 80)
    }

    #[test]
    fn fit_returns_natural_when_cell_size_unknown() {
        let s = sink_with_cell(None, 3);
        assert_eq!(s.fit(400, 150), (None, None));
    }

    #[test]
    fn fit_keeps_natural_when_under_cap() {
        // cell 10x20: a 200x40 image is 20 cols x 2 rows, under the 3-row cap.
        let s = sink_with_cell(Some((10, 20)), 3);
        assert_eq!(s.fit(200, 40), (Some(20), Some(2)));
    }

    #[test]
    fn fit_scales_down_tall_images_preserving_aspect() {
        // cell 10x20: a 400x200 image is 40 cols x 10 rows; cap at 3 rows ->
        // scale 3/10, cols ~= 40*3/10 = 12.
        let s = sink_with_cell(Some((10, 20)), 3);
        assert_eq!(s.fit(400, 200), (Some(12), Some(3)));
    }

    #[test]
    fn fit_disabled_when_max_rows_zero() {
        let s = sink_with_cell(Some((10, 20)), 0);
        assert_eq!(s.fit(400, 200), (None, None));
    }
}
