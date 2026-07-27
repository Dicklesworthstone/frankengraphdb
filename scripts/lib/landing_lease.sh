# shellcheck shell=bash
# =============================================================================
# landing_lease.sh — the LANDING lease: hold landings, never hold panes
# =============================================================================
# Owner bead: fgdb-eesn. Detection half: fgdb-49ng / 38cca3f.
#
# WHAT THIS IS FOR. `scripts/check.sh` runs for ~35 minutes while other panes
# land commits. The checker has two clocks — src/*.rs bakes pins in at COMPILE
# time via include_str!, tests/identity.rs reads the corpus at RUN time — so a
# commit landing between those moments desynchronises them and four identity
# pins go red on a tree where every pin is correct. 38cca3f made that loss
# LEGIBLE (the gate reports UNRUN carrying both shas). This file is the half that
# makes it not happen.
#
# WHY THE BUILD TOKENS CANNOT DO THIS. build-1/build-2 serialise BUILDS. The
# event that voids a run is a `git commit` from a pane holding no build slot at
# all. Excluding it needs a token a GATE RUN can take and a COMMIT must respect —
# which is why the enforcement point is a git hook, not a convention.
#
# -----------------------------------------------------------------------------
# WHY A MECHANISM AND NOT A RULE, stated because the rule was tried
# -----------------------------------------------------------------------------
# Doing this coordination by hand cost, in one evening: a ~40-minute landing hold
# placed on pane4 on the strength of a `pgrep` that was MATCHING ITS OWN COMMAND
# LINE (the operator's own diagnosis), and a mid-run landing that cost pane2 a
# full diagnostic cycle and produced two false attributions. Both failures are in
# the operator, not the code. A mechanism removes the operator from the loop.
#
# THE pgrep DEFECT IS DESIGNED OUT, not documented around. Liveness here is
# `kill -0 <pid>` against a pid recorded BY THE HOLDER, cross-checked against
# that pid's start time from /proc. A process cannot self-match the way a
# pattern search over command lines can, because nothing is being pattern
# matched. The identical trap fired again while building this: an early
# `pgrep -af check.sh` in this very session returned the pgrep's own shell.
#
# -----------------------------------------------------------------------------
# THE THREE DESIGN DECISIONS, and what each one chose
# -----------------------------------------------------------------------------
# 1. A STALE HOLDER CAN NEVER STARVE THE SWARM. DELIBERATE, and it is the
#    opposite of what `token.sh` does for `catalog`: there, `acquire` refuses
#    forever while the lock dir exists, no matter how old, and only SUGGESTS a
#    `steal` that requires a human to judge the holder dead. For a token that
#    gates EVERY COMMIT IN THE SWARM that is the catastrophic failure: one dead
#    pane and nobody can ever land again, which is the pane4 outage repeated
#    automatically and without end.
#    So this lease AUTO-EXPIRES, by two independent tests, either of which frees
#    it: the holder's process is gone, or the hold has outlived its declared TTL
#    plus a grace. TTL is a label in this substrate, so the timeout lives HERE.
#    The asymmetry that licenses it: over-blocking costs every pane indefinitely;
#    under-blocking costs ONE gate run, and 38cca3f already makes that run report
#    UNRUN with both shas instead of a false red. Detection is solved, so
#    prevention is allowed to fail open. It is never allowed to fail closed.
#
# 2. LIVENESS IS A TEST THAT CAN FAIL, NOT AN AGE THRESHOLD. An age sweep cannot
#    distinguish a dead holder from a legitimately long one — the mistake that
#    once deleted 37 clean staged rows in this project because BLOCKED work is
#    untouched precisely for being blocked. `kill -0` plus the /proc start-time
#    comparison answers the actual question and can return a definite NO. The age
#    cap is only the backstop for a pid that was never recorded.
#    The start-time cross-check is what makes it exact: a bare pid test would be
#    fooled by pid reuse, silently re-binding a lease to an unrelated process.
#
# 3. IT HOLDS LANDINGS, NOT PANES. A blocked pane must still be able to derive —
#    edit, build, test, run gates, commit in its own scratch worktree — because
#    holding panes rather than holding landings is exactly what wasted pane4's
#    time. Enforcement therefore binds ONLY a commit on branch `main`. Measured
#    at the time of writing: this repository has 29 linked worktrees and 28 of
#    them are detached HEAD, so "on main" and "is a landing" coincide almost
#    perfectly, and every scratch worktree is untouched by construction.
#
# -----------------------------------------------------------------------------
# ON-DISK FORMAT, and why it needs no change to token.sh
# -----------------------------------------------------------------------------
# The mutual-exclusion primitive stays `token.sh` (atomic `mkdir`), as directed.
# Its holder file is three lines: who / epoch / ttl, read with `sed -n 1p|2p|3p`.
# This lease APPENDS two more — pid and pid start time — which token.sh never
# reads, so `status`, `renew`, `release` and `steal` keep working untouched and
# an older reader simply sees the hold it always saw. Release stays token.sh's
# `rm -f holder; rmdir lock`, so nothing here deletes a file of its own.
# A hold created by a bare `token.sh acquire landing ...` has no pid recorded;
# that is legal and falls back to the age test alone.
# =============================================================================

