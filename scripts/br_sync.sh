#!/usr/bin/env bash
# =============================================================================
# br_sync.sh — the one tracked Beads-export path
# =============================================================================
# Owner beads: fgdb-09iz (ruling), fgdb-wv3v (implementation).
#
# `.beads/config.yaml` disables automatic JSONL export. Routine `br` mutations
# therefore update the shared, untracked database without moving the tracked
# `.beads/issues.jsonl` beneath an in-flight gate. This script owns the explicit
# export required at session completion.
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

if [ "$#" -ne 0 ]; then
  printf 'usage: bash scripts/br_sync.sh\n' >&2
  exit 64
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LANDING_LIB="${FGDB_LANDING_LIB:-$ROOT/scripts/lib/landing_lease.sh}"
BR_BIN="${FGDB_BR_BIN:-br}"
EXPORT_HOLDER="br-sync-$$"
EXPORT_LEASE_HELD=0

flush_export() {
  "$BR_BIN" sync --flush-only
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
    printf 'Retry `bash scripts/br_sync.sh` after the gate exits.\n'
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
