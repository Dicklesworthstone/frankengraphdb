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
#    `UNRUN ` as refinements (scripts/check.sh distinguishes a gate that ran and
#    failed from one that never ran), but only BESIDE a FAIL line, never instead
#    of one — so the single query never has to know which gate it is reading.
#    The vocabulary is closed: PASS, FAIL, RED, UNRUN. Nothing else may appear
#    at column 0 of stdout.
#
# 3. ONE EXIT DISCIPLINE.  Exit 0 if and only if zero FAIL lines were emitted
#    and the gate reached its verdict. THE EXIT CODE REMAINS AUTHORITATIVE — the
#    tokens exist so that a reader who greps anyway gets a true answer, not so
#    that grepping becomes the right way to read a gate.
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

# gate_verdict — the tally line; returns nonzero when anything failed.
gate_verdict() {
  printf '%s: %d passed, %d failed\n' "$GATE_NAME" "$GATE_PASS" "$GATE_FAIL"
  [ "$GATE_FAIL" -eq 0 ]
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
  if [ "$GATE_CONTRACT_LINE_EMITTED" -eq 0 ] \
    && [ "$rc" -ne 0 ] && [ "$GATE_FAIL" -eq 0 ]; then
    GATE_CONTRACT_LINE_EMITTED=1
    printf 'FAIL %s: exited %s without reporting a failure; every assertion after that point did not run\n' \
      "${GATE_NAME:-gate}" "$rc"
  fi
  return "$rc"
}
