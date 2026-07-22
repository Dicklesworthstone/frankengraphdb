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
die() { FAIL=$((FAIL + 1)); log "FAIL: $*"; exit 1; }

# Match required JSON fragments on one line without depending on field order.
# This deliberately recognizes only exact fragments; it is not a permissive
# substitute for JSON parsing.
jsonl_line_has_all() {
  local file="$1"
  shift
  local line fragment matched
  while IFS= read -r line; do
    matched=1
    for fragment in "$@"; do
      case "$line" in
        *"$fragment"*) ;;
        *) matched=0; break ;;
      esac
    done
    [ "$matched" -eq 1 ] && return 0
  done < "$file"
  return 1
}

# A structural identity load failure is currently wrapped by the CLI's
# run_error event.  The checker is moving to a dedicated load_error event, so
# accept exactly those two envelopes while requiring the precise typed path.
assert_load_error_path() {
  local file="$1"
  local expected_path="$2"
  if jsonl_line_has_all "$file" \
      '"event":"load_error"' \
      "\"path\":\"$expected_path\""; then
    return 0
  fi
  jsonl_line_has_all "$file" \
    '"event":"run_error"' \
    '"outcome":"error"' \
    "$expected_path"
}

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
  if grep -q "\"event\":\"registry_generated\",\"registry\":\"$reg\".*\"outcome\":\"pass\"" \
      "$WORK/identity-baseline.jsonl"; then
    ok "registry_generated pass: $reg"
  else
    die "missing/failed registry_generated for $reg"
  fi
done
if grep -q '"event":"dag_checked".*"faults":0,"outcome":"pass"' \
    "$WORK/identity-baseline.jsonl"; then
  ok "construction DAG acyclic with zero faults"
else
  die "construction DAG check missing or failed"
fi

# Freeze the six §5.1 BodyDigest recipe identities.  Counting every
# digest_verified event is unsound because target, transcript, and
# weak-identity digests are different identity laws.
BODY_RECIPES=(
  'AuthorityBindingRecord#body_digest|fnv1a64:2be6808e91bd9d0d'
  'RaftSnapshotLocal#body_digest|fnv1a64:3dedb18b0ac32f0c'
  'RaftSnapshotMeta#body_digest|fnv1a64:59702ceb4c836ec2'
  'RaftSnapshotShard#body_digest|fnv1a64:69bb104a85eb6128'
  'ShardHistoryInventory#body_digest|fnv1a64:7f652e0dd29a56aa'
  'GlobalKeyEnvelopeManifest#body_digest|fnv1a64:55336bfa5c150521'
)
for recipe in "${BODY_RECIPES[@]}"; do
  row_id="${recipe%%|*}"
  recipe_pin="${recipe#*|}"
  if jsonl_line_has_all "$WORK/identity-baseline.jsonl" \
      '"event":"digest_verified"' \
      '"digest_class":"body"' \
      "\"row_id\":\"$row_id\"" \
      "\"recipe_pin\":\"$recipe_pin\"" \
      '"outcome":"pass"'; then
    ok "BodyDigest recipe verified: $row_id ($recipe_pin)"
  else
    die "missing/failed BodyDigest recipe: $row_id ($recipe_pin)"
  fi
done
BODY_DIGEST_PASSES=$(awk '
  index($0, "\"event\":\"digest_verified\"") &&
  index($0, "\"digest_class\":\"body\"") &&
  index($0, "\"outcome\":\"pass\"") { count++ }
  END { print count + 0 }
' "$WORK/identity-baseline.jsonl")
if [ "$BODY_DIGEST_PASSES" -eq "${#BODY_RECIPES[@]}" ]; then
  ok "BodyDigest event closure is exact ($BODY_DIGEST_PASSES recipes)"
else
  die "expected exactly ${#BODY_RECIPES[@]} passing BodyDigest recipes, found $BODY_DIGEST_PASSES"
fi

# --- Phase 2: negative fixtures ----------------------------------------------
stage() { # stage <name> -> stages registries into $WORK/<name>/registries
  local name="$1"
  mkdir -p "$WORK/$name/registries"
  cp "$ROOT"/registries/*.toml "$WORK/$name/registries/"
}

stage_except() { # stage_except <name> <basename> -> leave one output uncreated
  local name="$1"
  local excluded="$2"
  local source basename
  mkdir -p "$WORK/$name/registries"
  for source in "$ROOT"/registries/*.toml; do
    basename="${source##*/}"
    [ "$basename" = "$excluded" ] || cp "$source" "$WORK/$name/registries/"
  done
}

expect_identity_violation() { # expect_identity_violation <fixture> <code> <registry> <row_id>
  local fixture="$1"
  local expected_code="$2"
  local expected_registry="$3"
  local expected_row_id="$4"
  local status
  if "$BIN" identity --root "$WORK/$fixture" \
      >"$WORK/$fixture.jsonl" 2>"$WORK/$fixture.err"; then
    die "$fixture unexpectedly passed"
  else
    status=$?
    if [ "$status" -ne 1 ]; then
      die "$fixture exited $status instead of the violation status 1"
    fi
  fi
  if jsonl_line_has_all "$WORK/$fixture.jsonl" \
      '"event":"violation"' \
      "\"code\":\"$expected_code\"" \
      "\"registry\":\"$expected_registry\"" \
      "\"row_id\":\"$expected_row_id\""; then
    ok "$fixture rejected with $expected_code at $expected_registry::$expected_row_id"
  else
    die "$fixture omitted exact $expected_code diagnostic for $expected_registry::$expected_row_id"
  fi
}

