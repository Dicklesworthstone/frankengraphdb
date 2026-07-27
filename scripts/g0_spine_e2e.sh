#!/usr/bin/env bash
# =============================================================================
# g0_spine_e2e.sh — end-to-end proof of the twenty-invariant spine
# (bead fgdb-g0-invariant-spine-tmm)
#
# Verifies the materialized registry: the twenty-ID table hash, resolution of
# every checker and negative-test symbol (stub-registered pre-Genesis), the
# activation closure for the sample capability manifest, and both negative
# fixtures (a twenty-first ID; a reachable-but-inactive clause), asserting
# each fails naming the exact clause. JSONL evidence retained for later gates
# to diff activation drift against this baseline.
#
# Two properties of this harness are load-bearing and were repaired under bead
# fgdb-g0-spine-e2e-red-measures-harness-not-spine-iy7e:
#
#   * The subject artifact is compiled from THIS tree into a private path. It
#     used to be resolved by path out of the shared CARGO_TARGET_DIR, so the
#     gate reported on whichever artifact happened to be there.
#   * EVERY negative assertion is attributed, through one shared reader
#     (`classify_negative_run`): the checker must be shown to have RUN, and the
#     failure must be shown to be the law under test. Both phases used to
#     accept any non-zero exit. Phase 3 passed on a fixture that never parsed;
#     phase 2 was worse — MEASURED, it printed "PASS: validate failed as
#     required on twenty-first ID" against a staged spine with no twenty-first
#     ID in it at all, because the staged copy fails validation for unrelated
#     reasons anyway. Each phase carries an in-band control that re-runs the
#     shared reader on a fixture that cannot parse and requires a different
#     verdict.
#
# WHAT THE CONTROLS GUARD, AND WHAT THEY DO NOT. They guard the RULE that
# decides whether a failure counts — widen `classify_negative_run` back to "any
# non-zero exit" and both controls go red in the same run, which is the point of
# there being one reader rather than one per phase. They do NOT guard against a
# future call site that bypasses the reader entirely and tests `$?` inline; no
# control can, short of re-running the assertion itself. That gap is stated here
# rather than left for a reader to discover, and it belongs to
# fgdb-validator-laws-never-witnessed-firing-xnxy's population.
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${G0_E2E_WORKDIR:-$(mktemp -d)}"
BIN="$WORK/bin/registry-check"
PASS=0
FAIL=0

log() { printf '[g0-spine-e2e] %s\n' "$*"; }
ok()  { PASS=$((PASS + 1)); log "PASS: $*"; }
die() { FAIL=$((FAIL + 1)); log "FAIL: $*"; }

# This gate already records an assertion failure and keeps going, so its verdict
# can say "3 failed". What it could not say is that it never got there: under
# `set -e` any unguarded command ends the run before the tally at the bottom, and
# MEASURED 2026-07-26 the output then carries ZERO tally lines -- the last thing a
# reader sees is a PASS line and a raw shell error, with nothing stating that the
# remaining assertions did not run. A truncated log read exactly like a whole one.
VERDICT_REACHED=0
report_partial_tally() {
  local rc=$?
  [ "$VERDICT_REACHED" -eq 1 ] && return 0
  log "ABORTED before the verdict (exit $rc): $PASS passed, $FAIL failed so far; every assertion after this point did not run"
  return 0
}
trap report_partial_tally EXIT

log "work directory: $WORK"
mkdir -p "$WORK/bin"

# --- The subject artifact ----------------------------------------------------
# Compiled from THIS tree into $WORK by scripts/lib/private_subject.sh, which
# is the single implementation shared with g0_claims_e2e.sh and
# g0_identity_e2e.sh — three copies would be three readers to drift, which is
# the defect this bead exists to fix. That file carries the measurement, the
# reason cargo is not used, and the 73MB-per-run disk price.
# shellcheck source=lib/private_subject.sh
. "$ROOT/scripts/lib/private_subject.sh"

log "building registry-check from this tree into $WORK/bin"
if ! subject_build "$ROOT" "$WORK/bin"; then
  log "FATAL: building registry-check from this tree failed (see $WORK/bin/build.log)"
  exit 2
fi
subject_is_fresh "$BIN" "$ROOT" || {
  log "FATAL: $BIN is not newer than $(subject_newest_source "$ROOT") — the build did not produce this tree's artifact"
  exit 2
}
log "subject artifact: $BIN (newer than $(subject_newest_source "$ROOT"))"

