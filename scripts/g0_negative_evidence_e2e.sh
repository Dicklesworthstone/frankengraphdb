#!/usr/bin/env bash
# =============================================================================
# g0_negative_evidence_e2e.sh — the doctrine memorial, enforced
# =============================================================================
# Owner bead: fgdb-negative-evidence-ledger-does-not-exist-m172.
#
# AGENTS.md, immediately above the eight doctrine items:
#
#   "These are the constitutional, non-negotiable rules from §1 of the plan.
#    Violating any of them is a revert, memorialized in
#    `docs/NEGATIVE_EVIDENCE.md`."
#
# That file did not exist. Nothing referenced it but the sentence above, so the
# enforcement clause of the doctrine was decorative: the project could violate a
# constitutional rule, repair it, and memorialize nothing, and no gate could tell.
# This script is what makes the sentence load-bearing.
#
# WHY THIS IS NOT FAIL-FAST. `set -e` is deliberately NOT set. fgdb-d1d4 measured
# the cost of the alternative in this exact directory: one stale assertion aborted
# g0_identity_e2e.sh and hid 92 others, so the reported failure set was the
# harness's evaluation order rather than the tree's state. An auditing tool that
# stops at its first red reports less than it knows. Every law below runs, every
# failure is recorded, and the tally is printed from an EXIT trap so that even an
# unexpected abort cannot produce a silent green (fgdb-gate-tallies: a tally at
# the bottom of a file is skipped by any abort above it).
#
# WHY EVERY ZERO IS CONTROLLED. fgdb-fginv-spine-zero-live-checkers-v05b and
# fgdb-regcheck-closure-vacuous-no-control-hp0f are both in the ledger this gate
# guards: a universally-quantified law is green when its domain is empty, and
# "nothing to check" is indistinguishable from "the reader is broken". Each law
# below therefore fails when its own population is empty.
# =============================================================================

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEDGER="$ROOT/docs/NEGATIVE_EVIDENCE.md"
AGENTS="$ROOT/AGENTS.md"
CONSTITUTION="$ROOT/registries/constitution.toml"
BEADS="$ROOT/.beads/issues.jsonl"

# The ledger may only grow. A monotone floor never goes stale upward, so it costs
# no re-freeze when an entry is added, while deleting entries turns this red.
# (fgdb-lzol / NE-0032: an equality pin over a growing artifact makes every
# writer a false positive; floors are the repair.)
LEDGER_ENTRY_FLOOR=44

TALLY_PRINTED=0

# The shared verdict contract (fgdb-udco). This gate was the worst of the ten:
# its failure line was BOTH indented ("    FAIL  ...", so off column 0) AND on
# stderr, so `bash scripts/g0_negative_evidence_e2e.sh > log` produced a log with
# every ok line and no trace of the failure. MEASURED 2026-07-27 on a genuinely
# red run: `grep -c '^FAIL' ` returned 0 even with BOTH streams merged.
# shellcheck source=lib/gate_verdict.sh
. "$ROOT/scripts/lib/gate_verdict.sh"

pass() { gate_pass "$1"; }
fail() { gate_fail "$1"; }

print_tally() {
  [ "$TALLY_PRINTED" -eq 1 ] && return 0
  TALLY_PRINTED=1
  echo
  echo "  negative-evidence gate: $GATE_PASS passed, $GATE_FAIL failed, $GATE_UNRUN unrun"
  if [ "$GATE_FAIL" -ne 0 ] || [ "$GATE_UNRUN" -ne 0 ]; then
    gate_diag "  docs/NEGATIVE_EVIDENCE.md is the memorial AGENTS.md designates;"
    gate_diag "  a failure here means the doctrine's enforcement clause is unbacked again."
  fi
}
gate_init "g0_negative_evidence_e2e" print_tally

echo "== g0 negative-evidence gate =="

# -----------------------------------------------------------------------------
# LAW 0 — the artifact AGENTS.md names exists at all.
# -----------------------------------------------------------------------------
echo "  [law 0] the designated memorial exists"
if [ -f "$LEDGER" ]; then
  pass "docs/NEGATIVE_EVIDENCE.md exists"
else
  fail "docs/NEGATIVE_EVIDENCE.md does not exist — AGENTS.md names it as the memorial for every doctrine violation"
  print_tally
  exit 1
fi
for f in "$AGENTS" "$CONSTITUTION" "$BEADS"; do
  [ -f "$f" ] || { fail "required input missing: ${f#"$ROOT"/}"; print_tally; exit 1; }
done

