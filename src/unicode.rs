//! LaTeX → Unicode text conversion.
//!
//! An alternative to image rendering: map LaTeX math to Unicode symbols and
//! substitute them inline as plain text. Because the result is *text*, it flows
//! through any program — including self-repainting TUIs like interactive Claude
//! Code — with none of the screen-ownership problems that image injection has.
//!
//! Trade-off: a single line of text can't express true 2-D layout, so stacked
//! structures degrade gracefully (`\frac{a}{b}` → `a/b`, superscripts use
//! Unicode super/subscripts where the characters exist, else a linear fallback).
//! For full-fidelity 2-D math, use the image renderer on line-oriented output.

/// Convert a LaTeX math fragment (delimiters already stripped) to a Unicode
/// approximation. Best-effort and total: unknown constructs pass through as
/// readable text rather than failing.
pub fn latex_to_unicode(latex: &str) -> String {
    let tokens = tokenize(latex);
    let mut out = String::new();
    render_tokens(&tokens, &mut out);
    // Collapse the runs of spaces that stripping commands can leave behind.
    squeeze_spaces(&out)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A `\command` (name without the backslash).
    Cmd(String),
    /// A `{ ... }` group (already tokenized).
    Group(Vec<Tok>),
    /// `^` superscript marker.
    Sup,
    /// `_` subscript marker.
    Sub,
    /// A single literal character.
    Ch(char),
}

fn tokenize(s: &str) -> Vec<Tok> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    tokenize_until(&chars, &mut i, false)
}

/// Tokenize until end (or, when `stop_at_brace`, until a closing `}`).
fn tokenize_until(chars: &[char], i: &mut usize, stop_at_brace: bool) -> Vec<Tok> {
    let mut toks = Vec::new();
    while *i < chars.len() {
        let c = chars[*i];
        match c {
            '}' if stop_at_brace => {
                *i += 1;
                break;
            }
            '\\' => {
                *i += 1;
                // Command name = following ASCII letters; or a single symbol
                // (e.g. `\,`, `\{`).
                let start = *i;
                while *i < chars.len() && chars[*i].is_ascii_alphabetic() {
                    *i += 1;
                }
                if *i > start {
                    toks.push(Tok::Cmd(chars[start..*i].iter().collect()));
                } else if *i < chars.len() {
                    // Escaped symbol like `\,` or `\{`.
                    toks.push(Tok::Cmd(chars[*i].to_string()));
                    *i += 1;
                }
            }
            '{' => {
                *i += 1;
                let inner = tokenize_until(chars, i, true);
                toks.push(Tok::Group(inner));
            }
            '^' => {
                toks.push(Tok::Sup);
                *i += 1;
            }
            '_' => {
                toks.push(Tok::Sub);
                *i += 1;
            }
            _ => {
                toks.push(Tok::Ch(c));
                *i += 1;
            }
        }
    }
    toks
}

fn render_tokens(toks: &[Tok], out: &mut String) {
    let mut idx = 0;
    while idx < toks.len() {
        match &toks[idx] {
            Tok::Cmd(name) => {
                idx += 1;
                render_command(name, toks, &mut idx, out);
            }
            Tok::Group(inner) => {
                render_tokens(inner, out);
                idx += 1;
            }
            Tok::Sup => {
                idx += 1;
                render_script(toks, &mut idx, out, true);
            }
            Tok::Sub => {
                idx += 1;
                render_script(toks, &mut idx, out, false);
            }
            Tok::Ch(c) => {
                out.push(*c);
                idx += 1;
            }
        }
    }
}

