//! A miniature TeX math-layout engine that renders to a character grid.
//!
//! This mimics, at character-cell resolution, the box-and-glue model TeX uses
//! for math (TeXbook Appendix G):
//!
//!  * Every subexpression is a **box** — a rectangle of text rows with a
//!    *baseline* (the row neighbours align on). `Box2 { lines, baseline }`.
//!    Rows above the baseline are its *ascent*, rows below its *depth*.
//!  * Boxes compose **horizontally** by aligning baselines (`hcat`) and
//!    **vertically** for fractions / limits (`frac`, `limits`).
//!  * Constructs follow Appendix-G-shaped rules: fractions stack
//!    numerator / rule / denominator centred on the baseline; sub/superscripts
//!    shift above and below it; radicals get a vinculum; big operators put
//!    their limits above and below in display style; `\left … \right`
//!    delimiters grow to the height of their contents.
//!  * Between atoms we insert spacing from a collapsed version of TeX's 8×8
//!    inter-atom **class** table (Ord/Op/Bin/Rel/Open/Close/Punct) — the reason
//!    `a+b`, `ab` and `a=b` are spaced differently.
//!
//! What does *not* port is the bottom of TeX's stack: the sub-pixel
//! `\fontdimen` shift amounts. A terminal cell is the smallest unit we have, so
//! those become integer rows and columns.
//!
//! The result is multi-line text, so — like the image path — it belongs on
//! line-oriented output (scripts, `claude -p`, markdown), and unlike the image
//! path it needs no graphics protocol, making it the best option on terminals
//! without Kitty graphics.

use crate::unicode::{boldify, subscript, superscript, symbol, tokenize, Tok};

/// TeX atom classes, used only to choose inter-atom spacing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Ord,
    Op,
    Bin,
    Rel,
    Open,
    Close,
    Punct,
}

/// A laid-out box: equal-conceptual-width rows plus the baseline row index.
#[derive(Clone, Debug)]
struct Box2 {
    lines: Vec<String>,
    baseline: usize,
}

/// Display width of a string (one column per `char`; every glyph we emit is
/// single-width).
fn dw(s: &str) -> usize {
    s.chars().count()
}

fn pad_right(line: &str, w: usize) -> String {
    let n = dw(line);
    if n >= w {
        line.to_string()
    } else {
        format!("{line}{}", " ".repeat(w - n))
    }
}

fn center(line: &str, w: usize) -> String {
    let n = dw(line);
    if n >= w {
        return line.to_string();
    }
    let total = w - n;
    let left = total / 2;
    let right = total - left;
    format!("{}{line}{}", " ".repeat(left), " ".repeat(right))
}

impl Box2 {
    fn leaf(s: &str) -> Box2 {
        Box2 {
            lines: vec![s.to_string()],
            baseline: 0,
        }
    }

    fn empty() -> Box2 {
        Box2 {
            lines: vec![String::new()],
            baseline: 0,
        }
    }

    fn width(&self) -> usize {
        self.lines.iter().map(|l| dw(l)).max().unwrap_or(0)
    }

    fn height(&self) -> usize {
        self.lines.len()
    }

    fn ascent(&self) -> usize {
        self.baseline
    }

    fn depth(&self) -> usize {
        self.height().saturating_sub(self.baseline + 1)
    }
}

/// Place two boxes side by side, aligning their baselines.
fn hcat(a: &Box2, b: &Box2) -> Box2 {
    let baseline = a.ascent().max(b.ascent());
    let depth = a.depth().max(b.depth());
    let height = baseline + depth + 1;
    let aw = a.width();
    let bw = b.width();
    let a_top = baseline - a.ascent();
    let b_top = baseline - b.ascent();
    let mut lines = Vec::with_capacity(height);
    for r in 0..height {
        let al = a
            .lines
            .get(r.wrapping_sub(a_top))
            .filter(|_| r >= a_top)
            .map(|l| pad_right(l, aw))
            .unwrap_or_else(|| " ".repeat(aw));
        let bl = b
            .lines
            .get(r.wrapping_sub(b_top))
            .filter(|_| r >= b_top)
            .map(|l| pad_right(l, bw))
            .unwrap_or_else(|| " ".repeat(bw));
        lines.push(format!("{al}{bl}"));
    }
    Box2 { lines, baseline }
}

