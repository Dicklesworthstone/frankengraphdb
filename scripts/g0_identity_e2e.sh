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

# Operational regeneration errors must retain one stable terminal envelope,
# emitted before the CLI's generic error. Counts are explicit even when the
# failure occurs before the projection-change census is available.
assert_regeneration_error_terminal() { # file projection changed unchanged published
  local file="$1"
  local projection_files="$2"
  local changed_files="$3"
  local unchanged_files="$4"
  local published_files="$5"
  jsonl_line_has_all "$file" \
    '"event":"appendix_regeneration_completed"' \
    "\"projection_files\":$projection_files" \
    "\"changed_files\":$changed_files" \
    "\"unchanged_files\":$unchanged_files" \
    "\"published_files\":$published_files" \
    '"violations":' \
    '"outcome":"error"' &&
    awk '
      index($0, "\"event\":\"appendix_regeneration_completed\"") {
        terminal_count++
        terminal_line = NR
      }
      index($0, "\"event\":\"run_error\"") {
        run_error_count++
        run_error_line = NR
      }
      END {
        exit !(terminal_count == 1 && run_error_count == 1 &&
               terminal_line < run_error_line)
      }
    ' "$file"
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

# --- Phase 0: canonical Appendix A source and projections -------------------
log "phase 0: canonical Appendix A catalog, exact source, and six projections"
if "$BIN" appendix --root "$ROOT" \
    >"$WORK/appendix-baseline.jsonl" 2>"$WORK/appendix-baseline.err"; then
  ok "canonical Appendix A catalog/source/projections validate cleanly"
else
  die "canonical Appendix A validation failed"
fi
if jsonl_line_has_all "$WORK/appendix-baseline.jsonl" \
    '"event":"appendix_source_manifest"' \
    '"start_line":1388' \
    '"end_line":2728' \
    '"line_count":1341' \
    '"byte_count":1020717' \
    '"sha256":"71a48b67304f94568590f79c5b1c1ee4731819aee022c57fece78a7e72bce7f1"' \
    '"outcome":"pass"'; then
  ok "Appendix A exact source manifest is pinned"
else
  die "Appendix A source-manifest event is missing or drifted"
fi
if jsonl_line_has_all "$WORK/appendix-baseline.jsonl" \
    '"event":"appendix_reference_manifest"' \
    '"target_count":813' \
    '"target_ids_sha256":"84276b6d97342e9ec1619424ddacb5b429e98e1862e03359afc837b65bb3392e"' \
    '"occurrence_count":2458' \
    '"occurrence_transcript_sha256":"9878e84c7c72d0e098a66794ce56a00ffdfed62aaf251bc0d87efd665e0a630b"' \
    '"outcome":"pass"'; then
  ok "full-plan Appendix A reference census is pinned"
else
  die "Appendix A reference-manifest event is missing or drifted"
fi
if jsonl_line_has_all "$WORK/appendix-baseline.jsonl" \
    '"event":"appendix_target_manifest"' \
    '"target_count":349' \
    '"projection_fallback_count":83' \
    '"target_source_assignment_sha256":"878ac35ac5df37458749ea1d3729df8d3220826abeb852359d010e3b281d7bc1"' \
    '"outcome":"pass"'; then
  ok "Appendix A target/source assignments are release-pinned"
else
  die "Appendix A target-manifest event is missing or drifted"
fi
APPENDIX_SLICE_PASSES=$(awk '
  index($0, "\"event\":\"appendix_slice_checked\"") &&
  index($0, "\"outcome\":\"pass\"") { count++ }
  END { print count + 0 }
' "$WORK/appendix-baseline.jsonl")
[ "$APPENDIX_SLICE_PASSES" -eq 21 ] \
  && ok "all 21 Appendix A slices validate" \
  || die "expected 21 passing Appendix A slices, found $APPENDIX_SLICE_PASSES"
APPENDIX_PROJECTION_PASSES=$(awk '
  index($0, "\"event\":\"appendix_projection_checked\"") &&
  index($0, "\"outcome\":\"pass\"") { count++ }
  END { print count + 0 }
' "$WORK/appendix-baseline.jsonl")
[ "$APPENDIX_PROJECTION_PASSES" -eq 6 ] \
  && ok "all six generated projections byte-match" \
  || die "expected six passing Appendix A projections, found $APPENDIX_PROJECTION_PASSES"
if jsonl_line_has_all "$WORK/appendix-baseline.jsonl" \
    '"event":"appendix_closure_checked"' \
    '"reservations":813' \
    '"existing_reservations":35' \
    '"reserved_reservations":778' \
    '"source_dispositions":848' \
    '"top_level_candidates":1229' \
    '"targets":349' \
    '"semantic_bindings":0' \
    '"evidence_rows":0' \
    '"reference_only_symbols":343' \
    '"appendix_structural_symbols":314' \
    '"outside_structural_symbols":0' \
    '"source_location_pairs":1999' \
    '"g0_projection_dispositions":35' \
    '"outcome":"pass"'; then
  ok "Appendix A source/target/owner/evidence scaffold closure is exact"
else
  die "Appendix A closure event is missing or drifted"
fi
if jsonl_line_has_all "$WORK/appendix-baseline.jsonl" \
    '"event":"appendix_completed"' \
    '"slices":21' \
    '"projection_rows":349' \
    '"projection_files":6' \
    '"reservations":813' \
    '"source_dispositions":848' \
    '"top_level_candidates":1229' \
    '"targets":349' \
    '"semantic_bindings":0' \
    '"evidence_rows":0' \
    '"reference_only_symbols":343' \
    '"violations":0' \
    '"outcome":"pass"'; then
  ok "Appendix A catalog closure is exact"
else
  die "Appendix A completion event is missing or incomplete"
fi
if (cd "$ROOT" && cargo test -p registry-check hash::tests --lib --quiet); then
  ok "SHA-256 standard vectors pass"
else
  die "SHA-256 standard vectors failed"
fi

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

# Freeze the §5.1 BodyDigest recipe identities.  Counting every
# digest_verified event is unsound because target, transcript, and
# weak-identity digests are different identity laws.
BODY_RECIPES=(
  'AuthorityBindingRecord#body_digest|fnv1a64:2be6808e91bd9d0d'
  'RootAuthorityTrustBody#body_digest|fnv1a64:1a58c8b267ed37c9'
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

stage_appendix_support() { # stage_appendix_support <name> -> non-registry proof inputs
  local name="$1"
  local manifest relative
  mkdir -p "$WORK/$name/.beads"
  cp "$ROOT/.beads/issues.jsonl" "$WORK/$name/.beads/"
  cp "$ROOT/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md" "$WORK/$name/"
  cp "$ROOT/Cargo.toml" "$WORK/$name/"
  for manifest in "$ROOT"/crates/*/Cargo.toml "$ROOT"/tools/*/Cargo.toml; do
    [ -f "$manifest" ] || continue
    relative="${manifest#"$ROOT"/}"
    mkdir -p "$WORK/$name/${relative%/*}"
    cp "$manifest" "$WORK/$name/$relative"
  done
  mkdir -p "$WORK/$name/scripts" "$WORK/$name/tools/registry-check/src"
  cp "$ROOT/scripts/g0_identity_e2e.sh" "$WORK/$name/scripts/"
  cp "$ROOT/tools/registry-check/src/appendix_a.rs" \
    "$WORK/$name/tools/registry-check/src/"
}

stage_appendix() { # stage_appendix <name> -> complete isolated Appendix root
  local name="$1"
  stage "$name"
  stage_appendix_support "$name"
}

stage_appendix_except() { # stage_appendix_except <name> <projection-basename>
  local name="$1"
  local excluded="$2"
  stage_except "$name" "$excluded"
  stage_appendix_support "$name"
}

snapshot_nonprojection_tree() { # snapshot_nonprojection_tree <staged-root>
  local staged_root="$1"
  (
    cd "$staged_root"
    find . \
      ! -path './registries/logical_object_kinds.toml' \
      ! -path './registries/physical_record_kinds.toml' \
      ! -path './registries/bootstrap_frames.toml' \
      ! -path './registries/prebootstrap_artifact_kinds.toml' \
      ! -path './registries/wire_types.toml' \
      ! -path './registries/durable_fields.toml' \
      -printf '%y|%m|%p|%l\n' | LC_ALL=C sort
    find . -type f \
      ! -path './registries/logical_object_kinds.toml' \
      ! -path './registries/physical_record_kinds.toml' \
      ! -path './registries/bootstrap_frames.toml' \
      ! -path './registries/prebootstrap_artifact_kinds.toml' \
      ! -path './registries/wire_types.toml' \
      ! -path './registries/durable_fields.toml' \
      -print0 | LC_ALL=C sort -z | xargs -0 sha256sum
  )
}

expect_appendix_violation() { # fixture code row_id
  local fixture="$1"
  local expected_code="$2"
  local expected_row_id="$3"
  local status
  if "$BIN" appendix --root "$WORK/$fixture" \
      >"$WORK/$fixture.jsonl" 2>"$WORK/$fixture.err"; then
    die "$fixture unexpectedly passed Appendix validation"
  else
    status=$?
    [ "$status" -eq 1 ] \
      || die "$fixture exited $status instead of Appendix violation status 1"
  fi
  if jsonl_line_has_all "$WORK/$fixture.jsonl" \
      '"event":"violation"' \
      "\"code\":\"$expected_code\"" \
      "\"row_id\":\"$expected_row_id\""; then
    ok "$fixture rejected with $expected_code at $expected_row_id"
  else
    die "$fixture omitted $expected_code at $expected_row_id"
  fi
}

expect_appendix_structural_error() { # fixture code row_id
  local fixture="$1"
  local expected_code="$2"
  local expected_row_id="$3"
  local status
  if "$BIN" appendix --root "$WORK/$fixture" \
      >"$WORK/$fixture.jsonl" 2>"$WORK/$fixture.err"; then
    die "$fixture unexpectedly passed Appendix validation"
  else
    status=$?
    [ "$status" -eq 2 ] \
      || die "$fixture exited $status instead of structural status 2"
  fi
  if jsonl_line_has_all "$WORK/$fixture.jsonl" \
      '"event":"violation"' \
      "\"code\":\"$expected_code\"" \
      "\"row_id\":\"$expected_row_id\""; then
    ok "$fixture rejected structurally with $expected_code at $expected_row_id"
  else
    die "$fixture omitted $expected_code at $expected_row_id"
  fi
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
  !changed && $0 == "registry_epoch = 2" {
    print "registry_epoch = 3"
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
    print "union_name = \"LogicalCommandInputRef\""
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

log "phase 2k1: reference-union name colliding with a reserved wire identity"
stage_except neg-reference-union-name-collision durable_fields.toml
awk '
  $0 == "exact_wire_type = \"LogicalCommandInputRef\"" {
    print "exact_wire_type = \"CommandRef\""
    changed++
    next
  }
  $0 == "union_name = \"LogicalCommandInputRef\"" {
    print "union_name = \"CommandRef\""
    changed++
    next
  }
  { print }
  END { if (changed != 4) exit 42 }
' "$ROOT/registries/durable_fields.toml" \
  > "$WORK/neg-reference-union-name-collision/registries/durable_fields.toml"
expect_identity_violation \
  neg-reference-union-name-collision reference_union_name_collision durable_fields CommandRef

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

# --- Phase 3: Appendix source/catalog/projection mutation corpus ------------
log "phase 3a: wrong Appendix slice Bead binding"
stage_appendix neg-appendix-bead
awk '
  !changed && $0 == "bead_id = \"fgdb-a01-reference-roots-2k0q\"" {
    print "bead_id = \"fgdb-a01-wrong-owner\""
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-bead/registries/appendix_a_catalog.toml"
expect_appendix_violation neg-appendix-bead catalog_pin_mismatch a01

log "phase 3a-redaction: attacker-controlled catalog values never reach diagnostics"
stage_appendix neg-appendix-redaction
APPENDIX_SECRET_SENTINEL='APPENDIX_SECRET_SENTINEL_7f7c9d5b'
awk -v sentinel="$APPENDIX_SECRET_SENTINEL" '
  !title_changed && $0 == "title = \"Appendix A exact catalog: Reference semantics, RootSlot, and RootBootstrap\"" {
    print "title = \"" sentinel "\""
    title_changed = 1
    next
  }
  !row_changed && $0 == "row_id = \"a03:logical-kind:logical-state-payload\"" {
    print "row_id = \"" sentinel "\""
    row_changed = 1
    next
  }
  { print }
  END { if (!title_changed || !row_changed) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-redaction/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-redaction catalog_row_id_derived_mismatch catalog_row
if grep -Fq "$APPENDIX_SECRET_SENTINEL" \
    "$WORK/neg-appendix-redaction.jsonl" \
    "$WORK/neg-appendix-redaction.err"; then
  die "Appendix diagnostic leaked attacker-controlled catalog text"
else
  ok "Appendix JSONL and stderr redact attacker-controlled catalog text"
fi

log "phase 3b: exact Appendix source-byte drift"
stage_appendix neg-appendix-source
awk '
  !changed && $0 == "## Appendix A — On-Disk Object Formats (normative contract)" {
    print "## Appendix X — On-Disk Object Formats (normative contract)"
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md" \
  > "$WORK/neg-appendix-source/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"
expect_appendix_violation \
  neg-appendix-source source_sha256_mismatch source_manifest

log "phase 3c: semantically invisible checked-in projection-byte drift"
stage_appendix neg-appendix-projection
printf '\n# planted byte-only projection drift\n' \
  >> "$WORK/neg-appendix-projection/registries/logical_object_kinds.toml"
expect_appendix_violation \
  neg-appendix-projection projection_byte_diff logical_object_kinds.toml
"$BIN" appendix --root "$WORK/neg-appendix-projection" \
  >"$WORK/neg-appendix-projection-repeat.jsonl" \
  2>"$WORK/neg-appendix-projection-repeat.err" || status=$?
[ "${status:-0}" -eq 1 ] \
  || die "repeat projection fixture did not exit with status 1"
if cmp -s "$WORK/neg-appendix-projection.jsonl" \
    "$WORK/neg-appendix-projection-repeat.jsonl"; then
  ok "Appendix projection-diff JSONL is deterministic"
else
  die "Appendix projection-diff JSONL changed across identical runs"
fi

log "phase 3d: projection generation is a read-only, deterministic verifier"
stage_appendix neg-appendix-generate-write
printf '\n# planted generation-write sentinel\n' \
  >> "$WORK/neg-appendix-generate-write/registries/logical_object_kinds.toml"
sha256sum \
  "$WORK/neg-appendix-generate-write/registries/logical_object_kinds.toml" \
  "$WORK/neg-appendix-generate-write/registries/physical_record_kinds.toml" \
  "$WORK/neg-appendix-generate-write/registries/bootstrap_frames.toml" \
  "$WORK/neg-appendix-generate-write/registries/prebootstrap_artifact_kinds.toml" \
  "$WORK/neg-appendix-generate-write/registries/wire_types.toml" \
  "$WORK/neg-appendix-generate-write/registries/durable_fields.toml" \
  > "$WORK/neg-appendix-generate-write-before.sha256"
status=0
"$BIN" appendix-generate --root "$WORK/neg-appendix-generate-write" \
  >"$WORK/neg-appendix-generate-write.jsonl" \
  2>"$WORK/neg-appendix-generate-write.err" || status=$?
[ "$status" -eq 1 ] \
  || die "drifted projection generation fixture did not exit with status 1"
sha256sum \
  "$WORK/neg-appendix-generate-write/registries/logical_object_kinds.toml" \
  "$WORK/neg-appendix-generate-write/registries/physical_record_kinds.toml" \
  "$WORK/neg-appendix-generate-write/registries/bootstrap_frames.toml" \
  "$WORK/neg-appendix-generate-write/registries/prebootstrap_artifact_kinds.toml" \
  "$WORK/neg-appendix-generate-write/registries/wire_types.toml" \
  "$WORK/neg-appendix-generate-write/registries/durable_fields.toml" \
  > "$WORK/neg-appendix-generate-write-after.sha256"
if cmp -s "$WORK/neg-appendix-generate-write-before.sha256" \
    "$WORK/neg-appendix-generate-write-after.sha256" &&
   jsonl_line_has_all "$WORK/neg-appendix-generate-write.jsonl" \
    '"event":"violation"' \
    '"code":"projection_byte_diff"' \
    '"row_id":"logical_object_kinds.toml"' &&
   jsonl_line_has_all "$WORK/neg-appendix-generate-write.jsonl" \
    '"event":"appendix_generation_completed"' \
    '"outcome":"fail"'; then
  ok "Appendix generation rejects drift without writing any projection"
else
  die "Appendix generation changed a checked-in projection"
fi

stage_appendix appendix-generate
sha256sum \
  "$WORK/appendix-generate/registries/logical_object_kinds.toml" \
  "$WORK/appendix-generate/registries/physical_record_kinds.toml" \
  "$WORK/appendix-generate/registries/bootstrap_frames.toml" \
  "$WORK/appendix-generate/registries/prebootstrap_artifact_kinds.toml" \
  "$WORK/appendix-generate/registries/wire_types.toml" \
  "$WORK/appendix-generate/registries/durable_fields.toml" \
  > "$WORK/appendix-generate-before.sha256"
if "$BIN" appendix-generate --root "$WORK/appendix-generate" \
    >"$WORK/appendix-generate-first.jsonl" \
    2>"$WORK/appendix-generate-first.err" &&
   "$BIN" appendix-generate --root "$WORK/appendix-generate" \
    >"$WORK/appendix-generate-second.jsonl" \
    2>"$WORK/appendix-generate-second.err"; then
  ok "Appendix projections render and verify successfully twice"
else
  die "Appendix projection verification failed"
fi
sha256sum \
  "$WORK/appendix-generate/registries/logical_object_kinds.toml" \
  "$WORK/appendix-generate/registries/physical_record_kinds.toml" \
  "$WORK/appendix-generate/registries/bootstrap_frames.toml" \
  "$WORK/appendix-generate/registries/prebootstrap_artifact_kinds.toml" \
  "$WORK/appendix-generate/registries/wire_types.toml" \
  "$WORK/appendix-generate/registries/durable_fields.toml" \
  > "$WORK/appendix-generate-after.sha256"
if cmp -s "$WORK/appendix-generate-first.jsonl" \
    "$WORK/appendix-generate-second.jsonl" &&
   cmp -s "$WORK/appendix-generate-before.sha256" \
    "$WORK/appendix-generate-after.sha256"; then
  ok "Appendix projection verification is deterministic and byte-preserving"
else
  die "Appendix projection verification changed JSONL or checked-in bytes"
fi

log "phase 3d-regenerate: sanctioned projection writer is scoped and idempotent"
stage_appendix appendix-regenerate
APPENDIX_REGENERATE_SENTINEL='APPENDIX_REGENERATE_SECRET_8b5ad169'
printf '\n# %s\n' "$APPENDIX_REGENERATE_SENTINEL" \
  >> "$WORK/appendix-regenerate/registries/logical_object_kinds.toml"
snapshot_nonprojection_tree "$WORK/appendix-regenerate" \
  > "$WORK/appendix-regenerate-nonprojection-before.sha256"
if "$BIN" appendix-regenerate --root "$WORK/appendix-regenerate" \
    >"$WORK/appendix-regenerate-first.jsonl" \
    2>"$WORK/appendix-regenerate-first.err"; then
  ok "Appendix regeneration restores a drifted staged projection"
else
  die "Appendix regeneration failed to restore a drifted staged projection"
fi
APPENDIX_REGENERATE_CHANGED=$(awk '
  index($0, "\"event\":\"appendix_projection_regenerated\"") &&
  index($0, "\"changed\":true") &&
  index($0, "\"outcome\":\"pass\"") { count++ }
  END { print count + 0 }
' "$WORK/appendix-regenerate-first.jsonl")
if [ "$APPENDIX_REGENERATE_CHANGED" -eq 1 ] &&
   jsonl_line_has_all "$WORK/appendix-regenerate-first.jsonl" \
    '"event":"appendix_regeneration_completed"' \
    '"projection_files":6' \
    '"changed_files":1' \
    '"unchanged_files":5' \
    '"published_files":1' \
    '"violations":0' \
    '"outcome":"pass"'; then
  ok "Appendix regeneration reports the exact changed-file set"
else
  die "Appendix regeneration emitted incomplete changed-file evidence"
fi
if grep -Fq "$APPENDIX_REGENERATE_SENTINEL" \
    "$WORK/appendix-regenerate-first.jsonl" \
    "$WORK/appendix-regenerate-first.err"; then
  die "Appendix regeneration leaked drifted projection contents"
else
  ok "Appendix regeneration diagnostics redact drifted projection contents"
fi
for projection in logical_object_kinds.toml physical_record_kinds.toml \
                  bootstrap_frames.toml prebootstrap_artifact_kinds.toml \
                  wire_types.toml durable_fields.toml; do
  cmp -s "$ROOT/registries/$projection" \
    "$WORK/appendix-regenerate/registries/$projection" \
    || die "Appendix regeneration did not restore $projection exactly"
done
sha256sum \
  "$WORK/appendix-regenerate/registries/logical_object_kinds.toml" \
  "$WORK/appendix-regenerate/registries/physical_record_kinds.toml" \
  "$WORK/appendix-regenerate/registries/bootstrap_frames.toml" \
  "$WORK/appendix-regenerate/registries/prebootstrap_artifact_kinds.toml" \
  "$WORK/appendix-regenerate/registries/wire_types.toml" \
  "$WORK/appendix-regenerate/registries/durable_fields.toml" \
  > "$WORK/appendix-regenerate-after-first.sha256"
if "$BIN" appendix-regenerate --root "$WORK/appendix-regenerate" \
    >"$WORK/appendix-regenerate-second.jsonl" \
    2>"$WORK/appendix-regenerate-second.err"; then
  ok "second Appendix regeneration succeeds as a no-op"
else
  die "second Appendix regeneration failed"
fi
sha256sum \
  "$WORK/appendix-regenerate/registries/logical_object_kinds.toml" \
  "$WORK/appendix-regenerate/registries/physical_record_kinds.toml" \
  "$WORK/appendix-regenerate/registries/bootstrap_frames.toml" \
  "$WORK/appendix-regenerate/registries/prebootstrap_artifact_kinds.toml" \
  "$WORK/appendix-regenerate/registries/wire_types.toml" \
  "$WORK/appendix-regenerate/registries/durable_fields.toml" \
  > "$WORK/appendix-regenerate-after-second.sha256"
APPENDIX_REGENERATE_UNCHANGED=$(awk '
  index($0, "\"event\":\"appendix_projection_regenerated\"") &&
  index($0, "\"changed\":false") &&
  index($0, "\"outcome\":\"pass\"") { count++ }
  END { print count + 0 }
' "$WORK/appendix-regenerate-second.jsonl")
if [ "$APPENDIX_REGENERATE_UNCHANGED" -eq 6 ] &&
   cmp -s "$WORK/appendix-regenerate-after-first.sha256" \
    "$WORK/appendix-regenerate-after-second.sha256" &&
   jsonl_line_has_all "$WORK/appendix-regenerate-second.jsonl" \
    '"event":"appendix_regeneration_completed"' \
    '"projection_files":6' \
    '"changed_files":0' \
    '"unchanged_files":6' \
    '"published_files":0' \
    '"violations":0' \
    '"outcome":"pass"'; then
  ok "second Appendix regeneration is byte-identical and reports a six-file no-op"
else
  die "second Appendix regeneration changed bytes or omitted no-op evidence"
fi
if "$BIN" appendix-regenerate --root "$WORK/appendix-regenerate" \
    >"$WORK/appendix-regenerate-third.jsonl" \
    2>"$WORK/appendix-regenerate-third.err" &&
   cmp -s "$WORK/appendix-regenerate-second.jsonl" \
    "$WORK/appendix-regenerate-third.jsonl"; then
  ok "Appendix regeneration no-op JSONL is deterministic"
else
  die "Appendix regeneration no-op JSONL drifted"
fi
snapshot_nonprojection_tree "$WORK/appendix-regenerate" \
  > "$WORK/appendix-regenerate-nonprojection-after.sha256"
if cmp -s "$WORK/appendix-regenerate-nonprojection-before.sha256" \
    "$WORK/appendix-regenerate-nonprojection-after.sha256"; then
  ok "Appendix regeneration changes no files outside the six projections"
else
  die "Appendix regeneration changed a file outside the six projections"
fi

stage_appendix neg-appendix-regenerate-load
printf '\nbroken = {}\n' \
  >> "$WORK/neg-appendix-regenerate-load/registries/appendix_a_catalog.toml"
status=0
"$BIN" appendix-regenerate --root "$WORK/neg-appendix-regenerate-load" \
  >"$WORK/neg-appendix-regenerate-load.jsonl" \
  2>"$WORK/neg-appendix-regenerate-load.err" || status=$?
if [ "$status" -eq 2 ] &&
   assert_regeneration_error_terminal \
    "$WORK/neg-appendix-regenerate-load.jsonl" 0 0 0 0; then
  ok "Appendix regeneration keeps its completion schema on early load failure"
else
  die "Appendix regeneration early failure emitted an unstable completion schema"
fi

log "phase 3d-regenerate-safety: unsafe projection destinations fail closed"
stage_appendix_except \
  neg-appendix-regenerate-symlink logical_object_kinds.toml
APPENDIX_SYMLINK_SENTINEL='APPENDIX_SYMLINK_TARGET_76e13f0b'
printf '%s\n' "$APPENDIX_SYMLINK_SENTINEL" \
  > "$WORK/appendix-regenerate-symlink-external.toml"
sha256sum "$WORK/appendix-regenerate-symlink-external.toml" \
  > "$WORK/appendix-regenerate-symlink-before.sha256"
ln -s "$WORK/appendix-regenerate-symlink-external.toml" \
  "$WORK/neg-appendix-regenerate-symlink/registries/logical_object_kinds.toml"
status=0
"$BIN" appendix-regenerate --root "$WORK/neg-appendix-regenerate-symlink" \
  >"$WORK/neg-appendix-regenerate-symlink.jsonl" \
  2>"$WORK/neg-appendix-regenerate-symlink.err" || status=$?
sha256sum "$WORK/appendix-regenerate-symlink-external.toml" \
  > "$WORK/appendix-regenerate-symlink-after.sha256"
if [ "$status" -eq 2 ] &&
   cmp -s "$WORK/appendix-regenerate-symlink-before.sha256" \
    "$WORK/appendix-regenerate-symlink-after.sha256" &&
   assert_regeneration_error_terminal \
    "$WORK/neg-appendix-regenerate-symlink.jsonl" 6 0 0 0 &&
   ! grep -Fq "$APPENDIX_SYMLINK_SENTINEL" \
    "$WORK/neg-appendix-regenerate-symlink.jsonl" \
    "$WORK/neg-appendix-regenerate-symlink.err"; then
  ok "Appendix regeneration rejects a projection symlink without touching its target"
else
  die "Appendix regeneration followed or leaked a projection symlink"
fi

stage_appendix_except \
  neg-appendix-regenerate-hardlink logical_object_kinds.toml
APPENDIX_HARDLINK_SENTINEL='APPENDIX_HARDLINK_TARGET_c4c5b322'
printf '%s\n' "$APPENDIX_HARDLINK_SENTINEL" \
  > "$WORK/appendix-regenerate-hardlink-external.toml"
sha256sum "$WORK/appendix-regenerate-hardlink-external.toml" \
  > "$WORK/appendix-regenerate-hardlink-before.sha256"
ln "$WORK/appendix-regenerate-hardlink-external.toml" \
  "$WORK/neg-appendix-regenerate-hardlink/registries/logical_object_kinds.toml"
status=0
"$BIN" appendix-regenerate --root "$WORK/neg-appendix-regenerate-hardlink" \
  >"$WORK/neg-appendix-regenerate-hardlink.jsonl" \
  2>"$WORK/neg-appendix-regenerate-hardlink.err" || status=$?
sha256sum "$WORK/appendix-regenerate-hardlink-external.toml" \
  > "$WORK/appendix-regenerate-hardlink-after.sha256"
if [ "$status" -eq 2 ] &&
   cmp -s "$WORK/appendix-regenerate-hardlink-before.sha256" \
    "$WORK/appendix-regenerate-hardlink-after.sha256" &&
   assert_regeneration_error_terminal \
    "$WORK/neg-appendix-regenerate-hardlink.jsonl" 6 0 0 0 &&
   ! grep -Fq "$APPENDIX_HARDLINK_SENTINEL" \
    "$WORK/neg-appendix-regenerate-hardlink.jsonl" \
    "$WORK/neg-appendix-regenerate-hardlink.err"; then
  ok "Appendix regeneration rejects a hard-linked projection without touching its peer"
else
  die "Appendix regeneration followed or leaked a projection hard link"
fi

stage_appendix_except \
  neg-appendix-regenerate-directory logical_object_kinds.toml
mkdir -p \
  "$WORK/neg-appendix-regenerate-directory/registries/logical_object_kinds.toml"
status=0
"$BIN" appendix-regenerate --root "$WORK/neg-appendix-regenerate-directory" \
  >"$WORK/neg-appendix-regenerate-directory.jsonl" \
  2>"$WORK/neg-appendix-regenerate-directory.err" || status=$?
if [ "$status" -eq 2 ] &&
   [ -d "$WORK/neg-appendix-regenerate-directory/registries/logical_object_kinds.toml" ] &&
   assert_regeneration_error_terminal \
    "$WORK/neg-appendix-regenerate-directory.jsonl" 6 0 0 0; then
  ok "Appendix regeneration rejects a directory projection destination"
else
  die "Appendix regeneration accepted or replaced a directory projection destination"
fi
if find "$WORK/neg-appendix-regenerate-symlink/registries" \
        "$WORK/neg-appendix-regenerate-hardlink/registries" \
        "$WORK/neg-appendix-regenerate-directory/registries" \
        -name '.appendix-regenerate-*.prepared' -print -quit | grep -q .; then
  die "unsafe Appendix destinations created prepared projection siblings"
else
  ok "unsafe Appendix destinations fail before any projection is prepared"
fi

log "phase 3e: every checked-in projection requires one target"
stage_appendix neg-appendix-target
awk '
  !removed && $0 == "[[target]]" { removed = 1; skipping = 1; next }
  skipping && /^\[\[/ { skipping = 0 }
  !skipping { print }
  END { if (!removed) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-target/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-target catalog_projection_target_missing \
  catalog_row

log "phase 3f: catalog-maintenance owners cannot masquerade as semantic owners"
stage_appendix neg-appendix-semantic-owner
cat >> "$WORK/neg-appendix-semantic-owner/registries/appendix_a_catalog.toml" <<'EOF'

[[semantic_binding]]
row_id = "a01:semantic-binding:bootstrap-frame-root-slot"
target_row_id = "a01:bootstrap-frame:root-slot"
owner_bead_id = "fgdb-appendix-a-catalog-scaffold-gvvf"
owner_crate = "registry-check"
owner_status = "planned"
consumer_crates = ["fgdb"]
EOF
expect_appendix_violation \
  neg-appendix-semantic-owner catalog_semantic_owner_invalid \
  catalog_row

log "phase 3g: row IDs are derived from typed projection identity"
stage_appendix neg-appendix-row-id
awk '
  !changed && $0 == "row_id = \"a03:logical-kind:logical-state-payload\"" {
    print "row_id = \"a03:logical-kind:logical-state-payload-wrong\""
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-row-id/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-row-id catalog_row_id_derived_mismatch \
  catalog_row

log "phase 3h: G0 projection ownership cannot be broadened"
stage_appendix neg-appendix-g0-owner
awk '
  !changed && $0 == "slice_id = \"a03\"" {
    print "slice_id = \"g0\""
    relabel = 1
    changed = 1
    next
  }
  relabel && $0 == "row_id = \"a03:logical-kind:logical-state-payload\"" {
    print "row_id = \"g0:logical-kind:logical-state-payload\""
    relabel = 0
    next
  }
  { print }
  END { if (!changed || relabel) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-g0-owner/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-g0-owner g0_projection_allowlist_drift g0

log "phase 3i: a declared slice cannot become vacuously complete"
stage_appendix neg-appendix-complete
awk '
  $0 == "id = \"a02\"" { in_slice = 1 }
  in_slice && !changed && $0 == "definition_status = \"declared\"" {
    print "definition_status = \"complete\""
    changed = 1
    in_slice = 0
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-complete/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-complete slice_census_pin_mismatch a02

log "phase 3j: full-plan reference occurrence drift fails closed"
stage_appendix neg-appendix-reference-source
awk '
  NR < 1388 && !changed && index($0, "StrongRef<") {
    sub(/StrongRef</, "StrongRefX<")
    changed = 1
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md" \
  > "$WORK/neg-appendix-reference-source/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"
expect_appendix_violation \
  neg-appendix-reference-source reference_source_manifest_mismatch \
  reference_manifest

log "phase 3j-target: exact target/source assignments cannot be downgraded"
stage_appendix neg-appendix-target-assignment
awk '
  !changed && $0 == "source_key = \"field|RootSlot|RootSlot.cluster_incarnation|cluster_incarnation\"" {
    print "source_key = \"projection|durable_fields|RootSlot.cluster_incarnation\""
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-target-assignment/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-target-assignment catalog_target_source_assignment_drift \
  target_manifest

log "phase 3j-owner: reservation ownership is derived from source"
stage_appendix neg-appendix-source-owner
awk '
  $0 == "row_id = \"plan:reservation:valid-time-contract\"" {
    print "row_id = \"a21:reservation:valid-time-contract\""
    reservation = 1
    changed++
    next
  }
  reservation && $0 == "slice_id = \"plan\"" {
    print "slice_id = \"a21\""
    reservation = 0
    changed++
    next
  }
  $0 == "row_id = \"plan:source-symbol-disposition:valid-time-contract\"" {
    print "row_id = \"a21:source-symbol-disposition:valid-time-contract\""
    disposition = 1
    changed++
    next
  }
  disposition && $0 == "slice_id = \"plan\"" {
    print "slice_id = \"a21\""
    disposition = 0
    changed++
    next
  }
  { print }
  END { if (changed != 4 || reservation || disposition) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-source-owner/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-source-owner reference_source_reservation_owner_mismatch \
  catalog_row

log "phase 3j-bindings: fabricated repository metadata cannot self-assert"
stage_appendix neg-appendix-repository-bindings
cat >> "$WORK/neg-appendix-repository-bindings/registries/appendix_a_catalog.toml" <<'EOF'

[[semantic_binding]]
row_id = "a01:semantic-binding:bootstrap-frame-root-slot"
target_row_id = "a01:bootstrap-frame:root-slot"
owner_bead_id = "fgdb-nonexistent-owner-z999"
owner_crate = "fgdb-nonexistent-owner-crate"
owner_status = "planned"
consumer_crates = ["fgdb-nonexistent-consumer-crate"]

[[evidence]]
row_id = "a01:evidence:bootstrap-frame-root-slot-static-contract"
target_row_id = "a01:bootstrap-frame:root-slot"
evidence_id = "static-contract"
phase = "static"
status = "live"
owner_bead_id = "fgdb-nonexistent-evidence-z999"
checker_ids = ["nonexistent_checker"]
scenario_ids = ["nonexistent_scenario"]
event_ids = ["nonexistent_event"]
gate_ids = ["G0"]
EOF
expect_appendix_violation \
  neg-appendix-repository-bindings catalog_semantic_owner_bead_unresolved \
  catalog_row
for code in \
  catalog_semantic_owner_crate_unresolved \
  catalog_semantic_consumer_crate_unresolved \
  catalog_evidence_owner_bead_unresolved \
  catalog_evidence_checker_unresolved \
  catalog_evidence_scenario_unresolved \
  catalog_evidence_event_unresolved \
  catalog_evidence_gate_unresolved; do
  if grep -q "\"code\":\"$code\"" \
      "$WORK/neg-appendix-repository-bindings.jsonl"; then
    ok "fabricated metadata rejected with $code"
  else
    die "fabricated metadata omitted $code"
  fi
done

log "phase 3j-binding-pins: real but unrelated repository metadata cannot self-authorize"
stage_appendix neg-appendix-unrelated-bindings
cat >> "$WORK/neg-appendix-unrelated-bindings/registries/appendix_a_catalog.toml" <<'EOF'

[[semantic_binding]]
row_id = "a01:semantic-binding:bootstrap-frame-root-slot"
target_row_id = "a01:bootstrap-frame:root-slot"
owner_bead_id = "fgdb-durable-capability-validation-evidence-dqym"
owner_crate = "fgdb-types"
owner_status = "live"
consumer_crates = ["fgdb", "fgdb-server"]

[[evidence]]
row_id = "a01:evidence:bootstrap-frame-root-slot-static-contract"
target_row_id = "a01:bootstrap-frame:root-slot"
evidence_id = "static-contract"
phase = "static"
status = "live"
owner_bead_id = "fgdb-durable-capability-validation-evidence-dqym"
checker_ids = ["appendix_a_catalog_closure"]
scenario_ids = ["g0_identity_e2e"]
event_ids = ["appendix_closure_checked"]
gate_ids = ["G0"]
EOF
expect_appendix_violation \
  neg-appendix-unrelated-bindings catalog_semantic_binding_contract_drift \
  semantic_binding
if grep -q '"code":"catalog_evidence_binding_contract_drift"' \
    "$WORK/neg-appendix-unrelated-bindings.jsonl" &&
   grep -q '"code":"catalog_semantic_binding_contract_unapproved"' \
    "$WORK/neg-appendix-unrelated-bindings.jsonl" &&
   grep -q '"code":"catalog_evidence_binding_contract_unapproved"' \
    "$WORK/neg-appendix-unrelated-bindings.jsonl"; then
  ok "real but unrelated metadata rejected by readable reciprocal pins"
else
  die "real but unrelated metadata bypassed readable reciprocal pins"
fi

log "phase 3j-annotation: placeholder annotations cannot self-assert"
stage_appendix neg-appendix-annotation-placeholder
cat >> "$WORK/neg-appendix-annotation-placeholder/registries/appendix_a_catalog.toml" <<'EOF'

[[annotation]]
row_id = "a01:annotation:bootstrap-frame-root-slot"
target_row_id = "a01:bootstrap-frame:root-slot"
exact_type = "StrongRef<T>"
cardinality = "one"
layout = "fixed"
role = "Role"
posture = "bootstrap"
authority = "root"
locality = "local"
generic_expansions = ["RootSlot"]
role_expansions = ["Local"]
reference_semantics = "strong"
target_schema_ids = ["NonexistentSchema"]
construction_order = "root-first"
retention_and_cut_rule = "TODO: define later"
digest_recipe = "slot-checksum"
redaction_class = "public-commitment"
resource_bounds = "fixed-4096-bytes"
compatibility = "v1"
EOF
expect_appendix_violation \
  neg-appendix-annotation-placeholder catalog_annotation_placeholder \
  catalog_row
if grep -q '"code":"catalog_annotation_target_schema_unresolved"' \
    "$WORK/neg-appendix-annotation-placeholder.jsonl" &&
   grep -q '"code":"catalog_annotation_reference_invalid"' \
    "$WORK/neg-appendix-annotation-placeholder.jsonl"; then
  ok "placeholder annotation also rejects unknown schema and non-concrete StrongRef"
else
  die "placeholder annotation omitted schema/reference diagnostics"
fi

log "phase 3j-annotation-reference: malformed and unregistered reference shapes fail closed"
stage_appendix neg-appendix-annotation-reference
cat >> "$WORK/neg-appendix-annotation-reference/registries/appendix_a_catalog.toml" <<'EOF'

[[annotation]]
row_id = "a01:annotation:bootstrap-frame-root-slot"
target_row_id = "a01:bootstrap-frame:root-slot"
exact_type = "StrongRef<RootManifest,Anything>"
cardinality = "one"
layout = "fixed"
role = "Local"
posture = "bootstrap"
authority = "root"
locality = "local"
generic_expansions = ["RootManifest"]
role_expansions = []
reference_semantics = "strong"
target_schema_ids = ["a05:reservation:root-manifest"]
construction_order = "root-first"
retention_and_cut_rule = "fixed-location"
digest_recipe = "slot-checksum"
redaction_class = "public-commitment"
resource_bounds = "fixed-4096-bytes"
compatibility = "v1"
EOF
expect_appendix_violation \
  neg-appendix-annotation-reference catalog_annotation_reference_invalid \
  catalog_row

log "phase 3k: maintenance proof ownership and evidence are release-pinned"
stage_appendix neg-appendix-maintenance
awk '
  !changed && $0 == "owner_crate = \"registry-check\"" {
    print "owner_crate = \"fgdb-warden\""
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-maintenance/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-maintenance catalog_maintenance_proof_mismatch \
  catalog_row

log "phase 3l: unknown catalog keys are structural load failures"
stage_appendix neg-appendix-unknown-key
awk '
  !changed && $0 == "schema_version = 4" {
    print
    print "unknown_catalog_root = true"
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-unknown-key/registries/appendix_a_catalog.toml"
expect_appendix_structural_error \
  neg-appendix-unknown-key catalog_unknown_key catalog

log "phase 3m: malformed projection schemas are structural load failures"
stage_appendix neg-appendix-projection-schema
awk '
  !changed && $0 == "[[logical_kind]]" {
    print
    print "unknown_projection_key = true"
    changed = 1
    next
  }
  { print }
  END { if (!changed) exit 42 }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-projection-schema/registries/appendix_a_catalog.toml"
expect_appendix_structural_error \
  neg-appendix-projection-schema catalog_projection_schema logical_object_kinds

# --- Verdict -----------------------------------------------------------------
log "evidence: $WORK/{appendix-baseline,identity-baseline,neg-future,neg-placement,neg-experimental,neg-recipe,neg-schema-version,neg-unknown-top-level,neg-unknown-row,neg-registry-epoch,neg-released-reuse,neg-missing-union-arm,neg-extra-union-arm,neg-reference-union-name-collision,neg-union-role,neg-appendix-bead,neg-appendix-redaction,neg-appendix-source,neg-appendix-projection,neg-appendix-target,neg-appendix-semantic-owner,neg-appendix-row-id,neg-appendix-g0-owner,neg-appendix-complete,neg-appendix-reference-source,neg-appendix-target-assignment,neg-appendix-source-owner,neg-appendix-repository-bindings,neg-appendix-unrelated-bindings,neg-appendix-annotation-placeholder,neg-appendix-annotation-reference,neg-appendix-maintenance,neg-appendix-unknown-key,neg-appendix-projection-schema,neg-appendix-generate-write,appendix-generate-first,appendix-generate-second,appendix-regenerate-first,appendix-regenerate-second,appendix-regenerate-third}.jsonl"
log "result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
log "G0 identity e2e: ALL GREEN"
