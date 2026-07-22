//! Identity-constitution suites (bead fgdb-g0-identity-registries-hrx).
//!
//! Named suites required by the bead's acceptance criteria:
//!   idr_schema_valid_all_six, idr_disjointness_no_dual_class,
//!   idr_code_space_retired_reuse_fails,
//!   idr_code_space_experimental_in_production_fails,
//!   idr_construction_dag_acyclic (+ negatives idr_neg_self_edge,
//!   idr_neg_mutual_edge, idr_neg_future_result_edge),
//!   idr_bodydigest_recipe_roundtrip, idr_neg_unregistered_field_unencodable,
//!   idr_reserved_w12_coverage, idr_reference_targets_resolve (property),
//!   idr_golden_vector_mutation (fuzz).
//!
//! Suites run against the REAL `registries/` identity artifacts plus
//! targeted in-memory mutations, so a defect in the shipped registries and a
//! defect in the checker are both build breaks.

use registry_check::appendix_a::{self, Catalog, Violation};
use registry_check::identity::{
    self, FieldRow, IdentityRegistries, LogicalKind, bodydigest_pin, bodydigest_transcript,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

fn real_identity() -> IdentityRegistries {
    identity::load_identity(&repo_root().join("registries")).expect("identity registries load")
}

fn real_appendix_catalog_text() -> String {
    std::fs::read_to_string(repo_root().join(appendix_a::CATALOG_PATH))
        .expect("Appendix A catalog is readable")
}

fn real_appendix_catalog() -> Catalog {
    appendix_a::parse_catalog(&real_appendix_catalog_text()).expect("Appendix A catalog parses")
}

fn real_plan_source() -> Vec<u8> {
    std::fs::read(repo_root().join(appendix_a::PLAN_PATH)).expect("plan source is readable")
}

fn source_range(source: &[u8], start_line: i64, end_line: i64) -> Vec<u8> {
    let skip = usize::try_from(start_line - 1).expect("positive source line");
    let take = usize::try_from(end_line - start_line + 1).expect("ordered source range");
    source
        .split_inclusive(|byte| *byte == b'\n')
        .skip(skip)
        .take(take)
        .flatten()
        .copied()
        .collect()
}

fn line_start_offset(source: &[u8], line: i64) -> usize {
    let preceding = usize::try_from(line - 1).expect("positive source line");
    source
        .split_inclusive(|byte| *byte == b'\n')
        .take(preceding)
        .map(<[u8]>::len)
        .sum()
}

fn has_violation(violations: &[Violation], code: &str, detail: &str) -> bool {
    violations
        .iter()
        .any(|violation| violation.code == code && violation.msg.contains(detail))
}

fn duplicate_slice(catalog: &mut Catalog) {
    catalog.slices[1].id = catalog.slices[0].id.clone();
}

fn reorder_slices(catalog: &mut Catalog) {
    catalog.slices.swap(0, 1);
}

fn gap_slices(catalog: &mut Catalog) {
    catalog.slices[1].start_line += 1;
}

fn off_by_one_manifest(catalog: &mut Catalog) {
    catalog.source_manifest.end_line -= 1;
}

fn wrong_slice_bead(catalog: &mut Catalog) {
    catalog.slices[10].bead_id.push_str("-wrong");
}

fn wrong_manifest_hash(catalog: &mut Catalog) {
    catalog.source_manifest.sha256.replace_range(0..1, "0");
}

fn wrong_slice_hash(catalog: &mut Catalog) {
    catalog.slices[10].sha256.replace_range(0..1, "0");
}

fn swap_first_two_table_blocks(source: &str, header: &str) -> String {
    let first = source.find(header).expect("first table block exists");
    let second = first
        + header.len()
        + source[first + header.len()..]
            .find(header)
            .expect("second table block exists");
    let third = second
        + header.len()
        + source[second + header.len()..]
            .find(header)
            .expect("third table block exists");

    let mut reordered = String::with_capacity(source.len());
    reordered.push_str(&source[..first]);
    reordered.push_str(&source[second..third]);
    reordered.push_str(&source[first..second]);
    reordered.push_str(&source[third..]);
    reordered
}

fn codes(r: &IdentityRegistries) -> Vec<String> {
    identity::validate_identity(r)
        .into_iter()
        .map(|v| v.code)
        .collect()
}

/// A synthetic field row with sane defaults for mutation fixtures.
fn field(containing: &str, tag: i64, name: &str, order: i64) -> FieldRow {
    FieldRow {
        containing_schema: containing.into(),
        field_tag: tag,
        stable_name: name.into(),
        exact_wire_type: "StrongRef".into(),
        cardinality: "one".into(),
        identity_class: "logical".into(),
        reference_semantics: "strong".into(),
        target_schema_id: None,
        construction_order: order,
        role_predicate: "true".into(),
        retention_and_cut_rule: "fixture".into(),
        version_status: "active".into(),
        max_size_bytes: 40,
        digest_class: None,
        transcript_recipe: None,
        bd_domain_separator: None,
        bd_schema_major: None,
        bd_included_field_tags: None,
        bd_excluded_field_tags: None,
        recipe_pin: None,
    }
}

fn kind(code: i64, name: &str, status: &str, order: i64) -> LogicalKind {
    LogicalKind {
        object_kind: code,
        name: name.into(),
        status: status.into(),
        construction_order: order,
        role_predicate: "true".into(),
        max_size_bytes: 4096,
        golden_corpus: "corpus/fixture/".into(),
    }
}

// ---------------------------------------------------------------------------
// Baseline.
// ---------------------------------------------------------------------------

#[test]
fn appendix_a_catalog_real_source_verifies_and_reconstructs() {
    let root = repo_root();
    let catalog = appendix_a::load_and_verify(&root).expect("real Appendix A source verifies");
    let source = real_plan_source();
    let appendix = source_range(
        &source,
        catalog.source_manifest.start_line,
        catalog.source_manifest.end_line,
    );

    assert_eq!(
        appendix.len(),
        usize::try_from(appendix_a::APPENDIX_BYTE_COUNT).expect("byte count fits usize")
    );
    assert_eq!(
        registry_check::hash::sha256_hex(&appendix),
        appendix_a::APPENDIX_SHA256
    );

    let mut reconstructed = Vec::with_capacity(appendix.len());
    for slice in &catalog.slices {
        let bytes = source_range(&source, slice.start_line, slice.end_line);
        assert_eq!(
            bytes.len(),
            usize::try_from(slice.byte_count).expect("slice byte count fits usize"),
            "{} byte count",
            slice.id
        );
        assert_eq!(
            registry_check::hash::sha256_hex(&bytes),
            slice.sha256,
            "{} source hash",
            slice.id
        );
        reconstructed.extend_from_slice(&bytes);
    }
    assert_eq!(
        reconstructed, appendix,
        "ordered slices reconstruct Appendix A"
    );
}

#[test]
fn appendix_a_catalog_parse_is_closed_and_versioned() {
    let source = real_appendix_catalog_text();
    appendix_a::parse_catalog(&source).expect("baseline catalog parses");

    let mutations = vec![
        (
            "unknown root",
            source.replacen(
                "schema_version = 1",
                "schema_version = 1\nunknown_root_key = true",
                1,
            ),
            "catalog_unknown_key",
            "unknown_root_key",
        ),
        (
            "unknown catalog key",
            source.replacen(
                "source_encoding = \"utf-8-lf\"",
                "source_encoding = \"utf-8-lf\"\nunknown_catalog_key = true",
                1,
            ),
            "catalog_unknown_key",
            "unknown_catalog_key",
        ),
        (
            "unknown source manifest key",
            source.replacen(
                "plan_path = \"COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md\"",
                "plan_path = \"COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md\"\nunknown_source_manifest_key = true",
                1,
            ),
            "catalog_unknown_key",
            "unknown_source_manifest_key",
        ),
        (
            "unknown slice key",
            source.replacen(
                "definition_status = \"declared\"",
                "definition_status = \"declared\"\nunknown_slice_key = true",
                1,
            ),
            "catalog_unknown_key",
            "unknown_slice_key",
        ),
        (
            "wrong schema version",
            source.replacen("schema_version = 1", "schema_version = 2", 1),
            "catalog_pin_mismatch",
            "schema_version",
        ),
        (
            "reordered projection epochs",
            swap_first_two_table_blocks(&source, "[[projection_epoch]]"),
            "projection_epoch_order",
            "expected registry",
        ),
        (
            "unknown projection epoch key",
            source.replacen(
                "registry_epoch = 1",
                "registry_epoch = 1\nunknown_projection_epoch_key = true",
                1,
            ),
            "catalog_unknown_key",
            "unknown_projection_epoch_key",
        ),
        (
            "unknown projection row key",
            source.replacen(
                "[[logical_kind]]",
                "[[logical_kind]]\nunknown_projection_row_key = true",
                1,
            ),
            "catalog_projection_schema",
            "unknown_projection_row_key",
        ),
        (
            "missing projection row metadata",
            source.replacen("slice_id = \"a03\"\n", "", 1),
            "catalog_schema",
            "slice_id",
        ),
    ];

    for (name, mutated, expected_code, expected_detail) in mutations {
        let violations = appendix_a::parse_catalog(&mutated)
            .expect_err("closed catalog mutation must be rejected");
        assert!(
            has_violation(&violations, expected_code, expected_detail),
            "{name} did not produce {expected_code}/{expected_detail}: {violations:?}"
        );
    }
}

#[test]
fn appendix_a_all_projection_row_schemas_reject_unknown_keys() {
    let source = real_appendix_catalog_text();
    for header in [
        "[[logical_kind]]",
        "[[physical_kind]]",
        "[[bootstrap_frame]]",
        "[[prebootstrap_kind]]",
        "[[wire_type]]",
        "[[field]]",
        "[[reference_union]]",
        "[[reference_union_arm]]",
    ] {
        let mutated = source.replacen(
            header,
            &format!("{header}\nunknown_projection_row_key = true"),
            1,
        );
        let violations = appendix_a::parse_catalog(&mutated)
            .expect_err("unknown projection-row key must fail closed");
        assert!(
            has_violation(
                &violations,
                "catalog_projection_schema",
                "unknown_projection_row_key"
            ),
            "{header} schema accepted an unknown key: {violations:?}"
        );
    }
}

#[test]
fn appendix_a_catalog_metadata_schemas_reject_unknown_keys() {
    let source = real_appendix_catalog_text();
    for (name, header) in [
        ("reservation", "[[reservation]]"),
        ("top-level candidate", "[[top_level_candidate]]"),
        ("target", "[[target]]"),
        ("source disposition", "[[source_symbol_disposition]]"),
    ] {
        let mutated = source.replacen(header, &format!("{header}\nunknown_metadata_key = true"), 1);
        let violations =
            appendix_a::parse_catalog(&mutated).expect_err("unknown metadata key must fail closed");
        assert!(
            has_violation(&violations, "catalog_unknown_key", "unknown_metadata_key"),
            "{name} schema accepted an unknown key: {violations:?}"
        );
    }

    let maintenance = source.replacen(
        "[maintenance_proof]",
        "[maintenance_proof]\nunknown_metadata_key = true",
        1,
    );
    let violations = appendix_a::parse_catalog(&maintenance)
        .expect_err("unknown maintenance-proof key must fail closed");
    assert!(has_violation(
        &violations,
        "catalog_unknown_key",
        "unknown_metadata_key"
    ));

    for (name, table) in [
        (
            "semantic binding",
            r#"
[[semantic_binding]]
row_id = "a01:semantic-binding:bootstrap-frame-root-slot"
target_row_id = "a01:bootstrap-frame:root-slot"
owner_bead_id = "fgdb-w10-fixture"
owner_crate = "fgdb-fixture"
consumer_crates = ["fgdb"]
unknown_metadata_key = true
"#,
        ),
        (
            "evidence",
            r#"
[[evidence]]
row_id = "a01:evidence:bootstrap-frame-root-slot-static-contract"
target_row_id = "a01:bootstrap-frame:root-slot"
evidence_id = "static-contract"
phase = "static"
status = "live"
owner_bead_id = "fgdb-a01-reference-roots-2k0q"
checker_ids = ["appendix_a_catalog_closure"]
scenario_ids = ["g0_identity_e2e"]
event_ids = ["appendix_closure_checked"]
gate_ids = ["G0"]
unknown_metadata_key = true
"#,
        ),
    ] {
        let mut mutated = source.clone();
        mutated.push_str(table);
        let violations = appendix_a::parse_catalog(&mutated)
            .expect_err("unknown metadata-row key must fail closed");
        assert!(
            has_violation(&violations, "catalog_unknown_key", "unknown_metadata_key"),
            "{name} schema accepted an unknown key: {violations:?}"
        );
    }

    let mut annotation = source;
    annotation.push_str(
        r#"

[[annotation]]
row_id = "a01:annotation:bootstrap-frame-root-slot"
target_row_id = "a01:bootstrap-frame:root-slot"
exact_type = "RootSlot"
cardinality = "one"
layout = "fixed"
role = "local"
posture = "bootstrap"
authority = "root"
locality = "local"
generic_expansions = []
role_expansions = []
reference_semantics = "embedded"
target_schema_ids = []
construction_order = "bootstrap-root-slot"
retention_and_cut_rule = "fixed-location"
digest_recipe = "slot-checksum"
redaction_class = "public-commitment"
resource_bounds = "fixed-4096-bytes"
compatibility = "v1"
unknown_metadata_key = true
"#,
    );
    let violations = appendix_a::parse_catalog(&annotation)
        .expect_err("unknown annotation key must fail closed");
    assert!(has_violation(
        &violations,
        "catalog_unknown_key",
        "unknown_metadata_key"
    ));
}

#[test]
fn appendix_a_catalog_projection_targets_are_exact_and_reservations_are_nonsemantic() {
    let baseline = real_appendix_catalog();
    assert!(
        appendix_a::appendix_a_catalog_closure(&baseline).is_empty(),
        "baseline metadata closure must be exact"
    );

    let mut missing_target = baseline.clone();
    missing_target.targets.remove(0);
    let violations = appendix_a::validate_catalog(&missing_target);
    assert!(has_violation(
        &violations,
        "catalog_projection_target_missing",
        "requires exactly one"
    ));

    let mut duplicate_target = baseline.clone();
    let mut duplicate = duplicate_target.targets[0].clone();
    duplicate.row_id.push_str("-duplicate");
    duplicate_target.targets.push(duplicate);
    let violations = appendix_a::validate_catalog(&duplicate_target);
    assert!(violations.iter().any(|violation| matches!(
        violation.code.as_str(),
        "catalog_target_duplicate" | "catalog_row_id_derived_mismatch"
    )));

    let mut self_target = baseline.clone();
    self_target.targets[0].target_row_id = self_target.targets[0].row_id.clone();
    let violations = appendix_a::validate_catalog(&self_target);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_target_self_reference")
    );

    let mut reservation_metadata = baseline.clone();
    let reservation = &reservation_metadata.reservations[0];
    reservation_metadata
        .semantic_bindings
        .push(appendix_a::SemanticBinding {
            row_id: format!(
                "{}:semantic-binding:reservation-{}",
                reservation.slice_id,
                reservation
                    .row_id
                    .split(':')
                    .nth(2)
                    .expect("reservation suffix")
            ),
            target_row_id: reservation.row_id.clone(),
            owner_bead_id: "fgdb-w10-fixture".to_owned(),
            owner_crate: "fgdb-fixture".to_owned(),
            consumer_crates: vec!["fgdb".to_owned()],
        });
    let violations = appendix_a::validate_catalog(&reservation_metadata);
    assert!(has_violation(
        &violations,
        "catalog_target_unresolved",
        "not a primary projection"
    ));
}

