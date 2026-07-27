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
# When CI is added, wire this script as the CI test step rather than
# duplicating the commands.
# =============================================================================
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECKER_INDEX="$ROOT/registries/checker_index.toml"

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
    echo "PASS core: $label"
  else
    LAST_GATE_RC=$?
    CORE_RED=$((CORE_RED + 1))
    echo "RED core: $label (exit $LAST_GATE_RC)" >&2
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

run_ubs() {
  local log="$GATE_LOG_DIR/core-ubs.log"
  local ubs_rc

  ubs --only=rust --ci . 2>&1 | tee "$log"
  ubs_rc=${PIPESTATUS[0]}
  if [ "$ubs_rc" -ne 0 ]; then
    return "$ubs_rc"
  fi
  if grep -Eiq \
    'nothing was checked|did not run any scanner|no supported languages detected' \
    "$log"; then
    echo "ERROR: UBS exited zero without running a Rust scanner" >&2
    return 1
  fi
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
      echo "PASS registered $kind $artifact — $detail"
      ;;
    red)
      REGISTERED_EXECUTED=$((REGISTERED_EXECUTED + 1))
      REGISTERED_RED=$((REGISTERED_RED + 1))
      increment_registered_kind_executed "$kind"
      echo "RED registered $kind $artifact — $detail" >&2
      ;;
    unrun)
      REGISTERED_UNRUN=$((REGISTERED_UNRUN + 1))
      echo "UNRUN registered $kind $artifact — $detail" >&2
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
    echo "GATES RED: a registered live gate failed or was not executed" >&2
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

  echo "CHECK.SH MUTATION SELF-TEST PASS"
  echo "  failing registered gate: RED"
  echo "  registered gate without a runner: UNRUN and nonzero"
  echo "  cargo-test attribution: pass / red / unrun / ambiguous all separated"
  echo "  unrun cargo-test artifact: UNRUN and nonzero"
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

run_core_gate \
  "file-coverage closure (every tracked file inspected or declared)" \
  run_file_coverage
run_core_gate \
  "shell lint (bash -n + shellcheck) over tracked shell deliverables" \
  run_shell_lint
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
  echo "QUALITY GATE RED" >&2
  exit 1
fi

echo "ALL GATES GREEN — core $CORE_EXECUTED/$CORE_EXPECTED; registered live $REGISTERED_EXECUTED/$REGISTERED_EXPECTED; file coverage $COV_INSPECTED/$COV_TRACKED inspected, $COV_EXEMPT declared-not-inspected"
