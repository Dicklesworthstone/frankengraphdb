#!/usr/bin/env bash
# =============================================================================
# bead_provenance_preflight.sh — what will this bead cost the tree?
#
# Bead provenance is TOTAL by design: every bead in `.beads/issues.jsonl` must
# resolve to a direct owner, bet label, exact override, or `[[bead_family]]`
# rule in `registries/architecture_decisions.toml`. That binding is what ties
# beads to architecture decisions, so it is not a bug. Each family also carries
# an `expected_match_count`, and five cardinality declarations are FLOORS.
#
# TWO EVENTS RED THE TREE, AND THIS SCRIPT NOW PREDICTS BOTH:
#
#   a record RESOLVES BY NOTHING  -> bead_provenance_orphan, or
#                                    bead_workstream_label_in_bet_position when
#                                    it carries a w<n>/g<n> tag and no bet label
#                                 -> bead_provenance_not_total
#   records DISAPPEAR             -> bead_source_count_below_floor
#                                 -> bead_resolution_class_count_below_floor
#
# `br create` with any b1..b6 label moves neither: creation only raises counts,
# which is why fgdb-lzol made the pins floors. Do NOT re-freeze a floor after a
# create — there is nothing to re-freeze, and raising one reds every pane that
# has not pulled.
#
# The second event is the one this script used to miss (fgdb-a5kb). Its probe
# called `resolve_bead_provenance`, which returns Ok whenever every record
# resolves and never looks at a count, so deleting six records exited 0 while
# `tests/architecture_decisions.rs` went 28 passed / 5 FAILED. Measured at
# 9e2ed85: delete 5 -> both green; delete 6 -> tests red, this script said
# "will not move bead provenance". A preflight that says safe on the event the
# floors exist to catch is worse than no preflight, so the probe now reports the
# checker's OWN verdict — `validate_architecture`, the same function the tests
# call — filtered to `contradiction_class == "bead_provenance"`. One reader.
#
# IT PROVES IT CAN FAIL BEFORE IT REPORTS THAT IT DID NOT. The failure mode of a
# preflight is passing without checking, so a green here is worthless unless the
# probe has just been seen to go red. Every run first drives the probe over two
# synthetic corpora — one with a planted orphan, one with half the records
# deleted — and refuses to report anything (exit 2) unless both come back red
# with the expected code. The floor half is what dies if anyone reverts the
# probe to `resolve_bead_provenance`.
#
# NOT affected, contrary to a natural guess: `appendix_a::binding_contract_tests`.
# Those call `load_catalog_file`, which is structural+semantic only. Only
# `load_and_verify` reaches `verify_repository_bindings` -> the Beads index.
# Verified 60/60 green at aa17857 with three orphan beads present.
#
# Uses plain rustc against tools/registry-check only — no cargo, so it never
# takes the shared package-cache lock, and it does not need the workspace to
# compile (which matters, because the tree is frequently mid-repair). The probe
# is cached and rebuilt when the checker sources OR this script change. Run it
# BEFORE you commit `.beads/`.
#
# Exit 0 = committing this `.beads/issues.jsonl` will not move bead provenance.
# Exit 1 = it will; the report says exactly which edit is required.
# Exit 2 = the probe failed its own red-proof; nothing was reported either way.
# =============================================================================

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF="${BASH_SOURCE[0]}"
cd "$ROOT"

BEADS="${1:-.beads/issues.jsonl}"
ADR="registries/architecture_decisions.toml"
PLAN="COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"

for f in "$BEADS" "$ADR" "$PLAN"; do
  # An unreadable input must fail, never skip: a preflight that silently checks
  # nothing is indistinguishable from one that passed.
  if [ ! -r "$f" ]; then
    echo "ERROR: cannot read $f — refusing to report bead provenance as checked" >&2
    exit 1
  fi
done

# Delegate to the REAL checker. An earlier draft of this script reimplemented
# the family-matching rules in python and reported 102 orphans where the
# authoritative resolver reports 3 — it ignored the direct-owner, bet-label and
# exact-override tables. A second implementation of a rule is a second source of
# truth, and the wrong one. So this compiles a thin probe against
# tools/registry-check and prints whatever `validate_architecture` says.
#
# Plain rustc, no cargo, no build slot (the shared package-cache lock is why).
# The rlib is cached in CACHE and reused until the checker sources change.
CACHE="${FGDB_PREFLIGHT_CACHE:-${TMPDIR:-/tmp}/fgdb-bead-preflight}"
mkdir -p "$CACHE"
# THIS SCRIPT is an input to the cached probe too: the probe source is a heredoc
# below, so a change to it must invalidate the cache exactly as a change to the
# checker does. Keying only on `src/*.rs` meant an edited probe kept running the
# old binary — silently, and with no way to tell from the output.
SRC_STAMP="$(find tools/registry-check/src "$SELF" -newer "$CACHE/probe" -print -quit 2>/dev/null || echo rebuild)"
if [ ! -f "$CACHE/probe" ] || [ -n "$SRC_STAMP" ]; then
  echo "  (building the checker probe once; plain rustc, no build slot)" >&2
  CARGO_MANIFEST_DIR="$ROOT/tools/registry-check" rustc --edition=2024 --crate-type=lib \
    --crate-name registry_check tools/registry-check/src/lib.rs -o "$CACHE/librc.rlib" \
    >/dev/null 2>&1 || { echo "ERROR: cannot build the checker probe" >&2; exit 1; }
  cat > "$CACHE/probe.rs" <<'RUST'