/// Numerator over a rule over denominator, centred and baselined on the rule —
/// TeX's fraction sitting on the math axis.
fn frac(num: &Box2, den: &Box2) -> Box2 {
    let inner = num.width().max(den.width()).max(1);
    let barw = inner + 2; // small overhang on each side
    let mut lines = Vec::with_capacity(num.height() + 1 + den.height());
    for l in &num.lines {
        lines.push(center(l, barw));
    }
    let baseline = num.height();
    lines.push("─".repeat(barw));
    for l in &den.lines {
        lines.push(center(l, barw));
    }
    Box2 { lines, baseline }
}

/// A radical: a surd on the baseline row with a vinculum roof over the
/// radicand.
fn radical(inner: &Box2) -> Box2 {
    let w = inner.width();
    let mut lines = Vec::with_capacity(inner.height() + 1);
    lines.push(format!(" {}", "─".repeat(w)));
    for (i, l) in inner.lines.iter().enumerate() {
        let prefix = if i == 0 { '√' } else { ' ' };
        lines.push(format!("{prefix}{}", pad_right(l, w)));
    }
    Box2 {
        lines,
        baseline: inner.baseline + 1,
    }
}

/// Attach a superscript and/or subscript as 2-D boxes to the right of `base`.
/// (The compact single-character case is handled earlier via Unicode
/// super/subscripts; this is the structural fallback.)
fn stack_scripts(base: &Box2, sup: Option<&Box2>, sub: Option<&Box2>) -> Box2 {
    let sw = sup
        .map(|b| b.width())
        .unwrap_or(0)
        .max(sub.map(|b| b.width()).unwrap_or(0));
    let mut right = Vec::new();
    let sup_h = sup.map(|b| b.height()).unwrap_or(0);
    if let Some(s) = sup {
        for l in &s.lines {
            right.push(pad_right(l, sw));
        }
    }
    for _ in 0..base.height() {
        right.push(" ".repeat(sw));
    }
    if let Some(u) = sub {
        for l in &u.lines {
            right.push(pad_right(l, sw));
        }
    }
    let right_box = Box2 {
        lines: right,
        baseline: sup_h + base.baseline,
    };
    hcat(base, &right_box)
}

/// Stack `over` / operator / `under`, centred — display-style big-operator
/// limits.
fn limits(op: &Box2, over: Option<&Box2>, under: Option<&Box2>) -> Box2 {
    let w = op
        .width()
        .max(over.map(|b| b.width()).unwrap_or(0))
        .max(under.map(|b| b.width()).unwrap_or(0));
    let mut lines = Vec::new();
    let mut top = 0;
    if let Some(o) = over {
        for l in &o.lines {
            lines.push(center(l, w));
        }
        top = o.height();
    }
    let baseline = top + op.baseline;
    for l in &op.lines {
        lines.push(center(l, w));
    }
    if let Some(u) = under {
        for l in &u.lines {
            lines.push(center(l, w));
        }
    }
    Box2 { lines, baseline }
}

/// Grow a delimiter glyph to `height` rows, aligned to `baseline`. Returns a
/// zero-width box for the null delimiter `.`.
fn grow_delim(sym: &str, height: usize, baseline: usize) -> Box2 {
    if sym.is_empty() {
        return Box2 {
            lines: vec![String::new(); height],
            baseline,
        };
    }
    if height <= 1 {
        return Box2 {
            lines: vec![sym.to_string()],
            baseline: 0,
        };
    }
    // (top, middle, bottom) pieces for stretchable delimiters.
    let (top, mid, bot) = match sym {
        "(" => ("⎛", "⎜", "⎝"),
        ")" => ("⎞", "⎟", "⎠"),
        "[" => ("⎡", "⎢", "⎣"),
        "]" => ("⎤", "⎥", "⎦"),
        "{" => ("⎧", "⎪", "⎩"),
        "}" => ("⎫", "⎪", "⎭"),
        "|" => ("│", "│", "│"),
        "‖" => ("║", "║", "║"),
        // Anything else (e.g. ⟨ ⟩): repeat the glyph.
        other => (other, other, other),
    };
    let mut lines = Vec::with_capacity(height);
    for r in 0..height {
        let piece = if r == 0 {
            top
        } else if r == height - 1 {
            bot
        } else if (sym == "{" || sym == "}") && r == height / 2 {
            // The brace's pointing middle.
            if sym == "{" {
                "⎨"
            } else {
                "⎬"
            }
        } else {
            mid
        };
        lines.push(piece.to_string());
    }
    Box2 { lines, baseline }
}

