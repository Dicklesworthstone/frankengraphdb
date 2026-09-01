# Local Agent Context Capsule

Status: **live advisory tooling**, format v2, on unreleased `main`.

`scripts/agent_context.sh` produces a credential-free repository handoff. `scripts/agent_context_verify.sh` verifies it deeply, and `scripts/agent_context_checkout.sh` materializes a verified detached checkout. None of these scripts is a product gate; the authoritative repository verdict remains:

```bash
bash scripts/check.sh
```

## Produce, verify, and consume

From a clean checkout:

```bash
bash scripts/agent_context.sh \
  --require-br \
  --output /tmp/fgdb-agent-context

bash scripts/agent_context_verify.sh \
  --scratch /tmp/fgdb-agent-context-verify \
  /tmp/fgdb-agent-context

bash scripts/agent_context_checkout.sh \
  --verify-scratch /tmp/fgdb-agent-context-checkout-verify \
  /tmp/fgdb-agent-context \
  /tmp/fgdb-work
```

The capsule, verifier scratch, and checkout directories must not already exist. They are never removed or overwritten. The resulting checkout is detached at the bundled commit and has no Git remote.

Run retained mutation-sensitive controls with:

```bash
bash scripts/agent_context_selftest.sh
```

## Clean and dirty trees

Clean export is the default. A dirty tree is refused unless the caller passes `--allow-dirty`.

A dirty format-v2 capsule contains:

- `worktree.patch`, produced by `git diff --binary HEAD`;
- `untracked-files.txt`, containing names only;
- `worktree-stability-proof.patch`, recomputed at the end of export;
- `git-status-stability-proof.txt`, the complete end-of-export porcelain status.

The producer requires the start/end tracked patch and status to remain byte-identical. `HEAD` and its committed tree must also remain unchanged.

Untracked contents are deliberately excluded. Automatically sweeping unknown files could capture credentials, local datasets, generated artifacts, or unrelated work. `agent_context_checkout.sh --apply-dirty` can reconstruct the tracked patch only; it prints the retained untracked-name inventory when one exists.

## Format-v2 contract

| File | Meaning |
|---|---|
| `manifest.txt` | Exact format, repository, commit, committed tree, bundle ref, dirty state, Beads mode, and authority boundary. |
| `repository.bundle` | Credential-free Git history advertising exactly the captured commit as `HEAD`. |
| `tracked-source.tar.gz` | Deterministic `git archive` of that exact commit. |
| `tracked-files.txt` | Exact tracked-path inventory derived from the commit. |
| `recent-commits.tsv` | Exact requested history window derived from the bundled commit. |
| `git-status.txt` | Initial complete porcelain worktree state. |
| `SHA256SUMS` | Exact checksum inventory for every other regular file. |
| `issues.jsonl` | Tracked Beads exchange projection, when enabled and present. |
| `br-*.json` | Read-only live Beads views, when `br` is available. |
| dirty evidence | Tracked patch, untracked names, and start/end stability witnesses. |

The manifest is strict line-oriented `key=value`; consumers parse it and never source it as shell input. Format v2 adds `tree` and `bundle_ref=HEAD`. The verifier continues to accept the prior v1 layout for migration, but new exports always use v2.

Declared Beads modes are:

- `br+jsonl`: tracked JSONL plus successful read-only `br` views;
- `br-only`: live read-only `br` views without tracked JSONL;
- `jsonl-only`: tracked JSONL without a usable `br` binary;
- `absent`: no `.beads` directory;
- `unavailable`: `.beads` existed but neither evidence family was available;
- `disabled`: the caller passed `--no-beads`.

Use `--require-br` when scheduler views are mandatory.

## Deep independent verification

Checksum validation alone is not sufficient: a party could replace both an archive and its neighboring checksum. The v2 verifier therefore derives provenance from the Git bundle itself.

It checks:

1. exact manifest keys, exact regular-file inventory, and no symlinks;
2. exact checksum coverage and every SHA-256 value;
3. one bundle head, named `HEAD`, at the manifest commit;
4. bundle import into an isolated retained repository;
5. manifest tree equality with the bundled commit tree;
6. byte-identical recomputation of `tracked-source.tar.gz` from `git archive`;
7. byte-identical recomputation of `tracked-files.txt` and `recent-commits.tsv`;
8. clean/dirty mutual exclusion and dirty patch applicability;
9. safe relative untracked paths;
10. exact Beads evidence closure for the declared mode.

The mutation controls prove rejection of:

- a checksum-invalid capsule;
- a checksum-consistent source archive unrelated to the bundle;
- checksum-consistent fabricated history;
- duplicate manifest keys;
- an undeclared extra file.

Success emits:

```text
AGENT_CONTEXT_VERIFIED
path=...
scratch=...
commit=...
tree=...
dirty=...
beads=...
```

This is a capsule verdict only. It must never be translated into `ALL GATES GREEN` or any claim about database correctness.

## Checkout semantics

`agent_context_checkout.sh` always runs the deep verifier first. It then:

1. creates a new repository at the destination;
2. fetches only `HEAD` from the verified bundle;
3. checks out the exact manifest commit detached;
4. proves the checkout has no remotes and is clean;
5. optionally applies and re-verifies the tracked dirty patch.

The verifier scratch and checkout are retained. No repository credential is copied because no source `.git` directory or remote configuration is exported.

## Trust and no-claim boundary

The bundle makes committed source/history self-authenticating by Git object identity and makes the adjacent views internally consistent. It does not authenticate the distributor: a party replacing the entire bundle can produce a different internally valid bundle. Signed distribution belongs to the registered release and external-verifier workstreams.

Canonical authorities remain separate:

- Git is authoritative for committed source and history.
- The live Beads database is authoritative for current tracker state; JSONL and `br-*.json` are captured views.
- `bash scripts/check.sh` is authoritative for the product verdict.
- The capsule manifest is authoritative only for the advisory package it describes.

This separation lets agents acquire exact context cheaply without creating a second source of product truth.
