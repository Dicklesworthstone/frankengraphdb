#!/usr/bin/env bash
# Independently verify a local advisory agent-context capsule.
#
# Verification is deep by default: after strict manifest/inventory/checksum
# checks, the bundle is imported into an isolated retained scratch repository.
# The source archive, tree id, tracked-file inventory, and recent history are
# then recomputed from the bundled commit. Nothing in the capsule is trusted
# merely because its checksum file was recomputed alongside it.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: bash scripts/agent_context_verify.sh [--scratch DIR] CAPSULE_DIR

Options:
  --scratch DIR  Retain verifier work in DIR. DIR must not exist.
                 Default: ${TMPDIR:-/tmp}/fgdb-agent-context-verify-<sha12>-<pid>
  -h, --help     Show this help.

Success proves that the capsule's tracked source and inventories are derived
from the exact commit carried by its Git bundle, in addition to strict format
and checksum consistency. It remains advisory and is not a product-gate verdict.
USAGE
}

fail() {
  printf 'agent-context-verify: %s\n' "$*" >&2
  exit 1
}

scratch=""
capsule_arg=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --scratch)
      [ "$#" -ge 2 ] || fail "--scratch requires a directory"
      scratch="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*) fail "unknown argument: $1" ;;
    *)
      [ -z "$capsule_arg" ] || fail "only one capsule directory may be supplied"
      capsule_arg="$1"
      shift
      ;;
  esac
done
[ -n "$capsule_arg" ] || { usage >&2; exit 2; }

for command in git tar gzip awk sed find cmp; do
  command -v "$command" >/dev/null 2>&1 || fail "$command is required"
done

capsule="$capsule_arg"
[ -d "$capsule" ] || fail "not a directory: $capsule"
capsule="$(cd "$capsule" && pwd -P)"

required_base=(
  manifest.txt SHA256SUMS repository.bundle git-bundle-verify.txt
  tracked-source.tar.gz tracked-files.txt recent-commits.tsv git-status.txt
)
for file in "${required_base[@]}"; do
  [ -f "$capsule/$file" ] || fail "missing required file: $file"
done
symlink="$(find "$capsule" -type l -print -quit)"
[ -z "$symlink" ] || fail "capsule contains a symlink: $symlink"

manifest_value() {
  local key="$1"
  awk -F= -v key="$key" '
    $1 == key {
      count += 1
      if (count == 1) { sub(/^[^=]*=/, ""); value=$0 }
    }
    END { if (count != 1) exit 1; print value }
  ' "$capsule/manifest.txt"
}

format_version="$(manifest_value format_version)" \
  || fail "manifest must contain exactly one format_version"
case "$format_version" in 1|2) ;; *) fail "unsupported format_version: $format_version" ;; esac

expected_manifest_keys_v1=(
  authority beads commit dirty format_version history recent_commit_count ref
  repository tracked_source untracked_contents
)
expected_manifest_keys_v2=(
  authority beads bundle_ref commit dirty format_version history recent_commit_count
  ref repository tracked_source tree untracked_contents
)
actual_manifest_keys="$(awk -F= 'NF >= 2 && length($1) > 0 { print $1; next } { exit 2 }' \
  "$capsule/manifest.txt" | LC_ALL=C sort)" \
  || fail "manifest contains a blank or malformed line"
if [ "$format_version" = 1 ]; then
  expected_manifest_keys="$(printf '%s\n' "${expected_manifest_keys_v1[@]}" | LC_ALL=C sort)"
else
  expected_manifest_keys="$(printf '%s\n' "${expected_manifest_keys_v2[@]}" | LC_ALL=C sort)"
fi
[ "$actual_manifest_keys" = "$expected_manifest_keys" ] \
  || fail "manifest key inventory is not exact for format v$format_version"

commit="$(manifest_value commit)" || fail "manifest lacks a unique commit"
printf '%s\n' "$commit" | grep -Eq '^[0-9a-f]{40}$' \
  || fail "manifest commit is not a lowercase 40-hex object id: $commit"
