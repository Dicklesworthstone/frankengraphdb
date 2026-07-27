#!/usr/bin/env bash
# =============================================================================
# g0_tree_stability_e2e.sh — the tripwire that makes a voided run legible, guarded
# =============================================================================
# Owner bead: fgdb-49ng (fix landed 38cca3f). Durable-regression bead: fgdb-mt2b.
#
# WHAT IS UNDER TEST. `scripts/lib/gate_verdict.sh` carries a tree-stability
# tripwire — gate_tree_fingerprint / gate_tree_head / gate_check_tree_stable —
# which makes a gate whose subject moved underneath it report UNRUN carrying both
# shas, instead of a PASS certifying a tree that was never wholly tested or a FAIL
# blaming whichever pane's code happened to be resident when the assertion fired.
#
# WHY IT NEEDS A GATE OF ITS OWN. The tripwire is the only thing in this repo
# that notices its own absence. Neuter gate_check_tree_stable and EVERY gate in
# the tree stays green — measured twice, at 38cca3f and again at 72aec3f: with it
# neutered, a gate raced by a real commit reports PASS rc=0, a green verdict over
# a moving tree, and nothing else anywhere reports anything. Until this file
# existed the fix was protected only by a transcript in /data/tmp.
#
# WHY THIS IS SYNTHETIC AND NOT A CLONE OF THE REPO. The original red-proof drove
# a real g0 gate inside a `git clone --local` and raced it with `sleep 0.35`. That
# is 61 MB per run (~25 MB of new blocks; .git is hardlinked) and it is a RACE, so
# it can flake. Here the subject is a minimal git repo of a few KB whose gate
# sources the REAL library under test, and the tree moves at a point the subject
# CHOOSES rather than one a sleep hopes for. Cheaper, deterministic, and it can
# reach a window the racing version could not — see case E.
#
# WHY EVERY DIRECTION IS TESTED, not just the firing one. A tripwire that only
# ever fires is as useless as one that never does: it would turn every gate UNRUN
# and be disabled within a day. Case B is the one that matters most on review —
# a genuinely red gate on a STILL tree must still report FAIL, because a third
# state that swallows real failures is worse than the false red it replaced.
#
# WHY CASE F EXISTS. Cases C/D/E prove an UNRUN appears. They do NOT prove the
# TRIPWIRE produced it — some other guard could be. F neuters
# gate_check_tree_stable in the subject's copy of the library and asserts the
# UNRUN STOPS. Without F this gate would stay green if the tripwire were deleted
# and something else coincidentally failed. The mutation is asserted to have
# applied, so F cannot pass vacuously by silently matching nothing.
#
# WHY THIS IS NOT FAIL-FAST. `set -e` is deliberately not set, for the reason
# recorded in g0_negative_evidence_e2e.sh: a gate that stops at its first red
# reports its own evaluation order rather than the tree's state.
#
# THIS GATE NEVER TOUCHES THE REPOSITORY IT LIVES IN. Every commit it makes is
# inside a scratch repo under $WORK_ROOT. Committing in the live worktree to prove
# this would void whatever real `scripts/check.sh` is in flight — the exact harm
# fgdb-49ng exists to remove.
#
# IT DELETES NOTHING. Per RULE 1 of AGENTS.md this script removes no file or
# directory, so its scratch accumulates: roughly 200 KB per run under
# $WORK_ROOT, inventoried by scripts/disk_hygiene.sh and reclaimed by a human.
# That is ~0.3% of the 61 MB the clone-based version would have left, and the
# reason this design was chosen over the clone.
# =============================================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$ROOT/scripts/lib/gate_verdict.sh"

# shellcheck source=lib/gate_verdict.sh
. "$LIB"

gate_init "g0_tree_stability_e2e"

WORK_ROOT="${FGDB_GATE_TMP:-/data/tmp/fgdb_swarm/g0_tree_stability}"
CASES_RUN=0

if [ ! -f "$LIB" ]; then
  gate_die "the library under test is missing: $LIB"
fi
if ! mkdir -p "$WORK_ROOT" 2>/dev/null; then
  gate_die "cannot create scratch root $WORK_ROOT" \
    "  Set FGDB_GATE_TMP to a writable directory."
fi
RUN_DIR="$(mktemp -d "$WORK_ROOT/run-XXXXXX" 2>/dev/null)"
if [ -z "$RUN_DIR" ] || [ ! -d "$RUN_DIR" ]; then
  gate_die "cannot create a scratch run directory under $WORK_ROOT"
fi
gate_diag "scratch: $RUN_DIR (left in place; this gate deletes nothing)"

