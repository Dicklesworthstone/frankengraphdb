# Negative Evidence

The memorial `AGENTS.md` designates for doctrine violations:

> These are the constitutional, non-negotiable rules from §1 of the plan.
> Violating any of them is a revert, memorialized in `docs/NEGATIVE_EVIDENCE.md`.

Owner bead: `fgdb-negative-evidence-ledger-does-not-exist-m172`.
Gate: `scripts/g0_negative_evidence_e2e.sh` (registered in `registries/checker_index.toml`).

## What this file is for, and why an empty one would be worse than none

The value of negative evidence is that it is **aggregated**. Every entry below was
already recorded per-incident, in a bead, at the moment it was fixed. None of that
made the *family* visible: a reader of the doctrine saw eight rules and no evidence
that the project had ever caught itself breaking them, and each new agent
rediscovered the same class from scratch. Four instances of one bug were fixed in
four different places before anyone wrote down that they were one bug
(`tools/registry-check/tests/metamorphic.rs`, commit `ed70996`).

So the load-bearing column is not `bead` or `repair`. It is **signature** — the
generalized shape, stated so the next instance is *recognized* rather than
rediscovered.

## A defect in the clause itself, found while populating this file

The clause is keyed to an event this project has never performed. Measured over
all 705 commits reachable at `e0bddd3`:

| population | count |
| --- | --- |
| commits produced by `git revert` | 1 |
| commits with any revert semantics in the subject | 3 |
| of those, reverting a **doctrine violation** | **0** |

All three are beads bookkeeping (`46e654e`, `7f36702`/`3a7248f`, `1994b8e`).

The reason is that `AGENTS.md` forbids the mechanism its own clause requires. Under
**Backwards Compatibility** it mandates: *"Never create compatibility shims or
wrappers for deprecated APIs. Just fix the code directly."* This project therefore
repairs violations **in place** and always has. A ledger that memorialized only
"reverts" would be empty forever while the violations kept happening — decorative
by construction.

**This file is therefore keyed to the event that actually occurs: a doctrine or
enforcement claim that was found to be false.** The revert-keyed reading is still
enforced (§ Reverts below), so the literal clause is not unguarded — it is simply
not the load-bearing population. Re-wording the `AGENTS.md` sentence to match is
filed as residue on the owner bead.

## Scope rule

An entry is **required** when a doctrine or enforcement claim *reported a verdict
it had not earned* — a gate, checker, pin, lint, or documented guarantee that
presented as passing (or as a meaningful failure) while not performing the check it
named.

An entry is **not** required for delivery work, catalog errata, or adjudications:
repairing a wrong row that the checker correctly reported is the system working.

## Entry schema

Each entry is a `### NE-nnnn` heading followed by these fields, one per line. The
gate parses them structurally; `doctrine` must resolve in
`registries/constitution.toml`, `bead` in `.beads/issues.jsonl`, and `repair` must
be a commit reachable in this repository.

---

## Ledger

### NE-0001 — Workspace `unsafe_code = "forbid"` was proved by the manifest's own comment

- **doctrine**: FG-CON-02
- **bead**: fgdb-regcheck-forbid-substring-vacuous-u9zp
- **repair**: 30fed33
- **claimed**: the workspace forbids unsafe code, checked in CI
- **actual**: `Cargo.toml` line 10 is a prose comment containing the literal string, so `text.contains(…)` was satisfied no matter what the lint table said; deleting both live lint lines left the check passing. It could not fail on this repository at all.
- **caught_by**: metamorphic relation — "what would this checker do if it were broken?"
- **signature**: a substring test standing in for structural parsing, where the corpus contains the substring in prose.

### NE-0002 — A cosmetic requote took the crate census from 14 to zero

- **doctrine**: FG-CON-02
- **bead**: fgdb-regcheck-member-enum-quote-scan-lx43
- **repair**: 30fed33
- **claimed**: every workspace member scanned for unsafe sites
- **actual**: member enumeration was a line/quote scan; TOML literal quotes in the members array took `crates_scanned` to ZERO, and every "0 sites, 0 orphans, pass" below it was quantified over nothing.
- **caught_by**: metamorphic relation over the manifest reader
- **signature**: a zero result reported as a pass, with no control proving the reader can find anything.

