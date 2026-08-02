#!/usr/bin/env bash
set -euo pipefail

# Default/report mode never deletes or mutates artifacts. Deletion requires the
# explicit owner-authorized `--reap <sha256>` mode and is restricted to the two
# exact pools named by fgdb-gate-workdir-lifetime-and-reaper-ruling-1dra. The
# digest is emitted by a prior report over the complete current REAPABLE set, so
# the human decision is bound to paths, stamps, inode identities, and byte
# accounting rather than to age or a moving glob.
usage() {
  cat <<'EOF'
usage: scripts/disk_hygiene.sh [--report] [--root <swarm-root>]
       scripts/disk_hygiene.sh --reap <confirmation-sha256> [--root <swarm-root>]

Default mode is report-only. --reap is destructive and accepts only the exact
REAPER_CONFIRMATION_SHA256 printed by a prior report of the complete current
REAPABLE set. It never considers paths outside the two pools in the owner ruling.
--root changes only the legacy swarm inventory; it never widens reaper scope.
EOF
}

mode=report
explicit_mode=0
confirmation_sha256=""
root=/data/tmp/fgdb_swarm
root_set=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --report)
      [ "$explicit_mode" -eq 0 ] || {
        printf 'error: choose exactly one of --report or --reap\n' >&2
        exit 64
      }
      mode=report
      explicit_mode=1
      shift
      ;;
    --reap)
      [ "$explicit_mode" -eq 0 ] || {
        printf 'error: choose exactly one of --report or --reap\n' >&2
        exit 64
      }
      [ "$#" -ge 2 ] || {
        printf 'error: --reap requires a confirmation SHA-256\n' >&2
        exit 64
      }
      mode=reap
      explicit_mode=1
      confirmation_sha256="$2"
      shift 2
      ;;
    --root)
      [ "$#" -ge 2 ] && [ "$root_set" -eq 0 ] || {
        printf 'error: --root requires one path and may appear only once\n' >&2
        exit 64
      }
      root="$2"
      root_set=1
      shift 2
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    --*)
      printf 'error: unknown option: %s\n' "$1" >&2
      usage >&2
      exit 64
      ;;
    *)
      [ "$root_set" -eq 0 ] || {
        printf 'error: unexpected positional argument: %s\n' "$1" >&2
        exit 64
      }
      root="$1"
      root_set=1
      shift
      ;;
  esac
done
if [ "$mode" = reap ] && [[ ! "$confirmation_sha256" =~ ^[0-9a-f]{64}$ ]]; then
  printf 'error: --reap confirmation must be 64 lowercase hexadecimal characters\n' >&2
  exit 64
fi

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
    [ -d "$path" ] && [ ! -L "$path" ] || continue
    base="${path##*/}"
    [[ "$base" =~ ^tmp\.[A-Za-z0-9._-]+$ ]] || continue
    [ -d "$path/neg-appendix-bead" ] \
      && [ ! -L "$path/neg-appendix-bead" ] || continue
    printf 'g0-identity-work\t%s\n' "$path"
  done
  for path in /data/tmp/fgdb-subject/subject-*; do
    [ -d "$path" ] && [ ! -L "$path" ] || continue
    base="${path##*/}"
    [[ "$base" =~ ^subject-[0-9a-f]{64}$ ]] || continue
    printf 'registry-check-subject\t%s\n' "$path"
  done
}

# candidate_is_authorized <pool> <path>
#
# Repeats the authority boundary without consulting the original glob. Apply
# mode uses this immediately before every destructive traversal so a path that
# was renamed or structurally changed after discovery fails closed.
candidate_is_authorized() {
  local pool="$1" path="$2" canonical_path
  canonical_path="$(realpath -e -- "$path")" || return 1
  [ "$canonical_path" = "$path" ] || return 1
  case "$pool" in
    g0-identity-work)
      [[ "$path" =~ ^/data/tmp/tmp\.[A-Za-z0-9._-]+$ ]] || return 1
      [ -d "$path" ] && [ ! -L "$path" ] || return 1
      [ -d "$path/neg-appendix-bead" ] \
        && [ ! -L "$path/neg-appendix-bead" ]
      ;;
    registry-check-subject)
      [[ "$path" =~ ^/data/tmp/fgdb-subject/subject-[0-9a-f]{64}$ ]] || return 1
      [ -d "$path" ] && [ ! -L "$path" ]
      ;;
    *) return 1 ;;
  esac
}