/// Render a `\command`, consuming following argument groups where needed
/// (`\frac{a}{b}`, `\sqrt{x}`, `\mathbf{v}`).
fn render_command(name: &str, toks: &[Tok], idx: &mut usize, out: &mut String) {
    match name {
        // Two-argument fraction -> a/b (parenthesize compound numerators).
        "frac" | "tfrac" | "dfrac" => {
            let a = take_group_string(toks, idx);
            let b = take_group_string(toks, idx);
            push_fraction(&a, &b, out);
        }
        "sqrt" => {
            let a = take_group_string(toks, idx);
            out.push('√');
            if a.chars().count() > 1 {
                out.push('(');
                out.push_str(&a);
                out.push(')');
            } else {
                out.push_str(&a);
            }
        }
        // Font wrappers: render the content, bolded for \mathbf.
        "mathbf" | "bm" | "boldsymbol" => {
            let a = take_group_string(toks, idx);
            out.push_str(&boldify(&a));
        }
        "mathrm" | "mathit" | "mathsf" | "mathcal" | "mathbb" | "text" | "operatorname" => {
            out.push_str(&take_group_string(toks, idx));
        }
        // Spacing commands.
        "," | ":" | ";" | "!" | " " | "quad" | "qquad" => out.push(' '),
        "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "displaystyle" => {} // structural, drop
        _ => {
            if let Some(sym) = symbol(name) {
                out.push_str(sym);
            } else {
                // Unknown macro: emit its name (readable fallback).
                out.push_str(name);
            }
        }
    }
}

/// Render a super/subscript: prefer Unicode super/subscript chars; if any
/// character isn't representable, fall back to `^(...)` / `_(...)`.
fn render_script(toks: &[Tok], idx: &mut usize, out: &mut String, sup: bool) {
    let body = take_one_string(toks, idx);
    let mapped: Option<String> = body
        .chars()
        .map(|c| if sup { superscript(c) } else { subscript(c) })
        .collect();
    match mapped {
        Some(s) if !s.is_empty() => out.push_str(&s),
        _ => {
            out.push(if sup { '^' } else { '_' });
            if body.chars().count() > 1 {
                out.push('(');
                out.push_str(&body);
                out.push(')');
            } else {
                out.push_str(&body);
            }
        }
    }
}

/// Render the next single token (a group, command, or char) to a string.
fn take_one_string(toks: &[Tok], idx: &mut usize) -> String {
    if *idx >= toks.len() {
        return String::new();
    }
    let mut s = String::new();
    match &toks[*idx] {
        Tok::Group(inner) => {
            render_tokens(inner, &mut s);
            *idx += 1;
        }
        Tok::Cmd(name) => {
            *idx += 1;
            render_command(name, toks, idx, &mut s);
        }
        Tok::Ch(c) => {
            s.push(*c);
            *idx += 1;
        }
        Tok::Sup | Tok::Sub => {
            *idx += 1;
        }
    }
    s
}

/// Render the next group (or single token) as a string — for command arguments.
fn take_group_string(toks: &[Tok], idx: &mut usize) -> String {
    // Skip whitespace-only chars between a command and its argument.
    while *idx < toks.len() {
        if let Tok::Ch(c) = &toks[*idx] {
            if c.is_whitespace() {
                *idx += 1;
                continue;
            }
        }
        break;
    }
    take_one_string(toks, idx)
}

fn push_fraction(a: &str, b: &str, out: &mut String) {
    let wrap = |s: &str| -> String {
        if s.chars().count() > 1 {
            format!("({s})")
        } else {
            s.to_string()
        }
    };
    out.push_str(&wrap(a));
    out.push('/');
    out.push_str(&wrap(b));
}

/// Map ASCII letters to the Unicode "mathematical bold" block, leaving other
/// characters unchanged.
fn boldify(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' => char::from_u32(0x1D400 + (c as u32 - 'A' as u32)).unwrap_or(c),
            'a'..='z' => char::from_u32(0x1D41A + (c as u32 - 'a' as u32)).unwrap_or(c),
            '0'..='9' => char::from_u32(0x1D7CE + (c as u32 - '0' as u32)).unwrap_or(c),
            _ => c,
        })
        .collect()
}

