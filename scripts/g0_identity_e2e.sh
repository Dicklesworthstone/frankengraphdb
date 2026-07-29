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

# Read one scalar from the catalog's [target_manifest] block. Block-scoped so a
# same-named key in another table -- e.g. the reservation partition's own
# target_count -- can never be picked up by accident.
catalog_manifest_value() {
  awk -v key="$1" '
    /^\[target_manifest\]/ { inblock = 1; next }
    /^\[/ { inblock = 0 }
    inblock && $1 == key { gsub(/"/, "", $3); print $3; found = 1; exit }
    END {
      if (!found) {
        print "target_manifest key " key " not found in the catalog" > "/dev/stderr"
        exit 42
      }
    }
  ' "$ROOT/registries/appendix_a_catalog.toml"
}

# Recount one closure-census number straight from the catalog rows.
#
# WHY A SECOND READER RATHER THAN A NUMBER TYPED HERE. Every count this returns
# is the size of a catalog that grows on nearly every Appendix A commit, so a
# number typed here is stale by construction and the gate then measures how
# recently somebody swept it rather than anything about the tree. MEASURED
# 2026-07-26: e7b6f09 (a20, fgdb-5cgb) flipped three a20 restore certificates
# reserved->existing and moved the reservation split 431/382 -> 434/379, and
# this script went red on a clean tree for every pane from that commit until
# fgdb-su5y; the three symbol/candidate counts survived that but were staled two
# hours later by ac572bc, which re-attributed four heading-led bodies.
#
# WHY THE READER AND NOT THE COMPILED PIN. The reservation partition is also
# pinned in tools/registry-check/src/appendix_a.rs, and restating that pin here
# looks like the stronger move. It is not: MEASURED 2026-07-26, mutating
# EXPECTED_EXISTING_TYPE_RESERVATION_COUNT 434 -> 433 with the rows untouched
# reds phase 0 ("canonical Appendix A validation failed") on the checker's own
# catalog_reservation_epoch_drift, before any assertion in this file runs. A
# shell-side pin-versus-rows comparison can therefore only ever pass, and an
# assertion that cannot fire is not a gate. What this file can still catch is
# the emitter disagreeing with the rows it claims to be counting -- so the
# expectation is derived by a reader that is structurally independent of the
# checker's TOML parser (line-oriented block counting here, a full parse there)
# and compared against the emitted event.
catalog_closure_census() {
  awk -v key="$1" '
    /^\[\[reservation\]\]/               { blk = "res"; next }
    /^\[\[source_symbol_disposition\]\]/ { blk = "ssd"; ssd++; slice = ""; next }
    /^\[\[top_level_candidate\]\]/       { blk = "tlc"; tlc++; next }
    /^\[\[/                              { blk = "other"; next }
    blk == "res" && $1 == "disposition" { gsub(/"/, "", $3); res[$3]++ }
    blk == "ssd" && $1 == "slice_id"    { gsub(/"/, "", $3); slice = $3 }
    blk == "ssd" && $1 == "disposition" { gsub(/"/, "", $3); ssd_disposition[$3]++ }
    blk == "ssd" && $1 == "source_locations" {
      entries = $0
      sub(/^[^=]*=[[:space:]]*\[/, "", entries)
      sub(/\].*$/, "", entries)
      gsub(/[[:space:]]/, "", entries)
      if (entries != "" && slice != "g0") {
        pairs += split(entries, parts, /","/)
      }
    }
    END {
      value["reservations"]                = res["existing"] + res["reserved"] + 0
      value["existing_reservations"]       = res["existing"] + 0
      value["reserved_reservations"]       = res["reserved"] + 0
      value["source_dispositions"]         = ssd + 0
      value["top_level_candidates"]        = tlc + 0
      value["reference_only_symbols"]      = ssd_disposition["reference-only"] + 0
      value["appendix_structural_symbols"] = \
        ssd_disposition["appendix-structural-definition"] + 0
      value["source_location_pairs"]       = pairs + 0
      if (!(key in value)) {
        print "closure census key " key " is not one this reader derives" > "/dev/stderr"
        exit 44
      }
      # A reader that matched nothing would hand its caller an empty expectation,
      # and an empty expectation is a substring of every observed value -- the
      # assertion would pass without ever comparing anything. Refuse to answer
      # instead: the tables below are non-empty in every tree that has a catalog.
      if (ssd == 0 || tlc == 0 || value["reservations"] == 0) {
        print "closure census reader parsed no catalog rows" > "/dev/stderr"
        exit 45
      }
      print value[key]
    }
  ' "$ROOT/registries/appendix_a_catalog.toml"
}
# The Appendix support fixtures below use hard links on purpose: their manifest
# proves that no negative fixture wrote through a shared input. A global TMPDIR
# can legitimately live on another filesystem (for example /dev/shm during
# root-disk pressure), where `cp -l` must fail with EXDEV. Give this one gate a
# specific override so the runner can keep its hard-link evidence on ROOT's
# filesystem without forcing every other gate's temporary data there.
WORK="${G0_IDENTITY_E2E_WORKDIR:-${G0_E2E_WORKDIR:-$(mktemp -d)}}"
BIN="$WORK/bin/registry-check"

# The shared verdict contract (fgdb-udco). Before it, this gate said
# "[g0-identity-e2e] FAIL: ..." — the token was right but the prefix pushed it
# off column 0, so `grep '^FAIL'` returned 0 on a red run. ok()/die() now
# delegate; the counters live in the library so there is one place they can
# drift from.
# shellcheck source=lib/gate_verdict.sh
. "$ROOT/scripts/lib/gate_verdict.sh"

log() { printf '[g0-identity-e2e] %s\n' "$*"; }
ok()  { gate_pass "$*"; }
# An ASSERTION failure is recorded and the run continues; it does not end the
# run. This matches g0_claims_e2e.sh:26 and g0_spine_e2e.sh:49, which have
# always been written this way -- this file was the one outlier, and its die()
# carried `exit 1` from e8ca589 onward.
#
# WHAT THE OUTLIER COST, MEASURED 2026-07-26 on a clean tree: one stale census
# constant at the phase-0 closure gate ended the run at assertion 8, so 91 of
# the file's 99 assertions never executed, AND `exit 1` inside die() jumps past
# the verdict at the bottom, so the run printed no tally at all. A reader saw
# seven PASS lines and one FAIL line, with nothing anywhere in the output
# saying that 91 checks had been skipped. Every pane that hit it concluded
# "one problem"; the gate could not tell them whether it was one or ninety-two.
#
# A STRUCTURAL failure still ends the run immediately and deliberately, with
# `exit 2` (the subject failed to build, or the artifact is not this tree's).
# Those invalidate every assertion after them, so continuing would manufacture
# failures rather than report them.
die() { gate_fail "$*"; }

# Fail-slow becomes fail-never if the run dies before the verdict prints. Under
# `set -e` any unguarded command can do that -- a derivation helper refusing to
# answer, a missing evidence file -- and the tally at the bottom is then never
# reached. This trap makes the count unconditional, and says plainly that the
# rest did not run rather than letting a truncated log read like a whole one.
VERDICT_REACHED=0
# The exit code arrives as $1: this now runs as gate_on_exit's tally hook, so
# `$?` here would be the library's last command, not the script's exit status.
report_partial_tally() {
  local rc="$1"
  [ "$VERDICT_REACHED" -eq 1 ] && return 0
  log "ABORTED before the verdict (exit $rc): $GATE_PASS passed, $GATE_FAIL failed so far; every assertion after this point did not run"
  return 0
}
gate_init "g0_identity_e2e" report_partial_tally

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

# Return the first event line for a failed exact-fragment assertion. Keeping the
# observed event in the failure makes stale script-side pins distinguishable
# from a missing checker event.
jsonl_event_or_missing() {
  local file="$1"
  local event_name="$2"
  awk -v needle="\"event\":\"$event_name\"" '
    index($0, needle) {
      print
      found = 1
      exit
    }
    END {
      if (!found) {
        print "<missing>"
      }
    }
  ' "$file"
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

# The subject is compiled from THIS tree into $WORK by
# scripts/lib/private_subject.sh, the single implementation shared with
# g0_spine_e2e.sh and g0_claims_e2e.sh. It used to be
# "${CARGO_TARGET_DIR:-$ROOT/target}/debug/registry-check" gated only on
# `[ -x "$BIN" ]` after a cargo build whose exit status nothing read — the
# shape that let a neighbouring pane's artifact decide this gate's verdict.
# The library states what that measured and what it costs. Disk price here:
# 73MB per run for the artifact, same as its two siblings.
# shellcheck source=lib/private_subject.sh
. "$ROOT/scripts/lib/private_subject.sh"

mkdir -p "$WORK/bin"
log "acquiring the subject for this tree state"
if ! SUBJECT_DIR="$(subject_acquire "$ROOT")"; then
  log "FATAL: building registry-check from this tree failed (see $SUBJECT_DIR/build.log)"
  exit 2
fi
BIN="$SUBJECT_DIR/registry-check"
subject_is_fresh "$BIN" "$ROOT" || {
  log "FATAL: $BIN is not newer than $(subject_newest_source "$ROOT") — the build did not produce this tree's artifact"
  exit 2
}
log "subject artifact: $BIN (newer than $(subject_newest_source "$ROOT"))"

# THIS SCRIPT'S OWN control over the shared predicate, counted in this script's
# own tally. Sharing the runner must not mean sharing the credit: a gate that
# passes only because some OTHER script proved the predicate has no evidence
# about its own subject. This gate is the one that pins Appendix A identity, so
# a subject it never verified is exactly how phantom pin drift gets reported.
subject_write_stale_probe "$WORK/bin/stale-probe"
if subject_is_fresh "$BIN" "$ROOT" && ! subject_is_fresh "$WORK/bin/stale-probe" "$ROOT"; then
  ok "control: the freshness rule accepts this run's artifact and rejects a backdated one"
else
  die "control: the freshness rule does not separate a fresh artifact from a stale one; this script's subject is unproven"
fi

# THIS SCRIPT'S OWN control over the shared subject RUNNER, counted in this
# script's own tally, for the same reason as the one above. The subject is no
# longer a private directory per run (fgdb-1j16: 113 abandoned directories,
# 18.01GB, two build freezes in one night), and a reuse that quietly stops
# reusing looks exactly like one that works.
if subject_residue_control "$WORK"; then
  ok "control: the subject runner reuses one directory and leaves no residue"
else
  die "control: the subject runner leaks a directory per run"
fi

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
    '"byte_count":1025645' \
    '"sha256":"74369512ac477bc7ec913b67c06612d516f495841f83737913859c1307ba5719"' \
    '"outcome":"pass"'; then
  ok "Appendix A exact source manifest is pinned"
else
  die "Appendix A source-manifest event is missing or drifted"
fi
# The reference manifest's target_count is the same reservation total the
# closure event reports -- appendix_a.rs:9887 compares it to
# EXPECTED_TYPE_RESERVATION_COUNT -- so it is derived from the rows for the same
# reason. It has held at 813 across the whole window only because no new
# StrongRef family has needed a reservation minted; the bijection law permits
# that to move, and a sixth typed copy of the census in this file would go stale
# the first time it does.
EXPECT_RESERVATION_COUNT="$(catalog_closure_census reservations)"
if jsonl_line_has_all "$WORK/appendix-baseline.jsonl" \
    '"event":"appendix_reference_manifest"' \
    '"target_count":'"$EXPECT_RESERVATION_COUNT" \
    '"target_ids_sha256":"84276b6d97342e9ec1619424ddacb5b429e98e1862e03359afc837b65bb3392e"' \
    '"occurrence_count":2454' \
    '"occurrence_transcript_sha256":"64535886e6dbb525694d6676b315397b959291e2901b9bcd456ae0e61861d4d3"' \
    '"outcome":"pass"'; then
  ok "full-plan Appendix A reference census is pinned"
else
  die "Appendix A reference-manifest event is missing or drifted"
fi
EXPECT_TARGET_COUNT="$(catalog_manifest_value target_count)"
EXPECT_FALLBACK_COUNT="$(catalog_manifest_value projection_fallback_count)"
EXPECT_TARGET_ASSIGNMENT_SHA="$(catalog_manifest_value target_source_assignment_sha256)"
EXPECT_EXISTING_RESERVATION_COUNT="$(catalog_closure_census existing_reservations)"
EXPECT_RESERVED_RESERVATION_COUNT="$(catalog_closure_census reserved_reservations)"
EXPECT_SOURCE_DISPOSITION_COUNT="$(catalog_closure_census source_dispositions)"
EXPECT_TOP_LEVEL_CANDIDATE_COUNT="$(catalog_closure_census top_level_candidates)"
EXPECT_REFERENCE_ONLY_SYMBOL_COUNT="$(catalog_closure_census reference_only_symbols)"
EXPECT_APPENDIX_STRUCTURAL_SYMBOL_COUNT="$(catalog_closure_census appendix_structural_symbols)"
EXPECT_SOURCE_LOCATION_PAIR_COUNT="$(catalog_closure_census source_location_pairs)"
if jsonl_line_has_all "$WORK/appendix-baseline.jsonl" \
    '"event":"appendix_target_manifest"' \
    '"target_count":'"$EXPECT_TARGET_COUNT" \
    '"projection_fallback_count":'"$EXPECT_FALLBACK_COUNT" \
    '"target_source_assignment_sha256":"'"$EXPECT_TARGET_ASSIGNMENT_SHA"'"' \
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
if [ "$APPENDIX_SLICE_PASSES" -eq 21 ]; then
  ok "all 21 Appendix A slices validate"
else
  die "expected 21 passing Appendix A slices, found $APPENDIX_SLICE_PASSES"
fi
APPENDIX_PROJECTION_PASSES=$(awk '
  index($0, "\"event\":\"appendix_projection_checked\"") &&
  index($0, "\"outcome\":\"pass\"") { count++ }
  END { print count + 0 }
' "$WORK/appendix-baseline.jsonl")
if [ "$APPENDIX_PROJECTION_PASSES" -eq 6 ]; then
  ok "all six generated projections byte-match"
else
  die "expected six passing Appendix A projections, found $APPENDIX_PROJECTION_PASSES"
fi
# THE RULE FOR THIS EVENT, so the next author does not have to re-derive it:
# a field whose value is the size of a growing catalog is DERIVED above, and a
# field whose correct value is a law is written here. The laws are the four
# empty completion layers, the empty outside-structural bucket, the four
# completion-layer schemas, and the 35-row G0 projection slice -- none of which
# moved across the 40 Appendix A commits before this one, while every derived
# field did. A law that moves is an event and should red this gate; a census
# that moves is Tuesday.
if jsonl_line_has_all "$WORK/appendix-baseline.jsonl" \
    '"event":"appendix_closure_checked"' \
    '"reservations":'"$EXPECT_RESERVATION_COUNT" \
    '"existing_reservations":'"$EXPECT_EXISTING_RESERVATION_COUNT" \
    '"reserved_reservations":'"$EXPECT_RESERVED_RESERVATION_COUNT" \
    '"source_dispositions":'"$EXPECT_SOURCE_DISPOSITION_COUNT" \
    '"top_level_candidates":'"$EXPECT_TOP_LEVEL_CANDIDATE_COUNT" \
    '"targets":'"$EXPECT_TARGET_COUNT" \
    '"completion_layer_schemas":4' \
    '"annotations":0' \
    '"semantic_bindings":0' \
    '"expansion_bindings":0' \
    '"evidence_rows":0' \
    '"reference_only_symbols":'"$EXPECT_REFERENCE_ONLY_SYMBOL_COUNT" \
    '"appendix_structural_symbols":'"$EXPECT_APPENDIX_STRUCTURAL_SYMBOL_COUNT" \
    '"outside_structural_symbols":0' \
    '"source_location_pairs":'"$EXPECT_SOURCE_LOCATION_PAIR_COUNT" \
    '"g0_projection_dispositions":35' \
    '"outcome":"pass"'; then
  ok "Appendix A source/target/owner/evidence scaffold closure is exact"
else
  OBSERVED_APPENDIX_CLOSURE="$(
    jsonl_event_or_missing "$WORK/appendix-baseline.jsonl" appendix_closure_checked
  )"
  die "Appendix A closure event is missing or drifted; expected reservations=$EXPECT_RESERVATION_COUNT existing=$EXPECT_EXISTING_RESERVATION_COUNT reserved=$EXPECT_RESERVED_RESERVATION_COUNT targets=$EXPECT_TARGET_COUNT; observed $OBSERVED_APPENDIX_CLOSURE"
fi
# THE UNCLASSIFIED RESIDUE, AS A CEILING THAT CAN ONLY CLOSE.
#
# A top-level source candidate whose class the source does not force sits at
# `identity_class = "unclassified"`. appendix_a.rs classifies candidates four
# ways and the fourth arm -- unprojected AND unclassified -- is empty: no
# violation, no count, no pin. Nothing anywhere in the tree stated how much
# residue there was. MEASURED 2026-07-27: 542 of 1237 candidates, 44%, while
# fgdb-a18-restore-union-source-gates-a4fq's prose named seven of them.
#
# A CEILING, not a pin, and the distinction is load-bearing. Between two reads
# eleven minutes apart the number fell 543 -> 542, because dec248a classified
# one: legitimate progress by another pane, which a pin would have redded. A
# ceiling lets the residue close and fails only when it GROWS, so adding an
# unclassified candidate becomes a deliberate bump of this line rather than
# silence. Same instrument as claims_lint.toml's unmarked_rows gap ledger.
#
# The slack is printed on every green run: a ceiling that drifts far above the
# observed value is a weakened gate, and the only way to notice is to say it.
EXPECT_UNCLASSIFIED_CEILING=542
UNCLASSIFIED_OBSERVED=$(awk '
  index($0, "\"event\":\"appendix_closure_checked\"") {
    if (match($0, /"unclassified_candidates":[0-9]+/)) {
      value = substr($0, RSTART, RLENGTH)
      sub(/^"unclassified_candidates":/, "", value)
      print value
      found = 1
      exit
    }
  }
  END { if (!found) print "missing" }
' "$WORK/appendix-baseline.jsonl")
if [ "$UNCLASSIFIED_OBSERVED" = "missing" ]; then
  die "the closure event carries no unclassified_candidates field; the residue is unreported again"
elif [ "$UNCLASSIFIED_OBSERVED" -le "$EXPECT_UNCLASSIFIED_CEILING" ]; then
  ok "unclassified source-candidate residue is within its ceiling ($UNCLASSIFIED_OBSERVED of $EXPECT_UNCLASSIFIED_CEILING, slack $((EXPECT_UNCLASSIFIED_CEILING - UNCLASSIFIED_OBSERVED)))"
else
  die "unclassified source-candidate residue GREW: $UNCLASSIFIED_OBSERVED observed against a ceiling of $EXPECT_UNCLASSIFIED_CEILING. Landing a candidate the source does not classify is allowed, but it is a deliberate act: lower or raise EXPECT_UNCLASSIFIED_CEILING in the same commit and say which candidates moved."
fi
if jsonl_line_has_all "$WORK/appendix-baseline.jsonl" \
    '"event":"appendix_completed"' \
    '"slices":21' \
    '"projection_rows":'"$EXPECT_TARGET_COUNT" \
    '"projection_files":6' \
    '"reservations":'"$EXPECT_RESERVATION_COUNT" \
    '"source_dispositions":'"$EXPECT_SOURCE_DISPOSITION_COUNT" \
    '"top_level_candidates":'"$EXPECT_TOP_LEVEL_CANDIDATE_COUNT" \
    '"targets":'"$EXPECT_TARGET_COUNT" \
    '"completion_layer_schemas":4' \
    '"annotations":0' \
    '"semantic_bindings":0' \
    '"expansion_bindings":0' \
    '"evidence_rows":0' \
    '"reference_only_symbols":'"$EXPECT_REFERENCE_ONLY_SYMBOL_COUNT" \
    '"violations":0' \
    '"outcome":"pass"'; then
  ok "Appendix A catalog closure is exact"
else
  OBSERVED_APPENDIX_COMPLETION="$(
    jsonl_event_or_missing "$WORK/appendix-baseline.jsonl" appendix_completed
  )"
  die "Appendix A completion event is missing or incomplete; expected projection_rows=$EXPECT_TARGET_COUNT reservations=$EXPECT_RESERVATION_COUNT targets=$EXPECT_TARGET_COUNT; observed $OBSERVED_APPENDIX_COMPLETION"
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

# Freeze the §5.1 BodyDigest recipe identities without a second hand-written
# reader. `registry-check identity` emits one `digest_verified` event for every
# digest-bearing durable field and recomputes each BodyDigest pin. The floor is
# monotone: a newly declared valid recipe is growth, while losing any of the 14
# recipes shipped when this law was installed is a durable-format regression.
BODY_DIGEST_FLOOR=14
BODY_DIGEST_EVENTS=$(awk '
  index($0, "\"event\":\"digest_verified\"") &&
  index($0, "\"digest_class\":\"body\"") { count++ }
  END { print count + 0 }
' "$WORK/identity-baseline.jsonl")
BODY_DIGEST_PASSES=$(awk '
  index($0, "\"event\":\"digest_verified\"") &&
  index($0, "\"digest_class\":\"body\"") &&
  index($0, "\"outcome\":\"pass\"") { count++ }
  END { print count + 0 }
' "$WORK/identity-baseline.jsonl")
if [ "$BODY_DIGEST_EVENTS" -lt "$BODY_DIGEST_FLOOR" ]; then
  die "BodyDigest recipe population regressed: found $BODY_DIGEST_EVENTS, floor $BODY_DIGEST_FLOOR"
elif [ "$BODY_DIGEST_PASSES" -eq "$BODY_DIGEST_EVENTS" ]; then
  ok "BodyDigest event closure is complete ($BODY_DIGEST_PASSES of $BODY_DIGEST_EVENTS recipes; floor $BODY_DIGEST_FLOOR)"
else
  die "BodyDigest event closure is incomplete: $BODY_DIGEST_PASSES of $BODY_DIGEST_EVENTS recipes pass"
fi

# --- Phase 2: negative fixtures ----------------------------------------------
registry_is_private() { # registry_is_private <basename> [private-basename...]
  local basename="$1"
  shift
  local private
  for private in "$@"; do
    [ "$basename" = "$private" ] && return 0
  done
  return 1
}

validate_private_registries() { # validate_private_registries [basename...]
  local private
  for private in "$@"; do
    [ -f "$ROOT/registries/$private" ] \
      || die "fixture names unknown private registry $private"
  done
}

stage() { # stage <name> [private-basename...] -> stages registries
  local name="$1"
  shift
  local source basename
  validate_private_registries "$@"
  mkdir -p "$WORK/$name/registries"
  for source in "$ROOT"/registries/*.toml; do
    basename="${source##*/}"
    if registry_is_private "$basename" "$@"; then
      cp "$source" "$WORK/$name/registries/"
    else
      link_shared_support "$source" "$WORK/$name/registries/"
    fi
  done
  assert_linked_manifest_complete "$WORK/$name"
}

stage_except() { # stage_except <name> <excluded> [private-basename...]
  local name="$1"
  local excluded="$2"
  shift 2
  local source basename
  [ -f "$ROOT/registries/$excluded" ] \
    || die "fixture excludes unknown registry $excluded"
  validate_private_registries "$@"
  mkdir -p "$WORK/$name/registries"
  for source in "$ROOT"/registries/*.toml; do
    basename="${source##*/}"
    if [ "$basename" = "$excluded" ]; then
      continue
    elif registry_is_private "$basename" "$@"; then
      cp "$source" "$WORK/$name/registries/"
    else
      link_shared_support "$source" "$WORK/$name/registries/"
    fi
  done
  assert_linked_manifest_complete "$WORK/$name"
}

# LINKED_MANIFEST — "<sha256>  <staged-path>", one row per hard-linked input.
#
# THE SUBJECT IS THE FIXTURE, NOT $ROOT, and that is the whole correction
# (fgdb-g0-root-guard-beads-blind-c7oe). The root before/after snapshot excludes
# './.beads' and './.beads/*' — added by fgdb-flbb so that a peer bead flush
# mid-run would not fire it — but .beads/issues.jsonl is 8,203 KB of the ~10 MB
# linkable set, so the guard was blind to roughly three quarters of the bytes
# whose safety it is cited for. Excluding .git was right; excluding .beads
# blinded the instrument to its own subject.
#
# WIDENING THE ROOT SNAPSHOT IS THE WRONG FIX. It would re-introduce the false
# positive flbb removed. The two things that touch .beads are NOT the same:
#
#   COORDINATION CHURN     a peer runs `br update` / `br sync --flush-only`
#   WRITE-THROUGH DAMAGE   a fixture writes a path it hard-links
#
# MEASURED 2026-07-27, and this is what separates them: `br` replaces the file
# rather than rewriting it. Staging a link at nlink=2 and then letting a peer
# write produced real-file digest 00b40f54 -> 4ead27df ON A NEW INODE, while the
# fixture's digest stayed 00b40f54 on the old inode at nlink=1. So coordination
# churn DETACHES the fixture; it cannot alter it. Write-through damage, by
# contrast, mutates the shared inode and therefore shows up in the FIXTURE.
# Digesting the fixture side is immune to peer churn BY CONSTRUCTION, and it
# measures the actual law — "no fixture wrote a linked inode after staging" —
# instead of a proxy for it.
LINKED_MANIFEST="$WORK/.linked-manifest"
SHARED_INPUT_ROOT="$WORK/.shared-inputs"

link_support() { # link_support <source-file> <destination-dir-or-file>
  local source="$1" destination="$2"
  cp -l "$source" "$destination"
  [ -d "$destination" ] && destination="${destination%/}/${source##*/}"
  sha256sum "$destination" >> "$LINKED_MANIFEST"
}

link_shared_support() { # link_shared_support <root-source> <destination>
  local source="$1" destination="$2"
  local relative anchor
  relative="${source#"$ROOT"/}"
  [ "$relative" != "$source" ] \
    || die "shared fixture input is outside the repository root: $source"
  anchor="$SHARED_INPUT_ROOT/$relative"
  if [ ! -f "$anchor" ]; then
    mkdir -p "${anchor%/*}"
    cp "$source" "$anchor"
  fi
  link_support "$anchor" "$destination"
}

stage_appendix_support() { # stage_appendix_support <name> <linked|private>
  local name="$1"
  local plan_disposition="$2"
  local manifest relative source
  mkdir -p "$WORK/$name/.beads"
  # Every hard link goes through link_support so the linked set is DERIVED
  # rather than declared — see assert_linked_manifest_complete for why a
  # declared list is not enough.
  link_support "$ROOT/.beads/issues.jsonl" "$WORK/$name/.beads/"
  case "$plan_disposition" in
    linked)
      link_shared_support \
        "$ROOT/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md" \
        "$WORK/$name/"
      ;;
    private)
      cp "$ROOT/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md" \
        "$WORK/$name/"
      ;;
    *)
      die "fixture $name has unknown plan disposition $plan_disposition"
      ;;
  esac
  link_support "$ROOT/Cargo.toml" "$WORK/$name/"
  for manifest in "$ROOT"/crates/*/Cargo.toml "$ROOT"/tools/*/Cargo.toml; do
    [ -f "$manifest" ] || continue
    relative="${manifest#"$ROOT"/}"
    mkdir -p "$WORK/$name/${relative%/*}"
    link_support "$manifest" "$WORK/$name/$relative"
  done
  mkdir -p "$WORK/$name/scripts" "$WORK/$name/tools/registry-check/src"
  link_support "$ROOT/scripts/g0_identity_e2e.sh" "$WORK/$name/scripts/"
  for source in "$ROOT"/tools/registry-check/src/*.rs; do
    link_support "$source" "$WORK/$name/tools/registry-check/src/"
  done
  assert_linked_manifest_complete "$WORK/$name"
}

# assert_linked_manifest_complete <fixture-root>
#
# THE COMPLETENESS GUARD, AND IT FAILS CLOSED. It does not trust link_support to
# have been called: it ENUMERATES every multiply-linked regular file under the
# fixture and requires each to appear in the manifest. A future `cp -l` added to
# stage_appendix_support without a record is caught here, rather than escaping
# verification in exactly the way .beads escapes the root snapshot today — that
# is this bead's own defect one layer up, so the guard has to be derived, not
# declared.
#
# Enumeration happens AT STAGING, not at exit, and deliberately: a peer write
# during the run drops the real file's link count, so `-links +1` stops being a
# reliable enumerator once the run is under way.
assert_linked_manifest_complete() { # assert_linked_manifest_complete <fixture-root>
  local fixture_root="$1" path unrecorded=0
  while IFS= read -r -d '' path; do
    if ! grep -Fqx -- "$(sha256sum "$path")" "$LINKED_MANIFEST"; then
      echo "ERROR: g0_linked_input_unrecorded: $path is hard-linked into the" >&2
      echo "  fixture but carries no manifest row, so nothing would notice if a" >&2
      echo "  fixture wrote through it. Stage it with link_support." >&2
      unrecorded=1
    fi
  done < <(find "$fixture_root" -type f -links +1 -print0)
  [ "$unrecorded" -eq 0 ] || die "g0_linked_input_unrecorded"
}

# assert_linked_inputs_unwritten
#
# THE INTEGRITY GUARD. Every recorded fixture path must still hash to what it
# hashed at staging. This is the law the root snapshot was standing in for, and
# unlike that snapshot it sees .beads/issues.jsonl — 76.7% of the linkable bytes.
assert_linked_inputs_unwritten() {
  local rechecked=0 recorded=0
  if [ ! -s "$LINKED_MANIFEST" ]; then
    die "g0_linked_manifest_empty: no linked inputs were recorded, so the \
write-through guard verified nothing"
  fi
  recorded=$(wc -l < "$LINKED_MANIFEST")
  if LC_ALL=C sha256sum -c --quiet "$LINKED_MANIFEST" >"$WORK/.linked-recheck.log" 2>&1; then
    rechecked=$recorded
    ok "linked inputs unwritten: $rechecked/$recorded staged paths byte-identical"
  else
    cat "$WORK/.linked-recheck.log" >&2
    die "g0_linked_input_written: a fixture wrote through a hard-linked input"
  fi
}

stage_appendix() { # stage_appendix <name> <plan-disposition> [private-registry...]
  local name="$1"
  local plan_disposition="$2"
  shift 2
  stage "$name" "$@"
  stage_appendix_support "$name" "$plan_disposition"
}

stage_appendix_except() { # stage_appendix_except <name> <excluded> <plan-disposition> [private-registry...]
  local name="$1"
  local excluded="$2"
  local plan_disposition="$3"
  shift 3
  stage_except "$name" "$excluded" "$@"
  stage_appendix_support "$name" "$plan_disposition"
}

snapshot_nonprojection_tree() { # snapshot_nonprojection_tree <staged-root>
  local staged_root="$1"
  (
    cd "$staged_root"
    find . \
      ! -path './.git' ! -path './.git/*' \
      ! -path './.beads' ! -path './.beads/*' \
      ! -path './registries/logical_object_kinds.toml' \
      ! -path './registries/physical_record_kinds.toml' \
      ! -path './registries/bootstrap_frames.toml' \
      ! -path './registries/prebootstrap_artifact_kinds.toml' \
      ! -path './registries/wire_types.toml' \
      ! -path './registries/durable_fields.toml' \
      -printf '%y|%m|%p|%l\n' | LC_ALL=C sort
    find . -type f \
      ! -path './.git' ! -path './.git/*' \
      ! -path './.beads' ! -path './.beads/*' \
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

ROOT_SNAPSHOT_BEFORE=$(snapshot_nonprojection_tree "$ROOT" | sha256sum)

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
stage neg-future durable_fields.toml
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
stage neg-placement durable_fields.toml
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
stage neg-experimental logical_object_kinds.toml
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
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 2d planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
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
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 2e planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
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
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 2f planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
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
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 2g planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
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
LOGICAL_EPOCH="$(awk '/^registry_epoch = /{gsub(/[^0-9]/,"",$3); print $3; exit}' \
  "$ROOT/registries/logical_object_kinds.toml")"
awk -v cur="registry_epoch = $LOGICAL_EPOCH" -v nxt="registry_epoch = $((LOGICAL_EPOCH + 1))" '
  !changed && $0 == cur {
    print nxt
    changed = 1
    next
  }
  { print }
  END {
    if (!changed) {
      print "epoch-drift fixture matched nothing: expected \"" cur "\"" > "/dev/stderr"
      exit 42
    }
  }
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
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 2i planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
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
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 2l planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/registries/durable_fields.toml" \
  > "$WORK/neg-union-role/registries/durable_fields.toml"
expect_identity_violation \
  neg-union-role union_role_mismatch durable_fields MandatoryInventoryRef

# --- Phase 3: Appendix source/catalog/projection mutation corpus ------------
log "phase 3a: wrong Appendix slice Bead binding"
stage_appendix neg-appendix-bead linked appendix_a_catalog.toml
awk '
  !changed && $0 == "bead_id = \"fgdb-a01-reference-roots-2k0q\"" {
    print "bead_id = \"fgdb-a01-wrong-owner\""
    changed = 1
    next
  }
  { print }
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 3a planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-bead/registries/appendix_a_catalog.toml"
expect_appendix_violation neg-appendix-bead catalog_pin_mismatch a01

log "phase 3a-redaction: attacker-controlled catalog values never reach diagnostics"
stage_appendix neg-appendix-redaction linked appendix_a_catalog.toml
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
stage_appendix neg-appendix-source private
awk '
  !changed && $0 == "## Appendix A — On-Disk Object Formats (normative contract)" {
    print "## Appendix X — On-Disk Object Formats (normative contract)"
    changed = 1
    next
  }
  { print }
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 3b planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md" \
  > "$WORK/neg-appendix-source/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"
expect_appendix_violation \
  neg-appendix-source source_sha256_mismatch source_manifest

log "phase 3c: semantically invisible checked-in projection-byte drift"
stage_appendix neg-appendix-projection linked logical_object_kinds.toml
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
stage_appendix neg-appendix-generate-write linked logical_object_kinds.toml
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

stage_appendix appendix-generate linked
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
stage_appendix appendix-regenerate linked \
  logical_object_kinds.toml physical_record_kinds.toml \
  bootstrap_frames.toml prebootstrap_artifact_kinds.toml \
  wire_types.toml durable_fields.toml
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

stage_appendix neg-appendix-regenerate-load linked \
  appendix_a_catalog.toml \
  logical_object_kinds.toml physical_record_kinds.toml \
  bootstrap_frames.toml prebootstrap_artifact_kinds.toml \
  wire_types.toml durable_fields.toml
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
  neg-appendix-regenerate-symlink logical_object_kinds.toml linked \
  physical_record_kinds.toml bootstrap_frames.toml \
  prebootstrap_artifact_kinds.toml wire_types.toml durable_fields.toml
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
  neg-appendix-regenerate-hardlink logical_object_kinds.toml linked \
  physical_record_kinds.toml bootstrap_frames.toml \
  prebootstrap_artifact_kinds.toml wire_types.toml durable_fields.toml
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
  neg-appendix-regenerate-directory logical_object_kinds.toml linked \
  physical_record_kinds.toml bootstrap_frames.toml \
  prebootstrap_artifact_kinds.toml wire_types.toml durable_fields.toml
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
stage_appendix neg-appendix-target linked appendix_a_catalog.toml
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
stage_appendix neg-appendix-semantic-owner linked appendix_a_catalog.toml
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
stage_appendix neg-appendix-row-id linked appendix_a_catalog.toml
awk '
  !changed && $0 == "row_id = \"a03:logical-kind:logical-state-payload\"" {
    print "row_id = \"a03:logical-kind:logical-state-payload-wrong\""
    changed = 1
    next
  }
  { print }
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 3g planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-row-id/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-row-id catalog_row_id_derived_mismatch \
  catalog_row

log "phase 3h: G0 projection ownership cannot be broadened"
stage_appendix neg-appendix-g0-owner linked appendix_a_catalog.toml
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
stage_appendix neg-appendix-complete linked appendix_a_catalog.toml
awk '
  $0 == "id = \"a02\"" { in_slice = 1 }
  in_slice && !changed && $0 == "definition_status = \"declared\"" {
    print "definition_status = \"complete\""
    changed = 1
    in_slice = 0
    next
  }
  { print }
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 3i planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-complete/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-complete slice_census_pin_mismatch a02

log "phase 3j: full-plan reference occurrence drift fails closed"
stage_appendix neg-appendix-reference-source private
awk '
  NR < 1388 && !changed && index($0, "StrongRef<") {
    sub(/StrongRef</, "StrongRefX<")
    changed = 1
  }
  { print }
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 3j planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md" \
  > "$WORK/neg-appendix-reference-source/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"
expect_appendix_violation \
  neg-appendix-reference-source reference_source_manifest_mismatch \
  reference_manifest

log "phase 3j-target: exact target/source assignments cannot be downgraded"
stage_appendix neg-appendix-target-assignment linked appendix_a_catalog.toml
awk '
  !changed && $0 == "source_key = \"field|RootSlot|RootSlot.cluster_incarnation|cluster_incarnation\"" {
    print "source_key = \"projection|durable_fields|RootSlot.cluster_incarnation\""
    changed = 1
    next
  }
  { print }
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 3j-target planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-target-assignment/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-target-assignment catalog_target_source_assignment_drift \
  target_manifest

log "phase 3j-owner: reservation ownership is derived from source"
stage_appendix neg-appendix-source-owner linked appendix_a_catalog.toml
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
stage_appendix neg-appendix-repository-bindings linked appendix_a_catalog.toml
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
stage_appendix neg-appendix-unrelated-bindings linked appendix_a_catalog.toml
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
stage_appendix neg-appendix-annotation-placeholder linked appendix_a_catalog.toml
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
stage_appendix neg-appendix-annotation-reference linked appendix_a_catalog.toml
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
stage_appendix neg-appendix-maintenance linked appendix_a_catalog.toml
awk '
  !changed && $0 == "owner_crate = \"registry-check\"" {
    print "owner_crate = \"fgdb-warden\""
    changed = 1
    next
  }
  { print }
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 3k planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-maintenance/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-maintenance catalog_maintenance_proof_mismatch \
  catalog_row

log "phase 3l: unknown catalog keys are structural load failures"
stage_appendix neg-appendix-unknown-key linked appendix_a_catalog.toml
awk '
  !changed && $0 == "schema_version = 5" {
    print
    print "unknown_catalog_root = true"
    changed = 1
    next
  }
  { print }
  END {
    if (!changed) {
      print "expected current Appendix catalog schema_version = 5" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-unknown-key/registries/appendix_a_catalog.toml"
expect_appendix_structural_error \
  neg-appendix-unknown-key catalog_unknown_key catalog

log "phase 3m: completion-layer schema contract drift is rejected"
stage_appendix neg-appendix-completion-schema linked appendix_a_catalog.toml
awk '
  !changed && $0 == "pin_policy = \"compiled-count-sha256-readable-row-contract\"" {
    print "pin_policy = \"catalog-self-authorized\""
    changed = 1
    next
  }
  { print }
  END {
    if (!changed) {
      print "expected frozen completion-layer pin_policy" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-completion-schema/registries/appendix_a_catalog.toml"
expect_appendix_violation \
  neg-appendix-completion-schema catalog_completion_layer_schema_drift \
  catalog_row

log "phase 3n: malformed projection schemas are structural load failures"
stage_appendix neg-appendix-projection-schema linked appendix_a_catalog.toml
awk '
  !changed && $0 == "[[logical_kind]]" {
    print
    print "unknown_projection_key = true"
    changed = 1
    next
  }
  { print }
  END {
    if (!changed) {
      print "FIXTURE STALE: phase 3n planted nothing - the line it rewrites no longer exists in the staged input" > "/dev/stderr"
      exit 42
    }
  }
' "$ROOT/registries/appendix_a_catalog.toml" \
  > "$WORK/neg-appendix-projection-schema/registries/appendix_a_catalog.toml"
expect_appendix_structural_error \
  neg-appendix-projection-schema catalog_projection_schema logical_object_kinds

# --- Verdict -----------------------------------------------------------------
ROOT_SNAPSHOT_AFTER=$(snapshot_nonprojection_tree "$ROOT" | sha256sum)
if [ "$ROOT_SNAPSHOT_BEFORE" != "$ROOT_SNAPSHOT_AFTER" ]; then
  die "source root changed during evidence gate (write-through guard fired)"
else
  ok "source root unchanged during evidence gate"
fi
# The root snapshot above still excludes .beads on purpose — peer coordination
# churn there is not a defect and firing on it is what fgdb-flbb fixed. The
# linked-input check is the instrument that actually covers those bytes.
assert_linked_inputs_unwritten
log "evidence: $WORK/{appendix-baseline,identity-baseline,neg-future,neg-placement,neg-experimental,neg-recipe,neg-schema-version,neg-unknown-top-level,neg-unknown-row,neg-registry-epoch,neg-released-reuse,neg-missing-union-arm,neg-extra-union-arm,neg-reference-union-name-collision,neg-union-role,neg-appendix-bead,neg-appendix-redaction,neg-appendix-source,neg-appendix-projection,neg-appendix-target,neg-appendix-semantic-owner,neg-appendix-row-id,neg-appendix-g0-owner,neg-appendix-complete,neg-appendix-reference-source,neg-appendix-target-assignment,neg-appendix-source-owner,neg-appendix-repository-bindings,neg-appendix-unrelated-bindings,neg-appendix-annotation-placeholder,neg-appendix-annotation-reference,neg-appendix-maintenance,neg-appendix-unknown-key,neg-appendix-completion-schema,neg-appendix-projection-schema,neg-appendix-generate-write,appendix-generate-first,appendix-generate-second,appendix-regenerate-first,appendix-regenerate-second,appendix-regenerate-third}.jsonl"
VERDICT_REACHED=1
gate_verdict || exit 1
log "G0 identity e2e: ALL GREEN"