# candidate_contains_mount <path>
#
# Returns 0 when the candidate itself or anything below it is a mountpoint, 1
# when no mountpoint is present, and 2 when the kernel mount table cannot be
# read. A bind mount can share the parent's device number, so `find -xdev` alone
# is not containment: refusing all mounts is the fail-closed answer.
candidate_contains_mount() {
  local path="$1" mount_targets target
  command -v findmnt >/dev/null 2>&1 || return 2
  mount_targets="$(findmnt --kernel=mountinfo --raw --noheadings --output TARGET)" \
    || return 2
  while IFS= read -r target; do
    [ "$target" = "$path" ] || [[ "$target" == "$path/"* ]] || continue
    return 0
  done <<<"$mount_targets"
  return 1
}

# reapable_tree_identity <path>
#
# Hashes the sorted relative entry inventory plus inode and change metadata.
# This is stronger than the allocation partition: a same-size rewrite, rename,
# hard-link change, or entry replacement invalidates the human confirmation.
# ctime is included because an unprivileged writer cannot restore it after a
# content mutation. The path is the final field and NUL terminates each record,
# so arbitrary non-NUL filename bytes cannot merge two inventory records.
reapable_tree_identity() {
  local path="$1" identity
  if ! identity="$({
    find "$path" -xdev -mindepth 1 \
      -printf '%y|%D|%i|%n|%s|%b|%m|%U|%G|%T@|%C@|%P\0'
  } | LC_ALL=C sort -z | sha256sum)"; then
    return 1
  fi
  printf '%s\n' "${identity%% *}"
}

G0_REAPER_PARENT_LOCK_FD=""
G0_REAPER_PARENT_DIR_FD=""
SUBJECT_REAPER_PARENT_LOCK_FD=""
SUBJECT_REAPER_PARENT_DIR_FD=""

release_reaper_parent_locks() {
  local lock_fd dir_fd
  for lock_fd in "$G0_REAPER_PARENT_LOCK_FD" "$SUBJECT_REAPER_PARENT_LOCK_FD"; do
    [ -n "$lock_fd" ] || continue
    exec {lock_fd}>&-
  done
  for dir_fd in "$G0_REAPER_PARENT_DIR_FD" "$SUBJECT_REAPER_PARENT_DIR_FD"; do
    [ -n "$dir_fd" ] || continue
    exec {dir_fd}>&-
  done
  G0_REAPER_PARENT_LOCK_FD=""
  G0_REAPER_PARENT_DIR_FD=""
  SUBJECT_REAPER_PARENT_LOCK_FD=""
  SUBJECT_REAPER_PARENT_DIR_FD=""
}