// ---------------------------------------------------------------------------
// Parse: token stream -> node tree (nuclei with attached scripts, fractions,
// radicals, grown delimiters).
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Node {
    Sym(String, Class),
    Row(Vec<Node>),
    Frac(Vec<Node>, Vec<Node>),
    Sqrt(Vec<Node>),
    Delim(String, Vec<Node>, String),
    Scripts {
        base: Box<Node>,
        sup: Option<Vec<Node>>,
        sub: Option<Vec<Node>>,
        op: bool,
    },
}

/// Commands that contribute no glyph; spacing is recovered from the class table.
fn is_skippable(name: &str) -> bool {
    matches!(
        name,
        "," | ":" | ";" | "!" | " " | "quad" | "qquad" | "displaystyle" | "limits" | "nolimits"
            | "\\"
    )
}

fn is_limits_glyph(s: &str) -> bool {
    matches!(s, "∑" | "∏" | "∐" | "⋃" | "⋂")
}

fn classify_char(c: char) -> Class {
    match c {
        '+' | '-' | '*' => Class::Bin,
        '=' | '<' | '>' => Class::Rel,
        '(' | '[' => Class::Open,
        ')' | ']' => Class::Close,
        ',' | ';' => Class::Punct,
        _ => Class::Ord,
    }
}

fn cmd_class(name: &str) -> Class {
    match name {
        "times" | "div" | "cdot" | "ast" | "pm" | "mp" | "cup" | "cap" | "setminus" | "oplus"
        | "otimes" | "wedge" | "vee" | "star" | "bullet" | "circ" => Class::Bin,
        "leq" | "le" | "geq" | "ge" | "neq" | "ne" | "approx" | "equiv" | "sim" | "simeq"
        | "cong" | "propto" | "ll" | "gg" | "subset" | "subseteq" | "supset" | "supseteq"
        | "in" | "notin" | "ni" | "to" | "rightarrow" | "leftarrow" | "leftrightarrow"
        | "Rightarrow" | "Leftarrow" | "Leftrightarrow" | "mapsto" => Class::Rel,
        "sum" | "prod" | "coprod" | "bigcup" | "bigcap" | "int" | "iint" | "oint" => Class::Op,
        _ => Class::Ord,
    }
}

/// The flat plain text of a node list (for font wrappers like `\mathbf`).
fn seq_plain(nodes: &[Node]) -> String {
    let mut s = String::new();
    for n in nodes {
        match n {
            Node::Sym(t, _) => s.push_str(t),
            Node::Row(inner) => s.push_str(&seq_plain(inner)),
            _ => {}
        }
    }
    s
}

fn parse(latex: &str) -> Vec<Node> {
    let toks = tokenize(latex);
    parse_seq(&toks)
}

fn parse_seq(toks: &[Tok]) -> Vec<Node> {
    let mut i = 0;
    let mut out: Vec<Node> = Vec::new();
    while i < toks.len() {
        match &toks[i] {
            // Inter-token whitespace is insignificant in math; all spacing comes
            // from the class table.
            Tok::Ch(c) if c.is_whitespace() => i += 1,
            Tok::Cmd(n) if is_skippable(n) => i += 1,
            Tok::Sup | Tok::Sub => {
                // A script with no preceding nucleus: attach to the previous
                // node if any, else drop the stray marker and its argument.
                if let Some(prev) = out.pop() {
                    out.push(parse_scripts(prev, toks, &mut i));
                } else {
                    i += 1;
                    let _ = parse_arg(toks, &mut i);
                }
            }
            _ => {
                if let Some(node) = parse_node(toks, &mut i) {
                    out.push(parse_scripts(node, toks, &mut i));
                } else {
                    i += 1;
                }
            }
        }
    }
    out
}

