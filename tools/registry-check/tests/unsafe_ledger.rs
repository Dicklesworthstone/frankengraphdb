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
    self, SAFE_FACING_FIXTURE_FINDINGS, SAFE_FACING_FIXTURE_PUB_TOKENS, SCANNER_FIXTURE_SITES,
    check_workspace, public_api, safe_facing_fixture, scan_sites, scanner_fixture,
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

const EMPTY_VERIFICATION_LANES: &str = r#"schema_version = 1

[[lane]]
tool = "miri"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["miri", "rust-src"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "fixture Miri lane"

[[lane]]
tool = "asan"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["rust-src", "llvm-tools-preview"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "fixture ASAN lane"

[[lane]]
tool = "tsan"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["rust-src", "llvm-tools-preview"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "fixture TSAN lane"
"#;

const ONE_SITE_VERIFICATION_LANES: &str = r#"schema_version = 1

[[lane]]
tool = "miri"
status = "checked"
target = "x86_64-unknown-linux-gnu"
required_components = ["miri", "rust-src"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "fixture Miri lane"

[[lane]]
tool = "asan"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["rust-src", "llvm-tools-preview"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "fixture ASAN lane"

[[lane]]
tool = "tsan"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["rust-src", "llvm-tools-preview"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "fixture TSAN lane"

[[cell]]
site_row_id = "simd-kernel-1"
tool = "miri"
disposition = "checked"
rationale = "fixture Miri rationale"
workload = "cargo miri test fixture"

[[cell]]
site_row_id = "simd-kernel-1"
tool = "asan"
disposition = "candidate"
rationale = "fixture ASAN rationale"
workload = ""

[[cell]]
site_row_id = "simd-kernel-1"
tool = "tsan"
disposition = "candidate"
rationale = "fixture TSAN rationale"
workload = ""
"#;

const FIXTURE_CHECKER_INDEX: &str = r#"schema_version = 1

[registry]
name = "checker_index"

[[checker]]
symbol = "w1_unsafe_tool_lanes"
kind = "script"
artifact = "scripts/w1_unsafe_tool_lanes.sh"
status = "live"
unit = "artifact"
"#;

/// A minimal but structurally faithful workspace: root manifest with the
/// forbid default, one ordinary member that inherits it, and the ledger.
fn clean_workspace(tag: &str) -> PathBuf {
    workspace_fixture(tag, true)
}

fn workspace_fixture(tag: &str, include_verification_manifest: bool) -> PathBuf {
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
    if include_verification_manifest {
        fs::write(
            root.join("registries/unsafe_verification_lanes.toml"),
            EMPTY_VERIFICATION_LANES,
        )
        .unwrap();
    }
    fs::write(
        root.join("registries/checker_index.toml"),
        FIXTURE_CHECKER_INDEX,
    )
    .unwrap();
    fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2026-07-05\"\ncomponents = [\"rustfmt\", \"clippy\", \"miri\", \"rust-src\", \"llvm-tools-preview\"]\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("scripts")).unwrap();
    fs::write(
        root.join("scripts/w1_unsafe_tool_lanes.sh"),
        "#!/usr/bin/env bash\nexit 1\n",
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
    fs::write(
        root.join("registries/unsafe_verification_lanes.toml"),
        ONE_SITE_VERIFICATION_LANES,
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
no_claim_boundary = "Miri is checked; ASAN and TSAN remain candidates."
tool_no_claim_boundaries = [
    "miri|checked|fixture Miri rationale",
    "asan|candidate|fixture ASAN rationale",
    "tsan|candidate|fixture TSAN rationale",
]
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
    assert_eq!(report.verification_lanes, 3);
    assert_eq!(report.verification_cells, 0);
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
fn absent_verification_manifest_fails_rather_than_inferring_tool_boundaries() {
    let root = workspace_fixture("no-verification-manifest", false);
    assert!(
        codes(&root).contains(&"unsafe_verification_lanes_absent_or_unreadable".to_owned()),
        "a missing site x tool manifest must fail rather than imply every cell is excluded"
    );
}

#[test]
fn a_lane_runner_that_cannot_fail_is_not_live() {
    let root = clean_workspace("lane-runner-cannot-fail");
    fs::write(
        root.join("scripts/w1_unsafe_tool_lanes.sh"),
        "#!/usr/bin/env bash\nexit 0\n",
    )
    .unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"unsafe_lane_runner_not_live".to_owned()),
        "status = live plus an existing script cannot replace the liveness proof: {found:?}"
    );
}

#[test]
fn a_lane_target_that_does_not_match_its_runner_fails() {
    let root = clean_workspace("lane-target-drift");
    let path = root.join("registries/unsafe_verification_lanes.toml");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replacen(
            "target = \"x86_64-unknown-linux-gnu\"",
            "target = \"aarch64-unknown-linux-gnu\"",
            1,
        ),
    )
    .unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"unsafe_lane_target_mismatch".to_owned()),
        "the target field must describe the target the runner actually executes: {found:?}"
    );
}

#[test]
fn removing_a_manifest_cell_fails_in_the_ledger_to_manifest_direction() {
    let root = workspace_with_landed_island("manifest-cell-removed", ISLAND_SITE_ROW);
    assert!(
        codes(&root).is_empty(),
        "the one-site control must pass before its manifest is mutated: {:?}",
        codes(&root)
    );
    let path = root.join("registries/unsafe_verification_lanes.toml");
    let text = fs::read_to_string(&path).unwrap();
    let block = r#"[[cell]]
site_row_id = "simd-kernel-1"
tool = "asan"
disposition = "candidate"
rationale = "fixture ASAN rationale"
workload = ""

"#;
    assert_eq!(
        text.matches(block).count(),
        1,
        "the mutation must remove exactly one controlled cell"
    );
    fs::write(&path, text.replacen(block, "", 1)).unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"unsafe_lane_cell_missing".to_owned()),
        "a ledger boundary with no manifest cell must fail, got {found:?}"
    );
    assert!(
        !found.contains(&"unsafe_ledger_tool_boundary_missing".to_owned()),
        "this mutation removed only the manifest side, got {found:?}"
    );
}

#[test]
fn removing_a_ledger_boundary_fails_in_the_manifest_to_ledger_direction() {
    let root = workspace_with_landed_island("ledger-boundary-removed", ISLAND_SITE_ROW);
    assert!(
        codes(&root).is_empty(),
        "the one-site control must pass before its ledger is mutated: {:?}",
        codes(&root)
    );
    let path = root.join("registries/unsafe_boundary_ledger.toml");
    let text = fs::read_to_string(&path).unwrap();
    let line = "    \"asan|candidate|fixture ASAN rationale\",\n";
    assert_eq!(
        text.matches(line).count(),
        1,
        "the mutation must remove exactly one controlled boundary"
    );
    fs::write(&path, text.replacen(line, "", 1)).unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"unsafe_ledger_tool_boundary_missing".to_owned()),
        "a manifest cell with no ledger boundary must fail, got {found:?}"
    );
    assert!(
        !found.contains(&"unsafe_lane_cell_missing".to_owned()),
        "this mutation removed only the ledger side, got {found:?}"
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
             stated_invariant = \"i\"\nevidence = \"e\"\nfallback = \"f\"\nno_claim_boundary = \"Miri ASAN TSAN\"\n\
             tool_no_claim_boundaries = [\"miri|excluded|m\", \"asan|candidate|a\", \"tsan|candidate|t\"]\n"
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
             stated_invariant = \"i\"\nevidence = \"   \"\nfallback = \"f\"\nno_claim_boundary = \"Miri ASAN TSAN\"\n\
             tool_no_claim_boundaries = [\"miri|excluded|m\", \"asan|candidate|a\", \"tsan|candidate|t\"]\n"
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
             stated_invariant = \"i\"\nevidence = \"e\"\nfallback = \"f\"\nno_claim_boundary = \"Miri ASAN TSAN\"\n\
             tool_no_claim_boundaries = [\"miri|excluded|m\", \"asan|candidate|a\", \"tsan|candidate|t\"]\n"
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
    // The safe-facing rule, on the tree it exists to protect. Both numbers are
    // load-bearing: a run that scanned zero islands, or read zero public items
    // out of the ones it did scan, concluded "no public unsafe fn anywhere"
    // over nothing at all — which is indistinguishable from a pass.
    assert_eq!(
        report.islands_api_scanned, 3,
        "the three landed islands must all have had their API read, got {}",
        report.islands_api_scanned
    );
    assert!(
        report.island_public_items >= 50,
        "the islands export a real API; reading {} public items out of them means the \
         reader walked past most of the surface it is quantified over",
        report.island_public_items
    );
    assert_eq!(
        report.safe_facing_self_test_findings,
        unsafe_ledger::SAFE_FACING_FIXTURE_FINDINGS
    );
    assert_eq!(report.verification_lanes, 3);
    assert_eq!(
        report.verification_cells, 18,
        "six ledger sites x three tools must be a complete matrix"
    );
    assert_eq!(report.checked_cells, 1);
    assert_eq!(report.candidate_cells, 12);
    assert_eq!(report.excluded_cells, 5);
}

// ---------------------------------------------------------------------------
// The safe-facing API of an island (bead `fgdb-n7mb`).
//
// The ledger enumerates where unsafe is WRITTEN. Nothing enumerated where it is
// REACHABLE FROM, so a `pub unsafe fn` or a raw pointer in a public signature
// moved the boundary outward while every check above stayed green — the rule
// lived in three crate-root doc comments and was enforced by nobody.
//
// Every test below seeds exactly one form and asserts the specific code, and
// each is paired with the clean control immediately above it, so neither a
// checker that fires on everything nor one that fires on nothing survives. The
// IGNORED cases are not padding: a reader that rejected any asterisk would pass
// every positive test here.
// ---------------------------------------------------------------------------

/// A workspace whose one island's `src/lib.rs` is exactly `source`.
///
/// The island carries no `allow(unsafe_code)` site and the ledger carries no
/// rows, so the site<->row bijection is trivially satisfied and the only thing
/// that can fire is the safe-facing rule.
fn workspace_with_island_source(tag: &str, source: &str) -> PathBuf {
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
    fs::write(island.join("src/lib.rs"), source).unwrap();
    fs::write(
        root.join("registries/unsafe_boundary_ledger.toml"),
        LEDGER_HEAD.replace("status = \"planned\"", "status = \"present\""),
    )
    .unwrap();
    root
}

/// A clean island source: a real public API, none of it unsafe-facing.
const CLEAN_ISLAND: &str = "\
#![deny(unsafe_code)]

/// A public struct with a public field.
pub struct Masks {
    pub matching: u16,
    lanes: usize,
}

/// A public fn over safe types only.
pub fn classify(lanes: &[u8; 16], tag: u8) -> Masks {
    let _ = (lanes, tag);
    Masks { matching: 0, lanes: 0 }
}
";

fn island_codes(tag: &str, source: &str) -> Vec<String> {
    codes(&workspace_with_island_source(tag, source))
}

/// The control every test below is measured against. A clean island must pass
/// the whole boundary check, safe-facing rule included.
#[test]
fn a_clean_island_api_passes() {
    let root = workspace_with_island_source("api-clean", CLEAN_ISLAND);
    let (report, violations) = check_workspace(&root);
    assert!(
        violations.is_empty(),
        "the clean island control must pass, got {violations:?}"
    );
    assert_eq!(report.islands_api_scanned, 1);
    assert_eq!(report.island_api_files, 1);
    assert!(
        report.island_public_items >= 3,
        "the control is vacuous unless the reader actually found the island's public \
         items, got {}",
        report.island_public_items
    );
}

// --- what must fire ---

#[test]
fn a_public_unsafe_fn_in_an_island_fails() {
    let found = island_codes(
        "api-unsafe-fn",
        "pub unsafe fn escape_hatch(len: usize) -> usize { len }\n",
    );
    assert!(
        found.contains(&"island_public_unsafe_fn".to_owned()),
        "an island that exports an unsafe fn is no longer an island, got {found:?}"
    );
}

#[test]
fn a_public_raw_pointer_parameter_fails() {
    let found = island_codes(
        "api-raw-param",
        "pub fn adopt(base: *mut u8) -> usize { base as usize }\n",
    );
    assert!(
        found.contains(&"island_public_raw_pointer".to_owned()),
        "got {found:?}"
    );
}

#[test]
fn a_public_raw_pointer_return_on_a_wrapped_signature_fails() {
    // The line break is the point: the `pub fn` and the `*const` are on
    // different lines, which is what defeats a line-wise matcher.
    let found = island_codes(
        "api-raw-wrapped",
        "pub fn origin(\n    len: usize,\n) -> *const u8 {\n    let _ = len;\n    core::ptr::null()\n}\n",
    );
    assert!(
        found.contains(&"island_public_raw_pointer".to_owned()),
        "a signature wrapped across lines must still be read whole, got {found:?}"
    );
}

#[test]
fn a_public_named_field_raw_pointer_fails() {
    let found = island_codes(
        "api-field",
        "pub struct View {\n    pub base: *mut u8,\n    len: usize,\n}\n",
    );
    assert!(
        found.contains(&"island_public_raw_pointer".to_owned()),
        "got {found:?}"
    );
}

/// The fail-open case, and the reason the field reader balances angle brackets.
///
/// `Slots<u32, *mut u8>` has a comma that belongs to the generic argument list.
/// A reader that ended the field there hands the raw pointer to a fragment with
/// no `pub` in front of it and reports the struct clean — a SILENT ACCEPT, and
/// silence is the direction that hides longest.
#[test]
fn a_field_type_with_an_angle_bracket_comma_is_not_split() {
    let found = island_codes(
        "api-field-generic",
        "pub struct Table {\n    pub slots: Slots<u32, *mut u8>,\n    len: usize,\n}\n",
    );
    assert!(
        found.contains(&"island_public_raw_pointer".to_owned()),
        "a comma inside the generic arguments must not end the field, got {found:?}"
    );
}

#[test]
fn a_public_tuple_field_raw_pointer_fails() {
    let found = island_codes("api-tuple", "pub struct Base(pub *const u8, usize);\n");
    assert!(
        found.contains(&"island_public_raw_pointer".to_owned()),
        "got {found:?}"
    );
}

#[test]
fn a_public_type_alias_to_a_raw_pointer_fails() {
    let found = island_codes("api-alias", "pub type Cell = *mut u8;\n");
    assert!(
        found.contains(&"island_public_raw_pointer".to_owned()),
        "got {found:?}"
    );
}

#[test]
fn an_enum_variant_payload_raw_pointer_fails() {
    // A variant payload says no `pub` and needs none: it is public with the
    // enum. A reader that required the keyword would miss every one of them.
    let found = island_codes(
        "api-enum",
        "pub enum Carrier {\n    Empty,\n    Raw(*mut u8),\n}\n",
    );
    assert!(
        found.contains(&"island_public_raw_pointer".to_owned()),
        "got {found:?}"
    );
}

#[test]
fn an_unsafe_trait_method_is_public_with_the_trait() {
    let found = island_codes(
        "api-trait",
        "pub trait Boundary {\n    unsafe fn arm();\n}\n",
    );
    assert!(
        found.contains(&"island_public_unsafe_fn".to_owned()),
        "a trait method says no `pub` and is exactly as public as its trait, got {found:?}"
    );
}

#[test]
fn a_public_static_raw_pointer_fails() {
    let found = island_codes(
        "api-static",
        "pub static ORIGIN: *const u8 = core::ptr::null();\n",
    );
    assert!(
        found.contains(&"island_public_raw_pointer".to_owned()),
        "got {found:?}"
    );
}

// --- what must NOT fire. A reader that rejected every asterisk would have
// --- passed every test above and still be worthless.

#[test]
fn a_doc_comment_naming_a_raw_pointer_is_not_a_violation() {
    let found = island_codes(
        "api-doc",
        "/// Returns the length. Callers used to pass a *mut u8 here.\npub fn len(v: &[u8]) -> usize {\n    v.len()\n}\n",
    );
    assert!(
        found.is_empty(),
        "prose is not a signature; the mask is what makes that true, got {found:?}"
    );
}

#[test]
fn a_raw_pointer_in_a_function_body_is_not_the_api() {
    // This is the case that would make every real island fail: the body is
    // where ledgered unsafe code lives, and it is not exported.
    let found = island_codes(
        "api-body",
        "pub fn addr(v: &mut [u8]) -> usize {\n    let base: *mut u8 = v.as_mut_ptr();\n    base as usize\n}\n",
    );
    assert!(
        found.is_empty(),
        "a raw pointer inside a body is ledgered unsafe code, not an export, got {found:?}"
    );
}

#[test]
fn a_restricted_visibility_is_not_the_safe_facing_api() {
    let found = island_codes(
        "api-restricted",
        "pub(crate) fn adopt(base: *mut u8) -> usize {\n    base as usize\n}\n",
    );
    assert!(
        found.is_empty(),
        "`pub(crate)` is not reachable from a safe crate, got {found:?}"
    );
}

#[test]
fn a_private_item_is_not_the_safe_facing_api() {
    let found = island_codes(
        "api-private",
        "unsafe fn kernel() {}\n\nstruct Hidden {\n    pub base: *mut u8,\n}\n\ntrait Interior {\n    unsafe fn arm();\n}\n",
    );
    assert!(
        found.is_empty(),
        "nothing here is reachable from outside the island, got {found:?}"
    );
}

#[test]
fn multiplication_is_not_a_raw_pointer() {
    let found = island_codes(
        "api-arith",
        "pub const WIDTH: usize = 16;\n\npub fn lanes(v: [u8; WIDTH * 2]) -> usize {\n    v.len()\n}\n",
    );
    assert!(
        found.is_empty(),
        "`*` is a raw pointer only when `const` or `mut` follows it, got {found:?}"
    );
}

#[test]
fn a_constant_exports_its_type_and_not_its_initialiser() {
    // The turbofish is live code the mask does not blank, so a reader that
    // scanned the whole item instead of the declared type would flag a `usize`.
    let found = island_codes(
        "api-const-init",
        "pub const WIDE: usize = core::mem::size_of::<*mut u8>();\n",
    );
    assert!(
        found.is_empty(),
        "the exported type is `usize`, got {found:?}"
    );
}

// --- vacuity. Every one of these must FAIL, never skip.

/// The licence for every zero above. `public_api` must reproduce its own
/// fixture exactly: the findings it has to catch, the `pub` tokens it has to
/// account for, and no parse failures. Count alone would be satisfied by a
/// reader that rejected everything, which is why the fixture carries as many
/// near misses as violations.
#[test]
fn the_api_reader_reproduces_its_own_fixture() {
    let api = public_api(safe_facing_fixture());
    assert_eq!(
        api.findings.len(),
        SAFE_FACING_FIXTURE_FINDINGS,
        "the reader must find exactly its fixture's violations, got {:?}",
        api.findings
    );
    assert_eq!(
        api.pub_tokens, SAFE_FACING_FIXTURE_PUB_TOKENS,
        "the fixture itself must not drift: a shrinking fixture makes the claim \
         control weaker without failing anything"
    );
    assert_eq!(
        api.pub_tokens_claimed, api.pub_tokens,
        "every `pub` in live Rust is a visibility; an unclaimed one is source the \
         reader walked past"
    );
    assert!(api.parse_failures.is_empty(), "{:?}", api.parse_failures);
    assert!(
        api.findings.iter().any(|f| f.unsafe_fn),
        "the fixture must exercise both halves of the rule"
    );
    assert!(api.findings.iter().any(|f| f.raw_pointer));
    // And the control is wired into the run, not merely available to a test.
    let clean = check_workspace(&clean_workspace("api-licensed")).0;
    assert_eq!(
        clean.safe_facing_self_test_findings,
        SAFE_FACING_FIXTURE_FINDINGS
    );
}

/// A `pub` the reader did not consume is a region it walked past. A macro body
/// is exactly that: it is not expanded, so the tokens inside it are unaccounted
/// for and the file's "no findings" verdict is quantified over source nobody
/// parsed. That must fail, not pass quietly.
#[test]
fn an_unaccounted_pub_token_fails_rather_than_reporting_no_findings() {
    let found = island_codes(
        "api-macro",
        "macro_rules! widen {\n    () => { pub unsafe fn hidden() {} };\n}\n",
    );
    assert!(
        found.contains(&"island_public_api_unparsed".to_owned()),
        "a pub token the reader never claimed must fail the run, got {found:?}"
    );
}

#[test]
fn an_island_with_no_source_fails_rather_than_being_reported_clean() {
    let root = workspace_with_island_source("api-nosrc", CLEAN_ISLAND);
    fs::remove_file(root.join("crates/fgdb-unsafe-simd/src/lib.rs")).unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"island_api_unscannable".to_owned()),
        "a rostered island whose source cannot be found must not be reported clean, \
         got {found:?}"
    );
}