# -----------------------------------------------------------------------------
# The subject: a minimal gate that sources the real library and can move its own
# tree at a chosen moment.
#
# SUBJ_WHEN  = none | before | after   -- before/after gate_verdict
# SUBJ_HOW   = commit | worktree       -- a landing, or an uncommitted tracked edit
# SUBJ_RED   = 0 | 1                   -- whether it has a genuine failure
# -----------------------------------------------------------------------------
write_subject() {
  cat >"$1/subject.sh" <<'SUBJECT'
#!/usr/bin/env bash
set -uo pipefail
D="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$D/lib/gate_verdict.sh"

move_tree() {
  printf 'moved %s\n' "$$" >>"$D/data.txt"
  if [ "${SUBJ_HOW:-commit}" = commit ]; then
    git -C "$D" add data.txt >/dev/null 2>&1
    git -C "$D" -c user.email=t@t -c user.name=t commit -q -m "a pane lands mid-run" >/dev/null 2>&1
  fi
}

cd "$D" || exit 1
gate_init "subject"
if [ "${SUBJ_WHEN:-none}" = before ]; then move_tree; fi
gate_pass "an assertion that does not depend on the tree"
if [ "${SUBJ_RED:-0}" = 1 ]; then
  gate_fail "a genuine, tree-independent failure"
fi
if [ "${SUBJ_WHEN:-none}" = after ]; then
  # Move the tree and exit 0 WITHOUT ever reaching gate_verdict. This is the only
  # window in which the EXIT trap's own tree check is reachable: gate_verdict sets
  # GATE_TREE_CHECKED=1, which makes the trap's call a no-op, so a gate that
  # reaches its verdict can never exercise the trap's branch.
  move_tree
  exit 0
fi
gate_verdict
exit "$?"
SUBJECT
}

# make_repo <dir> [neuter]  — a scratch git repo carrying the library under test.
make_repo() {
  local d="$1" neuter="${2:-no}"
  mkdir -p "$d/lib" || return 1
  cp "$LIB" "$d/lib/gate_verdict.sh" || return 1
  if [ "$neuter" = neuter ]; then
    # Disable ONLY the tripwire, by making it return before it compares.
    sed -i 's/^  \[ "\$GATE_TREE_CHECKED" -eq 1 \] \&\& return 0$/  return 0/' \
      "$d/lib/gate_verdict.sh" || return 1
    # The mutation must be asserted to have applied; a sed that matched nothing
    # would make case F vacuously green, which is the shape this repo keeps
    # closing. Compare against the pristine library.
    if cmp -s "$LIB" "$d/lib/gate_verdict.sh"; then
      return 2
    fi
  fi
  write_subject "$d"
  printf 'initial\n' >"$d/data.txt"
  (
    cd "$d" || exit 1
    git init -q -b main >/dev/null 2>&1
    git add -A >/dev/null 2>&1
    git -c user.email=t@t -c user.name=t commit -q -m initial >/dev/null 2>&1
  ) || return 1
  return 0
}

# run_case <label> <when> <how> <red> <want_token> <want_rc> [neuter]
#   want_rc = zero | nonzero
run_case() {
  local label="$1" when="$2" how="$3" red="$4" want="$5" want_rc="$6" neuter="${7:-no}"
  local d="$RUN_DIR/$label" out err rc token unrun mk
  d="${d// /_}"
  make_repo "$d" "$neuter"
  mk=$?
  if [ "$mk" -eq 2 ]; then
    gate_unrun "$label: the neutering mutation matched nothing, so this control is vacuous"
    gate_diag "  The sed target in gate_check_tree_stable has moved. Re-derive it;"
    gate_diag "  do NOT delete this case — a control that cannot fire is the defect."
    CASES_RUN=$((CASES_RUN + 1))
    return
  fi
  if [ "$mk" -ne 0 ]; then
    gate_fail "$label: could not build the scratch subject repo"
    CASES_RUN=$((CASES_RUN + 1))
    return
  fi

  out="$d/out.txt"; err="$d/err.txt"
  SUBJ_WHEN="$when" SUBJ_HOW="$how" SUBJ_RED="$red" \
    bash "$d/subject.sh" >"$out" 2>"$err"
  rc=$?

  token=NONE
  if grep -q '^UNRUN ' "$out"; then token=UNRUN
  elif grep -q '^FAIL ' "$out"; then token=FAIL
  elif grep -q '^PASS ' "$out"; then token=PASS
  fi
  unrun=$(grep -c '^UNRUN ' "$out")

  local ok=1
  if [ "$token" != "$want" ]; then ok=0; fi
  if [ "$want_rc" = zero ] && [ "$rc" -ne 0 ]; then ok=0; fi
  if [ "$want_rc" = nonzero ] && [ "$rc" -eq 0 ]; then ok=0; fi

  if [ "$ok" -eq 1 ]; then
    gate_pass "$label: token=$token rc=$rc (want $want/$want_rc)"
  else
    gate_fail "$label: token=$token rc=$rc (want $want/$want_rc)"
    gate_diag "  --- subject stdout ---"
    while IFS= read -r line; do gate_diag "  $line"; done <"$out"
    gate_diag "  --- subject stderr ---"
    while IFS= read -r line; do gate_diag "  $line"; done <"$err"
  fi
  CASES_RUN=$((CASES_RUN + 1))

  CASE_ERR="$err"; CASE_UNRUN="$unrun"; CASE_RC="$rc"
}

