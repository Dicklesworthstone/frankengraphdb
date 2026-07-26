#!/usr/bin/env bash
# =============================================================================
# g0_spine_e2e.sh — end-to-end proof of the twenty-invariant spine
# (bead fgdb-g0-invariant-spine-tmm)
#
# Verifies the materialized registry: the twenty-ID table hash, resolution of
# every checker and negative-test symbol (stub-registered pre-Genesis), the
# activation closure for the sample capability manifest, and both negative
# fixtures (a twenty-first ID; a reachable-but-inactive clause), asserting
# each fails naming the exact clause. JSONL evidence retained for later gates
# to diff activation drift against this baseline.
#
# Two properties of this harness are load-bearing and were repaired under bead
# fgdb-g0-spine-e2e-red-measures-harness-not-spine-iy7e:
#
#   * The subject artifact is compiled from THIS tree into a private path. It
#     used to be resolved by path out of the shared CARGO_TARGET_DIR, so the
#     gate reported on whichever artifact happened to be there.
#   * Phase 3's negative assertion is attributed: it requires the closure to
#     have been COMPUTED and to name a non-live reachable clause. It used to
#     accept any non-zero exit, and so passed on a fixture that never parsed —
#     the assertion whose whole job is proving that failure is detectable was
#     itself succeeding for the wrong reason. Phase 3 carries an in-band
#     control that re-runs the same classifier on exactly that malformed
#     fixture and requires a different verdict.
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${G0_E2E_WORKDIR:-$(mktemp -d)}"
BIN="$WORK/bin/registry-check"
PASS=0
FAIL=0

log() { printf '[g0-spine-e2e] %s\n' "$*"; }
ok()  { PASS=$((PASS + 1)); log "PASS: $*"; }
die() { FAIL=$((FAIL + 1)); log "FAIL: $*"; }

log "work directory: $WORK"
mkdir -p "$WORK/bin"

# --- The subject artifact ----------------------------------------------------
# BIN used to be "${CARGO_TARGET_DIR:-$ROOT/target}/debug/registry-check",
# gated only on `[ -x "$BIN" ]`, with a build step whose exit status nothing
# read. /data/tmp/cargo-target is written by six panes and three other
# projects, so that path names an artifact compiled from a state this repo may
# never have had, and the build step could not fail the run.
#
# MEASURED (2026-07-26, bead iy7e): one such artifact carried
# `wire_types` pin fnv1a64:0f3dcd03f7a9eaf7 — a value that occurs nowhere in
# the tracked tree and nowhere in identity.rs history — and reported 24
# violations against a green tree. Sharper still: this script returned
# "7 passed, 3 failed" at 19:22 and "8 passed, 2 failed" at 19:24 with no
# change to the repo between the runs, because another pane rebuilt the shared
# artifact in between. A verdict that moves when the subject does not is not a
# measurement of the subject.
#
# registry-check is std-only by constitution (FG-CON-01: the closed dependency
# universe applies to the tooling that enforces it), so it has no dependencies
# to resolve and the gate compiles it straight from this tree with rustc into
# $WORK — no cargo, no shared directory, no package-cache lock, ~8s. Running
# from $ROOT makes rustup honour rust-toolchain.toml.
#
# The price of that hermeticity is disk: a private build cannot share the
# swarm's artifact, so each run leaves 73MB in its workdir (a 64MB rlib the
# `-C strip=symbols` below cannot shrink, plus an 8MB binary it takes from
# 18MB). Measured 2026-07-26 on a filesystem at 95% with 9594 stale
# `/data/tmp/tmp.*` directories from every gate that mktemps. Reaping those is
# swarm hygiene, not this gate's business — and this script deletes nothing.
log "building registry-check from this tree into $WORK/bin"
if ! (cd "$ROOT" \
      && rustc --edition 2024 --crate-type rlib --crate-name registry_check \
           -C strip=symbols \
           tools/registry-check/src/lib.rs -o "$WORK/bin/libregistry_check.rlib" \
      && rustc --edition 2024 -C strip=symbols tools/registry-check/src/main.rs \
           --extern "registry_check=$WORK/bin/libregistry_check.rlib" \
           -o "$BIN") >"$WORK/build.log" 2>&1; then
  log "FATAL: building registry-check from this tree failed (see $WORK/build.log)"
  exit 2
fi
[ -x "$BIN" ] || { log "FATAL: no subject artifact at $BIN after a build that reported success"; exit 2; }

