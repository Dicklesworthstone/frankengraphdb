#!/usr/bin/env bash
# =============================================================================
# br_sync.sh — the one tracked Beads-export path
# =============================================================================
# Owner beads: fgdb-09iz (ruling), fgdb-wv3v (lease), fgdb-q57o (attribution).
#
# `.beads/config.yaml` disables automatic JSONL export. Routine `br` mutations
# therefore update the shared, untracked database without moving the tracked
# `.beads/issues.jsonl` beneath an in-flight gate. This script owns the explicit
# export required at session completion.
#
# EXPECTED RECORD IDS ARE THE ATTRIBUTION BOUNDARY (fgdb-q57o). The database is
# shared across panes, so a full export can contain another pane's pending
# record. The caller must name every record it intends to export:
#
#   bash scripts/br_sync.sh fgdb-123 [fgdb-456 ...]
#
# Before writing, the helper rejects an existing JSONL delta containing any
# other id and rejects a dirty-record count larger than the declared set. After
# writing, it parses the actual Git delta and requires exact set equality. A
# record that races into the DB between those two checks can move the worktree,
# but it cannot be staged or committed silently: the helper exits nonzero and
# names the complete observed set. Re-running with every id is an explicit
# co-landing; omitting one is never permission to sweep it.
#
# HOLD LANDINGS, NEVER PANES. A live gate does not block issue creation,
# updates, comments, or closure. It only defers this explicit tracked-file write
# and returns EX_TEMPFAIL (75), so the caller can retry when the gate exits.
# The exporter atomically takes the same landing token for its sub-second write;
# this closes the read-FREE/then-race-a-gate TOCTOU without holding any pane.
#
# FAIL OPEN, BUT LOUD. The landing lease is prevention, not the last word. If
# its state cannot be read, or its holder is provably gone, this script warns
# and exports. The tree-stability tripwire still detects any bad overlap.
#
# CALL landing_lease_state PLAIN. Command substitution would discard the
# LANDING_* return-channel variables and recreate the empty-holder defect that
# the landing-lease red proof found.
# =============================================================================

set -uo pipefail

if [ "$#" -eq 0 ]; then
  printf 'usage: bash scripts/br_sync.sh <expected-bead-id> [<expected-bead-id> ...]\n' >&2
  printf 'No export ran: the whole-file exporter requires an explicit record-id intent.\n' >&2
  exit 64
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LANDING_LIB="${FGDB_LANDING_LIB:-$ROOT/scripts/lib/landing_lease.sh}"
BR_BIN="${FGDB_BR_BIN:-br}"
EXPORT_HOLDER="br-sync-$$"
EXPORT_LEASE_HELD=0
EXPORT_INTENT_RC=65
JSONL_REL=".beads/issues.jsonl"
EXPECTED_IDS=""
EXPORT_ROOT=""
EXPORT_BASE_HEAD=""

print_id_set() {
  [ -n "$1" ] && printf '%s\n' "$1"
}

id_set_count() {
  if [ -z "$1" ]; then
    printf '0\n'
  else
    printf '%s\n' "$1" | wc -l | tr -d ' '
  fi
}

format_id_set() {
  if [ -z "$1" ]; then
    printf '    (none)\n'
  else
    while IFS= read -r id; do
      printf '    %s\n' "$id"
    done <<<"$1"
  fi
}

prepare_expected_ids() {
  local id supplied_count unique_count

  for id in "$@"; do
    case "$id" in
      "" | *[!A-Za-z0-9._-]* | [!A-Za-z0-9]*)
        printf 'BEADS EXPORT INTENT INVALID — not a Bead id: %s\n' "$id" >&2
        return 64
        ;;
    esac
  done

  supplied_count="$#"
  EXPECTED_IDS="$(printf '%s\n' "$@" | LC_ALL=C sort -u)"
  unique_count="$(id_set_count "$EXPECTED_IDS")"
  if [ "$unique_count" -ne "$supplied_count" ]; then
    printf 'BEADS EXPORT INTENT INVALID — duplicate record ids were supplied.\n' >&2
    format_id_set "$EXPECTED_IDS" >&2
    return 64
  fi
  return 0
}

