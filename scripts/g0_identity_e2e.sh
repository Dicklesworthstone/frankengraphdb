#!/usr/bin/env bash
# =============================================================================
# g0_identity_e2e.sh — end-to-end proof of the identity constitution
# (bead fgdb-g0-identity-registries-hrx)
#
# Validates the five disjoint identity-class registries plus
# durable_fields.toml, rebuilds the generated checks (reference unions,
# construction DAG, BodyDigest recipes, code-space laws), and runs the
# negative-fixture set, exiting nonzero on the first divergence. JSONL
# evidence (per-registry row counts, reserved-W12 coverage, digest recipes)
# is retained so later format work can diff identity behavior against this
# baseline.
#
# Byte-level golden-corpus encoding/decoding is w1-generated-parsers scope
# (the corpus paths are reserved in the registries; the walkers are
# stub-registered in checker_index.toml) — this e2e proves the registry-level
# identity laws that G0 owns.
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${G0_E2E_WORKDIR:-$(mktemp -d)}"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="$TARGET_DIR/debug/registry-check"
PASS=0
FAIL=0

log() { printf '[g0-identity-e2e] %s\n' "$*"; }
ok()  { PASS=$((PASS + 1)); log "PASS: $*"; }
die() { FAIL=$((FAIL + 1)); log "FAIL: $*"; }

log "work directory: $WORK"
mkdir -p "$WORK"

log "building registry-check"
(cd "$ROOT" && cargo build -p registry-check --quiet)
[ -x "$BIN" ] || { log "registry-check binary missing at $BIN"; exit 2; }

# --- Phase 1: shipped identity registries validate ---------------------------
log "phase 1: shipped identity registries (all six artifacts)"
if "$BIN" identity --root "$ROOT" >"$WORK/identity-baseline.jsonl" 2>"$WORK/identity-baseline.err"; then
  ok "shipped identity registries validate cleanly"
else
  die "shipped identity registries failed (see $WORK/identity-baseline.jsonl)"
fi
for reg in logical_object_kinds physical_record_kinds bootstrap_frames \
           prebootstrap_artifact_kinds wire_types durable_fields; do
  grep -q "\"event\":\"registry_generated\",\"registry\":\"$reg\".*\"outcome\":\"pass\"" \
    "$WORK/identity-baseline.jsonl" \
    && ok "registry_generated pass: $reg" \
    || die "missing/failed registry_generated for $reg"
done
grep -q '"event":"dag_checked".*"faults":0,"outcome":"pass"' "$WORK/identity-baseline.jsonl" \
  && ok "construction DAG acyclic with zero faults" \
  || die "construction DAG check missing or failed"
DIGESTS=$(grep -c '"event":"digest_verified".*"outcome":"pass"' "$WORK/identity-baseline.jsonl" || true)
if [ "$DIGESTS" -ge 6 ]; then
  ok "digest recipes verified ($DIGESTS rows incl. the §5.1-named BodyDigest recipes)"
else
  die "expected >= 6 digest_verified passes, found $DIGESTS"
fi

# --- Phase 2: negative fixtures ----------------------------------------------
stage() { # stage <name> -> stages registries into $WORK/<name>/registries
  local name="$1"
  mkdir -p "$WORK/$name/registries"
  cp "$ROOT"/registries/*.toml "$WORK/$name/registries/"
}

log "phase 2a: planted future-result edge (command input naming its applied record)"
stage neg-future
cat >> "$WORK/neg-future/registries/durable_fields.toml" <<'EOF'

[[field]]
containing_schema = "CommitCommand"
field_tag = 91
stable_name = "my_applied_record"
exact_wire_type = "StrongRef"
cardinality = "one"
identity_class = "logical"
reference_semantics = "strong"
target_schema_id = "LogicalCommandRecord"
construction_order = 10
role_predicate = "true"
retention_and_cut_rule = "planted"
version_status = "active"
max_size_bytes = 40
EOF
if "$BIN" identity --root "$WORK/neg-future" >"$WORK/neg-future.jsonl" 2>/dev/null; then
  die "future-result edge accepted"
else
  ok "future-result edge rejected"
fi
grep -q '"code":"dag_future_result".*CommitCommand#my_applied_record' "$WORK/neg-future.jsonl" \
  && ok "violation names the exact edge (CommitCommand#my_applied_record)" \
  || die "dag_future_result violation missing exact row"

log "phase 2b: planted StrongRef-to-placement (physical record as strong target)"
stage neg-placement
cat >> "$WORK/neg-placement/registries/durable_fields.toml" <<'EOF'

[[field]]
containing_schema = "RootManifest"
field_tag = 92
stable_name = "placement_shortcut"
exact_wire_type = "StrongRef"
cardinality = "one"
identity_class = "logical"
reference_semantics = "strong"
target_schema_id = "PlacementRecord"
construction_order = 40
role_predicate = "true"
retention_and_cut_rule = "planted"
version_status = "active"
max_size_bytes = 40
EOF
if "$BIN" identity --root "$WORK/neg-placement" >"$WORK/neg-placement.jsonl" 2>/dev/null; then
  die "StrongRef-to-placement accepted"
else
  ok "StrongRef-to-placement rejected"
fi
grep -q '"code":"ref_target_not_logical"' "$WORK/neg-placement.jsonl" \
  && ok "violation class is ref_target_not_logical" \
  || die "ref_target_not_logical violation missing"

log "phase 2c: planted experimental row in the production registry"
stage neg-experimental
cat >> "$WORK/neg-experimental/registries/logical_object_kinds.toml" <<'EOF'

[[kind]]
object_kind = 0xc001
name = "ExperimentalProbe"
status = "experimental"
construction_order = 10
role_predicate = "true"
max_size_bytes = 4096
golden_corpus = "corpus/fixture/"
EOF
if "$BIN" identity --root "$WORK/neg-experimental" >"$WORK/neg-experimental.jsonl" 2>/dev/null; then
  die "experimental row accepted in production registry"
else
  ok "experimental row rejected in production registry"
fi
grep -q '"code":"experimental_in_production"' "$WORK/neg-experimental.jsonl" \
  && ok "violation class is experimental_in_production" \
  || die "experimental_in_production violation missing"

log "phase 2d: planted BodyDigest recipe drift"
stage neg-recipe
sed -i 's|recipe_pin = "fnv1a64:2be6808e91bd9d0d"|recipe_pin = "fnv1a64:0000000000000000"|' \
  "$WORK/neg-recipe/registries/durable_fields.toml"
if "$BIN" identity --root "$WORK/neg-recipe" >"$WORK/neg-recipe.jsonl" 2>/dev/null; then
  die "recipe drift accepted"
else
  ok "recipe drift rejected"
fi
grep -q '"code":"bodydigest_pin_mismatch".*AuthorityBindingRecord#body_digest' "$WORK/neg-recipe.jsonl" \
  && ok "violation names the exact recipe (AuthorityBindingRecord#body_digest)" \
  || die "bodydigest_pin_mismatch missing exact row"

# --- Verdict -----------------------------------------------------------------
log "evidence: $WORK/{identity-baseline,neg-future,neg-placement,neg-experimental,neg-recipe}.jsonl"
log "result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
log "G0 identity e2e: ALL GREEN"
