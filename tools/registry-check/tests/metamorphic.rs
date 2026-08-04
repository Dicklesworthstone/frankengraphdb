//! metamorphic.rs — verdict RELATIONS over the workspace-manifest readers.
//!
//! # Why this file exists as a permanent gate
//!
//! Every other suite here asserts a verdict: given this input, expect that
//! violation. That catches a checker which computes the wrong answer. It does
//! not catch a checker which computes the right answer for the wrong reason —
//! and this repository has now produced that failure repeatedly, always with the
//! same shape: a substring or prefix test standing in for structural parsing, in
//! a checker whose entire job is to be unfoolable.
//!
//! Two of those were fixed in the same commit that added this file
//! (`fgdb-regcheck-forbid-substring-vacuous-u9zp`,
//! `fgdb-regcheck-member-enum-quote-scan-lx43`). The decisive fact is in
//! `unsafe_ledger.rs`'s own module doc: the pattern had already been removed
//! once, from the per-member lint read, and it came back one level up in the
//! workspace read. A fix that is not pinned by a test which fails without it has
//! a half-life. These relations are that pin.
//!
//! # The generalisation, which is worth more than any single fix
//!
//! One session produced seven bugs whose signature was "looks exactly like a
//! pass". FOUR OF THE SEVEN WERE IN THE ENFORCEMENT TOOLING ITSELF, and every
//! one of those four was the same mistake: **a substring, prefix, or
//! whole-line-equality test standing in for structural parsing, inside a checker
//! whose entire job is to be unfoolable.** Not four unrelated bugs — one bug,
//! found four times, because each was fixed where it was noticed instead of
//! where it came from:
//!
//! * `u9zp` — the workspace `unsafe_code = "forbid"` check could not fail on
//!   this repository at all. `Cargo.toml` line 10 is a prose comment containing
//!   the literal string, so `text.contains(…)` was satisfied no matter what the
//!   lint table said; deleting both live lint lines left it passing. Every claim
//!   this project makes about memory safety being structural rested on it.
//! * `lx43` — a cosmetic requote of the member roster (TOML literal quotes)
//!   took `crates_scanned` from 14 to ZERO, and every "0 sites, 0 orphans, pass"
//!   below it was then quantified over nothing.
//! * `ds45` — attribute candidacy from the trimmed line prefix: an attribute
//!   sharing a line was invisible, and one inside a block comment was counted.
//! * `ctv8` — the same, one file over, in the active-arm binding: a
//!   commented-out match arm satisfied a bijection the compiler could not see.
//!
//! The lesson that generalises: **structure the reader, not the pattern.** A
//! fix that widens a match leaves the class intact; a fix that parses the input
//! as what it is (a TOML document, a masked source file, an attribute) removes
//! it. And the direction that hides longest is not the false alarm but the
//! silent acceptance — three of the four above were discovered only by asking
//! "what would this checker do if it were broken?", which is exactly what an
//! equivalence relation asks mechanically.
//!
//! Two habits follow, and both are cheap:
//!
//! 1. **One reader per fact.** Where two pieces of code answer the same
//!    question, they will drift, and the weaker one wins by being the one that
//!    happens to run. `unsafe_ledger::mask_source` is the single reader for
//!    "which bytes of this Rust source are live code"; `topology` and `validate`
//!    both consume it rather than re-deriving it. Relation 6 pins that.
//! 2. **A zero result must be licensed.** `scanner_fixture` gives the site
//!    scanner a known non-zero answer, `workspace_has_no_members` makes an empty
//!    roster a violation, and `ClosureReport::licensed` proves the closure
//!    compiler reaches something before a zero-reach run may report a pass. Any
//!    checker that can conclude "nothing found" needs one of these, or it cannot
//!    tell an empty input from a broken self.
//!
//! # The two relations
//!
//! Both are stated over a TRANSFORMATION of an input, never over an absolute
//! expected verdict. That is deliberate: an absolute expectation encodes what
//! the implementation currently does, and drifts with it. A relation encodes
//! what must be true of ANY correct implementation.
//!
//! * **Equivalence.** If `T(x)` means the same thing as `x`, then
//!   `verdict(T(x)) == verdict(x)`. Reformatting a manifest — quoting style,
//!   line breaks, spacing, comments, key order — changes nothing about which
//!   crates exist or which lints apply, so it must change nothing about the
//!   verdict. This is the relation that catches the whole substring-for-
//!   structure class, because every one of those bugs is a reader that is
//!   sensitive to spelling.
//! * **Difference.** If `T(x)` means something DIFFERENT from `x`, then
//!   `verdict(T(x)) != verdict(x)`, and specifically it must gain the violation
//!   naming what changed. A checker that never rejects is the failure mode this
//!   project keeps hitting, and it is invisible to equivalence relations alone.
//!
//! # The non-vacuity control on the suite itself
//!
//! Two empty verdicts are trivially equal, so an equivalence relation over a
//! base that produces no violations proves nothing. [`base_verdict_is_not_vacuous`]
//! pins that the base fixture really does carry an unledgered unsafe site and a
//! non-empty roster. If it ever stops doing so, every relation below becomes
//! decoration and this test says so first.
//!
//! # Scope
//!
//! These relations cover the workspace-manifest readers, the attribute readers,
//! the activation closure and the checker-liveness readers. The same relations
//! applied to the remaining readers in this crate are currently RED and are
//! filed rather than encoded here, so that this suite stays a gate rather than a
//! known-failing list: `fgdb-regcheck-claimslint-allowlist-dead-excludes-5qcg`.
//! As each is fixed, its relation belongs here. Already landed and pinned below:
//! `fgdb-regcheck-two-readers-unsafe-relax-6amm` (relation 6),
//! `fgdb-regcheck-root-forbid-line-equality-fhnr` (relation 7),
//! `fgdb-regcheck-scansites-line-anchored-ds45` (relation 8),
//! `fgdb-regcheck-commented-arm-counts-live-ctv8` (relation 9),
//! `fgdb-regcheck-closure-vacuous-no-control-hp0f` (relation 10),
//! `fgdb-checker-index-live-is-only-file-existence-tl0o` (relation 11),
//! `fgdb-proof-lane-checked-is-only-file-existence-0f1l` (relation 12).
//!
//! Relation 11 is the same bug one level above every other entry in that list.
//! The others are checkers that read a source file the wrong way; it is the
//! predicate that decides whether a checker COUNTS — `status = "live"` proved by
//! `Path::is_file()`, in two readers that had already drifted. AGENTS.md rests
//! every G1–G4 exit gate on that word. Its relations carry an extra witness the
//! others do not need: each mutant is asserted to satisfy the pre-fix predicate
//! verbatim, so the tests state in one place that the old reader could not tell
//! the cases apart and the new one must.
//!
//! The THIRD face of the same fact is
//! `fgdb-clause-promotion-to-live-is-unguarded-nllh`: not "is this checker
//! live" or "is this proof lane checked" but **may this clause be promoted**.
//! Its relations are deliberately NOT duplicated here — they live in
//! `tests/claims.rs`, which owns clause laws and whose whole design is real-
//! registry content plus targeted in-memory mutation, which is what a promotion
//! mutant is. The delegation pin is `claims_promotion_delegates_to_the_liveness_reader`,
//! and it is the same assertion in the same spirit as relation 12's: the clause
//! verdict must CARRY `liveness`'s own words about the checker row, so a second
//! implementation of "is this checker live" fails it and nothing else.
//!
//! Relation 12 is relation 11 ONE ARTIFACT OVER, found thirty minutes later:
//! `proof_lanes.toml`'s `status = "checked"` was the same `Path::is_file()`,
//! against the strongest claim class in the system. It is in this file rather
//! than beside it because the fix is the habit, not the patch — the lane does
//! not re-derive "is a gate running this", it DELEGATES to the reader relation
//! 11 built, and one of its tests pins that by requiring the lane's verdict to
//! carry that reader's own words. Every entry above exists because somebody
//! wrote the second copy.
//!
//! # What the pin is worth, measured (2026-07-26)
//!
//! A fix that no test fails without has a half-life. So the reversion was run,
//! on a depth-matched scratchpad copy, in both halves:
//!
//! * **Restoring the pre-fix lane body** — `assess_lane` replaced by
//!   `root.join(artifact).is_file()`, verbatim — turns **five** tests red:
//!   `a_lane_whose_gate_is_not_live_is_not_checked`,
//!   `a_proof_that_admits_its_conclusion_is_not_checked`,
//!   `a_model_that_checks_no_property_is_not_checked`,
//!   `a_lane_of_an_unreadable_system_is_not_silently_checked`, and
//!   `a_lane_artifact_path_is_checked_before_it_is_joined`. 38 of 43 stay green.
//! * **Deleting the clause-side law** turns **one** test red, in the other
//!   suite: `claims::claims_proof_lane_declared_requires_a_stub_clause`.
//!
//! Under BOTH reversions every control stays green —
//! `proof_lane_base_and_readers_are_licensed`,
//! `proof_lane_checked_arm_is_vacuous_today_and_this_is_what_licenses_it`,
//! `liveness_base_and_readers_are_licensed`, `base_verdict_is_not_vacuous`,
//! `every_shipped_live_row_is_provably_live`, `claims_real_registries_validate`
//! and `claims_proof_lane_manifest_resolves`. That is the reading that matters:
//! the reds are caused by the missing law and not by a harness that broke, and
//! the last two are the sharpest of them — the shipped registries still validate
//! and the pre-existing proof-lane suite still passes with the fix reverted,
//! which is precisely why nothing caught this for as long as it stood.

use registry_check::unsafe_ledger::{self, LEDGER_PATH, VERIFICATION_LANES_PATH};
use std::collections::BTreeSet;
use std::fs;

/// What a run of the checker observably concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Verdict {
    codes: BTreeSet<String>,
    crates_scanned: usize,
}

/// One member crate to materialize under `crates/`.
struct Member {
    dir: String,
    /// `true` — carries an unledgered `allow(unsafe_code)` site.
    unsafe_site: bool,
    /// `true` — carries `[lints] workspace = true`.
    inherits: bool,
    /// Verbatim `src/lib.rs`, when the relation under test varies the source
    /// itself rather than merely whether a site is present.
    source: Option<String>,
}

impl Member {
    fn clean(dir: &str) -> Self {
        Member {
            dir: dir.to_owned(),
            unsafe_site: false,
            inherits: true,
            source: None,
        }
    }

    fn with_unsafe_site(dir: &str) -> Self {
        Member {
            dir: dir.to_owned(),
            unsafe_site: true,
            inherits: true,
            source: None,
        }
    }

    fn with_source(dir: &str, source: String) -> Self {
        Member {
            dir: dir.to_owned(),
            unsafe_site: false,
            inherits: true,
            source: Some(source),
        }
    }
}

/// The ledger every fixture uses: one island that is declared `planned` and
/// genuinely absent, so no island-roster violation fires and the only
/// violations in play come from the manifest readers under test.
const FIXTURE_LEDGER: &str = "schema_version = 1\n\n\
     [[island]]\n\
     name = \"fgdb-unsafe-arena\"\n\
     charter = \"Bump/region arena internals behind safe APIs.\"\n\
     status = \"planned\"\n";

/// Complete zero-site lane posture for the manifest-reader fixtures. These
/// relations intentionally exercise workspace topology, so the mandatory
/// verification-lane reader must be satisfied rather than short-circuiting the
/// checker before topology is observed.
const FIXTURE_VERIFICATION_LANES: &str = r#"schema_version = 1
cell = []

