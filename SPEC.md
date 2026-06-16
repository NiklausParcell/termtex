# mathterm — product & technical spec

## Vision

A terminal-layer tool that renders LaTeX math as inline images in any
Kitty-graphics terminal (Ghostty, Kitty, WezTerm), so anyone doing math in the
terminal — ML/science workflows, reading papers, and AI CLIs like Claude Code —
sees real equations instead of raw LaTeX. Installable and open-source, so it
evolves independently of the programs it wraps.

`mathterm` wraps a child program in a PTY, scans its output for LaTeX, renders
each equation to a PNG (pure Rust, no external deps), and emits it inline via the
Kitty graphics protocol. Everything else passes through byte-for-byte, so the
child keeps colors, interactivity, and exit codes.

## The central distinction: output regimes

How a program emits text determines whether a transparent proxy can augment it.

| Regime | Examples | Supported? |
|--------|----------|------------|
| **Line-oriented** — text flows top-to-bottom as newline-terminated lines | `python`/`ipython`, Jupyter console, R, Julia, scripts, `cat`, markdown, **`claude -p` (print mode)** | ✅ Works today |
| **Full-screen TUI** — paints the screen with cursor-movement escapes, repaints regions | **Claude Code interactive UI**, `vim`, `top`, `htop` | ⚠️ Hard — see below |

### Why full-screen TUIs are hard (and the plan)

A TUI doesn't "print output"; it owns the screen and repaints it with cursor
addressing. Two problems for a transparent proxy:

1. **Detection** — equation text arrives smeared across cursor-positioning codes
   and is redrawn several times as tokens stream; it is not one clean line.
2. **Screen ownership (the hard one)** — if we inject a Kitty image, the TUI
   doesn't know it exists. It thinks the cursor is at row N; our image shifted
   everything. Its next repaint paints over the image or in the wrong place →
   corruption.

No scanner cleverness fixes #2 — it is an ownership conflict. The plan is
**evidence-based**: capture Claude Code's real output stream (`--capture`),
characterize it (alt-screen? committed scrollback lines? cursor-move patterns?),
and only then decide whether a constrained interactive mode is feasible (e.g.
render only into stable scrollback, or detect message-commit boundaries) or
whether interactive Claude needs cooperation from Claude Code itself. Until that
data exists, interactive Claude Code is **not** a promised feature.

## Architecture (implemented)

```
src/
  main.rs      PTY setup, raw-mode guard, proxy loops, SIGWINCH, exit code, Sink
  config.rs    CLI parsing -> Config
  pty.rs       RawModeGuard (termios), terminal_size (TIOCGWINSZ)
  scanner.rs   delimiter state machine: $$..$$, \[..\], opt-in $..$ / \(..\)
  bare.rs      --detect-bare: heuristic delimiter-less display-LaTeX detection
  render/
    mod.rs     MathRenderer trait, RenderedImage, RenderError, LRU cache
    ratex.rs   RatexRenderer: RaTeX (parse->layout->SVG) + resvg -> PNG, recolor
  kitty.rs     PNG -> Kitty graphics protocol escapes (chunked)
```

- **Renderer:** pure Rust. RaTeX (`ratex-*` crates, embedded KaTeX fonts) → SVG →
  `resvg` → PNG, ~2–5ms/equation, zero runtime deps. Glyphs recolored white
  (configurable) on the rasterized pixmap so they're visible on dark terminals.
- **Delimited math** (`$$`, `\[`, opt-in `$`/`\(`) is *replaced* by an image
  (confident). **Bare math** (`--detect-bare`) is *augmented* (image appended
  after the source text) and never holds the byte stream, so a child's live UI
  is never frozen.
- **Fallbacks:** non-graphics terminal or unparseable LaTeX → original bytes
  verbatim. Never crashes, never corrupts.

## Supported invocations

```sh
mathterm                                   # wrap $SHELL (line-oriented programs)
mathterm -- python                         # REPL
mathterm --detect-bare -- claude -p "..."  # Claude print mode (recommended)
mathterm --inline --detect-bare -- <prog>  # full detection
```

## Roadmap

1. ✅ PTY passthrough; scanner; Kitty emitter; pure-Rust renderer; CLI; glyph
   visibility; bare-LaTeX detection.
2. **Characterize Claude Code's interactive stream** via `--capture`; decide
   interactive feasibility from data.
3. **Sizing/spacing polish** — fit images to text-cell heights; tune spacing
   around display math (uses the pixel dims already plumbed through).
4. **Robust capability detection** — Kitty graphics query with graceful fallback.
5. **Packaging** — crates.io publish, install docs, demo, `brew`/binary releases.

## Non-goals (for now)

- Rendering inside arbitrary full-screen TUIs (vim/top) — out of scope.
- Non-Kitty image protocols (Sixel, iTerm2) — possible later behind the emitter.
- Full LaTeX document typesetting — math mode only.
