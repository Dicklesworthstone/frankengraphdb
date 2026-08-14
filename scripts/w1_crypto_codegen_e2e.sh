#!/usr/bin/env bash
# =============================================================================
# w1_crypto_codegen_e2e.sh — inspect the optimized zeroization boundary
# =============================================================================
# Owner: fgdb-w1-crypto-y5o
#
# This gate makes one narrow compiled-code claim: every Secret drop delegates
# to a non-inlined production boundary, and the optimized host object retains
# that boundary as a call to memset with the source-pinned zero fill followed by
# a compiler fence. It does NOT claim that copies in registers, moved-from
# temporaries, swap, or other allocations are scrubbed, nor that every crypto
# kernel is constant-time on any microarchitecture.
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
SYMBOL_LOG="$EVIDENCE_DIR/scrub-slice.symbol.log"

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
  gate_pass "every Secret scrub delegates to the non-inlined witnessed boundary"
else
  gate_fail "Secret scrub no longer delegates to the witnessed boundary"
  gate_diag "  source-linkage transcript: $EVIDENCE_DIR/source-linkage.log"
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

if nm -C "$OBJECT" >"$NM_LOG" 2>&1; then
  SYMBOL_COUNT="$(grep -Fc ' T fgdb_crypto::zeroize::scrub_slice' "$NM_LOG" || true)"
  if [ "$SYMBOL_COUNT" -eq 1 ]; then
    gate_pass "optimized object exports exactly one scrub_slice codegen boundary"
  else
    gate_fail "optimized object exposes $SYMBOL_COUNT scrub_slice boundaries; expected exactly one"
  fi
else
  gate_unrun "nm could not inspect the optimized production object"
  gate_diag "  nm transcript: $NM_LOG"
fi

if objdump -drC "$OBJECT" >"$OBJDUMP_LOG" 2>&1; then
  awk '
    /<fgdb_crypto::zeroize::scrub_slice>:/ { capture = 1 }
    capture { print }
    capture && /^$/ { exit }
  ' "$OBJDUMP_LOG" >"$SYMBOL_LOG"
  CALL_COUNT="$(grep -Ec '[[:space:]](call|bl)[[:space:]]' "$SYMBOL_LOG" || true)"
  MEMSET_RELOCATION_COUNT="$(grep -Ec '[[:space:]]R_[^[:space:]]+[[:space:]]+memset([@+-]|$)' \
    "$SYMBOL_LOG" || true)"
  if [ "$CALL_COUNT" -eq 1 ] \
    && [ "$MEMSET_RELOCATION_COUNT" -eq 1 ] \
    && awk '
      /[[:space:]](call|bl)[[:space:]]/ { call_line = NR; next }
      /[[:space:]]R_[^[:space:]]+[[:space:]]+memset([@+-]|$)/ {
        if (call_line == NR - 1) found = 1
      }
      END { exit(found ? 0 : 1) }
    ' "$SYMBOL_LOG"; then
    gate_pass "optimized scrub_slice has exactly one call and it resolves to memset"
  else
    gate_fail "optimized scrub_slice no longer has exactly one memset-targeted call"
    gate_diag "  symbol disassembly: $SYMBOL_LOG"
  fi
else
  gate_unrun "objdump could not disassemble the optimized production object"
  gate_diag "  objdump transcript: $OBJDUMP_LOG"
fi

gate_diag "  retained evidence: $EVIDENCE_DIR"
gate_verdict
