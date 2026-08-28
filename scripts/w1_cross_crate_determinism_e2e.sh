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

# The shared verdict contract (fgdb-udco). Fail-fast under `set -e` with no
# assertion-level emitter: the EXIT trap derives the `FAIL` line on stdout from
# the exit code. The existing `echo "ERROR: ..." >&2` sites are diagnostics and
# stay unchanged.
# shellcheck source=lib/gate_verdict.sh
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_init "w1_cross_crate_determinism_e2e"

# Resolved to a real command, not a shell function: source_pin() pipes it
# through xargs, and xargs cannot invoke a function. Getting this wrong fails
# in the worst possible way — measured, not theorised. `xargs some_function`
# writes nothing to stdout, the trailing hash then digests an EMPTY STREAM, and
# the pin comes back as the constant e3b0c442…b855 (sha256 of ""). That is 64
# valid hex characters, identical on every tree, so it passes a length check
# and every before/after comparison succeeds: the concurrency guard is disabled
# with no error and no visible symptom. Hence both guards in source_pin below.
readonly EMPTY_SHA256="e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
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
    { gsub(/\x1b\[[0-9;]*[a-zA-Z]/, "") }
    /^[[:space:]]*Running / {
      bin = $0
      sub(/^.*\(/, "", bin)
      sub(/\)[[:space:]]*$/, "", bin)
      sub(/.*\//, "", bin)
      sub(/-[0-9a-f]+$/, "", bin)
      tgt = $0
      sub(/^[[:space:]]*Running[[:space:]]+/, "", tgt)
      sub(/[[:space:]]*\(.*$/, "", tgt)
      print bin " :: " tgt > BINS
      next
    }
    /^[[:space:]]*Doc-tests / {
      d = $0
      sub(/^[[:space:]]*Doc-tests[[:space:]]+/, "", d)
      print d " :: doctest" > BINS
      next
    }
    /^test .* \.\.\. / {
      print $0 > TESTS
      next
    }
    /^test result:/ {
      line = $0
      sub(/;[[:space:]]*finished in [0-9]+(\.[0-9]+)?s[[:space:]]*$/, "", line)
      print line > RESULTS
      next
    }
  ' TESTS="$out.tests.raw" RESULTS="$out.results.raw" BINS="$out.bins.raw" "$raw"

  : >>"$out.tests.raw"; : >>"$out.results.raw"; : >>"$out.bins.raw"
  LC_ALL=C sort "$out.tests.raw" >"$out.tests"
  LC_ALL=C sort "$out.results.raw" >"$out.results"
  LC_ALL=C sort -u "$out.bins.raw" >"$out.bins"
  rm -f "$out.tests.raw" "$out.results.raw" "$out.bins.raw"
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

# The measured shared-target race has two narrow compiler-driver signatures:
# rustc says outright that an `--extern` artifact no longer exists, while
# rustdoc reports E0463 and prints the `--extern` artifact only in its failed
# command line. In the rustdoc form, require the E0463 crate name and the
# command-line `--extern` name to agree; a bare E0463 remains a real failure.
# Neither form is a failed test or comparison drift: the workspace assertions
# never completed. Keep the classifier deliberately narrower than generic
# "No such file" or "can't find crate" text so a missing source/dependency
# declaration and an ordinary failing test remain RED.
cargo_extern_artifact_disappeared() { # cargo-test-log
  local log="$1"

  [ -f "$log" ] || return 1
  awk '
    BEGIN {
      apostrophe = sprintf("%c", 39)
      e0463_prefix = "error[E0463]: can" apostrophe "t find crate for `"
      rustdoc_failure = "process didn" apostrophe "t exit successfully: "
    }
    index($0, e0463_prefix) == 1 && $0 ~ /`$/ {
      crate = substr($0, length(e0463_prefix) + 1)
      sub(/`$/, "", crate)
      if (crate ~ /^[[:alnum:]_]+$/) {
        rustdoc_missing[crate] = 1
      }
    }
    /^error: extern location for [[:alnum:]_]+ does not exist:$/ {
      if ((getline artifact) > 0 \
          && artifact ~ /^[[:space:]]+.*\/(debug|release)\/deps\/lib[^[:space:]]+\.(rlib|rmeta)$/) {
        found = 1
      }
    }
    index($0, rustdoc_failure) > 0 && $0 ~ /\/rustdoc / {
      for (crate in rustdoc_missing) {
        marker = "--extern " crate "="
        marker_at = index($0, marker)
        if (marker_at == 0) {
          continue
        }
        artifact = substr($0, marker_at + length(marker))
        sub(/[[:space:]`].*$/, "", artifact)
        if (artifact ~ /\/(debug|release)\/deps\/lib[^[:space:]`]+\.(rlib|rmeta)$/) {
          found = 1
        }
      }
    }
    END {
      if (!found) {
        exit 1
      }
    }
  ' "$log"
}

workspace_test_failed() { # stage log exit-code genuine-failure-diagnostic
  local stage="$1"
  local log="$2"
  local rc="$3"
  local failure_diagnostic="$4"

  # fgdb-950i: an rch refusal or a cold-worker offline failure means cargo
  # never executed, so no verdict — product or otherwise — is attributable.
  # Classify before any logic below; gate_abort_unrun exits 2 on a match, so
  # everything underneath is reached only by genuine product failures.
  case "$(gate_env_failure_class "$log")" in
    rch-refusal|cargo-offline)
      gate_diag "retained evidence: $EVIDENCE_DIR"
      gate_abort_unrun "$stage did not execute ($(gate_env_failure_class "$log")); retryable environment refusal, not a product verdict"
      ;;
  esac

  # A failing run must not be classified RED while the tree it ran against was
  # mutating underneath it. The mid/after pin checks below the run sites never
  # execute on this path — the failure handler preempts them — which is exactly
  # how fgdb-gfim reported nondeterminism: a concurrent checkout swapped
  # registries/ mid-run-2, run 2 went red, and the gate blamed determinism
  # before the PIN_AFTER comparison could say INDETERMINATE. Re-check first.
  # A pin that cannot be computed skips this guard and falls through to the
  # red classification: masking a real failure is worse than missing an abort.
  local pin_now
  pin_now="$(source_pin || true)"
  if [ -n "$pin_now" ] && [ "$pin_now" != "$PIN_BEFORE" ]; then
    gate_diag "INDETERMINATE: the source tree changed during $stage (another agent committed)."
    gate_diag "  before=$PIN_BEFORE  now=$pin_now"
    gate_diag "  The failure cannot be attributed; this is NOT a determinism finding."
    gate_diag "retained evidence: $EVIDENCE_DIR"
    gate_abort_unrun "$stage: source tree changed mid-run; the failure cannot be attributed"
  fi

  if cargo_extern_artifact_disappeared "$log"; then
    gate_diag "INDETERMINATE: a Cargo dependency artifact disappeared during $stage."
    gate_diag "  The workspace assertions did not complete; this is NOT a determinism finding."
    grep -E -A1 \
      "^error: extern location for .* does not exist:$|^error\\[E0463\\]: can't find crate for " \
      "$log" \
      | head -10 >&2 || true
    grep "process didn't exit successfully: .*/rustdoc .*--extern " "$log" \
      | head -2 >&2 || true
    gate_diag "retained evidence: $EVIDENCE_DIR"
    gate_abort_unrun "$stage: Cargo dependency artifact disappeared before the workspace suite completed"
  fi

  gate_diag "$failure_diagnostic (exit $rc)"
  grep -E '^(test result: FAILED|error)' "$log" | head -20 >&2 || true
  gate_diag "retained evidence: $EVIDENCE_DIR"
  gate_die "$stage failed before the determinism gate could pass"
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

  # The environmental classifier must identify both measured missing-rlib
  # signatures and reject nearby real failures. In particular, a bare E0463,
  # an E0463 whose crate does not match the rustdoc --extern name, and a
  # non-target dependency path must not borrow UNRUN.
  cat >"$d/artifact-missing.txt" <<'FIXTURE'
error: extern location for fgdb_bigint does not exist:
  /data/tmp/cargo-target/debug/deps/libfgdb_bigint-20abfb7010900144.rlib
error: aborting due to 1 previous error
FIXTURE
  cat >"$d/rustdoc-artifact-missing.txt" <<'FIXTURE'
error[E0463]: can't find crate for `fgdb_types`
  --> crates/fgdb-codec/src/identity.rs:20:5
   |
20 | use fgdb_types::{CommitSeq, EId, VId};
   |     ^^^^^^^^^^ can't find crate
error: aborting due to 1 previous error
Caused by:
  process didn't exit successfully: `/toolchains/nightly/bin/rustdoc --edition=2024 --crate-name fgdb_codec --test crates/fgdb-codec/src/lib.rs --extern fgdb_codec=/data/tmp/cargo-target/debug/deps/libfgdb_codec-9702cb88adbcc8c6.rlib --extern fgdb_types=/data/tmp/cargo-target/debug/deps/libfgdb_types-570c87d0bb9d3d62.rlib -L dependency=/data/tmp/cargo-target/debug/deps` (exit status: 1)
FIXTURE
  cat >"$d/rustdoc-e0463-only.txt" <<'FIXTURE'
error[E0463]: can't find crate for `fgdb_types`
  --> crates/fgdb-codec/src/identity.rs:20:5
error: aborting due to 1 previous error
FIXTURE
  cat >"$d/rustdoc-wrong-extern.txt" <<'FIXTURE'
error[E0463]: can't find crate for `fgdb_types`
error: aborting due to 1 previous error
Caused by:
  process didn't exit successfully: `/toolchains/nightly/bin/rustdoc --edition=2024 --crate-name fgdb_codec --test crates/fgdb-codec/src/lib.rs --extern fgdb_evidence=/data/tmp/cargo-target/debug/deps/libfgdb_evidence-5cced8237e6720e5.rlib` (exit status: 1)
FIXTURE
  cat >"$d/rustdoc-nontarget-extern.txt" <<'FIXTURE'
error[E0463]: can't find crate for `fgdb_types`
error: aborting due to 1 previous error
Caused by:
  process didn't exit successfully: `/toolchains/nightly/bin/rustdoc --edition=2024 --crate-name fgdb_codec --test crates/fgdb-codec/src/lib.rs --extern fgdb_types=/workspace/vendor/libfgdb_types.rlib` (exit status: 1)
FIXTURE
  cat >"$d/compiler-error.txt" <<'FIXTURE'
error[E0308]: mismatched types
  --> crates/fgdb-bigint/src/lib.rs:42:9
error: aborting due to 1 previous error
FIXTURE
  cat >"$d/test-failure.txt" <<'FIXTURE'
test result: FAILED. 41 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
error: test failed, to rerun pass `-p fgdb-bigint --lib`
FIXTURE
  local classifier_case expect_classifier got_classifier
  for classifier_case in \
    artifact-missing:match \
    rustdoc-artifact-missing:match \
    rustdoc-e0463-only:reject \
    rustdoc-wrong-extern:reject \
    rustdoc-nontarget-extern:reject \
    compiler-error:reject \
    test-failure:reject; do
    expect_classifier="${classifier_case#*:}"
    classifier_case="${classifier_case%%:*}"
    if cargo_extern_artifact_disappeared "$d/$classifier_case.txt"; then
      got_classifier=match
    else
      got_classifier=reject
    fi
    if [ "$got_classifier" = "$expect_classifier" ]; then
      printf '    %-16s expected %-6s got %-6s OK\n' \
        "$classifier_case" "$expect_classifier" "$got_classifier"
    else
      printf '    %-16s expected %-6s got %-6s MISCLASSIFIED\n' \
        "$classifier_case" "$expect_classifier" "$got_classifier"
      fails=$((fails + 1))
    fi
  done
  if [ "$fails" -ne 0 ]; then
    echo "ERROR: $fails determinism/classification controls failed" >&2
    echo "retained self-test evidence: $d" >&2
    return 1
  fi
  gate_pass "determinism comparator mutants and Cargo artifact-race classifier controls"
  echo "    retained self-test evidence: $d"
}

