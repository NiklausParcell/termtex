//! `termtex watch` / `termtex render` — render the math in Claude Code's
//! responses from its **transcript** instead of the terminal stream.
//!
//! Interactive Claude paints equations onto the screen with cursor-positioning
//! escapes, so the LaTeX is shredded across the byte stream and can't be
//! reassembled without emulating a terminal. But Claude Code *also* writes every
//! response verbatim — clean markdown, `$$…$$` intact — to a JSONL transcript on
//! disk. So we sidestep the terminal entirely: tail that transcript and, each
//! time a response lands, run its clean text through the same detector + layout
//! engine used for line-oriented output. Run it in a side pane next to Claude.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bare::{BareDetector, BareEvent};
use crate::layout;
use crate::scanner::{Output, Scanner, DEFAULT_MAX_MATH_BYTES};

/// Render a whole transcript once (all past responses) and exit.
pub fn run_render(arg: Option<String>, cols: usize) -> i32 {
    let path = match resolve(arg) {
        Some(p) => p,
        None => return no_transcript(),
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("termtex render: cannot read {}: {e}", path.display());
            return 1;
        }
    };
    let mut stdout = std::io::stdout().lock();
    for line in data.split(|&b| b == b'\n') {
        if let Some(text) = assistant_text(line) {
            let _ = stdout.write_all(render_markdown(&text, cols).as_bytes());
            let _ = stdout.write_all(b"\n");
        }
    }
    0
}

