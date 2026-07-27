#!/usr/bin/env bash
# =============================================================================
# check.sh — the convenience quality-gate wrapper (AGENTS.md).
# Bead: fgdb-gate-coverage-checksh-omits-live-gates-u7mw
#
# Runs every mandatory core check and every unique live checker artifact
# declared by registries/checker_index.toml. Registered cargo-test artifacts
# are covered by the workspace cargo-test invocation. Registered scripts and
# registry-check binaries are discovered and invoked from their artifact paths.
#
# topology-check and unsafe-ledger-check are load-bearing for the file-coverage
# closure: the coverage table claims
# topology-check as the inspector for rust-toolchain.toml,
# registries/workspace_topology.toml, registries/unsafe_boundary_ledger.toml and
# docs/WORKSPACE_TOPOLOGY.md. A coverage claim naming a gate this script does not
# run would be false. The registry-derived runner makes such a removal UNRUN and
# red; if a checker is retired, move its files to an exemption in the same
# commit.
#
# Every expected gate receives exactly one PASS, RED, or UNRUN verdict. The
# wrapper reports every verdict before exiting, and a green summary is possible
# only when every registered live artifact was actually executed and passed.
#
# -----------------------------------------------------------------------------
# THE REPORTING CONTRACT (bead fgdb-checksh-red-not-fail-vbhd)
# -----------------------------------------------------------------------------
# THE EXIT CODE IS AUTHORITATIVE. 0 means every expected gate executed and
# passed; nonzero means it did not. No text-matching habit is required to learn
# that, and none of the tokens below may be trusted over the exit status.
#
# THE TRANSCRIPT LIVES ON STDOUT. Every per-gate verdict and every summary line
# is written to stdout; stderr carries only the diagnostics explaining WHY a
# gate failed. So `check.sh > gate.log` captures a complete, honest verdict
# transcript, and `check.sh 2>/dev/null | ...` does not hide a failure.
#
# Each verdict line is ANCHORED AT COLUMN 0 and each anchored token counts
# exactly one thing:
#
#   ^PASS   a gate that executed and passed
#   ^RED    a gate that executed and failed
#   ^UNRUN  a gate that was expected but never executed
#   ^FAIL   a gate that did not pass — the union of ^RED and ^UNRUN, emitted as
#           an alias line beside each so that a reader who greps FAIL (the token
#           the g0 e2e scripts use) and a reader who greps RED both get a true
#           answer instead of a silent zero
#
#   grep -c '^RED'  == red gates      grep -c '^FAIL' == red + unrun
#   grep -c '^UNRUN' == unrun gates   grep -c '^PASS' == passed gates
#
# The overall verdict is the line `QUALITY GATE RED` or `ALL GATES GREEN`, and
# `QUALITY GATE RED` is written to BOTH streams because it is the one line that
# must reach every reader.
#
# WHY THIS IS SPELLED OUT. MEASURED 2026-07-27 against the emission functions
# below, before the fix: RED and UNRUN went to stderr while PASS went to stdout,
# and the token was RED where every other gate script in this repo says FAIL. On
# the stdout of a run with a red core gate, a red registered gate and an unrun
# registered gate:
#     grep -c '^FAIL' -> 0     grep -c '^RED' -> 0     grep -c 'RED' -> 1
# and on the stdout of an all-green run, the SAME three greps returned 0, 0 and
# 1. The single unanchored hit was the substring in "REGISTE-RED LIVE GATES", so
# no grep of any kind distinguished a red run from a green one, and a redirected
# log was a plausible, complete-looking, all-green transcript of a red run. A
# pane read a red run that way on 2026-07-27 and landed a commit on it.
#
# When CI is added, wire this script as the CI test step rather than
# duplicating the commands.
# =============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECKER_INDEX="$ROOT/registries/checker_index.toml"
GATE_VERDICT_LIB="$ROOT/scripts/lib/gate_verdict.sh"

# The shared verdict contract (fgdb-udco). Sourced here for the emitters; the
# EXIT trap is installed in the main run path below rather than here, because
# `--self-test` runs its fixtures in subshells and a bash subshell inherits and
# fires the parent's EXIT trap — which would add a FAIL line to the very output
# verdict_stream_control counts.
# shellcheck source=lib/gate_verdict.sh
. "$GATE_VERDICT_LIB"

CORE_EXPECTED=0
CORE_EXECUTED=0
CORE_PASSED=0
CORE_RED=0
REGISTERED_EXPECTED=0
REGISTERED_EXECUTED=0
REGISTERED_PASSED=0
REGISTERED_RED=0
REGISTERED_UNRUN=0
REGISTERED_CARGO_EXPECTED=0
REGISTERED_CARGO_EXECUTED=0
REGISTERED_SCRIPT_EXPECTED=0
REGISTERED_SCRIPT_EXECUTED=0
REGISTERED_BINARY_EXPECTED=0
REGISTERED_BINARY_EXECUTED=0
REGISTERED_OTHER_EXPECTED=0
REGISTERED_SEQ=0
LAST_GATE_RC=0
GATE_LOG_DIR=""
CARGO_TEST_LOG=""
COV_TRACKED=0
COV_INSPECTED=0
COV_EXEMPT=0

run_core_gate() {
  local label="$1"
  shift
  CORE_EXPECTED=$((CORE_EXPECTED + 1))
  CORE_EXECUTED=$((CORE_EXECUTED + 1))
  echo "==> $label"
  if "$@"; then
    CORE_PASSED=$((CORE_PASSED + 1))
    LAST_GATE_RC=0
    gate_pass "core: $label"
  else
    LAST_GATE_RC=$?
    CORE_RED=$((CORE_RED + 1))
    # RED is the refinement, FAIL is the contract token. Both anchored, both on
    # stdout, emitted together. See THE REPORTING CONTRACT.
    printf 'RED core: %s (exit %s)\n' "$label" "$LAST_GATE_RC"
    gate_fail "core: $label (exit $LAST_GATE_RC)"
  fi
}

