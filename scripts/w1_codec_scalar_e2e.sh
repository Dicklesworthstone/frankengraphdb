#!/usr/bin/env bash
# Registry-independent scalar-kernel slice for fgdb-w1-codecs-3x8.
#
# This is deliberately narrower than the final codec_pipeline_e2e gate: codec
# registry IDs, identity columns, durable framing, SIMD parity, dispatch-path
# equivalence, and graph seal/scan/intersect remain on the parent Bead. This
# gate covers the safe scalar primitives, NeighborCodec arms, roaring-like
# containers, and deterministic block compression. Evidence is retained
# because repository policy forbids automated deletion and the transcript is
# useful for replay.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-codec-scalar-e2e.XXXXXX")"
FIRST="$EVIDENCE_DIR/first.txt"
SECOND="$EVIDENCE_DIR/second.txt"

echo "==> verify the scalar codec crate has no outside direct dependencies"
DIRECT_TREE="$(cargo tree --locked -p fgdb-codec --edges normal,dev,build --depth 1 --prefix none)"
OUTSIDE_DIRECT="$(grep -vE '^(fgdb-|asupersync )' <<<"$DIRECT_TREE" || true)"
if [[ -n "$OUTSIDE_DIRECT" ]]; then
  echo "ERROR: fgdb-codec has a direct dependency outside fgdb/asupersync" >&2
  echo "$OUTSIDE_DIRECT" >&2
  exit 1
fi

echo "==> run every scalar codec test target"
cargo test --locked -p fgdb-codec --all-targets
cargo test --locked -p fgdb-codec --doc

echo "==> reproduce the scalar codec transcript twice"
cargo run --locked --quiet -p fgdb-codec --example scalar_transcript >"$FIRST"
cargo run --locked --quiet -p fgdb-codec --example scalar_transcript >"$SECOND"
cmp "$FIRST" "$SECOND"

echo "==> assert canonical encodings and typed rejection paths"
grep -q "uleb128 max: ffffffffffffffffff01" "$FIRST"
grep -q "uleb128 reject nonminimal:" "$FIRST"
grep -q "delta_varint count=4: bytes=7f008001817e decoded=\[127, 127, 255, 16384\]" "$FIRST"
grep -q "delta_varint reject decreasing:" "$FIRST"
grep -q "block input=12 encoded=" "$FIRST"
grep -q "bitpack width=5 count=8:" "$FIRST"
grep -q "bitpack reject nonzero padding:" "$FIRST"
grep -q "for base=100 width=4 count=5:" "$FIRST"
grep -q "elias_fano count=10 low_bits=" "$FIRST"
grep -q "elias_fano rank_le(13)=7 select(7)=21" "$FIRST"
grep -q "roaring count=6 chunks=3 rank_le(10)=4 select(4)=Some(65536) intersection=\[2, 10, 65536\]" "$FIRST"
grep -q "neighbor codec=StreamVByte count=7 rank_le(128)=6 select(5)=Some(128) intersection=\[2, 3, 10, 1000\]" "$FIRST"

echo "==> transcript sha256"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$FIRST"
else
  shasum -a 256 "$FIRST"
fi
echo "codec scalar E2E GREEN; retained deterministic evidence: $EVIDENCE_DIR"
