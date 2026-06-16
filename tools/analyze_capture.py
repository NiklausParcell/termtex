#!/usr/bin/env python3
"""Characterize a captured terminal stream (from `mathterm --capture <file>`).

Answers the key question: does the wrapped program emit clean line-oriented text
(mathterm can render it) or paint a full-screen TUI with cursor addressing
(transparent injection is unsafe)?

Usage:  python3 tools/analyze_capture.py /tmp/claude-stream.bin
"""
import re
import sys

ESC = 0x1b


def main():
    if len(sys.argv) < 2:
        print("usage: analyze_capture.py <capture-file>")
        sys.exit(2)
    data = open(sys.argv[1], "rb").read()

    n = len(data)
    esc = data.count(bytes([ESC]))
    alt_enter = data.count(b"\x1b[?1049h")
    alt_exit = data.count(b"\x1b[?1049l")
    # CSI cursor moves: ESC [ <params> <A|B|C|D|E|F|G|H|f|J|K>
    cursor = len(re.findall(rb"\x1b\[[0-9;?]*[ABCDEFGHJKf]", data))
    crs = data.count(b"\r")
    lfs = data.count(b"\n")

    print(f"file bytes:            {n}")
    print(f"ESC (0x1b) total:      {esc}")
    print(f"alt-screen enter/exit: {alt_enter} / {alt_exit}   (>0 => full-screen TUI)")
    print(f"cursor-move escapes:   {cursor}   (CSI A/B/C/D/H/J/K/f)")
    print(f"carriage returns \\r:   {crs}")
    print(f"line feeds \\n:         {lfs}")
    print(f"escape density:        {esc / max(n,1):.1%} of bytes are part of escapes (rough)")

    # Strip ANSI to recover the "visible text" and see if the equation survives
    # as clean lines.
    stripped = re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", data)
    stripped = re.sub(rb"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", b"", stripped)
    stripped = stripped.replace(b"\r", b"")
    text = stripped.decode("utf-8", "replace")

    lines = text.split("\n")
    eq_lines = [l for l in lines if "\\rho" in l or "\\nabla" in l or "\\frac" in l or "\\mathbf" in l]
    print(f"\nlines after ANSI strip: {len(lines)}")
    print(f"lines containing LaTeX commands: {len(eq_lines)}")
    if eq_lines:
        print("--- LaTeX-bearing lines (clean) ---")
        for l in eq_lines[:12]:
            print(f"  | {l.strip()[:100]}")

    print("\n--- first 1200 chars of ANSI-stripped text ---")
    print(text[:1200])


if __name__ == "__main__":
    main()
