//! Cross-validator mutation witnesses for high-blast-radius laws.
//!
//! A string census can say that a violation code exists, but only a paired
//! control and mutation show that the production validator can emit it.  Each
//! test below first proves that its unmodified owner is accepted, changes one
//! load-bearing fact, and then requires the exact violation code.  Keeping the
//! control and mutation in the same harness prevents a malformed fixture from
//! masquerading as a witnessed law.

use registry_check::{appendix_a, threat, topology, unsafe_ledger};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root resolves")
}

fn appendix_catalog() -> appendix_a::Catalog {
    let text = std::fs::read_to_string(repo_root().join(appendix_a::CATALOG_PATH))
        .expect("Appendix A catalog is readable");
    appendix_a::parse_catalog(&text).expect("unmodified Appendix A catalog is valid")
}

fn assert_code(codes: &[String], expected: &str) {
    assert!(
        codes.iter().any(|code| code == expected),
        "expected violation {expected:?}, got {codes:?}"
    );
}

#[test]
fn missing_appendix_projection_is_seen_to_fire() {
    let catalog = appendix_catalog();
    let control = appendix_a::verify_projections(&repo_root(), &catalog);
    assert!(
        control.is_empty(),
        "unmodified projection control must pass: {control:?}"
    );

    let absent_root = std::env::temp_dir().join(format!(
        "fgdb-xnxy-uncreated-projection-root-{}",
        std::process::id()
    ));
    assert!(
        !absent_root.exists(),
        "mutation root must be absent so the read failure is deliberate"
    );
    let codes: Vec<String> = appendix_a::verify_projections(&absent_root, &catalog)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "projection_read");
}

#[test]
fn appendix_target_identity_mismatch_is_seen_to_fire() {
    let mut catalog = appendix_catalog();
    let control = appendix_a::validate_catalog(&catalog);
    assert!(
        control.is_empty(),
        "unmodified catalog control must pass: {control:?}"
    );

    catalog
        .targets
        .first_mut()
        .expect("Appendix catalog has targets")
        .slice_id
        .push_str("-wrong");
    let codes: Vec<String> = appendix_a::validate_catalog(&catalog)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "catalog_target_identity_mismatch");
}

#[test]
fn unexpected_appendix_projection_class_is_seen_to_fire() {
    let mut catalog = appendix_catalog();
    let control = appendix_a::validate_catalog(&catalog);
    assert!(
        control.is_empty(),
        "unmodified catalog control must pass: {control:?}"
    );

    let row = catalog
        .projection_rows
        .iter_mut()
        .find(|row| row.slice_id == "a02")
        .expect("A02 has projection rows");
    row.projection = "logical_object_kinds".to_owned();
    let codes: Vec<String> = appendix_a::validate_catalog(&catalog)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "catalog_projection_unexpected");
}

#[test]
fn workspace_unsafe_lint_drift_is_seen_to_fire() {
    let root = repo_root();
    let mut registry = topology::load_from_repo(&root).expect("unmodified topology registry loads");
    let control = topology::validate_topology(&registry, &root);
    assert!(
        control.is_empty(),
        "unmodified topology control must pass: {control:?}"
    );

    registry.registry.workspace_unsafe_lint = "deny".to_owned();
    let codes: Vec<String> = topology::validate_topology(&registry, &root)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "workspace_unsafe_lint_drift");
}

#[test]
fn security_identity_wire_tag_collision_is_seen_to_fire() {
    let root = repo_root();
    let mut registry = threat::load_from_repo(&root).expect("unmodified threat registry loads");
    let control = threat::validate_threat(&registry, &root);
    assert!(
        control.is_empty(),
        "unmodified threat-model control must pass: {control:?}"
    );

    let duplicate_tag = registry
        .identities
        .first()
        .expect("threat model has identities")
        .wire_tag
        .clone();
    registry
        .identities
        .get_mut(1)
        .expect("threat model has a second identity")
        .wire_tag = duplicate_tag;
    let codes: Vec<String> = threat::validate_threat(&registry, &root)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "identity_wire_tag_collision");
}

#[test]
fn unreadable_workspace_manifest_is_seen_to_fire() {
    let root = repo_root();
    let (_, control) = unsafe_ledger::check_workspace(&root);
    assert!(
        control.is_empty(),
        "unmodified unsafe-ledger control must pass: {control:?}"
    );

    let absent_root = std::env::temp_dir().join(format!(
        "fgdb-xnxy-uncreated-workspace-root-{}",
        std::process::id()
    ));
    assert!(
        !absent_root.exists(),
        "mutation root must be absent so the manifest read failure is deliberate"
    );
    let codes: Vec<String> = unsafe_ledger::check_workspace(&absent_root)
        .1
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "workspace_manifest_unreadable");
}
