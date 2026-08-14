#!/usr/bin/env bash
# =============================================================================
# w1_crypto_codegen_e2e.sh — inspect the optimized zeroization boundary
# =============================================================================
# Owner: fgdb-w1-crypto-y5o
#
# This gate makes one narrow compiled-code claim: Secret bytes and the original
# Argon2/BLAKE2b/BLAKE3/ChaCha20/Poly1305 word storage delegates to non-inlined boundaries,
# and the optimized host object retains each boundary as an unconditional call
# to memset with the source-pinned zero fill followed by a compiler fence. It
# does NOT claim that copies in registers, moved-from temporaries, allocator/OS
# copies, swap, or crash remnants are scrubbed, nor that every crypto kernel is
# constant-time on any microarchitecture.
# =============================================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

# shellcheck source=lib/gate_verdict.sh
. "$ROOT/scripts/lib/gate_verdict.sh"
gate_init "w1_crypto_codegen_e2e"

EVIDENCE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fgdb-crypto-codegen.XXXXXX")"
TARGET_DIR="$EVIDENCE_DIR/target"
BUILD_LOG="$EVIDENCE_DIR/cargo-rustc.log"
NM_LOG="$EVIDENCE_DIR/nm.log"
OBJDUMP_LOG="$EVIDENCE_DIR/objdump.log"

MISSING_TOOLS=0
for tool in cargo rustc nm objdump; do
  if command -v "$tool" >/dev/null 2>&1; then
    gate_pass "required tool is available: $tool"
  else
    gate_unrun "required code-generation inspection tool is unavailable: $tool"
    MISSING_TOOLS=1
  fi
done
if [ "$MISSING_TOOLS" -ne 0 ]; then
  gate_diag "  retained evidence: $EVIDENCE_DIR"
  gate_verdict
  exit $?
fi

HOST_TRIPLE="$(rustc -vV | awk '/^host:/ { print $2 }')"
NM_VERSION="$(nm --version 2>/dev/null | head -n 1)"
OBJDUMP_VERSION="$(objdump --version 2>/dev/null | head -n 1)"
case "$HOST_TRIPLE" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu)
    if [[ "$NM_VERSION" == *"GNU nm"* ]] \
      && [[ "$OBJDUMP_VERSION" == *"GNU objdump"* ]]; then
      gate_pass "host and binary-inspection dialect are supported: $HOST_TRIPLE / GNU binutils"
    else
      gate_unrun "host binary-inspection dialect is not the witnessed GNU binutils contract"
      gate_diag "  nm: ${NM_VERSION:-unidentified}"
      gate_diag "  objdump: ${OBJDUMP_VERSION:-unidentified}"
      gate_diag "  retained evidence: $EVIDENCE_DIR"
      gate_verdict
      exit $?
    fi
    ;;
  *)
    gate_unrun "host target is outside the witnessed ELF code-generation set: ${HOST_TRIPLE:-unknown}"
    gate_diag "  retained evidence: $EVIDENCE_DIR"
    gate_verdict
    exit $?
    ;;
esac

if cargo test --offline --locked -p fgdb-crypto --test constant_time_audit \
  secret_scrub_delegates_to_codegen_witnessed_boundary -- --exact \
  >"$EVIDENCE_DIR/source-linkage.log" 2>&1; then
  gate_pass "owned crypto-derived state delegates to witnessed scrub boundaries"
else
  gate_fail "crypto state no longer delegates to the witnessed scrub boundaries"
  gate_diag "  source-linkage transcript: $EVIDENCE_DIR/source-linkage.log"
fi

if cargo test --offline --locked -p fgdb-crypto --test constant_time_audit \
  aead_forgery_timing_probe_is_bounded_and_detector_is_live -- --exact --nocapture \
  >"$EVIDENCE_DIR/timing-evidence.log" 2>&1; then
  gate_pass "bounded AEAD Welch-t screen is quiet and its planted early-exit detector fires"
else
  gate_fail "bounded AEAD timing screen or its planted detector failed"
  gate_diag "  timing-evidence transcript: $EVIDENCE_DIR/timing-evidence.log"
fi

if cargo test --offline --locked -p fgdb-crypto --doc \
  >"$EVIDENCE_DIR/compile-fail.log" 2>&1; then
  gate_pass "public secret owners remain non-cloneable and consuming at compile time"
else
  gate_fail "a secret owner regained a clone or reuse path"
  gate_diag "  compile-fail transcript: $EVIDENCE_DIR/compile-fail.log"
fi

if CARGO_TARGET_DIR="$TARGET_DIR" cargo rustc --offline --locked --release \
  -p fgdb-crypto --lib -- --emit=obj >"$BUILD_LOG" 2>&1; then
  gate_pass "optimized fgdb-crypto production object compiled"
