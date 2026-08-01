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
# WHY CASES G-N EXIST. Detection alone still discarded nearly every full gate:
# routine `br` writes rewrote the tracked JSONL every few minutes. The project
# now disables automatic export and routes explicit export through br_sync.sh.
# G-L prove every lease direction plus a neutered deferral. M drives the
# deployed `br` binary and proves that one declared id cannot sweep a second
# pending record, then proves that declaring both ids exports exactly both. N
# neuters both attribution guards and requires the silent sweep to return. A
# wrapper-only test would miss a config regression, while a config-only test
# would miss either an unguarded explicit sync or an unattributed whole-file
# export.
#
# WHY CASE O EXISTS. Prevention makes routine Beads movement rare; it does not
# make every other tracked write impossible. check.sh therefore attributes a
# whole-run movement by each child gate's declared input domain. O runs
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
case "$*" in
  "sync --status --json")
    printf '{"dirty_count":1}\n'
    ;;
  "sync --flush-only")
    if [ -n "${BR_STUB_APPEND_ID:-}" ]; then
      printf '{"id":"%s","status":"open"}\n' "$BR_STUB_APPEND_ID" \
        >>.beads/issues.jsonl
    fi
    exit "${BR_STUB_RC:-0}"
    ;;
  *)
    exit 2
    ;;
