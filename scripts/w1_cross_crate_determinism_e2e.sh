#!/usr/bin/env bash
# =============================================================================
# w1_cross_crate_determinism_e2e.sh — the workspace-wide determinism gate.
#
# Doctrine #4 ("deterministic by default") is asserted per-crate by the
# property suites, but nothing yet asserted it ACROSS crates: that one
# `cargo test --workspace` and the next produce the same verdict, for the same
# tree, on every engine crate at once. A crate whose suite silently stopped
# running, or whose aggregate count went stale while a test vanished, passes
# every per-crate gate we have. This script is that missing gate.
#
# What is and is NOT a determinism signal
# ---------------------------------------
# `cargo test` runs on N threads, so the EMISSION ORDER of `test foo ... ok`
# lines is thread scheduling, not behaviour — comparing raw output byte-for-
# byte is a guaranteed false red (observed: 842 of 1068 lines reorder between
# a default-threads run and a `--test-threads=1` run of an identical tree).
# Wall-clock (`finished in 0.01s`) is noise for the same reason. The real
# signal is the SET of (binary#block, test, outcome) triples plus the
# aggregate counts, and those are compared byte-for-byte after sorting.
#
# Blocks, not just binaries: rustdoc emits two `test result:` batches under one
# `Doc-tests <crate>` header (merged and unmerged doctests), so binary name
# alone is not an injective label — 41 binary names covered 48 result blocks.
# Each `running N tests` block is numbered within its binary.
#
# The concurrency confound (this bit is load-bearing)
# ---------------------------------------------------
# Other agents commit to this tree while the suite runs. A mid-flight edit
# makes run 1 and run 2 disagree for a reason that has nothing to do with
# determinism, and it looks EXACTLY like a real finding (observed: a pane
# landed a fgdb-calibrate fix between two runs; the three tests it repaired
# read as nondeterminism until the source was pinned). So the source tree is
# hashed before run 1, between the runs, and after run 2, and a change aborts
# the comparison as INDETERMINATE rather than reporting either colour.
#
# Modes
# -----
#   (no args)     capture two runs and compare        [needs a build slot]
#   --self-test   prove the comparator can go red     [no cargo, no slot]
#
# `--self-test` is not decoration. An oracle that cannot go red is
# indistinguishable from a pass, so the four mutants below (dropped binary,
# vanished test under a stale count, flipped outcome, reordered+retimed) are
# checked on every invocation of the real gate too.
#
# The evidence directory is intentionally retained. Repository policy forbids
# automated deletion, and the transcripts are useful for replay.
# =============================================================================

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Resolved to a real command, not a shell function: source_pin() pipes it
# through xargs, and xargs cannot invoke a function. Getting this wrong is
# silent — the pin comes back empty, every comparison of it succeeds, and the
# concurrency guard below is disabled without a single error message.
if command -v sha256sum >/dev/null 2>&1; then
  SHA256_CMD=(sha256sum)
else
  SHA256_CMD=(shasum -a 256)
fi