fn parse_node(toks: &[Tok], i: &mut usize) -> Option<Node> {
    match &toks[*i] {
        Tok::Group(inner) => {
            *i += 1;
            Some(Node::Row(parse_seq(inner)))
        }
        Tok::Ch(c) => {
            let ch = *c;
            *i += 1;
            Some(Node::Sym(ch.to_string(), classify_char(ch)))
        }
        Tok::Cmd(name) => {
            let name = name.clone();
            *i += 1;
            Some(parse_cmd(&name, toks, i))
        }
        Tok::Sup | Tok::Sub => None,
    }
}

fn parse_cmd(name: &str, toks: &[Tok], i: &mut usize) -> Node {
    match name {
        "frac" | "dfrac" | "tfrac" => {
            let a = parse_arg(toks, i);
            let b = parse_arg(toks, i);
            Node::Frac(a, b)
        }
        "sqrt" => Node::Sqrt(parse_arg(toks, i)),
        "left" => parse_left_right(toks, i),
        "right" => Node::Sym(String::new(), Class::Ord),
        "mathbf" | "bm" | "boldsymbol" => {
            let a = parse_arg(toks, i);
            Node::Sym(boldify(&seq_plain(&a)), Class::Ord)
        }
        "mathrm" | "mathit" | "mathsf" | "mathcal" | "mathbb" | "text" | "operatorname" => {
            let a = parse_arg(toks, i);
            Node::Sym(seq_plain(&a), Class::Ord)
        }
        _ => {
            if let Some(sym) = symbol(name) {
                Node::Sym(sym.to_string(), cmd_class(name))
            } else {
                // Unknown macro: emit its name (readable fallback, no backslash).
                Node::Sym(name.to_string(), Class::Ord)
            }
        }
    }
}

/// Parse one command argument: a `{ … }` group, or the single following node.
fn parse_arg(toks: &[Tok], i: &mut usize) -> Vec<Node> {
    while *i < toks.len() {
        match &toks[*i] {
            Tok::Ch(c) if c.is_whitespace() => *i += 1,
            Tok::Cmd(n) if is_skippable(n) => *i += 1,
            _ => break,
        }
    }
    if *i >= toks.len() {
        return Vec::new();
    }
    if let Tok::Group(inner) = &toks[*i] {
        *i += 1;
        return parse_seq(inner);
    }
    match parse_node(toks, i) {
        Some(node) => vec![parse_scripts(node, toks, i)],
        None => Vec::new(),
    }
}

fn parse_left_right(toks: &[Tok], i: &mut usize) -> Node {
    let open = read_delim(toks, i);
    let start = *i;
    let mut depth = 0usize;
    while *i < toks.len() {
        match &toks[*i] {
            Tok::Cmd(n) if n == "left" => {
                depth += 1;
                *i += 1;
            }
            Tok::Cmd(n) if n == "right" => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                *i += 1;
            }
            _ => *i += 1,
        }
    }
    let inner = parse_seq(&toks[start..*i]);
    let close = if *i < toks.len() {
        *i += 1; // consume \right
        read_delim(toks, i)
    } else {
        String::new()
    };
    Node::Delim(open, inner, close)
}