#[test]
fn appendix_a_catalog_maintenance_and_semantic_binding_contracts_are_distinct() {
    let baseline = real_appendix_catalog();
    let mut maintenance_owner = baseline.clone();
    maintenance_owner.maintenance_proof.owner_crate = "fgdb-warden".to_owned();
    let violations = appendix_a::validate_catalog(&maintenance_owner);
    assert!(violations
        .iter()
        .any(|violation| violation.code == "catalog_maintenance_proof_mismatch"));

    let target = baseline
        .targets
        .iter()
        .find(|row| row.slice_id != "g0")
        .expect("Appendix target")
        .clone();
    let suffix = target
        .target_row_id
        .split_once(':')
        .and_then(|(_, rest)| rest.split_once(':'))
        .map(|(kind, name)| format!("{kind}-{name}"))
        .expect("three-part target row ID");
    let valid = appendix_a::SemanticBinding {
        row_id: format!("{}:semantic-binding:{suffix}", target.slice_id),
        target_row_id: target.target_row_id,
        owner_bead_id: "fgdb-w10-fixture".to_owned(),
        owner_crate: "fgdb-warden".to_owned(),
        consumer_crates: vec!["fgdb".to_owned(), "fgdb-server".to_owned()],
    };

    let mut semantic = baseline.clone();
    semantic.semantic_bindings.push(valid.clone());
    assert!(
        appendix_a::validate_catalog(&semantic).is_empty(),
        "a declared target may carry a real semantic owner before slice completion"
    );

    let mut fake_owner = baseline.clone();
    let mut fake = valid.clone();
    fake.owner_crate = "registry-check".to_owned();
    fake_owner.semantic_bindings.push(fake);
    let violations = appendix_a::validate_catalog(&fake_owner);
    assert!(violations
        .iter()
        .any(|violation| violation.code == "catalog_semantic_owner_invalid"));

    let mut unsorted_consumers = baseline;
    let mut unsorted = valid;
    unsorted.consumer_crates = vec!["z".to_owned(), "a".to_owned()];
    unsorted_consumers.semantic_bindings.push(unsorted);
    let violations = appendix_a::validate_catalog(&unsorted_consumers);
    assert!(violations
        .iter()
        .any(|violation| violation.code == "catalog_metadata_order"));
}

