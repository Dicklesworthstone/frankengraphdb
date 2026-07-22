#!/usr/bin/env bash
# foundation_types_e2e (bead fgdb-w1-foundation-types-tjk): one deterministic
# pass over all six foundation crates — canonical scalars under
# STRICT_PORTABLE, ZWeight promotion across the i128 boundary, every
# delta-row arm through template -> committed marker -> ordered batch, one
# evidence envelope per §15.0 claim kind with a scripted lattice violation,
# and a resource-admission loop ending in a typed ceiling rejection.
# The transcript must be byte-identical across two runs.
#
# The evidence directory is intentionally retained (repository policy forbids
# automated deletion; the transcripts are useful for replay).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-foundation-e2e.XXXXXX")"
FIRST="$EVIDENCE_DIR/first.txt"
SECOND="$EVIDENCE_DIR/second.txt"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$EVIDENCE_DIR/target}"

echo "==> verify the foundation crates stay inside the closed universe (path deps only)"
for crate in fgdb-types fgdb-claim fgdb-delta-types fgdb-evidence fgdb-resource; do
  if cargo tree -p "$crate" --edges normal --prefix none | grep -vE '^fgdb-' | grep -q .; then
    echo "ERROR: $crate has a non-fgdb normal dependency" >&2
    exit 1
  fi
done

echo "==> run every foundation test target"
cargo test -p fgdb-types -p fgdb-claim -p fgdb-delta-types -p fgdb-evidence -p fgdb-resource --all-targets
cargo test -p fgdb-claim --doc

echo "==> reproduce the foundation transcript twice"
cargo run --quiet -p fgdb-delta-types --example foundation_transcript >"$FIRST"
cargo run --quiet -p fgdb-delta-types --example foundation_transcript >"$SECOND"
cmp "$FIRST" "$SECOND"

echo "==> assert the scripted typed rejections are present"
grep -q "lattice violation (typed): claim-lattice violation" "$FIRST"
grep -q "rejection (typed): resource ceiling exceeded on cpu_micros" "$FIRST"
grep -q "scalar reject non-canonical float" "$FIRST"
grep -q "zweight demoted back: Some(170141183460469231731687303715884105727)" "$FIRST"

echo "==> transcript sha256"
sha256sum "$FIRST"
echo "foundation-types E2E GREEN; retained deterministic evidence: $EVIDENCE_DIR"