### NE-0003 — Attribute candidacy read from the trimmed line prefix

- **doctrine**: FG-CON-02
- **bead**: fgdb-regcheck-scansites-line-anchored-ds45
- **repair**: a4e7ec8
- **claimed**: every `unsafe` attribute site is scanned and ledgered
- **actual**: `scan_sites` only saw attributes that *begin* a line — one sharing a line was invisible — and counted attributes inside block comments as real.
- **caught_by**: the same metamorphic sweep that found NE-0001/0002
- **signature**: line-anchored pattern matching standing in for parsing the source as source.

### NE-0004 — A commented-out match arm satisfied a bijection

- **doctrine**: FG-CON-02
- **bead**: fgdb-regcheck-commented-arm-counts-live-ctv8
- **repair**: a4e7ec8
- **claimed**: active logical object kinds are in bijection with live `refs.rs` arms
- **actual**: the arm binding accepted a COMMENTED-OUT arm as live, satisfying a bijection the compiler could not see.
- **caught_by**: metamorphic sweep
- **signature**: reading masked source (comments, `cfg`) as live code.

### NE-0005 — Two readers of "does this crate relax unsafe_code"

- **doctrine**: FG-CON-02
- **bead**: fgdb-regcheck-two-readers-unsafe-relax-6amm
- **repair**: 2b0e005
- **claimed**: one enforced answer to whether a crate relaxes the workspace forbid
- **actual**: `topology.rs` kept a second, weaker reader of the same fact. Where two pieces of code answer one question they drift, and the weaker one wins by being the one that happens to run.
- **caught_by**: audit following NE-0001
- **signature**: two readers of one fact. The rule that removes the class is *one reader per fact*.

### NE-0006 — Root-forbid check was whole-line equality, and an unreadable root passed

- **doctrine**: FG-CON-02
- **bead**: fgdb-regcheck-root-forbid-line-equality-fhnr
- **repair**: 2b0e005
- **claimed**: every crate root carries `#![forbid(unsafe_code)]`
- **actual**: `root_forbids_unsafe` was whole-line string equality, and a crate root that could not be read was **silently skipped** rather than failing.
- **caught_by**: audit following NE-0001
- **signature**: an I/O error treated as a pass. An unreadable input must never be indistinguishable from a compliant one.

### NE-0007 — The `cfg_attr` bypass in the unsafe-site scanner

- **doctrine**: FG-CON-02
- **bead**: fgdb-w1-unsafe-ledger-icp
- **repair**: 4f58cc3
- **claimed**: CI rejects an unledgered unsafe site
- **actual**: a `cfg_attr`-wrapped attribute bypassed the scanner entirely, so an unsafe site could exist with no ledger row and no violation.
- **caught_by**: adversarial read of the scanner against its own contract
- **signature**: conditional-compilation forms not normalized before scanning.

### NE-0008 — `checker_index status = "live"` meant `Path::is_file()`

- **doctrine**: FG-CON-11
- **bead**: fgdb-checker-index-live-is-only-file-existence-tl0o
- **repair**: a4e7ec8
- **claimed**: "CI cross-checks that every ID has a live checker"; "no subsystem ships against an unenforced invariant"
- **actual**: `live` was proved by file existence alone. A checker could be registered live, be cited by a clause as its enforcement mechanism, be invoked by no gate, and contain no code capable of failing — and every registry gate stayed green.
- **caught_by**: asking what `live` actually asserted
- **signature**: existence of an artifact standing in for the artifact doing its job. Repaired by three independent reads: REGISTERED, INVOKED, CAN FAIL.

### NE-0009 — `proof_lanes status = "checked"` meant `Path::is_file()`

- **doctrine**: FG-CON-11
- **bead**: fgdb-proof-lane-checked-is-only-file-existence-0f1l
- **repair**: 60439d7
- **claimed**: proof-class claims are discharged by a checked formal artifact
- **actual**: `checked` was file existence. No Lean or TLC ever ran; a proof containing `sorry` would have passed.
- **caught_by**: the NE-0008 sweep, applied to the sibling registry
- **signature**: the NE-0008 class, one registry over — the same bug found twice because the first was fixed where it was noticed.

