#!/usr/bin/env bash
# dependency_policy_e2e.sh — scan the full locked dependency graph
# Owner: fgdb-rsfy.
#
# cargo-audit owns vulnerability and advisory policy. cargo-deny owns license
# and source policy over the resolved graph, including all features and dev
# dependencies through deny.toml. Exact direct foundation URLs/revisions and
# the rule against new direct external dependencies belong to topology-check;
# this gate adds the non-duplicative transitive-graph checks and never updates
# a manifest, lockfile, revision, or toolchain.
#
# Every registered run proves all three detectors can go red: a synthetic
# lockfile names a known vulnerable smallvec release; one scratch policy rejects
# the existing franken_networkx license bytes; another disallows crates.io and
# therefore rejects the resolved transitive registry graph. Scratch is retained
# because repository policy forbids this gate from deleting files.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POLICY="$ROOT/deny.toml"

# shellcheck source=lib/gate_verdict.sh
. "$ROOT/scripts/lib/gate_verdict.sh"

gate_init "dependency_policy_e2e"

if [ "$#" -ne 0 ]; then
  gate_unrun "usage: scripts/dependency_policy_e2e.sh"
  gate_verdict
  exit $?
fi

if [ ! -r "$POLICY" ]; then
  gate_unrun "dependency policy is absent or unreadable: $POLICY"
  gate_verdict
  exit $?
fi
if ! command -v cargo-audit >/dev/null 2>&1; then
  gate_unrun "cargo-audit is unavailable; the advisory assertion did not run"
  gate_verdict
  exit $?
fi
if ! command -v cargo-deny >/dev/null 2>&1; then
  gate_unrun "cargo-deny is unavailable; the policy assertions did not run"
  gate_verdict
  exit $?
fi

cd "$ROOT" || exit 1

if cargo audit --deny warnings; then
  gate_pass "cargo-audit found no vulnerability, unsoundness, yanked, or unmaintained advisory in Cargo.lock"
else
  gate_fail "cargo-audit rejected Cargo.lock"
fi

if cargo deny -L error --locked check licenses bans sources; then
  gate_pass "cargo-deny accepted the all-features dependency graph, including dev dependencies"
else
  gate_fail "cargo-deny rejected the dependency policy"
fi

WORK_ROOT="${FGDB_GATE_TMP:-/data/tmp/fgdb_swarm/dependency_policy}"
if ! mkdir -p "$WORK_ROOT" 2>/dev/null; then
  gate_unrun "cannot create dependency-policy scratch root: $WORK_ROOT"
  gate_verdict
  exit $?
fi
RUN_DIR="$(mktemp -d "$WORK_ROOT/run-XXXXXX" 2>/dev/null)"
if [ -z "$RUN_DIR" ] || [ ! -d "$RUN_DIR" ]; then
  gate_unrun "cannot create dependency-policy scratch run under $WORK_ROOT"
  gate_verdict
  exit $?
fi
gate_diag "scratch retained at $RUN_DIR"

cat >"$RUN_DIR/Cargo.lock" <<'LOCK'
# Synthetic negative control for RUSTSEC-2018-0003.
version = 3

[[package]]
name = "smallvec"
version = "0.6.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
LOCK
if cargo audit --no-fetch --deny warnings --file "$RUN_DIR/Cargo.lock" \
    >"$RUN_DIR/audit.stdout" 2>"$RUN_DIR/audit.stderr"; then
  gate_fail "control: cargo-audit accepted the planted vulnerable smallvec release"
elif grep -q 'RUSTSEC-2018-0003' "$RUN_DIR/audit.stdout" \
    "$RUN_DIR/audit.stderr"; then
  gate_pass "control: cargo-audit rejected the planted RUSTSEC-2018-0003 release"
else
  gate_fail "control: cargo-audit failed for a reason other than the planted advisory"
fi

awk '
  BEGIN { changed = 0 }
  changed == 0 && /hash = 0x3e2e0c66/ {
    sub(/hash = 0x3e2e0c66/, "hash = 0x00000000")
    changed = 1
  }
  { print }
' "$POLICY" >"$RUN_DIR/deny.license-mutant.toml"
if [ "$(grep -c 'hash = 0x00000000' "$RUN_DIR/deny.license-mutant.toml")" -ne 1 ]; then
  gate_fail "control: the license-hash mutation did not apply exactly once"
elif cargo deny -L error --locked check licenses \
    --config "$RUN_DIR/deny.license-mutant.toml" \
    >"$RUN_DIR/license.stdout" 2>"$RUN_DIR/license.stderr"; then
  gate_fail "control: cargo-deny accepted the planted foundation license mismatch"
elif grep -q 'unlicensed' "$RUN_DIR/license.stdout" "$RUN_DIR/license.stderr" \
    && grep -q 'fnx-classes' "$RUN_DIR/license.stdout" "$RUN_DIR/license.stderr"; then
  gate_pass "control: cargo-deny rejected the planted foundation license mismatch"
else
  gate_fail "control: cargo-deny failed for a reason other than the planted license mismatch"
fi

awk '
  $0 == "allow-registry = [\"https://github.com/rust-lang/crates.io-index\"]" {
    print "allow-registry = []"
    next
  }
  { print }
' "$POLICY" >"$RUN_DIR/deny.source-mutant.toml"
if cmp -s "$POLICY" "$RUN_DIR/deny.source-mutant.toml" \
    || grep -q '^allow-registry = \["https://github.com/rust-lang/crates.io-index"\]$' \
      "$RUN_DIR/deny.source-mutant.toml"; then
  gate_fail "control: the transitive-source mutation did not apply exactly"
elif cargo deny -L error --locked check sources \
    --config "$RUN_DIR/deny.source-mutant.toml" \
    >"$RUN_DIR/source.stdout" 2>"$RUN_DIR/source.stderr"; then
  gate_fail "control: cargo-deny accepted the planted unapproved transitive registry source"
elif grep -q 'source-not-allowed' "$RUN_DIR/source.stdout" \
    "$RUN_DIR/source.stderr" \
    && grep -q 'rust-lang/crates.io-index' "$RUN_DIR/source.stdout" \
    "$RUN_DIR/source.stderr"; then
  gate_pass "control: cargo-deny rejected the planted unapproved transitive registry source"
else
  gate_fail "control: cargo-deny failed for a reason other than the planted transitive-source violation"
fi

gate_verdict