#[test]
fn appendix_a_catalog_row_ids_and_g0_owners_are_release_pinned() {
    let baseline = real_appendix_catalog();

    let mut wrong_suffix = baseline.clone();
    wrong_suffix.projection_rows[0].row_id.push_str("-wrong");
    let violations = appendix_a::validate_catalog(&wrong_suffix);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_row_id_derived_mismatch")
    );

    let mut repeated_hyphen = baseline.clone();
    repeated_hyphen.projection_rows[0].row_id = repeated_hyphen.projection_rows[0]
        .row_id
        .replacen('-', "--", 1);
    let violations = appendix_a::validate_catalog(&repeated_hyphen);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_row_id_invalid")
    );

    let mut broadened_g0 = baseline;
    broadened_g0.projection_rows[0].slice_id = "g0".to_owned();
    broadened_g0.projection_rows[0].row_id = format!(
        "g0:{}:{}",
        broadened_g0.projection_rows[0].row_kind,
        broadened_g0.projection_rows[0]
            .row_id
            .split(':')
            .nth(2)
            .expect("row suffix")
    );
    let violations = appendix_a::validate_catalog(&broadened_g0);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "g0_projection_allowlist_drift")
    );
}

#[test]
fn appendix_a_catalog_reservation_and_source_census_is_exact() {
    let baseline = real_appendix_catalog();
    assert_eq!(
        appendix_a::reservation_assignment_sha256(&baseline.reservations),
        appendix_a::EXPECTED_RESERVATION_ASSIGNMENT_SHA256
    );

    let mut empty = baseline.clone();
    empty.reservations.clear();
    empty
        .source_symbol_dispositions
        .retain(|row| row.slice_id == "g0");
    let violations = appendix_a::validate_catalog(&empty);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_reservation_count")
    );

    let mut duplicate_code = baseline.clone();
    duplicate_code.reservations[1].code_reservation =
        duplicate_code.reservations[0].code_reservation.clone();
    let violations = appendix_a::validate_catalog(&duplicate_code);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_reservation_code_duplicate")
    );

    let mut malformed_code = baseline.clone();
    malformed_code.reservations[0].code_reservation = "0X0200".to_owned();
    let violations = appendix_a::validate_catalog(&malformed_code);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_reservation_code_invalid")
    );

    let mut reassigned_code = baseline.clone();
    reassigned_code
        .reservations
        .iter_mut()
        .find(|row| row.disposition == "reserved")
        .expect("reserved row exists")
        .code_reservation = "0x7ffe".to_owned();
    let violations = appendix_a::validate_catalog(&reassigned_code);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_reservation_assignment_drift")
    );

    let mut invalid_disposition = baseline.clone();
    let row = invalid_disposition
        .source_symbol_dispositions
        .iter_mut()
        .find(|row| row.slice_id != "g0")
        .expect("reference-target row exists");
    row.disposition = "unresolved".to_owned();
    let violations = appendix_a::validate_catalog(&invalid_disposition);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_disposition_invalid")
    );

    let mut bad_location = baseline.clone();
    let row = bad_location
        .source_symbol_dispositions
        .iter_mut()
        .find(|row| row.slice_id != "g0")
        .expect("census row exists");
    row.source_locations[0] = "a01:9999".to_owned();
    let violations = appendix_a::validate_catalog(&bad_location);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_source_location_invalid")
    );

    let mut unsorted_location = baseline;
    let row = unsorted_location
        .source_symbol_dispositions
        .iter_mut()
        .find(|row| row.slice_id != "g0" && row.source_locations.len() > 1)
        .expect("multi-location census row exists");
    row.source_locations.swap(0, 1);
    let violations = appendix_a::validate_catalog(&unsorted_location);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_source_location_order")
    );
}