# WHY this step exists: every mandated check is silent on the file types it does
# not handle. The cargo steps are no-ops on everything that is not Rust, and `ubs`
# is worse than silent — it exits 0 while printing "nothing was checked (this is
# NOT a pass)" for .sh, .jsonl, .toml and .md alike. So a file type that no gate
# ever opens is indistinguishable, at the exit code, from one that passed
# everything. This step closes that hole structurally: every tracked file must be
# claimed by a gate below or declared exempt with a reason, and an unclaimed file
# is a hard failure rather than a silent pass.
#
# The tracked set is enumerated from `git ls-files` at run time, so a newly added
# file cannot slip through by not being listed here — it lands in the unclaimed
# bucket and fails closed.
#
# EVERY "inspected" claim below was mutation-proven, not assumed: the named gate
# was run against a tree with that file destroyed and observed to go red (and for
# files that are only partly load-bearing, mutated inside the pinned region — a
# weaker mutation lands somewhere unchecked and reports a false pass). Do not add
# a row here without doing the same. An unverified coverage claim is precisely the
# defect this step exists to prevent, one level up.
coverage_of() {
  case "$1" in
    *.rs)                                echo "cargo fmt/check/clippy/test" ;;
    *.sh|*.bash)                         echo "bash -n + shellcheck (next step)" ;;
    Cargo.toml|*/Cargo.toml)             echo "registry-check all (workspace topology)" ;;
    Cargo.lock)                          echo "cargo check --all-targets" ;;
    rust-toolchain.toml)                 echo "topology-check" ;;
    registries/threat_model.toml)        echo "threat-check" ;;
    registries/workspace_topology.toml)  echo "topology-check" ;;
    registries/unsafe_boundary_ledger.toml) echo "topology-check" ;;
    # EXACT, not the `registries/*.toml` glob below. laws.toml is validated by
    # a cargo test, not by `registry-check all`, so the glob would claim a gate
    # that never opens the file -- the fail-open this row's own comment names.
    registries/laws.toml)                echo "cargo test --workspace (tools/registry-check/tests/laws.rs: schema, plan-anchor resolution, 12 mutation fixtures)" ;;
    registries/*.toml)                   echo "registry-check all" ;;
    .beads/issues.jsonl)                 echo "architecture-check (parses every record; malformed line fails file:line)" ;;
    docs/ARCHITECTURE_DECISION_RECORD.md) echo "architecture-check (generated document)" ;;
    docs/THREAT_AND_TRUST_MODEL.md)      echo "threat-check (generated document)" ;;
    docs/WORKSPACE_TOPOLOGY.md)          echo "topology-check (generated document)" ;;
    docs/NEGATIVE_EVIDENCE.md)           echo "g0_negative_evidence_e2e (parses every entry; each doctrine id, bead and repair commit must resolve)" ;;
    # EXACT, not `formal/lean/*.lean`. The gate runs the artifacts of CHECKED
    # lanes only, so a glob would claim coverage for a future DECLARED lane's
    # artifact that nothing runs — the same fail-open as the `registries/*.toml`
    # row above. A new .lean file lands unclaimed and reds this step until
    # somebody says which lane checks it.
    formal/lean/VersionChain.lean)       echo "g0_proof_lanes_e2e (runs \`lean\` on it; lane lean-version-chain is status=checked)" ;;
    formal/lean/lean-toolchain)          echo "g0_proof_lanes_e2e (the prover pin: the gate reds if it is missing, malformed, or does not match the lean that actually ran)" ;;
    COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md)
                                         echo "registry-check (source_block pins + claims-lint)" ;;
    README.md)                           echo "registry-check lint (claim markers, + every §Performance gate row must cite one)" ;;
    AGENTS.md)                           echo "registry-check lint (claim markers only, not content)" ;;
    *)                                   echo "" ;;
  esac
}
# An exemption is a CLAIM THAT NOTHING CHECKS THIS FILE, stated out loud. Each is
# printed on every run so "green" is never silent about what was skipped.
coverage_exempt_reason() {
  case "$1" in
    COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB__FABLE.md|\
    COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB__SOL.md|\
    COMPREHENSIVE_PLAN_REVIEW_BY_KIMI_K3.md|\
    META_ANALYSIS_OF_KIMI_K3_REVIEW_BY_FABLE.md|\
    PLAN_AUDIT_BY_SOL_ULTRA.md)
      echo "historical review artifact; exclusion reason recorded in registries/claims_lint.toml" ;;
    .beads/config.yaml|.beads/metadata.json|.beads/.gitignore)
      echo "beads-tool internal state, regenerated by br; not a deliverable" ;;
    .gitignore)
      echo "VCS configuration; not a deliverable" ;;
    LICENSE)
      echo "verbatim legal text; must not be edited" ;;
    .beads/.write.lock.stale-*)
      echo "STALE BEADS LOCK, COMMITTED BY ACCIDENT — should be removed from the index" ;;
    *)
      echo "" ;;
  esac
}
run_file_coverage() {
  local covfile
  local covreason
  local -a cov_unclaimed=()

  COV_TRACKED=0
  COV_INSPECTED=0
  COV_EXEMPT=0
  while IFS= read -r covfile; do
    COV_TRACKED=$((COV_TRACKED + 1))
    if [ -n "$(coverage_of "$covfile")" ]; then
      COV_INSPECTED=$((COV_INSPECTED + 1))
    else
      covreason="$(coverage_exempt_reason "$covfile")"
      if [ -n "$covreason" ]; then
        COV_EXEMPT=$((COV_EXEMPT + 1))
        printf '    NOT INSPECTED  %-44s %s\n' "$covfile" "$covreason"
      else
        cov_unclaimed+=("$covfile")
      fi
    fi
  done < <(git ls-files)
  if [ "$COV_TRACKED" -eq 0 ]; then
    echo "ERROR: git ls-files returned nothing — this step must never be vacuously green" >&2
    return 1
  fi
  if [ "${#cov_unclaimed[@]}" -ne 0 ]; then
    echo "ERROR: no gate in this script inspects the following tracked file(s):" >&2
    for covfile in "${cov_unclaimed[@]}"; do
      echo "    $covfile" >&2
    done
    echo "  Give it an inspector in coverage_of(), or declare it in" >&2
    echo "  coverage_exempt_reason() with a reason saying why nothing checks it." >&2
    echo "  Refusing to report green over a file this gate never opened." >&2
    return 1
  fi
  echo "    coverage: $COV_TRACKED tracked = $COV_INSPECTED inspected + $COV_EXEMPT declared-not-inspected"
}

# WHY this step exists: the mandated checks are all cargo-based, and cargo has
# nothing to say about a .sh file. `ubs` is worse than silent on them — it exits
# 0 while printing "no supported languages detected ... this is NOT a pass", so
# a shell deliverable that has never been linted reports exactly like one that
# passed. (Same family: ubs treats .jsonl as unsupported, so it is a no-op on
# .beads/ too.) Every script under scripts/ was therefore exempt from the
# quality gate until this step was added.
#
# This is not hypothetical. shellcheck caught SC2033 in the determinism gate:
# `xargs sha256` where sha256 was a shell function. xargs cannot invoke a
# function, so the pipeline digested an empty stream and the "source pin"
# came back as the constant sha256("") on every tree — 64 valid hex chars that
# passed a length check and made every before/after comparison succeed. The
# concurrency guard was disabled with no error and no visible symptom.
run_shell_lint() {
  local -a shell_files=()
  local file

  mapfile -t shell_files < <(git ls-files '*.sh' '*.bash')
  if [ "${#shell_files[@]}" -eq 0 ]; then
    echo "ERROR: no shell deliverables found — this step must never be vacuously green" >&2
    return 1
  fi
  for file in "${shell_files[@]}"; do
    bash -n "$file" || {
      echo "ERROR: bash -n failed for $file" >&2
      return 1
    }
  done
  echo "    bash -n: ${#shell_files[@]} file(s) parsed"
  if ! command -v shellcheck >/dev/null 2>&1; then
    echo "ERROR: shellcheck is not installed; the shell deliverables were NOT checked." >&2
    echo "  Refusing to report green on an unrun check — install shellcheck or run" >&2
    echo "  scripts/check.sh on a host that has it." >&2
    return 1
  fi
  # Notes are advisory; errors and warnings are not.
  shellcheck --severity=warning "${shell_files[@]}" || {
    echo "ERROR: shellcheck reported an error or warning" >&2
    return 1
  }
  echo "    shellcheck: clean at severity>=warning across ${#shell_files[@]} file(s)"
  # SC2015 (`A && B || C`) is only a *note*, so the line above never enforced it,
  # and 14 sites accumulated across the two largest gate scripts before anyone
  # looked. `--include` reports one code regardless of its severity, which pins
  # this class without dragging in the six unrelated notes the corpus still
  # carries (3x SC1091, 2x SC2016, 1x SC2012) — those are a separate decision.
  #
  # MEASURED 2026-07-27 before removing them: none of the 14 failed OPEN. Every
  # one was `check && ok "..." || die "..."`, and because `ok()` ends in a
  # `printf` that returns 0, a passing check never reached `die`. The exposure
  # was the other way: give `ok()` a non-zero return and all 14 report a FAIL
  # line against an assertion that actually PASSED — a false RED whose message
  # names the wrong thing. The if/then/else form turns that same breakage into
  # an honest `set -e` abort caught by the trap. Both exit non-zero; only one
  # tells the truth about which assertion failed.
  shellcheck --include=SC2015 "${shell_files[@]}" || {
    echo "ERROR: shellcheck reported SC2015 (A && B || C is not if-then-else)." >&2
    echo "  In a gate script the third branch runs when the SECOND command fails," >&2
    echo "  so the FAIL it reports is attributed to an assertion that passed." >&2
    echo "  Write it as: if check; then ok \"...\"; else die \"...\"; fi" >&2
    return 1
  }
  echo "    shellcheck: 0 SC2015 across ${#shell_files[@]} file(s)"
}

# =============================================================================
# THE VERDICT-CONTRACT CLOSURE (bead fgdb-udco)
# =============================================================================
# WHY THIS IS A GATE AND NOT NINE PATCHES. Converting ten gates to one token
# fixes ten gates. It does not stop the eleventh: a gate added next month writes
# its own `echo "BROKEN: ..." >&2`, no gate opens it, and the swarm is back to a
# failure nobody's query can see. This step is the difference — it derives the
# gate set from registries/checker_index.toml AT RUN TIME, so a newly registered
# gate is in scope the moment it is registered, and it FAILS CLOSED: an
# unreadable registry, an empty gate list or an unreadable gate is a violation,
# never a pass.
#
# THE CONTRACT IT ENFORCES is stated once in scripts/lib/gate_verdict.sh. Four
# laws, each one directional and each one reachable:
#
#   L1  every gate sources the contract library and calls gate_init
#   L2  no contract token is written to stderr (verdicts live on ONE stream)
#   L3  no `WORD:` verdict marker at column 0 of stdout outside the vocabulary
#   L4  vocabulary tokens are used as `TOKEN ` — `PASS:` is a fifth token
#
# L3's shape is deliberate and was chosen by measurement, not taste. The obvious
# rule — "any ALL-CAPS word at column 0 of stdout must be in the vocabulary" —
# was tried first and rejects legitimate prose: three gates end with lines like
# `ADR E2E GREEN;` and `THREAT MODEL E2E GREEN`. `WORD:` is what a verdict
# MARKER looks like and what prose does not, and it catches every historical
# rogue (`ERROR:`, `FAIL:` prefixed, `PASS:`) plus anything a future gate
# invents. Diagnostics on stderr stay unconstrained on purpose — the four
# fail-fast gates keep their ~20 `echo "ERROR: ..." >&2` sites unchanged, and
# their contract line comes from the library's exit-code-derived EXIT trap.
#
# WHAT IT CANNOT SEE, stated because an unstated limit reads as coverage: this
# is a static reader of shell source. A verdict assembled across two lines, or
# emitted through a variable, is invisible to it. That is why the fixture
# controls below are behavioural and why the vocabulary is closed rather than
# open — an unknown token is a violation, so the failure mode is a false RED
# that someone fixes, not a false green nobody sees.
VERDICT_VOCABULARY='PASS|FAIL|RED|UNRUN'

# verdict_contract_gate_list <registry> — the enumerated gate set, one per line.
#
# Enumeration, not a glob over scripts/*.sh: a glob would claim coverage for a
# file no runner opens, which is the fail-open this project has closed twice
# already. The list is check.sh plus every live `kind = "script"` artifact.
verdict_contract_gate_list() {
  local registry="$1"
  {
    echo "scripts/check.sh"
    awk '
      /^\[\[checker\]\]/ { kind = ""; artifact = ""; status = "" }
      /^[[:space:]]*kind[[:space:]]*=/      { kind = $0 }
      /^[[:space:]]*artifact[[:space:]]*=/  { artifact = $0 }
      /^[[:space:]]*status[[:space:]]*=/ {
        status = $0
        if (kind ~ /"script"/ && status ~ /"live"/) {
          gsub(/^[^"]*"|"[^"]*$/, "", artifact)
          print artifact
        }
      }
    ' "$registry"
  } | LC_ALL=C sort -u
}

# verdict_contract_violations <root> <gate> — prints one line per violation.
verdict_contract_violations() {
  local root="$1" gate="$2"
  local path="$root/$gate"
  local body

  if [ ! -r "$path" ]; then
    printf '%s: unreadable — refusing to report contract conformance as checked\n' "$gate"
    return
  fi
  body="$(grep -vE '^[[:space:]]*#' "$path")"

  # L1. check.sh installs the trap in its main path rather than at load, for the
  # subshell reason stated where it does so; it must still source the library.
  case "$body" in
    *lib/gate_verdict.sh*) ;;
    *) printf '%s: does not source scripts/lib/gate_verdict.sh (L1)\n' "$gate" ;;
  esac
  if [ "$gate" != "scripts/check.sh" ]; then
    case "$body" in
      *gate_init*) ;;
      *) printf '%s: never calls gate_init, so no EXIT trap derives its verdict (L1)\n' "$gate" ;;
    esac
  fi

  # L2. A verdict on stderr is the defect that made seven of ten gates invisible
  # to a stdout capture.
  printf '%s' "$body" \
    | grep -nE "(echo|printf)[^|;&]*['\"][[:space:]]*($VERDICT_VOCABULARY)[: ][^|;&]*>&2" \
    | sed "s|^|$gate: writes a contract token to stderr (L2) at body line |"

  # L3/L4. A `WORD:` marker at column 0 of an emitted line, on stdout, outside
  # the vocabulary. `printf '    ok ...'` and `echo "==> ..."` do not match:
  # the marker must be an unindented ALL-CAPS word followed by a colon.
  printf '%s' "$body" \
    | grep -vE '>&2' \
    | grep -nE "(echo|printf)[[:space:]]+['\"][A-Z][A-Z_]+:" \
    | grep -vE "['\"]($VERDICT_VOCABULARY) " \
    | sed "s|^|$gate: emits a non-vocabulary verdict marker on stdout (L3/L4) at body line |"
}

run_verdict_contract() {
  local -a gates=()
  local gate inventory violations total=0 conformant=0

  if [ ! -r "$CHECKER_INDEX" ]; then
    echo "ERROR: cannot read $CHECKER_INDEX — refusing to report the verdict contract as checked" >&2
    return 1
  fi
  inventory="$(verdict_contract_gate_list "$CHECKER_INDEX")"
  [ -n "$inventory" ] && mapfile -t gates <<<"$inventory"
  # CONTROL. An empty list makes every verdict below quantified over nothing,
  # and "no gates exist" is indistinguishable from "the reader is broken".
  # scripts/check.sh is itself in the list, so 1 is the floor and 0 is never
  # correct. The nine registered script gates make 10 the value at fgdb-udco.
  if [ "${#gates[@]}" -lt 2 ]; then
    echo "ERROR: enumerated ${#gates[@]} gate(s) from the checker index; a list this short" >&2
    echo "  cannot be distinguished from a broken reader, so it is a violation not a pass" >&2
    return 1
  fi

  for gate in "${gates[@]}"; do
    total=$((total + 1))
    violations="$(verdict_contract_violations "$ROOT" "$gate")"
    if [ -z "$violations" ]; then
      conformant=$((conformant + 1))
    else
      echo "ERROR: verdict-contract violation(s):" >&2
      printf '%s\n' "$violations" | sed 's/^/  /' >&2
    fi
  done
  if [ "$conformant" -ne "$total" ]; then
    echo "ERROR: $((total - conformant)) of $total gates do not report under the shared" >&2
    echo "  verdict contract (scripts/lib/gate_verdict.sh). A gate whose failure token" >&2
    echo "  differs from what its readers grep for is green to every automated reader." >&2
    return 1
  fi
  echo "    verdict contract: $conformant/$total gates conformant (vocabulary ${VERDICT_VOCABULARY//|/, }; stdout only)"
}

# gate_tracked_sources <pathspec>... -> NUL-separated tracked paths on stdout
#
# THE INPUT-SET LAW, and it is general, not a UBS workaround: a gate's domain
# must be DERIVED FROM THE TRACKED SET, never discovered by walking a directory.
# A tool pointed at a directory answers a different question from the one its
# name asks, and the difference does not appear anywhere in the verdict.
#
# MEASURED 2026-07-27 on this repository (fgdb-ubs-scans-directory-not-tracked-set-hdyv).
# `ubs --only=rust --ci .` walked 1043.3 MB, of which 1019.6 MB — 97.7% — was
# untracked tool state:
#     .beads/.br_history        816.2 MB   78.2% of the scan
#     .beads/.br_recovery       121.1 MB   11.6%
#     libregistry_check.rlib     66.1 MB    6.3%  (untracked, gitignored, stray)
#     .beads/beads.db            16.3 MB    1.6%
# The tracked Rust this gate is NAMED for is 6.6 MB across 119 files, so stated
# domain and actual domain differed by 159x. UBS hit its 1000 MB safety limit
# and REFUSED TO START — so a gate called "UBS over every tracked Rust source"
# reported a colour while inspecting ZERO Rust.
#
# AND THAT COLOUR TRACKED THE SIZE OF A LOG FILE. `.beads/.br_history` alone is
# 78% of the scan, so rotating it would flip this gate green having changed
# nothing whatsoever about the code. A gate whose verdict is a function of an
# irrelevant magnitude is not a gate. Raising UBS_MAX_DIR_SIZE_MB was therefore
# the WRONG fix: it would make the gate scan a gigabyte of tool state
# successfully, which is worse than failing, because it looks like success.
#
# FAILS CLOSED, WITH NO FALLBACK. If the tracked set cannot be derived — not a
# work tree, git unavailable, enumeration fails, or the result is empty — this
# returns nonzero and the caller MUST fail its gate. It never degrades to
# scanning whatever is on disk. Proceeding on a different, smaller input set
# while reporting the same verdict IS the defect, one level up: the same shape
# as a test that returns PASS when the corpus it reads is simply absent.
gate_tracked_sources() {
  git rev-parse --is-inside-work-tree >/dev/null 2>&1 || return 1
  local -a found=()
  # `git ls-files -z` is the only enumeration allowed here. Note the exit status
  # is taken from git via the process substitution's own guard below, because
  # `readarray < <(cmd)` reports READARRAY's status, not cmd's — that is exactly
  # the "third branch attributed to the wrong command" trap this file already
  # warns about for SC2015.
  local tmp="${TMPDIR:-/tmp}/gate-tracked-$$"
  git ls-files -z -- "$@" > "$tmp" || { rm -f "$tmp"; return 1; }
  readarray -d '' -t found < "$tmp"
  rm -f "$tmp"
  [ "${#found[@]}" -gt 0 ] || return 1
  printf '%s\0' "${found[@]}"
}

run_ubs() {
  local log="$GATE_LOG_DIR/core-ubs.log"
  local list="$GATE_LOG_DIR/core-ubs.sources"
  local ubs_rc
  local -a rust_sources=()

  # The domain is the TRACKED Rust set — precisely what this gate's name claims
  # — and not the directory the gate happens to run in. See gate_tracked_sources
  # for the measurement that forced this and for why there is no fallback.
  if ! gate_tracked_sources '*.rs' > "$list"; then
    echo "ERROR: the tracked Rust source set could not be derived from git." >&2
    echo "  This gate scans the TRACKED SET, never a directory, and it does not" >&2
    echo "  fall back. Scanning whatever is present would answer a different" >&2
    echo "  question under this gate's name — which is how it came to inspect" >&2
    echo "  zero Rust while reporting a colour (hdyv)." >&2
    return 1
  fi
  readarray -d '' -t rust_sources < "$list"
  if [ "${#rust_sources[@]}" -eq 0 ]; then
    echo "ERROR: the tracked Rust source set is empty; refusing to report a verdict." >&2
    return 1
  fi
  echo "    domain: ${#rust_sources[@]} tracked Rust source(s), from git ls-files"

  ubs --only=rust --ci "${rust_sources[@]}" 2>&1 | tee "$log"
  ubs_rc=${PIPESTATUS[0]}
  if grep -Eiq \
    'nothing was checked|did not run any scanner|no supported languages detected' \
    "$log"; then
    echo "ERROR: UBS exited zero without running a Rust scanner" >&2
    return 1
  fi
  if grep -Eiq 'Directory too large|exceeds limit of' "$log"; then
    echo "ERROR: UBS refused to start on a size guard. The domain is the tracked" >&2
    echo "  set, so this must not happen; do NOT raise UBS_MAX_DIR_SIZE_MB." >&2
    return 1
  fi
  # The verdict is the RATCHET, not ubs's raw exit status. ubs exits 1 whenever
  # any critical exists, so on a real backlog it is permanently red and says
  # nothing about whether the code got better or worse. `ubs_rc` is therefore
  # recorded, not returned.
  echo "    ubs exit ${ubs_rc} (raw); verdict is the critical ratchet below"
  ubs_critical_ratchet "$log"
}

# UBS_CRITICAL_BASELINE — the ratchet. "<check name>=<count>", one per class.
#
# WHY A RATCHET AND NOT A BURN-DOWN. Correcting this gate's domain (hdyv) made
# it inspect Rust for the first time and it reported 1049 criticals — a backlog
# no gate had ever looked at, not a regression any commit introduced. 1049 open
# tickets is not a plan; an un-inspectable backlog that nobody can widen
# silently is. Same shape as identity_code_set_is_ratcheted /
# UNREGISTERED_BASELINE in tests/satisfiability.rs.
#
# THE PARTITION, measured at 0411aa0 over 119 tracked Rust sources. The
# denominator is 1049 and these three classes are all of it:
#
#   794  75.7%  Secret/token comparisons without timing-safe equality
#   135  12.9%  panic!/unreachable!/todo!/unimplemented!
#   120  11.4%  JWT decode, validation bypass, or missing claim binding
#
# MOVED 794 -> 796 by fgdb-n061, and this is what the ratchet is for: the two
# new findings are named here rather than absorbed. Retiring the 450 mirrored
# rationale literals replaced one prose comparison with two digest comparisons
# in tools/registry-check/src/appendix_a.rs:
#   sha256_hex(row.rationale.as_bytes()) == pin.rationale_sha256   (the reader)
#   sha256_hex(row.rationale.as_bytes()) != pin.rationale_sha256   (the guard)
# UBS matches any `==`/`!=` over a value that looks like a digest, so both land
# in this class.
#
# ATTRIBUTED BY MEASUREMENT, not by arithmetic on the total: scanning
# appendix_a.rs ALONE reported 275 criticals at HEAD and 277 with the change --
# +2, in this class, from this file. Nothing else in the tree moved.
#
# THEY ARE NOT TIMING DEFECTS. registry-check is a build-time developer tool
# comparing content digests of PUBLIC repository files; there is no secret, no
# remote caller and no timing channel, and an adversary who can time it already
# has the repository. A constant-time helper would also need `subtle` or `ring`,
# which Doctrine #1 forbids. Recorded as a real, explained increase rather than
# suppressed -- the ratchet's job is to make growth deliberate, not zero.
#
# ALWAYS-FIX PER DOCTRINE — memory safety, UB, data races — IS ZERO OF 1049.
# Stating that plainly because it is the load-bearing result: nothing here is in
# the category AGENTS.md says to fix on sight. The workspace is
# `unsafe_code = "forbid"`, so that is the expected reading, not a lucky one.
#
# 914 OF 1049 (87.1%) ARE TWO RULES WHOSE DOMAIN ASSUMPTIONS DO NOT HOLD HERE:
#
#   * The 794 "secret comparison" hits fire on `==` next to identifiers named
#     `code`/`key`. Actual matched evidence:
#         codes.iter().any(|code| code == expected)
#         .filter(|schema| schema.key.source_key() == "top|RegisteredStrongRef")
#     These compare VIOLATION CODES and SCHEMA KEYS. Neither is a secret.
#   * The 120 "JWT" hits fire on the token `decode`. Actual matched evidence:
#         CanonicalScalar::decode(&pinned_text_encoding).unwrap_err()
#     MEASURED: `jsonwebtoken`, `DecodingKey` and `jwt` appear in ZERO tracked
#     files. Doctrine #1 closes the dependency universe, so no JWT library can
#     ever exist here. All 120 are false by construction.
#
# They are still PINNED rather than excluded. A waiver is forever and a red is
# temporary: pinning keeps the count visible and makes any change fail, whereas
# suppressing the rules would hide the day one of them matches something real.
#
# EQUALITY, NOT A CEILING. Drift in EITHER direction fails, exactly as
# UNREGISTERED_BASELINE does. A ceiling lets the baseline go stale, and a count
# that only ever rises silently absorbs improvements.
#
# KNOWN LIMIT, STATED RATHER THAN PAPERED OVER: this pins COUNTS, not the
# finding set. An aggregate pin does not pin the split, so 794 becoming 793 real
# plus 1 new one would pass. Per-finding identity is not obtainable from the
# tool: its `--format=jsonl` emits summary rows only, its SARIF comes from the
# `ast-grep` driver alone (122 results, panic-macro only — it does not see the
# other two classes), and the text listing prints roughly 3 findings per class
# with NO "and N more" marker, so it truncates silently. Tightening this to an
# exact set needs a per-finding output UBS does not currently emit.
UBS_CRITICAL_BASELINE=(
  "Secret/token comparisons without timing-safe equality=796"
  "panic!/unreachable!/todo!/unimplemented!=135"
  "JWT decode, validation bypass, or missing claim binding=120"
)

# ubs_critical_ratchet <log> -> 0 when the critical partition equals the baseline
#
# Fails closed on an UNKNOWN class: a critical class this table has never seen is
# by definition un-adjudicated, and defaulting it to "fine" is how a backlog
# becomes invisible in the first place.
ubs_critical_ratchet() {
  local log="$1"
  local -A observed=() expected=()
  local entry name count check line drift=0 total=0
  for entry in "${UBS_CRITICAL_BASELINE[@]}"; do
    expected["${entry%=*}"]="${entry##*=}"
  done
  # Sections read "• <check name>" followed by "🔥 CRITICAL (<n> found)".
  check=""
  while IFS= read -r line; do
    case "$line" in
      "• "*) check="${line#• }" ;;
      *"CRITICAL ("*)
        count="${line#*CRITICAL (}"
        count="${count%% found)*}"
        [ -n "$check" ] && observed["$check"]=$((${observed["$check"]:-0} + count))
        ;;
    esac
  done < "$log"

  for name in "${!observed[@]}"; do
    total=$((total + observed["$name"]))
    if [ -z "${expected[$name]+set}" ]; then
      echo "ERROR: unadjudicated UBS critical class \"$name\" (${observed[$name]} found)." >&2
      echo "  A class this baseline has never seen must be adjudicated and pinned," >&2
      echo "  not defaulted to acceptable." >&2
      drift=1
    elif [ "${observed[$name]}" -ne "${expected[$name]}" ]; then
      echo "ERROR: UBS critical ratchet drift in \"$name\":" >&2
      echo "  baseline ${expected[$name]}, observed ${observed[$name]}." >&2
      echo "  If this is a real change, update UBS_CRITICAL_BASELINE in the SAME" >&2
      echo "  commit and say in the message which findings moved and why." >&2
      drift=1
    fi
  done
  for name in "${!expected[@]}"; do
    if [ -z "${observed[$name]+set}" ]; then
      echo "ERROR: baselined UBS critical class \"$name\" no longer reported." >&2
      echo "  Either it was fixed — remove its row — or the rule stopped running," >&2
      echo "  which would make this gate quietly weaker." >&2
      drift=1
    fi
  done
  [ "$drift" -ne 0 ] && return 1
  echo "    critical ratchet: $total across ${#observed[@]} class(es), all at baseline"
  return 0
}

# Emit one row per unique live (kind, artifact) pair. Multiple symbols may
# deliberately name the same executable artifact; the artifact is one gate and
# is executed once. Checker rows use the registry's documented one-line string
# fields. A live row missing its kind or artifact remains visible as UNRUN.
live_gate_inventory() {
  local registry="$1"

  awk '
    function string_value(line, value) {
      value = line
      sub(/^[[:space:]]*[^=]*=[[:space:]]*"/, "", value)
      sub(/".*$/, "", value)
      return value
    }
    function emit() {
      if (in_checker && status == "live") {
        printf "%s\t%s\n", kind, artifact
      }
    }
    /^[[:space:]]*\[\[checker\]\][[:space:]]*(#.*)?$/ {
      emit()
      in_checker = 1
      kind = ""
      artifact = ""
      status = ""
      next
    }
    in_checker && /^[[:space:]]*kind[[:space:]]*=/ {
      kind = string_value($0)
      next
    }
    in_checker && /^[[:space:]]*artifact[[:space:]]*=/ {
      artifact = string_value($0)
      next
    }
    in_checker && /^[[:space:]]*status[[:space:]]*=/ {
      status = string_value($0)
      next
    }
    END { emit() }
  ' "$registry" | LC_ALL=C sort -u
}

safe_artifact() {
  local artifact="$1"

  case "$artifact" in
    "" | /* | .. | ../* | */.. | */../* | *$'\t'* | *$'\r'* | *$'\n'*)
      return 1
      ;;
  esac
  return 0
}

increment_registered_kind_expected() {
  case "$1" in
    cargo-test) REGISTERED_CARGO_EXPECTED=$((REGISTERED_CARGO_EXPECTED + 1)) ;;
    script) REGISTERED_SCRIPT_EXPECTED=$((REGISTERED_SCRIPT_EXPECTED + 1)) ;;
    binary) REGISTERED_BINARY_EXPECTED=$((REGISTERED_BINARY_EXPECTED + 1)) ;;
    *) REGISTERED_OTHER_EXPECTED=$((REGISTERED_OTHER_EXPECTED + 1)) ;;
  esac
}

increment_registered_kind_executed() {
  case "$1" in
    cargo-test) REGISTERED_CARGO_EXECUTED=$((REGISTERED_CARGO_EXECUTED + 1)) ;;
    script) REGISTERED_SCRIPT_EXECUTED=$((REGISTERED_SCRIPT_EXECUTED + 1)) ;;
    binary) REGISTERED_BINARY_EXECUTED=$((REGISTERED_BINARY_EXECUTED + 1)) ;;
  esac
}

record_registered_result() {
  local kind="$1"
  local artifact="$2"
  local outcome="$3"
  local detail="$4"

  REGISTERED_EXPECTED=$((REGISTERED_EXPECTED + 1))
  increment_registered_kind_expected "$kind"
  case "$outcome" in
    pass)
      REGISTERED_EXECUTED=$((REGISTERED_EXECUTED + 1))
      REGISTERED_PASSED=$((REGISTERED_PASSED + 1))
      increment_registered_kind_executed "$kind"
      gate_pass "registered $kind $artifact — $detail"
      ;;
    red)
      REGISTERED_EXECUTED=$((REGISTERED_EXECUTED + 1))
      REGISTERED_RED=$((REGISTERED_RED + 1))
      increment_registered_kind_executed "$kind"
      # RED/UNRUN are the refinements, FAIL is the contract token. Both anchored,
      # both on stdout, emitted together. See THE REPORTING CONTRACT.
      printf 'RED registered %s %s — %s\n' "$kind" "$artifact" "$detail"
      gate_fail "registered $kind $artifact — $detail"
      ;;
    unrun)
      REGISTERED_UNRUN=$((REGISTERED_UNRUN + 1))
      gate_unrun "registered $kind $artifact — $detail"
      ;;
    *)
      echo "internal error: unknown registered outcome $outcome" >&2
      return 2
      ;;
  esac
}

run_registered_command() {
  local kind="$1"
  local artifact="$2"
  shift 2
  local log
  local gate_rc

  REGISTERED_SEQ=$((REGISTERED_SEQ + 1))
  log="$GATE_LOG_DIR/registered-$REGISTERED_SEQ.log"
  echo "==> registered $kind: $artifact"
  if "$@" >"$log" 2>&1; then
    record_registered_result "$kind" "$artifact" pass "exit 0; log $log"
  else
    gate_rc=$?
    record_registered_result "$kind" "$artifact" red \
      "exit $gate_rc; log $log"
  fi
}

# Run the workspace test gate and keep its output, because the registered
# cargo-test artifacts below are attributed FROM it.
#
# --no-fail-fast, MEASURED 2026-07-26 on `-p registry-check` with two planted
# failures in two independent test targets:
#   without it   exit 101, 12 binaries run, 12 suites reported, 1 of 2 named
#   with it      exit 101, 16 binaries run, 17 suites reported, 2 of 2 named
# It only governs whether cargo keeps RUNNING binaries after one fails; it
# cannot recover a target that failed to COMPILE. That was measured too, and it
# is why the attribution below exists rather than a second flag: with a parse
# error planted in one test target, `cargo test` ran 0 of 16 binaries, and
# --no-fail-fast, --keep-going and both together all still ran 0 -- including
# the tests of a second, dependency-free package named on the same command line.
# No cargo flag recovers that case, so the honest verdict for an artifact whose
# binary never ran is UNRUN, not RED.
run_cargo_test_workspace() {
  cargo test --workspace --no-fail-fast 2>&1 | tee "$CARGO_TEST_LOG"
  return "${PIPESTATUS[0]}"
}

# Decide one registered cargo-test artifact's outcome from the workspace run,
# printing "<outcome>\t<detail>".
#
# WHY THIS REPLACED ONE SHARED EXIT CODE. Every cargo-test artifact used to take
# the workspace exit code verbatim: rc 0 -> all pass, rc != 0 -> all red. Two
# real runs on 2026-07-26 show what that reports.
#   4710fd6: a compile error in tools/registry-check/tests/claims.rs. 0 of 67
#            suites ran. Six artifacts were reported RED on zero test evidence.
#   9c0d3c1: a test binary vanished from the shared target dir mid-run
#            ("could not execute process ... (never executed)"). 31 suites ran
#            and ALL 31 PASSED; 36 never ran. Six artifacts were reported RED --
#            five whose binaries never executed, and one,
#            crates/fgdb-codec/tests/generated_durable_roundtrip.rs, which had
#            just reported "ok. 12 passed; 0 failed" in that very run.
# So the old rule could call a passing artifact red and an unmeasured artifact
# red, and it could never call anything unrun. UNRUN still reds the overall gate
# (print_registered_summary treats REGISTERED_UNRUN != 0 as red), so this is
# strictly more truthful without being more permissive.
cargo_test_artifact_outcome() { # log artifact cargo_test_rc
  local log="$1" artifact="$2" rc="$3"
  local target runs

  if [ ! -f "$log" ]; then
    printf 'unrun\tworkspace cargo-test log is absent; cargo test --workspace exited %s\n' "$rc"
    return 0
  fi
  # Only `<pkg>/tests/<name>.rs` integration targets are attributable from the
  # run's own output. Anything else falls back to the shared exit code and SAYS
  # that it did, so an unattributable artifact can never look measured.
  case "$artifact" in
    */tests/*.rs) target="tests/${artifact##*/tests/}" ;;
    *)
      if [ "$rc" -eq 0 ]; then
        printf 'pass\tcovered by cargo test --workspace (not individually attributable)\n'
      else
        printf 'unrun\tnot individually attributable; cargo test --workspace exited %s\n' "$rc"
      fi
      return 0
      ;;
  esac

  runs=$(grep -cE "^ *Running $target \(" "$log")
  if [ "$runs" -eq 0 ]; then
    printf 'unrun\tits test binary never ran; cargo test --workspace exited %s\n' "$rc"
    return 0
  fi
  if [ "$runs" -ne 1 ]; then
    # Two packages carrying the same test-file name would make the next
    # "test result:" line ambiguous. Refuse to guess.
    printf 'unrun\t%s matches %s test targets in this workspace; not attributable\n' \
      "$target" "$runs"
    return 0
  fi
  awk -v needle="Running $target (" '
    index($0, needle) { seen = 1; next }
    seen && /^ *Running .* \(/ { exit }
    seen && /^test result:/ {
      if ($0 ~ /^test result: ok\./) { print "pass\t" $0 } else { print "red\t" $0 }
      done = 1
      exit
    }
    END {
      if (!done) {
        print "unrun\tits binary was launched but reported no test result"
      }
    }
  ' "$log"
}