# -----------------------------------------------------------------------------
# LAW 1 — AGENTS.md self-containment.
#
# Every repository path AGENTS.md names must resolve. This is the law that closes
# the CLASS rather than this one instance: the ledger went missing because nothing
# checked that the paths the doctrine cites exist.
#
# The naive form of this law is WRONG and would invert the doctrine. AGENTS.md
# names `plannerV2.rs`, `strata_improved.rs` and `exec_enhanced.rs` under "NEVER
# create" — requiring those to resolve would mandate the files the doctrine
# forbids. Every extracted token is therefore classified, and an unclassified one
# is a failure rather than a default.
# -----------------------------------------------------------------------------
echo "  [law 1] every repository path AGENTS.md names resolves"

# Prohibitions: named by AGENTS.md as files that must NOT exist. Hardcoding the
# set would rot silently if the prose changed, so the classification is guarded:
# the line naming each one must still carry "NEVER".
PROHIBITED="plannerV2.rs strata_improved.rs exec_enhanced.rs"
for p in $PROHIBITED; do
  if ! grep -F -- "$p" "$AGENTS" | grep -Fq "NEVER"; then
    fail "prohibition classification is stale: AGENTS.md no longer says NEVER about $p"
  fi
done

l1_checked=0
l1_unclassified=0
while IFS= read -r tok; do
  case "$tok" in
    "") continue ;;
  esac
  l1_checked=$((l1_checked + 1))
  case "$tok" in
    # A bare extension used as a file-type word ("a .sh, .jsonl or .md file"),
    # not a path.
    .[a-z0-9]*) [ "${tok#*/}" = "$tok" ] && { continue; } ;;
  esac
  case "$tok" in
    /*)
      # Absolute path outside the repository (the three foundation checkouts).
      # Not a repository-relative claim; recorded, not enforced.
      continue
      ;;
  esac
  is_prohibited=0
  for p in $PROHIBITED; do
    [ "$tok" = "$p" ] && is_prohibited=1
  done
  if [ "$is_prohibited" -eq 1 ]; then
    if [ -e "$ROOT/$tok" ]; then
      fail "AGENTS.md forbids creating $tok, but it exists"
    fi
    continue
  fi
  if [ -e "$ROOT/$tok" ]; then
    continue
  fi
  # A bare basename may name a file that lives in a subdirectory
  # (AGENTS.md: "Appendix F / `invariants.toml`"). The candidate must exist ON
  # DISK, not merely in the index: `git ls-files` reports a tracked file that has
  # been deleted from the working tree, and accepting that would make this law
  # green over a path nobody can open — the same shape as NE-0008, where the
  # existence of a registration stood in for the artifact doing its job.
  if [ "${tok#*/}" = "$tok" ]; then
    esc="$(printf '%s' "$tok" | sed 's/[.[\*^$]/\\&/g')"
    found=0
    while IFS= read -r cand; do
      [ -e "$ROOT/$cand" ] && { found=1; break; }
    done < <(git -C "$ROOT" ls-files | grep -E "(^|/)$esc\$")
    [ "$found" -eq 1 ] && continue
  fi
  fail "AGENTS.md names a repository path that does not resolve: $tok"
  l1_unclassified=$((l1_unclassified + 1))
done < <(grep -oE '`[A-Za-z0-9_./-]+`' "$AGENTS" | tr -d '`' \
           | grep -E '/|\.(md|toml|rs|sh|lock|yaml|jsonl)$' | sort -u)

# CONTROL. If the extractor returns nothing, "no paths named" and "the extractor
# is broken" are indistinguishable, and every verdict above is quantified over
# nothing. AGENTS.md names scripts/check.sh, so zero is never correct.
if [ "$l1_checked" -eq 0 ]; then
  fail "extracted ZERO path tokens from AGENTS.md — a zero here cannot be distinguished from a broken extractor"
else
  [ "$l1_unclassified" -eq 0 ] && pass "all $l1_checked path tokens in AGENTS.md resolve or are declared prohibitions"
fi

# -----------------------------------------------------------------------------
# LAW 2 — the ledger is structurally well formed.
#
# Parsed as structure, never by substring. NE-0001..0004 in this very ledger are
# four instances of a substring or prefix test standing in for structural parsing
# inside a checker whose job is to be unfoolable; repeating that here would be
# the most embarrassing possible defect in this file.
# -----------------------------------------------------------------------------
echo "  [law 2] every ledger entry carries a complete record"

entries="$(grep -cE '^### NE-[0-9]{4} — .+$' "$LEDGER")"
if [ "$entries" -eq 0 ]; then
  fail "ledger contains ZERO entries — an empty memorial satisfies nothing"
elif [ "$entries" -lt "$LEDGER_ENTRY_FLOOR" ]; then
  fail "ledger has $entries entries, below the declared floor of $LEDGER_ENTRY_FLOOR (entries were deleted)"
else
  pass "ledger carries $entries entries (floor $LEDGER_ENTRY_FLOOR)"
