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
```

## How the passthrough works

`mathterm` allocates a PTY and runs the child inside it, so the child still
believes it is attached to a real terminal (and thus keeps colors, spinners, and
interactive features enabled). The real terminal is put into raw mode for the
duration (restored on exit, including on panic), stdin is forwarded to the child
unmodified, the child's stdout is proxied back, `SIGWINCH` resizes are propagated
to the child PTY, and `mathterm` exits with the child's exit code.

## License

Dual-licensed under MIT or Apache-2.0.
