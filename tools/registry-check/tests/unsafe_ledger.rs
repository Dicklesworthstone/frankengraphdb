//! Mutation proof for the unsafe-boundary ledger checker
//! (bead `fgdb-w1-unsafe-ledger-icp`; the `unsafe_ledger_ci_e2e` row).
//!
//! A boundary checker is only worth its exit code if each failure mode has been
//! SEEN to fire. Every test here builds a synthetic workspace, seeds exactly one
//! defect, and asserts the specific violation code appears — and each is paired
//! with the clean control, so a checker that failed everything (or nothing)
//! could not pass this suite.
//!
//! The vacuity cases matter most and are tested first. A ledger checker that
//! silently passes when it cannot find the ledger, cannot read a source file,
//! or has a broken site scanner is strictly worse than no checker at all: it
//! launders an unaudited tree as audited. Each of those is asserted to FAIL.

use registry_check::unsafe_ledger::{
    SCANNER_FIXTURE_SITES, check_workspace, scan_sites, scanner_fixture,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn scratch(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "fgdb-unsafe-ledger-{}-{}-{n}",
        std::process::id(),
        tag
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

const LEDGER_HEAD: &str = r#"schema_version = 1

[registry]
name = "unsafe_boundary_ledger"

[[island]]
name = "fgdb-unsafe-simd"
charter = "SIMD kernels with bit-identical scalar fallbacks."
status = "planned"
"#;

/// A minimal but structurally faithful workspace: root manifest with the
/// forbid default, one ordinary member that inherits it, and the ledger.
fn clean_workspace(tag: &str) -> PathBuf {
    let root = scratch(tag);
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\n    \"crates/fgdb-ordinary\",\n]\n\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
    )
    .unwrap();
    let member = root.join("crates/fgdb-ordinary");
    fs::create_dir_all(member.join("src")).unwrap();
    fs::write(
        member.join("Cargo.toml"),
        "[package]\nname = \"fgdb-ordinary\"\n\n[lints]\nworkspace = true\n",
    )
    .unwrap();
    fs::write(member.join("src/lib.rs"), "pub fn safe() {}\n").unwrap();
    fs::create_dir_all(root.join("registries")).unwrap();
    fs::write(
        root.join("registries/unsafe_boundary_ledger.toml"),
        LEDGER_HEAD,
    )
    .unwrap();
    root
}

/// The workspace as it will look the day the first island lands: the roster
/// says `present`, the crate exists, its root uses `deny` rather than `forbid`
/// — and its single unsafe site is written in the `cfg_attr`-wrapped form.
///
/// That form is the point. `deny` is exactly what a `cfg_attr`-wrapped allow
/// CAN lower, so this workspace is where the site scanner stops being a
/// formality and starts being the only thing standing between an unaudited
/// raw-pointer kernel and a green CI run. `ledger_sites` is appended verbatim,
/// so the same tree can be checked with the row present and absent.
fn workspace_with_landed_island(tag: &str, ledger_sites: &str) -> PathBuf {
    let root = clean_workspace(tag);
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\n    \"crates/fgdb-ordinary\",\n    \"crates/fgdb-unsafe-simd\",\n]\n\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
    )
    .unwrap();
    let island = root.join("crates/fgdb-unsafe-simd");
    fs::create_dir_all(island.join("src")).unwrap();
    fs::write(
        island.join("Cargo.toml"),
        "[package]\nname = \"fgdb-unsafe-simd\"\n",
    )
    .unwrap();
    let hash = '#';
    fs::write(
        island.join("src/lib.rs"),
        format!(
            "{hash}![deny(unsafe_code)]\n\n\
             {hash}[cfg_attr(target_arch = \"x86_64\", allow(unsafe_code))]\n\
             unsafe fn kernel() {{}}\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("registries/unsafe_boundary_ledger.toml"),
        format!(
            "{}{ledger_sites}",
            LEDGER_HEAD.replace("status = \"planned\"", "status = \"present\"")
        ),
    )
    .unwrap();
    root
}

const ISLAND_SITE_ROW: &str = r#"
[[site]]
row_id = "simd-kernel-1"
island = "fgdb-unsafe-simd"
path = "crates/fgdb-unsafe-simd/src/lib.rs"
symbol = "unsafe fn kernel() {}"
stated_invariant = "the caller has proven the slice is 16-lane aligned"
evidence = "kernel_dispatch_differential, miri lane"
fallback = "SCALAR_KERNEL, bit-identical on every dispatch path"
no_claim_boundary = "says nothing about targets outside the dispatch matrix"
"#;

fn codes(root: &Path) -> Vec<String> {
    check_workspace(root)
        .1
        .into_iter()
        .map(|v| v.code)
        .collect()
}

// ---------------------------------------------------------------------------
// The control. Everything below is only meaningful because this passes.
// ---------------------------------------------------------------------------

#[test]
fn clean_workspace_passes() {
    let root = clean_workspace("clean");
    let (report, violations) = check_workspace(&root);
    assert!(
        violations.is_empty(),
        "clean control must pass, got {violations:?}"
    );
    assert_eq!(report.crates_scanned, 1);
    assert_eq!(report.scanned_sites.len(), 0);
    assert_eq!(report.scanner_self_test_sites, SCANNER_FIXTURE_SITES);
}

// ---------------------------------------------------------------------------
// Vacuity cases: the checker must FAIL, never skip.
// ---------------------------------------------------------------------------

#[test]
fn absent_ledger_fails_rather_than_reporting_an_empty_unsafe_surface() {
    let root = clean_workspace("no-ledger");
    fs::remove_file(root.join("registries/unsafe_boundary_ledger.toml")).unwrap();
    assert!(
        codes(&root).contains(&"ledger_absent_or_unreadable".to_owned()),
        "a missing ledger must fail; passing here would launder an unaudited tree"
    );
}

#[test]
fn unreadable_source_fails_rather_than_being_skipped() {
    let root = clean_workspace("unreadable");
    // A directory where a .rs file is expected: read_to_string fails, and the
    // scan must not quietly treat the crate as clean.
    fs::create_dir_all(root.join("crates/fgdb-ordinary/src/trap.rs")).unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"source_unreadable".to_owned())
            || found.contains(&"source_tree_unreadable".to_owned()),
        "an unreadable source must fail the run, got {found:?}"
    );
}

