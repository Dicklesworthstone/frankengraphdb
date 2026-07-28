#!/usr/bin/env bash
# =============================================================================
# g0_tree_stability_e2e.sh — prevent and detect a tree moving under a gate
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
# WHY CASES G-M EXIST. Detection alone still discarded nearly every full gate:
# routine `br` writes rewrote the tracked JSONL every few minutes. The project
# now disables automatic export and routes explicit export through br_sync.sh.
# G-L prove every lease direction plus a neutered deferral; M drives the
# deployed `br` binary and proves the DB advances while JSONL stays byte-stable,
# then the helper exports it. A wrapper-only test would miss a config regression,
# while a config-only test would miss an unguarded explicit sync.
#
# WHY CASE N EXISTS. Prevention makes routine Beads movement rare; it does not
# make every other tracked write impossible. check.sh therefore attributes a
# whole-run movement by each child gate's declared input domain. N runs
# check.sh's mutation panel, which separates Beads, Rust, shell and gate-driver
# movement, then proves declaration, ledger, live-wiring and intersection
# mutations all turn the panel red.
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
# directory, so its scratch accumulates under $WORK_ROOT, is inventoried by
# scripts/disk_hygiene.sh, and is reclaimed by a human. The prevention control
# adds one tiny isolated Beads database per run; no live repository state moves.
# =============================================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB="$ROOT/scripts/lib/gate_verdict.sh"
LANDING_LIB="$ROOT/scripts/lib/landing_lease.sh"
BR_SYNC="$ROOT/scripts/br_sync.sh"
BR_CONFIG="$ROOT/.beads/config.yaml"
CHECK_SH="$ROOT/scripts/check.sh"

# shellcheck source=lib/gate_verdict.sh
. "$LIB"

gate_init "g0_tree_stability_e2e"

WORK_ROOT="${FGDB_GATE_TMP:-/data/tmp/fgdb_swarm/g0_tree_stability}"
CASES_RUN=0
EXPORT_CASES_RUN=0
SCOPE_CASES_RUN=0

if [ ! -f "$LIB" ]; then
  gate_die "the library under test is missing: $LIB"
fi
if [ ! -f "$LANDING_LIB" ]; then
  gate_die "the landing-lease library under test is missing: $LANDING_LIB"
fi
if [ ! -f "$BR_SYNC" ]; then
  gate_die "the Beads export helper under test is missing: $BR_SYNC"
fi
if [ ! -f "$BR_CONFIG" ]; then
  gate_die "the Beads project config under test is missing: $BR_CONFIG"
fi
if [ ! -f "$CHECK_SH" ]; then
  gate_die "the scoped aggregate under test is missing: $CHECK_SH"
fi
# shellcheck source=lib/landing_lease.sh
. "$LANDING_LIB"
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

# -----------------------------------------------------------------------------
# The prevention layer: project-wide deferred auto-flush plus one guarded
# explicit-export path.
# -----------------------------------------------------------------------------
write_br_stub() {
  cat >"$1" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${BR_STUB_LOG:?}"
exit "${BR_STUB_RC:-0}"
STUB
  chmod +x "$1"
}

write_token_stub() {
  cat >"$1" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"${TOKEN_STUB_LOG:?}"
case "${1:-}" in
  acquire)
    lock="${FGDB_TOKEN_DIR:?}/${2:?}.lock"
    mkdir "$lock" 2>/dev/null || exit 1
    printf '%s\n%s\n%s\n' "${3:?}" "$(date +%s)" "${4:?}" >"$lock/holder"
    ;;
  release)
    # No deletion: every test has a private token directory, so leaving the
    # released fixture behind cannot block another case.
    ;;
  *)
    exit 2
    ;;
esac
STUB
  chmod +x "$1"
}

make_export_fixture() {
  local d="$1" state="$2" epoch start
  mkdir -p "$d/tokens" || return 1
  write_br_stub "$d/br-stub" || return 1
  write_token_stub "$d/token-stub" || return 1

  case "$state" in
    FREE)
      ;;
    BINDING)
      mkdir -p "$d/tokens/landing.lock" || return 1
      epoch="$(date +%s)"
      start="$(_ll_starttime "$$" 2>/dev/null)" || return 1
      printf 'test-gate\n%s\n45\n%s\n%s\n' "$epoch" "$$" "$start" \
        >"$d/tokens/landing.lock/holder" || return 1
      ;;
    BREAKABLE)
      mkdir -p "$d/tokens/landing.lock" || return 1
      epoch="$(date +%s)"
      printf 'dead-gate\n%s\n45\n999999999\n0\n' "$epoch" \
        >"$d/tokens/landing.lock/holder" || return 1
      ;;
    UNREADABLE)
      mkdir -p "$d/tokens/landing.lock" || return 1
      ;;
    *)
      return 1
      ;;
  esac
}

