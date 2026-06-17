# mathterm — session log (2026-06-16)

A working record of the build journey, key decisions, and the disk-cleanup
detour. Code history is in git; this captures the *why* and the open threads.

## mathterm — what was built

Built in committed steps (see `git log`):

1. **PTY passthrough** — wrap a child in a PTY, raw-mode guard, SIGWINCH resize,
   exit-code propagation, byte-for-byte passthrough.
2. **Streaming scanner** (`scanner.rs`) — `$$…$$`, `\[…\]`, opt-in `$…$`/`\(…\)`;
   chunk-boundary safe; `MAX_MATH_BYTES` safety valve.
3. **Kitty emitter** (`kitty.rs`) — PNG → Kitty graphics escapes, chunked;
   `--selftest-image`.
4. **Renderer** (`render/`) — pure-Rust **RaTeX** (parse→layout→SVG) + `resvg`
   → PNG, ~2–5ms, no external deps, fonts embedded. LRU cache.
5. **CLI + polish** (`config.rs`) — flags; glyph **visibility fix** (recolor
   raster to white, since RaTeX hardcodes black → invisible on dark terminals);
   **`--detect-bare`** heuristic for delimiter-less display equations; image
   height-capping to text rows.
6. **`--strip`** (`strip.rs`) — reserved bottom strip for interactive TUIs
   (see below). **Implemented + committed; final build/test verification still
   pending** (disk filled mid-way).

### Key decisions / findings
- **Renderer = RaTeX (pure Rust), not Node CLIs** — fastest/lightest, no runtime
  deps, crates.io-publishable. ~100× faster than tex2svg/katex cold start.
- **Claude Code emits display math as BARE LaTeX (no `$$`)**, inline as `$…$`.
  Hence `--inline --detect-bare`.
- **Two output regimes:** line-oriented (python, scripts, markdown, `claude -p`)
  → works great. Full-screen TUIs (interactive Claude Code) → inline injection
  **corrupts** the layout (proven: it owns/repaints the screen). Capture showed
  no alt-screen but heavy cursor addressing (1145 moves).
- **`--strip` is the interactive answer:** give the child a shorter terminal
  (scroll region confines it to the top), mathterm owns a bottom strip and draws
  equations there with cursor save/restore — disjoint regions, no collision.

### Update (2026-06-16, later session)
- **Build/test thread resolved:** disk healthy (~107 GB free), `cargo test`
  green. `--strip` and `--unicode` both build and pass.
- **New `--pretty` mode (`src/layout.rs`): a miniature TeX.** Parses LaTeX into a
  math list and runs a *discretized box-and-glue layout* (TeXbook Appendix G) to
  a character grid: `Box2 { lines, baseline }` with ascent/depth; `hcat` aligns
  baselines; fractions stack num/rule/den on the baseline; sub/superscripts use
  Unicode super/subscripts when single-char-mappable (TeX's "script style") and
  fall back to 2-D stacking otherwise; `√` gets a vinculum; big operators
  (∑∏∐⋃⋂) put limits above/below in display style; `\left…\right` delimiters grow
  with bracket pieces (⎛⎜⎝). Inter-atom spacing comes from a collapsed version of
  TeX's 8×8 atom-class table (Ord/Op/Bin/Rel/Open/Close/Punct), incl. unary-minus
  handling. What doesn't port: sub-pixel `\fontdimen` shifts → integer cells.
  Reuses `unicode.rs` tokenizer + symbol/super/subscript/bold tables (now
  `pub(crate)`). 12 new tests (88 total). Renders Navier–Stokes, the quadratic
  formula, Σ-sums, nested grown parens correctly.
- **Three rendering regimes now exist:** image (Kitty, highest fidelity),
  `--unicode` (single-line text, TUI-safe), `--pretty` (2-D text, no graphics
  protocol — best for SSH/tmux/Terminal.app without Kitty graphics). `--pretty`
  is multi-line, so like images it's for line-oriented output, not interactive
  TUIs.
- **Default flipped to 2-D text (user decision).** The 2-D text layout is now the
  default mode; images are opt-in via **`--image`**. Rationale: the old default
  only rendered on Ghostty/Kitty/WezTerm and dumped *raw LaTeX* on every other
  terminal — strictly worse than text. Now: `--image` + graphics → image;
  otherwise → 2-D text (incl. when `--image` is set but graphics are absent —
  graceful fallback instead of raw). `--unicode` still wins for interactive TUIs;
  `--pretty` forces text over `--image`; `--strip` implies image. Image mode
  keeps one real edge: it scales *wide* equations to terminal width (text wraps).

### Update (2026-06-16, crate finalization)
- **Renamed `mathterm` → `termtex`** (crate + binary; local dir still
  `~/code/mathterm`). crates.io name free; GitHub `NiklausParcell/termtex`
  already exists. Added dual MIT/Apache-2.0 license files; `cargo package`
  builds clean (publish-ready); Cargo.toml `exclude`s dev artifacts.
- **`--strip` now carries 2-D text, not just images** (`Strip::draw_text`).
  Render kind (image/text/unicode) is now independent of placement (inline vs
  strip). For interactive Claude: `--strip` puts the full 2-D layout in the
  reserved bottom strip.
- **Width-aware wrapping** (`layout::latex_to_lines_wrapped`, used by the inline
  text path via `Sink.term_cols`): a block wider than the terminal splits into
  vertical panels at *seam* columns (all-blank cols — the gaps around top-level
  `+`/`=`) with a 4-col continuation indent, instead of the terminal hard-
  wrapping each row and destroying the 2-D alignment.
- **README rewritten** for the text-first default, with install (`cargo install
  termtex`), the rendering-mode table, sample outputs, and a Claude Code section
  (line-oriented vs interactive). 95 tests passing.

### Open threads for termtex (was mathterm)
- Verify `--strip` build (`cargo test`) — pending since disk was full.
- Test in real Ghostty: `./target/debug/mathterm --strip --detect-bare --inline -- claude`.
- Likely iteration on strip (scroll-region behavior, resize, placement) — can't
  test from the headless harness; needs the real terminal.
- Diagnostics available: `--capture <file>` + `tools/analyze_capture.py`.

## Disk cleanup detour (resolved)

Disk was 100% full (932 GB volume, ~900 GB used). Biggest culprits found via
`du`: `~/Library` (244 GB) and `~/code` (223 GB).

**Reclaimed:**
- **Docker (~80 GB inside, `Docker.raw` 56 GB → 4.5 GB auto-shrunk):** pruned
  build cache (35 GB), images (34 GB), containers (4 GB), volumes (7 GB).
- Plus caches (pip/yarn), Xcode DerivedData, unavailable simulators.
- Disk ended at **~94 GB free**.

**Docker volume backups (restorable):** archived all 12 volumes to
`~/docker-archive/*.tar.gz` (6.4 GB total) **before** deleting. Only two held
real data: `agentic-example-use-case_ollama_data` (6.3 GB, Ollama models) and
`golf-scores-idea_golf_pg_data` (6.9 MB, Postgres). Restore a volume with:
```sh
docker volume create <NAME>
docker run --rm -v <NAME>:/data -v ~/docker-archive:/backup \
  alpine tar xzf /backup/<NAME>.tar.gz -C /data
```
Move `~/docker-archive` to external storage to reclaim its 6.4 GB.

**Still available if more space is needed:** `~/code` Rust `target/` dirs
(~76 GB, all verified as Rust build output — safe:
`find ~/code -type d -name target -prune -exec rm -rf {} +`), and `mosaics-ai`
datasets (49 GB, Kaggle/re-downloadable — user's call).
