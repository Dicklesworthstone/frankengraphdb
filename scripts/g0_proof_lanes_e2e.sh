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

CHECKED_SEEN=0
TALLY_PRINTED=0

# The shared verdict contract (fgdb-udco). This gate's failure line was both
# indented ("    FAIL  ...") and on stderr, so a stdout-only capture of a red run
# carried every ok line and no trace of the failure.
# shellcheck source=lib/gate_verdict.sh
. "$ROOT/scripts/lib/gate_verdict.sh"

# ONE READER of "what does an admitted Lean proof look like". The self-test below
# and the detector in the lean branch must use the SAME string, or the self-test
# only proves its own copy works while the detector rots independently — which is
# exactly what red-proof caught when this was three separate literals.
ADMIT_PATTERN="declaration uses .sorry."

pass() { gate_pass "$1"; }
fail() { gate_fail "$1"; }

print_tally() {
  [ "$TALLY_PRINTED" -eq 1 ] && return 0
  TALLY_PRINTED=1
  echo
  echo "  proof-lane gate: $GATE_PASS passed, $GATE_FAIL failed, $GATE_UNRUN unrun, $CHECKED_SEEN checked lane(s) run"
}
gate_init "g0_proof_lanes_e2e" print_tally

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

# CONTROL for the admit matcher used on every accepted Lean proof. Its wording
# is the prover's, not ours, and it HAS already changed between two versions
# installed on this host. If the matcher silently stopped matching, "no admits
# found" and "the matcher is broken" would be indistinguishable, and every
# `pass` below would be unlicensed. Both measured spellings must fire.
for spelling in "declaration uses 'sorry'" 'declaration uses `sorry`'; do
  if ! printf '%s\n' "$spelling" | grep -qE "$ADMIT_PATTERN"; then
    fail "admit-matcher self-test FAILED: it no longer matches [$spelling]. Every accepted proof below would be unlicensed."
  fi
done

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

      # THE TOOLCHAIN PIN. A proof that holds only under whatever Lean happened
      # to be installed is not a reproducible claim, and Doctrine #1 requires
      # pinned versions. `elan` resolves the toolchain by searching upward from
      # the CWD, so the prover is invoked from the artifact's own directory and
      # `formal/lean/lean-toolchain` is what selects the version. That file is
      # the single reader of "which Lean": this gate does not carry a second
      # copy of the version, it checks that the prover elan actually produced
      # matches the pin.
      #
      # MEASURED, not assumed: setting the pin to v4.32.0 makes `lean --version`
      # report 4.32.0 from that directory, so the file is load-bearing rather
      # than decorative. 4.7.0 also happens to be this host's elan default,
      # which is exactly why the pin must be checked rather than trusted — on a
      # host with a different default, an unpinned run silently changes prover.
      lane_dir="$(dirname "$ROOT/$artifact")"
      lane_file="$(basename "$artifact")"
      if [ ! -f "$lane_dir/lean-toolchain" ]; then
        fail "$id: no lean-toolchain beside $artifact — the prover version would float with whatever elan defaults to. Refusing to report a proof as reproducible when nothing pins the prover."
        continue
      fi
      pin="$(tr -d '[:space:]' < "$lane_dir/lean-toolchain")"
      pin_version="${pin##*:v}"
      if [ -z "$pin_version" ] || [ "$pin_version" = "$pin" ]; then
        fail "$id: lean-toolchain does not name a pinned version: '$pin' (expected e.g. leanprover/lean4:v4.7.0)"
        continue
      fi
      actual_version="$(cd "$lane_dir" && lean --version 2>&1 | head -1)"
      case "$actual_version" in
        *"version $pin_version,"*) ;;
        *)
          fail "$id: TOOLCHAIN PIN DRIFT — lean-toolchain pins $pin_version but the prover is: $actual_version"
          continue
          ;;
      esac

      if (cd "$lane_dir" && lean "$lane_file") >/tmp/lane_$$.log 2>&1; then
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
        # The wording is VERSION-DEPENDENT: 4.7.0 emits `declaration uses
        # 'sorry'` and 4.32.0 emits it with backticks. A matcher hard-coded to
        # one spelling silently stops detecting admits when the prover moves,
        # which is why the pin above exists and why this pattern accepts either
        # quoting. Both spellings are measured, not guessed.
        if grep -qE "$ADMIT_PATTERN" /tmp/lane_$$.log; then
          fail "$id: lean exited 0 but the proof is ADMITTED — $(grep -cE "$ADMIT_PATTERN" /tmp/lane_$$.log) declaration(s) use sorry"
        else
          pass "$id: lean accepted $artifact under pinned $pin_version"
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
# Three states, not two: an UNRUN lane is not a passing lane (fgdb-udco).
if [ "$GATE_FAIL" -ne 0 ] || [ "$GATE_UNRUN" -ne 0 ]; then
  exit 1
fi
exit 0
