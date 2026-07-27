#!/usr/bin/env bash
# =============================================================================
# g0_proof_lanes_e2e.sh — the gate that actually runs the provers
# =============================================================================
# Owner bead: fgdb-dkrc, the residue of
# fgdb-proof-lane-checked-is-only-file-existence-0f1l.
#
# 0f1l made `status = "checked"` mean something: a checked lane must name a
# `checked_by` gate that liveness.rs proves live, and the artifact itself must be
# able to fail (a Lean file states a proposition and carries no sorry/admit/
# axiom/native_decide; a TLA+ model has a .cfg naming an INVARIANT or PROPERTY).
# What 0f1l explicitly did NOT do is run anything: there was no `lean`, no `lake`
# and no TLC invocation anywhere in this repository, so `checked_by` could not
# resolve — there was no gate to name. THIS SCRIPT IS THAT GATE.
#
# WHY IT REFUSES TO BE VACUOUS. A proof-lane runner that reports green while
# checking zero proofs is 0f1l's defect wearing a different hat, so:
#   * zero checked lanes is a FAILURE, not a pass — the whole point is that at
#     least one proof is actually machine-checked;
#   * a missing prover is a FAILURE, never a skip. `scripts/check.sh` already
#     takes this line with shellcheck ("Refusing to report green on an unrun
#     check"), and an absent prover reported as green is exactly the shape of
#     fgdb-shell-lint-silent-no-op-xi8p.
#
# NOT FAIL-FAST, for the fgdb-d1d4 reason: every lane is attempted, every failure
# is recorded, and the tally prints from an EXIT trap.
#
# TOOLCHAIN NOTE, recorded here because dkrc asked for it to be written down
# somewhere before somebody misreads Doctrine #1: the closed dependency universe
# governs CRATES LINKED INTO THE BUILD. Lean and TLC are developer toolchains
# invoked as external programs, on exactly the footing `scripts/check.sh` already
# gives `shellcheck`, `cargo` and `rustfmt`. Nothing here is linked into fgdb.
# =============================================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LANES="$ROOT/registries/proof_lanes.toml"

PASS=0
FAIL=0
CHECKED_SEEN=0
TALLY_PRINTED=0