fi

dupes="$(grep -oE '^### (NE-[0-9]{4})' "$LEDGER" | sort | uniq -d)"
if [ -n "$dupes" ]; then
  fail "duplicate entry ids: $(echo "$dupes" | tr '\n' ' ')"
else
  pass "entry ids are unique"
fi

# Field completeness, per entry, structurally.
incomplete="$(awk '
  /^### NE-[0-9][0-9][0-9][0-9] / {
    if (id != "") check()
    id = $2; d=b=r=c=a=g=s=0; next
  }
  /^## / { if (id != "") { check(); id="" } next }
  /^- \*\*doctrine\*\*: *[^ ]/   { d=1 }
  /^- \*\*bead\*\*: *[^ ]/       { b=1 }
  /^- \*\*repair\*\*: *[^ ]/     { r=1 }
  /^- \*\*claimed\*\*: *[^ ]/    { c=1 }
  /^- \*\*actual\*\*: *[^ ]/     { a=1 }
  /^- \*\*caught_by\*\*: *[^ ]/  { g=1 }
  /^- \*\*signature\*\*: *[^ ]/  { s=1 }
  END { if (id != "") check() }
  function check() {
    miss=""
    if (!d) miss=miss" doctrine"; if (!b) miss=miss" bead"; if (!r) miss=miss" repair"
    if (!c) miss=miss" claimed"; if (!a) miss=miss" actual"; if (!g) miss=miss" caught_by"
    if (!s) miss=miss" signature"
    if (miss != "") print id " missing:" miss
  }
' "$LEDGER")"
if [ -n "$incomplete" ]; then
  while IFS= read -r line; do fail "incomplete entry: $line"; done <<< "$incomplete"
else
  pass "every entry carries all seven fields"
fi

# -----------------------------------------------------------------------------
# LAW 3 — referential integrity. Every citation resolves.
#
# fgdb-checker-index-live-is-only-file-existence-tl0o (NE-0008) is the reason this
# law reads the referent rather than the syntax: a well-formed citation to nothing
# is exactly the shape of a check that passes without checking.
# -----------------------------------------------------------------------------
echo "  [law 3] every doctrine id, bead and repair commit resolves"

bad_doctrine=0
bad_bead=0
bad_commit=0
n_doctrine=0
n_bead=0
n_commit=0

while IFS= read -r id; do
  n_doctrine=$((n_doctrine + 1))
  grep -qE "^id = \"$id\"" "$CONSTITUTION" || {
    fail "doctrine id does not resolve in registries/constitution.toml: $id"
    bad_doctrine=$((bad_doctrine + 1))
  }
done < <(grep -oE '^- \*\*doctrine\*\*: *[A-Z0-9-]+' "$LEDGER" | awk '{print $NF}' | sort -u)

while IFS= read -r bead; do
  n_bead=$((n_bead + 1))
  grep -Fq "\"id\":\"$bead\"" "$BEADS" || {
    fail "bead does not resolve in .beads/issues.jsonl: $bead"
    bad_bead=$((bad_bead + 1))
  }
done < <(grep -oE '^- \*\*bead\*\*: *[A-Za-z0-9._-]+' "$LEDGER" | awk '{print $NF}' | sort -u)

while IFS= read -r sha; do
  n_commit=$((n_commit + 1))
  if ! git -C "$ROOT" cat-file -e "${sha}^{commit}" 2>/dev/null; then
    fail "repair commit is not reachable in this repository: $sha"
    bad_commit=$((bad_commit + 1))
  fi
done < <(grep -oE '^- \*\*repair\*\*: *[0-9a-f]{7,40}' "$LEDGER" | awk '{print $NF}' | sort -u)

if [ "$n_doctrine" -eq 0 ] || [ "$n_bead" -eq 0 ] || [ "$n_commit" -eq 0 ]; then
  fail "citation extraction returned an empty set (doctrine=$n_doctrine bead=$n_bead commit=$n_commit) — refusing to report referential integrity as checked"
else
  [ "$bad_doctrine" -eq 0 ] && pass "$n_doctrine distinct doctrine ids resolve in constitution.toml"
  [ "$bad_bead" -eq 0 ] && pass "$n_bead distinct beads resolve in .beads/issues.jsonl"
  [ "$bad_commit" -eq 0 ] && pass "$n_commit distinct repair commits are reachable"
fi

# -----------------------------------------------------------------------------
# LAW 4 — the literal AGENTS.md clause: every revert is memorialized.
#
# The population is small (measured: 3 in 705 commits, none a doctrine violation)
# because AGENTS.md's own Backwards Compatibility section mandates repairing in
# place. That finding is recorded in the ledger's preamble. The law is enforced
# anyway and costs nothing: the moment a revert does land, it must be dispositioned.
# -----------------------------------------------------------------------------
echo "  [law 4] every revert-semantics commit has a disposition"

