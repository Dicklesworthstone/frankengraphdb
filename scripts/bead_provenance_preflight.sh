#!/usr/bin/env bash
# =============================================================================
# bead_provenance_preflight.sh — what will this bead cost the tree?
#
# Bead provenance is TOTAL by design: every bead in `.beads/issues.jsonl` must
# resolve to a direct owner, bet label, exact override, or `[[bead_family]]`
# rule in `registries/architecture_decisions.toml`. That binding is what ties
# beads to architecture decisions, so it is not a bug. Each family also carries
# an `expected_match_count`, which is a FLOOR on how many beads it selects.
#
# ONLY ONE OUTCOME OF `br create` NOW REDS THE TREE (changed by fgdb-lzol):
#
#   bead matches NO family      -> bead_provenance_orphan, or
#                                  bead_workstream_label_in_bet_position when the
#                                  record carries a w<n>/g<n> tag and no bet label
#                               -> bead_provenance_not_total       STILL RED
#   bead matches a KNOWN family -> counts rise above their floors    GREEN
#
# The cardinality pins used to be equalities over `.beads/issues.jsonl`, a file
# with N writers, so a bead created by ANY pane invalidated every other pane's
# just-frozen pins and five tests went red until someone re-froze by hand. They
# are floors now: creation can only raise them, so there is nothing to
# re-freeze after a `br create`. Do not add one back.
#
# What still needs a human is the orphan branch, and that is the branch this
# script exists for: giving a bead a home is a judgement about which ADRs it
# binds to, and cannot be automated without fabricating provenance.
#
# NOT affected, contrary to a natural guess: `appendix_a::binding_contract_tests`.
# Those call `load_catalog_file`, which is structural+semantic only. Only
# `load_and_verify` reaches `verify_repository_bindings` -> the Beads index.
# Verified 60/60 green at aa17857 with three orphan beads present.
#
# Uses plain rustc against tools/registry-check only — no cargo, so it never
# takes the shared package-cache lock, and it does not need the workspace to
# compile (which matters, because the tree is frequently mid-repair). The probe
# is cached and rebuilt only when the checker sources change. Run it BEFORE you
# commit `.beads/`.
#
# Exit 0 = committing this `.beads/issues.jsonl` will not move bead provenance.
# Exit 1 = it will; the report says exactly which edit is required.
# =============================================================================

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BEADS="${1:-.beads/issues.jsonl}"
ADR="registries/architecture_decisions.toml"

for f in "$BEADS" "$ADR"; do
  # An unreadable input must fail, never skip: a preflight that silently checks
  # nothing is indistinguishable from one that passed.
  if [ ! -r "$f" ]; then
    echo "ERROR: cannot read $f — refusing to report bead provenance as checked" >&2
    exit 1
  fi
done

# Delegate to the REAL resolver. An earlier draft of this script reimplemented
# the family-matching rules in python and reported 102 orphans where the
# authoritative resolver reports 3 — it ignored the direct-owner, bet-label and
# exact-override tables. A second implementation of a rule is a second source of
# truth, and the wrong one. So this compiles a thin probe against
# tools/registry-check and prints whatever `resolve_bead_provenance` says.
#
# Plain rustc, no cargo, no build slot (the shared package-cache lock is why).
# The rlib is cached in CACHE and reused until the checker sources change.
CACHE="${FGDB_PREFLIGHT_CACHE:-${TMPDIR:-/tmp}/fgdb-bead-preflight}"
mkdir -p "$CACHE"
SRC_STAMP="$(find tools/registry-check/src -name '*.rs' -newer "$CACHE/librc.rlib" -print -quit 2>/dev/null || echo rebuild)"
if [ ! -f "$CACHE/librc.rlib" ] || [ -n "$SRC_STAMP" ]; then
  echo "  (building the checker probe once; plain rustc, no build slot)" >&2
  CARGO_MANIFEST_DIR="$ROOT/tools/registry-check" rustc --edition=2024 --crate-type=lib \
    --crate-name registry_check tools/registry-check/src/lib.rs -o "$CACHE/librc.rlib" \
    >/dev/null 2>&1 || { echo "ERROR: cannot build the checker probe" >&2; exit 1; }
  cat > "$CACHE/probe.rs" <<'RUST'
use registry_check::architecture;
use std::path::Path;
fn main() {
    let root = std::env::args().nth(1).expect("root");
    let root = Path::new(&root);
    let reg = match architecture::load_from_repo(root) {
        Ok(r) => r,
        Err(e) => { println!("REGISTRY_ERR {e}"); std::process::exit(1); }
    };
    match architecture::resolve_bead_provenance(&reg, root) {
        Ok(entries) => println!("OK {}", entries.len()),
        Err(e) => { println!("UNRESOLVED"); for part in e.split("; ") { println!("  {part}"); }
                    std::process::exit(1); }
    }
}
RUST
  CARGO_MANIFEST_DIR="$ROOT/tools/registry-check" rustc --edition=2024 --crate-name probe \
    "$CACHE/probe.rs" -L "$CACHE" --extern registry_check="$CACHE/librc.rlib" -o "$CACHE/probe" \
    >/dev/null 2>&1 || { echo "ERROR: cannot build the checker probe" >&2; exit 1; }
fi

if out="$("$CACHE/probe" "$ROOT" 2>&1)"; then
  echo "bead provenance is TOTAL: ${out#OK }"
  echo "Committing .beads/ will not move bead provenance."
  exit 0
fi
echo "$out"
cat <<'MSG'

Committing .beads/ in this state reds these tests for EVERY pane:
  architecture_decisions.rs  architecture_registry_parses_and_validates
                             architecture_bead_provenance_is_total_pinned_and_bidirectional
  identity.rs                appendix_a_repository_bindings_resolve_beads_crates_checkers_and_events

REQUIRED EDIT in registries/architecture_decisions.toml: give each unresolved
bead a home (a [[bead_family]] rule, direct owner, bet label, or exact
override). Which ADRs a bead binds to is a judgement call and cannot be
automated without fabricating provenance.

You do NOT need to re-freeze the cardinality pins afterwards: they are floors,
and creation only raises the observed counts. If you find yourself editing
bead_count after a `br create`, stop — that is the equality behaviour fgdb-lzol
removed, and putting it back reds every other pane.
MSG
exit 1
