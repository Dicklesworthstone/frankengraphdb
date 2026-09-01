#!/usr/bin/env bash
# Verify a local advisory agent-context capsule without trusting its producer.
#
# This validates the strict file inventory, SHA-256 checksums, exact commit in
# the Git bundle, archive hygiene, dirty-tree evidence shape, and Beads-mode
# declarations. It never extracts, removes, or overwrites capsule contents.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: bash scripts/agent_context_verify.sh CAPSULE_DIR

Verify an advisory capsule produced by scripts/agent_context.sh. Success proves
that the capsule is structurally complete and internally checksum-consistent;
it does not turn the capsule into a product verdict. `bash scripts/check.sh`
remains the authoritative repository gate.
USAGE
}

fail() {
  printf 'agent-context-verify: %s\n' "$*" >&2
  exit 1
}

[ "$#" -eq 1 ] || {
  usage >&2
  exit 2
}
case "$1" in
  -h|--help)
    usage
    exit 0
    ;;
esac

command -v git >/dev/null 2>&1 || fail "git is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

capsule="$1"
[ -d "$capsule" ] || fail "not a directory: $capsule"
capsule="$(cd "$capsule" && pwd -P)"

required=(
  manifest.txt
  SHA256SUMS
  repository.bundle
  git-bundle-verify.txt
  tracked-source.tar.gz
  tracked-files.txt
  recent-commits.tsv
  git-status.txt
)
for file in "${required[@]}"; do
  [ -f "$capsule/$file" ] || fail "missing required file: $file"
done

symlink="$(find "$capsule" -type l -print -quit)"
[ -z "$symlink" ] || fail "capsule contains a symlink: $symlink"

manifest_value() {
  local key="$1"
  awk -F= -v key="$key" '$1 == key { sub(/^[^=]*=/, ""); print; found=1; exit } END { if (!found) exit 1 }' \
    "$capsule/manifest.txt"
}

format_version="$(manifest_value format_version)" || fail "manifest lacks format_version"
[ "$format_version" = "1" ] || fail "unsupported format_version: $format_version"
commit="$(manifest_value commit)" || fail "manifest lacks commit"
printf '%s\n' "$commit" | grep -Eq '^[0-9a-f]{40}$' \
  || fail "manifest commit is not a lowercase 40-hex object id: $commit"
dirty="$(manifest_value dirty)" || fail "manifest lacks dirty"
case "$dirty" in true|false) ;; *) fail "manifest dirty must be true or false" ;; esac
beads="$(manifest_value beads)" || fail "manifest lacks beads"
authority="$(manifest_value authority)" || fail "manifest lacks authority"
case "$authority" in
  advisory-only*) ;;
  *) fail "manifest does not preserve the advisory-only authority boundary" ;;
esac

actual_files="$(
  cd "$capsule"
  find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort
)"
declared_files="$(sed -E 's/^[0-9a-fA-F]{64}[[:space:]]+//' "$capsule/SHA256SUMS" | LC_ALL=C sort)"
[ "$actual_files" = "$declared_files" ] || fail "SHA256SUMS does not name the exact regular-file inventory"

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
printf '%s\n' "$bundle_heads" | grep -Eq "^${commit}[[:space:]]" \
  || fail "repository.bundle does not advertise manifest commit $commit"

archive_member="$(tar -tzf "$capsule/tracked-source.tar.gz" | grep -E '(^|/)\.git(/|$)' | head -n 1 || true)"
[ -z "$archive_member" ] || fail "tracked source archive contains forbidden .git content: $archive_member"

case "$dirty" in
  true)
    [ -f "$capsule/worktree.patch" ] || fail "dirty capsule lacks worktree.patch"
    [ -f "$capsule/untracked-files.txt" ] || fail "dirty capsule lacks untracked-files.txt"
    [ -f "$capsule/worktree-stability-proof.patch" ] \
      || fail "dirty capsule lacks worktree stability proof"
    cmp --silent "$capsule/worktree.patch" "$capsule/worktree-stability-proof.patch" \
      || fail "dirty capsule's tracked patch changed during export"
    ;;
  false)
    [ ! -e "$capsule/worktree.patch" ] || fail "clean capsule unexpectedly contains worktree.patch"
    [ ! -e "$capsule/worktree-stability-proof.patch" ] \
      || fail "clean capsule unexpectedly contains a dirty-tree stability proof"
    ;;
esac

case "$beads" in
  br+jsonl)
    bead_files=(
      issues.jsonl
      br-version.txt
      br-ready.json
      br-open.json
      br-in-progress.json
      br-blocked.json
      br-stats.json
    )
    for file in "${bead_files[@]}"; do
      [ -f "$capsule/$file" ] || fail "br+jsonl capsule lacks $file"
    done
    ;;
  jsonl-only)
    [ -f "$capsule/issues.jsonl" ] || fail "jsonl-only capsule lacks issues.jsonl"
    ;;
  absent|disabled) ;;
  *) fail "unknown Beads mode: $beads" ;;
esac

printf 'AGENT_CONTEXT_VERIFIED\n'
printf 'path=%s\n' "$capsule"
printf 'commit=%s\n' "$commit"
printf 'dirty=%s\n' "$dirty"
printf 'beads=%s\n' "$beads"
