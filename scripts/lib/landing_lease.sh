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
# WHY A MECHANISM AND NOT A RULE, stated because the rule was tried. Doing this
# by hand cost, in one evening: a ~40-minute landing hold placed on pane4 on the
# strength of a `pgrep` that was MATCHING ITS OWN COMMAND LINE, and a mid-run
# landing that cost pane2 a full diagnostic cycle plus two false attributions.
# Both failures are in the operator, not the code.
#
# =============================================================================
# THE FOUR POLICY RULINGS THIS FILE IMPLEMENTS (operator, 2026-07-27)
# =============================================================================
#
# ONE — NO TIME-BASED EXPIRY. A lease is broken by a LIVENESS TEST THAT CAN
# FAIL, never by a clock. TTL in this substrate is a LABEL, not a timeout:
# `token.sh acquire` is a plain `mkdir` that fails regardless of age, so a TTL
# has never enforced anything. And an age threshold CANNOT DISTINGUISH A DEAD
# HOLDER FROM A SLOW ONE — measured the hard way in this project, where a
# time-based sweep destroyed 37 pin-clean staged rows precisely because BLOCKED
# work is untouched for being blocked, so it looked abandoned.
#   THE LEASE IS BREAKABLE IF AND ONLY IF THE HOLDER'S PID IS GONE.
# Age is RECORDED AND REPORTED. It is never a reason to reclaim. An earlier cut
# of this file had a TTL+grace backstop that silently freed the lease; it was
# wrong on exactly this point and has been removed.
#
# TWO — BREAKING IS LOUD. When the lease IS broken, say so: holder id, pid, and
# the evidence that the pid is gone. A mechanism that reclaims QUIETLY is how a
# team ends up trusting a guarantee that stopped holding.
#
# THREE — FAIL OPEN, BUT LOUD. If the lease cannot be read, the commit proceeds
# WITH A VISIBLE WARNING.
#   WHY THIS DOES NOT CONTRADICT THE THIRD-STATE DOCTRINE, which says a check
#   that did not run must never report green: A GATE MUST FAIL CLOSED BECAUSE IT
#   IS THE LAST WORD. A LEASE IS NOT THE LAST WORD. 38cca3f already DETECTS a
#   tree that moved under a gate and reports UNRUN carrying both shas.
#   Prevention plus detection is defence in depth, so an unreadable lease costs
#   a warning and the tripwire still catches the bad outcome. What cannot be
#   tolerated is SILENCE, not permissiveness. A gate has no second line behind
#   it; this does.
#
# FOUR — HOLD LANDINGS, NEVER PANES. A leased-out pane must still derive, stage
# and run read-only checks. Only `git commit` on branch `main` is bound; nothing
# here touches `git add`, a build, a test, a gate, or a commit in a scratch
# worktree. Measured: this repo has 29 linked worktrees and 28 are detached
# HEAD, so every staging worktree is unaffected by construction. Holding panes
# rather than landings is what cost pane4 forty minutes.
#
# =============================================================================
# STATES
# =============================================================================
#   FREE        no lease held.                          -> allow, silent
#   BINDING     holder's pid is ALIVE.                  -> refuse
#   BINDING     liveness INDETERMINATE (no pid on file) -> refuse, saying why,
#               Not breakable: we cannot PROVE the         and naming the
#               holder is gone, and ruling ONE forbids      manual remedy.
#               reclaiming on any other ground. Reported, never silently taken.
#   BREAKABLE   holder's pid is GONE.                   -> allow, LOUD
#   UNREADABLE  the lease cannot be read at all.        -> allow, LOUD
#
# =============================================================================
# ON-DISK FORMAT — the holder file is FIVE lines, written by token.sh
# =============================================================================
# The mutual-exclusion primitive stays `token.sh` (atomic `mkdir`), as directed.
# Since fgdb-5j16 the holder file is five lines — who / epoch / ttl / pid /
# pid start time — written by `token.sh acquire` itself: this library passes
# the gate's own `$$` as the holder-pid argument, so the recorded process is
# the gate run, exactly what the pre-commit hook tests. Bare three-line holds
# still parse (pid reads empty) and are reported INDETERMINATE, never
# reclaimed. Release stays token.sh's own `rm -f holder; rmdir lock`; nothing
# here deletes.
#
# The start-time cross-check is what makes liveness EXACT. A bare `kill -0`
# would be fooled by PID REUSE — an unrelated process inheriting the number
# would read as "holder alive" and the lease would never become breakable.
# pid + start time identifies a process uniquely for as long as it exists.
#
# The pid is recorded BY THE HOLDER at acquire. Nothing is pattern-matched, so
# nothing can self-match the way `pgrep -af check.sh` matched its own command
# line tonight — the defect this design removes rather than documents around.
# =============================================================================

FGDB_TOKEN_SH="${FGDB_TOKEN_SH:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/token.sh}"
FGDB_TOKEN_DIR="${FGDB_TOKEN_DIR:-/data/tmp/fgdb_swarm/tokens}"

LANDING_LOCK="$FGDB_TOKEN_DIR/landing.lock"
LANDING_META="$LANDING_LOCK/holder"

# Return channel, read across the source boundary by
# scripts/git_hooks/pre-commit.sh and by the red-proof harness.
# shellcheck disable=SC2034
LANDING_STATE=""
LANDING_HOLDER=""
LANDING_AGE_MIN=""
LANDING_TTL_MIN=""
LANDING_PID=""
LANDING_LIVENESS=""
LANDING_EVIDENCE=""