#[test]
fn scanner_self_test_licenses_every_zero_site_result() {
    // The scanner is the input to every other conclusion. If it cannot
    // reproduce its own fixture, "0 sites" means nothing.
    let sites = scan_sites("<fixture>", &scanner_fixture());
    assert_eq!(
        sites.len(),
        SCANNER_FIXTURE_SITES,
        "self-test must reproduce exactly; otherwise a broken scanner reports a clean boundary"
    );
    let clean = check_workspace(&clean_workspace("licensed")).0;
    assert_eq!(clean.scanner_self_test_sites, SCANNER_FIXTURE_SITES);
}

// ---------------------------------------------------------------------------
// The three seeded violations the bead's E2E row names.
// ---------------------------------------------------------------------------

#[test]
fn unsafe_in_an_ordinary_crate_fails() {
    let root = clean_workspace("ordinary-unsafe");
    let hash = '#';
    fs::write(
        root.join("crates/fgdb-ordinary/src/lib.rs"),
        format!("{hash}[allow(unsafe_code)]\nunsafe fn sneaky() {{}}\n"),
    )
    .unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"unsafe_allow_outside_island".to_owned()),
        "got {found:?}"
    );
    assert!(
        found.contains(&"site_unledgered".to_owned()),
        "got {found:?}"
    );
}

#[test]
fn orphan_ledger_row_fails_so_the_ledger_cannot_rot() {
    let root = clean_workspace("orphan");
    fs::write(
        root.join("registries/unsafe_boundary_ledger.toml"),
        format!(
            "{LEDGER_HEAD}\n[[site]]\nrow_id = \"orphan-1\"\nisland = \"fgdb-unsafe-simd\"\n\
             path = \"crates/fgdb-unsafe-simd/src/gone.rs\"\nsymbol = \"unsafe fn gone() {{}}\"\n\
             stated_invariant = \"i\"\nevidence = \"e\"\nfallback = \"f\"\nno_claim_boundary = \"n\"\n"
        ),
    )
    .unwrap();
    assert!(
        codes(&root).contains(&"ledger_row_orphaned".to_owned()),
        "a row describing code that no longer exists must fail"
    );
}

