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
# THE DISK PRICE, AND WHY THE SUBJECT IS SHARED RATHER THAN PRIVATE-PER-RUN.
# MEASURED 2026-07-27 on a live subject directory, after `-C strip=symbols`:
#   66MB  libregistry_check.rlib   (crate metadata; strip cannot shrink it)
#   8.3MB registry-check
#   ----
#   75MB  per subject
# Until fgdb-1j16 each sourcing script `mktemp -d`ed its own, so one
# scripts/check.sh cost 225MB and nothing ever reclaimed it. The header here used
# to say reaping was "swarm hygiene and is deliberately NOT done here", which
# deferred it to nobody: 113 abandoned directories holding 18.01GB, 104 of them a
# single day old, on a volume that hit 100% twice in one night. Of that 18.01GB
# this library's own artifact is 8.17GB (45.4%); the remaining 9.84GB is
# per-run evidence written by the sourcing gates and is fgdb-kwoz, not this file.
#
# The subject is now CONTENT-KEYED AND SHARED. `subject_acquire` builds into
# <cache>/subject-<sha of every compiler input>, and every gate wanting that same
# tree state reuses it. The key includes framed source paths and bytes, this
# build recipe, the pinned toolchain file, and the resolved compiler identity;
# the published manifest binds that key to every artifact hash. MEASURED: cold
# 8.2s, warm 0.024s and zero new bytes, so one scripts/check.sh goes from 3 x
# 75MB to 75MB.
#
# STILL NOTHING HERE DELETES A FILE, and that is deliberate (RULE 1). The publish
# step is `mv -T` onto a path that does not exist; a pane losing that race keeps
# its own copy rather than removing anyone's, which degrades exactly to the old
# behaviour. What the cache changes is the LIFETIME: one directory per distinct
# TREE STATE instead of one per RUN.
#
# THAT IS A 3x CUT, NOT A BOUND, and the honest number matters. MEASURED: 111
# commits touched tools/registry-check/src over the window in which 113 of those
# directories appeared -- 1.02 directories per commit. The tree state moves about
# as often as a gate runs, so sharing ACROSS runs recovers almost nothing; what is
# guaranteed is sharing WITHIN one check.sh, whose three gates run a minute or two
# apart. Bounding the cache means deleting old keys, and reuse and self-cleanup
# are mutually exclusive here -- if the first gate cleaned up on exit the other two
# could not reuse -- so a retention policy needs an owner with delete authority and
# is fgdb-1j16 option 2, not this library's business.
# =============================================================================

# subject_input_files <root> -> NUL-delimited, sorted compiler input paths
subject_input_files() {
  local root="$1"
  [ -d "$root/tools/registry-check/src" ] || return 1
  [ -f "$root/scripts/lib/private_subject.sh" ] || return 1
  [ -f "$root/rust-toolchain.toml" ] || return 1
  {
    find "$root/tools/registry-check/src" -type f -name '*.rs' -print0
    printf '%s\0' \
      "$root/scripts/lib/private_subject.sh" \
      "$root/rust-toolchain.toml"
  } | LC_ALL=C sort -z
}

# subject_rustc_identity <root> -> the compiler selected for this tree
subject_rustc_identity() {
  (cd "$1" && rustc -vV)
}

# subject_input_stream <root> -> framed bytes hashed by subject_key
#
# Raw concatenation is ambiguous: changing `ab` + `c` into `a` + `bc` preserves
# the byte stream, and omitting paths makes a rename invisible. Every record is
# therefore NUL-framed by kind, repository-relative path, byte length, and
# bytes. The resolved compiler is a separate framed record because its identity
# is not necessarily represented by a tracked file.
subject_input_stream() {
  local root="$1" file relative byte_count rustc_identity
  printf 'fgdb-private-subject-input-v1\0'
  while IFS= read -r -d '' file; do
    relative="${file#"$root"/}"
    [ "$relative" != "$file" ] || return 1
    read -r byte_count < <(wc -c <"$file")
    printf 'file\0%s\0%s\0' "$relative" "$byte_count"
    cat -- "$file"
    printf '\0'
  done < <(subject_input_files "$root")

  rustc_identity="$(subject_rustc_identity "$root")" || return 1
  read -r byte_count < <(printf '%s' "$rustc_identity" | wc -c)
  printf 'rustc-vV\0%s\0%s\0' "$byte_count" "$rustc_identity"
}

# subject_newest_source <root> -> prints the newest tracked compiler input
subject_newest_source() {
  local root="$1" file newest=""
  while IFS= read -r -d '' file; do
    if [ -z "$newest" ] || [ "$file" -nt "$newest" ]; then
      newest="$file"
    fi
  done < <(subject_input_files "$root")
  [ -n "$newest" ] || return 1
  printf '%s\n' "$newest"
}

