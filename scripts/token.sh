#!/usr/bin/env bash
# =============================================================================
# token.sh — dependency-free advisory tokens for the frankengraphdb swarm
# =============================================================================
# Owner bead: fgdb-5j16 (repair of the substrate documented in fgdb-wzyl).
#
# WHAT THIS IS. Advisory mutual exclusion for shared lanes (catalog writes,
# build slots, the landing lease). `mkdir` is atomic on POSIX, so two agents
# racing to acquire the same token cannot both win. MCP Agent Mail's
# `file_reservation_paths` is the primary reservation surface; this is the
# dependency-free fallback and the primitive under
# scripts/lib/landing_lease.sh. Nothing here touches git state.
#
# Usage:
#   token.sh acquire <token> <your-name> [ttl_minutes] [holder_pid] -> exit 0 = you hold it
#   token.sh renew   <token> <your-name> [ttl_minutes] [holder_pid] # your OWN hold only
#   token.sh release <token> <your-name>
#   token.sh status  [token]
#   token.sh steal   <token> <your-name>   # liveness-gated; see below
#
# Tokens in use:
#   catalog  - the ONE catalog write token. Required before editing any of:
#              registries/**, tools/registry-check/**, scripts/g0_identity_e2e.sh
#   build-1  - build slot 1 } take EITHER before `scripts/check.sh` or `cargo test`
#   build-2  - build slot 2 }
#   landing  - the gate-run lease (fgdb-eesn). Taken by gate_init for the run's
#              lifetime and enforced by scripts/git_hooks/pre-commit.sh.
#
# HOLDER NAMING (mandatory): name every hold <agent>-<bead-short> —
# pane2-a16, RoseForest-5j16. Ad-hoc names make `status` unreadable to every
# agent but the holder, which defeats a shared advisory token: on 2026-07-25
# cc_2 concluded its token yield had gone to a stranger and that cc_1 was still
# blocked, when `cc-a16-pane8` WAS cc_1 and had already landed.
#
# =============================================================================
# THE CONTRACT (fgdb-5j16; the twin policy statement is
# scripts/lib/landing_lease.sh ruling ONE)
# =============================================================================
#
# TTL IS A LABEL, NEVER A TIMEOUT. An age threshold cannot distinguish a dead
# holder from a slow one — measured the hard way in this project, where a
# time-based sweep destroyed 37 pin-clean staged rows precisely because BLOCKED
# work is untouched for being blocked, so it looked abandoned. Age is RECORDED
# AND REPORTED (status marks STALE past ttl+20m grace, advisory only). It is
# never a reason to reclaim.
#
# THE ONLY AUTOMATIC CLEARING IS A LIVENESS TEST THAT FAILED. Every hold
# records the holder's pid and that pid's /proc start time (lines 4-5 of the
# holder file — the same layout landing_lease.sh has always appended). A bare
# `kill -0` would be fooled by PID REUSE; pid + start time identifies a process
# uniquely for as long as it exists. `acquire` reaps a hold whose recorded
# process is gone, LOUDLY, with the evidence. `steal` refuses a hold whose
# recorded process is alive, and remains the manual path for holds with no pid
# recorded (legacy or bare holds), where a human vouches the holder is gone and
# announces the steal in a bead comment.
#
# THERE IS NO `force`. An earlier cut let `release <tok> force` bypass the
# holder check — an undocumented, zero-ceremony override that made the
# documented ceremony a lie (mutation-proven in fgdb-5j16). It is removed, not
# documented: the liveness gate above is the cheap path for a dead holder, and
# stealing from a live stranger is exactly what this substrate exists to refuse.
#
# There is no queue: acquire is non-blocking, so FIFO-by-wait-time is not
# expressible. If the reaping/liveness repair above ever proves insufficient
# against polling-loop starvation, that is a new bead, not a silent addition.
# =============================================================================

set -uo pipefail

DIR="${FGDB_TOKEN_DIR:-/data/tmp/fgdb_swarm/tokens}"
mkdir -p "$DIR"

cmd="${1:-status}"
tok="${2:-}"
who="${3:-unknown}"
ttl="${4:-45}"
holder_pid="${5:-$PPID}"

lock="$DIR/$tok.lock"
meta="$lock/holder"

now() { date +%s; }

