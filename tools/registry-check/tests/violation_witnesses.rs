//! Cross-validator mutation witnesses for high-blast-radius laws.
//!
//! A string census can say that a violation code exists, but only a paired
//! control and mutation show that the production validator can emit it.  Each
//! test below first proves that its unmodified owner is accepted, changes one
//! load-bearing fact, and then requires the exact violation code.  Keeping the
//! control and mutation in the same harness prevents a malformed fixture from
//! masquerading as a witnessed law.
//!
//! # What this file is for
//!
//! `topology.rs`'s own docstring states the standard: *a law nobody has watched
//! fire is a law nobody knows works.*  The census behind
//! `fgdb-validator-laws-never-witnessed-firing-xnxy` measured how far the
//! repository was from it — at `dec248a`, 426 distinct violation codes, of which
//! 234 were production-reachable and had never been asserted by any test or e2e
//! script.  Green from those 234 was not evidence of anything.
//!
//! 122 of the 234 belong to `topology`, `threat`, `validate` and
//! `unsafe_ledger`.  The tables below close every one of them that an input can
//! reach; the `unsafe_ledger` share lives in `tests/unsafe_ledger.rs`, beside the
//! synthetic-workspace builder those laws need.  The remaining 112 are
//! Appendix A's, and are not touched here.
//!
//! # The shape every row keeps
//!
//! * the control is the real registry or a controlled tree fixture, and its
//!   exact target-code count is recorded before mutation;
//! * the mutation changes ONE load-bearing fact, through the same public owner
//!   the production binary calls;
//! * the assertion names the exact violation code and requires the mutation to
//!   add it beyond the control count. A string-only census would pass against a
//!   law that exists in source but can never fire.
//!
//! A table rather than one function per law is deliberate: the control is loaded
//! once and each row is applied to a fresh clone, so the suite cannot drift into
//! asserting against an already-mutated registry, and a failure reports every
//! law that stopped firing rather than only the first.
//!
//! # The laws that CANNOT fire, and why that is a finding rather than a gap
//!
//! Three codes in the residue are unreachable from any input, by construction:
//!
//! * `site_scanner_self_test_failed` and `safe_facing_self_test_failed`
//!   (`unsafe_ledger::check_workspace`), and
//! * `checker_liveness_self_test_failed` (`validate::validate_all`).
//!
//! Each guards a reader against ITSELF: the predicate compares a reader's answer
//! on a compiled-in fixture against a compiled-in constant, and no argument to
//! the checker reaches it.  They are the opposite of vacuous — they are the
//! controls that license every zero the readers below them report — but no
//! registry, manifest or source tree can make one fire, so no input-driven
//! witness for them can exist.  What CAN be witnessed is that the guard
//! predicate is live rather than constant-true, and
//! [`the_liveness_self_test_guard_can_be_false`] here (with its two siblings in
//! `tests/unsafe_ledger.rs`) does exactly that: same public reader, perturbed
//! fixture, answer moves.  The remaining step — the violation itself — is
//! reachable only by breaking the reader's source, and is proved that way in the
//! bead rather than checked in.

use registry_check::architecture::{self, ArchitectureRegistry};
use registry_check::model::{self, Registries, ScriptDisposition};
use registry_check::threat::{self, ThreatRegistry};
use registry_check::topology::{self, TopologyRegistry, WorkspaceScan};
use registry_check::{appendix_a, liveness, unsafe_ledger, validate};
use std::collections::BTreeSet;
use std::fs;
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

// ===========================================================================
// Architecture-decision claims rendered into the ADR — `architecture`
// ===========================================================================

struct ArchitectureRegistryWitness {
    code: &'static str,
    fact: &'static str,
    mutate: fn(&mut ArchitectureRegistry),
}

#[rustfmt::skip]
fn architecture_registry_witnesses() -> Vec<ArchitectureRegistryWitness> {
    vec![
    ArchitectureRegistryWitness { code: "source_bytes_mismatch", fact: "a frozen source block's declared digest is changed", mutate: |r| r.source_blocks[0].fnv1a64 = "0x0000000000000000".into() },
    ArchitectureRegistryWitness { code: "id_table_hash_mismatch", fact: "the decision identity-table digest is changed", mutate: |r| r.registry.id_table_hash = "fnv1a64:0000000000000000".into() },
    ArchitectureRegistryWitness { code: "semantic_contract_hash_mismatch", fact: "one decision summary changes without moving the independent semantic pin", mutate: |r| r.decisions[0].summary.push_str(" [witness mutation]") },
    ArchitectureRegistryWitness { code: "owner_bead_unresolved", fact: "one explicit owner edge names a Bead absent from the corpus", mutate: |r| r.decisions.iter_mut().find(|decision| !decision.owner_beads.is_empty()).expect("an owned decision exists").owner_beads[0] = "fgdb-no-such-owner-witness-7cem".into() },
    ArchitectureRegistryWitness { code: "live_verification_checker_missing", fact: "one live verification declaration loses its checker binding", mutate: |r| r.verification_entrypoints.iter_mut().find(|entry| entry.status.as_str().eq("live")).expect("a live verification entrypoint exists").checker_id = None },
    ArchitectureRegistryWitness { code: "research_dependency_promotion", fact: "one bibliography citation is promoted into runtime crate ownership", mutate: |r| r.decisions.iter_mut().find(|decision| decision.category == "bibliography").expect("a bibliography decision exists").owner_crates.push("fgdb-types".into()) },
    ArchitectureRegistryWitness { code: "bead_bet_label_set", fact: "the configured bet-label vocabulary loses one member", mutate: |r| { r.bead_provenance.allowed_bet_labels.pop(); } },
    ArchitectureRegistryWitness { code: "provenance_rationale_missing", fact: "one owner-bearing decision's profile row is removed", mutate: |r| { let profile_id = r.decisions.iter().find(|decision| !decision.owner_beads.is_empty()).expect("an owned decision exists").profile.clone(); r.profiles.retain(|profile| profile.id != profile_id); } },
    ]
}

struct ArchitectureTreeWitness {
    code: &'static str,
    fact: &'static str,
    run: fn(&ArchitectureRegistry, &Path) -> Result<(), String>,
}

fn architecture_code_count(registry: &ArchitectureRegistry, root: &Path, code: &str) -> usize {
    architecture::validate_architecture(registry, root)
        .iter()
        .filter(|violation| violation.code == code)
        .count()
}

fn require_architecture_code_increase(
    code: &str,
    control_registry: &ArchitectureRegistry,
    control_root: &Path,
    mutated_registry: &ArchitectureRegistry,
    mutated_root: &Path,
) -> Result<(), String> {
    let control = architecture_code_count(control_registry, control_root, code);
    let mutated = architecture_code_count(mutated_registry, mutated_root, code);
    if mutated > control {
        Ok(())
    } else {
        Err(format!(
            "{code} count did not increase: control={control}, mutation={mutated}"
        ))
    }
}

fn bead_record(id: &str, labels: &[&str]) -> String {
    let labels = labels
        .iter()
        .map(|label| format!("\"{label}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"id\":\"{id}\",\"status\":\"open\",\"labels\":[{labels}]}}\n")
}

fn witness_unknown_bet_label(registry: &ArchitectureRegistry, _: &Path) -> Result<(), String> {
    let root = scratch_root("architecture-unknown-bet-label");
    let path = ".beads/issues.jsonl";
    write_fixture(
        &root,
        path,
        &bead_record("fgdb-witness-unknown-bet", &["b1"]),
    );
    let control = architecture_code_count(registry, &root, "bead_bet_label_unknown");
    write_fixture(
        &root,
        path,
        &bead_record("fgdb-witness-unknown-bet", &["b7"]),
    );
    let mutated = architecture_code_count(registry, &root, "bead_bet_label_unknown");
    if mutated > control {
        Ok(())
    } else {
        Err(format!(
            "bead_bet_label_unknown count did not increase: control={control}, mutation={mutated}"
        ))
    }
}

fn witness_shadowed_override(registry: &ArchitectureRegistry, _: &Path) -> Result<(), String> {
    let direct = architecture::owner_decision_index(registry);
    let override_rule = registry
        .bead_overrides
        .iter()
        .find(|rule| !direct.contains_key(&rule.bead_id))
        .ok_or_else(|| "no unshadowed exact override exists for the control".to_string())?;
    let root = scratch_root("architecture-shadowed-override");
    let path = ".beads/issues.jsonl";
    write_fixture(&root, path, &bead_record(&override_rule.bead_id, &[]));
    let control = architecture_code_count(registry, &root, "bead_override_shadowed");
    write_fixture(&root, path, &bead_record(&override_rule.bead_id, &["b1"]));
    let mutated = architecture_code_count(registry, &root, "bead_override_shadowed");
    if mutated > control {
        Ok(())
    } else {
        Err(format!(
            "bead_override_shadowed count did not increase: control={control}, mutation={mutated}"
        ))
    }
}

fn witness_ambiguous_family(registry: &ArchitectureRegistry, _: &Path) -> Result<(), String> {
    let root = scratch_root("architecture-ambiguous-family");
    write_fixture(
        &root,
        ".beads/issues.jsonl",
        &bead_record("fgdb-risk-witness-7cem", &[]),
    );
    let mut mutated = registry.clone();
    mutated
        .bead_families
        .iter_mut()
        .find(|family| family.id == "workstream-w1")
        .ok_or_else(|| "workstream-w1 family exists for the mutation".to_string())?
        .pattern = "fgdb-risk-".into();
    require_architecture_code_increase("bead_family_ambiguous", registry, &root, &mutated, &root)
}

fn witness_provenance_not_total(registry: &ArchitectureRegistry, _: &Path) -> Result<(), String> {
    let root = scratch_root("architecture-provenance-not-total");
    let path = ".beads/issues.jsonl";
    let id = "fgdb-witness-provenance-total-7cem";
    write_fixture(&root, path, &bead_record(id, &["b1"]));
    let control = architecture_code_count(registry, &root, "bead_provenance_not_total");
    write_fixture(&root, path, &bead_record(id, &[]));
    let mutated = architecture_code_count(registry, &root, "bead_provenance_not_total");
    if mutated > control {
        Ok(())
    } else {
        Err(format!(
            "bead_provenance_not_total count did not increase: control={control}, mutation={mutated}"
        ))
    }
}

fn witness_document_drift(registry: &ArchitectureRegistry, repo: &Path) -> Result<(), String> {
    let fixture = scratch_root("architecture-document-drift");
    for block in &registry.source_blocks {
        let destination = fixture.join(&block.plan_path);
        fs::create_dir_all(destination.parent().expect("plan path has a parent"))
            .map_err(|error| format!("create plan fixture parent: {error}"))?;
        fs::copy(repo.join(&block.plan_path), &destination)
            .map_err(|error| format!("copy {}: {error}", block.plan_path))?;
    }
    let generated = architecture::generate_document(registry, repo)?;
    write_fixture(&fixture, architecture::DOCUMENT_PATH, &generated);
    if !architecture::check_document(registry, &fixture)? {
        return Err("generated document failed its unmodified control".into());
    }
    let edited = format!("{generated}\n<!-- one-defect witness -->\n");
    write_fixture(&fixture, architecture::DOCUMENT_PATH, &edited);
    if architecture::check_document(registry, &fixture)? {
        return Err("one-byte-class document mutation was accepted".into());
    }
    let violation = architecture::document_drift_violation();
    if violation.code == "document_drift" {
        Ok(())
    } else {
        Err(format!(
            "document drift constructor emitted {:?}",
            violation.code
        ))
    }
}

#[rustfmt::skip]
fn architecture_tree_witnesses() -> Vec<ArchitectureTreeWitness> {
    vec![
    ArchitectureTreeWitness { code: "bead_bet_label_unknown", fact: "one controlled Bead changes from b1 to the out-of-vocabulary b7", run: witness_unknown_bet_label },
    ArchitectureTreeWitness { code: "bead_override_shadowed", fact: "one exact-override Bead gains a higher-precedence b1 label", run: witness_shadowed_override },
    ArchitectureTreeWitness { code: "bead_family_ambiguous", fact: "one controlled Bead matches two prefix rules instead of one", run: witness_ambiguous_family },
    ArchitectureTreeWitness { code: "bead_provenance_not_total", fact: "one controlled Bead loses its only provenance mechanism", run: witness_provenance_not_total },
    ArchitectureTreeWitness { code: "document_drift", fact: "one edit is made to the generated ADR bytes", run: witness_document_drift },
    ]
}

#[test]
fn architecture_registry_claims_are_seen_to_fire() {
    let root = repo_root();
    let base = architecture::load_from_repo(&root).expect("architecture registry loads");
    let mut silent = Vec::new();
    for row in architecture_registry_witnesses() {
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        if let Err(error) =
            require_architecture_code_increase(row.code, &base, &root, &mutated, &root)
        {
            silent.push(format!("{} [{}] -> {error}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{} architecture registry claims did not fire on their one-defect mutations:\n{}",
        silent.len(),
        silent.join("\n")
    );
}

#[test]
fn architecture_tree_claims_are_seen_to_fire() {
    let root = repo_root();
    let registry = architecture::load_from_repo(&root).expect("architecture registry loads");
    let mut silent = Vec::new();
    for row in architecture_tree_witnesses() {
        if let Err(error) = (row.run)(&registry, &root) {
            silent.push(format!("{} [{}] -> {error}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{} architecture tree claims did not fire on their one-defect mutations:\n{}",
        silent.len(),
        silent.join("\n")
    );
}

#[test]
fn architecture_document_citations_equal_the_executed_witness_set() {
    let cited: BTreeSet<&str> = architecture::DOCUMENT_ENFORCEMENT_VIOLATION_CODES
        .into_iter()
        .chain(architecture::DOCUMENT_FAILURE_VIOLATION_CODES)
        .collect();
    let witnessed: BTreeSet<&str> = architecture_registry_witnesses()
        .iter()
        .map(|row| row.code)
        .chain(architecture_tree_witnesses().iter().map(|row| row.code))
        .collect();
    assert_eq!(
        cited, witnessed,
        "every ADR citation must have one executed witness and every witness must be cited"
    );

    let root = repo_root();
    let registry = architecture::load_from_repo(&root).expect("architecture registry loads");
    let document = architecture::generate_document(&registry, &root).expect("document generates");
    for code in cited {
        assert!(
            document.contains(&format!("`{code}`")),
            "generated ADR does not cite witnessed code {code:?}"
        );
    }
}

#[test]
fn missing_appendix_projection_is_seen_to_fire() {
    let catalog = appendix_catalog();
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `projection_read` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = appendix_a::verify_projections(&repo_root(), &catalog);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "projection_read"),
        "projection_read is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
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
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `catalog_target_identity_mismatch` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = appendix_a::validate_catalog(&catalog);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "catalog_target_identity_mismatch"),
        "catalog_target_identity_mismatch is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
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
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `catalog_projection_unexpected` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = appendix_a::validate_catalog(&catalog);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "catalog_projection_unexpected"),
        "catalog_projection_unexpected is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
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

// ===========================================================================
// Appendix A top-level candidate metadata — `appendix_a::validate_catalog`
// ===========================================================================

/// One Appendix A law reached by changing one top-level-candidate fact.
///
/// This is deliberately a table: the real catalog is loaded once, every row
/// receives a fresh clone, and the exact code must be added by that row's
/// mutation. A code already present in the control is reported as UNRUN rather
/// than being allowed to masquerade as a witness.
struct AppendixCandidateWitness {
    code: &'static str,
    fact: &'static str,
    mutate: fn(&mut appendix_a::Catalog),
}

fn unprojected_candidate_mut(
    catalog: &mut appendix_a::Catalog,
) -> &mut appendix_a::TopLevelCandidate {
    let projected: BTreeSet<&str> = catalog
        .projection_rows
        .iter()
        .map(|row| row.canonical_symbol.as_str())
        .collect();
    catalog
        .top_level_candidates
        .iter_mut()
        .find(|row| {
            row.identity_class == "unclassified" && !projected.contains(row.symbol.as_str())
        })
        .expect("catalog has an unprojected unclassified candidate")
}

#[rustfmt::skip]
fn appendix_candidate_witnesses() -> Vec<AppendixCandidateWitness> {
    vec![
    AppendixCandidateWitness { code: "catalog_candidate_kind_invalid", fact: "a candidate's source_kind leaves the closed vocabulary", mutate: |c| c.top_level_candidates[0].source_kind = "invented".into() },
    AppendixCandidateWitness { code: "catalog_candidate_symbol_invalid", fact: "a candidate symbol stops being one source name", mutate: |c| c.top_level_candidates[0].symbol = "not a source symbol".into() },
    AppendixCandidateWitness { code: "catalog_candidate_class_invalid", fact: "a candidate's identity_class leaves the closed vocabulary", mutate: |c| c.top_level_candidates[0].identity_class = "invented".into() },
    AppendixCandidateWitness { code: "catalog_candidate_class_mismatch", fact: "a projected wire candidate is relabelled logical", mutate: |c| c.top_level_candidates.iter_mut().find(|row| row.symbol == "AbandonedRestoreTerminalPinBasisRef").expect("wire candidate exists").identity_class = "logical".into() },
    AppendixCandidateWitness { code: "catalog_candidate_class_conflict", fact: "one symbol is projected into both wire and logical registries", mutate: |c| { let symbol = c.top_level_candidates.iter().find(|row| row.symbol == "AbandonedRestoreTerminalPinBasisRef").expect("wire candidate exists").symbol.clone(); c.projection_rows.iter_mut().find(|row| row.projection == "logical_object_kinds").expect("logical projection exists").canonical_symbol = symbol; } },
    AppendixCandidateWitness { code: "catalog_candidate_class_unproved", fact: "an unprojected candidate claims a durable identity class", mutate: |c| unprojected_candidate_mut(c).identity_class = "wire".into() },
    AppendixCandidateWitness { code: "catalog_candidate_source_key_invalid", fact: "a candidate source_key stops deriving from its symbol and generic signature", mutate: |c| c.top_level_candidates[0].source_key.push_str("-wrong") },
    AppendixCandidateWitness { code: "catalog_candidate_duplicate", fact: "one top-level source candidate is repeated", mutate: |c| { let duplicate = c.top_level_candidates[0].clone(); c.top_level_candidates.push(duplicate); } },
    AppendixCandidateWitness { code: "slice_census_pin_invalid", fact: "a slice's field-candidate count becomes negative", mutate: |c| c.slices[0].field_candidate_count = -1 },
    ]
}

#[test]
fn appendix_candidate_metadata_laws_are_seen_to_fire() {
    let base = appendix_catalog();
    let control: BTreeSet<String> = appendix_a::validate_catalog(&base)
        .into_iter()
        .map(|violation| violation.code)
        .collect();

    let mut silent = Vec::new();
    let mut unrun = Vec::new();
    for row in appendix_candidate_witnesses() {
        if control.contains(row.code) {
            unrun.push(format!("{} [{}]", row.code, row.fact));
            continue;
        }
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        let codes: BTreeSet<String> = appendix_a::validate_catalog(&mutated)
            .into_iter()
            .map(|violation| violation.code)
            .collect();
        if !codes.contains(row.code) {
            silent.push(format!("{} [{}] -> {codes:?}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{} Appendix A candidate-metadata laws did not fire on the fact that violates them:\n{}",
        silent.len(),
        silent.join("\n")
    );
    assert!(
        unrun.is_empty(),
        "UNRUN: {} Appendix A witness row(s) were not exercised because their code is already \
         present in the baseline, so mutating for them would prove nothing. UNRUN is neither pass \
         nor fail:\n{}",
        unrun.len(),
        unrun.join("\n")
    );
}

#[test]
fn workspace_unsafe_lint_drift_is_seen_to_fire() {
    let root = repo_root();
    let mut registry = topology::load_from_repo(&root).expect("unmodified topology registry loads");
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `workspace_unsafe_lint_drift` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = topology::validate_topology(&registry, &root);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "workspace_unsafe_lint_drift"),
        "workspace_unsafe_lint_drift is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
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
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `identity_wire_tag_collision` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = threat::validate_threat(&registry, &root);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "identity_wire_tag_collision"),
        "identity_wire_tag_collision is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
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

// ===========================================================================
// workspace_topology.toml — `topology::validate_topology`
// ===========================================================================

/// One law, one fact, one expected code.
struct TopologyWitness {
    code: &'static str,
    /// The single load-bearing fact the mutation changes, in prose. It appears
    /// in the failure message, so a law that stops firing names what was done
    /// to it rather than only what was expected.
    fact: &'static str,
    mutate: fn(&mut TopologyRegistry),
}

/// One live-tree law whose one-defect fixture mutates the scan rather than the
/// registry. These rows prove the filesystem facts independently: relabelling
/// an ordinary crate as an island makes both island laws fire at once and is
/// therefore not a witness for either individual branch.
struct LiveTreeWitness {
    code: &'static str,
    fact: &'static str,
    mutate: fn(&mut WorkspaceScan),
}

fn crate_row_mut<'a>(r: &'a mut TopologyRegistry, name: &str) -> &'a mut topology::CrateRow {
    r.crates
        .iter_mut()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("crate row {name:?} exists"))
}

fn capability_mut<'a>(r: &'a mut TopologyRegistry, id: &str) -> &'a mut topology::Capability {
    r.capabilities
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("capability {id:?} exists"))
}

fn project_mut<'a>(r: &'a mut TopologyRegistry, id: &str) -> &'a mut topology::FoundationProject {
    r.foundation_projects
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("foundation project {id:?} exists"))
}