[[lane]]
tool = "miri"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["miri", "rust-src"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "Synthetic zero-site fixture; no checked Miri claim."

[[lane]]
tool = "asan"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["rust-src", "llvm-tools-preview"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "Synthetic zero-site fixture; no checked ASAN claim."

[[lane]]
tool = "tsan"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["rust-src", "llvm-tools-preview"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "Synthetic zero-site fixture; no checked TSAN claim."
"#;

/// Materialize a workspace whose root manifest is exactly `manifest`, run the
/// checker over it, and return what it concluded.
///
/// `scope` names the calling test and `tag` the variant. Both are in the
/// fixture path: `scope` so tests running in parallel cannot share a directory,
/// `tag` so each variant's tree is rebuilt identically on every run.
fn verdict(scope: &str, tag: &str, manifest: &str, members: &[Member]) -> Verdict {
    // The pid is not decoration. Several agents run this suite at once on this
    // machine, and the `remove_dir_all` below opens a fixture by DESTROYING it;
    // without process scoping, two concurrent runs delete each other's trees
    // mid-test. Measured before the pid was added: racing two copies of this
    // binary failed 6-11 of the 36 tests, a different set every round, while a
    // single run passed every time. The damage lands on the OTHER process, so
    // it reads as a verdict drift in whatever that agent had just changed.
    let root = std::env::temp_dir().join(format!(
        "fgdb-metamorphic-{}-{scope}-{tag}",
        std::process::id()
    ));
    // Rebuild from clean: a leftover member directory from an earlier run would
    // silently change what a glob roster resolves to.
    if root.is_dir() {
        fs::remove_dir_all(&root).expect("clear fixture root");
    }
    fs::create_dir_all(root.join("registries")).expect("registries dir");
    fs::create_dir_all(root.join("scripts")).expect("scripts dir");
    fs::write(root.join("Cargo.toml"), manifest).expect("workspace manifest");
    fs::write(root.join(LEDGER_PATH), FIXTURE_LEDGER).expect("ledger");
    fs::write(
        root.join(VERIFICATION_LANES_PATH),
        FIXTURE_VERIFICATION_LANES,
    )
    .expect("verification lanes");
    fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2026-07-05\"\ncomponents = [\"miri\", \"rust-src\", \"llvm-tools-preview\"]\n",
    )
    .expect("toolchain");
    fs::write(
        root.join("registries/checker_index.toml"),
        "[registry]\nname = \"checker_index\"\n\n[[checker]]\nsymbol = \"w1_unsafe_tool_lanes\"\nkind = \"script\"\nstatus = \"live\"\nartifact = \"scripts/w1_unsafe_tool_lanes.sh\"\nunit = \"artifact\"\n",
    )
    .expect("checker index");
    fs::write(
        root.join("scripts/w1_unsafe_tool_lanes.sh"),
        "#!/usr/bin/env bash\nexit 1\n",
    )
    .expect("lane runner");

    for member in members {
        let dir = root.join(&member.dir);
        fs::create_dir_all(dir.join("src")).expect("member src dir");
        let name = member.dir.rsplit('/').next().unwrap_or(&member.dir);
        let lints = if member.inherits {
            "\n[lints]\nworkspace = true\n"
        } else {
            ""
        };
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nedition = \"2024\"\n{lints}"),
        )
        .expect("member manifest");
        // Assembled from a `char` so this file never contains the attribute as
        // literal source text — the same precaution `scanner_fixture` takes,
        // for the same reason: the checker scans its own crate.
        let hash = '#';
        let source = match (&member.source, member.unsafe_site) {
            (Some(source), _) => source.clone(),
            (None, true) => format!("{hash}[allow(unsafe_code)]\npub unsafe fn probe() {{}}\n"),
            (None, false) => "pub fn probe() {}\n".to_owned(),
        };
        fs::write(dir.join("src/lib.rs"), source).expect("member source");
    }

    let (report, violations) = unsafe_ledger::check_workspace(&root);
    Verdict {
        codes: violations.into_iter().map(|v| v.code).collect(),
        crates_scanned: report.crates_scanned,
    }
}

/// The roster the base manifest declares: one crate carrying an unledgered
/// unsafe site, one clean crate. Two members so that a reader which drops all
/// but the first entry on a line is caught.
fn base_members() -> Vec<Member> {
    vec![
        Member::with_unsafe_site("crates/fgdb-probe"),
        Member::clean("crates/fgdb-quiet"),
    ]
}

/// The canonical manifest every relation transforms.
const BASE_MANIFEST: &str = "[workspace]\n\
     resolver = \"3\"\n\
     members = [\n    \"crates/fgdb-probe\",\n    \"crates/fgdb-quiet\",\n]\n\
     \n[workspace.lints.rust]\n\
     unsafe_code = \"forbid\"\n";

fn base_verdict(scope: &str) -> Verdict {
    verdict(scope, "base", BASE_MANIFEST, &base_members())
}

// ---------------------------------------------------------------------------
// The suite's own non-vacuity control
// ---------------------------------------------------------------------------

/// Two empty verdicts are equal, so an equivalence relation over a base that
/// finds nothing proves nothing. Everything below is quantified over this base;
/// if it goes vacuous, this test fails before the relations start passing for
/// the wrong reason.
#[test]
fn base_verdict_is_not_vacuous() {
    let base = base_verdict("vacuity");
    assert_eq!(
        base.crates_scanned, 2,
        "the base roster must resolve to both members: {base:?}"
    );
    assert!(
        base.codes.contains("site_unledgered"),
        "the base must carry a real unledgered site, or every equivalence \
         relation below compares two empty sets: {base:?}"
    );
    assert!(
        base.codes.contains("unsafe_allow_outside_island"),
        "the site must also be outside any island: {base:?}"
    );
    assert!(
        !base.codes.contains("workspace_forbid_absent"),
        "the base manifest does forbid unsafe_code: {base:?}"
    );
    assert!(
        !base.codes.contains("workspace_has_no_members"),
        "the base roster is non-empty: {base:?}"
    );
}

// ---------------------------------------------------------------------------
// Relation 1 — equivalence: respelling the manifest changes nothing
// ---------------------------------------------------------------------------

/// Manifests that mean exactly what [`BASE_MANIFEST`] means. Every one of these
/// was measured against the readers this suite replaced; the ones marked were
/// read WRONG, in the direction named.
fn equivalent_manifests() -> Vec<(&'static str, String)> {
    vec![
        // --- lint value spelling (the u9zp class) ---
        // read as "not forbidding" by the substring test
        (
            "lint_literal_quotes",
            BASE_MANIFEST.replace("unsafe_code = \"forbid\"", "unsafe_code = 'forbid'"),
        ),
        // read as "not forbidding" by the substring test
        (
            "lint_no_spaces",
            BASE_MANIFEST.replace("unsafe_code = \"forbid\"", "unsafe_code=\"forbid\""),
        ),
        (
            "lint_trailing_comment",
            BASE_MANIFEST.replace(
                "unsafe_code = \"forbid\"",
                "unsafe_code = \"forbid\"  # constitutional, never lower",
            ),
        ),
        (
            "lint_extra_inner_spacing",
            BASE_MANIFEST.replace("unsafe_code = \"forbid\"", "  unsafe_code   =   \"forbid\""),
        ),
        (
            "lint_beside_an_unrelated_lint",
            BASE_MANIFEST.replace(
                "unsafe_code = \"forbid\"",
                "unused_must_use = \"deny\"\nunsafe_code = \"forbid\"",
            ),
        ),
        // The exact shape that made the substring test vacuous on the real
        // repository: prose that quotes the lint. With the live table also
        // present the verdict must be unchanged -- and with it absent, the
        // difference table below requires the verdict to change.
        (
            "prose_comment_quoting_the_lint",
            format!(
                "# Workspace-level `unsafe_code = \"forbid\"`; islands relax to deny.\n{BASE_MANIFEST}"
            ),
        ),
        // --- roster layout (the lx43 class) ---
        // resolved to a bogus single entry by the line scan
        (
            "roster_one_line",
            BASE_MANIFEST.replace(
                "members = [\n    \"crates/fgdb-probe\",\n    \"crates/fgdb-quiet\",\n]",
                "members = [\"crates/fgdb-probe\", \"crates/fgdb-quiet\"]",
            ),
        ),
        // dropped the second member entirely
        (
            "roster_two_entries_one_line",
            BASE_MANIFEST.replace(
                "    \"crates/fgdb-probe\",\n    \"crates/fgdb-quiet\",",
                "    \"crates/fgdb-probe\", \"crates/fgdb-quiet\",",
            ),
        ),
        // resolved to ZERO members
        (
            "roster_literal_quotes",
            BASE_MANIFEST.replace(
                "    \"crates/fgdb-probe\",\n    \"crates/fgdb-quiet\",",
                "    'crates/fgdb-probe',\n    'crates/fgdb-quiet',",
            ),
        ),
        (
            "roster_no_trailing_comma",
            BASE_MANIFEST.replace(
                "    \"crates/fgdb-quiet\",\n]",
                "    \"crates/fgdb-quiet\"\n]",
            ),
        ),
        (
            "roster_interleaved_comments",
            BASE_MANIFEST.replace("members = [\n", "members = [\n    # the crate under test\n"),
        ),
        (
            "roster_glob",
            BASE_MANIFEST.replace(
                "    \"crates/fgdb-probe\",\n    \"crates/fgdb-quiet\",",
                "    \"crates/*\",",
            ),
        ),
        (
            "roster_reordered",
            BASE_MANIFEST.replace(
                "    \"crates/fgdb-probe\",\n    \"crates/fgdb-quiet\",",
                "    \"crates/fgdb-quiet\",\n    \"crates/fgdb-probe\",",
            ),
        ),
        // --- whole-document shape ---
        (
            "sections_reordered",
            "[workspace.lints.rust]\nunsafe_code = \"forbid\"\n\n\
             [workspace]\nresolver = \"3\"\n\
             members = [\n    \"crates/fgdb-probe\",\n    \"crates/fgdb-quiet\",\n]\n"
                .to_owned(),
        ),
    ]
}

#[test]
fn respelling_the_manifest_does_not_change_the_verdict() {
    let base = base_verdict("respell");
    for (tag, manifest) in equivalent_manifests() {
        let variant = verdict("respell", tag, &manifest, &base_members());
        assert_eq!(
            variant, base,
            "`{tag}` means exactly what the base manifest means, so it must \
             produce an identical verdict.\n  base:    {base:?}\n  variant: {variant:?}"
        );
    }
}

/// The non-vacuity control, restated per variant: no respelling may quietly
/// shrink what was actually inspected. This is the assertion that fails loudest
/// when a roster reader collapses, because `crates_scanned` goes to zero while
/// a naive verdict comparison could still look plausible.
#[test]
fn no_respelling_shrinks_the_scanned_roster() {
    let base = base_verdict("shrink");
    assert!(base.crates_scanned > 0, "control: the base scans something");
    for (tag, manifest) in equivalent_manifests() {
        let variant = verdict("shrink", tag, &manifest, &base_members());
        assert_eq!(
            variant.crates_scanned, base.crates_scanned,
            "`{tag}` must inspect the same number of crates as the base \
             ({} scanned, base scanned {})",
            variant.crates_scanned, base.crates_scanned
        );
        assert!(
            variant.crates_scanned > 0,
            "`{tag}` inspected nothing, so any clean conclusion from it would be \
             quantified over an empty set"
        );
    }
}

// ---------------------------------------------------------------------------
// Relation 2 — difference: a changed meaning must change the verdict
// ---------------------------------------------------------------------------

/// Manifests that mean something DIFFERENT from the base, each paired with the
/// violation the checker must gain. Every one of these satisfied the substring
/// test that this suite was written to bury.
fn different_manifests() -> Vec<(&'static str, String, &'static str)> {
    vec![
        (
            "lint_table_deleted",
            BASE_MANIFEST.replace("\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n", ""),
            "workspace_forbid_absent",
        ),
        (
            "lint_commented_out",
            BASE_MANIFEST.replace(
                "unsafe_code = \"forbid\"",
                "# unsafe_code = \"forbid\"  # TODO: restore",
            ),
            "workspace_forbid_absent",
        ),
        // THE regression case: the lint is gone, but prose still quotes it.
        // This is the real `Cargo.toml`'s shape, and it is why the substring
        // test could not fail on this repository.
        (
            "lint_deleted_but_prose_retained",
            format!(
                "# Workspace-level `unsafe_code = \"forbid\"`; islands relax to deny.\n{}",
                BASE_MANIFEST.replace("\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n", "")
            ),
            "workspace_forbid_absent",
        ),
        (
            "lint_under_clippy_namespace",
            BASE_MANIFEST.replace("[workspace.lints.rust]", "[workspace.lints.clippy]"),
            "workspace_forbid_absent",
        ),
        (
            "lint_at_package_level_not_workspace",
            BASE_MANIFEST.replace("[workspace.lints.rust]", "[lints.rust]"),
            "workspace_forbid_absent",
        ),
        (
            "lint_level_lowered_to_deny",
            BASE_MANIFEST.replace("unsafe_code = \"forbid\"", "unsafe_code = \"deny\""),
            "workspace_forbid_absent",
        ),
        (
            "roster_emptied",
            BASE_MANIFEST.replace(
                "members = [\n    \"crates/fgdb-probe\",\n    \"crates/fgdb-quiet\",\n]",
                "members = []",
            ),
            "workspace_has_no_members",
        ),
    ]
}

#[test]
fn changing_what_the_manifest_means_changes_the_verdict() {
    let base = base_verdict("differ");
    for (tag, manifest, expected) in different_manifests() {
        let variant = verdict("differ", tag, &manifest, &base_members());
        assert_ne!(
            variant, base,
            "`{tag}` does not mean what the base manifest means, so the verdict \
             must not be identical to it: {variant:?}"
        );
        assert!(
            variant.codes.contains(expected),
            "`{tag}` must be reported as {expected:?}, not merely differ: \
             got {:?}",
            variant.codes
        );
    }
}