/// Read the delimiter glyph following `\left` / `\right`. `.` is the null
/// delimiter (empty string).
fn read_delim(toks: &[Tok], i: &mut usize) -> String {
    if *i >= toks.len() {
        return String::new();
    }
    let s = match &toks[*i] {
        Tok::Ch('.') => String::new(),
        Tok::Ch(c) => c.to_string(),
        Tok::Cmd(name) => match name.as_str() {
            "langle" => "⟨".to_string(),
            "rangle" => "⟩".to_string(),
            "{" | "lbrace" => "{".to_string(),
            "}" | "rbrace" => "}".to_string(),
            "|" | "vert" | "lvert" | "rvert" => "|".to_string(),
            "Vert" | "lVert" | "rVert" => "‖".to_string(),
            other => symbol(other).unwrap_or("").to_string(),
        },
        _ => String::new(),
    };
    *i += 1;
    s
}

fn parse_scripts(node: Node, toks: &[Tok], i: &mut usize) -> Node {
    let mut sup = None;
    let mut sub = None;
    loop {
        match toks.get(*i) {
            Some(Tok::Sup) => {
                *i += 1;
                sup = Some(parse_arg(toks, i));
            }
            Some(Tok::Sub) => {
                *i += 1;
                sub = Some(parse_arg(toks, i));
            }
            _ => break,
        }
    }
    if sup.is_none() && sub.is_none() {
        return node;
    }
    let op = matches!(&node, Node::Sym(s, _) if is_limits_glyph(s));
    Node::Scripts {
        base: Box::new(node),
        sup,
        sub,
        op,
    }
}

// ---------------------------------------------------------------------------
// Layout: node tree -> Box2.
// ---------------------------------------------------------------------------

fn layout_node(node: &Node) -> (Box2, Class) {
    match node {
        Node::Sym(s, c) => (Box2::leaf(s), *c),
        Node::Row(seq) => (layout_seq(seq), Class::Ord),
        Node::Frac(a, b) => (frac(&layout_seq(a), &layout_seq(b)), Class::Ord),
        Node::Sqrt(a) => (radical(&layout_seq(a)), Class::Ord),
        Node::Delim(open, inner, close) => (layout_delim(open, inner, close), Class::Ord),
        Node::Scripts {
            base,
            sup,
            sub,
            op,
        } => layout_scripts(base, sup.as_deref(), sub.as_deref(), *op),
    }
}

fn layout_delim(open: &str, inner: &[Node], close: &str) -> Box2 {
    let body = layout_seq(inner);
    let left = grow_delim(open, body.height(), body.baseline);
    let right = grow_delim(close, body.height(), body.baseline);
    hcat(&hcat(&left, &body), &right)
}

fn layout_scripts(base: &Node, sup: Option<&[Node]>, sub: Option<&[Node]>, op: bool) -> (Box2, Class) {
    let (base_box, base_class) = layout_node(base);
    if op {
        let over = sup.map(layout_seq);
        let under = sub.map(layout_seq);
        return (limits(&base_box, over.as_ref(), under.as_ref()), Class::Op);
    }
    // Compact path: single-line base with scripts that map to Unicode
    // super/subscripts — TeX's "script style" rendered as small raised glyphs.
    if let Some(inline) = try_inline(&base_box, sup, sub) {
        return (Box2::leaf(&inline), base_class);
    }
    let sup_box = sup.map(layout_seq);
    let sub_box = sub.map(layout_seq);
    (
        stack_scripts(&base_box, sup_box.as_ref(), sub_box.as_ref()),
        base_class,
    )
}

/// If the base is a single row and every script character has a Unicode
/// super/subscript form, render inline (e.g. `x^{n+1}` -> `xⁿ⁺¹`). Spaces in
/// scripts are dropped first so class spacing doesn't leak in.
fn try_inline(base_box: &Box2, sup: Option<&[Node]>, sub: Option<&[Node]>) -> Option<String> {
    if base_box.height() != 1 {
        return None;
    }
    let mut s = base_box.lines[0].clone();
    if let Some(nodes) = sub {
        let plain = single_line(nodes)?;
        for c in plain.chars().filter(|c| !c.is_whitespace()) {
            s.push(subscript(c)?);
        }
    }
    if let Some(nodes) = sup {
        let plain = single_line(nodes)?;
        for c in plain.chars().filter(|c| !c.is_whitespace()) {
            s.push(superscript(c)?);
        }
    }
    Some(s)
}