# acquire_reaper_parent_locks
#
# Apply mode takes the producer protocol's two parent-namespace leases in a
# fixed order and holds them through the whole candidate scan and deletion.
# Nonblocking acquisition is intentional: a live producer makes the liveness
# test fail rather than letting an age-based job wait and later surprise it.
acquire_reaper_parent_locks() {
  local parent_dir lock_path lock_fd dir_fd canonical_parent
  local -a specs=(
    '/data/tmp|.fgdb-g0-identity-reaper-parent.lock|g0'
    '/data/tmp/fgdb-subject|.fgdb-reaper-parent.lock|subject'
  )
  local spec label
  mkdir -p /data/tmp/fgdb-subject || return 75
  for spec in "${specs[@]}"; do
    IFS='|' read -r parent_dir lock_path label <<<"$spec"
    [ -d "$parent_dir" ] && [ ! -L "$parent_dir" ] || {
      release_reaper_parent_locks
      return 75
    }
    canonical_parent="$(realpath -e -- "$parent_dir")" || {
      release_reaper_parent_locks
      return 75
    }
    [ "$canonical_parent" = "$parent_dir" ] || {
      release_reaper_parent_locks
      return 75
    }
    if ! { exec {dir_fd}<"$parent_dir"; }; then
      release_reaper_parent_locks
      return 75
    fi
    if [ "$(stat -Lc '%d:%i' "$parent_dir")" \
      != "$(stat -Lc '%d:%i' "/proc/${BASHPID:-$$}/fd/$dir_fd")" ]; then
      exec {dir_fd}>&-
      release_reaper_parent_locks
      return 75
    fi
    if ! : >>"/proc/${BASHPID:-$$}/fd/$dir_fd/$lock_path"; then
      exec {dir_fd}>&-
      release_reaper_parent_locks
      return 75
    fi
    if ! { exec {lock_fd}<>"/proc/${BASHPID:-$$}/fd/$dir_fd/$lock_path"; }; then
      exec {dir_fd}>&-
      release_reaper_parent_locks
      return 75
    fi
    if ! flock -n -x "$lock_fd"; then
      exec {lock_fd}>&-
      exec {dir_fd}>&-
      release_reaper_parent_locks
      return 75
    fi
    case "$label" in
      g0)
        G0_REAPER_PARENT_LOCK_FD="$lock_fd"
        G0_REAPER_PARENT_DIR_FD="$dir_fd"
        ;;
      subject)
        SUBJECT_REAPER_PARENT_LOCK_FD="$lock_fd"
        SUBJECT_REAPER_PARENT_DIR_FD="$dir_fd"
        ;;
    esac
  done
}

reaper_parent_dir_fd_for_pool() {
  case "$1" in
    g0-identity-work) printf '%s\n' "$G0_REAPER_PARENT_DIR_FD" ;;
    registry-check-subject) printf '%s\n' "$SUBJECT_REAPER_PARENT_DIR_FD" ;;
    *) return 1 ;;
  esac
}

reaper_parent_lock_fd_for_pool() {
  case "$1" in
    g0-identity-work) printf '%s\n' "$G0_REAPER_PARENT_LOCK_FD" ;;
    registry-check-subject) printf '%s\n' "$SUBJECT_REAPER_PARENT_LOCK_FD" ;;
    *) return 1 ;;
  esac
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

REAPABLE_PATHS=()
REAPABLE_POOLS=()
REAPABLE_AFTER_EPOCHS=()
REAPABLE_ALLOCATED_BYTES=()
REAPABLE_RECLAIMABLE_BYTES=()
REAPABLE_SHARED_BYTES=()
REAPABLE_STAMP_VALUES=()
REAPABLE_DIR_IDENTITIES=()
REAPABLE_STAMP_IDENTITIES=()
REAPABLE_LOCK_IDENTITIES=()
REAPABLE_LOCK_FDS=()
REAPABLE_DIR_FDS=()
REAPABLE_TREE_IDENTITIES=()
REAPABLE_RECORDS=()
REAPABLE_CONFIRMATION_SHA256=""
REAPABLE_INVALID_COUNT=0

release_reapable_locks() {
  local lock_fd dir_fd
  for lock_fd in "${REAPABLE_LOCK_FDS[@]}"; do
    [ -n "$lock_fd" ] || continue
    exec {lock_fd}>&-
  done
  for dir_fd in "${REAPABLE_DIR_FDS[@]}"; do
    [ -n "$dir_fd" ] || continue
    exec {dir_fd}>&-
  done
  REAPABLE_LOCK_FDS=()
  REAPABLE_DIR_FDS=()
}