#[test]
fn appendix_a_catalog_header_and_projection_order_are_canonical() {
    let baseline = real_appendix_catalog();
    let generated = appendix_a::generated_projections(&baseline);

    let mut reordered = baseline.clone();
    reordered.identity.logical.swap(0, 1);
    reordered.identity.fields.swap(0, 1);
    reordered.identity.unions[0].arms.swap(0, 1);
    assert_eq!(
        appendix_a::generated_projections(&reordered),
        generated,
        "renderer must canonicalize in-memory row order"
    );

    let mut headers = Vec::new();
    let mut catalog_epoch = baseline.clone();
    catalog_epoch.catalog_epoch += 1;
    headers.push(catalog_epoch);
    let mut row_grammar = baseline.clone();
    row_grammar.row_id_grammar_version += 1;
    headers.push(row_grammar);
    let mut diagnostic = baseline.clone();
    diagnostic.diagnostic_version += 1;
    headers.push(diagnostic);
    let mut order = baseline;
    order.canonical_order = "different".to_owned();
    headers.push(order);
    for catalog in headers {
        let violations = appendix_a::validate_catalog(&catalog);
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "catalog_pin_mismatch")
        );
    }
}

#[test]
fn appendix_a_catalog_manifest_mutations_fail_closed() {
    type Mutation = fn(&mut Catalog);
    let cases: [(&str, Mutation, &str); 7] = [
        ("duplicate slice", duplicate_slice, "slice_duplicate"),
        ("reordered slices", reorder_slices, "catalog_pin_mismatch"),
        ("gapped slices", gap_slices, "slice_range_mismatch"),
        (
            "off-by-one manifest",
            off_by_one_manifest,
            "source_manifest_range_mismatch",
        ),
        ("wrong Bead", wrong_slice_bead, "catalog_pin_mismatch"),
        (
            "wrong manifest hash",
            wrong_manifest_hash,
            "catalog_pin_mismatch",
        ),
        ("wrong slice hash", wrong_slice_hash, "catalog_pin_mismatch"),
    ];

    for (name, mutate, expected_code) in cases {
        let mut catalog = real_appendix_catalog();
        mutate(&mut catalog);
        let violations = appendix_a::validate_catalog(&catalog);
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == expected_code),
            "{name} did not produce {expected_code}: {violations:?}"
        );
    }
}

#[test]
fn appendix_a_every_slice_pin_rejects_independent_mutation() {
    let baseline = real_appendix_catalog();
    assert_eq!(baseline.slices.len(), appendix_a::SLICE_PINS.len());

    for (index, pin) in appendix_a::SLICE_PINS.iter().enumerate() {
        let mut wrong_bead = baseline.clone();
        wrong_bead.slices[index].bead_id.push_str("-wrong");
        let violations = appendix_a::validate_catalog(&wrong_bead);
        assert!(
            violations.iter().any(|violation| {
                violation.code == "catalog_pin_mismatch" && violation.row_id == pin.id
            }),
            "{} accepted an independently mutated Bead pin: {violations:?}",
            pin.id
        );

        let mut wrong_range = baseline.clone();
        wrong_range.slices[index].start_line += 1;
        let violations = appendix_a::validate_catalog(&wrong_range);
        assert!(
            violations.iter().any(|violation| {
                violation.code == "catalog_pin_mismatch" && violation.row_id == pin.id
            }),
            "{} accepted an independently mutated range pin: {violations:?}",
            pin.id
        );

        let mut wrong_hash = baseline.clone();
        let replacement = if wrong_hash.slices[index].sha256.starts_with('0') {
            "1"
        } else {
            "0"
        };
        wrong_hash.slices[index]
            .sha256
            .replace_range(0..1, replacement);
        let violations = appendix_a::validate_catalog(&wrong_hash);
        assert!(
            violations.iter().any(|violation| {
                violation.code == "catalog_pin_mismatch" && violation.row_id == pin.id
            }),
            "{} accepted an independently mutated hash pin: {violations:?}",
            pin.id
        );
    }
}

#[test]
fn appendix_a_complete_slice_requires_full_source_target_and_evidence_closure() {
    let mut catalog = real_appendix_catalog();
    let slice = catalog
        .slices
        .iter_mut()
        .find(|slice| slice.id == "a02")
        .expect("A02 exists");
    slice.definition_status = "complete".to_owned();

    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| matches!(
                violation.code.as_str(),
                "complete_slice_ambiguity"
                    | "complete_slice_target_declared"
                    | "slice_census_pin_mismatch"
            )),
        "vacuously complete A02 did not expose unresolved source coverage: {violations:?}"
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "complete_slice_annotation_missing"),
        "vacuously complete A02 did not require exact annotations: {violations:?}"
    );
    assert!(
        violations.iter().any(|violation| matches!(
            violation.code.as_str(),
            "complete_slice_semantic_binding_missing"
                | "complete_slice_static_evidence_missing"
                | "complete_slice_runtime_evidence_missing"
        )),
        "vacuously complete A02 did not require real owner/evidence closure: {violations:?}"
    );

    let mut class_drift = real_appendix_catalog();
    class_drift.slices[1].expected_projection_classes.swap(0, 1);
    let violations = appendix_a::validate_catalog(&class_drift);
    assert!(
        violations
            .iter()
            .any(|violation| { violation.code == "slice_projection_class_assignment_drift" }),
        "slice projection-class assignment/order drift was not release-pinned: {violations:?}"
    );
}

#[test]
fn appendix_a_catalog_raw_source_mutations_fail_closed() {
    let catalog = real_appendix_catalog();
    let source = real_plan_source();
    let appendix_start = line_start_offset(&source, appendix_a::APPENDIX_START_LINE);

    let mut cr = source.clone();
    cr.insert(appendix_start, b'\r');

    let mut byte_mutation = source.clone();
    byte_mutation[appendix_start] = b'!';

    let mut truncated = source.clone();
    truncated.truncate(line_start_offset(&source, appendix_a::APPENDIX_END_LINE));

    for (name, mutated, expected_code) in [
        ("carriage return", cr, "source_encoding"),
        ("source byte", byte_mutation, "source_sha256_mismatch"),
        ("truncation", truncated, "source_range_missing"),
    ] {
        let violations = appendix_a::verify_source(&catalog, &mutated);
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == expected_code),
            "{name} did not produce {expected_code}: {violations:?}"
        );
    }
}

#[test]
fn appendix_a_strong_ref_and_definition_census_is_exact() {
    let source = real_plan_source();
    let strong_refs = appendix_a::strong_ref_census(&source);
    assert_eq!(
        strong_refs.families.len(),
        appendix_a::EXPECTED_TYPE_RESERVATION_COUNT
    );
    assert_eq!(
        strong_refs.family_sha256,
        appendix_a::EXPECTED_SOURCE_FAMILY_SHA256
    );
    assert_eq!(
        strong_refs.location_pair_count,
        appendix_a::EXPECTED_SOURCE_LOCATION_PAIR_COUNT
    );
    assert_eq!(
        strong_refs.location_sha256,
        appendix_a::EXPECTED_SOURCE_LOCATION_SHA256
    );

    let definitions = appendix_a::definition_census(&source, &strong_refs);
    assert_eq!(
        definitions.first_locations.len(),
        appendix_a::EXPECTED_DEFINED_SOURCE_FAMILY_COUNT
    );
    assert_eq!(
        definitions.family_sha256,
        appendix_a::EXPECTED_DEFINED_SOURCE_FAMILY_SHA256
    );
    assert_eq!(
        definitions.location_sha256,
        appendix_a::EXPECTED_DEFINITION_LOCATION_SHA256
    );
}

