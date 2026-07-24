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
    self, FieldRow, IdentityRegistries, LogicalKind, WireType, bodydigest_pin,
    bodydigest_transcript,
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

fn ordinary_top_level_union_fixture() -> IdentityRegistries {
    let source = r#"
schema_version = 1

[registry]
name = "durable_fields"
registry_epoch = 9

[[union]]
union_name = "FixtureTopLevelUnion"
containing_schema = "RootBootstrap"
union_path = "fixture_top_level_union"
tag_wire_type = "u8"
encoding_context = "closed-tagged"
allowed_containing_schemas = ["RootBootstrap"]
role_predicate = "true"
version_status = "active"
max_size_bytes = 128

[[union_arm]]
union_name = "FixtureTopLevelUnion"
containing_schema = "RootBootstrap"
union_path = "fixture_top_level_union"
arm_tag = 1
source_arm_name = "Absent"
stable_name = "absent"
payload_kind = "unit"
role_predicate = "true"
version_status = "active"
max_size_bytes = 1

[[union_arm]]
union_name = "FixtureTopLevelUnion"
containing_schema = "RootBootstrap"
union_path = "fixture_top_level_union"
arm_tag = 2
source_arm_name = "Present"
stable_name = "present"
payload_kind = "inline-record"
payload_sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
role_predicate = "true"
version_status = "active"
max_size_bytes = 127
"#;
    let table = registry_check::toml::parse(source).expect("ordinary-union fixture parses");
    let (epoch, fields, ordinary_unions, reference_unions) =
        identity::fields_from(&table).expect("ordinary-union fixture models");

    assert_eq!(epoch, 9);
    assert!(fields.is_empty());
    assert!(reference_unions.is_empty());
    assert_eq!(ordinary_unions.len(), 1);
    let union = &ordinary_unions[0];
    assert_eq!(union.field_tag, None, "omitted field_tag means top-level");
    assert_eq!(union.arms.len(), 2);
    assert_eq!(union.arms[0].payload_kind, "unit");
    assert_eq!(union.arms[0].payload_sha256, None);
    assert_eq!(union.arms[1].payload_kind, "inline-record");
    assert_eq!(
        union.arms[1].payload_sha256.as_deref(),
        Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
    );

    let mut identity = real_identity();
    identity.fields_epoch = epoch;
    // Keep the real ordinary unions so their anchor fields still resolve; the
    // synthetic fixture union stays at index 0 for the mutation tests.
    let mut all_unions = ordinary_unions;
    all_unions.append(&mut identity.ordinary_unions);
    identity.ordinary_unions = all_unions;
    identity
}

fn codes_without_assignment_drift(r: &IdentityRegistries) -> Vec<String> {
    identity::validate_identity(r)
        .into_iter()
        .filter(|violation| violation.code != "registry_assignment_drift")
        .map(|violation| violation.code)
        .collect()
}

fn rename_logical_command_input_union(identity: &mut IdentityRegistries, name: &str) {
    let (containing_schema, field_tag) = {
        let union = identity
            .unions
            .iter_mut()
            .find(|union| {
                union.containing_schema == "LogicalCommandRecord" && union.field_tag == 0x0003
            })
            .expect("LogicalCommandRecord.command reference union exists");
        union.union_name = name.to_owned();
        for arm in &mut union.arms {
            arm.union_name = name.to_owned();
        }
        (union.containing_schema.clone(), union.field_tag)
    };
    identity
        .fields
        .iter_mut()
        .find(|field| field.containing_schema == containing_schema && field.field_tag == field_tag)
        .expect("LogicalCommandRecord.command anchor exists")
        .exact_wire_type = name.to_owned();
}

/// Reverse the transcript-visible A01 increment-2B exactness repairs so the
/// pre-erratum durable-fields pin keeps reconstructing from live rows.  The
/// repair's bound corrections are transcript-invisible (field max_size_bytes
/// is not pinned) and need no undo; only the five wire-type flips and the two
/// ordinary-union tag/bound corrections appear in the assignment transcript.
fn undo_a01_exactness_repair(identity: &mut IdentityRegistries) {
    for field in &mut identity.fields {
        let flipped = matches!(
            (field.containing_schema.as_str(), field.stable_name.as_str()),
            (
                "RemoteReleaseSummaryEntry" | "RemoteRetentionReleaseAckCertificate",
                "grant_id"
            ) | (
                "RemoteReleaseSummaryEntry"
                    | "RemoteRetentionReleaseAckCertificate"
                    | "RemoteRetentionReleaseTombstone",
                "release_nonce"
            )
        );
        if flipped {
            field.exact_wire_type = "id256".to_owned();
        }
    }
    for union in &mut identity.ordinary_unions {
        if matches!(
            union.union_name.as_str(),
            "RootAuthorityTrustArtifactKind" | "TrustTransition"
        ) {
            union.tag_wire_type = "u16".to_owned();
            union.max_size_bytes = 16_777_216;
            for arm in &mut union.arms {
                arm.max_size_bytes = 16_777_216;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Baseline.
// ---------------------------------------------------------------------------

#[test]
fn appendix_a_catalog_real_source_verifies_and_reconstructs() {
    let catalog = real_appendix_catalog();
    let source = real_plan_source();
    let violations = appendix_a::appendix_a_catalog_source(&catalog, &source);
    assert!(
        violations.is_empty(),
        "real Appendix A source does not verify: {violations:?}"
    );
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
                "schema_version = 4",
                "schema_version = 4\nunknown_root_key = true",
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
            "unknown reference manifest key",
            source.replacen(
                "target_count = 813",
                "target_count = 813\nunknown_reference_manifest_key = true",
                1,
            ),
            "catalog_unknown_key",
            "unknown_reference_manifest_key",
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
            "stale schema version",
            source.replacen("schema_version = 4", "schema_version = 3", 1),
            "catalog_pin_mismatch",
            "schema_version",
        ),
        (
            "future schema version",
            source.replacen("schema_version = 4", "schema_version = 5", 1),
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

    let target_manifest = source.replacen(
        "[target_manifest]",
        "[target_manifest]\nunknown_metadata_key = true",
        1,
    );
    let violations = appendix_a::parse_catalog(&target_manifest)
        .expect_err("unknown target-manifest key must fail closed");
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
    let baseline_violations = appendix_a::appendix_a_catalog_closure(&baseline);
    assert!(
        baseline_violations.is_empty(),
        "baseline metadata closure must be exact: {baseline_violations:?}"
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
            owner_status: "planned".to_owned(),
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
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_maintenance_proof_mismatch")
    );

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
        owner_status: "planned".to_owned(),
        consumer_crates: vec!["fgdb".to_owned(), "fgdb-server".to_owned()],
    };

    let mut semantic = baseline.clone();
    semantic.semantic_bindings.push(valid.clone());
    let violations = appendix_a::validate_catalog(&semantic);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_semantic_binding_contract_drift"),
        "an unpinned real-looking semantic owner self-authorized: {violations:?}"
    );
    assert!(
        !violations
            .iter()
            .any(|violation| violation.code == "catalog_semantic_owner_invalid"),
        "the well-shaped implementation owner should fail only the independent pin: {violations:?}"
    );

    let mut fake_owner = baseline.clone();
    let mut fake = valid.clone();
    fake.owner_crate = "registry-check".to_owned();
    fake_owner.semantic_bindings.push(fake);
    let violations = appendix_a::validate_catalog(&fake_owner);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_semantic_owner_invalid")
    );

    let mut unsorted_consumers = baseline;
    let mut unsorted = valid;
    unsorted.consumer_crates = vec!["z".to_owned(), "a".to_owned()];
    unsorted_consumers.semantic_bindings.push(unsorted);
    let violations = appendix_a::validate_catalog(&unsorted_consumers);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_metadata_order")
    );
}