#[test]
fn an_unreadable_island_source_fails_rather_than_being_skipped() {
    let root = workspace_with_island_source("api-unreadable", CLEAN_ISLAND);
    // A directory where a source file is expected.
    fs::create_dir_all(root.join("crates/fgdb-unsafe-simd/src/kernel.rs")).unwrap();
    let found = codes(&root);
    assert!(
        found.contains(&"source_unreadable".to_owned())
            || found.contains(&"source_tree_unreadable".to_owned()),
        "an island source the checker cannot read must fail the run, got {found:?}"
    );
}

/// Zero crates scanned is the outermost vacuity case, and the safe-facing rule
/// inherits it: with no members there is no island, and "no island exports an
/// unsafe fn" is then true of nothing.
#[test]
fn an_empty_roster_fails_before_any_safe_facing_conclusion() {
    let root = clean_workspace("api-empty-roster");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = []\n\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
    )
    .unwrap();
    let (report, violations) = check_workspace(&root);
    let found: Vec<String> = violations.into_iter().map(|v| v.code).collect();
    assert!(
        found.contains(&"workspace_has_no_members".to_owned()),
        "got {found:?}"
    );
    assert_eq!(report.islands_api_scanned, 0);
}

// ---------------------------------------------------------------------------
// The ten laws of `check_workspace` that had never been watched fire
// (`fgdb-validator-laws-never-witnessed-firing-xnxy.1`).
//
// The suite above proves the checker's headline verdicts. These ten are the
// failure modes BELOW those verdicts — the ones that decide whether a run
// examined anything at all — and until now none of them had been seen to fire
// from any input. Eight can be reached by a one-fact mutation of a workspace
// this file already builds; each pairs the clean control with that mutation and
// requires the exact code.
//
// The remaining two are `site_scanner_self_test_failed` and
// `safe_facing_self_test_failed`, and they are a different kind of thing: their
// predicate compares a reader's answer on a COMPILED-IN fixture against a
// compiled-in constant, so no workspace, ledger or source tree can move it.
// They are unreachable from any input by construction — which is exactly right
// for a control, and exactly why an input-driven witness for them cannot exist.
// What is witnessable is that the guard is a live predicate rather than a
// constant, and the last two tests do that: same public reader, perturbed
// fixture, answer moves.
// ---------------------------------------------------------------------------