# THIS SCRIPT'S OWN control over the shared predicate, counted in this script's
# own tally. The precondition above is what stops a foreign or stale artifact
# from producing a verdict; this proves that precondition can actually fire,
# here, rather than inheriting credit from the other two gates that source the
# same library. Weaken subject_is_fresh to a constant and all three go red.
subject_write_stale_probe "$WORK/bin/stale-probe"
if subject_is_fresh "$BIN" "$ROOT" && ! subject_is_fresh "$WORK/bin/stale-probe" "$ROOT"; then
  ok "control: the freshness rule accepts this run's artifact and rejects a backdated one"
else
  die "control: the freshness rule does not separate a fresh artifact from a stale one; this script's subject is unproven"
fi

# --- Phase 1: materialized spine passes validate + hash + closure ------------
log "phase 1: materialized spine (validate + hash + closure baseline)"
if "$BIN" all --root "$ROOT" >"$WORK/spine-baseline.jsonl" 2>"$WORK/spine-baseline.err"; then
  ok "materialized spine passes validate/hash/lint/closure"
else
  die "materialized spine failed (see $WORK/spine-baseline.jsonl)"
fi
CLAUSES=$(grep -c '"event":"clause_checked"' "$WORK/spine-baseline.jsonl" || true)
if [ "$CLAUSES" -ge 20 ]; then
  ok "clause_checked events for all materialized clauses ($CLAUSES >= 20)"
else
  die "expected >= 20 clause_checked events, found $CLAUSES"
fi
grep -q '"event":"hash_checked".*"outcome":"pass"' "$WORK/spine-baseline.jsonl" \
  && ok "twenty-ID table hash verified" \
  || die "twenty-ID table hash not verified"
grep -q '"event":"closure_computed".*"absent":0.*"outcome":"pass"' "$WORK/spine-baseline.jsonl" \
  && ok "pre-Genesis sample-manifest closure satisfied (no reachable stubs)" \
  || die "baseline closure not satisfied"
if grep -q '"code":"missing_checker"' "$WORK/spine-baseline.jsonl"; then
  die "unresolvable checker/negative-test symbol on the shipped spine"
else
  ok "every checker and negative-test symbol resolves (stub-registered)"
fi

# --- Shared instrument: how a negative gate is allowed to conclude -----------
#
# ONE READER for every negative assertion in this script and for the vacuity
# control beside each one. A control cannot then pass by exercising a rule its
# assertion does not use, and a repair to "did the tool even run" lands once
# rather than in whichever phase the fixer happens to open.
#
# This is the bead's own defect, generalized. A non-zero exit from a checker
# means one of three things: the fixture never parsed (exit 2, run_error), so
# the run says nothing about any law; the checker ran but something OTHER than
# the law under test failed it; or the law fired. Only the third is evidence.
# Until bead fgdb-g0-spine-e2e-red-measures-harness-not-spine-iy7e both phases
# below accepted all three and reported PASS.
#
# classify_negative_run <jsonl> <exit-code> <evidence-event> <attribution-pattern>
classify_negative_run() {
  local jsonl="$1" rc="$2" evidence="$3" attribution="$4"
  if [ "$rc" -eq 0 ]; then echo "tool_passed"; return; fi
  if grep -q '"event":"run_error"' "$jsonl"; then echo "never_ran"; return; fi
  if ! grep -q "\"event\":\"$evidence\"" "$jsonl"; then echo "no_evidence"; return; fi
  if ! grep -qE "$attribution" "$jsonl"; then echo "failed_elsewhere"; return; fi
  echo "attributed"
}

# --- Phase 2: twenty-first ID fails naming the exact row ---------------------
log "phase 2: planted twenty-first invariant ID"

