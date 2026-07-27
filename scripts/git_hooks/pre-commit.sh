#!/usr/bin/env bash
# =============================================================================
# pre-commit — refuse a LANDING while a gate run holds the landing lease
# =============================================================================
# Owner bead: fgdb-eesn. Source of truth for the policy: scripts/lib/landing_lease.sh.
#
# Installed to .git/hooks/pre-commit by scripts/git_hooks/install.sh. It is kept
# here under a .sh name rather than being pointed at with core.hooksPath because
# core.hooksPath requires the file be named exactly `pre-commit`, and an
# extensionless file lands UNCLAIMED in check.sh's file-coverage closure — which
# would turn check.sh red for every pane. The installer places the copy; the
# tracked original keeps the name that gets it linted and claimed.
#
# -----------------------------------------------------------------------------
# THIS HOOK FAILS OPEN. ALWAYS. THIS IS NOT A BUG.
# -----------------------------------------------------------------------------
# Every error path here exits 0 and lets the commit through: no repo root, no
# lease library, an unreadable holder file, a lease state it cannot parse. The
# asymmetry is the whole design (see decision 1 in landing_lease.sh) — this hook
# is shared by 29 linked worktrees, so a hook that fails CLOSED stops every pane
# in the swarm from committing anything, with no way to fix it that does not
# itself require a commit. A hook that fails OPEN costs, at worst, one voided
# gate run, which 38cca3f already reports honestly as UNRUN rather than as a
# false red against innocent code.
#
# Detection is already solved. Prevention is therefore allowed to fail open, and
# is never allowed to fail closed.
#
# -----------------------------------------------------------------------------
# WHAT IT BINDS, AND WHAT IT DELIBERATELY DOES NOT
# -----------------------------------------------------------------------------
# It binds a commit on branch `main` — a LANDING, the thing that moves the tree
# other panes' gates are running against. It does NOT bind a commit on a
# detached HEAD, which is every scratch/staging worktree in this repo (28 of 29
# at the time of writing). A pane that cannot land can still edit, build, test,
# run gates, and commit in its own worktree. Holding panes rather than holding
# landings is precisely what wasted 40 minutes of pane4's evening.
#
# ESCAPE HATCH: `git commit --no-verify` bypasses this, natively, with no flag of
# our own to remember. Doing so is legitimate — the owner must always be able to
# land — but it voids the in-flight gate run, so say so in the commit message.
# =============================================================================

set -uo pipefail

# allow — the single exit used by every path that is not a live refusal.
allow() { exit 0; }

top="$(git rev-parse --show-toplevel 2>/dev/null)" || allow
[ -n "$top" ] || allow

lib="$top/scripts/lib/landing_lease.sh"
[ -r "$lib" ] || allow
# shellcheck source=../lib/landing_lease.sh
. "$lib" 2>/dev/null || allow

# Only a commit on main is a landing. A detached HEAD is derivation.
branch="$(git symbolic-ref --quiet --short HEAD 2>/dev/null)" || allow
[ "$branch" = "main" ] || allow

# NOT `state="$(landing_lease_state)"`. A command substitution runs the function
# in a SUBSHELL, so every LANDING_* variable it sets is discarded and the refusal
# below prints an empty holder and "m of m". Found by the red-proof, not by
# reading: the refusal still fired, so only the diagnostic was hollow — which is
# the half a pane actually needs to know who to wait for.
landing_lease_state >/dev/null 2>&1 || allow
state="${LANDING_STATE:-}"

case "$state" in
  BINDING) ;;
  *)       allow ;;   # FREE, VOID, or anything unparseable
esac

# The holder is allowed to commit through its own lease. A gate run that needs to
# land something (the beads flow does) must not deadlock against itself.
if [ -n "${FGDB_LANDING_HOLDER:-}" ] && [ "${FGDB_LANDING_HOLDER}" = "${LANDING_HOLDER}" ]; then
  allow
fi

remain=$(( LANDING_TTL_MIN + FGDB_LANDING_GRACE_MIN - LANDING_AGE_MIN ))
[ "$remain" -lt 0 ] && remain=0

cat >&2 <<EOF

LANDING REFUSED — a gate run holds the landing lease.

  holder      : ${LANDING_HOLDER}
  held for    : ${LANDING_AGE_MIN}m of ${LANDING_TTL_MIN}m (+${FGDB_LANDING_GRACE_MIN}m grace)
  frees itself: in <= ${remain}m, or the moment that run's process exits

WHY. Your commit would move the tree underneath a gate that is mid-run. The
checker reads its own subject at two different times — pins are baked in at
COMPILE time, the corpus is read at RUN time — so a commit landing between those
moments turns identity pins red on a tree where every pin is correct, and the
failure names whichever file happens to be resident. That has already cost this
swarm one full diagnostic cycle and two false attributions to innocent code.

YOU ARE NOT BLOCKED FROM WORKING. This holds LANDINGS, not panes. You can still
edit, build, run tests, run gates, and commit in your own scratch worktree — only
a commit on 'main' is refused, and only while a run is actually in flight.

WHAT TO DO
  * Retry in a few minutes. The lease frees itself when the run's process exits;
    it cannot outlive the run, and it cannot starve you if that run dies.
  * Check it:      /data/tmp/fgdb_swarm/token.sh status landing
  * Land anyway:   git commit --no-verify
    Legitimate, and always available — but it VOIDS the in-flight run (~35 min if
    it is check.sh). Say so in the commit message so the UNRUN is attributable.

EOF
exit 1