run_registered_gates() {
  local root="$1"
  local registry="$2"
  local cargo_test_rc="$3"
  local cargo_test_log="${4:-$CARGO_TEST_LOG}"
  local inventory
  local row
  local kind
  local artifact
  local binary_name
  local cargo_outcome
  local cargo_detail
  local -a gates=()

  if [ ! -f "$registry" ]; then
    record_registered_result registry "$registry" unrun \
      "checker registry is absent"
    return
  fi
  if ! inventory="$(live_gate_inventory "$registry")"; then
    record_registered_result registry "$registry" unrun \
      "checker registry could not be parsed"
    return
  fi
  if [ -n "$inventory" ]; then
    mapfile -t gates <<<"$inventory"
  fi
  if [ "${#gates[@]}" -eq 0 ]; then
    record_registered_result registry "$registry" unrun \
      "no live gate artifacts were discovered"
    return
  fi

  for row in "${gates[@]}"; do
    kind=""
    artifact=""
    IFS=$'\t' read -r kind artifact <<<"$row"
    if ! safe_artifact "$artifact"; then
      record_registered_result "${kind:-missing-kind}" \
        "${artifact:-missing-artifact}" unrun \
        "artifact path is missing or unsafe"
      continue
    fi
    if [ ! -f "$root/$artifact" ]; then
      record_registered_result "$kind" "$artifact" unrun \
        "artifact does not exist"
      continue
    fi
    case "$kind" in
      cargo-test)
        cargo_outcome=""
        cargo_detail=""
        IFS=$'\t' read -r cargo_outcome cargo_detail < <(
          cargo_test_artifact_outcome \
            "$cargo_test_log" "$artifact" "$cargo_test_rc"
        )
        record_registered_result "$kind" "$artifact" \
          "${cargo_outcome:-unrun}" \
          "${cargo_detail:-attribution produced no verdict}"
        ;;
      script)
        run_registered_command "$kind" "$artifact" bash "$root/$artifact"
        ;;
      binary)
        case "$artifact" in
          tools/registry-check/src/main.rs)
            run_registered_command "$kind" "$artifact" \
              cargo run -p registry-check --quiet -- all --root "$root"
            ;;
          tools/registry-check/src/appendix_a.rs)
            run_registered_command "$kind" "$artifact" \
              cargo run -p registry-check --quiet -- appendix --root "$root"
            ;;
          tools/registry-check/src/bin/*.rs)
            binary_name="${artifact##*/}"
            binary_name="${binary_name%.rs}"
            run_registered_command "$kind" "$artifact" \
              cargo run -p registry-check --quiet --bin "$binary_name" -- \
              --root "$root"
            ;;
          *)
            record_registered_result "$kind" "$artifact" unrun \
              "no command mapping exists for this live binary artifact"
            ;;
        esac
        ;;
      *)
        record_registered_result "$kind" "$artifact" unrun \
          "live checker kind has no runner"
        ;;
    esac
  done
}

