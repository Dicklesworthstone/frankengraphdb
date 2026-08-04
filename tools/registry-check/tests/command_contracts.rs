//! Mutation suite for the command-contract registry (fgdb-5uw2).
//!
//! Every validation rule in `registry_check::command_contracts` gets a test
//! that takes a well-formed row, breaks exactly one thing, and asserts the
//! exact violation code. The registry shipped deliberately empty until the
//! owner-confirmed v1 tag freeze opened Phase B; it now carries the F1/F2
//! seed rows (all `reserved` — see the registry header). The single-defect
//! mutations still run against a synthetic row whose own clean baseline is
//! asserted first: a fixture control only proves the reader works on the
//! fixture, so the baseline assert is what licenses the mutations.

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

/// Phase B floor, replacing the Phase A deliberate-empty pin in the same
/// commit that landed the first rows (the point of pinning it): the landed
/// tranche rows of the owner-confirmed v1 freeze, by id. The population may
/// only grow from here — a missing row is either an illegal deletion (released
/// tags are permanent, plan line 290) or a gutted file, and both deserve a red.
#[test]
fn phase_b_seed_rows_are_present() {
    let registry = registry();
    for id in [
        // F1/F2 (87cf892)
        "cc:local:recovery-bridge-spec",
        "cc:local:local-begin-reservation-spec",
        "cc:local:local-begin-terminal-spec",
        // F3 attempt-lifecycle (frozen ordinals 0x0004-0x0011)
        "cc:local:local-attempt-registration-spec:explicit-begin",
        "cc:local:local-attempt-registration-spec:autocommit",
        "cc:local:local-autocommit-write-spec",
        "cc:local:txn-ownership-transition-spec:reattach",
        "cc:local:txn-ownership-transition-spec:renew",
        "cc:local:txn-ownership-expiry-abort-spec",
        "cc:local:local-statement-registration-spec",
        "cc:local:local-statement-publication-spec",
        "cc:local:local-statement-abort-spec",
        "cc:local:local-prepare-admission-spec",
        "cc:local:local-read-close-spec",
        "cc:local:local-terminal-completion-spec",
        "cc:local:txn-abort-spec",
        "cc:local:local-outcome-compaction-spec:never-registered",
        "cc:local:local-outcome-compaction-spec:terminal-ready",
        "cc:local:local-outcome-expiry-spec",
        "cc:local:local-conflict-compaction-spec",
        // F4 allocation-escape-and-finalization (frozen ordinals 0x0012-0x0015)
        "cc:local:allocation-reservation-transition-spec:escaping-reserve",
        "cc:local:allocation-reservation-transition-spec:binding-compaction",
        "cc:local:local-final-certification-reserve-spec",
        "cc:local:local-final-certification-cancel-spec",
        "cc:local:finalization-allocation-disposition-spec:consumed",
        "cc:local:finalization-allocation-disposition-spec:abandoned-spent",
    ] {
        assert!(
            registry
                .contracts
                .iter()
                .any(|row| row.command_contract_id == id && row.status == "reserved"),
            "confirmed seed row {id:?} is missing"
        );
    }
    assert!(
        registry.contracts.len() >= 26,
        "the population may only grow from the landed F1-F4 rows"
    );
}

/// An armed member's rows share the member's outer tag and differ only by
/// inner_wire_tag: the arm-slot law keys on (role, union, outer, inner), so
/// this shape validates clean while a same-inner duplicate or an
/// armless/armed mix on one outer tag reds contract_arm_slot_duplicate.
/// Assert the landed shape explicitly: shared outer tag, distinct inner tags.
#[test]
fn armed_member_rows_share_outer_tag_with_distinct_inner_tags() {
    let registry = registry();
    for (root, arms) in [
        ("cc:local:local-attempt-registration-spec", 2usize),
        ("cc:local:txn-ownership-transition-spec", 2),
        ("cc:local:local-outcome-compaction-spec", 2),
        ("cc:local:allocation-reservation-transition-spec", 2),
        ("cc:local:finalization-allocation-disposition-spec", 2),
    ] {
        let family: Vec<_> = registry
            .contracts
            .iter()
            .filter(|row| row.command_contract_id.starts_with(&format!("{root}:")))
            .collect();
        assert_eq!(
            family.len(),
            arms,
            "family {root:?} has the wrong arm count"
        );
        let outer: std::collections::BTreeSet<i64> =
            family.iter().map(|row| row.outer_wire_tag).collect();
        assert_eq!(outer.len(), 1, "family {root:?} must share one outer tag");
        let inner: std::collections::BTreeSet<Option<i64>> =
            family.iter().map(|row| row.inner_wire_tag).collect();
        assert_eq!(
            inner.len(),
            arms,
            "family {root:?} must carry distinct inner tags"
        );
        assert!(
            !inner.contains(&None),
            "an armed row must carry an inner_wire_tag"
        );
    }
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
    // Same (role, outer_command_union, outer_wire_tag), both armless: a
    // second command encoded under one tag.
    r.contracts.push(second);
    assert!(
        codes(&r).contains(&"contract_arm_slot_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Two arm rows may share the member's outer tag ONLY through distinct inner
/// tags; the same (outer, inner) pair is a second command under one tag.
#[test]
fn same_outer_and_same_inner_tag_is_rejected() {
    let mut r = with_row(|c| c.inner_wire_tag = Some(0x0001));
    let mut second = synthetic_row();
    second.command_contract_id = "cc-local-branch-retire-v2".into();
    second.inner_wire_tag = Some(0x0001);
    r.contracts.push(second);
    assert!(
        codes(&r).contains(&"contract_arm_slot_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Distinct inner tags under one outer tag are the SPECIFIED armed-family
/// shape (plan line 294) and must validate clean — the differential control
/// for the two rejections beside it.
#[test]
fn distinct_inner_tags_under_one_outer_tag_are_clean() {
    let mut r = with_row(|c| c.inner_wire_tag = Some(0x0001));
    let mut second = synthetic_row();
    second.command_contract_id = "cc-local-branch-retire-v2".into();
    second.inner_wire_tag = Some(0x0002);
    r.contracts.push(second);
    let violations = validate_contracts(&r);
    assert!(violations.is_empty(), "{violations:?}");
}

/// One outer tag is one armless command or one armed family, never both: an
/// armless row and an armed row sharing the outer tag hide an open subcommand.
#[test]
fn armless_and_armed_rows_on_one_outer_tag_are_rejected() {
    let mut r = with_row(|_| {});
    let mut second = synthetic_row();
    second.command_contract_id = "cc-local-branch-retire-v2".into();
    second.inner_wire_tag = Some(0x0001);
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
