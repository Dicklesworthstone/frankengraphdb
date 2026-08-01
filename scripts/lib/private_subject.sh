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
# MEASURED 2026-07-27 on the original two-artifact subject, after
# `-C strip=symbols`:
#   66MB  libregistry_check.rlib   (crate metadata; strip cannot shrink it)
#   8.3MB registry-check
#   ----
#   75MB  historical floor per subject
# The subject now also carries one binary per `*-check.rs`; the historical
# figure is retained only as the lower-bound evidence that motivated sharing,
# not presented as a current artifact census.
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
# the published manifest binds that key to every artifact hash. Every consumer
# for one tree state therefore reuses one directory rather than compiling a
# private copy; a cache hit deliberately re-hashes the promised artifacts.
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

# subject_inputs_exist <root> -> all mandatory input roots/files exist
subject_inputs_exist() {
  local root="$1"
  [ -d "$root/tools/registry-check/src" ] || return 1
  [ -f "$root/scripts/lib/private_subject.sh" ] || return 1
  [ -f "$root/rust-toolchain.toml" ] || return 1
}

# subject_input_files <root> -> NUL-delimited, sorted compiler input paths
subject_input_files() {
  local root="$1"
  subject_inputs_exist "$root" || return 1
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
  subject_inputs_exist "$root" || return 1
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

# subject_expected_artifacts <root> -> sorted artifact names promised by the recipe
subject_expected_artifacts() {
  local root="$1" gate_src
  {
    printf '%s\n' libregistry_check.rlib registry-check
    for gate_src in "$root"/tools/registry-check/src/bin/*-check.rs; do
      [ -e "$gate_src" ] || continue
      basename "$gate_src" .rs
    done
  } | LC_ALL=C sort -u
}

# subject_write_manifest <root> <outdir> <input-key>
#
# The manifest is written only after every compiler invocation succeeds. It
# binds the full input key to both the complete artifact-name set and each
# artifact's bytes, so an executable with a plausible mtime is not sufficient
# for a cache hit.
subject_write_manifest() {
  local root="$1" outdir="$2" expected_key="$3"
  local actual_key name artifact mode digest
  actual_key="$(subject_key "$root")" || return 1
  if [ "$actual_key" != "$expected_key" ]; then
    printf 'subject inputs changed during build: expected %s, found %s\n' \
      "$expected_key" "$actual_key" >&2
    return 1
  fi

  {
    printf 'format\tfgdb-private-subject-v1\n'
    printf 'input_sha256\t%s\n' "$expected_key"
    while IFS= read -r name; do
      artifact="$outdir/$name"
      case "$name" in
        *.rlib)
          [ -f "$artifact" ] || return 1
          mode="file"
          ;;
        *)
          [ -x "$artifact" ] || return 1
          mode="executable"
          ;;
      esac
      digest="$(sha256sum -- "$artifact")" || return 1
      printf 'artifact\t%s\t%s\t%s\n' "$mode" "${digest%% *}" "$name"
    done < <(subject_expected_artifacts "$root")
  } >"$outdir/subject.manifest"
}

# subject_manifest_is_valid <dir> <root> <input-key>
subject_manifest_is_valid() {
  local dir="$1" root="$2" expected_key="$3" manifest
  local first second expected_names listed_names expected_count line_count
  local kind mode digest name extra artifact actual_digest
  manifest="$dir/subject.manifest"
  [ -f "$manifest" ] || return 1

  first="$(sed -n '1p' "$manifest")"
  second="$(sed -n '2p' "$manifest")"
  [ "$first" = $'format\tfgdb-private-subject-v1' ] || return 1
  [ "$second" = "$(printf 'input_sha256\t%s' "$expected_key")" ] || return 1

  expected_names="$(subject_expected_artifacts "$root")" || return 1
  listed_names="$(awk -F '\t' '$1 == "artifact" && NF == 4 { print $4 }' \
    "$manifest" | LC_ALL=C sort)" || return 1
  [ "$listed_names" = "$expected_names" ] || return 1
  expected_count="$(printf '%s\n' "$expected_names" | awk 'NF { count++ } END { print count + 0 }')"
  read -r line_count < <(wc -l <"$manifest")
  [ "$line_count" -eq $((expected_count + 2)) ] || return 1

  while IFS=$'\t' read -r kind mode digest name extra; do
    [ "$kind" = "artifact" ] && [ -z "$extra" ] || return 1
    [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || return 1
    artifact="$dir/$name"
    case "$name" in
      *.rlib)
        [ "$mode" = "file" ] && [ -f "$artifact" ] || return 1
        ;;
      *)
        [ "$mode" = "executable" ] && [ -x "$artifact" ] || return 1
        ;;
    esac
    actual_digest="$(sha256sum -- "$artifact")" || return 1
    [ "${actual_digest%% *}" = "$digest" ] || return 1
  done < <(sed -n '3,$p' "$manifest")
}

# subject_cache_entry_is_valid <dir> <root> <input-key>
subject_cache_entry_is_valid() {
  local dir="$1" root="$2" expected_key="$3" name
  subject_manifest_is_valid "$dir" "$root" "$expected_key" || return 1
  while IFS= read -r name; do
    case "$name" in
      *.rlib) continue ;;
    esac
    subject_is_fresh "$dir/$name" "$root" || return 1
  done < <(subject_expected_artifacts "$root")
}

# subject_build <root> <outdir> <input-key> -> 0 on success
# Writes <outdir>/registry-check
# plus every gate-consumed `*-check` binary and a hash manifest (fresh-eyes I5:
# the topology, threat, and architecture gates ran cargo's shared-target-dir
# artifact with only test -x — a binary another pane's build can replace
# mid-run; they now consume this provenance-controlled subject like their
# sibling gates).
#
# The build log lands in <outdir>/build.log. Running from <root> makes rustup
# honour rust-toolchain.toml — building from elsewhere silently selects the
# default toolchain.
subject_build() {
  local root="$1" outdir="$2" expected_key="$3" gate_src gate_bin
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
       done \
    && subject_write_manifest "$root" "$outdir" "$expected_key") \
    >"$outdir/build.log" 2>&1
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
  local root="$1" dir stage key current_key
  key="$(subject_key "$root")" || return 1
  dir="$(subject_cache_dir)/subject-$key"
  if subject_cache_entry_is_valid "$dir" "$root" "$key"; then
    current_key="$(subject_key "$root")" || return 1
    [ "$current_key" = "$key" ] || return 1
    printf '%s' "$dir"
    return 0
  fi
  stage="$dir.partial.$$"
  mkdir -p "$(subject_cache_dir)"
  if ! subject_build "$root" "$stage" "$key"; then
    printf '%s' "$stage"
    return 1
  fi
  if ! subject_cache_entry_is_valid "$stage" "$root" "$key"; then
    printf '%s' "$stage"
    return 1
  fi
  if ! current_key="$(subject_key "$root")"; then
    printf '%s' "$stage"
    return 1
  fi
  if [ "$current_key" != "$key" ]; then
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

# subject_recipe_root -> the checkout whose sourced recipe is executing
subject_recipe_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd
}

# subject_fixture_support <root> -> copies the non-Rust compiler inputs
subject_fixture_support() {
  local root="$1" recipe_root
  recipe_root="$(subject_recipe_root)" || return 1
  mkdir -p "$root/scripts/lib"
  cp "$recipe_root/scripts/lib/private_subject.sh" "$root/scripts/lib/private_subject.sh"
  cp "$recipe_root/rust-toolchain.toml" "$root/rust-toolchain.toml"
}

# subject_residue_fixture <root> -> a throwaway crate shaped like registry-check
subject_residue_fixture() {
  local root="$1" dir
  subject_fixture_support "$root" || return 1
  dir="$root/tools/registry-check/src"
  mkdir -p "$dir/bin"
  printf 'pub fn subject_residue_fixture() {}\n' >"$dir/lib.rs"
  printf 'fn main() { registry_check::subject_residue_fixture(); }\n' >"$dir/main.rs"
  printf 'fn main() {}\n' >"$dir/bin/unused.rs"
}

# subject_key_fixture <root> -> only the support inputs needed to compute a key
subject_key_fixture() {
  local root="$1"
  subject_fixture_support "$root" || return 1
  mkdir -p "$root/tools/registry-check/src/bin"
}

# subject_provenance_control <scratch> -> 0 when every key axis is load-bearing
#
# This is intentionally hermetic. It never edits the checkout and never removes
# even its scratch files. The boundary pair has the SAME legacy raw
# concatenation (`ab` + `c` versus `a` + `bc`), and the incomplete-cache fixture
# has fresh executables for every promised name; only framing and the manifest
# can distinguish the bad states.
subject_provenance_control() {
  local scratch="$1" root base repeated resolved_key recipe_key toolchain_key
  local fake_bin path_a path_b boundary_a boundary_b key_a key_b raw_a raw_b
  local incomplete_root incomplete_cache incomplete_key incomplete_dir acquired
  local name artifact

  root="$scratch/subject-key-root"
  subject_residue_fixture "$root" || return 1
  base="$(subject_key "$root")" || return 1
  [[ "$base" =~ ^[0-9a-f]{64}$ ]] || {
    printf 'subject key is not full-width SHA-256: %s\n' "$base" >&2
    return 1
  }
  repeated="$(subject_key "$root")" || return 1
  [ "$base" = "$repeated" ] || {
    printf 'identical subject inputs produced different keys\n' >&2
    return 1
  }

  fake_bin="$scratch/subject-fake-rustc"
  mkdir -p "$fake_bin"
  printf '%s\n' \
    '#!/bin/sh' \
    '[ "$1" = "-vV" ] || exit 97' \
    'printf "rustc 0.0.0-subject-control\\nbinary: rustc\\ncommit-hash: control\\nhost: control\\n"' \
    >"$fake_bin/rustc"
  chmod +x "$fake_bin/rustc"
  resolved_key="$(PATH="$fake_bin:$PATH" subject_key "$root")" || return 1
  [ "$resolved_key" != "$base" ] || {
    printf 'resolved rustc identity did not invalidate the subject key\n' >&2
    return 1
  }

  printf '\n# subject recipe mutation control\n' >>"$root/scripts/lib/private_subject.sh"
  recipe_key="$(subject_key "$root")" || return 1
  [ "$recipe_key" != "$base" ] || {
    printf 'build recipe bytes did not invalidate the subject key\n' >&2
    return 1
  }
  printf '\n# subject toolchain mutation control\n' >>"$root/rust-toolchain.toml"
  toolchain_key="$(subject_key "$root")" || return 1
  [ "$toolchain_key" != "$recipe_key" ] || {
    printf 'pinned toolchain bytes did not invalidate the subject key\n' >&2
    return 1
  }

  path_a="$scratch/subject-path-a"
  path_b="$scratch/subject-path-b"
  subject_key_fixture "$path_a" || return 1
  subject_key_fixture "$path_b" || return 1
  printf 'fn main() {}\n' >"$path_a/tools/registry-check/src/bin/alpha-check.rs"
  printf 'fn main() {}\n' >"$path_b/tools/registry-check/src/bin/beta-check.rs"
  key_a="$(subject_key "$path_a")" || return 1
  key_b="$(subject_key "$path_b")" || return 1
  [ "$key_a" != "$key_b" ] || {
    printf 'relative source paths did not invalidate the subject key\n' >&2
    return 1
  }

  boundary_a="$scratch/subject-boundary-a"
  boundary_b="$scratch/subject-boundary-b"
  subject_key_fixture "$boundary_a" || return 1
  subject_key_fixture "$boundary_b" || return 1
  printf 'ab' >"$boundary_a/tools/registry-check/src/lib.rs"
  printf 'c' >"$boundary_a/tools/registry-check/src/main.rs"
  printf 'a' >"$boundary_b/tools/registry-check/src/lib.rs"
  printf 'bc' >"$boundary_b/tools/registry-check/src/main.rs"
  raw_a="$(cat "$boundary_a/tools/registry-check/src/lib.rs" \
    "$boundary_a/tools/registry-check/src/main.rs")"
  raw_b="$(cat "$boundary_b/tools/registry-check/src/lib.rs" \
    "$boundary_b/tools/registry-check/src/main.rs")"
  [ "$raw_a" = "$raw_b" ] || {
    printf 'boundary control precondition is false\n' >&2
    return 1
  }
  key_a="$(subject_key "$boundary_a")" || return 1
  key_b="$(subject_key "$boundary_b")" || return 1
  [ "$key_a" != "$key_b" ] || {
    printf 'file-boundary changes did not invalidate the subject key\n' >&2
    return 1
  }

  incomplete_root="$scratch/subject-incomplete-root"
  incomplete_cache="$scratch/subject-incomplete-cache"
  subject_residue_fixture "$incomplete_root" || return 1
  incomplete_key="$(subject_key "$incomplete_root")" || return 1
  incomplete_dir="$incomplete_cache/subject-$incomplete_key"
  mkdir -p "$incomplete_dir"
  while IFS= read -r name; do
    artifact="$incomplete_dir/$name"
    printf 'fresh but unmanifested control artifact\n' >"$artifact"
    case "$name" in
      *.rlib) ;;
      *) chmod +x "$artifact" ;;
    esac
  done < <(subject_expected_artifacts "$incomplete_root")
  while IFS= read -r name; do
    case "$name" in
      *.rlib) continue ;;
    esac
    subject_is_fresh "$incomplete_dir/$name" "$incomplete_root" || {
      printf 'incomplete-cache control artifact is not fresh: %s\n' "$name" >&2
      return 1
    }
  done < <(subject_expected_artifacts "$incomplete_root")
  [ ! -e "$incomplete_dir/subject.manifest" ] || return 1
  acquired="$(FGDB_SUBJECT_CACHE="$incomplete_cache" \
    subject_acquire "$incomplete_root")" || return 1
  [ "$acquired" != "$incomplete_dir" ] || {
    printf 'cache acquisition accepted fresh artifacts with no manifest\n' >&2
    return 1
  }
  subject_cache_entry_is_valid "$acquired" "$incomplete_root" "$incomplete_key" || {
    printf 'cache acquisition did not return a complete manifested subject\n' >&2
    return 1
  }
  printf '\npost-publication artifact mutation\n' >>"$acquired/registry-check"
  if subject_cache_entry_is_valid "$acquired" "$incomplete_root" "$incomplete_key"; then
    printf 'cache manifest accepted a post-publication artifact mutation\n' >&2
    return 1
  fi
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
# deliberately a minimal std-only crate rather than another live-tree build.
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