#[test]
fn appendix_a_annotations_reject_placeholders_and_unknown_schema_ids() {
    let mut catalog = real_appendix_catalog();
    let valid = appendix_a::Annotation {
        row_id: "a01:annotation:bootstrap-frame-root-slot".to_owned(),
        target_row_id: "a01:bootstrap-frame:root-slot".to_owned(),
        exact_type: "RootSlot".to_owned(),
        cardinality: "one".to_owned(),
        layout: "fixed".to_owned(),
        role: "Local".to_owned(),
        posture: "bootstrap".to_owned(),
        authority: "root".to_owned(),
        locality: "local".to_owned(),
        generic_expansions: Vec::new(),
        role_expansions: Vec::new(),
        reference_semantics: "embedded".to_owned(),
        target_schema_ids: Vec::new(),
        construction_order: "root-first".to_owned(),
        retention_and_cut_rule: "fixed-location".to_owned(),
        digest_recipe: "slot-checksum".to_owned(),
        redaction_class: "public-commitment".to_owned(),
        resource_bounds: "fixed-4096-bytes".to_owned(),
        compatibility: "v1".to_owned(),
    };
    catalog.annotations.push(valid);
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_contract_drift"),
        "an unpinned annotation self-authorized: {violations:?}"
    );
    for unexpected in [
        "catalog_annotation_placeholder",
        "catalog_annotation_target_schema_unresolved",
        "catalog_annotation_reference_invalid",
        "catalog_annotation_reference_target_mismatch",
    ] {
        assert!(
            !violations
                .iter()
                .any(|violation| violation.code == unexpected),
            "concrete Local annotation was rejected with {unexpected}: {violations:?}"
        );
    }

    let mut invented_definition_semantics = catalog.clone();
    invented_definition_semantics.annotations[0].reference_semantics = "strong".to_owned();
    let violations = appendix_a::validate_catalog(&invented_definition_semantics);
    assert!(
        violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_semantics_mismatch"
        }),
        "an ordinary top-level definition invented strong-reference semantics: {violations:?}"
    );

    for erased_or_union in [
        "StrongRef",
        "RegisteredStrongRef[]",
        "[StrongRef]",
        "StrongRef<ValidTimeContract|RootSlot>",
        "StrongRef<RootManifest,Anything>",
        "StrongRef<RootManifest::Anything>",
    ] {
        let mut invalid = catalog.clone();
        invalid.annotations[0].exact_type = erased_or_union.to_owned();
        let violations = appendix_a::validate_catalog(&invalid);
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "catalog_annotation_reference_invalid"),
            "erased or union StrongRef shape {erased_or_union:?} was accepted: {violations:?}"
        );
    }

    let root_manifest_schema_id = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "RootManifest")
        .expect("RootManifest reservation")
        .row_id
        .clone();
    catalog.annotations[0].exact_type = "StrongRef<RootManifest>".to_owned();
    catalog.annotations[0].reference_semantics = "strong".to_owned();
    catalog.annotations[0].target_schema_ids.clear();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| { violation.code == "catalog_annotation_reference_target_mismatch" }),
        "a StrongRef without an exact target schema ID was accepted: {violations:?}"
    );
    catalog.annotations[0].target_schema_ids = vec![root_manifest_schema_id];
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        !violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_target_mismatch"
                || violation.code == "catalog_annotation_reference_invalid"
        }),
        "a concrete StrongRef did not resolve one-for-one: {violations:?}"
    );
    catalog.annotations[0].exact_type = "Vec<StrongRef<RootManifest>>".to_owned();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        !violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_invalid"
                || violation.code == "catalog_annotation_reference_target_mismatch"
        }),
        "a valid collection of concrete StrongRefs was rejected: {violations:?}"
    );
    let logical_command_schema_id = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "LogicalCommandRecord")
        .expect("LogicalCommandRecord reservation")
        .row_id
        .clone();
    catalog.annotations[0].exact_type = "StrongCommandRef".to_owned();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| { violation.code == "catalog_annotation_reference_target_mismatch" }),
        "StrongCommandRef accepted a RootManifest target: {violations:?}"
    );
    catalog.annotations[0].target_schema_ids = vec![logical_command_schema_id];
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        !violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_invalid"
                || violation.code == "catalog_annotation_reference_target_mismatch"
                || violation.code == "catalog_annotation_reference_semantics_mismatch"
        }),
        "registered fixed-target StrongCommandRef was rejected: {violations:?}"
    );
    catalog.annotations[0].exact_type = "StrongBogusRef".to_owned();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_reference_invalid"),
        "unregistered fixed-target strong wrapper was accepted: {violations:?}"
    );
    catalog.annotations[0].exact_type = "u64".to_owned();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_semantics_mismatch"
        }),
        "reference semantics without a registered wrapper was accepted: {violations:?}"
    );

    let delta_block_version_schema_id = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "DeltaBlockVersion")
        .expect("DeltaBlockVersion reservation")
        .row_id
        .clone();
    catalog.annotations[0].exact_type = "ConditionalCoordinateRef<DeltaBlockVersion>".to_owned();
    catalog.annotations[0].reference_semantics = "conditional".to_owned();
    catalog.annotations[0].target_schema_ids = vec![delta_block_version_schema_id.clone()];
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        !violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_target_mismatch"
                || violation.code == "catalog_annotation_reference_invalid"
                || violation.code == "catalog_annotation_reference_semantics_mismatch"
        }),
        "registered conditional reference did not resolve: {violations:?}"
    );
    catalog.annotations[0].exact_type = "ConditionalBogusRef<DeltaBlockVersion>".to_owned();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_reference_invalid"),
        "unregistered conditional wrapper was accepted: {violations:?}"
    );
    catalog.annotations[0].exact_type = "ConditionalCoordinateRef".to_owned();
    catalog.annotations[0].target_schema_ids.clear();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| { violation.code == "catalog_annotation_reference_target_mismatch" }),
        "bare conditional reference without an exact target was accepted: {violations:?}"
    );
    catalog.annotations[0].exact_type = "[u8;32]".to_owned();
    catalog.annotations[0].reference_semantics = "weak_digest".to_owned();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        !violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_target_mismatch"
                || violation.code == "catalog_annotation_reference_semantics_mismatch"
        }),
        "a raw weak-digest relation without a typed target was rejected: {violations:?}"
    );
    catalog.annotations[0].target_schema_ids = vec![delta_block_version_schema_id];

    let annotation = &mut catalog.annotations[0];
    annotation.exact_type = "StrongRef<T>".to_owned();
    annotation.role = "Role".to_owned();
    annotation.generic_expansions = vec!["RootSlot".to_owned()];
    annotation.role_expansions = vec!["Local".to_owned()];
    annotation.reference_semantics = "strong".to_owned();
    annotation.target_schema_ids = vec!["NonexistentSchema".to_owned()];
    annotation.retention_and_cut_rule = "TODO".to_owned();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_placeholder"),
        "placeholder annotation assertions were accepted: {violations:?}"
    );
    assert!(
        violations
            .iter()
            .any(|violation| { violation.code == "catalog_annotation_target_schema_unresolved" }),
        "unknown annotation schema target was accepted: {violations:?}"
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_reference_invalid"),
        "non-concrete StrongRef target was accepted: {violations:?}"
    );

    for placeholder in [
        "TODO: define later",
        "TBD/v2",
        "unknown until A02",
        "retain through restart; TODO: define exact cut",
        "retention remains unknown until A02",
    ] {
        let mut embedded = real_appendix_catalog();
        let mut annotation = catalog.annotations[0].clone();
        annotation.exact_type = "RootSlot".to_owned();
        annotation.role = "Local".to_owned();
        annotation.generic_expansions.clear();
        annotation.role_expansions.clear();
        annotation.reference_semantics = "embedded".to_owned();
        annotation.target_schema_ids.clear();
        annotation.retention_and_cut_rule = placeholder.to_owned();
        embedded.annotations.push(annotation);
        let violations = appendix_a::validate_catalog(&embedded);
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "catalog_annotation_placeholder"),
            "embedded placeholder {placeholder:?} was accepted: {violations:?}"
        );
    }

    let mut negated = real_appendix_catalog();
    let mut annotation = catalog.annotations[0].clone();
    annotation.exact_type = "RootSlot".to_owned();
    annotation.role = "Local".to_owned();
    annotation.generic_expansions.clear();
    annotation.role_expansions.clear();
    annotation.reference_semantics = "embedded".to_owned();
    annotation.target_schema_ids.clear();
    annotation.retention_and_cut_rule = "no unresolved references remain".to_owned();
    negated.annotations.push(annotation);
    let violations = appendix_a::validate_catalog(&negated);
    assert!(
        !violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_placeholder"),
        "an explicitly negated unresolved marker was treated as a placeholder: {violations:?}"
    );
}

#[test]
fn appendix_a_field_annotations_match_source_type_and_cardinality() {
    let mut catalog = real_appendix_catalog();
    let annotation = appendix_a::Annotation {
        row_id: "a01:annotation:field-root-slot-cluster-incarnation".to_owned(),
        target_row_id: "a01:field:root-slot-cluster-incarnation".to_owned(),
        exact_type: "u64".to_owned(),
        cardinality: "one".to_owned(),
        layout: "fixed".to_owned(),
        role: "Local".to_owned(),
        posture: "bootstrap".to_owned(),
        authority: "root".to_owned(),
        locality: "local".to_owned(),
        generic_expansions: Vec::new(),
        role_expansions: Vec::new(),
        reference_semantics: "embedded".to_owned(),
        target_schema_ids: Vec::new(),
        construction_order: "root-first".to_owned(),
        retention_and_cut_rule: "fixed-location".to_owned(),
        digest_recipe: "slot-checksum".to_owned(),
        redaction_class: "public-commitment".to_owned(),
        resource_bounds: "fixed-u64".to_owned(),
        compatibility: "v1".to_owned(),
    };
    catalog.annotations.push(annotation.clone());
    let source = real_plan_source();
    let violations = appendix_a::appendix_a_catalog_source(&catalog, &source);
    assert!(
        !violations
            .iter()
            .any(|violation| violation.code == "source_annotation_contract_mismatch"),
        "source-exact field annotation was rejected: {violations:?}"
    );

    catalog.annotations[0].exact_type = "u32".to_owned();
    catalog.annotations[0].cardinality = "optional".to_owned();
    let violations = appendix_a::appendix_a_catalog_source(&catalog, &source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "source_annotation_contract_mismatch"),
        "field annotation drifted from source type/cardinality: {violations:?}"
    );

    let mut top_level = real_appendix_catalog();
    let mut top_annotation = annotation;
    top_annotation.row_id = "a01:annotation:bootstrap-frame-root-slot".to_owned();
    top_annotation.target_row_id = "a01:bootstrap-frame:root-slot".to_owned();
    top_annotation.exact_type = "WrongRootSlot".to_owned();
    top_level.annotations.push(top_annotation);
    let violations = appendix_a::appendix_a_catalog_source(&top_level, &source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "source_annotation_contract_mismatch"),
        "top-level annotation drifted from its source schema identity: {violations:?}"
    );
}