### NE-0010 — Nothing guarded stub → live promotion of an invariant clause

- **doctrine**: FG-CON-11
- **bead**: fgdb-clause-promotion-to-live-is-unguarded-nllh
- **repair**: ce7fa74
- **claimed**: an invariant is promoted to enforced only after its checker exists
- **actual**: `negative_test_entrypoint` was enforced as a non-empty string; nothing checked that promotion was earned.
- **caught_by**: the NE-0008 sweep
- **signature**: a field's *presence* enforced instead of its *meaning*.

### NE-0011 — All twenty FG-INV clauses were stub, so the cross-check passed over nothing

- **doctrine**: FG-CON-11
- **bead**: fgdb-fginv-spine-zero-live-checkers-v05b
- **repair**: 4710fd6
- **claimed**: AGENTS.md — "CI cross-checks that every ID has a live checker"
- **actual**: zero of the twenty had a live checker. The cross-check was true and empty.
- **caught_by**: counting the population the law quantified over
- **signature**: a universally-quantified law that is green because its domain is empty. Every such law needs a non-emptiness control.

### NE-0012 — `check.sh` printed ALL GATES GREEN while never running 4 of 7 registered gates

- **doctrine**: FG-CON-11
- **bead**: fgdb-gate-coverage-checksh-omits-live-gates-u7mw
- **repair**: 0ada651
- **claimed**: the runner executes the registered gates
- **actual**: 4 of 7 registered-live gate scripts and 2 of 6 gate binaries were never invoked, under a banner asserting the opposite.
- **caught_by**: comparing the registry's live set against what the runner actually executed
- **signature**: a summary line asserting more than the run performed. Repaired by deriving the runner from the registry.

### NE-0013 — Five scripts presented as gates while in no runner and no registry

- **doctrine**: FG-CON-11
- **bead**: fgdb-orphan-w1-e2e-gates-unregistered-unrun-vuq8
- **repair**: 7f60e5f
- **claimed**: by their own contents — `set -euo pipefail`, pinned counts, PASS/FAIL counters
- **actual**: no runner and no registry knew they existed; they held six hard-pinned magic numbers between them and had never run.
- **caught_by**: closing `checker_index` in the FILE → ROW direction
- **signature**: a one-directional closure law. Row → file was closed; file → row was not, so an artifact could carry every signal of a gate and be one to nobody.

### NE-0014 — 253 of 404 validator violation codes had never been asserted

- **doctrine**: FG-CON-11
- **bead**: fgdb-validator-laws-never-witnessed-firing-xnxy.1
- **repair**: d848284
- **claimed**: the validator enforces its registered laws
- **actual**: 253 codes were production-reachable and had never been asserted by any test or e2e script. Green from those was not evidence of anything.
- **caught_by**: census of emitted codes against codes asserted anywhere
- **signature**: a law nobody has watched fire is a law nobody knows works. Existence of a code is not evidence the path to it is live.

### NE-0015 — The activation closure passed with 20/20 clauses non-live

- **doctrine**: FG-CON-11
- **bead**: fgdb-regcheck-closure-vacuous-no-control-hp0f
- **repair**: 1b211eb
- **claimed**: the activation closure is enforced
- **actual**: it passed because the only manifest enabled zero clauses — vacuously true with no control to license the zero.
- **caught_by**: the NE-0011 question applied to the closure
- **signature**: same as NE-0011. A zero must be licensed by a control that proves the checker can find something.

### NE-0016 — Shell deliverables were exempt from every quality gate, disabling a concurrency guard