# subject_is_fresh <bin> <root> -> 0 when <bin> is newer than every file input
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
# plus the three gate-consumed checker binaries (fresh-eyes I5: the topology,
# threat, and architecture gates ran cargo's shared-target-dir artifact with
# only test -x — a binary another pane's build can replace mid-run; they now
# consume this provenance-controlled subject like their sibling gates).
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
         -o "$outdir/registry-check" \
    && for gate_src in tools/registry-check/src/bin/*-check.rs; do
         [ -e "$gate_src" ] || continue
         gate_bin="$(basename "$gate_src" .rs)"
         rustc --edition 2024 -C strip=symbols "$gate_src" \
           --extern "registry_check=$outdir/libregistry_check.rlib" \
           -o "$outdir/$gate_bin" || exit 1
       done) >"$outdir/build.log" 2>&1
}

# subject_cache_dir -> the directory shared subjects live in
subject_cache_dir() {
  printf '%s' "${FGDB_SUBJECT_CACHE:-${TMPDIR:-/tmp}/fgdb-subject}"
}

# subject_key <root> -> a full-width content key over EVERY compiler input
#
# `subject_input_files` is the ONE file enumerator shared with the freshness
# rule, and `find` deliberately covers nested modules rather than assuming the
# current two-level layout. The full 256 bits are retained: cache provenance is
# not the place to introduce a silent 64-bit collision domain.
subject_key() {
  local digest
  digest="$(subject_input_stream "$1" | sha256sum)" || return 1
  printf '%s' "${digest%% *}"
}

# subject_dir <root> -> where this tree state's subject belongs
subject_dir() {
  printf '%s/subject-%s' "$(subject_cache_dir)" "$(subject_key "$1")"
}

# subject_acquire <root> -> prints a directory holding a fresh registry-check
#
# Reuses the shared subject when one exists for this tree state and still passes
# `subject_is_fresh`; otherwise builds a private staging copy and publishes it by
# rename. Both guards are kept on purpose: the content key says the artifact was
# built from THESE sources, and the mtime rule says the build is not older than
# them. The key alone would accept a directory left by an interrupted build.
#
# Losing the publish race is not an error. `mv -T` onto an existing directory
# fails, and the loser returns its own staging copy -- the pre-fgdb-1j16
# behaviour, for one run, rather than reading bytes another process is mid-write.
subject_acquire() {
  local root="$1" dir stage
  dir="$(subject_dir "$root")"
  # The fast path must test the WHOLE promised artifact set, and the set is
  # what this tree's sources provide: a cache entry written before the set
  # grew is not a hit no matter how fresh its registry-check is (fresh-eyes
  # I5 follow-up), and a fixture crate with no gate-checker sources promises
  # none (the residue control builds exactly one of those).
  local gate_src gate_bin complete=0
  for gate_src in "$root"/tools/registry-check/src/bin/*-check.rs; do
    [ -e "$gate_src" ] || continue
    gate_bin="$dir/$(basename "$gate_src" .rs)"
    subject_is_fresh "$gate_bin" "$root" || complete=1
  done
  if subject_is_fresh "$dir/registry-check" "$root" && [ "$complete" -eq 0 ]; then
    printf '%s' "$dir"
    return 0
  fi
  stage="$dir.partial.$$"
  mkdir -p "$(subject_cache_dir)"
  if ! subject_build "$root" "$stage"; then
    printf '%s' "$stage"
    return 1
  fi
  if mv -T "$stage" "$dir" 2>/dev/null; then
    printf '%s' "$dir"
  else
    printf '%s' "$stage"
  fi
  return 0
}

# subject_residue_fixture <root> -> a throwaway crate shaped like registry-check
subject_residue_fixture() {
  local dir="$1/tools/registry-check/src"
  mkdir -p "$dir/bin"
  printf 'pub fn subject_residue_fixture() {}\n' >"$dir/lib.rs"
  printf 'fn main() { registry_check::subject_residue_fixture(); }\n' >"$dir/main.rs"
  printf 'fn main() {}\n' >"$dir/bin/unused.rs"
}

# subject_residue_control <scratch> -> 0 when the runner reuses and leaves nothing
#
# THE CONTROL THAT FAILS IF THE LEAK RETURNS (fgdb-1j16). It asserts the two
# properties that together mean "no directory per run": two acquisitions of the
# same tree state return the SAME path, and neither leaves a `*.partial.*`
# staging directory behind.
#
# IT RUNS AGAINST A FIXTURE, NOT THE LIVE TREE, AND THAT IS THE WHOLE POINT. The
# first draft asserted path stability against this repository and failed
# immediately, because a neighbouring pane committed a checker source in the 8
# seconds between the two acquisitions. Shipped that way it would have redded all
# three gates at random, which is a worse defect than the leak. The fixture is
# ~0.2s and 388KB, 0.5% of the 75MB it protects.
#
# Both halves are mutation-proved: keying the directory per run trips "does not
# reuse", and a runner that never publishes trips "left per-run residue".
subject_residue_control() {
  local scratch="$1" root cache first second path
  local -a leftovers=()
  root="$scratch/subject-residue-root"
  cache="$scratch/subject-residue-cache"
  subject_residue_fixture "$root" || return 1
  first="$(FGDB_SUBJECT_CACHE="$cache" subject_acquire "$root")" || return 1
  second="$(FGDB_SUBJECT_CACHE="$cache" subject_acquire "$root")" || return 1
  if [ "$first" != "$second" ]; then
    printf 'subject runner does not reuse: %s then %s\n' "$first" "$second" >&2
    return 1
  fi
  for path in "$cache"/*.partial.*; do
    [ -e "$path" ] && leftovers+=("$path")
  done
  if [ "${#leftovers[@]}" -ne 0 ]; then
    printf 'subject runner left per-run residue: %s\n' "${leftovers[*]}" >&2
    return 1
  fi
  return 0
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