resolve_export_tree() {
  command -v git >/dev/null 2>&1 || {
    printf 'BEADS EXPORT INTENT UNCHECKED — git is unavailable; no export ran.\n' >&2
    return "$EXPORT_INTENT_RC"
  }
  command -v jq >/dev/null 2>&1 || {
    printf 'BEADS EXPORT INTENT UNCHECKED — jq is unavailable; no export ran.\n' >&2
    return "$EXPORT_INTENT_RC"
  }

  EXPORT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    printf 'BEADS EXPORT INTENT UNCHECKED — not inside a Git worktree; no export ran.\n' >&2
    return "$EXPORT_INTENT_RC"
  }
  [ -r "$EXPORT_ROOT/$JSONL_REL" ] || {
    printf 'BEADS EXPORT INTENT UNCHECKED — cannot read %s; no export ran.\n' \
      "$EXPORT_ROOT/$JSONL_REL" >&2
    return "$EXPORT_INTENT_RC"
  }
  EXPORT_BASE_HEAD="$(git -C "$EXPORT_ROOT" rev-parse HEAD 2>/dev/null)" || {
    printf 'BEADS EXPORT INTENT UNCHECKED — HEAD does not resolve; no export ran.\n' >&2
    return "$EXPORT_INTENT_RC"
  }
  return 0
}

# changed_record_ids <base-commit>
#
# Parse records, not line numbers or substrings. `--unified=0` leaves only
# headers plus removed/added JSONL rows; every other payload shape is rejected
# so a malformed line cannot disappear from the attribution set.
changed_record_ids() {
  local base="$1"
  git -C "$EXPORT_ROOT" diff --no-ext-diff --unified=0 "$base" -- "$JSONL_REL" \
    | awk '
        /^(diff --git |index |--- |\+\+\+ |@@ )/ { next }
        /^\\ No newline at end of file$/ { next }
        /^[+-]\{/ { print substr($0, 2); next }
        {
          print "unexpected non-record line in issues.jsonl diff: " $0 >"/dev/stderr"
          bad = 1
        }
        END { exit bad }
      ' \
    | jq -r '
        if type == "object"
          and (.id | type == "string")
          and (.id | test("^[A-Za-z0-9][A-Za-z0-9._-]*$"))
        then .id
        else error("changed issues.jsonl row has no valid string id")
        end
      ' \
    | LC_ALL=C sort -u
}

dirty_record_count() {
  local status
  status="$("$BR_BIN" sync --status --json)" || return 1
  printf '%s\n' "$status" | jq -er '
    if (.dirty_count | type) == "number"
      and .dirty_count >= 0
      and .dirty_count == (.dirty_count | floor)
    then .dirty_count
    else error("sync status omitted a non-negative integer dirty_count")
    end
  '
}

refuse_existing_foreign_ids() {
  local existing unexpected
  existing="$(changed_record_ids "$EXPORT_BASE_HEAD")" || {
    printf 'BEADS EXPORT INTENT UNCHECKED — existing JSONL delta could not be parsed; no export ran.\n' >&2
    return "$EXPORT_INTENT_RC"
  }
  if ! unexpected="$(
    LC_ALL=C comm -23 \
      <(print_id_set "$existing") \
      <(print_id_set "$EXPECTED_IDS")
  )"; then
    printf 'BEADS EXPORT INTENT UNCHECKED — record-id set comparison failed; no export ran.\n' >&2
    return "$EXPORT_INTENT_RC"
  fi
  if [ -n "$unexpected" ]; then
    {
      printf 'BEADS EXPORT INTENT REFUSED — the existing JSONL delta contains undeclared record ids.\n'
      printf '  expected:\n'
      format_id_set "$EXPECTED_IDS"
      printf '  undeclared:\n'
      format_id_set "$unexpected"
      printf 'No export ran. Coordinate with the record owners or declare an explicit co-landing.\n'
    } >&2
    return "$EXPORT_INTENT_RC"
  fi
  return 0
}

