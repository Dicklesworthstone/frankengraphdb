#!/usr/bin/env bash
# =============================================================================
# check.sh — the convenience quality-gate wrapper (AGENTS.md).
#
# Runs, in order, stopping on the first failure:
#   1. file-coverage closure  (every tracked file is inspected or declared)
#   2. shell lint  (bash -n + shellcheck over every tracked shell deliverable)
#   3. cargo fmt --check
#   4. cargo check --all-targets
#   5. cargo clippy --all-targets -- -D warnings
#   6. cargo test
#   7. registry-check all  (the G0 claims-lint / registry-validation CI job)
#   8. scripts/g0_identity_e2e.sh  (canonical Appendix A/identity hard gate)
#   9. scripts/g0_architecture_decisions_e2e.sh  (frozen ADR e2e + provenance)
#  10. scripts/g0_threat_e2e.sh  (G0 threat/trust model hard gate)
#  11. threat-check  (frozen threat model + generated document)
#  12. architecture-check  (frozen ADR + reciprocal provenance)
#  13. topology-check  (workspace topology + unsafe boundary ledger)
#  14. unsafe-ledger-check  (unsafe-boundary scanner + its own self-test)
#
# Steps 13-14 are load-bearing for step 1: the coverage table claims
# topology-check as the inspector for rust-toolchain.toml,
# registries/workspace_topology.toml, registries/unsafe_boundary_ledger.toml and
# docs/WORKSPACE_TOPOLOGY.md. A coverage claim naming a gate this script does not
# run would be false — which is the exact failure step 1 exists to prevent. If you
# remove a gate below, move its files to an exemption in the same commit.
#
# STILL MISSING (fgdb-gate-coverage-checksh-omits-live-gates-u7mw, not this
# script's bead): scripts/g0_claims_e2e.sh, scripts/g0_spine_e2e.sh,
# scripts/g0_topology_e2e.sh and scripts/w1_cross_crate_determinism_e2e.sh are
# registered `status = "live"` in registries/checker_index.toml and are not run
# here. No coverage claim above depends on them.
#
# When CI is added, wire this script as the CI test step rather than
# duplicating the commands.
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> file-coverage closure (no gate may report green over a file it never opened)"
# WHY this step exists: every mandated check is silent on the file types it does
# not handle. The cargo steps are no-ops on everything that is not Rust, and `ubs`
# is worse than silent — it exits 0 while printing "nothing was checked (this is
# NOT a pass)" for .sh, .jsonl, .toml and .md alike. So a file type that no gate
# ever opens is indistinguishable, at the exit code, from one that passed
# everything. This step closes that hole structurally: every tracked file must be
# claimed by a gate below or declared exempt with a reason, and an unclaimed file
# is a hard failure rather than a silent pass.
#
# The tracked set is enumerated from `git ls-files` at run time, so a newly added
# file cannot slip through by not being listed here — it lands in the unclaimed
# bucket and fails closed.
#
# EVERY "inspected" claim below was mutation-proven, not assumed: the named gate
# was run against a tree with that file destroyed and observed to go red (and for
# files that are only partly load-bearing, mutated inside the pinned region — a
# weaker mutation lands somewhere unchecked and reports a false pass). Do not add
# a row here without doing the same. An unverified coverage claim is precisely the
# defect this step exists to prevent, one level up.
coverage_of() {
  case "$1" in
    *.rs)                                echo "cargo fmt/check/clippy/test" ;;
    *.sh|*.bash)                         echo "bash -n + shellcheck (next step)" ;;
    Cargo.toml|*/Cargo.toml)             echo "registry-check all (workspace topology)" ;;
    Cargo.lock)                          echo "cargo check --all-targets" ;;
    rust-toolchain.toml)                 echo "topology-check" ;;
    registries/threat_model.toml)        echo "threat-check" ;;
    registries/workspace_topology.toml)  echo "topology-check" ;;
    registries/unsafe_boundary_ledger.toml) echo "topology-check" ;;
    registries/*.toml)                   echo "registry-check all" ;;
    .beads/issues.jsonl)                 echo "architecture-check (parses every record; malformed line fails file:line)" ;;
    docs/ARCHITECTURE_DECISION_RECORD.md) echo "architecture-check (CHECKED-SOURCE region)" ;;
    docs/THREAT_AND_TRUST_MODEL.md)      echo "threat-check (generated document)" ;;
    docs/WORKSPACE_TOPOLOGY.md)          echo "topology-check (generated document)" ;;
    COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md)
                                         echo "registry-check (source_block pins + claims-lint)" ;;
    README.md|AGENTS.md)                 echo "registry-check lint (claim markers only, not content)" ;;
    *)                                   echo "" ;;
  esac
}
# An exemption is a CLAIM THAT NOTHING CHECKS THIS FILE, stated out loud. Each is
# printed on every run so "green" is never silent about what was skipped.
coverage_exempt_reason() {
  case "$1" in
    COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB__FABLE.md|\
    COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB__SOL.md|\
    COMPREHENSIVE_PLAN_REVIEW_BY_KIMI_K3.md|\
    META_ANALYSIS_OF_KIMI_K3_REVIEW_BY_FABLE.md|\
    PLAN_AUDIT_BY_SOL_ULTRA.md)
      echo "historical review artifact; exclusion reason recorded in registries/claims_lint.toml" ;;
    .beads/config.yaml|.beads/metadata.json|.beads/.gitignore)
      echo "beads-tool internal state, regenerated by br; not a deliverable" ;;
    .gitignore)
      echo "VCS configuration; not a deliverable" ;;
    LICENSE)
      echo "verbatim legal text; must not be edited" ;;
    .beads/.write.lock.stale-*)
      echo "STALE BEADS LOCK, COMMITTED BY ACCIDENT — should be removed from the index" ;;
    *)
      echo "" ;;
  esac
}
COV_TRACKED=0
COV_INSPECTED=0
COV_EXEMPT=0
COV_UNCLAIMED=""
while IFS= read -r covfile; do
  COV_TRACKED=$((COV_TRACKED + 1))
  if [ -n "$(coverage_of "$covfile")" ]; then
    COV_INSPECTED=$((COV_INSPECTED + 1))
  else
    covreason="$(coverage_exempt_reason "$covfile")"
    if [ -n "$covreason" ]; then
      COV_EXEMPT=$((COV_EXEMPT + 1))
      printf '    NOT INSPECTED  %-44s %s\n' "$covfile" "$covreason"
    else
      COV_UNCLAIMED="$COV_UNCLAIMED $covfile"
    fi
  fi
done < <(git ls-files)
if [ "$COV_TRACKED" -eq 0 ]; then
  echo "ERROR: git ls-files returned nothing — this step must never be vacuously green" >&2
  exit 1
fi
if [ -n "$COV_UNCLAIMED" ]; then
  echo "ERROR: no gate in this script inspects the following tracked file(s):" >&2
  for covfile in $COV_UNCLAIMED; do echo "    $covfile" >&2; done
  echo "  Give it an inspector in coverage_of(), or declare it in" >&2
  echo "  coverage_exempt_reason() with a reason saying why nothing checks it." >&2
  echo "  Refusing to report green over a file this gate never opened." >&2
  exit 1
fi
echo "    coverage: $COV_TRACKED tracked = $COV_INSPECTED inspected + $COV_EXEMPT declared-not-inspected"

echo "==> shell lint (bash -n + shellcheck) over every tracked shell deliverable"
# WHY this step exists: the mandated checks are all cargo-based, and cargo has
# nothing to say about a .sh file. `ubs` is worse than silent on them — it exits
# 0 while printing "no supported languages detected ... this is NOT a pass", so
# a shell deliverable that has never been linted reports exactly like one that
# passed. (Same family: ubs treats .jsonl as unsupported, so it is a no-op on
# .beads/ too.) Every script under scripts/ was therefore exempt from the
# quality gate until this step was added.
#
# This is not hypothetical. shellcheck caught SC2033 in the determinism gate:
# `xargs sha256` where sha256 was a shell function. xargs cannot invoke a
# function, so the pipeline digested an empty stream and the "source pin"
# came back as the constant sha256("") on every tree — 64 valid hex chars that
# passed a length check and made every before/after comparison succeed. The
# concurrency guard was disabled with no error and no visible symptom.
SHELL_FILES="$(git ls-files '*.sh' '*.bash')"
if [ -z "$SHELL_FILES" ]; then
  echo "ERROR: no shell deliverables found — this step must never be vacuously green" >&2
  exit 1
fi
SHELL_N=0
for f in $SHELL_FILES; do
  bash -n "$f" || { echo "ERROR: bash -n failed for $f" >&2; exit 1; }
  SHELL_N=$((SHELL_N + 1))
done
echo "    bash -n: $SHELL_N file(s) parsed"
if command -v shellcheck >/dev/null 2>&1; then
  # Notes are advisory; errors and warnings are not.
  # shellcheck disable=SC2086
  shellcheck --severity=warning $SHELL_FILES \
    || { echo "ERROR: shellcheck reported an error or warning" >&2; exit 1; }
  echo "    shellcheck: clean at severity>=warning across $SHELL_N file(s)"
else
  echo "ERROR: shellcheck is not installed; the shell deliverables were NOT checked." >&2
  echo "  Refusing to report green on an unrun check — install shellcheck or run" >&2
  echo "  scripts/check.sh on a host that has it." >&2
  exit 1
fi

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo check --all-targets"
cargo check --all-targets

echo "==> cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "==> cargo test"
cargo test

echo "==> registry-check all (claim registries + claims-lint + closure)"
cargo run -p registry-check --quiet -- all --root "$ROOT" > /dev/null

echo "==> G0 identity E2E (canonical Appendix A catalog + generated projections)"
scripts/g0_identity_e2e.sh > /dev/null

echo "==> G0 architecture-decisions E2E (frozen ADR + bead provenance)"
scripts/g0_architecture_decisions_e2e.sh > /dev/null

echo "==> G0 threat-model E2E (trust matrix + authority lattice + footprint)"
scripts/g0_threat_e2e.sh > /dev/null

echo "==> threat-check (frozen threat model + generated document)"
cargo run -p registry-check --quiet --bin threat-check -- --root "$ROOT" > /dev/null

echo "==> architecture-check (frozen ADR + reciprocal provenance)"
cargo run -p registry-check --quiet --bin architecture-check -- --root "$ROOT" > /dev/null

echo "==> topology-check (workspace topology + unsafe boundary ledger)"
cargo run -p registry-check --quiet --bin topology-check -- --root "$ROOT" > /dev/null

echo "==> unsafe-ledger-check (unsafe-boundary scanner + scanner self-test)"
cargo run -p registry-check --quiet --bin unsafe-ledger-check -- --root "$ROOT" > /dev/null

echo "ALL GATES GREEN — $COV_INSPECTED/$COV_TRACKED tracked files inspected, \
$COV_EXEMPT declared-not-inspected (listed at the top of this run)"