# --- the normalizer ----------------------------------------------------------
# Splits one raw `cargo test` transcript into three deterministic artifacts:
#   <out>.tests    sorted (binary#block, test, outcome) triples
#   <out>.results  aggregate `test result:` lines, timing stripped, emission order
#   <out>.bins     the sorted set of blocks that actually reported a result
normalize() {
  local raw="$1" out="$2"
  awk '
    /^[[:space:]]*Running / {
      bin = $0
      sub(/.*\/deps\//, "", bin)
      sub(/-[0-9a-f]+\)[[:space:]]*$/, "", bin)
      tgt = $0
      sub(/^[[:space:]]*Running[[:space:]]+/, "", tgt)
      sub(/[[:space:]]*\(.*$/, "", tgt)
      cur = bin " :: " tgt
      blk[cur] = 0
      next
    }
    /^[[:space:]]*Doc-tests / {
      d = $0
      sub(/^[[:space:]]*Doc-tests[[:space:]]+/, "", d)
      cur = d " :: doctest"
      blk[cur] = 0
      next
    }
    /^running [0-9]+ tests?$/ { blk[cur]++; next }
    /^test .* \.\.\. / {
      print "T\t" cur "#" blk[cur] "\t" $0 > TESTS
      next
    }
    /^test result:/ {
      line = $0
      sub(/;[[:space:]]*finished in [0-9]+(\.[0-9]+)?s[[:space:]]*$/, "", line)
      print "R\t" cur "#" blk[cur] "\t" line > RESULTS
      print cur "#" blk[cur] > BINS
      next
    }
  ' TESTS="$out.tests.raw" RESULTS="$out.results" BINS="$out.bins.raw" "$raw"

  : >>"$out.tests.raw"; : >>"$out.results"; : >>"$out.bins.raw"
  LC_ALL=C sort "$out.tests.raw" >"$out.tests"
  LC_ALL=C sort -u "$out.bins.raw" >"$out.bins"
  rm -f "$out.tests.raw" "$out.bins.raw"
}

# Compares two normalized runs. Returns 0 when every artifact matches.
compare_runs() {
  local a="$1" b="$2" label="$3" rc=0 part ha hb
  for part in results tests bins; do
    ha="$(LC_ALL=C "${SHA256_CMD[@]}" <"$a.$part" | cut -d' ' -f1)"
    hb="$(LC_ALL=C "${SHA256_CMD[@]}" <"$b.$part" | cut -d' ' -f1)"
    if [ "$ha" = "$hb" ]; then
      printf '    %-8s IDENTICAL  lines=%-6s sha256=%s\n' "$part" "$(wc -l <"$a.$part")" "$ha"
    else
      printf '    %-8s *** DRIFT ***  A=%s  B=%s\n' "$part" "$ha" "$hb"
      diff "$a.$part" "$b.$part" | head -20 || true
      rc=1
    fi
  done
  [ "$rc" -eq 0 ] || echo "ERROR: $label is not deterministic" >&2
  return "$rc"
}

# --- the red-proof -----------------------------------------------------------
# Four mutants over one synthetic transcript. Three MUST go red; the fourth
# (pure reordering and retiming) MUST stay green, because a gate that reds on
# thread scheduling would be turned off within a week.
self_test() {
  local d; d="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-det-selftest.XXXXXX")"
  cat >"$d/A.txt" <<'FIXTURE'
     Running unittests src/lib.rs (/tmp/target/debug/deps/fgdb_bigint-1a2b3c4d5e6f)

running 2 tests
test alpha ... ok
test beta ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

     Running tests/props.rs (/tmp/target/debug/deps/props-9f8e7d6c5b4a)

running 2 tests
test prop_one ... ok
test prop_two ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.44s
FIXTURE

  # M1 reorder + retime (must stay GREEN — scheduling is not drift)
  sed -e 's/^test alpha \.\.\. ok$/@@H@@/' -e 's/^test beta \.\.\. ok$/test alpha ... ok/' \
      -e 's/^@@H@@$/test beta ... ok/' -e 's/finished in 0\.01s/finished in 0.09s/' \
      "$d/A.txt" >"$d/M1.txt"
  # M2 flipped outcome (must go RED)
  sed -e 's/^test prop_two \.\.\. ok$/test prop_two ... FAILED/' "$d/A.txt" >"$d/M2.txt"
  # M3 vanished test, aggregate count left stale (must go RED on .tests alone)
  grep -v '^test beta \.\.\. ok$' "$d/A.txt" >"$d/M3.txt"
  # M4 an entire binary never ran (must go RED — the "unrun suite" shape)
  awk '/Running tests\/props.rs/{skip=1} skip&&/^$/{next} !skip' "$d/A.txt" >"$d/M4.txt"

  normalize "$d/A.txt" "$d/a"
  local m expect rc fails=0
  for m in M1:green M2:red M3:red M4:red; do
    expect="${m#*:}"; m="${m%%:*}"
    normalize "$d/$m.txt" "$d/m"
    if compare_runs "$d/a" "$d/m" "$m" >/dev/null 2>&1; then rc=green; else rc=red; fi
    if [ "$rc" = "$expect" ]; then
      printf '    %-4s expected %-5s got %-5s OK\n' "$m" "$expect" "$rc"
    else
      printf '    %-4s expected %-5s got %-5s MUTANT SURVIVED\n' "$m" "$expect" "$rc"
      fails=$((fails + 1))
    fi
  done
  if [ "$fails" -ne 0 ]; then
    echo "ERROR: the determinism comparator cannot detect $fails of its own mutants" >&2
    echo "retained self-test evidence: $d" >&2
    return 1
  fi
  echo "    comparator red-proof GREEN; retained evidence: $d"
}

# Hash of every input that can legitimately change the verdict. Used to detect
# a concurrent agent committing to the tree mid-run.
# A pin that fails must fail loudly: stderr is NOT swallowed, and an empty or
# short digest aborts. An empty pin compares equal to the next empty pin, which
# would turn the guard below into a no-op that always reports "unchanged".
source_pin() {
  local pin
  pin="$(find crates tools \( -name '*.rs' -o -name 'Cargo.toml' \) -type f \
    | LC_ALL=C sort | xargs "${SHA256_CMD[@]}" \
    | "${SHA256_CMD[@]}" - | cut -d' ' -f1)"
  if [ "${#pin}" -ne 64 ]; then
    echo "ERROR: source_pin produced no usable digest ('$pin'); refusing to run" >&2
    echo "  Without a pin, a concurrent commit is indistinguishable from nondeterminism." >&2
    return 1
  fi
  printf '%s\n' "$pin"
}

if [ "${1:-}" = "--self-test" ]; then
  echo "==> comparator red-proof (no cargo)"
  self_test
  echo "cross-crate determinism SELF-TEST GREEN"
  exit 0
fi

EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-det-e2e.XXXXXX")"

echo "==> comparator red-proof (the gate refuses to run an oracle it has not seen go red)"
self_test

