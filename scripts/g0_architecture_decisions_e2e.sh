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
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
case "$TARGET_DIR" in
  /*) ;;
  *) TARGET_DIR="$ROOT/$TARGET_DIR" ;;
esac
BIN="$TARGET_DIR/debug/architecture-check"
RELATION_REGISTRY="$EVIDENCE_DIR/contradictory-relationship.toml"
OWNER_REGISTRY="$EVIDENCE_DIR/invalid-secondary-owner.toml"
ORPHAN_REGISTRY="$EVIDENCE_DIR/orphan-family.toml"
AMBIGUOUS_REGISTRY="$EVIDENCE_DIR/ambiguous-family.toml"
RELATION_OUT="$EVIDENCE_DIR/contradictory-relationship.ndjson"
OWNER_OUT="$EVIDENCE_DIR/invalid-secondary-owner.ndjson"
ORPHAN_OUT="$EVIDENCE_DIR/orphan-family.ndjson"
AMBIGUOUS_OUT="$EVIDENCE_DIR/ambiguous-family.ndjson"

echo "==> build architecture-check"
cargo build -p registry-check --bin architecture-check
test -x "$BIN"

echo "==> validate the frozen ADR twice"
"$BIN" --root "$ROOT" >"$FIRST"
"$BIN" --root "$ROOT" >"$SECOND"
cmp "$FIRST" "$SECOND"

echo "==> assert deterministic event and provenance coverage"
test "$(rg -c '"event":"architecture_decision_checked"' "$FIRST")" -eq 256
test "$(rg -c '"event":"source_block_checked"' "$FIRST")" -eq 2
test "$(rg -c '"event":"architecture_bead_provenance_indexed"' "$FIRST")" -eq 299
rg -q '"event":"architecture_registry_checked".*"decision_count":256.*"bead_count":299.*"bead_binding_hash":"fnv1a64:7ff10744115e68f5".*"violations":0.*"outcome":"pass"' "$FIRST"
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

echo "==> structurally parse every baseline NDJSON row"
python3 - "$FIRST" <<'PY'
import collections
import json
import sys

def reject_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result

events = []
with open(sys.argv[1], encoding="utf-8") as stream:
    for line_number, line in enumerate(stream, 1):
        value = json.loads(line, object_pairs_hook=reject_duplicate_keys)
        assert isinstance(value, dict), (line_number, value)
        assert isinstance(value.get("event"), str), (line_number, value)
        events.append(value)

counts = collections.Counter(event["event"] for event in events)
assert counts["architecture_registry_checked"] == 1, counts
assert counts["architecture_decision_checked"] == 256, counts
assert counts["source_block_checked"] == 2, counts
assert counts["architecture_bead_provenance_indexed"] == 299, counts
assert counts["architecture_violation"] == 0, counts

registry = next(event for event in events if event["event"] == "architecture_registry_checked")
assert registry["decision_count"] == 256, registry
assert registry["bead_count"] == 299, registry
assert registry["bead_binding_hash"] == "fnv1a64:7ff10744115e68f5", registry
assert registry["violations"] == 0 and registry["outcome"] == "pass", registry

beads = [event for event in events if event["event"] == "architecture_bead_provenance_indexed"]
class_counts = collections.Counter(event["resolution_class"] for event in beads)
assert class_counts == {
    "direct_owner": 98,
    "bet_label": 156,
    "exact_override": 12,
    "family_rule": 33,
}, class_counts
for bead in beads:
    assert bead["bead_id"].startswith("fgdb-"), bead
    assert bead["rule_id"], bead
    for key in (
        "decision_ids",
        "profile_ids",
        "summaries",
        "rationales",
        "source_anchors",
        "replay_commands",
    ):
        assert isinstance(bead[key], list) and bead[key], (key, bead)
    assert bead["outcome"] == "pass", bead

by_bead = {event["bead_id"]: event for event in beads}
for bead_id, resolution_class in {
    "fgdb-w2-commit-protocol-9w3u": "direct_owner",
    "fgdb-tvg8": "bet_label",
    "fgdb-01q9": "exact_override",
    "fgdb-risk-register-ar1z": "family_rule",
}.items():
    assert by_bead[bead_id]["resolution_class"] == resolution_class, by_bead[bead_id]
PY

echo "==> generate contradictory CLI registry fixtures"
python3 - \
  "$ROOT/registries/architecture_decisions.toml" \
  "$RELATION_REGISTRY" "$OWNER_REGISTRY" "$ORPHAN_REGISTRY" "$AMBIGUOUS_REGISTRY" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")

def replace_in_decision(decision_id, old, new):
    marker = f'id = "{decision_id}"'
    start = source.index(marker)
    end = source.find("\n[[decision]]", start)
    section = source[start:] if end == -1 else source[start:end]
    assert section.count(old) == 1, (decision_id, old)
    changed = section.replace(old, new, 1)
    return source[:start] + changed + ("" if end == -1 else source[end:])

relation = replace_in_decision(
    "FG-ADR-BET-B1",
    'relationship_kind = "build_in_house"',
    'relationship_kind = "accidental_dependency"',
)
Path(sys.argv[2]).write_text(relation, encoding="utf-8")

owner = replace_in_decision(
    "FG-ADR-BET-B1",
    'owner_beads = ["fgdb-w2-object-identity-t0f", "fgdb-w2-commit-protocol-9w3u"]',
    'owner_beads = ["fgdb-w2-object-identity-t0f", "fgdb-w2-commit-protocol-9w3u", "fgdb-does-not-exist"]',
)
owner = owner.replace(
    'owner_crates = ["fgdb-ecs", "fgdb-order", "fgdb-chronicle", "fgdb-branch"]',
    'owner_crates = ["fgdb-ecs", "fgdb-order", "fgdb-chronicle", "fgdb-branch", "fgdb-not-planned"]',
    1,
)
Path(sys.argv[3]).write_text(owner, encoding="utf-8")

orphan_old = 'id = "risk-governance"\nmatch_kind = "prefix"\npattern = "fgdb-risk-"'
orphan_new = 'id = "risk-governance"\nmatch_kind = "prefix"\npattern = "fgdb-no-such-risk-"'
assert source.count(orphan_old) == 1
Path(sys.argv[4]).write_text(source.replace(orphan_old, orphan_new, 1), encoding="utf-8")

ambiguous_old = 'id = "workstream-w1"\nmatch_kind = "prefix"\npattern = "fgdb-w1-"'
ambiguous_new = 'id = "workstream-w1"\nmatch_kind = "prefix"\npattern = "fgdb-risk-"'
assert source.count(ambiguous_old) == 1
Path(sys.argv[5]).write_text(source.replace(ambiguous_old, ambiguous_new, 1), encoding="utf-8")
PY

run_failure() {
  local registry="$1"
  local output="$2"
  set +e
  "$BIN" --root "$ROOT" --registry "$registry" >"$output" 2>"$output.stderr"
  local status=$?
  set -e
  if [[ $status -ne 1 ]]; then
    echo "expected architecture violation exit 1 for $registry, got $status" >&2
    exit 1
  fi
}

echo "==> prove CLI failure on contradictory and orphaned registries"
run_failure "$RELATION_REGISTRY" "$RELATION_OUT"
run_failure "$OWNER_REGISTRY" "$OWNER_OUT"
run_failure "$ORPHAN_REGISTRY" "$ORPHAN_OUT"
run_failure "$AMBIGUOUS_REGISTRY" "$AMBIGUOUS_OUT"

python3 - "$RELATION_OUT" "$OWNER_OUT" "$ORPHAN_OUT" "$AMBIGUOUS_OUT" <<'PY'
import json
import sys

def reject_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result

def violations(path):
    result = []
    with open(path, encoding="utf-8") as stream:
        for line in stream:
            event = json.loads(line, object_pairs_hook=reject_duplicate_keys)
            if event["event"] == "architecture_violation":
                result.append(event)
    assert result, path
    required = {
        "code", "decision_id", "relationship_kind", "owner_bead", "owner_crate",
        "claim_class", "checker_ids", "evidence_ids", "status",
        "contradiction_class", "source_anchor", "replay_command", "outcome", "message",
    }
    for event in result:
        assert required <= event.keys(), (required - event.keys(), event)
        assert event["outcome"] == "fail", event
        assert event["replay_command"], event
    return result

relation, owner, orphan, ambiguous = map(violations, sys.argv[1:])
assert any(
    event["code"] == "closed_enum"
    and event["decision_id"] == "FG-ADR-BET-B1"
    and event["relationship_kind"] == "accidental_dependency"
    and event["contradiction_class"] == "schema"
    and event["source_anchor"] == "§0.B1"
    for event in relation
), relation
assert any(
    event["code"] == "owner_bead_unresolved"
    and event["decision_id"] == "FG-ADR-BET-B1"
    and event["owner_bead"] == "fgdb-does-not-exist"
    for event in owner
), owner
assert any(
    event["code"] == "owner_crate_unplanned"
    and event["decision_id"] == "FG-ADR-BET-B1"
    and event["owner_crate"] == "fgdb-not-planned"
    for event in owner
), owner
assert any(
    event["code"] == "bead_provenance_orphan"
    and event["owner_bead"].startswith("fgdb-risk-")
    for event in orphan
), orphan
assert any(
    event["code"] == "bead_family_ambiguous"
    and event["owner_bead"].startswith("fgdb-risk-")
    for event in ambiguous
), ambiguous
PY

echo "==> run the complete typed mutation and property suite"
cargo test -p registry-check --test architecture_decisions

echo "ADR E2E GREEN; retained deterministic evidence: $EVIDENCE_DIR"