#[test]
fn member_that_omits_lints_inheritance_fails() {
    // The sharpest silent hole: nothing at the crate root reveals that the
    // workspace forbid no longer applies to this crate.
    let root = clean_workspace("no-inherit");
    fs::write(
        root.join("crates/fgdb-ordinary/Cargo.toml"),
        "[package]\nname = \"fgdb-ordinary\"\n",
    )
    .unwrap();
    assert!(
        codes(&root).contains(&"member_does_not_inherit_forbid".to_owned()),
        "a member without `[lints] workspace = true` escapes forbid silently"
    );
}

/// The manifest half of the same silent hole, and the reason the inheritance
/// verdict is a section-aware parse rather than a substring test.
///
/// The first version asked whether the manifest text contained `[lints]` and
/// `workspace = true` *anywhere*. A crate that comments its lint table out
/// satisfies that while inheriting nothing — and the comment is the honest
/// spelling someone reaches for while debugging a lint, which is precisely when
/// the boundary needs to hold. Run against the real tree, that checker reported
/// `fgdb-types` as inheriting `forbid` and exited 0 with a raw-pointer deref in
/// its source, while `topology-check` failed the same tree with
/// `lints_not_inherited`.
#[test]
fn a_commented_out_lint_table_does_not_count_as_inheritance() {
    let root = clean_workspace("commented-lints");
    fs::write(
        root.join("crates/fgdb-ordinary/Cargo.toml"),
        "[package]\nname = \"fgdb-ordinary\"\n# TODO: restore [lints] workspace = true before release\n",
    )
    .unwrap();
    assert!(
        codes(&root).contains(&"member_does_not_inherit_forbid".to_owned()),
        "prose naming the lint table is not the lint table"
    );
}

/// The same hole reached without anyone doing anything odd at all.
///
/// `[lints]` carrying the crate's own lints and `dep = { workspace = true }` in
/// `[dependencies]` are both idiomatic, and a workspace that grows a
/// `[workspace.dependencies]` table produces the second spelling everywhere.
/// Together they satisfy a substring test while the crate inherits no lint
/// table at all, so `unsafe_code = "forbid"` does not reach it.
#[test]
fn an_own_lint_table_plus_a_workspace_dependency_is_not_inheritance() {
    let root = clean_workspace("own-lints");
    fs::write(
        root.join("crates/fgdb-ordinary/Cargo.toml"),
        "[package]\nname = \"fgdb-ordinary\"\n\n[lints]\nrust = { unused_imports = \"warn\" }\n\n[dependencies]\nfgdb-other = { workspace = true }\n",
    )
    .unwrap();
    assert!(
        codes(&root).contains(&"member_does_not_inherit_forbid".to_owned()),
        "a crate's own [lints] table overrides inheritance rather than granting it"
    );
}

/// The control for the two rows above: the same shape, inheriting for real.
/// Without this, "both bypasses fail" would also be satisfied by a checker that
/// had started failing every manifest it could not recognise.
#[test]
fn a_real_lint_table_beside_a_workspace_dependency_still_inherits() {
    let root = clean_workspace("inherit-with-deps");
    fs::write(
        root.join("crates/fgdb-ordinary/Cargo.toml"),
        "[package]\nname = \"fgdb-ordinary\"\n\n[lints]\nworkspace = true\n\n[dependencies]\nfgdb-other = { workspace = true }\n",
    )
    .unwrap();
    assert!(
        codes(&root).is_empty(),
        "inheriting members must pass: {:?}",
        codes(&root)
    );
}

/// Fail closed on a manifest the scanner cannot read. Guessing `true` here
/// would put the whole boundary behind a parser bug, which is the
/// looks-exactly-like-a-pass family this suite exists to close.
#[test]
fn an_unparseable_manifest_fails_rather_than_being_assumed_to_inherit() {
    let root = clean_workspace("unparseable");
    fs::write(
        root.join("crates/fgdb-ordinary/Cargo.toml"),
        "[package]\nname = \"fgdb-ordinary\"\n\n[lints]\nworkspace = true\nthis line is not key = value at all = = =\n[unterminated\n",
    )
    .unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"member_manifest_unparseable".to_owned()),
        "an unreadable manifest is unknown, not clean: {found:?}"
    );
}

