#!/usr/bin/env bash
# End-to-end ADR governance check (fgdb-architecture-decision-record-xwkw).
#
# The temporary evidence directory is intentionally retained: repository policy
# forbids automated deletion, and the two streams are useful replay artifacts.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-architecture-e2e.XXXXXX")"
FIRST="$EVIDENCE_DIR/first.ndjson"
SECOND="$EVIDENCE_DIR/second.ndjson"

echo "==> build architecture-check"
cargo build -p registry-check --bin architecture-check

echo "==> validate the frozen ADR twice"
target/debug/architecture-check --root "$ROOT" >"$FIRST"
target/debug/architecture-check --root "$ROOT" >"$SECOND"
cmp "$FIRST" "$SECOND"

echo "==> assert deterministic event and provenance coverage"
test "$(rg -c '"event":"architecture_decision_checked"' "$FIRST")" -eq 256
test "$(rg -c '"event":"source_block_checked"' "$FIRST")" -eq 2
rg -q '"event":"architecture_registry_checked".*"decision_count":256.*"violations":0.*"outcome":"pass"' "$FIRST"
rg -q '"event":"source_block_checked".*"exact_match":true.*"outcome":"pass"' "$FIRST"
rg -q '"event":"architecture_decision_checked".*"decision_id":"FG-ADR-BET-B1".*"owner_bead":"fgdb-w2-commit-protocol-9w3u".*"owner_crate":"fgdb-branch".*"profile_id":"FG-ADR-PROFILE-CONSTITUTIONAL".*"rationale":.*"contradiction_class":"none".*"replay_command":.*"outcome":"pass"' "$FIRST"
for owner_kind in bead crate checker evidence; do
  rg -q '"event":"architecture_owner_indexed".*"owner_kind":"'"$owner_kind"'".*"decision_ids":.*"profile_ids":.*"rationales":.*"outcome":"pass"' "$FIRST"
done
rg -q '"event":"architecture_owner_indexed".*"owner_kind":"bead".*"owner_id":"fgdb-w2-commit-protocol-9w3u".*"decision_ids":\[[^]]*"FG-ADR-BET-B1"' "$FIRST"
if rg -q '"event":"architecture_violation"' "$FIRST"; then
  echo "unexpected architecture violation event" >&2
  exit 1
fi

echo "==> run typed mutation and property negatives"
cargo test -p registry-check --test architecture_decisions architecture_neg_

echo "ADR E2E GREEN; retained deterministic evidence: $EVIDENCE_DIR"