#[test]
fn appendix_a_audit_outcome_uses_family_ref_plus_required_arm_predicate() {
    let source = String::from_utf8(real_plan_source()).expect("plan source is UTF-8");
    assert!(
        !source.contains("StrongRef<AuditTerminalAttemptRecord::VisibilityReleased>"),
        "variant-qualified StrongRef contradicts the Appendix reference law"
    );
    assert!(
        source.contains("terminal_attempt_visible_ref:StrongRef<AuditTerminalAttemptRecord>"),
        "AuditOutcomeRecord must reference the registered family"
    );
    assert!(
        source.contains("mandatory exact `VisibilityReleased` required-arm predicate"),
        "AuditOutcomeRecord must pin the required variant separately"
    );
}

#[test]
fn appendix_a_scaffold_metadata_generation_is_frozen_and_reproducible() {
    let catalog_text = real_appendix_catalog_text();
    let catalog = appendix_a::parse_catalog(&catalog_text).expect("complete catalog parses");
    let generated = appendix_a::generated_scaffold_metadata(&catalog, &real_plan_source())
        .expect("scaffold metadata regenerates");
    assert_eq!(
        generated.len(),
        appendix_a::EXPECTED_SCAFFOLD_METADATA_BYTE_COUNT
    );
    assert_eq!(
        registry_check::hash::sha256_hex(generated.as_bytes()),
        appendix_a::EXPECTED_SCAFFOLD_METADATA_SHA256
    );
    let marker = concat!(
        "\n# =============================================================================\n",
        "# Generated G0 Appendix-A type census and exact ownership/evidence closure."
    );
    let start = catalog_text
        .find(marker)
        .expect("generated scaffold marker is checked in");
    assert_eq!(
        &catalog_text[start..],
        generated,
        "checked-in metadata suffix must equal deterministic generation"
    );
}

#[test]
fn appendix_a_public_scaffold_generator_rejects_malformed_catalogs_without_panicking() {
    let source = real_plan_source();

    let mut unknown_projection = real_appendix_catalog();
    let row = unknown_projection
        .projection_rows
        .iter_mut()
        .find(|row| row.slice_id == "g0")
        .expect("G0 projection exists");
    row.projection = "unknown_projection".to_owned();
    let violations = appendix_a::generated_scaffold_metadata(&unknown_projection, &source)
        .expect_err("unknown projection must be a typed error");
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_projection_registry_unknown")
    );

    let mut malformed_row_id = real_appendix_catalog();
    let row = malformed_row_id
        .projection_rows
        .iter_mut()
        .find(|row| row.slice_id == "g0")
        .expect("G0 projection exists");
    row.row_id = "malformed".to_owned();
    let violations = appendix_a::generated_scaffold_metadata(&malformed_row_id, &source)
        .expect_err("malformed public row ID must be a typed error");
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_row_id_invalid")
    );

    let mut malformed_reservation = real_appendix_catalog();
    malformed_reservation.reservations[0].code_reservation = "not-a-code".to_owned();
    let violations = appendix_a::generated_scaffold_metadata(&malformed_reservation, &source)
        .expect_err("malformed released reservation must be a typed error");
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_reservation_code_invalid")
    );
}

#[test]
fn appendix_a_catalog_projections_are_deterministic_and_round_trip() {
    let catalog = real_appendix_catalog();
    let generated = appendix_a::generated_projections(&catalog);
    assert_eq!(
        generated,
        appendix_a::generated_projections(&catalog),
        "repeated projection generation must be byte-identical"
    );

    let actual_files: Vec<&str> = generated.iter().map(|(file, _)| file.as_str()).collect();
    let expected_files = vec![
        "logical_object_kinds.toml",
        "physical_record_kinds.toml",
        "bootstrap_frames.toml",
        "prebootstrap_artifact_kinds.toml",
        "wire_types.toml",
        "durable_fields.toml",
    ];
    assert_eq!(actual_files, expected_files, "exactly six projections");

    for (file, source) in generated {
        let table = registry_check::toml::parse(&source).expect("generated projection parses");
        match file.as_str() {
            "logical_object_kinds.toml" => {
                let (epoch, rows) = identity::logical_from(&table).expect("logical projection");
                assert_eq!(epoch, catalog.identity.logical_epoch);
                assert_eq!(rows, catalog.identity.logical);
            }
            "physical_record_kinds.toml" => {
                let (epoch, rows) = identity::physical_from(&table).expect("physical projection");
                assert_eq!(epoch, catalog.identity.physical_epoch);
                assert_eq!(rows, catalog.identity.physical);
            }
            "bootstrap_frames.toml" => {
                let (epoch, rows) = identity::bootstrap_from(&table).expect("bootstrap projection");
                assert_eq!(epoch, catalog.identity.bootstrap_epoch);
                assert_eq!(rows, catalog.identity.bootstrap);
            }
            "prebootstrap_artifact_kinds.toml" => {
                let (epoch, rows) =
                    identity::prebootstrap_from(&table).expect("prebootstrap projection");
                assert_eq!(epoch, catalog.identity.prebootstrap_epoch);
                assert_eq!(rows, catalog.identity.prebootstrap);
            }
            "wire_types.toml" => {
                let (epoch, rows) = identity::wire_from(&table).expect("wire projection");
                assert_eq!(epoch, catalog.identity.wire_epoch);
                assert_eq!(rows, catalog.identity.wire);
            }
            "durable_fields.toml" => {
                let (epoch, fields, unions) =
                    identity::fields_from(&table).expect("durable-field projection");
                assert_eq!(epoch, catalog.identity.fields_epoch);
                assert_eq!(fields, catalog.identity.fields);
                assert_eq!(unions, catalog.identity.unions);
            }
            // The exact filename assertion above proves this arm unreachable;
            // keep the match total without introducing a test-only panic site.
            _ => {}
        }
    }
}

#[test]
fn appendix_a_catalog_real_projections_match_generated() {
    let catalog = real_appendix_catalog();
    let violations = appendix_a::verify_projections(&repo_root(), &catalog);
    assert!(
        violations.is_empty(),
        "checked-in projections must equal generated bytes: {violations:?}"
    );
}

#[test]
fn appendix_a_catalog_projection_diff_is_deterministic_and_located() {
    let root = repo_root();
    let mut catalog = real_appendix_catalog();
    assert!(
        appendix_a::verify_projections(&root, &catalog).is_empty(),
        "baseline projections must be normalized before the mutation assertion"
    );

    catalog.identity.logical[0].max_size_bytes += 1;
    let first = appendix_a::verify_projections(&root, &catalog);
    let second = appendix_a::verify_projections(&root, &catalog);
    assert_eq!(first, second, "projection divergence must be deterministic");
    assert_eq!(first.len(), 1, "one logical-row mutation changes one file");
    let violation = &first[0];
    assert_eq!(violation.code, "projection_byte_diff");
    assert_eq!(violation.row_id, "logical_object_kinds.toml");
    for coordinate in ["byte ", "line ", "column "] {
        assert!(
            violation.msg.contains(coordinate),
            "diff omits {coordinate:?}: {violation:?}"
        );
    }
}

#[test]
fn idr_schema_valid_all_six() {
    let r = real_identity();
    let violations = identity::validate_identity(&r);
    assert!(
        violations.is_empty(),
        "shipped identity registries must validate cleanly: {violations:?}"
    );
    // Sanity on the seeded corpus shape.
    assert!(r.logical.len() >= 20, "logical spine seeded");
    assert!(r.physical.len() >= 6, "physical pipeline seeded");
    assert_eq!(r.bootstrap.len(), 2, "RootSlot + reserved RaftHardFrame");
    assert!(
        r.prebootstrap.len() >= 5,
        "prebootstrap artifact classes seeded"
    );
    assert!(r.fields.len() >= 40, "durable_fields cross-index seeded");
    // The four §5.1-required generated-union exemplars are present.
    let unions: BTreeSet<&str> = r.unions.iter().map(|u| u.union_name.as_str()).collect();
    for required in [
        "LocalCommandInputRef",
        "MetaAppliedResultRef",
        "ShardProtocolEvidenceRef",
        "MandatoryInventoryRef",
    ] {
        assert!(
            unions.contains(required),
            "missing required union exemplar {required}"
        );
    }
}