assert_only_violation_code() { # assert_only_violation_code <fixture> <code>
  local fixture="$1"
  local expected_code="$2"
  local violation_count expected_count
  violation_count=$(awk '
    index($0, "\"event\":\"violation\"") { count++ }
    END { print count + 0 }
  ' "$WORK/$fixture.jsonl")
  expected_count=$(awk -v code="$expected_code" '
    index($0, "\"event\":\"violation\"") &&
    index($0, "\"code\":\"" code "\"") { count++ }
    END { print count + 0 }
  ' "$WORK/$fixture.jsonl")
  if [ "$violation_count" -eq 1 ] && [ "$expected_count" -eq 1 ]; then
    ok "$fixture has exactly one violation: $expected_code"
  else
    die "$fixture expected only $expected_code, found $violation_count violations ($expected_count matching)"
  fi
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
if grep -q '"code":"dag_future_result".*CommitCommand#my_applied_record' \
    "$WORK/neg-future.jsonl"; then
  ok "violation names the exact edge (CommitCommand#my_applied_record)"
else
  die "dag_future_result violation missing exact row"
fi

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
if grep -q '"code":"ref_target_not_logical"' "$WORK/neg-placement.jsonl"; then
  ok "violation class is ref_target_not_logical"
else
  die "ref_target_not_logical violation missing"
fi

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
if grep -q '"code":"experimental_in_production"' \
    "$WORK/neg-experimental.jsonl"; then
  ok "violation class is experimental_in_production"
else
  die "experimental_in_production violation missing"
fi

log "phase 2d: planted BodyDigest recipe drift"
stage_except neg-recipe durable_fields.toml
awk '
  !changed && $0 == "recipe_pin = \"fnv1a64:2be6808e91bd9d0d\"" {
    print "recipe_pin = \"fnv1a64:0000000000000000\""
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/durable_fields.toml" \
  > "$WORK/neg-recipe/registries/durable_fields.toml"
if "$BIN" identity --root "$WORK/neg-recipe" >"$WORK/neg-recipe.jsonl" 2>/dev/null; then
  die "recipe drift accepted"
else
  ok "recipe drift rejected"
fi
if grep -q '"code":"bodydigest_pin_mismatch".*AuthorityBindingRecord#body_digest' \
    "$WORK/neg-recipe.jsonl"; then
  ok "violation names the exact recipe (AuthorityBindingRecord#body_digest)"
else
  die "bodydigest_pin_mismatch missing exact row"
fi

log "phase 2e: unsupported identity-registry schema_version"
stage_except neg-schema-version logical_object_kinds.toml
awk '
  !changed && $0 == "schema_version = 1" {
    print "schema_version = 2"
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/logical_object_kinds.toml" \
  > "$WORK/neg-schema-version/registries/logical_object_kinds.toml"
if "$BIN" identity --root "$WORK/neg-schema-version" \
    >"$WORK/neg-schema-version.jsonl" 2>"$WORK/neg-schema-version.err"; then
  die "schema_version = 2 accepted"
else
  status=$?
  if [ "$status" -eq 2 ]; then
    ok "schema_version = 2 rejected as a structural load error"
  else
    die "schema_version = 2 exited $status instead of 2"
  fi
fi
if assert_load_error_path "$WORK/neg-schema-version.jsonl" \
    'logical_object_kinds.toml.schema_version'; then
  ok "load error names logical_object_kinds.toml.schema_version"
else
  die "schema-version load error omitted its exact typed path"
fi

log "phase 2f: unknown identity-registry top-level key"
stage_except neg-unknown-top-level logical_object_kinds.toml
awk '
  !changed && $0 == "[registry]" {
    print "unknown_top_level = true"
    changed = 1
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/logical_object_kinds.toml" \
  > "$WORK/neg-unknown-top-level/registries/logical_object_kinds.toml"
if "$BIN" identity --root "$WORK/neg-unknown-top-level" \
    >"$WORK/neg-unknown-top-level.jsonl" 2>"$WORK/neg-unknown-top-level.err"; then
  die "unknown top-level key accepted"
else
  status=$?
  if [ "$status" -eq 2 ]; then
    ok "unknown top-level key rejected as a structural load error"
  else
    die "unknown top-level key exited $status instead of 2"
  fi
fi
if assert_load_error_path "$WORK/neg-unknown-top-level.jsonl" \
    'logical_object_kinds.toml.unknown_top_level'; then
  ok "load error names logical_object_kinds.toml.unknown_top_level"
else
  die "top-level-key load error omitted its exact typed path"
fi

log "phase 2g: unknown identity-registry row key"
stage_except neg-unknown-row logical_object_kinds.toml
awk '
  { print }
  !changed && $0 == "[[kind]]" {
    print "unknown_row_key = true"
    changed = 1
  }
  END { if (!changed) exit 42 }
' "$ROOT/registries/logical_object_kinds.toml" \
  > "$WORK/neg-unknown-row/registries/logical_object_kinds.toml"
if "$BIN" identity --root "$WORK/neg-unknown-row" \
    >"$WORK/neg-unknown-row.jsonl" 2>"$WORK/neg-unknown-row.err"; then
  die "unknown row key accepted"
else
  status=$?
  if [ "$status" -eq 2 ]; then
    ok "unknown row key rejected as a structural load error"
  else
    die "unknown row key exited $status instead of 2"
  fi
fi
if assert_load_error_path "$WORK/neg-unknown-row.jsonl" \
    'logical_object_kinds.toml.kind[0].unknown_row_key'; then
  ok "load error names logical_object_kinds.toml.kind[0].unknown_row_key"
else
  die "row-key load error omitted its exact typed path"
fi

log "phase 2h: registry epoch drift without a reviewed assignment change"
stage_except neg-registry-epoch logical_object_kinds.toml
awk '
  !changed && $0 == "registry_epoch = 1" {
    print "registry_epoch = 2"
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/logical_object_kinds.toml" \
  > "$WORK/neg-registry-epoch/registries/logical_object_kinds.toml"
expect_identity_violation \
  neg-registry-epoch registry_epoch_mismatch logical_object_kinds registry
assert_only_violation_code neg-registry-epoch registry_epoch_mismatch

log "phase 2i: duplicate-free released logical assignment rename/reuse"
stage_except neg-released-reuse logical_object_kinds.toml
awk '
  !changed && $0 == "name = \"MetaAuthorityBindingProjection\"" {
    print "name = \"ReleasedAssignmentReuseProbe\""
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/logical_object_kinds.toml" \
  > "$WORK/neg-released-reuse/registries/logical_object_kinds.toml"
expect_identity_violation \
  neg-released-reuse registry_assignment_drift logical_object_kinds registry
assert_only_violation_code neg-released-reuse registry_assignment_drift

log "phase 2j: missing explicit reference-union arm"
stage_except neg-missing-union-arm durable_fields.toml
awk '
  $0 == "[[reference_union_arm]]" && !removed {
    removed = 1
    skipping = 1
    next
  }
  skipping && $0 == "[[reference_union_arm]]" { skipping = 0 }
  !skipping { print }
  END { if (!removed) exit 42 }
' "$ROOT/registries/durable_fields.toml" \
  > "$WORK/neg-missing-union-arm/registries/durable_fields.toml"
expect_identity_violation \
  neg-missing-union-arm registry_assignment_drift durable_fields registry
assert_only_violation_code neg-missing-union-arm registry_assignment_drift

log "phase 2k: otherwise-valid unreviewed reference-union arm"
stage_except neg-extra-union-arm durable_fields.toml
awk '
  { print }
  END {
    print ""
    print "[[reference_union_arm]]"
    print "union_name = \"CommandRef\""
    print "containing_schema = \"LogicalCommandRecord\""
    print "field_tag = 3"
    print "arm_tag = 3"
    print "stable_name = \"AuthorityBindingRecord\""
    print "target_schema_id = \"AuthorityBindingRecord\""
    print "role = \"local\""
    print "identity_class = \"logical\""
    print "reference_semantics = \"strong\""
    print "role_predicate = \"role-local\""
    print "retention_and_cut_rule = \"planted otherwise-valid arm\""
    print "version_status = \"active\""
    print "max_size_bytes = 40"
  }
' "$ROOT/registries/durable_fields.toml" \
  > "$WORK/neg-extra-union-arm/registries/durable_fields.toml"
expect_identity_violation \
  neg-extra-union-arm registry_assignment_drift durable_fields registry
assert_only_violation_code neg-extra-union-arm registry_assignment_drift

log "phase 2l: reference-union role excluded by its anchor and container"
stage_except neg-union-role durable_fields.toml
awk '
  $0 == "[[reference_union]]" {
    in_union = 1
    target = 0
  }
  $0 == "[[reference_union_arm]]" {
    in_union = 0
    target = 0
  }
  in_union && $0 == "union_name = \"MandatoryInventoryRef\"" {
    target = 1
  }
  target && $0 == "role = \"local\"" {
    print "role = \"meta\""
    changed = 1
    target = 0
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/durable_fields.toml" \
  > "$WORK/neg-union-role/registries/durable_fields.toml"
expect_identity_violation \
  neg-union-role union_role_mismatch durable_fields MandatoryInventoryRef

# --- Verdict -----------------------------------------------------------------
log "evidence: $WORK/{identity-baseline,neg-future,neg-placement,neg-experimental,neg-recipe,neg-schema-version,neg-unknown-top-level,neg-unknown-row,neg-registry-epoch,neg-released-reuse,neg-missing-union-arm,neg-extra-union-arm,neg-union-role}.jsonl"
log "result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
log "G0 identity e2e: ALL GREEN"
