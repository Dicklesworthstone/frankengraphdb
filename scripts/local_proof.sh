#!/usr/bin/env bash
# Capture one exact-tree local `scripts/check.sh` run as immutable evidence.
#
# This wrapper does not reinterpret product failures. `check.sh`'s exit code is
# authoritative when the tree remains stable; any HEAD, committed-tree, or
# worktree movement voids the run. Output directories are never removed or
# overwritten.

set -uo pipefail

usage() {
  cat <<'USAGE'
Usage: bash scripts/local_proof.sh [--output DIR]

Run the repository-authoritative `bash scripts/check.sh`, preserve stdout and
stderr separately, and write a checksum-bound format-v2 manifest naming the
exact commit, source tree, and tracked check-script blob. DIR must be outside
the repository and must not exist.
USAGE
}

fail() {
  printf 'local-proof: %s\n' "$*" >&2
  exit 2
}

output=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output)
      [ "$#" -ge 2 ] || fail "--output requires a directory"
      output="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) fail "unknown argument: $1" ;;
  esac
done

command -v git >/dev/null 2>&1 || fail "git is required"
root="$(git rev-parse --show-toplevel 2>/dev/null)" || fail "not inside a Git worktree"
cd "$root"
[ -f scripts/check.sh ] || fail "scripts/check.sh is missing"

before_commit="$(git rev-parse --verify HEAD)"
before_tree="$(git rev-parse --verify "${before_commit}^{tree}")"
check_script_path="scripts/check.sh"
check_script_blob="$(git rev-parse --verify "${before_commit}:${check_script_path}")" \
  || fail "$check_script_path is not tracked by $before_commit"
short_sha="$(printf '%s' "$before_commit" | cut -c1-12)"
ref_name="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || printf 'detached')"
before_status="$(git status --porcelain=v1 --untracked-files=all)"
[ -z "$before_status" ] || {
  printf '%s\n' "$before_status" >&2
  fail "proof runs require a clean worktree"
}

if [ -z "$output" ]; then
  output="${TMPDIR:-/tmp}/fgdb-local-proof-${short_sha}-$$"
fi
case "$output" in /*) ;; *) output="$PWD/$output" ;; esac
parent="$(dirname "$output")"
[ -d "$parent" ] || fail "output parent does not exist: $parent"
parent="$(cd "$parent" && pwd -P)"
output="$parent/$(basename "$output")"
case "$output" in "$root"|"$root"/*) fail "output must be outside the repository worktree" ;; esac
[ ! -e "$output" ] || fail "output already exists: $output"
mkdir "$output"

printf '%s\n' "$before_commit" > "$output/commit-before.txt"
printf '%s\n' "$before_tree" > "$output/tree-before.txt"
printf '%s\n' "$check_script_blob" > "$output/check-script-blob.txt"
printf '%s' "$before_status" > "$output/status-before.txt"
printf 'bash scripts/check.sh\n' > "$output/command.txt"
{
  printf 'git=%s\n' "$(git --version 2>&1)"
  printf 'uname=%s\n' "$(uname -a 2>&1)"
  if command -v rustc >/dev/null 2>&1; then printf 'rustc=%s\n' "$(rustc --version 2>&1)"; else printf 'rustc=unavailable\n'; fi
  if command -v cargo >/dev/null 2>&1; then printf 'cargo=%s\n' "$(cargo --version 2>&1)"; else printf 'cargo=unavailable\n'; fi
  if command -v br >/dev/null 2>&1; then printf 'br=%s\n' "$(br --version 2>&1)"; else printf 'br=unavailable\n'; fi
} > "$output/tools.txt"

started_utc="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
set +e
bash scripts/check.sh > "$output/check.stdout.log" 2> "$output/check.stderr.log"
check_exit=$?
set -e
finished_utc="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

after_commit="$(git rev-parse --verify HEAD)"
after_tree="$(git rev-parse --verify "${after_commit}^{tree}")"
after_status="$(git status --porcelain=v1 --untracked-files=all)"
printf '%s\n' "$after_commit" > "$output/commit-after.txt"
printf '%s\n' "$after_tree" > "$output/tree-after.txt"
printf '%s' "$after_status" > "$output/status-after.txt"
printf '%s\n' "$check_exit" > "$output/check-exit.txt"

stable=true
if [ "$before_commit" != "$after_commit" ] \
  || [ "$before_tree" != "$after_tree" ] \
  || [ "$before_status" != "$after_status" ]; then
  stable=false
fi

if [ "$stable" != true ]; then
  verdict=void
  wrapper_exit=125
elif [ "$check_exit" -eq 0 ]; then
  verdict=pass
  wrapper_exit=0
else
  verdict=red
  wrapper_exit="$check_exit"
  if [ "$wrapper_exit" -eq 0 ]; then wrapper_exit=1; fi
fi

{
  printf 'format_version=2\n'
  printf 'repository=%s\n' "$(basename "$root")"
  printf 'commit=%s\n' "$before_commit"
  printf 'tree=%s\n' "$before_tree"
  printf 'ref=%s\n' "$ref_name"
  printf 'check_script_path=%s\n' "$check_script_path"
  printf 'check_script_blob=%s\n' "$check_script_blob"
  printf 'started_utc=%s\n' "$started_utc"
  printf 'finished_utc=%s\n' "$finished_utc"
  printf 'check_exit=%s\n' "$check_exit"
  printf 'tree_stable=%s\n' "$stable"
  printf 'verdict=%s\n' "$verdict"
  printf 'authority=check.sh exit code on the exact stable tree; void when HEAD, committed tree, or worktree state moves\n'
} > "$output/manifest.txt"

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1"
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1"
  else
    fail "sha256sum or shasum is required"
  fi
}
(
  cd "$output"
  : > SHA256SUMS
  while IFS= read -r file; do
    hash_file "$file" >> SHA256SUMS
  done < <(find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort)
)

case "$verdict" in
  pass) printf 'LOCAL_PROOF_PASS\n' ;;
  red) printf 'LOCAL_PROOF_RED\n' ;;
  void) printf 'LOCAL_PROOF_VOID\n' ;;
esac
printf 'path=%s\n' "$output"
printf 'commit=%s\n' "$before_commit"
printf 'tree=%s\n' "$before_tree"
printf 'check_script_blob=%s\n' "$check_script_blob"
printf 'check_exit=%s\n' "$check_exit"
printf 'tree_stable=%s\n' "$stable"
printf 'verdict=%s\n' "$verdict"
exit "$wrapper_exit"
