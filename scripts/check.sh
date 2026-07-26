#!/usr/bin/env bash
# =============================================================================
# check.sh — the convenience quality-gate wrapper (AGENTS.md).
#
# Runs, in order, stopping on the first failure:
#   1. cargo fmt --check
#   2. cargo check --all-targets
#   3. cargo clippy --all-targets -- -D warnings
#   4. cargo test
#   5. registry-check all  (the G0 claims-lint / registry-validation CI job)
#   6. scripts/g0_identity_e2e.sh  (canonical Appendix A/identity hard gate)
#   7. scripts/g0_architecture_decisions_e2e.sh  (frozen ADR e2e + provenance)
#   8. scripts/g0_threat_e2e.sh  (G0 threat/trust model hard gate)
#   9. threat-check  (frozen threat model + generated document)
#  10. architecture-check  (frozen ADR + reciprocal provenance)
#
# When CI is added, wire this script as the CI test step rather than
# duplicating the commands.
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

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

echo "ALL GATES GREEN"