short_commit="$(printf '%s' "$commit" | cut -c1-12)"
if [ "$format_version" = 2 ]; then
  tree="$(manifest_value tree)" || fail "manifest lacks a unique tree"
  printf '%s\n' "$tree" | grep -Eq '^[0-9a-f]{40}$' \
    || fail "manifest tree is not a lowercase 40-hex object id: $tree"
  [ "$(manifest_value bundle_ref)" = HEAD ] || fail "format v2 bundle_ref must be HEAD"
else
  tree=""
fi
[ "$(manifest_value tracked_source)" = tracked-source.tar.gz ] \
  || fail "tracked_source does not name the canonical archive"
[ "$(manifest_value history)" = repository.bundle ] \
  || fail "history does not name the canonical bundle"
[ "$(manifest_value untracked_contents)" = excluded ] \
  || fail "untracked_contents must remain excluded"

dirty="$(manifest_value dirty)" || fail "manifest lacks dirty"
case "$dirty" in true|false) ;; *) fail "manifest dirty must be true or false" ;; esac
beads="$(manifest_value beads)" || fail "manifest lacks beads"
case "$beads" in
  br+jsonl|br-only|jsonl-only|absent|disabled|unavailable) ;;
  *) fail "unknown Beads mode: $beads" ;;
esac
recent="$(manifest_value recent_commit_count)" || fail "manifest lacks recent_commit_count"
case "$recent" in ''|*[!0-9]*|0) fail "recent_commit_count must be positive" ;; esac
authority="$(manifest_value authority)" || fail "manifest lacks authority"
case "$authority" in advisory-only*) ;; *) fail "manifest lost the advisory-only boundary" ;; esac

expected_files=(
  ./SHA256SUMS ./git-bundle-verify.txt ./git-status.txt ./manifest.txt
  ./recent-commits.tsv ./repository.bundle ./tracked-files.txt ./tracked-source.tar.gz
)
if [ "$dirty" = true ]; then
  expected_files+=(
    ./untracked-files.txt ./worktree.patch ./worktree-stability-proof.patch
  )
  if [ "$format_version" = 2 ]; then
    expected_files+=(./git-status-stability-proof.txt)
  fi
fi
case "$beads" in
  br+jsonl)
    expected_files+=(
      ./issues.jsonl ./br-version.txt ./br-ready.json ./br-open.json
      ./br-in-progress.json ./br-blocked.json ./br-stats.json
    )
    ;;
  br-only)
    expected_files+=(
      ./br-version.txt ./br-ready.json ./br-open.json
      ./br-in-progress.json ./br-blocked.json ./br-stats.json
    )
    ;;
  jsonl-only) expected_files+=(./issues.jsonl) ;;
  absent|disabled|unavailable) ;;
esac
expected_inventory="$(printf '%s\n' "${expected_files[@]}" | LC_ALL=C sort)"
actual_inventory="$(cd "$capsule" && find . -type f -print | LC_ALL=C sort)"
[ "$actual_inventory" = "$expected_inventory" ] \
  || fail "capsule regular-file inventory is not exact for dirty=$dirty beads=$beads"

declared_inventory="$(sed -E 's/^[0-9a-fA-F]{64}[[:space:]]+//' \
  "$capsule/SHA256SUMS" | LC_ALL=C sort)"
actual_without_sums="$(cd "$capsule" && find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort)"
[ "$declared_inventory" = "$actual_without_sums" ] \
  || fail "SHA256SUMS does not name the exact checksum-covered inventory"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$capsule" && sha256sum -c SHA256SUMS >/dev/null) \
    || fail "SHA-256 verification failed"
elif command -v shasum >/dev/null 2>&1; then
  (cd "$capsule" && shasum -a 256 -c SHA256SUMS >/dev/null) \
    || fail "SHA-256 verification failed"
else
  fail "sha256sum or shasum is required"
fi

bundle_heads="$(git bundle list-heads "$capsule/repository.bundle")" \
  || fail "repository.bundle cannot be parsed"
[ "$(printf '%s\n' "$bundle_heads" | wc -l | tr -d ' ')" = 1 ] \
  || fail "repository.bundle must advertise exactly one head"