# run_export_case <label> <state> <want_calls> <want_rc> <diagnostic|NONE>
#                 [helper] [landing_lib]
run_export_case() {
  local label="$1" state="$2" want_calls="$3" want_rc="$4" diagnostic="$5"
  local helper="${6:-$BR_SYNC}" landing_lib="${7:-$LANDING_LIB}"
  local d="$RUN_DIR/$label" log token_log out err rc calls ok=1

  make_export_fixture "$d" "$state"
  if [ "$?" -ne 0 ]; then
    gate_fail "$label: could not build the lease/export fixture"
    EXPORT_CASES_RUN=$((EXPORT_CASES_RUN + 1))
    return
  fi

  log="$d/br-calls.log"; token_log="$d/token-calls.log"
  out="$d/out.txt"; err="$d/err.txt"
  : >"$log"
  : >"$token_log"
  BR_STUB_LOG="$log" FGDB_BR_BIN="$d/br-stub" \
    TOKEN_STUB_LOG="$token_log" FGDB_TOKEN_SH="$d/token-stub" \
    FGDB_TOKEN_DIR="$d/tokens" FGDB_LANDING_LIB="$landing_lib" \
    bash "$helper" >"$out" 2>"$err"
  rc=$?
  calls="$(wc -l <"$log")"

  [ "$calls" -eq "$want_calls" ] || ok=0
  [ "$rc" -eq "$want_rc" ] || ok=0
  if [ "$calls" -gt 0 ] && ! grep -qx 'sync --flush-only' "$log"; then
    ok=0
  fi
  if [ "$diagnostic" = NONE ]; then
    if grep -Eq '^(DEFERRED BEADS EXPORT|BEADS EXPORT LEASE WARNING)' "$err"; then
      ok=0
    fi
  elif ! grep -Fq "$diagnostic" "$err"; then
    ok=0
  fi

  if [ "$ok" -eq 1 ]; then
    gate_pass "$label: state=$state calls=$calls rc=$rc"
  else
    gate_fail "$label: state=$state calls=$calls/$want_calls rc=$rc/$want_rc"
    gate_diag "  --- helper stdout ---"
    while IFS= read -r line; do gate_diag "  $line"; done <"$out"
    gate_diag "  --- helper stderr ---"
    while IFS= read -r line; do gate_diag "  $line"; done <"$err"
    gate_diag "  --- br stub calls ---"
    while IFS= read -r line; do gate_diag "  $line"; done <"$log"
    gate_diag "  --- token stub calls ---"
    while IFS= read -r line; do gate_diag "  $line"; done <"$token_log"
  fi
  EXPORT_CASES_RUN=$((EXPORT_CASES_RUN + 1))
}

# G: no lease -> the explicit export runs exactly once.
run_export_case G_export_free FREE 1 0 NONE

# H: a live holder -> no tracked export, temporary failure for an explicit retry.
run_export_case H_export_binding BINDING 0 75 'DEFERRED BEADS EXPORT'

# I/J/K: prevention uncertainty fails open loudly; detection remains behind it.
run_export_case I_export_breakable BREAKABLE 1 0 'state   : BREAKABLE'
run_export_case J_export_unreadable UNREADABLE 1 0 'state   : UNREADABLE'
run_export_case K_export_missing_library FREE 1 0 'landing-lease library is missing' \
  "$BR_SYNC" "$RUN_DIR/K_export_missing_library/no-such-landing-lease.sh"

# L: MUTATION CONTROL — bypass the BINDING deferral and the forbidden export
# must return. The mutation is asserted to apply, so the control cannot pass on
# an obsolete sed target.
MUTATED_BR_SYNC="$RUN_DIR/L_export_mutation_control/br_sync.sh"
mkdir -p "$(dirname "$MUTATED_BR_SYNC")"
cp "$BR_SYNC" "$MUTATED_BR_SYNC"
sed -i 's/^    defer_export$/    flush_export/' "$MUTATED_BR_SYNC"
if cmp -s "$BR_SYNC" "$MUTATED_BR_SYNC"; then
  gate_unrun "L: the deferral-neutering mutation matched nothing"
  gate_diag "  Re-derive the exact BINDING branch; do not remove this control."
  EXPORT_CASES_RUN=$((EXPORT_CASES_RUN + 1))
