#!/usr/bin/env bash
#
# Visual smoke test for `termtex --pretty`.
#
# Renders a set of well-known equations as 2-D text. Run it in a UTF-8 terminal
# (e.g. Ghostty) and eyeball the output — fractions should stack, sqrt should
# get a roof, sums should show limits above/below, and \left(...\right) should
# grow around tall contents.
#
#   ./examples/pretty-demo.sh
#
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --quiet
# 2-D text is the default rendering mode — no flag needed.
exec target/debug/termtex -- cat examples/equations.tex