- **doctrine**: FG-CON-04
- **bead**: fgdb-shell-lint-silent-no-op-xi8p
- **repair**: 89f60dd
- **claimed**: the mandated checks cover the deliverables
- **actual**: every `cargo` step is a no-op on non-`.rs`, and `ubs` is worse — on `.sh`/`.jsonl`/`.toml`/`.md` it prints *"nothing was checked (this is NOT a pass)"* and then **exits 0**. Every script under `scripts/` was unlinted. shellcheck then caught SC2033 in the determinism gate: `xargs sha256` where `sha256` was a shell function, so the pipeline digested an empty stream and the "source pin" came back as the constant `sha256("")` on every tree — 64 valid hex chars that passed a length check and made every before/after comparison succeed. The concurrency guard was disabled with no error and no visible symptom.
- **caught_by**: adding `bash -n` + shellcheck as a gate step
- **signature**: a tool that exits 0 on input it cannot process. Two compounding forms: unsupported-input-as-pass, and a digest of an empty stream as a valid pin.

### NE-0017 — claims-lint scanned a static list of three files

- **doctrine**: FG-CON-11
- **bead**: fgdb-claims-lint-scan-set-not-total-nldg
- **repair**: fd7d169
- **claimed**: normative documents are lint-scanned for claim markers
- **actual**: the scan set was a static list of 3 files; a new normative `.md` was silently ignored. The closure walk was also non-recursive.
- **caught_by**: asking what happens to a file nobody added to the list
- **signature**: an enumerated scan set standing in for a derived one — silently incomplete the moment the corpus grows.

### NE-0018 — claims-lint `closure_dirs` was unvalidated data

- **doctrine**: FG-CON-11
- **bead**: fgdb-twob
- **repair**: fd7d169
- **claimed**: the closure covers the normative corpus
- **actual**: `closure_dirs` was unvalidated; narrowing it to `["docs"]` passed every test. The declared roots were the same bug one layer up from NE-0017.
- **caught_by**: mutating the configuration and observing that nothing failed
- **signature**: configuration that selects the domain of a law is part of the law, and must be validated as such.

### NE-0019 — claims-lint validated only the markers that were present

- **doctrine**: FG-CON-12
- **bead**: fgdb-claims-lint-one-directional-unmarked-budgets-sdpv
- **repair**: fd7d169
- **claimed**: every numeric performance budget carries a claim marker
- **actual**: the lint was one-directional — it validated markers that existed and said nothing about 12 numeric budgets in README that carried none.
- **caught_by**: checking the other direction of the law
- **signature**: a one-directional law. Validating present markers is not validating required ones.

### NE-0020 — The bead-provenance preflight exited 0 on a tree that reds four tests it names

- **doctrine**: FG-CON-11
- **bead**: fgdb-bead-provenance-preflight-misses-pin-drift-a5kb
- **repair**: 93657d7
- **claimed**: "Exit 0 = committing this `.beads/issues.jsonl` will not move bead provenance"
- **actual**: it exited 0 on a tree that red all four architecture tests it names — it missed exactly the event it existed to catch.
- **caught_by**: running it against a tree known to be red
- **signature**: a predictor never tested against the event it predicts. Repaired by making it red-prove its own probe on every run.

### NE-0021 — g0 gates aborted on the first failure; one stale assertion hid 92 others

- **doctrine**: FG-CON-11
- **bead**: fgdb-d1d4
- **repair**: 9e2ed85
- **claimed**: the gate reports the tree's conformance
- **actual**: `set -e` plus a `die()` on first failure meant one stale assertion hid 92 others in `g0_identity_e2e.sh`. The reported failure was not the tree's failure set.
- **caught_by**: counting assertions reached versus assertions present
- **signature**: fail-fast in an *auditing* tool. A gate that stops at the first red reports the harness's order, not the tree's state.

### NE-0022 — Six red cargo-test artifacts from one compile error, five binaries never run

- **doctrine**: FG-CON-11
- **bead**: fgdb-m2c1
- **repair**: b77982e
- **claimed**: six test artifacts were evaluated and failed
- **actual**: one compile error made all six read RED while five binaries never ran. No cargo flag recovers this: `--no-fail-fast` recovers running binaries, `--keep-going` recovers nothing.
- **caught_by**: separating "failed" from "never ran" in the runner's own tally
- **signature**: conflating UNRUN with FAILED. An artifact whose binary never executed must not be reported as a measured verdict.