fn layer_mut<'a>(r: &'a mut TopologyRegistry, id: &str) -> &'a mut topology::Layer {
    r.layers
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("layer {id:?} exists"))
}

fn source_block_mut<'a>(r: &'a mut TopologyRegistry, id: &str) -> &'a mut topology::SourceBlock {
    r.source_blocks
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("source block {id:?} exists"))
}

fn scanned_crate_mut<'a>(
    scan: &'a mut WorkspaceScan,
    name: &str,
) -> &'a mut topology::ScannedCrate {
    scan.crates
        .iter_mut()
        .find(|row| row.package_name == name)
        .expect("scanned crate exists")
}

#[rustfmt::skip]
fn topology_header_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "registry_name", fact: "registry.name is renamed", mutate: |r| r.registry.name = "not_workspace_topology".into() },
    TopologyWitness { code: "replay_command_drift", fact: "registry.replay_command is rewritten", mutate: |r| r.registry.replay_command = "cargo run -p registry-check --bin something-else".into() },
    TopologyWitness { code: "id_table_hash_drift", fact: "registry.id_table_hash is repinned", mutate: |r| r.registry.id_table_hash = "0000000000000000".into() },
    TopologyWitness { code: "embedded_block_unresolved", fact: "embedded_source_blocks names a block that does not exist", mutate: |r| r.registry.embedded_source_blocks.push("plan-no-such-block-v1".into()) },
    TopologyWitness { code: "required_edge_floor_vacuous", fact: "the monotone live-edge floor is emptied", mutate: |r| r.registry.required_dependency_live_floor.clear() },
    TopologyWitness { code: "required_edge_floor_regression", fact: "the live-edge floor is repointed at an edge that is still deferred", mutate: |r| r.registry.required_dependency_live_floor = vec!["txn-over-chronicle".into()] },
    TopologyWitness { code: "toolchain_channel_drift", fact: "registry.toolchain_channel is repinned", mutate: |r| r.registry.toolchain_channel = "nightly-1999-01-01".into() },
    TopologyWitness { code: "layer_order_not_dense", fact: "one layer's source_order leaves the dense range", mutate: |r| layer_mut(r, "foundation").source_order = 99 },
    TopologyWitness { code: "layer_edge_unresolved", fact: "allowed_outgoing_layers names an unknown layer", mutate: |r| layer_mut(r, "chronicle").allowed_outgoing_layers.push("no_such_layer".into()) },
    TopologyWitness { code: "reciprocal_pair_drift", fact: "layer_law.reciprocal_pair is repointed", mutate: |r| r.layer_law.reciprocal_pair = vec!["chronicle".into(), "strata".into()] },
    TopologyWitness { code: "layer_position_not_dense", fact: "one crate's layer_position leaves the dense range", mutate: |r| crate_row_mut(r, "fgdb-types").layer_position = 99 },
    TopologyWitness { code: "crate_duplicate", fact: "a crate row is repeated", mutate: |r| { let duplicate = r.crates[0].clone(); r.crates.push(duplicate); } },
    ]
}

#[rustfmt::skip]
fn topology_crate_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "crate_layer_unresolved", fact: "a crate row names an unknown layer", mutate: |r| crate_row_mut(r, "fgdb-types").layer = "no_such_layer".into() },
    TopologyWitness { code: "active_without_owner_bead", fact: "an active crate loses its owner bead", mutate: |r| crate_row_mut(r, "fgdb-types").owner_bead.clear() },
    TopologyWitness { code: "island_layer_drift", fact: "an unsafe island is moved off the unsafe_islands layer", mutate: |r| crate_row_mut(r, "fgdb-unsafe-simd").layer = "foundation".into() },
    TopologyWitness { code: "entry_crate_unclaimed", fact: "a posture's entry_crate is repointed, orphaning an entry_ row", mutate: |r| r.postures[0].entry_crate = "fgdb-cli".into() },
    TopologyWitness { code: "posture_entry_unresolved", fact: "a posture names an unregistered entry crate", mutate: |r| r.postures[0].entry_crate = "no-such-crate".into() },
    TopologyWitness { code: "design_only_pinned", fact: "the design-only donor gains a pinned revision", mutate: |r| project_mut(r, "frankensqlite").pinned_rev = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into() },
    TopologyWitness { code: "linkable_without_pin", fact: "a linked foundation project loses its pinned revision", mutate: |r| project_mut(r, "asupersync").pinned_rev.clear() },
    TopologyWitness { code: "foundation_without_prefix", fact: "a foundation project loses its package prefixes", mutate: |r| project_mut(r, "asupersync").package_prefixes.clear() },
    TopologyWitness { code: "foundation_source_endpoint", fact: "a required edge is sourced from a foundation project", mutate: |r| r.required_dependencies[0].from_kind = "foundation".into() },
    TopologyWitness { code: "required_edge_endpoint_unresolved", fact: "a required edge names an unresolvable source", mutate: |r| r.required_dependencies[0].from = "no-such-layer".into() },
    ]
}

#[rustfmt::skip]
fn topology_capability_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "capability_owner_unresolved", fact: "a capability names an unregistered owner crate", mutate: |r| capability_mut(r, "compression-codecs").owner_crate = "no-such-crate".into() },
    TopologyWitness { code: "build_here_with_foundation", fact: "a build_here capability names a foundation project", mutate: |r| capability_mut(r, "compression-codecs").foundation_project = "asupersync".into() },
    TopologyWitness { code: "consume_from_unresolved", fact: "a consume_from capability names an unregistered project", mutate: |r| capability_mut(r, "async-runtime").foundation_project = "no-such-project".into() },
    TopologyWitness { code: "consume_from_design_only", fact: "a consume_from capability is sourced from the design-only donor", mutate: |r| capability_mut(r, "async-runtime").foundation_project = "frankensqlite".into() },
    TopologyWitness { code: "consume_from_without_asset", fact: "a consume_from capability loses its asset evidence", mutate: |r| capability_mut(r, "async-runtime").foundation_asset.clear() },
    TopologyWitness { code: "design_only_wrong_project", fact: "a design_only capability is sourced from a linked project", mutate: |r| capability_mut(r, "design-ssi").foundation_project = "asupersync".into() },
    TopologyWitness { code: "design_only_with_asset", fact: "a design_only capability names a consumed asset", mutate: |r| capability_mut(r, "design-ssi").foundation_asset = "some asset".into() },
    TopologyWitness { code: "foundation_asset_unresolved", fact: "a consume_from asset phrase stops resolving in the asset block", mutate: |r| capability_mut(r, "async-runtime").foundation_asset = "an asset phrase that appears nowhere".into() },
    TopologyWitness { code: "asset_gap_unresolved", fact: "an evidence gap names an unregistered capability", mutate: |r| r.asset_evidence_gaps[0].capability_id = "no-such-capability".into() },
    TopologyWitness { code: "asset_gap_wrong_disposition", fact: "an evidence gap is moved onto a build_here capability", mutate: |r| r.asset_evidence_gaps[0].capability_id = "compression-codecs".into() },
    TopologyWitness { code: "asset_gap_with_asset", fact: "the gapped capability gains an asset row", mutate: |r| capability_mut(r, "queue-clients").foundation_asset = "some asset".into() },
    TopologyWitness { code: "asset_gap_block_unresolved", fact: "an evidence gap names an absent source block", mutate: |r| r.asset_evidence_gaps[0].verified_absent_from = "plan-no-such-block-v1".into() },
    ]
}