# --- A: still tree, subject green -> PASS, exit 0 ----------------------------
run_case A_still_green none commit 0 PASS zero

# --- B: still tree, subject GENUINELY RED -> FAIL, and NOT UNRUN -------------
# The load-bearing negative direction: the third state must not swallow a real
# failure, or it is a worse defect than the false red it replaced.
run_case B_still_red none commit 1 FAIL nonzero
if [ "${CASE_UNRUN:-0}" -eq 0 ]; then
  gate_pass "B: a real failure on a still tree emitted no UNRUN (the third state did not swallow it)"
else
  gate_fail "B: UNRUN swallowed a genuine failure on a still tree"
fi

# --- C: a commit lands mid-run -> UNRUN, non-zero, BOTH SHAS AND DIFFERENT ---
run_case C_landing_midrun before commit 0 UNRUN nonzero
c_start=$(grep -oE 'HEAD at start: [0-9a-f]+' "${CASE_ERR:-/dev/null}" 2>/dev/null | awk '{print $4}')
c_end=$(grep -oE 'HEAD at end: +[0-9a-f]+' "${CASE_ERR:-/dev/null}" 2>/dev/null | awk '{print $4}')
if [ -n "$c_start" ] && [ -n "$c_end" ] && [ "$c_start" != "$c_end" ]; then
  gate_pass "C: diagnostic carries both shas and they differ (${c_start:0:12} -> ${c_end:0:12})"
else
  gate_fail "C: diagnostic did not carry two differing shas (start='$c_start' end='$c_end')"
fi

# --- D: an UNCOMMITTED tracked edit -> UNRUN, with HEAD EQUAL at both ends ---
# This is the case that kills a HEAD-only fingerprint: HEAD never moves, so a
# design that hashed only HEAD would report PASS on a run whose subject changed.
run_case D_worktree_edit before worktree 0 UNRUN nonzero
d_start=$(grep -oE 'HEAD at start: [0-9a-f]+' "${CASE_ERR:-/dev/null}" 2>/dev/null | awk '{print $4}')
d_end=$(grep -oE 'HEAD at end: +[0-9a-f]+' "${CASE_ERR:-/dev/null}" 2>/dev/null | awk '{print $4}')
if [ -n "$d_start" ] && [ "$d_start" = "$d_end" ]; then
  gate_pass "D: HEAD identical at both ends, so content — not HEAD — caught the move"
else
  gate_fail "D: expected an unchanged HEAD (start='$d_start' end='$d_end')"
fi
if grep -q 'working-tree or index edit' "${CASE_ERR:-/dev/null}" 2>/dev/null; then
  gate_pass "D: diagnostic names it a working-tree/index edit rather than a landing"
else
  gate_fail "D: diagnostic did not distinguish a worktree edit from a landing"
fi

# --- E: the EXIT-TRAP WINDOW — a gate that exits 0 without a verdict ---------
# The window that caught the original defect: the first cut of gate_on_exit
# assigned `rc=1` instead of calling `exit`, so a raced gate printed UNRUN on
# stdout while EXITING 0 — "did not run" and "success" in the same breath.
#
# REACHING IT IS NARROWER THAN IT LOOKS, and getting this wrong is why the first
# version of this case failed. gate_verdict sets GATE_TREE_CHECKED=1, so once a
# gate reaches its verdict the trap's own tree check is a no-op and the branch is
# unreachable. It is live ONLY for a gate that exits WITHOUT reaching
# gate_verdict — a gate_die, an unguarded `set -e` abort, an explicit early exit
# — which is precisely the fail-fast path where a moving tree would otherwise be
# blamed on whichever code was resident. The subject here exits 0 after its
# assertions and never calls gate_verdict.
#
# The token alone would pass against the original bug. THE EXIT CODE IS THE
# ASSERTION.
run_case E_trap_window_no_verdict after commit 0 UNRUN nonzero
if [ "${CASE_RC:-0}" -ne 0 ]; then
  gate_pass "E: the EXIT trap overrode a would-be 0 exit (rc=${CASE_RC}) — it called exit, not rc="
else
  gate_fail "E: UNRUN reported while exiting 0 — the trap used an assignment, not exit"
fi

# --- F: MUTATION CONTROL — neuter the tripwire, the UNRUN must STOP ----------
# Proves cases C/D/E are produced by gate_check_tree_stable and not by something
# else. If this case reports UNRUN, the neutering did not disable what C/D/E are
# attributing their result to, and none of them are evidence about the tripwire.
run_case F_mutation_control before commit 0 PASS zero neuter

# --- the zero is licensed by accounting, not by finding nothing --------------
if [ "$CASES_RUN" -ne 6 ]; then
  gate_unrun "expected 6 cases to execute, $CASES_RUN did"
fi

gate_verdict
