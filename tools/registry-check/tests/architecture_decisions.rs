//! Executable ADR contract tests (fgdb-architecture-decision-record-xwkw).
//!
//! These tests exercise the shipped registry plus typed mutations. They do
//! not duplicate the checker: each negative changes one contract dimension
//! and asserts that the public validator rejects the resulting graph.

use registry_check::architecture::{
    self, ALLOWED_RELATIONSHIP_KINDS, ArchitectureRegistry, PINNED_BEAD_BINDING_HASH,
    PINNED_BEAD_COUNT, PINNED_BET_LABEL_COUNT, PINNED_BIBLIOGRAPHY_COUNT,
    PINNED_BIBLIOGRAPHY_ID_HASH, PINNED_DECISION_COUNT, PINNED_DECISION_ID_HASH,
    PINNED_DIRECT_OWNER_COUNT, PINNED_EXACT_OVERRIDE_COUNT, PINNED_EXTERNAL_REVIEW_DECISION_COUNT,
    PINNED_EXTERNAL_REVIEW_HISTORY_HASH, PINNED_FAMILY_RULE_COUNT, PINNED_SEMANTIC_CONTRACT_HASH,
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
fn transparency_decisions_bind_the_exact_governed_invariants() -> Result<(), String> {
    let registry = real_registry();
    let expected = ["FG-INV-08", "FG-INV-10", "FG-INV-16"];

    for id in [
        "FG-ADR-GAP-VERIFIABILITY-ACCUMULATOR",
        "FG-ADR-CAL-TRANSPARENCY-AUTHENTICATED-STORAGE",
    ] {
        let decision = registry
            .decisions
            .iter()
            .find(|decision| decision.id == id)
            .ok_or_else(|| format!("missing transparency decision {id}"))?;
        assert_eq!(
            decision
                .affected_invariants
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            expected,
            "{id} must bind the complete transparency invariant set"
        );
    }

    Ok(())
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
    // A floor: another pane's `br create` may legitimately have grown the
    // corpus since this pin was frozen, and that must not fail the suite.
    assert!(
        first.len() >= PINNED_BEAD_COUNT,
        "resolved {} beads, floor is {PINNED_BEAD_COUNT}",
        first.len()
    );
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
    for (class, floor) in [
        ("bet_label", PINNED_BET_LABEL_COUNT),
        ("direct_owner", PINNED_DIRECT_OWNER_COUNT),
        ("exact_override", PINNED_EXACT_OVERRIDE_COUNT),
        ("family_rule", PINNED_FAMILY_RULE_COUNT),
    ] {
        let actual = class_counts.get(class).copied().unwrap_or(0);
        assert!(
            actual >= floor,
            "resolution class {class:?} has {actual} rows, floor is {floor}"
        );
    }
    // Keyed by rule, so this is an EQUALITY: no bead can move it, and a rule
    // edit must. The corpus-wide binding hash below is a projection, not a pin.
    assert_eq!(
        architecture::recompute_rule_binding_hash(&registry),
        PINNED_BEAD_BINDING_HASH
    );
    assert_eq!(
        registry.bead_provenance.binding_hash,
        PINNED_BEAD_BINDING_HASH
    );
    assert!(
        architecture::recompute_bead_binding_hash(&first).starts_with("fnv1a64:"),
        "the corpus projection must still compute"
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
                .expect("explicit owner must have a provenance row");
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
        BTreeSet::from([
            "semantic_contract_hash_mismatch".to_string(),
            "bead_rule_binding_hash_mismatch".to_string(),
            "independent_bead_rule_binding_hash_mismatch".to_string(),
        ]),
        "even currently zero-match routing rules are independently pinned, and \
         retargeting one now also moves the rule-keyed binding hash"
    );

    let mut binding = real_registry();
    binding.bead_provenance.binding_hash = "fnv1a64:0000000000000000".into();
    assert_code(&binding, "bead_rule_binding_hash_mismatch");

    // Raising a floor ABOVE the observed corpus must still fire. This is the
    // vacuity control for the floors: a bound nothing can violate protects
    // nothing, and `<` would silently pass every one of these.
    //
    // Each mutation is derived from the OBSERVED count, not from the declared
    // floor. Another pane's `br create` legitimately lifts the corpus above the
    // floor, and `declared + 1` then lands at or under the actual — which is
    // how this control first went vacuous the moment the corpus reached 402
    // against a floor of 401.
    let observed = architecture::bead_provenance_index(&real_registry(), &repo_root())
        .expect("provenance resolves");
    let observed_total = observed.len();
    let observed_direct = observed
        .iter()
        .filter(|entry| entry.resolution_class == "direct_owner")
        .count();
    let observed_risk = observed
        .iter()
        .filter(|entry| entry.rule_id == "risk-governance")
        .count();

    let mut count = real_registry();
    count.bead_provenance.bead_count = observed_total + 1;
    let codes = violation_codes(&count);
    assert!(codes.contains("bead_count_pin"));
    assert!(codes.contains("bead_source_count_below_floor"));

    let mut class_count = real_registry();
    class_count.bead_provenance.direct_owner_count = observed_direct + 1;
    let codes = violation_codes(&class_count);
    assert!(codes.contains("bead_count_pin"));
    assert!(codes.contains("bead_resolution_class_count_below_floor"));

    let mut family_count = real_registry();
    family_count
        .bead_families
        .iter_mut()
        .find(|family| family.id == "risk-governance")
        .expect("risk family exists")
        .expected_match_count = observed_risk + 1;
    assert_code(&family_count, "bead_family_match_count_below_floor");
}

#[test]
fn architecture_neg_planned_crate_universe_drift() {
    let mut registry = real_registry();
    registry.registry.planned_crates.pop();
    assert_code(&registry, "planned_crates_pin");
}

#[test]
fn architecture_external_review_chains_cover_every_active_foundation_and_sota_claim() {
    let registry = real_registry();
    let applicable = registry
        .decisions
        .iter()
        .filter(|decision| {
            decision.status != "superseded"
                && (decision.category.starts_with("foundation_")
                    || decision.category.starts_with("sota_"))
        })
        .count();
    assert_eq!(applicable, PINNED_EXTERNAL_REVIEW_DECISION_COUNT);
    assert_eq!(
        registry
            .external_reviews
            .iter()
            .map(|review| review.decision_id.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        PINNED_EXTERNAL_REVIEW_DECISION_COUNT
    );
    assert!(
        architecture::validate_external_review_contract(&registry).is_empty(),
        "shipped external-review chains must be complete and current"
    );
    assert_eq!(
        architecture::recompute_external_review_history_hash(&registry),
        PINNED_EXTERNAL_REVIEW_HISTORY_HASH
    );
    assert_eq!(
        registry.registry.external_review_history_hash,
        PINNED_EXTERNAL_REVIEW_HISTORY_HASH
    );
}

#[test]
fn architecture_neg_external_review_claim_stales_independently_of_semantic_pin() {
    let mut registry = real_registry();
    registry
        .decisions
        .iter_mut()
        .find(|decision| {
            decision.category.starts_with("foundation_") && decision.status == "frozen"
        })
        .expect("frozen foundation decision exists")
        .summary
        .push_str(" changed after review");

    let codes: BTreeSet<String> = architecture::validate_external_review_contract(&registry)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert!(codes.contains("external_review_claim_stale"), "{codes:?}");

    let mut rewritten_tip = real_registry();
    let decision_id = rewritten_tip
        .decisions
        .iter()
        .find(|decision| {
            decision.category.starts_with("foundation_") && decision.status == "frozen"
        })
        .expect("frozen foundation decision exists")
        .id
        .clone();
    let profile_id = rewritten_tip
        .decisions
        .iter()
        .find(|decision| decision.id == decision_id)
        .expect("selected decision exists")
        .profile
        .clone();
    rewritten_tip
        .decisions
        .iter_mut()
        .find(|decision| decision.id == decision_id)
        .expect("selected decision exists")
        .summary
        .push_str(" rewritten in place");
    let new_claim = architecture::recompute_external_review_claim_fingerprint(
        rewritten_tip
            .decisions
            .iter()
            .find(|decision| decision.id == decision_id)
            .expect("selected decision exists"),
        rewritten_tip
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .expect("selected profile exists"),
    );
    let tip_index = rewritten_tip
        .external_reviews
        .iter()
        .enumerate()
        .filter(|(_, review)| review.decision_id == decision_id)
        .max_by_key(|(_, review)| review.sequence)
        .map(|(index, _)| index)
        .expect("selected decision has a review tip");
    rewritten_tip.external_reviews[tip_index].claim_fingerprint = new_claim;
    let new_record = architecture::recompute_external_review_record_fingerprint(
        &rewritten_tip,
        &rewritten_tip.external_reviews[tip_index],
    )
    .expect("self-consistent rewritten review hashes");
    rewritten_tip.external_reviews[tip_index].record_fingerprint = new_record;
    rewritten_tip.registry.external_review_history_hash =
        architecture::recompute_external_review_history_hash(&rewritten_tip);

    let codes: BTreeSet<String> = architecture::validate_external_review_contract(&rewritten_tip)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert!(
        codes.contains("independent_external_review_history_hash_mismatch"),
        "a self-consistent in-place review rewrite must still trip the independent append-only pin: {codes:?}"
    );
}

#[test]
fn architecture_neg_external_review_chain_gap_and_source_tamper() {
    let mut missing = real_registry();
    let decision_id = missing
        .decisions
        .iter()
        .find(|decision| decision.category.starts_with("sota_") && decision.status != "superseded")
        .expect("active SOTA decision exists")
        .id
        .clone();
    missing
        .external_reviews
        .retain(|review| review.decision_id != decision_id);
    let codes: BTreeSet<String> = architecture::validate_external_review_contract(&missing)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert!(
        codes.contains("external_review_coverage_missing"),
        "{codes:?}"
    );

    let mut broken_predecessor = real_registry();
    broken_predecessor.external_reviews[0].predecessor = "FG-ADR-REVIEW-NOT-PRIOR".into();
    let codes: BTreeSet<String> =
        architecture::validate_external_review_contract(&broken_predecessor)
            .into_iter()
            .map(|violation| violation.code)
            .collect();
    assert!(codes.contains("external_review_predecessor"), "{codes:?}");
    assert!(
        codes.contains("external_review_record_fingerprint"),
        "{codes:?}"
    );

    let mut source_tamper = real_registry();
    source_tamper.external_review_sources[0]
        .uri
        .push_str("?mutable=1");
    let codes: BTreeSet<String> = architecture::validate_external_review_contract(&source_tamper)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert!(
        codes.contains("external_review_source_fingerprint"),
        "{codes:?}"
    );
    assert!(
        codes.contains("external_review_record_fingerprint"),
        "{codes:?}"
    );

    let mut malformed_source = real_registry();
    malformed_source.external_review_sources[0].published_at = "2099-01-01".into();
    malformed_source.external_review_sources[0].content_digest = "sha256:not-a-digest".into();
    let codes: BTreeSet<String> =
        architecture::validate_external_review_contract(&malformed_source)
            .into_iter()
            .map(|violation| violation.code)
            .collect();
    assert!(
        codes.contains("external_review_source_date_order"),
        "{codes:?}"
    );
    assert!(codes.contains("external_review_source_digest"), "{codes:?}");
}

#[test]
fn architecture_live_entrypoints_resolve_exact_targets_and_preserve_scope() {
    let registry = real_registry();
    let declaration = registry
        .verification_entrypoints
        .iter()
        .find(|declaration| declaration.entrypoint == "cargo-test:architecture_decisions")
        .expect("architecture governance entrypoint is declared");
    assert_eq!(declaration.status, "live");
    assert_eq!(declaration.evidence_scope, "governance");
    assert_eq!(
        declaration.checker_id.as_deref(),
        Some("cargo-test:architecture_decisions")
    );

    let decision_id = &registry.decisions[0].id;
    let governance = architecture::resolved_live_entrypoints_for_scope(
        &registry,
        &repo_root(),
        decision_id,
        "governance",
    )
    .expect("governance evidence resolves");
    let implementation = architecture::resolved_live_entrypoints_for_scope(
        &registry,
        &repo_root(),
        decision_id,
        "implementation",
    )
    .expect("implementation evidence query resolves");
    assert!(governance.contains(&"cargo-test:architecture_decisions".to_string()));
    assert!(
        !implementation.contains(&"cargo-test:architecture_decisions".to_string()),
        "universal ADR governance must never count as subsystem implementation evidence"
    );
}

#[test]
fn architecture_neg_live_entrypoint_checker_target_selector_and_command() {
    let mut swapped_checker = real_registry();
    swapped_checker
        .verification_entrypoints
        .iter_mut()
        .find(|declaration| declaration.entrypoint == "cargo-test:architecture_decisions")
        .expect("architecture entrypoint exists")
        .checker_id = Some("architecture_decisions".into());
    assert_code(&swapped_checker, "verification_entrypoint_checker_identity");

    let mut missing_target = real_registry();
    missing_target
        .verification_entrypoints
        .iter_mut()
        .find(|declaration| declaration.entrypoint == "cargo-test:architecture_decisions")
        .expect("architecture entrypoint exists")
        .target = Some("does_not_exist".into());
    let codes = violation_codes(&missing_target);
    assert!(
        codes.contains("verification_entrypoint_target_mismatch"),
        "{codes:?}"
    );
    assert!(
        codes.contains("verification_entrypoint_target_missing"),
        "{codes:?}"
    );

    let mut wrong_package = real_registry();
    wrong_package
        .verification_entrypoints
        .iter_mut()
        .find(|declaration| declaration.entrypoint == "cargo-test:architecture_decisions")
        .expect("architecture entrypoint exists")
        .package = Some("not-a-workspace-package".into());
    assert_code(&wrong_package, "verification_entrypoint_package");

    let mut missing_selector = real_registry();
    missing_selector
        .verification_entrypoints
        .iter_mut()
        .find(|declaration| declaration.entrypoint == "cargo-test:architecture_decisions")
        .expect("architecture entrypoint exists")
        .selector = Some("no_such_test".into());
    assert_code(&missing_selector, "verification_entrypoint_selector");

    let mut wrong_command = real_registry();
    wrong_command
        .verification_entrypoints
        .iter_mut()
        .find(|declaration| declaration.entrypoint == "cargo-test:architecture_decisions")
        .expect("architecture entrypoint exists")
        .command_argv = Some(vec!["cargo".into(), "test".into()]);
    assert_code(&wrong_command, "verification_entrypoint_command");

    let mut reused_checker = real_registry();
    let live = reused_checker
        .verification_entrypoints
        .iter()
        .find(|declaration| declaration.entrypoint == "cargo-test:architecture_decisions")
        .expect("architecture entrypoint exists")
        .clone();
    let planned = reused_checker
        .verification_entrypoints
        .iter_mut()
        .find(|declaration| declaration.status == "planned")
        .expect("planned entrypoint exists");
    planned.status = "live".into();
    planned.evidence_scope = "governance".into();
    planned.checker_id = live.checker_id;
    planned.package = live.package;
    planned.target = live.target;
    planned.selector = live.selector;
    planned.command_argv = live.command_argv;
    assert_code(&reused_checker, "verification_entrypoint_checker_reused");
}

#[test]
fn architecture_neg_planned_and_governance_entrypoints_cannot_claim_implementation() {
    let mut planned = real_registry();
    let declaration = planned
        .verification_entrypoints
        .iter_mut()
        .find(|declaration| declaration.entrypoint == "cargo-test:architecture_decisions")
        .expect("architecture entrypoint exists");
    declaration.status = "planned".into();
    let codes = violation_codes(&planned);
    assert!(
        codes.contains("planned_verification_invocation_present"),
        "{codes:?}"
    );
    assert!(
        codes.contains("live_governance_entrypoint_missing"),
        "{codes:?}"
    );

    let mut relabeled = real_registry();
    relabeled
        .verification_entrypoints
        .iter_mut()
        .find(|declaration| declaration.entrypoint == "cargo-test:architecture_decisions")
        .expect("architecture entrypoint exists")
        .evidence_scope = "implementation".into();
    assert_code(&relabeled, "verification_entrypoint_scope_mismatch");
    let implementation = architecture::resolved_live_entrypoints_for_scope(
        &relabeled,
        &repo_root(),
        &relabeled.decisions[0].id,
        "implementation",
    )
    .expect("scope query resolves");
    assert!(
        !implementation.contains(&"cargo-test:architecture_decisions".to_string()),
        "scope mismatch must fail closed rather than laundering governance as implementation"
    );
}

/// The concurrent-writer lock for fgdb-lzol.
///
/// `.beads/issues.jsonl` has N writers. Under the old equality pins, a bead
/// created by ANY pane invalidated every other pane's just-frozen pins, so a
/// correct pane racing another correct pane still went red. That defect only
/// exists under N>1, so a single-writer test cannot see it.
///
/// The reachable states of "pane A freezes while pane B creates" are exactly
/// the corpora that are supersets of what A read. This walks a prefix of that
/// space with real files and the real validator, and the deletion control at
/// the end proves the floors are not vacuously satisfied.
#[cfg(unix)]
#[test]
fn concurrent_bead_creation_cannot_red_another_panes_tree() {
    use std::fs;

    let root = repo_root();
    let corpus = fs::read_to_string(root.join(".beads/issues.jsonl")).expect("corpus reads");

    // A pid-scoped fixture: a shared fixture name lets a concurrent pane's run
    // delete this one's tree mid-test and fail a different assertion each time.
    let fixture = std::env::temp_dir().join(format!(
        "fgdb-lzol-concurrent-writers-{}",
        std::process::id()
    ));
    fs::create_dir_all(fixture.join(".beads")).expect("fixture root");
    for entry in fs::read_dir(&root).expect("repo root reads") {
        let entry = entry.expect("entry");
        if entry.file_name() == ".beads" {
            continue;
        }
        let link = fixture.join(entry.file_name());
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(entry.path(), &link).ok();
    }
    let beads = fixture.join(".beads/issues.jsonl");

    let write_and_validate = |text: &str| -> Vec<String> {
        fs::write(&beads, text).expect("fixture corpus writes");
        let registry = architecture::load_from_repo(&fixture).expect("fixture registry loads");
        architecture::validate_architecture(&registry, &fixture)
            .into_iter()
            .map(|violation| violation.code)
            .collect()
    };

    // Control: the fixture must reproduce the repo's own verdict. Without this,
    // an empty violation set could mean the fixture is not being read at all.
    assert!(
        write_and_validate(&corpus).is_empty(),
        "the unmodified fixture must be as green as the repo it mirrors"
    );

    // Pane B creates k beads while pane A holds a freeze taken before any of
    // them landed. Every k must leave pane A's tree green.
    for k in 1..=5 {
        let mut grown = corpus.clone();
        for i in 0..k {
            grown.push_str(&format!(
                "{{\"id\":\"fgdb-lzolrace{i}\",\"title\":\"concurrent create\",\"status\":\"open\",\"labels\":[\"b1\"]}}\n"
            ));
        }
        let codes = write_and_validate(&grown);
        assert!(
            codes.is_empty(),
            "{k} concurrently created bead(s) red the tree: {codes:?}"
        );
    }

    // Vacuity control. Floors that never fire would pass every assertion above
    // while protecting nothing, so losing a record MUST still be caught.
    let first_line_end = corpus.find('\n').expect("corpus has a first record") + 1;
    let shrunk = corpus[first_line_end..].to_string();
    let codes = write_and_validate(&shrunk);
    assert!(
        codes.iter().any(|code| code.ends_with("_below_floor")),
        "deleting a bead must trip a floor, got {codes:?}"
    );
}

// ---------------------------------------------------------------------------
// Orphan diagnosis (bead fgdb-bead-provenance-orphan-workstream-tag-7u5m)
//
// A bead that resolves by no mechanism used to be told only that it failed:
//
//     bead has no direct owner, bet label, exact override, or family rule
//
// which is one sentence for three structurally different faults needing three
// different repairs. Measured on the 405-record corpus at 2326fe8: 232 records
// carry a workstream/gate tag (239 label-instances over 16 tokens), 36 carry no
// labels at all, and 16 carry only labels irrelevant to provenance. Diagnosing
// a single orphan therefore cost a `git log -S` per record.
//
// The repair is diagnostic precision, NOT a prohibition. A workstream tag is a
// legitimate, pervasive, orthogonal taxonomy — rejecting one would redden 232
// records to catch the handful that are actually misfiled.
// ---------------------------------------------------------------------------

/// The real beads corpus plus planted records, under a scratch root.
///
/// `tag` must be unique per test: these run in parallel and the builder opens
/// its fixture by destroying it.
fn corpus_with(tag: &str, planted: &[(&str, &[&str])]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("fgdb-orphan-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".beads")).expect("fixture .beads dir");
    let mut text = std::fs::read_to_string(repo_root().join(".beads/issues.jsonl"))
        .expect("real corpus reads");
    for (id, labels) in planted {
        let labels = labels
            .iter()
            .map(|label| format!("\"{label}\""))
            .collect::<Vec<_>>()
            .join(",");
        text.push_str(&format!(
            "{{\"id\":\"fgdb-zz7u5m-{id}\",\"status\":\"open\",\"labels\":[{labels}]}}\n"
        ));
    }
    std::fs::write(root.join(".beads/issues.jsonl"), text).expect("fixture corpus");
    root
}

/// Issues about the planted records only.
///
/// The live corpus has six concurrent writers, so a global issue count is not a
/// stable assertion; every test below quantifies over its own planted ids.
fn planted_issues(registry: &ArchitectureRegistry, root: &Path) -> Vec<String> {
    architecture::resolve_bead_provenance(registry, root)
        .expect_err("planted orphans must make the index non-total")
        .split("; ")
        .filter(|issue| issue.contains("zz7u5m"))
        .map(str::to_string)
        .collect()
}

fn issues_naming<'a>(issues: &'a [String], bead_id: &str) -> Vec<&'a str> {
    issues
        .iter()
        .filter(|issue| issue.contains(bead_id))
        .map(String::as_str)
        .collect()
}

#[test]
fn architecture_orphan_names_the_workstream_tag_it_carries() {
    // The real record this bead was filed about. `fgdb-zwhh` carries
    // labels = ["w1"] and nothing else, and resolves today only because an
    // exact override was added for it by hand. Drop that override and it
    // orphans again — the one condition under which the checker is allowed to
    // mention its label at all.
    let mut registry = real_registry();
    let before = registry.bead_overrides.len();
    registry
        .bead_overrides
        .retain(|rule| rule.bead_id != "fgdb-zwhh");
    assert_eq!(
        registry.bead_overrides.len(),
        before - 1,
        "the fixture must remove exactly one override"
    );

    let error = architecture::resolve_bead_provenance(&registry, &repo_root())
        .expect_err("dropping fgdb-zwhh's override must orphan it");
    let issues: Vec<String> = error.split("; ").map(str::to_string).collect();
    let mine = issues_naming(&issues, "fgdb-zwhh");
    assert_eq!(mine.len(), 1, "one fault, one diagnosis: {error}");
    assert!(
        mine[0].starts_with("bead_workstream_label_in_bet_position fgdb-zwhh:"),
        "the diagnosis must name its own code and bead: {:?}",
        mine[0]
    );
    assert!(
        mine[0].contains("[\"w1\"]"),
        "the diagnosis must name the label that failed to resolve: {:?}",
        mine[0]
    );
    assert!(
        !mine[0].contains("bead_provenance_orphan"),
        "a labelled record must not fall back to the undifferentiated message: {:?}",
        mine[0]
    );
}

#[test]
fn architecture_orphan_diagnoses_each_shape_differently() {
    // Three faults, three repairs, three messages. Collapse them back into one
    // sentence and the pairwise-distinct assert below dies — which is this
    // bead's defect, restated as a law.
    let registry = real_registry();
    let root = corpus_with(
        "shapes",
        &[
            ("workstream", &["w1"]),
            ("bare", &[]),
            ("topical", &["performance", "verification"]),
        ],
    );
    let issues = planted_issues(&registry, &root);
    assert_eq!(issues.len(), 3, "{issues:#?}");

    let workstream = issues_naming(&issues, "zz7u5m-workstream");
    assert_eq!(workstream.len(), 1, "{issues:#?}");
    assert!(
        workstream[0].starts_with("bead_workstream_label_in_bet_position")
            && workstream[0].contains("[\"w1\"]"),
        "{:?}",
        workstream[0]
    );

    let bare = issues_naming(&issues, "zz7u5m-bare");
    assert_eq!(bare.len(), 1, "{issues:#?}");
    assert!(
        bare[0].starts_with("bead_provenance_orphan")
            && bare[0].contains("carries no labels at all"),
        "{:?}",
        bare[0]
    );

    let topical = issues_naming(&issues, "zz7u5m-topical");
    assert_eq!(topical.len(), 1, "{issues:#?}");
    assert!(
        topical[0].starts_with("bead_provenance_orphan")
            && topical[0].contains("[\"performance\", \"verification\"]"),
        "{:?}",
        topical[0]
    );

    let bodies: BTreeSet<&str> = issues
        .iter()
        .map(|issue| issue.split_once(": ").expect("issue has a body").1)
        .collect();
    assert_eq!(
        bodies.len(),
        3,
        "three different faults must not share one message: {issues:#?}"
    );
}

#[test]
fn architecture_neg_workstream_diagnostic_is_scoped_to_the_orphan_path() {
    // THE CONTROL, and it fires in both directions. The tempting fix for this
    // bead is to reject workstream tags outright; 232 corpus records carry one,
    // so that fix reddens 232 rows to catch a handful. These three records all
    // carry a workstream tag and differ only in whether they resolve some other
    // way. Exactly ONE may be diagnosed: an implementation that validates the
    // tag instead of explaining the orphan reports three here, and one that
    // never fires reports zero.
    let registry = real_registry();
    let root = corpus_with(
        "scoped",
        &[
            ("resolves-bet", &["w1", "b1"]),
            ("resolves-gate", &["g0", "b3"]),
            ("orphans", &["w9"]),
        ],
    );
    let issues = planted_issues(&registry, &root);
    assert_eq!(
        issues.len(),
        1,
        "a workstream tag is legitimate on a record that resolves: {issues:#?}"
    );
    assert!(
        issues[0].starts_with("bead_workstream_label_in_bet_position fgdb-zz7u5m-orphans:")
            && issues[0].contains("[\"w9\"]"),
        "{:?}",
        issues[0]
    );
}

#[test]
fn architecture_bet_label_unknown_survives_the_orphan_diagnostic() {
    // The check that already worked must keep working, and must keep being the
    // one that speaks for a bet-shaped label. `b9` is not a workstream tag, so
    // it draws the undifferentiated orphan message plus its own vocabulary
    // violation — two issues for one record, naming the label in both.
    let registry = real_registry();
    let root = corpus_with("unknown-bet", &[("b9", &["b9"])]);
    let issues = planted_issues(&registry, &root);
    assert_eq!(issues.len(), 2, "{issues:#?}");
    assert!(
        issues[0].starts_with("bead_bet_label_unknown fgdb-zz7u5m-b9:")
            && issues[0].contains("\"b9\""),
        "{:?}",
        issues[0]
    );
    assert!(
        issues[1].starts_with("bead_provenance_orphan fgdb-zz7u5m-b9:")
            && issues[1].contains("[\"b9\"]"),
        "{:?}",
        issues[1]
    );
}

#[test]
fn architecture_provenance_issue_messages_are_parseable() {
    // `resolve_bead_provenance` joins its issues with "; ", so a message that
    // contains that sequence silently splits into two and every caller counting
    // issues gets a wrong number. Not hypothetical: the first draft of the
    // workstream diagnosis contained a semicolon and made a one-orphan fixture
    // report two. One law over every shape at once.
    let registry = real_registry();
    let root = corpus_with(
        "parseable",
        &[
            ("workstream", &["w1"]),
            ("bare", &[]),
            ("topical", &["performance", "verification"]),
            ("unknown-bet", &["b9"]),
        ],
    );
    let issues = planted_issues(&registry, &root);
    assert_eq!(issues.len(), 5, "{issues:#?}");
    for issue in &issues {
        let body = issue.split_once(": ").expect("issue has a body").1;
        assert!(
            !body.contains("; "),
            "an issue message may not contain the joiner separator: {issue:?}"
        );
    }
}