/// Build the clean control, assert it really is clean, seed one defect, and
/// require the exact code. The control is asserted per test rather than once
/// for the file: a fixture builder that silently started producing a violating
/// workspace would otherwise make every mutation below pass for the wrong
/// reason.
fn assert_seeded_defect_fires(tag: &str, code: &str, seed: impl Fn(&Path)) {
    let root = clean_workspace(tag);
    let control = check_workspace(&root).1;
    assert!(
        control.is_empty(),
        "the control must pass, or the mutation below proves nothing: {control:?}"
    );
    seed(&root);
    let found = codes(&root);
    assert!(
        found.contains(&code.to_owned()),
        "expected {code:?} after seeding it, got {found:?}"
    );
}

#[test]
fn a_manifest_without_a_workspace_section_fails() {
    assert_seeded_defect_fires("no-ws-section", "workspace_section_absent", |root| {
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"solo\"\n").unwrap();
    });
}

#[test]
fn an_unparseable_workspace_manifest_fails_rather_than_being_skipped() {
    assert_seeded_defect_fires("ws-unparseable", "workspace_manifest_unparseable", |root| {
        fs::write(root.join("Cargo.toml"), "[workspace\nmembers = ]\n").unwrap();
    });
}

#[test]
fn a_member_roster_that_cannot_be_resolved_fails() {
    // A roster written as a bare string rather than an array. The resolver
    // returns an error instead of an empty roster on purpose: an empty roster
    // is a clean pass over nothing, which is the vacuity this checker exists to
    // refuse.
    assert_seeded_defect_fires(
        "members-unresolvable",
        "workspace_members_unresolvable",
        |root| {
            fs::write(
                root.join("Cargo.toml"),
                "[workspace]\nresolver = \"3\"\nmembers = \"crates/fgdb-ordinary\"\n\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
            )
            .unwrap();
        },
    );
}

