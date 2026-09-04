#!/usr/bin/env bash
# Retained mutation-sensitive controls for local proof production and verification.

set -euo pipefail

fail() {
  printf 'local-proof-selftest: %s\n' "$*" >&2
  exit 1
}

source_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
run_id="$$-${RANDOM:-0}"
base="${TMPDIR:-/tmp}/fgdb-local-proof-selftest-$run_id"
fixture="$base/repository"
mkdir -p "$fixture/scripts"
cp "$source_root/scripts/local_proof.sh" "$fixture/scripts/local_proof.sh"
cp "$source_root/scripts/local_proof_verify.sh" "$fixture/scripts/local_proof_verify.sh"

cat > "$fixture/scripts/check.sh" <<'CHECK'
#!/usr/bin/env bash
set -eu
if [ -n "${FAKE_PROOF_CHECK_MARKER:-}" ]; then
  printf 'ran\n' > "$FAKE_PROOF_CHECK_MARKER"
fi
case "${FAKE_PROOF_MODE:-pass}" in
  pass)
    printf 'PASS fake gate\n'
    printf 'ALL GATES GREEN\n'
    ;;
  red)
    printf 'RED fake gate\n'
    printf 'FAIL fake gate\n'
    printf 'QUALITY GATE RED\n'
    printf 'QUALITY GATE RED\n' >&2
    exit "${FAKE_PROOF_RED_EXIT:-7}"
    ;;
  move)
    printf 'changed\n' >> tracked.txt
    printf 'PASS fake gate\n'
    printf 'ALL GATES GREEN\n'
    ;;
  *) exit 99 ;;
esac
CHECK
chmod +x "$fixture/scripts/"*.sh

rehash() {
  local proof="$1"
  (
    cd "$proof"
    : > SHA256SUMS
    while IFS= read -r file; do
      if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" >> SHA256SUMS
      else
        shasum -a 256 "$file" >> SHA256SUMS
      fi
    done < <(find . -type f ! -name SHA256SUMS -print | LC_ALL=C sort)
  )
}

expect_red() {
  local proof="$1" reason="$2"
  if bash scripts/local_proof_verify.sh --repository "$fixture" "$proof" >/dev/null 2>&1; then
    fail "verifier accepted $reason"
  fi
}

make_v1_proof() {
  local source="$1" target="$2"
  mkdir "$target"
  local file
  for file in command.txt tools.txt commit-before.txt commit-after.txt \
      status-before.txt status-after.txt check.stdout.log check.stderr.log \
      check-exit.txt; do
    cp "$source/$file" "$target/$file"
  done
  awk -F= '
    $1 == "format_version" { print "format_version=1"; next }
    $1 == "tree" || $1 == "check_script_path" || $1 == "check_script_blob" { next }
    { print }
  ' "$source/manifest.txt" > "$target/manifest.txt"
  rehash "$target"
}

cd "$fixture"
git init -q -b main
git config user.name "Local Proof Self-Test"
git config user.email "local-proof-selftest@example.invalid"
printf 'stable\n' > tracked.txt
git add .
git commit -q -m initial

pass_proof="$base/pass"
FAKE_PROOF_MODE=pass bash scripts/local_proof.sh --output "$pass_proof" >/dev/null
bash scripts/local_proof_verify.sh --repository "$fixture" "$pass_proof" >/dev/null
grep -Fxq 'format_version=2' "$pass_proof/manifest.txt" || fail "producer did not emit v2"

v1_proof="$base/v1-pass"
make_v1_proof "$pass_proof" "$v1_proof"
bash scripts/local_proof_verify.sh --repository "$fixture" "$v1_proof" >/dev/null

red_proof="$base/red"
set +e
FAKE_PROOF_MODE=red bash scripts/local_proof.sh --output "$red_proof" >/dev/null
red_exit=$?
set -e
[ "$red_exit" -eq 7 ] || fail "red proof did not preserve check exit 7"
bash scripts/local_proof_verify.sh --repository "$fixture" "$red_proof" >/dev/null