### NE-0023 — The ADR was neither regenerated, byte-compared, nor claims-lint scanned

- **doctrine**: FG-CON-11
- **bead**: fgdb-adr-doc-prose-unchecked-and-drifted-uyt2
- **repair**: 1485dfa
- **claimed**: by its siblings' precedent — generated documents are byte-compared against their registry
- **actual**: `docs/ARCHITECTURE_DECISION_RECORD.md` alone among the three docs was unchecked, and had drifted.
- **caught_by**: comparing each doc against the treatment its siblings receive
- **signature**: an artifact exempt from a law its whole class obeys. Uniformity of treatment is itself checkable.

### NE-0024 — Merge-algebra suites did not constrain their kernel

- **doctrine**: FG-CON-11
- **bead**: fgdb-ml05
- **repair**: 372c319
- **claimed**: the merge algebra is property-tested
- **actual**: three families of suites passed against *any* confusable operator pair — the properties did not distinguish the intended kernel from wrong ones.
- **caught_by**: mutation — substituting a confusable operator and observing green
- **signature**: properties that hold too widely. A suite that cannot fail on a wrong implementation is not testing the implementation.

### NE-0025 — A StrongRef field could declare no reference semantics and pass every gate

- **doctrine**: FG-CON-11
- **bead**: fgdb-refsem-not-forced-by-wire-type-gls4
- **repair**: 563a06b
- **claimed**: durable reference semantics are forced by the declared wire type
- **actual**: `reference_semantics` was not forced by `exact_wire_type`; a StrongRef field could declare none and pass every gate.
- **caught_by**: synthesizing the under-specified row and running the validator on it
- **signature**: a required-by-intent field with no rule making it required.

### NE-0026 — 79 strict construction-order violations were invisible to the checker

- **doctrine**: FG-CON-11
- **bead**: fgdb-suhb
- **repair**: f8b8428
- **claimed**: construction order is validated
- **actual**: a latent sweep found 79 strict violations and 15 self-edges, none visible to the checker.
- **caught_by**: an independent sweep computed outside the checker
- **signature**: a validator whose coverage was never measured against an independent computation of the same property.

### NE-0027 — Four spelled schema bodies produced zero field candidates

- **doctrine**: FG-CON-11
- **bead**: fgdb-8kzt
- **repair**: 0246145
- **claimed**: the census enumerates the source's field candidates
- **actual**: four spelled schema bodies (a10, a12, a13) produced ZERO candidates — a silent census gap reported as completeness.
- **caught_by**: comparing spelled bodies against candidates emitted
- **signature**: a reader's empty output for a non-empty input, with no control distinguishing "nothing there" from "reader missed it".

### NE-0028 — The census silently merged two unions defined in one sentence

- **doctrine**: FG-CON-11
- **bead**: fgdb-census-merges-two-unions-in-one-sentence-801o
- **repair**: 0246145
- **claimed**: each closed union in the source becomes one census row
- **actual**: `census_appendix_source` merged two closed unions defined in one sentence, silently.
- **caught_by**: reading the source spelling against the census output
- **signature**: a parser whose sentence-level assumption is unstated and unchecked; the merge produced a well-formed row, so nothing downstream could notice.

### NE-0029 — Structural source keys mis-parsed on a generic signature containing a pipe

- **doctrine**: FG-CON-11
- **bead**: fgdb-tfow
- **repair**: 0246145
- **claimed**: structural source keys are matched by reconstruction
- **actual**: a generic signature containing a pipe broke the key match, stranding the pipe-bearing owner.
- **caught_by**: the owner failing to resolve
- **signature**: delimiter-splitting without balancing. The repository has two delimiter readers; patching one recovers nothing.

### NE-0030 — FG-INV-12 documentation asserted a false implication

- **doctrine**: FG-CON-11
- **bead**: fgdb-scalar-hash-encoding-doc-ona5
- **repair**: 5ed9d8f
- **claimed**: the scalar docs stated that hash equality implies encoding equality
- **actual**: false as stated, and load-bearing — it is the kind of claim downstream code is written against.
- **caught_by**: reading the invariant's prose against its actual guarantee
- **signature**: a documented guarantee stronger than the implemented one. Prose is a claim surface and drifts silently.