refuse_impossible_dirty_count() {
  local dirty expected_count
  dirty="$(dirty_record_count)" || {
    printf 'BEADS EXPORT INTENT UNCHECKED — br sync status could not report dirty_count; no export ran.\n' >&2
    return "$EXPORT_INTENT_RC"
  }
  expected_count="$(id_set_count "$EXPECTED_IDS")"
  if [ "$dirty" -gt "$expected_count" ]; then
    {
      printf 'BEADS EXPORT INTENT REFUSED — %s dirty DB records cannot fit the %s declared id(s).\n' \
        "$dirty" "$expected_count"
      printf '  expected:\n'
      format_id_set "$EXPECTED_IDS"
      printf 'No export ran. Inspect the pending record owners before widening the intent.\n'
    } >&2
    return "$EXPORT_INTENT_RC"
  fi
  return 0
}

verify_exported_ids() {
  local observed head_after missing_declared unexpected_observed
  observed="$(changed_record_ids "$EXPORT_BASE_HEAD")" || {
    printf 'BEADS EXPORT ATTRIBUTION FAILED — the resulting JSONL delta could not be parsed.\n' >&2
    return "$EXPORT_INTENT_RC"
  }
  head_after="$(git -C "$EXPORT_ROOT" rev-parse HEAD 2>/dev/null)" || {
    printf 'BEADS EXPORT ATTRIBUTION FAILED — HEAD stopped resolving after export.\n' >&2
    return "$EXPORT_INTENT_RC"
  }

  if [ "$head_after" != "$EXPORT_BASE_HEAD" ]; then
    {
      printf 'BEADS EXPORT ATTRIBUTION FAILED — HEAD moved during the export window.\n'
      printf '  HEAD before: %s\n' "$EXPORT_BASE_HEAD"
      printf '  HEAD after:  %s\n' "$head_after"
      printf 'Do not stage the JSONL until its record-id delta is re-audited.\n'
    } >&2
    return "$EXPORT_INTENT_RC"
  fi

  if [ "$observed" != "$EXPECTED_IDS" ]; then
    if ! missing_declared="$(
      LC_ALL=C comm -13 \
        <(print_id_set "$observed") \
        <(print_id_set "$EXPECTED_IDS")
    )"; then
      printf 'BEADS EXPORT ATTRIBUTION FAILED — missing-id set comparison failed.\n' >&2
      return "$EXPORT_INTENT_RC"
    fi
    if ! unexpected_observed="$(
      LC_ALL=C comm -23 \
        <(print_id_set "$observed") \
        <(print_id_set "$EXPECTED_IDS")
    )"; then
      printf 'BEADS EXPORT ATTRIBUTION FAILED — unexpected-id set comparison failed.\n' >&2
      return "$EXPORT_INTENT_RC"
    fi
    {
      printf 'BEADS EXPORT ATTRIBUTION FAILED — exported record ids differ from the declared intent.\n'
      printf '  expected:\n'
      format_id_set "$EXPECTED_IDS"
      printf '  observed:\n'
      format_id_set "$observed"
      printf '  missing declared ids:\n'
      format_id_set "$missing_declared"
      printf '  undeclared observed ids:\n'
      format_id_set "$unexpected_observed"
      printf 'The JSONL may have moved, but no commit is authorized. Re-run only after coordinating every observed id.\n'
    } >&2
    return "$EXPORT_INTENT_RC"
  fi

  printf 'BEADS EXPORT RECORD IDS (exact):\n'
  format_id_set "$observed"
  return 0
}