pass() { PASS=$((PASS + 1)); printf '    ok    %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); printf '    FAIL  %s\n' "$1" >&2; }

print_tally() {
  [ "$TALLY_PRINTED" -eq 1 ] && return 0
  TALLY_PRINTED=1
  echo
  echo "  proof-lane gate: $PASS passed, $FAIL failed, $CHECKED_SEEN checked lane(s) run"
}
trap print_tally EXIT

echo "== g0 proof-lane gate =="

[ -f "$LANES" ] || { fail "registries/proof_lanes.toml is missing"; exit 1; }

# Structural read of the [[lane]] blocks: id, lane, artifact, status. Parsed as
# records rather than grepped, because a substring search over this file would
# happily pair one lane's id with another lane's status.
LANE_RECORDS="$(awk '
  /^\[\[lane\]\]/ { if (id != "") emit(); id=""; kind=""; art=""; st=""; next }
  /^id = "/       { id   = val($0) }
  /^lane = "/     { kind = val($0) }
  /^artifact = "/ { art  = val($0) }
  /^status = "/   { st   = val($0) }
  END { if (id != "") emit() }
  function val(line,   n) { n = split(line, p, "\""); return p[2] }
  function emit() { print id "\t" kind "\t" art "\t" st }
' "$LANES")"

TOTAL_LANES="$(printf '%s\n' "$LANE_RECORDS" | grep -c . || true)"
# CONTROL. Ten lanes are registered; a reader that returns none is broken, and
# every verdict below would then be quantified over nothing.
if [ "$TOTAL_LANES" -eq 0 ]; then
  fail "parsed ZERO lanes from proof_lanes.toml — a zero here cannot be distinguished from a broken reader"
  exit 1
fi
echo "  $TOTAL_LANES lane(s) registered"

while IFS=$'\t' read -r id kind artifact status; do
  [ -z "$id" ] && continue
  [ "$status" != "checked" ] && continue
  CHECKED_SEEN=$((CHECKED_SEEN + 1))

  if [ ! -f "$ROOT/$artifact" ]; then
    fail "$id: checked lane's artifact does not exist: $artifact"
    continue
  fi

  case "$kind" in
    lean)
      if ! command -v lean >/dev/null 2>&1; then
        fail "$id: lean is not installed; the proof was NOT checked. Refusing to report green on an unrun prover."
        continue
      fi
      if lean "$ROOT/$artifact" >/tmp/lane_$$.log 2>&1; then
        # LEAN EXITS 0 ON `sorry`. It is a warning, not an error: a file whose
        # every theorem is admitted typechecks and returns success. Measured by
        # red-proof — appending `theorem cheat : False := by sorry` left this
        # gate GREEN until this branch existed. That is 0f1l's exact defect
        # reappearing one layer out, in the runner rather than the registry.
        #
        # liveness.rs already scans the SOURCE for admit tokens. This is not a
        # second reader of that fact: it reads the PROVER'S VERDICT, which is
        # the authority, and catches admits no text scan can see (a `sorry`
        # produced by a macro, or `sorryAx` reached through elaboration).
        if grep -q "declaration uses 'sorry'" /tmp/lane_$$.log; then
          fail "$id: lean exited 0 but the proof is ADMITTED — $(grep -c "declaration uses 'sorry'" /tmp/lane_$$.log) declaration(s) use \`sorry\`"
        else
          pass "$id: lean accepted $artifact ($(lean --version | head -1))"
        fi
      else
        fail "$id: lean REJECTED $artifact"
        sed 's/^/        /' /tmp/lane_$$.log >&2
      fi
      rm -f /tmp/lane_$$.log
      ;;
    tlaplus)
      # TLC is not installed on this host and no tla2tools.jar is on disk. That
      # is a hard failure for a CHECKED lane, never a skip: a model checker that
      # did not run has proved nothing, and saying so is the entire lesson of
      # 0f1l. Declared TLA+ lanes are untouched — they are not checked and this
      # loop never reaches them.
      if ! command -v tlc >/dev/null 2>&1 && [ -z "${TLA2TOOLS_JAR:-}" ]; then
        fail "$id: no TLC and no TLA2TOOLS_JAR; the model was NOT checked. Refusing to report green on an unrun model checker."
        continue
      fi
      cfg="${artifact%.tla}.cfg"
      if [ ! -f "$ROOT/$cfg" ]; then
        fail "$id: checked TLA+ lane has no companion .cfg: $cfg"
        continue
      fi
      if [ -n "${TLA2TOOLS_JAR:-}" ]; then
        runner=(java -cp "$TLA2TOOLS_JAR" tlc2.TLC -config "$ROOT/$cfg" "$ROOT/$artifact")
      else
        runner=(tlc -config "$ROOT/$cfg" "$ROOT/$artifact")
      fi
      if "${runner[@]}" >/tmp/lane_$$.log 2>&1; then
        pass "$id: TLC accepted $artifact"
      else
        fail "$id: TLC REJECTED $artifact"
        tail -20 /tmp/lane_$$.log | sed 's/^/        /' >&2
      fi
      rm -f /tmp/lane_$$.log
      ;;
    *)
      fail "$id: unknown lane system '$kind' — no runner exists for it, so it cannot be checked"
      ;;
  esac
done <<< "$LANE_RECORDS"

# CONTROL. See the header: a proof-lane runner that checks nothing and reports
# green is the defect 0f1l closed, one layer out.
if [ "$CHECKED_SEEN" -eq 0 ]; then
  fail "NO lane has status = \"checked\", so this gate proved nothing. A green here would mean only that no proof was attempted."
fi

print_tally
[ "$FAIL" -eq 0 ] || exit 1
exit 0