echo "==> pin the source tree"
PIN_BEFORE="$(source_pin)"
HEAD_BEFORE="$(git rev-parse HEAD 2>/dev/null || echo 'not-a-git-tree')"
echo "    HEAD=$HEAD_BEFORE source=$PIN_BEFORE"

echo "==> workspace test run 1 of 2"
cargo test --locked --workspace --no-fail-fast >"$EVIDENCE_DIR/run1.txt" 2>&1 || {
  echo "ERROR: run 1 did not pass; determinism is not the question yet" >&2
  grep -E '^(test result: FAILED|error)' "$EVIDENCE_DIR/run1.txt" | head -20 >&2 || true
  echo "retained evidence: $EVIDENCE_DIR" >&2
  exit 1
}

PIN_MID="$(source_pin)"
if [ "$PIN_MID" != "$PIN_BEFORE" ]; then
  echo "INDETERMINATE: the source tree changed during run 1 (another agent committed)." >&2
  echo "  before=$PIN_BEFORE  after=$PIN_MID" >&2
  echo "  This is NOT a determinism finding. Re-run against a quiet tree." >&2
  echo "retained evidence: $EVIDENCE_DIR" >&2
  exit 2
fi

echo "==> workspace test run 2 of 2"
cargo test --locked --workspace --no-fail-fast >"$EVIDENCE_DIR/run2.txt" 2>&1 || {
  echo "ERROR: run 2 did not pass although run 1 did — that is itself nondeterminism" >&2
  grep -E '^(test result: FAILED|error)' "$EVIDENCE_DIR/run2.txt" | head -20 >&2 || true
  echo "retained evidence: $EVIDENCE_DIR" >&2
  exit 1
}

PIN_AFTER="$(source_pin)"
if [ "$PIN_AFTER" != "$PIN_BEFORE" ]; then
  echo "INDETERMINATE: the source tree changed during run 2 (another agent committed)." >&2
  echo "  before=$PIN_BEFORE  after=$PIN_AFTER" >&2
  echo "  This is NOT a determinism finding. Re-run against a quiet tree." >&2
  echo "retained evidence: $EVIDENCE_DIR" >&2
  exit 2
fi

echo "==> normalize and byte-compare the two runs"
normalize "$EVIDENCE_DIR/run1.txt" "$EVIDENCE_DIR/n1"
normalize "$EVIDENCE_DIR/run2.txt" "$EVIDENCE_DIR/n2"
compare_runs "$EVIDENCE_DIR/n1" "$EVIDENCE_DIR/n2" "the workspace suite"

echo "==> assert no test reported anything but ok"
if grep -qE '\.\.\. (FAILED|ignored)$' "$EVIDENCE_DIR/n1.tests"; then
  echo "ERROR: a test is FAILED or ignored — the gate is green-bar only" >&2
  grep -E '\.\.\. (FAILED|ignored)$' "$EVIDENCE_DIR/n1.tests" >&2
  exit 1
fi
if grep -vq 'test result: ok\.' "$EVIDENCE_DIR/n1.results"; then
  echo "ERROR: an aggregate result line is not ok" >&2
  grep -v 'test result: ok\.' "$EVIDENCE_DIR/n1.results" >&2
  exit 1
fi

# Coverage is derived from the workspace membership, never from a pinned count:
# a hard-coded expected total is exactly the check that goes stale and starts
# certifying a crate whose suite stopped running.
echo "==> assert every engine crate actually reported a green lib suite"
MISSING=0
while read -r crate; do
  [ -n "$crate" ] || continue
  underscored="${crate//-/_}"
  if ! grep -q "^R	${underscored} :: unittests src/lib.rs#1	test result: ok\." "$EVIDENCE_DIR/n1.results"; then
    echo "ERROR: $crate reported no green lib test block — it did not run" >&2
    MISSING=$((MISSING + 1))
  fi
done < <(sed -n 's#^ *"crates/\([a-z0-9-]*\)".*#\1#p' Cargo.toml)
[ "$MISSING" -eq 0 ] || { echo "retained evidence: $EVIDENCE_DIR" >&2; exit 1; }

CRATES="$(sed -n 's#^ *"crates/\([a-z0-9-]*\)".*#\1#p' Cargo.toml | wc -l)"
BLOCKS="$(wc -l <"$EVIDENCE_DIR/n1.bins")"
TESTS="$(wc -l <"$EVIDENCE_DIR/n1.tests")"
SET_HASH="$(LC_ALL=C "${SHA256_CMD[@]}" <"$EVIDENCE_DIR/n1.tests" | cut -d' ' -f1)"

echo "==> sorted-set hash over $TESTS (binary#block, test, outcome) triples"
echo "    $SET_HASH"
echo "cross-crate determinism E2E GREEN: $CRATES engine crates, $BLOCKS result blocks, $TESTS tests"
echo "  HEAD=$HEAD_BEFORE source=$PIN_BEFORE"
echo "  retained deterministic evidence: $EVIDENCE_DIR"