print_registered_summary() {
  echo
  echo "REGISTERED LIVE GATES: $REGISTERED_EXECUTED of $REGISTERED_EXPECTED executed; $REGISTERED_PASSED passed; $REGISTERED_RED red; $REGISTERED_UNRUN unrun"
  echo "  cargo-test artifacts: $REGISTERED_CARGO_EXECUTED/$REGISTERED_CARGO_EXPECTED executed"
  echo "  script artifacts:     $REGISTERED_SCRIPT_EXECUTED/$REGISTERED_SCRIPT_EXPECTED executed"
  echo "  binary artifacts:     $REGISTERED_BINARY_EXECUTED/$REGISTERED_BINARY_EXPECTED executed"
  if [ "$REGISTERED_OTHER_EXPECTED" -ne 0 ]; then
    echo "  unknown-kind artifacts: 0/$REGISTERED_OTHER_EXPECTED executed"
  fi
  if [ "$REGISTERED_RED" -ne 0 ] || [ "$REGISTERED_UNRUN" -ne 0 ] \
    || [ "$REGISTERED_EXECUTED" -ne "$REGISTERED_EXPECTED" ]; then
    echo "GATES RED: a registered live gate failed or was not executed"
    return 1
  fi
  return 0
}

reset_registered_counters() {
  REGISTERED_EXPECTED=0
  REGISTERED_EXECUTED=0
  REGISTERED_PASSED=0
  REGISTERED_RED=0
  REGISTERED_UNRUN=0
  REGISTERED_CARGO_EXPECTED=0
  REGISTERED_CARGO_EXECUTED=0
  REGISTERED_SCRIPT_EXPECTED=0
  REGISTERED_SCRIPT_EXECUTED=0
  REGISTERED_BINARY_EXPECTED=0
  REGISTERED_BINARY_EXECUTED=0
  REGISTERED_OTHER_EXPECTED=0
  REGISTERED_SEQ=0
}

