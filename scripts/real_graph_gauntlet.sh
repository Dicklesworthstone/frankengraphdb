#!/usr/bin/env bash
# Run with PYTHON pointing at an interpreter with the Kùzu binding installed.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
exec "${PYTHON:-python3}" "$root/crates/fgdb-bench/examples/real_graph_gauntlet.py" "$@"