# pid_starttime <pid> — field 22 of /proc/<pid>/stat: process start time in
# clock ticks since boot. Constant for the life of a process, so pid+starttime
# is an exact identity. Twin of _ll_starttime in scripts/lib/landing_lease.sh;
# the two implementations must stay identical.
#
# The comm field (2) is parenthesised and MAY CONTAIN SPACES AND PARENTHESES,
# so `awk '{print $22}'` is wrong for any process whose name has one. Cut
# through the LAST ')' and count from there: start time is field 20 of the
# remainder.
pid_starttime() {
  local pid="$1" stat rest
  [ -r "/proc/$pid/stat" ] || return 1
  stat="$(cat "/proc/$pid/stat" 2>/dev/null)" || return 1
  rest="${stat##*) }"
  # shellcheck disable=SC2086
  set -- $rest
  [ "$#" -ge 20 ] || return 1
  printf '%s\n' "${20}"
}

# read_meta — load the holder file of $lock into M_WHO/M_EPOCH/M_TTL/M_PID/
# M_START. Missing fields read empty; a legacy three-line hold yields an empty
# M_PID, which the verdict below reports as indeterminate.
read_meta() {
  M_WHO="$(sed -n 1p "$meta" 2>/dev/null)"
  M_EPOCH="$(sed -n 2p "$meta" 2>/dev/null)"
  M_TTL="$(sed -n 3p "$meta" 2>/dev/null)"
  M_PID="$(sed -n 4p "$meta" 2>/dev/null)"
  M_START="$(sed -n 5p "$meta" 2>/dev/null)"
}

# torn_meta — is the holder file present but unusable?
#
# A hold whose first three fields are not all present is TORN, not legacy. The
# distinction is the whole point: a legacy three-line hold has who/epoch/ttl and
# only lacks the pid, so it reads `indeterminate` and a human can vouch for it.
# A torn hold has no epoch, and `hold_line` then computes its age from epoch 0
# and prints a confident "for 29764144m" — a plausible, parseable, completely
# false answer. That is the shape this repo already refuses in its gates: three
# states, not two, and a state that did not complete must never report like one
# that did.
torn_meta() {
  [ -z "${M_WHO:-}" ] || [ -z "${M_EPOCH:-}" ] || [ -z "${M_TTL:-}" ]
}

# write_meta — record a hold: who / epoch / ttl / pid / pid start time.
#
# ENOSPC-SAFE BY CONSTRUCTION, because it was not: `printf >"$meta"` creates the
# file, then fails mid-write when the filesystem is full, leaving a zero-byte
# holder inside a lock directory `mkdir` already took. MEASURED 2026-08-04 —
# scripts/token.sh acquire catalog ... hit "printf: write error: No space left
# on device" during fgdb-5ekk and left exactly that: catalog.lock/holder at 0
# bytes, reported ever since as "HELD by ? for 29764144m (ttl ?m)".
#
# Publication is now write-elsewhere-then-rename: a full write to a temp file in
# the SAME directory (so the rename is same-filesystem and therefore atomic),
# verified for both exit status and the exact expected line count, and only then
# moved onto the real name. A reader consequently sees either the previous state
# or a complete record, never a half-written one. The line-count check is not
# redundant with the exit status: a short write can succeed per-call and still
# leave a truncated file when the failure lands between the printf and the flush.
write_meta() {
  local tmp
  tmp="$meta.$$.tmp"
  if ! printf '%s\n%s\n%s\n%s\n%s\n' \
      "$who" "$(now)" "$ttl" "$holder_pid" "$(pid_starttime "$holder_pid")" \
      >"$tmp" 2>/dev/null; then
    rm -f "$tmp"
    return 1
  fi
  if [ "$(wc -l <"$tmp" 2>/dev/null || echo 0)" -ne 5 ]; then
    rm -f "$tmp"
    return 1
  fi
  if ! mv -f "$tmp" "$meta" 2>/dev/null; then
    rm -f "$tmp"
    return 1
  fi
  return 0
}

# publish_or_release — take-side wrapper: if the hold cannot be recorded, do not
# keep the lock.
#
# The old acquire path called write_meta and ignored its status, so a failed
# metadata write still printed ACQUIRED and exited 0: the caller was told it held
# the catalog token while the holder file was empty. Rolling the mkdir back is
# what makes "acquire failed" and "no lease exists" the same observable state,
# which is the property that lets a later acquire proceed without anyone having
# to steal a lease.
publish_or_release() {
  if write_meta; then
    return 0
  fi
  rm -f "$meta" 2>/dev/null
  rmdir "$lock" 2>/dev/null
  return 1
}