report_reapable_pools() {
  local now_epoch pool path stamp lock_path lock_fd stamp_values
  local after_epoch stamped_allocated stamped_reclaimable stamped_shared
  local current_values
  local current_allocated current_reclaimable current_shared state reason kb
  local dir_identity fd_dir_identity stamp_identity lock_identity record
  local mount_status dir_fd tree_identity candidate_rows
  local total_reapable_bytes=0 reapable_count=0 invalid_count=0
  REAPABLE_PATHS=()
  REAPABLE_POOLS=()
  REAPABLE_AFTER_EPOCHS=()
  REAPABLE_ALLOCATED_BYTES=()
  REAPABLE_RECLAIMABLE_BYTES=()
  REAPABLE_SHARED_BYTES=()
  REAPABLE_STAMP_VALUES=()
  REAPABLE_DIR_IDENTITIES=()
  REAPABLE_STAMP_IDENTITIES=()
  REAPABLE_LOCK_IDENTITIES=()
  REAPABLE_LOCK_FDS=()
  REAPABLE_DIR_FDS=()
  REAPABLE_TREE_IDENTITIES=()
  REAPABLE_RECORDS=()
  REAPABLE_CONFIRMATION_SHA256=""
  REAPABLE_INVALID_COUNT=0
  now_epoch="$(date -u +%s)"
  if ! candidate_rows="$(authorized_reaper_candidates \
    | LC_ALL=C sort -t $'\t' -k2,2)"; then
    printf 'error: authorized candidate enumeration was incomplete\n' >&2
    return 1
  fi
  printf 'REAPABLE_KB\tPATH\tPOOL\tSTATE\tREAPABLE_AFTER_EPOCH\tREASON\n'
  if [ -n "$candidate_rows" ]; then
    while IFS=$'\t' read -r pool path; do
    stamp="$path/REAPABLE-AFTER"
    lock_path="$path/.fgdb-reaper.lock"
    if [ ! -f "$stamp" ]; then
      printf '0\t%s\t%s\tUNSTAMPED\t-\tproducer supplied no deletion authority\n' \
        "$path" "$pool"
      continue
    fi
    if [ ! -f "$lock_path" ] || [ -L "$lock_path" ]; then
      printf '0\t%s\t%s\tINVALID\t-\tstamp has no liveness lock\n' \
        "$path" "$pool"
      invalid_count=$((invalid_count + 1))
      continue
    fi
    if [ ! -f "$stamp" ] || [ -L "$stamp" ]; then
      printf '0\t%s\t%s\tINVALID\t-\tstamp is not a regular non-symlink file\n' \
        "$path" "$pool"
      invalid_count=$((invalid_count + 1))
      continue
    fi
    if ! { exec {lock_fd}<>"$lock_path"; }; then
      printf '0\t%s\t%s\tINVALID\t-\tliveness lock could not be opened\n' \
        "$path" "$pool"
      invalid_count=$((invalid_count + 1))
      continue
    fi
    if ! flock -n -x "$lock_fd"; then
      printf '0\t%s\t%s\tLIVE\t-\texclusive liveness lock refused\n' \
        "$path" "$pool"
      exec {lock_fd}>&-
      continue
    fi
    mount_status=0
    candidate_contains_mount "$path" || mount_status=$?
    case "$mount_status" in
      0)
        printf '0\t%s\t%s\tINVALID\t-\tcandidate contains a mountpoint\n' \
          "$path" "$pool"
        invalid_count=$((invalid_count + 1))
        exec {lock_fd}>&-
        continue
        ;;
      1) ;;
      *)
        printf '0\t%s\t%s\tINVALID\t-\tkernel mount table could not be read\n' \
          "$path" "$pool"
        invalid_count=$((invalid_count + 1))
        exec {lock_fd}>&-
        continue
        ;;
    esac
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
      if ! { exec {dir_fd}<"$path"; }; then
        printf '0\t%s\t%s\tINVALID\t%s\tdirectory descriptor could not be opened\n' \
          "$path" "$pool" "$after_epoch"
        invalid_count=$((invalid_count + 1))
        reapable_count=$((reapable_count - 1))
        total_reapable_bytes=$((total_reapable_bytes - current_reclaimable))
        exec {lock_fd}>&-
        continue
      fi
      if ! dir_identity="$(stat -Lc '%d:%i' "$path")" \
        || ! fd_dir_identity="$(stat -Lc '%d:%i' "/proc/${BASHPID:-$$}/fd/$dir_fd")" \
        || ! stamp_identity="$(stat -Lc '%d:%i' "$stamp")" \
        || ! lock_identity="$(stat -Lc '%d:%i' "$lock_path")"; then
        printf '0\t%s\t%s\tINVALID\t%s\tinode identity capture failed\n' \
          "$path" "$pool" "$after_epoch"
        invalid_count=$((invalid_count + 1))
        reapable_count=$((reapable_count - 1))
        total_reapable_bytes=$((total_reapable_bytes - current_reclaimable))
        exec {dir_fd}>&-
        exec {lock_fd}>&-
        continue
      fi
      if [ "$dir_identity" != "$fd_dir_identity" ] \
        || ! tree_identity="$(reapable_tree_identity \
          "/proc/${BASHPID:-$$}/fd/$dir_fd/.")"; then
        printf '0\t%s\t%s\tINVALID\t%s\tdirectory identity capture failed\n' \
          "$path" "$pool" "$after_epoch"
        invalid_count=$((invalid_count + 1))
        reapable_count=$((reapable_count - 1))
        total_reapable_bytes=$((total_reapable_bytes - current_reclaimable))
        exec {dir_fd}>&-
        exec {lock_fd}>&-
        continue
      fi
      record="$pool"$'\t'"$path"$'\t'"$stamp_values"$'\t'"$dir_identity"$'\t'"$stamp_identity"$'\t'"$lock_identity"$'\t'"$tree_identity"
      REAPABLE_PATHS+=("$path")
      REAPABLE_POOLS+=("$pool")
      REAPABLE_AFTER_EPOCHS+=("$after_epoch")
      REAPABLE_ALLOCATED_BYTES+=("$current_allocated")
      REAPABLE_RECLAIMABLE_BYTES+=("$current_reclaimable")
      REAPABLE_SHARED_BYTES+=("$current_shared")
      REAPABLE_STAMP_VALUES+=("$stamp_values")
      REAPABLE_DIR_IDENTITIES+=("$dir_identity")
      REAPABLE_STAMP_IDENTITIES+=("$stamp_identity")
      REAPABLE_LOCK_IDENTITIES+=("$lock_identity")
      REAPABLE_LOCK_FDS+=("$lock_fd")
      REAPABLE_DIR_FDS+=("$dir_fd")
      REAPABLE_TREE_IDENTITIES+=("$tree_identity")
      REAPABLE_RECORDS+=("$record")
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$kb" "$path" "$pool" "$state" "$after_epoch" "$reason"
    if [ "$state" != REAPABLE ]; then
      exec {lock_fd}>&-
    fi
    done <<<"$candidate_rows"
  fi
  if [ "$reapable_count" -gt 0 ]; then
    REAPABLE_CONFIRMATION_SHA256="$(printf '%s\0' "${REAPABLE_RECORDS[@]}" | sha256sum)"
    REAPABLE_CONFIRMATION_SHA256="${REAPABLE_CONFIRMATION_SHA256%% *}"
  else
    REAPABLE_CONFIRMATION_SHA256="-"
  fi
  REAPABLE_INVALID_COUNT="$invalid_count"
  printf 'REAPABLE_COUNT\t%s\n' "$reapable_count"
  printf 'REAPABLE_BYTES\t%s\n' "$total_reapable_bytes"
  printf 'INVALID_STAMP_COUNT\t%s\n' "$invalid_count"
  printf 'REAPER_CONFIRMATION_SHA256\t%s\n' "$REAPABLE_CONFIRMATION_SHA256"
  printf 'REAPER_POLICY\tstrict-stamp+child-lock+tree-identity; apply=parent-lease+descriptor-pinned; never age-only; default report-only\n'
}