### NE-0031 — CalibrationWindow permitted invalid direct construction

- **doctrine**: FG-CON-09
- **bead**: fgdb-calibration-window-invariant-2zrk
- **repair**: 5ed9d8f
- **claimed**: the constructor rejects invalid windows, so the invariant holds
- **actual**: the constructor rejected them while direct construction did not — the invariant was enforced on one path only.
- **caught_by**: constructing the value by the unguarded path
- **signature**: an invariant enforced at one entry point rather than made structural. A subset of an abstraction standing in for it.

### NE-0032 — Bead-provenance pins were a count over a file every pane writes

- **doctrine**: FG-CON-04
- **bead**: fgdb-lzol
- **repair**: ce12c7d
- **claimed**: the provenance pins detect unauthorized movement of bead provenance
- **actual**: they were counts over `.beads/issues.jsonl`, a shared export any agent rewrites, so any `br create` in any pane invalidated every other pane's justification and reds main for everyone. The quantification domain, not the count form, was the defect.
- **caught_by**: the pins reddening main on work that had not touched them
- **signature**: a pin whose domain is a shared mutable artifact. Content-addressing a multi-writer file makes every writer a false positive; monotone floors are the fix.

### NE-0033 — Registry gates stayed green while the workspace stopped compiling

- **doctrine**: FG-CON-11
- **bead**: fgdb-active-kind-arm-bijection-break-cuvx
- **repair**: 30fed33
- **claimed**: the registry gates certify the tree
- **actual**: 17 active logical kinds against 10 `refs.rs` arms — main did not compile, while every registry gate reported green.
- **caught_by**: building the workspace
- **signature**: registry green is not build green. A registry law cannot certify a property only the compiler decides.

### NE-0034 — `cargo clippy --all-targets -D warnings` was red on main, unnoticed

- **doctrine**: FG-CON-11
- **bead**: fgdb-clippy-all-targets-red-claims-never-loops-k2mj
- **repair**: a4f2ee8
- **claimed**: the mandatory lint gate is green
- **actual**: red on main — `promote_first_clause` in `tests/claims.rs` tripped `never_loop` — and had been for some time, because nothing ran the mandatory gate.
- **caught_by**: running the mandatory gate
- **signature**: a gate declared mandatory that no runner invokes. Declaring a check does not schedule it.

### NE-0035 — Two mandatory gates were red on main simultaneously

- **doctrine**: FG-CON-11
- **bead**: fgdb-zwhh
- **repair**: 89f60dd
- **claimed**: main is green
- **actual**: bead-provenance pins (4 orphans) and `cargo fmt --check` (42 hunks) were both red on main.
- **caught_by**: running the mandatory set rather than trusting it
- **signature**: "main is green" as an inherited belief rather than a measurement.

### NE-0036 — The spine e2e's RED measured its harness, not the spine

- **doctrine**: FG-CON-11
- **bead**: fgdb-g0-spine-e2e-red-measures-harness-not-spine-iy7e
- **repair**: 60439d7
- **claimed**: a red negative gate demonstrates the closure law firing
- **actual**: two gates failed on a fixture stale against the now-required shape — the red proved the harness was out of date, not that the law worked.
- **caught_by**: asking what the failure actually demonstrated
- **signature**: a false RED. Negative evidence must name *why* it failed, or a stale fixture masquerades as a witnessed law.

### NE-0037 — `g0_identity_e2e.sh` was red on a clean tree from stale reservation pins

- **doctrine**: FG-CON-11
- **bead**: fgdb-su5y
- **repair**: 6e88025
- **claimed**: the gate's red reflects the tree
- **actual**: `EXPECT_EXISTING`/`RESERVED_RESERVATION_COUNT` were a stale a14-era snapshot, so the gate red on a clean tree.
- **caught_by**: running it on a clean tree
- **signature**: census constants stale by construction. A pinned count over a growing corpus decays into noise and trains readers to ignore the gate.

### NE-0038 — Identity fixtures stale at an old durable-fields epoch

