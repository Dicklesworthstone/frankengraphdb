#!/usr/bin/env bash
# Registry-independent scalar-kernel slice for fgdb-w1-codecs-3x8.
#
# This is deliberately narrower than the final codec_pipeline_e2e gate: codec
# registry IDs, identity columns, durable framing, SIMD parity, cross-dispatch
# equivalence, and production graph seal/scan/intersect remain on the parent
# Bead. This gate covers the safe scalar kernel traits, NeighborCodec arms,
# roaring-like containers, deterministic block compression, and provenance-
# bound evidence for every byte encoder exercised by the transcript. Evidence
# is retained because repository policy forbids automated deletion and the
# transcript is useful for replay.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-codec-scalar-e2e.XXXXXX")"
FIRST="$EVIDENCE_DIR/first.txt"
SECOND="$EVIDENCE_DIR/second.txt"
EVIDENCE_ONLY="$EVIDENCE_DIR/evidence.ndjson"

echo "==> verify the scalar codec crate has no outside direct dependencies"
DIRECT_METADATA="$(cargo metadata --locked --offline --no-deps --format-version 1 | jq -r '
  .packages[]
  | select(.name == "fgdb-codec")
  | .dependencies[]
  | [.name, (.kind // "normal"), (.source // "path"), (.path // "")]
  | @tsv
' | LC_ALL=C sort)"
EXPECTED_DIRECT="$(
  printf '%s\t%s\t%s\t%s\n' \
    'asupersync' 'normal' 'git+https://github.com/Dicklesworthstone/asupersync?rev=3e8d08efe6617e08635fa8705c62a2a531373f43' '' \
    'fgdb-types' 'normal' 'path' "$ROOT/crates/fgdb-types" \
    'fnx-generators' 'dev' 'git+https://github.com/Dicklesworthstone/franken_networkx.git?rev=9d710b1c33e99412c94de7fa4de2f7ce4954110f' '' \
    | LC_ALL=C sort
)"
if [[ "$DIRECT_METADATA" != "$EXPECTED_DIRECT" ]]; then
  echo "ERROR: fgdb-codec direct dependency identities differ from the exact allowlist" >&2
  diff -u <(printf '%s\n' "$EXPECTED_DIRECT") <(printf '%s\n' "$DIRECT_METADATA") >&2 || true
  exit 1
fi

echo "==> run every scalar codec test target"
cargo test --locked --offline -p fgdb-codec --all-targets
cargo test --locked --offline -p fgdb-codec --doc

echo "==> reproduce the scalar codec transcript twice"
cargo run --locked --offline --quiet -p fgdb-codec --example scalar_transcript >"$FIRST"
cargo run --locked --offline --quiet -p fgdb-codec --example scalar_transcript >"$SECOND"
cmp "$FIRST" "$SECOND"

echo "==> validate provenance-bound scalar evidence rows"
grep '^{"codec_id":' "$FIRST" >"$EVIDENCE_ONLY"
if [[ "$(wc -l <"$EVIDENCE_ONLY")" -ne 5 ]]; then
  echo "ERROR: expected exactly five scalar encoder evidence rows" >&2
  exit 1
fi
EVIDENCE_PATTERN='^\{"codec_id":"[^"]+","corpus_id":"[^"]+","entry_count":[0-9]+,"encoded_bytes":[0-9]+,"bytes_per_entry":(\{"numerator":[0-9]+,"denominator":[1-9][0-9]*\}|null),"dispatch_path":"scalar","output_checksum":\{"algorithm":"fnv1a64-output-evidence-v1","hex":"[0-9a-f]{16}"\}\}$'
if grep -Ev "$EVIDENCE_PATTERN" "$EVIDENCE_ONLY"; then
  echo "ERROR: scalar encoder evidence row violated the frozen field contract" >&2
  exit 1
fi
for CODEC_ID in \
  uleb128-scalar-diagnostic \
  delta-varint-scalar-diagnostic \
  block-scalar-diagnostic \
  bitpack-scalar-diagnostic \
  for-bitpack-scalar-diagnostic
do
  if [[ "$(grep -c "\"codec_id\":\"$CODEC_ID\"" "$EVIDENCE_ONLY")" -ne 1 ]]; then
    echo "ERROR: expected one evidence row for $CODEC_ID" >&2
    exit 1
  fi
done

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
