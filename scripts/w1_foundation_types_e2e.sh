#!/usr/bin/env bash
# foundation_types_e2e (bead fgdb-w1-foundation-types-tjk): one deterministic
# pass over all six foundation crates — a verified STRICT_PORTABLE scalar
# profile, canonical scalar/Map/List values and exact coercion, ZWeight
# promotion across the i128 boundary, every
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

echo "==> verify direct dependencies stay inside the fgdb/asupersync universe"
for crate in fgdb-bigint fgdb-types fgdb-claim fgdb-delta-types fgdb-evidence fgdb-resource; do
  if cargo tree --locked -p "$crate" --edges normal,dev,build --depth 1 --prefix none | grep -vE '^(fgdb-|asupersync )' | grep -q .; then
    echo "ERROR: $crate has a direct dependency outside fgdb/asupersync" >&2
    exit 1
  fi
done

echo "==> run every foundation test target"
cargo test --locked -p fgdb-bigint -p fgdb-types -p fgdb-claim -p fgdb-delta-types -p fgdb-evidence -p fgdb-resource --all-targets
cargo test --locked -p fgdb-bigint -p fgdb-types -p fgdb-claim -p fgdb-delta-types -p fgdb-evidence -p fgdb-resource --doc

echo "==> reproduce the foundation transcript twice"
cargo run --locked --quiet -p fgdb-delta-types --example foundation_transcript -- 1 >"$FIRST"
cargo run --locked --quiet -p fgdb-delta-types --example foundation_transcript -- 2 >"$SECOND"
cmp "$FIRST" "$SECOND"

echo "==> assert the scripted typed rejections are present"
grep -q "lattice violation (typed): claim-lattice violation" "$FIRST"
grep -q "rejection (typed): resource ceiling exceeded on cpu_micros" "$FIRST"
grep -q "scalar reject non-canonical float" "$FIRST"
grep -q "scalar reject invalid memcomparable marker" "$FIRST"
grep -q "scalar reject absent collation resolver" "$FIRST"
grep -q "scalar reject missing collation artifact" "$FIRST"
grep -q "scalar reject forged collation sort key" "$FIRST"
grep -q "scalar reject absent tzdb resolver" "$FIRST"
grep -q "scalar reject missing tzdb artifact" "$FIRST"
grep -q "timestamp reject tzdb offset mismatch" "$FIRST"
grep -q "scalar profile oid=.*collations=1 missing-artifact-rejection=missing UnicodeData artifact" "$FIRST"
grep -q "property Map/List round trip: profile_oid=.*keys=a,aa,b bytes=" "$FIRST"
grep -q "scalar profile coercion: decimal=Decimal.*integer=Int(42).*float-rejection=unsupported scalar coercion Int -> Float" "$FIRST"
grep -q "decimal half-even boundary: source=25e-19 coefficient=2" "$FIRST"
grep -q "decimal reject profile overflow" "$FIRST"
grep -q "zweight demoted back: Some(170141183460469231731687303715884105727)" "$FIRST"
grep -q "replay grades: replayable=true; structural reproduced=2 omitted=1; verifiable missing=2; audit missing_or_redacted=2" "$FIRST"
grep -q "narrowed cx obligation: role=Commit kind=ReservePreparedBytes units=4096 resolution=Discharged stages=Acquisition,Transfer,Publication,Cleanup,Resolution" "$FIRST"
grep -q "merge capabilities: spawn=false time=false random=false io=false remote=false" "$FIRST"

echo "==> transcript sha256"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$FIRST"
else
  shasum -a 256 "$FIRST"
fi
echo "foundation-types E2E GREEN; retained deterministic evidence: $EVIDENCE_DIR"