# Freshness is asserted, not assumed. This is the exact property the old build
# step lacked: cargo printed an error, exited 0, left a stale artifact in
# place, and the gate proceeded to report on it.
NEWEST_SRC="$(ls -t "$ROOT"/tools/registry-check/src/*.rs "$ROOT"/tools/registry-check/src/bin/*.rs | head -1)"
[ "$BIN" -nt "$NEWEST_SRC" ] || {
  log "FATAL: $BIN is not newer than $NEWEST_SRC — the build did not produce this tree's artifact"
  exit 2
}
log "subject artifact: $BIN (newer than $NEWEST_SRC)"

# --- Phase 1: materialized spine passes validate + hash + closure ------------
log "phase 1: materialized spine (validate + hash + closure baseline)"
if "$BIN" all --root "$ROOT" >"$WORK/spine-baseline.jsonl" 2>"$WORK/spine-baseline.err"; then
  ok "materialized spine passes validate/hash/lint/closure"
else
  die "materialized spine failed (see $WORK/spine-baseline.jsonl)"
fi
CLAUSES=$(grep -c '"event":"clause_checked"' "$WORK/spine-baseline.jsonl" || true)
if [ "$CLAUSES" -ge 20 ]; then
  ok "clause_checked events for all materialized clauses ($CLAUSES >= 20)"
else
  die "expected >= 20 clause_checked events, found $CLAUSES"
fi
grep -q '"event":"hash_checked".*"outcome":"pass"' "$WORK/spine-baseline.jsonl" \
  && ok "twenty-ID table hash verified" \
  || die "twenty-ID table hash not verified"
grep -q '"event":"closure_computed".*"absent":0.*"outcome":"pass"' "$WORK/spine-baseline.jsonl" \
  && ok "pre-Genesis sample-manifest closure satisfied (no reachable stubs)" \
  || die "baseline closure not satisfied"
if grep -q '"code":"missing_checker"' "$WORK/spine-baseline.jsonl"; then
  die "unresolvable checker/negative-test symbol on the shipped spine"
else
  ok "every checker and negative-test symbol resolves (stub-registered)"
fi

# --- Phase 2: twenty-first ID fails naming the exact row ---------------------
log "phase 2: planted twenty-first invariant ID"
SPINE="$WORK/spine-stage"
mkdir -p "$SPINE/registries"
cp "$ROOT"/registries/*.toml "$SPINE/registries/"
cat >> "$SPINE/registries/invariants.toml" <<'EOF'

[[invariant]]
id = "FG-INV-21"
title = "planted illegal twenty-first row"
EOF
if "$BIN" validate --root "$SPINE" >"$WORK/spine-neg-21.jsonl" 2>/dev/null; then
  die "validate passed despite twenty-first ID"
else
  ok "validate failed as required on twenty-first ID"
fi
grep -q '"code":"twenty_id_violation".*FG-INV-21' "$WORK/spine-neg-21.jsonl" \
  && ok "violation names FG-INV-21 exactly" \
  || die "twenty_id_violation missing FG-INV-21 (see $WORK/spine-neg-21.jsonl)"

# --- Phase 3: reachable-but-inactive clause forces the capability off --------
log "phase 3: capability manifest enabling a stub-guarded feature"

# write_hot_manifest <path> <expected_reachable_clauses | omit>
# One writer for the live fixture and for the control below, so the control
# cannot differ from the fixture in any way except the one it is controlling.
write_hot_manifest() {
  local path="$1" expected="$2"
  cat > "$path" <<'EOF'
schema_version = 1
[manifest]
name = "e2e-hot"
description = "enables mvcc-visibility before its checker is live"
features = ["mvcc-visibility"]
postures = []
roles = []
EOF
  [ "$expected" = "omit" ] || printf 'expected_reachable_clauses = %s\n' "$expected" >> "$path"
}

# classify_closure_run <jsonl> <exit-code>  ->  one verdict token on stdout
#
# ONE READER. The live fixture and the vacuity control are both judged here, so
# the control cannot pass by exercising a rule the assertion does not use.
#
# The distinction this makes is the whole point of the phase. A non-zero exit
# from `closure` means one of: the manifest never parsed (exit 2, run_error);
# the closure ran and something other than a non-live reachable clause failed
# it (a count mismatch, an undeclared atom); or the law under test fired. Only
# the last one is evidence about the reachable-stub law.
classify_closure_run() {
  local jsonl="$1" rc="$2" absent
  if [ "$rc" -eq 0 ]; then echo "closure_passed"; return; fi
  if grep -q '"event":"run_error"' "$jsonl"; then echo "never_ran"; return; fi
  if ! grep -q '"event":"closure_computed"' "$jsonl"; then echo "no_closure"; return; fi
  absent="$(sed -n 's/.*"event":"closure_computed".*"absent":\([0-9][0-9]*\).*/\1/p' "$jsonl" | head -1)"
  if [ "${absent:-0}" -lt 1 ]; then echo "failed_elsewhere"; return; fi
  echo "stub_absent"
}

