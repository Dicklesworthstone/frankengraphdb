#!/usr/bin/env bash
set -euo pipefail

# Default/report mode never deletes or mutates artifacts. The only future
# deletion mode is the owner-authorized, confirmation-bound scoped reaper for
# the two exact pools named by fgdb-gate-workdir-lifetime-and-reaper-ruling-1dra.
# This first landing is intentionally the prerequisite only: producer stamps,
# inode-aware accounting, and a liveness report that can refuse a candidate.
root=${1:-/data/tmp/fgdb_swarm}
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/private_subject.sh
. "$repo_root/scripts/lib/private_subject.sh"

classify() {
  case "$1" in
    *xfrc*) printf 'pane7\tfgdb-xfrc\tCLOSED\t[B]';;
    *2pbf*) printf 'pane5\tfgdb-2pbf\tCLOSED\t[B]';;
    *kwoz*) printf 'pane6\tfgdb-kwoz\tIN_PROGRESS\t[R]~7s';;
    *uug4*) printf 'pane3\tfgdb-uug4\tCLOSED\t[B]';;
    *njpk*) printf 'pane5\tfgdb-njpk\tCLOSED\t[B]';;
    *ymqm*) printf 'pane7\tfgdb-ymqm\tCLOSED\t[B]';;
    *dkjg*) printf 'pane5\tfgdb-dkjg\tCLOSED\t[B]';;
    *) printf 'unknown\tunknown\tUNKNOWN\t[!]';;
  esac
}

# authorized_reaper_candidates -> pool<TAB>absolute-path
#
# This is the complete deletion authority boundary. Do not replace either
# direct-child glob with a recursive walk: any third pool needs a new written
# owner ruling. The g0 identity marker is positive provenance; an arbitrary
# /data/tmp/tmp.* directory is not in scope.
authorized_reaper_candidates() {
  local path base
  for path in /data/tmp/tmp.*; do
    [ -d "$path" ] || continue
    [ -d "$path/neg-appendix-bead" ] || continue
    printf 'g0-identity-work\t%s\n' "$path"
  done
  for path in /data/tmp/fgdb-subject/subject-*; do
    [ -d "$path" ] || continue
    base="${path##*/}"
    [[ "$base" =~ ^subject-[0-9a-f]{64}$ ]] || continue
    printf 'registry-check-subject\t%s\n' "$path"
  done
}

# read_reapable_stamp <stamp> <expected-pool>
#
# Independent reader for private_subject.sh's producer record. It accepts the
# exact v1 key set once each, validates all numeric fields and the byte
# partition, then prints the seven values the inventory needs. Producer and
# consumer sharing a typo would otherwise turn a malformed stamp into authority.
read_reapable_stamp() {
  local stamp="$1" expected_pool="$2"
  awk -F '=' -v expected_pool="$expected_pool" '
    BEGIN {
      allowed["format"] = 1
      allowed["pool"] = 1
      allowed["reapable_after_epoch"] = 1
      allowed["measured_at_epoch"] = 1
      allowed["producer_pid"] = 1
      allowed["allocated_bytes"] = 1
      allowed["reclaimable_bytes"] = 1
      allowed["shared_bytes"] = 1
    }
    {
      split_at = index($0, "=")
      if (split_at == 0) bad = 1
      key = substr($0, 1, split_at - 1)
      value = substr($0, split_at + 1)
      if (!(key in allowed) || seen[key]++) bad = 1
      values[key] = value
    }
    END {
      for (key in allowed) if (seen[key] != 1) bad = 1
      if (values["format"] != "fgdb-reapable-v1") bad = 1
      if (values["pool"] != expected_pool) bad = 1
      numeric[1] = "reapable_after_epoch"
      numeric[2] = "measured_at_epoch"
      numeric[3] = "producer_pid"
      numeric[4] = "allocated_bytes"
      numeric[5] = "reclaimable_bytes"
      numeric[6] = "shared_bytes"
      for (i = 1; i <= 6; i++) {
        key = numeric[i]
        if (values[key] !~ /^[0-9]+$/) bad = 1
      }
      if (values["allocated_bytes"] + 0 != values["reclaimable_bytes"] + values["shared_bytes"]) bad = 1
      if (bad) exit 42
      printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n", \
        values["reapable_after_epoch"], values["measured_at_epoch"], \
        values["producer_pid"], values["allocated_bytes"], \
        values["reclaimable_bytes"], values["shared_bytes"], values["pool"]
    }
  ' "$stamp"
}