// ---------------------------------------------------------------------------
// Relation 3 — an unrelated valid addition disturbs nothing
// ---------------------------------------------------------------------------

/// Adding a crate that is itself clean must not remove, add, or alter any
/// violation that belonged to the crates already there. A reader that mis-slices
/// the roster fails this even when it happens to get the base right.
#[test]
fn adding_a_clean_member_preserves_every_existing_violation() {
    let base = base_verdict("addclean");
    let manifest = BASE_MANIFEST.replace(
        "    \"crates/fgdb-quiet\",\n]",
        "    \"crates/fgdb-quiet\",\n    \"crates/fgdb-spare\",\n]",
    );
    let mut members = base_members();
    members.push(Member::clean("crates/fgdb-spare"));
    let grown = verdict("addclean", "added_clean_member", &manifest, &members);

    assert_eq!(
        grown.codes, base.codes,
        "a clean addition introduces no violation of its own and erases none: \
         \n  base:  {:?}\n  grown: {:?}",
        base.codes, grown.codes
    );
    assert_eq!(
        grown.crates_scanned,
        base.crates_scanned + 1,
        "the new member must actually be inspected"
    );
}

// ---------------------------------------------------------------------------
// Relation 4 — consistent renaming moves names, not verdicts
// ---------------------------------------------------------------------------

/// Renaming a crate everywhere at once — directory, roster entry, package name —
/// changes which strings appear in the report but not which violations exist.
#[test]
fn renaming_a_crate_consistently_preserves_the_violation_set() {
    let base = base_verdict("rename");
    let manifest = BASE_MANIFEST.replace("crates/fgdb-probe", "crates/fgdb-renamed");
    let members = vec![
        Member::with_unsafe_site("crates/fgdb-renamed"),
        Member::clean("crates/fgdb-quiet"),
    ];
    let renamed = verdict("rename", "consistent_rename", &manifest, &members);

    assert_eq!(
        renamed.codes, base.codes,
        "renaming a crate consistently changes subjects, not the violation set:\
         \n  base:    {:?}\n  renamed: {:?}",
        base.codes, renamed.codes
    );
    assert_eq!(
        renamed.crates_scanned, base.crates_scanned,
        "the same number of crates exists under either name"
    );
}

// ---------------------------------------------------------------------------
// Relation 5 — the site is found however the roster is written
// ---------------------------------------------------------------------------

/// The narrow statement of what `lx43` cost: an unledgered unsafe site must be
/// reported no matter how the roster that leads to it is spelled. Stated
/// separately from the equivalence relation because this is the consequence a
/// reader of the ledger actually cares about, and because it fails with a
/// message that names the harm rather than a set difference.
#[test]
fn an_unledgered_site_is_found_under_every_roster_spelling() {
    for (tag, manifest) in equivalent_manifests() {
        let variant = verdict("sitefound", tag, &manifest, &base_members());
        assert!(
            variant.codes.contains("site_unledgered"),
            "`{tag}`: an unledgered `allow(unsafe_code)` site sits in this \
             workspace and the checker did not report it — the roster spelling \
             hid it. Codes: {:?}",
            variant.codes
        );
    }
}

// ---------------------------------------------------------------------------
// Relation 6 — one reader per fact: the topology scanner and the ledger
//             scanner must agree on every attribute form
// ---------------------------------------------------------------------------

/// Materialize a one-crate workspace whose `src/lib.rs` is `source`, and report
/// what `topology::scan_workspace` concluded about that crate.
fn topology_fixture_root(scope: &str, tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-metamorphic-topo-{}-{scope}-{tag}",
        std::process::id()
    ))
}

fn topology_flags(scope: &str, tag: &str, root_attr: &str, source: &str) -> (bool, bool, bool) {
    // Process scope separates concurrent suite binaries. Relation scope also
    // separates tests inside this binary: several relations intentionally use
    // the same semantic variant tag (notably `nothing`) and run in parallel.
    let root = topology_fixture_root(scope, tag);
    if root.is_dir() {
        fs::remove_dir_all(&root).expect("clear fixture root");
    }
    let dir = root.join("crates/fgdb-probe");
    fs::create_dir_all(dir.join("src")).expect("member src dir");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\n    \"crates/fgdb-probe\",\n]\n\n\
         [workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
    )
    .expect("workspace manifest");
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fgdb-probe\"\nedition = \"2024\"\n\n[lints]\nworkspace = true\n",
    )
    .expect("member manifest");
    fs::write(dir.join("src/lib.rs"), format!("{root_attr}{source}")).expect("member source");

    let scan = registry_check::topology::scan_workspace(&root).expect("workspace scans");
    let crate_scan = scan.by_dir("crates/fgdb-probe").expect("crate scanned");
    (
        crate_scan.relaxes_unsafe,
        crate_scan.root_forbids_unsafe,
        crate_scan.root_denies_unsafe,
    )
}

#[test]
fn topology_fixture_identity_includes_the_calling_relation() {
    assert_ne!(
        topology_fixture_root("attribute_forms", "nothing"),
        topology_fixture_root("root_policy_negatives", "nothing"),
        "parallel relations using the same semantic variant must never share a live fixture"
    );
}

/// Every form in this table either relaxes `unsafe_code` or merely mentions it.
/// The topology scanner must classify each one the same way the ledger scanner
/// does — they are two consumers of a single fact, and the substring reader this
/// replaced disagreed on five of these ten.
#[test]
fn topology_and_ledger_agree_on_every_attribute_form() {
    let hash = '#';
    let cases: Vec<(&str, String, bool)> = vec![
        (
            "plain_allow",
            format!("{hash}[allow(unsafe_code)]\npub unsafe fn f() {{}}\n"),
            true,
        ),
        // missed by the substring reader: the closing paren is not adjacent
        (
            "multi_arg_allow",
            format!("{hash}[allow(unsafe_code, dead_code)]\npub unsafe fn f() {{}}\n"),
            true,
        ),
        (
            "spaced_allow",
            format!("{hash}[allow( unsafe_code )]\npub unsafe fn f() {{}}\n"),
            true,
        ),
        // missed entirely: `warn` was not in the substring vocabulary
        (
            "warn",
            format!("{hash}[warn(unsafe_code)]\npub unsafe fn f() {{}}\n"),
            true,
        ),
        (
            "expect",
            format!("{hash}[expect(unsafe_code)]\npub unsafe fn f() {{}}\n"),
            true,
        ),
        (
            "cfg_attr_allow",
            format!("{hash}[cfg_attr(unix, allow(unsafe_code))]\npub unsafe fn f() {{}}\n"),
            true,
        ),
        // INVENTED by the substring reader: a doc string is not an attribute
        (
            "doc_string_decoy",
            format!("{hash}[doc = \"never write allow(unsafe_code) here\"]\npub fn f() {{}}\n"),
            false,
        ),
        // INVENTED by the substring reader: this crate's own sources do this
        (
            "comment_decoy",
            "// never write allow(unsafe_code) here\npub fn f() {}\n".to_owned(),
            false,
        ),
        (
            "cfg_attr_forbid",
            format!("{hash}[cfg_attr(unix, forbid(unsafe_code))]\npub fn f() {{}}\n"),
            false,
        ),
        ("nothing", "pub fn f() {}\n".to_owned(), false),
    ];
    for (tag, source, relaxes) in cases {
        let ledger_says = !registry_check::unsafe_ledger::scan_sites("<probe>", &source).is_empty();
        let (topology_says, _, _) = topology_flags("attribute_forms", tag, "", &source);
        assert_eq!(
            ledger_says, relaxes,
            "`{tag}`: the ledger scanner is the reference reader and must be right first"
        );
        assert_eq!(
            topology_says, ledger_says,
            "`{tag}`: two readers of one fact disagreed — topology said {topology_says}, \
             the ledger scanner said {ledger_says}"
        );
    }
}

// ---------------------------------------------------------------------------
// Relation 7 — the root lint policy survives every respelling
// ---------------------------------------------------------------------------

/// Spellings of a crate-root `forbid` that all forbid exactly as hard as the
/// canonical one. Whole-line string equality read every variant below as
/// "declares nothing", which surfaces as `root_missing_forbid` against a crate
/// that is doing the right thing.
#[test]
fn a_root_forbid_is_recognised_however_it_is_spelled() {
    let hash = '#';
    for (tag, attr) in [
        ("canonical", format!("{hash}![forbid(unsafe_code)]\n")),
        (
            "trailing_comment",
            format!("{hash}![forbid(unsafe_code)] // constitutional\n"),
        ),
        (
            "grouped_with_sibling",
            format!("{hash}![forbid(unsafe_code, unsafe_op_in_unsafe_fn)]\n"),
        ),
        (
            "inner_spacing",
            format!("{hash}![ forbid( unsafe_code ) ]\n"),
        ),
        (
            "spread_across_lines",
            format!("{hash}![forbid(\n    unsafe_code\n)]\n"),
        ),
    ] {
        let (_, forbids, _) =
            topology_flags("root_forbid_spellings", tag, &attr, "pub fn f() {}\n");
        assert!(
            forbids,
            "`{tag}` forbids unsafe_code at the crate root and must be read as doing so"
        );
    }
}

/// The difference direction: text that merely mentions the attribute, or sets a
/// weaker level, must NOT be read as a root forbid.
#[test]
fn text_that_only_mentions_a_root_forbid_is_not_one() {
    let hash = '#';
    for (tag, attr) in [
        ("doc_comment", format!("//! {hash}![forbid(unsafe_code)]\n")),
        ("line_comment", format!("// {hash}![forbid(unsafe_code)]\n")),
        (
            "outer_not_inner",
            format!("{hash}[forbid(unsafe_code)]\npub fn g() {{}}\n"),
        ),
        ("deny_not_forbid", format!("{hash}![deny(unsafe_code)]\n")),
        ("nothing", String::new()),
    ] {
        let (_, forbids, _) =
            topology_flags("root_policy_negatives", tag, &attr, "pub fn f() {}\n");
        assert!(
            !forbids,
            "`{tag}` does not forbid unsafe_code at the crate root and must not be read as \
             doing so"
        );
    }
}

/// `deny` is read with the same machinery and must not be confused with
/// `forbid`: the island half of the topology law turns on exactly this.
#[test]
fn a_root_deny_is_distinguished_from_a_root_forbid() {
    let hash = '#';
    let (_, forbids, denies) = topology_flags(
        "root_deny",
        "deny_grouped",
        &format!("{hash}![deny(unsafe_code, unused)] // island root\n"),
        "pub fn f() {}\n",
    );
    assert!(denies, "a grouped, comment-trailed deny is still a deny");
    assert!(!forbids, "a deny is not a forbid");
}

// ---------------------------------------------------------------------------
// Relation 8 — an attribute is a site because of what it says, not where the
//              line breaks fall (`fgdb-regcheck-scansites-line-anchored-ds45`)
// ---------------------------------------------------------------------------

/// Placements of a real `allow(unsafe_code)` that all compile the same unsafe
/// surface. Moving an attribute off the start of its line is a semantics-
/// PRESERVING transformation of the source, so it must be a verdict-preserving
/// one — every entry here is a relaxation a `deny` island root would otherwise
/// reject, and every one of them scanned to ZERO sites under the line-anchored
/// candidacy test this relation was written to bury.
fn equivalent_site_placements() -> Vec<(&'static str, String)> {
    let hash = '#';
    vec![
        // the control: the one form the old scanner could see
        (
            "line_leading",
            format!("{hash}[allow(unsafe_code)]\npub unsafe fn probe() {{}}\n"),
        ),
        (
            "sharing_a_line_with_a_preceding_token",
            format!("pub mod m {{ {hash}[allow(unsafe_code)] pub unsafe fn probe() {{}} }}\n"),
        ),
        (
            "trailing_a_sibling_attribute",
            format!("{hash}[inline] {hash}[allow(unsafe_code)]\npub unsafe fn probe() {{}}\n"),
        ),
        (
            "inner_attribute_sharing_a_line",
            format!("pub mod m {{ {hash}![allow(unsafe_code)] pub unsafe fn probe() {{}} }}\n"),
        ),
        (
            "statement_position",
            format!("pub fn probe() {{ {hash}[allow(unsafe_code)] let _x = 1; }}\n"),
        ),
        // rustfmt does not normalise attribute placement inside a macro body,
        // so this is not a hypothetical spelling
        (
            "indented_inside_a_macro_body",
            format!(
                "macro_rules! m {{ () => {{ {hash}[allow(unsafe_code)] unsafe fn probe() {{}} }} }}\n"
            ),
        ),
        (
            "cfg_attr_wrapped_and_sharing_a_line",
            format!(
                "pub mod m {{ {hash}[cfg_attr(unix, allow(unsafe_code))] pub unsafe fn probe() {{}} }}\n"
            ),
        ),
    ]
}