fn squeeze_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(c);
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰', '1' => '¹', '2' => '²', '3' => '³', '4' => '⁴',
        '5' => '⁵', '6' => '⁶', '7' => '⁷', '8' => '⁸', '9' => '⁹',
        '+' => '⁺', '-' => '⁻', '=' => '⁼', '(' => '⁽', ')' => '⁾',
        'n' => 'ⁿ', 'i' => 'ⁱ', 'a' => 'ᵃ', 'b' => 'ᵇ', 'c' => 'ᶜ',
        'd' => 'ᵈ', 'e' => 'ᵉ', 'k' => 'ᵏ', 'm' => 'ᵐ', 'p' => 'ᵖ',
        't' => 'ᵗ', 'x' => 'ˣ', 'y' => 'ʸ', ' ' => ' ',
        _ => return None,
    })
}

fn subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀', '1' => '₁', '2' => '₂', '3' => '₃', '4' => '₄',
        '5' => '₅', '6' => '₆', '7' => '₇', '8' => '₈', '9' => '₉',
        '+' => '₊', '-' => '₋', '=' => '₌', '(' => '₍', ')' => '₎',
        'a' => 'ₐ', 'e' => 'ₑ', 'i' => 'ᵢ', 'j' => 'ⱼ', 'o' => 'ₒ',
        'x' => 'ₓ', 'n' => 'ₙ', 'm' => 'ₘ', 't' => 'ₜ', ' ' => ' ',
        _ => return None,
    })
}

