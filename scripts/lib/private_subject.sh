# shellcheck shell=bash
# =============================================================================
# private_subject.sh — ONE implementation of "the artifact under test is the
# artifact THIS TREE compiles"
# (bead fgdb-g0-spine-e2e-red-measures-harness-not-spine-iy7e)
#
# Sourced by scripts/g0_spine_e2e.sh, scripts/g0_claims_e2e.sh and
# scripts/g0_identity_e2e.sh. It is a library: not executable, registers no
# gate, and asserts nothing on its own. The scripts that source it are the
# registered artifacts, and each of them runs its own control over the
# predicate below.
#
# WHY THIS EXISTS. All three gates used to resolve their subject by path out of
# "${CARGO_TARGET_DIR:-$ROOT/target}/debug/registry-check" — a directory six
# panes and three other projects write — gating only on `[ -x "$BIN" ]`, with a
# build step whose exit status nothing read. A gate wired that way reports on
# whichever artifact happens to be there, and its verdict is not a statement
# about the tree. MEASURED 2026-07-26: g0_spine_e2e.sh returned "7 passed, 3
# failed" at 19:22 and "8 passed, 2 failed" at 19:24 with no change to the repo
# between the runs, because a neighbouring pane rebuilt the shared artifact in
# between. An artifact from that same directory carried `wire_types` pin
# fnv1a64:0f3dcd03f7a9eaf7 — a value occurring nowhere in the tracked tree and
# nowhere in identity.rs history — and reported 24 violations against a tree
# that was green.
#
# THREE COPIES OF THIS WOULD BE THREE READERS TO DRIFT, which is the defect the
# bead exists to fix. It lives here once.
#
# WHY rustc AND NOT cargo. registry-check is std-only by constitution (FG-CON-01
# applies to the tooling that enforces it), so it has no dependencies to
# resolve and two rustc calls build it in about 8s: no cargo, no shared
# directory, no package-cache lock, no workspace resolution and therefore no
# git-dep refetch. Nothing is lost by it — `cargo check --all-targets` in
# scripts/check.sh already proves registry-check compiles under cargo.
#
# THE DISK PRICE, since a private build cannot share the swarm's artifact.
# MEASURED per run, per sourcing script, after `-C strip=symbols`:
#   64MB  libregistry_check.rlib   (crate metadata; strip cannot shrink it)
#   8.1MB registry-check
#   ----
#   73MB  per script per run, so 219MB for one full scripts/check.sh
# On 2026-07-26 that landed on a volume at 95% carrying 9594 stale
# `/data/tmp/tmp.*` directories from every gate that mktemps. Reaping those is
# swarm hygiene and is deliberately NOT done here: nothing in this library, or
# in any script that sources it, deletes a file.
# =============================================================================

# subject_newest_source <root> -> prints the most recently modified checker source
subject_newest_source() {
  ls -t "$1"/tools/registry-check/src/*.rs "$1"/tools/registry-check/src/bin/*.rs | head -1
}

# subject_is_fresh <bin> <root> -> 0 when <bin> is newer than every checker source
#
# THE predicate, and the only one. It is an mtime rule, and it is the exact
# property the old cargo step lacked: cargo printed an error, exited 0, left the
# previous artifact in place, and the gate went on to report on it.
subject_is_fresh() {
  local bin="$1" root="$2" newest
  newest="$(subject_newest_source "$root")"
  [ -x "$bin" ] && [ "$bin" -nt "$newest" ]
}

# subject_build <root> <outdir> -> 0 on success; writes <outdir>/registry-check
#
# The build log lands in <outdir>/build.log. Running from <root> makes rustup
# honour rust-toolchain.toml — building from elsewhere silently selects the
# default toolchain.
subject_build() {
  local root="$1" outdir="$2"
  mkdir -p "$outdir"
  (cd "$root" \
    && rustc --edition 2024 --crate-type rlib --crate-name registry_check \
         -C strip=symbols \
         tools/registry-check/src/lib.rs -o "$outdir/libregistry_check.rlib" \
    && rustc --edition 2024 -C strip=symbols tools/registry-check/src/main.rs \
         --extern "registry_check=$outdir/libregistry_check.rlib" \
         -o "$outdir/registry-check") >"$outdir/build.log" 2>&1
}

# subject_write_stale_probe <path> -> an executable file older than any source
#
# The stand-in each sourcing script feeds to its own control. It is chmod +x ON
# PURPOSE: `subject_is_fresh` rejects both a non-executable file and a stale
# one, so a probe that was merely unreadable would be rejected for the wrong
# reason and the control would pass while proving nothing. That is the precise
# defect this bead is about, so the probe differs from a good artifact in the
# mtime and in nothing else. If the predicate ever grows a rule beyond mtime,
# this probe must grow with it or the controls go quiet.
subject_write_stale_probe() {
  local path="$1"
  : >"$path"
  chmod +x "$path"
  touch -d 2020-01-01 "$path"
}