use registry_check::architecture;
use std::collections::BTreeMap;
use std::path::Path;
fn main() {
    let root = std::env::args().nth(1).expect("root");
    let root = Path::new(&root);
    let reg = match architecture::load_from_repo(root) {
        Ok(r) => r,
        Err(e) => { println!("REGISTRY_ERR {e}"); std::process::exit(1); }
    };
    // The checker's own verdict, narrowed to the contract this script predicts.
    // `resolve_bead_provenance` was the old call and answers a strictly
    // narrower question -- every record RESOLVES -- so it is blind to the
    // count floors, which is the event a disappearing record trips.
    let violations: Vec<_> = architecture::validate_architecture(&reg, root)
        .into_iter()
        .filter(|v| v.contradiction_class == "bead_provenance")
        .collect();
    if !violations.is_empty() {
        println!("VIOLATIONS {}", violations.len());
        for v in &violations {
            let who = if v.owner_bead.is_empty() { "-" } else { v.owner_bead.as_str() };
            println!("  {} {} {}", v.code, who, v.message);
        }
        std::process::exit(1);
    }
    // A green verdict must state what it measured, or it is indistinguishable
    // from a green that measured nothing.
    let entries = match architecture::bead_provenance_membership(&reg, root) {
        Ok(e) => e,
        Err(e) => { println!("MEMBERSHIP_ERR {e}"); std::process::exit(1); }
    };
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in &entries {
        *counts.entry(entry.resolution_class.as_str()).or_default() += 1;
    }
    let census: Vec<String> = counts.iter().map(|(k, n)| format!("{k} {n}")).collect();
    println!("OK {} records ({})", entries.len(), census.join(", "));
}
RUST
  CARGO_MANIFEST_DIR="$ROOT/tools/registry-check" rustc --edition=2024 --crate-name probe \
    "$CACHE/probe.rs" -L "$CACHE" --extern registry_check="$CACHE/librc.rlib" -o "$CACHE/probe" \
    >/dev/null 2>&1 || { echo "ERROR: cannot build the checker probe" >&2; exit 1; }
fi

# --- the red-proof -----------------------------------------------------------
# A synthetic root is enough: the bead_provenance verdict depends only on the
# registry and on `.beads/issues.jsonl`, so everything else can be symlinked and
# the ~1000 unrelated violations such a root produces are filtered out anyway.
selftest_root() {  # $1 = name; stdin = the corpus for that root
  # PID-keyed: six panes share this cache dir, and a concurrent run rewriting a
  # fixture mid-read would abort THIS run with a false SELF-TEST FAILED -- the
  # same shared-fixture race that invents failures in the cargo suites.
  local d="$CACHE/selftest/$$-$1"
  mkdir -p "$d/.beads"
  ln -sfn "$ROOT/registries" "$d/registries"
  ln -sf "$ROOT/$PLAN" "$d/$PLAN"
  cat > "$d/.beads/issues.jsonl"
  printf '%s' "$d"
}

red_proof() {  # $1 = label, $2 = root, $3 = code the report must name
  local out rc
  out="$("$CACHE/probe" "$2" 2>&1)" && rc=0 || rc=$?
  if [ "$rc" -eq 0 ]; then
    echo "SELF-TEST FAILED: the probe reports a $1 corpus as clean." >&2
    echo "  Nothing below is trustworthy, so nothing is reported. Fix the probe." >&2
    exit 2
  fi
  if ! printf '%s' "$out" | grep -q "$3"; then
    echo "SELF-TEST FAILED: the probe rejected the $1 corpus without naming $3:" >&2
    printf '%s\n' "$out" >&2
    exit 2
  fi
}

ST_ORPHAN="$( { cat "$BEADS"; \
  printf '{"id":"fgdb-preflight-selftest-orphan","status":"open","labels":[]}\n'; } \
  | selftest_root orphan )"
# Half the corpus removed: below any floor at any corpus size, so this stays a
# red-proof as the project grows.
ST_FLOOR="$( head -n "$(( $(wc -l < "$BEADS") / 2 ))" "$BEADS" | selftest_root floor )"

red_proof "planted-orphan" "$ST_ORPHAN" "bead_provenance_orphan"
red_proof "half-deleted" "$ST_FLOOR" "below_floor"
echo "  (red-proof: the probe rejects a planted orphan and a half-deleted corpus)" >&2

# --- the tree you are about to commit ----------------------------------------
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

If the report says ORPHAN (or workstream_label_in_bet_position), the REQUIRED
EDIT is in registries/architecture_decisions.toml: give that bead a home (a
[[bead_family]] rule, direct owner, b1..b6 label, or exact override). Which ADRs
a bead binds to is a judgement call and cannot be automated without fabricating
provenance.

If the report says BELOW_FLOOR, records have DISAPPEARED from
.beads/issues.jsonl — that is what the floors exist to catch, and the repair is
to get them back (an import deletes DB rows absent from this file; check for an
`br sync --import-only` or a bad merge), NOT to lower the floor. Lowering a
floor to match a loss makes the loss permanent and invisible.

You do NOT need to re-freeze the floors after a `br create`: creation only
raises the observed counts. If you find yourself editing bead_count after a
create, stop — that is the equality behaviour fgdb-lzol removed, and putting it
back reds every other pane.
MSG
exit 1
