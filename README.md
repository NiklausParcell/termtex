# termtex

[![crates.io](https://img.shields.io/crates/v/termtex.svg)](https://crates.io/crates/termtex)
[![license](https://img.shields.io/crates/l/termtex.svg)](#license)

**Render LaTeX math in your terminal.** `termtex` is a tiny TeX engine that draws
equations as clean 2-D text — fractions stack, square roots get a roof, sums show
their limits, and `\left(…\right)` delimiters grow to fit. It works in *any*
terminal, no graphics protocol required.

```
x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}
```
becomes

```
           ────────
     -b ± √b² - 4ac
x = ────────────────
           2a
```

`termtex` wraps a child program (a shell, `claude`, `python`, …), watches its
stdout for LaTeX, and renders each equation in place. Everything else passes
through byte-for-byte, so colors, cursor control, and prompts keep working.

## Install

```sh
cargo install termtex
```

Or build the latest from source:

```sh
cargo install --git https://github.com/NiklausParcell/termtex
# or
git clone https://github.com/NiklausParcell/termtex && cd termtex && cargo install --path .
```

`termtex` is pure Rust with no runtime dependencies — math fonts are embedded in
the binary.

## Quick start

```sh
termtex -- python              # math in a REPL / script output
termtex -- cat paper.md        # a markdown file with LaTeX
termtex                        # no command: wrap your $SHELL

# With Claude (see the Claude Code section below):
termtex --inline --detect-bare -- claude -p "derive the quadratic formula"
```

Try the gallery:

```sh
./examples/pretty-demo.sh
```

```
Navier-Stokes momentum:

 ⎛ ∂𝐯          ⎞
ρ⎜──── + 𝐯 · ∇𝐯⎟ = -∇p + μ∇²𝐯 + 𝐟
 ⎝ ∂t          ⎠

Basel problem:

  n    1      π²
  ∑   ──── = ────
i = 1  i²     6
```

## Three ways to run it

`termtex` is a wrapper — it renders math for whatever program you start *through*
it. Pick how automatic you want that to be:

### 1. Per-command (no setup)

Just prefix the command. Nothing to install into your shell.

```sh
termtex -- python
termtex -- cat paper.md
termtex --inline --detect-bare -- claude -p "derive the quadratic formula"
```

### 2. Auto-wrap one program (e.g. Claude)

A shell function so plain `claude` transparently routes through `termtex`. Add to
`~/.zshrc` (or `~/.bashrc`):

```sh
claude() {
  if [[ " $* " == *" -p "* || " $* " == *" --print "* ]]; then
    command termtex --inline --detect-bare -- claude "$@"          # print mode → full 2-D
  else
    command termtex --strip --inline --detect-bare -- claude "$@"  # interactive → bottom strip
  fi
}
```

No recursion — `termtex` runs the real `claude` binary, not the function. (Swap
`--strip` for `--unicode` if you prefer rock-solid single-line math in the
interactive TUI.)

### 3. Always on (wrap your whole terminal)

Render math in *every* command of a terminal session. The cleanest way is to point
your terminal's shell at `termtex`. For **Ghostty** (`~/.config/ghostty/config`):

```
command = termtex --unicode -- /bin/zsh
```

Or do it from your shell rc, guarded so it only wraps once:

```sh
# ~/.zshrc
if [[ -o interactive && -z $TERMTEX_WRAPPED ]]; then
  export TERMTEX_WRAPPED=1
  exec termtex --unicode -- "$SHELL"
fi
```

Because an always-on wrap scans *everything* — including full-screen TUIs like
`vim`, `less`, and interactive `claude` — use **`--unicode`** here: single-line
math is the only mode guaranteed not to disturb a program that repaints the
screen. (Drop `--unicode` for the full 2-D layout if you only ever run
line-oriented programs in that terminal.)

## How it renders

`termtex` recognizes **delimited** math (`$$…$$`, `\[…\]`, and — with `--inline`
— `$…$`, `\(…\)`) and, with `--detect-bare`, **bare** display equations that have
no delimiters at all (the form Claude emits). It can render each one three ways:

| Mode | Flag | Notes |
|------|------|-------|
| **2-D text** | *(default)* | A miniature TeX layout engine (box-and-glue, [TeXbook Appendix G](https://en.wikipedia.org/wiki/Appendix_G)) drawn to a character grid. No graphics protocol; copy-pasteable; survives scrollback and tmux. |
| **Image** | `--image` | Real typeset images via the [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) (Ghostty, Kitty, WezTerm). Highest fidelity; scales wide equations to fit. Falls back to 2-D text where graphics aren't available. |
| **Unicode** | `--unicode` | A single line of Unicode (`∇·𝐯=0`). Lowest fidelity, but flows through *anything*, including full-screen TUIs. |

**Wide equations wrap cleanly.** When a 2-D equation is wider than your terminal,
`termtex` reads the window width and breaks it into vertical panels at clean
seams (the gaps around top-level `+`/`=`), indenting the continuation — instead
of letting the terminal hard-wrap each row and shred the alignment:

```
 ⎛ ∂u      ∂u      ∂u      ∂u ⎞     ∂p     ⎛ ∂²u     ∂²u
ρ⎜──── + u──── + v──── + w────⎟ = -──── + μ⎜───── + ───── +
 ⎝ ∂t      ∂x      ∂y      ∂z ⎠     ∂x     ⎝ ∂x²     ∂y²

     ∂²u ⎞
    ─────⎟ + ρgₓ
     ∂z² ⎠
```

## With Claude Code

Claude writes display equations as **bare LaTeX** (no delimiters) and inline math
as `$…$`. So the recommended invocation enables both:

```sh
termtex --inline --detect-bare -- claude -p "explain the Navier-Stokes equations"
```

What Claude prints:

```
For incompressible flow $\nabla \cdot \mathbf{v} = 0$, and momentum obeys
\rho \left( \frac{\partial \mathbf{v}}{\partial t} + \mathbf{v} \cdot \nabla \mathbf{v} \right) = -\nabla p + \mu \nabla^2 \mathbf{v} + \mathbf{f}
```

What you see through `termtex`:

```
For incompressible flow ∇ · 𝐯 = 0, and momentum obeys

 ⎛ ∂𝐯          ⎞
ρ⎜──── + 𝐯 · ∇𝐯⎟ = -∇p + μ∇²𝐯 + 𝐟
 ⎝ ∂t          ⎠
```

### Interactive vs. line-oriented

The 2-D rendering is **multi-line**, so it shines on **line-oriented** output —
`claude -p`, pipes, scripts, `python`, markdown — where output only ever appends.

**Interactive** `claude` is different: it's a full-screen TUI that continuously
repaints the screen, so injecting extra lines mid-stream corrupts its layout (the
same is true of inline images). For the interactive CLI, use one of:

```sh
termtex --unicode --inline --detect-bare -- claude        # single-line: Re = ρuL/μ
termtex --strip --inline --detect-bare -- claude          # 2-D text in a reserved bottom strip
termtex --image --strip --inline --detect-bare -- claude  # images in a reserved bottom strip
```

`--unicode` is safe because it replaces math with a single line (same line count).
`--strip` reserves a region at the bottom of the screen that `termtex` owns
(confining Claude to a scroll region above it), so the two never collide.

## How the passthrough works

`termtex` allocates a PTY and runs the child inside it, so the child still
believes it is attached to a real terminal (keeping colors, spinners, and
interactive features on). The real terminal is put into raw mode for the duration
(restored on exit, including on panic), stdin is forwarded unmodified, the child's
stdout is proxied back, `SIGWINCH` resizes propagate to the child PTY, and
`termtex` exits with the child's exit code.

## Options

| Option | Effect |
|--------|--------|
| `--inline` | Also render inline `$...$` and `\(...\)` |
| `--detect-bare` | Render bare (delimiter-less) display equations, e.g. Claude's |
| `--image` | Render to typeset images via the Kitty graphics protocol |
| `--unicode` | Render math as single-line Unicode text |
| `--strip` | Render into a reserved bottom strip (for interactive TUIs) |
| `--strip-rows <n>` | Height of the reserved strip (default 8) |
| `--font-size <px>` / `--scale <f>` / `--color <hex\|name>` | Image rendering tweaks |
| `--no-cache` | Disable the render cache |
| `-h, --help` | Full help |

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
