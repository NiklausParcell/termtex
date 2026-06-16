# mathterm

A transparent terminal-graphics proxy that renders LaTeX math inline in any
terminal supporting the [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
(Ghostty, Kitty, WezTerm).

`mathterm` wraps a child program (a shell, `claude`, `python`, …), watches that
program's stdout for LaTeX blocks, renders each block to an image, and emits it
inline. All non-LaTeX output passes through byte-for-byte so colors, cursor
control, and interactive prompts keep working.

> **Status: early development.** Build step 1 (transparent PTY passthrough) is
> complete. LaTeX scanning, rendering, and the Kitty emitter are in progress.

## Usage

```
mathterm [OPTIONS] [-- <command> [args...]]

mathterm -- claude          # wrap a one-off command
mathterm -- python script.py
mathterm                    # no command: wrap your $SHELL interactively

# Recommended for Claude Code (renders inline $...$ and bare display equations):
mathterm --inline --detect-bare -- claude
```

### Options

| Option | Effect |
|--------|--------|
| `--inline` | Also render inline `$...$` and `\(...\)` |
| `--detect-bare` | Heuristically render bare (delimiter-less) display equations, e.g. Claude's. Best-effort; appends an image after the source text |
| `--font-size <px>` | Em size in pixels (default 40) |
| `--scale <f>` | DPI/size multiplier (default 1.0) |
| `--color <hex\|name>` | Glyph color, e.g. `#ffffff` or `white` (default white, for dark terminals) |
| `--no-cache` | Disable the render cache |
| `--max-math-bytes <n>` | Unterminated-block byte cap (default 4096) |
| `--no-graphics` / `--force-graphics` | Override terminal capability detection |
| `--selftest-image` | Emit a test image and exit (checks terminal support) |

### What gets rendered

- **Delimited math** — `$$...$$` and `\[...\]` (block), plus `$...$` and `\(...\)`
  with `--inline`. Confident: the delimiters are removed and replaced by an image.
- **Bare display LaTeX** — equations with no delimiters (with `--detect-bare`).
  Heuristic: a line is treated as an equation when it has multiple LaTeX commands
  and a math construct and isn't prose. Consecutive/wrapped equation lines are
  joined and rendered as one image, appended after the source text (the text is
  kept as a fallback, since the heuristic is best-effort).

## How the passthrough works

`mathterm` allocates a PTY and runs the child inside it, so the child still
believes it is attached to a real terminal (and thus keeps colors, spinners, and
interactive features enabled). The real terminal is put into raw mode for the
duration (restored on exit, including on panic), stdin is forwarded to the child
unmodified, the child's stdout is proxied back, `SIGWINCH` resizes are propagated
to the child PTY, and `mathterm` exits with the child's exit code.

## License

Dual-licensed under MIT or Apache-2.0.