/// Render past responses, then tail the transcript and render new ones live.
pub fn run_watch(arg: Option<String>, cols: usize) -> i32 {
    let path = match resolve(arg) {
        Some(p) => p,
        None => return no_transcript(),
    };
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "\x1b[2mtermtex: watching {}\x1b[0m\n", path.display());
    let mut offset: u64 = 0;
    let mut partial: Vec<u8> = Vec::new();
    loop {
        if let Ok((bytes, new_off)) = read_from(&path, offset) {
            offset = new_off;
            partial.extend_from_slice(&bytes);
            while let Some(pos) = partial.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = partial.drain(..=pos).collect();
                if let Some(text) = assistant_text(&line) {
                    let _ = stdout.write_all(render_markdown(&text, cols).as_bytes());
                    let _ = stdout.write_all(b"\n");
                    let _ = stdout.flush();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Companion loop (runs in a thread beside a wrapped `claude`): follow the
/// session transcript from the point it starts, and as each response lands,
/// print the pretty 2-D form of its display equations to the terminal. Reading
/// the transcript (Claude's finalized text) sidesteps the cursor-painted screen
/// stream entirely. Never returns.
pub fn companion(cols: usize) {
    let mut current: Option<(PathBuf, u64)> = None;
    let mut partial: Vec<u8> = Vec::new();
    loop {
        // Track the newest transcript for this directory; when a new session
        // file appears, follow it from its current end (skip prior history).
        if let Some(latest) = latest_transcript() {
            let switch = !matches!(&current, Some((p, _)) if *p == latest);
            if switch {
                let end = std::fs::metadata(&latest).map(|m| m.len()).unwrap_or(0);
                current = Some((latest, end));
                partial.clear();
            }
        }
        if let Some((path, offset)) = current.as_mut() {
            if let Ok((bytes, new_off)) = read_from(path, *offset) {
                *offset = new_off;
                partial.extend_from_slice(&bytes);
                while let Some(pos) = partial.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = partial.drain(..=pos).collect();
                    if let Some(text) = assistant_text(&line) {
                        for eq in equations(&text, cols) {
                            write_block(&eq);
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Write one rendered equation block to the terminal (raw mode → CRLF), under a
/// short fresh lock so it interleaves cleanly with the child's passthrough.
fn write_block(eq: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(b"\r\n");
    for line in eq.split('\n') {
        let _ = lock.write_all(line.as_bytes());
        let _ = lock.write_all(b"\r\n");
    }
    let _ = lock.flush();
}

fn no_transcript() -> i32 {
    eprintln!(
        "termtex: no Claude transcript found for this directory.\n\
         Run from the project dir where you use Claude, or pass a path:\n\
         \ttermtex watch ~/.claude/projects/<project>/<session>.jsonl"
    );
    1
}

/// Resolve the transcript path: an explicit arg, else the newest `.jsonl` in the
/// Claude project directory for the current working directory.
fn resolve(arg: Option<String>) -> Option<PathBuf> {
    if let Some(a) = arg {
        return Some(PathBuf::from(a));
    }
    latest_transcript()
}

fn latest_transcript() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let cwd = std::env::current_dir().ok()?;
    // Claude encodes the project path by replacing each '/' with '-'.
    let encoded = cwd.to_string_lossy().replace('/', "-");
    let dir = Path::new(&home).join(".claude/projects").join(encoded);
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                    newest = Some((modified, p));
                }
            }
        }
    }
    newest.map(|(_, p)| p)
}

fn read_from(path: &Path, offset: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len <= offset {
        return Ok((Vec::new(), offset)); // no growth (or truncated/rotated)
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::with_capacity((len - offset) as usize);
    f.read_to_end(&mut buf)?;
    Ok((buf, len))
}

/// Extract the concatenated assistant text from one transcript JSONL line, or
/// `None` if the line is not an assistant message with text.
fn assistant_text(line: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let blocks = v.get("message")?.get("content")?.as_array()?;
    let mut out = String::new();
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
    }
    (!out.trim().is_empty()).then_some(out)
}

/// Render a markdown response: prose passes through, and detected math becomes a
/// 2-D text layout. A fenced ```` ```latex ```` / ```` ```math ```` block is the
/// strongest signal — its entire (often multi-line) body is one equation, so we
/// join and render it whole rather than letting line-by-line detection fragment
/// it. Everything else flows through the `$$…$$` / `$…$` / bare detectors.
pub(crate) fn render_markdown(text: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut prose = String::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut k = 0;
    while k < lines.len() {
        match fence_lang(lines[k]) {
            Some(lang) => {
                // Collect the fenced body up to the closing ```.
                let mut body: Vec<&str> = Vec::new();
                k += 1;
                while k < lines.len() && fence_lang(lines[k]).is_none() {
                    body.push(lines[k]);
                    k += 1;
                }
                k += 1; // skip the closing ```
                if is_math_lang(lang) {
                    flush_prose(&mut prose, &mut out, cols);
                    emit_math(&body.join(" "), true, &mut out, cols);
                } else {
                    // A non-math fence (code) passes through verbatim.
                    prose.push_str("```");
                    prose.push_str(lang);
                    prose.push('\n');
                    for b in &body {
                        prose.push_str(b);
                        prose.push('\n');
                    }
                    prose.push_str("```\n");
                }
            }
            None => {
                prose.push_str(lines[k]);
                prose.push('\n');
                k += 1;
            }
        }
    }
    flush_prose(&mut prose, &mut out, cols);
    out
}

/// If `line` is a fence marker (```` ``` ```` optionally followed by a language),
/// return the language (possibly empty); otherwise `None`.
fn fence_lang(line: &str) -> Option<&str> {
    line.trim_start().strip_prefix("```").map(|s| s.trim())
}

fn is_math_lang(lang: &str) -> bool {
    matches!(lang, "latex" | "math" | "tex")
}

/// Run accumulated non-fence text through the `$$`/`$`/bare detectors.
fn flush_prose(prose: &mut String, out: &mut String, cols: usize) {
    if prose.is_empty() {
        return;
    }
    let mut scanner = Scanner::with_config(true, DEFAULT_MAX_MATH_BYTES);
    let mut bare = BareDetector::new(DEFAULT_MAX_MATH_BYTES);
    let mut events = bare.feed(prose.as_bytes());
    events.extend(bare.finish());
    for ev in events {
        match ev {
            BareEvent::Pass(bytes) => {
                for o in scanner.feed(&bytes) {
                    emit(o, out, cols);
                }
            }
            BareEvent::Math(latex) => emit_math(&latex, true, out, cols),
        }
    }
    for o in scanner.finish() {
        emit(o, out, cols);
    }
    prose.clear();
}

/// Extract just the rendered *display* equations from a markdown response (for
/// companion mode, which adds equations alongside Claude rather than rewriting
/// its text). Inline `$…$` is skipped — it is small and Claude shows it fine.
pub fn equations(text: &str, cols: usize) -> Vec<String> {
    let mut eqs: Vec<String> = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut prose = String::new();
    let mut k = 0;
    while k < lines.len() {
        match fence_lang(lines[k]) {
            Some(lang) => {
                let mut body: Vec<&str> = Vec::new();
                k += 1;
                while k < lines.len() && fence_lang(lines[k]).is_none() {
                    body.push(lines[k]);
                    k += 1;
                }
                k += 1;
                if is_math_lang(lang) {
                    collect_display_eqs(&mut prose, &mut eqs, cols);
                    push_eq(&body.join(" "), &mut eqs, cols);
                }
            }
            None => {
                prose.push_str(lines[k]);
                prose.push('\n');
                k += 1;
            }
        }
    }
    collect_display_eqs(&mut prose, &mut eqs, cols);
    eqs
}

fn push_eq(latex: &str, eqs: &mut Vec<String>, cols: usize) {
    let mut s = String::new();
    emit_math(latex, true, &mut s, cols);
    let t = s.trim_matches('\n').to_string();
    if !t.is_empty() {
        eqs.push(t);
    }
}

fn collect_display_eqs(prose: &mut String, eqs: &mut Vec<String>, cols: usize) {
    if prose.is_empty() {
        return;
    }
    let mut scanner = Scanner::with_config(true, DEFAULT_MAX_MATH_BYTES);
    let mut bare = BareDetector::new(DEFAULT_MAX_MATH_BYTES);
    let mut events = bare.feed(prose.as_bytes());
    events.extend(bare.finish());
    let handle = |o: Output, eqs: &mut Vec<String>| {
        if let Output::Math { latex, display, .. } = o {
            if display {
                push_eq(&latex, eqs, cols);
            }
        }
    };
    for ev in events {
        match ev {
            BareEvent::Pass(b) => {
                for o in scanner.feed(&b) {
                    handle(o, eqs);
                }
            }
            BareEvent::Math(latex) => push_eq(&latex, eqs, cols),
        }
    }
    for o in scanner.finish() {
        handle(o, eqs);
    }
    prose.clear();
}

fn emit(o: Output, out: &mut String, cols: usize) {
    match o {
        Output::Passthrough(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
        Output::Math { latex, display, .. } => emit_math(&latex, display, out, cols),
    }
}

/// Append a rendered equation. Inline single-line math stays in place; display
/// math (and any multi-row block) is set on its own lines.
fn emit_math(latex: &str, display: bool, out: &mut String, cols: usize) {
    let lines = layout::latex_to_lines_wrapped(latex, cols);
    if !display && lines.len() <= 1 {
        out.push_str(lines.first().map(String::as_str).unwrap_or(""));
        return;
    }
    if display && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    if display {
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_assistant_text() {
        let line = br#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi $x^2$"}]}}"#;
        assert_eq!(assistant_text(line).as_deref(), Some("hi $x^2$"));
        // non-assistant lines are ignored
        let user = br#"{"type":"user","message":{"content":[{"type":"text","text":"q"}]}}"#;
        assert_eq!(assistant_text(user), None);
        assert_eq!(assistant_text(b"not json"), None);
    }

    #[test]
    fn renders_inline_and_display_math() {
        // Inline single-char script stays on the line; a display equation blocks.
        let md = "Energy $x^2$ and the fraction:\n\\[ \\frac{a}{b} \\]\ndone";
        let r = render_markdown(md, 80);
        assert!(r.contains("Energy x² and"), "inline stays inline: {r}");
        assert!(r.contains('─'), "display fraction rendered as 2-D: {r}");
        assert!(r.contains("done"));
    }

    #[test]
    fn equations_extracts_display_blocks_only() {
        // Companion mode pulls out display equations (fenced / $$ / bare), and
        // skips inline $…$ (small, shown fine by Claude).
        let md = "Inline $x^2$ here.\n```latex\n\\frac{a}{b}\n```\nAnd bare:\n\\nabla \\cdot \\mathbf{v} = 0\ndone";
        let eqs = equations(md, 80);
        assert_eq!(eqs.len(), 2, "two display equations, inline x² skipped: {eqs:?}");
        assert!(eqs[0].contains('─'), "fenced fraction: {:?}", eqs[0]);
        assert!(eqs[1].contains("∇ · 𝐯 = 0"), "bare equation: {:?}", eqs[1]);
    }

    #[test]
    fn fenced_latex_block_renders_as_one_equation() {
        // Claude often writes multi-line LaTeX inside a ```latex fence; the whole
        // body is one equation, not fragmented per line.
        let md = "Result:\n```latex\n\\rho \\left(\n  \\frac{\\partial u}{\\partial t}\n\\right)\n= 0\n```\ndone";
        let r = render_markdown(md, 80);
        assert!(r.contains("─"), "fraction rendered: {r}");
        // No stray raw LaTeX fragments like a lone \right) leak through.
        assert!(!r.contains("\\right") && !r.contains("\\rho"), "no raw fragments: {r}");
        assert!(r.contains("Result:") && r.contains("done"));
    }

    #[test]
    fn renders_bare_display_equation() {
        let md = "Constraint:\n\\nabla \\cdot \\mathbf{v} = 0\nThat's it.";
        let r = render_markdown(md, 80);
        assert!(r.contains("∇ · 𝐯 = 0"), "{r}");
        assert!(!r.contains("\\nabla"), "raw LaTeX replaced: {r}");
    }
}