# THE CONTROL FOR THE REPORTING CONTRACT (bead fgdb-checksh-red-not-fail-vbhd).
#
# WHY IT CAPTURES THE STREAMS SEPARATELY, AND WHY THAT IS THE WHOLE POINT. Every
# other assertion in this self-test runs its fixture under `>"$log" 2>&1`, which
# MERGES the streams — so all of them passed, for months, over emission code that
# sent PASS to stdout and RED to stderr. A merged log cannot see the defect: it
# is a control outside the law's domain. This one redirects stdout and stderr to
# different files and asserts only against stdout, which is the stream a reader
# who types `check.sh > gate.log` actually keeps.
#
# It asserts the property in BOTH directions. Asserting only that a red run's
# stdout carries the tokens would pass against emission code that printed them
# unconditionally; the green half pins that they are absent when nothing failed.
# Together they say the tokens DISCRIMINATE, which is the thing a reader relies
# on and the thing that was false.
#
# MUTATION-PROVEN 2026-07-27, on a scratchpad copy, each mutation applied alone:
#   restore `>&2` on the core RED line      -> "red-run stdout has 0 ^RED lines"
#   restore `>&2` on the registered lines   -> "red-run stdout has 0 ^UNRUN lines"
#   drop the FAIL alias lines               -> "red-run stdout has 0 ^FAIL lines"
#   emit the tokens unconditionally         -> "green-run stdout has 1 ^FAIL lines"
# Four mutations, four distinct failures, so no assertion here is decoration.
verdict_stream_control() {
  local work="$1"
  local red_out="$work/verdict-red.out"
  local red_err="$work/verdict-red.err"
  local green_out="$work/verdict-green.out"

  # A red run: one core gate red, one registered gate red, one registered gate
  # unrun. Subshells, so the fixture cannot move this run's real counters.
  (
    reset_registered_counters
    run_core_gate "control: a core gate that fails" false
    record_registered_result script scripts/control-red.sh red "exit 23"
    record_registered_result binary tools/control-unrun.rs unrun "no runner"
    print_registered_summary
  ) >"$red_out" 2>"$red_err"

  # The same shape, all green.
  (
    reset_registered_counters
    run_core_gate "control: a core gate that passes" true
    record_registered_result script scripts/control-green.sh pass "exit 0"
    print_registered_summary
  ) >"$green_out" 2>/dev/null

  verdict_token_case() { # file expected-count pattern label
    local got
    got="$(grep -cE "$3" "$1")"
    if [ "$got" -ne "$2" ]; then
      echo "SELF-TEST RED: $4 stdout has $got $3 lines, expected $2" >&2
      return 1
    fi
    return 0
  }

  # A red run's stdout must carry every verdict, under both tokens.
  #   ^RED   1 red core gate + 1 red registered gate
  #   ^UNRUN 1 unrun registered gate
  #   ^FAIL  the union: 2 red + 1 unrun
  verdict_token_case "$red_out" 2 '^RED ' "red-run" || return 1
  verdict_token_case "$red_out" 1 '^UNRUN ' "red-run" || return 1
  verdict_token_case "$red_out" 3 '^FAIL ' "red-run" || return 1
  verdict_token_case "$red_out" 0 '^PASS ' "red-run" || return 1

  # A green run's stdout must carry none of them.
  verdict_token_case "$green_out" 0 '^RED ' "green-run" || return 1
  verdict_token_case "$green_out" 0 '^UNRUN ' "green-run" || return 1
  verdict_token_case "$green_out" 0 '^FAIL ' "green-run" || return 1
  verdict_token_case "$green_out" 2 '^PASS ' "green-run" || return 1

  # The failure must not live on stderr alone, which is where it used to live.
  # This pins the contract against a future re-mirroring: the transcript is
  # stdout, stderr is diagnostics.
  if grep -qE '^(RED|FAIL|UNRUN|PASS) ' "$red_err"; then
    echo "SELF-TEST RED: a per-gate verdict line was written to stderr" >&2
    return 1
  fi
  return 0
}