#[test]
fn appendix_a_field_annotations_match_identity_reference_contract() {
    let mut catalog = real_appendix_catalog();
    let root_manifest_schema_id = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "RootManifest")
        .expect("RootManifest reservation")
        .row_id
        .clone();
    let root_manifest_projection_id = catalog
        .projection_rows
        .iter()
        .find(|projection| projection.canonical_symbol == "RootManifest")
        .expect("RootManifest projection row")
        .row_id
        .clone();
    let unrelated_schema_id = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "LogicalCommandRecord")
        .expect("LogicalCommandRecord reservation")
        .row_id
        .clone();
    catalog.annotations.push(appendix_a::Annotation {
        row_id: "a01:annotation:field-root-slot-root-manifest-oid".to_owned(),
        target_row_id: "a01:field:root-slot-root-manifest-oid".to_owned(),
        exact_type: "oid256".to_owned(),
        cardinality: "one".to_owned(),
        layout: "fixed".to_owned(),
        role: "Local".to_owned(),
        posture: "bootstrap".to_owned(),
        authority: "root".to_owned(),
        locality: "local".to_owned(),
        generic_expansions: Vec::new(),
        role_expansions: Vec::new(),
        reference_semantics: "external_root".to_owned(),
        target_schema_ids: vec![root_manifest_schema_id.clone()],
        construction_order: "root-first".to_owned(),
        retention_and_cut_rule: "nonretaining-manifest-locator".to_owned(),
        digest_recipe: "slot-checksum".to_owned(),
        redaction_class: "public-commitment".to_owned(),
        resource_bounds: "fixed-32-bytes".to_owned(),
        compatibility: "v1".to_owned(),
    });
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        !violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_field_contract_mismatch"),
        "the exact durable-field reference contract was rejected: {violations:?}"
    );

    catalog.annotations[0].target_schema_ids = vec![root_manifest_projection_id];
    let violations = appendix_a::validate_catalog(&catalog);
    for expected in [
        "catalog_annotation_target_schema_unresolved",
        "catalog_annotation_field_contract_mismatch",
    ] {
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == expected),
            "an alternate same-family catalog layer bypassed canonical target ID {expected}: {violations:?}"
        );
    }

    catalog.annotations[0].reference_semantics = "none".to_owned();
    catalog.annotations[0].target_schema_ids.clear();
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_field_contract_mismatch"),
        "a field annotation suppressed its authoritative locator target: {violations:?}"
    );

    catalog.annotations[0].reference_semantics = "external_root".to_owned();
    catalog.annotations[0].target_schema_ids = vec![unrelated_schema_id];
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_field_contract_mismatch"),
        "a field annotation substituted an unrelated valid schema target: {violations:?}"
    );

    catalog.annotations[0].exact_type = "StrongRef".to_owned();
    catalog.annotations[0].reference_semantics = "strong".to_owned();
    catalog.annotations[0].target_schema_ids = vec![root_manifest_schema_id];
    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_reference_invalid"),
        "a field-use bare StrongRef was mistaken for a top-level definition: {violations:?}"
    );
}

#[test]
fn appendix_a_top_level_generic_annotations_discharge_source_formals() {
    let mut catalog = real_appendix_catalog();
    catalog.annotations.push(appendix_a::Annotation {
        row_id: "a19:annotation:logical-kind-recovery-bridge-spec".to_owned(),
        target_row_id: "a19:logical-kind:recovery-bridge-spec".to_owned(),
        exact_type: "RecoveryBridgeSpec".to_owned(),
        cardinality: "one".to_owned(),
        layout: "canonical".to_owned(),
        role: "Local".to_owned(),
        posture: "recovery".to_owned(),
        authority: "recovery".to_owned(),
        locality: "local".to_owned(),
        generic_expansions: Vec::new(),
        role_expansions: vec!["Local".to_owned(), "Meta".to_owned()],
        reference_semantics: "embedded".to_owned(),
        target_schema_ids: Vec::new(),
        construction_order: "source-before-bridge".to_owned(),
        retention_and_cut_rule: "retain-through-recovery".to_owned(),
        digest_recipe: "canonical-fields".to_owned(),
        redaction_class: "authority-metadata".to_owned(),
        resource_bounds: "bounded-by-source-manifest".to_owned(),
        compatibility: "v1".to_owned(),
    });
    let source = real_plan_source();
    let violations = appendix_a::appendix_a_catalog_source(&catalog, &source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "source_annotation_contract_mismatch"),
        "an unpinned flattened role expansion self-authorized: {violations:?}"
    );

    catalog.annotations[0].role_expansions = vec!["Local".to_owned()];
    let violations = appendix_a::appendix_a_catalog_source(&catalog, &source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "source_annotation_contract_mismatch"),
        "an incomplete concrete role expansion was accepted: {violations:?}"
    );

    catalog.annotations[0].role_expansions = vec!["Local".to_owned(), "Meta".to_owned()];
    catalog.annotations[0].exact_type = "RecoveryBridgeSpec<Role>".to_owned();
    let mut violations = appendix_a::validate_catalog(&catalog);
    violations.extend(appendix_a::appendix_a_catalog_source(&catalog, &source));
    for expected in [
        "catalog_annotation_placeholder",
        "source_annotation_contract_mismatch",
    ] {
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == expected),
            "residual source formal omitted {expected}: {violations:?}"
        );
    }

    let mut definition = real_appendix_catalog();
    definition.annotations.push(appendix_a::Annotation {
        row_id: "a01:annotation:wire-type-strong-ref".to_owned(),
        target_row_id: "a01:wire-type:strong-ref".to_owned(),
        exact_type: "StrongRef".to_owned(),
        cardinality: "one".to_owned(),
        layout: "canonical".to_owned(),
        role: "Local".to_owned(),
        posture: "durable".to_owned(),
        authority: "object".to_owned(),
        locality: "portable".to_owned(),
        generic_expansions: Vec::new(),
        role_expansions: Vec::new(),
        reference_semantics: "strong".to_owned(),
        target_schema_ids: Vec::new(),
        construction_order: "target-before-reference".to_owned(),
        retention_and_cut_rule: "retaining-reference".to_owned(),
        digest_recipe: "canonical-target-id".to_owned(),
        redaction_class: "public-commitment".to_owned(),
        resource_bounds: "fixed-reference".to_owned(),
        compatibility: "v1".to_owned(),
    });
    let violations = appendix_a::appendix_a_catalog_source(&definition, &source);
    assert!(
        !violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_invalid"
                || violation.code == "catalog_annotation_reference_target_mismatch"
                || violation.code == "source_annotation_contract_mismatch"
        }),
        "the exact top-level StrongRef definition was treated as an erased field use: {violations:?}"
    );

    definition.annotations[0].reference_semantics = "none".to_owned();
    let violations = appendix_a::validate_catalog(&definition);
    assert!(
        violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_semantics_mismatch"
        }),
        "a top-level StrongRef definition suppressed its strong semantics: {violations:?}"
    );
    definition.annotations[0].reference_semantics = "strong".to_owned();

    let arbitrary_target = definition
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "RootManifest")
        .expect("RootManifest reservation")
        .row_id
        .clone();
    definition.annotations[0].target_schema_ids = vec![arbitrary_target];
    let violations = appendix_a::validate_catalog(&definition);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_reference_target_mismatch"),
        "a top-level reference definition claimed an arbitrary target: {violations:?}"
    );

    let mut weak_definition = real_appendix_catalog();
    let mut weak_annotation = definition.annotations[0].clone();
    weak_annotation.row_id = "a01:annotation:wire-type-weak-digest".to_owned();
    weak_annotation.target_row_id = "a01:wire-type:weak-digest".to_owned();
    weak_annotation.exact_type = "WeakDigest".to_owned();
    weak_annotation.reference_semantics = "strong".to_owned();
    weak_annotation.target_schema_ids.clear();
    weak_definition.annotations.push(weak_annotation);
    let violations = appendix_a::validate_catalog(&weak_definition);
    assert!(
        violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_semantics_mismatch"
        }),
        "a top-level WeakDigest definition claimed strong semantics: {violations:?}"
    );

    let mut marker_definition = real_appendix_catalog();
    let mut marker_annotation = definition.annotations[0].clone();
    marker_annotation.row_id = "a01:annotation:wire-type-marker-ref".to_owned();
    marker_annotation.target_row_id = "a01:wire-type:marker-ref".to_owned();
    marker_annotation.exact_type = "MarkerRef".to_owned();
    marker_annotation.reference_semantics = "identity".to_owned();
    marker_annotation.target_schema_ids.clear();
    marker_definition.annotations.push(marker_annotation);
    let violations = appendix_a::validate_catalog(&marker_definition);
    assert!(
        !violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_semantics_mismatch"
                || violation.code == "catalog_annotation_reference_target_mismatch"
        }),
        "the authoritative MarkerRef identity definition was rejected: {violations:?}"
    );
    marker_definition.annotations[0].reference_semantics = "none".to_owned();
    let violations = appendix_a::validate_catalog(&marker_definition);
    assert!(
        violations.iter().any(|violation| {
            violation.code == "catalog_annotation_reference_semantics_mismatch"
        }),
        "a MarkerRef definition erased its identity semantics: {violations:?}"
    );
}