// ---------------------------------------------------------------------------
// The evasions. A site the scanner cannot see is worse than an absent ledger,
// because the report says "0 sites" with the same confidence either way.
// ---------------------------------------------------------------------------

/// The bypass this suite exists to keep closed.
///
/// `#[cfg_attr(target_arch = "x86_64", allow(unsafe_code))]` is a real, ordinary
/// thing to write in a SIMD island — and the first version of the scanner
/// required the attribute body to begin literally with `allow(`, so it returned
/// false, the site was never counted, never matched against the ledger, and
/// never reported. It was harmless only for as long as every crate inherited
/// `forbid`; an island root uses `deny`, which this form lowers.
#[test]
fn a_cfg_attr_wrapped_allow_inside_an_island_must_still_be_ledgered() {
    let root = workspace_with_landed_island("cfg-attr-unledgered", "");
    let (report, violations) = check_workspace(&root);
    let found: Vec<&str> = violations.iter().map(|v| v.code.as_str()).collect();
    assert_eq!(
        report.scanned_sites.len(),
        1,
        "the wrapped allow must be COUNTED, not skipped: {:?}",
        report.scanned_sites
    );
    assert_eq!(report.scanned_sites[0].symbol, "unsafe fn kernel() {}");
    assert!(
        found.contains(&"site_unledgered"),
        "an unledgered site must fail even when its allow is wrapped, got {found:?}"
    );
    assert!(
        !found.contains(&"unsafe_allow_outside_island"),
        "an island is allowed to hold the site; it is not allowed to hold it unaudited"
    );
}

/// The other half of the proof. Without this, "the wrapped form fails" would
/// also be satisfied by a checker that had simply started failing everything.
#[test]
fn the_same_wrapped_site_passes_once_its_ledger_row_exists() {
    let root = workspace_with_landed_island("cfg-attr-ledgered", ISLAND_SITE_ROW);
    let (report, violations) = check_workspace(&root);
    assert!(
        violations.is_empty(),
        "a ledgered island site is the one thing that must pass, got {violations:?}"
    );
    assert_eq!(report.scanned_sites.len(), 1);
    assert!(report.orphan_rows.is_empty(), "the row matched its site");
    assert_eq!(report.crates_scanned, 2);
}

/// An inner attribute relaxes everything up to the end of its module, which is
/// the broadest form there is — and it was invisible for a different reason:
/// the scanner only ever looked at lines beginning `#[`.
#[test]
fn a_module_scoped_allow_in_an_ordinary_crate_fails() {
    let root = clean_workspace("module-scope");
    let hash = '#';
    fs::write(
        root.join("crates/fgdb-ordinary/src/lib.rs"),
        format!("{hash}![allow(unsafe_code)]\n\nunsafe fn sneaky() {{}}\n"),
    )
    .unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"unsafe_allow_outside_island".to_owned()),
        "got {found:?}"
    );
    assert!(
        found.contains(&"site_unledgered".to_owned()),
        "got {found:?}"
    );
}