move_proof="$base/move"
set +e
FAKE_PROOF_MODE=move bash scripts/local_proof.sh --output "$move_proof" >/dev/null
move_exit=$?
set -e
[ "$move_exit" -eq 125 ] || fail "moving tree did not produce void exit 125"
bash scripts/local_proof_verify.sh --repository "$fixture" "$move_proof" >/dev/null

git add tracked.txt
git commit -q -m stabilize-after-movement

# Exercise producer infrastructure failures without compiling or running Rust.
# All shims and markers live outside the observed repository. Retain every
# artifact, including a competing writer's sentinel, for diagnosis.
fault_bin="$base/fault-bin"
mkdir "$fault_bin"
LOCAL_PROOF_REAL_GIT="$(command -v git)"
LOCAL_PROOF_REAL_MKDIR="$(command -v mkdir)"
LOCAL_PROOF_REAL_FIND="$(command -v find)"
LOCAL_PROOF_REAL_SORT="$(command -v sort)"
LOCAL_PROOF_REAL_HASH="$(command -v sha256sum || command -v shasum)"
export LOCAL_PROOF_REAL_GIT LOCAL_PROOF_REAL_MKDIR LOCAL_PROOF_REAL_FIND
export LOCAL_PROOF_REAL_SORT LOCAL_PROOF_REAL_HASH

cat > "$fault_bin/git" <<'SHIM'
#!/usr/bin/env bash
set -eu
if [ "${1:-}" = status ]; then
  case "${FAKE_PROOF_FAULT:-}" in
    before-status)
      if [ ! -e "$FAKE_PROOF_CHECK_MARKER" ]; then
        printf 'injected status capture failure\n' >&2
        exit 69
      fi
      ;;
    after-status)
      if [ -e "$FAKE_PROOF_CHECK_MARKER" ]; then
        printf 'injected status capture failure\n' >&2
        exit 69
      fi
      ;;
  esac
fi
exec "$LOCAL_PROOF_REAL_GIT" "$@"
SHIM
cat > "$fault_bin/mkdir" <<'SHIM'
#!/usr/bin/env bash
set -eu
if [ "${FAKE_PROOF_FAULT:-}" = output-claim ]; then
  "$LOCAL_PROOF_REAL_MKDIR" "$@"
  printf 'other-writer-evidence\n' > "$1/manifest.txt"
  printf 'injected output claim failure\n' >&2
  exit 73
fi
exec "$LOCAL_PROOF_REAL_MKDIR" "$@"
SHIM
cat > "$fault_bin/find" <<'SHIM'
#!/usr/bin/env bash
set -eu
if [ "${FAKE_PROOF_FAULT:-}" = inventory-find ]; then
  printf './command.txt\n'
  printf 'injected inventory find failure\n' >&2
  exit 74
fi
exec "$LOCAL_PROOF_REAL_FIND" "$@"
SHIM
cat > "$fault_bin/sort" <<'SHIM'
#!/usr/bin/env bash
set -eu
if [ "${FAKE_PROOF_FAULT:-}" = inventory-sort ]; then
  "$LOCAL_PROOF_REAL_SORT" "$@"
  printf 'injected inventory sort failure\n' >&2
  exit 75
fi
exec "$LOCAL_PROOF_REAL_SORT" "$@"
SHIM
cat > "$fault_bin/$(basename "$LOCAL_PROOF_REAL_HASH")" <<'SHIM'
#!/usr/bin/env bash
set -eu
if [ "${FAKE_PROOF_FAULT:-}" = checksum ]; then
  printf 'injected checksum failure\n' >&2
  exit 76
fi
exec "$LOCAL_PROOF_REAL_HASH" "$@"
SHIM
chmod +x "$fault_bin/"*

