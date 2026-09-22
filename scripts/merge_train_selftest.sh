#!/usr/bin/env bash
# Mutation-sensitive controls for scripts/merge_train.sh (fgdb-verified-landing-queue-kqglu).
#
# Builds a throwaway remote with a fake scripts/check.sh and drives the real
# merge_train.sh, local_proof.sh and local_proof_verify.sh through every
# verdict. No Rust is compiled.
#
# The fake check is red exactly when a tracked file named RED exists. Each run
# appends one line to a counter file outside the repository, so the self-test
# can prove that a bounced batch is NOT re-proved.
#
# Scenarios:
#   1. no staging branch                   -> NOTHING, exit 0, main unchanged
#   2. staging adds a good commit          -> PROMOTED; main is a --no-ff train merge with parents
#                                             (old main, staging), carrying a pass note
#   3. staging adds RED                    -> RED, exit 1; main unchanged; red note with the FAIL line
#                                             on the staging tip
#   4. rerun, same main and staging        -> BOUNCED-ALREADY, exit 1; the check is not re-run
#   5. direct push to main by the toolchain-less identity, plus a conflicting staging edit
#                                          -> CONFLICT, exit 3, conflict note naming the file
#   6. audit                               -> exit 1 with a VIOLATION for the direct push; the
#                                             train merge is counted as proven
#   7. staging repaired, run --no-push     -> PASS (--no-push), exit 0, remote main unchanged;
#                                             then a real run -> PROMOTED
#
# Every fixture is retained under the scratch root for diagnosis (AGENTS.md RULE 1).

set -euo pipefail

fail() {
  printf 'merge-train-selftest: FAIL %s\n' "$*" >&2
  exit 1
}
ok() {
  printf 'ok  %s\n' "$*"
}

source_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
base="${TMPDIR:-/tmp}/fgdb-merge-train-selftest-$$-${RANDOM:-0}"
mkdir -p "$base"
remote="$base/remote.git"
dev="$base/dev"
op="$base/operator"
work="$base/work"
counter="$base/check-runs.txt"
: > "$counter"
export FAKE_CHECK_COUNTER="$counter"
# The train must not reach for rch or a shared target dir here; nothing compiles.
export RCH_CARGO_WRAPPER_BYPASS=1

git init -q --bare -b main "$remote"
git clone -q "$remote" "$dev" 2>/dev/null
cd "$dev"
git config user.name "Self-Test Dev"
git config user.email "dev@example.invalid"
mkdir -p scripts
cp "$source_root/scripts/merge_train.sh" scripts/merge_train.sh
cp "$source_root/scripts/local_proof.sh" scripts/local_proof.sh
cp "$source_root/scripts/local_proof_verify.sh" scripts/local_proof_verify.sh
cat > scripts/check.sh <<'CHECK'
#!/usr/bin/env bash
set -eu
printf 'run\n' >> "${FAKE_CHECK_COUNTER:-/dev/null}"
if [ -e RED ]; then
  printf 'FAIL fake gate: RED is tracked\n'
  printf 'QUALITY GATE RED\n'
  printf 'QUALITY GATE RED\n' >&2
  exit 7