#[test]
fn an_unledgered_site_is_found_at_any_column() {
    let base = base_verdict("placement");
    for (tag, source) in equivalent_site_placements() {
        let members = vec![
            Member::with_source("crates/fgdb-probe", source),
            Member::clean("crates/fgdb-quiet"),
        ];
        let variant = verdict("placement", tag, BASE_MANIFEST, &members);
        assert_eq!(
            variant, base,
            "`{tag}` relaxes unsafe_code exactly as the base source does — inside an \
             island root, whose `deny` an inner `allow` CAN lower, it compiles unsafe \
             code — so it must produce an identical verdict.\n  base:    {base:?}\n  \
             variant: {variant:?}"
        );
    }
}

/// Stated separately from the equivalence relation for the same reason
/// [`an_unledgered_site_is_found_under_every_roster_spelling`] is: this is the
/// consequence a reader of the ledger actually cares about, and it fails with a
/// message naming the harm rather than a set difference.
#[test]
fn no_placement_hides_a_site_from_the_scanner() {
    for (tag, source) in equivalent_site_placements() {
        let sites = registry_check::unsafe_ledger::scan_sites("<probe>", &source);
        assert_eq!(
            sites.len(),
            1,
            "`{tag}`: this source relaxes unsafe_code and the scanner found {} site(s) \
             in it. A missed site is an unsafe surface with no ledger row, no \
             `site_unledgered`, and no `unsafe_allow_outside_island`.",
            sites.len()
        );
    }
}

/// Text the compiler deletes is not code. Commenting a site out, or quoting it
/// inside a string, is a semantics-DESTROYING transformation, so the verdict
/// must change — and specifically it must LOSE the site.
///
/// This is the fail-closed-but-harmful direction, and it is not a lesser one:
/// the ledger's whole value is that its rows mean something, and a row
/// describing a commented-out attribute is a row describing nothing. Under the
/// line-anchored scan all four of these produced a real site, because each
/// begins its line; the masker that already understood every one of these
/// constructs was run per candidate, starting AT the candidate, so it could
/// never learn that the line it was handed was already inside a `/*`.
fn dead_text_placements() -> Vec<(&'static str, String)> {
    let hash = '#';
    vec![
        (
            "inside_a_block_comment",
            format!(
                "/*\n{hash}[allow(unsafe_code)]\npub unsafe fn probe() {{}}\n*/\npub fn probe() {{}}\n"
            ),
        ),
        (
            "inside_a_nested_block_comment",
            format!(
                "/* outer /* inner\n{hash}[allow(unsafe_code)]\n*/ still outer */\npub fn probe() {{}}\n"
            ),
        ),
        (
            "inside_a_multi_line_string",
            format!("pub const S: &str = \"\n{hash}[allow(unsafe_code)]\n\";\n"),
        ),
        (
            "inside_a_raw_string",
            format!("pub const S: &str = r{hash}\"\n{hash}[allow(unsafe_code)]\n\"{hash};\n"),
        ),
        // the one form the line-anchored scan already excluded, kept as the
        // regression control for the exclusion it did get right
        (
            "inside_a_line_comment",
            format!("// {hash}[allow(unsafe_code)]\npub fn probe() {{}}\n"),
        ),
    ]
}

#[test]
fn text_the_compiler_cannot_see_is_never_a_site() {
    let base = base_verdict("deadtext");
    for (tag, source) in dead_text_placements() {
        let members = vec![
            Member::with_source("crates/fgdb-probe", source),
            Member::clean("crates/fgdb-quiet"),
        ];
        let variant = verdict("deadtext", tag, BASE_MANIFEST, &members);
        assert_ne!(
            variant, base,
            "`{tag}` contains no live attribute at all, so it must not produce the \
             same verdict as a workspace that does: {variant:?}"
        );
        for code in ["site_unledgered", "unsafe_allow_outside_island"] {
            assert!(
                !variant.codes.contains(code),
                "`{tag}`: the checker reported {code:?} against text the compiler \
                 deletes. A ledger row minted for it would describe nothing, which is \
                 the one thing a ledger cannot survive. Codes: {:?}",
                variant.codes
            );
        }
        assert_eq!(
            variant.crates_scanned, base.crates_scanned,
            "`{tag}` must still inspect both crates — a verdict that loses the site \
             because it stopped looking is not the same result"
        );
    }
}

/// The pairing that keeps the relation above from passing vacuously: the SAME
/// attribute text, uncommented, must be a site. Without this a scanner that
/// found nothing anywhere would satisfy `text_the_compiler_cannot_see_is_never_a_site`
/// perfectly.
#[test]
fn the_dead_text_relation_has_a_live_control() {
    let hash = '#';
    let commented = format!("/*\n{hash}[allow(unsafe_code)]\npub unsafe fn probe() {{}}\n*/\n");
    let live = commented.replace("/*\n", "").replace("*/\n", "");
    assert!(
        registry_check::unsafe_ledger::scan_sites("<live>", &live).len() == 1,
        "control: with the comment delimiters removed this text IS a site"
    );
    assert!(
        registry_check::unsafe_ledger::scan_sites("<commented>", &commented).is_empty(),
        "and with them restored it is not"
    );
}

/// A site that shares a line with its item must name the ITEM. The symbol is
/// what a reviewer matches a ledger row against and what tells them how much
/// code the row covers, so naming the whole line — `impl T { … }` — would both
/// misidentify the site and overstate its scope.
#[test]
fn a_site_sharing_a_line_names_the_item_it_covers() {
    let hash = '#';
    let sites = registry_check::unsafe_ledger::scan_sites(
        "<inline>",
        &format!("impl T {{ {hash}[allow(unsafe_code)] unsafe fn f() {{}} }}\n"),
    );
    assert_eq!(sites.len(), 1, "one site: {sites:?}");
    assert_eq!(sites[0].line, 1, "reported where it opens");
    assert!(
        sites[0].symbol.starts_with("unsafe fn f()"),
        "the symbol must name the item the attribute covers, not the line it \
         shares: {:?}",
        sites[0].symbol
    );
}

/// The same defect one layer up, where the fail-open direction is sharpest:
/// `topology`'s `root_forbids_unsafe` comes from the crate-root attribute
/// reader, and every ordinary crate is required to carry a root `forbid`. Under
/// the line-anchored reader a root policy that had been COMMENTED OUT still read
/// as a live one, so a crate that had silently withdrawn its own memory-safety
/// declaration passed the law that exists to check exactly that.
#[test]
fn a_root_policy_the_compiler_cannot_see_is_not_a_policy() {
    let hash = '#';
    let (_, live, _) = topology_flags(
        "hidden_root_policy",
        "root_live_control",
        &format!("{hash}![forbid(unsafe_code)]\n"),
        "pub fn f() {}\n",
    );
    assert!(
        live,
        "control: the live attribute must be read as forbidding, or the cases \
         below pass by finding nothing anywhere"
    );
    for (tag, attr) in [
        (
            "root_block_comment",
            format!("/*\n{hash}![forbid(unsafe_code)]\n*/\n"),
        ),
        (
            "root_nested_block_comment",
            format!("/* /*\n{hash}![forbid(unsafe_code)]\n*/ */\n"),
        ),
        (
            "root_multi_line_string",
            format!("const S: &str = \"\n{hash}![forbid(unsafe_code)]\n\";\n"),
        ),
    ] {
        let (_, forbids, _) = topology_flags("hidden_root_policy", tag, &attr, "pub fn f() {}\n");
        assert!(
            !forbids,
            "`{tag}`: this crate root declares nothing to the compiler, and reporting \
             it as forbidding unsafe_code launders an undeclared crate as a declared one"
        );
    }
}

// ---------------------------------------------------------------------------
// Relation 9 — commenting code out is deleting it
//              (`fgdb-regcheck-commented-arm-counts-live-ctv8`)
// ---------------------------------------------------------------------------

/// The registry both arm fixtures are checked against: two `active` kinds.
const ARM_REGISTRY: &str = "schema_version = 1\n\n\
     [[kind]]\n\
     object_kind = 0x0001\n\
     name = \"Alpha\"\n\
     status = \"active\"\n\n\
     [[kind]]\n\
     object_kind = 0x0002\n\
     name = \"Beta\"\n\
     status = \"active\"\n";

fn refs_source(body: &str) -> String {
    format!("active_logical_object_kinds! {{\n{body}}}\n")
}

/// Materialize a root holding just the two files the active-arm binding reads,
/// and return the codes that binding produced.
///
/// The registries themselves come from the real repository — the same way
/// `spine.rs` and `claims.rs` drive `validate_all` — and are held CONSTANT
/// across every variant, so what the relations below compare is exactly the
/// transformation applied to `refs.rs` or to the kind registry.
fn arm_binding_codes(tag: &str, refs_src: &str, registry: &str) -> BTreeSet<String> {
    // Process-scoped for the reason given on the fixture root above.
    let root = std::env::temp_dir().join(format!(
        "fgdb-metamorphic-arms-{}-{tag}",
        std::process::id()
    ));
    if root.is_dir() {
        fs::remove_dir_all(&root).expect("clear fixture root");
    }
    fs::create_dir_all(root.join("crates/fgdb-types/src")).expect("fgdb-types src dir");
    fs::create_dir_all(root.join("registries")).expect("registries dir");
    fs::write(root.join("crates/fgdb-types/src/refs.rs"), refs_src).expect("refs.rs");
    fs::write(root.join("registries/logical_object_kinds.toml"), registry).expect("kind registry");

    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("repo root");
    let registries =
        registry_check::model::load_registries(&repo.join("registries")).expect("registries load");
    registry_check::validate::validate_all(&registries, &root)
        .into_iter()
        .map(|v| v.code)
        .filter(|code| code.contains("logical_kind") || code.contains("arm_without"))
        .collect()
}

const BOTH_ARMS: &str = "    Alpha = 0x0001 => \"Alpha\",\n    Beta = 0x0002 => \"Beta\",\n";
const ALPHA_ONLY: &str = "    Alpha = 0x0001 => \"Alpha\",\n";

/// THE relation. Commenting an arm out is semantically identical to deleting it
/// — the compiler sees neither — so the two must produce the same verdict.
///
/// They did not: a commented-out arm parsed as a live one, because the scanner
/// split any line containing `=>` out of raw source with no comment handling.
/// The bijection this binding exists to enforce is what makes the `fgdb-types`
/// const-assert meaningful, and it was satisfiable by text the compiler ignores.
/// A kind deactivated in Rust by commenting, with its registry row left
/// `active`, passed every registry gate while the workspace could not build —
/// which is the exact failure (84418b2, `main` broken for hours with every gate
/// green) that this checker was written to catch, reached from the other side.
#[test]
fn commenting_out_an_arm_is_the_same_as_deleting_it() {
    let deleted = arm_binding_codes("deleted", &refs_source(ALPHA_ONLY), ARM_REGISTRY);
    let present = arm_binding_codes("present", &refs_source(BOTH_ARMS), ARM_REGISTRY);

    // The control that makes the comparison mean something: deleting an arm
    // must be visible in the first place.
    assert!(
        deleted.contains("active_logical_kind_without_arm"),
        "control: an active row whose arm is deleted must be reported: {deleted:?}"
    );
    assert!(
        present.is_empty(),
        "control: with both arms present the binding is clean: {present:?}"
    );

    for (tag, body) in [
        (
            "line_comment",
            format!("{ALPHA_ONLY}    // Beta = 0x0002 => \"Beta\",\n"),
        ),
        (
            "block_comment",
            format!("{ALPHA_ONLY}    /*\n    Beta = 0x0002 => \"Beta\",\n    */\n"),
        ),
        (
            "trailing_comment_on_a_live_arm",
            "    Alpha = 0x0001 => \"Alpha\", // Beta = 0x0002 => \"Beta\",\n".to_owned(),
        ),
    ] {
        let commented = arm_binding_codes(tag, &refs_source(&body), ARM_REGISTRY);
        assert_eq!(
            commented, deleted,
            "`{tag}`: this arm is commented out, so the compiler does not see it and \
             neither may the checker. Commented must equal DELETED, not PRESENT.\n  \
             deleted:   {deleted:?}\n  commented: {commented:?}"
        );
    }
}