#[rustfmt::skip]
fn topology_derivation_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "narrowing_crate_unresolved", fact: "a narrowing names an unregistered crate", mutate: |r| r.dependency_narrowings[0].crate_name = "no-such-crate".into() },
    TopologyWitness { code: "narrowing_layer_unresolved", fact: "a narrowing allows an unknown layer", mutate: |r| r.dependency_narrowings[0].allowed_layers.push("no_such_layer".into()) },
    TopologyWitness { code: "narrowing_exception_unresolved", fact: "a narrowing allows an unregistered crate", mutate: |r| r.dependency_narrowings[0].allowed_crates.push("no-such-crate".into()) },
    TopologyWitness { code: "source_block_missing", fact: "the §18.1 crate-layer source block is removed", mutate: |r| r.source_blocks.retain(|block| block.id != "plan-crate-layer-table-v1") },
    TopologyWitness { code: "layer_table_unparsable", fact: "the §18.1 block is repointed at prose", mutate: |r| { let block = source_block_mut(r, "plan-crate-layer-table-v1"); block.plan_start_line = 1; block.plan_end_line = 1; } },
    TopologyWitness { code: "layer_row_count_drift", fact: "the §18.1 block is truncated to fewer rows", mutate: |r| source_block_mut(r, "plan-crate-layer-table-v1").plan_end_line = 1290 },
    TopologyWitness { code: "layer_title_position_drift", fact: "a layer title stops matching its §18.1 row", mutate: |r| layer_mut(r, "foundation").title = "Not Foundation".into() },
    TopologyWitness { code: "workstream_title_drift", fact: "a workstream title stops matching §19", mutate: |r| r.owner_scopes.iter_mut().find(|scope| scope.id == "W1").expect("W1 is registered").title = "Not Bedrock".into() },
    TopologyWitness { code: "design_row_count_drift", fact: "one design_only capability is dropped", mutate: |r| { let id = r.capabilities.iter().rev().find(|row| row.disposition == "design_only").expect("a design_only row exists").id.clone(); r.capabilities.retain(|row| row.id != id); } },
    TopologyWitness { code: "design_phrase_unresolved", fact: "a design capability's source phrase matches no donor row", mutate: |r| capability_mut(r, "design-ssi").source_phrase = "a phrase no donor row contains".into() },
    TopologyWitness { code: "design_phrase_ambiguous", fact: "a design capability's source phrase matches every donor row", mutate: |r| capability_mut(r, "design-ssi").source_phrase = String::new() },
    TopologyWitness { code: "island_roster_unreadable", fact: "the island roster path points at a file that does not exist", mutate: |r| r.registry.unsafe_ledger_registry = "registries/no-such-ledger.toml".into() },
    TopologyWitness { code: "island_roster_unparsable", fact: "the island roster path points at a file that is not TOML", mutate: |r| r.registry.unsafe_ledger_registry = "AGENTS.md".into() },
    TopologyWitness { code: "inventory_coverage_incomplete", fact: "the §18.2 build-inventory source block points to a prose line with residue outside the alphabet", mutate: |r| { let block = source_block_mut(r, "plan-build-inventory-v1"); block.plan_start_line = 1; block.plan_end_line = 1; } },
    ]
}

#[rustfmt::skip]
fn topology_live_tree_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "active_not_a_member", fact: "an active row's manifest_dir is repointed off the workspace", mutate: |r| crate_row_mut(r, "fgdb-types").manifest_dir = "crates/not-a-member".into() },
    TopologyWitness { code: "active_manifest_missing", fact: "an active row's manifest_dir is repointed at an absent manifest", mutate: |r| crate_row_mut(r, "fgdb-types").manifest_dir = "crates/not-a-member".into() },
    TopologyWitness { code: "package_name_drift", fact: "an active row is renamed away from its manifest package", mutate: |r| crate_row_mut(r, "fgdb-types").name = "fgdb-types-renamed".into() },
    TopologyWitness { code: "internal_dependency_not_path", fact: "a reserved crate row is renamed onto a git-sourced package", mutate: |r| crate_row_mut(r, "fgdb-shard").name = "asupersync".into() },
    TopologyWitness { code: "dependency_on_inactive_crate", fact: "a depended-on crate row is demoted to planned", mutate: |r| crate_row_mut(r, "fgdb-types").activation_status = "planned".into() },
    TopologyWitness { code: "forbidden_dependency", fact: "a forbidden package prefix is repointed at a linked foundation package", mutate: |r| r.forbidden_dependencies[0].package_prefix = "asupersync".into() },
    TopologyWitness { code: "ordinary_crate_relaxes_unsafe", fact: "an unsafe island is relabelled forbid, exposing its ledgered allow sites to the ordinary-crate live-root law", mutate: |r| crate_row_mut(r, "fgdb-unsafe-simd").unsafe_policy = "forbid".into() },
    TopologyWitness { code: "design_only_linked", fact: "a linked foundation project is relabelled design_only", mutate: |r| project_mut(r, "asupersync").linkage = "design_only".into() },
    TopologyWitness { code: "foundation_source_drift", fact: "a foundation project's git_url is repointed", mutate: |r| project_mut(r, "asupersync").git_url = "https://example.invalid/elsewhere".into() },
    TopologyWitness { code: "default_feature_escape", fact: "franken_networkx is changed to require disabled defaults while fgdb-codec consumes fnx-generators with defaults", mutate: |r| project_mut(r, "franken_networkx").default_features_must_be_disabled = true },
    ]
}

#[rustfmt::skip]
fn unsafe_island_live_tree_witnesses() -> Vec<LiveTreeWitness> {
    vec![
    LiveTreeWitness { code: "island_inherits_forbid", fact: "an unsafe island grows [lints] workspace = true", mutate: |scan| scanned_crate_mut(scan, "fgdb-unsafe-simd").lints_workspace = true },
    LiveTreeWitness { code: "island_root_missing_deny", fact: "an unsafe island loses #![deny(unsafe_code)] at its crate root", mutate: |scan| scanned_crate_mut(scan, "fgdb-unsafe-simd").root_denies_unsafe = false },
    LiveTreeWitness { code: "lints_not_inherited", fact: "an active crate loses [lints] workspace = true", mutate: |scan| scanned_crate_mut(scan, "fgdb-types").lints_workspace = false },
    LiveTreeWitness { code: "root_missing_forbid", fact: "an ordinary crate root loses #![forbid(unsafe_code)]", mutate: |scan| scanned_crate_mut(scan, "fgdb-types").root_forbids_unsafe = false },
    LiveTreeWitness { code: "external_dependency", fact: "an active crate adds a dependency outside the closed universe", mutate: |scan| scanned_crate_mut(scan, "fgdb-types").dependencies.push(topology::ManifestDependency { key: "serde".into(), package: "serde".into(), table: "dependencies".into(), path: String::new(), git: String::new(), rev: String::new(), default_features_disabled: false }) },
    ]
}

#[rustfmt::skip]
fn topology_live_graph_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "layer_inversion", fact: "the foundation layer stops allowing its own outgoing edges", mutate: |r| layer_mut(r, "foundation").allowed_outgoing_layers.clear() },
    TopologyWitness { code: "narrowing_violated", fact: "the narrowing row is repointed at a live crate it does not admit", mutate: |r| r.dependency_narrowings[0].crate_name = "fgdb-calibrate".into() },
    TopologyWitness { code: "required_edge_missing", fact: "a required edge is repointed at a project its source does not depend on", mutate: |r| r.required_dependencies.iter_mut().find(|edge| edge.id == "calibrate-over-asupersync").expect("the calibrate edge is registered").to = "franken_networkx".into() },
    // NOT `postures[0]`, and the reason is a landed regression rather than
    // taste. `postures[0]` is `embedded`, and fgdb-j0vu activated its entry
    // crate, so the mutation became `live` -> `live`: a NO-OP, and the witness
    // went silent while the law it witnesses was working perfectly. Selecting
    // the first STILL-DEFERRED posture keeps the fact a real fact across future
    // activations. The law is not weakened — the mutated fact is still exactly
    // "a posture whose entry crate is absent from the workspace declares itself
    // live", which is the same half of `posture_status_drift` this row always
    // covered. When the LAST posture activates this mutation becomes a no-op
    // again, and the harness reds with `posture_status_drift [...] -> {}` rather
    // than passing — the correct direction to fail, and the message names itself.
    TopologyWitness { code: "posture_status_drift", fact: "a posture whose entry crate is absent from the workspace is declared live", mutate: |r| {
        let deferred = r
            .postures
            .iter_mut()
            .find(|posture| posture.status == "deferred")
            .expect("at least one posture must still be deferred for this witness to be plantable");
        deferred.status = "live".into();
    } },
    TopologyWitness { code: "tooling_dependency", fact: "a crate that has dependencies is registered as G0 tooling", mutate: |r| r.registry.tooling_members.push("crates/fgdb-calibrate".into()) },
    // The only two-fact row in the file, and it is two because the law cannot
    // be reached with one: with the whole composition layer planned, every
    // posture closure is `deferred` and holds nothing. Making a posture live on
    // an active crate is one fact; putting a forbidden participation inside its
    // closure is the other. Both are stated here rather than hidden behind a
    // single prose label.
    TopologyWitness { code: "posture_closure_violation", fact: "a posture is made live on a crate whose closure holds a test-only crate", mutate: |r| { r.postures[0].entry_crate = "fgdb-calibrate".into(); crate_row_mut(r, "fgdb-types").posture_participation = "test_only".into(); } },
    ]
}

fn topology_codes(registry: &TopologyRegistry, root: &Path) -> BTreeSet<String> {
    topology::validate_topology(registry, root)
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

fn run_topology_witnesses(rows: Vec<TopologyWitness>) {
    let root = repo_root();
    let base = topology::load_from_repo(&root).expect("unmodified topology registry loads");
    let control = topology_codes(&base, &root);
    assert!(
        control.is_empty(),
        "the control must be clean, or no row below is a witness: {control:?}"
    );

    let mut silent: Vec<String> = Vec::new();
    for row in rows {
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        let codes = topology_codes(&mutated, &root);
        if !codes.contains(row.code) {
            silent.push(format!("{} [{}] -> {codes:?}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{} topology laws did not fire on the fact that violates them:\n{}",
        silent.len(),
        silent.join("\n")
    );
}

fn run_unsafe_island_live_tree_witnesses(rows: Vec<LiveTreeWitness>) {
    let root = repo_root();
    let registry = topology::load_from_repo(&root).expect("unmodified topology registry loads");
    let base_scan = topology::scan_workspace(&root).expect("unmodified workspace scans");
    let control: BTreeSet<String> = topology::live_tree_violations(&registry, &root, &base_scan)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert!(
        control.is_empty(),
        "the live-tree control must be clean, or no scan mutation below is a witness: {control:?}"
    );

    let mut failures = Vec::new();
    for row in rows {
        let mut mutated = base_scan.clone();
        (row.mutate)(&mut mutated);
        let codes: BTreeSet<String> = topology::live_tree_violations(&registry, &root, &mutated)
            .into_iter()
            .map(|violation| violation.code)
            .collect();
        let expected = BTreeSet::from([row.code.to_owned()]);
        if codes != expected {
            failures.push(format!(
                "{} [{}] -> expected {expected:?}, got {codes:?}",
                row.code, row.fact
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} unsafe-island laws were not isolated by their one-defect scan mutation:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn topology_header_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_header_witnesses());
}

#[test]
fn topology_crate_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_crate_witnesses());
}

#[test]
fn topology_capability_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_capability_witnesses());
}

#[test]
fn topology_derivation_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_derivation_witnesses());
}

#[test]
fn topology_live_tree_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_live_tree_witnesses());
}

#[test]
fn unsafe_island_live_tree_laws_are_seen_to_fire_independently() {
    run_unsafe_island_live_tree_witnesses(unsafe_island_live_tree_witnesses());
}

#[test]
fn topology_live_graph_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_live_graph_witnesses());
}

/// The one topology law whose subject is the root itself rather than a registry
/// row: a workspace that cannot be scanned must fail, never be skipped.
#[test]
fn topology_workspace_scan_failure_is_seen_to_fire() {
    let root = repo_root();
    let base = topology::load_from_repo(&root).expect("unmodified topology registry loads");
    assert!(
        topology_codes(&base, &root).is_empty(),
        "the control must be clean"
    );

    let absent = std::env::temp_dir().join(format!(
        "fgdb-xnxy1-uncreated-topology-root-{}",
        std::process::id()
    ));
    assert!(
        !absent.exists(),
        "the mutation root must be absent so the scan failure is deliberate"
    );
    let codes = topology_codes(&base, &absent);
    assert!(
        codes.contains("workspace_scan_failed"),
        "expected workspace_scan_failed, got {codes:?}"
    );
}

// ===========================================================================
// threat_model.toml — `threat::validate_threat`
// ===========================================================================

struct ThreatWitness {
    code: &'static str,
    fact: &'static str,
    mutate: fn(&mut ThreatRegistry),
}

fn identity_mut<'a>(r: &'a mut ThreatRegistry, name: &str) -> &'a mut threat::Identity {
    r.identities
        .iter_mut()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("identity {name:?} exists"))
}

fn dimension_mut<'a>(r: &'a mut ThreatRegistry, id: &str) -> &'a mut threat::AuthorityDimension {
    r.authority_dimensions
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("authority dimension {id:?} exists"))
}

fn threat_posture_mut<'a>(r: &'a mut ThreatRegistry, id: &str) -> &'a mut threat::Posture {
    r.postures
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("posture {id:?} exists"))
}

