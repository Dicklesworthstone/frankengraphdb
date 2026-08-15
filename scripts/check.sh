#!/usr/bin/env bash
# =============================================================================
# check.sh — the convenience quality-gate wrapper (AGENTS.md).
# Bead: fgdb-gate-coverage-checksh-omits-live-gates-u7mw
#
# Runs every mandatory core check and every unique live checker artifact
# declared by registries/checker_index.toml. Registered cargo-test artifacts
# are covered by the workspace cargo-test invocation, except for the proven
# catalog-only scope below. Registered scripts and registry-check binaries are
# discovered and invoked from their artifact paths.
#
# topology-check and unsafe-ledger-check are load-bearing for the file-coverage
# closure: the coverage table claims
# topology-check as the inspector for rust-toolchain.toml,
# registries/workspace_topology.toml and registries/unsafe_boundary_ledger.toml;
# unsafe-ledger-check plus w1_unsafe_tool_lanes inspect
# registries/unsafe_verification_lanes.toml; topology-check also inspects
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
CORE_UNRUN=0
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
CARGO_TEST_MODE="workspace"
COV_TRACKED=0
COV_INSPECTED=0
COV_EXEMPT=0

CORE_GATE_FILE_COVERAGE="file-coverage closure (every tracked file inspected or declared)"
CORE_GATE_SHELL_LINT="shell lint (bash -n + shellcheck) over tracked shell deliverables"
CORE_GATE_VERDICT_CONTRACT="verdict-contract closure (every gate reports under one token on one stream)"
CORE_GATE_DOMAIN_CLOSURE="gate-domain closure (every verdict declares its tracked input domain)"
CORE_GATE_FMT="cargo fmt --check"
CORE_GATE_CHECK="cargo check --all-targets"
CORE_GATE_CLIPPY="cargo clippy --all-targets -- -D warnings"
CORE_GATE_TEST="cargo test --workspace --no-fail-fast"
CORE_GATE_UBS="UBS over every tracked Rust source"
CORE_GATE_ROSTER=(
  "$CORE_GATE_FILE_COVERAGE"
  "$CORE_GATE_SHELL_LINT"
  "$CORE_GATE_VERDICT_CONTRACT"
  "$CORE_GATE_DOMAIN_CLOSURE"
  "$CORE_GATE_FMT"
  "$CORE_GATE_CHECK"
  "$CORE_GATE_CLIPPY"
  "$CORE_GATE_TEST"
  "$CORE_GATE_UBS"
)