/// The other half of the trailing-comment case: a comment must not corrupt the
/// arm it trails. Splitting the raw line on the first `=>` put the comment's own
/// text inside Alpha's name and reported `active_logical_kind_name_mismatch`
/// against a registry row that was correct — a wrong diagnosis wearing the right
/// verdict, which costs a reader more than a missed one.
#[test]
fn a_trailing_comment_does_not_rename_the_arm_it_follows() {
    let codes = arm_binding_codes(
        "trailing_rename",
        &refs_source("    Alpha = 0x0001 => \"Alpha\", // Beta = 0x0002 => \"Beta\",\n"),
        ARM_REGISTRY,
    );
    assert!(
        !codes.contains("active_logical_kind_name_mismatch"),
        "Alpha's arm is spelled exactly as its registry row; only the comment after \
         it mentions Beta: {codes:?}"
    );
}

/// The block must end where the invocation ends, not at the first line that
/// happens to be `}`. A nested block inside the invocation truncated the arm
/// set, so every arm below it was reported missing — a fail-closed direction,
/// but one that manufactures work against correct code.
#[test]
fn a_nested_block_does_not_truncate_the_arm_set() {
    let codes = arm_binding_codes(
        "nested_block",
        &refs_source(&format!(
            "{ALPHA_ONLY}    cfg! {{\n    }}\n    Beta = 0x0002 => \"Beta\",\n"
        )),
        ARM_REGISTRY,
    );
    assert!(
        codes.is_empty(),
        "both arms are present; a nested block between them ends nothing: {codes:?}"
    );
}

/// The registry half of the same defect. These spellings all declare exactly the
/// two active kinds the base declares, and every one of them was read WRONG by
/// the line scan that matched `name = ` and `status = ` as literal prefixes —
/// failing in the direction that manufactures work, by dropping a row out of the
/// active set so its perfectly good Rust arm was reported orphaned.
#[test]
fn respelling_the_kind_registry_does_not_change_the_arm_verdict() {
    let base = arm_binding_codes("reg_base", &refs_source(BOTH_ARMS), ARM_REGISTRY);
    assert!(
        base.is_empty(),
        "control: the base registry and both arms agree: {base:?}"
    );
    // The third column is whether the respelling preserves the RAW BYTE LAYOUT.
    //
    // TWO LAWS LIVE ON THESE FIXTURES AND THEY DISAGREE ON PURPOSE (fgdb-gg4b).
    // This test's law is the ARM VERDICT: a respelling that declares the same
    // kinds must bind the same arms. But `logical_kind_projection_layout` is a
    // RAW-BYTE law -- fgdb-types consumes logical_object_kinds.toml as bytes and
    // requires `object_kind`/`name`/`status` adjacent and in that order, so its
    // needle is spelled with double quotes and single spaces. For that consumer a
    // respelling is NOT equivalent, and the layout law is right to fire.
    // Excluding it from the comparison without saying so would silence a real law,
    // so each variant now states which side it lands on and BOTH are asserted.
    for (tag, registry, layout_preserved) in [
        // read as an unreadable row -> `arm_without_active_logical_kind`
        (
            "literal_quotes",
            ARM_REGISTRY.replace("name = \"Beta\"", "name = 'Beta'"),
            false,
        ),
        // read as an unreadable row -> `arm_without_active_logical_kind`
        (
            "no_spaces_around_equals",
            ARM_REGISTRY.replace("name = \"Beta\"", "name=\"Beta\""),
            false,
        ),
        // dropped EVERY row -> `active_logical_kind_none_parsed`
        (
            "trailing_comment_on_status",
            ARM_REGISTRY.replace("status = \"active\"\n", "status = \"active\"  # pinned\n"),
            true,
        ),
        (
            "rows_reordered",
            "schema_version = 1\n\n\
             [[kind]]\nobject_kind = 0x0002\nname = \"Beta\"\nstatus = \"active\"\n\n\
             [[kind]]\nobject_kind = 0x0001\nname = \"Alpha\"\nstatus = \"active\"\n"
                .to_owned(),
            true,
        ),
        (
            "keys_reordered_within_a_row",
            "schema_version = 1\n\n\
             [[kind]]\nname = \"Alpha\"\nstatus = \"active\"\nobject_kind = 0x0001\n\n\
             [[kind]]\nstatus = \"active\"\nobject_kind = 0x0002\nname = \"Beta\"\n"
                .to_owned(),
            false,
        ),
    ] {
        const LAYOUT: &str = "logical_kind_projection_layout";
        let raw = arm_binding_codes(&format!("reg_{tag}"), &refs_source(BOTH_ARMS), &registry);
        let variant: BTreeSet<String> =
            raw.iter().filter(|code| *code != LAYOUT).cloned().collect();
        assert_eq!(
            variant, base,
            "`{tag}` declares exactly the kinds the base declares, so the arm binding \
             must reach the same verdict.\n  base:    {base:?}\n  variant: {variant:?}"
        );
        // The other half, so the exclusion above is licensed by a law rather than
        // by silence: the layout code must be present exactly when the respelling
        // moves the bytes fgdb-types reads.
        assert_eq!(
            !raw.contains(LAYOUT),
            layout_preserved,
            "`{tag}` is declared layout_preserved={layout_preserved}, but the raw verdict \
             {raw:?} says otherwise; a respelling either keeps the fgdb-types byte layout \
             or it does not, and this table must say which"
        );
    }
}

/// And the difference direction for the registry: a row that really did change
/// meaning must still be caught, or the equivalences above are satisfied by a
/// reader that sees nothing at all.
#[test]
fn changing_what_the_kind_registry_means_changes_the_arm_verdict() {
    let base = arm_binding_codes("regdiff_base", &refs_source(BOTH_ARMS), ARM_REGISTRY);
    for (tag, registry, expected) in [
        (
            "row_deactivated",
            ARM_REGISTRY.replace(
                "name = \"Beta\"\nstatus = \"active\"",
                "name = \"Beta\"\nstatus = \"reserved\"",
            ),
            "arm_without_active_logical_kind",
        ),
        (
            "row_renamed",
            ARM_REGISTRY.replace("name = \"Beta\"", "name = \"Gamma\""),
            "active_logical_kind_name_mismatch",
        ),
        (
            "row_commented_out",
            ARM_REGISTRY.replace(
                "[[kind]]\nobject_kind = 0x0002\nname = \"Beta\"\nstatus = \"active\"",
                "# [[kind]]\n# object_kind = 0x0002\n# name = \"Beta\"\n# status = \"active\"",
            ),
            "arm_without_active_logical_kind",
        ),
    ] {
        let variant = arm_binding_codes(
            &format!("regdiff_{tag}"),
            &refs_source(BOTH_ARMS),
            &registry,
        );
        assert_ne!(variant, base, "`{tag}` changed the registry's meaning");
        assert!(
            variant.contains(expected),
            "`{tag}` must be reported as {expected:?}: {variant:?}"
        );
    }
}

/// And the equivalence direction for the root reader: an inner attribute that
/// does not begin its line still binds the crate.
#[test]
fn a_root_forbid_is_recognised_at_any_column() {
    let hash = '#';
    let (_, forbids, _) = topology_flags(
        "root_forbid_column",
        "root_second_inner_on_one_line",
        &format!("{hash}![no_std] {hash}![forbid(unsafe_code)]\n"),
        "pub fn f() {}\n",
    );
    assert!(
        forbids,
        "two inner attributes on one line is valid Rust and the second one forbids \
         unsafe_code exactly as hard as it would on its own line"
    );
}

// ---------------------------------------------------------------------------
// Relation 10 — the activation closure must be able to fail, and a capability
//               atom must mean something
//               (`fgdb-regcheck-closure-vacuous-no-control-hp0f`)
// ---------------------------------------------------------------------------

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("repo root")
        .to_path_buf()
}

/// Load the real registry set with `invariants.toml` rewritten by `patch`.
///
/// Every other registry is copied verbatim, so what the relations below compare
/// is exactly the transformation applied to the invariant spine.
fn registries_with(tag: &str, patch: impl Fn(&str) -> String) -> registry_check::model::Registries {
    let src = repo_root().join("registries");
    // Process-scoped for the reason given on the fixture root above.
    let dir = std::env::temp_dir().join(format!(
        "fgdb-metamorphic-atoms-{}-{tag}",
        std::process::id()
    ));
    if dir.is_dir() {
        fs::remove_dir_all(&dir).expect("clear fixture registries");
    }
    fs::create_dir_all(&dir).expect("fixture registries dir");
    for entry in fs::read_dir(&src).expect("read registries") {
        let path = entry.expect("registry entry").path();
        if path.extension().is_some_and(|e| e == "toml") {
            let text = fs::read_to_string(&path).expect("read registry");
            let name = path.file_name().expect("registry file name");
            let text = if name == "invariants.toml" {
                patch(&text)
            } else {
                text
            };
            fs::write(dir.join(name), text).expect("write registry");
        }
    }
    registry_check::model::load_registries(&dir).expect("patched registries load")
}

fn validation_codes(r: &registry_check::model::Registries) -> BTreeSet<String> {
    registry_check::validate::validate_all(r, &repo_root())
        .into_iter()
        .map(|v| v.code)
        .collect()
}

/// The control that licenses every "closure satisfied" verdict.
///
/// The shipped manifest enables nothing, so its closure reaches zero clauses and
/// passes trivially — measured `clauses=20 reachable=0 live=0 absent=0 ok=true`
/// with 20 of 20 clauses non-live. That green bar is indistinguishable from the
/// one a BROKEN closure compiler would print, which is the whole
/// looks-exactly-like-a-pass family. `saturated_reachable` is the non-vacuity
/// control: what a manifest enabling every atom the spine names would reach. If
/// it is zero while the spine holds clauses, the compiler reaches nothing at all
/// and no conclusion from the run is licensed.
#[test]
fn a_zero_reach_closure_is_licensed_by_its_own_control() {
    let r = registry_check::model::load_registries(&repo_root().join("registries"))
        .expect("real registries");
    let manifest = registry_check::model::load_manifest(
        &repo_root().join("registries/sample_capability_manifest.toml"),
    )
    .expect("sample manifest");
    let report = registry_check::closure::compute(&r, &manifest);

    assert!(
        report.spine_clauses > 0,
        "control: the spine must hold clauses at all, or everything below is vacuous"
    );
    assert!(
        report.saturated_reachable > 0,
        "the closure compiler reaches NOTHING even with every atom the spine names \
         enabled: a reachable set of {} is then a broken compiler, not an empty \
         manifest, and `ok()` must not be reported as a pass",
        report.reachable.len()
    );
    assert!(report.licensed(), "the run must be licensed: {report:?}");
    assert_eq!(
        report.reachable.len() as i64,
        manifest.expected_reachable_clauses,
        "the manifest declares how many clauses it reaches; silence is not agreement, \
         so a drift in either direction must fail rather than pass quietly"
    );
}

/// The difference direction: a manifest that enables a real atom must reach
/// clauses. Without this, the licensing relation above could be satisfied by a
/// checker that reaches everything and nothing distinguishes the two.
#[test]
fn enabling_a_real_atom_reaches_clauses() {
    let r = registry_check::model::load_registries(&repo_root().join("registries"))
        .expect("real registries");
    let atom = registry_check::closure::spine_atoms(&r)
        .into_iter()
        .next()
        .expect("the spine names at least one capability atom");
    let manifest = registry_check::model::Manifest {
        name: "one-atom".into(),
        features: vec![atom.clone()],
        postures: vec![],
        roles: vec![],
        expected_reachable_clauses: 0,
    };
    let report = registry_check::closure::compute(&r, &manifest);
    assert!(
        !report.reachable.is_empty(),
        "enabling {atom:?} must reach the clauses whose predicate names it"
    );
}

/// The metamorphic statement of the typo class. Renaming a capability atom
/// CONSISTENTLY — in the vocabulary and in every predicate that names it —
/// changes what the atom is called and nothing else, so the verdict must not
/// move.
///
/// The rename is applied to the two places the atom is an ATOM — its vocabulary
/// entry and the predicate that names it — and nowhere else. `mvcc-visibility`
/// is also a proof-lane name in this registry set, and rewriting that too would
/// be a different change wearing the same spelling: the first draft of this test
/// did exactly that and produced `proof_lane_unresolved`, which is the relation
/// working, not failing.
#[test]
fn consistently_renaming_a_capability_atom_changes_no_verdict() {
    let rename = |text: &str| {
        text.replace(
            "activation_predicate = \"mvcc-visibility\"",
            "activation_predicate = \"mvcc-observability\"",
        )
        .replace("    \"mvcc-visibility\",", "    \"mvcc-observability\",")
    };
    let base = validation_codes(&registries_with("rename_base", str::to_owned));
    let renamed = validation_codes(&registries_with("rename_all", rename));
    assert_eq!(
        base, renamed,
        "renaming an atom everywhere at once changes its spelling, not the spine's \
         meaning:\n  base:    {base:?}\n  renamed: {renamed:?}"
    );
}

