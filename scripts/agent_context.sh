#!/usr/bin/env bash
# Export an exact-SHA, credential-free advisory context capsule for local agents.
#
# This is intentionally local and read-only. It neither runs product gates nor
# turns its own success into a product verdict. `bash scripts/check.sh` remains
# authoritative. The output directory must not already exist and is never
# removed or overwritten by this script.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: bash scripts/agent_context.sh [options]

Options:
  --output DIR       Write the capsule to DIR. DIR must not already exist.
                     Default: ${TMPDIR:-/tmp}/fgdb-agent-context-<sha12>-<pid>
  --allow-dirty      Export a dirty tree. Tracked changes are captured as a
                     binary patch; untracked file names are listed, but their
                     contents are deliberately excluded.
  --no-beads         Do not export Beads JSONL or run read-only `br` queries.
  --require-br       Fail when the repository has Beads state but `br` is not
                     available. Implies the default Beads export behavior.
  --recent N         Number of recent commits to export (default: 100).
  -h, --help         Show this help.

Format v2 contains an exact-HEAD Git bundle, a deterministic tracked-source
archive, the exact source-tree object id, recent history, tracked-file inventory,
worktree state, optional Beads views, a strict manifest, and SHA-256 checksums.
It contains no .git directory, remote credentials, or untracked file contents.
USAGE
}

fail() {
  printf 'agent-context: %s\n' "$*" >&2
  exit 1
}

allow_dirty=0
export_beads=1
require_br=0
recent=100
output=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --output)
      [ "$#" -ge 2 ] || fail "--output requires a directory"
      output="$2"
      shift 2
      ;;
    --allow-dirty)
      allow_dirty=1
      shift
      ;;
    --no-beads)
      export_beads=0
      require_br=0
      shift
      ;;
    --require-br)
      export_beads=1
      require_br=1
      shift
      ;;
    --recent)
      [ "$#" -ge 2 ] || fail "--recent requires a positive integer"
      recent="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *) fail "unknown argument: $1" ;;
  esac
done

case "$recent" in
  ''|*[!0-9]*) fail "--recent must be a positive integer" ;;
  0) fail "--recent must be greater than zero" ;;
esac

command -v git >/dev/null 2>&1 || fail "git is required"
command -v gzip >/dev/null 2>&1 || fail "gzip is required"
root="$(git rev-parse --show-toplevel 2>/dev/null)" || fail "not inside a Git worktree"
cd "$root"

head_sha="$(git rev-parse --verify HEAD)"
head_tree="$(git rev-parse --verify "${head_sha}^{tree}")"
short_sha="$(printf '%s' "$head_sha" | cut -c1-12)"
ref_name="$(git symbolic-ref --quiet --short HEAD 2>/dev/null || printf 'detached')"
repository="$(basename "$root")"
initial_status="$(git status --porcelain=v1 --untracked-files=all)"

if [ -n "$initial_status" ] && [ "$allow_dirty" -ne 1 ]; then
  printf '%s\n' "$initial_status" >&2
  fail "worktree is dirty; commit/stash it or pass --allow-dirty"
fi

if [ -z "$output" ]; then
  output="${TMPDIR:-/tmp}/fgdb-agent-context-${short_sha}-$$"
