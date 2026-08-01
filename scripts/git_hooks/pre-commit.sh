#!/usr/bin/env bash
# =============================================================================
# pre-commit — refuse a LANDING while a live gate run holds the landing lease
# =============================================================================
# Owner bead: fgdb-eesn. Policy lives in scripts/lib/landing_lease.sh; this file
# is only the enforcement point. Installed by scripts/git_hooks/install.sh.
#
# Kept here under a .sh name rather than pointed at with core.hooksPath, because
# core.hooksPath requires a file named exactly `pre-commit`, and an
# extensionless tracked file lands UNCLAIMED in check.sh's file-coverage
# closure — which would turn check.sh red for every pane.
#
# -----------------------------------------------------------------------------
# THIS HOOK FAILS OPEN — LOUDLY. BOTH HALVES ARE THE POLICY.
# -----------------------------------------------------------------------------
# Every path that cannot read the lease lets the commit through AND PRINTS WHY.
# A gate must fail CLOSED because it is the last word. A LEASE IS NOT THE LAST
# WORD: 38cca3f already detects a tree that moved under a gate and reports UNRUN
# carrying both shas. Prevention plus detection is defence in depth, so an
# unreadable lease costs a warning and the tripwire still catches the bad
# outcome. What cannot be tolerated is SILENCE, not permissiveness.
#
# The silent-allow paths are exactly two, and neither is a failure: no lease is
# held, or this commit is not a landing. Everything else announces itself.
#
# -----------------------------------------------------------------------------
# WHAT IT BINDS, AND WHAT IT DELIBERATELY DOES NOT
# -----------------------------------------------------------------------------
# It binds a commit on branch `main` — a LANDING, the thing that moves the tree
# other panes' gates are running against. It does NOT bind a commit on a
# detached HEAD, which is every scratch/staging worktree here (28 of 29 at the
# time of writing), and it touches nothing else at all: `git add`, builds,
# tests, gates and read-only checks are all unaffected. Hold landings, never
# panes — holding panes is what cost pane4 forty minutes.
#
# ESCAPE HATCH: `git commit --no-verify`, native, always available. It voids the
# in-flight run, so say so in the commit message.
# =============================================================================

set -uo pipefail

allow() { exit 0; }

# warn_allow — fail open, loudly (ruling THREE).
warn_allow() {
  {
    printf '\n'
    printf 'LANDING LEASE NOT ENFORCED — commit allowed, and you are being told.\n\n'
    printf '  reason: %s\n\n' "$1"
    printf 'This is fail-open by policy, not a bug: the lease is prevention, and\n'
    printf 'detection still stands behind it. A gate whose tree moves under it will\n'
    printf 'still report UNRUN carrying both shas (fgdb-49ng / 38cca3f). What is not\n'
    printf 'tolerated is silence, which is why this notice exists.\n\n'
  } >&2
  exit 0
}

top="$(git rev-parse --show-toplevel 2>/dev/null)" || allow
[ -n "$top" ] || allow

# Not a landing => not our business, and silence is correct here.
branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null)" || allow
[ "$branch" = "main" ] || allow

lib="$top/scripts/lib/landing_lease.sh"
[ -r "$lib" ] || warn_allow "the lease library is missing or unreadable at $lib"
# shellcheck source=../lib/landing_lease.sh
. "$lib" 2>/dev/null || warn_allow "the lease library at $lib could not be sourced"

# NOT `state=$(landing_lease_state)`. A command substitution runs it in a
# SUBSHELL and every LANDING_* variable it sets is discarded — which is how the
# first cut of this hook printed an empty holder and "m of m" while still
# correctly refusing. Found by the red-proof, not by reading.
landing_lease_state >/dev/null 2>&1 \
  || warn_allow "the lease state could not be determined (landing_lease_state failed)"
state="${LANDING_STATE:-}"