# _ll_starttime <pid> — field 22 of /proc/<pid>/stat: process start time in clock
# ticks since boot. Constant for the life of a process, so pid+starttime is an
# exact identity.
#
# The comm field (2) is parenthesised and MAY CONTAIN SPACES AND PARENTHESES, so
# `awk '{print $22}'` is wrong for any process whose name has one. Cut through
# the LAST ')' and count from there: start time is field 20 of the remainder.
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

# landing_lease_state — the one query. Sets LANDING_* and echoes the state.
#
# CALL IT PLAIN, NOT IN A COMMAND SUBSTITUTION. `s=$(landing_lease_state)` runs
# it in a SUBSHELL and every variable it sets is discarded — which is exactly how
# the first cut of the hook came to print an empty holder and "m of m" while
# still correctly refusing. Read $LANDING_STATE instead.
# shellcheck disable=SC2034  # LANDING_* are the return channel; see above
landing_lease_state() {
  LANDING_STATE=""; LANDING_HOLDER=""; LANDING_AGE_MIN=""; LANDING_TTL_MIN=""
  LANDING_PID=""; LANDING_LIVENESS=""; LANDING_EVIDENCE=""

  if [ ! -d "$LANDING_LOCK" ]; then
    LANDING_STATE=FREE; printf 'FREE\n'; return 0
  fi
  if [ ! -r "$LANDING_META" ]; then
    LANDING_STATE=UNREADABLE
    LANDING_EVIDENCE="lock directory exists at $LANDING_LOCK but its holder file is unreadable"
    printf 'UNREADABLE\n'; return 0
  fi

  local who epoch ttl pid start now age cur
  who="$(sed -n 1p "$LANDING_META" 2>/dev/null)"
  epoch="$(sed -n 2p "$LANDING_META" 2>/dev/null)"
  ttl="$(sed -n 3p "$LANDING_META" 2>/dev/null)"
  pid="$(sed -n 4p "$LANDING_META" 2>/dev/null)"
  start="$(sed -n 5p "$LANDING_META" 2>/dev/null)"

  case "$epoch" in ''|*[!0-9]*) epoch=0 ;; esac
  now="$(date +%s)"
  # Age is REPORTED ONLY. Ruling ONE: never a reason to reclaim.
  if [ "$epoch" -gt 0 ]; then age=$(( ( now - epoch ) / 60 )); else age="?"; fi

  LANDING_HOLDER="${who:-?}"
  LANDING_AGE_MIN="$age"
  LANDING_TTL_MIN="${ttl:-?}"
  LANDING_PID="${pid:-}"

  # THE ONLY TEST THAT MAY BREAK A LEASE.
  case "$pid" in
    ''|*[!0-9]*)
      # No pid on file — e.g. a hold made by a bare `token.sh acquire landing`.
      # We cannot PROVE the holder is gone, and ruling ONE forbids reclaiming on
      # any other ground. So it BINDS, and says exactly why.
      LANDING_LIVENESS=indeterminate
      LANDING_EVIDENCE="no pid recorded in the holder file, so the liveness test cannot be run; ruling ONE forbids reclaiming a lease on any ground except a liveness test that failed"
      LANDING_STATE=BINDING; printf 'BINDING\n'; return 0
      ;;
  esac

  # THE ONLY TEST THAT MAY BREAK A LEASE, and it must be uid-independent:
  # kill -0 fails EPERM for a process that EXISTS under another uid, and
  # reading that as ESRCH would reap a LIVE foreign-owned holder — two
  # processes then believe they hold the lease. /proc answers existence
  # regardless of ownership.
  if [ ! -d "/proc/$pid" ]; then
    LANDING_LIVENESS=dead
    LANDING_EVIDENCE="/proc/$pid does not exist: the holder's process is gone"
    LANDING_STATE=BREAKABLE; printf 'BREAKABLE\n'; return 0
  fi
  if [ -n "$start" ]; then
    cur="$(_ll_starttime "$pid" 2>/dev/null)"
    if [ -n "$cur" ] && [ "$cur" != "$start" ]; then
      LANDING_LIVENESS=dead
      LANDING_EVIDENCE="pid $pid exists but its start time is $cur, not the recorded $start — the pid was REUSED by an unrelated process, so the holder itself is gone"
      LANDING_STATE=BREAKABLE; printf 'BREAKABLE\n'; return 0
    fi
  fi

  LANDING_LIVENESS=alive
  LANDING_EVIDENCE="/proc/$pid exists (ownership-independent) and its start time matches the recorded $start; the holder is running"
  LANDING_STATE=BINDING; printf 'BINDING\n'; return 0
}

# landing_lease_acquire <holder> [ttl_label] — take the lease for a gate run.
# The ttl is recorded as a LABEL for a human reading `token.sh status`. Nothing
# in this file acts on it (ruling ONE). The gate's own `$$` is passed as the
# holder pid, so the recorded process is the run the hook must test — never
# token.sh's own transient pid or its caller's parent.
landing_lease_acquire() {
  local who="$1" ttl="${2:-45}"
  [ -x "$FGDB_TOKEN_SH" ] || return 1
  "$FGDB_TOKEN_SH" acquire landing "$who" "$ttl" "$$" >/dev/null 2>&1 || return 1
  export FGDB_LANDING_HOLDER="$who"
  return 0
}

# landing_lease_release <holder> — give it back. Safe when not held.
landing_lease_release() {
  local who="$1"
  [ -x "$FGDB_TOKEN_SH" ] || return 0
  "$FGDB_TOKEN_SH" release landing "$who" >/dev/null 2>&1 || true
  unset FGDB_LANDING_HOLDER
  return 0
}