# fgdb-fa3k: this is deliberately a positive, mechanical scope rather than a
# heuristic about file extensions. workspace_topology.rs proves that crates/**
# reaches registry content at compile time through exactly one edge:
# fgdb-types/src/refs.rs -> registries/logical_object_kinds.toml. A catalog
# increment outside that projection cannot change a crate build; changes to the
# projection, an engine crate, or any other path retain the full workspace run.
catalog_lane_test_scope_for_paths() {
  [ "$#" -ne 0 ] || return 1

  local path
  for path in "$@"; do
    case "$path" in
      registries/logical_object_kinds.toml) return 1 ;;
      registries/*|tools/registry-check/*)  ;;
      *)                                    return 1 ;;
    esac
  done
  return 0
}

catalog_lane_test_scope() {
  local -a paths=()

  mapfile -t paths < <(
    {
      git diff --no-renames --name-only HEAD
      git ls-files --others --exclude-standard
    } | sort -u
  )
  catalog_lane_test_scope_for_paths "${paths[@]}"
}

select_cargo_test_mode() {
  if catalog_lane_test_scope; then
    local previous="$CORE_GATE_TEST" replaced=0 i
    CARGO_TEST_MODE="catalog"
    CORE_GATE_TEST="cargo test --catalog-lane (registry-check + registered codec target)"
    # CORE_GATE_ROSTER captured the workspace label at array assignment, so the
    # reassignment above must be mirrored into the roster or the gate-domain
    # closure iterates a label core_gate_domain no longer declares and reds
    # every catalog-lane run. Replace exactly one entry and say so if not.
    for i in "${!CORE_GATE_ROSTER[@]}"; do
      if [ "${CORE_GATE_ROSTER[$i]}" = "$previous" ]; then
        CORE_GATE_ROSTER[i]="$CORE_GATE_TEST"
        replaced=$((replaced + 1))
      fi
    done
    if [ "$replaced" -ne 1 ]; then
      echo "SELF-TEST RED: catalog-lane mode expected to replace exactly one" \
        "roster entry for \"$previous\", replaced $replaced" >&2
      exit 1
    fi
  fi
}

catalog_lane_test_scope_control() {
  if ! catalog_lane_test_scope_for_paths \
      registries/appendix_a_catalog.toml \
      tools/registry-check/tests/identity.rs; then
    echo "SELF-TEST RED: a non-logical catalog lane did not select the scoped test" >&2
    return 1
  fi
  if catalog_lane_test_scope_for_paths; then
    echo "SELF-TEST RED: an empty change set selected the scoped test" >&2
    return 1
  fi
  if catalog_lane_test_scope_for_paths registries/logical_object_kinds.toml; then
    echo "SELF-TEST RED: the crate-bound logical-object registry selected the scoped test" >&2
    return 1
  fi
  if catalog_lane_test_scope_for_paths crates/fgdb-types/src/refs.rs; then
    echo "SELF-TEST RED: a crate change selected the scoped test" >&2
    return 1
  fi
  if catalog_lane_test_scope_for_paths scripts/check.sh; then
    echo "SELF-TEST RED: a gate-driver change selected the scoped test" >&2
    return 1
  fi
}

# Per-result input-domain attribution (fgdb-41p3). The outer check.sh tripwire
# still watches every tracked file for its whole run. These records let its final
# sample answer the narrower question: which already-emitted verdicts actually
# read a path that moved? Unknown declarations are treated as all-tracked and
# make their gate UNRUN; they can never rescue a verdict.
GATE_SCOPE_TRACKING=0
GATE_SCOPE_COUNT=0
GATE_SCOPE_FATAL=0
GATE_SCOPE_ABORTED=0
declare -a GATE_SCOPE_CLASS=()
declare -a GATE_SCOPE_KIND=()
declare -a GATE_SCOPE_LABEL=()
declare -a GATE_SCOPE_OUTCOME=()
declare -a GATE_SCOPE_DOMAIN=()

core_gate_domain() {
  case "$1" in
    "$CORE_GATE_FILE_COVERAGE")  printf 'all-tracked\n' ;;
    "$CORE_GATE_SHELL_LINT")     printf 'tracked-shell\n' ;;
    "$CORE_GATE_VERDICT_CONTRACT") printf 'verdict-shell\n' ;;
    "$CORE_GATE_DOMAIN_CLOSURE") printf 'domain-closure\n' ;;
    "$CORE_GATE_FMT")            printf 'rust-format\n' ;;
    # Compilation and runtime readers are deliberately broad. In particular,
    # registry-check tests read registries, generated documents and .beads at
    # run time; pretending their domain is Rust would silently retain a verdict
    # over the absent-corpus defect AGENTS.md documents.
    "$CORE_GATE_CHECK" | "$CORE_GATE_CLIPPY" | "$CORE_GATE_TEST")
      printf 'all-tracked\n'
      ;;
    "$CORE_GATE_UBS")            printf 'tracked-rust\n' ;;
    *)                           return 1 ;;
  esac
}

registered_gate_domain() {
  # Registered artifacts can read repository state through runtime paths that
  # checker_index.toml does not enumerate. Until a narrower domain has its own
  # completeness proof, every executable kind is explicitly all-tracked.
  case "$1" in
    cargo-test | script | binary) printf 'all-tracked\n' ;;
    *)                            return 1 ;;
  esac
}

gate_scope_record() {
  local class="$1" kind="$2" label="$3" outcome="$4" domain="$5"
  local i="$GATE_SCOPE_COUNT"
  GATE_SCOPE_CLASS[i]="$class"
  GATE_SCOPE_KIND[i]="$kind"
  GATE_SCOPE_LABEL[i]="$label"
  GATE_SCOPE_OUTCOME[i]="$outcome"
  GATE_SCOPE_DOMAIN[i]="$domain"
  GATE_SCOPE_COUNT=$((GATE_SCOPE_COUNT + 1))
}

gate_scope_reset() {
  GATE_SCOPE_COUNT=0
  GATE_SCOPE_FATAL=0
  GATE_SCOPE_ABORTED=0
  GATE_SCOPE_CLASS=()
  GATE_SCOPE_KIND=()
  GATE_SCOPE_LABEL=()
  GATE_SCOPE_OUTCOME=()
  GATE_SCOPE_DOMAIN=()
}

run_core_gate() {
  local label="$1"
  local domain="all-tracked"
  local outcome
  shift
  CORE_EXPECTED=$((CORE_EXPECTED + 1))

  if [ "$GATE_SCOPE_TRACKING" -eq 1 ]; then
    if ! domain="$(core_gate_domain "$label")"; then
      CORE_UNRUN=$((CORE_UNRUN + 1))
      LAST_GATE_RC=125
      gate_unrun "core: $label — tracked input domain is undeclared; treated as all-tracked"
      gate_scope_record core "" "$label" unrun all-tracked
      return 0
    fi
  fi

  if [ "$GATE_SCOPE_ABORTED" -eq 1 ]; then
    CORE_UNRUN=$((CORE_UNRUN + 1))
    LAST_GATE_RC="$GATE_EXIT_UNRUN"
    gate_unrun "core: $label — skipped after tracked tree movement"
    gate_scope_record core "" "$label" unrun "$domain"
    return 0
  fi

  CORE_EXECUTED=$((CORE_EXECUTED + 1))
  echo "==> $label"
  if "$@"; then
    CORE_PASSED=$((CORE_PASSED + 1))
    LAST_GATE_RC=0
    gate_pass "core: $label"
    outcome=pass
  else
    LAST_GATE_RC=$?
    CORE_RED=$((CORE_RED + 1))
    # RED is the refinement, FAIL is the contract token. Both anchored, both on
    # stdout, emitted together. See THE REPORTING CONTRACT.
    printf 'RED core: %s (exit %s)\n' "$label" "$LAST_GATE_RC"
    gate_fail "core: $label (exit $LAST_GATE_RC)"
    outcome=red
  fi
  if [ "$GATE_SCOPE_TRACKING" -eq 1 ]; then
    gate_scope_record core "" "$label" "$outcome" "$domain"
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
    .cargo/config.toml)                  echo "cargo check/clippy/test (workspace rustc configuration)" ;;
    deny.toml)
      if live_gate_inventory "$CHECKER_INDEX" 2>/dev/null \
          | grep -Fx $'script\tscripts/dependency_policy_e2e.sh' >/dev/null; then
        echo "dependency_policy_e2e (resolved dependency advisory, license, and source policy)"
      else
        echo ""
      fi
      ;;
    rust-toolchain.toml)                 echo "topology-check" ;;
    registries/threat_model.toml)        echo "threat-check" ;;
    registries/workspace_topology.toml)  echo "topology-check" ;;
    registries/unsafe_boundary_ledger.toml) echo "topology-check" ;;
    registries/unsafe_verification_lanes.toml) echo "unsafe-ledger-check + w1_unsafe_tool_lanes" ;;
    # EXACT, not the `registries/*.toml` glob below. laws.toml is validated by
    # a cargo test, not by `registry-check all`, so the glob would claim a gate
    # that never opens the file -- the fail-open this row's own comment names.
    registries/laws.toml)                echo "cargo test --workspace (tools/registry-check/tests/laws.rs: schema, plan-anchor resolution, the law-citation guard over registries/appendix_a_catalog.toml, 27 mutation fixtures)" ;;
    # EXACT for the same reason as laws.toml: both §5.1 command registries are
    # validated by cargo tests, not by `registry-check all`, so the glob below
    # would claim a gate that never opens either file.
    registries/command_contracts.toml)   echo "cargo test --workspace (tools/registry-check/tests/command_contracts.rs: schema, closed vocabularies, tag space, arm-slot uniqueness, live-row refusal, seed-population floor)" ;;
    registries/command_type_classification.toml) echo "cargo test --workspace (tools/registry-check/tests/command_type_classification.rs: schema, six-class vocabulary, contract-id coupling, plan-anchor naming, seed-population floor)" ;;
    registries/durable_state_slots.toml|registries/state_payload_fields.toml|registries/protocol_state_fields.toml|registries/prepared_state_fields.toml|registries/consensus_state_fields.toml)
                                         echo "registry-check all (durable-state-slot schema, command-ref/writer bijection, active-sentinel refusal, exact plane-local field projections)" ;;
    registries/*.toml)                   echo "registry-check all" ;;
    .beads/issues.jsonl)                 echo "architecture-check (parses every record; malformed line fails file:line)" ;;
    docs/ARCHITECTURE_DECISION_RECORD.md) echo "architecture-check (generated document)" ;;
    docs/THREAT_AND_TRUST_MODEL.md)      echo "threat-check (generated document)" ;;
    docs/WORKSPACE_TOPOLOGY.md)          echo "topology-check (generated document)" ;;
    docs/NEGATIVE_EVIDENCE.md)           echo "g0_negative_evidence_e2e (parses every entry; each doctrine id, bead and repair commit must resolve)" ;;
    # Hand-written narrative, unlike the three generated documents above -- so the
    # coverage is genuinely weaker and this row says so rather than implying a
    # content gate that does not exist. What IS enforced: membership in the prose
    # closure, and that every claim marker written into it resolves.
    #
    # MUTATION-PROVEN 2026-07-31 on a quiet root, both directions, same binary:
    #   * removed from `lint.scan` -> `registry-check all` exits 1 with
    #     `unclaimed_prose` naming this exact path (this is not hypothetical --
    #     the document landed in 21f8cc7 unregistered and the gate was red
    #     repo-wide until this row and its `scan` entry landed);
    #   * appended `FG-ZZZ-99` -> exits 1 with `unregistered_marker` at the
    #     injected line. The append control matters here: a truncation probe
    #     would only have re-proven the path law the first bullet already covers.
    #   * registered and unmutated -> exits 0, `files_scanned` 7->8 with
    #     `markers_seen` unchanged at 162, so admitting it flipped exactly one
    #     verdict and blessed nothing else.
    docs/REALITY_CHECK_AND_BRIDGE_PLAN.md) echo "registry-check lint (prose-closure membership + claim markers resolve; NOT the narrative content)" ;;
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
# MOVED 2026-07-27 (fgdb-gpms): timing-safe 796 -> 798, +2, all of it in
# `tools/registry-check/tests/identity.rs`. The two findings are the only two
# `==` comparisons added by `idr_refinement_claims_resolve_to_a_registered_arm`:
#     .position(|w| w.name == "OperationalRestoreTerminalPinBasisRef")
#     .position(|w| w.name == "ExternalCasRestoreServicePromotionReceiptRef")
# Both are wire-type-name lookups in a test fixture, the same false-positive
# class as the 338 this one file already contributes; no secret, signature or
# token is involved and no `subtle`-style helper could exist under Doctrine #1.
#
# ATTRIBUTED BY DIFFERENTIAL, not by reading the report. Per the KNOWN LIMIT
# above, the text listing samples ~3 sites per class and truncates silently, so
# site-level attribution is impossible from it — re-confirmed here, the sampled
# sites named none of the two. What discriminates is a per-file count against a
# settled pre-change root (`git archive 0c2d593 | tar -x` plus `git init`, since
# UBS refuses a shadow workspace outside a repo): tests/identity.rs 338 -> 340,
# src/identity.rs and tests/satisfiability.rs unchanged at 0.
# MOVED 2026-07-28 (fgdb-a10-active-spec-gap-nowp): timing-safe 798 -> 799, +1,
# in `tools/registry-check/tests/identity.rs`. The one finding is the only `==`
# comparison added by the historical-witness filter for the landed wrapper rows:
#     let post_erratum_a10_wrapper_field = |schema: &str| schema == "SequenceNeutralSpec<Tag>";
# A schema-name lookup in a test filter — the same false-positive class as the
# 340 this file already contributes; no secret, signature or token is involved.
# ATTRIBUTED BY DIFFERENTIAL per the standing recipe: per-file count against the
# settled pre-change copy, tests/identity.rs 340 -> 341, every other changed
# file unchanged.
# MOVED 2026-07-29 (fgdb-w2-object-identity-t0f): JWT 120 -> 123, +3, all of it
# in `crates/fgdb-chronicle/tests/erasure_recovery.rs`. The scanner matches the
# substring "decode" in TEST FUNCTION NAMES:
#     a_corrupted_symbol_is_rejected_before_it_can_perturb_a_decode
#     a_symbol_from_another_encoding_cannot_join_a_decode
#     ...and the module's third decode-named test
# No JWT, no token, and no signature validation exists anywhere in this
# workspace — the closed dependency universe forbids `jsonwebtoken` outright.
# Renaming the tests would buy a smaller number at the cost of names that no
# longer say what they prove, which is the wrong trade.
# ATTRIBUTED BY DIFFERENTIAL: fgdb-chronicle is a new crate, so `ubs
# crates/fgdb-chronicle/` reports the whole delta; measured 3 in that one file
# and 0 elsewhere in the crate.
# MOVED 2026-07-29 (fgdb-w2-root-bootstrap-hbf): timing-safe 799 -> 800, +1, in
# `crates/fgdb-chronicle/src/root.rs`. The one finding is the identity-tuple
# comparison that ends self-sufficient root recovery:
#     if recovered_identity_tuple(&recovered) != slot.identity_tuple()
# That tuple is an AUTHENTICATED PUBLIC IDENTITY (database id, namespace,
# incarnation, continuity digest, visibility epoch), not a secret or a MAC, and
# the comparison happens AFTER the AEAD and the keyed-ObjectId recomputation
# have already authenticated the bytes. There is nothing to leak by timing: an
# attacker who could vary the input has already failed two cryptographic
# checks. The genuine constant-time compares in this workspace are written as
# explicit XOR-accumulate loops (aead.rs tag compare, symbol.rs MAC compare).
# ATTRIBUTED BY DIFFERENTIAL: `ubs crates/fgdb-chronicle/` before this
# increment reported 3 criticals and after reports 4, with the single new site
# in root.rs and 0 elsewhere.
# MOVED 2026-07-29 (fgdb-w2-commit-protocol-9w3u, carrying fgdb-2a50's work):
# panic 135 -> 137 (+2) AND timing-safe 800 -> 808 (+8), all of both in
# `tools/registry-check/tests/identity.rs`.
#
# WHOSE CHANGE, AND WHY IT IS ACCOUNTED HERE. a9db867 is a chronicle commit, but
# the shared git index handed it another pane's staged MetaRestorePhase work as
# well — registries/appendix_a_catalog.toml (+381) and three registry-check
# sources. The findings are that pane's; the commit that carried them past the
# ratchet is mine, so the accounting is mine. This is the failure mode the
# ratchet exists to expose: without it the two panes' work would have merged
# into one unattributed number.
#
# The two panics are the projection lookups the MetaRestorePhase twin adds:
#     .unwrap_or_else(|| panic!("MetaRestorePhase arm {tag:#06x} exists"))
#     .unwrap_or_else(|| panic!("{variant_name} wire variant exists"))
# Both are test-only "the fixture must contain this row" assertions. A missing
# row has to abort the test, and `expect` cannot carry the formatted tag, so the
# alternative is a worse message rather than fewer panics.
#
# The eight timing-safe hits are the same false-positive class this one file
# already contributes 341 of — `==` beside an identifier named `key` or `code`,
# comparing SCHEMA KEYS and VIOLATION CODES to literals:
#     .filter(|schema| schema.key.source_key() == "top|RegisteredStrongRef")
#     .any(|violation| violation.code == "catalog_annotation_reference_target_mismatch")
# No secret, signature or token is involved, and Doctrine #1 forbids the
# `subtle`/`ring` helpers the scanner recommends.
#
# ATTRIBUTED BY DIFFERENTIAL against a detached worktree at cf75e59 — the commit
# that last set this baseline — rather than by arithmetic on the total. Over the
# IDENTICAL 123-file non-chronicle non-crypto domain (verified set-equal, not
# merely same-sized): panic 135 -> 137, timing-safe 799 -> 807, JWT 120 -> 120.
# Per file: tests/identity.rs 42 -> 44 and 341 -> 349, the whole delta, with
# appendix_a.rs and src/identity.rs unchanged. crates/fgdb-chronicle plus
# crates/fgdb-crypto contribute 0 / 1 / 3 and have not moved since cf75e59, so
# this increment's own new files (commit.rs, crash_point_matrix.rs) contribute
# zero — confirmed separately by scanning the 150-file domain with and without
# them, which left both counts identical.
# MOVED 2026-07-29 (fgdb-w1-foundation-types-tjk): JWT 123 -> 124, +1, in
# `crates/fgdb-delta-types/src/canonical.rs`. The one finding is the scanner
# matching the substring "decode" in the delta codec's scalar reader:
#     CanonicalScalar::decode(encoded).map_err(|_| CanonicalError::Scalar)
# Same false-positive class as the 123 already pinned, and false by construction
# for the same reason: `jsonwebtoken`, `DecodingKey` and `jwt` appear in ZERO
# tracked files and Doctrine #1 forbids ever adding them. Renaming a canonical
# decoder to avoid the word "decode" would be the wrong trade.
# ATTRIBUTED BY DIFFERENTIAL: the two files this increment adds
# (src/canonical.rs, tests/canonical_encoding.rs) scanned alone report exactly 1
# critical, this one; scanning the whole post-commit 152-file domain gives
# 137 / 808 / 124, so panic and timing-safe did not move at all.
# MOVED 2026-07-29 (fgdb-w2-delta-batches-og6n): timing-safe 808 -> 809, +1, in
# `crates/fgdb-reference/src/lib.rs`. The one finding is the sketch family's
# before-image check in the semantics oracle:
#     if actual != *before_state_digest
# It compares a SKETCH STATE DIGEST the row declares against the one the oracle
# materialized. Both sides are ordinary content digests of test-visible state in
# a crate that is compiled for tests and fuzzing only and never shipped; there
# is no secret, no remote caller, and no timing channel. Making it constant-time
# would need `subtle` or `ring`, which Doctrine #1 forbids, and the genuine
# constant-time compares in this workspace are explicit XOR-accumulate loops
# (aead.rs tag compare, symbol.rs MAC compare).
# ATTRIBUTED BY DIFFERENTIAL: the new crate's two files scanned alone report
# exactly 1 critical, this one, with panic and JWT both clean.
# MOVED 2026-07-29 (fgdb-verif-sim-q97e): timing-safe 809 -> 810, +1, in
# `crates/fgdb-sim/src/lib.rs`. The one finding is the FG-INV-09 cross-check
# that stands between a rewritten capsule and silently different graph state:
#     if recomputed != *logical_delta_template_digest
# It compares a digest recomputed from durable bytes against the one the commit
# marker declared. Both sides are content digests of a local file; there is no
# secret, no remote caller, and an attacker who can rewrite the capsule can
# rewrite the marker beside it, so a timing channel buys nothing that direct
# access does not already give. Doctrine #1 forbids the `subtle`/`ring` helpers
# the scanner recommends, and this workspace's genuine constant-time compares
# are explicit XOR-accumulate loops (aead.rs tag, symbol.rs MAC).
# ATTRIBUTED BY DIFFERENTIAL: the new crate's two files scanned alone report
# exactly this one critical, with panic and JWT both clean.
# MOVED 2026-07-29 (fgdb-rbab): JWT 124 -> 123, -1, by suppressing the exact
# false match on `CanonicalScalar::decode(encoded)` in
# crates/fgdb-delta-types/src/canonical.rs. This is the closed durable graph
# scalar decoder, not a JWT decoder; the crate has no JWT, signature-bypass, or
# authentication-validation state. The suppression is pinned at the call with
# that rationale rather than teaching the scanner to ignore arbitrary `decode`.
# ATTRIBUTED BY DIFFERENTIAL: the two changed delta files scanned alone moved
# from 1 critical in this class to 0, while the full tracked-source scan moved
# from 1071 total to 1070 and reported only this class changing, 124 -> 123.
# MOVED 2026-07-30 (fgdb-reference-snapshot-provenance-9bvm, fgdb-ew8z):
# timing-safe 810 -> 808, -2. Line-specific `ubs:ignore` dispositions now name
# the public, non-authentication semantics of the pre-existing sketch before-image
# digest and logical-template integrity digest comparisons. The new stream-prefix
# provenance comparison carries the same narrow disposition, so it contributes
# zero rather than increasing this class by one. These are content fingerprints
# any holder can recompute, not MACs, credentials, bearer tokens, or other secrets.
# WHY NOT MAKE THEM CONSTANT-TIME ANYWAY: doctrine #1 forbids the subtle/ring
# helpers the scanner recommends, while this workspace's genuine constant-time
# compares are explicit XOR-accumulate loops guarding real MACs. Using that shape
# here would falsely claim secret material. ATTRIBUTION: the eight changed files
# scan at 0 criticals; the full 175-source scan measures this class at exactly 808.
# MOVED 2026-07-30 (fgdb-u27g): timing-safe 808 -> 817, +9, all re-anchor
# residue. The hyphenated-member fix and its two census tests insert ~180 lines
# into tools/registry-check/src/appendix_source.rs; the matcher's (file,line)
# anchoring re-flags nine PRE-EXISTING comparison statements at their shifted
# windows (`assigned == display_name`, `affected_source_keys == [...]`,
# `schema.key.family == ...`), each one the long-standing false-positive shape
# where a catalog-name identifier (`name`, `family`, `path`) is file-wide in the
# sensitive-var set. The three genuinely new test lines of that same shape are
# ubs:ignore-annotated at their exact lines (net 0). ATTRIBUTED BY DIFFERENTIAL:
# tools/registry-check/src/appendix_source.rs scanned alone moved 51 -> 60 in
# this class with panic and JWT both unmoved; the full tracked-source scan moved
# 808 -> 817 with only this class changing.
# MOVED 2026-07-30 (fgdb-raptorq-decoder-boundary-panic-hpjb, df25e8c): JWT
# 123 -> 122, -1. The typed-decoder-bound fix replaced InactivationDecoder::new
# with try_new and attributed the exact false match inline at the call —
# `InactivationDecoder::decode` is the RFC 6330 erasure decoder, not a JWT
# decoder, and the crate has no JWT, signature-bypass, or authentication-
# validation state. The finding suppression landed there; the count was not
# updated, so this line records it. ATTRIBUTED BY DIFFERENTIAL: the UBS
# module's own JWT matcher reports crates/fgdb-chronicle/src/symbolize.rs at
# 0 findings against 1 at its parent, and the full tracked-source scan moved
# 123 -> 122 with only this class changing.
# MOVED 2026-07-31 (fgdb-teqw, 9e11f4a): panic 137 -> 132, -5, all in
# `crates/fgdb-types/src/context.rs`. The five test-only panic branches were:
#     lab report omitted an expected invariant
#     cancelled boundary unexpectedly succeeded
#     cancelled acquisition unexpectedly succeeded
#     cancelled context unexpectedly acquired an obligation
#     no terminal path existed at the selected depth
# Each became an assertion followed by explicit non-panicking control flow,
# preserving the test verdict while removing the scanner-critical macro. The
# parent file scans at exactly 5 findings in this class and the current file at
# 0; the commit diff contains five deletions and zero additions, and committed
# Rust changes through 8d651fa add or remove none of these four macro families.
# FOLLOW-UP 2026-08-01 (8c53adb): that later landing introduced three explicit
# test-only panic branches in the generated-history harness. This same repair
# converts them to `Result` returns or `assert_eq!`, retaining the failure text
# without moving the ratchet back upward. UBS reports those three macro sites as
# seven line findings because one invocation spans four lines; after the rewrite
# both newly landed files report zero findings in this class.
# UNMOVED by fgdb-j0vu, and the "unmoved" is the measurement rather than an
# assumption. Activating `crates/fgdb` (the end-to-end spine) added two tracked
# Rust files. Scanned alone they report ZERO criticals in all three classes:
#     ubs crates/fgdb/src/lib.rs crates/fgdb/tests/spine.rs   -> exit 0, 0 critical
# Both classes it could have moved were held flat deliberately:
#   * TIMING-SAFE. `rebuild` compares the recomputed template digest with the one
#     its marker declared — FG-INV-09's recompute-from-registered-bytes check, and
#     the thing that stops silent corruption from becoming silently different graph
#     state. Both operands are non-secret content fingerprints over LOCAL capsule
#     bytes; there is no secret, no remote caller and no timing channel, and the
#     `subtle`/`ring` helpers the scanner recommends are forbidden by doctrine #1.
#     Disposed at the exact line with `ubs:ignore`, same shape and same reasoning
#     as fgdb-ew8z and as the pre-existing dispositions in fgdb-chronicle.
#   * PANIC MACROS. The first draft of the law file used five
#     `other => panic!(..)` match arms and scanned at 9 criticals. They were
#     rewritten as `assert!(matches!(..), "{refusal:?}")` — same verdict, same
#     diagnostic text, zero macro findings — following the repair 8c53adb already
#     applied to the generated-history harness, rather than widening this table.
#
# MEASURED HAZARD IN THE `ubs:ignore` CONVENTION ITSELF, recorded here because it
# silently costs a disposition and every future one is exposed to it: UBS anchors
# the annotation to the IMMEDIATELY FOLLOWING line. A disposition written as a
# multi-line comment with `ubs:ignore` on the FIRST line does not suppress
# anything. Measured both ways on the same comparison — annotation four lines
# above the `if`: still 1 critical; annotation on the line directly above it:
# 0 criticals, exit 0. Control: `ubs crates/fgdb-chronicle/src/commit.rs` scans at
# 0 criticals, and its dispositions are all single lines directly above their
# comparisons. So a disposition that reads correctly to a human can be inert, and
# the only way to know is to re-scan the file after writing it.
# MOVED 2026-08-05 (fgdb-84p2): timing-safe 817 -> 164, -653, and it is TOOL
# DRIFT, not tree movement. ATTRIBUTED BY TOOL CONTROL rather than by code
# differential: today's ubs (Meta-Runner v5.3.8) run on a detached tree at
# 93a6eb0 — the exact commit that set baseline 817 — reports 164 for this class
# while panic (132) and JWT (122) match that baseline EXACTLY, so the scan
# domain is the same and only this one detector narrowed. No tracked Rust file
# moved this count; re-pinning to the current tool's partition is the honest
# baseline, and the 653 retired findings were the `==`-beside-key/code
# false-positive population this block already adjudicated repeatedly above.
# ADDED 2026-08-05 (fgdb-84p2): "Security-sensitive non-crypto randomness=2",
# a class v5.3.8 introduces. Both findings are `Instant::now()` in
# `crates/fgdb/tests/cx_probe.rs` (304, 311) — the §17 write-cost sweep's
# stopwatch reading elapsed time in a test. No token, session, nonce, salt, or
# key is generated anywhere near it; the scanner pattern-matched a timing call
# inside what it took for a generation context. OsRng/getrandom would be
# nonsensical here, and Doctrine #1 forbids the `ring`/`openssl` helpers the
# scanner recommends. Adjudicated false-positive and pinned at exactly 2 so a
# THIRD finding in this class fails closed like any other unadjudicated drift.
# MOVED 2026-08-05 (fgdb-j8lt): panic 132 -> 137, +5, and it is TREE MOVEMENT
# that skipped the update-in-same-commit protocol: two commits landed after
# the fgdb-84p2 re-pin's measurement root (27bf649-based) without touching
# this table. ATTRIBUTED BY COMMIT DIFFERENTIAL, additions grepped per commit
# over '*.rs', removals zero everywhere in between:
#   * 9b80da3 (+2): two test-harness `poll_ready` helpers in the Vfs-generic
#     CommitCoordinator work panic when a future suspends over a ready-only
#     source — deliberate cannot-happen assertions in test code.
#   * 8876ea4 (+3): the FaultVfs crash-matrix campaigns assert a typed ENOSPC
#     Io error, a TornWrite fault kind, and a refused open-after-hole — all
#     three `panic!`s are test-only mismatch arms carrying diagnostics.
# Re-measured with the gate's own invocation (ubs v5.3.8, --only=rust --ci,
# 238 tracked sources, HEAD 7dbed03 plus this tranche): four test-only panic
# branches became explicit assertions/typed comparisons, reducing panic from
# 137 to 133 while timing-safe (164), JWT (122), and randomness (2) remained
# exact. The partition closes at 133+164+122+2 = 421 = the reported Critical
# total. Same tool version as the baseline-setting run, so this is not detector
# drift.
# MOVED 2026-08-12 (fgdb-verif-sextant-ss83): timing-safe 164 -> 183, +19,
# and it is TREE MOVEMENT introduced by f292858's bounded BOCPD+SR evidence
# implementation. ATTRIBUTED BY EXACT FILE DIFFERENTIAL with the gate's UBS
# v5.3.8 invocation: regime.rs reports 19 at f292858^, 38 at f292858, 38 at
# 3d0098c, and 38 at 23a1ac7. A stable whole-tree scan at 23a1ac7 over 241
# tracked Rust sources closed at 133+183+122+2 = 440.
#
# The added matches compare canonical encoding domains, public detector/profile
# ObjectIds, evidence counters/status/thresholds, and replay state. They are not
# bearer tokens, credentials, HMACs, signatures, nonces, or other secrets, and
# there is no remote timing oracle. Constant-time equality would be semantically
# inapplicable; the scanner's suggested crypto dependencies are also forbidden
# by Doctrine #1. Keep the findings visible and equality-pinned here instead of
# suppressing them locally: any future increase OR decrease still fails closed.
UBS_CRITICAL_BASELINE=(
  "Secret/token comparisons without timing-safe equality=183"
  "panic!/unreachable!/todo!/unimplemented!=133"
  "JWT decode, validation bypass, or missing claim binding=122"
  "Security-sensitive non-crypto randomness=2"
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

gate_domain_wiring_complete() {
  local source="$1"
  local tracking_calls finalize_calls closure_calls ledger_calls checkpoint_calls failures=0

  if [ ! -r "$source" ]; then
    echo "ERROR: gate-domain wiring source is unreadable: $source" >&2
    return 1
  fi
  tracking_calls="$(grep -c '^GATE_SCOPE_TRACKING=1$' "$source")"
  finalize_calls="$(grep -c '^gate_scope_finalize_tree_stability$' "$source")"
  closure_calls="$(grep -c \
    '^run_core_gate "\$CORE_GATE_DOMAIN_CLOSURE" run_gate_domain_closure$' \
    "$source")"
  ledger_calls="$(grep -c '^  if ! gate_scope_records_complete; then$' "$source")"
  checkpoint_calls="$(grep -c '^[[:space:]]*gate_scope_abort_if_tree_moved ' "$source")"
  if [ "$tracking_calls" -ne 1 ]; then
    echo "ERROR: check.sh enables scoped result recording $tracking_calls time(s), expected 1" >&2
    failures=$((failures + 1))
  fi
  if [ "$finalize_calls" -ne 1 ]; then
    echo "ERROR: check.sh invokes scoped final attribution $finalize_calls time(s), expected 1" >&2
    failures=$((failures + 1))
  fi
  if [ "$closure_calls" -ne 1 ]; then
    echo "ERROR: check.sh invokes gate-domain closure $closure_calls time(s), expected 1" >&2
    failures=$((failures + 1))
  fi
  if [ "$ledger_calls" -ne 1 ]; then
    echo "ERROR: check.sh checks scoped result-ledger closure $ledger_calls time(s), expected 1" >&2
    failures=$((failures + 1))
  fi
  # Nine core gates, all three registered-artifact control-flow exits, and one
  # after the registered inventory runner returns. A moved worktree must stop
  # the remaining chain at the first completed boundary (fgdb-3e12).
  if [ "$checkpoint_calls" -ne 13 ]; then
    echo "ERROR: check.sh wires $checkpoint_calls early tree checkpoint(s), expected 13" >&2
    failures=$((failures + 1))
  fi
  [ "$failures" -eq 0 ]
}

# Validate the declaration layer before trusting it to retain any verdict.
#
# This is an inverse closure over both populations: every core label in the
# fixed roster resolves to a known, nonempty domain, and every live registered
# artifact kind resolves too. A new core gate cannot inherit a narrow neighbour
# accidentally, and an unknown registered kind is all-tracked + UNRUN at
# execution time. The latter is deliberately stricter than defaulting to an
# empty set, which would make any unrelated movement "prove" the gate stable.
run_gate_domain_closure() {
  local label domain projection inventory row kind artifact
  local core_count=0 registered_count=0 failures=0
  local -A seen_core=()

  if [ -z "$GATE_TREE_LIST_START" ]; then
    echo "ERROR: the run-start tracked listing is absent; no domain can be proved nonempty" >&2
    return 1
  fi
  if ! gate_domain_wiring_complete "$ROOT/scripts/check.sh"; then
    failures=$((failures + 1))
  fi

  for label in "${CORE_GATE_ROSTER[@]}"; do
    if [ -n "${seen_core[$label]+set}" ]; then
      echo "ERROR: duplicate core gate-domain declaration: $label" >&2
      failures=$((failures + 1))
      continue
    fi
    seen_core["$label"]=1
    if ! domain="$(core_gate_domain "$label")"; then
      echo "ERROR: core gate has no tracked input-domain declaration: $label" >&2
      failures=$((failures + 1))
      continue
    fi
    if ! gate_tree_domain_known "$domain"; then
      echo "ERROR: core gate $label declares unknown domain $domain" >&2
      failures=$((failures + 1))
      continue
    fi
    if ! projection="$(gate_tree_domain_listing "$GATE_TREE_LIST_START" "$domain")" \
      || [ -z "$projection" ]; then
      echo "ERROR: core gate $label declares an empty or unreadable domain $domain" >&2
      failures=$((failures + 1))
      continue
    fi
    core_count=$((core_count + 1))
  done

  if [ ! -r "$CHECKER_INDEX" ]; then
    echo "ERROR: cannot read $CHECKER_INDEX for registered gate-domain closure" >&2
    return 1
  fi
  if ! inventory="$(live_gate_inventory "$CHECKER_INDEX")" || [ -z "$inventory" ]; then
    echo "ERROR: no live registered artifacts were derived for gate-domain closure" >&2
    return 1
  fi
  while IFS=$'\t' read -r kind artifact; do
    if [ -z "$kind" ] || [ -z "$artifact" ]; then
      echo "ERROR: malformed live checker row has no kind/artifact domain key" >&2
      failures=$((failures + 1))
      continue
    fi
    if ! domain="$(registered_gate_domain "$kind")"; then
      echo "ERROR: registered $kind $artifact has no tracked input-domain declaration" >&2
      failures=$((failures + 1))
      continue
    fi
    if ! gate_tree_domain_known "$domain"; then
      echo "ERROR: registered $kind $artifact declares unknown domain $domain" >&2
      failures=$((failures + 1))
      continue
    fi
    if ! projection="$(gate_tree_domain_listing "$GATE_TREE_LIST_START" "$domain")" \
      || [ -z "$projection" ]; then
      echo "ERROR: registered $kind $artifact declares empty domain $domain" >&2
      failures=$((failures + 1))
      continue
    fi
    registered_count=$((registered_count + 1))
  done <<<"$inventory"

  if [ "$failures" -ne 0 ]; then
    echo "ERROR: $failures gate-domain declaration failure(s); undeclared means all-tracked + UNRUN" >&2
    return 1
  fi
  echo "    gate domains: $core_count core + $registered_count registered artifacts declared; unknown defaults to all-tracked + UNRUN"
  return 0
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

decrement_registered_kind_executed() {
  case "$1" in
    cargo-test) REGISTERED_CARGO_EXECUTED=$((REGISTERED_CARGO_EXECUTED - 1)) ;;
    script) REGISTERED_SCRIPT_EXECUTED=$((REGISTERED_SCRIPT_EXECUTED - 1)) ;;
    binary) REGISTERED_BINARY_EXECUTED=$((REGISTERED_BINARY_EXECUTED - 1)) ;;
  esac
}

record_registered_result() {
  local kind="$1"
  local artifact="$2"
  local outcome="$3"
  local detail="$4"
  local domain="all-tracked"

  REGISTERED_EXPECTED=$((REGISTERED_EXPECTED + 1))
  increment_registered_kind_expected "$kind"
  if [ "$GATE_SCOPE_TRACKING" -eq 1 ] \
    && ! domain="$(registered_gate_domain "$kind")"; then
    domain="all-tracked"
    outcome=unrun
    detail="tracked input domain is undeclared; treated as all-tracked"
  fi
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
  if [ "$GATE_SCOPE_TRACKING" -eq 1 ]; then
    gate_scope_record registered "$kind" "$artifact" "$outcome" "$domain"
  fi
}

# A child may refine its non-green exit as UNRUN only when BOTH channels of the
# shared contract agree:
#   1. exit GATE_EXIT_UNRUN (the authoritative non-green status), and
#   2. stdout contains one or more exactly paired UNRUN + FAIL lines, with no
#      standalone FAIL or RED line.
#
# The pairing is load-bearing. Several live scripts historically used exit 2
# for build/usage errors; accepting the code alone would relabel a real failure
# as environmental. Reading the merged log is also unsafe because diagnostics
# on stderr are deliberately unconstrained, so run_registered_command captures
# the contract transcript and diagnostics separately.
registered_command_reported_only_unrun() { # exit-code stdout-transcript
  local rc="$1"
  local transcript="$2"

  [ "$rc" -eq "$GATE_EXIT_UNRUN" ] || return 1
  [ -f "$transcript" ] || return 1
  awk '
    /^UNRUN / {
      detail = substr($0, 7)
      unrun[detail]++
      unrun_count++
      next
    }
    /^FAIL / {
      detail = substr($0, 6)
      fail[detail]++
      fail_count++
      next
    }
    /^RED / {
      red_count++
      next
    }
    END {
      if (unrun_count == 0 || fail_count != unrun_count || red_count != 0) {
        exit 1
      }
      for (detail in unrun) {
        if (unrun[detail] != fail[detail]) {
          exit 1
        }
      }
      for (detail in fail) {
        if (fail[detail] != unrun[detail]) {
          exit 1
        }
      }
    }
  ' "$transcript"
}

run_registered_command() {
  local kind="$1"
  local artifact="$2"
  shift 2
  local log
  local diagnostics_log
  local gate_rc

  REGISTERED_SEQ=$((REGISTERED_SEQ + 1))
  log="$GATE_LOG_DIR/registered-$REGISTERED_SEQ.log"
  diagnostics_log="$GATE_LOG_DIR/registered-$REGISTERED_SEQ.diagnostics.log"
  echo "==> registered $kind: $artifact"
  if "$@" >"$log" 2>"$diagnostics_log"; then
    record_registered_result "$kind" "$artifact" pass \
      "exit 0; transcript $log; diagnostics $diagnostics_log"
  else
    gate_rc=$?
    if registered_command_reported_only_unrun "$gate_rc" "$log"; then
      record_registered_result "$kind" "$artifact" unrun \
        "exit $gate_rc; transcript $log; diagnostics $diagnostics_log"
    else
      record_registered_result "$kind" "$artifact" red \
        "exit $gate_rc; transcript $log; diagnostics $diagnostics_log"
    fi
  fi
}

# Return success only when a failed workspace test log contains the measured
# shared-target race and no evidence of another failure.
#
# Cargo reports this race as one `could not execute process ... (never
# executed)` diagnostic paired with `No such file or directory (os error 2)`.
# The pairing matters: ENOENT by itself can describe a missing input, and
# "never executed" by itself can describe another process-launch failure.
#
# A real failure always dominates the retry opportunity. In particular, a
# failing test, compiler diagnostic, panic, signal, or a different child-process
# failure makes the first attempt authoritative and red. This prevents a flaky
# test from borrowing the missing-binary retry and turning green on its second
# run.
cargo_test_retryable_missing_binary_race() { # log
  local log="$1"
  [ -f "$log" ] || return 1

  awk '
    index($0, "could not execute process") &&
      index($0, "(never executed)") {
        never_executed += 1
      }
    index($0, "No such file or directory (os error 2)") {
      missing_binary += 1
    }
    /^test result: FAILED/ ||
      /^failures:$/ ||
      /^thread .* panicked/ ||
      /process did not exit successfully/ ||
      /process didn.t exit successfully/ ||
      /signal: [0-9]+/ {
        disqualifying_failure = 1
      }
    /^error(\[[^]]+\])?:/ &&
      $0 !~ /^error: test failed, to rerun pass / &&
      $0 !~ /^error: [0-9]+ targets? failed:$/ {
        disqualifying_failure = 1
      }
    END {
      if (never_executed > 0 &&
          never_executed == missing_binary &&
          !disqualifying_failure) {
        exit 0
      }
      exit 1
    }
  ' "$log"
}

# Run the selected test gate and keep its output, because the registered
# cargo-test artifacts below are attributed FROM it. The catalog mode is safe
# only for the mechanically selected shape above: registry-check owns the
# registries, while the registered codec target transitively compiles fgdb-types
# and therefore the sole crates-to-registry include edge.
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
#
# fgdb-a9tg adds one narrower recovery: the shared `/data/tmp/cargo-target`
# sometimes removes a scheduled test binary underneath another pane. When the
# classifier above proves that is the ONLY failure, rerun the workspace once
# and say so in the transcript. The first log remains in the gate-log directory;
# registered cargo-test attribution uses only the retry log. A second ENOENT or
# any real failure stays red, and there is never a third attempt.
run_cargo_test_once() { # log
  local log="$1"
  local registry_check_rc
  local codec_rc

  if [ "$CARGO_TEST_MODE" = "catalog" ]; then
    : >"$log" || return 1
    cargo test -p registry-check --no-fail-fast 2>&1 | tee -a "$log"
    registry_check_rc="${PIPESTATUS[0]}"
    cargo test -p fgdb-codec --test generated_durable_roundtrip 2>&1 | tee -a "$log"
    codec_rc="${PIPESTATUS[0]}"
    if [ "$registry_check_rc" -ne 0 ]; then
      return "$registry_check_rc"
    fi
    return "$codec_rc"
  fi

  cargo test --workspace --no-fail-fast 2>&1 | tee "$log"
}

run_cargo_test_workspace() {
  local first_log="$CARGO_TEST_LOG"
  local first_rc
  local retry_log
  local retry_rc

  run_cargo_test_once "$first_log"
  first_rc=$?
  if [ "$first_rc" -eq 0 ] ||
      [ "$first_rc" -ne 101 ] ||
      ! cargo_test_retryable_missing_binary_race "$first_log"; then
    return "$first_rc"
  fi

  retry_log="${first_log%.log}.retry-1.log"
  printf '    cargo-test retry 1/1: attempt 1 hit only the shared-target '\
'never-executed ENOENT race; rerunning once (attempt log: %s)\n' "$first_log"
  run_cargo_test_once "$retry_log"
  retry_rc=$?
  CARGO_TEST_LOG="$retry_log"
  return "$retry_rc"
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
    if [ "$GATE_SCOPE_ABORTED" -eq 1 ]; then
      record_registered_result "${kind:-missing-kind}" \
        "${artifact:-missing-artifact}" unrun \
        "skipped after tracked tree movement"
      continue
    fi
    if ! safe_artifact "$artifact"; then
      record_registered_result "${kind:-missing-kind}" \
        "${artifact:-missing-artifact}" unrun \
        "artifact path is missing or unsafe"
      gate_scope_abort_if_tree_moved "registered ${kind:-missing-kind} ${artifact:-missing-artifact}"
      continue
    fi
    if [ ! -f "$root/$artifact" ]; then
      record_registered_result "$kind" "$artifact" unrun \
        "artifact does not exist"
      gate_scope_abort_if_tree_moved "registered $kind $artifact"
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
    gate_scope_abort_if_tree_moved "registered $kind $artifact"
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

gate_scope_subject_name() {
  local i="$1"
  if [ "${GATE_SCOPE_CLASS[i]}" = core ]; then
    printf 'core: %s' "${GATE_SCOPE_LABEL[i]}"
  else
    printf 'registered %s %s' \
      "${GATE_SCOPE_KIND[i]}" "${GATE_SCOPE_LABEL[i]}"
  fi
}

# Prove that every emitted child verdict is represented exactly once before
# using the ledger to decide which claims survive a tree move. Merely enabling
# recording is insufficient: a missed call would otherwise leave that verdict
# outside attribution, and another affected record would keep the aggregate
# from noticing the omission.
gate_scope_records_complete() {
  local i class outcome domain
  local core_records=0 core_pass=0 core_red=0 core_unrun=0
  local registered_records=0 registered_pass=0 registered_red=0
  local registered_unrun=0 failures=0

  for ((i = 0; i < GATE_SCOPE_COUNT; i++)); do
    class="${GATE_SCOPE_CLASS[i]-}"
    outcome="${GATE_SCOPE_OUTCOME[i]-}"
    domain="${GATE_SCOPE_DOMAIN[i]-}"
    if ! gate_tree_domain_known "$domain"; then
      echo "ERROR: scoped result $i has an unknown or empty domain '$domain'" >&2
      failures=$((failures + 1))
    fi
    case "$class:$outcome" in
      core:pass)
        core_records=$((core_records + 1))
        core_pass=$((core_pass + 1))
        ;;
      core:red)
        core_records=$((core_records + 1))
        core_red=$((core_red + 1))
        ;;
      core:unrun)
        core_records=$((core_records + 1))
        core_unrun=$((core_unrun + 1))
        ;;
      registered:pass)
        registered_records=$((registered_records + 1))
        registered_pass=$((registered_pass + 1))
        ;;
      registered:red)
        registered_records=$((registered_records + 1))
        registered_red=$((registered_red + 1))
        ;;
      registered:unrun)
        registered_records=$((registered_records + 1))
        registered_unrun=$((registered_unrun + 1))
        ;;
      *)
        echo "ERROR: scoped result $i has invalid class/outcome '$class:$outcome'" >&2
        failures=$((failures + 1))
        ;;
    esac
  done

  if [ "$core_records" -ne "$CORE_EXPECTED" ] \
    || [ "$core_pass" -ne "$CORE_PASSED" ] \
    || [ "$core_red" -ne "$CORE_RED" ] \
    || [ "$core_unrun" -ne "$CORE_UNRUN" ] \
    || [ $((core_pass + core_red)) -ne "$CORE_EXECUTED" ]; then
    echo "ERROR: scoped core ledger does not match verdict counters: records=$core_records/$CORE_EXPECTED pass=$core_pass/$CORE_PASSED red=$core_red/$CORE_RED unrun=$core_unrun/$CORE_UNRUN executed=$((core_pass + core_red))/$CORE_EXECUTED" >&2
    failures=$((failures + 1))
  fi
  if [ "$registered_records" -ne "$REGISTERED_EXPECTED" ] \
    || [ "$registered_pass" -ne "$REGISTERED_PASSED" ] \
    || [ "$registered_red" -ne "$REGISTERED_RED" ] \
    || [ "$registered_unrun" -ne "$REGISTERED_UNRUN" ] \
    || [ $((registered_pass + registered_red)) -ne "$REGISTERED_EXECUTED" ]; then
    echo "ERROR: scoped registered ledger does not match verdict counters: records=$registered_records/$REGISTERED_EXPECTED pass=$registered_pass/$REGISTERED_PASSED red=$registered_red/$REGISTERED_RED unrun=$registered_unrun/$REGISTERED_UNRUN executed=$((registered_pass + registered_red))/$REGISTERED_EXECUTED" >&2
    failures=$((failures + 1))
  fi
  [ "$failures" -eq 0 ]
}

# Reclassify one previously pass/red child verdict as UNRUN. The original token
# remains in the transcript as evidence of what the assertion reported; the
# appended UNRUN+FAIL says why it is not a claim about one stable input. That is
# the existing third-state contract, now applied to the child whose domain
# actually moved instead of to the whole 35-minute aggregate.
gate_scope_void_result() {
  local i="$1"
  local class="${GATE_SCOPE_CLASS[i]}"
  local kind="${GATE_SCOPE_KIND[i]}"
  local label="${GATE_SCOPE_LABEL[i]}"
  local outcome="${GATE_SCOPE_OUTCOME[i]}"
  local domain="${GATE_SCOPE_DOMAIN[i]}"

  [ "$outcome" = unrun ] && return 0
  case "$class" in
    core)
      CORE_EXECUTED=$((CORE_EXECUTED - 1))
      if [ "$outcome" = pass ]; then
        CORE_PASSED=$((CORE_PASSED - 1))
      else
        CORE_RED=$((CORE_RED - 1))
      fi
      CORE_UNRUN=$((CORE_UNRUN + 1))
      gate_unrun "core: $label — tracked input domain $domain moved during check.sh"
      ;;
    registered)
      REGISTERED_EXECUTED=$((REGISTERED_EXECUTED - 1))
      decrement_registered_kind_executed "$kind"
      if [ "$outcome" = pass ]; then
        REGISTERED_PASSED=$((REGISTERED_PASSED - 1))
      else
        REGISTERED_RED=$((REGISTERED_RED - 1))
      fi
      REGISTERED_UNRUN=$((REGISTERED_UNRUN + 1))
      gate_unrun "registered $kind $label — tracked input domain $domain moved during check.sh"
      ;;
    *)
      GATE_SCOPE_FATAL=1
      gate_unrun "check.sh: unknown recorded gate class $class; verdict cannot be attributed"
      return 1
      ;;
  esac
  GATE_SCOPE_OUTCOME[i]=unrun
  return 0
}

# gate_scope_apply_tree_change <start-listing> <end-listing> <start-head> <end-head>
#
# Attribute one whole-run movement to every recorded child verdict. A HEAD move
# invalidates all domains: gates may consult revision identity without a tracked
# path spelling that dependency, and the landing lease should have prevented it
# in the first place. For a worktree/index move, byte-compare each declared
# tracked domain. Unknown/unreadable declarations fail closed as affected.
gate_scope_apply_tree_change() {
  local start_listing="$1" end_listing="$2" start_head="$3" end_head="$4"
  local i domain outcome subject rc affected affected_count=0 retained_count=0
  local head_moved=0

  [ "$start_head" != "$end_head" ] && head_moved=1
  gate_diag "  SCOPED CHILD VERDICTS:"
  for ((i = 0; i < GATE_SCOPE_COUNT; i++)); do
    domain="${GATE_SCOPE_DOMAIN[i]}"
    outcome="${GATE_SCOPE_OUTCOME[i]}"
    subject="$(gate_scope_subject_name "$i")"
    affected=0
    if [ "$head_moved" -eq 1 ]; then
      affected=1
    elif gate_tree_domain_changed "$start_listing" "$end_listing" "$domain"; then
      affected=1
    else
      rc=$?
      if [ "$rc" -ne 1 ]; then
        # Status 2 is an unknown/unreadable declaration. It is all-tracked in
        # effect, never an empty set that can retain a verdict.
        affected=1
        gate_diag "    DOMAIN ERROR  $subject [$domain] status=$rc; failing closed"
      fi
    fi

    if [ "$affected" -eq 1 ]; then
      affected_count=$((affected_count + 1))
      gate_diag "    VOID  $subject [$domain] prior=$outcome"
      gate_scope_void_result "$i" || true
    else
      retained_count=$((retained_count + 1))
      gate_diag "    KEEP  $subject [$domain] verdict=$outcome"
    fi
  done

  if [ "$GATE_SCOPE_COUNT" -eq 0 ] || [ "$affected_count" -eq 0 ]; then
    GATE_SCOPE_FATAL=1
    gate_unrun "check.sh: tree moved but no recorded gate domain accepted responsibility"
    gate_diag "  A zero affected set is not evidence that movement was irrelevant;"
    gate_diag "  it means the declaration layer failed open."
    return 1
  fi
  gate_diag "  scoped attribution: $affected_count affected, $retained_count retained"
  return 0
}

# Take check.sh's run-level end sample BEFORE printing the aggregate summary.
# After attribution, re-baseline the shared EXIT-tripwire to this sample. That
# leaves the tiny summary/exit window protected: movement after the scoped
# sample still produces the older conservative whole-run UNRUN instead of
# slipping through because GATE_TREE_CHECKED was set early.
gate_scope_finalize_tree_stability() {
  local list_end fp_end head_end unstable=0
  if ! gate_scope_records_complete; then
    GATE_SCOPE_FATAL=1
    unstable=1
    gate_unrun "check.sh: scoped result ledger is incomplete; attribution is unsafe"
  fi
  gate_tree_snapshot_into list_end fp_end
  head_end="$(gate_tree_head)"
  if [ "$fp_end" != "$GATE_TREE_FP_START" ]; then
    unstable=1
    gate_diag "  tree moved during check.sh; attributing the movement by declared child domain:"
    gate_diag "    HEAD at start: $GATE_TREE_HEAD_START"
    gate_diag "    HEAD at end:   $head_end"
    gate_diag "  WHAT MOVED:"
    gate_tree_diff "$GATE_TREE_LIST_START" "$list_end"
    gate_scope_apply_tree_change \
      "$GATE_TREE_LIST_START" "$list_end" \
      "$GATE_TREE_HEAD_START" "$head_end" || true
  fi

  # Protect the remaining summary/exit window with the ordinary tripwire.
  GATE_TREE_LIST_START="$list_end"
  GATE_TREE_FP_START="$fp_end"
  GATE_TREE_HEAD_START="$head_end"
  # shellcheck disable=SC2034 # consumed by gate_on_exit in the sourced library
  GATE_TREE_CHECKED=0
  return "$unstable"
}

# Stop expensive execution after the first completed phase that observes a
# tracked-tree move. The main driver still visits later wrappers so each
# expected core and registered artifact receives an explicit UNRUN; silently
# exiting here would save time by breaking verdict-accounting closure. The final
# scoped attribution remains authoritative (fgdb-3e12).
gate_scope_abort_if_tree_moved() {
  local completed="$1"
  [ "$GATE_SCOPE_TRACKING" -eq 1 ] || return 0
  [ "$GATE_SCOPE_ABORTED" -eq 0 ] || return 0
  if gate_scope_finalize_tree_stability; then
    return 0
  fi
  GATE_SCOPE_ABORTED=1
  gate_unrun "check.sh: tracked tree moved after $completed; remaining gates did not run"
  gate_diag "  Expensive execution stops here; remaining gates are accounted as UNRUN."
  gate_diag "  Re-run from a settled main checkout, or run in a scratch worktree."
  return 1
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
  if [ "$skip_rc" -ne "$GATE_EXIT_UNRUN" ]; then
    echo "SELF-TEST RED: a gate that executed no assertions exited $skip_rc, expected UNRUN exit $GATE_EXIT_UNRUN" >&2
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
  if [ "$unrun_rc" -ne "$GATE_EXIT_UNRUN" ]; then
    echo "SELF-TEST RED: a gate with one UNRUN check exited $unrun_rc, expected $GATE_EXIT_UNRUN" >&2
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

tree_scope_beads_fixture() (
  local work="$1"
  local head start_listing end_listing
  head="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  start_listing="$head
0000000000000000000000000000000000000000000000000000000000000000  .beads/issues.jsonl
1111111111111111111111111111111111111111111111111111111111111111  Cargo.toml
2222222222222222222222222222222222222222222222222222222222222222  scripts/check.sh
3333333333333333333333333333333333333333333333333333333333333333  scripts/lib/gate_verdict.sh
4444444444444444444444444444444444444444444444444444444444444444  tools/registry-check/src/lib.rs"
  end_listing="$head
9999999999999999999999999999999999999999999999999999999999999999  .beads/issues.jsonl
1111111111111111111111111111111111111111111111111111111111111111  Cargo.toml
2222222222222222222222222222222222222222222222222222222222222222  scripts/check.sh
3333333333333333333333333333333333333333333333333333333333333333  scripts/lib/gate_verdict.sh
4444444444444444444444444444444444444444444444444444444444444444  tools/registry-check/src/lib.rs"

  gate_scope_reset
  CORE_EXPECTED=3
  CORE_EXECUTED=3
  CORE_PASSED=2
  CORE_RED=1
  CORE_UNRUN=0
  gate_scope_record core "" "fmt fixture" pass rust-format
  gate_scope_record core "" "UBS fixture" red tracked-rust
  gate_scope_record core "" "cargo-test fixture" pass all-tracked
  gate_scope_apply_tree_change \
    "$start_listing" "$end_listing" "$head" "$head" \
    >"$work/scope-beads.out" 2>"$work/scope-beads.err" || return 1

  [ "$CORE_EXECUTED" -eq 2 ] \
    && [ "$CORE_PASSED" -eq 1 ] \
    && [ "$CORE_RED" -eq 1 ] \
    && [ "$CORE_UNRUN" -eq 1 ] \
    && [ "${GATE_SCOPE_OUTCOME[0]}" = pass ] \
    && [ "${GATE_SCOPE_OUTCOME[1]}" = red ] \
    && [ "${GATE_SCOPE_OUTCOME[2]}" = unrun ] \
    && [ "$(grep -c '^UNRUN core: cargo-test fixture' "$work/scope-beads.out")" -eq 1 ] \
    && ! grep -q '^UNRUN core: \(fmt\|UBS\) fixture' "$work/scope-beads.out"
)

tree_scope_rust_fixture() (
  local work="$1"
  local head start_listing end_listing
  head="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
  start_listing="$head
0000000000000000000000000000000000000000000000000000000000000000  .beads/issues.jsonl
1111111111111111111111111111111111111111111111111111111111111111  Cargo.toml
2222222222222222222222222222222222222222222222222222222222222222  scripts/check.sh
3333333333333333333333333333333333333333333333333333333333333333  scripts/lib/gate_verdict.sh
4444444444444444444444444444444444444444444444444444444444444444  tools/registry-check/src/lib.rs"
  end_listing="$head
0000000000000000000000000000000000000000000000000000000000000000  .beads/issues.jsonl
1111111111111111111111111111111111111111111111111111111111111111  Cargo.toml
2222222222222222222222222222222222222222222222222222222222222222  scripts/check.sh
3333333333333333333333333333333333333333333333333333333333333333  scripts/lib/gate_verdict.sh
5555555555555555555555555555555555555555555555555555555555555555  tools/registry-check/src/lib.rs"

  gate_scope_reset
  CORE_EXPECTED=3
  CORE_EXECUTED=3
  CORE_PASSED=2
  CORE_RED=1
  CORE_UNRUN=0
  gate_scope_record core "" "fmt fixture" pass rust-format
  gate_scope_record core "" "UBS fixture" red tracked-rust
  gate_scope_record core "" "cargo-test fixture" pass all-tracked
  gate_scope_apply_tree_change \
    "$start_listing" "$end_listing" "$head" "$head" \
    >"$work/scope-rust.out" 2>"$work/scope-rust.err" || return 1

  [ "$CORE_EXECUTED" -eq 0 ] \
    && [ "$CORE_PASSED" -eq 0 ] \
    && [ "$CORE_RED" -eq 0 ] \
    && [ "$CORE_UNRUN" -eq 3 ] \
    && [ "$(grep -c '^UNRUN core:' "$work/scope-rust.out")" -eq 3 ]
)

tree_scope_shell_fixture() (
  local work="$1"
  local head start_listing end_listing
  head="cccccccccccccccccccccccccccccccccccccccc"
  start_listing="$head
0000000000000000000000000000000000000000000000000000000000000000  .beads/issues.jsonl
1111111111111111111111111111111111111111111111111111111111111111  Cargo.toml
2222222222222222222222222222222222222222222222222222222222222222  registries/checker_index.toml
3333333333333333333333333333333333333333333333333333333333333333  scripts/check.sh
4444444444444444444444444444444444444444444444444444444444444444  scripts/lib/gate_verdict.sh
5555555555555555555555555555555555555555555555555555555555555555  tools/gate.sh
6666666666666666666666666666666666666666666666666666666666666666  tools/registry-check/src/lib.rs"
  end_listing="$head
0000000000000000000000000000000000000000000000000000000000000000  .beads/issues.jsonl
1111111111111111111111111111111111111111111111111111111111111111  Cargo.toml
2222222222222222222222222222222222222222222222222222222222222222  registries/checker_index.toml
3333333333333333333333333333333333333333333333333333333333333333  scripts/check.sh
4444444444444444444444444444444444444444444444444444444444444444  scripts/lib/gate_verdict.sh
7777777777777777777777777777777777777777777777777777777777777777  tools/gate.sh
6666666666666666666666666666666666666666666666666666666666666666  tools/registry-check/src/lib.rs"

  gate_scope_reset
  CORE_EXPECTED=6
  CORE_EXECUTED=6
  CORE_PASSED=5
  CORE_RED=1
  CORE_UNRUN=0
  gate_scope_record core "" "UBS fixture" red tracked-rust
  gate_scope_record core "" "fmt fixture" pass rust-format
  gate_scope_record core "" "shell-lint fixture" pass tracked-shell
  gate_scope_record core "" "verdict-contract fixture" pass verdict-shell
  gate_scope_record core "" "domain-closure fixture" pass domain-closure
  gate_scope_record core "" "cargo-test fixture" pass all-tracked
  gate_scope_apply_tree_change \
    "$start_listing" "$end_listing" "$head" "$head" \
    >"$work/scope-shell.out" 2>"$work/scope-shell.err" || return 1

  [ "$CORE_EXECUTED" -eq 3 ] \
    && [ "$CORE_PASSED" -eq 2 ] \
    && [ "$CORE_RED" -eq 1 ] \
    && [ "$CORE_UNRUN" -eq 3 ] \
    && [ "${GATE_SCOPE_OUTCOME[0]}" = red ] \
    && [ "${GATE_SCOPE_OUTCOME[1]}" = pass ] \
    && [ "${GATE_SCOPE_OUTCOME[2]}" = unrun ] \
    && [ "${GATE_SCOPE_OUTCOME[3]}" = unrun ] \
    && [ "${GATE_SCOPE_OUTCOME[4]}" = pass ] \
    && [ "${GATE_SCOPE_OUTCOME[5]}" = unrun ]
)

tree_scope_gate_source_fixture() {
  local head start_listing end_listing domain
  head="dddddddddddddddddddddddddddddddddddddddd"
  start_listing="$head
1111111111111111111111111111111111111111111111111111111111111111  scripts/check.sh
2222222222222222222222222222222222222222222222222222222222222222  scripts/lib/gate_verdict.sh"
  end_listing="$head
3333333333333333333333333333333333333333333333333333333333333333  scripts/check.sh
2222222222222222222222222222222222222222222222222222222222222222  scripts/lib/gate_verdict.sh"
  for domain in all-tracked tracked-rust tracked-shell rust-format \
      verdict-shell domain-closure; do
    if ! gate_tree_domain_changed "$start_listing" "$end_listing" "$domain"; then
      return 1
    fi
  done
  return 0
}

tree_listing_option_path_fixture() {
  local work="$1" first_listing second_listing third_listing
  local first_fp second_fp third_fp
  mkdir -p "$work" || return 1
  (
    cd "$work" || exit 1
    git init -q || exit 1
    git config user.email gate@example.invalid || exit 1
    git config user.name fgdb-gate || exit 1
    git config commit.gpgsign false || exit 1
    printf 'first tracked bytes\n' > ./--help
    printf 'first stdin-shaped bytes\n' > ./-
    git add -- --help - || exit 1
    git commit -qm 'fixture: option-shaped tracked paths' || exit 1
    first_listing="$(gate_tree_listing)" || exit 1
    printf 'second tracked bytes\n' > ./--help
    second_listing="$(gate_tree_listing)" || exit 1
    printf 'second stdin-shaped bytes\n' > ./-
    third_listing="$(gate_tree_listing)" || exit 1
    first_fp="$(gate_tree_fp_of "$first_listing")"
    second_fp="$(gate_tree_fp_of "$second_listing")"
    third_fp="$(gate_tree_fp_of "$third_listing")"
    [ "$first_fp" != "$second_fp" ] \
      && [ "$second_fp" != "$third_fp" ] \
      && printf '%s\n' "$first_listing" | grep -q '  --help$' \
      && printf '%s\n' "$second_listing" | grep -q '  --help$' \
      && printf '%s\n' "$third_listing" | grep -q '  -$'
  )
}

tree_listing_hash_failure_fixture() {
  local work="$1" listing rc=0
  mkdir -p "$work/bin" || return 1
  {
    printf '#!/usr/bin/env bash\n'
    printf 'printf "injected sha256sum failure\\n" >&2\n'
    printf 'exit 17\n'
  } >"$work/bin/sha256sum" || return 1
  chmod +x "$work/bin/sha256sum" || return 1
  (
    cd "$ROOT" || exit 1
    PATH="$work/bin:$PATH" gate_tree_listing
  ) >"$work/listing.out" 2>"$work/listing.err" || rc=$?
  listing="$(<"$work/listing.out")"
  [ "$rc" -ne 0 ] \
    && printf '%s\n' "$listing" | grep -Fq 'injected sha256sum failure'
}

# Prove the checkpoint wrapper's control flow without racing the live worktree.
# The finalizer is the authority for stable versus moved; this fixture supplies
# both outcomes and asserts that moved skips expensive work while retaining the
# lightweight downstream accounting pass.
tree_scope_early_abort_fixture() {
  local work="$1" checkpoint_fn=gate_scope_abort_if_tree_moved rc=0
  mkdir -p "$work" || return 1

  (
    GATE_SCOPE_TRACKING=1
    GATE_SCOPE_ABORTED=0
    GATE_EXIT_UNRUN=2
    gate_scope_finalize_tree_stability() { return 1; }
    gate_unrun() { printf 'UNRUN %s\nFAIL %s\n' "$1" "$1"; }
    gate_diag() { :; }
    "$checkpoint_fn" "fixture boundary" || rc=$?
    if [ "$GATE_SCOPE_ABORTED" -eq 1 ]; then
      printf 'ACCOUNTED LATER WORK AS UNRUN\n'
    else
      printf 'REACHED EXPENSIVE LATER WORK\n'
    fi
    exit "$rc"
  ) >"$work/moved.out" 2>"$work/moved.err" || rc=$?
  [ "$rc" -eq 1 ] || return 1
  grep -Fq 'UNRUN check.sh: tracked tree moved after fixture boundary' \
    "$work/moved.out" || return 1
  grep -Fq 'ACCOUNTED LATER WORK AS UNRUN' "$work/moved.out" || return 1
  ! grep -Fq 'REACHED EXPENSIVE LATER WORK' "$work/moved.out" || return 1

  (
    GATE_SCOPE_TRACKING=1
    GATE_SCOPE_ABORTED=0
    gate_scope_finalize_tree_stability() { return 0; }
    gate_unrun() { printf 'UNEXPECTED UNRUN %s\n' "$1"; }
    gate_diag() { :; }
    "$checkpoint_fn" "stable fixture boundary" || exit 1
    if [ "$GATE_SCOPE_ABORTED" -eq 0 ]; then
      printf 'REACHED EXPENSIVE LATER WORK\n'
    fi
  ) >"$work/stable.out" 2>"$work/stable.err" || return 1
  grep -Fq 'REACHED EXPENSIVE LATER WORK' "$work/stable.out" \
    && ! grep -Fq 'UNEXPECTED UNRUN' "$work/stable.out"
}

tree_scope_abort_accounting_fixture() {
  local work="$1" core_marker="$1/core-executed" registered_marker="$1/registered-executed"
  local registry="$1/checker-index.toml" root="$1/root"
  mkdir -p "$root/scripts" || return 1
  {
    printf '[[checker]]\n'
    printf 'symbol = "abort_accounting_fixture"\n'
    printf 'kind = "script"\n'
    printf 'artifact = "scripts/should-not-run.sh"\n'
    printf 'status = "live"\n'
  } >"$registry" || return 1
  {
    printf '#!/usr/bin/env bash\n'
    printf ': > "$ABORT_ACCOUNTING_REGISTERED_MARKER"\n'
  } >"$root/scripts/should-not-run.sh" || return 1
  chmod +x "$root/scripts/should-not-run.sh" || return 1

  (
    abort_accounting_core() { : >"$core_marker"; }
    CORE_EXPECTED=0
    CORE_EXECUTED=0
    CORE_PASSED=0
    CORE_RED=0
    CORE_UNRUN=0
    reset_registered_counters
    gate_scope_reset
    GATE_SCOPE_TRACKING=1
    GATE_SCOPE_ABORTED=1
    export ABORT_ACCOUNTING_REGISTERED_MARKER="$registered_marker"
    run_core_gate "$CORE_GATE_FMT" abort_accounting_core
    run_registered_gates "$root" "$registry" "$GATE_EXIT_UNRUN" /dev/null
    [ "$CORE_EXPECTED" -eq 1 ] \
      && [ "$CORE_EXECUTED" -eq 0 ] \
      && [ "$CORE_UNRUN" -eq 1 ] \
      && [ "$REGISTERED_EXPECTED" -eq 1 ] \
      && [ "$REGISTERED_EXECUTED" -eq 0 ] \
      && [ "$REGISTERED_UNRUN" -eq 1 ] \
      && [ "$GATE_SCOPE_COUNT" -eq 2 ] \
      && gate_scope_records_complete
  ) >"$work/accounting.out" 2>"$work/accounting.err" || return 1
  [ ! -e "$core_marker" ] && [ ! -e "$registered_marker" ]
}

landing_guidance_complete() {
  local source="$1"
  [ -r "$source" ] \
    && grep -Fq 'DO NOT EDIT TRACKED FILES IN THE MAIN' "$source" \
    && grep -Fq 'voids the in-flight run even before commit' "$source" \
    && grep -Fq 'in your own scratch worktree' "$source"
}

landing_guidance_control() {
  local work="$1" source mutant
  source="$ROOT/scripts/git_hooks/pre-commit.sh"
  mutant="$work/pre-commit-no-main-edit-warning.sh"
  landing_guidance_complete "$source" || return 1
  sed '/DO NOT EDIT TRACKED FILES IN THE MAIN/d' "$source" >"$mutant" \
    || return 1
  ! landing_guidance_complete "$mutant"
}

tree_scope_control() {
  local work="$1"
  local closure_listing _closure_fp mutant_source ledger_mutant_source
  local checkpoint_mutant_source mutated_abort mutation_count

  gate_tree_snapshot_into closure_listing _closure_fp
  GATE_TREE_LIST_START="$closure_listing"
  if ! run_gate_domain_closure >"$work/domain-closure.out" 2>"$work/domain-closure.err"; then
    echo "SELF-TEST RED: the live gate-domain declaration closure is not satisfied" >&2
    return 1
  fi
  # Remove the live aggregate-attribution call from a scratch copy. The
  # declaration checker must reject that copy, proving the closure guards the
  # main-path wiring rather than only validating helpers which might never run.
  mutant_source="$work/check-no-scope-finalize.sh"
  if ! sed '/^gate_scope_finalize_tree_stability$/d' \
      "$ROOT/scripts/check.sh" >"$mutant_source"; then
    echo "SELF-TEST RED: could not construct the missing-finalizer control" >&2
    return 1
  fi
  if grep -qx 'gate_scope_finalize_tree_stability' "$mutant_source"; then
    echo "SELF-TEST RED: the missing-finalizer control did not apply" >&2
    return 1
  fi
  if gate_domain_wiring_complete "$mutant_source" \
      >"$work/domain-wiring-mutant.out" 2>"$work/domain-wiring-mutant.err"; then
    echo "SELF-TEST RED: deleting live scoped final attribution left closure green" >&2
    return 1
  fi
  ledger_mutant_source="$work/check-no-ledger-closure.sh"
  if ! sed 's/^  if ! gate_scope_records_complete; then$/  if false; then/' \
      "$ROOT/scripts/check.sh" >"$ledger_mutant_source"; then
    echo "SELF-TEST RED: could not construct the missing-ledger-closure control" >&2
    return 1
  fi
  if gate_domain_wiring_complete "$ledger_mutant_source" \
      >"$work/ledger-wiring-mutant.out" 2>"$work/ledger-wiring-mutant.err"; then
    echo "SELF-TEST RED: deleting live scoped ledger closure left wiring green" >&2
    return 1
  fi
  checkpoint_mutant_source="$work/check-no-early-checkpoints.sh"
  if ! sed '/^[[:space:]]*gate_scope_abort_if_tree_moved /d' \
      "$ROOT/scripts/check.sh" >"$checkpoint_mutant_source"; then
    echo "SELF-TEST RED: could not construct the missing-checkpoints control" >&2
    return 1
  fi
  if grep -q '^[[:space:]]*gate_scope_abort_if_tree_moved ' "$checkpoint_mutant_source"; then
    echo "SELF-TEST RED: the missing-checkpoints control did not apply" >&2
    return 1
  fi
  if gate_domain_wiring_complete "$checkpoint_mutant_source" \
      >"$work/checkpoint-wiring-mutant.out" 2>"$work/checkpoint-wiring-mutant.err"; then
    echo "SELF-TEST RED: deleting every early tree checkpoint left wiring green" >&2
    return 1
  fi
  if ! tree_scope_early_abort_fixture "$work/early-abort"; then
    echo "SELF-TEST RED: the early tree checkpoint did not stop moved input or retain stable input" >&2
    return 1
  fi
  if ! tree_scope_abort_accounting_fixture "$work/abort-accounting"; then
    echo "SELF-TEST RED: early abort did not account downstream core and registered gates as UNRUN" >&2
    return 1
  fi
  mutated_abort="$(declare -f gate_scope_abort_if_tree_moved \
    | sed 's/GATE_SCOPE_ABORTED=1/GATE_SCOPE_ABORTED=0/')"
  mutation_count="$(printf '%s\n' "$mutated_abort" | grep -c 'GATE_SCOPE_ABORTED=0')"
  if [ "$mutation_count" -ne 1 ] \
      || printf '%s\n' "$mutated_abort" | grep -q 'GATE_SCOPE_ABORTED=1'; then
    echo "SELF-TEST RED: could not construct the early-abort flag mutation" >&2
    return 1
  fi
  if (
    eval "$mutated_abort"
    tree_scope_early_abort_fixture "$work/early-abort-mutant"
  ); then
    echo "SELF-TEST RED: disabling the checkpoint abort flag left its control green" >&2
    return 1
  fi
  # One emitted core verdict with no ledger record must fail cardinality
  # closure. This is the exact omission that would otherwise escape attribution.
  if (
    gate_scope_reset
    reset_registered_counters
    CORE_EXPECTED=1
    CORE_EXECUTED=1
    CORE_PASSED=1
    CORE_RED=0
    CORE_UNRUN=0
    gate_scope_records_complete
  ) >"$work/ledger-cardinality-mutant.out" \
      2>"$work/ledger-cardinality-mutant.err"; then
    echo "SELF-TEST RED: an unrecorded child verdict left ledger closure green" >&2
    return 1
  fi
  # Delete one core declaration semantically by overriding its reader. The
  # closure must go red; otherwise its completeness claim is decorative.
  if (
    eval "$(declare -f core_gate_domain \
      | sed '1s/^core_gate_domain /core_gate_domain_original /')"
    core_gate_domain() {
      if [ "$1" = "$CORE_GATE_UBS" ]; then return 1; fi
      core_gate_domain_original "$@"
    }
    run_gate_domain_closure
  ) >"$work/domain-closure-mutant.out" 2>"$work/domain-closure-mutant.err"; then
    echo "SELF-TEST RED: deleting the UBS domain declaration left closure green" >&2
    return 1
  fi

  if ! tree_scope_beads_fixture "$work"; then
    echo "SELF-TEST RED: a Beads-only move did not retain Rust-domain verdicts and void all-tracked" >&2
    return 1
  fi
  if ! tree_scope_rust_fixture "$work"; then
    echo "SELF-TEST RED: a Rust-source move did not void every Rust-reading verdict" >&2
    return 1
  fi
  if ! tree_scope_shell_fixture "$work"; then
    echo "SELF-TEST RED: a shell-gate move did not separate shell and Rust domains" >&2
    return 1
  fi
  if ! tree_scope_gate_source_fixture; then
    echo "SELF-TEST RED: moving check.sh did not invalidate every declared domain" >&2
    return 1
  fi
  if ! tree_listing_option_path_fixture "$work/option-path"; then
    echo "SELF-TEST RED: an option-shaped tracked path was not content-fingerprinted" >&2
    return 1
  fi
  if ! tree_listing_hash_failure_fixture "$work/hash-failure"; then
    echo "SELF-TEST RED: a content-hash failure was swallowed by tree listing" >&2
    return 1
  fi
  if (
    gate_tree_listing() {
      git rev-parse HEAD 2>&1
      git ls-files -z 2>/dev/null | xargs -0 sha256sum 2>&1
    }
    tree_listing_option_path_fixture "$work/option-path-mutant"
  ); then
    echo "SELF-TEST RED: removing sha256sum option termination left option-path control green" >&2
    return 1
  fi
  # Neuter the intersection predicate. The Rust fixture must stop passing,
  # proving its three UNRUNs came from domain attribution rather than from an
  # unrelated counter or token.
  mkdir -p "$work/mutant" || return 1
  if (
    gate_tree_domain_changed() { return 1; }
    tree_scope_rust_fixture "$work/mutant"
  ) >/dev/null 2>&1; then
    echo "SELF-TEST RED: neutering domain intersection did not break the Rust mutation control" >&2
    return 1
  fi
  return 0
}

run_mutation_self_test() {
  local work
  local fixture_root
  local failing_log
  local unrun_log
  local child_state_log

  work="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-check-self-test.XXXXXX")"
  fixture_root="$work/root"
  mkdir -p "$fixture_root/registries" "$fixture_root/scripts/lib" \
    "$fixture_root/tools"
  cp "$GATE_VERDICT_LIB" "$fixture_root/scripts/lib/gate_verdict.sh" \
    || return 1
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

  # A registered child has a three-state contract of its own. Exit 2 alone is
  # not enough — several scripts use it for usage/build errors — and UNRUN text
  # alone is not enough either. Only a dedicated exit plus exactly paired
  # UNRUN+FAIL stdout lines may propagate as UNRUN. A standalone FAIL dominates
  # and stays RED.
  cat >"$fixture_root/scripts/child_unrun.sh" <<'EOF'
#!/usr/bin/env bash
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_init "child_unrun"
gate_pass "the precondition probe ran"
gate_abort_unrun "a required Cargo artifact disappeared"
EOF
  cat >"$fixture_root/scripts/child_bare_exit2.sh" <<'EOF'
#!/usr/bin/env bash
echo "usage/build error" >&2
exit 2
EOF
  cat >"$fixture_root/scripts/child_mixed.sh" <<'EOF'
#!/usr/bin/env bash
printf 'UNRUN a required Cargo artifact disappeared\n'
printf 'FAIL a required Cargo artifact disappeared\n'
printf 'FAIL a real assertion also failed\n'
exit 2
EOF
  cat >"$fixture_root/scripts/child_wrong_exit.sh" <<'EOF'
#!/usr/bin/env bash
printf 'UNRUN a required Cargo artifact disappeared\n'
printf 'FAIL a required Cargo artifact disappeared\n'
exit 1
EOF
  cat >"$fixture_root/scripts/child_stderr_unrun.sh" <<'EOF'
#!/usr/bin/env bash
un=UN
fa=FA
printf '%s %s\n' "${un}RUN" 'a required Cargo artifact disappeared' >&2
printf '%s %s\n' "${fa}IL" 'a required Cargo artifact disappeared' >&2
exit 2
EOF
  cat >"$fixture_root/registries/child_states.toml" <<'EOF'
[[checker]]
symbol = "mutation_child_unrun"
kind = "script"
artifact = "scripts/child_unrun.sh"
status = "live"

[[checker]]
symbol = "mutation_child_bare_exit2"
kind = "script"
artifact = "scripts/child_bare_exit2.sh"
status = "live"

[[checker]]
symbol = "mutation_child_mixed"
kind = "script"
artifact = "scripts/child_mixed.sh"
status = "live"

[[checker]]
symbol = "mutation_child_wrong_exit"
kind = "script"
artifact = "scripts/child_wrong_exit.sh"
status = "live"

[[checker]]
symbol = "mutation_child_stderr_unrun"
kind = "script"
artifact = "scripts/child_stderr_unrun.sh"
status = "live"
EOF
  child_state_log="$work/registered-child-states.log"
  if (
    reset_registered_counters
    GATE_LOG_DIR="$work/registered-child-gate-logs"
    mkdir -p "$GATE_LOG_DIR"
    run_registered_gates \
      "$fixture_root" "$fixture_root/registries/child_states.toml" 0
    print_registered_summary
  ) >"$child_state_log" 2>&1; then
    echo "SELF-TEST RED: registered child-state controls produced a green exit" >&2
    return 1
  fi
  if ! grep -Fq "UNRUN registered script scripts/child_unrun.sh" \
      "$child_state_log"; then
    echo "SELF-TEST RED: a conforming child UNRUN was collapsed into RED" >&2
    return 1
  fi
  local malformed_child
  for malformed_child in \
    child_bare_exit2.sh child_mixed.sh child_wrong_exit.sh \
    child_stderr_unrun.sh; do
    if ! grep -Fq "RED registered script scripts/$malformed_child" \
        "$child_state_log"; then
      echo "SELF-TEST RED: malformed child $malformed_child was not RED" >&2
      return 1
    fi
    if grep -Fq "UNRUN registered script scripts/$malformed_child" \
        "$child_state_log"; then
      echo "SELF-TEST RED: malformed child $malformed_child borrowed UNRUN" >&2
      return 1
    fi
  done

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

  # Shared-target retry controls (fgdb-a9tg). A fake Cargo command gives the
  # runner three deterministic histories:
  #   race -> pass        exactly one visible retry, final evidence selected
  #   race + real red     no retry; the real failure dominates
  #   race -> race        one retry only; the second race remains red
  #   race + exit 130     no retry; only Cargo's measured exit 101 qualifies
  # The first and third histories distinguish "retry exists" from an unbounded
  # loop, while the second prevents a flaky real test from borrowing the retry.
  mkdir -p "$work/fake-cargo"
  cat >"$work/fake-cargo/cargo" <<'EOF'
#!/usr/bin/env bash
set -u
attempt=0
if [ -f "$FGDB_RETRY_COUNTER" ]; then
  read -r attempt <"$FGDB_RETRY_COUNTER"
fi
attempt=$((attempt + 1))
printf '%s\n' "$attempt" >"$FGDB_RETRY_COUNTER"

race() {
  cat >&2 <<'RACE'
error: test failed, to rerun pass `-p demo --test vanished`
Caused by:
  could not execute process `/data/tmp/cargo-target/debug/deps/vanished-1234` (never executed)
Caused by:
  No such file or directory (os error 2)
RACE
}

case "$FGDB_RETRY_MODE:$attempt" in
  race_then_pass:1 | race_twice:1 | race_twice:2 | race_wrong_exit:1)
    race
    if [ "$FGDB_RETRY_MODE" = "race_wrong_exit" ]; then
      exit 130
    fi
    exit 101
    ;;
  race_with_real_failure:1)
    race
    printf '%s\n' \
      'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured' >&2
    exit 101
    ;;
  *)
    cat <<'PASS'
     Running tests/ran_ok.rs (/t/deps/ran_ok-1111111111111111)
running 1 test
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
PASS
    exit 0
    ;;
esac
EOF
  chmod +x "$work/fake-cargo/cargo"

  cargo_retry_case() { # mode expected-rc expected-attempts label
    local mode="$1" expected_rc="$2" expected_attempts="$3" label="$4"
    local case_root="$work/retry-$mode"
    local output="$case_root/output.log"
    local selected="$case_root/selected.log"
    local counter="$case_root/attempts"
    local rc=0
    mkdir -p "$case_root"
    if (
      PATH="$work/fake-cargo:$PATH"
      export PATH
      FGDB_RETRY_MODE="$mode"
      FGDB_RETRY_COUNTER="$counter"
      export FGDB_RETRY_MODE FGDB_RETRY_COUNTER
      CARGO_TEST_LOG="$case_root/core-cargo-test.log"
      run_cargo_test_workspace || rc=$?
      printf '%s\n' "$CARGO_TEST_LOG" >"$selected"
      exit "$rc"
    ) >"$output" 2>&1; then
      rc=0
    else
      rc=$?
    fi
    if [ "$rc" -ne "$expected_rc" ]; then
      echo "SELF-TEST RED: $label exited $rc, expected $expected_rc" >&2
      return 1
    fi
    if [ "$(cat "$counter")" -ne "$expected_attempts" ]; then
      echo "SELF-TEST RED: $label ran an unexpected number of attempts" >&2
      return 1
    fi
    case "$mode" in
      race_then_pass)
        if ! grep -Fq "cargo-test retry 1/1" "$output" ||
            ! grep -Fq ".retry-1.log" "$selected" ||
            ! grep -Fq "test result: ok." "$(cat "$selected")"; then
          echo "SELF-TEST RED: the passing retry was not visible and attributable" >&2
          return 1
        fi
        ;;
      race_with_real_failure)
        if grep -Fq "cargo-test retry 1/1" "$output"; then
          echo "SELF-TEST RED: a real test failure borrowed the ENOENT retry" >&2
          return 1
        fi
        ;;
      race_wrong_exit)
        if grep -Fq "cargo-test retry 1/1" "$output"; then
          echo "SELF-TEST RED: a non-Cargo failure exit borrowed the ENOENT retry" >&2
          return 1
        fi
        ;;
      race_twice)
        if [ "$(grep -Fc "cargo-test retry 1/1" "$output")" -ne 1 ]; then
          echo "SELF-TEST RED: the retry cap was not exactly one" >&2
          return 1
        fi
        ;;
    esac
  }
  cargo_retry_case race_then_pass 0 2 \
    "a pure missing-binary race followed by pass" || return 1
  cargo_retry_case race_with_real_failure 101 1 \
    "a missing-binary race beside a real test failure" || return 1
  cargo_retry_case race_twice 101 2 \
    "a repeated missing-binary race" || return 1
  cargo_retry_case race_wrong_exit 130 1 \
    "an ENOENT transcript beside a non-Cargo exit" || return 1

  verdict_stream_control "$work" || return 1
  verdict_contract_control "$work" || return 1
  tree_scope_control "$work" || return 1
  catalog_lane_test_scope_control || return 1
  landing_guidance_control "$work" || return 1

  echo "CHECK.SH MUTATION SELF-TEST PASS"
  echo "  failing registered gate: RED"
  echo "  registered child outcomes: paired stdout + exit-2 UNRUN propagated;"
  echo "    bare, mixed, wrong-exit, and stderr-only evidence stayed RED"
  echo "  registered gate without a runner: UNRUN and nonzero"
  echo "  cargo-test attribution: pass / red / unrun / ambiguous all separated"
  echo "  unrun cargo-test artifact: UNRUN and nonzero"
  echo "  shared-target cargo race: one visible retry only; real failures dominate;"
  echo "    a repeated ENOENT remains red and final attribution uses the retry log"
  echo "  reporting contract: every verdict on stdout, under both ^RED and ^FAIL,"
  echo "    and absent from a green run's stdout (streams captured separately)"
  echo "  verdict contract: conformant gate silent; L1/L2/L3/L4 rogues each named;"
  echo "    unreadable gate fails closed; a set -e abort still emits ^FAIL"
  echo "  three states: a gate executing no assertions reports UNRUN + FAIL, never"
  echo "    PASS, and exits nonzero; gate_unrun emits both tokens exactly once"
  echo "  tree-domain scoping: Beads, Rust, shell, and gate-driver movements separate"
  echo "    correctly; missing declarations, live wiring, ledger records, and the"
  echo "    intersection each red under mutation; option-shaped paths stay hashed;"
  echo "    early checkpoints stop on UNRUN"
  echo "  catalog test scoping: only a nonempty registry-check/catalog change set that"
  echo "    excludes the crate-bound logical-object registry skips the workspace test"
  echo "  landing guidance: main-checkout edits warn before commit; scratch remedy pinned"
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
select_cargo_test_mode
# set -uo pipefail has no -e: an mktemp failure would leave this empty and
# cascade into gate logs at "/core-ubs.log" — red, but unnamed and confusing.
GATE_LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-check-gates.XXXXXX")" || {
  echo "QUALITY GATE RED: cannot create the gate log directory under ${TMPDIR:-/tmp}" >&2
  exit 1
}
echo "gate logs: $GATE_LOG_DIR"

# Hold the landing lease for this run (fgdb-eesn). gate_init acquires it and the
# EXIT trap gives it back, so the window is exactly this process's lifetime.
#
# WHY THE WHOLE RUN AND NOT JUST THE CARGO PHASES. The two-clocks exposure spans
# `cargo check`/`clippy` (which BAKE pins in at compile time via include_str!)
# through `cargo test` (which READS the corpus at run time) and on through
# run_registered_gates, because g0_identity_e2e reads .beads at run time too.
# That is everything below except the three fast closure gates, so leasing only
# the cargo block would block landings for nearly the same wall-clock while
# protecting strictly less. Measured from the run order, not assumed.
#
# INERT WITHOUT THE HOOK. This only takes a token; nothing enforces it until
# scripts/git_hooks/install.sh has been run. And a lease is broken only by a
# failed liveness test, never by a clock, so if this process dies the lease
# becomes breakable immediately rather than stranding every other pane.
export FGDB_LANDING_LEASE=1
export FGDB_LANDING_NAME="${FGDB_LANDING_NAME:-check.sh-$$}"

# Installed here, not at load: --self-test runs fixtures in subshells, and a
# bash subshell inherits and fires the parent's EXIT trap.
gate_init "check.sh"
GATE_SCOPE_TRACKING=1

run_core_gate "$CORE_GATE_FILE_COVERAGE" run_file_coverage
gate_scope_abort_if_tree_moved "$CORE_GATE_FILE_COVERAGE"
run_core_gate "$CORE_GATE_SHELL_LINT" run_shell_lint
gate_scope_abort_if_tree_moved "$CORE_GATE_SHELL_LINT"
run_core_gate "$CORE_GATE_VERDICT_CONTRACT" run_verdict_contract
gate_scope_abort_if_tree_moved "$CORE_GATE_VERDICT_CONTRACT"
run_core_gate "$CORE_GATE_DOMAIN_CLOSURE" run_gate_domain_closure
gate_scope_abort_if_tree_moved "$CORE_GATE_DOMAIN_CLOSURE"
run_core_gate "$CORE_GATE_FMT" cargo fmt --check
gate_scope_abort_if_tree_moved "$CORE_GATE_FMT"
run_core_gate "$CORE_GATE_CHECK" cargo check --all-targets
gate_scope_abort_if_tree_moved "$CORE_GATE_CHECK"
run_core_gate "$CORE_GATE_CLIPPY" \
  cargo clippy --all-targets -- -D warnings
gate_scope_abort_if_tree_moved "$CORE_GATE_CLIPPY"
CARGO_TEST_LOG="$GATE_LOG_DIR/core-cargo-test.log"
run_core_gate "$CORE_GATE_TEST" run_cargo_test_workspace
CARGO_TEST_RC="$LAST_GATE_RC"
gate_scope_abort_if_tree_moved "$CORE_GATE_TEST"
run_core_gate "$CORE_GATE_UBS" run_ubs
gate_scope_abort_if_tree_moved "$CORE_GATE_UBS"

run_registered_gates "$ROOT" "$CHECKER_INDEX" "$CARGO_TEST_RC"
gate_scope_abort_if_tree_moved "registered gate inventory"

# The aggregate sample comes after every child gate, but before the summary so
# affected verdicts can be reclassified as UNRUN while disjoint real reds stay
# authoritative. The ordinary EXIT tripwire is re-baselined inside this call and
# still protects the remaining summary/exit window.
gate_scope_finalize_tree_stability

echo
echo "CORE GATES: $CORE_EXECUTED of $CORE_EXPECTED executed; $CORE_PASSED passed; $CORE_RED red; $CORE_UNRUN unrun"
REGISTERED_SUMMARY_RC=0
print_registered_summary || REGISTERED_SUMMARY_RC=$?
if [ "$CORE_RED" -ne 0 ] || [ "$CORE_UNRUN" -ne 0 ] \
  || [ "$CORE_EXECUTED" -ne "$CORE_EXPECTED" ] \
  || [ "$REGISTERED_SUMMARY_RC" -ne 0 ] || [ "$GATE_SCOPE_FATAL" -ne 0 ]; then
  # The overall verdict goes to BOTH streams: it is the one line that must
  # reach a reader who captured only one of them. Every per-gate verdict above
  # is on stdout, so the stdout transcript is complete on its own.
  echo "QUALITY GATE RED"
  echo "QUALITY GATE RED" >&2
  exit 1
fi

echo "ALL GATES GREEN — core $CORE_EXECUTED/$CORE_EXPECTED; registered live $REGISTERED_EXECUTED/$REGISTERED_EXPECTED; file coverage $COV_INSPECTED/$COV_TRACKED inspected, $COV_EXEMPT declared-not-inspected"
