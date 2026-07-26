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
    assert!(found.contains(&"site_unledgered".to_owned()), "got {found:?}");
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
    assert!(
        report.forbid_verdicts.values().all(|v| *v),
        "every real member must inherit the workspace forbid"
    );
}