/// And the direction that was previously invisible forever: renaming an atom in
/// a PREDICATE but not in the vocabulary is a typo, and a typo evaluates false
/// exactly as an unlanded capability does. Measured before the vocabulary
/// existed: the reachable set silently shrank 20 -> 19 with no violation of any
/// kind, so in a tree where the other 19 clauses were live the misspelled clause
/// would have escaped enforcement under a green verdict, permanently.
#[test]
fn an_atom_named_only_by_a_predicate_is_reported() {
    let base = validation_codes(&registries_with("typo_base", str::to_owned));
    assert!(
        !base.contains("undeclared_capability_atom"),
        "control: the shipped spine declares every atom it names: {base:?}"
    );
    let typo = validation_codes(&registries_with("typo_pred", |text| {
        // Only the predicate occurrence: the vocabulary entry keeps its spelling.
        text.replace(
            "activation_predicate = \"mvcc-visibility\"",
            "activation_predicate = \"mvcc-visibilty\"",
        )
    }));
    assert!(
        typo.contains("undeclared_capability_atom"),
        "a predicate naming an atom outside the vocabulary is a typo that makes its \
         clause unreachable forever, and it must be reported: {typo:?}"
    );
}

/// The same for a manifest: an atom it enables must exist, or the closure it
/// asks for is quietly smaller than the one its author believed they declared.
#[test]
fn a_manifest_atom_outside_the_vocabulary_is_reported() {
    let r = registry_check::model::load_registries(&repo_root().join("registries"))
        .expect("real registries");
    let real = registry_check::closure::spine_atoms(&r)
        .into_iter()
        .next()
        .expect("at least one atom");
    let good = registry_check::model::Manifest {
        name: "good".into(),
        features: vec![real.clone()],
        postures: vec![],
        roles: vec![],
        expected_reachable_clauses: 0,
    };
    let typo = registry_check::model::Manifest {
        name: "typo".into(),
        features: vec![format!("{real}x")],
        postures: vec![],
        roles: vec![],
        expected_reachable_clauses: 0,
    };
    assert!(
        registry_check::validate::validate_manifest_atoms(&r, &good).is_empty(),
        "control: a declared atom is accepted"
    );
    let codes: Vec<String> = registry_check::validate::validate_manifest_atoms(&r, &typo)
        .into_iter()
        .map(|v| v.code)
        .collect();
    assert!(
        codes.contains(&"undeclared_manifest_atom".to_owned()),
        "a manifest atom outside the vocabulary enables nothing and must be reported: \
         {codes:?}"
    );
}

// ---------------------------------------------------------------------------
// Relation 11 — `status = "live"` must mean REGISTERED, INVOKED and CAPABLE OF
//               FAILING, not `Path::is_file()`
//               (`fgdb-checker-index-live-is-only-file-existence-tl0o`)
// ---------------------------------------------------------------------------
//
// AGENTS.md rests every G1–G4 exit gate on the word `live`: "CI cross-checks
// that every ID has a live checker", "no subsystem ships against an unenforced
// invariant". `live` was proved by `Path::is_file()`, so a checker registered
// live, cited by a clause as its enforcement mechanism, invoked by nothing and
// containing no code capable of failing was byte-identical to one that runs
// every commit.
//
// These are DIFFERENCE relations, and each carries the sharpest witness this
// suite can state: [`legacy_is_file_predicate`] recomputes the ENTIRE
// pre-fix predicate, and every mutant below is asserted to satisfy it. So each
// test says, in one place, "the old reader cannot tell these apart and the new
// one must" — which is precisely what makes reverting the fix turn this file
// red rather than merely dropping a check nobody notices.

/// The pre-fix liveness predicate, verbatim: a safe repository-relative path
/// that names an existing file.
///
/// Kept here on purpose. A difference relation whose mutant the OLD code would
/// also have rejected proves nothing about the fix.
fn legacy_is_file_predicate(root: &std::path::Path, artifact: &str) -> bool {
    let path = std::path::Path::new(artifact);
    let safe = !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
    safe && root.join(artifact).is_file()
}

fn checker(symbol: &str, kind: &str, artifact: &str, unit: &str) -> registry_check::model::Checker {
    registry_check::model::Checker {
        symbol: symbol.to_owned(),
        kind: kind.to_owned(),
        artifact: artifact.to_owned(),
        status: "live".to_owned(),
        unit: Some(unit.to_owned()),
    }
}

fn liveness_codes(
    root: &std::path::Path,
    row: &registry_check::model::Checker,
) -> BTreeSet<String> {
    registry_check::liveness::assess(root, row)
        .into_iter()
        .map(|defect| defect.kind.code().to_owned())
        .collect()
}

/// A miniature repository holding one of everything a checker row can name.
///
/// Every artifact below EXISTS, which is the point: the pre-fix predicate says
/// yes to all of them.
fn liveness_fixture(tag: &str) -> std::path::PathBuf {
    // Process-scoped for the reason given on the fixture root above.
    let root = std::env::temp_dir().join(format!(
        "fgdb-metamorphic-liveness-{}-{tag}",
        std::process::id()
    ));
    if root.is_dir() {
        fs::remove_dir_all(&root).expect("clear liveness fixture");
    }
    let member = root.join("crates/probe");
    fs::create_dir_all(member.join("src/bin")).expect("member src");
    fs::create_dir_all(member.join("tests")).expect("member tests");
    fs::create_dir_all(member.join("notes")).expect("member notes");
    fs::create_dir_all(root.join("scripts")).expect("scripts dir");

    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\n    \"crates/probe\",\n]\n",
    )
    .expect("workspace manifest");
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nedition = \"2024\"\n",
    )
    .expect("member manifest");

    // A binary whose `main` can exit nonzero, and one that cannot.
    fs::write(
        member.join("src/main.rs"),
        "pub mod reached;\nfn main() -> std::process::ExitCode { std::process::ExitCode::from(1) }\n",
    )
    .expect("main");
    fs::write(
        member.join("src/bin/toothless.rs"),
        "fn main() -> std::process::ExitCode { std::process::ExitCode::SUCCESS }\n",
    )
    .expect("toothless bin");
    fs::write(member.join("src/reached.rs"), "pub fn work() {}\n").expect("reached module");
    // Identical content, declared by no `mod`: dead source that still exists.
    fs::write(member.join("src/orphan.rs"), "pub fn work() {}\n").expect("orphan module");

    // The assertion is assembled from a `char` for the same reason
    // `scanner_fixture` assembles its attribute that way: this file is scanned
    // by the very readers under test, and a literal here would be a real site.
    let bang = '!';
    let test_source = format!("#[test]\nfn probe_gate() {{\n    assert{bang}(1 + 1 == 2);\n}}\n");
    let gutted_source = "#[test]\nfn probe_gate() {\n    let _ = 1 + 1;\n}\n";
    fs::write(member.join("tests/gate.rs"), &test_source).expect("test target");
    fs::write(member.join("tests/gutted.rs"), gutted_source).expect("gutted test target");
    // The same bytes, one directory over — a place `cargo test` never compiles.
    fs::write(member.join("notes/gate.rs"), &test_source).expect("uncompiled copy");

    fs::write(
        root.join("scripts/gate.sh"),
        "#!/usr/bin/env bash\nset -euo pipefail\nif [ -z \"${1:-}\" ]; then exit 1; fi\n",
    )
    .expect("gate script");
    fs::write(
        root.join("scripts/toothless.sh"),
        "#!/usr/bin/env bash\n# exit 1 would be the honest answer here\necho checked\nexit 0\n",
    )
    .expect("toothless script");

    // --- proof-lane artifacts (relation 12) ------------------------------
    //
    // Every file below EXISTS, for the same reason as everything above it: the
    // pre-fix proof-lane predicate was `root.join(artifact).is_file()` and says
    // yes to all of them.
    fs::create_dir_all(root.join("formal/lean")).expect("lean dir");
    fs::create_dir_all(root.join("formal/tla")).expect("tla dir");
    // Assembled from a `char` for the same reason the assertion above is: a
    // literal admit token in a file this suite ships is a real site the moment
    // somebody points a reader at these sources.
    let sorry = format!("{}orry", 's');
    fs::write(
        root.join("formal/lean/Proved.lean"),
        "theorem two : 1 + 1 = 2 := by decide\n",
    )
    .expect("proved lean");
    fs::write(
        root.join("formal/lean/Admitted.lean"),
        format!("theorem two : 1 + 1 = 2 := {sorry}\n"),
    )
    .expect("admitted lean");
    fs::write(
        root.join("formal/lean/Axiomatised.lean"),
        "axiom two : 1 + 1 = 2\n",
    )
    .expect("axiomatised lean");
    // The file the bead names: it exists, it builds, it says nothing.
    fs::write(root.join("formal/lean/Empty.lean"), "").expect("empty lean");
    // The MASKED control: an admit token that is not an admit. A reader that
    // widened its pattern instead of parsing its input calls this one red.
    fs::write(
        root.join("formal/lean/Masked.lean"),
        format!(
            "/- an earlier draft of this ended in {sorry}, /- even nested -/ -/\n\
             -- and this line mentions {sorry} too\n\
             theorem two : 1 + 1 = 2 := by decide\n"
        ),
    )
    .expect("masked lean");

    fs::write(
        root.join("formal/tla/Checked.tla"),
        "---- MODULE Checked ----\nTypeOK == TRUE\n====\n",
    )
    .expect("checked tla");
    fs::write(
        root.join("formal/tla/Checked.cfg"),
        "SPECIFICATION Spec\nINVARIANT TypeOK\n",
    )
    .expect("checked cfg");
    fs::write(
        root.join("formal/tla/Unchecked.tla"),
        "---- MODULE Unchecked ----\nTypeOK == TRUE\n====\n",
    )
    .expect("unchecked tla");
    // A config that runs and asserts nothing — the TLA+ spelling of a `main`
    // that can only return success.
    fs::write(
        root.join("formal/tla/Unchecked.cfg"),
        "SPECIFICATION Spec\n\\* INVARIANT TypeOK\nCONSTANTS N = 3\n",
    )
    .expect("unchecked cfg");
    fs::write(
        root.join("formal/tla/NoConfig.tla"),
        "---- MODULE NoConfig ----\nTypeOK == TRUE\n====\n",
    )
    .expect("configless tla");
    root
}

/// The control. Two red verdicts differ from each other in uninteresting ways,
/// so a difference relation over a base that is ALREADY defective proves
/// nothing — and a liveness reader that has stopped reading returns "no
/// defects" for every row, which is exactly what a healthy registry returns.
#[test]
fn liveness_base_and_readers_are_licensed() {
    let control = registry_check::liveness::self_test();
    assert!(
        control.cases > 0,
        "the liveness self-test ran no cases at all; a control that checks nothing \
         licenses nothing"
    );
    assert!(
        control.licensed(),
        "the liveness readers got known answers wrong ({:?}); every verdict below is \
         unlicensed until they are fixed",
        control.failures
    );

    let root = liveness_fixture("base");
    for row in [
        checker(
            "probe_gate",
            "cargo-test",
            "crates/probe/tests/gate.rs",
            "symbol",
        ),
        checker(
            "gate_law",
            "cargo-test",
            "crates/probe/tests/gate.rs",
            "artifact",
        ),
        checker(
            "reached_law",
            "binary",
            "crates/probe/src/reached.rs",
            "artifact",
        ),
        checker("gate_e2e", "script", "scripts/gate.sh", "artifact"),
    ] {
        assert_eq!(
            liveness_codes(&root, &row),
            BTreeSet::new(),
            "control: {:?} is genuinely live and must be reported live",
            row.symbol
        );
    }
}

/// The real registry is the other half of the control: a law that only ever
/// rejects would be caught by nothing else here.
#[test]
fn every_shipped_live_row_is_provably_live() {
    let r = registry_check::model::load_registries(&repo_root().join("registries"))
        .expect("real registries");
    let live: Vec<_> = r
        .checker_index
        .iter()
        .filter(|row| row.status == "live")
        .collect();
    assert!(
        !live.is_empty(),
        "control: the shipped registry declares no live checker at all, so every \
         conclusion below is quantified over nothing"
    );
    let mut defective = Vec::new();
    for row in &live {
        let codes = liveness_codes(&repo_root(), row);
        if !codes.is_empty() {
            defective.push((row.symbol.clone(), codes));
        }
    }
    assert!(
        defective.is_empty(),
        "shipped rows claim `status = \"live\"` without being live: {defective:?}"
    );
}