/// Map a LaTeX command name to a Unicode symbol.
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // lowercase Greek
        "alpha" => "α", "beta" => "β", "gamma" => "γ", "delta" => "δ",
        "epsilon" => "ε", "varepsilon" => "ε", "zeta" => "ζ", "eta" => "η",
        "theta" => "θ", "vartheta" => "ϑ", "iota" => "ι", "kappa" => "κ",
        "lambda" => "λ", "mu" => "μ", "nu" => "ν", "xi" => "ξ", "pi" => "π",
        "varpi" => "ϖ", "rho" => "ρ", "varrho" => "ϱ", "sigma" => "σ",
        "varsigma" => "ς", "tau" => "τ", "upsilon" => "υ", "phi" => "φ",
        "varphi" => "ϕ", "chi" => "χ", "psi" => "ψ", "omega" => "ω",
        // uppercase Greek
        "Gamma" => "Γ", "Delta" => "Δ", "Theta" => "Θ", "Lambda" => "Λ",
        "Xi" => "Ξ", "Pi" => "Π", "Sigma" => "Σ", "Upsilon" => "Υ",
        "Phi" => "Φ", "Psi" => "Ψ", "Omega" => "Ω",
        // operators & relations
        "times" => "×", "div" => "÷", "cdot" => "·", "ast" => "∗",
        "pm" => "±", "mp" => "∓", "leq" | "le" => "≤", "geq" | "ge" => "≥",
        "neq" | "ne" => "≠", "approx" => "≈", "equiv" => "≡", "sim" => "∼",
        "simeq" => "≃", "cong" => "≅", "propto" => "∝", "ll" => "≪",
        "gg" => "≫", "subset" => "⊂", "subseteq" => "⊆", "supset" => "⊃",
        "supseteq" => "⊇", "in" => "∈", "notin" => "∉", "ni" => "∋",
        "cup" => "∪", "cap" => "∩", "setminus" => "∖", "emptyset" => "∅",
        "oplus" => "⊕", "otimes" => "⊗", "wedge" => "∧", "vee" => "∨",
        // big operators
        "sum" => "∑", "prod" => "∏", "int" => "∫", "iint" => "∬",
        "oint" => "∮", "coprod" => "∐", "bigcup" => "⋃", "bigcap" => "⋂",
        // calculus / misc
        "nabla" => "∇", "partial" => "∂", "infty" => "∞", "forall" => "∀",
        "exists" => "∃", "neg" => "¬", "angle" => "∠", "triangle" => "△",
        "perp" => "⊥", "parallel" => "∥", "cdots" => "⋯", "ldots" => "…",
        "dots" => "…", "vdots" => "⋮", "ddots" => "⋱", "prime" => "′",
        "hbar" => "ℏ", "ell" => "ℓ", "Re" => "ℜ", "Im" => "ℑ",
        "aleph" => "ℵ", "degree" => "°", "circ" => "∘", "bullet" => "•",
        "star" => "⋆", "dagger" => "†", "surd" => "√",
        // arrows
        "to" | "rightarrow" => "→", "leftarrow" => "←", "leftrightarrow" => "↔",
        "Rightarrow" => "⇒", "Leftarrow" => "⇐", "Leftrightarrow" => "⇔",
        "mapsto" => "↦", "uparrow" => "↑", "downarrow" => "↓",
        "langle" => "⟨", "rangle" => "⟩",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navier_stokes_momentum() {
        let got = latex_to_unicode(
            "\\rho \\left( \\frac{\\partial \\mathbf{v}}{\\partial t} + \\mathbf{v} \\cdot \\nabla \\mathbf{v} \\right) = -\\nabla p + \\mu \\nabla^2 \\mathbf{v} + \\mathbf{f}",
        );
        // Spot-check the salient pieces rather than the exact spacing.
        assert!(got.contains('ρ'));
        assert!(got.contains('∂'));
        assert!(got.contains('∇'));
        assert!(got.contains('·'));
        assert!(got.contains('μ'));
        assert!(got.contains("∇²") || got.contains('²'));
        assert!(!got.contains('\\'), "no backslashes should remain: {got}");
        assert!(!got.contains("left") && !got.contains("right"));
    }

    #[test]
    fn continuity() {
        assert_eq!(latex_to_unicode("\\nabla \\cdot \\mathbf{v} = 0"), "∇ · 𝐯 = 0");
    }

    #[test]
    fn fractions_linearize() {
        assert_eq!(latex_to_unicode("\\frac{a}{b}"), "a/b");
        assert_eq!(latex_to_unicode("\\frac{a+b}{c}"), "(a+b)/c");
    }

    #[test]
    fn superscripts_and_subscripts() {
        assert_eq!(latex_to_unicode("x^2"), "x²");
        assert_eq!(latex_to_unicode("x^{2}"), "x²");
        assert_eq!(latex_to_unicode("a_i"), "aᵢ");
        assert_eq!(latex_to_unicode("x^{n+1}"), "xⁿ⁺¹");
        assert_eq!(latex_to_unicode("e^{-x}"), "e⁻ˣ");
    }

    #[test]
    fn unmappable_script_falls_back() {
        // 'z' has no subscript glyph -> linear fallback.
        assert_eq!(latex_to_unicode("a_z"), "a_z");
        assert_eq!(latex_to_unicode("x^{abz}"), "x^(abz)");
    }

    #[test]
    fn common_symbols() {
        assert_eq!(latex_to_unicode("\\alpha + \\beta"), "α + β");
        assert_eq!(latex_to_unicode("\\sum_{i=1}^{n}"), "∑ᵢ₌₁ⁿ");
        assert_eq!(latex_to_unicode("\\sqrt{2}"), "√2");
        assert_eq!(latex_to_unicode("a \\leq b \\times c"), "a ≤ b × c");
    }

    #[test]
    fn unknown_macro_is_readable() {
        // No crash, no backslash; emits the name.
        let got = latex_to_unicode("\\foobar x");
        assert!(!got.contains('\\'));
        assert!(got.contains("foobar"));
    }

    #[test]
    fn total_no_panics_on_messy_input() {
        for s in ["", "\\", "{", "}", "^", "_", "\\frac{}{}", "^^__", "\\\\"] {
            let _ = latex_to_unicode(s); // must not panic
        }
    }
}
