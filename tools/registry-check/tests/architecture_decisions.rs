//! Executable ADR contract tests (fgdb-architecture-decision-record-xwkw).
//!
//! These tests exercise the shipped registry plus typed mutations. They do
//! not duplicate the checker: each negative changes one contract dimension
//! and asserts that the public validator rejects the resulting graph.

use registry_check::architecture::{
    self, ALLOWED_RELATIONSHIP_KINDS, ArchitectureRegistry, PINNED_BEAD_BINDING_HASH,
    PINNED_BEAD_COUNT, PINNED_BET_LABEL_COUNT, PINNED_BIBLIOGRAPHY_COUNT,
    PINNED_BIBLIOGRAPHY_ID_HASH, PINNED_DECISION_COUNT, PINNED_DECISION_ID_HASH,
    PINNED_DIRECT_OWNER_COUNT, PINNED_EXACT_OVERRIDE_COUNT, PINNED_FAMILY_RULE_COUNT,
    PINNED_SEMANTIC_CONTRACT_HASH,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

fn real_registry() -> ArchitectureRegistry {
    architecture::load_from_repo(&repo_root()).expect("architecture registry loads")
}

fn violation_codes(registry: &ArchitectureRegistry) -> BTreeSet<String> {
    architecture::validate_architecture(registry, &repo_root())
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

fn assert_code(registry: &ArchitectureRegistry, expected: &str) {
    let codes = violation_codes(registry);
    assert!(
        codes.contains(expected),
        "expected violation {expected:?}, got {codes:?}"
    );
}

#[test]
fn architecture_registry_parses_and_validates() {
    let root = repo_root();
    let text = std::fs::read_to_string(root.join("registries/architecture_decisions.toml"))
        .expect("registry text reads");
    let parsed = architecture::parse_architecture(&text).expect("registry text parses");
    assert_eq!(parsed, real_registry());

    let violations = architecture::validate_architecture(&parsed, &root);
    assert!(
        violations.is_empty(),
        "shipped architecture registry must be clean: {violations:#?}"
    );
}

#[test]
fn architecture_source_blocks_are_exact() {
    let registry = real_registry();
    let checks = architecture::check_source_blocks(&registry, &repo_root());
    assert_eq!(checks.len(), 2);
    for check in checks {
        let check = check.expect("source block can be checked");
        assert!(check.exact_match, "{} source bytes drifted", check.id);
        assert_eq!(check.outcome, "pass", "{} metadata drifted", check.id);
    }
}

#[test]
fn architecture_identity_and_semantic_pins_are_independent() {
    let registry = real_registry();
    assert_eq!(registry.decisions.len(), PINNED_DECISION_COUNT);
    assert_eq!(
        registry
            .decisions
            .iter()
            .filter(|decision| decision.category == "bibliography")
            .count(),
        PINNED_BIBLIOGRAPHY_COUNT
    );
    assert_eq!(
        architecture::recompute_decision_id_hash(&registry),
        PINNED_DECISION_ID_HASH
    );
    assert_eq!(registry.registry.id_table_hash, PINNED_DECISION_ID_HASH);
    assert_eq!(
        architecture::recompute_bibliography_id_hash(&registry),
        PINNED_BIBLIOGRAPHY_ID_HASH
    );
    assert_eq!(
        architecture::recompute_semantic_contract_hash(&registry),
        PINNED_SEMANTIC_CONTRACT_HASH
    );
}

#[test]
fn architecture_owner_reverse_walk_is_total_and_deterministic() {
    let registry = real_registry();
    let first = architecture::provenance_index(&registry);
    let second = architecture::provenance_index(&registry);
    assert_eq!(first, second);
    assert!(!first.is_empty());

    let kinds: BTreeSet<&str> = first
        .iter()
        .map(|entry| entry.owner_kind.as_str())
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from(["bead", "checker", "crate", "evidence"])
    );
    for entry in first {
        assert!(!entry.owner_id.is_empty());
        assert!(!entry.decision_ids.is_empty());
        assert!(!entry.profile_ids.is_empty());
        assert!(!entry.rationales.is_empty());
        assert!(entry.decision_ids.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(entry.profile_ids.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(entry.rationales.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

#[test]
fn architecture_bead_provenance_is_total_pinned_and_bidirectional() {
    let registry = real_registry();
    let root = repo_root();
    let first = architecture::bead_provenance_index(&registry, &root)
        .expect("every Bead resolves to architecture rationale");
    let second = architecture::bead_provenance_index(&registry, &root)
        .expect("repeat provenance walk resolves");
    assert_eq!(
        first, second,
        "provenance order and contents must be stable"
    );
    assert_eq!(first.len(), PINNED_BEAD_COUNT);
    assert!(
        first
            .windows(2)
            .all(|pair| pair[0].bead_id < pair[1].bead_id),
        "Bead provenance must be strictly sorted and unique"
    );

    let mut class_counts = BTreeMap::new();
    for entry in &first {
        *class_counts
            .entry(entry.resolution_class.as_str())
            .or_insert(0usize) += 1;
        assert!(!entry.bead_id.is_empty());
        assert!(!entry.rule_id.is_empty(), "{} has no rule", entry.bead_id);
        assert!(
            !entry.decision_ids.is_empty(),
            "{} has no decision",
            entry.bead_id
        );
        assert!(
            !entry.profile_ids.is_empty(),
            "{} has no profile",
            entry.bead_id
        );
        assert!(
            !entry.summaries.is_empty(),
            "{} has no summary",
            entry.bead_id
        );
        assert!(
            !entry.rationales.is_empty(),
            "{} has no rationale",
            entry.bead_id
        );
        assert!(
            !entry.source_anchors.is_empty(),
            "{} has no source anchor",
            entry.bead_id
        );
        assert!(
            !entry.replay_commands.is_empty(),
            "{} has no replay command",
            entry.bead_id
        );
        for values in [
            &entry.decision_ids,
            &entry.profile_ids,
            &entry.summaries,
            &entry.rationales,
            &entry.source_anchors,
            &entry.replay_commands,
        ] {
            assert!(
                values.windows(2).all(|pair| pair[0] < pair[1]),
                "{} contains unsorted or duplicate provenance values",
                entry.bead_id
            );
        }
    }
    assert_eq!(
        class_counts,
        BTreeMap::from([
            ("bet_label", PINNED_BET_LABEL_COUNT),
            ("direct_owner", PINNED_DIRECT_OWNER_COUNT),
            ("exact_override", PINNED_EXACT_OVERRIDE_COUNT),
            ("family_rule", PINNED_FAMILY_RULE_COUNT),
        ])
    );
    assert_eq!(
        architecture::recompute_bead_binding_hash(&first),
        PINNED_BEAD_BINDING_HASH
    );
    assert_eq!(
        registry.bead_provenance.binding_hash,
        PINNED_BEAD_BINDING_HASH
    );

    let entries: BTreeMap<&str, &architecture::BeadProvenanceEntry> = first
        .iter()
        .map(|entry| (entry.bead_id.as_str(), entry))
        .collect();
    for decision in registry
        .decisions
        .iter()
        .filter(|decision| decision.status != "superseded")
    {
        for owner in &decision.owner_beads {
            let entry = entries
                .get(owner.as_str())
                .unwrap_or_else(|| panic!("explicit owner {owner} has no provenance row"));
            assert!(
                entry.decision_ids.contains(&decision.id),
                "explicit edge {owner} -> {} is absent from reverse provenance",
                decision.id
            );
        }
    }
}

#[test]
fn architecture_relationship_vocabulary_is_closed_and_exercised() {
    let registry = real_registry();
    let actual: BTreeSet<&str> = registry
        .decisions
        .iter()
        .map(|decision| decision.relationship_kind.as_str())
        .collect();
    assert_eq!(actual, BTreeSet::from(ALLOWED_RELATIONSHIP_KINDS));

    let mut mutation = registry;
    mutation
        .decisions
        .iter_mut()
        .find(|decision| decision.id == "FG-ADR-BET-B1")
        .expect("B1 decision exists")
        .relationship_kind = "accidental_dependency".into();
    let codes = violation_codes(&mutation);
    assert!(codes.contains("closed_enum"));
    assert!(codes.contains("semantic_contract_hash_mismatch"));
}

#[test]
fn architecture_neg_missing_owner() {
    let mut registry = real_registry();
    registry.decisions[0].owner_beads.clear();
    assert_code(&registry, "owner_bead_missing");
}

#[test]
fn architecture_neg_unresolved_owner_and_crate() {
    let mut registry = real_registry();
    registry.decisions[0].owner_beads = vec!["fgdb-does-not-exist".into()];
    registry.decisions[0].owner_crates = vec!["fgdb-not-planned".into()];
    let codes = violation_codes(&registry);
    assert!(codes.contains("owner_bead_unresolved"));
    assert!(codes.contains("owner_crate_unplanned"));
}

#[test]
fn architecture_neg_reports_the_actual_invalid_secondary_owner() {
    let mut registry = real_registry();
    let decision = registry
        .decisions
        .iter_mut()
        .find(|decision| decision.id == "FG-ADR-BET-B1")
        .expect("B1 decision exists");
    decision.owner_beads.push("fgdb-does-not-exist".into());
    decision.owner_crates.push("fgdb-not-planned".into());

    let violations = architecture::validate_architecture(&registry, &repo_root());
    assert!(violations.iter().any(|violation| {
        violation.code == "owner_bead_unresolved"
            && violation.decision_id == "FG-ADR-BET-B1"
            && violation.owner_bead == "fgdb-does-not-exist"
    }));
    assert!(violations.iter().any(|violation| {
        violation.code == "owner_crate_unplanned"
            && violation.decision_id == "FG-ADR-BET-B1"
            && violation.owner_crate == "fgdb-not-planned"
    }));
}

#[test]
fn architecture_neg_invert_rejection() {
    let mut registry = real_registry();
    let rejection = registry
        .decisions
        .iter_mut()
        .find(|decision| decision.category == "rejection" && decision.disposition == "reject")
        .expect("literal rejection exists");
    rejection.disposition = "adopt".into();
    rejection.relationship_kind = "design_donor".into();
    let codes = violation_codes(&registry);
    assert!(codes.contains("frozen_rejection_changed"));
    assert!(codes.contains("semantic_contract_hash_mismatch"));
}

#[test]
fn architecture_neg_widen_profile_claim() {
    let mut registry = real_registry();
    registry.profiles[0].no_claim_boundary.clear();
    let codes = violation_codes(&registry);
    assert!(codes.contains("profile_required_array"));
    assert!(codes.contains("semantic_contract_hash_mismatch"));
}

#[test]
fn architecture_neg_promote_research_citation_to_dependency() {
    let mut registry = real_registry();
    let citation = registry
        .decisions
        .iter_mut()
        .find(|decision| decision.category == "bibliography")
        .expect("bibliography row exists");
    citation.disposition = "consume".into();
    citation.relationship_kind = "consume_as_is".into();
    citation.owner_crates = vec!["fgdb-types".into()];
    let codes = violation_codes(&registry);
    assert!(codes.contains("bibliography_promoted"));
    assert!(codes.contains("semantic_contract_hash_mismatch"));
}

#[test]
fn architecture_neg_semantic_change_with_stable_id() {
    let mut registry = real_registry();
    registry.decisions[0].summary.push_str(" widened");
    let codes = violation_codes(&registry);
    assert_eq!(
        codes,
        BTreeSet::from(["semantic_contract_hash_mismatch".to_string()]),
        "an otherwise well-formed semantic edit must trip the independent pin"
    );
}

#[test]
fn architecture_neg_duplicate_identity() {
    let mut registry = real_registry();
    registry.decisions[1].id = registry.decisions[0].id.clone();
    registry.decisions[1].stable_key = registry.decisions[0].stable_key.clone();
    let codes = violation_codes(&registry);
    assert!(codes.contains("decision_id_duplicate"));
    assert!(codes.contains("stable_key_duplicate"));
}

#[test]
fn architecture_neg_source_metadata_drift() {
    let mut registry = real_registry();
    registry.source_blocks[0].byte_count += 1;
    assert_code(&registry, "source_metadata_pin");
}

#[test]
fn architecture_neg_duplicate_source_anchor() {
    let mut registry = real_registry();
    registry.decisions[1].source_anchor = registry.decisions[0].source_anchor.clone();
    assert_code(&registry, "source_anchor_duplicate");
}

#[test]
fn architecture_neg_missing_profile_assumption() {
    let mut registry = real_registry();
    registry
        .profiles
        .iter_mut()
        .find(|profile| profile.id == "FG-ADR-PROFILE-CONSTITUTIONAL")
        .expect("constitutional profile exists")
        .assumptions
        .clear();
    let codes = violation_codes(&registry);
    assert!(codes.contains("profile_required_array"));
    assert!(codes.contains("semantic_contract_hash_mismatch"));
}

#[test]
fn architecture_neg_orphan_and_ambiguous_bead_families() {
    let mut orphan = real_registry();
    orphan
        .bead_families
        .iter_mut()
        .find(|family| family.id == "risk-governance")
        .expect("risk family exists")
        .pattern = "fgdb-no-such-risk-".into();
    let error = architecture::resolve_bead_provenance(&orphan, &repo_root())
        .expect_err("removing the risk family must orphan live Beads");
    assert!(error.contains("bead_provenance_orphan"), "{error}");
    assert!(error.contains("fgdb-risk-"), "{error}");

    let mut ambiguous = real_registry();
    let family = ambiguous
        .bead_families
        .iter_mut()
        .find(|family| family.id == "workstream-w1")
        .expect("zero-match W1 family exists");
    family.pattern = "fgdb-risk-".into();
    let error = architecture::resolve_bead_provenance(&ambiguous, &repo_root())
        .expect_err("overlapping family rules must fail closed");
    assert!(error.contains("bead_family_ambiguous"), "{error}");
    assert!(error.contains("fgdb-risk-"), "{error}");
}

#[test]
fn architecture_neg_rule_tables_and_resolution_pins() {
    let mut zero_match_rule = real_registry();
    zero_match_rule
        .bead_families
        .iter_mut()
        .find(|family| family.id == "workstream-w1")
        .expect("zero-match W1 family exists")
        .decision_ids = vec!["FG-ADR-CON-02".into()];
    assert_eq!(
        violation_codes(&zero_match_rule),
        BTreeSet::from(["semantic_contract_hash_mismatch".to_string()]),
        "even currently zero-match routing rules are independently pinned"
    );

    let mut binding = real_registry();
    binding.bead_provenance.binding_hash = "fnv1a64:0000000000000000".into();
    assert_code(&binding, "bead_binding_hash_mismatch");

    let mut count = real_registry();
    count.bead_provenance.bead_count += 1;
    let codes = violation_codes(&count);
    assert!(codes.contains("bead_count_pin"));
    assert!(codes.contains("bead_source_count"));

    let mut class_count = real_registry();
    class_count.bead_provenance.direct_owner_count += 1;
    let codes = violation_codes(&class_count);
    assert!(codes.contains("bead_count_pin"));
    assert!(codes.contains("bead_resolution_class_count"));

    let mut family_count = real_registry();
    family_count
        .bead_families
        .iter_mut()
        .find(|family| family.id == "risk-governance")
        .expect("risk family exists")
        .expected_match_count += 1;
    assert_code(&family_count, "bead_family_match_count");
}

#[test]
fn architecture_neg_planned_crate_universe_drift() {
    let mut registry = real_registry();
    registry.registry.planned_crates.pop();
    assert_code(&registry, "planned_crates_pin");
}