flush_export() {
  refuse_existing_foreign_ids || return $?
  refuse_impossible_dirty_count || return $?
  "$BR_BIN" sync --flush-only || return $?
  verify_exported_ids
}

release_export_lease() {
  [ "$EXPORT_LEASE_HELD" -eq 1 ] || return 0
  EXPORT_LEASE_HELD=0
  landing_lease_release "$EXPORT_HOLDER" || true
}

defer_export() {
  {
    printf 'DEFERRED BEADS EXPORT — a live gate owns the landing lease.\n\n'
    printf '  holder  : %s\n' "${LANDING_HOLDER:-?}"
    printf '  pid     : %s (%s)\n' \
      "${LANDING_PID:-none recorded}" "${LANDING_LIVENESS:-?}"
    printf '  held for: %s minute(s) (ttl label %s)\n' \
      "${LANDING_AGE_MIN:-?}" "${LANDING_TTL_MIN:-?}"
    printf '  evidence: %s\n\n' "${LANDING_EVIDENCE:-none recorded}"
    printf 'No tracked file was written. Beads mutations remain live in the shared DB.\n'
    printf 'Retry the same explicit record-id set after the gate exits.\n'
  } >&2
  return 75
}

warn_and_flush() {
  local state="$1"
  local detail="$2"
  {
    printf 'BEADS EXPORT LEASE WARNING — proceeding under fail-open policy.\n\n'
    printf '  state   : %s\n' "$state"
    printf '  holder  : %s\n' "${LANDING_HOLDER:-?}"
    printf '  pid     : %s (%s)\n' \
      "${LANDING_PID:-none recorded}" "${LANDING_LIVENESS:-?}"
    printf '  evidence: %s\n' "$detail"
    printf '\nThe prevention layer is unavailable, so the explicit export will run. The\n'
    printf 'tree-stability tripwire remains the fail-closed detection backstop.\n\n'
  } >&2
  flush_export
}

prepare_expected_ids "$@" || exit $?
resolve_export_tree || exit $?

if [ ! -r "$LANDING_LIB" ]; then
  warn_and_flush UNREADABLE "landing-lease library is missing or unreadable at $LANDING_LIB"
  exit $?
fi

# shellcheck source=lib/landing_lease.sh
if ! . "$LANDING_LIB" 2>/dev/null; then
  warn_and_flush UNREADABLE "landing-lease library at $LANDING_LIB could not be sourced"
  exit $?
fi

landing_lease_acquire "$EXPORT_HOLDER" 2
acquire_rc=$?
if [ "$acquire_rc" -eq 0 ]; then
  EXPORT_LEASE_HELD=1
  trap release_export_lease EXIT
  flush_export
  flush_rc=$?
  release_export_lease
  trap - EXIT
  exit "$flush_rc"
fi

# The atomic acquire lost or the prevention substrate failed. Query only to
# distinguish a live holder (defer) from the fail-open states; never use a
# read-FREE answer as authority to claim mutual exclusion.
if ! landing_lease_state >/dev/null 2>&1; then
  warn_and_flush UNREADABLE \
    "atomic landing-lease acquire failed with exit $acquire_rc, and landing_lease_state failed"
  exit $?
fi

case "${LANDING_STATE:-}" in
  FREE)
    warn_and_flush UNAVAILABLE \
      "atomic landing-lease acquire failed with exit $acquire_rc while the state reads FREE"
    exit $?
    ;;
  BINDING)
    defer_export
    exit $?
    ;;
  BREAKABLE)
    warn_and_flush BREAKABLE "${LANDING_EVIDENCE:-the recorded holder is gone}"
    exit $?
    ;;
  UNREADABLE)
    warn_and_flush UNREADABLE "${LANDING_EVIDENCE:-the lease could not be read}"
    exit $?
    ;;
  *)
    warn_and_flush UNKNOWN "unrecognised lease state '${LANDING_STATE:-}'"
    exit $?
    ;;
esac