report_reapable_pools() {
  local now_epoch pool path stamp lock_path lock_fd stamp_values
  local after_epoch stamped_allocated stamped_reclaimable stamped_shared
  local current_values
  local current_allocated current_reclaimable current_shared state reason kb
  local total_reapable_bytes=0 reapable_count=0 invalid_count=0
  now_epoch="$(date -u +%s)"
  printf 'REAPABLE_KB\tPATH\tPOOL\tSTATE\tREAPABLE_AFTER_EPOCH\tREASON\n'
  while IFS=$'\t' read -r pool path; do
    stamp="$path/REAPABLE-AFTER"
    lock_path="$path/.fgdb-reaper.lock"
    if [ ! -f "$stamp" ]; then
      printf '0\t%s\t%s\tUNSTAMPED\t-\tproducer supplied no deletion authority\n' \
        "$path" "$pool"
      continue
    fi
    if [ ! -f "$lock_path" ]; then
      printf '0\t%s\t%s\tINVALID\t-\tstamp has no liveness lock\n' \
        "$path" "$pool"
      invalid_count=$((invalid_count + 1))
      continue
    fi
    exec {lock_fd}<"$lock_path"
    if ! flock -n -x "$lock_fd"; then
      printf '0\t%s\t%s\tLIVE\t-\texclusive liveness lock refused\n' \
        "$path" "$pool"
      exec {lock_fd}>&-
      continue
    fi
    if ! stamp_values="$(read_reapable_stamp "$stamp" "$pool")"; then
      printf '0\t%s\t%s\tINVALID\t-\tmalformed or cross-pool stamp\n' \
        "$path" "$pool"
      invalid_count=$((invalid_count + 1))
      exec {lock_fd}>&-
      continue
    fi
    IFS=$'\t' read -r after_epoch _measured_epoch _producer_pid \
      stamped_allocated stamped_reclaimable stamped_shared _stamped_pool \
      <<<"$stamp_values"
    if ! current_values="$(reapable_measure_allocated_bytes "$path")"; then
      printf '0\t%s\t%s\tINVALID\t%s\tbyte measurement failed\n' \
        "$path" "$pool" "$after_epoch"
      invalid_count=$((invalid_count + 1))
      exec {lock_fd}>&-
      continue
    fi
    IFS=$'\t' read -r current_allocated current_reclaimable current_shared \
      <<<"$current_values"
    if [ "$current_allocated" != "$stamped_allocated" ] \
      || [ "$current_reclaimable" != "$stamped_reclaimable" ] \
      || [ "$current_shared" != "$stamped_shared" ]; then
      printf '0\t%s\t%s\tDRIFT\t%s\tbytes changed after producer stamp\n' \
        "$path" "$pool" "$after_epoch"
      invalid_count=$((invalid_count + 1))
      exec {lock_fd}>&-
      continue
    fi
    kb=$(((current_reclaimable + 1023) / 1024))
    state=WAIT
    reason="retention boundary not reached"
    if [ "$now_epoch" -ge "$after_epoch" ]; then
      state=REAPABLE
      reason="stamp due and exclusive liveness lock acquired"
      total_reapable_bytes=$((total_reapable_bytes + current_reclaimable))
      reapable_count=$((reapable_count + 1))
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$kb" "$path" "$pool" "$state" "$after_epoch" "$reason"
    # Phase one is report-only, so no lock needs to survive this record.
    exec {lock_fd}>&-
  done < <(authorized_reaper_candidates | LC_ALL=C sort -t $'\t' -k2,2)
  printf 'REAPABLE_COUNT\t%s\n' "$reapable_count"
  printf 'REAPABLE_BYTES\t%s\n' "$total_reapable_bytes"
  printf 'INVALID_STAMP_COUNT\t%s\n' "$invalid_count"
  printf 'REAPER_POLICY\tstamp+exclusive-lock+byte-stability; never age-only; report-only phase\n'
}

printf 'KB\tPATH\tOWNER\tBEAD\tSTATUS\tDISPOSITION\n'
closed_kb=0
closed_paths=''
while IFS=$'\t' read -r kb path; do
  meta=$(classify "$path")
  printf '%s\t%s\t%s\n' "$kb" "$path" "$meta"
  if [[ "$meta" == *$'\tCLOSED\t'* ]]; then
    closed_kb=$((closed_kb + kb))
    closed_paths+="$path\n"
  fi
done < <(du -sk "$root"/* 2>/dev/null | awk '$1 >= 51200' | sort -nr)
printf 'CLOSED_SUBTOTAL_KB\t%s\n' "$closed_kb"
printf 'CLOSED_PATHS\n%b' "$closed_paths"
printf 'POLICY\tstatus+ownership+recoverability liveness; never age-only\n'
report_reapable_pools
