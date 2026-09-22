#!/usr/bin/env bash
# =============================================================================
# merge_train.sh — the verified landing queue from `staging` to `main`
# =============================================================================
# Owner bead: fgdb-verified-landing-queue-kqglu. Owner ruling 2026-09-22:
# staging branch plus local merge train.
#
# WHY. Between 2026-09-08 and 2026-09-22, 662 of 912 non-merge commits on main
# came from an environment with no Rust toolchain, pushed straight to main
# (NE-0045 recurring at 4.7x). No local hook can refuse a remote push, and
# hosted CI is retired by owner ruling. So the refusal has to live on the only
# path to main: an environment that cannot compile pushes to `staging`, and
# this train is the one thing that moves main.
#
# WHAT A PASS DOES. `run` does the following:
#   1. updates a train-owned bare mirror of the remote;
#   2. clones a fresh private candidate from it;
#   3. builds main + staging as a --no-ff merge. A --no-ff merge, even when a
#      fast-forward is possible, keeps main's first-parent chain made only of
#      train merges;
#   4. runs scripts/local_proof.sh on that exact tree;
#   5. independently verifies the bundle with scripts/local_proof_verify.sh;
#   6. pushes the merge to main only if the manifest says verdict=pass,
#      check_exit=0, tree_stable=true for exactly that commit.
#
# That push is a plain fast-forward (never forced). If main moved during the
# proof, the push is refused and the run exits STALE.
#
# EVERY VERDICT IS RECORDED where the pushing environment can read it: an
# appended git note under the notes ref "merge-train". A promoted merge carries
# its pass note. A bounced staging tip carries red / void / conflict /
# unverifiable plus the FAIL lines from the proof transcript. A batch that
# bounced against the same main is not re-proved; push a fix to staging instead.
#
# `audit` is the measurement. On main's first-parent chain after --since, every
# commit should be a train merge with a pass note. Commits authored by
# --author-pattern (default: the toolchain-less identity) that reached main
# outside a proven train merge are VIOLATIONS, and audit exits 1. Other unproven
# first-parent commits are reported and counted; --strict makes them violations
# too.
#
# THIS SCRIPT NEVER force-pushes, never deletes or rewrites a remote ref, and
# never removes a directory. Candidates, proof bundles and logs accumulate under
# --work for the operator to prune (AGENTS.md RULE 1).
#
# Environment (run): RCH_CARGO_WRAPPER_BYPASS=1 is exported unless already set,
# so the verdict is local. CARGO_TARGET_DIR defaults to <work>/target, a private
# directory reused across runs; this script only ever runs one tree at a time.
# =============================================================================

set -uo pipefail

EX_NOTHING=0
EX_RED=1
EX_USAGE=2
EX_CONFLICT=3
EX_STALE=4
EX_UNVERIFIABLE=5
EX_VOID=125
NOTES_REF=merge-train

usage() {
  cat <<'USAGE'
Usage:
  bash scripts/merge_train.sh run   [--remote NAME|URL] [--main BRANCH] [--staging BRANCH]
                                    [--work DIR] [--no-push]
  bash scripts/merge_train.sh audit --since REV [--remote NAME|URL] [--main BRANCH]
                                    [--work DIR] [--author-pattern ERE] [--strict]

run exit codes:  0 promoted or nothing to promote   1 red (bounced)
                 3 merge conflict (bounced)          4 stale (main moved; retry)
                 5 proof bundle failed verification  125 void (tree moved)
                 2 usage or environment error
audit exit codes: 0 no violation   1 violations found   2 usage or environment error
USAGE
}

die() {
  printf 'merge-train: %s\n' "$*" >&2
  exit "$EX_USAGE"
}

say() {
  printf 'merge-train: %s\n' "$*"
}

[ "$#" -ge 1 ] || { usage >&2; exit "$EX_USAGE"; }
command="$1"
shift
case "$command" in
  run|audit) ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit "$EX_USAGE" ;;
esac

remote="origin"
main_branch="main"
staging_branch="staging"
work="${TMPDIR:-/tmp}/fgdb-merge-train"
push=1
since=""
author_pattern='users\.noreply\.github\.com'
strict=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --remote) [ "$#" -ge 2 ] || die "--remote needs a value"; remote="$2"; shift 2 ;;
    --main) [ "$#" -ge 2 ] || die "--main needs a value"; main_branch="$2"; shift 2 ;;
    --staging) [ "$#" -ge 2 ] || die "--staging needs a value"; staging_branch="$2"; shift 2 ;;
    --work) [ "$#" -ge 2 ] || die "--work needs a value"; work="$2"; shift 2 ;;
    --no-push) push=0; shift ;;
    --since) [ "$#" -ge 2 ] || die "--since needs a value"; since="$2"; shift 2 ;;
    --author-pattern) [ "$#" -ge 2 ] || die "--author-pattern needs a value"; author_pattern="$2"; shift 2 ;;
    --strict) strict=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