expect_producer_abort() {
  local fault="$1" check_ran="$2" diagnostic="$3"
  local proof="$base/fault-$fault" marker="$base/ran-$fault"
  if PATH="$fault_bin:$PATH" FAKE_PROOF_MODE=pass \
      FAKE_PROOF_FAULT="$fault" FAKE_PROOF_CHECK_MARKER="$marker" \
      bash scripts/local_proof.sh --output "$proof" \
      > "$base/$fault.stdout" 2> "$base/$fault.stderr"; then
    fail "producer accepted $fault infrastructure failure"
  fi
  if grep -Eq '^LOCAL_PROOF_(PASS|RED|VOID)$' "$base/$fault.stdout"; then
    fail "producer published a completed proof after $fault"
  fi
  grep -Fq "$diagnostic" "$base/$fault.stderr" \
    || fail "producer lost $fault tool diagnostics"
  if [ "$check_ran" = yes ]; then
    [ -f "$marker" ] || fail "$fault control did not reach check.sh"
  else
    [ ! -e "$marker" ] || fail "producer ran check.sh after $fault"
  fi
  if [ "$fault" = output-claim ]; then
    [ "$(cat "$proof/manifest.txt")" = other-writer-evidence ] \
      || fail "producer overwrote another writer's proof"
    [ ! -e "$proof/commit-before.txt" ] \
      || fail "producer wrote evidence without claiming the directory"
  fi
}

expect_producer_abort before-status no 'injected status capture failure'
expect_producer_abort after-status yes 'injected status capture failure'
expect_producer_abort output-claim no 'injected output claim failure'
expect_producer_abort inventory-find yes 'injected inventory find failure'
expect_producer_abort inventory-sort yes 'injected inventory sort failure'
expect_producer_abort checksum yes 'injected checksum failure'

# A stable check exit 125 is RED, not VOID; 255 remains a valid shell status.
for code in 125 255; do
  boundary_proof="$base/red-$code"
  if FAKE_PROOF_MODE=red FAKE_PROOF_RED_EXIT="$code" \
      bash scripts/local_proof.sh --output "$boundary_proof" >/dev/null; then
    fail "producer accepted check exit $code"
  else
    boundary_exit=$?
  fi
  [ "$boundary_exit" -eq "$code" ] || fail "producer changed check exit $code"
  grep -Fxq 'verdict=red' "$boundary_proof/manifest.txt" \
    || fail "stable check exit $code was not RED"
  bash scripts/local_proof_verify.sh --repository "$fixture" "$boundary_proof" >/dev/null
done

checksum_tamper="$base/checksum-tamper"
cp -R "$pass_proof" "$checksum_tamper"
printf 'tamper\n' >> "$checksum_tamper/check.stdout.log"
expect_red "$checksum_tamper" "a checksum-invalid proof"

inventory_tamper="$base/inventory-tamper"
cp -R "$pass_proof" "$inventory_tamper"
printf 'extra\n' > "$inventory_tamper/undeclared.txt"
rehash "$inventory_tamper"
expect_red "$inventory_tamper" "a checksum-consistent extra file"

manifest_tamper="$base/manifest-tamper"
cp -R "$pass_proof" "$manifest_tamper"
printf 'commit=%s\n' "$(git rev-parse HEAD)" >> "$manifest_tamper/manifest.txt"
rehash "$manifest_tamper"
expect_red "$manifest_tamper" "a checksum-consistent duplicate manifest key"

tree_tamper="$base/tree-tamper"
cp -R "$pass_proof" "$tree_tamper"
awk -F= '$1 == "tree" { print "tree=0000000000000000000000000000000000000000"; next } { print }' \
  "$pass_proof/manifest.txt" > "$tree_tamper/manifest.txt"
printf '%040d\n' 0 > "$tree_tamper/tree-before.txt"
rehash "$tree_tamper"
expect_red "$tree_tamper" "a checksum-consistent false source tree"

red_contract_tamper="$base/red-contract-tamper"
cp -R "$red_proof" "$red_contract_tamper"
printf 'RED fake gate\nFAIL fake gate\n' > "$red_contract_tamper/check.stdout.log"
: > "$red_contract_tamper/check.stderr.log"
rehash "$red_contract_tamper"
expect_red "$red_contract_tamper" "a red proof without the check.sh reporting contract"

printf 'PASS local-proof v2 semantic controls\n'
printf 'fixture=%s\n' "$fixture"
printf 'pass_proof=%s\n' "$pass_proof"
printf 'v1_proof=%s\n' "$v1_proof"
printf 'red_proof=%s\n' "$red_proof"
printf 'void_proof=%s\n' "$move_proof"
printf 'tamper_root=%s\n' "$base"
