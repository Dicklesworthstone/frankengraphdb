#!/usr/bin/env bash
# =============================================================================
# g0_claims_e2e.sh — end-to-end proof of the G0 claim constitution
# (bead fgdb-g0-claim-registries-myx)
#
# Authors the three registries plus a seeded prose corpus containing one
# planted unregistered load-bearing claim and one planted cross-class
# escalation, then runs schema validation, claims-lint, the activation-
# closure compiler for a sample capability manifest, and the twenty-ID hash
# pin — asserting each planted defect is caught with file/line and that the
# real shipped registries pass everything.
#
# Deterministic: no timestamps in assertions; JSONL evidence is written under
# a work directory that is printed at the end for inspection.
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${G0_E2E_WORKDIR:-$(mktemp -d)}"
BIN="$WORK/bin/registry-check"
PASS=0
FAIL=0

log() { printf '[g0-claims-e2e] %s\n' "$*"; }
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
mkdir -p "$WORK"

# --- Build the checker -------------------------------------------------------
# The subject is compiled from THIS tree into $WORK by
# scripts/lib/private_subject.sh, the single implementation shared with
# g0_spine_e2e.sh and g0_identity_e2e.sh. It used to be
# "${CARGO_TARGET_DIR:-$ROOT/target}/debug/registry-check" gated only on
# `[ -x "$BIN" ]` after a cargo build whose exit status nothing read; the
# library states what that measured and what it cost. Disk price here: 73MB per
# run, same as its two siblings.
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
# own tally. Sharing the runner must not mean sharing the credit: a gate that
# passes only because some OTHER script proved the predicate has no evidence
# about its own subject. The precondition above is what stops a foreign or
# stale artifact from producing a verdict here; this proves it can fire here.
subject_write_stale_probe "$WORK/bin/stale-probe"
if subject_is_fresh "$BIN" "$ROOT" && ! subject_is_fresh "$WORK/bin/stale-probe" "$ROOT"; then
  ok "control: the freshness rule accepts this run's artifact and rejects a backdated one"
else
  die "control: the freshness rule does not separate a fresh artifact from a stale one; this script's subject is unproven"
fi

# --- Phase 1: the shipped registries pass everything -------------------------
log "phase 1: shipped registries (validate + hash + lint + closure)"
if "$BIN" all --root "$ROOT" >"$WORK/shipped.jsonl" 2>"$WORK/shipped.err"; then
  ok "shipped registries pass validate/hash/lint/closure"
else
  die "shipped registries failed (see $WORK/shipped.jsonl)"
fi
if grep -q '"event":"registry_validated"' "$WORK/shipped.jsonl"; then
  ok "registry_validated events present"
else
  die "missing registry_validated events"
fi
if grep -q '"event":"hash_checked".*"outcome":"pass"' "$WORK/shipped.jsonl"; then
  ok "twenty-ID hash pin verified"
else
  die "twenty-ID hash pin not verified"
fi
if grep -q '"event":"closure_computed".*"outcome":"pass"' "$WORK/shipped.jsonl"; then
  ok "activation closure computed for the sample capability manifest"
else
  die "activation closure missing or failed"
fi

