//! Compose a *display* screen from a reconstructed *source* screen.
//!
//! The source rows are **styled** (they carry the program's SGR colors). We
//! detect equations on the plain (ANSI-stripped) text but emit the original
//! styled line for everything that *isn't* an equation — so Claude's colors,
//! boxes, and input chrome stay pixel-identical, and only the equations are
//! re-typeset as 2-D text in place.

use crate::bare::looks_like_bare_math;
use crate::layout;

/// Replace equation regions with their 2-D renders; keep every other (styled)
/// line verbatim. `cols` is the terminal width (for wrapping).
pub fn compose(source: &[String], cols: usize) -> Vec<String> {
    compose_mapped(source, cols).0
}

/// Like [`compose`], but also returns, for each source row, the display row it
/// begins at — so a cursor on a source row can be placed in the display.
pub fn compose_mapped(source: &[String], cols: usize) -> (Vec<String>, Vec<usize>) {
    let plain: Vec<String> = source.iter().map(|l| strip_ansi(l)).collect();
    let mut out: Vec<String> = Vec::new();
    let mut map: Vec<usize> = vec![0; source.len()];
    let mut i = 0;
    while i < source.len() {
        let trimmed = plain[i].trim();
        let indent = indent_of(&plain[i]);
        let here = out.len();

        // `$$ … $$` display block (possibly spanning several lines).
        if trimmed.starts_with("$$") {
            let (latex, next) = take_dollar_block(&plain, i);
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
            while i < source.len() && looks_like_bare_math(plain[i].trim()) {
                parts.push(plain[i].trim().to_string());
                i += 1;
            }
            push_block(&parts.join(" "), indent, cols, &mut out);
            for r in start..i {
                map[r] = here;
            }
            continue;
        }

        // Everything else: keep the styled line exactly as the program drew it.
        out.push(source[i].clone());
        map[i] = here;
        i += 1;
    }
    (out, map)
}

/// Collect a `$$ … $$` block starting at `start`; returns the inner LaTeX and
/// the index just past the closing `$$`.
fn take_dollar_block(plain: &[String], start: usize) -> (String, usize) {
    let first = plain[start].trim();
    let body = first.trim_start_matches("$$");
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
    while i < plain.len() {
        let t = plain[i].trim();
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

/// Drop ANSI escape sequences, leaving the plain glyphs (for detection).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next(); // consume the CSI introducer
                // Parameters/intermediates are 0x20..0x3f; the final byte that
                // ends the sequence is 0x40..0x7e (e.g. 'm', 'H', 'K').
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next(); // OSC: consume until BEL or ESC
                for c in chars.by_ref() {
                    if c == '\u{7}' || c == '\u{1b}' {
                        break;
                    }
                }
            }
            _ => {
                chars.next(); // other two-byte escape: drop the next byte
            }
        }
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
    fn keeps_styled_non_equation_lines_verbatim() {
        // A colored UI line must survive byte-for-byte (colors preserved).
        let colored = "\x1b[38;5;1m❯ prompt\x1b[0m";
        let src = vec![colored.to_string(), "\\nabla \\cdot \\mathbf{v} = 0".to_string()];
        let out = compose(&src, 80);
        assert_eq!(out[0], colored, "styled line unchanged");
        assert!(out.iter().any(|l| l.contains("∇ · 𝐯 = 0")));
    }

    #[test]
    fn detects_equation_even_when_styled() {
        // The equation carries color codes; detection strips them, render is clean.
        let src = vec!["\x1b[2m  \\frac{\\partial u}{\\partial t} = 0\x1b[0m".to_string()];
        let out = compose(&src, 80);
        assert!(out.iter().any(|l| l.contains('─')), "fraction rendered: {out:?}");
        assert!(!out.iter().any(|l| l.contains("\\frac")));
    }

    #[test]
    fn strip_ansi_handles_multi_param_sgr() {
        // The bug: breaking on '[' left the params as text. Multi-param SGR and
        // truecolor must be fully removed.
        assert_eq!(strip_ansi("\x1b[0;2;38;5;4mρ\x1b[0m"), "ρ");
        assert_eq!(strip_ansi("\x1b[1;31mred\x1b[0m x"), "red x");
        assert_eq!(strip_ansi("\x1b[38;2;10;20;30mc\x1b[0m"), "c");
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
}