FGDB_TOKEN_SH="${FGDB_TOKEN_SH:-/data/tmp/fgdb_swarm/token.sh}"
FGDB_TOKEN_DIR="${FGDB_TOKEN_DIR:-/data/tmp/fgdb_swarm/tokens}"
# Grace beyond the declared TTL before a hold is treated as abandoned. Small on
# purpose: see decision 1 — the failure mode of holding too long is unbounded.
FGDB_LANDING_GRACE_MIN="${FGDB_LANDING_GRACE_MIN:-5}"

LANDING_LOCK="$FGDB_TOKEN_DIR/landing.lock"
LANDING_META="$LANDING_LOCK/holder"

# These are set by landing_lease_state for its caller to read — they are this
# library's return channel, not dead stores. LANDING_HOLDER/TTL/AGE are read by
# scripts/git_hooks/pre-commit.sh to compose its refusal; LANDING_PID and
# LANDING_REASON are read by diagnostics and by the red-proof harness. shellcheck
# cannot see across the source boundary, so SC2034 is silenced here with that
# reason rather than by deleting a live output.
# shellcheck disable=SC2034
LANDING_STATE=""
LANDING_HOLDER=""
LANDING_AGE_MIN=""
LANDING_TTL_MIN=""
LANDING_PID=""
LANDING_REASON=""

# _ll_starttime <pid> — field 22 of /proc/<pid>/stat, the process start time in
# clock ticks since boot. Constant for the life of a process and not reused with
# the pid, so pid+starttime identifies a process exactly.
#
# The comm field (field 2) is parenthesised and MAY CONTAIN SPACES AND
# PARENTHESES, so awk '{print $22}' is wrong on any process whose name has one.
# Cut through the LAST ')' and count from there: start time is then field 20.
_ll_starttime() {
  local pid="$1" stat rest
  [ -r "/proc/$pid/stat" ] || return 1
  stat="$(cat "/proc/$pid/stat" 2>/dev/null)" || return 1
  rest="${stat##*) }"
  # shellcheck disable=SC2086
  set -- $rest
  [ "$#" -ge 20 ] || return 1
  printf '%s\n' "${20}"
}