# THE CONTROLS FOR THE VERDICT CONTRACT (bead fgdb-udco).
#
# Two halves, and both are needed. The LAW half plants one rogue gate per law
# and asserts the guard names it — a guard that never fires is a decoration.
# The CONFORMANT half plants a gate that obeys the contract and asserts the
# guard stays silent — without it, a guard hard-wired to `return 1` would pass
# every law test. The BEHAVIOURAL half then runs a conformant gate that actually
# fails and asserts the contract query catches it on stdout alone, which is the
# property every other assertion here is a proxy for.
verdict_contract_control() {
  local skip_rc unrun_rc
  local work="$1"
  local root="$work/contract-root"
  local got

  mkdir -p "$root/scripts/lib" || return 1
  cp "$GATE_VERDICT_LIB" "$root/scripts/lib/gate_verdict.sh" || return 1

  contract_case() { # name, body, expected-substring-or-empty, label
    local name="$1" body="$2" want="$3" label="$4" out
    printf '%s\n' "$body" >"$root/scripts/$name"
    out="$(verdict_contract_violations "$root" "scripts/$name")"
    if [ -z "$want" ]; then
      if [ -n "$out" ]; then
        echo "SELF-TEST RED: $label was flagged but conforms: $out" >&2
        return 1
      fi
    else
      case "$out" in
        *"$want"*) ;;
        *)
          echo "SELF-TEST RED: $label was not flagged ($want); got: ${out:-<nothing>}" >&2
          return 1
          ;;
      esac
    fi
    return 0
  }

  # The conformant control. If this is ever flagged, every law below is vacuous.
  contract_case conformant.sh \