#[test]
fn a_member_whose_manifest_cannot_be_read_fails() {
    assert_seeded_defect_fires("member-unreadable", "member_manifest_unreadable", |root| {
        fs::remove_file(root.join("crates/fgdb-ordinary/Cargo.toml")).unwrap();
    });
}

#[test]
fn an_unknown_ledger_schema_version_fails() {
    assert_seeded_defect_fires("schema-unknown", "ledger_schema_version_unknown", |root| {
        fs::write(
            root.join("registries/unsafe_boundary_ledger.toml"),
            LEDGER_HEAD.replace("schema_version = 1", "schema_version = 2"),
        )
        .unwrap();
    });
}

#[test]
fn an_island_with_an_unknown_status_fails() {
    assert_seeded_defect_fires("status-unknown", "island_status_unknown", |root| {
        fs::write(
            root.join("registries/unsafe_boundary_ledger.toml"),
            LEDGER_HEAD.replace("status = \"planned\"", "status = \"maybe\""),
        )
        .unwrap();
    });
}

#[test]
fn an_island_without_a_charter_fails() {
    assert_seeded_defect_fires("charter-empty", "island_charter_empty", |root| {
        fs::write(
            root.join("registries/unsafe_boundary_ledger.toml"),
            LEDGER_HEAD.replace(
                "charter = \"SIMD kernels with bit-identical scalar fallbacks.\"",
                "charter = \"   \"",
            ),
        )
        .unwrap();
    });
}

