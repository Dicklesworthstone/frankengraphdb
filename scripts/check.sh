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
#   8. architecture-check  (frozen ADR + reciprocal provenance)
#
# When CI is added, wire this script as the CI test step rather than
# duplicating the commands.
# =============================================================================
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

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

echo "==> architecture-check (frozen ADR + reciprocal provenance)"
cargo run -p registry-check --quiet --bin architecture-check -- --root "$ROOT" > /dev/null

echo "ALL GATES GREEN"