# --- Phase 2: planted claim defects, BOTH directions (claims-lint) -----------
# claims-lint answers two questions and a seeded corpus must plant one defect
# for each. Direction 1 — a marker that resolves to nothing. Direction 2 — a
# numeric budget that carries no marker at all, which direction 1 cannot see
# because an absent marker is not an unresolved one (bead
# fgdb-claims-lint-one-directional-unmarked-budgets-sdpv).
log "phase 2: planted claim defects in both lint directions"
STAGE="$WORK/lint-stage"
mkdir -p "$STAGE/registries"
cp "$ROOT"/registries/*.toml "$STAGE/registries/"

# The stage gets its own lint config: the shipped one points at six artifacts
# and at README's real thirteen-row gate table, none of which exist here.
write_stage_lint_config() {  # $1 = unmarked_rows body
  cat > "$STAGE/registries/claims_lint.toml" <<EOF
schema_version = 1

[lint]
marker_pattern = "FG-[A-Z]{2,5}-[0-9]{2}"
scan = ["README.md"]
closure_dirs = ["."]

[[gate_table]]
file = "README.md"
heading = "## Performance"
owner_bead = "g0-claims-e2e"
unmarked_rows = [$1]
EOF
}

# Seeded prose corpus. Line numbers below are asserted verbatim, so this block
# is the fixture: line 3 plants the unresolvable marker, line 10 plants the
# budget with no marker, and line 9 is the control row that cites a registered
# one (FG-INV-04 is a real registry row — the lint asks only that the citation
# resolve, not that the namespace suit the claim).
write_stage_readme() {
  {
    echo "# Seeded corpus"
    echo "This paragraph cites the registered invariant FG-INV-04 legitimately."
    echo "This paragraph plants the unregistered claim FG-INV-77 as load-bearing."
    echo ""
    echo "## Performance"
    echo ""
    echo "| Domain | Gate |"
    echo "|---|---|"
    echo "| Registered gate | < 50 ms (FG-INV-04) |"
    echo "| Planted budget | >= 40M edges/s sustained; p99 < 15 us |"
  } > "$STAGE/README.md"
}

write_stage_lint_config ""
write_stage_readme
if "$BIN" lint --root "$STAGE" >"$WORK/lint.jsonl" 2>/dev/null; then
  die "lint passed despite planted FG-INV-77 and an unmarked budget row"
else
  ok "lint failed as required on the planted defects"
fi
if grep -q '"event":"lint_hit","kind":"unregistered_marker","file":"README.md","line":3,"subject":"FG-INV-77"' "$WORK/lint.jsonl"; then
  ok "direction 1: hit names exact file/line/marker (README.md:3 FG-INV-77)"
else
  die "direction 1: hit missing exact file/line/marker (see $WORK/lint.jsonl)"
fi
if grep -q '"event":"lint_hit","kind":"unmarked_gate_row","file":"README.md","line":10,"subject":"Planted budget"' "$WORK/lint.jsonl"; then
  ok "direction 2: hit names the unmarked budget row (README.md:10 Planted budget)"
else
  die "direction 2: unmarked budget row not caught (see $WORK/lint.jsonl)"
fi
if grep -q '"event":"lint_completed","files_scanned":1,"markers_seen":3,"prose_files_seen":1,"gate_rows_read":2,"gate_rows_marked":1,"gate_rows_unmarked":1' "$WORK/lint.jsonl"; then
  ok "census reports what was opened (1 file, 3 markers, 2 gate rows, 1 marked)"
else
  die "census missing or wrong — a lint that examines nothing passes (see $WORK/lint.jsonl)"
fi

# CONTROL. Both failures above must come from the plants and from nothing else
# in the staging: remove the unresolvable marker, register the budget row in the
# ledger, change nothing else, and the same corpus must pass clean. Without this
# the phase proves only that the lint fails on SOMETHING.
write_stage_lint_config '"Planted budget"'
sed '3d' "$STAGE/README.md" > "$STAGE/README.clean" && mv "$STAGE/README.clean" "$STAGE/README.md"
if "$BIN" lint --root "$STAGE" >"$WORK/lint-control.jsonl" 2>/dev/null; then
  ok "control: the same staged corpus passes once both plants are removed"
else
  die "control: staged corpus fails for a reason other than the plants (see $WORK/lint-control.jsonl)"
fi
# ... and the ledger entry that licensed the budget row is itself checked: mark
# that row without deleting its ledger line and the lint must fail again, so the
# gap between claimed and registered budgets can only move deliberately.
sed 's/| Planted budget | >= 40M/| Planted budget | (FG-INV-04) >= 40M/' "$STAGE/README.md" > "$STAGE/README.marked" \
  && mv "$STAGE/README.marked" "$STAGE/README.md"
if "$BIN" lint --root "$STAGE" >"$WORK/lint-stale.jsonl" 2>/dev/null; then
  die "a stale unmarked_rows entry passed after its row was marked (see $WORK/lint-stale.jsonl)"
else
  ok "a ledger entry whose row is now marked fails as required"
fi
if grep -q '"kind":"dead_gate_exemption","file":"README.md","line":9,"subject":"Planted budget"' "$WORK/lint-stale.jsonl"; then
  ok "stale ledger entry named with file/line (README.md:9 Planted budget)"
else
  die "stale ledger entry not named (see $WORK/lint-stale.jsonl)"
fi

# --- Phase 3: planted cross-class escalation ---------------------------------
log "phase 3: planted cross-class escalation (slo justifying an invariant)"
ESC="$WORK/escalation-stage"
mkdir -p "$ESC/registries"
cp "$ROOT"/registries/*.toml "$ESC/registries/"
# Register a synthetic slo row, then plant a clause justified by it.
cat >> "$ESC/registries/slo.toml" <<'EOF'

[[slo]]
id = "FG-SLO-91"
claim_class = "slo"
qualified_claim = "planted synthetic latency budget"
required_disclosures = ["e2e fixture"]
operation_class = "SnapshotQuery"
posture = "quorum-one"
audit_class = "NotRequired"
EOF
cat >> "$ESC/registries/invariants.toml" <<'EOF'

[[invariant.clause]]
key = "FG-INV-20.planted-escalation"
claim_class = "invariant"
exact_statement = "planted clause claiming justification from an slo row"
activation_predicate = "true"
dependencies = []
checker_entrypoint = "claims_hash_twenty_id_pin"
negative_test_entrypoint = "claims_neg_waiver_present"
model_or_proof_scope = "n/a (e2e fixture)"
owner = "g0-e2e"
first_gate = "G1"
status = "live"
waiver = "forbidden"
justified_by = ["FG-SLO-91"]
EOF
if "$BIN" validate --root "$ESC" >"$WORK/escalation.jsonl" 2>/dev/null; then
  die "validate passed despite planted cross-class escalation"
else
  ok "validate failed as required on planted escalation"
fi
if grep -q '"code":"class_escalation".*"row_id":"FG-INV-20.planted-escalation"' "$WORK/escalation.jsonl"; then
  ok "escalation violation names the exact clause"
else
  die "class_escalation violation missing (see $WORK/escalation.jsonl)"
fi

# --- Phase 4: twenty-first ID breaks the hash pin ----------------------------
log "phase 4: planted twenty-first invariant ID"
SPINE="$WORK/spine-stage"
mkdir -p "$SPINE/registries"
cp "$ROOT"/registries/*.toml "$SPINE/registries/"
cat >> "$SPINE/registries/invariants.toml" <<'EOF'

[[invariant]]
id = "FG-INV-21"
title = "planted illegal twenty-first row"
EOF
if "$BIN" hash --root "$SPINE" >"$WORK/spine.jsonl" 2>/dev/null; then
  die "hash pin passed despite twenty-first ID"
else
  ok "hash pin failed as required on twenty-first ID"
fi
if grep -q '"extra":\["FG-INV-21"\]' "$WORK/spine.jsonl"; then
  ok "hash mismatch logs the exact row-level diff (extra FG-INV-21)"
else
  die "row-level diff missing from hash event (see $WORK/spine.jsonl)"
fi

# --- Verdict -----------------------------------------------------------------
log "evidence: $WORK/{shipped,lint,escalation,spine}.jsonl"
VERDICT_REACHED=1
log "result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
log "G0 claims e2e: ALL GREEN"