# landing_lease_state — the one query. Echoes FREE, BINDING or VOID and sets the
# LANDING_* variables. A caller that cannot parse this must treat it as FREE:
# failing open is the whole point (decision 1).
# shellcheck disable=SC2034  # LANDING_* are this library's return channel, read by
# scripts/git_hooks/pre-commit.sh and the red-proof harness across a source boundary
landing_lease_state() {
  LANDING_STATE=""; LANDING_HOLDER=""; LANDING_AGE_MIN=""; LANDING_TTL_MIN=""
  LANDING_PID=""; LANDING_REASON=""

  if [ ! -d "$LANDING_LOCK" ]; then
    LANDING_STATE=FREE; printf 'FREE\n'; return 0
  fi
  if [ ! -r "$LANDING_META" ]; then
    LANDING_REASON="lock directory exists but its holder file is unreadable"
    LANDING_STATE=VOID; printf 'VOID\n'; return 0
  fi

  local who epoch ttl pid start now age live
  who="$(sed -n 1p "$LANDING_META" 2>/dev/null)"
  epoch="$(sed -n 2p "$LANDING_META" 2>/dev/null)"
  ttl="$(sed -n 3p "$LANDING_META" 2>/dev/null)"
  pid="$(sed -n 4p "$LANDING_META" 2>/dev/null)"
  start="$(sed -n 5p "$LANDING_META" 2>/dev/null)"

  case "$epoch" in ''|*[!0-9]*) epoch=0 ;; esac
  case "$ttl"   in ''|*[!0-9]*) ttl=45 ;; esac
  now="$(date +%s)"
  age=$(( ( now - epoch ) / 60 ))

  LANDING_HOLDER="${who:-?}"
  LANDING_AGE_MIN="$age"
  LANDING_TTL_MIN="$ttl"
  LANDING_PID="$pid"

  # TEST 1 — liveness, the one that can return a definite NO.
  case "$pid" in
    ''|*[!0-9]*) ;;                       # no pid recorded; fall through to age
    *)
      live=yes
      kill -0 "$pid" 2>/dev/null || live=no
      if [ "$live" = yes ] && [ -n "$start" ]; then
        local cur
        cur="$(_ll_starttime "$pid" 2>/dev/null)"
        # A different start time means the pid was REUSED by an unrelated
        # process. Treating that as alive would re-bind the lease to a stranger.
        [ -n "$cur" ] && [ "$cur" != "$start" ] && live=no
      fi
      if [ "$live" = no ]; then
        LANDING_REASON="holder process $pid is gone (lease abandoned, not released)"
        LANDING_STATE=VOID; printf 'VOID\n'; return 0
      fi
      ;;
  esac

  # TEST 2 — the age backstop. Independent of test 1 on purpose: a wedged but
  # still-running holder must not hold the swarm forever either.
  if [ "$age" -gt $(( ttl + FGDB_LANDING_GRACE_MIN )) ]; then
    # shellcheck disable=SC2034  # return channel; see the declaration block
    LANDING_REASON="hold is ${age}m old, past its ${ttl}m TTL + ${FGDB_LANDING_GRACE_MIN}m grace"
    LANDING_STATE=VOID; printf 'VOID\n'; return 0
  fi

  LANDING_STATE=BINDING; printf 'BINDING\n'; return 0
}

# landing_lease_acquire <holder> [ttl_min] — take the lease for a gate run.
# Exit 0 = held by you. Non-zero = someone else holds it; DO NOT proceed as if
# you do. Never blocks: a gate that cannot get the lease should still run (its
# verdict may then be voided by a landing, which 38cca3f reports honestly).
landing_lease_acquire() {
  local who="$1" ttl="${2:-45}" start
  if [ ! -x "$FGDB_TOKEN_SH" ]; then
    return 1
  fi
  "$FGDB_TOKEN_SH" acquire landing "$who" "$ttl" >/dev/null 2>&1 || return 1
  # Append the liveness half. token.sh reads only lines 1-3, so this is additive.
  start="$(_ll_starttime "$$" 2>/dev/null)"
  printf '%s\n%s\n' "$$" "${start:-}" >> "$LANDING_META" 2>/dev/null || true
  export FGDB_LANDING_HOLDER="$who"
  return 0
}

# landing_lease_release <holder> — give it back. Safe to call when not held.
landing_lease_release() {
  local who="$1"
  [ -x "$FGDB_TOKEN_SH" ] || return 0
  "$FGDB_TOKEN_SH" release landing "$who" >/dev/null 2>&1 || true
  unset FGDB_LANDING_HOLDER
  return 0
}