#[test]
fn a_duplicated_ledger_row_id_fails() {
    assert_seeded_defect_fires("row-id-dup", "ledger_row_id_duplicated", |root| {
        let site = "[[site]]\nrow_id = \"dup-1\"\nisland = \"fgdb-unsafe-simd\"\n\
                    path = \"p.rs\"\nsymbol = \"unsafe fn x() {}\"\n\
                    stated_invariant = \"i\"\nevidence = \"e\"\nfallback = \"f\"\n\
                    no_claim_boundary = \"Miri ASAN TSAN\"\n\
                    tool_no_claim_boundaries = [\"miri|excluded|m\", \"asan|candidate|a\", \"tsan|candidate|t\"]\n";
        fs::write(
            root.join("registries/unsafe_boundary_ledger.toml"),
            format!("{LEDGER_HEAD}\n{site}\n{site}"),
        )
        .unwrap();
    });
}

/// `site_scanner_self_test_failed` cannot be reached from any input, so what is
/// proved here is the next best thing and the only thing that matters: the
/// guard is a LIVE predicate. If the scanner stopped reading, its answer on the
/// fixture would stop matching `SCANNER_FIXTURE_SITES` — which is what the
/// production guard compares. A scanner that had become constant would pass
/// that comparison forever, and would fail this test.
#[test]
fn the_site_scanner_self_test_guard_is_a_live_predicate() {
    let fixture = scanner_fixture();
    assert_eq!(
        scan_sites("<fixture>", &fixture).len(),
        SCANNER_FIXTURE_SITES,
        "the shipped scanner must reproduce its own fixture"
    );
    // Remove what the scanner is looking for and the count must move. It is the
    // SAME public reader the guard calls, on text that differs in exactly the
    // fact the reader exists to find.
    let blinded = fixture.replace("allow(unsafe_code)", "allow(dead_code)");
    assert_ne!(
        scan_sites("<fixture>", &blinded).len(),
        SCANNER_FIXTURE_SITES,
        "the scanner's answer must depend on its input, or its self-test licenses nothing"
    );
}

/// The same argument for the safe-facing reader's control. Both halves of the
/// guard are exercised: the finding count AND the `pub` token count, because a
/// reader that found nothing at all would satisfy a count-only comparison.
#[test]
fn the_safe_facing_self_test_guard_is_a_live_predicate() {
    let fixture = safe_facing_fixture();
    let control = public_api(fixture);
    assert_eq!(control.findings.len(), SAFE_FACING_FIXTURE_FINDINGS);
    assert_eq!(control.pub_tokens, SAFE_FACING_FIXTURE_PUB_TOKENS);

    let blinded = fixture.replace("pub ", "pub(crate) ");
    let mutated = public_api(&blinded);
    assert!(
        mutated.findings.len() != SAFE_FACING_FIXTURE_FINDINGS
            || mutated.pub_tokens != SAFE_FACING_FIXTURE_PUB_TOKENS,
        "the safe-facing reader's answer must depend on its input, or its self-test licenses nothing"
    );
}