# verdict — the one liveness test. Sets LIVENESS to alive|dead|indeterminate
# and EVIDENCE to the reason. Call read_meta first.
verdict() {
  case "$M_PID" in
    ''|*[!0-9]*)
      LIVENESS=indeterminate
      EVIDENCE="no holder pid recorded, so the liveness test cannot run"
      return 0
      ;;
  esac
  # Existence must be uid-independent: kill -0 fails EPERM for a process that
  # EXISTS under another uid, and reading that as ESRCH would reap a LIVE
  # foreign-owned holder — the catastrophic direction for a lock. /proc
  # answers existence regardless of ownership.
  if [ ! -d "/proc/$M_PID" ]; then
    LIVENESS=dead
    EVIDENCE="/proc/$M_PID does not exist: the holder's process is gone"
    return 0
  fi
  if [ -n "$M_START" ]; then
    local cur
    cur="$(pid_starttime "$M_PID" 2>/dev/null)"
    if [ -n "$cur" ] && [ "$cur" != "$M_START" ]; then
      LIVENESS=dead
      EVIDENCE="pid $M_PID exists but its start time is $cur, not the recorded $M_START — the pid was REUSED"
      return 0
    fi
  fi
  LIVENESS=alive
  EVIDENCE="/proc/$M_PID exists (ownership-independent) and its start time matches the recording"
  return 0
}

# hold_line — the ONE reader of the age/grace rule, shared by acquire's HELD
# report and by status. Age is reported; the STALE marker past ttl+20m grace
# is advisory and never triggers an action (labels do not expire holds).
hold_line() {
  local age
  age=$(( ( $(now) - ${M_EPOCH:-0} ) / 60 ))
  if [ -n "${M_EPOCH:-}" ] && [ "$age" -gt "$(( ${M_TTL:-45} + 20 ))" ]; then
    printf 'for %sm (ttl %sm) — STALE past ttl+20m grace (advisory only)' "$age" "${M_TTL:-?}"
  else
    printf 'for %sm (ttl %sm)' "$age" "${M_TTL:-?}"
  fi
}

# take_after_clearing — remove a cleared hold and take the token, with mkdir
# still the arbiter if a third party races the window.
take_after_clearing() {
  rm -f "$meta"
  rmdir "$lock" 2>/dev/null
  if mkdir "$lock" 2>/dev/null; then
    publish_or_release || return 1
    return 0
  fi
  return 1
}