revalidate_reapable_candidate() {
  local index="$1" path pool lock_fd dir_fd parent_fd parent_lock_fd base
  local pinned_dir pinned_candidate stamp lock_path stamp_values measurement
  local allocated_bytes reclaimable_bytes shared_bytes now_epoch tree_identity
  local dir_identity path_identity parent_identity path_parent_identity
  local stamp_identity lock_identity fd_lock_identity mount_status
  path="${REAPABLE_PATHS[$index]}"
  pool="${REAPABLE_POOLS[$index]}"
  lock_fd="${REAPABLE_LOCK_FDS[$index]}"
  dir_fd="${REAPABLE_DIR_FDS[$index]}"
  parent_fd="$(reaper_parent_dir_fd_for_pool "$pool")" || return 1
  parent_lock_fd="$(reaper_parent_lock_fd_for_pool "$pool")" || return 1
  base="${path##*/}"
  pinned_dir="/proc/${BASHPID:-$$}/fd/$dir_fd/."
  pinned_candidate="/proc/${BASHPID:-$$}/fd/$parent_fd/$base"
  stamp="$pinned_dir/REAPABLE-AFTER"
  lock_path="$pinned_dir/.fgdb-reaper.lock"
  candidate_is_authorized "$pool" "$path" || return 1
  mount_status=0
  candidate_contains_mount "$path" || mount_status=$?
  [ "$mount_status" -eq 1 ] || return 1
  reapable_lock_fd_is_open "$lock_fd" || return 1
  reapable_lock_fd_is_open "$dir_fd" || return 1
  reapable_lock_fd_is_open "$parent_fd" || return 1
  reapable_lock_fd_is_open "$parent_lock_fd" || return 1
  flock -n -x "$parent_lock_fd" || return 1
  flock -n -x "$lock_fd" || return 1
  [ -f "$stamp" ] && [ ! -L "$stamp" ] || return 1
  [ -f "$lock_path" ] && [ ! -L "$lock_path" ] || return 1
  dir_identity="$(stat -Lc '%d:%i' "$pinned_dir")" || return 1
  path_identity="$(stat -Lc '%d:%i' "$pinned_candidate")" || return 1
  parent_identity="$(stat -Lc '%d:%i' "/proc/${BASHPID:-$$}/fd/$parent_fd")" \
    || return 1
  path_parent_identity="$(stat -Lc '%d:%i' "${path%/*}")" || return 1
  stamp_identity="$(stat -Lc '%d:%i' "$stamp")" || return 1
  lock_identity="$(stat -Lc '%d:%i' "$lock_path")" || return 1
  fd_lock_identity="$(stat -Lc '%d:%i' "/proc/${BASHPID:-$$}/fd/$lock_fd")" || return 1
  [ "$dir_identity" = "${REAPABLE_DIR_IDENTITIES[$index]}" ] || return 1
  [ "$path_identity" = "$dir_identity" ] || return 1
  [ "$path_parent_identity" = "$parent_identity" ] || return 1
  [ "$stamp_identity" = "${REAPABLE_STAMP_IDENTITIES[$index]}" ] || return 1
  [ "$lock_identity" = "${REAPABLE_LOCK_IDENTITIES[$index]}" ] || return 1
  [ "$fd_lock_identity" = "$lock_identity" ] || return 1
  stamp_values="$(read_reapable_stamp "$stamp" "$pool")" || return 1
  [ "$stamp_values" = "${REAPABLE_STAMP_VALUES[$index]}" ] || return 1
  now_epoch="$(date -u +%s)" || return 1
  [ "$now_epoch" -ge "${REAPABLE_AFTER_EPOCHS[$index]}" ] || return 1
  measurement="$(reapable_measure_allocated_bytes "$pinned_dir")" || return 1
  IFS=$'\t' read -r allocated_bytes reclaimable_bytes shared_bytes \
    <<<"$measurement"
  [ "$allocated_bytes" = "${REAPABLE_ALLOCATED_BYTES[$index]}" ] || return 1
  [ "$reclaimable_bytes" = "${REAPABLE_RECLAIMABLE_BYTES[$index]}" ] || return 1
  [ "$shared_bytes" = "${REAPABLE_SHARED_BYTES[$index]}" ] || return 1
  tree_identity="$(reapable_tree_identity "$pinned_dir")" || return 1
  [ "$tree_identity" = "${REAPABLE_TREE_IDENTITIES[$index]}" ]
}

reap_confirmed_candidates() {
  local expected_sha256="$1" index path pool lock_fd dir_fd parent_fd base
  local pinned_dir pinned_candidate path_identity dir_identity reaped_count=0
  local reaped_reclaimable_bytes=0
  [ "${#REAPABLE_PATHS[@]}" -gt 0 ] || {
    printf 'error: there are no REAPABLE candidates; refusing --reap\n' >&2
    return 65
  }
  [ "$expected_sha256" = "$REAPABLE_CONFIRMATION_SHA256" ] || {
    printf 'error: confirmation mismatch; run the default report and review the current REAPABLE set\n' >&2
    return 65
  }
  if [ "$REAPABLE_INVALID_COUNT" -gt 0 ]; then
    printf 'REAPER_SKIPPED_INVALID\t%s fail-closed candidates are outside this confirmation\n' \
      "$REAPABLE_INVALID_COUNT"
  fi
  for index in "${!REAPABLE_PATHS[@]}"; do
    if ! revalidate_reapable_candidate "$index"; then
      printf 'error: candidate changed after report; refusing all deletion: %s\n' \
        "${REAPABLE_PATHS[$index]}" >&2
      return 65
    fi
  done
  printf 'REAPER_APPLY\tconfirmation accepted; deleting exactly %s locked candidates\n' \
    "${#REAPABLE_PATHS[@]}"
  for index in "${!REAPABLE_PATHS[@]}"; do
    path="${REAPABLE_PATHS[$index]}"
    pool="${REAPABLE_POOLS[$index]}"
    lock_fd="${REAPABLE_LOCK_FDS[$index]}"
    dir_fd="${REAPABLE_DIR_FDS[$index]}"
    parent_fd="$(reaper_parent_dir_fd_for_pool "$pool")" || return 65
    base="${path##*/}"
    pinned_dir="/proc/${BASHPID:-$$}/fd/$dir_fd/."
    pinned_candidate="/proc/${BASHPID:-$$}/fd/$parent_fd/$base"
    if ! revalidate_reapable_candidate "$index"; then
      printf 'error: candidate changed immediately before traversal; stopped at: %s\n' \
        "$path" >&2
      return 65
    fi
    # The pool-wide parent lease is the cooperative stop-the-world boundary.
    # The pinned directory descriptor prevents the recursive walk from ever
    # re-resolving a swapped root name. Static mountpoints have been refused;
    # -xdev is an additional boundary, not the mount proof. A process with
    # CAP_SYS_ADMIN or one that ignores the lifecycle lease is outside this
    # operator's stated threat boundary; this code does not claim otherwise.
    if ! find -H "$pinned_dir" -xdev -mindepth 1 -depth -delete; then
      printf 'error: deletion was incomplete; the remaining path is fail-closed: %s\n' \
        "$path" >&2
      return 74
    fi
    path_identity="$(stat -Lc '%d:%i' "$pinned_candidate")" || {
      printf 'error: pinned candidate name disappeared after child traversal: %s\n' \
        "$path" >&2
      return 74
    }
    dir_identity="$(stat -Lc '%d:%i' "/proc/${BASHPID:-$$}/fd/$dir_fd")" || {
      printf 'error: pinned candidate descriptor became unreadable: %s\n' \
        "$path" >&2
      return 74
    }
    [ "$path_identity" = "$dir_identity" ] || {
      printf 'error: candidate name changed after child traversal; refusing root removal: %s\n' \
        "$path" >&2
      return 74
    }
    if ! rmdir -- "$pinned_candidate"; then
      printf 'error: candidate root removal failed; the remaining path is fail-closed: %s\n' \
        "$path" >&2
      return 74
    fi
    printf 'REAPED\t%s\t%s\t%s\n' \
      "$path" "$pool" "${REAPABLE_RECLAIMABLE_BYTES[$index]}"
    reaped_count=$((reaped_count + 1))
    reaped_reclaimable_bytes=$((reaped_reclaimable_bytes + REAPABLE_RECLAIMABLE_BYTES[index]))
    exec {dir_fd}>&-
    REAPABLE_DIR_FDS[$index]=""
    exec {lock_fd}>&-
    REAPABLE_LOCK_FDS[$index]=""
  done
  printf 'REAPED_COUNT\t%s\n' "$reaped_count"
  printf 'REAPED_ELIGIBLE_BYTES\t%s\n' "$reaped_reclaimable_bytes"
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
if [ "$mode" = reap ]; then
  if ! acquire_reaper_parent_locks; then
    printf 'error: a producer holds a parent-namespace lease, or the namespace could not be pinned; refusing --reap\n' >&2
    exit 75
  fi
fi
report_reapable_pools
if [ "$mode" = reap ]; then
  reap_confirmed_candidates "$confirmation_sha256"
  release_reapable_locks
  release_reaper_parent_locks
else
  release_reapable_locks
fi