printf '%s\n' "$bundle_heads" | grep -Eq "^${commit}[[:space:]]+HEAD$" \
  || fail "repository.bundle does not advertise exactly $commit as HEAD"

if [ -z "$scratch" ]; then
  scratch="${TMPDIR:-/tmp}/fgdb-agent-context-verify-${short_commit}-$$"
fi
case "$scratch" in /*) ;; *) scratch="$PWD/$scratch" ;; esac
scratch_parent="$(dirname "$scratch")"
[ -d "$scratch_parent" ] || fail "scratch parent does not exist: $scratch_parent"
scratch_parent="$(cd "$scratch_parent" && pwd -P)"
scratch="$scratch_parent/$(basename "$scratch")"
case "$scratch" in "$capsule"|"$capsule"/*) fail "scratch must be outside the capsule" ;; esac
[ ! -e "$scratch" ] || fail "scratch already exists: $scratch"
mkdir "$scratch"
mkdir "$scratch/repository"
git -C "$scratch/repository" init -q
git -C "$scratch/repository" fetch -q "$capsule/repository.bundle" HEAD
git -C "$scratch/repository" checkout -q --detach FETCH_HEAD
actual_commit="$(git -C "$scratch/repository" rev-parse --verify HEAD)"
[ "$actual_commit" = "$commit" ] \
  || fail "bundle checkout landed at $actual_commit instead of $commit"
actual_tree="$(git -C "$scratch/repository" rev-parse "${commit}^{tree}")"
if [ -n "$tree" ] && [ "$actual_tree" != "$tree" ]; then
  fail "manifest tree $tree disagrees with bundled commit tree $actual_tree"
fi

git -C "$scratch/repository" archive --format=tar "$commit" \
  > "$scratch/tracked-source.tar"
gzip -dc "$capsule/tracked-source.tar.gz" \
  | cmp --silent - "$scratch/tracked-source.tar" \
  || fail "tracked-source.tar.gz is not the exact archive of bundled commit $commit"
git -C "$scratch/repository" ls-tree -r --name-only "$commit" \
  > "$scratch/tracked-files.txt"
cmp --silent "$capsule/tracked-files.txt" "$scratch/tracked-files.txt" \
  || fail "tracked-files.txt is not derived from bundled commit $commit"
git -C "$scratch/repository" log -n "$recent" --date=iso-strict \
  --format='%H%x09%aI%x09%an%x09%s' "$commit" \
  > "$scratch/recent-commits.tsv"
cmp --silent "$capsule/recent-commits.tsv" "$scratch/recent-commits.tsv" \
  || fail "recent-commits.tsv is not derived from bundled commit $commit"

if [ "$dirty" = true ]; then
  grep -q '[^[:space:]]' "$capsule/git-status.txt" \
    || fail "dirty capsule has an empty git-status witness"
  if [ "$format_version" = 2 ]; then
    cmp --silent "$capsule/git-status.txt" "$capsule/git-status-stability-proof.txt" \
      || fail "dirty capsule's status changed during export"
  fi
  cmp --silent "$capsule/worktree.patch" "$capsule/worktree-stability-proof.patch" \
    || fail "dirty capsule's tracked patch changed during export"
  git -C "$scratch/repository" apply --check --binary "$capsule/worktree.patch" \
    || fail "dirty worktree.patch does not apply to bundled commit $commit"
  while IFS= read -r path || [ -n "$path" ]; do
    [ -n "$path" ] || continue
    case "/$path/" in
      /*/../*|/*/./*|//*) fail "unsafe untracked path: $path" ;;
    esac
    case "$path" in /*) fail "absolute untracked path: $path" ;; esac
  done < "$capsule/untracked-files.txt"
else
  if grep -q '[^[:space:]]' "$capsule/git-status.txt"; then
    fail "clean capsule has a nonempty git-status witness"
  fi
fi

printf 'AGENT_CONTEXT_VERIFIED\n'
printf 'path=%s\n' "$capsule"
printf 'scratch=%s\n' "$scratch"
printf 'commit=%s\n' "$commit"
printf 'tree=%s\n' "$actual_tree"
printf 'dirty=%s\n' "$dirty"
printf 'beads=%s\n' "$beads"
