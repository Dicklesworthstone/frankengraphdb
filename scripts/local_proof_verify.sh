#!/usr/bin/env bash
# Independently verify a local proof bundle without rerunning its commands.
#
# This authenticates format and internal consistency, not the identity of the
# bundle's producer. With --repository it additionally binds the named commit,
# tree, and check-script blob to an independently supplied Git object database.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: bash scripts/local_proof_verify.sh [--repository DIR] PROOF_DIR

Options:
  --repository DIR  Require DIR's Git object database to contain the proof's
                    commit, tree, and exact tracked scripts/check.sh blob.
  -h, --help        Show this help.

Verification never reruns check.sh. It proves strict format, checksums,
exact-tree attribution, verdict/exit consistency, and the reporting contract.
USAGE
}

fail() {
  printf 'local-proof-verify: %s\n' "$*" >&2
  exit 1
}

repository=""
proof_arg=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --repository)
      [ "$#" -ge 2 ] || fail "--repository requires a directory"
      repository="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*) fail "unknown argument: $1" ;;
    *)
      [ -z "$proof_arg" ] || fail "only one proof directory may be supplied"
      proof_arg="$1"
      shift
      ;;
  esac
done
[ -n "$proof_arg" ] || { usage >&2; exit 2; }

proof="$proof_arg"
[ -d "$proof" ] || fail "not a directory: $proof"
proof="$(cd "$proof" && pwd -P)"
if [ -n "$repository" ]; then
  [ -d "$repository" ] || fail "not a repository directory: $repository"
  repository="$(cd "$repository" && pwd -P)"
  git -C "$repository" rev-parse --git-dir >/dev/null 2>&1 \
    || fail "--repository is not a Git worktree or repository"
fi

base_files=(
  manifest.txt SHA256SUMS command.txt tools.txt
  commit-before.txt commit-after.txt status-before.txt status-after.txt
  check.stdout.log check.stderr.log check-exit.txt
)
for file in "${base_files[@]}"; do
  [ -f "$proof/$file" ] || fail "missing required file: $file"
done
symlink="$(find "$proof" -type l -print -quit)"
[ -z "$symlink" ] || fail "proof contains a symlink: $symlink"

manifest_value() {
  local key="$1"
  awk -F= -v key="$key" '
    $1 == key {
      count += 1
      if (count == 1) { sub(/^[^=]*=/, ""); value=$0 }
    }
    END { if (count != 1) exit 1; print value }
  ' "$proof/manifest.txt"
}

format_version="$(manifest_value format_version)" \
  || fail "manifest must contain exactly one format_version"
case "$format_version" in 1|2) ;; *) fail "unsupported format_version: $format_version" ;; esac
keys_v1=(authority check_exit commit finished_utc format_version ref repository started_utc tree_stable verdict)
keys_v2=(authority check_exit check_script_blob check_script_path commit finished_utc format_version ref repository started_utc tree tree_stable verdict)
actual_keys="$(awk -F= 'NF >= 2 && length($1) > 0 { print $1; next } { exit 2 }' \
  "$proof/manifest.txt" | LC_ALL=C sort)" \
  || fail "manifest contains a blank or malformed line"
if [ "$format_version" = 1 ]; then
  expected_keys="$(printf '%s\n' "${keys_v1[@]}" | LC_ALL=C sort)"
else
  expected_keys="$(printf '%s\n' "${keys_v2[@]}" | LC_ALL=C sort)"
fi
[ "$actual_keys" = "$expected_keys" ] \
  || fail "manifest key inventory is not exact for format v$format_version"

commit="$(manifest_value commit)" || fail "manifest lacks commit"
printf '%s\n' "$commit" | grep -Eq '^[0-9a-f]{40}$' || fail "invalid manifest commit"
check_exit="$(manifest_value check_exit)" || fail "manifest lacks check_exit"
printf '%s\n' "$check_exit" | grep -Eq '^[0-9]+$' || fail "check_exit is not an unsigned integer"
stable="$(manifest_value tree_stable)" || fail "manifest lacks tree_stable"
case "$stable" in true|false) ;; *) fail "tree_stable must be true or false" ;; esac
verdict="$(manifest_value verdict)" || fail "manifest lacks verdict"
case "$verdict" in pass|red|void) ;; *) fail "unknown verdict: $verdict" ;; esac
authority="$(manifest_value authority)" || fail "manifest lacks authority"
case "$authority" in check.sh\ exit\ code*) ;; *) fail "manifest authority boundary is invalid" ;; esac
[ -n "$(manifest_value started_utc)" ] || fail "started_utc is empty"
[ -n "$(manifest_value finished_utc)" ] || fail "finished_utc is empty"

expected_files=(
  ./SHA256SUMS ./check-exit.txt ./check.stderr.log ./check.stdout.log
  ./command.txt ./commit-after.txt ./commit-before.txt ./manifest.txt
  ./status-after.txt ./status-before.txt ./tools.txt
)
if [ "$format_version" = 2 ]; then
  expected_files+=(./check-script-blob.txt ./tree-after.txt ./tree-before.txt)
  for file in check-script-blob.txt tree-after.txt tree-before.txt; do
    [ -f "$proof/$file" ] || fail "format v2 proof lacks $file"
  done
fi
expected_inventory="$(printf '%s\n' "${expected_files[@]}" | LC_ALL=C sort)"
actual_inventory="$(cd "$proof" && find . -type f -print | LC_ALL=C sort)"
[ "$actual_inventory" = "$expected_inventory" ] \
  || fail "proof regular-file inventory is not exact"