else
  gate_unrun "optimized fgdb-crypto production object did not compile"
  gate_diag "  compiler transcript: $BUILD_LOG"
  gate_diag "  retained evidence: $EVIDENCE_DIR"
  gate_verdict
  exit $?
fi

mapfile -t OBJECTS < <(
  find "$TARGET_DIR/release/deps" -maxdepth 1 -type f \
    -name 'fgdb_crypto-*.o' -print | sort
)
if [ "${#OBJECTS[@]}" -eq 1 ]; then
  gate_pass "release compilation produced exactly one inspectable fgdb-crypto object"
else
  gate_unrun "release compiler output layout produced ${#OBJECTS[@]} fgdb-crypto objects; expected exactly one"
  gate_diag "  candidates: ${OBJECTS[*]:-(none)}"
  gate_diag "  retained evidence: $EVIDENCE_DIR"
  gate_verdict
  exit $?
fi
OBJECT="${OBJECTS[0]}"

if objdump -f "$OBJECT" >"$EVIDENCE_DIR/object-format.log" 2>&1 \
  && grep -E 'file format elf64-(x86-64|littleaarch64)' \
    "$EVIDENCE_DIR/object-format.log" >/dev/null; then
  gate_pass "compiled artifact is a supported ELF production object"
else
  gate_unrun "compiled artifact is outside the witnessed ELF object dialect"
  gate_diag "  object-format transcript: $EVIDENCE_DIR/object-format.log"
  gate_diag "  retained evidence: $EVIDENCE_DIR"
  gate_verdict
  exit $?
fi

if ! nm -C "$OBJECT" >"$NM_LOG" 2>&1; then
  gate_unrun "nm could not inspect the optimized production object"
  gate_diag "  nm transcript: $NM_LOG"
fi

if ! objdump -drC --no-show-raw-insn "$OBJECT" >"$OBJDUMP_LOG" 2>&1; then
  gate_unrun "objdump could not disassemble the optimized production object"
  gate_diag "  objdump transcript: $OBJDUMP_LOG"
else
  inspect_scrub_boundary() {
    local symbol="$1"
    local storage_kind="$2"
    local symbol_log="$EVIDENCE_DIR/${symbol//_/-}.symbol.log"
    local symbol_count call_count memset_relocation_count conditional_branch_count

    symbol_count="$(grep -Ec " T fgdb_crypto::zeroize::${symbol}$" "$NM_LOG" || true)"
    if [ "$symbol_count" -eq 1 ]; then
      gate_pass "optimized object exports exactly one $symbol codegen boundary"
    else
      gate_fail "optimized object exposes $symbol_count $symbol boundaries; expected exactly one"
    fi

  awk '
    index($0, "<fgdb_crypto::zeroize::" symbol ">:") { capture = 1 }
    capture { print }
    capture && /^$/ { exit }
  ' symbol="$symbol" "$OBJDUMP_LOG" >"$symbol_log"
  call_count="$(grep -Ec '[[:space:]](call|bl)[[:space:]]' "$symbol_log" || true)"
  memset_relocation_count="$(grep -Ec '[[:space:]]R_[^[:space:]]+[[:space:]]+memset([@+-]|$)' \
    "$symbol_log" || true)"
  conditional_branch_count="$(grep -Ec \
    '^[[:space:]]*[0-9a-f]+:[[:space:]]+(j[a-z]+|loop[a-z]*|b\.[a-z]+|cbz|cbnz|tbz|tbnz)[[:space:]]' \
    "$symbol_log" || true)"
  if [ "$call_count" -eq 1 ] \
    && [ "$memset_relocation_count" -eq 1 ] \
    && [ "$conditional_branch_count" -eq 0 ] \
    && awk '
      /[[:space:]](call|bl)[[:space:]]/ { call_line = NR; next }
      /[[:space:]]R_[^[:space:]]+[[:space:]]+memset([@+-]|$)/ {
        if (call_line == NR - 1) found = 1
      }
      END { exit(found ? 0 : 1) }
    ' "$symbol_log"; then
    gate_pass "optimized $symbol unconditionally scrubs $storage_kind with exactly one memset call"
  else
    gate_fail "optimized $symbol is conditional or lacks exactly one memset-targeted call"
    gate_diag "  symbol disassembly: $symbol_log"
  fi
  }

  inspect_scrub_boundary "scrub_slice" "byte storage"
  inspect_scrub_boundary "scrub_words" "word storage"
  inspect_scrub_boundary "scrub_words32" "32-bit word storage"
fi

gate_diag "  retained evidence: $EVIDENCE_DIR"
gate_verdict