#[test]
fn appendix_a_repository_bindings_resolve_beads_crates_checkers_and_events() {
    let mut catalog = real_appendix_catalog();
    let owner = "fgdb-durable-capability-validation-evidence-dqym";
    catalog.semantic_bindings.push(appendix_a::SemanticBinding {
        row_id: "a01:semantic-binding:bootstrap-frame-root-slot".to_owned(),
        target_row_id: "a01:bootstrap-frame:root-slot".to_owned(),
        owner_bead_id: owner.to_owned(),
        owner_crate: "fgdb-types".to_owned(),
        owner_status: "live".to_owned(),
        consumer_crates: vec!["fgdb".to_owned(), "fgdb-server".to_owned()],
    });
    catalog.evidence.push(appendix_a::EvidenceBinding {
        row_id: "a01:evidence:bootstrap-frame-root-slot-static-contract".to_owned(),
        target_row_id: "a01:bootstrap-frame:root-slot".to_owned(),
        evidence_id: "static-contract".to_owned(),
        phase: "static".to_owned(),
        status: "live".to_owned(),
        owner_bead_id: owner.to_owned(),
        checker_ids: vec!["appendix_a_catalog_closure".to_owned()],
        scenario_ids: vec!["g0_identity_e2e".to_owned()],
        event_ids: vec!["appendix_closure_checked".to_owned()],
        gate_ids: vec!["G0".to_owned()],
    });
    let pinned = appendix_a::validate_catalog(&catalog);
    for expected in [
        "catalog_semantic_binding_contract_drift",
        "catalog_semantic_binding_contract_unapproved",
        "catalog_evidence_binding_contract_drift",
        "catalog_evidence_binding_contract_unapproved",
    ] {
        assert!(
            pinned.iter().any(|violation| violation.code == expected),
            "real but unrelated metadata bypassed independent {expected}: {pinned:?}"
        );
    }
    let root = repo_root();
    if !root.join(".beads/issues.jsonl").is_file() {
        // Remote compilation workers may deliberately omit hidden runtime
        // state. Unit tests cover the deterministic index-level branches;
        // the CLI E2E stages the authoritative Beads file explicitly.
        return;
    }
    let resolved = appendix_a::verify_repository_bindings(&root, &catalog);
    assert!(
        resolved.is_empty(),
        "the separate repository-existence layer failed real IDs: {resolved:?}"
    );

    let mut merely_planned_owner = catalog.clone();
    merely_planned_owner.semantic_bindings[0].owner_crate = "fgdb-warden".to_owned();
    let violations = appendix_a::verify_repository_bindings(&root, &merely_planned_owner);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_semantic_live_owner_crate_unresolved"),
        "an absent crate was accepted as a live implementation owner: {violations:?}"
    );

    merely_planned_owner.semantic_bindings[0].owner_status = "planned".to_owned();
    let violations = appendix_a::verify_repository_bindings(&root, &merely_planned_owner);
    assert!(
        !violations.iter().any(|violation| matches!(
            violation.code.as_str(),
            "catalog_semantic_owner_crate_unresolved"
                | "catalog_semantic_live_owner_crate_unresolved"
        )),
        "an architecture-planned owner was incorrectly required to exist in the workspace: {violations:?}"
    );

    let mut stub_live = catalog.clone();
    stub_live.evidence[0].checker_ids = vec!["idr_generated_encoder_decoder_roundtrip".to_owned()];
    let violations = appendix_a::verify_repository_bindings(&root, &stub_live);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_live_evidence_checker_not_live"),
        "live evidence was allowed to cite a stub checker: {violations:?}"
    );
    stub_live.evidence[0].status = "planned".to_owned();
    let violations = appendix_a::verify_repository_bindings(&root, &stub_live);
    assert!(
        violations.is_empty(),
        "planned evidence must be allowed to cite a registered stub checker: {violations:?}"
    );

    let mut fabricated = catalog;
    fabricated.semantic_bindings[0].owner_bead_id = "fgdb-nonexistent-owner-z999".to_owned();
    fabricated.semantic_bindings[0].owner_crate = "fgdb-nonexistent-owner-crate".to_owned();
    fabricated.semantic_bindings[0].consumer_crates =
        vec!["fgdb-nonexistent-consumer-crate".to_owned()];
    fabricated.evidence[0].owner_bead_id = "fgdb-nonexistent-evidence-z999".to_owned();
    fabricated.evidence[0].checker_ids = vec!["nonexistent_checker".to_owned()];
    fabricated.evidence[0].scenario_ids = vec!["nonexistent_scenario".to_owned()];
    fabricated.evidence[0].event_ids = vec!["nonexistent_event".to_owned()];
    fabricated.evidence[0].gate_ids = vec!["G5".to_owned()];
    let mut violations = appendix_a::validate_catalog(&fabricated);
    violations.extend(appendix_a::verify_repository_bindings(&root, &fabricated));
    for expected in [
        "catalog_semantic_owner_bead_unresolved",
        "catalog_semantic_owner_crate_unresolved",
        "catalog_semantic_consumer_crate_unresolved",
        "catalog_evidence_owner_bead_unresolved",
        "catalog_evidence_checker_unresolved",
        "catalog_evidence_scenario_unresolved",
        "catalog_evidence_event_unresolved",
        "catalog_evidence_gate_invalid",
    ] {
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == expected),
            "fabricated repository metadata omitted {expected}: {violations:?}"
        );
    }
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
    assert_eq!(baseline.reservations.len(), 813);
    assert_eq!(
        baseline
            .reservations
            .iter()
            .filter(|row| row.disposition == "existing")
            .count(),
        appendix_a::EXPECTED_EXISTING_TYPE_RESERVATION_COUNT
    );
    assert_eq!(
        baseline
            .reservations
            .iter()
            .filter(|row| row.disposition == "reserved")
            .count(),
        appendix_a::EXPECTED_RESERVED_TYPE_RESERVATION_COUNT
    );
    assert_eq!(baseline.source_symbol_dispositions.len(), 848);
    assert_eq!(baseline.top_level_candidates.len(), 1_229);
    assert_eq!(
        baseline.targets.len(),
        appendix_a::EXPECTED_PROJECTION_ROW_COUNT
    );
    assert_eq!(
        baseline
            .targets
            .iter()
            .filter(|row| row.source_key.starts_with("projection|"))
            .count(),
        appendix_a::EXPECTED_PROJECTION_FALLBACK_COUNT
    );
    assert_eq!(
        baseline.target_manifest.target_count,
        i64::try_from(appendix_a::EXPECTED_PROJECTION_ROW_COUNT)
            .expect("projection row count fits i64")
    );
    assert_eq!(
        baseline.target_manifest.projection_fallback_count,
        i64::try_from(appendix_a::EXPECTED_PROJECTION_FALLBACK_COUNT)
            .expect("projection fallback count fits i64")
    );
    assert_eq!(
        appendix_a::target_source_assignment_sha256(&baseline.targets),
        appendix_a::EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256
    );
    let mut reversed_targets = baseline.targets.clone();
    reversed_targets.reverse();
    assert_eq!(
        appendix_a::target_source_assignment_sha256(&reversed_targets),
        appendix_a::EXPECTED_TARGET_SOURCE_ASSIGNMENT_SHA256,
        "target/source transcript must sort by target_row_id, not file order"
    );
    assert!(baseline.semantic_bindings.is_empty());
    assert!(baseline.evidence.is_empty());
    assert_eq!(
        appendix_a::reservation_assignment_sha256(&baseline.reservations),
        appendix_a::EXPECTED_RESERVATION_ASSIGNMENT_SHA256
    );

    let mut reassigned_target = baseline.clone();
    reassigned_target
        .targets
        .iter_mut()
        .find(|row| row.target_row_id == "a01:field:root-slot-cluster-incarnation")
        .expect("source-backed RootSlot.cluster_incarnation target")
        .source_key = "projection|durable_fields|RootSlot.cluster_incarnation".to_owned();
    let violations = appendix_a::validate_catalog(&reassigned_target);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_target_source_assignment_drift"),
        "exact target/source assignment was silently downgraded: {violations:?}"
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
        violations.iter().any(|violation| matches!(
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

    let mut projection_fallback = real_appendix_catalog();
    let fallback = projection_fallback
        .targets
        .iter_mut()
        .find(|row| row.slice_id != "g0" && row.source_key.starts_with("projection|"))
        .expect("declared Appendix projection-only fallback exists");
    fallback.definition_status = "complete".to_owned();
    let violations = appendix_a::validate_catalog(&projection_fallback);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_target_projection_incomplete"),
        "projection-only source incorrectly backed a complete target: {violations:?}"
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
fn appendix_a_source_derived_catalog_rows_and_slice_census_fail_closed() {
    let source = real_plan_source();

    let mut missing_candidate = real_appendix_catalog();
    let removed = missing_candidate.top_level_candidates.remove(0);
    let violations = appendix_a::verify_source(&missing_candidate, &source);
    assert!(
        violations.iter().any(|violation| {
            violation.code == "source_top_level_candidate_missing"
                && violation.row_id == removed.source_key
        }),
        "missing source candidate did not identify its exact key: {violations:?}"
    );

    let mut mismatched_candidate = real_appendix_catalog();
    mismatched_candidate.top_level_candidates[0].source_kind =
        if mismatched_candidate.top_level_candidates[0].source_kind == "name-only" {
            "confirmed"
        } else {
            "name-only"
        }
        .to_owned();
    let violations = appendix_a::verify_source(&mismatched_candidate, &source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "source_top_level_candidate_mismatch"),
        "source-candidate metadata drift escaped reconciliation: {violations:?}"
    );

    let mut wrong_field_pin = real_appendix_catalog();
    let replacement = if wrong_field_pin.slices[0]
        .field_candidate_ids_sha256
        .starts_with('0')
    {
        "1"
    } else {
        "0"
    };
    wrong_field_pin.slices[0]
        .field_candidate_ids_sha256
        .replace_range(0..1, replacement);
    let violations = appendix_a::verify_source(&wrong_field_pin, &source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "source_structural_census_mismatch"),
        "source structural-census pin drift escaped reconciliation: {violations:?}"
    );

    let mut moved_owner = real_appendix_catalog();
    let reservation = moved_owner
        .reservations
        .iter_mut()
        .find(|row| row.symbol == "ValidTimeContract")
        .expect("plan-only reference reservation");
    reservation.slice_id = "a21".to_owned();
    reservation.row_id = "a21:reservation:valid-time-contract".to_owned();
    let disposition = moved_owner
        .source_symbol_dispositions
        .iter_mut()
        .find(|row| row.symbol == "ValidTimeContract")
        .expect("plan-only reference disposition");
    disposition.slice_id = "a21".to_owned();
    disposition.row_id = "a21:source-symbol-disposition:valid-time-contract".to_owned();
    let violations = appendix_a::verify_source(&moved_owner, &source);
    assert!(
        violations
            .iter()
            .any(|violation| { violation.code == "reference_source_reservation_owner_mismatch" }),
        "coherent reservation/disposition owner drift escaped source derivation: {violations:?}"
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "reference_source_disposition_mismatch"),
        "source disposition owner drift escaped source derivation: {violations:?}"
    );
}