#[rustfmt::skip]
fn threat_witnesses() -> Vec<ThreatWitness> {
    vec![
    // header and source blocks
    ThreatWitness { code: "document_path", fact: "registry.document_path is repointed", mutate: |r| r.registry.document_path = "docs/SOMEWHERE_ELSE.md".into() },
    ThreatWitness { code: "source_block_path", fact: "a source block's plan_path is repointed off the plan", mutate: |r| r.source_blocks[0].plan_path = "AGENTS.md".into() },
    ThreatWitness { code: "source_block_unreadable", fact: "a source block's line range runs past the end of the plan", mutate: |r| { r.source_blocks[0].plan_start_line = 10_000_000; r.source_blocks[0].plan_end_line = 10_000_001; } },
    // actors, assets, assumptions, registered rejections
    ThreatWitness { code: "actor_unsummarized", fact: "an actor loses its summary", mutate: |r| r.actors[0].summary.clear() },
    ThreatWitness { code: "asset_unbound_claim", fact: "an asset loses its primary claim reference", mutate: |r| r.assets[0].primary_claim_ref.clear() },
    ThreatWitness { code: "assumption_unbounded", fact: "an assumption loses its bounds", mutate: |r| r.assumptions[0].bounds.clear() },
    ThreatWitness { code: "out_of_scope_unreasoned", fact: "a registered rejection loses its rationale", mutate: |r| r.out_of_scope[0].rationale.clear() },
    ThreatWitness { code: "ordinal_gap", fact: "an actor's source_order leaves the dense range", mutate: |r| r.actors[0].source_order = 99 },
    // exposure matrix
    ThreatWitness { code: "exposure_malformed", fact: "an exposure entry loses its asset:assumption shape", mutate: |r| r.actors[0].defended_assets[0] = "no-binding-separator".into() },
    ThreatWitness { code: "exposure_unknown_asset", fact: "an exposure entry names an unregistered asset", mutate: |r| r.actors[0].defended_assets[0] = "no_such_asset:A-PROCESS-TRUSTED".into() },
    ThreatWitness { code: "exposure_duplicated", fact: "one asset is dispositioned twice for the same actor", mutate: |r| { let duplicate = r.actors[0].defended_assets[0].clone(); r.actors[0].conditional_assets.push(duplicate); } },
    // identity and epoch lattice
    ThreatWitness { code: "epoch_domains_unify", fact: "the adaptive epoch is moved into the security domain", mutate: |r| identity_mut(r, "DecisionPolicyEpoch").epoch_domain = "security".into() },
    ThreatWitness { code: "epoch_identity_absent", fact: "the security epoch identity is renamed away", mutate: |r| identity_mut(r, "SecurityPolicyEpoch").name = "RetiredEpoch".into() },
    // presentation bindings
    ThreatWitness { code: "binding_rank_not_dense", fact: "a presentation binding's rank leaves the dense range", mutate: |r| r.presentation_bindings[0].rank = 9 },
    ThreatWitness { code: "binding_transition_duplicated", fact: "a transition cell is declared twice", mutate: |r| { let duplicate = r.binding_transitions[0].clone(); r.binding_transitions.push(duplicate); } },
    ThreatWitness { code: "binding_transition_law", fact: "a transition's declared law contradicts the rank order", mutate: |r| r.binding_transitions[0].law = "weakened_binding".into() },
    ThreatWitness { code: "binding_transition_missing", fact: "one cell of the transition matrix is dropped", mutate: |r| { r.binding_transitions.pop(); } },
    ThreatWitness { code: "binding_transition_unknown_class", fact: "a transition names an unregistered presentation binding", mutate: |r| r.binding_transitions[0].from = "NoSuchBinding".into() },
    // attenuation laws
    ThreatWitness { code: "attenuation_law_ungoverned", fact: "an attenuation law governs no authority dimension", mutate: |r| r.attenuation_laws[0].dimension_ids.clear() },
    ThreatWitness { code: "attenuation_law_no_fixture", fact: "an attenuation law names no negative fixture", mutate: |r| r.attenuation_laws[0].negative_fixture.clear() },
    ThreatWitness { code: "attenuation_law_one_sided", fact: "every prohibition is dropped, leaving only permissions", mutate: |r| r.attenuation_laws.retain(|law| law.class == "permitted") },
    ThreatWitness { code: "prohibition_operator_mismatch", fact: "a governed dimension's narrowing_operator stops matching its prohibition", mutate: |r| dimension_mut(r, "redaction_profile").narrowing_operator = "intersect".into() },
    // product space and postures
    ThreatWitness { code: "product_space_cell_count", fact: "the declared product-space cell count drifts", mutate: |r| r.product_space.cell_count += 1 },
    ThreatWitness { code: "posture_off_axis", fact: "a posture is placed off a declared axis", mutate: |r| r.postures[0].service_class = "NotOnAnyAxis".into() },
    ThreatWitness { code: "posture_duplicated", fact: "a posture id is declared twice", mutate: |r| { let duplicate = r.postures[0].clone(); r.postures.push(duplicate); } },
    ThreatWitness { code: "posture_empty_unjustified", fact: "an empty-footprint posture loses its justification", mutate: |r| threat_posture_mut(r, "local_directory_bound").empty_justification.clear() },
    ThreatWitness { code: "posture_complete_with_justification", fact: "a complete-footprint posture gains an empty justification", mutate: |r| threat_posture_mut(r, "local_external_cas").empty_justification = "not applicable here".into() },
    ThreatWitness { code: "posture_empty_with_rows", fact: "a posture carrying footprint rows is relabelled empty", mutate: |r| threat_posture_mut(r, "local_external_cas").footprint_declaration = "empty".into() },
    ThreatWitness { code: "exclusion_law_vacuous", fact: "an exclusion law's id stops resolving any excluded cell", mutate: |r| r.exclusion_laws[0].id = "PX-UNREFERENCED".into() },
    // deferred postures
    ThreatWitness { code: "deferred_posture_unowned", fact: "a deferred posture loses its owner bead", mutate: |r| r.deferred_postures[0].owner_bead.clear() },
    ThreatWitness { code: "deferred_posture_unexplained", fact: "a deferred posture loses its reason", mutate: |r| r.deferred_postures[0].reason.clear() },
    ThreatWitness { code: "deferred_posture_also_registered", fact: "a deferred posture takes a registered posture's id", mutate: |r| r.deferred_postures[0].id = "local_directory_bound".into() },
    // authority footprint
    ThreatWitness { code: "footprint_unknown_posture", fact: "a footprint row names an unregistered posture", mutate: |r| r.footprints[0].posture_id = "no_such_posture".into() },
    ThreatWitness { code: "footprint_unknown_authority", fact: "a footprint row names an unregistered authority", mutate: |r| r.footprints[0].authority_id = "no_such_authority".into() },
    ThreatWitness { code: "footprint_duplicated", fact: "a footprint cell is declared twice", mutate: |r| { let duplicate = r.footprints[0].clone(); r.footprints.push(duplicate); } },
    ThreatWitness { code: "footprint_empty_named_classes", fact: "a cell naming no operation class is relabelled named_in_source", mutate: |r| r.footprints[0].operation_class_basis = "named_in_source".into() },
    ThreatWitness { code: "footprint_unknown_operation_class", fact: "a named_in_source cell names a class outside the closed sixteen", mutate: |r| { let mut row = r.footprints[0].clone(); row.authority_id = "transparency_witness".into(); row.operation_class_basis = "named_in_source".into(); row.operation_classes = vec!["not_an_operation_class".into()]; r.footprints.push(row); } },
    ThreatWitness { code: "footprint_unowned_deferral", fact: "a trigger-site-only cell loses its deferred binding owner", mutate: |r| r.footprints[0].deferred_binding_owner.clear() },
    ThreatWitness { code: "footprint_position_absent", fact: "a synchronous-path cell loses its position", mutate: |r| r.footprints[0].sync_path_position.clear() },
    ThreatWitness { code: "footprint_zero_touches", fact: "a synchronous-path cell drops to zero touches", mutate: |r| r.footprints[0].touch_count = 0 },
    ]
}