/// The single-line rendering of a node list, or `None` if it needs >1 row.
fn single_line(nodes: &[Node]) -> Option<String> {
    let b = layout_seq(nodes);
    if b.height() == 1 {
        Some(b.lines[0].clone())
    } else {
        None
    }
}

fn layout_seq(nodes: &[Node]) -> Box2 {
    let mut items: Vec<(Box2, Class)> = nodes.iter().map(layout_node).collect();
    // Reclassify a binary operator with no left operand (or following another
    // operator/relation/open) as ordinary — TeX's unary-minus handling.
    for k in 0..items.len() {
        if items[k].1 == Class::Bin {
            let unary = k == 0
                || matches!(
                    items[k - 1].1,
                    Class::Bin | Class::Rel | Class::Op | Class::Open | Class::Punct
                );
            if unary {
                items[k].1 = Class::Ord;
            }
        }
    }

    let mut acc: Option<(Box2, Class)> = None;
    for (bx, cls) in items {
        if bx.width() == 0 && bx.height() <= 1 {
            continue;
        }
        match acc.take() {
            None => acc = Some((bx, cls)),
            Some((left, lclass)) => {
                let mut joined = left;
                if space_cols(lclass, cls) > 0 {
                    joined = hcat(&joined, &Box2::leaf(" "));
                }
                joined = hcat(&joined, &bx);
                acc = Some((joined, cls));
            }
        }
    }
    acc.map(|(b, _)| b).unwrap_or_else(Box2::empty)
}

/// Collapsed inter-atom spacing: 1 column where TeX inserts thin/med/thick
/// space, 0 otherwise.
fn space_cols(l: Class, r: Class) -> usize {
    use Class::*;
    let space = match (l, r) {
        (Punct, _) => true,
        (_, Punct) => false,
        (Open, _) => false,
        (_, Close) => false,
        (_, Open) => matches!(l, Bin | Rel | Op),
        (Close, _) => matches!(r, Bin | Rel | Op),
        (Op, _) | (_, Op) => true,
        (Bin, _) | (_, Bin) => true,
        (Rel, _) | (_, Rel) => true,
        _ => false,
    };
    space as usize
}