fi
printf 'PASS fake gate\n'
# The verifier's reporting contract: a pass transcript carries exactly one
# anchored green summary (local_proof_verify.sh refuses a pass without it).
printf 'ALL GATES GREEN\n'
CHECK
chmod +x scripts/*.sh
printf 'base\n' > README.txt
git add .
git commit -q -m initial
git push -q origin main
initial_main="$(git rev-parse HEAD)"

git clone -q "$remote" "$op"
git -C "$op" config user.name "Self-Test Train"
git -C "$op" config user.email "train@example.invalid"

remote_main() { git --git-dir="$remote" rev-parse refs/heads/main; }
runs() { wc -l < "$counter" | tr -d ' '; }
train() {
  local rc=0
  (cd "$op" && bash scripts/merge_train.sh "$@" --work "$work") > "$base/last.out" 2>&1 || rc=$?
  printf '%s' "$rc"
}
note_of() { git --git-dir="$remote" notes --ref=merge-train show "$1" 2>/dev/null || true; }
# has_note OBJECT GREP-ARGS...: here-string, not a pipe, so grep -q cannot SIGPIPE a pipefail writer.
has_note() { local obj="$1"; shift; local n; n="$(note_of "$obj")"; grep "$@" <<< "$n"; }

# 1. No staging branch.
rc="$(train run)"
[ "$rc" = 0 ] || fail "scenario 1: exit $rc, expected 0 ($(cat "$base/last.out"))"
grep -q '^merge-train: NOTHING' "$base/last.out" || fail "scenario 1: no NOTHING line"
[ "$(remote_main)" = "$initial_main" ] || fail "scenario 1: main moved"
ok "1 no staging branch -> NOTHING, main unchanged"

# 2. A good staging commit is promoted through a --no-ff train merge.
cd "$dev"
git checkout -q -b staging
printf 'feature\n' > feature.txt
git add feature.txt
git -c user.email=35050222+bot@users.noreply.github.com commit -q -m "feat: good change"
git push -q origin staging
good_staging="$(git rev-parse HEAD)"
rc="$(train run)"
[ "$rc" = 0 ] || fail "scenario 2: exit $rc ($(cat "$base/last.out"))"
grep -q '^merge-train: PROMOTED' "$base/last.out" || fail "scenario 2: no PROMOTED line"
promoted="$(remote_main)"
parents="$(git --git-dir="$remote" rev-list --parents -n1 "$promoted")"
[ "$parents" = "$promoted $initial_main $good_staging" ] \
  || fail "scenario 2: main tip is not a --no-ff merge of (old main, staging): $parents"
has_note "$promoted" -qx 'verdict=pass' || fail "scenario 2: no pass note on the promoted merge"
has_note "$promoted" -qx 'check_exit=0' || fail "scenario 2: pass note lacks check_exit=0"
[ "$(runs)" = 1 ] || fail "scenario 2: expected exactly 1 check run, saw $(runs)"
ok "2 good staging -> PROMOTED as --no-ff merge with pass note"

# 3. A red staging commit is refused; main does not move.
cd "$dev"
printf 'boom\n' > RED
git add RED
git commit -q -m "feat: breaks the gate"
git push -q origin staging
red_staging="$(git rev-parse HEAD)"
rc="$(train run)"
[ "$rc" = 1 ] || fail "scenario 3: exit $rc, expected 1 ($(cat "$base/last.out"))"
grep -q '^merge-train: RED' "$base/last.out" || fail "scenario 3: no RED line"
[ "$(remote_main)" = "$promoted" ] || fail "scenario 3: main moved on a red proof"
has_note "$red_staging" -qx 'verdict=red' || fail "scenario 3: no red note on the staging tip"
has_note "$red_staging" -q '^fail: FAIL fake gate: RED is tracked$' \
  || fail "scenario 3: red note lacks the FAIL transcript line"
[ "$(runs)" = 2 ] || fail "scenario 3: expected 2 check runs, saw $(runs)"
ok "3 red staging -> RED, main unchanged, red note carries the FAIL line"

# 4. The same pair is not re-proved.
rc="$(train run)"
[ "$rc" = 1 ] || fail "scenario 4: exit $rc, expected 1"
grep -q '^merge-train: BOUNCED-ALREADY' "$base/last.out" || fail "scenario 4: no BOUNCED-ALREADY line"
[ "$(runs)" = 2 ] || fail "scenario 4: the check was re-run ($(runs) runs)"
ok "4 unchanged bounced pair -> BOUNCED-ALREADY without re-proving"

# 5. A direct push to main (the violation audit must catch), plus a conflicting staging edit.
direct="$base/direct"
git clone -q "$remote" "$direct"
cd "$direct"
printf 'main version\n' > README.txt
git -c user.name=Bot -c user.email=35050222+bot@users.noreply.github.com commit -q -am "feat: pushed straight to main"
git push -q origin main
direct_commit="$(git rev-parse HEAD)"
cd "$dev"
git rm -q RED
printf 'staging version\n' > README.txt
git commit -q -am "fix: remove RED, edit README"
git push -q origin staging
conflict_staging="$(git rev-parse HEAD)"
rc="$(train run)"
[ "$rc" = 3 ] || fail "scenario 5: exit $rc, expected 3 ($(cat "$base/last.out"))"
grep -q '^merge-train: CONFLICT' "$base/last.out" || fail "scenario 5: no CONFLICT line"
[ "$(remote_main)" = "$direct_commit" ] || fail "scenario 5: main moved on a conflict"
has_note "$conflict_staging" -qx 'conflict=README.txt' || fail "scenario 5: conflict note lacks the file"
ok "5 conflicting staging -> CONFLICT note naming README.txt"

# 6. Audit catches the direct push and counts the train merge as proven.
rc="$(train audit --since "$initial_main")"
[ "$rc" = 1 ] || fail "scenario 6: audit exit $rc, expected 1 ($(cat "$base/last.out"))"
grep -q "^VIOLATION first-parent ${direct_commit:0:12}" "$base/last.out" \
  || fail "scenario 6: audit did not name the direct push ($(cat "$base/last.out"))"
grep -q 'proven=1 unproven=1 violations=1' "$base/last.out" \
  || fail "scenario 6: unexpected audit tally ($(tail -1 "$base/last.out"))"
ok "6 audit -> VIOLATION for the direct toolchain-less push; train merge counted proven"

# 7. Repair staging by merging main into it; --no-push proves without moving main, then a real run promotes.
cd "$dev"
git fetch -q origin
if git merge -q --no-edit origin/main >/dev/null 2>&1; then
  fail "scenario 7: fixture expected a conflict to resolve"
fi
printf 'resolved\n' > README.txt
git add README.txt
git commit -q -m "merge main into staging; resolve README"
git push -q origin staging
before="$(remote_main)"
rc="$(train run --no-push)"
[ "$rc" = 0 ] || fail "scenario 7a: exit $rc ($(cat "$base/last.out"))"
grep -q '^merge-train: PASS (--no-push)' "$base/last.out" || fail "scenario 7a: no PASS (--no-push) line"
[ "$(remote_main)" = "$before" ] || fail "scenario 7a: --no-push moved main"
rc="$(train run)"
[ "$rc" = 0 ] || fail "scenario 7b: exit $rc ($(cat "$base/last.out"))"
grep -q '^merge-train: PROMOTED' "$base/last.out" || fail "scenario 7b: no PROMOTED line"
[ "$(git --git-dir="$remote" show "$(remote_main)":README.txt)" = resolved ] \
  || fail "scenario 7b: promoted tree lacks the resolution"
ok "7 --no-push proves without moving main; the real run promotes"

printf 'merge-train-selftest: all 7 scenarios passed (fixtures retained at %s)\n' "$base"