#[test]
fn threat_model_laws_are_seen_to_fire() {
    let root = repo_root();
    let base = threat::load_from_repo(&root).expect("unmodified threat registry loads");
    let control: BTreeSet<String> = threat::validate_threat(&base, &root)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    // SCOPED PER ROW, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). One
    // global "the control must be clean" gated this ENTIRE witness table: a single
    // ambient violation anywhere disabled every row, including rows whose code was
    // nowhere near it. The baseline is now tolerated, and only the rows it actually
    // collides with are withheld -- and those are REPORTED as UNRUN rather than
    // silently skipped, because a witness that did not run must read as neither passed
    // nor failed (fgdb-1nqb, and the PASS/FAIL/RED/UNRUN vocabulary of
    // scripts/lib/gate_verdict.sh, fgdb-udco).

    let mut silent: Vec<String> = Vec::new();
    let mut unrun: Vec<String> = Vec::new();
    for row in threat_witnesses() {
        if control.contains(row.code) {
            unrun.push(format!("{} [{}]", row.code, row.fact));
            continue;
        }
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        let codes: BTreeSet<String> = threat::validate_threat(&mutated, &root)
            .into_iter()
            .map(|violation| violation.code)
            .collect();
        if !codes.contains(row.code) {
            silent.push(format!("{} [{}] -> {codes:?}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{} threat-model laws did not fire on the fact that violates them:\n{}",
        silent.len(),
        silent.join("\n")
    );
    assert!(
        unrun.is_empty(),
        "UNRUN: {} witness row(s) were not exercised because their code is already \
         present in the baseline, so mutating for them would prove nothing. UNRUN is \
         neither pass nor fail:\n{}",
        unrun.len(),
        unrun.join("\n")
    );
}

// ===========================================================================
// the claim registries — `validate::validate_all`
// ===========================================================================

struct RegistryWitness {
    code: &'static str,
    fact: &'static str,
    mutate: fn(&mut Registries),
}

#[rustfmt::skip]
fn registry_witnesses() -> Vec<RegistryWitness> {
    vec![
    RegistryWitness { code: "unregistered_justifier", fact: "a clause justifies itself by an unregistered row", mutate: |r| r.invariants.invariants[0].clauses[0].justified_by.push("no-such-registry-row".into()) },
    RegistryWitness { code: "script_undeclared", fact: "one on-disk script loses its disposition row", mutate: |r| { r.script_dispositions.pop(); } },
    RegistryWitness { code: "script_disposition_dangling", fact: "a disposition names a script that does not exist", mutate: |r| r.script_dispositions.push(ScriptDisposition { path: "scripts/no_such_script.sh".into(), role: "advisory".into(), reason: "witness fixture".into() }) },
    RegistryWitness { code: "script_disposition_conflict", fact: "a registered gate also declares a non-gate disposition", mutate: |r| { let artifact = r.checker_index.iter().map(|row| row.artifact.clone()).find(|artifact| artifact.starts_with("scripts/")).expect("a registered scripts/ artifact exists"); r.script_dispositions.push(ScriptDisposition { path: artifact, role: "advisory".into(), reason: "witness fixture".into() }); } },
    ]
}

fn registry_codes(registries: &Registries, root: &Path) -> BTreeSet<String> {
    validate::validate_all(registries, root)
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

#[test]
fn claim_registry_laws_are_seen_to_fire() {
    let root = repo_root();
    let base = model::load_registries(&root.join("registries")).expect("registries load");
    let control = registry_codes(&base, &root);
    // SCOPED PER ROW, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). One
    // global "the control must be clean" gated this ENTIRE witness table: a single
    // ambient violation anywhere disabled every row, including rows whose code was
    // nowhere near it. The baseline is now tolerated, and only the rows it actually
    // collides with are withheld -- and those are REPORTED as UNRUN rather than
    // silently skipped, because a witness that did not run must read as neither passed
    // nor failed (fgdb-1nqb, and the PASS/FAIL/RED/UNRUN vocabulary of
    // scripts/lib/gate_verdict.sh, fgdb-udco).

    let mut silent: Vec<String> = Vec::new();
    let mut unrun: Vec<String> = Vec::new();
    for row in registry_witnesses() {
        if control.contains(row.code) {
            unrun.push(format!("{} [{}]", row.code, row.fact));
            continue;
        }
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        let codes = registry_codes(&mutated, &root);
        if !codes.contains(row.code) {
            silent.push(format!("{} [{}] -> {codes:?}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{} claim-registry laws did not fire on the fact that violates them:\n{}",
        silent.len(),
        silent.join("\n")
    );
    assert!(
        unrun.is_empty(),
        "UNRUN: {} witness row(s) were not exercised because their code is already \
         present in the baseline, so mutating for them would prove nothing. UNRUN is \
         neither pass nor fail:\n{}",
        unrun.len(),
        unrun.join("\n")
    );
}

// --- the laws whose subject is the tree, not a registry row -----------------

fn scratch_root(tag: &str) -> PathBuf {
    // Process-scoped: a bare name lets a concurrent run of this binary delete
    // the fixture out from under this one, which fails a DIFFERENT test each
    // round and reads exactly like a real defect.
    let dir = std::env::temp_dir().join(format!("fgdb-xnxy1-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("fixture root");
    dir
}

fn write_fixture(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
    fs::write(&path, text).expect("fixture file");
}

const WITNESS_REFS: &str = "active_logical_object_kinds! {\n    Alpha = 0x0001 => \"Alpha\",\n}\n";
const WITNESS_KINDS: &str =
    "schema_version = 1\n\n[[kind]]\nobject_kind = 0x0001\nname = \"Alpha\"\nstatus = \"active\"\n";

#[test]
fn claim_registry_tree_laws_are_seen_to_fire() {
    let repo = repo_root();
    let base = model::load_registries(&repo.join("registries")).expect("registries load");
    assert!(
        registry_codes(&base, &repo).is_empty(),
        "the control must be clean, or no row below is a witness"
    );

    let mut silent: Vec<String> = Vec::new();
    let mut witness = |tag: &str, code: &str, build: &dyn Fn(&Path)| {
        let root = scratch_root(tag);
        build(&root);
        let codes = registry_codes(&base, &root);
        if !codes.contains(code) {
            silent.push(format!("{code} [{tag}] -> {codes:?}"));
        }
    };

    witness("scan-failed", "script_scan_failed", &|_| {});
    witness("scan-empty", "script_scan_empty", &|root| {
        fs::create_dir_all(root.join("scripts")).expect("empty scripts directory");
    });
    witness(
        "refs-unreadable",
        "active_logical_kind_source_unreadable",
        &|_| {},
    );
    witness(
        "macro-absent",
        "active_logical_kind_macro_absent",
        &|root| {
            write_fixture(
                root,
                "crates/fgdb-types/src/refs.rs",
                "// no active_logical_object_kinds! invocation here\n",
            );
            write_fixture(root, "registries/logical_object_kinds.toml", WITNESS_KINDS);
        },
    );
    witness(
        "arm-duplicate",
        "active_logical_kind_arm_duplicate",
        &|root| {
            write_fixture(
                root,
                "crates/fgdb-types/src/refs.rs",
                "active_logical_object_kinds! {\n    Alpha = 0x0001 => \"Alpha\",\n    Beta = 0x0001 => \"Beta\",\n}\n",
            );
            write_fixture(root, "registries/logical_object_kinds.toml", WITNESS_KINDS);
        },
    );
    witness(
        "kinds-unparseable",
        "active_logical_kind_registry_unparseable",
        &|root| {
            write_fixture(root, "crates/fgdb-types/src/refs.rs", WITNESS_REFS);
            write_fixture(
                root,
                "registries/logical_object_kinds.toml",
                "this is not = = toml\n",
            );
        },
    );
    witness(
        "row-unreadable",
        "active_logical_kind_row_unreadable",
        &|root| {
            write_fixture(root, "crates/fgdb-types/src/refs.rs", WITNESS_REFS);
            write_fixture(
                root,
                "registries/logical_object_kinds.toml",
                "schema_version = 1\n\n[[kind]]\nobject_kind = 0x0001\nstatus = \"active\"\n",
            );
        },
    );

    assert!(
        silent.is_empty(),
        "{} claim-registry tree laws did not fire on the fact that violates them:\n{}",
        silent.len(),
        silent.join("\n")
    );
}

// ===========================================================================
// The law that cannot fire, and its live-guard witness
// ===========================================================================

/// `checker_liveness_self_test_failed` is unreachable from any input.
///
/// `liveness::self_test()` takes no arguments: it asks the liveness readers a
/// fixed set of questions whose answers are compiled in, so no registry, no
/// manifest and no source tree can move its verdict. That is the point of it —
/// it is the control that licenses every clean liveness verdict below it — but
/// it means the code is emitted only by a reader regression, never by an input,
/// and an input-driven witness for it cannot exist.
///
/// What is witnessable is that the guard is a live predicate rather than a
/// constant: the same public readers give a DIFFERENT answer on perturbed text.
/// If this stops holding, `self_test().licensed()` has become true by
/// construction and the control it provides is worth nothing.
#[test]
fn the_liveness_self_test_guard_can_be_false() {
    let control = liveness::self_test();
    assert!(
        control.licensed(),
        "the shipped liveness readers must answer their own fixture correctly: {control:?}"
    );
    assert!(
        control.cases > 0,
        "a self-test over zero cases is licensed by construction and licenses nothing"
    );

    // The readers are input-sensitive, and specifically comment-aware, which is
    // what half the self-test's cases assert. `mask_shell` is the public reader
    // under the shell half of it: a reader that had stopped reading would return
    // its input unchanged and pass every "the comment was ignored" case for the
    // wrong reason.
    let commented = "#!/usr/bin/env bash\n# exit 1\nexit 0\n";
    let live = "#!/usr/bin/env bash\nexit 1\n";
    assert!(
        !liveness::mask_shell(commented).contains("exit 1"),
        "the shell reader must blank a commented exit"
    );
    assert!(
        liveness::mask_shell(live).contains("exit 1"),
        "the shell reader must leave live text in place"
    );
}

// ===========================================================================
// fgdb-tsfs: the Appendix A closure
// ===========================================================================
//
// The census behind `fgdb-tsfs` measured the Appendix A share of the
// never-witnessed laws at claim time (2026-08-25, worktree during the a06/atke
// mint program): 234 distinct codes in `src/appendix_a.rs`, of which 115 were
// production-reachable and named nowhere. The tables below close that set.
// Four runner shapes cover the four surfaces the emitters sit behind:
//
// * `CatalogLaw`   -- rows reached through `validate_catalog(&Catalog)`;
// * `RootLaw`      -- rows reached through a repo-root entry
//                     (`verify_projections`, `verify_repository_bindings`);
// * `TextLaw`      -- rows reached through catalog-text parsing
//                     (`parse_catalog` on munged real catalog text);
// * `SourceLaw`    -- rows reached through plan-source verification
//                     (`verify_source(&Catalog, &[u8])` over the real plan
//                     bytes and/or a mutated manifest).
//
// Every runner is differential, per row: the code must be ABSENT from the
// unmutated control and PRESENT after the mutation adds exactly one defect.
// A validator that silently returns nothing therefore cannot pass a witness,
// and a pre-existing violation elsewhere in the control cannot mask one.
//
// Laws no input can reach are declared CANNOT_FIRE in the bead comment trail
// with their reason; they deliberately have no row here.

fn appendix_catalog_text() -> String {
    fs::read_to_string(repo_root().join(appendix_a::CATALOG_PATH))
        .expect("Appendix A catalog text is readable")
}

fn appendix_plan_source() -> Vec<u8> {
    let catalog = appendix_catalog();
    fs::read(repo_root().join(&catalog.source_manifest.plan_path))
        .expect("plan source bytes are readable")
}

/// A law fired or silenced by mutating ONLY the parsed catalog.
struct CatalogLaw {
    code: &'static str,
    fact: &'static str,
    mutate: fn(&mut appendix_a::Catalog),
}

/// A law whose public owner also takes the repository root.
struct RootLaw {
    code: &'static str,
    fact: &'static str,
    entry: RootEntry,
    mutate: fn(&mut appendix_a::Catalog),
}

enum RootEntry {
    Bindings,
    /// The aggregate projection gate over the REAL repository: projections +
    /// generated-family unions + adjudication law allowlists.
    ProjectionDiff,
    /// The live type/owner/evidence closure (validate_catalog_metadata's own
    /// gate), which `validate_catalog` does not reach.
    Closure,
    /// The aggregate gate over a throwaway root, for fail-closed *_unavailable
    /// legs; per-row fixtures live in dedicated tests, not table rows.
    ProjectionDiffScratch,
}

impl RootEntry {
    fn run(&self, catalog: &appendix_a::Catalog) -> Vec<String> {
        let violations = match self {
            RootEntry::Bindings => appendix_a::verify_repository_bindings(&repo_root(), catalog),
            RootEntry::ProjectionDiff | RootEntry::ProjectionDiffScratch => {
                let root = match self {
                    RootEntry::ProjectionDiffScratch => scratch_root("law"),
                    _ => repo_root(),
                };
                appendix_a::appendix_a_catalog_projection_diff(&root, catalog)
            }
            RootEntry::Closure => appendix_a::appendix_a_catalog_closure(catalog),
        };
        violations
            .into_iter()
            .map(|violation| violation.code)
            .collect()
    }

    /// The control always reads the REAL repository, even for scratch-root
    /// entries: a law that already fires on the clean tree proves nothing.
    fn run_control(&self, catalog: &appendix_a::Catalog) -> Vec<String> {
        match self {
            RootEntry::ProjectionDiffScratch => {
                appendix_a::appendix_a_catalog_projection_diff(&repo_root(), catalog)
                    .into_iter()
                    .map(|violation| violation.code)
                    .collect()
            }
            other => other.run(catalog),
        }
    }
}

/// A law fired by feeding munged catalog TEXT to the parser.
struct TextLaw {
    code: &'static str,
    fact: &'static str,
    munge: fn(&str) -> String,
}

/// A law fired through `verify_source`: mutate the manifest pins, the source
/// bytes, or both.
struct SourceLaw {
    code: &'static str,
    fact: &'static str,
    mutate_catalog: Option<fn(&mut appendix_a::Catalog)>,
    mutate_bytes: fn(Vec<u8>) -> Vec<u8>,
}

fn run_catalog_laws(name: &str, laws: &[CatalogLaw]) {
    let base = appendix_catalog();
    let control: BTreeSet<String> = appendix_a::validate_catalog(&base)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    let mut silent = Vec::new();
    for row in laws {
        if control.contains(row.code) {
            let code = row.code;
            let fact = row.fact;
            panic!(
                "{name}: {code} [{fact}] is already present in the clean control, so its \
                 mutation proves nothing"
            );
        }
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        let fired: BTreeSet<String> = appendix_a::validate_catalog(&mutated)
            .into_iter()
            .map(|violation| violation.code)
            .collect();
        if !fired.contains(row.code) {
            silent.push(format!("{} [{}] -> {:?}", row.code, row.fact, fired));
        }
    }
    assert!(
        silent.is_empty(),
        "{name}: {} of {} laws stopped firing: {silent:?}",
        silent.len(),
        laws.len()
    );
}

fn run_root_laws(name: &str, laws: &[RootLaw]) {
    let base = appendix_catalog();
    let mut silent = Vec::new();
    for row in laws {
        if row
            .entry
            .run_control(&base)
            .iter()
            .any(|code| code == row.code)
        {
            let code = row.code;
            let fact = row.fact;
            panic!("{name}: {code} [{fact}] is already present without the mutation");
        }
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        if !row.entry.run(&mutated).iter().any(|code| code == row.code) {
            silent.push(format!("{} [{}]", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{name}: {} of {} laws stopped firing: {silent:?}",
        silent.len(),
        laws.len()
    );
}

fn run_text_laws(name: &str, laws: &[TextLaw]) {
    let base_text = appendix_catalog_text();
    let control_codes: Vec<String> = match appendix_a::parse_catalog(&base_text) {
        Ok(_) => Vec::new(),
        Err(violations) => violations.into_iter().map(|v| v.code).collect(),
    };
    let mut silent = Vec::new();
    for row in laws {
        if control_codes.iter().any(|code| code == row.code) {
            let code = row.code;
            let fact = row.fact;
            panic!("{name}: {code} [{fact}] fires on unmutated text");
        }
        let munged = (row.munge)(&base_text);
        let fired: Vec<String> = match appendix_a::parse_catalog(&munged) {
            Ok(_) => Vec::new(),
            Err(violations) => violations.into_iter().map(|v| v.code).collect(),
        };
        if !fired.iter().any(|code| code == row.code) {
            silent.push(format!("{} [{}] -> {fired:?}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{name}: {} of {} laws stopped firing: {silent:?}",
        silent.len(),
        laws.len()
    );
}

fn run_source_laws(name: &str, laws: &[SourceLaw]) {
    let base = appendix_catalog();
    let base_bytes = appendix_plan_source();
    let control: BTreeSet<String> = appendix_a::verify_source(&base, &base_bytes)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    let mut silent = Vec::new();
    for row in laws {
        if control.contains(row.code) {
            let code = row.code;
            let fact = row.fact;
            panic!("{name}: {code} [{fact}] is already present without the mutation");
        }
        let mut catalog = base.clone();
        if let Some(mutate_catalog) = row.mutate_catalog {
            (mutate_catalog)(&mut catalog);
        }
        let bytes = (row.mutate_bytes)(base_bytes.clone());
        let fired: BTreeSet<String> = appendix_a::verify_source(&catalog, &bytes)
            .into_iter()
            .map(|violation| violation.code)
            .collect();
        if !fired.contains(row.code) {
            silent.push(format!("{} [{}] -> {:?}", row.code, row.fact, fired));
        }
    }
    assert!(
        silent.is_empty(),
        "{name}: {} of {} laws stopped firing: {silent:?}",
        silent.len(),
        laws.len()
    );
}

// --- fgdb-tsfs family tables -------------------------------------------------
// Each row was derived from its emitter in src/appendix_a.rs and is proven by
// execution: the runner asserts the exact code is ABSENT from the clean
// control and PRESENT after the mutation. Benign collateral violations from
// cross-pinned laws never suppress a target code.

/// Expansion bindings are empty in the clean catalog, so every expansion law
/// pushes a synthetic row; the row_id mirrors the derivation law
/// (`{scope}:expansion-binding:{kind}-{suffix}-parameter-{ordinal}-{kebab}`),
/// keeping collateral noise to unrelated laws.
fn eb_row(
    target_row_id: &str,
    ordinal: i64,
    formal: &str,
    formal_class: &str,
    values: &[&str],
) -> appendix_a::ExpansionBinding {
    let parts: Vec<&str> = target_row_id.split(':').collect();
    appendix_a::ExpansionBinding {
        row_id: format!(
            "{}:expansion-binding:{}-{}-parameter-{}-{}",
            parts[0],
            parts[1],
            parts[2],
            ordinal,
            formal.to_ascii_lowercase()
        ),
        target_row_id: target_row_id.to_string(),
        parameter_ordinal: ordinal,
        formal: formal.to_string(),
        formal_class: formal_class.to_string(),
        values: values.iter().map(|v| v.to_string()).collect(),
        rationale: "fgdb-tsfs witness".into(),
    }
}

fn tsfs_annotation(
    suffix: &str,
    target_row_id: String,
    exact_type: &str,
    generic_expansions: Vec<String>,
) -> appendix_a::Annotation {
    appendix_a::Annotation {
        row_id: format!("a03:annotation:witness-{suffix}"),
        target_row_id,
        exact_type: exact_type.into(),
        cardinality: "one".into(),
        layout: "inline".into(),
        role: "row-store".into(),
        posture: "static".into(),
        authority: "local".into(),
        locality: "resident".into(),
        generic_expansions,
        role_expansions: vec![],
        reference_semantics: "owned-value".into(),
        target_schema_ids: vec![],
        construction_order: "direct".into(),
        retention_and_cut_rule: "retain-all".into(),
        digest_recipe: "sha256-payload".into(),
        redaction_class: "none-redacted".into(),
        resource_bounds: "bounded-small".into(),
        compatibility: "stable-additive".into(),
    }
}

fn appendix_ambiguity_laws() -> Vec<CatalogLaw> {
    vec![
        CatalogLaw {
            code: "catalog_ambiguity_adjudication_contract_unapproved",
            fact: "an adjudication row carries a row_id absent from the readable pin table",
            mutate: |c| {
                let row = c
                    .ambiguity_adjudications
                    .first_mut()
                    .expect("adjudication row exists");
                row.row_id.push_str("-unapproved");
            },
        },
        CatalogLaw {
            code: "catalog_ambiguity_adjudication_duplicate",
            fact: "two adjudication rows share one ambiguity_source_key",
            mutate: |c| {
                let duplicate = c.ambiguity_adjudications[0].clone();
                c.ambiguity_adjudications.push(duplicate);
            },
        },
        CatalogLaw {
            code: "catalog_ambiguity_adjudication_invalid",
            fact: "an adjudication's resolution leaves the closed four-value vocabulary",
            mutate: |c| c.ambiguity_adjudications[0].resolution = "deferred".into(),
        },
        CatalogLaw {
            code: "catalog_ambiguity_resolution_target_invalid",
            fact: "a non-final adjudication still names resolved source keys",
            mutate: |c| c.ambiguity_adjudications[1].resolution = "needs-source-fix".into(),
        },
        CatalogLaw {
            code: "catalog_ambiguity_adjudication_contract_mismatch",
            fact: "an adjudication's resolved_source_keys stop byte-matching its readable pin",
            mutate: |c| {
                c.ambiguity_adjudications[0]
                    .resolved_source_keys
                    .push("zzz".into())
            },
        },
        CatalogLaw {
            code: "catalog_ambiguity_adjudication_contract_missing",
            fact: "the catalog drops an adjudication row its readable contract still pins",
            mutate: |c| {
                c.ambiguity_adjudications.remove(0);
            },
        },
        CatalogLaw {
            code: "catalog_ambiguity_rationale_digest_mismatch",
            fact: "an adjudication's rationale prose stops hashing to its pinned digest",
            mutate: |c| c.ambiguity_adjudications[0].rationale.push(' '),
        },
    ]
}

fn appendix_metadata_laws() -> Vec<CatalogLaw> {
    vec![
        CatalogLaw {
            code: "catalog_definition_status_invalid",
            fact: "a target's definition_status leaves declared|complete",
            mutate: |c| c.targets[0].definition_status = "halfway".into(),
        },
        CatalogLaw {
            code: "catalog_metadata_blank",
            fact: "a candidate's source_locations becomes empty",
            mutate: |c| c.top_level_candidates[0].source_locations.clear(),
        },
        CatalogLaw {
            code: "catalog_target_source_unresolved",
            fact: "a top-level source_key matches no candidate or known prefix",
            mutate: |c| c.targets[0].source_key = "top|NoSuchWitnessSymbol".into(),
        },
        CatalogLaw {
            code: "catalog_target_reference_incomplete",
            fact: "a reference-backed source_key ships as complete while its symbol stays reserved",
            mutate: |c| {
                c.targets
                    .iter_mut()
                    .find(|t| t.source_key == "reference|DeltaBlockVersion")
                    .expect("reserved-reference target exists")
                    .definition_status = "complete".into();
            },
        },
        CatalogLaw {
            code: "catalog_target_source_owner_mismatch",
            fact: "a completed top-keyed target moves off its candidate's canonical slice",
            mutate: |c| {
                let t = &mut c.targets[0];
                t.definition_status = "complete".into();
                t.slice_id = "a01".into();
            },
        },
        CatalogLaw {
            code: "catalog_target_class_mismatch",
            fact: "a candidate's identity_class stops matching its projected logical kind",
            mutate: |c| {
                c.top_level_candidates
                    .iter_mut()
                    .find(|r| r.symbol == "RecoveryCheckpoint")
                    .expect("RecoveryCheckpoint candidate exists")
                    .identity_class = "physical".into();
            },
        },
        CatalogLaw {
            code: "complete_slice_target_missing",
            fact: "a slice promoted to complete loses both its targets and coverage candidates",
            mutate: |c| {
                c.slices
                    .iter_mut()
                    .find(|s| s.id == "a01")
                    .expect("slice a01 exists")
                    .definition_status = "complete".into();
                c.targets.retain(|t| t.slice_id != "a01");
                c.top_level_candidates.retain(|t| t.slice_id != "a01");
            },
        },
        CatalogLaw {
            code: "catalog_annotation_duplicate",
            fact: "two annotation rows share one target_row_id",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                c.annotations
                    .push(tsfs_annotation("dup-1", t.clone(), "u64", vec![]));
                c.annotations
                    .push(tsfs_annotation("dup-2", t, "u64", vec![]));
            },
        },
        CatalogLaw {
            code: "catalog_expansion_missing",
            fact: "a generic exact_type carries no expansion vectors at all",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                c.annotations
                    .push(tsfs_annotation("exp-missing", t, "Vec<u8>", vec![]));
            },
        },
        CatalogLaw {
            code: "catalog_evidence_duplicate",
            fact: "two evidence rows share the (target_row_id, evidence_id) key",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                for i in 0..2 {
                    c.evidence.push(appendix_a::EvidenceBinding {
                        row_id: format!("{}:evidence:witness-dup-{i}", t.replace(':', "-")),
                        target_row_id: t.clone(),
                        evidence_id: "witness".into(),
                        phase: "static".into(),
                        status: "live".into(),
                        owner_bead_id: "fgdb-witness-0".into(),
                        checker_ids: vec!["checker-a".into()],
                        scenario_ids: vec!["scenario-a".into()],
                        event_ids: vec!["event-a".into()],
                        gate_ids: vec!["G0".into()],
                    });
                }
            },
        },
    ]
}

fn appendix_reservation_laws() -> Vec<CatalogLaw> {
    vec![
        CatalogLaw {
            code: "catalog_reservation_symbol_invalid",
            fact: "a reservation symbol stops being a type-family name",
            mutate: |c| c.reservations[0].symbol = "bad-symbol".into(),
        },
        CatalogLaw {
            code: "catalog_reservation_duplicate",
            fact: "two reservations claim one symbol",
            mutate: |c| c.reservations[1].symbol = c.reservations[0].symbol.clone(),
        },
        CatalogLaw {
            code: "catalog_reservation_class_invalid",
            fact: "a reservation's row_kind leaves the logical-kind class",
            mutate: |c| c.reservations[0].row_kind = "physical-kind".into(),
        },
        CatalogLaw {
            code: "catalog_reservation_disposition_invalid",
            fact: "an unprojected reservation's disposition leaves reserved",
            mutate: |c| {
                c.reservations
                    .iter_mut()
                    .find(|r| r.disposition == "reserved")
                    .expect("reserved row exists")
                    .disposition = "existing".into();
            },
        },
        CatalogLaw {
            code: "catalog_reservation_code_collision",
            fact: "a reserved symbol is re-assigned a code owned by a projected kind",
            mutate: |c| {
                let code = c.identity.logical[0].object_kind;
                c.reservations
                    .iter_mut()
                    .find(|r| r.disposition == "reserved")
                    .expect("reserved row exists")
                    .code_reservation = format!("0x{code:04x}");
            },
        },
    ]
}

fn appendix_disposition_laws() -> Vec<CatalogLaw> {
    vec![
        CatalogLaw {
            code: "catalog_source_disposition_count",
            fact: "the disposition census shrinks below its pinned size",
            mutate: |c| {
                c.source_symbol_dispositions.pop();
            },
        },
        CatalogLaw {
            code: "catalog_reservation_disposition_missing",
            fact: "the census row covering a reservation symbol is deleted",
            mutate: |c| {
                let sym = c.reservations[0].symbol.clone();
                c.source_symbol_dispositions.retain(|d| d.symbol != sym);
            },
        },
        CatalogLaw {
            code: "catalog_reservation_owner_mismatch",
            fact: "a non-g0 disposition moves to a different slice than its reservation",
            mutate: |c| {
                c.source_symbol_dispositions
                    .iter_mut()
                    .find(|d| d.slice_id == "a03")
                    .expect("a03 disposition exists")
                    .slice_id = "a04".into();
            },
        },
        CatalogLaw {
            code: "catalog_source_disposition_duplicate",
            fact: "a cloned census row keeps its symbol so the key set collapses",
            mutate: |c| {
                let mut d = c
                    .source_symbol_dispositions
                    .iter()
                    .find(|d| d.slice_id != "g0")
                    .expect("non-g0 disposition exists")
                    .clone();
                d.row_id = format!("{}-witness-dup", d.row_id);
                c.source_symbol_dispositions.push(d);
            },
        },
        CatalogLaw {
            code: "catalog_source_disposition_orphan",
            fact: "a non-g0 disposition names a symbol no reservation claims",
            mutate: |c| {
                c.source_symbol_dispositions
                    .iter_mut()
                    .find(|d| d.slice_id != "g0")
                    .expect("non-g0 disposition exists")
                    .symbol = "ZzOrphanWitness".into();
            },
        },
        CatalogLaw {
            code: "g0_projection_disposition_count",
            fact: "one g0 disposition row is removed from the pinned 35",
            mutate: |c| {
                let i = c
                    .source_symbol_dispositions
                    .iter()
                    .position(|d| d.slice_id == "g0")
                    .expect("g0 row exists");
                c.source_symbol_dispositions.remove(i);
            },
        },
        CatalogLaw {
            code: "g0_projection_disposition_mismatch",
            fact: "a g0 row keeps its pinned id but names the wrong symbol",
            mutate: |c| {
                c.source_symbol_dispositions
                    .iter_mut()
                    .find(|d| d.slice_id == "g0")
                    .expect("g0 row exists")
                    .symbol = "WrongWitnessSymbol".into();
            },
        },
        CatalogLaw {
            code: "g0_projection_disposition_missing",
            fact: "a g0 row_id leaves its derived key so the lookup misses",
            mutate: |c| {
                c.source_symbol_dispositions
                    .iter_mut()
                    .find(|d| d.slice_id == "g0")
                    .expect("g0 row exists")
                    .row_id = "g0:source-symbol-disposition:witness-missing".into();
            },
        },
    ]
}

fn appendix_expansion_laws() -> Vec<CatalogLaw> {
    vec![
        CatalogLaw {
            code: "catalog_expansion_parameter_ordinal_invalid",
            fact: "an expansion binding claims ordinal zero",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                c.expansion_bindings
                    .push(eb_row(&t, 0, "Role", "role", &["Owner"]));
            },
        },
        CatalogLaw {
            code: "catalog_expansion_parameter_ordinal_duplicate",
            fact: "two bindings share one (target, ordinal)",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                c.expansion_bindings
                    .push(eb_row(&t, 1, "Role", "role", &["Owner"]));
                c.expansion_bindings
                    .push(eb_row(&t, 1, "Realm", "role", &["Owner"]));
            },
        },
        CatalogLaw {
            code: "catalog_expansion_formal_invalid",
            fact: "a role-class binding carries a generic formal",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                c.expansion_bindings
                    .push(eb_row(&t, 1, "Role", "generic", &["Owner"]));
            },
        },
        CatalogLaw {
            code: "catalog_expansion_contract_invalid",
            fact: "a bound value leaves the identifier byte class",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                c.expansion_bindings
                    .push(eb_row(&t, 1, "Role", "role", &["alpha-beta"]));
            },
        },
        CatalogLaw {
            code: "catalog_expansion_binding_contract_unapproved",
            fact: "a live expansion binding has no readable contract pin",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                c.expansion_bindings
                    .push(eb_row(&t, 1, "Role", "role", &["Owner"]));
            },
        },
        CatalogLaw {
            code: "catalog_expansion_binding_contract_drift",
            fact: "the live expansion-binding table drifts from its compiled-in pin",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                c.expansion_bindings
                    .push(eb_row(&t, 1, "Role", "role", &["Owner"]));
            },
        },
        CatalogLaw {
            code: "catalog_ambiguity_adjudication_contract_drift",
            fact: "the live adjudication table drifts from its pinned count and sha",
            mutate: |c| {
                c.ambiguity_adjudications.pop();
            },
        },
        CatalogLaw {
            code: "catalog_expansion_invalid",
            fact: "a generic expansion list arrives unsorted",
            mutate: |c| {
                let t = c.projection_rows[0].row_id.clone();
                c.annotations.push(tsfs_annotation(
                    "unsorted",
                    t,
                    "Vec<u8>",
                    vec!["Zeta".into(), "Alpha".into()],
                ));
            },
        },
        CatalogLaw {
            code: "catalog_expansion_source_coverage_mismatch",
            fact: "bindings never cover the source family's declared dimensions",
            mutate: |c| {
                c.expansion_bindings.push(eb_row(
                    "a17:logical-kind:canonical-pre-bootstrap-evidence-reencryption-owner",
                    99,
                    "Role",
                    "role",
                    &["Owner"],
                ));
            },
        },
    ]
}

fn appendix_source_ambiguity_laws() -> Vec<SourceLaw> {
    vec![
        SourceLaw {
            code: "source_ambiguity_adjudication_orphan",
            fact: "an adjudication key names no ambiguity in the raw source census",
            mutate_catalog: Some(|c| {
                c.ambiguity_adjudications[0]
                    .ambiguity_source_key
                    .push_str("-drift");
            }),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_ambiguity_adjudication_mismatch",
            fact: "an adjudication's source_locations stop matching the raw census",
            mutate_catalog: Some(|c| {
                c.ambiguity_adjudications[0]
                    .source_locations
                    .push("a14:99999".into());
            }),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_ambiguity_resolution_relation_mismatch",
            fact: "a final adjudication's resolved set stops matching the parser-owned set",
            mutate_catalog: Some(|c| {
                c.ambiguity_adjudications[0].resolved_source_keys.pop();
            }),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_complete_slice_ambiguity_unresolved",
            fact: "a slice with zero adjudicated ambiguities is promoted to complete",
            mutate_catalog: Some(|c| {
                c.slices
                    .iter_mut()
                    .find(|s| s.id == "a04")
                    .expect("slice a04 exists")
                    .definition_status = "complete".into();
            }),
            mutate_bytes: identity_bytes,
        },
    ]
}

fn identity_bytes(bytes: Vec<u8>) -> Vec<u8> {
    bytes
}

fn appendix_resource_bucket_laws() -> Vec<SourceLaw> {
    vec![SourceLaw {
        code: "resource_bucket_contract_cardinality",
        fact: "the ResourceLedgerState paragraph splits into zero charge-bearing paragraphs",
        mutate_catalog: None,
        mutate_bytes: |mut bytes| {
            let marker = b"`ResourceLedgerState` is ";
            let pos = bytes
                .windows(marker.len())
                .position(|w| w == marker)
                .expect("ResourceLedgerState line exists");
            bytes.splice(pos..pos + 1, b"The ".iter().copied());
            bytes
        },
    }]
}

fn appendix_citation_registry_law() -> Vec<RootLaw> {
    vec![RootLaw {
        code: "catalog_ambiguity_citation_registry_unavailable",
        fact: "the repository under test lacks registries/laws.toml so the gate fails closed",
        entry: RootEntry::ProjectionDiffScratch,
        mutate: |_| {},
    }]
}

fn appendix_source_census_laws() -> Vec<SourceLaw> {
    vec![
        SourceLaw {
            code: "source_concatenation_mismatch",
            fact: "catalog slice order no longer reconstructs the Appendix byte sequence",
            mutate_catalog: Some(|c| c.slices.reverse()),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_heading_mismatch",
            fact: "the pinned heading text no longer byte-matches the start_line content",
            mutate_catalog: Some(|c| c.source_manifest.heading.push('!')),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_byte_count_mismatch",
            fact: "the pinned byte_count no longer equals the extracted Appendix length",
            mutate_catalog: Some(|c| c.source_manifest.byte_count += 1),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_heading_missing",
            fact: "the line after the Appendix range (next_heading pin) does not exist",
            mutate_catalog: None,
            mutate_bytes: |mut bytes| {
                let end = appendix_catalog().source_manifest.end_line as usize;
                let mut seen = 0usize;
                let mut cut = bytes.len();
                for (i, byte) in bytes.iter().enumerate() {
                    if *byte == b'\n' {
                        seen += 1;
                        if seen == end {
                            cut = i + 1;
                            break;
                        }
                    }
                }
                bytes.truncate(cut);
                bytes
            },
        },
        SourceLaw {
            code: "source_census_range_invalid",
            fact: "a declared slice's start_line is negative so its coordinates do not fit usize",
            mutate_catalog: Some(|c| c.slices[0].start_line = -1),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_structural_census_error",
            fact: "two declared slices claim overlapping line ranges so the partition is not disjoint",
            mutate_catalog: Some(|c| c.slices[1].start_line = c.slices[0].start_line),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_top_level_candidate_orphan",
            fact: "a catalog top-level candidate's source_key is absent from the source census",
            mutate_catalog: Some(|c| {
                let mut orphan = c.top_level_candidates[0].clone();
                orphan.source_key = format!("zz-orphan-witness|{}", orphan.symbol);
                c.top_level_candidates.push(orphan);
            }),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "reference_source_reservation_orphan",
            fact: "a permanent reservation's symbol never appears as a plan-derived reference family",
            mutate_catalog: Some(|c| {
                c.reservations.push(appendix_a::Reservation {
                    row_id: "zz:witness:reservation-orphan".into(),
                    slice_id: "a01".into(),
                    symbol: "ZzWitnessOrphanType".into(),
                    row_kind: "logical-kind".into(),
                    identity_class: "identity".into(),
                    code_reservation: String::new(),
                    disposition: "permanent".into(),
                });
            }),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "reference_source_disposition_missing",
            fact: "a plan-derived reference family has no non-g0 source-symbol disposition row",
            mutate_catalog: Some(|c| {
                c.source_symbol_dispositions
                    .retain(|row| row.slice_id == "g0" || row.symbol != "ConfigurationState");
            }),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "reference_source_disposition_orphan",
            fact: "a catalog disposition row names a symbol absent from the plan-derived reference census",
            mutate_catalog: Some(|c| {
                c.source_symbol_dispositions
                    .push(appendix_a::SourceSymbolDisposition {
                        row_id: "zz:witness:disposition-orphan".into(),
                        slice_id: "a01".into(),
                        symbol: "ZzWitnessOrphanType".into(),
                        disposition: "permanent".into(),
                        source_locations: vec!["a01:1".into()],
                    });
            }),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_annotation_contract_ambiguous",
            fact: "a complete field annotation stands over a field whose ambiguity discharge was revoked, so no single unambiguous source exact_type backs it",
            mutate_catalog: Some(|c| {
                let adj = c
                    .ambiguity_adjudications
                    .iter_mut()
                    .find(|row| {
                        row.row_id
                            == "a01:ambiguity-adjudication:9902cb5d9fadf41a985fd54c1bc021af6ff2e124af9886e02fb808aac5c05459"
                    })
                    .expect("discharging adjudication exists");
                adj.resolution = "corrupted".into();
                let target = c
                    .targets
                    .iter_mut()
                    .find(|t| {
                        t.source_key
                            == "field|ExportLeaf<T>|ExportLeaf<T>.authority_ledger_floor|authority_ledger_floor"
                    })
                    .expect("ExportLeaf floor field target exists");
                target.definition_status = "complete".into();
                let projection_row_id = target.target_row_id.clone();
                c.annotations.push(appendix_a::Annotation {
                    row_id: "zz:witness:ambiguous-annotation".into(),
                    target_row_id: projection_row_id,
                    exact_type: String::new(),
                    cardinality: String::new(),
                    layout: String::new(),
                    role: String::new(),
                    posture: String::new(),
                    authority: String::new(),
                    locality: String::new(),
                    generic_expansions: Vec::new(),
                    role_expansions: Vec::new(),
                    reference_semantics: String::new(),
                    target_schema_ids: Vec::new(),
                    construction_order: String::new(),
                    retention_and_cut_rule: String::new(),
                    digest_recipe: String::new(),
                    redaction_class: String::new(),
                    resource_bounds: String::new(),
                    compatibility: String::new(),
                });
            }),
            mutate_bytes: identity_bytes,
        },
    ]
}

fn appendix_source_manifest_laws() -> Vec<CatalogLaw> {
    vec![
        CatalogLaw {
            code: "source_manifest_pin_invalid",
            fact: "the manifest byte_count pin becomes non-positive",
            mutate: |c| c.source_manifest.byte_count = 0,
        },
        CatalogLaw {
            code: "reference_manifest_mismatch",
            fact: "the reference manifest target count leaves the reservation-symbol census",
            mutate: |c| c.reference_manifest.target_count += 1,
        },
    ]
}

/// File-surface laws: the public loaders over scratch roots. Each pairs a
/// clean control (real catalog loads) with a corrupted fixture.
#[test]
fn appendix_tsfs_file_surface_laws_are_seen_to_fire() {
    // catalog_read: the catalog file does not exist at all.
    let absent_root = scratch_root("catalog-read");
    let control = appendix_a::load_catalog_file(&repo_root().join(appendix_a::CATALOG_PATH));
    assert!(control.is_ok(), "clean catalog must load for the control");
    let fired = appendix_a::load_catalog_file(&absent_root.join(appendix_a::CATALOG_PATH))
        .expect_err("missing catalog file must fail")
        .into_iter()
        .map(|violation| violation.code)
        .collect::<Vec<_>>();
    assert_code(&fired, "catalog_read");

    // catalog_encoding: one invalid UTF-8 byte appended to the real text.
    let encoding_root = scratch_root("catalog-encoding");
    let mut bad = appendix_catalog_text().into_bytes();
    bad.push(0xFF);
    std::fs::create_dir_all(encoding_root.join("registries")).expect("fixture dir");
    std::fs::write(encoding_root.join(appendix_a::CATALOG_PATH), &bad).expect("fixture write");
    let fired = appendix_a::load_catalog_file(&encoding_root.join(appendix_a::CATALOG_PATH))
        .expect_err("invalid UTF-8 must fail")
        .into_iter()
        .map(|violation| violation.code)
        .collect::<Vec<_>>();
    assert_code(&fired, "catalog_encoding");

    // source_read: valid catalog, but the pinned plan source is absent.
    let bare_root = scratch_root("source-read");
    write_fixture(
        &bare_root,
        appendix_a::CATALOG_PATH,
        &appendix_catalog_text(),
    );
    let fired = appendix_a::load_and_verify(&bare_root)
        .expect_err("missing plan source must fail")
        .into_iter()
        .map(|violation| violation.code)
        .collect::<Vec<_>>();
    assert_code(&fired, "source_read");
}

fn appendix_bindings_laws() -> Vec<RootLaw> {
    vec![
        RootLaw {
            code: "catalog_maintenance_owner_crate_unresolved",
            fact: "maintenance_proof.owner_crate does not resolve to a workspace package",
            entry: RootEntry::Bindings,
            mutate: |c| c.maintenance_proof.owner_crate = "fgdb-ghost-crate".to_owned(),
        },
        RootLaw {
            code: "catalog_scenario_target_scope_drift",
            fact: "the manifest-pinned scenario's sha no longer equals the catalog pin",
            entry: RootEntry::Bindings,
            mutate: |c| c.target_manifest.target_source_assignment_sha256 = "0".repeat(64),
        },
        RootLaw {
            code: "catalog_evidence_scenario_target_uncovered",
            fact: "an evidence row references a live scenario that does not cover its target",
            entry: RootEntry::Bindings,
            mutate: |c| {
                c.evidence.push(appendix_a::EvidenceBinding {
                    row_id: "a10:evidence:union-witness-target-witness".into(),
                    target_row_id: "a10:union:no-such-witness-target".into(),
                    evidence_id: "witness".into(),
                    phase: "static".into(),
                    status: "planned".into(),
                    owner_bead_id: "fgdb-appendix-a-catalog-scaffold-gvvf".into(),
                    checker_ids: vec![],
                    scenario_ids: vec!["g0_identity_e2e".into()],
                    event_ids: vec!["appendix_closure_checked".into()],
                    gate_ids: vec!["G0".into()],
                });
            },
        },
        RootLaw {
            code: "catalog_evidence_scenario_uncovered",
            fact: "the referenced scenario contributes none of the maintenance event ids",
            entry: RootEntry::Bindings,
            mutate: |c| c.maintenance_proof.event_ids.clear(),
        },
    ]
}

fn appendix_generated_family_laws() -> Vec<RootLaw> {
    vec![
        RootLaw {
            code: "generated_family_union_missing",
            fact: "a projected ordinary union carrying a generated family name is renamed away",
            entry: RootEntry::ProjectionDiff,
            mutate: |c| {
                if let Some(u) = c
                    .identity
                    .ordinary_unions
                    .iter_mut()
                    .find(|u| u.union_name == "GlobalSequenceNeutralSpec<Tag>")
                {
                    u.union_name = "UnprojectedFamilyUnion".into();
                }
            },
        },
        RootLaw {
            code: "generated_family_arm_set_mismatch",
            fact: "one projected family arm's source_arm_name diverges from the derived set",
            entry: RootEntry::ProjectionDiff,
            mutate: |c| {
                let u = c
                    .identity
                    .ordinary_unions
                    .iter_mut()
                    .find(|u| u.union_name == "SequenceNeutralSpec<Tag>")
                    .expect("SequenceNeutralSpec union is projected");
                u.arms[0].source_arm_name = "drifted-arm".into();
            },
        },
        RootLaw {
            code: "generated_family_payload_drift",
            fact: "one projected family arm's payload digest diverges from the derived digest",
            entry: RootEntry::ProjectionDiff,
            mutate: |c| {
                let u = c
                    .identity
                    .ordinary_unions
                    .iter_mut()
                    .find(|u| u.union_name == "SequenceNeutralSpec<Tag>")
                    .expect("SequenceNeutralSpec union is projected");
                u.arms[0].payload_sha256 = Some("0".repeat(64));
            },
        },
        RootLaw {
            code: "generated_family_name_squatting",
            fact: "a non-derived ordinary union claims a generated wrapper name",
            entry: RootEntry::ProjectionDiff,
            mutate: |c| {
                if let Some(u) = c
                    .identity
                    .ordinary_unions
                    .iter_mut()
                    .find(|u| u.union_name == "TrustTransition")
                {
                    u.union_name = "TrustTransitionSequenceNeutralSpec".into();
                }
            },
        },
    ]
}

fn appendix_closure_laws() -> Vec<RootLaw> {
    vec![
        RootLaw {
            code: "catalog_evidence_contract_invalid",
            fact: "an evidence binding's phase leaves static|runtime",
            entry: RootEntry::Closure,
            mutate: |c| {
                c.evidence.push(appendix_a::EvidenceBinding {
                    row_id: "a10:evidence:witness-target-witness".into(),
                    target_row_id: "a10:union:witness-target".into(),
                    evidence_id: "witness".into(),
                    phase: "quantum".into(),
                    status: "planned".into(),
                    owner_bead_id: "fgdb-appendix-a-catalog-scaffold-gvvf".into(),
                    checker_ids: vec![],
                    scenario_ids: vec![],
                    event_ids: vec![],
                    gate_ids: vec![],
                });
            },
        },
        RootLaw {
            code: "catalog_semantic_consumer_invalid",
            fact: "a semantic binding names the maintenance crate as a consumer",
            entry: RootEntry::Closure,
            mutate: |c| {
                c.semantic_bindings.push(appendix_a::SemanticBinding {
                    row_id: "catalog:semantic-binding:witness-consumer".into(),
                    target_row_id: "a10:union:witness-target".into(),
                    owner_bead_id: "fgdb-witness-owner-zz".into(),
                    owner_crate: "fgdb-witness".into(),
                    owner_status: "planned".into(),
                    consumer_crates: vec!["appendix-a-catalog".into()],
                });
            },
        },
        RootLaw {
            code: "catalog_annotation_field_contract_unresolved",
            fact: "a field annotation's authoritative durable-field row no longer resolves",
            entry: RootEntry::Closure,
            mutate: |c| {
                let proj = c
                    .projection_rows
                    .iter()
                    .find(|p| p.projection == "durable_fields" && p.row_kind == "field")
                    .expect("projected field row exists")
                    .clone();
                let f = c
                    .identity
                    .fields
                    .iter_mut()
                    .find(|f| {
                        format!("{}.{}", f.containing_schema, f.stable_name)
                            == proj.canonical_symbol
                    })
                    .expect("annotated field exists");
                f.stable_name.push_str("_unresolved");
                c.annotations.push(appendix_a::Annotation {
                    row_id: "catalog:annotation:witness-unresolved".into(),
                    target_row_id: proj.row_id,
                    exact_type: "Witness".into(),
                    cardinality: "one".into(),
                    layout: "unit".into(),
                    role: String::new(),
                    posture: String::new(),
                    authority: String::new(),
                    locality: String::new(),
                    generic_expansions: Vec::new(),
                    role_expansions: Vec::new(),
                    reference_semantics: String::new(),
                    target_schema_ids: Vec::new(),
                    construction_order: String::new(),
                    retention_and_cut_rule: String::new(),
                    digest_recipe: String::new(),
                    redaction_class: String::new(),
                    resource_bounds: String::new(),
                    compatibility: String::new(),
                });
            },
        },
    ]
}

fn appendix_source_union_laws() -> Vec<SourceLaw> {
    vec![
        SourceLaw {
            code: "source_union_annotation_mismatch",
            fact: "a complete union target has no matching annotation (annotations are empty)",
            mutate_catalog: Some(|c| {
                let mut symbols: Vec<String> = c
                    .identity
                    .ordinary_unions
                    .iter()
                    .map(|u| format!("{}.{}", u.containing_schema, u.union_path))
                    .collect();
                symbols.sort();
                symbols.dedup();
                let row_id = c
                    .projection_rows
                    .iter()
                    .find(|p| p.row_kind == "union" && symbols.contains(&p.canonical_symbol))
                    .expect("projected union row exists")
                    .row_id
                    .clone();
                c.targets
                    .iter_mut()
                    .find(|t| t.target_row_id == row_id)
                    .expect("union target exists")
                    .definition_status = "complete".into();
            }),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_union_arm_annotation_mismatch",
            fact: "a complete union-arm target has no matching annotation",
            mutate_catalog: Some(|c| {
                let mut symbols: Vec<String> = c
                    .identity
                    .ordinary_unions
                    .iter()
                    .flat_map(|u| {
                        u.arms.iter().map(move |a| {
                            format!(
                                "{}.{}.{}",
                                a.containing_schema, a.union_path, a.source_arm_name
                            )
                        })
                    })
                    .collect();
                symbols.sort();
                symbols.dedup();
                let row_id = c
                    .projection_rows
                    .iter()
                    .find(|p| p.row_kind == "union-arm" && symbols.contains(&p.canonical_symbol))
                    .expect("projected union-arm row exists")
                    .row_id
                    .clone();
                c.targets
                    .iter_mut()
                    .find(|t| t.target_row_id == row_id)
                    .expect("union-arm target exists")
                    .definition_status = "complete".into();
            }),
            mutate_bytes: identity_bytes,
        },
        SourceLaw {
            code: "source_union_contract_mismatch",
            fact: "a union target resolves to a different census union candidate",
            mutate_catalog: Some(|c| {
                let mut keys: Vec<String> = Vec::new();
                for p in &c.projection_rows {
                    if p.row_kind != "union" {
                        continue;
                    }
                    if let Some(t) = c.targets.iter().find(|t| t.target_row_id == p.row_id) {
                        // Generated-family wrapper unions are checked by
                        // verify_generated_family_unions, not here; swapping
                        // their keys is invisible to this law.
                        if t.source_key.contains("SequenceNeutralSpec") {
                            continue;
                        }
                        keys.push(t.source_key.clone());
                    }
                }
                keys.sort();
                keys.dedup();
                assert!(
                    keys.len() >= 2,
                    "witness needs two distinct union source keys"
                );
                let victim = c
                    .targets
                    .iter_mut()
                    .find(|t| t.source_key == keys[0] && t.target_kind == "union")
                    .expect("union target exists");
                victim.source_key = keys[1].clone();
            }),
            mutate_bytes: identity_bytes,
        },
    ]
}

fn appendix_text_laws() -> Vec<TextLaw> {
    vec![TextLaw {
        code: "catalog_toml_parse",
        fact: "catalog text stops being parseable TOML",
        munge: |text| format!("@@@ not toml @@@\n{text}"),
    }]
}

fn appendix_projection_laws() -> Vec<CatalogLaw> {
    vec![
        CatalogLaw {
            code: "projection_epoch_mismatch",
            fact: "the logical registry epoch desyncs from its projection_epochs pin",
            mutate: |c| c.identity.logical_epoch += 1,
        },
        CatalogLaw {
            code: "projection_row_count",
            fact: "one projection row is removed so the count leaves its pinned size",
            mutate: |c| {
                let i = c
                    .projection_rows
                    .iter()
                    .position(|r| r.slice_id != "g0")
                    .expect("non-g0 projection row exists");
                c.projection_rows.remove(i);
            },
        },
        CatalogLaw {
            code: "projection_owner_assignment_drift",
            fact: "the released sorted row-id transcript sha no longer matches its pin",
            mutate: |c| {
                let i = c
                    .projection_rows
                    .iter()
                    .position(|r| r.slice_id != "g0")
                    .expect("non-g0 projection row exists");
                c.projection_rows.remove(i);
            },
        },
        CatalogLaw {
            code: "slice_projection_invalid",
            fact: "a slice expects an unknown projection class",
            mutate: |c| {
                c.slices[0]
                    .expected_projection_classes
                    .push("bogus_class".into())
            },
        },
        CatalogLaw {
            code: "slice_census_duplicate",
            fact: "a cloned candidate duplicates a source key inside one slice census",
            mutate: |c| {
                let clone = c.top_level_candidates[0].clone();
                c.top_level_candidates.push(clone);
            },
        },
        CatalogLaw {
            code: "catalog_slice_unknown",
            fact: "a projection row names a slice id that does not resolve",
            mutate: |c| {
                let i = c
                    .projection_rows
                    .iter()
                    .position(|r| r.slice_id != "g0")
                    .expect("non-g0 projection row exists");
                c.projection_rows[i].slice_id = "zz_unknown_zz".into();
            },
        },
        CatalogLaw {
            code: "slice_adjacency_mismatch",
            fact: "slice 2 claims slice 0 as its predecessor",
            mutate: |c| c.slices[2].predecessor = c.slices[0].id.clone(),
        },
        CatalogLaw {
            code: "slice_count_mismatch",
            fact: "the slice table shrinks below the pinned SLICE_PINS length",
            mutate: |c| {
                c.slices.pop();
            },
        },
        CatalogLaw {
            code: "slice_endpoint_mismatch",
            fact: "the manifest end_line moves and strands the last slice endpoint",
            mutate: |c| c.source_manifest.end_line += 1,
        },
        CatalogLaw {
            code: "slice_enum_invalid",
            fact: "a slice's definition_status leaves declared|complete",
            mutate: |c| c.slices[0].definition_status = "draft".into(),
        },
        CatalogLaw {
            code: "slice_pin_invalid",
            fact: "a slice's byte_count becomes non-positive",
            mutate: |c| c.slices[0].byte_count = 0,
        },
    ]
}

fn appendix_dag_law() -> Vec<SourceLaw> {
    vec![SourceLaw {
        code: "census_dag_cycle",
        fact: "one inserted reverse strong ref closes a genuine construction-order 2-cycle",
        mutate_catalog: None,
        mutate_bytes: |mut bytes| {
            let needle = b"`AuthorizationDecisionRecord<Role>` is `{authority_binding:AuthorityBindingFor<Role>,decision_body}`";
            let pos = bytes
                .windows(needle.len())
                .position(|w| w == needle)
                .expect("AuthorizationDecisionRecord record body exists");
            let inserted = b"`AuthorizationDecisionRecord<Role>` is `{authority_binding:AuthorityBindingFor<Role>,witness_lpas_ref:StrongRef<LocalPrepareAdmissionSpec>,decision_body}`";
            bytes.splice(pos..pos + needle.len(), inserted.iter().copied());
            bytes
        },
    }]
}

fn appendix_epoch_text_laws() -> Vec<TextLaw> {
    fn append_bogus(text: &str) -> String {
        format!("{text}\n[[projection_epoch]]\nregistry = \"bogus_registry\"\nregistry_epoch = 1\n")
    }
    vec![
        TextLaw {
            code: "projection_epoch_count",
            fact: "a seventh epoch table exceeds the six registered projection classes",
            munge: append_bogus,
        },
        TextLaw {
            code: "projection_epoch_unknown",
            fact: "an epoch table names an unregistered projection class",
            munge: append_bogus,
        },
        TextLaw {
            code: "projection_epoch_duplicate",
            fact: "the final epoch block repeats its registry name verbatim",
            munge: |text| {
                let header = text
                    .rfind("[[projection_epoch]]")
                    .expect("epoch block exists");
                format!("{text}{}", &text[header..])
            },
        },
        TextLaw {
            code: "projection_epoch_invalid",
            fact: "the first epoch value arrives non-positive",
            munge: |text| {
                let h = text
                    .find("[[projection_epoch]]")
                    .expect("epoch block exists");
                let p = h + text[h..]
                    .find("registry_epoch = ")
                    .expect("epoch key exists");
                let end = p + text[p..].find('\n').expect("key line ends");
                let mut out = String::with_capacity(text.len());
                out.push_str(&text[..p]);
                out.push_str("registry_epoch = -5");
                out.push_str(&text[end..]);
                out
            },
        },
        TextLaw {
            code: "projection_epoch_missing",
            fact: "the durable_fields epoch table is deleted while projections still parse for it",
            munge: |text| {
                let header = text
                    .rfind("[[projection_epoch]]")
                    .expect("epoch block exists");
                text[..header].to_string()
            },
        },
    ]
}

#[test]
fn appendix_tsfs_tables_are_seen_to_fire() {
    run_catalog_laws("appendix_ambiguity", &appendix_ambiguity_laws());
    run_catalog_laws("appendix_metadata", &appendix_metadata_laws());
    run_catalog_laws("appendix_reservations", &appendix_reservation_laws());
    run_catalog_laws("appendix_dispositions", &appendix_disposition_laws());
    run_catalog_laws("appendix_expansions", &appendix_expansion_laws());
    run_catalog_laws("appendix_source_manifest", &appendix_source_manifest_laws());
    run_catalog_laws("appendix_projections", &appendix_projection_laws());
    run_source_laws(
        "appendix_source_ambiguity",
        &appendix_source_ambiguity_laws(),
    );
    run_source_laws("appendix_resource_bucket", &appendix_resource_bucket_laws());
    run_source_laws("appendix_source_census", &appendix_source_census_laws());
    run_source_laws("appendix_source_unions", &appendix_source_union_laws());
    run_source_laws("appendix_dag_cycle", &appendix_dag_law());
    run_root_laws(
        "appendix_citation_registry",
        &appendix_citation_registry_law(),
    );
    run_root_laws("appendix_bindings", &appendix_bindings_laws());
    run_root_laws(
        "appendix_generated_family",
        &appendix_generated_family_laws(),
    );
    run_root_laws("appendix_closure", &appendix_closure_laws());
    run_text_laws("appendix_text", &appendix_text_laws());
    run_text_laws("appendix_epoch_text", &appendix_epoch_text_laws());
}

/// Scratch-root recipes for the fail-closed repository-binding legs and the
/// generated-family contract loader. Each fixture is the minimal tree whose
/// only defect is the named one; the control is the same reader over the real
/// repository, where the code must be absent.
#[test]
fn appendix_tsfs_scratch_recipes_are_seen_to_fire() {
    fn codes_of(root: &Path, catalog: &appendix_a::Catalog) -> Vec<String> {
        appendix_a::verify_repository_bindings(root, catalog)
            .into_iter()
            .map(|violation| violation.code)
            .collect()
    }
    let base = appendix_catalog();

    // catalog_repository_registry_unavailable: an empty root has no
    // architecture registry at all.
    let fired = codes_of(&scratch_root("bindings-empty"), &base);
    assert_code(&fired, "catalog_repository_registry_unavailable");

    // Shared base: registries the binding reader loads before the workspace.
    let make_base = |tag: &str| {
        let root = scratch_root(tag);
        write_fixture(
            &root,
            "registries/architecture_decisions.toml",
            &fs::read_to_string(repo_root().join("registries/architecture_decisions.toml"))
                .expect("architecture decisions readable"),
        );
        write_fixture(
            &root,
            ".beads/issues.jsonl",
            &fs::read_to_string(repo_root().join(".beads/issues.jsonl"))
                .unwrap_or_else(|_| String::new()),
        );
        root
    };

    // catalog_repository_workspace_unavailable: no root Cargo.toml.
    let root = make_base("bindings-no-workspace");
    let fired = codes_of(&root, &base);
    assert_code(&fired, "catalog_repository_workspace_unavailable");

    // Minimal workspace fixture builder: base + root/member Cargo.tomls plus
    // an optionally mutated checker index.
    let with_workspace = |tag: &str, index: Option<&str>| {
        let root = make_base(tag);
        write_fixture(&root, "Cargo.toml", "[workspace]\nmembers = [\"m\"]\n");
        write_fixture(
            &root,
            "m/Cargo.toml",
            "[package]\nname = \"registry-check\"\n",
        );
        if let Some(index) = index {
            write_fixture(&root, "registries/checker_index.toml", index);
        }
        root
    };

    // catalog_repository_checker_index_unavailable: workspace present, no
    // checker_index.toml.
    let fired = codes_of(&with_workspace("bindings-no-index", None), &base);
    assert_code(&fired, "catalog_repository_checker_index_unavailable");

    // catalog_repository_checker_index_ambiguous: one identical duplicate row.
    // Anchor the marker at line start: the file's header comments mention
    // `[[checker]]` too, and a mid-comment match would duplicate prose and
    // break the TOML parse into the unavailable code instead.
    let duplicate = |index: &mut String| {
        let marker = "\n[[checker]]";
        let start = index
            .find(marker)
            .expect("checker table exists in the real index")
            + 1;
        let end = index[start + marker.len() - 1..]
            .find("\n[[checker]]")
            .map(|offset| start + marker.len() - 1 + offset)
            .unwrap_or(index.len());
        let block: String = index[start..end].to_owned();
        index.insert_str(end, &block);
    };
    let mut duplicated = fs::read_to_string(repo_root().join("registries/checker_index.toml"))
        .expect("checker index readable");
    duplicate(&mut duplicated);
    let root = with_workspace("bindings-dup-index", Some(&duplicated));
    let fired = codes_of(&root, &base);
    assert_code(&fired, "catalog_repository_checker_index_ambiguous");

    // catalog_scenario_registry_drift: the scenario's checker is renamed.
    let renamed = fs::read_to_string(repo_root().join("registries/checker_index.toml"))
        .expect("checker index readable")
        .replace("g0_identity_e2e", "g0_identity_e2e_renamed");
    let root = with_workspace("bindings-renamed-checker", Some(&renamed));
    let fired = codes_of(&root, &base);
    assert_code(&fired, "catalog_scenario_registry_drift");

    // generated_family_contracts_unavailable / _derivation_invalid via the
    // aggregate projection gate over scratch roots.
    let diff_codes = |root: &Path| {
        appendix_a::appendix_a_catalog_projection_diff(root, &base)
            .into_iter()
            .map(|violation| violation.code)
            .collect::<Vec<_>>()
    };

    let fired = diff_codes(&scratch_root("genfam-empty"));
    assert_code(&fired, "generated_family_contracts_unavailable");

    let contracts_root = scratch_root("genfam-broken-contract");
    let contracts = fs::read_to_string(repo_root().join("registries/command_contracts.toml"))
        .expect("command contracts readable");
    let broken = contracts.replacen("cc:local:recovery-bridge-spec", "broken-id", 1);
    write_fixture(
        &contracts_root,
        "registries/command_contracts.toml",
        &broken,
    );
    let fired = diff_codes(&contracts_root);
    assert_code(&fired, "generated_family_derivation_invalid");
}

// ===========================================================================
// The suite's own guard
// ===========================================================================

/// Every table row must name a distinct law, and the total must be the pinned
/// one.
///
/// Without this a row can be deleted, or two rows can collapse onto one code
/// after a rename, and the suite still passes — the exact failure mode the whole
/// file exists to remove, reached from inside. The pin is a local fact about
/// this file, not a census of the corpus, so it moves only when a row is
/// deliberately added or removed.
#[test]
fn every_witness_row_names_a_distinct_law() {
    let mut codes: Vec<&'static str> = Vec::new();
    codes.extend(appendix_candidate_witnesses().iter().map(|row| row.code));
    for group in [
        topology_header_witnesses(),
        topology_crate_witnesses(),
        topology_capability_witnesses(),
        topology_derivation_witnesses(),
        topology_live_tree_witnesses(),
        topology_live_graph_witnesses(),
    ] {
        codes.extend(group.iter().map(|row| row.code));
    }
    codes.extend(
        unsafe_island_live_tree_witnesses()
            .iter()
            .map(|row| row.code),
    );
    codes.extend(threat_witnesses().iter().map(|row| row.code));
    codes.extend(registry_witnesses().iter().map(|row| row.code));
    codes.extend(architecture_registry_witnesses().iter().map(|row| row.code));
    codes.extend(architecture_tree_witnesses().iter().map(|row| row.code));
    codes.extend(appendix_ambiguity_laws().iter().map(|row| row.code));
    codes.extend(appendix_metadata_laws().iter().map(|row| row.code));
    codes.extend(appendix_reservation_laws().iter().map(|row| row.code));
    codes.extend(appendix_disposition_laws().iter().map(|row| row.code));
    codes.extend(appendix_expansion_laws().iter().map(|row| row.code));
    codes.extend(appendix_source_ambiguity_laws().iter().map(|row| row.code));
    codes.extend(appendix_resource_bucket_laws().iter().map(|row| row.code));
    codes.extend(appendix_citation_registry_law().iter().map(|row| row.code));
    codes.extend(appendix_bindings_laws().iter().map(|row| row.code));
    codes.extend(appendix_generated_family_laws().iter().map(|row| row.code));
    codes.extend(appendix_closure_laws().iter().map(|row| row.code));
    codes.extend(appendix_source_census_laws().iter().map(|row| row.code));
    codes.extend(appendix_source_manifest_laws().iter().map(|row| row.code));
    codes.extend(appendix_source_union_laws().iter().map(|row| row.code));
    codes.extend(appendix_text_laws().iter().map(|row| row.code));
    codes.extend(appendix_projection_laws().iter().map(|row| row.code));
    codes.extend(appendix_dag_law().iter().map(|row| row.code));
    codes.extend(appendix_epoch_text_laws().iter().map(|row| row.code));

    assert_eq!(codes.len(), 221, "table row count moved");
    let distinct: BTreeSet<&str> = codes.iter().copied().collect();
    // `active_not_a_member`/`active_manifest_missing` are two laws reached by
    // one fact. The island rows above deliberately use separate scan facts.
    // In both cases every row still owns one distinct code.
    assert_eq!(
        distinct.len(),
        codes.len(),
        "two rows name the same law, so one of them is proving nothing"
    );
    assert!(
        codes.iter().all(|code| !code.is_empty()),
        "a row with an empty code asserts nothing"
    );
}

// ===========================================================================
// fgdb-wi4f: logical_object_kinds arm-binding witnesses
//
// `validate_active_logical_kind_arms` reads BOTH of its inputs fresh from the
// root — `registries/logical_object_kinds.toml` and the generated
// `crates/fgdb-types/src/refs.rs` — so each witness below is a scratch root
// carrying the real refs.rs and a singly mutated kind table (plus a copy of
// the whole registries dir, because `model::load_registries` loads every
// registry it knows about). The full validate_all sweep runs on top; codes
// from unrelated minimal-root legs co-fire freely and every assertion names
// exactly one target code.
// ===========================================================================

fn logical_kind_scratch(tag: &str, mutate_toml: fn(&mut String)) -> PathBuf {
    let root = scratch_root(tag);
    let registries_src = repo_root().join("registries");
    for entry in std::fs::read_dir(&registries_src).expect("registries dir readable") {
        let entry = entry.expect("registries dir entry");
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        write_fixture(
            &root,
            &std::path::Path::new("registries")
                .join(&name)
                .to_string_lossy(),
            &std::fs::read_to_string(entry.path()).unwrap_or_default(),
        );
    }
    let refs = repo_root().join("crates/fgdb-types/src/refs.rs");
    write_fixture(
        &root,
        "crates/fgdb-types/src/refs.rs",
        &std::fs::read_to_string(refs).expect("refs.rs readable"),
    );
    let mut toml =
        std::fs::read_to_string(repo_root().join("registries/logical_object_kinds.toml"))
            .expect("kind table readable");
    mutate_toml(&mut toml);
    write_fixture(&root, "registries/logical_object_kinds.toml", &toml);
    root
}

#[test]
fn appendix_validate_logical_kind_mutations_are_seen_to_fire() {
    let codes_of = |root: &Path| -> Vec<String> {
        let registries =
            model::load_registries(&root.join("registries")).expect("scratch registries load");
        validate::validate_all(&registries, root)
            .into_iter()
            .map(|violation| violation.code)
            .collect::<Vec<_>>()
    };

    // active_logical_kind_none_parsed: zero active rows survive.
    let root = logical_kind_scratch("lk-none-parsed", |toml| {
        *toml = toml.replace("status = \"active\"", "status = \"zzz\"");
    });
    assert_code(&codes_of(&root), "active_logical_kind_none_parsed");

    // logical_kind_projection_layout: an active row's object_kind/name/status
    // stop being adjacent in that exact order.
    let root = logical_kind_scratch("lk-layout", |toml| {
        let needle = "\nobject_kind = 0x0001\nname = \"LogicalStatePayload\"\nstatus = \"active\"";
        assert!(
            toml.contains(needle),
            "layout witness anchor moved; re-derive from the real table"
        );
        *toml = toml.replacen(
            needle,
            "\nname = \"LogicalStatePayload\"\nobject_kind = 0x0001\nstatus = \"active\"",
            1,
        );
    });
    assert_code(&codes_of(&root), "logical_kind_projection_layout");

    // arm_without_active_logical_kind: one entire active row disappears while
    // refs.rs still declares its arm.
    let root = logical_kind_scratch("lk-arm-orphan", |toml| {
        let needle = "\nobject_kind = 0x0001\nname = \"LogicalStatePayload\"\nstatus = \"active\"";
        assert!(
            toml.contains(needle),
            "arm-orphan witness anchor moved; re-derive from the real table"
        );
        *toml = toml.replacen(needle, "", 1);
    });
    assert_code(&codes_of(&root), "arm_without_active_logical_kind");
}
