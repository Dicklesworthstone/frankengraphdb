#!/usr/bin/env bash
# Honest registry-independent scalar pipeline gate for fgdb-w1-codecs-3x8.
#
# This is intentionally NOT the final codec_pipeline_e2e gate. It proves a
# deterministic fnx-generators Barabasi-Albert fixture, all three explicit
# scalar NeighborCodec arms, allocation-free scans of every encoded list,
# bounded pairwise logical-equivalence checks, graph-bound multi-prefix FOR
# VId/EId columns, and a diagnostic stable-ID adjacency transcript through
# scalar block/scan mechanics inside a pinned asupersync lab root task. It does
# not claim a production seal/run layout, codec chaos/cancellation coverage,
# durable framing, registered IDs, a durable logical digest, SIMD parity,
# OriginBirthOrder, delta-coded identity slots, or final graph-codec coverage.
#
# Evidence directories are retained: repository policy forbids automated file
# deletion, and the two complete transcripts are useful replay artifacts.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-codec-pipeline-scalar-e2e.XXXXXX")"
FIRST="$EVIDENCE_DIR/first.ndjson"
SECOND="$EVIDENCE_DIR/second.ndjson"
EVIDENCE_ONLY="$EVIDENCE_DIR/evidence-only.ndjson"

echo "==> verify direct dependencies stay inside fgdb and the pinned foundations"
grep -Fqx 'asupersync = { git = "https://github.com/Dicklesworthstone/asupersync", rev = "e464a484cb65c1a55be0d9c925e6e9c20318edcb", default-features = false }' crates/fgdb-codec/Cargo.toml
grep -Fqx 'fnx-generators = { git = "https://github.com/Dicklesworthstone/franken_networkx.git", rev = "9d710b1c33e99412c94de7fa4de2f7ce4954110f" }' crates/fgdb-codec/Cargo.toml
grep -Fq 'git+https://github.com/Dicklesworthstone/asupersync?rev=e464a484cb65c1a55be0d9c925e6e9c20318edcb#e464a484cb65c1a55be0d9c925e6e9c20318edcb' Cargo.lock
grep -Fq 'git+https://github.com/Dicklesworthstone/franken_networkx.git?rev=9d710b1c33e99412c94de7fa4de2f7ce4954110f#9d710b1c33e99412c94de7fa4de2f7ce4954110f' Cargo.lock
DIRECT_METADATA="$(cargo metadata --locked --offline --no-deps --format-version 1 | jq -r '
  .packages[]
  | select(.name == "fgdb-codec")
  | .dependencies[]
  | [.name, (.kind // "normal"), (.source // "path"), (.path // "")]
  | @tsv
' | LC_ALL=C sort)"
EXPECTED_DIRECT="$(
  printf '%s\t%s\t%s\t%s\n' \
    'asupersync' 'dev' 'git+https://github.com/Dicklesworthstone/asupersync?rev=e464a484cb65c1a55be0d9c925e6e9c20318edcb' '' \
    'fgdb-types' 'normal' 'path' "$ROOT/crates/fgdb-types" \
    'fnx-generators' 'dev' 'git+https://github.com/Dicklesworthstone/franken_networkx.git?rev=9d710b1c33e99412c94de7fa4de2f7ce4954110f' '' \
    | LC_ALL=C sort
)"
if [[ "$DIRECT_METADATA" != "$EXPECTED_DIRECT" ]]; then
  echo "ERROR: fgdb-codec direct dependency identities differ from the exact allowlist" >&2
  diff -u <(printf '%s\n' "$EXPECTED_DIRECT") <(printf '%s\n' "$DIRECT_METADATA") >&2 || true
  exit 1
fi

echo "==> check the scalar pipeline example from the locked offline graph"
cargo check --locked --offline -p fgdb-codec --example codec_pipeline_scalar_e2e

echo "==> run the pinned lab pipeline twice and compare every output byte"
cargo run --locked --offline --quiet -p fgdb-codec --example codec_pipeline_scalar_e2e >"$FIRST"
cargo run --locked --offline --quiet -p fgdb-codec --example codec_pipeline_scalar_e2e >"$SECOND"
cmp "$FIRST" "$SECOND"

echo "==> validate the exact seven CodecRunRow evidence keys"
grep '^{"codec_id":' "$FIRST" >"$EVIDENCE_ONLY"
if [[ "$(wc -l <"$EVIDENCE_ONLY")" -ne 67 ]]; then
  echo "ERROR: expected 64 StreamVByte rows, two identity rows, and one block row" >&2
  exit 1
fi
EVIDENCE_PATTERN='^\{"codec_id":"[^"]+","corpus_id":"[^"]+","entry_count":[0-9]+,"encoded_bytes":[0-9]+,"bytes_per_entry":(\{"numerator":[0-9]+,"denominator":[1-9][0-9]*\}|null),"dispatch_path":"scalar","output_checksum":\{"algorithm":"fnv1a64-output-evidence-v1","hex":"[0-9a-f]{16}"\}\}$'
if grep -Ev "$EVIDENCE_PATTERN" "$EVIDENCE_ONLY"; then
  echo "ERROR: an evidence row did not contain exactly the seven frozen keys" >&2
  exit 1
fi
if [[ "$(grep -c '"codec_id":"stream-vbyte-payload-fences-scalar-diagnostic"' "$EVIDENCE_ONLY")" -ne 64 ]]; then
  echo "ERROR: every fixture row needs complete StreamVByte payload/fence diagnostic evidence" >&2
  exit 1
fi
if [[ "$(grep -c '"codec_id":"block-scalar-diagnostic-transcript"' "$EVIDENCE_ONLY")" -ne 1 ]]; then
  echo "ERROR: the diagnostic adjacency transcript needs one block byte-evidence row" >&2
  exit 1
fi
if [[ "$(grep -c '"codec_id":"identity-shared-prefix-for-scalar-payload-diagnostic"' "$EVIDENCE_ONLY")" -ne 2 ]]; then
  echo "ERROR: expected exact scalar-payload evidence for both graph VIds and EIds" >&2
  exit 1
fi
grep -q '"corpus_id":"ba64-vertex-ids","entry_count":64,"encoded_bytes":144,"bytes_per_entry":{"numerator":9,"denominator":4}' "$EVIDENCE_ONLY"
grep -q '"corpus_id":"ba64-edge-ids","entry_count":183,"encoded_bytes":361,"bytes_per_entry":{"numerator":361,"denominator":183}' "$EVIDENCE_ONLY"

echo "==> validate the one explicit partial-scope summary and omissions"
if [[ "$(grep -c '^{"kind":"scope-summary"' "$FIRST")" -ne 1 ]]; then
  echo "ERROR: expected exactly one scope-summary row" >&2
  exit 1
fi
grep -q '"proof":"scalar-graph-codec-pipeline-v3"' "$FIRST"
grep -q '"scope":"registry-independent-partial-e2e"' "$FIRST"
grep -q '"fixture":"barabasi-albert-n64-m3-seed424242"' "$FIRST"
grep -q '"nodes":64,"edges":183,"adjacency_entries":366' "$FIRST"
grep -q '"neighbor_arms_per_list":3,"stream_evidence_rows":64' "$FIRST"
grep -q '"neighbor_scan_checks":192,"neighbor_equivalence_checks":576' "$FIRST"
grep -q '"identity_payload_evidence_rows":2' "$FIRST"
grep -q '"vertex_identity_rows":64,"edge_identity_rows":183' "$FIRST"
grep -q '"vertex_identity_prefixes":3,"edge_identity_prefixes":3' "$FIRST"
grep -q '"lab_scope":"root-task-lifecycle-only"' "$FIRST"
grep -q '"lab_quiescent":true,"lab_oracles_passed":true' "$FIRST"
grep -q '"omissions":\["durable-framing","registered-ids","logical-digest","simd-parity","origin-birth-order","identity-delta","production-seal-run","lab-chaos-cancellation","final-codec-pipeline-e2e"\]' "$FIRST"
if [[ "$(wc -l <"$FIRST")" -ne 68 ]]; then
  echo "ERROR: transcript must contain only 67 evidence rows and one summary" >&2
  exit 1
fi

echo "==> deterministic transcript sha256"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$FIRST"
else
  shasum -a 256 "$FIRST"
fi
echo "codec scalar registry-independent PARTIAL E2E GREEN; retained evidence: $EVIDENCE_DIR"