- **doctrine**: FG-CON-04
- **bead**: fgdb-un4g
- **repair**: 0baf00a
- **claimed**: the identity fixtures pin current behaviour
- **actual**: fixtures stale at durable-fields epoch 69 produced 12 deterministic failures unrelated to any current change.
- **caught_by**: the failures not tracking the change under test
- **signature**: the NE-0037 class in fixture form.

### NE-0039 — `bead_provenance_orphan` could not name a workstream tag

- **doctrine**: FG-CON-11
- **bead**: fgdb-bead-provenance-orphan-workstream-tag-7u5m
- **repair**: 0754359
- **claimed**: the orphan diagnostic identifies why a bead is orphaned
- **actual**: a `w1`-style tag never entered the bet-label position, so the diagnostic could not name it — and 232 records carry such a tag.
- **caught_by**: the diagnostic being unable to explain real records
- **signature**: a diagnostic whose vocabulary does not cover its corpus. The fix is a three-way diagnosis, not a prohibition.

### NE-0040 — A checker census gap: arm-payload field identity and wire-hosted fields

- **doctrine**: FG-CON-11
- **bead**: fgdb-z35a
- **repair**: 4674951
- **claimed**: the checker's census covers the catalog's field population
- **actual**: arm-payload field identity and wire-hosted fields were outside the census, so rows in those shapes were unchecked.
- **caught_by**: measuring the census against the catalog's actual shapes
- **signature**: a checker whose domain is narrower than the corpus it certifies, with no accounting of the difference.

### NE-0041 — `unsafe_ledger` and `validate` self-tests are unreachable from any input

- **doctrine**: FG-CON-11
- **bead**: fgdb-validator-laws-never-witnessed-firing-xnxy
- **repair**: d848284
- **claimed**: every violation code is reachable and therefore witnessable
- **actual**: three codes (`site_scanner_self_test_failed`, `safe_facing_self_test_failed`, `checker_liveness_self_test_failed`) guard a reader against ITSELF — no registry, manifest, or source tree can make one fire. They are the controls that license every zero beneath them, but no input-driven witness for them can exist.
- **caught_by**: attempting to witness every code and finding three that no input reaches
- **signature**: an unwitnessable law is not automatically a defect — but it must be *named* as unwitnessable, or it is indistinguishable from one nobody tried to witness.

---

## Reverts

The literal `AGENTS.md` clause, enforced. Every commit in this repository whose
subject carries revert semantics must appear here with a disposition. None to date
is a doctrine violation; see the finding at the top of this file.

- `46e654e` — `Revert "chore(beads): re-split a03 and a05 on pane2's basis"` — beads bookkeeping; a bead split re-measured and undone. Not a doctrine violation.
- `3a7248f` — `hand pane2 the provenance-churn measurement; revert my stale +1` — beads bookkeeping; a stale pin increment withdrawn. Not a doctrine violation.
- `1994b8e` — `a03 — unions cannot land before the identity kinds; write reverted` — a catalog write withdrawn because a construction-order law forbade it. The law worked; this is the system functioning, not a violation of it.

---

## Accounting

The ledger above is seeded from the measured history at `e0bddd3` and is subject to
the laws in `scripts/g0_negative_evidence_e2e.sh`:

1. every repository-relative path `AGENTS.md` names resolves (references), or is a
   declared prohibition that must NOT resolve;
2. every entry's `doctrine` resolves in `registries/constitution.toml`, `bead` in
   `.beads/issues.jsonl`, and `repair` to a commit in this repository;
3. every revert-semantics commit has a disposition in § Reverts;
4. the entry count never decreases below its declared floor.

**What these laws do not catch**, stated so it is not mistaken for coverage: a
doctrine violation that is found, repaired, and closed without anyone adding an
entry here. Closing that mechanically requires either total accounting over every
closed bead — which in a multi-pane swarm reds main every few minutes and makes
this file a contended write for every agent — or a text classifier over bead prose,
which fails open in the silent direction and would be an instance of the very class
this ledger records. The gate therefore *reports* the unclassified closed-bead
residue as a count without failing on it. Raising that report to a law is filed as
residue on the owner bead.