else
  run_export_case L_export_mutation_control BINDING 1 0 NONE "$MUTATED_BR_SYNC"
fi

# M: deployed-tool composition. The DB advances to two records while the
# configured JSONL remains at one byte-identical line; the helper then exports
# the second line under a FREE lease.
run_project_config_case() {
  local d="$RUN_DIR/M_project_config_and_explicit_export"
  local before after_mutation after_flush count lines br_bin rc=0
  mkdir -p "$d/tokens" || rc=1
  write_token_stub "$d/token-stub" || rc=1
  : >"$d/token-calls.log"
  br_bin="$(command -v br 2>/dev/null)"
  [ -n "$br_bin" ] || rc=1

  if [ "$rc" -eq 0 ]; then
    (
      cd "$d" || exit 1
      "$br_bin" --quiet init --prefix probe >init.out 2>&1 || exit 1
      "$br_bin" --quiet create --title='control record' --type=task --priority=2 \
        >create-control.out 2>&1 || exit 1
      cp "$BR_CONFIG" .beads/config.yaml || exit 1
      before="$(sha256sum .beads/issues.jsonl | awk '{print $1}')"
      "$br_bin" --quiet create --title='deferred record' --type=task --priority=2 \
        >create-deferred.out 2>&1 || exit 1
      after_mutation="$(sha256sum .beads/issues.jsonl | awk '{print $1}')"
      "$br_bin" --json count >count.out 2>&1 || exit 1
      count="$(grep -oE '"count":[[:space:]]*[0-9]+' count.out \
        | tail -1 | grep -oE '[0-9]+')"
      FGDB_BR_BIN="$br_bin" FGDB_TOKEN_DIR="$d/tokens" \
        FGDB_TOKEN_SH="$d/token-stub" TOKEN_STUB_LOG="$d/token-calls.log" \
        bash "$BR_SYNC" >flush.out 2>flush.err || exit 1
      after_flush="$(sha256sum .beads/issues.jsonl | awk '{print $1}')"
      lines="$(wc -l <.beads/issues.jsonl)"
      [ "$before" = "$after_mutation" ] || exit 1
      [ "$count" = 2 ] || exit 1
      [ "$after_flush" != "$before" ] || exit 1
      [ "$lines" -eq 2 ] || exit 1
    )
    rc=$?
  fi

  if [ "$rc" -eq 0 ]; then
    gate_pass "M: deployed br deferred DB record 2, then guarded export produced JSONL line 2"
  else
    gate_fail "M: project auto-flush configuration and explicit export did not compose"
    for artifact in init.out create-control.out create-deferred.out count.out flush.out flush.err; do
      if [ -f "$d/$artifact" ]; then
        gate_diag "  --- $artifact ---"
        while IFS= read -r line; do gate_diag "  $line"; done <"$d/$artifact"
      fi
    done
  fi
  EXPORT_CASES_RUN=$((EXPORT_CASES_RUN + 1))
}
run_project_config_case

# N: the aggregate scoping layer and both of its mutation controls.
run_scope_case() {
  local out="$RUN_DIR/N_scoped_aggregate.out"
  local err="$RUN_DIR/N_scoped_aggregate.err"
  local rc

  bash "$CHECK_SH" --self-test >"$out" 2>"$err"
  rc=$?
  if [ "$rc" -eq 0 ] \
    && grep -Fq "tree-domain scoping: Beads, Rust, shell, and gate-driver movements separate" "$out"; then
    gate_pass "N: aggregate domain attribution and its closure mutants passed"
  else
    gate_fail "N: check.sh domain-attribution mutation panel failed (rc=$rc)"
    gate_diag "  --- self-test stdout ---"
    while IFS= read -r line; do gate_diag "  $line"; done <"$out"
    gate_diag "  --- self-test stderr ---"
    while IFS= read -r line; do gate_diag "  $line"; done <"$err"
  fi
  SCOPE_CASES_RUN=$((SCOPE_CASES_RUN + 1))
}
run_scope_case

# --- the zero is licensed by accounting, not by finding nothing --------------
if [ "$CASES_RUN" -ne 6 ]; then
  gate_unrun "expected 6 cases to execute, $CASES_RUN did"
fi
if [ "$EXPORT_CASES_RUN" -ne 7 ]; then
  gate_unrun "expected 7 Beads-export cases to execute, $EXPORT_CASES_RUN did"
fi
if [ "$SCOPE_CASES_RUN" -ne 1 ]; then
  gate_unrun "expected 1 aggregate-scope case to execute, $SCOPE_CASES_RUN did"
fi

gate_verdict