case "$cmd" in
  acquire)
    [ -z "$tok" ] && { echo "usage: token.sh acquire <token> <agent> [ttl_min] [holder_pid]" >&2; exit 2; }
    if mkdir "$lock" 2>/dev/null; then
      if ! publish_or_release; then
        echo "FAILED to record the hold for $tok (could not publish the holder file; the filesystem may be full). The lock was released, so no torn lease remains — check disk space and retry." >&2
        exit 1
      fi
      echo "ACQUIRED $tok by $who (ttl ${ttl}m, holder pid $holder_pid)"
      exit 0
    fi
    read_meta
    if torn_meta; then
      echo "TORN $tok: a lock directory exists but its holder record is incomplete (who=${M_WHO:-<empty>} epoch=${M_EPOCH:-<empty>} ttl=${M_TTL:-<empty>}). This is a HALF-WRITTEN acquire, not evidence that no writer exists, and it is NOT reapable by the liveness path — there is no pid to test. Do not steal, remove, or edit it on this evidence: get coordinator confirmation that no lane holds $tok, then repair through the approved path." >&2
      exit 1
    fi
    verdict
    case "$LIVENESS" in
      dead)
        if take_after_clearing; then
          echo "ACQUIRED $tok by $who (ttl ${ttl}m, holder pid $holder_pid) — REAPED a dead hold from ${M_WHO:-?}: $EVIDENCE"
          exit 0
        fi
        echo "HELD: $tok was re-taken while the dead hold was being reaped; retry or run: token.sh status $tok" >&2
        exit 1
        ;;
      alive)
        echo "HELD by ${M_WHO:-?} $(hold_line) — holder pid $M_PID alive ($EVIDENCE). Do NOT edit those files."
        echo "Go do design/census work and retry in ~10 minutes."
        exit 1
        ;;
      *)
        echo "HELD by ${M_WHO:-?} $(hold_line) — $EVIDENCE; cannot prove the holder is gone."
        echo "If you have confirmed the holder is gone (no pane activity, no new commits),"
        echo "run: token.sh steal $tok $who"
        exit 1
        ;;
    esac
    ;;
  release)
    [ -z "$tok" ] && { echo "usage: token.sh release <token> <agent>" >&2; exit 2; }
    if [ ! -d "$lock" ]; then echo "NOT HELD: $tok"; exit 0; fi
    read_meta
    if [ "$M_WHO" != "$who" ]; then
      echo "REFUSED: $tok is held by ${M_WHO:-?}, not $who. Only the holder may release." >&2
      echo "If the holder is gone, clearing is liveness-gated: token.sh steal $tok $who" >&2
      exit 1
    fi
    rm -f "$meta"; rmdir "$lock" 2>/dev/null
    echo "RELEASED $tok (was ${M_WHO:-?})"
    ;;
  renew)
    # Extend YOUR OWN hold when the work is genuinely still in flight. Without
    # this, a legitimately-working holder's TTL label silently becomes a lie,
    # which invites either a bad steal or an agent abandoning correct work to
    # look compliant.
    [ -z "$tok" ] && { echo "usage: token.sh renew <token> <agent> [ttl_min] [holder_pid]" >&2; exit 2; }
    if [ ! -d "$lock" ]; then echo "NOT HELD: $tok — use acquire" >&2; exit 1; fi
    read_meta
    if [ "$M_WHO" != "$who" ]; then
      echo "REFUSED: $tok is held by ${M_WHO:-?}, not $who. You may only renew your own hold." >&2
      exit 1
    fi
    write_meta
    echo "RENEWED $tok for $who (fresh ttl ${ttl}m, holder pid $holder_pid) — say why in your bead comment"
    ;;
  steal)
    [ -z "$tok" ] && { echo "usage: token.sh steal <token> <agent>" >&2; exit 2; }
    if [ ! -d "$lock" ]; then
      if mkdir "$lock" 2>/dev/null; then
        write_meta
        echo "ACQUIRED $tok by $who (ttl ${ttl}m, holder pid $holder_pid) — it was free"
        exit 0
      fi
    fi
    read_meta
    verdict
    case "$LIVENESS" in
      alive)
        echo "REFUSED: $tok holder ${M_WHO:-?} is alive (pid $M_PID: $EVIDENCE). Stealing from a live holder is what this substrate exists to refuse." >&2
        exit 1
        ;;
      dead)
        if take_after_clearing; then
          echo "STOLEN $tok from ${M_WHO:-?} by $who — the hold was provably dead: $EVIDENCE. Announce this in your bead comment."
          exit 0
        fi
        echo "HELD: $tok was re-taken while the dead hold was being cleared; retry or run: token.sh status $tok" >&2
        exit 1
        ;;
      *)
        if take_after_clearing; then
          echo "STOLEN $tok from ${M_WHO:-?} by $who — $EVIDENCE; you vouched the holder is gone. Announce this in your bead comment."
          exit 0
        fi
        echo "HELD: $tok was re-taken while the hold was being cleared; retry or run: token.sh status $tok" >&2
        exit 1
        ;;
    esac
    ;;
  status)
    if [ -n "$tok" ]; then set -- "$DIR/$tok.lock"; else set -- "$DIR"/*.lock; fi
    any=0
    for L in "$@"; do
      [ -d "$L" ] || continue
      any=1
      n=$(basename "$L" .lock)
      lock="$L"
      meta="$L/holder"
      read_meta
      # A torn hold is reported as TORN and never as HELD-with-an-age. Deriving
      # an age from a missing epoch is what produced "HELD by ? for 29764144m"
      # for the ENOSPC-torn catalog token: fifty-six years, printed with the same
      # confidence as a real answer, and indistinguishable at a glance from a
      # legacy hold that a human may legitimately vouch for.
      if torn_meta; then
        echo "$n: TORN — lock directory exists but the holder record is incomplete (who=${M_WHO:-<empty>} epoch=${M_EPOCH:-<empty>} ttl=${M_TTL:-<empty>}); no age or liveness can be derived. NOT reapable by the liveness path and NOT evidence that no writer exists; needs coordinator confirmation plus the approved repair path."
        continue
      fi
      verdict
      case "$LIVENESS" in
        dead)      v="; holder pid $M_PID DEAD ($EVIDENCE) — reapable on next acquire" ;;
        alive)     v="; holder pid $M_PID alive" ;;
        *)         v="; liveness indeterminate ($EVIDENCE)" ;;
      esac
      echo "$n: HELD by ${M_WHO:-?} $(hold_line)$v"
    done
    if [ "$any" -eq 0 ]; then echo "no tokens held"; fi
    exit 0
    ;;
  *) echo "usage: token.sh {acquire|renew|release|steal|status} <token> <agent> [ttl_min] [holder_pid]" >&2
     echo "       name your hold <agent>-<bead-short>, e.g. pane2-a16" >&2; exit 2 ;;
esac
