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
    exit 7
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