/// INVOKED. The artifact exists; nothing runs it.
///
/// `crates/probe/notes/gate.rs` is byte-identical to the integration test one
/// directory over, and `cargo test --workspace` never compiles it. `check.sh`
/// would still report it PASS, because it credits every `cargo-test` row with
/// the one workspace `cargo test` exit code.
#[test]
fn an_artifact_no_gate_compiles_is_not_live() {
    let root = liveness_fixture("uninvoked");
    let base = checker(
        "probe_gate",
        "cargo-test",
        "crates/probe/tests/gate.rs",
        "symbol",
    );
    let moved = checker(
        "probe_gate",
        "cargo-test",
        "crates/probe/notes/gate.rs",
        "symbol",
    );

    assert!(
        legacy_is_file_predicate(&root, &base.artifact)
            && legacy_is_file_predicate(&root, &moved.artifact),
        "witness: the pre-fix predicate cannot tell these two apart"
    );
    assert_eq!(liveness_codes(&root, &base), BTreeSet::new());
    assert!(
        liveness_codes(&root, &moved).contains("checker_not_invocable"),
        "a test file outside `tests/` is never run, so registering it live is a claim \
         about a gate that does not exist: {:?}",
        liveness_codes(&root, &moved)
    );
}

/// INVOKED, the `binary` case: a module no `mod` declaration reaches is not
/// compiled into any program, so no gate can run it.
#[test]
fn a_module_no_binary_contains_is_not_live() {
    let root = liveness_fixture("orphan");
    let reached = checker(
        "reached_law",
        "binary",
        "crates/probe/src/reached.rs",
        "artifact",
    );
    let orphan = checker(
        "orphan_law",
        "binary",
        "crates/probe/src/orphan.rs",
        "artifact",
    );

    assert!(
        legacy_is_file_predicate(&root, &reached.artifact)
            && legacy_is_file_predicate(&root, &orphan.artifact),
        "witness: the pre-fix predicate cannot tell these two apart"
    );
    assert_eq!(liveness_codes(&root, &reached), BTreeSet::new());
    assert!(
        liveness_codes(&root, &orphan).contains("checker_not_invocable"),
        "dead source registered as a live binary checker: {:?}",
        liveness_codes(&root, &orphan)
    );
}

/// CAPABLE OF FAILING, three ways. Each artifact is exactly where a runner
/// looks for it, and each is executed on every run — and none of them can
/// report a violation.
#[test]
fn an_invoked_gate_that_cannot_fail_is_not_live() {
    let root = liveness_fixture("toothless");

    let live_test = checker(
        "probe_gate",
        "cargo-test",
        "crates/probe/tests/gate.rs",
        "symbol",
    );
    let gutted_test = checker(
        "probe_gate",
        "cargo-test",
        "crates/probe/tests/gutted.rs",
        "symbol",
    );
    let live_script = checker("gate_e2e", "script", "scripts/gate.sh", "artifact");
    let gutted_script = checker(
        "toothless_e2e",
        "script",
        "scripts/toothless.sh",
        "artifact",
    );
    let toothless_bin = checker(
        "toothless_law",
        "binary",
        "crates/probe/src/bin/toothless.rs",
        "artifact",
    );

    for row in [&gutted_test, &gutted_script, &toothless_bin] {
        assert!(
            legacy_is_file_predicate(&root, &row.artifact),
            "witness: the pre-fix predicate accepts {:?}",
            row.artifact
        );
    }
    assert_eq!(liveness_codes(&root, &live_test), BTreeSet::new());
    assert_eq!(liveness_codes(&root, &live_script), BTreeSet::new());

    assert!(
        liveness_codes(&root, &gutted_test).contains("checker_cannot_fail"),
        "a `#[test]` with no assertion runs every commit and passes every commit: {:?}",
        liveness_codes(&root, &gutted_test)
    );
    assert!(
        liveness_codes(&root, &gutted_script).contains("checker_cannot_fail"),
        "a script whose only nonzero exit is in a comment cannot report anything: {:?}",
        liveness_codes(&root, &gutted_script)
    );
    assert!(
        liveness_codes(&root, &toothless_bin).contains("checker_cannot_fail"),
        "a checker binary whose `main` returns success unconditionally cannot report a \
         violation however much its modules compute: {:?}",
        liveness_codes(&root, &toothless_bin)
    );
}

/// REGISTERED. A `unit = "symbol"` row names one function; renaming or deleting
/// it must turn the row red rather than leave it green over a file that merely
/// still exists. This is the rot the bead measured: every `cargo-test` symbol
/// resolves today by author discipline, and nothing enforced it.
#[test]
fn a_renamed_test_symbol_does_not_stay_live() {
    let root = liveness_fixture("renamed");
    let present = checker(
        "probe_gate",
        "cargo-test",
        "crates/probe/tests/gate.rs",
        "symbol",
    );
    let renamed = checker(
        "probe_gate_after_the_rename",
        "cargo-test",
        "crates/probe/tests/gate.rs",
        "symbol",
    );

    assert!(
        legacy_is_file_predicate(&root, &renamed.artifact),
        "witness: the artifact still exists, so the pre-fix predicate still says live"
    );
    assert_eq!(liveness_codes(&root, &present), BTreeSet::new());
    assert!(
        liveness_codes(&root, &renamed).contains("checker_symbol_unresolved"),
        "{:?}",
        liveness_codes(&root, &renamed)
    );

    // And the attribute is load-bearing: a plain `fn` of the right name is not
    // something `cargo test` runs, so accepting one would let a deleted test
    // keep its row green because a helper survived under the same name.
    let helper_only = liveness_fixture("renamed-helper");
    fs::write(
        helper_only.join("crates/probe/tests/gate.rs"),
        "fn probe_gate() { assert!(1 + 1 == 2); }\n",
    )
    .expect("helper-only test file");
    assert!(
        liveness_codes(&helper_only, &present).contains("checker_symbol_unresolved"),
        "a `fn` without `#[test]` is not a gate: {:?}",
        liveness_codes(&helper_only, &present)
    );
}

/// A live row that does not say what its `symbol` names cannot be checked at
/// all, and must not be credited as live for it.
#[test]
fn a_live_row_must_declare_what_its_symbol_names() {
    let root = liveness_fixture("undeclared");
    let mut row = checker(
        "probe_gate",
        "cargo-test",
        "crates/probe/tests/gate.rs",
        "symbol",
    );
    row.unit = None;
    assert!(
        liveness_codes(&root, &row).contains("checker_unit_undeclared"),
        "{:?}",
        liveness_codes(&root, &row)
    );
    row.unit = Some("whole-file".to_owned());
    assert!(
        liveness_codes(&root, &row).contains("checker_unit_undeclared"),
        "an unrecognised unit spelling must not be silently treated as either one: {:?}",
        liveness_codes(&root, &row)
    );
}

