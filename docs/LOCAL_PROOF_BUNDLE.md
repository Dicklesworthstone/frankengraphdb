# Local Proof Bundle

Status: **live local verification tooling**, format v2, on unreleased `main`.

`scripts/local_proof.sh` runs the repository-authoritative `bash scripts/check.sh` on one clean exact commit and preserves the result without relying on GitHub Actions. `scripts/local_proof_verify.sh` checks the captured bundle without rerunning the expensive gate.

## Run and verify

```bash
bash scripts/local_proof.sh --output /tmp/fgdb-proof

bash scripts/local_proof_verify.sh \
  --repository /path/to/independent/frankengraphdb \
  /tmp/fgdb-proof
```

The producer exits:

- `0` and emits `LOCAL_PROOF_PASS` only when `check.sh` exits `0` and commit, committed tree, and complete worktree state remain unchanged;
- the original nonzero `check.sh` exit and emits `LOCAL_PROOF_RED` when the source state is stable but a gate is red or unrun;
- `125` and emits `LOCAL_PROOF_VOID` when the commit, committed tree, or worktree state moves.

A well-formed red or void bundle is still useful evidence. Verification preserves that verdict and never manufactures a green product claim.

## Format-v2 output contract

The output directory must be outside the repository, its parent must exist, and it must not already exist. The scripts never remove or overwrite evidence.

| File | Meaning |
|---|---|
| `manifest.txt` | Exact commit, committed tree, tracked check-script blob, timestamps, raw exit, stability, verdict, and authority boundary. |
| `check.stdout.log` | Complete stdout from `scripts/check.sh`, including anchored verdicts. |
| `check.stderr.log` | Complete stderr diagnostics. |
| `check-exit.txt` | Unmodified `check.sh` exit code. |
| `commit-before.txt` / `commit-after.txt` | Exact Git commit at both boundaries. |
| `tree-before.txt` / `tree-after.txt` | Exact committed source-tree object at both boundaries. |
| `check-script-blob.txt` | Exact tracked blob id of `scripts/check.sh` at the proof commit. |
| `status-before.txt` / `status-after.txt` | Complete porcelain worktree state at both boundaries. |
| `command.txt` | The only permitted proof command: `bash scripts/check.sh`. |
| `tools.txt` | Git, host, Rust, Cargo, and Beads versions or explicit unavailability. |
| `SHA256SUMS` | Exact checksum inventory for every other regular file. |

New proof bundles use format v2. The verifier accepts v1 for migration, where committed-tree and check-script-blob records were not yet present.

The proof starts only from a clean tree. The underlying gate still performs its own tree tripwires; the wrapper adds exact before/after evidence and a named void state.

## Strict verification

The verifier checks:

1. exact manifest keys and exact regular-file inventory for the declared version;
2. no symlinks, exact checksum coverage, and every SHA-256 value;
3. command, commit, raw exit, timestamps, authority statement, and standalone records;
4. commit/tree/worktree stability agreement;
5. pass: stable state, exit `0`, exactly one anchored green summary, and no anchored non-pass verdict;
6. red: stable state, nonzero exit, `QUALITY GATE RED` on both streams, an anchored failing gate verdict, and no green summary;
7. void: captured source state is actually unstable.

With `--repository DIR`, verification additionally proves that an independently supplied Git object database contains:

- the proof commit;
- the exact committed tree named by the proof;
- the exact `scripts/check.sh` blob named by the proof.

This prevents a checksum-consistent proof directory from silently attributing logs to a fabricated tree or a different gate driver. It still does not authenticate the distributor; signed publication belongs to release/external-verifier work.

Success emits:

```text
LOCAL_PROOF_VERIFIED
path=...
commit=...
tree=...
check_script_blob=...
check_exit=...
tree_stable=...
verdict=...
```

Read `verdict`; successful artifact verification is not synonymous with a green product verdict.

## Mutation-sensitive controls

Run:

```bash
bash scripts/local_proof_selftest.sh
```

The retained fixture proves:

- format-v2 stable pass;
- format-v1 compatibility;
- stable exit-7 red with exit preservation;
- moving-tree void with wrapper exit `125`;
- checksum-invalid rejection;
- checksum-consistent undeclared-file rejection;
- duplicate manifest-key rejection;
- false committed-tree rejection against an independent repository;
- malformed red reporting-contract rejection.

Fixtures are retained and printed for inspection, matching the repository's no-deletion doctrine.

## Relationship to the agent context capsule

The packages have different authority:

- `scripts/agent_context.sh` is a cheap source/history/Beads handoff whose bundle can reconstruct the repository without credentials;
- `scripts/local_proof.sh` is an expensive verdict capture around the canonical gate.

A consumer can deep-verify a context capsule, create a detached no-remote checkout with `agent_context_checkout.sh`, and then verify a proof with `local_proof_verify.sh --repository <that checkout>`. Matching commit/tree identities tie the two packages together without letting either inherit the other's authority.

## No-claim boundary

The proof captures and attributes one invocation of `scripts/check.sh`. It does not replace raw logs, rerun tests, certify an individual assertion beyond the gate's own reporting, create a release, or turn red/void into pass.

`SHA256SUMS` detects partial mutation, while independent-repository verification anchors source identity. Neither authenticates the publisher. Future signatures must enter through the registered release and external-verifier constitution rather than an ad hoc local key path.
