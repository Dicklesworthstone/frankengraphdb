//! Mutation suite for the named-law registry (fgdb-law-citation-sweep-uzzh).
//!
//! Every validation rule in `registry_check::laws` gets a test that takes the
//! real registry, breaks exactly one thing, and asserts the exact violation
//! code. A checker nobody has watched go red is a checker nobody knows works.
//!
//! The load-bearing test is `every_registered_anchor_resolves_in_the_plan`.
//! The whole point of `source_location` is that a law becomes falsifiable — a
//! reader opens the cited line and checks — so the suite checks it mechanically
//! rather than trusting the row. Without that test the registry would be a
//! second place to write unverifiable prose, which is the defect it exists to
//! end.

use registry_check::laws::{Law, LawRegistry, load_from_repo, validate_laws};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn registry() -> LawRegistry {
    load_from_repo(&repo_root()).expect("laws.toml loads")
}

fn codes(registry: &LawRegistry) -> Vec<String> {
    validate_laws(registry)
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

/// The control. Every mutation test below is meaningless if the unmutated
/// registry is not clean, so this failing first tells you the suite is lying.
#[test]
fn real_registry_is_clean() {
    let violations = validate_laws(&registry());
    assert!(
        violations.is_empty(),
        "the shipped law registry must validate clean, found: {violations:?}"
    );
}

#[test]
fn registry_declares_at_least_one_registered_law() {
    let registry = registry();
    assert!(
        registry.laws.iter().any(|law| law.status == "registered"),
        "a registry with no registered law licenses no citation at all"
    );
}

/// The reason `source_location` exists: a registered law must point at a plan
/// line a reader can open. This resolves every anchor against the real plan.
#[test]
fn every_registered_anchor_resolves_in_the_plan() {
    let plan = std::fs::read_to_string(
        repo_root().join("COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"),
    )
    .expect("plan is readable");
    let lines: Vec<&str> = plan.lines().collect();
    let registry = registry();
    let mut checked = 0usize;
    for law in registry.laws.iter().filter(|l| l.status == "registered") {
        let anchor = law.source_location.split_once(':');
        assert!(anchor.is_some(), "{} has no aNN:LINE anchor", law.id);
        let (_slice, digits) = anchor.expect("anchor presence asserted above");
        assert!(
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()),
            "{} anchor line {digits:?} is not a number",
            law.id
        );
        let line_no: usize = digits.parse().expect("digits asserted above");
        assert!(
            line_no >= 1 && line_no <= lines.len(),
            "{} cites line {line_no}, past the end of the plan ({} lines)",
            law.id,
            lines.len()
        );
        let text = lines[line_no - 1];
        assert!(
            !text.trim().is_empty(),
            "{} cites plan line {line_no}, which is blank",
            law.id
        );
        checked += 1;
    }
    assert!(checked > 0, "no registered anchors were checked");
}

fn mutate(f: impl FnOnce(&mut Law)) -> LawRegistry {
    let mut registry = registry();
    f(&mut registry.laws[0]);
    registry
}

#[test]
fn malformed_id_is_rejected() {
    let r = mutate(|law| law.id = "LAW-1".into());
    assert!(
        codes(&r).contains(&"law_id_malformed".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_id_is_rejected() {
    let mut r = registry();
    let dup = r.laws[0].id.clone();
    r.laws[1].id = dup;
    assert!(
        codes(&r).contains(&"law_id_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_name_is_rejected() {
    let mut r = registry();
    let dup = r.laws[0].name.clone();
    r.laws[1].name = dup;
    assert!(
        codes(&r).contains(&"law_name_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_name_is_rejected() {
    let r = mutate(|law| law.name = "   ".into());
    assert!(
        codes(&r).contains(&"law_name_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn unknown_status_is_rejected() {
    let r = mutate(|law| law.status = "probably-fine".into());
    assert!(
        codes(&r).contains(&"law_status_unknown".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn registered_law_without_statement_is_rejected() {
    let r = mutate(|law| {
        law.status = "registered".into();
        law.statement = String::new();
    });
    assert!(
        codes(&r).contains(&"law_statement_missing".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn registered_law_without_enforcement_is_rejected() {
    let r = mutate(|law| {
        law.status = "registered".into();
        law.enforcement = String::new();
    });
    assert!(
        codes(&r).contains(&"law_enforcement_missing".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// The discriminator. A registered law with no anchor is exactly the shape
/// every fabricated citation in the catalog had.
#[test]
fn registered_law_without_anchor_is_rejected() {
    for bad in ["", "somewhere in the plan", "a01", "1412", "A01:1412"] {
        let r = mutate(|law| {
            law.status = "registered".into();
            law.source_location = bad.into();
        });
        assert!(
            codes(&r).contains(&"law_source_anchor_missing".to_string()),
            "anchor {bad:?} should have been rejected, got {:?}",
            codes(&r)
        );
    }
}

#[test]
fn a_real_anchor_is_accepted() {
    let r = mutate(|law| {
        law.status = "registered".into();
        law.source_location = "a01:1412".into();
    });
    assert!(
        !codes(&r).contains(&"law_source_anchor_missing".to_string()),
        "a well-formed anchor must not be rejected: {:?}",
        codes(&r)
    );
}

#[test]
fn unregistered_law_without_a_note_is_rejected() {
    let r = mutate(|law| {
        law.status = "fabrication-candidate".into();
        law.note = String::new();
    });
    assert!(
        codes(&r).contains(&"law_adjudication_note_missing".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_registry_is_rejected() {
    let r = LawRegistry { laws: Vec::new() };
    assert!(
        codes(&r).contains(&"law_registry_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Fail-closed reader: a key this reader does not understand is a row it has
/// not understood. Silently ignoring it is how a field gets added to some rows
/// and dropped on others.
#[test]
fn unknown_key_is_rejected_at_load() {
    let dir = std::env::temp_dir().join(format!("fgdb-laws-unknown-key-{}", std::process::id()));
    let registries = dir.join("registries");
    std::fs::create_dir_all(&registries).expect("scratch dir");
    let path = registries.join("laws.toml");
    std::fs::write(
        &path,
        "[[law]]\nid = \"FG-LAW-01\"\nname = \"x\"\nstatus = \"unadjudicated\"\nnote = \"n\"\nsurprise = \"y\"\n",
    )
    .expect("write fixture");
    let error = load_from_repo(&dir).expect_err("an unknown key must fail the load");
    assert!(
        error.message.contains("unknown key"),
        "expected an unknown-key load error, got {error}"
    );
}
