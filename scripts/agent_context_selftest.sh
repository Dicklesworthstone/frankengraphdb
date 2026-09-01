#!/usr/bin/env bash
# Semantic controls for the local agent-context producer and verifier.
#
# The fixture and capsules are deliberately retained and printed. Repository
# policy forbids automated deletion, and retaining a failed fixture makes the
# exact control state inspectable.

set -euo pipefail

fail() {
  printf 'agent-context-selftest: %s\n' "$*" >&2
  exit 1
}

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
producer="$root/scripts/agent_context.sh"
verifier="$root/scripts/agent_context_verify.sh"
[ -f "$producer" ] || fail "missing producer: $producer"
[ -f "$verifier" ] || fail "missing verifier: $verifier"
command -v git >/dev/null 2>&1 || fail "git is required"

run_id="$$-${RANDOM:-0}"
base="${TMPDIR:-/tmp}/fgdb-agent-context-selftest-$run_id"
fixture="$base/repository"
clean_capsule="$base/clean"
dirty_capsule="$base/dirty"
tampered_capsule="$base/tampered"
mkdir -p "$fixture"

cd "$fixture"
git init -q -b main
git config user.name "Agent Context Self-Test"
git config user.email "agent-context-selftest@example.invalid"
printf 'alpha\n' > tracked.txt
git add tracked.txt
git commit -q -m initial

bash "$producer" --no-beads --recent 1 --output "$clean_capsule" >/dev/null
bash "$verifier" "$clean_capsule" >/dev/null
(
  cd "$clean_capsule"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c SHA256SUMS >/dev/null
  else
    shasum -a 256 -c SHA256SUMS >/dev/null
  fi
)

printf 'beta\n' >> tracked.txt
refused="$base/dirty-refused"
if bash "$producer" --no-beads --output "$refused" >/dev/null 2>&1; then
  fail "dirty tree was accepted without --allow-dirty"
fi

bash "$producer" --allow-dirty --no-beads --recent 1 \
  --output "$dirty_capsule" >/dev/null
bash "$verifier" "$dirty_capsule" >/dev/null
cmp --silent "$dirty_capsule/worktree.patch" \
  "$dirty_capsule/worktree-stability-proof.patch" \
  || fail "dirty patch stability control failed"

if bash "$producer" --allow-dirty --no-beads \
  --output "$dirty_capsule" >/dev/null 2>&1; then
  fail "producer overwrote an existing capsule"
fi

cp -R "$clean_capsule" "$tampered_capsule"
printf 'tamper\n' >> "$tampered_capsule/manifest.txt"
if bash "$verifier" "$tampered_capsule" >/dev/null 2>&1; then
  fail "verifier accepted a checksum-invalid capsule"
fi

printf 'PASS agent-context capsule semantic controls\n'
printf 'fixture=%s\n' "$fixture"
printf 'clean_capsule=%s\n' "$clean_capsule"
printf 'dirty_capsule=%s\n' "$dirty_capsule"
printf 'tampered_capsule=%s\n' "$tampered_capsule"
