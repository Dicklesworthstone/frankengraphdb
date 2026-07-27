# shellcheck shell=bash
# =============================================================================
# gate_verdict.sh — THE verdict contract every gate in this repo reports under
# (bead fgdb-udco)
#
# Sourced by scripts/check.sh and by every live `kind = "script"` artifact in
# registries/checker_index.toml. It is a library: not executable, registers no
# gate, asserts nothing on its own. `run_verdict_contract` in scripts/check.sh
# is the guard that makes conforming to it mandatory.
#
# -----------------------------------------------------------------------------
# WHY THIS EXISTS
# -----------------------------------------------------------------------------
# MEASURED 2026-07-27 at d4b0aa2, by EVALUATING each gate's failure emitter with
# its two streams captured to separate files. Across scripts/check.sh plus the
# nine live script artifacts — ten gates — there were THREE failure tokens under
# TWO indentation conventions on TWO streams:
#
#   "RED core: <label>"          column 0,  stderr   check.sh
#   "[g0-claims-e2e] FAIL: ..."  prefixed,  stdout   claims, identity, spine
#   "    FAIL  <detail>"         indented,  stderr   negative_evidence, proof_lanes
#   "ERROR: <detail>"            column 0,  stderr   architecture_decisions,
#                                                    threat, topology, w1_cross_crate
#
# So NO gate emitted a line beginning with FAIL at column 0, and `grep '^FAIL'`
# returned 0 on a red run of all ten. Seven of the ten wrote the failure to
# stderr only, which means `gate.sh > log` produced a plausible, complete-looking,
# all-green transcript of a red run. That is silent-green: a gate failing on a
# stream nobody greps is green to every automated reader.
#
# The defect was never one gate's token. It was the ABSENCE OF A CONTRACT — ten
# gates each choosing its own, so no single query could answer "did this gate
# fail" across the set. This file is that contract, in one place, so there is one
# reader to drift rather than ten. scripts/lib/private_subject.sh is the
# precedent for a shared gate library living here.
#
# -----------------------------------------------------------------------------
# THE CONTRACT
# -----------------------------------------------------------------------------
# 1. ONE STREAM.  The verdict transcript is STDOUT. stderr carries diagnostics
#    only — the prose explaining WHY — and is unconstrained. A reader who keeps
#    only stdout keeps a complete, honest verdict.
#
# 2. ONE TOKEN.   Every failure emits an anchored `FAIL ` line at column 0.
#    `grep -c '^FAIL ' <stdout>` is THE query, and it is total over all ten
#    gates. `PASS ` is its counterpart. A gate MAY additionally emit `RED ` and
#    `UNRUN ` as refinements, but only BESIDE a FAIL line, never instead of one
#    — so the single query never has to know which gate it is reading. The
#    vocabulary is closed: PASS, FAIL, RED, UNRUN. Nothing else may appear at
#    column 0 of stdout.
#
# 3. THREE STATES, NOT TWO.  `ran and passed`, `ran and failed`, and `DID NOT
#    RUN` are three distinct outcomes, and only the first is green. A check that
#    did not execute its assertions MUST NOT emit the passing token. This is the
#    same doctrine fgdb-1nqb states for guards, said once here for everything.
#
#    WHY IT IS IN THE CONTRACT AND NOT A FOOTNOTE. pane3 mutation-proved on
#    2026-07-27 that tools/registry-check/tests/identity.rs returns early and
#    reports `ok. 1 passed` when `.beads/issues.jsonl` is absent: 6 of its 7
#    assertions and the ONLY witness for ten violation codes are skipped, and
#    the suite still reports passing. Two quiet roots differing only by
#    `--exclude='.beads/*'` — separate compiles, because `repo_root()` is
#    compile-time — ran 5.23s WITH the corpus and 1.06s WITHOUT, and the marker
#    printed only in the first. The trigger is routine and ours: rch remote
#    workers do not sync `.beads`, so every offloaded run of that test was
#    silently vacuous. UNTIL THAT IS FIXED, RUN ANY SUITE THAT READS `.beads`
#    LOCALLY, NOT THROUGH rch.
#
#    A gate that fails invisibly and a test that skips invisibly are the same
#    defect wearing different clothes: the first is a red nobody's query can
#    see, the second is a green nobody earned. `gate_verdict` and the EXIT trap
#    below both refuse to report green over zero executed assertions, so a gate
#    that skips its whole body reports UNRUN rather than falling through silent.
#
# 4. ONE EXIT DISCIPLINE.  Exit 0 if and only if zero FAIL lines AND zero UNRUN
#    lines were emitted and the gate reached its verdict. THE EXIT CODE REMAINS
#    AUTHORITATIVE — the tokens exist so that a reader who greps anyway gets a
#    true answer, not so that grepping becomes the right way to read a gate.
#
# -----------------------------------------------------------------------------
# WHY THE TRAP IS THE LOAD-BEARING PART
# -----------------------------------------------------------------------------
# `gate_init` installs an EXIT trap that DERIVES the contract line from the exit
# code. A gate that dies on an unguarded `set -e` abort, a missing file or a
# helper refusing to answer still emits `FAIL`, because the trap fires on the
# exit status rather than on any assertion having noticed. That closes the class
# the four fail-fast gates were in: they had no PASS/FAIL emitter at all, and
# converting each of their ~20 `echo "ERROR: ..." >&2; exit 1` sites by hand
# would have left the contract false on every path no site covers. Deriving the
# verdict from the exit code makes it impossible for the line to disagree with
# the instrument that is authoritative.
# =============================================================================