esac
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
  local project="$d/project"
  mkdir -p "$d/tokens" "$project/.beads" || return 1
  write_br_stub "$d/br-stub" || return 1
  write_token_stub "$d/token-stub" || return 1
  printf '{"id":"fgdb-export-fixture","status":"open"}\n' \
    >"$project/.beads/issues.jsonl" || return 1
  git -C "$project" init -q || return 1
  git -C "$project" config user.email gate@example.invalid || return 1
  git -C "$project" config user.name fgdb-gate || return 1
  git -C "$project" config commit.gpgsign false || return 1
  git -C "$project" add .beads/issues.jsonl || return 1
  git -C "$project" commit -qm 'fixture: baseline Beads export' || return 1
  printf '{"id":"fgdb-export-fixture","status":"closed"}\n' \
    >"$project/.beads/issues.jsonl" || return 1

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
#                 [helper] [landing_lib] [raced_record_id]
run_export_case() {
  local label="$1" state="$2" want_calls="$3" want_rc="$4" diagnostic="$5"
  local helper="${6:-$BR_SYNC}" landing_lib="${7:-$LANDING_LIB}"
  local raced_record_id="${8:-}"
  local d="$RUN_DIR/$label" log token_log expected_calls out err rc calls ok=1

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
  (
    cd "$d/project" || exit 1
    BR_STUB_LOG="$log" FGDB_BR_BIN="$d/br-stub" \
      TOKEN_STUB_LOG="$token_log" FGDB_TOKEN_SH="$d/token-stub" \
      FGDB_TOKEN_DIR="$d/tokens" FGDB_LANDING_LIB="$landing_lib" \
      BR_STUB_APPEND_ID="$raced_record_id" \
      bash "$helper" fgdb-export-fixture
  ) >"$out" 2>"$err"
  rc=$?
  calls="$(wc -l <"$log")"

  [ "$calls" -eq "$want_calls" ] || ok=0
  [ "$rc" -eq "$want_rc" ] || ok=0
  if [ "$calls" -gt 0 ]; then
    expected_calls="$d/expected-br-calls.log"
    printf '%s\n' 'sync --status --json' 'sync --flush-only' >"$expected_calls"
    cmp -s "$expected_calls" "$log" || ok=0
    if [ "$want_rc" -eq 0 ]; then
      grep -Fq 'fgdb-export-fixture' "$out" || ok=0
    fi
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

# G: no lease -> the status audit and explicit export each run exactly once.
run_export_case G_export_free FREE 2 0 NONE

# G2: a record racing in after the pre-check makes the post-export exact-set
# audit fail closed and names the undeclared id.
run_export_case G2_export_race_attribution FREE 2 65 'fgdb-raced-foreign' \
  "$BR_SYNC" "$LANDING_LIB" fgdb-raced-foreign

# H: a live holder -> no tracked export, temporary failure for an explicit retry.
run_export_case H_export_binding BINDING 0 75 'DEFERRED BEADS EXPORT'

# I/J/K: prevention uncertainty fails open loudly; detection remains behind it.
run_export_case I_export_breakable BREAKABLE 2 0 'state   : BREAKABLE'
run_export_case J_export_unreadable UNREADABLE 2 0 'state   : UNREADABLE'
run_export_case K_export_missing_library FREE 2 0 'landing-lease library is missing' \
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
  run_export_case L_export_mutation_control BINDING 2 0 NONE "$MUTATED_BR_SYNC"
fi

# Build one committed JSONL baseline plus two DB-only records with the deployed
# `br`. The caller gets the two pending ids in id-one.txt and id-two.txt.
make_real_attribution_fixture() {
  local d="$1" br_bin="$2"
  local project="$d/project"
  local before after dirty

  mkdir -p "$project" "$d/tokens-refuse" "$d/tokens-export" \
    "$d/tokens-mutant" || return 1
  write_token_stub "$d/token-stub" || return 1
  : >"$d/token-calls.log"

  (
    cd "$project" || exit 1
    RUST_LOG=error "$br_bin" --quiet init --prefix probe \
      >"$d/init.out" 2>"$d/init.err" || exit 1
    RUST_LOG=error "$br_bin" --quiet create --title='control record' \
      --type=task --priority=2 \
      >"$d/create-control.out" 2>"$d/create-control.err" || exit 1
    cp "$BR_CONFIG" .beads/config.yaml || exit 1

    git init -q || exit 1
    git config user.email gate@example.invalid || exit 1
    git config user.name fgdb-gate || exit 1
    git config commit.gpgsign false || exit 1
    git add -f .beads/issues.jsonl || exit 1
    git commit -qm 'fixture: baseline Beads export' || exit 1

    before="$(sha256sum .beads/issues.jsonl | awk '{print $1}')"
    RUST_LOG=error "$br_bin" --json create --title='first deferred record' \
      --type=task --priority=2 \
      >"$d/create-one.out" 2>"$d/create-one.err" || exit 1
    RUST_LOG=error "$br_bin" --json create --title='second deferred record' \
      --type=task --priority=2 \
      >"$d/create-two.out" 2>"$d/create-two.err" || exit 1
    jq -er '.id' "$d/create-one.out" >"$d/id-one.txt" || exit 1
    jq -er '.id' "$d/create-two.out" >"$d/id-two.txt" || exit 1
    RUST_LOG=error "$br_bin" sync --status --json \
      >"$d/status.out" 2>"$d/status.err" || exit 1
    dirty="$(jq -er '.dirty_count' "$d/status.out")" || exit 1
    after="$(sha256sum .beads/issues.jsonl | awk '{print $1}')"
    printf '%s\n' "$before" >"$d/before.sha"
    printf '%s\n' "$after" >"$d/after-create.sha"
    [ "$before" = "$after" ] || exit 1
    [ "$dirty" -eq 2 ] || exit 1
  )
}

# M: deployed-tool composition and the positive attribution contract. The DB
# advances by two records while configured JSONL stays byte-identical. Declaring
# one id refuses before write; declaring both exports exactly both.
run_project_config_case() {
  local d="$RUN_DIR/M_project_config_and_explicit_export"
  local before after_refuse after_flush lines br_bin id_one id_two
  local refuse_rc flush_rc rc=0
  br_bin="$(command -v br 2>/dev/null)"
  # A missing tool is an environment deficiency, not a doctrine failure —
  # this gate's own anti-misattribution doctrine says UNRUN, never FAIL.
  if [ -z "$br_bin" ]; then
    gate_unrun "case M: the br binary is not on PATH; the project-config export case did not run"
    exit 2
  fi

  if [ "$rc" -eq 0 ]; then
    make_real_attribution_fixture "$d" "$br_bin" || rc=1
  fi
  if [ "$rc" -eq 0 ]; then
    before="$(cat "$d/before.sha")"
    id_one="$(cat "$d/id-one.txt")"
    id_two="$(cat "$d/id-two.txt")"
    (
      cd "$d/project" || exit 1
      FGDB_BR_BIN="$br_bin" FGDB_TOKEN_DIR="$d/tokens-refuse" \
        FGDB_TOKEN_SH="$d/token-stub" TOKEN_STUB_LOG="$d/token-calls.log" \
        RUST_LOG=error bash "$BR_SYNC" "$id_one"
    ) >"$d/refuse.out" 2>"$d/refuse.err"
    refuse_rc=$?
    after_refuse="$(sha256sum "$d/project/.beads/issues.jsonl" | awk '{print $1}')"

    (
      cd "$d/project" || exit 1
      FGDB_BR_BIN="$br_bin" FGDB_TOKEN_DIR="$d/tokens-export" \
        FGDB_TOKEN_SH="$d/token-stub" TOKEN_STUB_LOG="$d/token-calls.log" \
        RUST_LOG=error bash "$BR_SYNC" "$id_two" "$id_one"
    ) >"$d/flush.out" 2>"$d/flush.err"
    flush_rc=$?
    after_flush="$(sha256sum "$d/project/.beads/issues.jsonl" | awk '{print $1}')"
    lines="$(wc -l <"$d/project/.beads/issues.jsonl")"

    [ "$refuse_rc" -eq 65 ] || rc=1
    [ "$after_refuse" = "$before" ] || rc=1
    grep -Fq '2 dirty DB records' "$d/refuse.err" || rc=1
    grep -Fq "$id_one" "$d/refuse.err" || rc=1
    [ "$flush_rc" -eq 0 ] || rc=1
    [ "$after_flush" != "$before" ] || rc=1
    [ "$lines" -eq 3 ] || rc=1
    grep -Fqx "$id_one" <(
      jq -r '.id' "$d/project/.beads/issues.jsonl"
    ) || rc=1
    grep -Fqx "$id_two" <(
      jq -r '.id' "$d/project/.beads/issues.jsonl"
    ) || rc=1
    grep -Fq "$id_one" "$d/flush.out" || rc=1
    grep -Fq "$id_two" "$d/flush.out" || rc=1
  fi

  if [ "$rc" -eq 0 ]; then
    gate_pass "M: one-id intent refused two dirty records byte-stably; exact two-id intent exported both"
  else
    gate_fail "M: project auto-flush and exact record-id attribution did not compose"
    for artifact in init.out init.err create-control.out create-control.err \
      create-one.out create-one.err create-two.out create-two.err status.out \
      status.err refuse.out refuse.err flush.out flush.err token-calls.log; do
      if [ -f "$d/$artifact" ]; then
        gate_diag "  --- $artifact ---"
        while IFS= read -r line; do gate_diag "  $line"; done <"$d/$artifact"
      fi
    done
  fi
  EXPORT_CASES_RUN=$((EXPORT_CASES_RUN + 1))
}
run_project_config_case

# N: MUTATION CONTROL — remove both dirty-count and post-export exact-set
# enforcement. A one-id declaration must then silently export both dirty rows,
# proving M is evidence about the attribution guards rather than incidental br
# behavior.
run_attribution_mutation_case() {
  local d="$RUN_DIR/N_attribution_mutation_control"
  local br_bin mutant before after id_one id_two lines mutation_count rc=0

  br_bin="$(command -v br 2>/dev/null)"
  if [ -z "$br_bin" ]; then
    gate_unrun "case N: the br binary is not on PATH; the attribution-mutation control did not run"
    exit 2
  fi
  if [ "$rc" -eq 0 ]; then
    make_real_attribution_fixture "$d" "$br_bin" || rc=1
  fi
  if [ "$rc" -eq 0 ]; then
    mutant="$d/br_sync.sh"
    cp "$BR_SYNC" "$mutant" || rc=1
    sed -i \
      -e 's/^  refuse_impossible_dirty_count || return \$?$/  : # MUTANT admits undeclared dirty records/' \
      -e 's/^  verify_exported_ids$/  : # MUTANT suppresses exact-set verification/' \
      "$mutant" || rc=1
    mutation_count="$(grep -c '^  : # MUTANT ' "$mutant")"
    [ "$mutation_count" -eq 2 ] || rc=1
  fi
  if [ "$rc" -eq 0 ]; then
    before="$(cat "$d/before.sha")"
    id_one="$(cat "$d/id-one.txt")"
    id_two="$(cat "$d/id-two.txt")"
    (
      cd "$d/project" || exit 1
      FGDB_BR_BIN="$br_bin" FGDB_TOKEN_DIR="$d/tokens-mutant" \
        FGDB_TOKEN_SH="$d/token-stub" TOKEN_STUB_LOG="$d/token-calls.log" \
        FGDB_LANDING_LIB="$LANDING_LIB" \
        RUST_LOG=error bash "$mutant" "$id_one"
    ) >"$d/mutant.out" 2>"$d/mutant.err"
    rc=$?
    after="$(sha256sum "$d/project/.beads/issues.jsonl" | awk '{print $1}')"
    lines="$(wc -l <"$d/project/.beads/issues.jsonl")"
    [ "$after" != "$before" ] || rc=1
    [ "$lines" -eq 3 ] || rc=1
    grep -Fqx "$id_two" <(
      jq -r '.id' "$d/project/.beads/issues.jsonl"
    ) || rc=1
  fi

  if [ "$rc" -eq 0 ]; then
    gate_pass "N: neutering both attribution guards restored a silent undeclared-record sweep"
  else
    gate_fail "N: attribution mutation control did not restore the forbidden sweep"
    for artifact in create-one.out create-two.out status.out mutant.out \
      mutant.err token-calls.log; do
      if [ -f "$d/$artifact" ]; then
        gate_diag "  --- $artifact ---"
        while IFS= read -r line; do gate_diag "  $line"; done <"$d/$artifact"
      fi
    done
  fi
  EXPORT_CASES_RUN=$((EXPORT_CASES_RUN + 1))
}
run_attribution_mutation_case

# O: the aggregate scoping layer and both of its mutation controls.
run_scope_case() {
  local out="$RUN_DIR/O_scoped_aggregate.out"
  local err="$RUN_DIR/O_scoped_aggregate.err"
  local rc

  bash "$CHECK_SH" --self-test >"$out" 2>"$err"
  rc=$?
  if [ "$rc" -eq 0 ] \
    && grep -Fq "tree-domain scoping: Beads, Rust, shell, and gate-driver movements separate" "$out"; then
    gate_pass "O: aggregate domain attribution and its closure mutants passed"
  else
    gate_fail "O: check.sh domain-attribution mutation panel failed (rc=$rc)"
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
if [ "$EXPORT_CASES_RUN" -ne 9 ]; then
  gate_unrun "expected 9 Beads-export cases to execute, $EXPORT_CASES_RUN did"
fi
if [ "$SCOPE_CASES_RUN" -ne 1 ]; then
  gate_unrun "expected 1 aggregate-scope case to execute, $SCOPE_CASES_RUN did"
fi

gate_verdict
