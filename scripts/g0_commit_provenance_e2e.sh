#!/usr/bin/env bash
# =============================================================================
# g0_commit_provenance_e2e.sh — every bead id a commit cites must resolve
# =============================================================================
# Owner bead: fgdb-baru.
#
# MEASURED 2026-09-02: 71 of the previous ~130 commits on main carried a
# bracketed bead id that existed nowhere — `[fgdb-w10-embedded-54r.1]` x40,
# `[fgdb-gate-genesis-lce.2]` x21, `[fgdb-3w75]` x9,
# `[fgdb-w4-g1-txn-core-qpmg.24]` x2. A bead id is the swarm's only
# cross-pane coordination key (Agent Mail thread, file-reservation reason,
# commit provenance, acceptance criteria). A fabricated id makes a commit
# unattributable to any contract, and nothing in the gate chain read commit
# messages at all: `architecture-check`'s "orphan" law runs the other way
# (records lacking provenance labels).
#
# WHAT IT ASSERTS. For every commit reachable from HEAD in the last
# PROVENANCE_WINDOW_DAYS days, every `[fgdb-…]` token in the commit message
# must resolve to a bead record. Resolution sources, in order:
#   1. the tracked export `.beads/issues.jsonl` (present on every host, CI
#      included);
#   2. the local beads database via `br show`, when `br` and `.beads/*.db`
#      are available (dev boxes);
#   3. the ADJUDICATED table below — ids that were fabricated before this gate
#      existed, recorded under a real bead by fgdb-baru; each row names the
#      real record and expires with the window.
# An id that resolves nowhere is FAIL. An id absent from the export on a host
# with NO database is FAIL only once the citing commit is older than
# PROVENANCE_EXPORT_GRACE_HOURS; younger commits PASS with the deferral named,
# because `br_sync.sh` exports at landing time and a same-day export is the
# documented discipline, not a defect.
#
# WHAT IT DOES NOT ASSERT. It does not read bead contents, does not judge
# whether a commit matches its bead's acceptance criteria, and does not touch
# the database or the export. Crate names (`fgdb-gql`, `fgdb-types`) are never
# bracketed in subjects and are not bead ids; only bracketed tokens count.
# =============================================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# When invoked from inside a checkout that is not the script's own parent (a
# scratch copy during authoring), the checkout wins: the subject is the tree,
# not the script's location.
if top="$(git rev-parse --show-toplevel 2>/dev/null)" && [ -n "$top" ]; then
  ROOT="$top"
fi
cd "$ROOT" || exit 2
# shellcheck source=lib/gate_verdict.sh
. "$ROOT/scripts/lib/gate_verdict.sh"

PROVENANCE_WINDOW_DAYS="${PROVENANCE_WINDOW_DAYS:-7}"
PROVENANCE_EXPORT_GRACE_HOURS="${PROVENANCE_EXPORT_GRACE_HOURS:-24}"
JSONL=".beads/issues.jsonl"

# ADJUDICATED historical citations (id<TAB>real record<TAB>bead that adjudicated).
# Rows are only consulted for commits inside the window; they retire on their own
# once those commits age out. Add a row only with the adjudicating bead named.
ADJUDICATED='fgdb-3w75	fgdb-gate-genesis-lce.2	fgdb-baru'

gate_init "g0-commit-provenance"

[ -r "$JSONL" ] || gate_abort_unrun "the tracked export $JSONL is absent or unreadable; resolution is impossible"

export_ids="$(grep -o '"id":"[^"]*"' "$JSONL" | cut -d'"' -f4 | sort -u)"
[ -n "$export_ids" ] || gate_abort_unrun "the tracked export $JSONL enumerates no record ids"

db_available=0
if command -v br >/dev/null 2>&1 && ls .beads/*.db >/dev/null 2>&1; then
  db_available=1
fi

resolve_in_export() { printf '%s\n' "$export_ids" | grep -qxF -- "$1"; }
resolve_in_db() { [ "$db_available" -eq 1 ] && br show "$1" >/dev/null 2>&1; }
resolve_adjudicated() {
  printf '%s\n' "$ADJUDICATED" | awk -F'\t' -v id="$1" '$1 == id { print $2 "\t" $3; found = 1 } END { exit !found }'
}

now="$(date -u +%s)"
commits="$(git rev-list --since="${PROVENANCE_WINDOW_DAYS}.days" HEAD)"
if [ -z "$commits" ]; then
  gate_pass "no commits within the last ${PROVENANCE_WINDOW_DAYS} days; nothing to attribute"
else
  cited=0
  while IFS= read -r sha; do
    [ -n "$sha" ] || continue
    message="$(git log -1 --format='%B' "$sha")"
    ids="$(printf '%s\n' "$message" | grep -oE '\[fgdb-[a-z0-9][a-z0-9.-]*[a-z0-9]\]' | tr -d '[]' | sort -u)"
    [ -n "$ids" ] || continue
    age_hours=$(( (now - $(git log -1 --format='%ct' "$sha")) / 3600 ))
    short="$(git log -1 --format='%h' "$sha")"
    while IFS= read -r id; do
      [ -n "$id" ] || continue
      cited=$((cited + 1))
      if resolve_in_export "$id"; then
        gate_pass "$short cites $id (tracked export)"
      elif resolve_in_db "$id"; then
        gate_pass "$short cites $id (local beads database; not yet in the tracked export)"
      elif adjudication="$(resolve_adjudicated "$id")"; then
        gate_pass "$short cites $id (adjudicated -> ${adjudication%%	*} by ${adjudication##*	})"
      elif [ "$db_available" -eq 0 ] && [ "$age_hours" -lt "$PROVENANCE_EXPORT_GRACE_HOURS" ]; then
        gate_pass "$short cites $id (deferred: absent from the export, no local database on this host, commit is ${age_hours}h old < ${PROVENANCE_EXPORT_GRACE_HOURS}h export grace)"
      else
        gate_fail "$short cites $id, which resolves in neither $JSONL nor the local beads database (commit ${age_hours}h old)"
      fi
    done <<< "$ids"
  done <<< "$commits"
  if [ "$cited" -eq 0 ]; then
    gate_pass "no bracketed bead citations in the last ${PROVENANCE_WINDOW_DAYS} days of commits"
  fi
fi

# Negative control: a synthetic message citing an id that cannot exist must be
# refused by the same resolver the real commits went through.
if resolve_in_export "fgdb-does-not-exist-000" || resolve_in_db "fgdb-does-not-exist-000" || resolve_adjudicated "fgdb-does-not-exist-000" >/dev/null; then
  gate_fail "control: the resolver accepted fgdb-does-not-exist-000"
else
  gate_pass "control: the resolver refuses fgdb-does-not-exist-000"
fi

gate_verdict
