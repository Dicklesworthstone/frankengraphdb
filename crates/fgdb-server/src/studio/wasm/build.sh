#!/usr/bin/env bash
# Offline, reproducible freestanding presentation module. Not a native library.
set -euo pipefail
cd "$(dirname "$0")"
: "${CLANG:=clang}"
"$CLANG" --target=wasm32 -std=c11 -O2 -nostdlib -fno-builtin \
  -ffp-contract=off -Wall -Wextra -Werror \
  -Wl,--no-entry -Wl,--export-memory -Wl,--stack-first \
  -Wl,-z,stack-size=65536 -Wl,--initial-memory=4194304 \
  -Wl,--max-memory=4194304 -Wl,--strip-all \
  force.c -o force.wasm