command -v git >/dev/null 2>&1 || die "git is required"
root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "run from inside the repository"
if remote_url="$(git -C "$root" remote get-url "$remote" 2>/dev/null)"; then
  :
else
  remote_url="$remote"
fi
case "$work" in /*) ;; *) work="$PWD/$work" ;; esac
case "$work" in "$root"|"$root"/*) die "--work must be outside the repository worktree" ;; esac
mkdir -p "$work/candidates" "$work/proofs" || die "cannot create $work"

train_name="$(git -C "$root" config user.name 2>/dev/null || true)"
train_email="$(git -C "$root" config user.email 2>/dev/null || true)"
[ -n "$train_name" ] || train_name="fgdb merge-train"
[ -n "$train_email" ] || train_email="merge-train@localhost"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"
mirror="$work/mirror.git"

# The mirror is train-owned. It is created once, reusing local objects but
# dissociated from them so a gc in the shared repository cannot corrupt it.
refresh_mirror() {
  if [ ! -d "$mirror" ]; then
    git clone --quiet --bare --reference-if-able "$root" --dissociate "$remote_url" "$mirror" \
      || die "cannot create mirror of $remote_url"
  fi
  git -C "$mirror" fetch --quiet "$remote_url" \
    "+refs/heads/*:refs/heads/*" "+refs/notes/$NOTES_REF:refs/notes/$NOTES_REF" 2>/dev/null \
    || git -C "$mirror" fetch --quiet "$remote_url" "+refs/heads/*:refs/heads/*" \
    || die "cannot fetch $remote_url"
}

gitc() {
  git -c user.name="$train_name" -c user.email="$train_email" "$@"
}

# Append a verdict note to OBJECT in the candidate clone and publish it, retrying
# against concurrent note writers by merging (cat_sort_uniq), never forcing.
publish_note() {
  local clone="$1" object="$2" body="$3" attempt
  gitc -C "$clone" notes --ref="$NOTES_REF" append -m "$body" "$object" \
    || { say "WARNING: could not write the note locally"; return 1; }
  [ "$push" -eq 1 ] || { say "--no-push: note written locally only"; return 0; }
  for attempt in 1 2 3; do
    if git -C "$clone" push --quiet "$remote_url" \
        "refs/notes/$NOTES_REF:refs/notes/$NOTES_REF" 2>/dev/null; then
      return 0
    fi
    git -C "$clone" fetch --quiet "$remote_url" \
      "+refs/notes/$NOTES_REF:refs/notes/$NOTES_REF-remote" 2>/dev/null || true
    gitc -C "$clone" notes --ref="$NOTES_REF" merge -q -s cat_sort_uniq \
      "$NOTES_REF-remote" 2>/dev/null || true
    say "note push rejected (attempt $attempt); merged remote notes and retrying"
  done
  say "WARNING: could not publish the note after 3 attempts"
  return 1
}

note_body() {
  # $1 verdict; remaining key=value pairs are appended verbatim.
  local verdict="$1"
  shift
  printf 'merge-train v1\nverdict=%s\nrecorded_utc=%s\nhost=%s\n' \
    "$verdict" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(hostname 2>/dev/null || echo unknown)"
  local kv
  for kv in "$@"; do printf '%s\n' "$kv"; done
}

run_train() {
  refresh_mirror
  local main_sha staging_sha
  main_sha="$(git -C "$mirror" rev-parse --verify --quiet "refs/heads/$main_branch^{commit}")" \
    || die "remote has no $main_branch branch"
  if ! staging_sha="$(git -C "$mirror" rev-parse --verify --quiet "refs/heads/$staging_branch^{commit}")"; then
    say "NOTHING: remote has no $staging_branch branch"
    return "$EX_NOTHING"
  fi
  if git -C "$mirror" merge-base --is-ancestor "$staging_sha" "$main_sha"; then
    say "NOTHING: $staging_branch ${staging_sha:0:12} is already contained in $main_branch ${main_sha:0:12}"
    return "$EX_NOTHING"
  fi
  local prior
  prior="$(git -C "$mirror" notes --ref="$NOTES_REF" show "$staging_sha" 2>/dev/null || true)"
  # Here-strings, not `printf | grep -q`: under pipefail an early-exiting grep -q
  # can SIGPIPE the writer and turn a real match into a false negative.
  if [ -n "$prior" ] && grep -qx "main=$main_sha" <<< "$prior" \
      && grep -Eqx 'verdict=(red|conflict)' <<< "$prior"; then
    say "BOUNCED-ALREADY: $staging_branch ${staging_sha:0:12} was already refused against $main_branch ${main_sha:0:12}; push a fix to $staging_branch"
    grep -E '^(verdict|fail|conflict)' <<< "$prior" | sed 's/^/    /'
    if grep -qx 'verdict=conflict' <<< "$prior"; then return "$EX_CONFLICT"; fi
    return "$EX_RED"
  fi

  local cand="$work/candidates/$stamp-$$"
  git clone --quiet --local "$mirror" "$cand" || die "cannot clone candidate"
  git -C "$cand" fetch --quiet origin "+refs/notes/$NOTES_REF:refs/notes/$NOTES_REF" 2>/dev/null || true
  git -C "$cand" checkout --quiet --detach "$main_sha" || die "cannot check out $main_sha"
  say "candidate: $cand"
  say "merging $staging_branch ${staging_sha:0:12} into $main_branch ${main_sha:0:12} (--no-ff)"
  if ! gitc -C "$cand" merge --quiet --no-ff --no-edit \
      -m "merge-train: promote $staging_branch ${staging_sha:0:12} onto $main_branch ${main_sha:0:12}" \
      "$staging_sha" >/dev/null 2>&1; then
    local conflicts
    conflicts="$(git -C "$cand" diff --name-only --diff-filter=U)"
    git -C "$cand" merge --abort >/dev/null 2>&1 || true
    say "CONFLICT: $staging_branch does not merge cleanly onto $main_branch"
    printf '%s\n' "$conflicts" | sed 's/^/    /'
    local -a kv=("main=$main_sha" "staging=$staging_sha")
    local f
    while IFS= read -r f; do [ -n "$f" ] && kv+=("conflict=$f"); done <<< "$conflicts"
    publish_note "$cand" "$staging_sha" "$(note_body conflict "${kv[@]}")"
    return "$EX_CONFLICT"
  fi
  local cand_sha cand_tree
  cand_sha="$(git -C "$cand" rev-parse HEAD)"
  cand_tree="$(git -C "$cand" rev-parse 'HEAD^{tree}')"

  local proof="$work/proofs/$stamp-${cand_sha:0:12}"
  local log="$work/proofs/$stamp-${cand_sha:0:12}.train.log"
  say "proving ${cand_sha:0:12} (tree ${cand_tree:0:12}); transcript: $log"
  : "${RCH_CARGO_WRAPPER_BYPASS:=1}"
  : "${CARGO_TARGET_DIR:=$work/target}"
  export RCH_CARGO_WRAPPER_BYPASS CARGO_TARGET_DIR
  local proof_rc=0
  (cd "$cand" && bash scripts/local_proof.sh --output "$proof") > "$log" 2>&1 || proof_rc=$?
  say "local_proof.sh exited $proof_rc"

  local base_kv=("main=$main_sha" "staging=$staging_sha" "candidate=$cand_sha" "tree=$cand_tree" "proof=$proof")
  if [ ! -f "$proof/manifest.txt" ] \
      || ! bash "$cand/scripts/local_proof_verify.sh" --repository "$cand" "$proof" >> "$log" 2>&1; then
    say "UNVERIFIABLE: the proof bundle did not verify; nothing promoted"
    publish_note "$cand" "$staging_sha" "$(note_body unverifiable "${base_kv[@]}" "local_proof_exit=$proof_rc")"
    return "$EX_UNVERIFIABLE"
  fi
  local verdict check_exit stable proved_commit
  verdict="$(sed -n 's/^verdict=//p' "$proof/manifest.txt")"
  check_exit="$(sed -n 's/^check_exit=//p' "$proof/manifest.txt")"
  stable="$(sed -n 's/^tree_stable=//p' "$proof/manifest.txt")"
  proved_commit="$(sed -n 's/^commit=//p' "$proof/manifest.txt")"
  base_kv+=("check_exit=$check_exit" "tree_stable=$stable"
    "started_utc=$(sed -n 's/^started_utc=//p' "$proof/manifest.txt")"
    "finished_utc=$(sed -n 's/^finished_utc=//p' "$proof/manifest.txt")")

  if [ "$verdict" = pass ] && [ "$check_exit" = 0 ] && [ "$stable" = true ] \
      && [ "$proved_commit" = "$cand_sha" ]; then
    if [ "$push" -eq 0 ]; then
      say "PASS (--no-push): ${cand_sha:0:12} would be promoted to $main_branch"
      publish_note "$cand" "$cand_sha" "$(note_body pass "${base_kv[@]}")"
      return "$EX_NOTHING"
    fi
    if ! git -C "$cand" push --quiet "$remote_url" "$cand_sha:refs/heads/$main_branch" 2>>"$log"; then
      say "STALE: $main_branch moved during the proof; nothing promoted (rerun the train)"
      return "$EX_STALE"
    fi
    say "PROMOTED: $main_branch ${main_sha:0:12} -> ${cand_sha:0:12}"
    publish_note "$cand" "$cand_sha" "$(note_body pass "${base_kv[@]}")"
    return "$EX_NOTHING"
  fi

  local fails
  fails="$(grep -a '^FAIL ' "$proof/check.stdout.log" 2>/dev/null | head -n 40)"
  local -a fail_kv=()
  local line
  while IFS= read -r line; do [ -n "$line" ] && fail_kv+=("fail: $line"); done <<< "$fails"
  if [ "$verdict" = void ]; then
    say "VOID: the tree moved during the proof; nothing promoted"
    publish_note "$cand" "$staging_sha" "$(note_body void "${base_kv[@]}")"
    return "$EX_VOID"
  fi
  say "RED: check.sh exited $check_exit on ${cand_sha:0:12}; $staging_branch bounced"
  printf '%s\n' "$fails" | sed 's/^/    /'
  publish_note "$cand" "$staging_sha" "$(note_body red "${base_kv[@]}" "${fail_kv[@]}")"
  return "$EX_RED"
}

audit_train() {
  [ -n "$since" ] || die "audit requires --since REV (the adoption point)"
  refresh_mirror
  local since_sha main_sha
  since_sha="$(git -C "$mirror" rev-parse --verify --quiet "$since^{commit}")" \
    || die "cannot resolve --since $since in the mirror"
  main_sha="$(git -C "$mirror" rev-parse --verify --quiet "refs/heads/$main_branch^{commit}")" \
    || die "remote has no $main_branch branch"
  git -C "$mirror" merge-base --is-ancestor "$since_sha" "$main_sha" \
    || die "--since $since is not an ancestor of $main_branch"

  local proven=0 unproven=0 violations=0 m note offenders
  while IFS= read -r m; do
    [ -n "$m" ] || continue
    note="$(git -C "$mirror" notes --ref="$NOTES_REF" show "$m" 2>/dev/null || true)"
    if grep -qx 'verdict=pass' <<< "$note"; then
      proven=$((proven + 1))
      continue
    fi
    unproven=$((unproven + 1))
    offenders="$(git -C "$mirror" log --format='%h%x09%ae%x09%s' "$m^1..$m" 2>/dev/null \
      | MT_AUTHOR_PATTERN="$author_pattern" awk -F '\t' '$2 ~ ENVIRON["MT_AUTHOR_PATTERN"]')"
    if [ -n "$offenders" ]; then
      violations=$((violations + $(printf '%s\n' "$offenders" | wc -l)))
      printf 'VIOLATION first-parent %s carries unproven commits by the toolchain-less identity:\n' "${m:0:12}"
      printf '%s\n' "$offenders" | sed 's/^/    /'
    elif [ "$strict" -eq 1 ]; then
      violations=$((violations + 1))
      printf 'VIOLATION first-parent %s has no pass note (--strict)\n' "${m:0:12}"
    else
      printf 'UNPROVEN first-parent %s %s\n' "${m:0:12}" \
        "$(git -C "$mirror" log -1 --format='%ae %s' "$m")"
    fi
  done < <(git -C "$mirror" rev-list --first-parent --reverse "$since_sha..$main_sha")

  printf 'audit: %s..%s first-parent commits: proven=%d unproven=%d violations=%d\n' \
    "${since_sha:0:12}" "${main_sha:0:12}" "$proven" "$unproven" "$violations"
  [ "$violations" -eq 0 ] || return 1
  return 0
}

case "$command" in
  run) run_train; exit $? ;;
  audit) audit_train; exit $? ;;
esac