#[test]
fn appendix_a_wire_backed_union_requires_confirmed_owner_and_exact_arm_set() {
    let source = real_plan_source();
    let source_key = "top|ServicePromotionExternalOperationKind";

    let mut unconfirmed_owner = real_appendix_catalog();
    unconfirmed_owner
        .top_level_candidates
        .iter_mut()
        .find(|candidate| candidate.source_key == source_key)
        .expect("Service promotion kind candidate")
        .source_kind = "ambiguous".to_owned();
    let violations = appendix_a::verify_source(&unconfirmed_owner, &source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "source_union_top_level_owner_mismatch"),
        "an unconfirmed top-level candidate acquired wire-backed ordinary-union authority: {violations:?}"
    );

    for union_name in [
        "ServicePromotionExternalOperationKind",
        "KeyDestroyExternalAckRef",
        "KeyDestroyFloorRef",
        "KeyDestructionTarget",
    ] {
        let mut missing_arm = real_appendix_catalog();
        let union = missing_arm
            .identity
            .ordinary_unions
            .iter_mut()
            .find(|union| union.union_name == union_name)
            .expect("source-backed ordinary union fixture exists");
        union.arms.pop().expect("source-backed union has arms");
        let violations = appendix_a::verify_source(&missing_arm, &source);
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "source_union_arm_set_mismatch"),
            "a missing {union_name} arm escaped the source bijection: {violations:?}"
        );
    }

    let mut wrong_wire_source = real_appendix_catalog();
    wrong_wire_source
        .targets
        .iter_mut()
        .find(|target| {
            target.target_row_id
                == "a20:wire-type:service-promotion-external-operation-kind-catalog-reserve-hidden"
        })
        .expect("Service promotion wire-variant target")
        .source_key = "arm|ServicePromotionExternalOperationKind|ServicePromotionExternalOperationKind|CatalogActivateReserved".to_owned();
    let violations = appendix_a::appendix_a_catalog_closure(&wrong_wire_source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_target_source_identity_mismatch"),
        "a wire variant mapped to the wrong structural arm: {violations:?}"
    );

    let mut fallback_wire_source = real_appendix_catalog();
    fallback_wire_source
        .targets
        .iter_mut()
        .find(|target| {
            target.target_row_id
                == "a20:wire-type:service-promotion-external-operation-kind-catalog-reserve-hidden"
        })
        .expect("Service promotion wire-variant target")
        .source_key =
        "projection|wire_types|ServicePromotionExternalOperationKind.CatalogReserveHidden"
            .to_owned();
    let violations = appendix_a::appendix_a_catalog_closure(&fallback_wire_source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_target_source_identity_mismatch"),
        "a wire variant downgraded to projection fallback: {violations:?}"
    );

    let mut fallback_wire_parent_source = real_appendix_catalog();
    fallback_wire_parent_source
        .targets
        .iter_mut()
        .find(|target| {
            target.target_row_id == "a20:wire-type:service-promotion-external-operation-kind"
        })
        .expect("Service promotion wire-parent target")
        .source_key = "projection|wire_types|ServicePromotionExternalOperationKind".to_owned();
    let violations = appendix_a::appendix_a_catalog_closure(&fallback_wire_parent_source);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_target_source_identity_mismatch"),
        "a wire parent downgraded to projection fallback: {violations:?}"
    );

    for (target_row_id, fallback_source) in [
        (
            "a20:union:service-promotion-external-operation-kind-cbc46ac1a7231315",
            "projection|durable_fields|ServicePromotionExternalOperationKind.ServicePromotionExternalOperationKind",
        ),
        (
            "a20:union-arm:service-promotion-external-operation-kind-catalog-reserve-hidden-cb21b33f2418f561",
            "projection|durable_fields|ServicePromotionExternalOperationKind.ServicePromotionExternalOperationKind.CatalogReserveHidden",
        ),
    ] {
        let mut fallback_structural_source = real_appendix_catalog();
        fallback_structural_source
            .targets
            .iter_mut()
            .find(|target| target.target_row_id == target_row_id)
            .expect("Service promotion structural target")
            .source_key = fallback_source.to_owned();
        let violations = appendix_a::appendix_a_catalog_closure(&fallback_structural_source);
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "catalog_target_source_identity_mismatch"),
            "an ordinary-union structural target downgraded to projection fallback: {violations:?}"
        );
    }
}

#[test]
fn appendix_a_inline_record_unions_require_exact_payload_digests() {
    let source = real_plan_source();
    for (union_name, arm_name) in [
        ("NewDatabaseIdentityTargetCreationCommitment", "ExternalCas"),
        ("KeyDestroyExternalAckRef", "Backup"),
        ("KeyDestroyExternalAckRef", "LegalHold"),
        ("KeyDestroyExternalAckRef", "RemoteConsumer"),
        ("KeyDestroyFloorRef", "Checkpoint"),
        ("KeyDestroyFloorRef", "Configuration"),
        ("KeyDestructionTarget", "KmsKeyVersion"),
        ("KeyDestructionTarget", "HsmObject"),
        ("KeyDestructionTarget", "StorageMemberReplica"),
    ] {
        let mut wrong_payload = real_appendix_catalog();
        let arm = wrong_payload
            .identity
            .ordinary_unions
            .iter_mut()
            .find(|union| union.union_name == union_name)
            .expect("source-backed ordinary union fixture exists")
            .arms
            .iter_mut()
            .find(|arm| arm.source_arm_name == arm_name)
            .expect("source-backed ordinary union arm fixture exists");
        let payload_sha256 = arm
            .payload_sha256
            .as_mut()
            .expect("inline-record arm has a payload digest");
        payload_sha256.replace_range(
            0..1,
            if payload_sha256.starts_with('0') {
                "1"
            } else {
                "0"
            },
        );

        let violations = appendix_a::verify_source(&wrong_payload, &source);
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "source_union_arm_contract_mismatch"),
            "{union_name}.{arm_name} payload digest drift escaped source reconciliation: {violations:?}"
        );
    }
}