# Dispositions are read STRUCTURALLY, from the § Reverts section only.
#
# This started as `grep -Fq "$sha" "$LEDGER"` over the whole file, and the
# red-proof harness caught it: deleting 46e654e's disposition left the gate GREEN,
# because that sha also appears in the preamble's prose. A substring test standing
# in for structural parsing — NE-0001 through NE-0004 exactly — committed inside
# the gate whose whole purpose is to memorialize that class. Review did not find
# it; mutating the input did.
disposed="$(awk '/^## Reverts$/ {r=1; next} /^## / {r=0} r' "$LEDGER" \
              | sed -nE 's/^- `([0-9a-f]+)`.*/\1/p')"

# A DISPOSITION IS NOT A REVERT. Measured 2026-07-27: this law went red on
# `4d20077 docs(negative-evidence): dispose 649cbf7 — the first revert to reach
# the gate`, which is the commit that DISPOSITIONED a revert. The subject scan
# below matches any commit that *mentions* reverts, so satisfying law 4 emits a
# commit that violates law 4, whose disposition would emit another. An infinite
# regress, and it fires the first time anyone ever obeys the law — which is why
# it lay dormant until tonight: 649cbf7 was the first revert to reach this gate
# at all, so 4d20077 was the first disposition ever written.
#
# The discriminator is STRUCTURAL, in the same spirit as the § Reverts parse
# above: a disposition act touches the ledger and NOTHING else. A real revert
# always restores content somewhere outside it. Anything touching a second path
# stays in the population, so a commit that both reverts and disposes is still
# scanned, and a `Revert "docs(negative-evidence): ..."` — a revert OF the ledger
# — is excluded only if it changes the ledger alone, in which case it is exactly
# the ledger-only edit this rule is about.
#
# The exclusion is REPORTED, never silent: a discriminator that can swallow the
# population without saying so is the failure this whole gate exists to prevent.
n_reverts=0
missing_reverts=0
n_disposition_acts=0
while IFS= read -r sha; do
  [ -z "$sha" ] && continue
  touched="$(git -C "$ROOT" show --pretty=format: --name-only "$sha" | grep -c .)"
  ledger_only="$(git -C "$ROOT" show --pretty=format: --name-only "$sha" \
                   | grep -cx 'docs/NEGATIVE_EVIDENCE.md')"
  if [ "$touched" -ge 1 ] && [ "$touched" -eq "$ledger_only" ]; then
    n_disposition_acts=$((n_disposition_acts + 1))
    continue
  fi
  n_reverts=$((n_reverts + 1))
  printf '%s\n' "$disposed" | grep -Fxq "$sha" || {
    subject="$(git -C "$ROOT" log -1 --pretty=format:%s "$sha")"
    fail "revert commit has no disposition in the ledger: $sha — $subject"
    missing_reverts=$((missing_reverts + 1))
  }
done < <(git -C "$ROOT" log --all --pretty=format:'%h|%s' \
           | grep -iE '\brevert(s|ed|ing)?\b' \
           | cut -d'|' -f1)

echo "    [law 4] $n_disposition_acts ledger-only disposition act(s) excluded from the revert population"

if [ "$n_reverts" -eq 0 ]; then
  fail "found ZERO revert-semantics commits — this repository is known to contain at least one (46e654e), so the scan is broken"
elif [ "$missing_reverts" -eq 0 ]; then
  pass "$n_reverts revert-semantics commit(s) all dispositioned"
fi

# -----------------------------------------------------------------------------
# LAW 5 (report, not law) — the unclassified closed-bead residue.
#
# Stated so that what this gate does NOT enforce cannot be mistaken for coverage.
# A doctrine violation that is found, repaired and closed without an entry here is
# invisible to every law above. Making it visible requires total accounting over
# every closed bead, which in a multi-pane swarm reds main every few minutes and
# makes this file a contended write for every agent. The count is reported so the
# gap is measured rather than assumed.
# -----------------------------------------------------------------------------
echo "  [report] closed-bead residue (advisory, not enforced)"
closed_total="$(grep -c '"status":"closed"' "$BEADS")"
ledgered="$(grep -oE '^- \*\*bead\*\*: *[A-Za-z0-9._-]+' "$LEDGER" | awk '{print $NF}' | sort -u | wc -l)"
echo "    $closed_total closed beads; $ledgered are ledgered here; residue is unclassified by construction"

print_tally
# Three states, not two: an UNRUN law is not a passing law (fgdb-udco).
if [ "$GATE_FAIL" -ne 0 ] || [ "$GATE_UNRUN" -ne 0 ]; then
  exit 1
fi
exit 0
