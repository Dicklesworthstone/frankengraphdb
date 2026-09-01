#!/usr/bin/env bash
# Independently verify a local proof bundle without rerunning its commands.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: bash scripts/local_proof_verify.sh PROOF_DIR

Validate one proof directory produced by scripts/local_proof.sh: strict file
inventory, checksums, exact-tree attribution, verdict/exit consistency, and the
check.sh reporting contract. Verification authenticates internal consistency,
not the identity of whoever supplied the directory.
USAGE
}

fail() {
  printf 'local-proof-verify: %s\n' "$*" >&2
  exit 1
}

[ "$#" -eq 1 ] || { usage >&2; exit 2; }
case "$1" in -h|--help) usage; exit 0 ;; esac
proof="$1"
[ -d "$proof" ] || fail "not a directory: $proof"
proof="$(cd "$proof" && pwd -P)"

required=(
  manifest.txt SHA256SUMS command.txt tools.txt
  commit-before.txt commit-after.txt status-before.txt status-after.txt
  check.stdout.log check.stderr.log check-exit.txt
)
for file in "${required[@]}"; do
  [ -f "$proof/$file" ] || fail "missing required file: $file"
done
symlink="$(find "$proof" -type l -print -quit)"
[ -z "$symlink" ] || fail "proof contains a symlink: $symlink"

manifest_value() {
  local key="$1"
  awk -F= -v key="$key" '$1 == key { sub(/^[^=]*=/, ""); print; found=1; exit } END { if (!found) exit 1 }' \
    "$proof/manifest.txt"
}

[ "$(manifest_value format_version)" = 1 ] || fail "unsupported or missing format_version"
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

actual_files="$(cd "$proof" && find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort)"
declared_files="$(sed -E 's/^[0-9a-fA-F]{64}[[:space:]]+//' "$proof/SHA256SUMS" | LC_ALL=C sort)"
[ "$actual_files" = "$declared_files" ] || fail "SHA256SUMS does not name the exact file inventory"
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

before_commit="$(cat "$proof/commit-before.txt")"
after_commit="$(cat "$proof/commit-after.txt")"
before_status="$(cat "$proof/status-before.txt")"
after_status="$(cat "$proof/status-after.txt")"
observed_stable=false
if [ "$before_commit" = "$after_commit" ] && [ "$before_status" = "$after_status" ]; then
  observed_stable=true
fi
[ "$observed_stable" = "$stable" ] || fail "tree_stable disagrees with captured state"

case "$verdict" in
  pass)
    [ "$stable" = true ] || fail "pass proof is not tree-stable"
    [ "$check_exit" -eq 0 ] || fail "pass proof has nonzero check exit"
    [ -z "$before_status" ] || fail "pass proof started dirty"
    grep -Fxq 'ALL GATES GREEN' "$proof/check.stdout.log" \
      || grep -Eq '^ALL GATES GREEN([[:space:]]|$)' "$proof/check.stdout.log" \
      || fail "pass proof lacks the anchored green summary"
    if grep -Eq '^(RED|UNRUN|FAIL)([[:space:]]|$)' "$proof/check.stdout.log"; then
      fail "pass proof contains an anchored non-pass verdict"
    fi
    ;;
  red)
    [ "$stable" = true ] || fail "red proof is not tree-stable"
    [ "$check_exit" -ne 0 ] || fail "red proof has zero check exit"
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
printf 'check_exit=%s\n' "$check_exit"
printf 'tree_stable=%s\n' "$stable"
printf 'verdict=%s\n' "$verdict"
