#!/usr/bin/env bash
# Materialize a verified agent-context capsule as a credential-free checkout.
#
# The verifier runs before any destination is created. The destination and its
# verifier scratch must not already exist and are retained on success or failure;
# this script never removes or overwrites user data.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: bash scripts/agent_context_checkout.sh [options] CAPSULE_DIR DEST

Options:
  --apply-dirty     Apply the capsule's tracked worktree.patch after checkout.
                    Untracked file contents were never exported and cannot be
                    reconstructed; their names remain in untracked-files.txt.
  --verify-scratch DIR
                    Retain deep-verifier work in DIR. DIR must not exist.
  -h, --help        Show this help.

The resulting checkout is detached at the exact bundled commit and has no Git
remotes. Success does not turn the advisory capsule into a product verdict.
USAGE
}

fail() {
  printf 'agent-context-checkout: %s\n' "$*" >&2
  exit 1
}

apply_dirty=0
verify_scratch=""
positional=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --apply-dirty)
      apply_dirty=1
      shift
      ;;
    --verify-scratch)
      [ "$#" -ge 2 ] || fail "--verify-scratch requires a directory"
      verify_scratch="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*) fail "unknown argument: $1" ;;
    *) positional+=("$1"); shift ;;
  esac
done
[ "${#positional[@]}" -eq 2 ] || { usage >&2; exit 2; }

capsule="${positional[0]}"
destination="${positional[1]}"
[ -d "$capsule" ] || fail "not a capsule directory: $capsule"
capsule="$(cd "$capsule" && pwd -P)"
manifest="$capsule/manifest.txt"
[ -f "$manifest" ] || fail "capsule lacks manifest.txt"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
verifier="$root/scripts/agent_context_verify.sh"
[ -f "$verifier" ] || fail "missing verifier: $verifier"
command -v git >/dev/null 2>&1 || fail "git is required"

manifest_value() {
  local key="$1"
  awk -F= -v key="$key" '$1 == key { count += 1; if (count == 1) { sub(/^[^=]*=/, ""); value=$0 } } END { if (count != 1) exit 1; print value }' \
    "$manifest"
}
commit="$(manifest_value commit)" || fail "manifest lacks a unique commit"
dirty="$(manifest_value dirty)" || fail "manifest lacks a unique dirty declaration"
case "$dirty" in true|false) ;; *) fail "invalid dirty declaration: $dirty" ;; esac
if [ "$apply_dirty" -eq 1 ] && [ "$dirty" != true ]; then
  fail "--apply-dirty requires a dirty capsule"
fi

case "$destination" in /*) ;; *) destination="$PWD/$destination" ;; esac
destination_parent="$(dirname "$destination")"
[ -d "$destination_parent" ] || fail "destination parent does not exist: $destination_parent"
destination_parent="$(cd "$destination_parent" && pwd -P)"
destination="$destination_parent/$(basename "$destination")"
case "$destination" in
  "$capsule"|"$capsule"/*) fail "destination must be outside the capsule" ;;
esac
[ ! -e "$destination" ] || fail "destination already exists: $destination"

if [ -z "$verify_scratch" ]; then
  verify_scratch="${TMPDIR:-/tmp}/fgdb-agent-context-checkout-verify-$(printf '%s' "$commit" | cut -c1-12)-$$"
fi
case "$verify_scratch" in /*) ;; *) verify_scratch="$PWD/$verify_scratch" ;; esac
verify_parent="$(dirname "$verify_scratch")"
[ -d "$verify_parent" ] || fail "verify-scratch parent does not exist: $verify_parent"
verify_parent="$(cd "$verify_parent" && pwd -P)"
verify_scratch="$verify_parent/$(basename "$verify_scratch")"
case "$verify_scratch" in
  "$capsule"|"$capsule"/*) fail "verify scratch must be outside the capsule" ;;
  "$destination"|"$destination"/*) fail "verify scratch must be outside the destination" ;;
esac
[ ! -e "$verify_scratch" ] || fail "verify scratch already exists: $verify_scratch"

bash "$verifier" --scratch "$verify_scratch" "$capsule" >/dev/null

mkdir "$destination"
git -C "$destination" init -q
git -C "$destination" fetch -q "$capsule/repository.bundle" HEAD
git -C "$destination" checkout -q --detach FETCH_HEAD
actual_commit="$(git -C "$destination" rev-parse --verify HEAD)"
[ "$actual_commit" = "$commit" ] \
  || fail "checkout landed at $actual_commit instead of bundled commit $commit"
[ -z "$(git -C "$destination" remote)" ] \
  || fail "credential-free checkout unexpectedly has a Git remote"
[ -z "$(git -C "$destination" status --porcelain=v1 --untracked-files=all)" ] \
  || fail "fresh bundled checkout is not clean"

if [ "$apply_dirty" -eq 1 ]; then
  git -C "$destination" apply --binary "$capsule/worktree.patch"
  git -C "$destination" diff --binary HEAD > "$verify_scratch/applied-worktree.patch"
  cmp --silent "$capsule/worktree.patch" "$verify_scratch/applied-worktree.patch" \
    || fail "applied tracked worktree differs from the capsule patch"
fi

printf 'AGENT_CONTEXT_CHECKOUT_OK\n'
printf 'path=%s\n' "$destination"
printf 'verify_scratch=%s\n' "$verify_scratch"
printf 'commit=%s\n' "$commit"
printf 'dirty_applied=%s\n' "$([ "$apply_dirty" -eq 1 ] && printf true || printf false)"
if [ "$dirty" = true ] && [ -s "$capsule/untracked-files.txt" ]; then
  printf 'untracked_names=%s\n' "$capsule/untracked-files.txt"
fi
