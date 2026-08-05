//! Mutation suite for the input-looking type classification registry
//! (fgdb-5uw2).
//!
//! Every validation rule in `registry_check::command_type_classification` gets
//! a test that takes the real registry, breaks exactly one thing, and asserts
//! the exact violation code. The load-bearing test is
//! `every_anchor_names_its_type_at_the_cited_line`: a classification row's
//! whole claim is that its class is FORCED at the cited plan line, so the
//! suite opens the line and requires the classified type to be named there —
//! without that, the registry would be a place to write choices wearing
//! citations, the defect §5.1 pairs this registry against.

use registry_check::command_contracts::{ContractRegistry, load_from_repo as load_contracts};
use registry_check::command_type_classification::{
    ClassificationRegistry, load_from_repo, validate_classifications,
};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn registry() -> ClassificationRegistry {
    load_from_repo(&repo_root()).expect("command_type_classification.toml loads")
}

fn contracts() -> ContractRegistry {
    load_contracts(&repo_root()).expect("command_contracts.toml loads")
}

fn codes(registry: &ClassificationRegistry) -> Vec<String> {
    validate_classifications(registry, &contracts())
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

/// The control. Every mutation test below is meaningless if the unmutated
/// registry is not clean.
#[test]
fn real_registry_is_clean() {
    let violations = validate_classifications(&registry(), &contracts());
    assert!(
        violations.is_empty(),
        "the shipped classification registry must validate clean, found: {violations:?}"
    );
}

/// Non-vacuity floor: the source-forced seed population, by name. A row
/// leaving this set is either Phase B progress (add, never remove) or a gutted
/// file — both deserve a red here.
#[test]
fn source_forced_seed_population_is_present() {
    let registry = registry();
    for name in [
        "TxnSubmissionSpec",
        "AuditExternalResolutionBatch",
        "AuditExternalResolutionReceipt",
        "AllocationReservationTransitionSpec",
        "LocalFinalCertificationReserveSpec",
        "LocalFinalCertificationCancelSpec",
        "FinalizationAllocationDispositionSpec",
        "CheckpointInstallSpec",
        "InitialConfigFloorInstallSpec",
        "HistoryCutActivationSpec",
        "ConfigurationTransitionSpec",
    ] {
        assert!(
            registry
                .classifications
                .iter()
                .any(|row| row.type_name == name && row.status == "registered"),
            "source-forced seed row {name:?} is missing"
        );
    }
    assert!(
        registry.classifications.len() >= 28,
        "the population may only grow from the landed F1-F4/F5a/F5b rows"
    );
}

/// The load-bearing test: open every cited plan line and require the
/// classified type to be named there. A class whose anchor does not name the
/// type is a choice, not a derivation.
#[test]
fn every_anchor_names_its_type_at_the_cited_line() {
    let plan = std::fs::read_to_string(
        repo_root().join("COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"),
    )
    .expect("plan is readable");
    let lines: Vec<&str> = plan.lines().collect();
    let registry = registry();
    let mut checked = 0usize;
    for row in &registry.classifications {
        let anchor = row.source_location.split_once(':');
        assert!(anchor.is_some(), "{} has no aNN:LINE anchor", row.type_name);
        let (_slice, digits) = anchor.expect("anchor presence asserted above");
        let parsed: Result<usize, _> = digits.parse();
        assert!(
            parsed.is_ok(),
            "{} anchor line is not a number",
            row.type_name
        );
        let line_no = parsed.expect("digits asserted above");
        assert!(
            line_no >= 1 && line_no <= lines.len(),
            "{} cites line {line_no}, past the end of the plan ({} lines)",
            row.type_name,
            lines.len()
        );
        let text = lines[line_no - 1];
        // AuditExternalResolutionReceipt is cited via the "Batch/receipt"
        // contraction at a07:1770; the family stem is the named form.
        let stem = row
            .type_name
            .strip_suffix("Receipt")
            .filter(|_| !text.contains(&row.type_name))
            .unwrap_or(&row.type_name);
        assert!(
            text.contains(stem),
            "{} cites plan line {line_no}, but that line does not name it",
            row.type_name
        );
        checked += 1;
    }
    assert!(checked > 0, "no anchors were checked");
}

fn mutate(
    f: impl FnOnce(&mut registry_check::command_type_classification::Classification),
) -> ClassificationRegistry {
    let mut registry = registry();
    f(&mut registry.classifications[0]);
    registry
}

#[test]
fn empty_registry_is_rejected() {
    let r = ClassificationRegistry {
        registry_epoch: 1,
        classifications: Vec::new(),
    };
    assert!(
        codes(&r).contains(&"classification_registry_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn epoch_below_one_is_rejected() {
    let mut r = registry();
    r.registry_epoch = 0;
    assert!(
        codes(&r).contains(&"classification_registry_epoch_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_type_name_is_rejected() {
    let r = mutate(|row| row.type_name = " ".into());
    assert!(
        codes(&r).contains(&"classification_type_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_type_is_rejected() {
    let mut r = registry();
    let dup = r.classifications[0].type_name.clone();
    r.classifications[1].type_name = dup;
    assert!(
        codes(&r).contains(&"classification_type_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_class_is_rejected() {
    let r = mutate(|row| row.class = "CommandInput".into());
    assert!(
        codes(&r).contains(&"classification_class_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_status_is_rejected() {
    let r = mutate(|row| row.status = "proposed".into());
    assert!(
        codes(&r).contains(&"classification_status_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn registered_command_input_without_contract_id_is_rejected() {
    let r = mutate(|row| row.class = "RegisteredCommandInput".into());
    assert!(
        codes(&r).contains(&"classification_contract_id_missing".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn dangling_contract_id_is_rejected() {
    let r = mutate(|row| {
        row.class = "RegisteredCommandInput".into();
        row.command_contract_id = Some("cc-does-not-exist-v1".into());
    });
    assert!(
        codes(&r).contains(&"classification_contract_unresolved".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Family resolution (round-12 T4), the differential: the armed members'
/// classification rows bind member-root ids that are NOT exact contract row
/// ids — only "{root}:{arm}" rows exist — and the registry validates clean,
/// so resolution can only have come from the family rule. If someone later
/// lands an exact member-root row, this assert flags the shape change.
#[test]
fn family_root_binding_resolves_through_arm_rows_not_an_exact_row() {
    let contracts = contracts();
    let ids: std::collections::BTreeSet<&str> = contracts
        .contracts
        .iter()
        .map(|c| c.command_contract_id.as_str())
        .collect();
    for root in [
        "cc:local:local-attempt-registration-spec",
        "cc:local:txn-ownership-transition-spec",
        "cc:local:local-outcome-compaction-spec",
        "cc:local:allocation-reservation-transition-spec",
        "cc:local:finalization-allocation-disposition-spec",
    ] {
        assert!(!ids.contains(root), "{root:?} must not be an exact row id");
        let prefix = format!("{root}:");
        assert!(
            ids.iter().any(|id| id.starts_with(prefix.as_str())),
            "{root:?} must have arm rows"
        );
    }
    let violations = validate_classifications(&registry(), &contracts);
    assert!(violations.is_empty(), "{violations:?}");
}

/// The ":"-boundary law: a bare prefix of a longer id (no arm delimiter at
/// the cut) is NOT a member root and must stay unresolved.
#[test]
fn bare_prefix_without_arm_boundary_is_rejected() {
    let r = mutate(|row| {
        row.class = "RegisteredCommandInput".into();
        // A strict prefix of "cc:local:local-attempt-registration-spec:..."
        // that does not end at a ":" boundary.
        row.command_contract_id = Some("cc:local:local-attempt".into());
    });
    assert!(
        codes(&r).contains(&"classification_contract_unresolved".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn contract_id_on_non_command_class_is_rejected() {
    let r = mutate(|row| row.command_contract_id = Some("cc-does-not-exist-v1".into()));
    assert!(
        codes(&r).contains(&"classification_contract_id_forbidden".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn malformed_anchor_is_rejected() {
    let r = mutate(|row| row.source_location = "section 5.1".into());
    assert!(
        codes(&r).contains(&"classification_source_anchor_malformed".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_statement_is_rejected() {
    let r = mutate(|row| row.statement = "".into());
    assert!(
        codes(&r).contains(&"classification_statement_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}
