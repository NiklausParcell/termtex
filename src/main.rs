//! mathterm — transparent terminal-graphics proxy for inline LaTeX.
//!
//! Wraps a child program in a PTY (so it still believes it is on a real
//! terminal), scans its stdout for LaTeX blocks, renders each to an image, and
//! emits it inline via the Kitty graphics protocol. All non-LaTeX output passes
//! through byte-for-byte.

mod bare;
mod config;
mod kitty;
mod pty;
mod render;
mod scanner;

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

/// LRU capacity for rendered equations.
const CACHE_CAPACITY: usize = 256;

/// stdin file descriptor of the real (controlling) terminal.
const STDIN_FD: i32 = libc::STDIN_FILENO;

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let cfg = match config::parse(std::env::args().skip(1)) {
        ParseOutcome::Run(cfg) => cfg,
        ParseOutcome::Exit(msg) => {
            print!("{msg}");
            return 0;
        }
        ParseOutcome::Error(msg) => {
            eprintln!("mathterm: {msg}");
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
                eprintln!("mathterm: no command and $SHELL is unset");
                return 1;
            }
        }
    } else {
        cfg.command.clone()
    };

    // --- Allocate the PTY ---------------------------------------------------
    let pty_system = native_pty_system();
    let size = terminal_size(STDIN_FD);
    let pair = match pty_system.openpty(size) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("mathterm: failed to open pty: {e}");
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
            eprintln!("mathterm: failed to spawn {:?}: {e}", command[0]);
            return 1;
        }
    };

    // Take the reader/writer before moving the master behind a mutex. Drop the
    // slave in the parent so the master sees EOF once the child exits.
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let mut writer = pair.master.take_writer().expect("take pty writer");
    drop(pair.slave);
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    // --- Enter raw mode (restored on drop, including on panic) --------------
    let _raw_guard = match RawModeGuard::new(STDIN_FD) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("mathterm: failed to enter raw mode: {e}");
            return 1;
        }
    };

    // --- SIGWINCH -> propagate new size to the child PTY --------------------
    spawn_resize_handler(Arc::clone(&master));

    // --- Thread B: real stdin -> child PTY ----------------------------------
    // Detached: it blocks on stdin.read and is reaped when the process exits.
    let debug_stdin = std::env::var_os("MT_DEBUG").is_some();
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
                    if debug_stdin {
                        eprintln!(
                            "[mt] stdin forwarded {n} bytes: {}",
                            buf[..n]
                                .iter()
                                .map(|b| format!("{b:02x}"))
                                .collect::<String>()
                        );
                    }
                    if writer.write_all(&buf[..n]).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // --- Thread A (main): child PTY -> scanner -> render -> real stdout -----
    // The scanner partitions the byte stream; passthrough bytes are copied
    // verbatim and completed math blocks are rendered to images and emitted via
    // the Kitty protocol. Rendering happens synchronously at block close: it is
    // ~2-5ms and math blocks are occasional, so it never meaningfully stalls the
    // passthrough path, and emitting in place keeps output ordering correct. On
    // any render failure or a non-graphics terminal, the original LaTeX bytes
    // are emitted verbatim (never crash, never corrupt).
    let mut stdout = std::io::stdout().lock();
    let mut scanner = Scanner::with_config(cfg.inline, cfg.max_math_bytes);
    // Optional bare-LaTeX detector sits in front of the delimiter scanner: it
    // passes every byte through (to the scanner) and additionally emits images
    // for delimiter-less equations. Off unless --detect-bare.
    let mut bare = cfg
        .detect_bare
        .then(|| BareDetector::new(cfg.max_math_bytes));
    let sink = Sink::new(&cfg);
    let mut buf = [0u8; 8192];
    let mut broken = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // child closed the PTY (exited)
            Ok(n) => {
                broken = feed_output(&mut stdout, &sink, &mut scanner, bare.as_mut(), &buf[..n]);
                if broken || stdout.flush().is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    // Flush anything buffered at EOF (scanner tail + any pending bare equation).
    if !broken {
        if let Some(bare) = bare.as_mut() {
            for event in bare.finish() {
                if dispatch_bare(&mut stdout, &sink, &mut scanner, event).is_err() {
                    broken = true;
                    break;
                }
            }
        }
        if !broken {
            for event in scanner.finish() {
                if sink.emit(&mut stdout, event).is_err() {
                    break;
                }
            }
        }
        let _ = stdout.flush();
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

/// Emit a hardcoded test image inline via the Kitty graphics protocol.
fn run_selftest_image() -> i32 {
    let png = kitty::selftest_png();
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(b"mathterm Kitty graphics self-test:\r\n");
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
    detect_log: Mutex<Option<std::fs::File>>,
}

impl Sink {
    fn new(cfg: &Config) -> Self {
        let [r, g, b] = cfg.color;
        let base = RatexRenderer::new(cfg.font_px()).with_color(r, g, b);
        let renderer: Box<dyn MathRenderer> = if cfg.no_cache {
            Box::new(base)
        } else {
            Box::new(CachingRenderer::new(base, CACHE_CAPACITY))
        };
        let graphics = match cfg.graphics {
            GraphicsMode::Force => true,
            GraphicsMode::Off => false,
            GraphicsMode::Auto => terminal_supports_graphics(),
        };
        Sink {
            renderer,
            graphics,
            detect_log: Mutex::new(open_detect_log()),
        }
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
                if !self.graphics {
                    return stdout.write_all(&raw);
                }
                match self.renderer.render(&latex, display) {
                    Ok(img) => {
                        // Display math sits on its own line; inline renders in place.
                        if display {
                            stdout.write_all(b"\r\n")?;
                        }
                        kitty::emit_png(stdout, &img.png, None, None)?;
                        if display {
                            stdout.write_all(b"\r\n")?;
                        }
                        Ok(())
                    }
                    // Never crash on bad LaTeX: fall back to the original bytes.
                    Err(_) => stdout.write_all(&raw),
                }
            }
        }
    }

    /// Render a heuristically-detected bare equation and emit it on its own
    /// line. The source text has already passed through (augment, not replace),
    /// so on any failure we simply emit nothing extra.
    fn emit_bare_math(&self, stdout: &mut impl Write, latex: &str) -> std::io::Result<()> {
        self.log_detection(latex, true, latex.len());
        if !self.graphics {
            return Ok(());
        }
        if let Ok(img) = self.renderer.render(latex, true) {
            stdout.write_all(b"\r\n")?;
            kitty::emit_png(stdout, &img.png, None, None)?;
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
        .open("mathterm-detect.log")
        .ok()
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
fn spawn_resize_handler(master: Arc<Mutex<Box<dyn MasterPty + Send>>>) {
    let mut signals = match Signals::new([SIGWINCH]) {
        Ok(s) => s,
        Err(_) => return, // resize is a nicety; failing to register isn't fatal
    };
    thread::spawn(move || {
        for _ in signals.forever() {
            let size = terminal_size(STDIN_FD);
            if let Ok(master) = master.lock() {
                let _ = master.resize(size);
            }
        }
    });
}
