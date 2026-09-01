#!/usr/bin/env bash
# Mutation-sensitive controls for agent-context production, deep verification,
# and credential-free checkout. Fixtures are retained and printed; repository
# policy forbids automated deletion and retained failures are easier to inspect.

set -euo pipefail

fail() {
  printf 'agent-context-selftest: %s\n' "$*" >&2
  exit 1
}

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
producer="$root/scripts/agent_context.sh"
verifier="$root/scripts/agent_context_verify.sh"
checkout="$root/scripts/agent_context_checkout.sh"
for file in "$producer" "$verifier" "$checkout"; do
  [ -f "$file" ] || fail "missing script: $file"
done
for command in git tar gzip; do
  command -v "$command" >/dev/null 2>&1 || fail "$command is required"
done

run_id="$$-${RANDOM:-0}"
base="${TMPDIR:-/tmp}/fgdb-agent-context-selftest-$run_id"
fixture="$base/repository"
clean_capsule="$base/clean"
dirty_capsule="$base/dirty"
clean_checkout="$base/clean-checkout"
dirty_checkout="$base/dirty-checkout"
v1_clean_capsule="$base/v1-clean"
v1_dirty_capsule="$base/v1-dirty"
mkdir -p "$fixture"

rehash() {
  local capsule="$1"
  (
    cd "$capsule"
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

expect_verify_red() {
  local capsule="$1" scratch="$2" reason="$3"
  if bash "$verifier" --scratch "$scratch" "$capsule" >/dev/null 2>&1; then
    fail "verifier accepted $reason"
  fi
}

make_v1_capsule() {
  local source="$1" target="$2" dirty="$3"
  mkdir "$target"
  local file
  for file in git-bundle-verify.txt git-status.txt recent-commits.tsv \
      repository.bundle tracked-files.txt tracked-source.tar.gz; do
    cp "$source/$file" "$target/$file"
  done
  if [ "$dirty" = true ]; then
    for file in untracked-files.txt worktree.patch worktree-stability-proof.patch; do
      cp "$source/$file" "$target/$file"
    done
  fi
  awk -F= '
    $1 == "format_version" { print "format_version=1"; next }
    $1 == "tree" || $1 == "bundle_ref" { next }
    { print }
  ' "$source/manifest.txt" > "$target/manifest.txt"
  rehash "$target"
}

cd "$fixture"
git init -q -b main
git config user.name "Agent Context Self-Test"
git config user.email "agent-context-selftest@example.invalid"
printf 'alpha\n' > tracked.txt
git add tracked.txt
git commit -q -m initial
commit="$(git rev-parse HEAD)"
tree="$(git rev-parse HEAD^{tree})"

bash "$producer" --no-beads --recent 1 --output "$clean_capsule" >/dev/null
bash "$verifier" --scratch "$base/verify-clean" "$clean_capsule" >/dev/null
grep -Fxq 'format_version=2' "$clean_capsule/manifest.txt" \
  || fail "producer did not emit format v2"
grep -Fxq "commit=$commit" "$clean_capsule/manifest.txt" \
  || fail "manifest commit mismatch"
grep -Fxq "tree=$tree" "$clean_capsule/manifest.txt" \
  || fail "manifest tree mismatch"
[ ! -s "$clean_capsule/git-status.txt" ] || fail "clean status witness is not empty"

bash "$checkout" --verify-scratch "$base/verify-checkout-clean" \
  "$clean_capsule" "$clean_checkout" >/dev/null
[ "$(git -C "$clean_checkout" rev-parse HEAD)" = "$commit" ] \
  || fail "clean checkout commit mismatch"
[ -z "$(git -C "$clean_checkout" remote)" ] \
  || fail "clean checkout unexpectedly has a remote"
cmp --silent "$fixture/tracked.txt" "$clean_checkout/tracked.txt" \
  || fail "clean checkout content mismatch"

make_v1_capsule "$clean_capsule" "$v1_clean_capsule" false
bash "$verifier" --scratch "$base/verify-v1-clean" "$v1_clean_capsule" >/dev/null

printf 'beta\n' >> tracked.txt
refused="$base/dirty-refused"
if bash "$producer" --no-beads --output "$refused" >/dev/null 2>&1; then
  fail "dirty tree was accepted without --allow-dirty"
fi
bash "$producer" --allow-dirty --no-beads --recent 1 \
  --output "$dirty_capsule" >/dev/null
bash "$verifier" --scratch "$base/verify-dirty" "$dirty_capsule" >/dev/null
cmp --silent "$dirty_capsule/worktree.patch" \
  "$dirty_capsule/worktree-stability-proof.patch" \
  || fail "dirty patch stability control failed"
cmp --silent "$dirty_capsule/git-status.txt" \
  "$dirty_capsule/git-status-stability-proof.txt" \
  || fail "dirty status stability control failed"

bash "$checkout" --apply-dirty --verify-scratch "$base/verify-checkout-dirty" \
  "$dirty_capsule" "$dirty_checkout" >/dev/null
printf 'alpha\nbeta\n' | cmp --silent - "$dirty_checkout/tracked.txt" \
  || fail "dirty checkout did not reproduce the tracked patch"

make_v1_capsule "$dirty_capsule" "$v1_dirty_capsule" true
bash "$verifier" --scratch "$base/verify-v1-dirty" "$v1_dirty_capsule" >/dev/null

if bash "$producer" --allow-dirty --no-beads \
  --output "$dirty_capsule" >/dev/null 2>&1; then
  fail "producer overwrote an existing capsule"
fi

checksum_tamper="$base/checksum-tamper"
cp -R "$clean_capsule" "$checksum_tamper"
printf 'tamper\n' >> "$checksum_tamper/manifest.txt"
expect_verify_red "$checksum_tamper" "$base/verify-checksum-tamper" \
  "a checksum-invalid capsule"

source_tamper="$base/source-tamper"
source_payload="$base/source-payload"
cp -R "$clean_capsule" "$source_tamper"
mkdir "$source_payload"
printf 'not the bundled tree\n' > "$source_payload/tracked.txt"
tar -C "$source_payload" -cf - tracked.txt | gzip -n \
  > "$source_tamper/tracked-source.tar.gz"
rehash "$source_tamper"
expect_verify_red "$source_tamper" "$base/verify-source-tamper" \
  "a checksum-consistent source archive unrelated to the bundle"

manifest_tamper="$base/manifest-tamper"
cp -R "$clean_capsule" "$manifest_tamper"
printf 'commit=%s\n' "$commit" >> "$manifest_tamper/manifest.txt"
rehash "$manifest_tamper"
expect_verify_red "$manifest_tamper" "$base/verify-manifest-tamper" \
  "a checksum-consistent duplicate manifest key"

inventory_tamper="$base/inventory-tamper"
cp -R "$clean_capsule" "$inventory_tamper"
printf 'extra\n' > "$inventory_tamper/undeclared.txt"
rehash "$inventory_tamper"
expect_verify_red "$inventory_tamper" "$base/verify-inventory-tamper" \
  "a checksum-consistent extra regular file"

history_tamper="$base/history-tamper"
cp -R "$clean_capsule" "$history_tamper"
printf 'fabricated history\n' > "$history_tamper/recent-commits.tsv"
rehash "$history_tamper"
expect_verify_red "$history_tamper" "$base/verify-history-tamper" \
  "checksum-consistent fabricated recent history"

printf 'PASS agent-context v2 producer/verifier/checkout controls\n'
printf 'fixture=%s\n' "$fixture"
printf 'clean_capsule=%s\n' "$clean_capsule"
printf 'dirty_capsule=%s\n' "$dirty_capsule"
printf 'clean_checkout=%s\n' "$clean_checkout"
printf 'dirty_checkout=%s\n' "$dirty_checkout"
printf 'v1_clean_capsule=%s\n' "$v1_clean_capsule"
printf 'v1_dirty_capsule=%s\n' "$v1_dirty_capsule"
printf 'tamper_root=%s\n' "$base"
