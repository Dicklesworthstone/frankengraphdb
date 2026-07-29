//! Mutation suite for the command-contract registry (fgdb-5uw2).
//!
//! Every validation rule in `registry_check::command_contracts` gets a test
//! that takes a well-formed row, breaks exactly one thing, and asserts the
//! exact violation code. The shipped registry is deliberately EMPTY (plan line
//! 296's bijection quantifies over live rows and inhabitable arms, and both
//! domains are measured empty at creation — see the registry header), so the
//! single-defect mutations run against a synthetic row whose own clean
//! baseline is asserted first: a fixture control only proves the reader works
//! on the fixture, so the baseline assert is what licenses the mutations.

use registry_check::command_contracts::{
    Contract, ContractRegistry, load_contracts, load_from_repo, validate_contracts,
};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn registry() -> ContractRegistry {
    load_from_repo(&repo_root()).expect("command_contracts.toml loads")
}

fn codes(registry: &ContractRegistry) -> Vec<String> {
    validate_contracts(registry)
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

/// A fully well-formed reserved row. Every mutation below starts from this and
/// breaks exactly one thing.
fn synthetic_row() -> Contract {
    Contract {
        command_contract_id: "cc-local-branch-retire-v1".into(),
        role: "Local".into(),
        outer_command_union: "SequenceNeutralSpec".into(),
        outer_wire_tag: 0x0001,
        input_schema_id: "BranchRetireSpec".into(),
        input_wire_tag: 0x0001,
        inner_wire_tag: None,
        body_schema_id: "BranchRetireSpecBody".into(),
        result_schema_id: "BranchRetireResult".into(),
        applied_record_schema_id: "BranchRetireTombstone".into(),
        handler_symbol: "fgdb_chronicle::apply_branch_retire".into(),
        transition_class: "Semantic".into(),
        sequence_effects: "advances LogicalCommandSeq and HLC, never CommitSeq".into(),
        expected_state_schema_id: "ExpectedStateCondition".into(),
        authority_arm: "BranchAuthority".into(),
        authority_evidence_target_schema_id: None,
        terminal_audit_freeze_arm: "LocalControl".into(),
        terminal_audit_gate_arm: "Required".into(),
        payload_availability_rule: "None".into(),
        publication_mode: "SinglePlane".into(),
        construction_dag_recipe_id: "cdr-branch-retire-v1".into(),
        consumed_state_slots: vec!["SemanticPayload|Local|branch_registry".into()],
        written_state_slots: vec!["SemanticPayload|Local|branch_registry".into()],
        checkpoint_floor_classes: vec!["branch-registry".into()],
        backup_restore_gc_classes: vec!["branch-registry".into()],
        posture_feature_predicate: "role-local".into(),
        format_epoch_range: "1..".into(),
        status: "reserved".into(),
    }
}

fn with_row(f: impl FnOnce(&mut Contract)) -> ContractRegistry {
    let mut row = synthetic_row();
    f(&mut row);
    ContractRegistry {
        registry_epoch: 1,
        contracts: vec![row],
    }
}

/// The control. Every mutation test below is meaningless if the shipped
/// registry is not clean.
#[test]
fn real_registry_is_clean() {
    let violations = validate_contracts(&registry());
    assert!(
        violations.is_empty(),
        "the shipped contract registry must validate clean, found: {violations:?}"
    );
}

/// Phase A pin, intentional: the registry ships EMPTY because the full family
/// expansion (the tag authority) has not been derived. fgdb-5uw2 Phase B must
/// move this pin in the same commit that lands the first rows — that is the
/// point of pinning it.
#[test]
fn phase_a_registry_is_deliberately_empty() {
    assert_eq!(
        registry().contracts.len(),
        0,
        "rows landed: update this pin alongside the fgdb-5uw2 Phase B tag-expansion derivation"
    );
}

/// The synthetic baseline must itself be clean, or every mutation below tests
/// the fixture rather than the law.
#[test]
fn synthetic_baseline_is_clean() {
    let r = with_row(|_| {});
    let violations = validate_contracts(&r);
    assert!(
        violations.is_empty(),
        "the synthetic baseline row must validate clean, found: {violations:?}"
    );
}

#[test]
fn epoch_below_one_is_rejected() {
    let r = ContractRegistry {
        registry_epoch: 0,
        contracts: Vec::new(),
    };
    assert!(
        codes(&r).contains(&"contract_registry_epoch_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_id_is_rejected() {
    let r = with_row(|c| c.command_contract_id = "  ".into());
    assert!(
        codes(&r).contains(&"contract_id_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_id_is_rejected() {
    let mut r = with_row(|_| {});
    let mut second = synthetic_row();
    second.outer_wire_tag = 0x0002;
    r.contracts.push(second);
    assert!(
        codes(&r).contains(&"contract_id_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_role_is_rejected() {
    let r = with_row(|c| c.role = "Global".into());
    assert!(
        codes(&r).contains(&"contract_role_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_transition_class_is_rejected() {
    let r = with_row(|c| c.transition_class = "Housekeeping".into());
    assert!(
        codes(&r).contains(&"contract_transition_class_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_status_is_rejected() {
    let r = with_row(|c| c.status = "active".into());
    assert!(
        codes(&r).contains(&"contract_status_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn zero_wire_tag_is_rejected() {
    let r = with_row(|c| c.outer_wire_tag = 0x0000);
    assert!(
        codes(&r).contains(&"contract_wire_tag_out_of_space".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn sentinel_wire_tag_is_rejected() {
    let r = with_row(|c| c.input_wire_tag = 0xffff);
    assert!(
        codes(&r).contains(&"contract_wire_tag_out_of_space".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn optional_inner_tag_is_bounded_too() {
    let r = with_row(|c| c.inner_wire_tag = Some(0xffff));
    assert!(
        codes(&r).contains(&"contract_wire_tag_out_of_space".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_arm_slot_is_rejected() {
    let mut r = with_row(|_| {});
    let mut second = synthetic_row();
    second.command_contract_id = "cc-local-branch-retire-v2".into();
    // Same (role, outer_command_union, outer_wire_tag): a second command
    // encoded under one tag.
    r.contracts.push(second);
    assert!(
        codes(&r).contains(&"contract_arm_slot_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn malformed_state_slot_is_rejected() {
    let r = with_row(|c| c.written_state_slots = vec!["branch_registry".into()]);
    assert!(
        codes(&r).contains(&"contract_state_slot_malformed".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn unregistered_slot_plane_is_rejected() {
    let r = with_row(|c| c.consumed_state_slots = vec!["Payload|Local|branch_registry".into()]);
    assert!(
        codes(&r).contains(&"contract_state_slot_malformed".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// The plan-line-296 bijection quantifies over live rows; the enumerators that
/// discharge a live row's obligations do not exist yet, so `live` is refused
/// rather than silently accepted (green-over-unchecked is the failure class
/// this registry exists to end).
#[test]
fn live_row_is_refused_until_bijection_enumerators_exist() {
    let r = with_row(|c| c.status = "live".into());
    assert!(
        codes(&r).contains(&"contract_live_row_unverifiable".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Parse-level closed-key law: a row carrying an unknown key is a load error,
/// not a violation. Uses a process-unique temp path so a concurrent pane
/// cannot race this fixture.
#[test]
fn unknown_key_fails_the_load() {
    let dir = std::env::temp_dir().join(format!(
        "fgdb-command-contracts-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("registries").join("command_contracts.toml");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("temp registries dir");
    std::fs::write(
        &path,
        "registry_epoch = 1\n\n[[contract]]\nwildcard_body = \"*\"\n",
    )
    .expect("fixture written");
    let error = load_contracts(&path).expect_err("unknown key must fail the load");
    assert!(
        error.message.contains("unknown key"),
        "load error must name the closed-key law, got: {error}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
