//! mathterm — transparent terminal-graphics proxy for inline LaTeX.
//!
//! Build step 1: pure PTY passthrough. `mathterm -- <cmd>` must behave exactly
//! like running `<cmd>` directly — colors, interactivity, resize, and exit code
//! all intact. No LaTeX scanning yet; bytes flow through verbatim. This is the
//! riskiest layer, so it is built and verified on its own first.

mod pty;
mod scanner;

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty};
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;

use pty::{terminal_size, RawModeGuard};
use scanner::{Output, Scanner};

/// stdin file descriptor of the real (controlling) terminal.
const STDIN_FD: i32 = libc::STDIN_FILENO;

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    // --- Resolve the child command -----------------------------------------
    // Everything after a `--` is the command to wrap. With no command, wrap the
    // user's $SHELL as an interactive session.
    let command = match resolve_command() {
        Some(cmd) => cmd,
        None => {
            eprintln!("mathterm: no command and $SHELL is unset");
            return 1;
        }
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

    // --- Thread A (main): child PTY -> scanner -> real stdout ---------------
    // Step 2 (detect-only): the scanner partitions the byte stream, but we
    // re-emit math blocks verbatim via their `raw` bytes, so output is still
    // byte-for-byte identical to step 1. Detected blocks are logged (when
    // MT_DEBUG is set) to a file so the live terminal is never polluted.
    let mut stdout = std::io::stdout().lock();
    let mut scanner = Scanner::new();
    let mut detect_log = open_detect_log();
    let mut buf = [0u8; 8192];
    let mut broken = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // child closed the PTY (exited)
            Ok(n) => {
                for event in scanner.feed(&buf[..n]) {
                    if emit(&mut stdout, &mut detect_log, event).is_err() {
                        broken = true;
                        break;
                    }
                }
                if broken || stdout.flush().is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    // Flush any block left buffered at EOF (emitted verbatim).
    if !broken {
        for event in scanner.finish() {
            if emit(&mut stdout, &mut detect_log, event).is_err() {
                break;
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

/// Write a single scanner output to the terminal. In detect-only mode math
/// blocks are re-emitted via their original bytes (`raw`), keeping passthrough
/// byte-for-byte identical; detections are logged when a log is open.
fn emit(
    stdout: &mut impl Write,
    detect_log: &mut Option<std::fs::File>,
    event: Output,
) -> std::io::Result<()> {
    match event {
        Output::Passthrough(bytes) => stdout.write_all(&bytes),
        Output::Math {
            latex,
            display,
            raw,
        } => {
            if let Some(log) = detect_log {
                let kind = if display { "block" } else { "inline" };
                let _ = writeln!(log, "[detect] {kind} ({} bytes): {latex}", raw.len());
                let _ = log.flush();
            }
            stdout.write_all(&raw)
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

/// Resolve the child command: args after `--`, else `$SHELL`.
fn resolve_command() -> Option<Vec<String>> {
    let mut command: Vec<String> = Vec::new();
    let mut saw_separator = false;

    for arg in std::env::args().skip(1) {
        if !saw_separator && arg == "--" {
            saw_separator = true;
            continue;
        }
        if saw_separator {
            command.push(arg);
        }
        // Before `--` we currently ignore options; flags arrive in a later step.
    }

    if command.is_empty() {
        let shell = std::env::var("SHELL").ok()?;
        command.push(shell);
    }
    Some(command)
}