'#!/usr/bin/env bash
ROOT=.
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_init "conformant"
gate_pass "an assertion that held"
gate_fail "an assertion that did not"
echo "ERROR: why it did not" >&2
gate_verdict' \
    "" "a gate that obeys the contract" || return 1

  # L1 — a gate that reports on its own, with no shared emitter behind it.
  contract_case rogue_unsourced.sh \
'#!/usr/bin/env bash
echo "PASS something"
echo "FAIL something else"' \
    "does not source" "a gate that never sources the contract library" || return 1

  # L1, second half. Sourcing the library is not conforming to it: without
  # gate_init there is no EXIT trap, so a `set -e` abort produces no FAIL line
  # at all. FOUND BY THE MUTATION MATRIX — silencing this check left the
  # self-test green, because every other fixture here either does both or
  # neither, so the assertion was quantified over nothing.
  contract_case rogue_no_init.sh \
'#!/usr/bin/env bash
ROOT=.
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_pass "sourced the library but installed no trap"' \
    "never calls gate_init" "a gate that sources the library but skips gate_init" || return 1

  # The next three fixtures ASSEMBLE their rogue line through a `%s` rather
  # than writing it literally, and that is not cosmetic. check.sh is itself in
  # the gate list, so a literal `echo "BROKEN: ..."` here is indistinguishable —
  # to a static reader of shell source — from check.sh actually emitting one.
  # MEASURED: written literally, all three tripped the guard against check.sh
  # itself and the gate went red 1-of-10 on a conformant tree. A guard whose
  # subject contains its own fixtures is reporting on the wrong thing; building
  # the marker removes the ambiguity instead of special-casing the file. A real
  # rogue is still a literal and is still caught.
  rogue_line() { # <marker-and-rest> -> a fixture body ending in that echo
    printf '#!/usr/bin/env bash\nROOT=.\n. "$ROOT/scripts/lib/gate_verdict.sh"\ngate_init "rogue"\necho "%s"%s\n' \
      "$1" "${2:-}"
  }

  # L2 — the right token on the wrong stream. This is the seven-of-ten defect.
  contract_case rogue_stderr.sh "$(rogue_line 'FAIL it broke' ' >&2')" \
    "writes a contract token to stderr" "a gate emitting FAIL to stderr" || return 1

  # L3 — the eleventh token. This is the case the whole guard exists for.
  contract_case rogue_token.sh "$(rogue_line 'BROKEN: it broke')" \
    "non-vocabulary verdict marker" "a gate inventing an eleventh token" || return 1

  # L4 — the near-miss. `PASS:` is not `PASS `, and g0_topology_e2e.sh really
  # ended this way before fgdb-udco.
  contract_case rogue_colon.sh "$(rogue_line 'PASS: g0_something')" \
    "non-vocabulary verdict marker" "a gate using PASS: instead of PASS " || return 1

  # An unreadable gate must be a violation, never a pass — fail closed.
  got="$(verdict_contract_violations "$root" "scripts/does_not_exist.sh")"
  case "$got" in
    *unreadable*) ;;
    *)
      echo "SELF-TEST RED: an unreadable gate did not fail closed; got: ${got:-<nothing>}" >&2
      return 1
      ;;
  esac

  # THE BEHAVIOURAL HALF. Run the conformant fixture for real, capture the two
  # streams separately, and assert the contract query answers correctly from
  # stdout alone — the property the static laws above are a proxy for.
  ( cd "$root" && bash scripts/conformant.sh ) \
    >"$work/contract-run.out" 2>"$work/contract-run.err"
  if [ "$(grep -cE '^FAIL ' "$work/contract-run.out")" -ne 1 ]; then
    echo "SELF-TEST RED: the contract query found no FAIL on a failing conformant gate" >&2
    return 1
  fi
  if [ "$(grep -cE '^PASS ' "$work/contract-run.out")" -ne 1 ]; then
    echo "SELF-TEST RED: a conformant gate's PASS line is missing from stdout" >&2
    return 1
  fi
  if grep -qE '^(PASS|FAIL|RED|UNRUN) ' "$work/contract-run.err"; then
    echo "SELF-TEST RED: a conformant gate wrote a verdict line to stderr" >&2
    return 1
  fi

  # And the exit-code-derived line: a gate that dies before any assertion still
  # reports FAIL. This is the path no per-site conversion could have covered.
  printf '%s\n' \
'#!/usr/bin/env bash
set -euo pipefail
ROOT=.
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_init "aborts"
false
gate_pass "never reached"' >"$root/scripts/aborts.sh"
  ( cd "$root" && bash scripts/aborts.sh ) >"$work/contract-abort.out" 2>/dev/null
  if [ "$(grep -cE '^FAIL ' "$work/contract-abort.out")" -ne 1 ]; then
    echo "SELF-TEST RED: a set -e abort produced no FAIL line on stdout" >&2
    return 1
  fi

  # THE THIRD STATE. `ran and passed`, `ran and failed`, `did not run` — and
  # only the first is green. This fixture is the identity.rs shape at gate
  # scope: a guard clause skips the whole body and the script falls off the end
  # with status 0, so silence reads as success. It must report UNRUN, must
  # carry the FAIL token so the single query sees it, must NOT emit PASS, and
  # must exit nonzero.
  printf '%s\n' \
'#!/usr/bin/env bash
set -euo pipefail
ROOT=.
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_init "skips_everything"
if [ ! -e "/nonexistent-corpus" ]; then
  exit 0
fi
gate_pass "never reached"' >"$root/scripts/skips.sh"
  ( cd "$root" && bash scripts/skips.sh ) \
    >"$work/contract-skip.out" 2>/dev/null
  skip_rc=$?
  if [ "$skip_rc" -eq 0 ]; then
    echo "SELF-TEST RED: a gate that executed no assertions exited 0" >&2
    return 1
  fi
  if [ "$(grep -cE '^UNRUN ' "$work/contract-skip.out")" -ne 1 ]; then
    echo "SELF-TEST RED: a gate that executed no assertions emitted no UNRUN line" >&2
    return 1
  fi
  if [ "$(grep -cE '^FAIL ' "$work/contract-skip.out")" -ne 1 ]; then
    echo "SELF-TEST RED: an UNRUN gate is invisible to the '^FAIL ' query" >&2
    return 1
  fi
  if grep -qE '^PASS ' "$work/contract-skip.out"; then
    echo "SELF-TEST RED: a gate that executed no assertions emitted the passing token" >&2
    return 1
  fi

  # And gate_unrun itself: the refinement and the contract token together, and
  # a verdict that is not green over it.
  printf '%s\n' \
'#!/usr/bin/env bash
ROOT=.
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_init "one_unrun"
gate_pass "a check that ran"
gate_unrun "a check that could not run: its corpus is absent"
gate_verdict' >"$root/scripts/one_unrun.sh"
  ( cd "$root" && bash scripts/one_unrun.sh ) \
    >"$work/contract-unrun.out" 2>/dev/null
  unrun_rc=$?
  if [ "$unrun_rc" -eq 0 ]; then
    echo "SELF-TEST RED: a gate with one UNRUN check reported a green verdict" >&2
    return 1
  fi
  if [ "$(grep -cE '^UNRUN ' "$work/contract-unrun.out")" -ne 1 ] \
    || [ "$(grep -cE '^FAIL ' "$work/contract-unrun.out")" -ne 1 ] \
    || [ "$(grep -cE '^PASS ' "$work/contract-unrun.out")" -ne 1 ]; then
    echo "SELF-TEST RED: gate_unrun did not emit exactly UNRUN + FAIL beside the PASS" >&2
    return 1
  fi
  return 0
}

run_mutation_self_test() {
  local work
  local fixture_root
  local failing_log
  local unrun_log

  work="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-check-self-test.XXXXXX")"
  fixture_root="$work/root"
  mkdir -p "$fixture_root/registries" "$fixture_root/scripts" \
    "$fixture_root/tools"
  GATE_LOG_DIR="$work/gate-logs"
  mkdir -p "$GATE_LOG_DIR"

  cat >"$fixture_root/scripts/fails.sh" <<'EOF'
#!/usr/bin/env bash
exit 23
EOF
  cat >"$fixture_root/registries/failing.toml" <<'EOF'