# write_planted_spine <dir> <twenty_first | unparseable>
# One writer for the live fixture and its control, so the control differs from
# the fixture in exactly one thing: whether the file can be read at all.
write_planted_spine() {
  local dir="$1" mode="$2"
  mkdir -p "$dir/registries"
  cp "$ROOT"/registries/*.toml "$dir/registries/"
  case "$mode" in
    twenty_first)
      cat >> "$dir/registries/invariants.toml" <<'EOF'

[[invariant]]
id = "FG-INV-21"
title = "planted illegal twenty-first row"
EOF
      ;;
    unparseable)
      # The same planted row with its table header left unclosed. validate
      # cannot read the registry at all: exit 2, run_error, no law checked.
      cat >> "$dir/registries/invariants.toml" <<'EOF'

[[invariant]
id = "FG-INV-21"
title = "planted illegal twenty-first row"
EOF
      ;;
    *)
      log "internal error: unknown planted-spine mode $mode"
      exit 2
      ;;
  esac
}

# The law fired at all; that it named the right row is asserted separately
# below, exactly as phase 3 separates "the closure found something absent"
# from "it named FG-INV-04.core".
TWENTY_ID_ATTRIBUTION='"code":"twenty_id_violation"'

# CONTROL FIRST, as in phase 3 and as the binary reports closure_self_test
# before its own verdicts. Until this commit phase 2's assertion was a bare
# `if "$BIN" validate …; then die; else ok; fi`, and MEASURED against this very
# fixture it printed "PASS: validate failed as required on twenty-first ID" on
# a registry no reader ever parsed. The phase as a whole still went red on the
# line beneath it, so the defect was a false PASS rather than a false green —
# but a reader scanning PASS lines was being told a law had fired that had not.
write_planted_spine "$WORK/spine-stage-unparseable" unparseable
P2_CONTROL_RC=0
"$BIN" validate --root "$WORK/spine-stage-unparseable" \
  >"$WORK/spine-neg-21-control.jsonl" 2>/dev/null || P2_CONTROL_RC=$?
P2_CONTROL_VERDICT="$(classify_negative_run "$WORK/spine-neg-21-control.jsonl" \
  "$P2_CONTROL_RC" registry_validated "$TWENTY_ID_ATTRIBUTION")"
if [ "$P2_CONTROL_RC" -ne 0 ] && [ "$P2_CONTROL_VERDICT" = "never_ran" ]; then
  ok "control: a staged spine that never parsed classifies never_ran, not attributed (exit $P2_CONTROL_RC)"
else
  die "control: unparseable staged spine classified $P2_CONTROL_VERDICT (exit $P2_CONTROL_RC); the twenty-first-ID assertion below is vacuous"
fi

SPINE="$WORK/spine-stage"
write_planted_spine "$SPINE" twenty_first
P2_RC=0
"$BIN" validate --root "$SPINE" >"$WORK/spine-neg-21.jsonl" 2>/dev/null || P2_RC=$?
P2_VERDICT="$(classify_negative_run "$WORK/spine-neg-21.jsonl" "$P2_RC" \
  registry_validated "$TWENTY_ID_ATTRIBUTION")"
case "$P2_VERDICT" in
  attributed)
    ok "validate failed on the twenty-ID law itself (exit $P2_RC, twenty_id_violation emitted over a spine that parsed)" ;;
  tool_passed)
    die "validate passed despite twenty-first ID" ;;
  never_ran)
    die "validate never ran: the staged spine did not parse (run_error), so exit $P2_RC proves nothing about the twenty-ID law (see $WORK/spine-neg-21.jsonl)" ;;
  no_evidence)
    die "validate exited $P2_RC without validating a single registry (see $WORK/spine-neg-21.jsonl)" ;;
  failed_elsewhere)
    die "validate failed without emitting twenty_id_violation: the red is not the twenty-ID law (see $WORK/spine-neg-21.jsonl)" ;;
esac
grep -q '"code":"twenty_id_violation".*FG-INV-21' "$WORK/spine-neg-21.jsonl" \
  && ok "violation names FG-INV-21 exactly" \
  || die "twenty_id_violation missing FG-INV-21 (see $WORK/spine-neg-21.jsonl)"

# --- Phase 3: reachable-but-inactive clause forces the capability off --------
log "phase 3: capability manifest enabling a stub-guarded feature"

# write_hot_manifest <path> <expected_reachable_clauses | omit>
# One writer for the live fixture and for the control below, so the control
# cannot differ from the fixture in any way except the one it is controlling.
write_hot_manifest() {
  local path="$1" expected="$2"
  cat > "$path" <<'EOF'
schema_version = 1
[manifest]
name = "e2e-hot"
description = "enables mvcc-visibility before its checker is live"
features = ["mvcc-visibility"]
postures = []
roles = []
EOF
  [ "$expected" = "omit" ] || printf 'expected_reachable_clauses = %s\n' "$expected" >> "$path"
}

# Phase 3's attribution: the closure must have found something absent. `absent`
# is the count on closure_computed, and `[1-9]` excludes the zero that a count
# mismatch or an undeclared atom would leave behind; `absent_clauses` cannot
# match, since `"absent"` there is followed by `_`. The specificity assertions
# further down name the exact clause and capability.
REACHABLE_STUB_ATTRIBUTION='"absent":[1-9]'

# The fixture's own `expected_reachable_clauses` is DERIVED, not frozen.
# 948e1a5 (bead fgdb-regcheck-closure-vacuous-no-control-hp0f) made the key
# required and swept registries/sample_capability_manifest.toml, but not this
# heredoc — which is how two assertions below went red for a reason that has
# nothing to do with the law they test. Freezing a number here would only
# re-arm that trap: this fixture's subject is the reachable-STUB law, and the
# count law already has its own test site — the shipped sample manifest phase 1
# checks, whose zero is licensed by closure_self_test. So the number is read
# back from the closure report for this very manifest. Deriving it cannot hide
# a broken closure compiler: a compiler that reached nothing would report
# absent=0, and the attributed assertion below rejects that outright.
write_hot_manifest "$WORK/hot-probe.toml" 0
"$BIN" closure --root "$ROOT" --manifest "$WORK/hot-probe.toml" \
  >"$WORK/spine-closure-probe.jsonl" 2>/dev/null || true
REACHED="$(sed -n 's/.*"event":"closure_computed".*"reachable":\([0-9][0-9]*\).*/\1/p' \
  "$WORK/spine-closure-probe.jsonl" | head -1)"
if [ -z "$REACHED" ]; then
  die "cannot derive expected_reachable_clauses: probe emitted no closure_computed (see $WORK/spine-closure-probe.jsonl)"
  REACHED=0
fi
log "derived expected_reachable_clauses=$REACHED for the hot manifest"

# CONTROL, reported before the measurement it licenses, exactly as the binary
# reports closure_self_test before its own closure verdicts. Until bead iy7e
# the assertion below was `if "$BIN" closure …; then die; else ok; fi`, which
# any non-zero exit satisfied — including the exit 2 this control produces, on
# an input the closure compiler never read. Revert the classifier to "non-zero
# exit ⇒ pass" and this line goes red.
write_hot_manifest "$WORK/hot-manifest-unparseable.toml" omit
CONTROL_RC=0
"$BIN" closure --root "$ROOT" --manifest "$WORK/hot-manifest-unparseable.toml" \
  >"$WORK/spine-closure-control.jsonl" 2>/dev/null || CONTROL_RC=$?
CONTROL_VERDICT="$(classify_negative_run "$WORK/spine-closure-control.jsonl" \
  "$CONTROL_RC" closure_computed "$REACHABLE_STUB_ATTRIBUTION")"
if [ "$CONTROL_RC" -ne 0 ] && [ "$CONTROL_VERDICT" = "never_ran" ]; then
  ok "control: a fixture that never parsed classifies never_ran, not attributed (exit $CONTROL_RC)"
else
  die "control: malformed fixture classified $CONTROL_VERDICT (exit $CONTROL_RC); the reachable-stub assertion below is vacuous"
fi

write_hot_manifest "$WORK/hot-manifest.toml" "$REACHED"
CLOSURE_RC=0
"$BIN" closure --root "$ROOT" --manifest "$WORK/hot-manifest.toml" \
  >"$WORK/spine-closure-hot.jsonl" 2>/dev/null || CLOSURE_RC=$?
CLOSURE_VERDICT="$(classify_negative_run "$WORK/spine-closure-hot.jsonl" \
  "$CLOSURE_RC" closure_computed "$REACHABLE_STUB_ATTRIBUTION")"
case "$CLOSURE_VERDICT" in
  attributed)
    ok "closure failed on the reachable stub clause itself (exit $CLOSURE_RC, closure_computed reports absent >= 1)" ;;
  tool_passed)
    die "closure passed despite reachable stub clause" ;;
  never_ran)
    die "closure never ran: the hot manifest did not parse (run_error), so exit $CLOSURE_RC proves nothing about the reachable-stub law (see $WORK/spine-closure-hot.jsonl)" ;;
  no_evidence)
    die "closure exited $CLOSURE_RC without emitting closure_computed (see $WORK/spine-closure-hot.jsonl)" ;;
  failed_elsewhere)
    die "closure failed with absent=0: the red is not the reachable-stub law (see $WORK/spine-closure-hot.jsonl)" ;;
esac
grep -q '"event":"closure_computed".*FG-INV-04.core' "$WORK/spine-closure-hot.jsonl" \
  && ok "closure names the exact absent clause (FG-INV-04.core)" \
  || die "absent clause not named (see $WORK/spine-closure-hot.jsonl)"
grep -q '"event":"capability_absent","capability":"mvcc-visibility"' "$WORK/spine-closure-hot.jsonl" \
  && ok "capability_absent names mvcc-visibility with its clauses" \
  || die "capability_absent event missing"

# --- Verdict -----------------------------------------------------------------
log "evidence: $WORK/{spine-baseline,spine-neg-21,spine-closure-probe,spine-closure-control,spine-closure-hot}.jsonl"
VERDICT_REACHED=1
log "result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
log "G0 spine e2e: ALL GREEN"