/// `expect` and `warn` are levels below `deny`, so both compile `unsafe` inside
/// an island. A ledger that enumerated only `allow` would be enumerating a
/// spelling, not the unsafe surface.
#[test]
fn expect_and_warn_are_relaxations_too() {
    for (tag, level) in [("expect-level", "expect"), ("warn-level", "warn")] {
        let root = clean_workspace(tag);
        let hash = '#';
        fs::write(
            root.join("crates/fgdb-ordinary/src/lib.rs"),
            format!("{hash}[{level}(unsafe_code)]\nunsafe fn sneaky() {{}}\n"),
        )
        .unwrap();
        let found = codes(&root);
        assert!(
            found.contains(&"unsafe_allow_outside_island".to_owned()),
            "{level} lowers deny just as allow does, got {found:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The roster is a claim, and both directions of it are enforced.
// ---------------------------------------------------------------------------

#[test]
fn island_declared_present_but_missing_fails() {
    let root = clean_workspace("present-missing");
    fs::write(
        root.join("registries/unsafe_boundary_ledger.toml"),
        LEDGER_HEAD.replace("status = \"planned\"", "status = \"present\""),
    )
    .unwrap();
    assert!(
        codes(&root).contains(&"island_declared_present_but_absent".to_owned()),
        "the ledger claimed a boundary crate that does not exist"
    );
}

#[test]
fn island_that_appears_while_still_planned_fails() {
    let root = clean_workspace("planned-appeared");
    fs::create_dir_all(root.join("crates/fgdb-unsafe-simd/src")).unwrap();
    assert!(
        codes(&root).contains(&"island_declared_planned_but_present".to_owned()),
        "an island must be admitted to the ledger before it lands, never after"
    );
}

#[test]
fn workspace_without_forbid_fails() {
    let root = clean_workspace("no-forbid");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\n    \"crates/fgdb-ordinary\",\n]\n",
    )
    .unwrap();
    assert!(
        codes(&root).contains(&"workspace_forbid_absent".to_owned()),
        "forbid cannot be lowered, which is the whole reason islands are separate crates"
    );
}

#[test]
fn ledger_row_with_blank_evidence_fails() {
    let root = clean_workspace("vacuous-row");
    fs::write(
        root.join("registries/unsafe_boundary_ledger.toml"),
        format!(
            "{LEDGER_HEAD}\n[[site]]\nrow_id = \"blank-1\"\nisland = \"fgdb-unsafe-simd\"\n\
             path = \"p.rs\"\nsymbol = \"unsafe fn x() {{}}\"\n\
             stated_invariant = \"i\"\nevidence = \"   \"\nfallback = \"f\"\nno_claim_boundary = \"n\"\n"
        ),
    )
    .unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"ledger_row_field_vacuous".to_owned()),
        "a site with blank evidence is an unaudited site wearing a ledger row, got {found:?}"
    );
}

#[test]
fn site_naming_an_undeclared_island_fails() {
    let root = clean_workspace("unknown-island");
    fs::write(
        root.join("registries/unsafe_boundary_ledger.toml"),
        format!(
            "{LEDGER_HEAD}\n[[site]]\nrow_id = \"x-1\"\nisland = \"fgdb-unsafe-nowhere\"\n\
             path = \"p.rs\"\nsymbol = \"unsafe fn x() {{}}\"\n\
             stated_invariant = \"i\"\nevidence = \"e\"\nfallback = \"f\"\nno_claim_boundary = \"n\"\n"
        ),
    )
    .unwrap();
    assert!(
        codes(&root).contains(&"ledger_row_island_unknown".to_owned()),
        "a site may only name a declared island"
    );
}

/// The real repository must pass its own checker. This is the row that would
/// have caught the fixture leak: on the first run the checker failed this very
/// crate with two `site_unledgered` violations, because the scanner fixture was
/// written as literal source text.
#[test]
fn the_real_workspace_passes_its_own_boundary_check() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (report, violations) = check_workspace(&root);
    assert!(
        violations.is_empty(),
        "the real workspace must satisfy its own unsafe boundary, got {violations:?}"
    );
    assert!(
        report.crates_scanned >= 11,
        "expected the real member list, got {}",
        report.crates_scanned
    );
    // Policy-dependent, and both directions matter. An ordinary member that
    // does not inherit has silently escaped `forbid`; an ISLAND that does
    // inherit could not compile a single ledgered site, because `forbid` cannot
    // be lowered — so a `true` verdict there is not a stricter reading of the
    // law, it is the checker misreading the manifest.
    //
    // This assertion previously read `values().all(|v| *v)` and passed only
    // because the verdict came from a substring test that matched the island
    // manifest's own comment explaining why it omits the lint table. A green
    // bar built on a false fact is the failure mode this suite is about.
    for (name, inherits) in &report.forbid_verdicts {
        let is_island = name.starts_with("fgdb-unsafe-");
        assert_eq!(
            *inherits, !is_island,
            "{name}: inherits_workspace_forbid={inherits}, but is_island={is_island} \
             (ordinary crates must inherit `forbid`; islands must not, or their \
             ledgered allow sites cannot compile)"
        );
    }
    assert!(
        report
            .forbid_verdicts
            .keys()
            .any(|name| name.starts_with("fgdb-unsafe-")),
        "the island half of the rule above is vacuous unless an island is present"
    );
}