/// Render a LaTeX math fragment (delimiters already stripped) to a grid of text
/// lines (no trailing whitespace). Total and best-effort: never panics, unknown
/// constructs degrade to readable text.
pub fn latex_to_lines(latex: &str) -> Vec<String> {
    let nodes = parse(latex);
    let b = layout_seq(&nodes);
    b.lines
        .into_iter()
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// Like [`latex_to_lines`], but re-flowed to fit `width` columns. An equation
/// wider than the terminal would otherwise be hard-wrapped by the terminal —
/// each row independently — which shreds the 2-D alignment. Instead we split the
/// block into vertical panels at clean seams and indent the continuations.
pub fn latex_to_lines_wrapped(latex: &str, width: usize) -> Vec<String> {
    wrap_block(&latex_to_lines(latex), width)
}

/// Continuation indent (columns) for wrapped equation panels.
const WRAP_INDENT: usize = 4;

/// A column is a "seam" if every row is blank there — a safe place to split the
/// block without cutting through a glyph.
fn is_seam(grid: &[Vec<char>], col: usize) -> bool {
    grid.iter().all(|row| row.get(col).copied().unwrap_or(' ') == ' ')
}

/// Split a rendered equation block into terminal-width panels, breaking at seams
/// (preferring the rightmost one within budget — e.g. the space around a
/// top-level `+` or `=`) and indenting continuation panels. Panels are separated
/// by a blank line. Returns the input unchanged when it already fits.
fn wrap_block(lines: &[String], width: usize) -> Vec<String> {
    let block_w = lines.iter().map(|l| dw(l)).max().unwrap_or(0);
    if width == 0 || block_w <= width {
        return lines.to_vec();
    }
    // Pad every row to the full block width so columns line up.
    let grid: Vec<Vec<char>> = lines
        .iter()
        .map(|l| {
            let mut v: Vec<char> = l.chars().collect();
            v.resize(block_w, ' ');
            v
        })
        .collect();

    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut first = true;
    while start < block_w {
        let budget = if first { width } else { width.saturating_sub(WRAP_INDENT) }.max(1);
        let end = if block_w - start <= budget {
            block_w
        } else {
            // Largest seam in (start, start+budget]; else a hard cut at the edge.
            let hard = start + budget;
            (start + 1..=hard).rev().find(|&c| is_seam(&grid, c)).unwrap_or(hard)
        };
        if !first {
            out.push(String::new()); // blank line between panels
        }
        let prefix = if first { "" } else { &" ".repeat(WRAP_INDENT) };
        for row in &grid {
            let seg: String = row[start..end].iter().collect();
            out.push(format!("{prefix}{}", seg.trim_end()).trim_end().to_string());
        }
        // Advance past the panel, skipping seam columns so the next panel doesn't
        // start with a leading gap.
        start = end;
        while start < block_w && is_seam(&grid, start) {
            start += 1;
        }
        first = false;
    }
    out
}

/// Convenience: the grid joined with newlines.
#[cfg(test)]
pub fn latex_to_pretty(latex: &str) -> String {
    latex_to_lines(latex).join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_noop_when_it_fits() {
        let eq = latex_to_lines("\\frac{a}{b} = c");
        assert_eq!(wrap_block(&eq, 80), eq);
        assert_eq!(latex_to_lines_wrapped("\\frac{a}{b} = c", 80), eq);
    }

    #[test]
    fn wrap_splits_wide_block_into_indented_panels() {
        // A wide single-row sum; force a narrow width.
        let eq = latex_to_lines("a + b + c + d + e + f + g + h");
        let width = 12;
        let wrapped = wrap_block(&eq, width);
        // No output line exceeds the width.
        assert!(wrapped.iter().all(|l| l.chars().count() <= width), "{wrapped:?}");
        // It actually split (more lines than the original single row).
        assert!(wrapped.len() > eq.len());
        // Continuation content is indented.
        assert!(wrapped.iter().any(|l| l.starts_with("    ") && l.trim().starts_with(|c: char| c.is_alphanumeric())));
    }

    #[test]
    fn wrap_breaks_at_seams_not_mid_token() {
        // Break should land in the spaces around operators, never slicing a run.
        let eq = latex_to_lines("xx + yy + zz");
        let wrapped = wrap_block(&eq, 7);
        // No panel line should contain a partial "xx"/"yy"/"zz" followed by nothing
        // odd — every alphanumeric run stays whole. Check the tokens survive intact.
        let joined: String = wrapped.join("\n");
        for tok in ["xx", "yy", "zz"] {
            assert!(joined.contains(tok), "token {tok} was sliced: {wrapped:?}");
        }
    }

    #[test]
    fn wrap_preserves_2d_alignment_within_a_panel() {
        // A wide expression with fractions; within each panel the rows stay aligned.
        let eq = latex_to_lines("\\frac{a}{b} + \\frac{c}{d} + \\frac{e}{f} + \\frac{g}{h}");
        let wrapped = wrap_block(&eq, 16);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 16));
        // Fraction bars survive in the wrapped output.
        assert!(wrapped.iter().any(|l| l.contains('─')));
    }

    #[test]
    fn fraction_stacks_three_rows() {
        let lines = latex_to_lines("\\frac{a}{b}");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains('─'));
        // numerator above the bar, denominator below
        assert!(lines[0].contains('a'));
        assert!(lines[2].contains('b'));
    }

    #[test]
    fn compound_numerator_widens_bar() {
        let p = latex_to_pretty("\\frac{a+b}{c}");
        let lines: Vec<&str> = p.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("a + b"));
    }

    #[test]
    fn simple_scripts_are_inline() {
        assert_eq!(latex_to_pretty("x^2"), "x²");
        assert_eq!(latex_to_pretty("x^{n+1}"), "xⁿ⁺¹");
        assert_eq!(latex_to_pretty("a_i"), "aᵢ");
        assert_eq!(latex_to_pretty("e^{-x}"), "e⁻ˣ");
    }

    #[test]
    fn unmappable_subscript_stacks() {
        // 'z' has no subscript glyph, so it drops below-right on its own row.
        let lines = latex_to_lines("a_z");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains('a'));
        assert!(lines[1].contains('z'));
    }

    #[test]
    fn relations_and_bins_get_spaced() {
        assert_eq!(latex_to_pretty("a=b"), "a = b");
        assert_eq!(latex_to_pretty("a+b"), "a + b");
        // unary minus binds tight
        assert_eq!(latex_to_pretty("-x"), "-x");
    }

    #[test]
    fn sum_uses_display_limits() {
        let lines = latex_to_lines("\\sum_{i=1}^{n}");
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains('n'));
        assert!(lines[1].contains('∑'));
        assert!(lines[2].contains("i = 1"));
    }

    #[test]
    fn sqrt_gets_a_roof() {
        let lines = latex_to_lines("\\sqrt{x}");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains('─'));
        assert!(lines[1].contains('√'));
        assert!(lines[1].contains('x'));
    }

    #[test]
    fn left_right_delimiters_grow() {
        let lines = latex_to_lines("\\left( \\frac{a}{b} \\right)");
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains('⎛'));
        assert!(lines[2].contains('⎝'));
        assert!(lines[0].contains('⎞'));
        assert!(lines[2].contains('⎠'));
    }

    #[test]
    fn greek_and_symbols_map() {
        assert_eq!(latex_to_pretty("\\alpha + \\beta"), "α + β");
        assert_eq!(latex_to_pretty("\\nabla \\cdot \\mathbf{v}"), "∇ · 𝐯");
    }

    #[test]
    fn nested_fraction_in_parens() {
        // -\frac{1}{\rho} \nabla p  with grown parens around a fraction
        let p = latex_to_pretty("\\left( \\frac{\\partial v}{\\partial t} \\right) = 0");
        assert!(p.lines().count() >= 3);
        assert!(p.contains('∂'));
        assert!(p.contains('='));
    }

    #[test]
    fn never_panics_on_messy_input() {
        for s in [
            "", "\\", "{", "}", "^", "_", "\\frac{}{}", "^^__", "\\\\", "\\left(",
            "\\right)", "\\sqrt", "\\frac{a}", "x^", "_y", "\\left",
        ] {
            let _ = latex_to_lines(s);
        }
    }

    /// Visual dump of wrapping: `cargo test wrap_demo -- --nocapture`.
    #[test]
    fn wrap_demo() {
        let eq = "\\rho \\left( \\frac{\\partial u}{\\partial t} + u \\frac{\\partial u}{\\partial x} + v \\frac{\\partial u}{\\partial y} + w \\frac{\\partial u}{\\partial z} \\right) = -\\frac{\\partial p}{\\partial x} + \\mu \\left( \\frac{\\partial^2 u}{\\partial x^2} + \\frac{\\partial^2 u}{\\partial y^2} + \\frac{\\partial^2 u}{\\partial z^2} \\right) + \\rho g_x";
        for width in [80usize, 60] {
            println!("\n--- wrapped at {width} columns ---");
            for line in latex_to_lines_wrapped(eq, width) {
                println!("{line}");
            }
        }
    }

    /// Visual dump: `cargo test demo -- --nocapture` to eyeball the renderings.
    #[test]
    fn demo() {
        let samples = [
            "\\frac{\\partial \\mathbf{v}}{\\partial t} + (\\mathbf{v} \\cdot \\nabla) \\mathbf{v} = -\\frac{1}{\\rho} \\nabla p + \\nu \\nabla^2 \\mathbf{v}",
            "\\sum_{i=1}^{n} \\frac{1}{i^2} = \\frac{\\pi^2}{6}",
            "x = \\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}",
            "\\left( \\frac{a+b}{c} \\right)^2",
        ];
        for s in samples {
            println!("\nLaTeX: {s}");
            for line in latex_to_lines(s) {
                println!("  {line}");
            }
        }
    }
}