case "$state" in
  FREE)
    allow
    ;;
  BREAKABLE)
    # Ruling TWO: breaking is LOUD. Holder, pid, and the evidence.
    {
      printf '\n'
      printf 'LANDING LEASE BROKEN — the holder is gone. Commit allowed.\n\n'
      printf '  holder  : %s\n' "${LANDING_HOLDER:-?}"
      printf '  pid     : %s\n' "${LANDING_PID:-?}"
      printf '  held for: %s minute(s) (ttl label %s — labels do not expire leases)\n' \
        "${LANDING_AGE_MIN:-?}" "${LANDING_TTL_MIN:-?}"
      printf '  evidence: %s\n\n' "${LANDING_EVIDENCE:-none recorded}"
      printf 'A lease is broken ONLY by a liveness test that failed, never by a clock:\n'
      printf 'an age threshold cannot tell a dead holder from a slow one. The stale\n'
      printf 'lock is left in place for the operator; reclaiming it quietly is how a\n'
      printf 'team ends up trusting a guarantee that stopped holding.\n\n'
      printf '  clear it: %s/scripts/token.sh steal landing <your-name>\n\n' "$top"
    } >&2
    exit 0
    ;;
  UNREADABLE)
    warn_allow "${LANDING_EVIDENCE:-the lease could not be read}"
    ;;
  BINDING)
    ;;
  *)
    warn_allow "unrecognised lease state '${state}'"
    ;;
esac

# The holder may commit through its own lease — a run that needs to land
# something must not deadlock against itself.
if [ -n "${FGDB_LANDING_HOLDER:-}" ] && [ "${FGDB_LANDING_HOLDER}" = "${LANDING_HOLDER:-}" ]; then
  allow
fi

{
  printf '\n'
  printf 'LANDING REFUSED — a gate run holds the landing lease.\n\n'
  printf '  holder  : %s\n' "${LANDING_HOLDER:-?}"
  printf '  pid     : %s (%s)\n' "${LANDING_PID:-none recorded}" "${LANDING_LIVENESS:-?}"
  printf '  held for: %s minute(s) (ttl label %s)\n' \
    "${LANDING_AGE_MIN:-?}" "${LANDING_TTL_MIN:-?}"
  printf '  evidence: %s\n\n' "${LANDING_EVIDENCE:-none recorded}"

  if [ "${LANDING_LIVENESS:-}" = "indeterminate" ]; then
    printf 'LIVENESS COULD NOT BE TESTED, so this lease is NOT breakable. It is being\n'
    printf 'reported rather than reclaimed: a lease may be broken only by a liveness\n'
    printf 'test that FAILED, and no other ground -- an age threshold cannot tell a\n'
    printf 'dead holder from a slow one. If you know the holder is gone, say so\n'
    printf 'explicitly:\n\n'
    printf '  %s/scripts/token.sh steal landing <your-name>\n\n' "$top"
  else
    printf 'It frees itself the moment that run exits. It will not expire on a timer,\n'
    printf 'and it will not be reclaimed while its process is alive.\n\n'
  fi

  printf 'WHY. Your commit would move the tree underneath a gate that is mid-run. The\n'
  printf 'checker reads its subject at two different times -- pins baked in at COMPILE\n'
  printf 'time, corpus read at RUN time -- so a commit landing between them turns\n'
  printf 'identity pins red on a tree where every pin is correct, and the failure names\n'
  printf 'whichever file happened to be resident. That has already cost this swarm a\n'
  printf 'full diagnostic cycle and two false attributions to innocent code.\n\n'
  printf 'YOU ARE NOT BLOCKED FROM WORKING, BUT DO NOT EDIT TRACKED FILES IN THE MAIN\n'
  printf 'CHECKOUT while this lease is live. check.sh fingerprints working-tree bytes,\n'
  printf 'so a tracked main-checkout edit voids the in-flight run even before commit.\n'
  printf 'Edit, build, test, run gates, git add, and commit in your own scratch worktree;\n'
  printf 'those actions are unaffected. Only the landing commit on main is refused, and\n'
  printf 'only while a run is actually in flight.\n\n'
  printf 'WHAT TO DO\n'
  printf '  * Retry shortly, or watch it:  %s/scripts/token.sh status landing\n' "$top"
  printf '  * Land anyway:                 git commit --no-verify\n'
  printf '    Legitimate and always available -- but it VOIDS the in-flight run\n'
  printf '    (~35 min if that is check.sh). Say so in the commit message so the\n'
  printf '    resulting UNRUN is attributable.\n\n'
} >&2
exit 1
