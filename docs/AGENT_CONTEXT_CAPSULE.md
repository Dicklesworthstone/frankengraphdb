# Local Agent Context Capsule

Status: **live advisory tooling** on unreleased `main`.

This document defines the local, credential-free handoff produced by
`scripts/agent_context.sh` and independently checked by
`scripts/agent_context_verify.sh`.

The capsule exists for a practical reason: a coding agent often needs one
self-contained snapshot containing source, recent history, current Beads views,
and enough provenance to know exactly what it is looking at. Fetching those
pieces separately is slower, easier to race, and encourages agents to mistake a
partial observation for repository truth.

The capsule is deliberately **not** a product gate. Its successful production or
verification says nothing about formatting, compilation, tests, invariants, or
runtime behavior. The authoritative repository verdict remains:

```bash
bash scripts/check.sh
```

## Produce and verify

From a clean checkout:

```bash
bash scripts/agent_context.sh \
  --require-br \
  --output /tmp/fgdb-agent-context

bash scripts/agent_context_verify.sh /tmp/fgdb-agent-context
```

The output directory:

- must be outside the repository worktree;
- must have an existing parent;
- must not already exist;
- is never removed or overwritten by either script.

Run the semantic controls with:

```bash
bash scripts/agent_context_selftest.sh
```

The self-test retains and prints its clean, dirty, and deliberately corrupted
fixtures. This matches the repository's no-deletion doctrine and leaves a failed
control state available for inspection.

## Clean and dirty trees

Clean export is the default. A dirty worktree is refused because an exact Git
bundle alone would silently omit the state an agent is actually reviewing.

When dirty export is intentional:

```bash
bash scripts/agent_context.sh \
  --allow-dirty \
  --output /tmp/fgdb-agent-context-dirty
```

The dirty capsule includes:

- `worktree.patch`, produced by `git diff --binary HEAD` and therefore covering
  staged and unstaged tracked changes;
- `untracked-files.txt`, containing names only;
- `worktree-stability-proof.patch`, recomputed at the end of export and required
  to be byte-identical to `worktree.patch`.

Untracked file contents are excluded on purpose. Automatically sweeping unknown
files into an agent handoff can capture credentials, local datasets, generated
artifacts, or unrelated work. An agent that needs an untracked file should ask
for that file explicitly.

The producer also rechecks `HEAD` and the complete porcelain status after all
outputs are written. A moving commit or worktree makes the export fail rather
than producing a plausible mixed-time snapshot.

## Capsule contract

| File | Meaning |
|---|---|
| `manifest.txt` | Version, repository, exact commit, ref, dirty state, Beads mode, and authority boundary. |
| `repository.bundle` | Credential-free Git history rooted at the exact captured `HEAD`. |
| `git-bundle-verify.txt` | Producer-side Git bundle verification transcript. |
| `tracked-source.tar.gz` | Deterministic `git archive` of the exact captured commit. |
| `tracked-files.txt` | Exact tracked path inventory from that commit. |
| `recent-commits.tsv` | Commit, author time, author, and subject for the requested recent-history window. |
| `git-status.txt` | Porcelain worktree state captured before export. |
| `SHA256SUMS` | Strict checksum inventory for every other regular file in the capsule. |
| `issues.jsonl` | Tracked Beads exchange projection, when present and enabled. |
| `br-*.json` | Read-only live Beads views when `br` is available. |
| `worktree.patch` | Dirty tracked state, only with `--allow-dirty`. |
| `untracked-files.txt` | Dirty untracked names, never their contents. |
| `worktree-stability-proof.patch` | End-of-export tracked-state control for a dirty capsule. |

`manifest.txt` format version 1 is line-oriented `key=value`. It is intentionally
small enough to inspect without a parser dependency. Values are metadata, not
shell input; consumers must parse keys rather than source the file.

The declared Beads modes are:

- `br+jsonl`: tracked JSONL plus successful read-only `br` views;
- `jsonl-only`: tracked JSONL was present but `br` was unavailable;
- `absent`: no tracked Beads JSONL existed;
- `disabled`: the caller passed `--no-beads`.

Use `--require-br` when current scheduler views are required. Without it, a
machine lacking `br` still produces a useful source capsule, but the manifest
makes the weaker Beads observation explicit.

## Independent verification

The verifier does not trust the producer's success token. It independently
checks:

1. every required file exists;
2. the capsule contains no symlinks;
3. the manifest version, commit syntax, dirty flag, Beads mode, and
   advisory-only authority statement are valid;
4. `SHA256SUMS` names the exact regular-file inventory, with no omitted or extra
   file;
5. every checksum matches;
6. the Git bundle can be parsed and advertises the manifest's exact commit;
7. the tracked-source archive contains no `.git` directory;
8. clean and dirty evidence have the correct mutually exclusive shape;
9. dirty start/end patches are identical;
10. each Beads mode carries exactly its required evidence family.

A successful verifier run emits:

```text
AGENT_CONTEXT_VERIFIED
path=...
commit=...
dirty=...
beads=...
```

The token is anchored and machine-readable, but it is only a capsule verdict.
It must never be translated into `ALL GATES GREEN` or any claim about database
correctness.

## Consuming the source

A Git-aware agent can reconstruct history without repository credentials:

```bash
git clone /tmp/fgdb-agent-context/repository.bundle /tmp/fgdb-work
```

A source-only consumer can unpack the exact tracked tree:

```bash
mkdir /tmp/fgdb-source
tar -xzf /tmp/fgdb-agent-context/tracked-source.tar.gz \
  -C /tmp/fgdb-source
```

For a dirty capsule, apply `worktree.patch` only after checking out the manifest
commit. The untracked-name list is informational; the capsule intentionally
contains no bytes to recreate those files.

## Trust and no-claim boundary

The capsule provides internal integrity and exact-state attribution, not
publisher authenticity. `SHA256SUMS` is stored beside the files it covers, so a
party capable of replacing the whole capsule can also replace its checksums.
Future signed distribution belongs to the registered release and external
verifier workstreams; it must not be improvised here.

The canonical authorities remain separate:

- Git is authoritative for committed source and history.
- The live Beads database is authoritative for current tracker state; JSONL and
  `br-*.json` are exchange views captured at one local instant.
- `bash scripts/check.sh` is authoritative for the repository's product verdict.
- The capsule manifest is authoritative only for what this advisory package
  claims to contain.

This separation is load-bearing. It lets agents acquire context cheaply without
turning observability infrastructure into a second source of truth.
