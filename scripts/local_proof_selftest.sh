#!/usr/bin/env bash
# Retained semantic controls for local proof production and verification.

set -euo pipefail

fail() {
  printf 'local-proof-selftest: %s\n' "$*" >&2
  exit 1
}

source_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
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

cd "$fixture"
git init -q -b main
git config user.name "Local Proof Self-Test"
git config user.email "local-proof-selftest@example.invalid"
printf 'stable\n' > tracked.txt
git add .
git commit -q -m initial

pass_proof="$base/pass"
FAKE_PROOF_MODE=pass bash scripts/local_proof.sh --output "$pass_proof" >/dev/null
bash scripts/local_proof_verify.sh "$pass_proof" >/dev/null

red_proof="$base/red"
set +e
FAKE_PROOF_MODE=red bash scripts/local_proof.sh --output "$red_proof" >/dev/null
red_exit=$?
set -e
[ "$red_exit" -eq 7 ] || fail "red proof did not preserve check exit 7"
bash scripts/local_proof_verify.sh "$red_proof" >/dev/null

move_proof="$base/move"
set +e
FAKE_PROOF_MODE=move bash scripts/local_proof.sh --output "$move_proof" >/dev/null
move_exit=$?
set -e
[ "$move_exit" -eq 125 ] || fail "moving tree did not produce void exit 125"
bash scripts/local_proof_verify.sh "$move_proof" >/dev/null

git add tracked.txt
git commit -q -m stabilize-after-movement

tampered="$base/tampered"
cp -R "$pass_proof" "$tampered"
printf 'tamper\n' >> "$tampered/check.stdout.log"
if bash scripts/local_proof_verify.sh "$tampered" >/dev/null 2>&1; then
  fail "verifier accepted a checksum-invalid proof"
fi

printf 'PASS local-proof semantic controls\n'
printf 'fixture=%s\n' "$fixture"
printf 'pass_proof=%s\n' "$pass_proof"
printf 'red_proof=%s\n' "$red_proof"
printf 'void_proof=%s\n' "$move_proof"
printf 'tampered_proof=%s\n' "$tampered"
