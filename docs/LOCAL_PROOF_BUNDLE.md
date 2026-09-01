# Local Proof Bundle

Status: **live local verification tooling** on unreleased `main`.

`scripts/local_proof.sh` runs the repository-authoritative
`bash scripts/check.sh` on one clean, exact commit and preserves its evidence in
an immutable directory. `scripts/local_proof_verify.sh` independently checks the
bundle without rerunning the expensive gate. The scripts replace no part of the
gate itself and do not depend on GitHub Actions.

## Run and verify

From a clean checkout:

```bash
bash scripts/local_proof.sh --output /tmp/fgdb-proof
bash scripts/local_proof_verify.sh /tmp/fgdb-proof
```

The producer exits:

- `0` and emits `LOCAL_PROOF_PASS` only when `check.sh` exits `0` and the exact
  commit and complete worktree state are unchanged;
- the original nonzero `check.sh` exit and emits `LOCAL_PROOF_RED` when the tree
  is stable but a gate is red or unrun;
- `125` and emits `LOCAL_PROOF_VOID` when `HEAD` or worktree state changes during
  the run.

A red or void proof is still useful evidence. The verifier accepts a
well-formed red or void bundle while preserving its non-pass verdict; it never
translates successful artifact verification into a green product claim.

## Output contract

The output directory must be outside the repository, its parent must exist, and
it must not already exist. The scripts never remove or overwrite evidence.

| File | Meaning |
|---|---|
| `manifest.txt` | Exact commit/ref, timestamps, raw check exit, tree stability, verdict, and authority rule. |
| `check.stdout.log` | Complete stdout from `scripts/check.sh`, including its anchored verdict stream. |
| `check.stderr.log` | Complete stderr diagnostics from `scripts/check.sh`. |
| `check-exit.txt` | Unmodified `check.sh` exit code. |
| `commit-before.txt` / `commit-after.txt` | Exact Git commit at both boundaries. |
| `status-before.txt` / `status-after.txt` | Full porcelain status at both boundaries. |
| `command.txt` | The one permitted proof command: `bash scripts/check.sh`. |
| `tools.txt` | Git, host, Rust, Cargo, and Beads tool versions or explicit unavailability. |
| `SHA256SUMS` | Exact checksum inventory for every other regular file. |

The proof starts only from a clean tree. This is stricter than a diagnostic
`check.sh` invocation because a reusable proof needs one unambiguous source
identity. The underlying gate still performs its own tree tripwires; the wrapper
adds before/after evidence and an explicit void state.

## Independent verification

The verifier checks:

1. every required file exists and no symlink is present;
2. the checksum file names the exact regular-file inventory;
3. every SHA-256 checksum matches;
4. manifest values and the captured command are valid;
5. manifest commit and check exit match their standalone records;
6. `tree_stable` agrees with the before/after commit and worktree evidence;
7. a pass has exit `0`, a clean stable tree, an anchored green summary, and no
   anchored `RED`, `UNRUN`, or `FAIL` line;
8. a red has a nonzero check exit, stable tree, and no green summary;
9. a void is actually unstable.

A verified pass emits:

```text
LOCAL_PROOF_VERIFIED
path=...
commit=...
check_exit=0
tree_stable=true
verdict=pass
```

`LOCAL_PROOF_VERIFIED` means only that the captured proof is internally
consistent. Read the `verdict` field to learn what `check.sh` established.

## Semantic controls

Run:

```bash
bash scripts/local_proof_selftest.sh
```

The retained fixture proves four independent cases:

- a stable zero-exit run verifies as `pass`;
- a stable exit-7 run is preserved and verifies as `red`;
- a nominally green command that changes a tracked file becomes exit `125` and
  verifies as `void`;
- a checksum-invalid copy is rejected.

The fixture and all proof directories are retained and printed for inspection,
matching the repository's no-deletion doctrine.

## Relationship to the agent context capsule

The two local packages have different authority:

- `scripts/agent_context.sh` is a cheap advisory source/history/Beads handoff.
- `scripts/local_proof.sh` is an expensive verdict capture around the canonical
  repository gate.

An agent can verify both, clone `repository.bundle` from the context capsule,
and then require a proof whose manifest commit matches the capsule commit. They
remain separate so refreshing context cannot silently manufacture or inherit a
product verdict.

## Trust and no-claim boundary

The checksum inventory detects accidental or partial mutation, not replacement
by a malicious distributor who can rewrite the entire directory. Signed
publication belongs to the registered release and external-verifier
workstreams.

The proof does not attest an individual test beyond what `scripts/check.sh`
reports, does not replace raw gate logs, does not create a release, and does not
make a red or void run green. Its purpose is exact attribution, durable local
diagnostics, and cheap downstream verification when hosted CI is unavailable or
undesirable.