fi
case "$output" in
  /*) ;;
  *) output="$PWD/$output" ;;
esac

output_parent="$(dirname "$output")"
[ -d "$output_parent" ] || fail "output parent does not exist: $output_parent"
output_parent="$(cd "$output_parent" && pwd -P)"
output="$output_parent/$(basename "$output")"
case "$output" in
  "$root"|"$root"/*) fail "output must be outside the repository worktree" ;;
esac
[ ! -e "$output" ] || fail "output already exists: $output"
mkdir "$output"

git status --porcelain=v1 --untracked-files=all > "$output/git-status.txt"
git log -n "$recent" --date=iso-strict \
  --format='%H%x09%aI%x09%an%x09%s' "$head_sha" \
  > "$output/recent-commits.tsv"
git ls-tree -r --name-only "$head_sha" > "$output/tracked-files.txt"

git bundle create "$output/repository.bundle" HEAD
git bundle verify "$output/repository.bundle" \
  > "$output/git-bundle-verify.txt" 2>&1
git archive --format=tar "$head_sha" | gzip -n > "$output/tracked-source.tar.gz"

if [ -n "$initial_status" ]; then
  git diff --binary HEAD > "$output/worktree.patch"
  git ls-files --others --exclude-standard > "$output/untracked-files.txt"
fi

beads_mode="disabled"
if [ "$export_beads" -eq 1 ]; then
  if [ ! -d .beads ]; then
    beads_mode="absent"
  else
    have_jsonl=0
    have_br=0
    if [ -f .beads/issues.jsonl ]; then
      cp .beads/issues.jsonl "$output/issues.jsonl"
      have_jsonl=1
    fi
    if command -v br >/dev/null 2>&1; then
      br --version > "$output/br-version.txt"
      RUST_LOG=error br ready --json > "$output/br-ready.json"
      RUST_LOG=error br list --status open --json > "$output/br-open.json"
      RUST_LOG=error br list --status in_progress --json > "$output/br-in-progress.json"
      RUST_LOG=error br blocked --json > "$output/br-blocked.json"
      RUST_LOG=error br stats --json > "$output/br-stats.json"
      have_br=1
    elif [ "$require_br" -eq 1 ]; then
      fail "--require-br was set, but br is unavailable"
    fi

    if [ "$have_jsonl" -eq 1 ] && [ "$have_br" -eq 1 ]; then
      beads_mode="br+jsonl"
    elif [ "$have_jsonl" -eq 1 ]; then
      beads_mode="jsonl-only"
    elif [ "$have_br" -eq 1 ]; then
      beads_mode="br-only"
    else
      beads_mode="unavailable"
    fi
  fi
fi

final_head="$(git rev-parse --verify HEAD)"
[ "$final_head" = "$head_sha" ] || fail "HEAD moved during export: $head_sha -> $final_head"
final_tree="$(git rev-parse --verify "${final_head}^{tree}")"
[ "$final_tree" = "$head_tree" ] || fail "source tree moved during export"
cmp --silent "$output/git-status.txt" \
  <(git status --porcelain=v1 --untracked-files=all) \
  || fail "worktree state changed during export"
if [ -n "$initial_status" ]; then
  git status --porcelain=v1 --untracked-files=all \
    > "$output/git-status-stability-proof.txt"
  git diff --binary HEAD > "$output/worktree-stability-proof.patch"
  cmp --silent "$output/worktree.patch" "$output/worktree-stability-proof.patch" \
    || fail "tracked worktree content changed during export"
fi

{
  printf 'format_version=2\n'
  printf 'repository=%s\n' "$repository"
  printf 'commit=%s\n' "$head_sha"
  printf 'tree=%s\n' "$head_tree"
  printf 'ref=%s\n' "$ref_name"
  printf 'bundle_ref=HEAD\n'
  printf 'dirty=%s\n' "$([ -n "$initial_status" ] && printf true || printf false)"
  printf 'beads=%s\n' "$beads_mode"
  printf 'recent_commit_count=%s\n' "$recent"
  printf 'tracked_source=tracked-source.tar.gz\n'
  printf 'history=repository.bundle\n'
  printf 'untracked_contents=excluded\n'
  printf 'authority=advisory-only; git, the live Beads database, and bash scripts/check.sh remain canonical\n'
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

printf 'AGENT_CONTEXT_OK\n'
printf 'path=%s\n' "$output"
printf 'commit=%s\n' "$head_sha"
printf 'tree=%s\n' "$head_tree"
printf 'dirty=%s\n' "$([ -n "$initial_status" ] && printf true || printf false)"
printf 'beads=%s\n' "$beads_mode"