#[test]
fn appendix_a_full_plan_reference_occurrence_drift_fails_closed() {
    let catalog = real_appendix_catalog();
    let source = real_plan_source();
    let appendix_start = line_start_offset(&source, appendix_a::APPENDIX_START_LINE);
    let needle = b"StrongRef<ValidTimeContract>";
    let replacement = b"StrongRef<ValidTimeContracx>";
    let offset = source[..appendix_start]
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("reference occurrence exists before Appendix A");
    let mut mutated = source;
    mutated[offset..offset + needle.len()].copy_from_slice(replacement);

    let violations = appendix_a::verify_source(&catalog, &mutated);
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "reference_source_manifest_mismatch"),
        "full-plan reference occurrence drift escaped the pinned manifest: {violations:?}"
    );
    assert!(
        violations.iter().any(|violation| {
            violation.code == "reference_source_reservation_missing"
                && violation.row_id == "ValidTimeContracx"
        }),
        "new reference family did not require a reservation: {violations:?}"
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
                let (epoch, fields, ordinary_unions, unions) =
                    identity::fields_from(&table).expect("durable-field projection");
                assert_eq!(epoch, catalog.identity.fields_epoch);
                assert_eq!(fields, catalog.identity.fields);
                assert_eq!(ordinary_unions, catalog.identity.ordinary_unions);
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
    assert_eq!(
        r.bootstrap.len(),
        3,
        "RootSlot, RootBootstrap, and reserved RaftHardFrame"
    );
    assert!(
        r.prebootstrap.len() >= 5,
        "prebootstrap artifact classes seeded"
    );
    assert!(r.fields.len() >= 40, "durable_fields cross-index seeded");
    // The five §5.1-required generated-union exemplars are present.
    let unions: BTreeSet<&str> = r.unions.iter().map(|u| u.union_name.as_str()).collect();
    for required in [
        "LogicalCommandInputRef",
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
    assert!(
        r.wire.iter().any(|wire| wire.name == "CommandRef"),
        "A01's bare CommandRef identity must remain a registered wire type"
    );
    assert!(
        !unions.contains("CommandRef"),
        "CommandRef must not also resolve as a generated reference union"
    );
    let command_field = r
        .fields
        .iter()
        .find(|field| {
            field.containing_schema == "LogicalCommandRecord" && field.field_tag == 0x0003
        })
        .expect("LogicalCommandRecord.command field exists");
    assert_eq!(command_field.exact_wire_type, "LogicalCommandInputRef");
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

#[test]
fn idr_ordinary_top_level_union_parses_and_validates() {
    let identity = ordinary_top_level_union_fixture();
    let violations = identity::validate_identity(&identity);
    assert_eq!(
        violations
            .iter()
            .filter(|violation| violation.code == "registry_assignment_drift")
            .count(),
        1,
        "the synthetic union must differ only from the released assignment pin: {violations:?}"
    );
    assert!(
        codes_without_assignment_drift(&identity).is_empty(),
        "a top-level closed tagged union with unit and inline-record arms was rejected: {violations:?}"
    );
}

fn wire_backed_top_level_union_fixture() -> IdentityRegistries {
    let mut identity = ordinary_top_level_union_fixture();
    let union = &mut identity.ordinary_unions[0];
    union.union_name = "FixtureWireBackedUnion".into();
    union.containing_schema = "FixtureWireBackedUnion".into();
    union.union_path = "FixtureWireBackedUnion".into();
    union.role_predicate = "role-local || role-meta".into();
    for arm in &mut union.arms {
        arm.union_name = "FixtureWireBackedUnion".into();
        arm.containing_schema = "FixtureWireBackedUnion".into();
        arm.union_path = "FixtureWireBackedUnion".into();
        arm.role_predicate = "role-local || role-meta".into();
    }
    let variants: Vec<_> = union
        .arms
        .iter()
        .enumerate()
        .map(|(index, arm)| WireType {
            wire_type_id: 0x7ff1 + i64::try_from(index).expect("fixture index fits i64"),
            name: format!("{}.{}", union.union_name, arm.stable_name),
            kind: "union_variant".into(),
            status: arm.version_status.clone(),
            containing_union: Some(union.union_name.clone()),
            wire_tag: Some(arm.arm_tag),
            encoding_context: arm.payload_kind.clone(),
            allowed_containing_schemas: vec![union.union_name.clone()],
            max_size_bytes: arm.max_size_bytes,
        })
        .collect();
    identity.wire.push(WireType {
        wire_type_id: 0x7ff0,
        name: union.union_name.clone(),
        kind: "union".into(),
        status: union.version_status.clone(),
        containing_union: None,
        wire_tag: None,
        encoding_context: union.encoding_context.clone(),
        allowed_containing_schemas: union.allowed_containing_schemas.clone(),
        max_size_bytes: union.max_size_bytes,
    });
    identity.wire.extend(variants);
    identity
}

#[test]
fn idr_wire_backed_top_level_union_requires_exact_cross_index() {
    let identity = wire_backed_top_level_union_fixture();
    assert!(
        codes_without_assignment_drift(&identity).is_empty(),
        "a top-level ordinary union must cross-index one exact wire parent and variant set"
    );

    let mut wrong_path = identity.clone();
    wrong_path.ordinary_unions[0].union_path = "wrong_path".into();
    for arm in &mut wrong_path.ordinary_unions[0].arms {
        arm.union_path = "wrong_path".into();
    }
    assert!(
        codes_without_assignment_drift(&wrong_path)
            .contains(&"ordinary_union_name_collision".to_owned()),
        "partial name equality must not acquire the top-level wire exception"
    );

    let mut embedded = identity.clone();
    embedded.ordinary_unions[0].field_tag = Some(1);
    let codes = codes_without_assignment_drift(&embedded);
    assert!(
        codes.contains(&"ordinary_union_name_collision".to_owned())
            && codes.contains(&"ordinary_union_field_mismatch".to_owned()),
        "a field tag removes the top-level wire exception and requires an exact anchor: {codes:?}"
    );

    let mut missing_variant = identity.clone();
    missing_variant.wire.pop().expect("fixture wire variant");
    assert!(
        codes_without_assignment_drift(&missing_variant)
            .contains(&"ordinary_union_wire_contract_mismatch".to_owned()),
        "a missing wire variant escaped the exact cross-index"
    );

    let mut discriminant_with_payload = identity.clone();
    discriminant_with_payload
        .wire
        .iter_mut()
        .find(|wire| wire.name == "FixtureWireBackedUnion")
        .expect("fixture wire parent")
        .kind = "discriminant".into();
    assert!(
        codes_without_assignment_drift(&discriminant_with_payload)
            .contains(&"ordinary_union_wire_contract_mismatch".to_owned()),
        "a discriminant parent accepted an inline-record arm"
    );

    let mut wrong_tag = identity;
    wrong_tag
        .wire
        .iter_mut()
        .find(|wire| wire.containing_union.as_deref() == Some("FixtureWireBackedUnion"))
        .expect("fixture wire variant")
        .wire_tag = Some(3);
    assert!(
        codes_without_assignment_drift(&wrong_tag)
            .contains(&"ordinary_union_wire_contract_mismatch".to_owned()),
        "wire/ordinary tag drift escaped the exact cross-index"
    );
}

#[test]
fn idr_wire_backed_top_level_union_rejects_container_scope_drift() {
    let identity = wire_backed_top_level_union_fixture();
    let parent_name = identity.ordinary_unions[0].union_name.clone();

    let mut wildcard_parent = identity.clone();
    wildcard_parent
        .wire
        .iter_mut()
        .find(|wire| wire.name == parent_name)
        .expect("fixture wire parent")
        .allowed_containing_schemas = vec!["*".into()];
    assert!(
        codes_without_assignment_drift(&wildcard_parent)
            .contains(&"ordinary_union_wire_contract_mismatch".to_owned()),
        "a wildcard wire-parent scope escaped the exact ordinary-union cross-index"
    );

    let mut wildcard_union = identity.clone();
    wildcard_union.ordinary_unions[0].allowed_containing_schemas = vec!["*".into()];
    assert!(
        codes_without_assignment_drift(&wildcard_union)
            .contains(&"ordinary_union_container_contract_mismatch".to_owned()),
        "a wildcard ordinary-union scope escaped the concrete-container contract"
    );

    let mut extra_parent = identity.clone();
    extra_parent
        .wire
        .iter_mut()
        .find(|wire| wire.name == parent_name)
        .expect("fixture wire parent")
        .allowed_containing_schemas
        .push("RootSlot".into());
    assert!(
        codes_without_assignment_drift(&extra_parent)
            .contains(&"ordinary_union_wire_contract_mismatch".to_owned()),
        "an extra wire-parent container escaped the exact ordinary-union cross-index"
    );

    let mut missing_parent = identity;
    missing_parent
        .wire
        .iter_mut()
        .find(|wire| wire.name == parent_name)
        .expect("fixture wire parent")
        .allowed_containing_schemas
        .clear();
    let codes = codes_without_assignment_drift(&missing_parent);
    assert!(
        codes.contains(&"ordinary_union_wire_contract_mismatch".to_owned())
            && codes.contains(&"bad_field".to_owned()),
        "a missing wire-parent container escaped the closed contract: {codes:?}"
    );
}

#[test]
fn idr_key_destruction_target_consumer_closure_is_exact() {
    let identity = real_identity();
    let expected = vec![
        "ExternalKeyDestructionOperationRecord".to_owned(),
        "KeyDestructionOperationPlan".to_owned(),
        "ShardKeyDestroyApplySpec".to_owned(),
    ];
    let union = identity
        .ordinary_unions
        .iter()
        .find(|union| union.union_name == "KeyDestructionTarget")
        .expect("KeyDestructionTarget ordinary union exists");
    assert_eq!(
        union.allowed_containing_schemas, expected,
        "the source-derived ordinary-union consumer closure must remain exact"
    );
    let wire_parent = identity
        .wire
        .iter()
        .find(|wire| wire.name == "KeyDestructionTarget")
        .expect("KeyDestructionTarget wire parent exists");
    assert_eq!(
        wire_parent.allowed_containing_schemas, expected,
        "the wire parent must exactly mirror the ordinary-union consumer closure"
    );
}

#[test]
fn idr_ordinary_union_container_pin_is_unambiguously_framed() {
    let mut split = wire_backed_top_level_union_fixture();
    split.ordinary_unions[0].allowed_containing_schemas = vec!["A".into(), "B".into()];
    let split_pin = identity::assignment_pins(&split)
        .into_iter()
        .find(|pin| pin.registry == "durable_fields")
        .expect("durable-fields assignment pin")
        .actual_pin;

    let mut comma_bearing = split;
    comma_bearing.ordinary_unions[0].allowed_containing_schemas = vec!["A,B".into()];
    let comma_bearing_pin = identity::assignment_pins(&comma_bearing)
        .into_iter()
        .find(|pin| pin.registry == "durable_fields")
        .expect("durable-fields assignment pin")
        .actual_pin;

    assert_ne!(
        split_pin, comma_bearing_pin,
        "container-list framing must distinguish two entries from one comma-bearing schema"
    );
}

#[test]
fn idr_wire_backed_top_level_union_rejects_conventional_class_collision() {
    let identity = wire_backed_top_level_union_fixture();
    let union_name = identity.ordinary_unions[0].union_name.clone();
    let assert_unresolved = |identity: &IdentityRegistries, class: &str| {
        let codes = codes_without_assignment_drift(identity);
        assert!(
            codes.contains(&"ordinary_union_unresolved_schema".to_owned()),
            "wire ownership hid a same-name {class} schema: {codes:?}"
        );
    };

    let mut logical_collision = identity.clone();
    logical_collision
        .logical
        .push(kind(0x7ffe, &union_name, "active", 1));
    assert_unresolved(&logical_collision, "logical");

    let mut physical_collision = identity.clone();
    let mut physical = physical_collision.physical[0].clone();
    physical.record_kind = 0x7ffe;
    physical.name = union_name.clone();
    physical_collision.physical.push(physical);
    assert_unresolved(&physical_collision, "physical");

    let mut bootstrap_collision = identity.clone();
    let mut bootstrap = bootstrap_collision.bootstrap[0].clone();
    bootstrap.frame_kind = 0x7ffe;
    bootstrap.name = union_name.clone();
    bootstrap_collision.bootstrap.push(bootstrap);
    assert_unresolved(&bootstrap_collision, "bootstrap");

    let mut prebootstrap_collision = identity.clone();
    let mut prebootstrap = prebootstrap_collision.prebootstrap[0].clone();
    prebootstrap.artifact_kind = 0x7ffe;
    prebootstrap.name = union_name.clone();
    prebootstrap_collision.prebootstrap.push(prebootstrap);
    assert_unresolved(&prebootstrap_collision, "prebootstrap");
}

#[test]
fn idr_wire_backed_top_level_union_validates_every_consumer() {
    let mut identity = wire_backed_top_level_union_fixture();
    let union_name = identity.ordinary_unions[0].union_name.clone();
    let union_bound = identity.ordinary_unions[0].max_size_bytes;
    let second_container = identity.logical[0].name.clone();
    identity.ordinary_unions[0]
        .allowed_containing_schemas
        .push(second_container.clone());
    identity
        .wire
        .iter_mut()
        .find(|wire| wire.name == union_name)
        .expect("fixture wire parent")
        .allowed_containing_schemas
        .push(second_container.clone());
    let mut consumer = FieldRow {
        containing_schema: "RootBootstrap".into(),
        field_tag: 0x7ffe,
        stable_name: "fixture_wire_backed_union".into(),
        exact_wire_type: union_name.clone(),
        cardinality: "one".into(),
        identity_class: "inline".into(),
        reference_semantics: "none".into(),
        target_schema_id: None,
        construction_order: 0,
        role_predicate: "role-local".into(),
        retention_and_cut_rule: "fixture consumer".into(),
        version_status: "active".into(),
        max_size_bytes: union_bound,
        digest_class: None,
        transcript_recipe: None,
        bd_domain_separator: None,
        bd_schema_major: None,
        bd_included_field_tags: None,
        bd_excluded_field_tags: None,
        recipe_pin: None,
    };
    identity.fields.push(consumer.clone());
    consumer.containing_schema = second_container;
    consumer.field_tag = 0x7ffd;
    consumer.stable_name = "second_fixture_wire_backed_union".into();
    consumer.construction_order = identity.logical[0].construction_order;
    consumer.role_predicate = "role-meta".into();
    identity.fields.push(consumer);

    let valid_violations: Vec<_> = identity::validate_identity(&identity)
        .into_iter()
        .filter(|violation| violation.code != "registry_assignment_drift")
        .collect();
    assert!(
        valid_violations.is_empty(),
        "a named top-level union may be reused by multiple exact inline fields: {valid_violations:?}"
    );

    identity
        .fields
        .last_mut()
        .expect("second consumer exists")
        .max_size_bytes = union_bound - 1;
    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_field_mismatch".to_owned()],
        "every consumer must admit the full top-level union encoding"
    );

    identity
        .fields
        .last_mut()
        .expect("second consumer exists")
        .max_size_bytes = union_bound;
    identity
        .fields
        .last_mut()
        .expect("second consumer exists")
        .role_predicate = "role-shard".into();
    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_field_mismatch".to_owned()],
        "a shard-only consumer must not inhabit a Local-or-Meta union"
    );
}