#[test]
fn idr_schema_rejects_unknown_keys_and_versions() {
    let source = std::fs::read_to_string(repo_root().join("registries/logical_object_kinds.toml"))
        .expect("read logical registry");

    let wrong_version = source.replacen("schema_version = 1", "schema_version = 2", 1);
    let table = registry_check::toml::parse(&wrong_version).expect("fixture parses");
    let err = identity::logical_from(&table).expect_err("unknown schema version must fail");
    assert_eq!(err.path, "logical_object_kinds.toml.schema_version");
    assert!(err.msg.contains("expected schema version 1"));

    let unknown_root = source.replacen("[registry]", "unknown_top_level = true\n\n[registry]", 1);
    let table = registry_check::toml::parse(&unknown_root).expect("fixture parses");
    let err = identity::logical_from(&table).expect_err("unknown root key must fail");
    assert_eq!(err.path, "logical_object_kinds.toml.unknown_top_level");

    let unknown_registry =
        source.replacen("[registry]", "[registry]\nunknown_registry_key = true", 1);
    let table = registry_check::toml::parse(&unknown_registry).expect("fixture parses");
    let err = identity::logical_from(&table).expect_err("unknown registry key must fail");
    assert_eq!(
        err.path,
        "logical_object_kinds.toml.registry.unknown_registry_key"
    );

    let unknown_row = source.replacen("[[kind]]", "[[kind]]\nunknown_row_key = true", 1);
    let table = registry_check::toml::parse(&unknown_row).expect("fixture parses");
    let err = identity::logical_from(&table).expect_err("unknown row key must fail");
    assert_eq!(
        err.path,
        "logical_object_kinds.toml.kind[0].unknown_row_key"
    );
}

// ---------------------------------------------------------------------------
// Disjointness.
// ---------------------------------------------------------------------------

#[test]
fn idr_disjointness_no_dual_class() {
    let r = real_identity();
    assert!(!codes(&r).contains(&"disjointness_dual_class".to_string()));
    // Mutation: registering a bootstrap frame's name as a logical kind must
    // fail — no schema may inhabit two identity classes.
    let mut mutated = r.clone();
    mutated.logical.push(kind(0x7001, "RootSlot", "active", 50));
    assert!(
        codes(&mutated).contains(&"disjointness_dual_class".to_string()),
        "dual-class schema must be rejected"
    );
}

// ---------------------------------------------------------------------------
// Code-space laws.
// ---------------------------------------------------------------------------

#[test]
fn idr_code_space_retired_reuse_fails() {
    let mut r = real_identity();
    // Retire a code, then attempt to reassign it: a released code is never
    // reassigned, so the duplicate fails even against a retired row.
    r.logical
        .push(kind(0x7002, "RetiredExemplar", "retired", 10));
    r.logical.push(kind(0x7002, "ReuseAttempt", "active", 10));
    let codes = codes(&r);
    assert!(
        codes.contains(&"code_duplicate".to_string()),
        "retired-code reuse must fail, got {codes:?}"
    );
    // Boundary codes are permanently invalid.
    let mut boundary = real_identity();
    boundary
        .logical
        .push(kind(0xffff, "InvalidCode", "active", 10));
    assert!(codes_of(&boundary).contains(&"code_invalid".to_string()));
}

#[test]
fn idr_assignment_history_and_epoch_are_frozen() {
    let r = real_identity();
    for pin in identity::assignment_pins(&r) {
        assert_eq!(
            pin.actual_epoch, pin.expected_epoch,
            "{} epoch drift",
            pin.registry
        );
        assert_eq!(
            pin.actual_pin, pin.expected_pin,
            "{} pin drift",
            pin.registry
        );
    }

    // A delete-and-reuse mutation can be internally duplicate-free; the
    // independent released-assignment witness must still reject it.
    let mut reassigned = r.clone();
    let released_code = reassigned.logical[0].object_kind;
    reassigned.logical.remove(0);
    reassigned
        .logical
        .push(kind(released_code, "ReuseAfterDeletion", "active", 30));
    assert!(
        codes(&reassigned).contains(&"registry_assignment_drift".to_string()),
        "delete-and-reuse must fail against released history"
    );

    let mut epoch_only = r.clone();
    epoch_only.logical_epoch += 1;
    assert!(
        codes(&epoch_only).contains(&"registry_epoch_mismatch".to_string()),
        "epoch may not change without a reviewed assignment update"
    );

    let mut missing_arm = r.clone();
    missing_arm.unions[0].arms.pop();
    assert!(
        codes(&missing_arm).contains(&"registry_assignment_drift".to_string()),
        "missing closed-union arm must fail the released manifest"
    );
}

fn codes_of(r: &IdentityRegistries) -> Vec<String> {
    codes(r)
}

#[test]
fn idr_code_space_experimental_in_production_fails() {
    // An experimental-range row in the shipped (production) registry fails.
    let mut r = real_identity();
    r.logical
        .push(kind(0xc001, "ExperimentalProbe", "experimental", 10));
    let codes = codes(&r);
    assert!(
        codes.contains(&"experimental_in_production".to_string()),
        "experimental row must be rejected in production, got {codes:?}"
    );
    // Range/status coherence both ways.
    let mut wrong_status = real_identity();
    wrong_status
        .logical
        .push(kind(0xc002, "RangeButNotStatus", "active", 10));
    assert!(codes_of(&wrong_status).contains(&"range_status_mismatch".to_string()));
    let mut wrong_range = real_identity();
    wrong_range
        .logical
        .push(kind(0x7003, "StatusButNotRange", "experimental", 10));
    assert!(codes_of(&wrong_range).contains(&"range_status_mismatch".to_string()));
}

// ---------------------------------------------------------------------------
// Construction DAG.
// ---------------------------------------------------------------------------

#[test]
fn idr_construction_dag_acyclic() {
    let r = real_identity();
    let violations = identity::validate_identity(&r);
    assert!(
        !violations.iter().any(|v| v.code.starts_with("dag_")),
        "shipped construction DAG must be clean: {violations:?}"
    );
}

#[test]
fn idr_neg_self_edge() {
    let mut r = real_identity();
    let mut f = field("LogicalStatePayload", 90, "self_ref", 20);
    f.target_schema_id = Some("LogicalStatePayload".into());
    r.fields.push(f);
    let codes = codes(&r);
    assert!(
        codes.contains(&"dag_self_edge".to_string()),
        "self-edge must be rejected, got {codes:?}"
    );
}

#[test]
fn idr_neg_mutual_edge() {
    let mut r = real_identity();
    // CommitCommand -> ControlCommand -> CommitCommand (same order 10, so
    // no future-result fault masks the cycle).
    let mut a = field("CommitCommand", 90, "to_control", 10);
    a.target_schema_id = Some("ControlCommand".into());
    let mut b = field("ControlCommand", 90, "to_commit", 10);
    b.target_schema_id = Some("CommitCommand".into());
    r.fields.push(a);
    r.fields.push(b);
    let codes = codes(&r);
    assert!(
        codes.contains(&"dag_cycle".to_string()),
        "mutual cycle must be rejected, got {codes:?}"
    );
}

