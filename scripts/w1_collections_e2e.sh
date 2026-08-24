#!/usr/bin/env bash
# =============================================================================
# w1_collections_e2e.sh — the five-oracle collections matrix (fgdb-5mqr)
#
# This gate composes existing, independently attributable witnesses:
#   1. ART Levenshtein traversal against a simple dynamic-programming oracle.
#   2. Deterministic hash-table operations against BTreeMap and HashMap.
#   3. Succinct rank/select mutual inversion over a boundary-heavy corpus.
#   4. Scalar/SWAR hash-probe dispatch parity under each of two distinct seeds.
#   5. Real ART/hash/succinct cancellation with exact region-byte reclamation.
#
# The complete crate suite runs once. The matrix then requires one exact passing
# witness for every row; Cargo exit zero without a named witness is UNRUN, not
# green. A synthetic control proves the witness reader rejects both an omitted
# row and a duplicated row. This gate makes no latency, throughput, allocation-
# pressure, ART point/range benchmark, or consumer-Miri claim.
#
# Evidence is intentionally retained. Repository policy forbids automated
# deletion, and the transcript is useful for replay.
# =============================================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

# shellcheck source=lib/gate_verdict.sh
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_init "w1_collections_e2e"

EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-collections-e2e.XXXXXX")"
CONTROL_COMPLETE="$EVIDENCE_DIR/control-complete.log"
CONTROL_MISSING="$EVIDENCE_DIR/control-missing.log"
CONTROL_DUPLICATE="$EVIDENCE_DIR/control-duplicate.log"
SUITE_LOG="$EVIDENCE_DIR/cargo-test.log"

ART_MARKER="test art::tests::generated_token_product_walk_matches_simple_distance_oracle ... ok"
HASH_MARKER="test hash_table::tests::differential_operation_stream_matches_standard_maps ... ok"
SUCCINCT_MARKER="test rank_select_are_mutual_inverses ... ok"
DISPATCH_MARKER="test hash_table::tests::two_seeds_have_identical_physical_iteration_across_dispatches ... ok"
REGION_ART_MARKER="test art_growth_is_region_backed_and_cancel_reclaimed ... ok"
REGION_HASH_MARKER="test hash_growth_is_region_backed_and_cancel_reclaimed ... ok"
REGION_SUCCINCT_MARKER="test succinct_growth_is_region_backed_and_cancel_reclaimed ... ok"
REGION_CANCEL_MARKER="test cancelled_query_refuses_consumer_mutation_before_state_changes ... ok"

MATRIX_MARKERS=(
  "$ART_MARKER"
  "$HASH_MARKER"
  "$SUCCINCT_MARKER"
  "$DISPATCH_MARKER"
  "$REGION_ART_MARKER"
  "$REGION_HASH_MARKER"
  "$REGION_SUCCINCT_MARKER"
  "$REGION_CANCEL_MARKER"
)

marker_count() {
  local log="$1"
  local marker="$2"
  grep -Fxc -- "$marker" "$log" || true
}

matrix_complete() {
  local log="$1"
  local marker
  for marker in "${MATRIX_MARKERS[@]}"; do
    [ "$(marker_count "$log" "$marker")" -eq 1 ] || return 1
  done
  return 0
}

printf '%s\n' "${MATRIX_MARKERS[@]}" >"$CONTROL_COMPLETE"
printf '%s\n' "${MATRIX_MARKERS[@]:0:${#MATRIX_MARKERS[@]}-1}" >"$CONTROL_MISSING"
{
  printf '%s\n' "${MATRIX_MARKERS[@]}"
  printf '%s\n' "$ART_MARKER"
} >"$CONTROL_DUPLICATE"

if matrix_complete "$CONTROL_COMPLETE" \
  && ! matrix_complete "$CONTROL_MISSING" \
  && ! matrix_complete "$CONTROL_DUPLICATE"; then
  gate_pass "control: the matrix reader accepts one complete witness set and rejects missing or duplicate rows"
else
  gate_fail "control: the matrix reader did not distinguish complete, missing, and duplicate witness sets"
fi

SUITE_RC=0
cargo test --locked -p fgdb-collections --all-targets -- --test-threads=1 \
  >"$SUITE_LOG" 2>&1 || SUITE_RC=$?
if [ "$SUITE_RC" -eq 0 ]; then
  gate_pass "the complete fgdb-collections target suite ran and passed"
else
  case "$(gate_env_failure_class "$SUITE_LOG")" in
    rch-refusal|cargo-offline)
      gate_diag "  Cargo transcript: $SUITE_LOG"
      gate_diag "  retained evidence: $EVIDENCE_DIR"
      gate_abort_unrun "the fgdb-collections suite did not execute ($(gate_env_failure_class "$SUITE_LOG")); retryable environment refusal, not a product verdict"
      ;;
    *)
      gate_fail "the fgdb-collections target suite exited $SUITE_RC"
      gate_diag "  Cargo transcript: $SUITE_LOG"
      gate_diag "  retained evidence: $EVIDENCE_DIR"
      gate_verdict
      exit $?
      ;;
  esac
fi

if matrix_complete "$SUITE_LOG"; then
  gate_pass "the controlled matrix reader found every required witness exactly once"
else
  for marker in "${MATRIX_MARKERS[@]}"; do
    gate_diag "  witness count $(marker_count "$SUITE_LOG" "$marker"): $marker"
  done
  gate_unrun "the controlled matrix reader did not observe one complete witness set"
  gate_diag "  Cargo transcript: $SUITE_LOG"
  gate_diag "  retained evidence: $EVIDENCE_DIR"
  gate_verdict
  exit $?
fi

gate_pass "oracle 1/5: ART Levenshtein traversal matched the independent DP oracle"
gate_pass "oracle 2/5: hash operations matched both independent standard maps"
gate_pass "oracle 3/5: succinct rank/select mutual inversion passed"
gate_pass "oracle 4/5: scalar/SWAR physical parity passed independently under two seeds"
gate_pass "oracle 5/5: every real collection consumer balanced cancellation reclamation"

gate_diag "  retained evidence: $EVIDENCE_DIR"
gate_verdict