#[test]
fn idr_ordinary_union_rejects_duplicate_arm_tag() {
    let mut identity = ordinary_top_level_union_fixture();
    let first_arm_tag = identity.ordinary_unions[0].arms[0].arm_tag;
    identity.ordinary_unions[0].arms[1].arm_tag = first_arm_tag;

    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_arm_duplicate_tag".to_owned()],
    );
}

#[test]
fn idr_ordinary_union_rejects_invalid_inline_record_hash() {
    let mut identity = ordinary_top_level_union_fixture();
    identity.ordinary_unions[0].arms[1].payload_sha256 = Some("not-a-sha256".into());

    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_arm_payload_mismatch".to_owned()],
    );
}

#[test]
fn idr_ordinary_union_rejects_unresolved_containing_schema() {
    let mut identity = ordinary_top_level_union_fixture();
    identity.ordinary_unions[0].containing_schema = "MissingFixtureSchema".into();
    for arm in &mut identity.ordinary_unions[0].arms {
        arm.containing_schema = "MissingFixtureSchema".into();
    }

    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_unresolved_schema".to_owned()],
    );
}

#[test]
fn idr_ordinary_union_rejects_reference_union_name_collision() {
    let mut identity = ordinary_top_level_union_fixture();
    let colliding_name = identity.unions[0].union_name.clone();
    identity.ordinary_unions[0]
        .union_name
        .clone_from(&colliding_name);
    for arm in &mut identity.ordinary_unions[0].arms {
        arm.union_name.clone_from(&colliding_name);
    }

    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_name_collision".to_owned()],
    );
}

#[test]
fn idr_ordinary_union_rejects_wire_type_name_collision() {
    let mut identity = ordinary_top_level_union_fixture();
    let colliding_name = identity.wire[0].name.clone();
    identity.ordinary_unions[0]
        .union_name
        .clone_from(&colliding_name);
    for arm in &mut identity.ordinary_unions[0].arms {
        arm.union_name.clone_from(&colliding_name);
    }

    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_name_collision".to_owned()],
    );
}

#[test]
fn idr_reference_union_rejects_registered_wire_name_collision_at_every_lifecycle() {
    for (offset, lifecycle) in ["active", "reserved", "retired"].into_iter().enumerate() {
        let name = format!("FixtureWireCollision{offset}");
        let mut identity = real_identity();
        rename_logical_command_input_union(&mut identity, &name);
        identity.wire.push(WireType {
            wire_type_id: 0x7f00 + i64::try_from(offset).expect("fixture offset fits i64"),
            name,
            kind: "reference_wrapper".into(),
            status: lifecycle.into(),
            containing_union: None,
            wire_tag: None,
            encoding_context: "fixture wire/reference namespace collision".into(),
            allowed_containing_schemas: vec!["*".into()],
            max_size_bytes: 48,
        });

        assert_eq!(
            codes_without_assignment_drift(&identity),
            vec!["reference_union_name_collision".to_owned()],
            "{lifecycle} wire assignment did not permanently own its type name"
        );
    }
}

#[test]
fn idr_reference_union_rejects_builtin_wire_name_collision() {
    let mut identity = real_identity();
    rename_logical_command_input_union(&mut identity, "u64");

    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["reference_union_name_collision".to_owned()],
    );
}

#[test]
fn idr_reference_union_rejects_ordinary_union_name_collision() {
    let mut identity = ordinary_top_level_union_fixture();
    let name = identity.ordinary_unions[0].union_name.clone();
    rename_logical_command_input_union(&mut identity, &name);

    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_name_collision".to_owned()],
    );
}

#[test]
fn appendix_a_catalog_propagates_reference_union_name_collision() {
    let mut catalog = real_appendix_catalog();
    rename_logical_command_input_union(&mut catalog.identity, "CommandRef");

    let violations = appendix_a::validate_catalog(&catalog);
    assert!(
        violations.iter().any(|violation| {
            violation.code == "projection_reference_union_name_collision"
                && violation.row_id == "durable_fields::CommandRef"
        }),
        "catalog validation did not propagate the identity collision: {violations:?}"
    );
}