#[test]
fn idr_neg_future_result_edge() {
    let mut r = real_identity();
    // A command input naming its own future applied record: the canonical
    // future-result fault (FG-INV-07).
    let mut f = field("CommitCommand", 91, "my_applied_record", 10);
    f.target_schema_id = Some("LogicalCommandRecord".into());
    r.fields.push(f);
    let codes = codes(&r);
    assert!(
        codes.contains(&"dag_future_result".to_string()),
        "future-result edge must be rejected, got {codes:?}"
    );
}

// ---------------------------------------------------------------------------
// BodyDigest recipe discipline.
// ---------------------------------------------------------------------------

#[test]
fn idr_bodydigest_recipe_roundtrip() {
    let r = real_identity();
    // Every shipped BodyDigest row: recipe transcript is deterministic and
    // the pinned FNV drift pin recomputes exactly.
    let mut body_rows = 0;
    for f in r
        .fields
        .iter()
        .filter(|f| matches!(f.digest_class.as_deref(), Some("body")))
    {
        body_rows += 1;
        let transcript = bodydigest_transcript(
            &f.containing_schema,
            f.bd_domain_separator.as_deref().expect("domain"),
            f.bd_schema_major.expect("major"),
            f.bd_included_field_tags.as_deref().expect("included"),
            f.bd_excluded_field_tags.as_deref().expect("excluded"),
        );
        assert_eq!(
            bodydigest_pin(&transcript),
            *f.recipe_pin.as_ref().expect("pin"),
            "recipe pin drift on {}#{}",
            f.containing_schema,
            f.stable_name
        );
        // Determinism: recomputation is bit-stable.
        let again = bodydigest_transcript(
            &f.containing_schema,
            f.bd_domain_separator.as_deref().expect("domain"),
            f.bd_schema_major.expect("major"),
            f.bd_included_field_tags.as_deref().expect("included"),
            f.bd_excluded_field_tags.as_deref().expect("excluded"),
        );
        assert_eq!(transcript, again);
    }
    assert!(body_rows >= 6, "the §5.1-named BodyDigest rows are seeded");

    // Mutations against one generated recipe:
    // (a) unknown exclusion tag
    let mut unknown = real_identity();
    for f in &mut unknown.fields {
        if f.containing_schema == "AuthorityBindingRecord" && f.stable_name == "body_digest" {
            f.bd_excluded_field_tags = Some(vec![11, 99]);
        }
    }
    assert!(codes(&unknown).contains(&"bodydigest_unknown_exclusion".to_string()));
    // (b) two BodyDigest fields in one schema
    let mut two = real_identity();
    let mut second = field("AuthorityBindingRecord", 12, "second_body_digest", 10);
    second.exact_wire_type = "digest256".into();
    second.identity_class = "scalar".into();
    second.reference_semantics = "none".into();
    second.digest_class = Some("body".into());
    second.bd_domain_separator = Some("fgdb:body:second:v1".into());
    second.bd_schema_major = Some(1);
    second.bd_included_field_tags = Some(vec![]);
    second.bd_excluded_field_tags = Some(vec![12]);
    second.recipe_pin = Some(bodydigest_pin(&bodydigest_transcript(
        "AuthorityBindingRecord",
        "fgdb:body:second:v1",
        1,
        &[],
        &[12],
    )));
    two.fields.push(second);
    assert!(codes(&two).contains(&"bodydigest_two_fields".to_string()));
    // (c) self-including computation
    let mut selfinc = real_identity();
    for f in &mut selfinc.fields {
        if f.containing_schema == "AuthorityBindingRecord" && f.stable_name == "body_digest" {
            f.bd_excluded_field_tags = Some(vec![]);
        }
    }
    assert!(codes(&selfinc).contains(&"bodydigest_self_included".to_string()));
    // (d) pin drift
    let mut drift = real_identity();
    for f in &mut drift.fields {
        if f.containing_schema == "AuthorityBindingRecord" && f.stable_name == "body_digest" {
            f.recipe_pin = Some("fnv1a64:0000000000000000".into());
        }
    }
    assert!(codes(&drift).contains(&"bodydigest_pin_mismatch".to_string()));
}

// ---------------------------------------------------------------------------
// Encodability: a field absent from the table is unencodable.
// ---------------------------------------------------------------------------

#[test]
fn idr_neg_unregistered_field_unencodable() {
    let r = real_identity();
    // Registered fields are encodable.
    let ok = identity::check_encodable(
        &r,
        "LogicalCommandRecord",
        &["logical_command_seq", "origin", "command"],
    );
    assert!(ok.is_empty(), "registered fields must be encodable: {ok:?}");
    // An English-named but unregistered field must be unencodable.
    let bad = identity::check_encodable(
        &r,
        "LogicalCommandRecord",
        &["logical_command_seq", "plausible_english_named_field"],
    );
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0].code, "unregistered_field");
    assert!(bad[0].msg.contains("plausible_english_named_field"));
}

// ---------------------------------------------------------------------------
// Reserved W12 kinds and role-tagged variants.
// ---------------------------------------------------------------------------

#[test]
fn idr_reserved_w12_coverage() {
    let r = real_identity();
    let by_name: std::collections::BTreeMap<&str, &LogicalKind> =
        r.logical.iter().map(|k| (k.name.as_str(), k)).collect();
    // §19 G0: every reserved W12 kind and role-tagged Raft/root/checkpoint
    // variant lands now, implementation trailing (a05-a08 populate schemas).
    for name in [
        "RaftSnapshotLocal",
        "RaftSnapshotMeta",
        "RaftSnapshotShard",
        "RootManifestMeta",
        "RootManifestShard",
        "CheckpointStateVectorMeta",
        "CheckpointStateVectorShard",
        "MetaAuthorityBindingProjection",
        "ShardAuthorityBindingProjection",
        "MetaAppliedResult",
        "ShardProtocolEvidence",
        "ShardHistoryInventory",
        "GlobalKeyEnvelopeManifest",
    ] {
        let k = by_name.get(name).expect("reserved kind must be present");
        assert_eq!(k.status, "reserved", "{name} must be status reserved");
    }
    // The reserved bootstrap frame and the restore artifact classes.
    assert!(
        r.bootstrap
            .iter()
            .any(|f| f.name == "RaftHardFrame" && f.status == "reserved"),
        "RaftHardFrame frame reservation missing"
    );
    assert!(
        r.prebootstrap.iter().all(|k| k.status == "reserved"),
        "prebootstrap artifact classes are reserved pending a17-a21"
    );
}

// ---------------------------------------------------------------------------
// Property: every reference-union arm and reference target resolves to a
// live logical row — and removal of any referenced row is caught.
// ---------------------------------------------------------------------------