# Hash of every input that can legitimately change the verdict. Used to detect
# a concurrent agent committing to the tree mid-run.
# A pin that fails must fail loudly: stderr is NOT swallowed, and an empty or
# short digest aborts. An empty pin compares equal to the next empty pin, which
# would turn the guard below into a no-op that always reports "unchanged".
#
# COVERAGE IS RUNTIME INPUTS, NOT JUST COMPILED SOURCE (fgdb-gfim, measured
# 2026-08-05). The workspace suite READS the tree at run time: registry-check
# alone consumes registries/**, .beads/issues.jsonl, AGENTS.md, the plan
# document, docs/*.md and scripts/*.sh, and its corpus fixtures. A pin
# restricted to crates/tools *.rs+Cargo.toml certified "unchanged" while a
# concurrent `git checkout` in the same root swapped registries/ to a
# three-week-old vintage between run 1 and run 2 — the identical binary then
# failed 8 targets, reading as nondeterminism. So the pin is the union of the
# tracked file set (git ls-files, total over every runtime input in a git
# tree) and an extension-blind sweep of crates/tools/registries (which also
# catches an UNTRACKED file cargo would still compile — a property the
# tracked set alone loses).
source_pin() {
  local pin files count
  files="$(
    {
      find crates tools registries -type f 2>/dev/null
      git ls-files 2>/dev/null
      printf '%s\n' Cargo.toml Cargo.lock
    } | LC_ALL=C sort -u | while IFS= read -r f; do
      if [ -f "$f" ]; then printf '%s\n' "$f"; fi
    done
  )"
  count="$(printf '%s' "$files" | grep -c . || true)"
  # An empty walk is never legitimate here and must not be pinnable. GNU xargs
  # runs its command ONCE even with no input, so `find | xargs sha256sum` over
  # an empty tree still emits a digest — another tree-independent constant that
  # sails past both checks below. Assert the walk found something first.
  if [ "$count" -lt 2 ]; then
    echo "ERROR: source_pin matched $count source files; refusing to pin." >&2
    echo "  Run from the repository root — an empty walk yields a constant digest." >&2
    return 1
  fi
  pin="$(printf '%s\n' "$files" | xargs -r "${SHA256_CMD[@]}" \
    | "${SHA256_CMD[@]}" - | cut -d' ' -f1)"
  if [ "${#pin}" -ne 64 ]; then
    echo "ERROR: source_pin produced no usable digest ('$pin'); refusing to run" >&2
    echo "  Without a pin, a concurrent commit is indistinguishable from nondeterminism." >&2
    return 1
  fi
  if [ "$pin" = "$EMPTY_SHA256" ]; then
    echo "ERROR: source_pin digested an empty stream (got sha256 of \"\")." >&2
    echo "  The file walk matched nothing, or the hash command was not invoked." >&2
    echo "  This is length-check-proof: refusing to run rather than pin a constant." >&2
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