#[test]
fn idr_ordinary_union_embedded_field_requires_exact_anchor() {
    let mut identity = ordinary_top_level_union_fixture();
    let field_tag = 0x7ffe;
    let anchor_index = identity.fields.len();
    identity.fields.push(FieldRow {
        containing_schema: "RootBootstrap".into(),
        field_tag,
        stable_name: "fixture_union".into(),
        exact_wire_type: "FixtureTopLevelUnion".into(),
        cardinality: "one".into(),
        identity_class: "inline".into(),
        reference_semantics: "none".into(),
        target_schema_id: None,
        construction_order: 0,
        role_predicate: "true".into(),
        retention_and_cut_rule: "embedded-fixture".into(),
        version_status: "active".into(),
        max_size_bytes: 128,
        digest_class: None,
        transcript_recipe: None,
        bd_domain_separator: None,
        bd_schema_major: None,
        bd_included_field_tags: None,
        bd_excluded_field_tags: None,
        recipe_pin: None,
    });

    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_field_mismatch".to_owned()],
    );

    identity.ordinary_unions[0].field_tag = Some(field_tag);
    assert!(
        codes_without_assignment_drift(&identity).is_empty(),
        "an embedded union with one exact field anchor must validate"
    );

    let mut scalar_anchor = identity.clone();
    scalar_anchor.fields[anchor_index].identity_class = "scalar".into();
    assert_eq!(
        codes_without_assignment_drift(&scalar_anchor),
        vec!["ordinary_union_field_mismatch".to_owned()],
        "an ordinary union is an inline value, not a scalar field"
    );

    let mut reference_anchor = identity.clone();
    reference_anchor.fields[anchor_index].reference_semantics = "locator".into();
    assert_eq!(
        codes_without_assignment_drift(&reference_anchor),
        vec!["ordinary_union_field_mismatch".to_owned()],
        "an ordinary union field cannot silently acquire reference semantics"
    );

    let mut targeted_anchor = identity.clone();
    targeted_anchor.fields[anchor_index].target_schema_id = Some(identity.logical[0].name.clone());
    assert_eq!(
        codes_without_assignment_drift(&targeted_anchor),
        vec!["ordinary_union_field_mismatch".to_owned()],
        "a non-reference ordinary union field cannot name a reference target"
    );

    let mut undersized_anchor = identity.clone();
    undersized_anchor.fields[anchor_index].max_size_bytes =
        identity.ordinary_unions[0].max_size_bytes - 1;
    assert_eq!(
        codes_without_assignment_drift(&undersized_anchor),
        vec!["ordinary_union_field_mismatch".to_owned()],
        "the field bound must admit every byte allowed by the union bound"
    );

    let mut lifecycle_mismatched_anchor = identity.clone();
    lifecycle_mismatched_anchor.fields[anchor_index].version_status = "reserved".into();
    assert_eq!(
        codes_without_assignment_drift(&lifecycle_mismatched_anchor),
        vec!["ordinary_union_field_mismatch".to_owned()],
        "field and union lifecycle states must move together"
    );

    let mut role_broadened_anchor = identity;
    role_broadened_anchor.ordinary_unions[0].role_predicate = "role-local".into();
    for arm in &mut role_broadened_anchor.ordinary_unions[0].arms {
        arm.role_predicate = "role-local".into();
    }
    assert_eq!(
        codes_without_assignment_drift(&role_broadened_anchor),
        vec!["ordinary_union_field_mismatch".to_owned()],
        "an embedded field must not expose its ordinary union outside the union role scope"
    );
}

#[test]
fn idr_ordinary_union_arm_bound_must_fit_union_bound() {
    let mut identity = ordinary_top_level_union_fixture();
    identity.ordinary_unions[0].arms[1].max_size_bytes = 129;

    assert_eq!(
        codes_without_assignment_drift(&identity),
        vec!["ordinary_union_arm_bound_exceeds_union".to_owned()],
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
    assert_eq!(
        identity::A10_COMMAND_REF_ERRATUM_PREVIOUS_FIELDS_PIN,
        "fnv1a64:bdbcdc27ccd92518",
        "the pre-codec A10 CommandRef erratum witness must remain explicit"
    );
    let mut pre_erratum = r.clone();
    let current_union_count = pre_erratum.ordinary_unions.len();
    pre_erratum.ordinary_unions.retain(|union| {
        !matches!(
            union.union_name.as_str(),
            "KeyDestroyExternalAckRef" | "KeyDestroyFloorRef" | "KeyDestructionTarget"
        )
    });
    assert_eq!(
        pre_erratum.ordinary_unions.len() + 3,
        current_union_count,
        "the historical witness must remove exactly the three post-erratum A15 unions"
    );
    rename_logical_command_input_union(&mut pre_erratum, "CommandRef");
    undo_a01_exactness_repair(&mut pre_erratum);
    let reconstructed_previous_fields_pin = identity::assignment_pins(&pre_erratum)
        .into_iter()
        .find(|pin| pin.registry == "durable_fields")
        .expect("durable-fields assignment pin exists")
        .actual_pin;
    assert_eq!(
        reconstructed_previous_fields_pin,
        identity::A10_COMMAND_REF_ERRATUM_PREVIOUS_FIELDS_PIN,
        "the historical witness must reconstruct from the exact pre-erratum namespace"
    );
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

#[test]
fn idr_a01_incomplete_activation_cohort_is_reserved() {
    const INCOMPLETE_LOGICAL_KINDS: [&str; 18] = [
        "ExportLeaf",
        "RemoteAuthorityConfigurationEvidence",
        "RemotePayloadAvailabilityEvidence",
        "RemoteReleaseSummaryEntry",
        "RemoteRetentionAckPublishRecord",
        "RemoteRetentionConsumeAckRecord",
        "RemoteRetentionGrantEvidence",
        "RemoteRetentionGrantRecord",
        "RemoteRetentionGrantSpec",
        "RemoteRetentionReleaseAckCertificate",
        "RemoteRetentionReleaseApplySpec",
        "RemoteRetentionReleaseRequestCertificate",
        "RemoteRetentionReleaseRequestRecord",
        "RemoteRetentionReleaseRequestSpec",
        "RemoteRetentionReleaseTombstone",
        "RoleTransitionActivationState",
        "RootAuthorityTrustArtifact",
        "RootAuthorityTrustBody",
    ];
    const INCOMPLETE_FIELD_SCHEMAS: [&str; 16] = [
        "RemoteAuthorityConfigurationEvidence",
        "RemotePayloadAvailabilityEvidence",
        "RemoteReleaseSummaryEntry",
        "RemoteRetentionAckPublishRecord",
        "RemoteRetentionConsumeAckRecord",
        "RemoteRetentionGrantEvidence",
        "RemoteRetentionGrantRecord",
        "RemoteRetentionGrantSpec",
        "RemoteRetentionReleaseAckCertificate",
        "RemoteRetentionReleaseApplySpec",
        "RemoteRetentionReleaseRequestCertificate",
        "RemoteRetentionReleaseRequestRecord",
        "RemoteRetentionReleaseRequestSpec",
        "RemoteRetentionReleaseTombstone",
        "RootAuthorityTrustArtifact",
        "RootAuthorityTrustBody",
    ];

    let r = real_identity();
    let logical_names: BTreeSet<_> = INCOMPLETE_LOGICAL_KINDS.into_iter().collect();
    let logical: Vec<_> = r
        .logical
        .iter()
        .filter(|row| logical_names.contains(row.name.as_str()))
        .collect();
    assert_eq!(logical.len(), 18);
    assert!(
        logical.iter().all(|row| row.status == "reserved"),
        "incomplete A01 logical kinds must not be consumable"
    );

    let wire: Vec<_> = r
        .wire
        .iter()
        .filter(|row| (0x0012..=0x0026).contains(&row.wire_type_id))
        .collect();
    assert_eq!(wire.len(), 21);
    assert!(
        wire.iter().all(|row| row.status == "reserved"),
        "incomplete A01 wire rows must not be consumable"
    );

    let incomplete_schemas: BTreeSet<_> = INCOMPLETE_FIELD_SCHEMAS.into_iter().collect();
    let fields: Vec<_> = r
        .fields
        .iter()
        .filter(|row| incomplete_schemas.contains(row.containing_schema.as_str()))
        .collect();
    assert_eq!(fields.len(), 109);
    assert!(
        fields.iter().all(|row| row.version_status == "reserved"),
        "incomplete A01 durable fields must not be consumable"
    );

    let bootstrap_fields: Vec<_> = r
        .fields
        .iter()
        .filter(|row| matches!(row.containing_schema.as_str(), "RootSlot" | "RootBootstrap"))
        .collect();
    assert_eq!(bootstrap_fields.len(), 48);
    assert!(
        bootstrap_fields
            .iter()
            .all(|row| row.version_status == "active"),
        "source-exact RootSlot and RootBootstrap fields stay active"
    );

    let unions: Vec<_> = r
        .ordinary_unions
        .iter()
        .filter(|row| {
            matches!(
                row.union_name.as_str(),
                "TrustTransition" | "RootAuthorityTrustArtifactKind"
            )
        })
        .collect();
    assert_eq!(unions.len(), 2);
    assert!(
        unions.iter().all(|row| {
            row.version_status == "reserved"
                && row.arms.iter().all(|arm| arm.version_status == "reserved")
        }),
        "incomplete A01 ordinary-union closure must stay reserved"
    );
    assert_eq!(unions.iter().map(|row| row.arms.len()).sum::<usize>(), 5);
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
    let (epoch, fields, ordinary_unions, unions) =
        identity::fields_from(&table).expect("mutated fields model");
    let mut mutated = r.clone();
    mutated.fields_epoch = epoch;
    mutated.fields = fields;
    mutated.ordinary_unions = ordinary_unions;
    mutated.unions = unions;
    assert!(!identity::validate_identity(&mutated).is_empty());
}