/// EQUIVALENCE. Reformatting `checker_index.toml` — quoting style, key order,
/// comments, blank lines — changes nothing about which checkers are registered,
/// so it must change nothing about the verdict. This is the relation that
/// catches the whole substring-for-structure class in the registry reader
/// itself, the same way `member_roster_is_quote_and_layout_invariant` catches it
/// in the manifest reader.
#[test]
fn checker_rows_survive_a_meaning_preserving_requote() {
    let text = fs::read_to_string(repo_root().join("registries/checker_index.toml"))
        .expect("read checker index");
    let table = registry_check::toml::parse(&text).expect("parse checker index");
    let base = registry_check::model::checker_index_from(&table).expect("read checker index");

    let requoted: String = text
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            match trimmed.split_once(" = ") {
                // A TOML literal string is delimited by `'` and has no escapes,
                // so a value containing `'`, `"` or `\` has no literal spelling
                // and must be left alone. A transform that changes what the
                // document MEANS is not an equivalence relation, and one that
                // makes it unparseable is not a relation at all.
                Some((key, value))
                    if !trimmed.starts_with('#')
                        && value.starts_with('"')
                        && value.ends_with('"')
                        && value.len() >= 2
                        && !value[1..value.len() - 1].contains(['"', '\'', '\\']) =>
                {
                    format!("{key}='{}'", &value[1..value.len() - 1])
                }
                _ => line.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let requoted_table = registry_check::toml::parse(&requoted).expect("parse requoted index");
    let after =
        registry_check::model::checker_index_from(&requoted_table).expect("read requoted index");

    assert!(
        base.iter().any(|row| row.status == "live"),
        "control: the base roster must hold a live row, or this relation compares two \
         empty sets"
    );
    // The transform must actually transform. `lx43` — the requote that took
    // `crates_scanned` from 14 to zero — would have been invisible to a
    // relation whose "variant" was the original document.
    let rewritten = text
        .lines()
        .zip(requoted.lines())
        .filter(|(before, after)| before != after)
        .count();
    assert!(
        rewritten >= base.len(),
        "control: the requote rewrote only {rewritten} lines across {} rows, so this \
         relation is comparing a document with itself",
        base.len()
    );
    assert_eq!(
        base, after,
        "a cosmetic requote changed which checkers this registry declares"
    );
}

// ---------------------------------------------------------------------------
// Relation 12 — `status = "checked"` on a PROOF LANE must mean the same three
//               things, and the INVOKED fact must be answered by the reader
//               that already answers it
//               (`fgdb-proof-lane-checked-is-only-file-existence-0f1l`)
// ---------------------------------------------------------------------------
//
// Relation 11 removed `Path::is_file()` from `checker_index.toml`. The identical
// hole was one artifact over: `registries/proof_lanes.toml`'s header defines
// `checked` as "the artifact exists in-repo AND is CI-checked", and
// `validate_proof_lanes` proved it with `root.join(&lane.artifact).is_file()` —
// the pre-`tl0o` read, down to the missing path-safety guard. Nothing ran a
// prover, so `theorem foo : False := sorry` was `checked`, and so was an empty
// file.
//
// These are DIFFERENCE relations built exactly like relation 11's, with the same
// sharpest-available witness: [`legacy_lane_is_file_predicate`] recomputes the
// ENTIRE pre-fix predicate and every mutant below is asserted to satisfy it. So
// each test says in one place, "the old reader called this checked and the new
// one must not."
//
// The relation that matters most is [`a_lane_whose_gate_is_not_live_is_not_checked`]:
// it does not re-derive what a running gate is, it asserts that the lane's
// verdict CARRIES `liveness`'s own words about the gate. That is the delegation,
// pinned. A parallel implementation would pass every other test in this block
// and fail that one.

/// The pre-fix proof-lane predicate, verbatim.
///
/// Note what is missing: there is no path-safety guard. That is not a
/// simplification in the copy — `validate_proof_lanes` really did call
/// `root.join(&lane.artifact).is_file()` on an unvalidated path, which is the
/// same omission `tl0o` found in `validate`'s checker read and fixed only there.
fn legacy_lane_is_file_predicate(root: &std::path::Path, artifact: &str) -> bool {
    root.join(artifact).is_file()
}

fn lane(id: &str, system: &str, artifact: &str, status: &str) -> registry_check::model::Lane {
    registry_check::model::Lane {
        id: id.to_owned(),
        lane: system.to_owned(),
        model_scope: "scoped for the fixture: proves 1 + 1 = 2 and nothing else".to_owned(),
        artifact: artifact.to_owned(),
        status: status.to_owned(),
        checked_by: Some("lane_gate".to_owned()),
    }
}

/// The `checker_index` roster the fixture lanes bind to: one gate that is live
/// in the full sense, one that claims live and cannot fail, one still a stub.
fn lane_gates() -> Vec<registry_check::model::Checker> {
    let mut stub = checker("stub_gate", "script", "scripts/gate.sh", "artifact");
    stub.status = "stub".to_owned();
    vec![
        checker("lane_gate", "script", "scripts/gate.sh", "artifact"),
        checker(
            "toothless_gate",
            "script",
            "scripts/toothless.sh",
            "artifact",
        ),
        stub,
    ]
}

fn lane_defects(
    root: &std::path::Path,
    row: &registry_check::model::Lane,
) -> Vec<registry_check::liveness::Defect> {
    registry_check::liveness::assess_lane(root, row, &lane_gates())
}

fn lane_codes(root: &std::path::Path, row: &registry_check::model::Lane) -> BTreeSet<String> {
    lane_defects(root, row)
        .into_iter()
        .map(|defect| defect.kind.code().to_owned())
        .collect()
}

/// Assert that the PRE-FIX predicate accepted this lane, so the difference
/// relation below it is a statement about the fix rather than about a mutant
/// the old code would also have rejected.
fn assert_legacy_accepted(root: &std::path::Path, row: &registry_check::model::Lane) {
    assert!(
        legacy_lane_is_file_predicate(root, &row.artifact),
        "witness: the pre-fix predicate must accept {:?}, or this relation proves \
         nothing about the fix",
        row.id
    );
}

/// The control. Two red verdicts differ in uninteresting ways, so a difference
/// relation over an already-defective base proves nothing — and a reader that
/// has stopped reading returns "no defects" for every lane, which is exactly
/// what a healthy registry returns.
#[test]
fn proof_lane_base_and_readers_are_licensed() {
    let control = registry_check::liveness::self_test();
    assert!(
        control.cases > 0,
        "the liveness self-test ran no cases at all; a control that checks nothing \
         licenses nothing"
    );
    assert!(
        control.licensed(),
        "the liveness readers got known answers wrong ({:?}); every verdict below is \
         unlicensed until they are fixed",
        control.failures
    );

    let root = liveness_fixture("lane-base");
    for row in [
        lane("lean-proved", "lean", "formal/lean/Proved.lean", "checked"),
        // The masked case is a POSITIVE control: its admit tokens are all inside
        // comments, including a nested one. A reader that widened a pattern
        // instead of parsing its input calls this red, and this suite exists
        // because that is the mistake this repository keeps making.
        lane("lean-masked", "lean", "formal/lean/Masked.lean", "checked"),
        lane(
            "tla-checked",
            "tlaplus",
            "formal/tla/Checked.tla",
            "checked",
        ),
        // A declared lane owes only a safe path, and this one has it.
        lane(
            "lean-declared",
            "lean",
            "formal/lean/DoesNotExistYet.lean",
            "declared",
        ),
    ] {
        assert_eq!(
            lane_codes(&root, &row),
            BTreeSet::new(),
            "control: {:?} is genuinely checked and must be reported checked",
            row.id
        );
    }
}

/// THE VACUITY CONTROL, and it fires.
///
/// Zero `.lean`, `.tla`, `.cfg` or `lakefile*` files exist in this repository
/// and all ten shipped lanes are `declared`, so a sweep asserting "every shipped
/// `checked` lane is really checked" is quantified over the empty set — it would
/// pass just as loudly if every reader above had been deleted. A zero result is
/// not a result without a control, so this test MAKES one: it takes a real
/// shipped lane, promotes it exactly as a future author would, and requires the
/// readers to produce a real defect from real registry data.
///
/// It is written to survive Genesis. When the first proof artifact lands the
/// promoted-row branch stops firing and the shipped-cohort branch takes over,
/// and neither the count of lanes nor the emptiness of the cohort is pinned.
#[test]
fn proof_lane_checked_arm_is_vacuous_today_and_this_is_what_licenses_it() {
    let r = registry_check::model::load_registries(&repo_root().join("registries"))
        .expect("real registries");
    assert!(
        !r.proof_lanes.is_empty(),
        "control: the lane roster came back empty, so every verdict about it is \
         quantified over nothing"
    );

    // Half one: whatever IS shipped as checked must really be checked.
    //
    // RESOLVED AGAINST THE REAL checker_index, not the synthetic `lane_gates()`
    // fixture. This half quantifies over REAL lanes, so it must judge them by the
    // REAL gate roster; the fixture exists for the synthetic relations below it.
    // While zero lanes shipped as `checked` the loop body never ran and the
    // mismatch was invisible -- then 06e5d72 (fgdb-dkrc) promoted lean-version-chain
    // and the test reported `proof_lane_gate_unresolved` for a gate that IS
    // registered and live, because a real symbol cannot resolve in a fixture that
    // only ever contained lane_gate/toothless_gate/stub_gate. A vacuous assertion
    // hid a defect in itself until the population it quantifies over stopped being
    // empty, which is the reason half two exists at all.
    let mut defective = Vec::new();
    for row in r.proof_lanes.iter().filter(|l| l.status == "checked") {
        let codes: BTreeSet<String> =
            registry_check::liveness::assess_lane(&repo_root(), row, &r.checker_index)
                .into_iter()
                .map(|defect| defect.kind.code().to_owned())
                .collect();
        if !codes.is_empty() {
            defective.push((row.id.clone(), codes));
        }
    }
    assert!(
        defective.is_empty(),
        "shipped lanes claim `status = \"checked\"` without being checked: {defective:?}"
    );

    // Half two: the licence. Promote a real shipped lane the way a future author
    // would — flip the status, leave everything else alone — and require the
    // readers to say something. If this comes back clean, the `checked` arm is
    // not merely unexercised, it is dead, and half one proved nothing.
    let Some(shipped) = r.proof_lanes.first() else {
        unreachable!("the roster is non-empty");
    };
    let mut promoted = shipped.clone();
    promoted.status = "checked".to_owned();
    let codes = lane_codes(&repo_root(), &promoted);
    assert!(
        !codes.is_empty(),
        "control: promoting shipped lane {:?} to \"checked\" produced no defect at all, \
         so the checked arm cannot tell a proof from an absent one",
        promoted.id
    );
    assert!(
        codes.contains("artifact_missing"),
        "the promoted lane's artifact {:?} does not exist, which is the one thing even \
         the pre-fix predicate caught; got {codes:?}",
        promoted.artifact
    );
}

/// INVOKED, and THE DELEGATION. The proof exists; no live gate runs it.
///
/// "Is CI-checked" is a checker-liveness question, so the lane must not answer
/// it a second way. The last assertion is the pin: the lane's verdict has to
/// carry `liveness`'s OWN words about the gate. A parallel implementation of
/// "does a gate run this" would satisfy every other assertion here and fail that
/// one, which is the whole point of the relation.
#[test]
fn a_lane_whose_gate_is_not_live_is_not_checked() {
    let root = liveness_fixture("lane-gate");
    let base = lane("lean-proved", "lean", "formal/lean/Proved.lean", "checked");
    assert_legacy_accepted(&root, &base);
    assert_eq!(lane_codes(&root, &base), BTreeSet::new());

    let mut undeclared = base.clone();
    undeclared.checked_by = None;
    assert_legacy_accepted(&root, &undeclared);
    assert!(
        lane_codes(&root, &undeclared).contains("proof_lane_gate_undeclared"),
        "a checked lane that names no gate must not be credited with one: {:?}",
        lane_codes(&root, &undeclared)
    );

    let mut unresolved = base.clone();
    unresolved.checked_by = Some("no_such_gate".to_owned());
    assert_legacy_accepted(&root, &unresolved);
    assert!(
        lane_codes(&root, &unresolved).contains("proof_lane_gate_unresolved"),
        "{:?}",
        lane_codes(&root, &unresolved)
    );

    let mut stubbed = base.clone();
    stubbed.checked_by = Some("stub_gate".to_owned());
    assert_legacy_accepted(&root, &stubbed);
    assert!(
        lane_codes(&root, &stubbed).contains("proof_lane_gate_not_live"),
        "a lane checked by a stub row is not CI-checked: {:?}",
        lane_codes(&root, &stubbed)
    );

    // The delegation. `toothless_gate` is `status = "live"` and its artifact
    // exists, so nothing short of the full liveness read distinguishes it from
    // `lane_gate` — the difference is that `scripts/toothless.sh` has no nonzero
    // exit in live code, which only `liveness::assess` knows.
    let mut toothless = base.clone();
    toothless.checked_by = Some("toothless_gate".to_owned());
    assert_legacy_accepted(&root, &toothless);
    let defects = lane_defects(&root, &toothless);
    let codes: BTreeSet<String> = defects
        .iter()
        .map(|defect| defect.kind.code().to_owned())
        .collect();
    assert!(
        codes.contains("proof_lane_gate_not_live"),
        "a lane checked by a gate that cannot fail is not CI-checked: {codes:?}"
    );
    assert!(
        defects
            .iter()
            .any(|defect| defect.detail.contains("has no nonzero exit in live code")),
        "the lane verdict must CARRY the checker-liveness reader's own finding — a \
         second implementation of \"does a gate run this\" would pass every other \
         assertion here. Got: {:?}",
        defects
            .iter()
            .map(|defect| defect.detail.clone())
            .collect::<Vec<_>>()
    );
}

/// CAN FAIL. The artifact exists and typechecks; it assumes its conclusion.
#[test]
fn a_proof_that_admits_its_conclusion_is_not_checked() {
    let root = liveness_fixture("lane-admit");
    let proved = lane("lean-proved", "lean", "formal/lean/Proved.lean", "checked");
    assert_eq!(lane_codes(&root, &proved), BTreeSet::new());

    for (id, artifact, expected) in [
        (
            "lean-admitted",
            "formal/lean/Admitted.lean",
            "proof_lane_admits_anything",
        ),
        (
            "lean-axiomatised",
            "formal/lean/Axiomatised.lean",
            "proof_lane_admits_anything",
        ),
        (
            "lean-empty",
            "formal/lean/Empty.lean",
            "proof_lane_proves_nothing",
        ),
    ] {
        let row = lane(id, "lean", artifact, "checked");
        assert_legacy_accepted(&root, &row);
        assert!(
            lane_codes(&root, &row).contains(expected),
            "{id}: expected {expected}, got {:?}",
            lane_codes(&root, &row)
        );
    }

    // The other direction, which is the one that catches a fix that merely
    // widened a pattern: an admit token inside a comment — including a NESTED
    // block comment — is not an admit.
    let masked = lane("lean-masked", "lean", "formal/lean/Masked.lean", "checked");
    assert_eq!(
        lane_codes(&root, &masked),
        BTreeSet::new(),
        "an admit token in a comment is not an admit; a reader that says otherwise is \
         pattern-matching, not parsing"
    );
}

/// CAN FAIL, for a model checker. TLC explores the state space and asserts
/// nothing about it unless a config names a property.
#[test]
fn a_model_that_checks_no_property_is_not_checked() {
    let root = liveness_fixture("lane-model");
    let checked = lane(
        "tla-checked",
        "tlaplus",
        "formal/tla/Checked.tla",
        "checked",
    );
    assert_eq!(lane_codes(&root, &checked), BTreeSet::new());

    for (id, artifact) in [
        ("tla-unchecked", "formal/tla/Unchecked.tla"),
        ("tla-configless", "formal/tla/NoConfig.tla"),
    ] {
        let row = lane(id, "tlaplus", artifact, "checked");
        assert_legacy_accepted(&root, &row);
        assert!(
            lane_codes(&root, &row).contains("proof_lane_proves_nothing"),
            "{id}: {:?}",
            lane_codes(&root, &row)
        );
    }
}

/// THE COMPLETENESS GUARD. An unguarded reader fails OPEN.
///
/// `validate` rejects an unknown `lane` value in its schema pass, so it would be
/// easy to leave the checkedness reader with two arms and no third. Then a row
/// type nothing here understands would come back with no defects — reported
/// checked because no reader looked at it, which is the failure mode of every
/// entry in this file.
#[test]
fn a_lane_of_an_unreadable_system_is_not_silently_checked() {
    let root = liveness_fixture("lane-system");
    let mut row = lane(
        "isabelle-what",
        "lean",
        "formal/lean/Proved.lean",
        "checked",
    );
    assert_eq!(lane_codes(&root, &row), BTreeSet::new());
    row.lane = "isabelle".to_owned();
    assert_legacy_accepted(&root, &row);
    assert!(
        lane_codes(&root, &row).contains("proof_lane_system_unreadable"),
        "a checked lane of a system no reader adjudicates must be reported, not passed: \
         {:?}",
        lane_codes(&root, &row)
    );
}

/// REGISTERED. The path is checked before it is joined — and on EVERY lane,
/// including a declared one.
///
/// `Path::join` discards the root when handed an absolute path, so an unsafe
/// artifact passes `is_file()` the instant somebody flips the status. The lane
/// reader never had the guard `appendix_a::safe_repository_relative` provides;
/// `tl0o` found the same omission in the checker read and fixed it only there.
#[test]
fn a_lane_artifact_path_is_checked_before_it_is_joined() {
    let root = liveness_fixture("lane-path");
    let escape = root.join("Cargo.toml");
    let escape = escape.to_str().expect("fixture path is utf-8");

    let mut checked = lane("lean-escape", "lean", escape, "checked");
    assert_legacy_accepted(&root, &checked);
    assert!(
        lane_codes(&root, &checked).contains("checker_artifact_path_unsafe"),
        "{:?}",
        lane_codes(&root, &checked)
    );

    // And on a DECLARED lane, where nothing checked anything at all. This one
    // the pre-fix predicate never even evaluated: the `declared` arm was an
    // empty match arm, so an unsafe path sat in the registry unremarked until
    // the promotion that made it dangerous.
    checked.status = "declared".to_owned();
    assert!(
        lane_codes(&root, &checked).contains("checker_artifact_path_unsafe"),
        "a declared lane's artifact path is checkable today even though its artifact is \
         not: {:?}",
        lane_codes(&root, &checked)
    );
}