echo "==> prebuild workspace test binaries"
cargo test --no-run --locked --workspace

echo "==> workspace test run 1 of 2"
if CARGO_TERM_COLOR=never cargo test -j 1 --color=never --locked --workspace --no-fail-fast \
    >"$EVIDENCE_DIR/run1.txt" 2>&1; then
  :
else
  workspace_test_failed "run 1" "$EVIDENCE_DIR/run1.txt" "$?" \
    "ERROR: run 1 did not pass; determinism is not the question yet"
fi

PIN_MID="$(source_pin)"
if [ "$PIN_MID" != "$PIN_BEFORE" ]; then
  gate_diag "INDETERMINATE: the source tree changed during run 1 (another agent committed)."
  gate_diag "  before=$PIN_BEFORE  after=$PIN_MID"
  gate_diag "  This is NOT a determinism finding. Re-run against a quiet tree."
  gate_diag "retained evidence: $EVIDENCE_DIR"
  gate_abort_unrun "source tree changed during run 1; the determinism comparison did not run"
fi

echo "==> workspace test run 2 of 2"
if CARGO_TERM_COLOR=never cargo test -j 1 --color=never --locked --workspace --no-fail-fast \
    >"$EVIDENCE_DIR/run2.txt" 2>&1; then
  :
else
  workspace_test_failed "run 2" "$EVIDENCE_DIR/run2.txt" "$?" \
    "ERROR: run 2 did not pass although run 1 did — that is itself nondeterminism"