[[checker]]
symbol = "mutation_failing_gate"
kind = "script"
artifact = "scripts/fails.sh"
status = "live"
EOF
  failing_log="$work/failing-registration.log"
  if (
    reset_registered_counters
    run_registered_gates \
      "$fixture_root" "$fixture_root/registries/failing.toml" 0
    print_registered_summary
  ) >"$failing_log" 2>&1; then
    echo "SELF-TEST RED: a registered failing gate produced a green exit" >&2
    return 1
  fi
  if ! grep -Fq "RED registered script scripts/fails.sh" "$failing_log"; then
    echo "SELF-TEST RED: failing registration was not reported RED" >&2
    return 1
  fi
  if grep -Fq "ALL GATES GREEN" "$failing_log"; then
    echo "SELF-TEST RED: failing registration printed a green verdict" >&2
    return 1
  fi

  cat >"$fixture_root/tools/unwired.rs" <<'EOF'
// Existing artifact with no supported binary runner mapping.
EOF
  cat >"$fixture_root/registries/unwired.toml" <<'EOF'
[[checker]]
symbol = "mutation_unwired_gate"
kind = "binary"
artifact = "tools/unwired.rs"
status = "live"
EOF
  unrun_log="$work/unrun-registration.log"
  if (
    reset_registered_counters
    run_registered_gates \
      "$fixture_root" "$fixture_root/registries/unwired.toml" 0
    print_registered_summary
  ) >"$unrun_log" 2>&1; then
    echo "SELF-TEST RED: an unwired registered gate produced a green exit" >&2
    return 1
  fi
  if ! grep -Fq "UNRUN registered binary tools/unwired.rs" "$unrun_log"; then
    echo "SELF-TEST RED: unwired registration was not reported UNRUN" >&2
    return 1
  fi
  if grep -Fq "ALL GATES GREEN" "$unrun_log"; then
    echo "SELF-TEST RED: unwired registration printed a green verdict" >&2
    return 1
  fi

  # cargo-test attribution. The old rule was one workspace exit code stamped on
  # every cargo-test artifact, so it could not express any of the four verdicts
  # below and could never say UNRUN. These fixtures are the control: each one
  # asserts a DIFFERENT verdict from the SAME nonzero workspace exit code, which
  # the old rule would have reported identically as RED.
  mkdir -p "$fixture_root/crates/demo/tests" "$fixture_root/crates/twin/tests"
  : >"$fixture_root/crates/demo/tests/ran_ok.rs"
  : >"$fixture_root/crates/demo/tests/ran_failed.rs"
  : >"$fixture_root/crates/demo/tests/never_ran.rs"
  : >"$fixture_root/crates/demo/tests/dup.rs"
  : >"$fixture_root/crates/twin/tests/dup.rs"
  cargo_fixture_log="$work/cargo-test-fixture.log"
  cat >"$cargo_fixture_log" <<'EOF'
     Running tests/ran_ok.rs (/t/deps/ran_ok-1111111111111111)
running 3 tests
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
     Running tests/ran_failed.rs (/t/deps/ran_failed-2222222222222222)
running 2 tests
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
     Running tests/dup.rs (/t/deps/dup-3333333333333333)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
     Running tests/dup.rs (/t/deps/dup-4444444444444444)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
EOF
  cargo_case() { # artifact expected-outcome label
    local got
    got="$(cargo_test_artifact_outcome "$cargo_fixture_log" "$1" 101 | cut -f1)"
    if [ "$got" != "$2" ]; then
      echo "SELF-TEST RED: $3 attributed '$got', expected '$2'" >&2
      return 1
    fi
    return 0
  }
  cargo_case "crates/demo/tests/ran_ok.rs" pass \
    "a cargo-test artifact whose own binary passed" || return 1
  cargo_case "crates/demo/tests/ran_failed.rs" red \
    "a cargo-test artifact whose own binary failed" || return 1
  cargo_case "crates/demo/tests/never_ran.rs" unrun \
    "a cargo-test artifact whose binary never ran" || return 1
  cargo_case "crates/demo/tests/dup.rs" unrun \
    "an ambiguous cargo-test artifact name" || return 1
  if [ "$(cargo_test_artifact_outcome /nonexistent-log \
      crates/demo/tests/ran_ok.rs 101 | cut -f1)" != "unrun" ]; then
    echo "SELF-TEST RED: a missing workspace log did not attribute UNRUN" >&2
    return 1
  fi
  if [ "$(cargo_test_artifact_outcome "$cargo_fixture_log" \
      tools/registry-check/src/main.rs 101 | cut -f1)" != "unrun" ]; then
    echo "SELF-TEST RED: an unattributable artifact did not fall back to UNRUN" >&2
    return 1
  fi

  cat >"$fixture_root/registries/cargo_never_ran.toml" <<'EOF'
[[checker]]
symbol = "mutation_cargo_never_ran"
kind = "cargo-test"
artifact = "crates/demo/tests/never_ran.rs"
status = "live"
EOF
  cargo_unrun_log="$work/cargo-unrun-registration.log"
  if (
    reset_registered_counters
    run_registered_gates "$fixture_root" \
      "$fixture_root/registries/cargo_never_ran.toml" 101 "$cargo_fixture_log"
    print_registered_summary
  ) >"$cargo_unrun_log" 2>&1; then
    echo "SELF-TEST RED: an unrun cargo-test artifact produced a green exit" >&2
    return 1
  fi
  if ! grep -Fq "UNRUN registered cargo-test crates/demo/tests/never_ran.rs" \
      "$cargo_unrun_log"; then
    echo "SELF-TEST RED: an unrun cargo-test artifact was not reported UNRUN" >&2
    return 1
  fi

  verdict_stream_control "$work" || return 1
  verdict_contract_control "$work" || return 1

  echo "CHECK.SH MUTATION SELF-TEST PASS"
  echo "  failing registered gate: RED"
  echo "  registered gate without a runner: UNRUN and nonzero"
  echo "  cargo-test attribution: pass / red / unrun / ambiguous all separated"
  echo "  unrun cargo-test artifact: UNRUN and nonzero"
  echo "  reporting contract: every verdict on stdout, under both ^RED and ^FAIL,"
  echo "    and absent from a green run's stdout (streams captured separately)"
  echo "  verdict contract: conformant gate silent; L1/L2/L3/L4 rogues each named;"
  echo "    unreadable gate fails closed; a set -e abort still emits ^FAIL"
  echo "  three states: a gate executing no assertions reports UNRUN + FAIL, never"
  echo "    PASS, and exits nonzero; gate_unrun emits both tokens exactly once"
  echo "  evidence retained at $work"
}

case "${1:-}" in
  "")
    ;;
  --self-test)
    run_mutation_self_test
    exit $?
    ;;
  *)
    echo "usage: scripts/check.sh [--self-test]" >&2
    exit 2
    ;;
esac

cd "$ROOT" || exit 1
GATE_LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-check-gates.XXXXXX")"
echo "gate logs: $GATE_LOG_DIR"

# Installed here, not at load: --self-test runs fixtures in subshells, and a
# bash subshell inherits and fires the parent's EXIT trap.
gate_init "check.sh"

run_core_gate \
  "file-coverage closure (every tracked file inspected or declared)" \
  run_file_coverage
run_core_gate \
  "shell lint (bash -n + shellcheck) over tracked shell deliverables" \
  run_shell_lint
run_core_gate \
  "verdict-contract closure (every gate reports under one token on one stream)" \
  run_verdict_contract
run_core_gate "cargo fmt --check" cargo fmt --check
run_core_gate "cargo check --all-targets" cargo check --all-targets
run_core_gate "cargo clippy --all-targets -- -D warnings" \
  cargo clippy --all-targets -- -D warnings
CARGO_TEST_LOG="$GATE_LOG_DIR/core-cargo-test.log"
run_core_gate "cargo test --workspace --no-fail-fast" run_cargo_test_workspace
CARGO_TEST_RC="$LAST_GATE_RC"
run_core_gate "UBS over every tracked Rust source" run_ubs

run_registered_gates "$ROOT" "$CHECKER_INDEX" "$CARGO_TEST_RC"

echo
echo "CORE GATES: $CORE_EXECUTED of $CORE_EXPECTED executed; $CORE_PASSED passed; $CORE_RED red"
REGISTERED_SUMMARY_RC=0
print_registered_summary || REGISTERED_SUMMARY_RC=$?
if [ "$CORE_RED" -ne 0 ] || [ "$CORE_EXECUTED" -ne "$CORE_EXPECTED" ] \
  || [ "$REGISTERED_SUMMARY_RC" -ne 0 ]; then
  # The overall verdict goes to BOTH streams: it is the one line that must
  # reach a reader who captured only one of them. Every per-gate verdict above
  # is on stdout, so the stdout transcript is complete on its own.
  echo "QUALITY GATE RED"
  echo "QUALITY GATE RED" >&2
  exit 1
fi

echo "ALL GATES GREEN — core $CORE_EXECUTED/$CORE_EXPECTED; registered live $REGISTERED_EXECUTED/$REGISTERED_EXPECTED; file coverage $COV_INSPECTED/$COV_TRACKED inspected, $COV_EXEMPT declared-not-inspected"