GATE_NAME=""
GATE_PASS=0
GATE_FAIL=0
GATE_UNRUN=0
GATE_TALLY_HOOK=""
GATE_CONTRACT_LINE_EMITTED=0

# gate_init <name> [tally_hook]
#
# The optional hook is the gate's own pre-existing tally function (the fail-slow
# gates each have one, printing "N passed, M failed" from their own EXIT trap).
# It is called from ours so that installing this contract does not silently
# replace a gate's existing partial-tally reporting — that reporting exists
# because a truncated log used to read exactly like a whole one, and dropping it
# here would reintroduce the defect it was built to close.
gate_init() {
  GATE_NAME="$1"
  GATE_TALLY_HOOK="${2:-}"
  GATE_PASS=0
  GATE_FAIL=0
  GATE_UNRUN=0
  GATE_CONTRACT_LINE_EMITTED=0
  trap gate_on_exit EXIT
}

# gate_pass <detail> — an assertion that passed. stdout, anchored.
gate_pass() {
  GATE_PASS=$((GATE_PASS + 1))
  printf 'PASS %s\n' "$*"
}

# gate_fail <detail> — an assertion that failed. stdout, anchored.
#
# Fail-slow: recording a failure does not end the run. A gate that stops at its
# first failure cannot say whether it found one problem or ninety-two, and this
# repo has measured that cost (g0_identity_e2e.sh once ran 8 of 99 assertions
# and reported no tally at all).
gate_fail() {
  GATE_FAIL=$((GATE_FAIL + 1))
  printf 'FAIL %s\n' "$*"
}

# gate_unrun <detail> — the third state: an assertion or artifact that DID NOT
# EXECUTE. Emits the refinement AND the contract token, because "did not run" is
# not green and the single `^FAIL ` query must see it. Reach for this whenever a
# check is skipped — a missing corpus, an absent toolchain, a fixture that could
# not be built — instead of returning early and letting silence read as success.
gate_unrun() {
  GATE_UNRUN=$((GATE_UNRUN + 1))
  printf 'UNRUN %s\n' "$*"
  printf 'FAIL %s\n' "$*"
}

# gate_diag <line...> — the WHY. stderr, unconstrained, never a verdict.
gate_diag() {
  printf '%s\n' "$*" >&2
}

# gate_die <detail> [diagnostic...] — fail-fast: one FAIL line, then exit 1.
#
# For the structural failures that invalidate every assertion after them (the
# subject failed to build; a fixture did not fail when it must). Continuing past
# one of those manufactures failures rather than reporting them.
gate_die() {
  local detail="$1"
  shift
  gate_fail "$detail"
  [ "$#" -gt 0 ] && gate_diag "$@"
  exit 1
}

# gate_verdict — the three-state tally; nonzero when anything failed OR did not
# run.
#
# THE ZERO IS LICENSED BY ACCOUNTING, not by finding nothing. A gate that
# reaches its verdict having executed NO assertions has not passed; it has not
# run, and reporting it green is the identity.rs defect at gate scope. So zero
# recorded outcomes is itself an UNRUN.
gate_verdict() {
  if [ $((GATE_PASS + GATE_FAIL + GATE_UNRUN)) -eq 0 ]; then
    gate_unrun "$GATE_NAME: reached its verdict having executed no assertions"
  fi
  printf '%s: %d passed, %d failed, %d unrun\n' \
    "$GATE_NAME" "$GATE_PASS" "$GATE_FAIL" "$GATE_UNRUN"
  [ "$GATE_FAIL" -eq 0 ] && [ "$GATE_UNRUN" -eq 0 ]
}

# gate_on_exit — the EXIT trap. `local rc=$?` MUST be the first statement.
#
# The contract line is derived from the exit code, so a gate that never reached
# an assertion still reports FAIL. Emitting it only when no failure was recorded
# keeps `grep -c '^FAIL '` an exact count of failures rather than failures plus
# one.
gate_on_exit() {
  local rc=$?
  if [ -n "$GATE_TALLY_HOOK" ]; then
    "$GATE_TALLY_HOOK" "$rc" || true
  fi
  # A GREEN EXIT OVER ZERO EXECUTED ASSERTIONS IS THE THIRD STATE, NOT THE
  # FIRST. This is the path that catches a gate whose body was skipped whole —
  # a guard clause that returned early, a corpus that was not there — which
  # otherwise exits 0 in silence and reads as a pass. The trap overrides the
  # status, so "did not run" cannot be reported green.
  if [ "$GATE_CONTRACT_LINE_EMITTED" -eq 0 ] && [ "$rc" -eq 0 ] \
    && [ $((GATE_PASS + GATE_FAIL + GATE_UNRUN)) -eq 0 ]; then
    GATE_CONTRACT_LINE_EMITTED=1
    gate_unrun "${GATE_NAME:-gate}: exited 0 having executed no assertions"
    exit 1
  fi
  # An UNRUN already carries the FAIL token, so it counts as "reported". Testing
  # GATE_FAIL alone here emitted a SECOND FAIL line for a gate whose only
  # not-passing outcome was an UNRUN, which broke `grep -c '^FAIL '` as an exact
  # count. Found by the gate_unrun control, not by reading.
  if [ "$GATE_CONTRACT_LINE_EMITTED" -eq 0 ] \
    && [ "$rc" -ne 0 ] && [ $((GATE_FAIL + GATE_UNRUN)) -eq 0 ]; then
    GATE_CONTRACT_LINE_EMITTED=1
    printf 'FAIL %s: exited %s without reporting a failure; every assertion after that point did not run\n' \
      "${GATE_NAME:-gate}" "$rc"
  fi
  return "$rc"
}