fi

PIN_AFTER="$(source_pin)"
if [ "$PIN_AFTER" != "$PIN_BEFORE" ]; then
  gate_diag "INDETERMINATE: the source tree changed during run 2 (another agent committed)."
  gate_diag "  before=$PIN_BEFORE  after=$PIN_AFTER"
  gate_diag "  This is NOT a determinism finding. Re-run against a quiet tree."
  gate_diag "retained evidence: $EVIDENCE_DIR"
  gate_abort_unrun "source tree changed during run 2; the determinism comparison did not run"
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
  if ! grep -qE "^${underscored} :: unittests src/lib\.rs$" "$EVIDENCE_DIR/n1.bins"; then
    echo "ERROR: $crate reported no green lib test block — it did not run" >&2
    MISSING=$((MISSING + 1))
  fi
done < <(sed -n 's#^ *"crates/\([a-z0-9-]*\)".*#\1#p' Cargo.toml)
[ "$MISSING" -eq 0 ] || { echo "retained evidence: $EVIDENCE_DIR" >&2; exit 1; }

CRATES="$(sed -n 's#^ *"crates/\([a-z0-9-]*\)".*#\1#p' Cargo.toml | wc -l)"
BLOCKS="$(wc -l <"$EVIDENCE_DIR/n1.bins")"
TESTS="$(wc -l <"$EVIDENCE_DIR/n1.tests")"
SET_HASH="$(LC_ALL=C "${SHA256_CMD[@]}" <"$EVIDENCE_DIR/n1.tests" | cut -d' ' -f1)"

echo "==> sorted-set hash over $TESTS tests across $BLOCKS binaries/doctests"
echo "    $SET_HASH"
gate_pass "cross-crate determinism E2E GREEN: $CRATES engine crates, $BLOCKS result blocks, $TESTS tests"
echo "  HEAD=$HEAD_BEFORE source=$PIN_BEFORE"
echo "  retained deterministic evidence: $EVIDENCE_DIR"