#[test]
fn idr_reference_targets_resolve() {
    let r = real_identity();
    // Compute, from the model itself, which kinds are load-bearing: they
    // carry field rows, are named as a field target, or appear as union arms.
    let mut load_bearing: BTreeSet<&str> = BTreeSet::new();
    for f in &r.fields {
        load_bearing.insert(f.containing_schema.as_str());
        if let Some(t) = &f.target_schema_id {
            load_bearing.insert(t.as_str());
        }
    }
    for u in &r.unions {
        load_bearing.insert(u.containing_schema.as_str());
        for arm in &u.arms {
            load_bearing.insert(arm.target_schema_id.as_str());
        }
    }
    // Exhaustive single-removal property over every logical kind.
    for victim in r.logical.iter().map(|k| k.name.clone()).collect::<Vec<_>>() {
        let mut mutated = r.clone();
        mutated.logical.retain(|k| k.name != victim);
        let violations = identity::validate_identity(&mutated);
        let resolution_fault = violations.iter().any(|v| {
            matches!(
                v.code.as_str(),
                "union_arm_unresolved" | "ref_target_unresolved" | "field_unresolved_schema"
            )
        });
        if load_bearing.contains(victim.as_str()) {
            assert!(
                resolution_fault,
                "removing load-bearing kind {victim:?} must break resolution; got {violations:?}"
            );
        } else {
            assert!(
                violations
                    .iter()
                    .all(|violation| violation.code == "registry_assignment_drift"),
                "removing a leaf kind may only trip the immutable assignment witness; got {violations:?}"
            );
        }
    }
}

#[test]
fn idr_reference_union_role_and_arm_closure() {
    let r = real_identity();
    assert!(
        !identity::validate_identity(&r)
            .iter()
            .any(|v| v.code.starts_with("union_")),
        "shipped reference unions must be role- and lifecycle-closed"
    );

    let mut invalid_role = r.clone();
    invalid_role.unions[0].role = "global".into();
    assert!(
        codes(&invalid_role).contains(&"union_role_invalid".to_string()),
        "unknown union role must fail"
    );

    let mut mismatched_arm = r.clone();
    mismatched_arm.unions[0].arms[0].role = "meta".into();
    assert!(
        codes(&mismatched_arm).contains(&"union_arm_metadata_mismatch".to_string()),
        "arm metadata must exactly close over its union"
    );

    let mut empty = r.clone();
    empty.unions[0].arms.clear();
    assert!(
        codes(&empty).contains(&"union_arm_missing".to_string()),
        "closed union with a missing inventory must fail"
    );

    let mut retired_target = r.clone();
    let target = retired_target.unions[0].arms[0].target_schema_id.clone();
    retired_target
        .logical
        .iter_mut()
        .find(|row| row.name == target)
        .expect("arm target exists")
        .status = "retired".into();
    assert!(
        codes(&retired_target).contains(&"union_arm_lifecycle_mismatch".to_string()),
        "retired targets are not live reference-union arms"
    );
}

// ---------------------------------------------------------------------------
// Fuzz: mutated registry bytes and drifted recipe vectors fail closed,
// naming the exact failing recipe.
// ---------------------------------------------------------------------------

fn replace_first_assignment(source: &str, key: &str, replacement: &str) -> String {
    let needle = format!("{key} = ");
    let start = source.find(&needle).expect("assignment exists") + needle.len();
    let end = source[start..]
        .find('\n')
        .map(|offset| start + offset)
        .unwrap_or(source.len());
    let mut mutated = source.to_string();
    mutated.replace_range(start..end, replacement);
    mutated
}

#[test]
fn idr_golden_vector_mutation() {
    let root = repo_root();

    // (a) Bit-flipped recipe "golden vectors": flipping any bit of a pinned
    // recipe pin must be caught, and the violation names the exact row.
    let r = real_identity();
    let body_rows: Vec<(String, String)> = r
        .fields
        .iter()
        .filter(|f| matches!(f.digest_class.as_deref(), Some("body")))
        .map(|f| (f.containing_schema.clone(), f.stable_name.clone()))
        .collect();
    for (row_index, (schema, name)) in body_rows.iter().enumerate() {
        let mut mutated = r.clone();
        for f in &mut mutated.fields {
            if &f.containing_schema == schema && &f.stable_name == name {
                let pin = f.recipe_pin.clone().expect("pin");
                // Flip one hex nibble deterministically.
                let mut bytes = pin.into_bytes();
                let idx = bytes.len() - 1 - (row_index % 8);
                bytes[idx] = if bytes[idx] == b'0' { b'1' } else { b'0' };
                f.recipe_pin = Some(String::from_utf8(bytes).expect("ascii pin"));
            }
        }
        let violations = identity::validate_identity(&mutated);
        let hit = violations
            .iter()
            .find(|v| v.code == "bodydigest_pin_mismatch");
        let hit = hit.expect("pin flip must be caught");
        assert_eq!(
            hit.row_id,
            format!("{schema}#{name}"),
            "violation must name the exact failing recipe"
        );
    }

    // (b) Semantically targeted byte mutations in every identity registry
    // must parse into a rejected model. This avoids the old false-positive
    // loop that silently accepted mutations landing in comments/whitespace.
    let read = |name: &str| {
        std::fs::read_to_string(root.join("registries").join(name)).expect("registry readable")
    };

    let source = replace_first_assignment(&read("logical_object_kinds.toml"), "object_kind", "0");
    let table = registry_check::toml::parse(&source).expect("mutated logical parses");
    let (epoch, rows) = identity::logical_from(&table).expect("mutated logical models");
    let mut mutated = r.clone();
    mutated.logical_epoch = epoch;
    mutated.logical = rows;
    assert!(!identity::validate_identity(&mutated).is_empty());

    let source = replace_first_assignment(&read("physical_record_kinds.toml"), "record_kind", "0");
    let table = registry_check::toml::parse(&source).expect("mutated physical parses");
    let (epoch, rows) = identity::physical_from(&table).expect("mutated physical models");
    let mut mutated = r.clone();
    mutated.physical_epoch = epoch;
    mutated.physical = rows;
    assert!(!identity::validate_identity(&mutated).is_empty());

    let source = replace_first_assignment(&read("bootstrap_frames.toml"), "frame_kind", "0");
    let table = registry_check::toml::parse(&source).expect("mutated bootstrap parses");
    let (epoch, rows) = identity::bootstrap_from(&table).expect("mutated bootstrap models");
    let mut mutated = r.clone();
    mutated.bootstrap_epoch = epoch;
    mutated.bootstrap = rows;
    assert!(!identity::validate_identity(&mutated).is_empty());

    let source = replace_first_assignment(
        &read("prebootstrap_artifact_kinds.toml"),
        "artifact_kind",
        "0",
    );
    let table = registry_check::toml::parse(&source).expect("mutated prebootstrap parses");
    let (epoch, rows) = identity::prebootstrap_from(&table).expect("mutated prebootstrap models");
    let mut mutated = r.clone();
    mutated.prebootstrap_epoch = epoch;
    mutated.prebootstrap = rows;
    assert!(!identity::validate_identity(&mutated).is_empty());

    let source = replace_first_assignment(&read("wire_types.toml"), "wire_type_id", "0");
    let table = registry_check::toml::parse(&source).expect("mutated wire parses");
    let (epoch, rows) = identity::wire_from(&table).expect("mutated wire models");
    let mut mutated = r.clone();
    mutated.wire_epoch = epoch;
    mutated.wire = rows;
    assert!(!identity::validate_identity(&mutated).is_empty());

    let source = replace_first_assignment(&read("durable_fields.toml"), "field_tag", "0");
    let table = registry_check::toml::parse(&source).expect("mutated fields parse");
    let (epoch, fields, unions) = identity::fields_from(&table).expect("mutated fields model");
    let mut mutated = r.clone();
    mutated.fields_epoch = epoch;
    mutated.fields = fields;
    mutated.unions = unions;
    assert!(!identity::validate_identity(&mutated).is_empty());
}
