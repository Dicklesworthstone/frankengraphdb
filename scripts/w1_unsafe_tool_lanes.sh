#!/usr/bin/env bash
# =============================================================================
# w1_unsafe_tool_lanes.sh — execute checked unsafe-boundary tool cells
# =============================================================================
# Owner: fgdb-4s28
#
# The complete 7-site x 3-tool posture lives in
# registries/unsafe_verification_lanes.toml.  This gate asks the authoritative
# Rust checker for the checked plan and executes every distinct returned
# workload, attributing its exit status to every cell that names it. It does not
# infer work from prose and does not treat a declared candidate as a pass.
# Adding a checked cell without teaching this runner its exact command produces
# UNRUN + FAIL rather than silently skipping it.
#
# Evidence directories are retained. Repository policy forbids automated
# deletion, and Miri transcripts are useful for replay.
# =============================================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

# shellcheck source=lib/gate_verdict.sh
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_init "w1_unsafe_tool_lanes"

EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-unsafe-tool-lanes.XXXXXX")"
PLAN="$EVIDENCE_DIR/checked-plan.tsv"
PLAN_ERR="$EVIDENCE_DIR/checked-plan.err"

if cargo run --quiet --locked -p registry-check --bin unsafe-ledger-check -- \
  --root . --checked-plan >"$PLAN" 2>"$PLAN_ERR"; then
  gate_pass "unsafe verification manifest and ledger form one checked plan"
else
  gate_fail "unsafe verification manifest could not produce a checked plan"
  gate_diag "  checker diagnostics: $PLAN_ERR"
  gate_diag "  retained evidence: $EVIDENCE_DIR"
  gate_verdict
  exit $?
fi

PLAN_COUNT="$(grep -c . "$PLAN" || true)"
if [ "$PLAN_COUNT" -eq 0 ]; then
  gate_unrun "unsafe verification manifest produced zero checked workloads"
  gate_diag "  retained evidence: $EVIDENCE_DIR"
  gate_verdict
  exit $?
fi
gate_pass "checked plan contains $PLAN_COUNT workload(s)"

# miri_outcome_from_rc <exit-status>
#
# Cargo/Miri uses ordinary non-zero statuses for a completed negative verdict,
# including an assertion failure or a Miri UB finding. Those are FAIL. Process
# interruption and runner-invocation statuses say that no verdict completed;
# those are UNRUN. Keeping this translation in one function prevents the two
# checked cells that share the arena workload from classifying the same exit
# differently.
miri_outcome_from_rc() {
  local rc="$1"
  case "$rc" in
    0)
      printf 'passed\n'
      ;;
    124 | 125 | 126 | 127)
      printf 'unrun\n'
      ;;
    *)
      if [ "$rc" -ge 129 ] && [ "$rc" -le 192 ]; then
        printf 'unrun\n'
      else
        printf 'failed\n'
      fi
      ;;
  esac
}

miri_unrun_detail() {
  local rc="$1"
  case "$rc" in
    124)
      printf 'timed out (exit 124)'
      ;;
    125)
      printf 'runner failed to determine an outcome (exit 125)'
      ;;
    126)
      printf 'runner command could not be invoked (exit 126)'
      ;;
    127)
      printf 'runner command was not found (exit 127)'
      ;;
    *)
      printf 'was interrupted by signal %d (exit %d)' "$((rc - 128))" "$rc"
      ;;
  esac
}

if [ "$(miri_outcome_from_rc 0)" = "passed" ] \
  && [ "$(miri_outcome_from_rc 101)" = "failed" ] \
  && [ "$(miri_outcome_from_rc 124)" = "unrun" ] \
  && [ "$(miri_outcome_from_rc 126)" = "unrun" ] \
  && [ "$(miri_outcome_from_rc 130)" = "unrun" ] \
  && [ "$(miri_outcome_from_rc 137)" = "unrun" ] \
  && [ "$(miri_outcome_from_rc 143)" = "unrun" ]; then
  gate_pass "Miri outcome classifier distinguishes completed failures from unrun workloads"
else
  gate_fail "Miri outcome classifier does not preserve pass/fail/unrun"
fi

EXECUTED=0
MIRI_ARENA_STATUS="not-started"
MIRI_ARENA_RC=0
MIRI_ARENA_UNRUN_DETAIL=""
MIRI_ARENA_LOG="$EVIDENCE_DIR/miri-arena-edit-path.log"
while IFS=$'\t' read -r tool site workload; do
  [ -n "$tool" ] || continue
  case "$tool|$site|$workload" in
    "miri|arena-region-blocks-mut|cargo miri test --locked -p fgdb-unsafe-arena --test edit_path_differential" | \
    "miri|arena-region-vec-allocator|cargo miri test --locked -p fgdb-unsafe-arena --test edit_path_differential")
      if ! rustup component list --installed | grep -q '^miri-'; then
        gate_unrun "$tool $site: pinned Miri component is not installed"
        continue
      fi
      if ! rustup component list --installed | grep -q '^rust-src$'; then
        gate_unrun "$tool $site: pinned rust-src component is not installed"
        continue
      fi
      if [ "$MIRI_ARENA_STATUS" = "not-started" ]; then
        if cargo miri test --locked -p fgdb-unsafe-arena \
          --test edit_path_differential >"$MIRI_ARENA_LOG" 2>&1; then
          MIRI_ARENA_RC=0
        else
          MIRI_ARENA_RC=$?
        fi
        MIRI_ARENA_STATUS="$(miri_outcome_from_rc "$MIRI_ARENA_RC")"
        if [ "$MIRI_ARENA_STATUS" = "unrun" ]; then
          MIRI_ARENA_UNRUN_DETAIL="$(miri_unrun_detail "$MIRI_ARENA_RC")"
        fi
      fi
      case "$MIRI_ARENA_STATUS" in
        passed)
          EXECUTED=$((EXECUTED + 1))
          gate_pass "$tool $site: arena edit-path and typed-region differential passed"
          ;;
        failed)
          EXECUTED=$((EXECUTED + 1))
          gate_fail "$tool $site: edit-path differential failed"
          gate_diag "  Miri transcript: $MIRI_ARENA_LOG"
          ;;
        unrun)
          gate_unrun "$tool $site: Miri workload $MIRI_ARENA_UNRUN_DETAIL before completing"
          gate_diag "  Miri transcript: $MIRI_ARENA_LOG"
          ;;
        *)
          gate_unrun "$tool $site: Miri outcome classifier returned an unknown state"
          gate_diag "  Miri transcript: $MIRI_ARENA_LOG"
          ;;
      esac
      ;;
    *)
      gate_unrun "$tool $site: checked workload has no fail-closed runner dispatch"
      gate_diag "  unrecognized workload: $workload"
      ;;
  esac
done <"$PLAN"

if [ "$EXECUTED" -ne "$PLAN_COUNT" ]; then
  gate_diag "  executed $EXECUTED of $PLAN_COUNT checked workload(s)"
else
  gate_pass "executed every checked unsafe verification workload"
fi
gate_diag "  retained evidence: $EVIDENCE_DIR"
gate_verdict
