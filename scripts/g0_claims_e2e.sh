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
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET_DIR/debug/registry-check"
PASS=0
FAIL=0

log() { printf '[g0-claims-e2e] %s\n' "$*"; }
ok()  { PASS=$((PASS + 1)); log "PASS: $*"; }
die() { FAIL=$((FAIL + 1)); log "FAIL: $*"; }

log "work directory: $WORK"
mkdir -p "$WORK"

# --- Build the checker -------------------------------------------------------
log "building registry-check"
(cd "$ROOT" && cargo build -p registry-check --quiet)
[ -x "$BIN" ] || { log "registry-check binary missing at $BIN"; exit 2; }

# --- Phase 1: the shipped registries pass everything -------------------------
log "phase 1: shipped registries (validate + hash + lint + closure)"
if "$BIN" all --root "$ROOT" >"$WORK/shipped.jsonl" 2>"$WORK/shipped.err"; then
  ok "shipped registries pass validate/hash/lint/closure"
else
  die "shipped registries failed (see $WORK/shipped.jsonl)"
fi
grep -q '"event":"registry_validated"' "$WORK/shipped.jsonl" \
  && ok "registry_validated events present" \
  || die "missing registry_validated events"
grep -q '"event":"hash_checked".*"outcome":"pass"' "$WORK/shipped.jsonl" \
  && ok "twenty-ID hash pin verified" \
  || die "twenty-ID hash pin not verified"
grep -q '"event":"closure_computed".*"outcome":"pass"' "$WORK/shipped.jsonl" \
  && ok "activation closure computed for the sample capability manifest" \
  || die "activation closure missing or failed"

# --- Phase 2: planted unregistered claim marker (claims-lint, file/line) -----
log "phase 2: planted unregistered claim marker"
STAGE="$WORK/lint-stage"
mkdir -p "$STAGE/registries"
cp "$ROOT"/registries/*.toml "$STAGE/registries/"
# Seeded prose corpus: README plus one planted defect on a known line.
{
  echo "# Seeded corpus"
  echo "This paragraph cites the registered invariant FG-INV-04 legitimately."
  echo "This paragraph plants the unregistered claim FG-INV-77 as load-bearing."
} > "$STAGE/README.md"
: > "$STAGE/AGENTS.md"
: > "$STAGE/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"
if "$BIN" lint --root "$STAGE" >"$WORK/lint.jsonl" 2>/dev/null; then
  die "lint passed despite planted FG-INV-77"
else
  ok "lint failed as required on planted marker"
fi
grep -q '"event":"lint_hit","file":"README.md","line":3,"marker":"FG-INV-77"' "$WORK/lint.jsonl" \
  && ok "lint hit names exact file/line/marker (README.md:3 FG-INV-77)" \
  || die "lint hit missing exact file/line/marker (see $WORK/lint.jsonl)"

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
grep -q '"code":"class_escalation".*"row_id":"FG-INV-20.planted-escalation"' "$WORK/escalation.jsonl" \
  && ok "escalation violation names the exact clause" \
  || die "class_escalation violation missing (see $WORK/escalation.jsonl)"

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
grep -q '"extra":\["FG-INV-21"\]' "$WORK/spine.jsonl" \
  && ok "hash mismatch logs the exact row-level diff (extra FG-INV-21)" \
  || die "row-level diff missing from hash event (see $WORK/spine.jsonl)"

# --- Verdict -----------------------------------------------------------------
log "evidence: $WORK/{shipped,lint,escalation,spine}.jsonl"
log "result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
log "G0 claims e2e: ALL GREEN"