# The fixture's own `expected_reachable_clauses` is DERIVED, not frozen.
# 948e1a5 (bead fgdb-regcheck-closure-vacuous-no-control-hp0f) made the key
# required and swept registries/sample_capability_manifest.toml, but not this
# heredoc — which is how two assertions below went red for a reason that has
# nothing to do with the law they test. Freezing a number here would only
# re-arm that trap: this fixture's subject is the reachable-STUB law, and the
# count law already has its own test site — the shipped sample manifest phase 1
# checks, whose zero is licensed by closure_self_test. So the number is read
# back from the closure report for this very manifest. Deriving it cannot hide
# a broken closure compiler: a compiler that reached nothing would report
# absent=0, and the attributed assertion below rejects that outright.
write_hot_manifest "$WORK/hot-probe.toml" 0
"$BIN" closure --root "$ROOT" --manifest "$WORK/hot-probe.toml" \
  >"$WORK/spine-closure-probe.jsonl" 2>/dev/null || true
REACHED="$(sed -n 's/.*"event":"closure_computed".*"reachable":\([0-9][0-9]*\).*/\1/p' \
  "$WORK/spine-closure-probe.jsonl" | head -1)"
if [ -z "$REACHED" ]; then
  die "cannot derive expected_reachable_clauses: probe emitted no closure_computed (see $WORK/spine-closure-probe.jsonl)"
  REACHED=0
fi
log "derived expected_reachable_clauses=$REACHED for the hot manifest"

# CONTROL, reported before the measurement it licenses, exactly as the binary
# reports closure_self_test before its own closure verdicts. Until bead iy7e
# the assertion below was `if "$BIN" closure …; then die; else ok; fi`, which
# any non-zero exit satisfied — including the exit 2 this control produces, on
# an input the closure compiler never read. Revert the classifier to "non-zero
# exit ⇒ pass" and this line goes red.
write_hot_manifest "$WORK/hot-manifest-unparseable.toml" omit
CONTROL_RC=0
"$BIN" closure --root "$ROOT" --manifest "$WORK/hot-manifest-unparseable.toml" \
  >"$WORK/spine-closure-control.jsonl" 2>/dev/null || CONTROL_RC=$?
CONTROL_VERDICT="$(classify_closure_run "$WORK/spine-closure-control.jsonl" "$CONTROL_RC")"
if [ "$CONTROL_RC" -ne 0 ] && [ "$CONTROL_VERDICT" = "never_ran" ]; then
  ok "control: a fixture that never parsed classifies never_ran, not stub_absent (exit $CONTROL_RC)"
else
  die "control: malformed fixture classified $CONTROL_VERDICT (exit $CONTROL_RC); the reachable-stub assertion below is vacuous"
fi

write_hot_manifest "$WORK/hot-manifest.toml" "$REACHED"
CLOSURE_RC=0
"$BIN" closure --root "$ROOT" --manifest "$WORK/hot-manifest.toml" \
  >"$WORK/spine-closure-hot.jsonl" 2>/dev/null || CLOSURE_RC=$?
CLOSURE_VERDICT="$(classify_closure_run "$WORK/spine-closure-hot.jsonl" "$CLOSURE_RC")"
case "$CLOSURE_VERDICT" in
  stub_absent)
    ok "closure failed on the reachable stub clause itself (exit $CLOSURE_RC, closure_computed reports absent >= 1)" ;;
  closure_passed)
    die "closure passed despite reachable stub clause" ;;
  never_ran)
    die "closure never ran: the hot manifest did not parse (run_error), so exit $CLOSURE_RC proves nothing about the reachable-stub law (see $WORK/spine-closure-hot.jsonl)" ;;
  no_closure)
    die "closure exited $CLOSURE_RC without emitting closure_computed (see $WORK/spine-closure-hot.jsonl)" ;;
  failed_elsewhere)
    die "closure failed with absent=0: the red is not the reachable-stub law (see $WORK/spine-closure-hot.jsonl)" ;;
esac
grep -q '"event":"closure_computed".*FG-INV-04.core' "$WORK/spine-closure-hot.jsonl" \
  && ok "closure names the exact absent clause (FG-INV-04.core)" \
  || die "absent clause not named (see $WORK/spine-closure-hot.jsonl)"
grep -q '"event":"capability_absent","capability":"mvcc-visibility"' "$WORK/spine-closure-hot.jsonl" \
  && ok "capability_absent names mvcc-visibility with its clauses" \
  || die "capability_absent event missing"

# --- Verdict -----------------------------------------------------------------
log "evidence: $WORK/{spine-baseline,spine-neg-21,spine-closure-probe,spine-closure-control,spine-closure-hot}.jsonl"
log "result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
log "G0 spine e2e: ALL GREEN"
