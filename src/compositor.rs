//! Compose a *display* screen from a reconstructed *source* screen.
//!
//! The source is what a program (Claude) actually painted, recovered by the
//! [`grid`](crate::grid) screen model. This replaces each detected equation, in
//! place, with its pretty 2-D render — preserving the surrounding layout (the
//! left indentation, the UI chrome, the prose) so the result looks like the
//! original screen with the math typeset.
//!
//! Three equation forms are handled: a bare delimiter-less display equation
//! (one or more consecutive lines, the form Claude emits), a `$$ … $$` block,
//! and inline `$ … $` spans within a prose line.

use crate::bare::looks_like_bare_math;
use crate::layout;
use crate::scanner::{Output, Scanner};

/// Replace equation regions in `source` with their 2-D renders, keeping every
/// other line verbatim. `cols` is the terminal width (for wrapping).
pub fn compose(source: &[String], cols: usize) -> Vec<String> {
    compose_mapped(source, cols).0
}

/// Like [`compose`], but also returns, for each source row, the display row it
/// begins at — so a cursor on a source row can be placed in the display.
pub fn compose_mapped(source: &[String], cols: usize) -> (Vec<String>, Vec<usize>) {
    let mut out: Vec<String> = Vec::new();
    let mut map: Vec<usize> = vec![0; source.len()];
    let mut i = 0;
    while i < source.len() {
        let line = &source[i];
        let trimmed = line.trim();
        let indent = indent_of(line);
        let here = out.len();

        // `$$ … $$` display block (possibly spanning several lines).
        if trimmed.starts_with("$$") {
            let (latex, next) = take_dollar_block(source, i);
            push_block(&latex, indent, cols, &mut out);
            for r in i..next.min(source.len()) {
                map[r] = here;
            }
            i = next;
            continue;
        }

        // Bare display equation: consecutive lines that classify as math.
        if looks_like_bare_math(trimmed) {
            let start = i;
            let mut parts: Vec<String> = Vec::new();
            while i < source.len() && looks_like_bare_math(source[i].trim()) {
                parts.push(source[i].trim().to_string());
                i += 1;
            }
            push_block(&parts.join(" "), indent, cols, &mut out);
            for r in start..i {
                map[r] = here;
            }
            continue;
        }

        // A prose line carrying inline `$ … $`: typeset the spans in place.
        if line.contains('$') {
            out.push(render_inline_line(line, cols));
        } else {
            out.push(line.clone());
        }
        map[i] = here;
        i += 1;
    }
    (out, map)
}

/// Collect a `$$ … $$` block starting at line `start`; returns the inner LaTeX
/// and the index just past the closing `$$`.
fn take_dollar_block(source: &[String], start: usize) -> (String, usize) {
    let first = source[start].trim();
    let body = first.trim_start_matches("$$");
    // Single-line `$$ … $$`.
    if let Some(inner) = body.strip_suffix("$$").map(str::trim) {
        if !inner.is_empty() {
            return (inner.to_string(), start + 1);
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if !body.trim().is_empty() {
        parts.push(body.trim().to_string());
    }
    let mut i = start + 1;
    while i < source.len() {
        let t = source[i].trim();
        if let Some(inner) = t.strip_suffix("$$") {
            if !inner.trim().is_empty() {
                parts.push(inner.trim().to_string());
            }
            i += 1;
            break;
        }
        parts.push(t.to_string());
        i += 1;
    }
    (parts.join(" "), i)
}

/// Render a display equation block, indented to match its source position.
fn push_block(latex: &str, indent: usize, cols: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(indent);
    for line in layout::latex_to_lines_wrapped(latex, cols.saturating_sub(indent).max(1)) {
        if line.is_empty() {
            out.push(String::new());
        } else {
            out.push(format!("{pad}{line}"));
        }
    }
}

/// Typeset the inline `$ … $` spans in a single prose line, leaving the rest of
/// the line verbatim. Multi-row inline math is collapsed onto the line as best
/// it can (single-line renders only; a structural fraction stays linear).
fn render_inline_line(line: &str, cols: usize) -> String {
    let mut scanner = Scanner::with_config(true, 4096);
    let mut out = String::new();
    let mut handle = |o: Output, out: &mut String| match o {
        Output::Passthrough(b) => out.push_str(&String::from_utf8_lossy(&b)),
        Output::Math { latex, .. } => {
            let lines = layout::latex_to_lines_wrapped(&latex, cols);
            out.push_str(lines.first().map(String::as_str).unwrap_or(""));
        }
    };
    for o in scanner.feed(line.as_bytes()) {
        handle(o, &mut out);
    }
    for o in scanner.finish() {
        handle(o, &mut out);
    }
    out
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn replaces_bare_equation_preserving_indent() {
        let src = s(&["  Intro:", "  \\nabla \\cdot \\mathbf{v} = 0", "  done"]);
        let out = compose(&src, 80);
        assert_eq!(out[0], "  Intro:");
        assert!(out.iter().any(|l| l == "  ∇ · 𝐯 = 0"), "indented render: {out:?}");
        assert!(out.iter().any(|l| l == "  done"));
        assert!(!out.iter().any(|l| l.contains("\\nabla")));
    }

    #[test]
    fn joins_wrapped_bare_equation() {
        let src = s(&[
            "\\rho \\left( \\frac{\\partial \\mathbf{v}}{\\partial t} \\right) = -\\nabla p +",
            "\\mu \\nabla^2 \\mathbf{v} + \\mathbf{f}",
        ]);
        let out = compose(&src, 80);
        assert!(out.iter().any(|l| l.contains('─')), "fraction bar: {out:?}");
        assert!(out.iter().any(|l| l.contains('ρ')));
        assert!(!out.iter().any(|l| l.contains("\\right")));
    }

    #[test]
    fn renders_dollar_block_and_inline() {
        let src = s(&["$$ \\frac{a}{b} $$", "Energy $E = mc^2$ here"]);
        let out = compose(&src, 80);
        assert!(out.iter().any(|l| l.contains('─')), "display fraction: {out:?}");
        assert!(out.iter().any(|l| l.contains("E = mc²")), "inline kept on line: {out:?}");
    }

    #[test]
    fn leaves_non_math_lines_verbatim() {
        let src = s(&["│  just a UI box line  │", "plain text"]);
        let out = compose(&src, 80);
        assert_eq!(out, src);
    }
}
