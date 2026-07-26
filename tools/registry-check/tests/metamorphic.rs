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
//! These relations cover the workspace-manifest readers. The same relations
//! applied to the other readers in this crate are currently RED and are filed
//! rather than encoded here, so that this suite stays a gate rather than a
//! known-failing list: `fgdb-regcheck-commented-arm-counts-live-ctv8`,
//! `fgdb-regcheck-scansites-line-anchored-ds45`,
//! `fgdb-regcheck-two-readers-unsafe-relax-6amm`,
//! `fgdb-regcheck-root-forbid-line-equality-fhnr`,
//! `fgdb-regcheck-closure-vacuous-no-control-hp0f`,
//! `fgdb-regcheck-claimslint-allowlist-dead-excludes-5qcg`. As each is fixed,
//! its relation belongs here.

use registry_check::unsafe_ledger::{self, LEDGER_PATH};
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
}

impl Member {
    fn clean(dir: &str) -> Self {
        Member {
            dir: dir.to_owned(),
            unsafe_site: false,
            inherits: true,
        }
    }

    fn with_unsafe_site(dir: &str) -> Self {
        Member {
            dir: dir.to_owned(),
            unsafe_site: true,
            inherits: true,
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

/// Materialize a workspace whose root manifest is exactly `manifest`, run the
/// checker over it, and return what it concluded.
///
/// `scope` names the calling test and `tag` the variant. Both are in the
/// fixture path: `scope` so tests running in parallel cannot share a directory,
/// `tag` so each variant's tree is rebuilt identically on every run.
fn verdict(scope: &str, tag: &str, manifest: &str, members: &[Member]) -> Verdict {
    let root = std::env::temp_dir().join(format!("fgdb-metamorphic-{scope}-{tag}"));
    // Rebuild from clean: a leftover member directory from an earlier run would
    // silently change what a glob roster resolves to.
    if root.is_dir() {
        fs::remove_dir_all(&root).expect("clear fixture root");
    }
    fs::create_dir_all(root.join("registries")).expect("registries dir");
    fs::write(root.join("Cargo.toml"), manifest).expect("workspace manifest");
    fs::write(root.join(LEDGER_PATH), FIXTURE_LEDGER).expect("ledger");

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
        let source = if member.unsafe_site {
            format!("{hash}[allow(unsafe_code)]\npub unsafe fn probe() {{}}\n")
        } else {
            "pub fn probe() {}\n".to_owned()
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
fn topology_flags(tag: &str, root_attr: &str, source: &str) -> (bool, bool, bool) {
    let root = std::env::temp_dir().join(format!("fgdb-metamorphic-topo-{tag}"));
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
        let (topology_says, _, _) = topology_flags(tag, "", &source);
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
        let (_, forbids, _) = topology_flags(tag, &attr, "pub fn f() {}\n");
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
        let (_, forbids, _) = topology_flags(tag, &attr, "pub fn f() {}\n");
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
        "deny_grouped",
        &format!("{hash}![deny(unsafe_code, unused)] // island root\n"),
        "pub fn f() {}\n",
    );
    assert!(denies, "a grouped, comment-trailed deny is still a deny");
    assert!(!forbids, "a deny is not a forbid");
}