declared_files="$(sed -E 's/^[0-9a-fA-F]{64}[[:space:]]+//' "$proof/SHA256SUMS" | LC_ALL=C sort)"
actual_without_sums="$(cd "$proof" && find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort)"
[ "$declared_files" = "$actual_without_sums" ] \
  || fail "SHA256SUMS does not name the exact checksum-covered inventory"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$proof" && sha256sum -c SHA256SUMS >/dev/null) || fail "SHA-256 verification failed"
elif command -v shasum >/dev/null 2>&1; then
  (cd "$proof" && shasum -a 256 -c SHA256SUMS >/dev/null) || fail "SHA-256 verification failed"
else
  fail "sha256sum or shasum is required"
fi

[ "$(cat "$proof/command.txt")" = "bash scripts/check.sh" ] || fail "unexpected proof command"
[ "$(cat "$proof/commit-before.txt")" = "$commit" ] || fail "manifest/before-commit mismatch"
recorded_exit="$(cat "$proof/check-exit.txt")"
[ "$recorded_exit" = "$check_exit" ] || fail "manifest/check-exit file mismatch"
if grep -q '[^[:space:]]' "$proof/status-before.txt"; then
  fail "proof did not start from a clean worktree"
fi

before_commit="$(cat "$proof/commit-before.txt")"
after_commit="$(cat "$proof/commit-after.txt")"
before_status="$(cat "$proof/status-before.txt")"
after_status="$(cat "$proof/status-after.txt")"
if [ "$format_version" = 2 ]; then
  tree="$(manifest_value tree)" || fail "manifest lacks tree"
  printf '%s\n' "$tree" | grep -Eq '^[0-9a-f]{40}$' || fail "invalid manifest tree"
  path="$(manifest_value check_script_path)" || fail "manifest lacks check_script_path"
  [ "$path" = scripts/check.sh ] || fail "unexpected check_script_path: $path"
  script_blob="$(manifest_value check_script_blob)" || fail "manifest lacks check_script_blob"
  printf '%s\n' "$script_blob" | grep -Eq '^[0-9a-f]{40}$' || fail "invalid check_script_blob"
  [ "$(cat "$proof/tree-before.txt")" = "$tree" ] || fail "manifest/before-tree mismatch"
  [ "$(cat "$proof/check-script-blob.txt")" = "$script_blob" ] || fail "manifest/check-script-blob mismatch"
  before_tree="$(cat "$proof/tree-before.txt")"
  after_tree="$(cat "$proof/tree-after.txt")"
else
  tree=""
  path="scripts/check.sh"
  script_blob=""
  before_tree="legacy"
  after_tree="legacy"
fi

observed_stable=false
if [ "$before_commit" = "$after_commit" ] \
  && [ "$before_tree" = "$after_tree" ] \
  && [ "$before_status" = "$after_status" ]; then
  observed_stable=true
fi
[ "$observed_stable" = "$stable" ] || fail "tree_stable disagrees with captured state"
if [ "$stable" = true ]; then
  if grep -q '[^[:space:]]' "$proof/status-after.txt"; then
    fail "stable proof ended dirty"
  fi
fi

if [ -n "$repository" ]; then
  git -C "$repository" cat-file -e "${commit}^{commit}" 2>/dev/null \
    || fail "repository does not contain proof commit $commit"
  repository_tree="$(git -C "$repository" rev-parse "${commit}^{tree}")"
  if [ -n "$tree" ] && [ "$repository_tree" != "$tree" ]; then
    fail "repository tree $repository_tree disagrees with proof tree $tree"
  fi
  if [ -n "$script_blob" ]; then
    repository_blob="$(git -C "$repository" rev-parse "${commit}:${path}")" \
      || fail "repository commit lacks $path"
    [ "$repository_blob" = "$script_blob" ] \
      || fail "repository $path blob disagrees with proof"
  fi
fi

case "$verdict" in
  pass)
    [ "$stable" = true ] || fail "pass proof is not tree-stable"
    [ "$check_exit" -eq 0 ] || fail "pass proof has nonzero check exit"
    [ "$(grep -Ec '^ALL GATES GREEN([[:space:]]|$)' "$proof/check.stdout.log")" -eq 1 ] \
      || fail "pass proof must contain exactly one anchored green summary"
    if grep -Eq '^(RED|UNRUN|FAIL|QUALITY GATE RED)([[:space:]]|$)' "$proof/check.stdout.log"; then
      fail "pass proof contains an anchored non-pass verdict"
    fi
    ;;
  red)
    [ "$stable" = true ] || fail "red proof is not tree-stable"
    [ "$check_exit" -ne 0 ] || fail "red proof has zero check exit"
    grep -Fxq 'QUALITY GATE RED' "$proof/check.stdout.log" \
      || fail "red proof lacks QUALITY GATE RED on stdout"
    grep -Fxq 'QUALITY GATE RED' "$proof/check.stderr.log" \
      || fail "red proof lacks QUALITY GATE RED on stderr"
    grep -Eq '^(RED|UNRUN|FAIL)([[:space:]]|$)' "$proof/check.stdout.log" \
      || fail "red proof lacks an anchored failing gate verdict"
    if grep -Eq '^ALL GATES GREEN([[:space:]]|$)' "$proof/check.stdout.log"; then
      fail "red proof contains a green summary"
    fi
    ;;
  void)
    [ "$stable" = false ] || fail "void proof claims a stable tree"
    ;;
esac

printf 'LOCAL_PROOF_VERIFIED\n'
printf 'path=%s\n' "$proof"
printf 'commit=%s\n' "$commit"
printf 'tree=%s\n' "${tree:-legacy-unrecorded}"
printf 'check_script_blob=%s\n' "${script_blob:-legacy-unrecorded}"
printf 'check_exit=%s\n' "$check_exit"
printf 'tree_stable=%s\n' "$stable"
printf 'verdict=%s\n' "$verdict"
