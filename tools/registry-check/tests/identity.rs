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
use registry_check::architecture;
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
registry_epoch = 70

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

    assert_eq!(epoch, 70);
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

/// Reverse the cq4x capsule retarget so the pre-erratum durable-fields pin keeps
/// reconstructing from live rows.  `CommitMarker.capsule_ref` is a pre-erratum
/// field whose row was MODIFIED rather than added -- its `target_schema_id` moved
/// from the g0 scaffold `CommitCapsule` to the source-named `CommittedEffectCapsule`
/// (the plan spells `capsule_ref:StrongRef<CommittedEffectCapsule>` at 393/1912/1944
/// and never mentions `CommitCapsule`).  A modification is invisible to the
/// post-erratum row filters above, which only drop ADDED rows, so it has to be
/// undone here the same way the A01 wire-type flips are.
fn undo_cq4x_capsule_retarget(identity: &mut IdentityRegistries) {
    let field = identity
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "CommitMarker" && field.stable_name == "capsule_ref"
        })
        .expect("CommitMarker.capsule_ref exists");
    field.target_schema_id = Some("CommitCapsule".to_owned());
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
                "schema_version = 5",
                "schema_version = 5\nunknown_root_key = true",
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
            source.replacen("schema_version = 5", "schema_version = 4", 1),
            "catalog_pin_mismatch",
            "schema_version",
        ),
        (
            "future schema version",
            source.replacen("schema_version = 5", "schema_version = 6", 1),
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
                "registry_epoch = 1\n",
                "registry_epoch = 1\nunknown_projection_epoch_key = true\n",
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
fn appendix_a_completion_layer_schemas_are_readable_closed_and_versioned() {
    let source = real_appendix_catalog_text();
    let baseline = real_appendix_catalog();
    assert_eq!(baseline.completion_layers.len(), 4);
    assert_eq!(
        appendix_a::completion_layer_schema_sha256(&baseline.completion_layers),
        appendix_a::EXPECTED_COMPLETION_LAYER_SCHEMA_SHA256
    );

    let unknown = source.replacen(
        "[[completion_layer]]",
        "[[completion_layer]]\nunknown_completion_key = true",
        1,
    );
    let violations = appendix_a::parse_catalog(&unknown)
        .expect_err("unknown completion-layer key must fail closed");
    assert!(has_violation(
        &violations,
        "catalog_unknown_key",
        "unknown_completion_key"
    ));

    let wrong_type = source.replacen(
        "layer = \"annotation\"\nschema_version = 1",
        "layer = \"annotation\"\nschema_version = \"1\"",
        1,
    );
    let violations =
        appendix_a::parse_catalog(&wrong_type).expect_err("wrong schema_version type must fail");
    assert!(has_violation(
        &violations,
        "catalog_schema",
        "schema_version"
    ));

    let missing = source.replacen(
        "authoring_policy = \"reviewed-source-assisted;policy-fields-owner-authored\"\n",
        "",
        1,
    );
    let violations =
        appendix_a::parse_catalog(&missing).expect_err("required authoring policy must fail");
    assert!(has_violation(
        &violations,
        "catalog_schema",
        "authoring_policy"
    ));

    let reordered = swap_first_two_table_blocks(&source, "[[completion_layer]]");
    let violations =
        appendix_a::parse_catalog(&reordered).expect_err("completion layers have canonical order");
    assert!(
        violations.iter().any(|violation| {
            matches!(
                violation.code.as_str(),
                "catalog_completion_layer_schema_drift"
                    | "catalog_completion_layer_schema_mismatch"
            )
        }),
        "reordered completion layers escaped the readable/hash pins: {violations:?}"
    );
}

#[test]
fn appendix_a_empty_completion_row_schemas_still_reject_wrong_field_types() {
    let source = real_appendix_catalog_text();
    let baseline = real_appendix_catalog();
    let fixtures = [
        (
            "annotation",
            "annotation",
            r#"
[[annotation]]
row_id = "a01:annotation:bootstrap-frame-root-slot"
target_row_id = "a01:bootstrap-frame:root-slot"
exact_type = "RootSlot"
cardinality = "one"
layout = "fixed"
role = "Local"
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
"#,
        ),
        (
            "semantic binding",
            "semantic_binding",
            r#"
[[semantic_binding]]
row_id = "a01:semantic-binding:bootstrap-frame-root-slot"
target_row_id = "a01:bootstrap-frame:root-slot"
owner_bead_id = "fgdb-w10-fixture"
owner_crate = "fgdb-fixture"
owner_status = "planned"
consumer_crates = ["fgdb"]
"#,
        ),
        (
            "expansion binding",
            "expansion_binding",
            r#"
[[expansion_binding]]
row_id = "a01:expansion-binding:bootstrap-frame-root-slot-parameter-1-role"
target_row_id = "a01:bootstrap-frame:root-slot"
parameter_ordinal = 1
formal = "Role"
formal_class = "role"
values = ["Local"]
rationale = "fixture"
"#,
        ),
        (
            "evidence",
            "evidence",
            r#"
[[evidence]]
row_id = "a01:evidence:bootstrap-frame-root-slot-static-contract"
target_row_id = "a01:bootstrap-frame:root-slot"
evidence_id = "static-contract"
phase = "static"
status = "live"
owner_bead_id = "fgdb-w10-fixture"
checker_ids = ["appendix_a_catalog_closure"]
scenario_ids = ["g0_identity_e2e"]
event_ids = ["appendix_closure_checked"]
gate_ids = ["G0"]
"#,
        ),
    ];
    for (name, layer, fixture) in fixtures {
        let mut shaped = source.clone();
        shaped.push_str(fixture);
        let violations =
            appendix_a::parse_catalog(&shaped).expect_err("unreleased fixture row must fail pins");
        assert!(
            !violations
                .iter()
                .any(|violation| violation.code == "catalog_schema"),
            "{name} fixture does not satisfy its frozen structural shape: {violations:?}"
        );

        let schema = baseline
            .completion_layers
            .iter()
            .find(|row| row.layer == layer)
            .expect("fixture completion schema");
        for contract in &schema.field_contracts {
            let mut parts = contract.split(':');
            let field = parts.next().expect("field contract name");
            let field_type = parts.next().expect("field contract type");
            assert_eq!(parts.next(), Some("required"));
            assert_eq!(parts.next(), None);
            let prefix = format!("{field} = ");
            let assignment = fixture
                .lines()
                .find(|line| line.starts_with(&prefix))
                .expect("fixture carries every required field");
            assert!(
                matches!(field_type, "string" | "integer" | "string-array"),
                "unsupported frozen field type {field_type:?}"
            );
            let wrong_assignment = if field_type == "string" {
                format!("{field} = 1")
            } else {
                format!("{field} = \"wrong-type\"")
            };

            let mut malformed = source.clone();
            malformed.push_str(&fixture.replacen(assignment, &wrong_assignment, 1));
            let violations = appendix_a::parse_catalog(&malformed)
                .expect_err("wrong completion-row field type must fail structurally");
            assert!(
                has_violation(&violations, "catalog_schema", field),
                "{name}.{field} accepted the wrong field type: {violations:?}"
            );

            let mut missing = source.clone();
            missing.push_str(&fixture.replacen(assignment, "", 1));
            let violations = appendix_a::parse_catalog(&missing)
                .expect_err("missing required completion-row field must fail structurally");
            assert!(
                has_violation(&violations, "catalog_schema", field),
                "{name}.{field} was not required: {violations:?}"
            );
        }
    }
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

    let mut duplicate_semantic = baseline.clone();
    duplicate_semantic.semantic_bindings.push(valid.clone());
    let mut duplicate = valid.clone();
    duplicate.row_id.push_str("-duplicate");
    duplicate_semantic.semantic_bindings.push(duplicate);
    let violations = appendix_a::validate_catalog(&duplicate_semantic);
    assert!(
        violations
            .iter()
            .any(|violation| { violation.code == "catalog_semantic_binding_duplicate" })
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
    assert!(
        violations
            .iter()
            .any(|violation| violation.code == "catalog_annotation_contract_unapproved"),
        "an annotation without an independent readable pin self-authorized: {violations:?}"
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
    let generated_before = appendix_a::generated_projections(&catalog);
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
    let mut all_completion_metadata = catalog.clone();
    all_completion_metadata
        .semantic_bindings
        .push(appendix_a::SemanticBinding {
            row_id: "a01:semantic-binding:field-root-slot-cluster-incarnation".to_owned(),
            target_row_id: annotation.target_row_id.clone(),
            owner_bead_id: "fgdb-w10-fixture".to_owned(),
            owner_crate: "fgdb-formats".to_owned(),
            owner_status: "planned".to_owned(),
            consumer_crates: vec!["fgdb".to_owned()],
        });
    all_completion_metadata
        .expansion_bindings
        .push(appendix_a::ExpansionBinding {
            row_id: "a01:expansion-binding:field-root-slot-cluster-incarnation-parameter-1-role"
                .to_owned(),
            target_row_id: annotation.target_row_id.clone(),
            parameter_ordinal: 1,
            formal: "Role".to_owned(),
            formal_class: "role".to_owned(),
            values: vec!["Local".to_owned()],
            rationale: "projection non-effect fixture".to_owned(),
        });
    all_completion_metadata
        .evidence
        .push(appendix_a::EvidenceBinding {
            row_id: "a01:evidence:field-root-slot-cluster-incarnation-static-contract".to_owned(),
            target_row_id: annotation.target_row_id.clone(),
            evidence_id: "static-contract".to_owned(),
            phase: "static".to_owned(),
            status: "live".to_owned(),
            owner_bead_id: "fgdb-verification-fixture".to_owned(),
            checker_ids: vec!["appendix_a_catalog_closure".to_owned()],
            scenario_ids: vec!["g0_identity_e2e".to_owned()],
            event_ids: vec!["appendix_closure_checked".to_owned()],
            gate_ids: vec!["G0".to_owned()],
        });
    assert_eq!(
        appendix_a::generated_projections(&all_completion_metadata),
        generated_before,
        "catalog-only completion metadata must not participate in a generated registry epoch"
    );
    let source = real_plan_source();
    let violations = appendix_a::appendix_a_catalog_source(&catalog, &source);
    assert!(
        !violations
            .iter()
            .any(|violation| violation.code == "source_annotation_contract_mismatch"),
        "source-exact field annotation was rejected: {violations:?}"
    );
    // Positive control above licenses the source measurement. Its
    // interpretation is deliberately narrower: the census determines type
    // and cardinality, but cannot choose policy. Both policy-distinct rows
    // satisfy the identical source facts and therefore require reviewed,
    // independently pinned authoring rather than a census generator.
    let mut policy_distinct = catalog.clone();
    policy_distinct.annotations[0].posture = "durable".to_owned();
    policy_distinct.annotations[0].authority = "local-authority".to_owned();
    policy_distinct.annotations[0].digest_recipe = "canonical-u64".to_owned();
    policy_distinct.annotations[0].redaction_class = "private-metadata".to_owned();
    let violations = appendix_a::appendix_a_catalog_source(&policy_distinct, &source);
    assert!(
        !violations
            .iter()
            .any(|violation| violation.code == "source_annotation_contract_mismatch"),
        "source facts unexpectedly chose policy fields: {violations:?}"
    );
    assert_ne!(policy_distinct.annotations[0], catalog.annotations[0]);

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

/// An unresolvable bead ANYWHERE must not make the Appendix A repository
/// bindings unavailable, and a bead Appendix A actually NAMES must still fail
/// when it does not resolve. Those two are the whole contract, and they used to
/// be one: the check consumed the TOTAL bead index, so a single orphaned record
/// about anything at all returned `catalog_repository_beads_unavailable`, which
/// blocked `appendix-regenerate` for every slice at once and stalled the
/// catalog. Membership is what this check needs; totality is the architecture
/// registry's own claim and is still enforced there.
///
/// PROVEN RED BY: pointing `verify_repository_bindings` back at
/// `bead_provenance_index` — this test and
/// `appendix_a_repository_bindings_resolve_beads_crates_checkers_and_events`
/// both go red while the tree carries an orphaned record.
#[test]
fn an_unrelated_orphan_bead_does_not_make_appendix_bindings_unavailable() {
    let root = repo_root();
    let catalog = real_appendix_catalog();

    // The live tree is the strongest fixture available when it genuinely
    // carries records that resolve nowhere. Guard against the test quietly
    // becoming vacuous if the tree is later made total.
    let registry = architecture::load_from_repo(&root).expect("architecture registry loads");
    let membership =
        architecture::bead_provenance_membership(&registry, &root).expect("membership resolves");
    assert!(
        !membership.is_empty(),
        "membership must return the beads that DO resolve"
    );
    if architecture::bead_provenance_index(&registry, &root).is_err() {
        let violations = appendix_a::verify_repository_bindings(&root, &catalog);
        assert!(
            !violations
                .iter()
                .any(|v| v.code == "catalog_repository_beads_unavailable"),
            "an unresolvable bead elsewhere must not make Appendix bindings unavailable; got {:?}",
            violations
                .iter()
                .map(|v| v.code.as_str())
                .collect::<Vec<_>>()
        );
    }

    // The other direction: a bead Appendix A NAMES but which does not resolve
    // must still fail, attributed to the row that named it.
    let mut broken = real_appendix_catalog();
    broken.maintenance_proof.owner_bead_id = "fgdb-a-bead-that-does-not-exist".to_owned();
    let violations = appendix_a::verify_repository_bindings(&root, &broken);
    assert!(
        violations
            .iter()
            .any(|v| v.code == "catalog_maintenance_owner_bead_unresolved"),
        "a named bead that resolves nowhere must still fail; got {:?}",
        violations
            .iter()
            .map(|v| v.code.as_str())
            .collect::<Vec<_>>()
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
    // Any registered STUB symbol proves the rule; this one is deliberately a
    // deeply-blocked oracle checker (crates/fgdb-oracles does not exist), so it
    // stays stub far longer than a checker whose subsystem is already in-tree.
    // It previously cited idr_generated_encoder_decoder_roundtrip, which has
    // since gone live now that its harness landed.
    stub_live.evidence[0].checker_ids = vec!["fg_inv_01_core_checker".to_owned()];
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
    // 1_231 -> 1_234: fgdb-ihtt bound the four heading-led appendix bodies, and
    // LogicalDeltaTemplate, RecoveryCheckpoint and BranchManifest became candidates
    // for the first time (CommitCommand already had a row, name-only, and was
    // promoted to confirmed rather than added). 1_234 -> 1_236: fgdb-801o
    // separated the two second definition heads that an earlier union in the
    // same sentence had swallowed.
    assert_eq!(baseline.top_level_candidates.len(), 1_236);
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
fn appendix_a_generated_reference_union_targets_have_a_satisfiable_state() {
    // A per-anchor reference union is GENERATED, never written.  The plan
    // states the rule, not the instance — a01:1402 defines the family as "the
    // containing-schema-generated closed union with one typed strong-reference
    // arm per exportable authority-local target kind" — so the per-anchor name
    // can never acquire a source census key in any spelling.
    //
    // Before this law, such a row had NO satisfiable state once its slice went
    // complete: `declared` fired `complete_slice_target_declared`, `complete`
    // fired `catalog_target_projection_incomplete`, and either state fired
    // `complete_slice_source_contract_unverified`, which is not gated on the
    // target's own status.  Three laws, empty intersection.
    let mut catalog = real_appendix_catalog();
    let generated: Vec<(String, String, String)> = catalog
        .targets
        .iter()
        .filter(|target| {
            matches!(
                target.target_kind.as_str(),
                "reference-union" | "reference-union-arm"
            ) && target.slice_id != "g0"
        })
        .map(|target| {
            (
                target.slice_id.clone(),
                target.row_id.clone(),
                target.target_row_id.clone(),
            )
        })
        .collect();
    // g0 carries three more, structurally exempt because g0 owns no `[[slice]]`
    // row and the completion laws iterate slices.
    assert_eq!(
        generated.len(),
        10,
        "the generated reference-union population moved; re-measure before trusting this suite"
    );

    let armed_slices: BTreeSet<&str> = generated
        .iter()
        .map(|(slice_id, _, _)| slice_id.as_str())
        .collect();
    assert_eq!(
        armed_slices,
        BTreeSet::from(["a04", "a06", "a10"]),
        "generated reference unions moved slices"
    );
    let armed_slices: BTreeSet<String> = armed_slices.iter().map(|id| (*id).to_owned()).collect();
    let armed_rows: BTreeSet<&str> = generated
        .iter()
        .map(|(_, _, target_row_id)| target_row_id.as_str())
        .collect();
    let armed_rows: BTreeSet<String> = armed_rows.iter().map(|id| (*id).to_owned()).collect();
    for slice in &mut catalog.slices {
        if armed_slices.contains(&slice.id) {
            slice.definition_status = "complete".to_owned();
        }
    }
    for target in &mut catalog.targets {
        if armed_rows.contains(&target.target_row_id) {
            target.definition_status = "complete".to_owned();
        }
    }

    let violations = appendix_a::validate_catalog(&catalog);

    // POSITIVE CONTROL, and it is load-bearing: without it every assertion
    // below passes vacuously on an unarmed battery.  a04 also holds top-level
    // rows whose owner is named in the plan but never structurally rendered;
    // those are a separate, unresolved question and MUST still fire here.
    assert!(
        violations.iter().any(|violation| {
            violation.code == "complete_slice_source_contract_unverified"
                && violation.row_id == "a04:logical-kind:root-manifest"
        }),
        "the completion battery is not armed, so this suite proves nothing: {violations:?}"
    );

    for (_, row_id, target_row_id) in &generated {
        let blocking: Vec<&Violation> = violations
            .iter()
            .filter(|violation| {
                (&violation.row_id == row_id || &violation.row_id == target_row_id)
                    && matches!(
                        violation.code.as_str(),
                        "complete_slice_source_contract_unverified"
                            | "catalog_target_projection_incomplete"
                            | "complete_slice_target_declared"
                    )
            })
            .collect();
        assert!(
            blocking.is_empty(),
            "generated reference union {target_row_id} has no satisfiable state: {blocking:?}"
        );
    }

    // g0 must NOT gain the same escape.  It owns no `[[slice]]` row, so every
    // law that gives `complete` its meaning is slice-gated past it and
    // `catalog_target_projection_incomplete` is the only thing left standing
    // between a g0 target and an unverifiable completion claim.  Widening the
    // escape to g0 passes every count-level check — g0 rows are absent from
    // those counts by construction — so it is pinned here by name.
    let mut g0_complete = real_appendix_catalog();
    let mut flipped = 0;
    for target in &mut g0_complete.targets {
        if target.slice_id == "g0"
            && matches!(
                target.target_kind.as_str(),
                "reference-union" | "reference-union-arm"
            )
        {
            target.definition_status = "complete".to_owned();
            flipped += 1;
        }
    }
    assert_eq!(flipped, 3, "g0 generated reference-union population moved");
    let violations = appendix_a::validate_catalog(&g0_complete);
    assert_eq!(
        violations
            .iter()
            .filter(|violation| violation.code == "catalog_target_projection_incomplete")
            .count(),
        3,
        "a g0 generated reference union may not claim complete: {violations:?}"
    );
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
        ("RoleTransitionActivationState", "Meta"),
        ("RoleTransitionActivationState", "Shard"),
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
fn idr_a18_wire_consumer_allowlists_are_exact() {
    let identity = real_identity();
    for (wire_name, container, field_name, field_tag, construction_order) in [
        (
            "CatalogAbandonPredecessor",
            "CatalogTombstoneRestoreTargetReceipt<Contract>",
            "predecessor",
            0x0003,
            20,
        ),
        (
            "Operational",
            "RestoreTerminalPinBasis<Role>",
            "terminal_disposition",
            0x0003,
            40,
        ),
        (
            "RestoreTerminalPinReleaseAuthorizationBody",
            "RestoreTerminalPinReleaseAuthorization",
            "body",
            0x0001,
            44,
        ),
    ] {
        let expected_consumers = vec![container.to_owned()];
        let wire = identity
            .wire
            .iter()
            .find(|wire| wire.name == wire_name)
            .expect("A18 exact wire type exists");
        assert_eq!(
            wire.allowed_containing_schemas, expected_consumers,
            "{wire_name} must admit exactly its source-derived A18 consumer"
        );
        let field = identity
            .fields
            .iter()
            .find(|field| field.containing_schema == container && field.stable_name == field_name)
            .expect("A18 exact field exists");
        assert_eq!(field.field_tag, field_tag);
        assert_eq!(field.exact_wire_type, wire_name);
        assert_eq!(field.identity_class, "inline");
        assert_eq!(field.reference_semantics, "none");
        assert_eq!(field.construction_order, construction_order);
    }

    let union = identity
        .ordinary_unions
        .iter()
        .find(|union| union.union_name == "CatalogAbandonPredecessor")
        .expect("CatalogAbandonPredecessor ordinary union exists");
    assert_eq!(
        union.allowed_containing_schemas,
        vec!["CatalogTombstoneRestoreTargetReceipt<Contract>".to_owned()],
        "the ordinary union must exactly mirror its wire-parent consumer closure"
    );
}

#[test]
fn idr_a03_wire_consumer_repairs_are_exact_and_non_vacuous() {
    let identity = real_identity();
    let expected = [
        (
            "LocalAuditTicketOwner",
            &["LocalAuditTicketOwner", "LocalAuditTicketClaimRecord"][..],
            "LocalAuditTicketClaimRecord",
            "owner",
            0x0002,
            15,
        ),
        (
            "ResultManifestRef",
            &["ResultManifestRef", "ResultDeliveryPolicy<Role>"][..],
            "ResultDeliveryPolicy<Role>",
            "manifest_ref",
            0x0004,
            20,
        ),
        (
            "ResultDeliveryServiceAuthority",
            &[
                "ResultDeliveryServiceAuthority",
                "ResultDeliveryPolicy<Role>",
            ][..],
            "ResultDeliveryPolicy<Role>",
            "service_authority",
            0x000a,
            20,
        ),
        (
            "LocalResultDeliveryOwner",
            &["LocalResultDeliveryOwner", "LocalResultDeliveryLease"][..],
            "LocalResultDeliveryLease",
            "owner",
            0x0001,
            20,
        ),
        (
            "ResultActivationAppliedRef",
            &[
                "ResultActivationAppliedRef",
                "LocalResultDeliveryLease",
                "ResultDeliveryLease",
            ][..],
            "LocalResultDeliveryLease",
            "activation_applied_ref",
            0x0010,
            20,
        ),
    ];

    for (wire_name, expected_consumers, container, field_name, field_tag, construction_order) in
        expected
    {
        let expected_consumers: Vec<_> = expected_consumers
            .iter()
            .map(|consumer| (*consumer).to_owned())
            .collect();
        let wire = identity
            .wire
            .iter()
            .find(|wire| wire.name == wire_name)
            .expect("A03 repaired wire type exists");
        assert_eq!(
            wire.allowed_containing_schemas, expected_consumers,
            "{wire_name} must admit only its source-derived consumers"
        );
        let union = identity
            .ordinary_unions
            .iter()
            .find(|union| union.union_name == wire_name)
            .expect("A03 repaired ordinary union exists");
        assert_eq!(
            union.allowed_containing_schemas, expected_consumers,
            "{wire_name} ordinary union must exactly mirror its wire parent"
        );
        let field = identity
            .fields
            .iter()
            .find(|field| field.containing_schema == container && field.stable_name == field_name)
            .expect("A03 repaired field exists");
        assert_eq!(field.field_tag, field_tag);
        assert_eq!(field.exact_wire_type, wire_name);
        assert_eq!(field.identity_class, "inline");
        assert_eq!(field.reference_semantics, "none");
        assert_eq!(field.construction_order, construction_order);
        assert_eq!(field.version_status, "reserved");

        let mut narrowed = identity.clone();
        narrowed
            .wire
            .iter_mut()
            .find(|wire| wire.name == wire_name)
            .expect("A03 repaired wire type exists")
            .allowed_containing_schemas
            .retain(|consumer| consumer != container);
        narrowed
            .ordinary_unions
            .iter_mut()
            .find(|union| union.union_name == wire_name)
            .expect("A03 repaired ordinary union exists")
            .allowed_containing_schemas
            .retain(|consumer| consumer != container);
        let row_id = format!("{container}#{field_name}");
        let violations = identity::validate_identity(&narrowed);
        assert!(
            violations.iter().any(|violation| {
                violation.code == "wire_context_mismatch" && violation.row_id == row_id
            }),
            "removing {container} from the exact {wire_name} closure did not restore the original rejection: {violations:?}"
        );
    }

    for wire_name in [
        "LocalAuditTicketOwner",
        "ResultManifestRef",
        "ResultDeliveryServiceAuthority",
        "LocalResultDeliveryOwner",
        "ResultActivationAppliedRef",
    ] {
        let mut unrelated = identity.clone();
        unrelated
            .fields
            .iter_mut()
            .find(|field| {
                field.containing_schema == "AdmittedTxnAbortCommand"
                    && field.stable_name == "authority_bound_header"
            })
            .expect("unrelated real inline field exists")
            .exact_wire_type = wire_name.to_owned();
        let violations = identity::validate_identity(&unrelated);
        assert!(
            violations.iter().any(|violation| {
                violation.code == "wire_context_mismatch"
                    && violation.row_id == "AdmittedTxnAbortCommand#authority_bound_header"
            }),
            "{wire_name} accepted an unrelated registered logical host: {violations:?}"
        );
    }
}

#[test]
fn idr_a18_logical_union_consumers_have_exact_self_rooted_closures() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();
    let expected = [
        (
            "RecoveryTransformSourceBasis<Role>",
            "RecoveryIncarnationTransformPlan<Role>",
            "source_basis",
            0x0003,
            20,
            "field|RecoveryIncarnationTransformPlan<Role>|RecoveryIncarnationTransformPlan<Role>.source_basis|source_basis",
        ),
        (
            "RestoreLeaseOperationTerminalHistory<Role>",
            "RestoreSourceLeaseReleaseOperationSummary<Role:AuthorityOwningRole>",
            "lease_operation_terminal_history",
            0x000d,
            30,
            "field|RestoreSourceLeaseReleaseOperationSummary<Role:AuthorityOwningRole>|RestoreSourceLeaseReleaseOperationSummary<Role:AuthorityOwningRole>.lease_operation_terminal_history|lease_operation_terminal_history",
        ),
        (
            "RestoreAbandonmentTombstoneRef<Role>",
            "RestoreTerminalPinBasis<Role>",
            "abandonment_tombstone_ref",
            0x0011,
            40,
            "field|RestoreTerminalPinBasis<Role>|RestoreTerminalPinBasis<Role>.Abandoned.abandonment_tombstone_ref|abandonment_tombstone_ref",
        ),
    ];
    for (union_name, consumer, stable_name, field_tag, construction_order, source_key) in expected {
        let union = identity
            .ordinary_unions
            .iter()
            .find(|union| union.union_name == union_name)
            .expect("A18 logical union exists");
        assert_eq!(
            union.allowed_containing_schemas,
            vec![union_name.to_owned(), consumer.to_owned()]
        );
        let field = identity
            .fields
            .iter()
            .find(|field| field.containing_schema == consumer && field.stable_name == stable_name)
            .expect("A18 logical-union consumer field exists");
        assert_eq!(field.field_tag, field_tag);
        assert_eq!(field.exact_wire_type, union_name);
        assert_eq!(field.cardinality, "one");
        assert_eq!(field.identity_class, "inline");
        assert_eq!(field.reference_semantics, "none");
        assert_eq!(field.target_schema_id, None);
        assert_eq!(field.construction_order, construction_order);
        assert_eq!(field.version_status, "reserved");
        assert_eq!(field.max_size_bytes, 16_777_216);

        let target = catalog
            .targets
            .iter()
            .find(|target| target.source_key == source_key)
            .expect("A18 logical-union consumer target exists");
        assert_eq!(target.slice_id, "a18");
        assert_eq!(target.target_kind, "field");
        assert_eq!(target.definition_status, "declared");
    }

    let mut missing_consumer = identity.clone();
    missing_consumer
        .ordinary_unions
        .iter_mut()
        .find(|union| union.union_name == "RecoveryTransformSourceBasis<Role>")
        .expect("RecoveryTransformSourceBasis<Role> exists")
        .allowed_containing_schemas
        .pop();
    let missing_codes = codes(&missing_consumer);
    assert!(
        missing_codes.contains(&"ordinary_union_logical_contract_mismatch".to_owned())
            && missing_codes.contains(&"ordinary_union_field_mismatch".to_owned()),
        "an actual A18 inline consumer omitted from the closure must fail: {missing_codes:?}"
    );

    let mut unrelated_consumer = identity;
    unrelated_consumer
        .ordinary_unions
        .iter_mut()
        .find(|union| union.union_name == "RecoveryTransformSourceBasis<Role>")
        .expect("RecoveryTransformSourceBasis<Role> exists")
        .allowed_containing_schemas
        .push("RootManifest".to_owned());
    assert!(
        codes(&unrelated_consumer).contains(&"ordinary_union_logical_contract_mismatch".to_owned()),
        "an unrelated schema without a matching A18 inline field must remain rejected"
    );
}

#[test]
fn idr_key_destruction_operation_plan_reserved_wire_shell_is_exact() {
    let identity = real_identity();
    let wire = identity
        .wire
        .iter()
        .find(|wire| wire.name == "KeyDestructionOperationPlan")
        .expect("KeyDestructionOperationPlan wire record exists");
    assert_eq!(wire.wire_type_id, 0x003a);
    assert_eq!(wire.kind, "record");
    assert_eq!(wire.status, "reserved");
    assert_eq!(wire.containing_union, None);
    assert_eq!(wire.wire_tag, None);
    assert_eq!(
        wire.allowed_containing_schemas,
        vec!["KeyDestroyProposal".to_owned()]
    );
    assert_eq!(wire.max_size_bytes, 65_536);
    for source_member in [
        "operation_id",
        "idempotency_token_digest",
        "target:KeyDestructionTarget",
        "canonical_request_transcript_digest",
        "required_receipt_profile_oid",
    ] {
        assert!(
            wire.encoding_context.contains(source_member),
            "reserved shell lost source member {source_member}"
        );
    }
    assert!(
        wire.encoding_context.contains("sorted by target identity")
            && wire
                .encoding_context
                .contains("no duplicate target or operation ID"),
        "reserved shell lost the source ordering or deduplication law"
    );

    let catalog = real_appendix_catalog();
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == "top|KeyDestructionOperationPlan")
        .expect("KeyDestructionOperationPlan source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "wire");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == "top|KeyDestructionOperationPlan")
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a15:target:wire-type-key-destruction-operation-plan"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a15:wire-type:key-destruction-operation-plan"
    );
    assert_eq!(targets[0].target_kind, "wire-type");
    assert!(
        !catalog
            .reservations
            .iter()
            .any(|reservation| reservation.symbol == "KeyDestructionOperationPlan"),
        "a non-StrongRef wire record must not acquire a type reservation"
    );
}

#[test]
fn idr_weak_epoch_identity_reserved_wire_record_is_exact() {
    let identity = real_identity();
    let wire = identity
        .wire
        .iter()
        .find(|wire| wire.name == "WeakEpochIdentity")
        .expect("WeakEpochIdentity wire record exists");
    assert_eq!(wire.wire_type_id, 0x013d);
    assert_eq!(wire.kind, "record");
    assert_eq!(wire.status, "reserved");
    assert_eq!(wire.containing_union, None);
    assert_eq!(wire.wire_tag, None);
    assert_eq!(wire.allowed_containing_schemas, vec!["*".to_owned()]);
    assert_eq!(wire.max_size_bytes, 16_777_216);
    assert!(
        wire.encoding_context.contains("nonretaining")
            && wire.encoding_context.contains("a13:2004"),
        "the wire record must preserve the source semantics and citation"
    );
    assert!(
        !identity
            .logical
            .iter()
            .any(|logical| logical.name == "WeakEpochIdentity"),
        "the nonretaining identity must not acquire a logical object identity"
    );

    let catalog = real_appendix_catalog();
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == "top|WeakEpochIdentity")
        .expect("WeakEpochIdentity source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "wire");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == "top|WeakEpochIdentity")
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].target_row_id,
        "a13:wire-type:weak-epoch-identity"
    );
    assert_eq!(targets[0].target_kind, "wire-type");
    assert_eq!(targets[0].definition_status, "declared");
    assert!(
        !catalog
            .reservations
            .iter()
            .any(|reservation| reservation.symbol == "WeakEpochIdentity"),
        "a nonretaining wire record must not acquire a type reservation"
    );
}

#[test]
fn idr_a13_branch_install_specs_are_forced_logical_at_the_source_floor() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();
    let cases = [
        (
            "BranchForkSpec",
            0x0556,
            "top|BranchForkSpec<Role:AuthorityOwningRole>",
            "a13:logical-kind:branch-fork-spec",
            "a13:target:logical-kind-branch-fork-spec",
            "corpus/logical/branch_fork_spec/",
        ),
        (
            "BranchGrantSpec",
            0x0557,
            "top|BranchGrantSpec<Role:AuthorityOwningRole>",
            "a13:logical-kind:branch-grant-spec",
            "a13:target:logical-kind-branch-grant-spec",
            "corpus/logical/branch_grant_spec/",
        ),
    ];

    for (name, object_kind, source_key, logical_row_id, target_row_id, corpus) in cases {
        let logical = identity
            .logical
            .iter()
            .find(|row| row.name == name)
            .expect("branch install spec logical kind exists");
        assert_eq!(logical.object_kind, object_kind);
        assert_eq!(logical.status, "reserved");
        assert_eq!(
            logical.construction_order, 80,
            "the spec follows its already-built order-80 bundle"
        );
        assert_eq!(logical.role_predicate, "true");
        assert_eq!(logical.max_size_bytes, 16_777_216);
        assert_eq!(logical.golden_corpus, corpus);
        assert!(
            !identity.wire.iter().any(|row| row.name == name),
            "a source record that owns durable fields cannot be wire-owned"
        );

        let candidate = catalog
            .top_level_candidates
            .iter()
            .find(|row| row.source_key == source_key)
            .expect("branch install spec source candidate exists");
        assert_eq!(candidate.source_kind, "confirmed");
        assert_eq!(candidate.identity_class, "logical");
        assert_eq!(candidate.generic_signature, "<Role:AuthorityOwningRole>");

        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{name} source maps exactly once");
        assert_eq!(targets[0].row_id, target_row_id);
        assert_eq!(targets[0].target_row_id, logical_row_id);
        assert_eq!(targets[0].target_kind, "logical-kind");
        assert_eq!(targets[0].definition_status, "declared");
        assert!(
            !catalog
                .reservations
                .iter()
                .any(|reservation| reservation.symbol == name),
            "fresh post-reservation logical kinds must not fabricate reservations"
        );
    }

    for target in [
        "BranchEpochBoundaryReservation",
        "BranchForkBundle",
        "BranchGrantBundle",
        "PayloadAvailabilityCertificate",
    ] {
        let target_order = identity
            .logical
            .iter()
            .find(|row| row.name == target)
            .expect("branch install spec source dependency exists")
            .construction_order;
        assert!(
            target_order <= 80,
            "{target} must be constructed no later than either install spec"
        );
    }
}

#[test]
fn idr_object_creation_boundary_is_source_ordered_and_wire_backed() {
    let identity = real_identity();
    let parent = identity
        .wire
        .iter()
        .find(|wire| wire.name == "ObjectCreationBoundary")
        .expect("ObjectCreationBoundary wire parent exists");
    assert_eq!(parent.wire_type_id, 0x013e);
    assert_eq!(parent.kind, "union", "a wire record cannot back this union");
    assert_eq!(parent.status, "reserved");
    assert_eq!(parent.containing_union, None);
    assert_eq!(parent.wire_tag, None);
    assert_eq!(
        parent.allowed_containing_schemas,
        vec!["KeyWrap".to_owned()]
    );

    let variants = identity
        .wire
        .iter()
        .filter(|wire| wire.containing_union.as_deref() == Some("ObjectCreationBoundary"))
        .map(|wire| (wire.name.as_str(), wire.wire_type_id, wire.wire_tag))
        .collect::<Vec<_>>();
    assert_eq!(
        variants,
        vec![
            (
                "ObjectCreationBoundary.committed_existing",
                0x013f,
                Some(0x0001),
            ),
            (
                "ObjectCreationBoundary.transaction_reserved",
                0x0140,
                Some(0x0002),
            ),
            (
                "ObjectCreationBoundary.branch_epoch_reserved",
                0x0141,
                Some(0x0003),
            ),
        ],
        "wire variants must preserve source order, not alphabetical census order"
    );
    assert!(
        identity
            .wire
            .iter()
            .filter(|wire| wire.containing_union.as_deref() == Some("ObjectCreationBoundary"))
            .all(|wire| {
                wire.allowed_containing_schemas == vec!["ObjectCreationBoundary".to_owned()]
            }),
        "every wire variant must be scoped to its exact union"
    );

    let union = identity
        .ordinary_unions
        .iter()
        .find(|union| union.union_name == "ObjectCreationBoundary")
        .expect("ObjectCreationBoundary ordinary union exists");
    assert_eq!(union.containing_schema, union.union_name);
    assert_eq!(union.union_path, union.union_name);
    assert_eq!(union.field_tag, None);
    assert_eq!(union.allowed_containing_schemas, vec!["KeyWrap".to_owned()]);
    let arms = union
        .arms
        .iter()
        .map(|arm| {
            (
                arm.source_arm_name.as_str(),
                arm.arm_tag,
                arm.stable_name.as_str(),
                arm.payload_sha256.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        arms,
        vec![
            (
                "CommittedExisting",
                0x0001,
                "committed_existing",
                Some("de6fc7f8a71f1d25a8c555795484a9439a1edbe139003a50355edf9081195887"),
            ),
            (
                "TransactionReserved",
                0x0002,
                "transaction_reserved",
                Some("bb2d7c8843ad9a86b0f4c61ed6baf2bb172ed187a33d2fcb34de12b24e7d679f"),
            ),
            (
                "BranchEpochReserved",
                0x0003,
                "branch_epoch_reserved",
                Some("ce0e32685eef34d92ba0a1bee0e1f217316dbce6c59013fb92faebf7a925b7ce"),
            ),
        ],
        "the plan spells CommittedExisting, TransactionReserved, BranchEpochReserved"
    );

    let catalog = real_appendix_catalog();
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == "top|ObjectCreationBoundary")
        .expect("ObjectCreationBoundary source candidate exists");
    assert_eq!(candidate.identity_class, "wire");
    assert_eq!(
        catalog
            .targets
            .iter()
            .filter(|target| {
                target.source_key == "top|ObjectCreationBoundary"
                    || target
                        .source_key
                        .starts_with("union|ObjectCreationBoundary|")
                    || target.source_key.starts_with("arm|ObjectCreationBoundary|")
            })
            .count(),
        8,
        "one parent, one union, three arms, and three wire variants must be targeted"
    );
    assert!(
        !catalog
            .reservations
            .iter()
            .any(|reservation| reservation.symbol == "ObjectCreationBoundary"),
        "an inline non-StrongRef family must not acquire a reservation"
    );
}

#[test]
fn idr_a02_location_form_is_source_ordered_and_wire_backed() {
    let identity = real_identity();
    let parent = identity
        .wire
        .iter()
        .find(|wire| wire.name == "LocationForm")
        .expect("LocationForm wire parent exists");
    assert_eq!(parent.wire_type_id, 0x051e);
    assert_eq!(parent.kind, "union");
    assert_eq!(parent.status, "reserved");
    assert_eq!(parent.containing_union, None);
    assert_eq!(parent.wire_tag, None);
    assert_eq!(
        parent.allowed_containing_schemas,
        vec!["PlacementDescriptorWithoutId".to_owned()],
        "a top-level wire union must name its actual consumer"
    );

    let variants = identity
        .wire
        .iter()
        .filter(|wire| wire.containing_union.as_deref() == Some("LocationForm"))
        .map(|wire| (wire.name.as_str(), wire.wire_type_id, wire.wire_tag))
        .collect::<Vec<_>>();
    assert_eq!(
        variants,
        vec![
            ("LocationForm.contiguous_span", 0x051f, Some(0x0001)),
            ("LocationForm.explicit", 0x0520, Some(0x0002)),
        ],
        "the source spells ContiguousSpan before Explicit; alphabetical census order is not authoritative"
    );
    assert!(
        identity
            .wire
            .iter()
            .filter(|wire| wire.containing_union.as_deref() == Some("LocationForm"))
            .all(|wire| wire.allowed_containing_schemas == vec!["LocationForm".to_owned()])
    );

    let union = identity
        .ordinary_unions
        .iter()
        .find(|union| union.union_name == "LocationForm")
        .expect("LocationForm ordinary union exists");
    assert_eq!(union.containing_schema, union.union_name);
    assert_eq!(union.union_path, union.union_name);
    assert_eq!(union.field_tag, None);
    assert_eq!(
        union.allowed_containing_schemas,
        vec!["PlacementDescriptorWithoutId".to_owned()]
    );
    assert_eq!(
        union
            .arms
            .iter()
            .map(|arm| {
                (
                    arm.source_arm_name.as_str(),
                    arm.arm_tag,
                    arm.stable_name.as_str(),
                    arm.payload_sha256.as_deref(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            (
                "ContiguousSpan",
                0x0001,
                "contiguous_span",
                Some("b0d4e74e2c1c2056425f412f5f267bbeb348846b8c0910fbdef8fc3c9755fa82"),
            ),
            (
                "Explicit",
                0x0002,
                "explicit",
                Some("bfc317ac3870a16c28748c12d745185fe2ec3c1d2e34043528fabbcccdda3e16"),
            ),
        ]
    );

    let catalog = real_appendix_catalog();
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == "top|LocationForm")
        .expect("LocationForm source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "wire");
    assert_eq!(
        catalog
            .targets
            .iter()
            .filter(|target| {
                target.source_key == "top|LocationForm"
                    || target.source_key == "union|LocationForm|LocationForm"
                    || target.source_key.starts_with("arm|LocationForm|")
            })
            .count(),
        6,
        "one parent, one ordinary union, two arms, and two wire variants must be targeted"
    );
    assert!(
        !catalog
            .reservations
            .iter()
            .any(|reservation| reservation.symbol == "LocationForm"),
        "a non-StrongRef wire family must not acquire a reservation"
    );
}

#[test]
fn idr_a02_physical_record_fields_are_source_ordered_and_digest_typed() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();

    let field_signature = |schema: &str| {
        identity
            .fields
            .iter()
            .filter(|field| field.containing_schema == schema)
            .map(|field| {
                (
                    field.field_tag,
                    field.stable_name.as_str(),
                    field.exact_wire_type.as_str(),
                    field.identity_class.as_str(),
                    field.construction_order,
                    field.digest_class.as_deref(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        field_signature("CiphertextRecord"),
        vec![
            (
                0x0001,
                "descriptor",
                "CipherDescriptorWithoutDigest",
                "inline",
                10,
                None,
            ),
            (0x0002, "ciphertext_id", "id256", "physical", 10, None,),
            (
                0x0003,
                "ciphertext_digest",
                "digest256",
                "inline",
                10,
                Some("target"),
            ),
            (
                0x0004,
                "object_tag_digest",
                "digest256",
                "inline",
                10,
                Some("target"),
            ),
            (0x0005, "protected_length", "u64", "scalar", 10, None,),
        ],
        "CiphertextRecord tags follow descriptor, ciphertext_id, ciphertext_digest, object_tag_digest, protected_length source order"
    );
    assert_eq!(
        field_signature("SymbolRecord"),
        vec![
            (0x0001, "magic", "bytes", "scalar", 20, None),
            (0x0002, "format_version", "u16", "scalar", 20, None),
            (0x0003, "header_len", "u16", "scalar", 20, None),
            (0x0004, "record_len", "u32", "scalar", 20, None),
            (0x0005, "logical_oid", "oid256", "logical", 20, None),
            (0x0006, "ciphertext_id", "id256", "physical", 20, None),
            (0x0007, "encoding_id", "id256", "physical", 20, None),
            (0x0008, "object_kind", "u16", "scalar", 20, None),
            (0x0009, "source_block", "u32", "scalar", 20, None),
            (0x000a, "esi", "u32", "scalar", 20, None),
            (0x000b, "symbol_len", "u32", "scalar", 20, None),
            (0x000c, "transfer_length", "u64", "scalar", 20, None),
            (0x000d, "oti_common", "u64", "scalar", 20, None),
            (0x000e, "oti_scheme", "u32", "scalar", 20, None),
            (0x000f, "flags", "u32", "scalar", 20, None),
            (0x0010, "symbol_mac_profile", "u16", "scalar", 20, None,),
            (0x0011, "symbol_mac_len", "u16", "scalar", 20, None),
            (0x0012, "payload", "bytes", "inline", 20, None),
            (0x0013, "symbol_mac", "bytes", "inline", 20, None),
        ],
        "the fenced SymbolRecord source order, not the alphabetical census, assigns tags"
    );
    assert_eq!(
        field_signature("PlacementRecord"),
        vec![
            (0x0001, "placement_id", "id256", "physical", 30, None,),
            (
                0x0002,
                "descriptor",
                "PlacementDescriptorWithoutId",
                "inline",
                30,
                None,
            ),
        ],
        "PlacementRecord tags follow placement_id then descriptor source order"
    );

    let a02_fields = identity
        .fields
        .iter()
        .filter(|field| {
            matches!(
                field.containing_schema.as_str(),
                "CiphertextRecord" | "PlacementRecord" | "SymbolRecord"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(a02_fields.len(), 26);
    assert!(a02_fields.iter().all(|field| {
        field.cardinality == "one"
            && field.reference_semantics == "none"
            && field.target_schema_id.is_none()
            && field.version_status == "active"
    }));
    assert!(
        !identity.fields.iter().any(|field| {
            matches!(
                field.containing_schema.as_str(),
                "CipherDescriptorWithoutDigest"
                    | "EncodingDescriptorWithoutId"
                    | "FilesystemDurabilityProfile"
                    | "FilesystemInstanceRecord"
                    | "LocationForm"
                    | "PlacementDescriptorWithoutId"
            )
        }),
        "wire envelopes commit their interiors and can never own field rows"
    );
    assert!(
        !identity.wire.iter().any(|wire| matches!(
            wire.name.as_str(),
            "ciphertext_digest" | "object_tag_digest"
        )),
        "plan-named digests stay digest256 fields rather than becoming wire types"
    );

    let a02_field_targets = catalog
        .targets
        .iter()
        .filter(|target| {
            target.slice_id == "a02"
                && target.target_kind == "field"
                && (target.source_key.starts_with("field|CiphertextRecord|")
                    || target.source_key.starts_with("field|PlacementRecord|")
                    || target.source_key.starts_with("field|SymbolRecord|"))
        })
        .collect::<Vec<_>>();
    assert_eq!(a02_field_targets.len(), 26);
    assert!(
        a02_field_targets
            .iter()
            .all(|target| target.definition_status == "declared")
    );
    let physical_host_adjudications = catalog
        .ambiguity_adjudications
        .iter()
        .filter(|row| {
            row.slice_id == "a02"
                && row.resolved_source_keys.iter().any(|key| {
                    key.starts_with("field|CiphertextRecord|")
                        || key.starts_with("field|PlacementRecord|")
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(physical_host_adjudications.len(), 7);
    assert!(
        physical_host_adjudications
            .iter()
            .all(|row| row.resolution == "maps-to-source"),
        "the seven formerly misclassified physical-host ambiguities map to their owner-authored field rows"
    );

    let mut wire_host_mutation = identity.clone();
    wire_host_mutation
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "CiphertextRecord" && field.stable_name == "descriptor"
        })
        .expect("CiphertextRecord descriptor field exists")
        .containing_schema = "CipherDescriptorWithoutDigest".to_owned();
    let violations = identity::validate_identity(&wire_host_mutation);
    assert!(
        violations.iter().any(|violation| {
            violation.code == "field_unresolved_schema"
                && violation.msg.contains("resolves as a WIRE type")
        }),
        "negative control: moving a candidate field onto its wire-only descriptor host must fire"
    );
}

#[test]
fn idr_a18_reserved_reference_targets_and_strong_fields_are_exact() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();

    for (name, code, order, role, source_key) in [
        (
            "CertificateAttemptRecord",
            0x0266,
            16,
            "true",
            "projection|logical_object_kinds|CertificateAttemptRecord",
        ),
        (
            "PortableSemanticVisibilityCertificate",
            0x038f,
            12,
            "role-local || role-meta",
            "top|PortableSemanticVisibilityCertificate<Meta>",
        ),
    ] {
        let logical = identity
            .logical
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("missing A18 reference target {name}"));
        assert_eq!(logical.object_kind, code);
        assert_eq!(logical.status, "reserved");
        assert_eq!(logical.construction_order, order);
        assert_eq!(logical.role_predicate, role);
        assert_eq!(logical.max_size_bytes, 16_777_216);

        let reservation = catalog
            .reservations
            .iter()
            .find(|row| row.symbol == name)
            .unwrap_or_else(|| panic!("missing reservation for {name}"));
        assert_eq!(reservation.identity_class, "logical");
        assert_eq!(reservation.code_reservation, format!("0x{code:04x}"));
        assert_eq!(reservation.disposition, "existing");

        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{name} must have one exact target");
        assert_eq!(targets[0].target_kind, "logical-kind");
        assert_eq!(targets[0].definition_status, "declared");
    }

    let portable_candidates = catalog
        .top_level_candidates
        .iter()
        .filter(|row| row.symbol == "PortableSemanticVisibilityCertificate")
        .collect::<Vec<_>>();
    assert_eq!(portable_candidates.len(), 2);
    assert!(
        portable_candidates
            .iter()
            .all(|row| row.identity_class == "logical")
    );

    for (owner, name, tag, wire, target, max_size, source_key) in [
        (
            "GlobalRestoreAbandonParticipantApplyCertificate",
            "post_authorization_global_state_root_ref",
            0x0010,
            "StrongRef",
            "GlobalStateRoot",
            40,
            "field|GlobalRestoreAbandonParticipantApplyCertificate|GlobalRestoreAbandonParticipantApplyCertificate.post_authorization_global_state_root_ref|post_authorization_global_state_root_ref",
        ),
        (
            "GlobalRestoreAbandonParticipantApplyCertificate",
            "visibility_certificate_ref",
            0x0012,
            "StrongRef",
            "PortableSemanticVisibilityCertificate",
            40,
            "field|GlobalRestoreAbandonParticipantApplyCertificate|GlobalRestoreAbandonParticipantApplyCertificate.visibility_certificate_ref|visibility_certificate_ref",
        ),
        (
            "GlobalRestoreAbandonParticipantApplyCertificate",
            "certificate_attempt_ref",
            0x0013,
            "StrongRef",
            "CertificateAttemptRecord",
            40,
            "field|GlobalRestoreAbandonParticipantApplyCertificate|GlobalRestoreAbandonParticipantApplyCertificate.certificate_attempt_ref|certificate_attempt_ref",
        ),
        (
            "GlobalRestoreParticipantPinReleaseCompletionCertificate",
            "visibility_certificate_ref",
            0x000b,
            "StrongRef",
            "PortableSemanticVisibilityCertificate",
            40,
            "field|GlobalRestoreParticipantPinReleaseCompletionCertificate|GlobalRestoreParticipantPinReleaseCompletionCertificate.visibility_certificate_ref|visibility_certificate_ref",
        ),
        (
            "GlobalRestoreParticipantPinReleaseCompletionCertificate",
            "certificate_attempt_ref",
            0x000c,
            "StrongRef",
            "CertificateAttemptRecord",
            40,
            "field|GlobalRestoreParticipantPinReleaseCompletionCertificate|GlobalRestoreParticipantPinReleaseCompletionCertificate.certificate_attempt_ref|certificate_attempt_ref",
        ),
        (
            "RestoreShardAbandonAck",
            "certificate_attempt_ref",
            0x0010,
            "StrongRef",
            "CertificateAttemptRecord",
            40,
            "field|RestoreShardAbandonAck|RestoreShardAbandonAck.certificate_attempt_ref|certificate_attempt_ref",
        ),
        (
            "RestoreSourceLeaseAuthorityObservationImport<Role:AuthorityOwningRole>",
            "current_lease_record_ref",
            0x0004,
            "StrongRef",
            "RestoreSourceLeaseRecord",
            40,
            "field|RestoreSourceLeaseAuthorityObservationImport<Role:AuthorityOwningRole>|RestoreSourceLeaseAuthorityObservationImport<Role:AuthorityOwningRole>.current_lease_record_ref|current_lease_record_ref",
        ),
        (
            "RestoreTerminalPinReleaseAuthorization",
            "post_release_meta_state_root_ref",
            0x0002,
            "StrongRef",
            "GlobalStateRoot",
            40,
            "field|RestoreTerminalPinReleaseAuthorization|RestoreTerminalPinReleaseAuthorization.post_release_meta_state_root_ref|post_release_meta_state_root_ref",
        ),
        (
            "RestoreTerminalPinReleaseAuthorization",
            "visibility_certificate_ref",
            0x0004,
            "StrongRef",
            "PortableSemanticVisibilityCertificate",
            40,
            "field|RestoreTerminalPinReleaseAuthorization|RestoreTerminalPinReleaseAuthorization.visibility_certificate_ref|visibility_certificate_ref",
        ),
        (
            "RestoreTerminalPinReleaseAuthorization",
            "certificate_attempt_ref",
            0x0005,
            "StrongRef",
            "CertificateAttemptRecord",
            40,
            "field|RestoreTerminalPinReleaseAuthorization|RestoreTerminalPinReleaseAuthorization.certificate_attempt_ref|certificate_attempt_ref",
        ),
        (
            "ShardRestoreAbandonmentTombstone",
            "participant_apply_authorization_ref",
            0x000b,
            "CertifiedRemoteStrongRef",
            "GlobalRestoreAbandonParticipantApplyCertificate",
            537,
            "field|ShardRestoreAbandonmentTombstone|ShardRestoreAbandonmentTombstone.participant_apply_authorization_ref|participant_apply_authorization_ref",
        ),
    ] {
        let fields = identity
            .fields
            .iter()
            .filter(|row| row.containing_schema == owner && row.stable_name == name)
            .collect::<Vec<_>>();
        assert_eq!(fields.len(), 1, "{owner}.{name} must exist exactly once");
        let field = fields[0];
        assert_eq!(field.field_tag, tag);
        assert_eq!(field.exact_wire_type, wire);
        assert_eq!(field.cardinality, "one");
        assert_eq!(field.identity_class, "logical");
        assert_eq!(field.reference_semantics, "strong");
        assert_eq!(field.target_schema_id.as_deref(), Some(target));
        let owner_order = identity
            .logical
            .iter()
            .find(|row| row.name == identity::generic_free_family(owner))
            .expect("field owner resolves")
            .construction_order;
        assert_eq!(field.construction_order, owner_order);
        assert_eq!(field.version_status, "reserved");
        assert_eq!(field.max_size_bytes, max_size);

        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{owner}.{name} must have one target");
        assert_eq!(targets[0].target_kind, "field");
        assert_eq!(targets[0].definition_status, "declared");
    }
}

#[test]
fn idr_a20_reservation_backed_promotion_certificates_and_ready_edges_are_exact() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();

    for (name, code, order, role, source_key) in [
        (
            "GlobalRestoreServiceFinalCertificate",
            0x02c6,
            44,
            "role-meta",
            "top|GlobalRestoreServiceFinalCertificate",
        ),
        (
            "LocalRestoreReadyCertificate",
            0x031e,
            40,
            "role-local",
            "top|LocalRestoreReadyCertificate",
        ),
        (
            "RestoreShardOperationalAck",
            0x03ea,
            50,
            "role-shard",
            "top|RestoreShardOperationalAck",
        ),
    ] {
        let logical = identity
            .logical
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("missing A20 reservation-backed kind {name}"));
        assert_eq!(logical.object_kind, code);
        assert_eq!(logical.status, "reserved");
        assert_eq!(logical.construction_order, order);
        assert_eq!(logical.role_predicate, role);
        assert_eq!(logical.max_size_bytes, 16_777_216);

        let reservation = catalog
            .reservations
            .iter()
            .find(|row| row.symbol == name)
            .unwrap_or_else(|| panic!("missing reservation for {name}"));
        assert_eq!(reservation.identity_class, "logical");
        assert_eq!(reservation.code_reservation, format!("0x{code:04x}"));
        assert_eq!(reservation.disposition, "existing");

        let candidate = catalog
            .top_level_candidates
            .iter()
            .find(|row| row.source_key == source_key)
            .unwrap_or_else(|| panic!("missing source candidate for {name}"));
        assert_eq!(candidate.identity_class, "logical");

        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{name} must have one exact target");
        assert_eq!(targets[0].target_kind, "logical-kind");
        assert_eq!(targets[0].definition_status, "declared");
    }

    for (owner, name, tag, target, source_key) in [
        (
            "GlobalRestoreServiceFinalCertificate",
            "finalize_record_ref",
            0x0002,
            "GlobalControlRecord",
            "field|GlobalRestoreServiceFinalCertificate|GlobalRestoreServiceFinalCertificate.finalize_record_ref|finalize_record_ref",
        ),
        (
            "GlobalRestoreServiceFinalCertificate",
            "post_apply_global_state_root_ref",
            0x0005,
            "GlobalStateRoot",
            "field|GlobalRestoreServiceFinalCertificate|GlobalRestoreServiceFinalCertificate.post_apply_global_state_root_ref|post_apply_global_state_root_ref",
        ),
        (
            "GlobalRestoreServiceFinalCertificate",
            "visibility_certificate_ref",
            0x0007,
            "PortableSemanticVisibilityCertificate",
            "field|GlobalRestoreServiceFinalCertificate|GlobalRestoreServiceFinalCertificate.visibility_certificate_ref|visibility_certificate_ref",
        ),
        (
            "GlobalRestoreServiceFinalCertificate",
            "certificate_attempt_ref",
            0x0008,
            "CertificateAttemptRecord",
            "field|GlobalRestoreServiceFinalCertificate|GlobalRestoreServiceFinalCertificate.certificate_attempt_ref|certificate_attempt_ref",
        ),
        (
            "LocalRestoreReadyCertificate",
            "current_hidden_state_root_ref",
            0x0004,
            "LogicalStateRoot",
            "field|LocalRestoreReadyCertificate|LocalRestoreReadyCertificate.current_hidden_state_root_ref|current_hidden_state_root_ref",
        ),
        (
            "LocalRestoreReadyCertificate",
            "reconciliation_completion_proof_ref",
            0x0005,
            "RestoreReconciliationCompletionProof",
            "field|LocalRestoreReadyCertificate|LocalRestoreReadyCertificate.reconciliation_completion_proof_ref|reconciliation_completion_proof_ref",
        ),
        (
            "LocalRestoreReadyCertificate",
            "ready_closure_inventory_ref",
            0x0006,
            "RestoreReadyClosureInventory",
            "field|LocalRestoreReadyCertificate|LocalRestoreReadyCertificate.ready_closure_inventory_ref|ready_closure_inventory_ref",
        ),
        (
            "LocalRestoreReadyCertificate",
            "current_configuration_ref",
            0x0007,
            "ConfigurationState",
            "field|LocalRestoreReadyCertificate|LocalRestoreReadyCertificate.current_configuration_ref|current_configuration_ref",
        ),
        (
            "LocalRestoreReadyCertificate",
            "current_config_floor_ref",
            0x0009,
            "ConfigPayloadFloor",
            "field|LocalRestoreReadyCertificate|LocalRestoreReadyCertificate.current_config_floor_ref|current_config_floor_ref",
        ),
        (
            "RestoreShardOperationalAck",
            "post_close_state_root_ref",
            0x0006,
            "ShardLogicalStateRoot",
            "field|RestoreShardOperationalAck|RestoreShardOperationalAck.post_close_state_root_ref|post_close_state_root_ref",
        ),
        (
            "RestoreShardOperationalAck",
            "source_access_closure_ref",
            0x000b,
            "ShardRestoreSourceAccessClosure",
            "field|RestoreShardOperationalAck|RestoreShardOperationalAck.source_access_closure_ref|source_access_closure_ref",
        ),
        (
            "RestoreShardOperationalAck",
            "certificate_attempt_ref",
            0x0010,
            "CertificateAttemptRecord",
            "field|RestoreShardOperationalAck|RestoreShardOperationalAck.certificate_attempt_ref|certificate_attempt_ref",
        ),
    ] {
        let fields = identity
            .fields
            .iter()
            .filter(|row| row.containing_schema == owner && row.stable_name == name)
            .collect::<Vec<_>>();
        assert_eq!(fields.len(), 1, "{owner}.{name} must exist exactly once");
        let field = fields[0];
        assert_eq!(field.field_tag, tag);
        assert_eq!(field.exact_wire_type, "StrongRef");
        assert_eq!(field.cardinality, "one");
        assert_eq!(field.identity_class, "logical");
        assert_eq!(field.reference_semantics, "strong");
        assert_eq!(field.target_schema_id.as_deref(), Some(target));
        let owner_order = identity
            .logical
            .iter()
            .find(|row| row.name == owner)
            .expect("field owner resolves")
            .construction_order;
        assert_eq!(field.construction_order, owner_order);
        assert_eq!(field.version_status, "reserved");
        assert_eq!(field.max_size_bytes, 40);

        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{owner}.{name} must have one target");
        assert_eq!(targets[0].target_kind, "field");
        assert_eq!(targets[0].definition_status, "declared");
    }
}

#[test]
fn idr_a20_structural_body_promotion_commands_and_activation_union_are_exact() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();

    for (name, code, order, role) in [
        (
            "GlobalRestoreServiceCompletionSpec",
            0x055f,
            50,
            "role-meta",
        ),
        ("GlobalRestoreServiceFinalizeSpec", 0x0560, 24, "role-meta"),
        ("LocalRestoreActivationSpec", 0x0561, 40, "role-local"),
        (
            "LocalRestoreServiceCompletionSpec",
            0x0562,
            35,
            "role-local",
        ),
        ("LocalRestoreServicePromotionSpec", 0x0563, 35, "role-local"),
        ("ShardRestoreReopenConfirmSpec", 0x0564, 44, "role-shard"),
        ("ShardRestoreServiceOpenSpec", 0x0565, 44, "role-shard"),
    ] {
        let logical = identity
            .logical
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("missing A20 structural-body kind {name}"));
        assert_eq!(logical.object_kind, code);
        assert!(
            logical.object_kind > i64::from(appendix_a::EXPECTED_RESERVATION_HIGH_WATER),
            "{name} has no reservation and must use fresh post-reservation code space"
        );
        assert_eq!(logical.status, "reserved");
        assert_eq!(logical.construction_order, order);
        assert_eq!(logical.role_predicate, role);
        assert_eq!(logical.max_size_bytes, 16_777_216);
        assert!(
            catalog
                .reservations
                .iter()
                .all(|reservation| reservation.symbol != name),
            "non-StrongRef family {name} must not acquire a reservation"
        );

        let source_key = format!("top|{name}");
        let candidate = catalog
            .top_level_candidates
            .iter()
            .find(|row| row.source_key == source_key)
            .unwrap_or_else(|| panic!("missing source candidate for {name}"));
        assert_eq!(candidate.identity_class, "logical");
        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{name} must have one exact target");
        assert_eq!(targets[0].target_kind, "logical-kind");
        assert_eq!(targets[0].definition_status, "declared");
    }

    for (owner, name, tag, wire, cardinality, class, semantics, target, max_size) in [
        (
            "GlobalRestoreServiceCompletionSpec",
            "final_certificate_ref",
            0x0002,
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("GlobalRestoreServiceFinalCertificate"),
            40,
        ),
        (
            "GlobalRestoreServiceCompletionSpec",
            "exact_sorted_operational_ack_refs",
            0x0005,
            "CertifiedRemoteStrongRef",
            "many",
            "logical",
            "strong",
            Some("RestoreShardOperationalAck"),
            40,
        ),
        (
            "GlobalRestoreServiceFinalizeSpec",
            "prepare_certificate_ref",
            0x0002,
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("GlobalRestoreServicePrepareCertificate"),
            40,
        ),
        (
            "GlobalRestoreServiceFinalizeSpec",
            "exact_sorted_ready_ack_refs",
            0x0005,
            "CertifiedRemoteStrongRef",
            "many",
            "logical",
            "strong",
            Some("RestoreShardServiceReadyAck"),
            40,
        ),
        (
            "GlobalRestoreServiceFinalizeSpec",
            "expected_meta_restore_state",
            0x0006,
            "WeakStateIdentity",
            "one",
            "inline",
            "none",
            None,
            16_777_216,
        ),
        (
            "LocalRestoreActivationSpec",
            "ready_certificate_ref",
            0x0002,
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("LocalRestoreReadyCertificate"),
            40,
        ),
        (
            "LocalRestoreActivationSpec",
            "expected_local_restore_state",
            0x0003,
            "WeakStateIdentity",
            "one",
            "inline",
            "none",
            None,
            16_777_216,
        ),
        (
            "LocalRestoreActivationSpec",
            "promotion_authority_basis",
            0x0005,
            "LocalRestoreActivationSpecPromotionAuthorityBasis",
            "one",
            "inline",
            "none",
            None,
            16_777_216,
        ),
        (
            "LocalRestoreServiceCompletionSpec",
            "promotion_record_ref",
            0x0002,
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("LogicalCommandRecord"),
            40,
        ),
        (
            "LocalRestoreServiceCompletionSpec",
            "promotion_receipt_ref",
            0x0003,
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("RestoreServicePromotionReceipt"),
            40,
        ),
        (
            "LocalRestoreServicePromotionSpec",
            "manifest_ref",
            0x0003,
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("RestoreServicePromotionManifest"),
            40,
        ),
        (
            "LocalRestoreServicePromotionSpec",
            "promotion_receipt_ref",
            0x0005,
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("RestoreServicePromotionReceipt"),
            40,
        ),
        (
            "LocalRestoreServicePromotionSpec",
            "expected_local_restore_state",
            0x0007,
            "WeakStateIdentity",
            "one",
            "inline",
            "none",
            None,
            16_777_216,
        ),
        (
            "ShardRestoreReopenConfirmSpec",
            "final_certificate_ref",
            0x0002,
            "CertifiedRemoteStrongRef",
            "one",
            "logical",
            "strong",
            Some("GlobalRestoreServiceFinalCertificate"),
            40,
        ),
        (
            "ShardRestoreServiceOpenSpec",
            "final_certificate_ref",
            0x0001,
            "CertifiedRemoteStrongRef",
            "one",
            "logical",
            "strong",
            Some("GlobalRestoreServiceFinalCertificate"),
            40,
        ),
        (
            "ShardRestoreServiceOpenSpec",
            "own_ready_ack_ref",
            0x0002,
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("RestoreShardServiceReadyAck"),
            40,
        ),
        (
            "ShardRestoreServiceOpenSpec",
            "expected_local_restore_state",
            0x0004,
            "WeakStateIdentity",
            "one",
            "inline",
            "none",
            None,
            16_777_216,
        ),
    ] {
        let fields = identity
            .fields
            .iter()
            .filter(|row| row.containing_schema == owner && row.stable_name == name)
            .collect::<Vec<_>>();
        assert_eq!(fields.len(), 1, "{owner}.{name} must exist exactly once");
        let field = fields[0];
        assert_eq!(field.field_tag, tag);
        assert_eq!(field.exact_wire_type, wire);
        assert_eq!(field.cardinality, cardinality);
        assert_eq!(field.identity_class, class);
        assert_eq!(field.reference_semantics, semantics);
        assert_eq!(field.target_schema_id.as_deref(), target);
        let owner_order = identity
            .logical
            .iter()
            .find(|row| row.name == owner)
            .expect("field owner resolves")
            .construction_order;
        assert_eq!(field.construction_order, owner_order);
        assert_eq!(field.version_status, "reserved");
        assert_eq!(field.max_size_bytes, max_size);

        let source_key = format!("field|{owner}|{owner}.{name}|{name}");
        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{owner}.{name} must have one target");
        assert_eq!(targets[0].target_kind, "field");
        assert_eq!(targets[0].definition_status, "declared");
    }

    let union = identity
        .ordinary_unions
        .iter()
        .find(|row| row.union_name == "LocalRestoreActivationSpecPromotionAuthorityBasis")
        .expect("A20 Local activation promotion authority union exists");
    assert_eq!(union.containing_schema, "LocalRestoreActivationSpec");
    assert_eq!(
        union.union_path,
        "LocalRestoreActivationSpec.promotion_authority_basis"
    );
    assert_eq!(union.field_tag, Some(0x0005));
    assert_eq!(
        union.allowed_containing_schemas,
        vec!["LocalRestoreActivationSpec".to_owned()]
    );
    assert_eq!(union.role_predicate, "role-local");
    assert_eq!(union.max_size_bytes, 16_777_216);
    assert_eq!(
        union
            .arms
            .iter()
            .map(|arm| {
                (
                    arm.arm_tag,
                    arm.source_arm_name.as_str(),
                    arm.payload_sha256.as_deref(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            (
                0x0001,
                "ExternalCasCataloged",
                Some("f67a8f6db7128c3f0a858239a7466ae44afbd01381e040b67e87cb956341ecc4"),
            ),
            (
                0x0002,
                "DirectoryBound",
                Some("c4bb9639cea89e2d19ea6437e6ff78e5228736a4c01371d863309de0e8feb02b"),
            ),
        ],
        "A20 activation union tags follow source order, not lexical order"
    );
    for source_key in [
        "union|LocalRestoreActivationSpec|LocalRestoreActivationSpec.promotion_authority_basis",
        "arm|LocalRestoreActivationSpec|LocalRestoreActivationSpec.promotion_authority_basis|ExternalCasCataloged",
        "arm|LocalRestoreActivationSpec|LocalRestoreActivationSpec.promotion_authority_basis|DirectoryBound",
    ] {
        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{source_key} must have one exact target");
        assert_ne!(targets[0].target_kind, "field");
        assert_eq!(targets[0].definition_status, "declared");
    }
}

#[test]
fn idr_key_destroy_proposal_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "KeyDestroyProposal")
        .expect("KeyDestroyProposal logical shell exists");
    assert_eq!(logical.object_kind, 0x02da);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 80);
    assert_eq!(logical.role_predicate, "role-local");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(
        logical.golden_corpus,
        "corpus/logical/key_destroy_proposal/"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "KeyDestroyProposal")
        .expect("KeyDestroyProposal permanent reservation exists");
    assert_eq!(reservation.row_id, "a15:reservation:key-destroy-proposal");
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x02da");
    assert_eq!(reservation.disposition, "existing");

    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == "top|KeyDestroyProposal")
        .expect("KeyDestroyProposal source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == "top|KeyDestroyProposal")
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a15:target:logical-kind-key-destroy-proposal"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a15:logical-kind:key-destroy-proposal"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");

    let mut proposal_fields = identity
        .fields
        .iter()
        .filter(|field| field.containing_schema == "KeyDestroyProposal")
        .collect::<Vec<_>>();
    proposal_fields.sort_by_key(|field| field.field_tag);
    let expected_fields = [
        (
            0x0001,
            "key_identity",
            "id256",
            "one",
            "inline",
            "none",
            None,
            32,
            "fgdb:key-identity:v1",
        ),
        (
            0x0002,
            "expected_key_state",
            "u8",
            "one",
            "inline",
            "none",
            None,
            1,
            "exactly 0x04=RetainedDecryptOnly",
        ),
        (
            0x0003,
            "basis_state",
            "WeakStateIdentity",
            "one",
            "inline",
            "none",
            None,
            16_777_216,
            "comparison-only WeakStateIdentity",
        ),
        (
            0x0004,
            "expected_current_configuration_ref",
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("ConfigurationState"),
            40,
            "5 <= 80",
        ),
        (
            0x0007,
            "checkpoint_and_configuration_floor_refs",
            "KeyDestroyFloorRef",
            "many",
            "inline",
            "none",
            None,
            16_777_216,
            "generated union walkers",
        ),
        (
            0x0008,
            "generated_scanned_root_inventory_ref",
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("KeyReferenceInventory"),
            40,
            "10 <= 80",
        ),
        (
            0x0009,
            "zero_reference_proof_ref",
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("ZeroReferenceProof"),
            40,
            "10 <= 80",
        ),
        (
            0x000a,
            "backup_legal_hold_and_external_consumer_ack_refs",
            "KeyDestroyExternalAckRef",
            "many",
            "inline",
            "none",
            None,
            16_777_216,
            "generated union walkers",
        ),
        (
            0x000b,
            "threshold_authorization_ref",
            "StrongRef",
            "one",
            "logical",
            "strong",
            Some("KeyDestructionAuthorization"),
            40,
            "10 <= 80",
        ),
        (
            0x000c,
            "sorted_destruction_operation_plans",
            "KeyDestructionOperationPlan",
            "many",
            "inline",
            "none",
            None,
            16_777_216,
            "no duplicate target or operation ID",
        ),
        (
            0x000e,
            "expected_state_conditions",
            "ExpectedStateCondition",
            "many",
            "inline",
            "none",
            None,
            16_777_216,
            "source-ordered comparison-only CAS conditions",
        ),
        (
            0x000f,
            "terminal_audit_gate",
            "TerminalAuditGate",
            "one",
            "inline",
            "none",
            None,
            16_777_216,
            "one source-required TerminalAuditGate",
        ),
    ];
    assert_eq!(
        proposal_fields.len(),
        expected_fields.len(),
        "only source-forced and fully resolved proposal fields may land"
    );
    for (
        field,
        (
            field_tag,
            stable_name,
            exact_wire_type,
            cardinality,
            identity_class,
            reference_semantics,
            target_schema_id,
            max_size_bytes,
            retention_fragment,
        ),
    ) in proposal_fields.into_iter().zip(expected_fields)
    {
        assert_eq!(field.field_tag, field_tag);
        assert_eq!(field.stable_name, stable_name);
        assert_eq!(field.exact_wire_type, exact_wire_type);
        assert_eq!(field.cardinality, cardinality);
        assert_eq!(field.identity_class, identity_class);
        assert_eq!(field.reference_semantics, reference_semantics);
        assert_eq!(field.target_schema_id.as_deref(), target_schema_id);
        assert_eq!(field.construction_order, 80);
        assert_eq!(field.role_predicate, "role-local");
        assert_eq!(field.version_status, "reserved");
        assert_eq!(field.max_size_bytes, max_size_bytes);
        assert!(
            field.retention_and_cut_rule.contains(retention_fragment),
            "{stable_name} lost its retention/order law"
        );
    }

    for (name, row_suffix, code, role, slice_id) in [
        (
            "KeyDestructionAuthorization",
            "key-destruction-authorization",
            0x02dc,
            "role-local",
            "a15",
        ),
        (
            "KeyReferenceInventory",
            "key-reference-inventory",
            0x02e8,
            "true",
            "a06",
        ),
        (
            "ZeroReferenceProof",
            "zero-reference-proof",
            0x04bd,
            "true",
            "a06",
        ),
    ] {
        let logical = identity
            .logical
            .iter()
            .find(|logical| logical.name == name)
            .expect("I8 logical shell exists");
        assert_eq!(logical.object_kind, code);
        assert_eq!(logical.status, "reserved");
        assert_eq!(logical.construction_order, 10);
        assert_eq!(logical.role_predicate, role);
        assert_eq!(logical.max_size_bytes, 16_777_216);

        let reservation = catalog
            .reservations
            .iter()
            .find(|reservation| reservation.symbol == name)
            .expect("I8 reservation exists");
        assert_eq!(reservation.slice_id, slice_id);
        assert_eq!(reservation.code_reservation, format!("{code:#06x}"));
        assert_eq!(reservation.disposition, "existing");

        let target_row_id = format!("{slice_id}:logical-kind:{row_suffix}");
        let target = catalog
            .targets
            .iter()
            .find(|target| target.target_row_id == target_row_id)
            .expect("I8 projection target exists");
        assert_eq!(
            target.source_key,
            format!("projection|logical_object_kinds|{name}")
        );
        assert_eq!(target.target_kind, "logical-kind");
        assert_eq!(target.definition_status, "declared");
    }

    let mut proposal_field_targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key.starts_with("field|KeyDestroyProposal|"))
        .collect::<Vec<_>>();
    proposal_field_targets.sort_by(|left, right| left.source_key.cmp(&right.source_key));
    let mut expected_target_rows = [
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.backup_legal_hold_and_external_consumer_ack_refs|backup_legal_hold_and_external_consumer_ack_refs",
            "a15:field:key-destroy-proposal-backup-legal-hold-and-external-consumer-ack-refs",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.basis_state|basis_state",
            "a15:field:key-destroy-proposal-basis-state",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.checkpoint_and_configuration_floor_refs|checkpoint_and_configuration_floor_refs",
            "a15:field:key-destroy-proposal-checkpoint-and-configuration-floor-refs",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.expected_current_configuration_ref|expected_current_configuration_ref",
            "a15:field:key-destroy-proposal-expected-current-configuration-ref",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.expected_key_state|expected_key_state",
            "a15:field:key-destroy-proposal-expected-key-state",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.expected_state_conditions|expected_state_conditions",
            "a15:field:key-destroy-proposal-expected-state-conditions",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.generated_scanned_root_inventory_ref|generated_scanned_root_inventory_ref",
            "a15:field:key-destroy-proposal-generated-scanned-root-inventory-ref",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.key_identity|key_identity",
            "a15:field:key-destroy-proposal-key-identity",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.sorted_destruction_operation_plans|sorted_destruction_operation_plans",
            "a15:field:key-destroy-proposal-sorted-destruction-operation-plans",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.threshold_authorization_ref|threshold_authorization_ref",
            "a15:field:key-destroy-proposal-threshold-authorization-ref",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.terminal_audit_gate|terminal_audit_gate",
            "a15:field:key-destroy-proposal-terminal-audit-gate",
        ),
        (
            "field|KeyDestroyProposal|KeyDestroyProposal.zero_reference_proof_ref|zero_reference_proof_ref",
            "a15:field:key-destroy-proposal-zero-reference-proof-ref",
        ),
    ];
    expected_target_rows.sort_by_key(|(source_key, _)| *source_key);
    assert_eq!(
        proposal_field_targets.len(),
        expected_target_rows.len(),
        "each I7/I8 field source key must map exactly once"
    );
    for (target, (source_key, target_row_id)) in
        proposal_field_targets.into_iter().zip(expected_target_rows)
    {
        assert_eq!(target.source_key, source_key);
        assert_eq!(target.target_row_id, target_row_id);
        assert_eq!(target.target_kind, "field");
        assert_eq!(target.definition_status, "declared");
    }
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "KeyDestroyProposal"
                || union.union_name == "KeyDestroyProposal"
        }) && !identity
            .unions
            .iter()
            .any(|union| union.containing_schema == "KeyDestroyProposal"),
        "the increment must use shared producers, never an A15-local proposal union"
    );

    let weak_state = identity
        .wire
        .iter()
        .find(|wire| wire.name == "WeakStateIdentity")
        .expect("WeakStateIdentity has its own producer row");
    let weak_digest = identity
        .wire
        .iter()
        .find(|wire| wire.name == "WeakDigest")
        .expect("WeakDigest remains independently registered");
    assert_eq!(weak_state.wire_type_id, 0x0193);
    assert_eq!(weak_state.kind, "record");
    assert_eq!(weak_state.status, "reserved");
    assert_eq!(weak_state.containing_union, None);
    assert_eq!(weak_state.wire_tag, None);
    assert_eq!(weak_digest.wire_type_id, 0x0003);
    assert_ne!(
        weak_state.wire_type_id, weak_digest.wire_type_id,
        "source-distinct WeakStateIdentity and WeakDigest must not be aliased"
    );
    assert_eq!(
        catalog
            .targets
            .iter()
            .filter(|target| target.source_key == "top|WeakStateIdentity")
            .count(),
        1
    );

    let assert_shared_union = |union_name: &str,
                               allowed: &[&str],
                               expected_arms: &[(&str, i64, &str, &str)],
                               expected_target_count: usize| {
        let union = identity
            .ordinary_unions
            .iter()
            .find(|union| union.union_name == union_name)
            .expect("shared ordinary union exists");
        assert_eq!(union.containing_schema, union_name);
        assert_eq!(union.union_path, union_name);
        assert_eq!(union.field_tag, None);
        assert_eq!(
            union.allowed_containing_schemas,
            allowed
                .iter()
                .map(|schema| (*schema).to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            union
                .arms
                .iter()
                .map(|arm| {
                    (
                        arm.source_arm_name.as_str(),
                        arm.arm_tag,
                        arm.stable_name.as_str(),
                        arm.payload_sha256
                            .as_deref()
                            .expect("every shared arm has a payload digest"),
                    )
                })
                .collect::<Vec<_>>(),
            expected_arms
        );
        assert_eq!(
            catalog
                .targets
                .iter()
                .filter(|target| {
                    target.source_key == format!("top|{union_name}")
                        || target
                            .source_key
                            .starts_with(&format!("union|{union_name}|"))
                        || target.source_key.starts_with(&format!("arm|{union_name}|"))
                })
                .count(),
            expected_target_count,
            "the shared union must carry its complete 2N+2 source/target shell"
        );
        let candidate = catalog
            .top_level_candidates
            .iter()
            .find(|candidate| candidate.source_key == format!("top|{union_name}"))
            .expect("shared union source candidate exists");
        assert_eq!(candidate.identity_class, "wire");
    };
    assert_shared_union(
        "ExpectedStateCondition",
        &["ControlCommand", "KeyDestroyProposal"],
        &[
            (
                "WeakStateIdentity",
                0x0001,
                "weak_state_identity",
                "fab58bb486883dcb63e169533e152986932398afb581c026e1990c8ca846ab4a",
            ),
            (
                "WeakMarkerIdentity",
                0x0002,
                "weak_marker_identity",
                "f169ae0163b8faaa673cc0f2ee8c60c68dcbd4ecd40eaccb8610dca3f80097b8",
            ),
            (
                "ExpectedEpoch",
                0x0003,
                "expected_epoch",
                "bb0335783a1df7a0472b98ddb3633885de35d9133af1a10da90609303c2f3c60",
            ),
            (
                "ExpectedIndex",
                0x0004,
                "expected_index",
                "74dc0cbf0ad1e646cf563514c96c8961ac8b3ff4bb0ffdc0244e29698aacba70",
            ),
        ],
        10,
    );
    assert_shared_union(
        "TerminalAuditGate",
        &["KeyDestroyProposal", "SequenceNeutralSpec<Tag>"],
        &[
            (
                "StructurallyInapplicable",
                0x0001,
                "structurally_inapplicable",
                "1a750c560dc03b9c5328def7c075e44d5eb0464e20b3e9ad0f0b8405cf6a5fc7",
            ),
            (
                "NotRequired",
                0x0002,
                "not_required",
                "27acef18721dfa1ec3a2bf2c4742c60a7aae1d17ac79ef1599ad68f6f0987966",
            ),
            (
                "Required",
                0x0003,
                "required",
                "d6bf34c187c66a2091c7655f9874144be1a581ef32d08124b86f9866d7daadb9",
            ),
        ],
        8,
    );

    let mut proposal_adjudications = catalog
        .ambiguity_adjudications
        .iter()
        .filter(|row| row.ambiguity_source_key.contains("|KeyDestroyProposal|"))
        .flat_map(|row| row.resolved_source_keys.iter().map(String::as_str))
        .collect::<Vec<_>>();
    proposal_adjudications.sort_unstable();
    assert_eq!(
        proposal_adjudications,
        vec![
            "field|KeyDestroyProposal|KeyDestroyProposal.expected_state_conditions|expected_state_conditions",
            "field|KeyDestroyProposal|KeyDestroyProposal.key_identity|key_identity",
            "field|KeyDestroyProposal|KeyDestroyProposal.terminal_audit_gate|terminal_audit_gate",
        ],
        "only the three source-forced shorthand fields may be adjudicated"
    );
    for unresolved in [
        "expected_prospective_configuration_set_digest",
        "exact_root_slot_generations",
        "complete_target_set_digest",
    ] {
        assert!(
            !identity.fields.iter().any(|field| {
                field.containing_schema == "KeyDestroyProposal" && field.stable_name == unresolved
            }),
            "{unresolved} must remain absent pending its producer/transcript ruling"
        );
        assert!(
            !catalog.ambiguity_adjudications.iter().any(|row| {
                row.resolved_source_keys
                    .iter()
                    .any(|source_key| source_key.contains(&format!(".{unresolved}|")))
            }),
            "{unresolved} must remain unadjudicated"
        );
    }

    use registry_check::appendix_source::{SourceSliceSpec, census_appendix_source};

    let plan = real_plan_source();
    let appendix = source_range(
        &plan,
        catalog.source_manifest.start_line,
        catalog.source_manifest.end_line,
    );
    let specs = catalog
        .slices
        .iter()
        .map(|slice| SourceSliceSpec {
            id: &slice.id,
            start_line: usize::try_from(slice.start_line).expect("slice start fits"),
            end_line: usize::try_from(slice.end_line).expect("slice end fits"),
        })
        .collect::<Vec<_>>();
    let census = census_appendix_source(
        &appendix,
        usize::try_from(catalog.source_manifest.start_line).expect("source start fits"),
        &specs,
    )
    .expect("source census");
    let source_field = census
        .fields
        .iter()
        .find(|field| {
            field.key.source_key()
                == "field|KeyDestroyProposal|KeyDestroyProposal.expected_key_state|expected_key_state"
        })
        .expect("expected_key_state source candidate exists");
    assert_eq!(source_field.exact_types, vec!["u8"]);
    assert_eq!(
        source_field
            .cardinalities
            .iter()
            .map(|cardinality| cardinality.as_str())
            .collect::<Vec<_>>(),
        vec!["one"]
    );
    assert!(!source_field.type_conflict);
    assert!(!source_field.ambiguous);
    assert_eq!(source_field.locations.len(), 1);
    assert_eq!(source_field.locations[0].start.line, 2059);
    assert_eq!(source_field.locations[0].start.column, 59);
    assert_eq!(source_field.locations[0].end.line, 2059);
    assert_eq!(source_field.locations[0].end.column, 80);
    assert!(
        !census.ambiguities.iter().any(|ambiguity| ambiguity
            .affected_source_keys
            .iter()
            .any(|key| key == &source_field.key.source_key())),
        "the explicit u8 spelling must not require an ambiguity adjudication"
    );

    let plan = String::from_utf8(plan).expect("plan is UTF-8");
    let lifecycle_law = |source: &str| {
        source.contains(
            "The one-byte key-lifecycle discriminant table is closed and source-ordered: \
             0x01 means Generated, 0x02 means Active, 0x03 means Retiring, 0x04 means \
             RetainedDecryptOnly, 0x05 means DestroyPending, 0x06 means \
             DestroyedPendingCertificate, and 0x07 means Destroyed; 0x00 and 0x08 through \
             0xff are invalid.",
        ) && source.contains(
            "A proposal MUST carry exactly 0x04; readers reject every other value before \
             scanning or applying it.",
        ) && source.contains(
            "This field is only the compare-and-swap state tag and never carries the \
             quarantine_generation and target_set_digest payload of DestroyPending",
        )
    };
    assert!(
        lifecycle_law(&plan),
        "the plan must pin the full source-ordered lifecycle table, 0x04 proposal law, and payload boundary"
    );
    let wrong_state = plan.replacen(
        "0x04 means RetainedDecryptOnly",
        "0x03 means RetainedDecryptOnly",
        1,
    );
    assert!(
        !lifecycle_law(&wrong_state),
        "negative control: a changed lifecycle code must fire"
    );

    let expected_key_state = identity
        .fields
        .iter()
        .find(|field| {
            field.containing_schema == "KeyDestroyProposal"
                && field.stable_name == "expected_key_state"
        })
        .expect("KeyDestroyProposal.expected_key_state exists");
    assert!(
        expected_key_state
            .retention_and_cut_rule
            .contains("never carries the later DestroyPending payload"),
        "the field row must preserve the source payload boundary"
    );
    assert!(
        !identity.wire.iter().any(|row| matches!(
            row.name.as_str(),
            "KeyLifecycleState" | "RetainedDecryptOnly"
        )) && !identity.logical.iter().any(|row| {
            matches!(
                row.name.as_str(),
                "KeyLifecycleState" | "RetainedDecryptOnly"
            )
        }) && !catalog.reservations.iter().any(|row| {
            matches!(
                row.symbol.as_str(),
                "KeyLifecycleState" | "RetainedDecryptOnly"
            )
        }),
        "an inline lifecycle discriminant must not mint a producer or reservation"
    );

    let mut duplicate_tag = identity.clone();
    duplicate_tag
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "KeyDestroyProposal"
                && field.stable_name == "expected_key_state"
        })
        .expect("KeyDestroyProposal.expected_key_state exists")
        .field_tag = 0x0003;
    assert!(
        codes_without_assignment_drift(&duplicate_tag).contains(&"code_duplicate".to_owned()),
        "negative control: reusing the source-adjacent basis_state tag must fire"
    );

    let mut invented_lifecycle_producer = identity.clone();
    invented_lifecycle_producer
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "KeyDestroyProposal"
                && field.stable_name == "expected_key_state"
        })
        .expect("KeyDestroyProposal.expected_key_state exists")
        .exact_wire_type = "RetainedDecryptOnly".to_owned();
    assert!(
        codes_without_assignment_drift(&invented_lifecycle_producer)
            .contains(&"field_unresolved_wire_type".to_owned()),
        "negative control: an invented RetainedDecryptOnly producer must fire"
    );

    let key_identity = identity
        .fields
        .iter()
        .find(|field| {
            field.containing_schema == "KeyDestroyProposal" && field.stable_name == "key_identity"
        })
        .expect("KeyDestroyProposal.key_identity exists");
    assert!(
        key_identity.retention_and_cut_rule.contains(
            "canonical(0x0001 database_security_namespace_id:id256, 0x0002 material_class:u16, \
             0x0003 key_id:id256, 0x0004 key_epoch:u64)"
        ),
        "KeyIdentity must retain its exact canonical input transcript"
    );
    assert!(
        !identity.wire.iter().any(|row| row.name == "KeyIdentity")
            && !identity.logical.iter().any(|row| row.name == "KeyIdentity")
            && !catalog
                .reservations
                .iter()
                .any(|row| row.symbol == "KeyIdentity"),
        "the opaque builtin id256 ruling must not fabricate a KeyIdentity producer or reservation"
    );

    let mut invented_structured_producer = identity.clone();
    invented_structured_producer
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "KeyDestroyProposal" && field.stable_name == "key_identity"
        })
        .expect("KeyDestroyProposal.key_identity exists")
        .exact_wire_type = "KeyIdentity".to_owned();
    assert!(
        codes_without_assignment_drift(&invented_structured_producer)
            .contains(&"field_unresolved_wire_type".to_owned()),
        "negative control: an invented structured KeyIdentity producer must fire"
    );
}

#[test]
fn idr_backup_proof_reserved_logical_shells_are_exact() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();
    for (name, code, row_id, corpus) in [
        (
            "BackupInventoryBijectionProof",
            0x023d,
            "a15:logical-kind:backup-inventory-bijection-proof",
            "corpus/logical/backup_inventory_bijection_proof/",
        ),
        (
            "BackupRecoverabilityProof",
            0x0244,
            "a15:logical-kind:backup-recoverability-proof",
            "corpus/logical/backup_recoverability_proof/",
        ),
    ] {
        let logical = identity
            .logical
            .iter()
            .find(|logical| logical.name == name)
            .expect("backup-proof logical shell exists");
        assert_eq!(logical.object_kind, code);
        assert_eq!(logical.status, "reserved");
        assert_eq!(logical.construction_order, 10);
        assert_eq!(logical.role_predicate, "true");
        assert_eq!(logical.max_size_bytes, 16_777_216);
        assert_eq!(logical.golden_corpus, corpus);

        let reservation = catalog
            .reservations
            .iter()
            .find(|reservation| reservation.symbol == name)
            .expect("backup-proof permanent reservation exists");
        assert_eq!(reservation.identity_class, "logical");
        assert_eq!(reservation.disposition, "existing");

        let source_key = format!("top|{name}");
        let candidate = catalog
            .top_level_candidates
            .iter()
            .find(|candidate| candidate.source_key == source_key)
            .expect("backup-proof source candidate exists");
        assert_eq!(candidate.source_kind, "confirmed");
        assert_eq!(candidate.identity_class, "logical");

        let targets = catalog
            .targets
            .iter()
            .filter(|target| target.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{name} maps exactly once");
        assert_eq!(targets[0].target_row_id, row_id);
        assert_eq!(targets[0].definition_status, "declared");
    }
}

#[test]
fn idr_delta_delivery_output_payload_ref_is_exact() {
    let identity = real_identity();
    let owner = identity
        .logical
        .iter()
        .find(|logical| logical.name == "DeltaDeliveryEnvelope")
        .expect("DeltaDeliveryEnvelope logical owner exists");
    let target = identity
        .logical
        .iter()
        .find(|logical| logical.name == "DeliveredDeltaPayload")
        .expect("DeliveredDeltaPayload logical target exists");
    assert_eq!(owner.construction_order, 35);
    assert_eq!(target.construction_order, 30);

    let fields = identity
        .fields
        .iter()
        .filter(|field| {
            field.containing_schema == "DeltaDeliveryEnvelope<Role:AuthorityOwningRole>"
                && field.stable_name == "output_payload_ref"
        })
        .collect::<Vec<_>>();
    assert_eq!(fields.len(), 1);
    let field = fields[0];
    assert_eq!(field.field_tag, 0x000a);
    assert_eq!(field.exact_wire_type, "StrongRef");
    assert_eq!(field.cardinality, "one");
    assert_eq!(field.identity_class, "logical");
    assert_eq!(field.reference_semantics, "strong");
    assert_eq!(
        field.target_schema_id.as_deref(),
        Some("DeliveredDeltaPayload")
    );
    assert_eq!(field.construction_order, owner.construction_order);
    assert_eq!(field.role_predicate, "true");
    assert_eq!(field.version_status, "reserved");
    assert_eq!(field.max_size_bytes, 40);
    assert!(field.retention_and_cut_rule.contains("30 <= 35"));

    let catalog = real_appendix_catalog();
    let target_rows = catalog
        .targets
        .iter()
        .filter(|row| {
            row.target_row_id
                == "a11:field:delta-delivery-envelope-role-authority-owning-role-output-payload-ref"
        })
        .collect::<Vec<_>>();
    assert_eq!(target_rows.len(), 1);
    assert_eq!(
        target_rows[0].source_key,
        "field|DeltaDeliveryEnvelope<Role:AuthorityOwningRole>|DeltaDeliveryEnvelope<Role:AuthorityOwningRole>.output_payload_ref|output_payload_ref"
    );
    assert_eq!(target_rows[0].target_kind, "field");
    assert_eq!(target_rows[0].definition_status, "declared");
}

#[test]
fn idr_a11_digest_header_and_marker_source_rows_are_exact() {
    let identity = real_identity();
    let field = |schema: &str, name: &str| {
        identity
            .fields
            .iter()
            .find(|field| field.containing_schema == schema && field.stable_name == name)
            .unwrap_or_else(|| panic!("missing A11 field {schema}.{name}"))
    };

    let internal_baseline = field("DeliveredBaselinePayload", "internal_baseline_digest");
    assert_eq!(internal_baseline.field_tag, 0x000c);
    assert_eq!(internal_baseline.exact_wire_type, "digest256");
    assert_eq!(internal_baseline.identity_class, "inline");
    assert_eq!(internal_baseline.reference_semantics, "none");
    assert_eq!(internal_baseline.construction_order, 30);
    assert_eq!(
        internal_baseline.digest_class.as_deref(),
        Some("transcript")
    );
    let internal_recipe = internal_baseline
        .transcript_recipe
        .as_deref()
        .expect("internal baseline transcript recipe");
    assert!(internal_recipe.contains("fgdb:internal-baseline:v1"));
    assert!(internal_recipe.contains("canonical_row_segment_refs"));

    let public_baseline = field("DeliveredBaselinePayload", "public_baseline_digest");
    assert_eq!(public_baseline.field_tag, 0x000d);
    assert_eq!(public_baseline.exact_wire_type, "digest256");
    assert_eq!(public_baseline.identity_class, "inline");
    assert_eq!(public_baseline.construction_order, 30);
    assert_eq!(public_baseline.digest_class.as_deref(), Some("transcript"));
    let public_recipe = public_baseline
        .transcript_recipe
        .as_deref()
        .expect("public baseline transcript recipe");
    assert!(public_recipe.contains("fgdb:public-baseline:v1"));
    assert!(public_recipe.contains("derived_identity_digest"));
    assert!(public_recipe.contains("source_coverage"));
    assert!(public_recipe.contains("baseline_frontier"));

    let delivered_delta = field("DeliveredDeltaPayload", "output_payload_digest");
    assert_eq!(delivered_delta.field_tag, 0x0007);
    assert_eq!(delivered_delta.identity_class, "inline");
    assert_eq!(delivered_delta.construction_order, 30);
    assert_eq!(delivered_delta.digest_class.as_deref(), Some("transcript"));
    assert!(
        delivered_delta
            .transcript_recipe
            .as_deref()
            .is_some_and(|recipe| recipe.contains("fgdb:delivered-zset:v1"))
    );

    let envelope = "DeltaDeliveryEnvelope<Role:AuthorityOwningRole>";
    let header = field(envelope, "authority_bound_header");
    assert_eq!(header.field_tag, 0x0001);
    assert_eq!(header.exact_wire_type, "AuthorityBoundHeader");
    assert_eq!(header.identity_class, "inline");
    assert_eq!(header.reference_semantics, "none");
    assert_eq!(header.construction_order, 35);

    for (name, tag) in [
        ("output_payload_digest", 0x000b),
        ("internal_delivery_digest", 0x000d),
    ] {
        let digest = field(envelope, name);
        assert_eq!(digest.field_tag, tag);
        assert_eq!(digest.exact_wire_type, "digest256");
        assert_eq!(digest.identity_class, "inline");
        assert_eq!(digest.reference_semantics, "none");
        assert_eq!(digest.construction_order, 35);
        assert_eq!(digest.digest_class.as_deref(), Some("transcript"));
        assert!(
            digest
                .transcript_recipe
                .as_ref()
                .is_some_and(|recipe| !recipe.trim().is_empty())
        );
    }

    let marker_source = field("CommitMarker", "effect_source");
    assert_eq!(marker_source.field_tag, 0x0003);
    assert_eq!(marker_source.exact_wire_type, "CommitMarkerEffectSource");
    assert_eq!(marker_source.identity_class, "inline");
    // 15 -> 30 (fgdb-oicl): CommitMarker strongly references CommittedEffectCapsule,
    // whose own body roots at TerminalWriteResultPreparation -> BufferedResultManifest@30,
    // so 15 sat below its own floor. A field row's order must equal its containing
    // kind's, so this witness moves with the kind.
    assert_eq!(marker_source.construction_order, 30);
    let marker_union = identity
        .ordinary_unions
        .iter()
        .find(|union| union.union_name == "CommitMarkerEffectSource")
        .expect("CommitMarker effect-source union");
    assert_eq!(marker_union.containing_schema, "CommitMarker");
    assert_eq!(marker_union.union_path, "CommitMarker.effect_source");
    assert_eq!(marker_union.field_tag, Some(0x0003));
    assert_eq!(marker_union.arms.len(), 2);
    assert_eq!(marker_union.arms[0].source_arm_name, "Local");
    assert_eq!(marker_union.arms[0].arm_tag, 0x0001);
    assert_eq!(
        marker_union.arms[0].payload_sha256.as_deref(),
        Some("9a91654c7169a5fafe5c796da51ae710600b5e21a4e79b10b32f318fe417a130")
    );
    assert_eq!(marker_union.arms[1].source_arm_name, "Global");
    assert_eq!(marker_union.arms[1].arm_tag, 0x0002);
    assert_eq!(
        marker_union.arms[1].payload_sha256.as_deref(),
        Some("44a8e21d2806516e8d1fe1095f5a6375262835d503439d75563403305b031053")
    );

    let capsule = field("CommitMarker", "capsule_ref");
    assert_eq!(
        capsule.target_schema_id.as_deref(),
        Some("CommittedEffectCapsule")
    );

    let catalog = real_appendix_catalog();
    for symbol in [
        "top|InternalBaselineDigest",
        "top|PublicBaselineDigest",
        "top|PublicDeliveryDigest",
    ] {
        let adjudication = catalog
            .ambiguity_adjudications
            .iter()
            .find(|row| row.resolved_source_keys.iter().any(|key| key == symbol))
            .unwrap_or_else(|| panic!("missing A11 adjudication for {symbol}"));
        assert_eq!(adjudication.slice_id, "a11");
        assert_eq!(adjudication.resolution, "not-a-durable-schema");
    }
}

#[test]
fn idr_key_reference_quarantine_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "KeyReferenceQuarantine")
        .expect("KeyReferenceQuarantine logical shell exists");
    assert_eq!(logical.object_kind, 0x02e9);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 10);
    assert_eq!(logical.role_predicate, "role-local");
    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "KeyReferenceQuarantine")
        .expect("KeyReferenceQuarantine permanent reservation exists");
    assert_eq!(reservation.code_reservation, "0x02e9");
    assert_eq!(reservation.disposition, "existing");
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == "top|KeyReferenceQuarantine")
        .expect("KeyReferenceQuarantine source candidate exists");
    assert_eq!(candidate.source_kind, "ambiguous");
    assert_eq!(candidate.identity_class, "logical");
    let targets = catalog
        .targets
        .iter()
        .filter(|target| {
            target.source_key == "projection|logical_object_kinds|KeyReferenceQuarantine"
        })
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].target_row_id,
        "a15:logical-kind:key-reference-quarantine"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|KeyReferenceQuarantine|")
                || row
                    .resolved_source_keys
                    .iter()
                    .any(|source_key| source_key.contains("|KeyReferenceQuarantine|"))
        }),
        "the shell mint must not pre-adjudicate the ambiguous source body"
    );
}

#[test]
fn idr_external_key_destruction_provider_receipt_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "ExternalKeyDestructionProviderReceipt")
        .expect("ExternalKeyDestructionProviderReceipt logical shell exists");
    assert_eq!(logical.object_kind, 0x0297);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 6);
    assert_eq!(logical.role_predicate, "true");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(
        logical.golden_corpus,
        "corpus/logical/external_key_destruction_provider_receipt/"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "ExternalKeyDestructionProviderReceipt")
        .expect("ExternalKeyDestructionProviderReceipt permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a15:reservation:external-key-destruction-provider-receipt"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x0297");
    assert_eq!(reservation.disposition, "existing");

    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == "top|ExternalKeyDestructionProviderReceipt")
        .expect("ExternalKeyDestructionProviderReceipt source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == "top|ExternalKeyDestructionProviderReceipt")
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a15:target:logical-kind-external-key-destruction-provider-receipt"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a15:logical-kind:external-key-destruction-provider-receipt"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity
            .fields
            .iter()
            .any(|field| { field.containing_schema == "ExternalKeyDestructionProviderReceipt" }),
        "the shell increment must not preempt its receipt-field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "ExternalKeyDestructionProviderReceipt"
                || union.union_name == "ExternalKeyDestructionProviderReceipt"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "ExternalKeyDestructionProviderReceipt"
                || union.union_name == "ExternalKeyDestructionProviderReceipt"
        }),
        "the shell increment must not preempt receipt unions or arms"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|ExternalKeyDestructionProviderReceipt|")
                || row.resolved_source_keys.iter().any(|source_key| {
                    source_key.contains("|ExternalKeyDestructionProviderReceipt|")
                })
        }),
        "receipt shorthand ambiguities must remain open until exact field types are settled"
    );

    let target_row_id = "a15:logical-kind:external-key-destruction-provider-receipt";
    let a15 = catalog
        .slices
        .iter()
        .find(|slice| slice.id == "a15")
        .expect("a15 slice exists");
    assert_eq!(
        a15.definition_status, "declared",
        "coverage must close before completion-layer authoring"
    );
    assert!(
        !catalog
            .annotations
            .iter()
            .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .semantic_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .expansion_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .evidence
                .iter()
                .any(|row| row.target_row_id == target_row_id),
        "a declared shell must not skip coverage-first sequencing with premature completion rows"
    );
}

#[test]
fn idr_role_transition_activation_state_is_a_logical_backed_whole_schema_union() {
    let identity = real_identity();
    let union = identity
        .ordinary_unions
        .iter()
        .find(|union| union.union_name == "RoleTransitionActivationState")
        .expect("RoleTransitionActivationState ordinary union exists");
    assert!(
        union.field_tag.is_none()
            && union.containing_schema == union.union_name
            && union.union_path == union.union_name,
        "the role union must keep the whole-schema top-level shape"
    );
    assert_eq!(
        union.allowed_containing_schemas,
        vec!["RoleTransitionActivationState".to_owned()],
        "a whole-schema role union admits only its own object as container"
    );
    let logical_parent = identity
        .logical
        .iter()
        .find(|kind| kind.name == "RoleTransitionActivationState")
        .expect("RoleTransitionActivationState logical kind exists");
    assert_eq!(
        logical_parent.status, union.version_status,
        "the logical parent and the role union must stay lifecycle-identical"
    );
    assert!(
        union.max_size_bytes <= logical_parent.max_size_bytes,
        "the union bound must stay within the object bound"
    );
    assert!(
        !identity
            .wire
            .iter()
            .any(|wire| wire.name == "RoleTransitionActivationState"),
        "disjointness: the role union must never gain a same-name wire row"
    );
}

#[test]
fn idr_generic_signed_role_unions_resolve_through_their_family_rows() {
    let identity = real_identity();
    for signed in [
        "RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>",
        "RoleTimeIssuanceReservationClosure<Role>",
        "TimeSubjectDisposition<Role>",
        "RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>",
        "RoleTimeAuthorityDrainFloorSet<Role>",
        "RoleTimeAuthorityRetirementFloorSet<Role>",
        "ContinuityAuthorityCurrentBasis<Role>",
    ] {
        let union = identity
            .ordinary_unions
            .iter()
            .find(|union| union.union_name == signed)
            .expect("signed role union exists");
        let expected_containers = match signed {
            "RoleTimeAuthorityDrainFloorSet<Role>" => vec![
                signed.to_owned(),
                "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>".to_owned(),
            ],
            "ContinuityAuthorityCurrentBasis<Role>" => vec![
                signed.to_owned(),
                "ContinuityAuthorityObservationImport<Role>".to_owned(),
            ],
            _ => vec![signed.to_owned()],
        };
        assert_eq!(
            union.allowed_containing_schemas, expected_containers,
            "a whole-schema role union must carry its exact self-rooted inline-consumer closure"
        );
        let family = identity::generic_free_family(signed);
        assert!(
            identity.logical.iter().any(|kind| kind.name == family),
            "the generic-free family row must exist: {family:?}"
        );
        assert!(
            identity.logical.iter().all(|kind| kind.name != signed),
            "the signed form itself must never become a kind row"
        );
    }
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
            // The collision law is over the wire NAME set and is indifferent to
            // kind. `record` keeps this a single-law fixture: a fabricated
            // `reference_wrapper` would additionally, and correctly, trip
            // `unclassified_reference_wrapper`.
            kind: "record".into(),
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

/// The generic-free family amendment: a field row may host in a generic
/// expansion of a registered kind, because one registered row commits every
/// expansion of its family.  The relaxation is a *lookup* only — an unregistered
/// family still fails, the family symbol itself is never accepted as a
/// substitute for a real registration, and the construction-order equality that
/// previously skipped generic hosts now applies to them.
#[test]
fn idr_field_containing_schema_resolves_by_generic_free_family() {
    let host = real_identity().logical[0].clone();
    let expansion = format!("{}<Role:Local|Meta>", host.name);

    let mut resolved = real_identity();
    resolved.fields.push(FieldRow {
        containing_schema: expansion.clone(),
        field_tag: 0x7ffd,
        stable_name: "family_fixture".into(),
        exact_wire_type: "u64".into(),
        cardinality: "one".into(),
        identity_class: "scalar".into(),
        reference_semantics: "none".into(),
        target_schema_id: None,
        construction_order: host.construction_order,
        role_predicate: "true".into(),
        retention_and_cut_rule: "family-fixture".into(),
        version_status: "active".into(),
        max_size_bytes: 8,
        digest_class: None,
        transcript_recipe: None,
        bd_domain_separator: None,
        bd_schema_major: None,
        bd_included_field_tags: None,
        bd_excluded_field_tags: None,
        recipe_pin: None,
    });
    let anchor_index = resolved.fields.len() - 1;
    assert_eq!(
        codes_without_assignment_drift(&resolved),
        Vec::<String>::new(),
        "a generic expansion of a registered kind is a resolvable field host"
    );

    // The family is a lookup, not an escape hatch: an unregistered family is
    // still unresolved, exactly as a bare unregistered symbol would be.
    let mut unregistered = resolved.clone();
    unregistered.fields[anchor_index].containing_schema =
        "MissingFixtureFamily<Role:Local|Meta>".into();
    assert_eq!(
        codes_without_assignment_drift(&unregistered),
        vec!["field_unresolved_schema".to_owned()],
        "a generic signature must not launder an unregistered containing family"
    );

    // Resolving through the family makes the containing kind visible, so the
    // construction-order equality that silently skipped generic hosts before
    // the amendment now binds them.
    let mut drifted_order = resolved;
    drifted_order.fields[anchor_index].construction_order = host.construction_order + 1;
    assert_eq!(
        codes_without_assignment_drift(&drifted_order),
        vec!["field_construction_order_mismatch".to_owned()],
        "a generic-hosted field must share its family's construction order"
    );
}

/// The StrongRef-only arm-payload law
/// (`identity::STRONGREF_ONLY_ARM_PAYLOAD_SHAPES`), in all three directions it
/// can be broken, plus the positive: the released tree satisfies it.
///
/// The law exists because a NAMED arm payload shape reads like a schema and the
/// checker accepts both paths for it, so the choice had to be ruled
/// (fgdb-a11-residue-unresolved-schema-ref-laws-54sd) rather than measured.
/// These fixtures are what stops the ruling from decaying back into precedent.
#[test]
fn idr_strongref_only_arm_payload_shapes_stay_on_the_wire_path() {
    let base = real_identity();
    assert_eq!(
        codes_without_assignment_drift(&base),
        Vec::<String>::new(),
        "the released tree satisfies the arm-payload law; every rejection below \
         is therefore attributable to its own mutation"
    );
    assert!(
        !identity::STRONGREF_ONLY_ARM_PAYLOAD_SHAPES.is_empty(),
        "an empty governed set would make every assertion below vacuous"
    );

    for shape in &identity::STRONGREF_ONLY_ARM_PAYLOAD_SHAPES {
        // 0. THE COMPLETENESS GUARD, and it lives here rather than in
        //    `validate_identity` because it is a claim about the RELEASED
        //    registries, not about any registry set: inside the validator it
        //    fired on every synthetic fixture in the suite. Without it, guards
        //    1 and 2 below pass VACUOUSLY once a governed row is renamed or
        //    deleted, and the law has failed open.
        assert!(
            base.wire.iter().any(|w| w.name == shape.name),
            "{} ({}) is governed by the arm-payload law and must stay a registered \
             wire type; with no wire row the guards below bind nothing",
            shape.name,
            shape.source
        );

        // 1. A field row on a governed shape. This is the mistake the law is
        //    written to stop, and it arrives with `field_unresolved_schema`
        //    because a wire owner can never host a field row.
        let mut with_field = base.clone();
        let host = with_field
            .logical
            .iter()
            .map(|k| k.construction_order)
            .next()
            .expect("a logical kind exists");
        with_field.fields.push(FieldRow {
            containing_schema: shape.name.into(),
            field_tag: 0x7ff1,
            stable_name: "arm_payload_fixture".into(),
            exact_wire_type: "u64".into(),
            cardinality: "one".into(),
            identity_class: "scalar".into(),
            reference_semantics: "none".into(),
            target_schema_id: None,
            construction_order: host,
            role_predicate: "true".into(),
            retention_and_cut_rule: "arm-payload-fixture".into(),
            version_status: "active".into(),
            max_size_bytes: 8,
            digest_class: None,
            transcript_recipe: None,
            bd_domain_separator: None,
            bd_schema_major: None,
            bd_included_field_tags: None,
            bd_excluded_field_tags: None,
            recipe_pin: None,
        });
        let mut codes = codes_without_assignment_drift(&with_field);
        codes.sort();
        codes.dedup();
        assert_eq!(
            codes,
            vec![
                "arm_payload_shape_field_row".to_owned(),
                "field_unresolved_schema".to_owned(),
            ],
            "{} must reject a field row",
            shape.name
        );

        // 2. The shape moved wholesale onto the logical path — the wire row
        //    gone, a logical kind in its place. `disjointness_dual_class`
        //    cannot see this: nothing is dual, the class simply changed.
        let mut relanded = base.clone();
        relanded.wire.retain(|w| w.name != shape.name);
        let template = relanded.logical[0].clone();
        relanded.logical.push(LogicalKind {
            object_kind: 0x7ff2,
            name: shape.name.into(),
            ..template
        });
        let mut codes = codes_without_assignment_drift(&relanded);
        codes.sort();
        codes.dedup();
        assert_eq!(
            codes,
            vec!["arm_payload_shape_field_row".to_owned()],
            "{} must reject a re-mint onto a field-owning class",
            shape.name
        );
    }
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
        "fnv1a64:236efa5babe190fe",
        "the pre-codec A10 CommandRef erratum witness must remain explicit"
    );
    let mut pre_erratum = r.clone();
    let current_union_count = pre_erratum.ordinary_unions.len();
    let current_field_count = pre_erratum.fields.len();
    // Post-erratum ordinary unions, whole-schema and embedded alike.  An
    // embedded union's anchor field row exists only because the union does, so
    // the two are removed together and the witness stays exact as the a16
    // embedded-union closure lands.
    let post_erratum_union = |name: &str| {
        matches!(
            name,
            "KeyDestroyExternalAckRef"
                | "TimeAuthorityRegistryProfileState"
                | "LocationForm"
                | "AuthorityPermitRef<Role:AuthorityOwningRole>"
                | "LifecycleAuthoritySource<Role>"
                | "ReadCapablePermitRef<Role:AuthorityOwningRole>"
                | "ReadYourWritesCapabilityLineageState<Role>"
                | "ReplayEvidenceRef<T>"
                | "ReplayObjectRef<T>"
                | "AuditCoverageClaimRef"
                | "AuthenticatedDurableCapabilityStatusBasis"
                | "AuthorizationAuditGate"
                | "DurableCapabilityConsumerTerminalTarget"
                | "DurableCapabilityCurrentMembershipBasis"
                | "DurableCapabilityCurrentMembershipProof"
                | "DurableCapabilityCurrentSecurityBasis"
                | "DurableCapabilityLegacyCurrentMembershipProof"
                | "DurableCapabilityLegacyReplayRef"
                | "DurableCapabilityPresentedReplayRef"
                | "DurableCapabilityStatusClassification"
                | "DurableCapabilityStatusPresentedReplayRef"
                | "ImportedOperationAuditAdmissionEvidence"
                | "ImportedReadAuditEvidence"
                | "ImportedStatementAuditAdmissionEvidence"
                | "LifecycleOperation"
                | "NonRequiredAuditEvidence"
                | "ReplayLogicalStateEvidence"
                | "TimeAuthorityUnavailableStatusEvidence"
                | "KeyDestroyFloorRef"
                | "KeyDestructionTarget"
                | "ExpectedStateCondition"
                | "TerminalAuditGate"
                | "RoleTransitionActivationState"
                | "RestoreSourceAcquisitionSourceGate"
                | "LeaseWindowSuccessorProof"
                | "TimeAuthorityObservationImport"
                | "RoleTimeBoundSubjectInventoryClosure<Role:AuthorityOwningRole>"
                | "RoleTimeIssuanceReservationClosure<Role>"
                | "TimeSubjectDisposition<Role>"
                | "TimeSubjectTerminalProjection"
                | "RoleConfigurationRetentionBasis<Role:AuthorityOwningRole>"
                | "RoleTimeAuthorityDrainFloorSet<Role>"
                | "RoleTimeAuthorityRetirementFloorSet<Role>"
                | "ContinuityAuthorityCurrentBasis<Role>"
                | "ShardRestoreSourceLeaseProjectionSource"
                | "RestoreClaimedTargetAuthorityRecipe"
                | "RestoreIdentityKeyPlan"
                | "RestorePromotionAuthorityProfile"
                | "RestoreServicePromotionReceipt"
                | "RestoreServicePromotionManifestTargetPosture"
                | "LocalRestoreActivationSpecPromotionAuthorityBasis"
                | "RestoreIdentityKeyDispositionEvidence"
                | "TimeValidationClassification"
                | "OfflineMacaroonIssuerEpochState"
                | "TimeAuthorityRegistryTransitionTerminalDisposition"
                | "PortableRestoreSourceLeaseLineageBasis"
                | "TimeSubjectIssuanceReservationState"
                | "MacaroonRootIssuanceState"
                | "RestoreSourceLeasePredecessor"
                | "RestoreSourceLeaseRecordKind"
                | "RestoreSourceLeaseLineageBasis"
                | "TimeBoundOnlineMacaroonRootProjection"
                | "DeliveryFrontier"
                | "CommittedDeltaSourceRef"
                | "CommitMarkerEffectSource"
                | "DeltaDeliveryEnvelopeProvenance"
                | "DeltaDeliveryEnvelopeSourceRole"
                | "RaftMaintenanceCommand"
                | "RemoteConfigurationTrustRoot"
                | "RemoteRetentionControlSpec"
                | "RemoteRetentionConsumerRoot"
                | "RemoteRetentionObligationRoot"
                | "WeakAuthorityAppliedIdentity"
                | "CanonicalConstraintDomainKey"
                | "ConstraintOwnerAssignment"
                | "ConstraintStateMutation"
                | "ConstraintStateMutationSetEqualityBefore"
                | "ConstraintStateMutationSetEqualityAfter"
                | "ConstraintStateMutationSetReferenceBefore"
                | "ConstraintStateMutationSetReferenceAfter"
                | "ConstraintStateMutationSetTemporalIntervalBefore"
                | "ConstraintStateMutationSetTemporalIntervalAfter"
                | "ConstraintStateValue"
                | "QuotaPath"
                | "RetentionMap"
                | "ResourceOwnerKey"
                | "DeliveryTransitionAppliedRef"
                | "LocalAttemptRegistrationSpec"
                | "LocalAuditTicketOwner"
                | "LocalOperationAuditAdmission"
                | "LocalOutcomeCompactionSpec"
                | "LocalResultDeliveryOwner"
                | "LocalResultDeliveryTransitionSpec"
                | "LocalStatementPublishedOutput"
                | "ResultAckAuthority<Role>"
                | "ResultActivationAppliedRef"
                | "ResultDeliveryServiceAuthority"
                | "ResultManifestRef"
                | "ResultReleaseEvidence<Local>"
                | "LocalBeginIdempotencyIndex"
                | "LocalStatementIndex"
                | "ShardingMigrationState"
                | "TerminalWriteResultPreparation"
                | "LocalAttemptRegistrationOperationAuditAdmission"
                | "LocalBeginReservationSpecOperationAuditAdmission"
                | "LocalPrepareAdmissionSpecOperationAuditAdmission"
                | "LocalStatementPublicationSpecStatementAuditAdmission"
                | "LocalStatementRegistrationSpecStatementAuditAdmission"
                | "LocalStatementRegistrationStatementAuditAdmission"
                | "AuthenticatedClientResultAckReceiptPostureAndRole"
                | "AuthenticatedClientResultReleaseReceiptPostureAndRole"
                | "DeliveryLeaseExpiryEvidenceLease"
                | "DerivedGenerationRegistryStatus"
                | "EmbeddedResultFullyConsumedRecordCompletionSource"
                | "LocalAttemptRegistrationRequestKind"
                | "LocalAttemptRegistrationSpecReadYourWritesBasis"
                | "LocalBeginReservationSpecOperationClass"
                | "LocalBeginReservationSpecReadYourWritesBasis"
                | "LocalConflictEdgeKind"
                | "LocalConflictEntryDetailState"
                | "LocalConflictEntryState"
                | "LocalOutcomeExpirySpecExpectedBeginIndexEntry"
                | "LocalReadCloseSpecMode"
                | "LocalSerializationCertificateLifecycleBasis"
                | "LocalTerminalCompletionSpecPrebuiltReadYourWritesIssuanceRef"
                | "LocalTerminalCompletionSpecResultDisposition"
                | "LocalTxnWorkspaceGenerationAuthorityRelationToRegistration"
                | "RequestTerminalAbortStage"
                | "ResultDeliveryDispositionProofDisposition"
                | "ResultEndSendCompletionRecordSurface"
                | "SessionRegionCloseReceiptPosture"
                | "TxnAbortSpecRequiredAbortSource"
                | "TxnOutcomeRecordAuditState"
                | "TxnOutcomeRecordState"
                | "AllocationValueKind"
                | "EscapingAllocationCause"
                | "TxnAllocationSlotKeyPosture"
                | "EscapingAllocationBindingLeaseRef"
                | "CertifiedLegacyLocalArtifactRef<T>"
                | "CheckpointBasis"
                | "GlobalControlOrigin"
                | "GlobalOutcomeDirectoryValue"
                | "GlobalStatePayloadBootstrapPhase"
                | "GlobalTxnRecordDecision"
                | "LegacyDispositionRef"
                | "LegacyTransferEvidenceRef"
                | "PostActivationCapabilityMigrationInitialSubject"
                | "PostActivationCapabilityMigrationSpec"
                | "PublicTerminalProgressProjection"
                | "PublicTerminalProgressProjectionFinalResult"
                | "PublicTerminalProgressProjectionTerminal"
                | "RetiredLocalReclamationProgressState"
                | "RetirementReadinessRef"
                | "ShardingMigrationRetentionFloor"
                | "TerminalPublicOutcome"
                | "TerminalPublicOutcomeFinalResult"
                | "TerminalPublicOutcomeTerminal"
                | "ShardCommandOrigin"
                | "GlobalKeyDestroyAckRef"
                | "CheckpointFieldRecipe"
                | "LocalPreparedRootEntry"
                | "CheckpointStateVectorRole"
                | "RecoveryCheckpointCommandBasis"
                | "RecoveryCheckpointMarkerBasis"
                | "GlobalBranchKeyDistributionPlanOperation"
                | "GlobalRecoveryCheckpointBasis"
                | "MetaPreparedCommandRecordStatus"
                | "MetaRelevanceRangeProofSortedCommandEntriesRecordTerminalKind"
                | "PreparedOwnershipTransferRecordCommandStatus"
                | "PreparedOwnershipTransferRecordTransitionPhase"
                | "ShardPreparedPayloadRecordStatus"
                | "ShardRecoveryCheckpointBasis"
                | "AdministrativeAbortAuthorizationRecordOperationRequestTerminalAbortStage"
                | "GlobalBeginIdempotencyIndex"
                | "GlobalBeginReservationRecordOperationAuditAdmission"
                | "GlobalBeginReservationSpecOperationAuditAdmission"
                | "GlobalBeginReservationSpecReadYourWritesBasis"
                | "GlobalFinalCertificationReservationSelection"
                | "GlobalFinalCertificationReservationState"
                | "GlobalFinalCertificationReserveSpecSelection"
                | "GlobalPrepareAdmissionSpecOperationAuditAdmission"
                | "GlobalReadAuthorizationDecisionRecord"
                | "GlobalReadCloseSpecMode"
                | "GlobalReadCloseSpecOperationAuditAdmission"
                | "GlobalStatementIndex"
                | "GlobalTxnOutcomePreparationRecordOperationAuditAdmission"
                | "GlobalTxnOutcomePreparationRecordTerminalWriteResult"
                | "GlobalTxnOutcomeRecordAuditState"
                | "GlobalTxnOutcomeRecordState"
                | "GlobalTxnOutcomeRecordStatePrepareAdmittedOperationAuditAdmission"
                | "GlobalTxnWorkspaceGenerationAuthorityRelationToRegistration"
                | "LocalBeginTerminalSpecOperationAuditAdmission"
                | "LocalFinalCertificationReservationSelection"
                | "LocalFinalCertificationReservationState"
                | "LocalFinalCertificationReserveSpecSelection"
                | "LocalOrderAttemptInputExpectedNextCommitSeq"
                | "MetaOrderAttemptInputExpectedNextGlobalCommitSeq"
                | "PreparedOrderAttemptRootEntries"
                | "PreparedOrderAttemptRootEntriesOrderedCommitOrder"
                | "ReadParticipantRoutingCertificateExecutionScope"
                | "ResultReleaseEvidence"
                | "ShardReadWitnessBasisExecutionScope"
                | "TerminalAbortAuthorityRequiredLifecycleStage"
                | "TerminalAbortAuthoritySource"
                | "TerminalAbortAuthoritySourceCorrectnessRequiredRegisteredReason"
                | "AttemptFamilyStateActiveGeneration"
                | "AttemptFamilyStateLatestState"
                | "GlobalAttemptIndex"
                | "MetaReadAuditEvidence"
                | "MetaReadAuditEvidenceStatementOperation"
                | "MetaReadAuditEvidenceStatementStatement"
                | "MetaReadAuditEvidenceTerminalAttemptOperation"
                | "NoTerminalPlanLockShareOrOrderProofAllMatchingOrderAttemptDispositions"
                | "NoTerminalPlanLockShareOrOrderProofRole"
                | "ResultDeliveryState"
                | "BranchEpochBoundaryReserveSpecOperation"
                | "BranchGrantBundleOperation"
                | "BranchKeyEpochRecordKind"
                | "BranchKeyEpochRecordPredecessor"
                | "BranchKeyEpochRecordState"
                | "KeyEnvelopeGrantBytesMode"
                | "KeyEnvelopeGrantRecordState"
                | "KeyGrantRegistryEntries"
                | "KeyGrantRegistryEntriesTerminalStatus"
                | "ArchiveLeaseReleaseReceiptOutcome"
                | "AuthorityOwningRestoreAbandonmentTombstoneProfileAppliedBasis"
                | "CatalogAuthorityHeadPredecessor"
                | "CatalogAuthorityHeadState"
                | "LocalRestoreCompletionEvidence"
                | "MetaRestoreCompletionEvidence"
                | "PortableRestoreSourceLeaseAuthorityObservationActionBinding"
                | "PortableRestoreSourceLeaseAuthorityObservationHeadState"
                | "PortableRestoreSourceLeaseAuthorityObservationWindowClass"
                | "RecoveryIncarnationTransformPlanMode"
                | "RecoveryIncarnationTransformPlanSourcePosture"
                | "RestoreAbandonOperationRecordPinOwnerPlan"
                | "RestoreAbandonOperationRecordProfileOperationPlan"
                | "RestoreAbandonmentReceipt"
                | "RestoreLeaseOperationTerminalRecordTerminal"
                | "RestoreRegistry"
                | "RestoreRegistryLocalActivity"
                | "RestoreRegistryMetaActivity"
                | "RestoreRegistryShardActivity"
                | "RestoreSourceKeyAccessCleanupProgressSortedResourceStates"
                | "RestoreTerminalCleanupAuthorityAllowedEffects"
                | "RestoreTerminalCleanupAuthorityTerminalDisposition"
                | "RestoreTerminalPhysicalInventoryAvailabilityRequirement"
                | "RestoreTerminalPinBasis"
                | "RestoreTerminalPinBasisOperationalAckSubject"
                | "ShardRestoreSourceAccessClosureSortedSourceAccessMembersRecordTerminalDisposition"
                | "CatalogAbandonPredecessor"
                | "LocalRestorePhase"
                | "LocalRestorePhaseAwaitingSourceAccessCleanupTerminal"
                | "LocalRestoreRegistryValue"
                | "MetaRestoreRegistryValue"
                | "RecoveryTransformSourceBasis<Role>"
                | "RestoreAbandonAuthorityProfile<Role:AuthorityOwningRole>"
                | "RestoreAbandonmentApplySubjectRecipeCommandKind"
                | "RestoreAbandonmentTombstoneRef<Role>"
                | "RestoreAbandonmentTombstoneSkeletonRecipe<Role>"
                | "RestoreAbandonmentTombstoneSkeletonRecipeAuthorityOwningRole"
                | "RestoreLeaseOperationTerminalHistory<Role>"
                | "RestoreLeaseReleaseEligibility<Role:AuthorityOwningRole>"
                | "RestoreLeaseState<Role:AuthorityOwningRole>"
                | "RestoreRegistryCommonMode"
                | "RestoreRegistryTerminalCommonTerminalDisposition"
                | "RestoreSourceKeyAccessDisposition"
                | "RestoreSourceKeyAccessDispositionEphemeralUseConsumedAndClosedRequiredKind"
                | "RestoreTerminalPinReleaseAuthorizationBodyTerminalDisposition"
                | "ShardRestoreRegistryValue"
                | "CanonicalCatalogRestoreTargetTerminalDisposition<Role,Profile>"
                | "RestoreAbandonAuthorityProfileProjection<Role:AuthorityOwningRole>"
                | "RestoreLeaseOperationTerminalRecordRef<Role>"
                | "RestoreRetentionAnchor<Role>"
                | "ActivatedRetentionCutSetCheckpointInstallRecordRef"
                | "ProvisionalRetentionCutSetCheckpointRef"
                | "ProvisionalRetentionCutSetInstallInputRef"
                | "RetentionCutBodyRole"
                | "GcDecisionRecordDecision"
                | "GcPhysicalDispositionImportSpec<Role>"
                | "GcSemanticState<Role>"
                | "MandatoryInventoryRole"
                | "GlobalAuthorizationDecisionRecordOperationAuditAdmission"
                | "AuditTerminalAttemptRecordState"
                | "AuditTerminalFreezeRecordDomainRecordGroupRole"
                | "AuditTerminalFreezeRecordState"
                | "ConstraintReservationRecordState"
                | "DistributedSerializationCertificateCandidateEdgesRecordKind"
                | "DistributedSerializationCertificateLifecycleBasis"
                | "GlobalAttemptCompactionFloorLastUpdate"
                | "MetaConstraintReservationRecordState"
                | "NoTerminalSignatureOrOrderProofPlanDisposition"
                | "RemoteGrantRetirementPlanSortedEntriesRecordDisposition"
                | "ShardAppliedAckTerminalResult"
                | "ShardPrepareRecordState"
                | "TopologyAttemptClosureProofSortedAttemptEvidence"
                | "TopologyTransitionRecordPhase"
                | "TopologyTransitionRecordPhasePredecessor"
                | "TypedCertifiedMetaProjectionPayloadOrderedSlotDeltasRecordAfterValue"
                | "AllocationEpochReservationMode"
                | "BootstrapReservationAppliedUseIdentityKindSpecificUseSubject"
                | "NewDatabaseIdentityTargetCreationCommitmentExternalCasTargetPosture"
                | "PortableReservationAuthorityObservationIntendedAction"
                | "PortableRestoreContinuityAncestryProofOrderedHeadsRecordTransitionKind"
                | "PreBootstrapReservationBodyRef"
                | "PriorIncarnationLeaseCohortDisposition"
                | "PriorIncarnationLeaseIssuerFenceRowIssuerKind"
                | "RecoveryAllocationEpochAuthority<Role>"
                | "RecoveryBridgeAuthority<Role>"
                | "RecoveryBridgeSourceLeaseBasis<Role>"
                | "ReservationAppliedUseIdentity<Role:AuthorityOwningRole>"
                | "ReservationAuthorityHeadState"
                | "ReservationBodyRef"
                | "ReservationBurnEligibility<Role>"
                | "ReservationBurnSource<Role:AuthorityOwningRole>"
                | "ReservationConsumeAuthorizationBasis<Role,Kind>"
                | "ReservationDispositionEligibility<Role:AuthorityOwningRole>"
                | "ReservationDispositionEligibilityExpiredFixedClaimRequiredRecordTag"
                | "ReservationOperationTerminalRecordAction"
                | "ReservationOperationTerminalRecordTerminal"
                | "ReservationOperationTerminalRecordTerminalNoEffectReason"
                | "ReservationUseRecord<Role:AuthorityOwningRole>"
                | "ReservationUseRecordReservationConflictQuarantinedConflictClass"
                | "RestoreDirectCreationAuthorityRecordMode"
                | "RestoreDirectCreationAuthorityRecordModeCloneNewIdentityCreationAuthority"
                | "RestoreFirstRootPublicationAuthority<Role>"
                | "RestoreReconciliationRoot<Role>"
                | "RestoreReconciliationStatus<Role>"
                | "RestoreState"
                | "RestoreStateLocalPhase"
                | "RestoreStateMetaPhase"
                | "RestoreStateShardPhase"
                | "CertificateAttemptAbandonSpecExpectedLedgerState"
                | "ConfigurationStateForm"
                | "ConfigurationStateGroupRole"
                | "InitialProtocolStateRecipeInheritedOrEmptyAuditQueueRecipe"
                | "InitialProtocolStateRecipeSourceKind"
                | "TopologyStateForm"
                | "TopologyStatePartitionScheme"
                | "TopologyStateSortedShardsRecordState"
                | "ObjectCreationBoundary"
                | "AuditResolutionOrigin"
                | "AuditResolutionOriginUnclaimedTerminalClass"
                | "AuditTicketOwner"
                | "BeginTerminalEvidence"
                | "BufferedResultOwner"
                | "FinalCertificationSelection"
                | "FinalCertificationSelectionLocalCommitTerminalWriteResult"
                | "FinalCertificationSelectionMetaCommitTerminalWriteResult"
                | "LocalOrderSubject"
                | "MetaOrderSubject"
                | "OperationAuditAdmission"
                | "PublicPostconditionProgressProjection"
                | "ReadExecutionScope"
                | "SemanticTerminalDescriptor"
                | "SemanticTerminalDescriptorLocalTerminal"
                | "SemanticTerminalDescriptorShardedTerminal"
                | "StatementAuditAdmission"
                | "StatementPublishedOutput"
                | "TransactionErrorPolicy"
                | "TxnCompletionState"
                | "TxnCompletionStateInProgressPhase"
                | "WorkspaceReleaseEvidenceRef"
                | "GlobalConflictIndexEntriesRecordState"
                | "SequenceNeutralAuditEventBodyOutcome"
                | "TopologyRetirementAckFloorRef"
        )
    };
    pre_erratum
        .ordinary_unions
        .retain(|union| !post_erratum_union(&union.union_name));
    // The A01 2D applied-result fields are typed by `AuthorityAppliedRef`, a
    // wire union that PREDATES the erratum, so `post_erratum_union` cannot
    // recognize them the way it recognizes an embedded union's anchor field.
    // They are matched by their own (schema, field) identity instead.
    let post_erratum_a01_applied_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("RemoteRetentionAckPublishRecord", "authority_applied_ref")
                | ("RemoteRetentionConsumeAckRecord", "consumer_applied_ref")
                | ("RemoteRetentionGrantRecord", "authority_applied_ref")
                | (
                    "RemoteRetentionReleaseRequestRecord",
                    "consumer_applied_ref"
                )
                | ("RemoteRetentionGrantSpec", "authority_configuration_ref")
                | ("ExportLeaf<T>", "target_identity")
                | ("ExportLeaf<T>", "local_strong_ref_projection")
                | ("ExportLeaf<T>", "export_projection_version")
                | ("ExportLeaf<T>", "object_specific_scalar_projection")
                | ("ExportLeaf<T>", "target_closure_inventory_digest")
                | ("ExportLeaf<T>", "authority_ledger_floor")
                | ("ExportLeaf<T>", "quorum_signatures")
        )
    };
    // The a16 field-coverage closure lands plain `StrongRef` rows, which carry
    // no union name either, so they are matched by (schema, field) identity for
    // the same reason as the A01 applied-result fields above.
    let post_erratum_a16_reference_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            (
                "ContinuityAuthorityObservationImport<Role>",
                "portable_observation_ref"
            ) | (
                "ContinuityAuthorityObservationImport<Role>",
                "time_observation_import_ref"
            ) | (
                "ContinuityAuthorityObservationImport<Role>",
                "time_validation_evidence_ref"
            ) | (
                "MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>",
                "issuance_receipt_ref"
            ) | (
                "RestoreSourceLeaseRecord<Role:AuthorityOwningRole>",
                "grant_lineage_proof_ref"
            ) | (
                "RestoreSourceLeaseRecord<Role:AuthorityOwningRole>",
                "lease_lineage_proof_ref"
            ) | (
                "ShardTimeAuthorityRetirementAck",
                "zero_live_old_profile_subject_proof_ref"
            ) | (
                "ShardTimeAuthorityRetirementAck",
                "installed_retirement_floor_ref"
            ) | (
                "ShardTimeAuthorityRetirementFloor",
                "inventory_certificate_ref"
            ) | ("ShardTimeAuthorityRetirementFloor", "retirement_proof_ref")
                | (
                    "TimeAuthorityDrainHold<Role:AuthorityOwningRole>",
                    "retiring_profile_ref"
                )
                | (
                    "TimeAuthorityDrainHold<Role:AuthorityOwningRole>",
                    "issuance_fence_ref"
                )
                | (
                    "TimeAuthorityDrainHold<Role:AuthorityOwningRole>",
                    "offline_issuer_verify_only_service_floor_ref"
                )
                | (
                    "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
                    "plan_ref"
                )
                | (
                    "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
                    "registry_dispatch_terminal_evidence_ref"
                )
                | (
                    "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
                    "receipt_ref"
                )
                | (
                    "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
                    "predecessor_profile_ref"
                )
                | (
                    "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
                    "successor_profile_ref"
                )
                | (
                    "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
                    "issuance_fence_ref"
                )
                | ("TimeValidationEvidence", "profile_ref")
                | ("TimeValidationEvidence", "observation_import_ref")
        )
    };
    // q7ut closes the remaining A16 generic StrongRef target-family law with
    // eight exact field rows. They postdate the historical witness just like
    // the earlier A16 reference cohort and therefore must be removed from its
    // reconstruction by exact owner/member identity.
    let post_erratum_a16_generic_target_family_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("ShardTimeBoundSubjectInventoryCertificate", "inventory_ref")
                | (
                    "ShardTimeBoundSubjectRetirementProof",
                    "shard_inventory_ref"
                )
                | (
                    "TimeAuthorityDrainHold<Role:AuthorityOwningRole>",
                    "inventory_closure_ref"
                )
                | (
                    "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
                    "subject_inventory_closure_ref"
                )
                | (
                    "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
                    "drain_hold_ref"
                )
                | ("TimeBoundSubjectInventoryProof<Role>", "inventory_ref")
                | (
                    "TimeBoundSubjectRetirementProof<Role:AuthorityOwningRole>",
                    "transition_record_ref"
                )
                | (
                    "TimeBoundSubjectRetirementProof<Role:AuthorityOwningRole>",
                    "current_inventory_ref"
                )
        )
    };
    // A15 adds the first seven fully resolved KeyDestroyProposal members, the
    // source-forced WeakStateIdentity basis, n45i's opaque key identity, and
    // wh81's closed one-byte expected lifecycle-state tag.
    // The two shared-union consumer fields are already removed through
    // `post_erratum_union`. Remove the remaining cohort so the historical witness still
    // reconstructs the exact namespace predating every post-erratum field
    // increment.
    let post_erratum_a15_field = |schema: &str, name: &str| {
        schema == "KeyDestroyProposal"
            && matches!(
                name,
                "key_identity"
                    | "expected_key_state"
                    | "basis_state"
                    | "expected_current_configuration_ref"
                    | "checkpoint_and_configuration_floor_refs"
                    | "generated_scanned_root_inventory_ref"
                    | "zero_reference_proof_ref"
                    | "backup_legal_hold_and_external_consumer_ack_refs"
                    | "threshold_authorization_ref"
                    | "sorted_destruction_operation_plans"
            )
    };
    // A11's source-forced non-union fields have no union name for the
    // historical reconstruction to match, so remove them by exact owner/member.
    // The effect_source anchor is also listed here for clarity; the union
    // cohort removes it first through its exact wire type.
    let post_erratum_a11_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("CommitMarker", "effect_source")
                | ("DeliveredBaselinePayload", "internal_baseline_digest")
                | ("DeliveredBaselinePayload", "public_baseline_digest")
                | ("DeliveredDeltaPayload", "output_payload_digest")
                | (
                    "DeltaDeliveryEnvelope<Role:AuthorityOwningRole>",
                    "authority_bound_header"
                )
                | (
                    "DeltaDeliveryEnvelope<Role:AuthorityOwningRole>",
                    "output_payload_ref"
                )
                | (
                    "DeltaDeliveryEnvelope<Role:AuthorityOwningRole>",
                    "output_payload_digest"
                )
                | (
                    "DeltaDeliveryEnvelope<Role:AuthorityOwningRole>",
                    "internal_delivery_digest"
                )
        )
    };
    // a05's single post-erratum field row (fgdb-a05-w12-role-transition-wjj2).
    let post_erratum_a05_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("GlobalTxnRecord", "resulting_global_state_payload_digest")
                | ("ActivationMetaProjectionPayload", "consumer_domain")
                | ("CertifiedRoleTransitionRef", "certificate_identity")
                | ("CertifiedTransitionArtifactRef<T>", "artifact_identity")
                | ("GenesisMetaProjectionPayload", "consumer_domain")
                | (
                    "GlobalControlRecord",
                    "resulting_global_state_payload_digest"
                )
                | (
                    "LegacyRetentionAuthorityTransferEvidence",
                    "source_transfer_record_identity"
                )
                | ("LegacyRetentionAuthorityTransferPlan", "old_local_domain")
                | ("LegacyRetentionAuthorityTransferPlan", "new_meta_domain")
                | ("LocalToShardProjection", "target_shard_domain")
        )
    };
    let post_erratum_a04_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("TopologyState", "applied_control_ref")
                | (
                    "ValidatedRemoteConfigurationAnchor",
                    "consumer_applied_identity"
                )
        )
    };
    // A14 adds GcDecisionRecord's retaining configuration-state reference and
    // inline applied-control identity. Its two ordinary-union anchors are
    // already removed by `post_erratum_union`. Remove the remaining A14
    // cohort so the historical witness still reconstructs the exact namespace
    // predating every post-erratum field increment.
    let post_erratum_a14_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("GcDecisionRecord", "stable_configuration_ref")
                | ("GcDecisionRecord", "applied_control_ref")
                | ("MandatoryInventory", "body_digest")
        )
    };
    // The a04 StrongRef field tranche. Every row is a post-erratum
    // addition, so the historical witness must reconstruct the namespace
    // that predates it.
    let post_erratum_a04_field_tranche = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("CertificateAttemptPlan", "configuration_ref")
                | ("CertificateSignatureShare", "lock_ref")
                | ("CertificateSignerLock", "plan_ref")
                | (
                    "RaftConsensusCutProjection<Role>",
                    "configuration_at_cut_ref"
                )
                | ("RaftHardState", "configuration_state_ref")
                | ("RaftHardState", "prepared_order_attempt_root_ref")
                | ("RaftStateRoot<Role>", "hard_state_ref")
                | ("RemoteConfigurationTrustRoot", "evidence_ref")
                | ("RemoteConfigurationTrustRoot", "anchor_ref")
                | ("RemoteRetentionConsumerRoot", "grant_evidence_ref")
                | (
                    "RemoteRetentionConsumerRoot",
                    "new_authority_grant_evidence_ref"
                )
                | ("RemoteRetentionConsumerRoot", "request_record_ref")
                | ("RemoteRetentionConsumerRoot", "request_leaf_ref")
                | ("RemoteRetentionConsumerRoot", "ack_leaf_ref")
                | ("RemoteRetentionObligationRoot", "grant_record_ref")
                | ("RemoteRetentionObligationRoot", "transfer_record_ref")
                | ("RemoteRetentionObligationRoot", "migration_floor_ref")
                | ("RemoteRetentionObligationRoot", "transition_ref")
                | ("RemoteRetentionObligationRoot", "new_meta_grant_record_ref")
                | ("RemoteRetentionObligationRoot", "transfer_evidence_ref")
                | ("RemoteRetentionObligationRoot", "tombstone_ref")
                | ("RemoteRetentionObligationRoot", "ack_leaf_ref")
                | ("TopologyState", "meta_configuration_ref")
                | ("RaftStateRoot<Role>", "matching_applied_cut_snapshot_ref")
                | (
                    "RemoteRetentionObligationRoot",
                    "source_transfer_record_identity"
                )
                | ("TopologyState", "predecessor_topology_identity")
                | ("ValidatedRemoteConfigurationAnchor", "input_spec_digest")
        )
    };
    // A12's settled field tranches have source-forced types, owners, tags, and
    // dependencies. Remove them as one cohort so the historical witness
    // continues to reconstruct the namespace before post-erratum increments.
    let post_erratum_a12_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("ActivatedRetentionCutSet", "activation_applied_ref")
                | ("ActivatedRetentionCutSet", "provisional_cut_ref")
                | ("ProvisionalRetentionCutSet", "basis_projection_digest")
                | ("ProvisionalRetentionCutSet", "body_ref")
                | ("ConstraintStateRoot", "coordinate")
                | (
                    "ResourceLedgerTransition<Role:AuthorityOwningRole>",
                    "authority_bound_header"
                )
                | (
                    "ResourceLedgerTransition<Role:AuthorityOwningRole>",
                    "basis"
                )
                | (
                    "ResourceLedgerTransition<Role:AuthorityOwningRole>",
                    "owner"
                )
                | (
                    "ResourceLedgerTransition<Role:AuthorityOwningRole>",
                    "authorization_decision_ref"
                )
                | ("RecoveryCheckpoint", "basis_payload_digest")
                | ("RecoveryCheckpoint", "basis_projection_digest")
                | (
                    "RecoveryCheckpoint",
                    "nonretaining_predecessor_checkpoint_digest"
                )
                | ("CheckpointInstallSpec", "basis")
                | ("CheckpointInstallSpec", "basis_payload_digest")
                | ("CheckpointInstallSpec", "basis_projection_digest")
                | ("CheckpointInstallSpec", "checkpoint_ref")
                | ("CheckpointInstallSpec", "checkpoint_state_vector_digest")
                | ("CheckpointInstallSpec", "paired_config_payload_floor_ref")
                | ("CheckpointInstallSpec", "retention_cut_body_ref")
                | ("ConstraintMutationBatch", "apply_basis")
                | ("ConstraintMutationBatch", "before_root_ref")
                | ("ConstraintMutationBatch", "after_root_ref")
                | ("HistoryCutActivationSpec", "provisional_cut_ref")
                | ("HistoryCutActivationSpec", "checkpoint_install_record_ref")
                | ("HistoryCutActivationSpec", "expected_retention_map_basis")
                | (
                    "InitialConfigFloorInstallSpec",
                    "checkpoint_installed_state"
                )
                | ("InitialConfigFloorInstallSpec", "checkpoint_ref")
                | (
                    "InitialConfigFloorInstallSpec",
                    "checkpoint_state_vector_digest"
                )
                | (
                    "InitialConfigFloorInstallSpec",
                    "initial_config_payload_floor_ref"
                )
                | ("InitialConfigFloorInstallSpec", "initial_configuration_ref")
                | ("ResourceChargeEffect", "transition_ref")
                | ("ConstraintStateRoot", "definition_set_ref")
                | ("ResourceLedgerState", "limit_policy_ref")
                | ("RetentionCutBody", "basis_state_identity")
                | (
                    "ResourceLedgerTransition<Role:AuthorityOwningRole>",
                    "idempotency_key_digest"
                )
        )
    };
    // a10 adds the seven field rows the source and the landed tree already
    // determine. Remove them as one cohort so the historical witness keeps
    // reconstructing the namespace that predates post-erratum increments.
    let post_erratum_a10_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("CommittedEffectCapsule", "authority_bound_header")
                | ("CommittedEffectCapsule", "authorization_decision_ref")
                | ("CommittedEffectCapsule", "authorization_decision_digest")
                | ("ControlCommand", "authority_bound_header")
                | ("ControlCommand", "typed_payload_ref")
                | ("ControlCommand", "authorization_decision_ref")
                | ("PreparedCommitRecord", "configuration_ref")
        )
    };
    // a21 landed DurableCapabilityValidationEvidence's field table after the
    // erratum, so those rows are not part of the pre-erratum namespace.
    let post_erratum_a21_field = |schema: &str| schema == "DurableCapabilityValidationEvidence";
    // The a19 StrongRef field tranche. Every row is a post-erratum
    // addition, so the historical witness must reconstruct the namespace
    // that predates it.
    let post_erratum_a19_field_tranche = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            (
                "BootstrapReservationUsePublicationCertificate<Role,Kind>",
                "reservation_claim_import_ref"
            ) | (
                "BootstrapReservationUsePublicationCertificate<Role,Kind>",
                "claim_to_applied_use_derivation_proof_ref"
            ) | (
                "DirectoryBoundCreationEvidence<Role:AuthorityOwningRole>",
                "new_database_identity_claim_import_ref"
            ) | (
                "DirectoryBoundCreationEvidence<Role:AuthorityOwningRole>",
                "allocation_claim_import_ref"
            ) | (
                "PreBootstrapReservationClaimCanonicalImportRecord<Role:AuthorityOwningRole,Kind>",
                "claimed_head_ref"
            ) | (
                "PreBootstrapReservationClaimCanonicalImportRecord<Role:AuthorityOwningRole,Kind>",
                "claim_receipt_ref"
            ) | (
                "PriorIncarnationLeaseBarrierBootstrapImport<Role:AuthorityOwningRole>",
                "portable_barrier_ref"
            ) | (
                "PriorIncarnationLeaseBarrierBootstrapImport<Role:AuthorityOwningRole>",
                "continuity_ancestry_proof_ref"
            ) | (
                "PriorIncarnationLeaseBarrierBootstrapImport<Role:AuthorityOwningRole>",
                "fence_receipt_ref"
            ) | (
                "PriorIncarnationLeaseBarrierBootstrapImport<Role:AuthorityOwningRole>",
                "cohort_window_ref"
            ) | (
                "PriorIncarnationLeaseBarrierBootstrapImport<Role:AuthorityOwningRole>",
                "prebootstrap_portable_expiry_attestation_ref"
            ) | (
                "PriorIncarnationLeaseBarrierBootstrapImport<Role:AuthorityOwningRole>",
                "revocation_receipt_ref"
            ) | (
                "RecoveryAllocationEpochAuthority<Role>",
                "allocation_claim_import_ref"
            ) | (
                "RecoveryBridgeAuthority<Role>",
                "direct_creation_authority_ref"
            ) | (
                "RecoveryBridgeAuthority<Role>",
                "source_acquisition_bundle_ref"
            ) | (
                "RecoveryBridgeAuthority<Role>",
                "latest_source_lease_record_ref"
            ) | (
                "RecoveryBridgeAuthority<Role>",
                "source_lease_projection_ref"
            ) | ("RecoveryIncarnationProjectionResult<Role>", "plan_ref")
                | (
                    "ReservationAuthorityObservationImport<Role:AuthorityOwningRole>",
                    "current_head_ref"
                )
                | (
                    "ReservationAuthorityObservationImport<Role:AuthorityOwningRole>",
                    "portable_observation_ref"
                )
                | (
                    "ReservationAuthorityObservationImport<Role:AuthorityOwningRole>",
                    "time_observation_import_ref"
                )
                | (
                    "ReservationAuthorityObservationImport<Role:AuthorityOwningRole>",
                    "time_validation_evidence_ref"
                )
                | ("ReservationBurnEligibility<Role>", "expired_evidence_ref")
                | (
                    "ReservationBurnEligibility<Role>",
                    "portable_expiry_attestation_ref"
                )
                | (
                    "ReservationBurnSource<Role:AuthorityOwningRole>",
                    "current_observation_import_ref"
                )
                | (
                    "ReservationClaimOperationRecord<Role:AuthorityOwningRole>",
                    "reserved_head_ref"
                )
                | (
                    "ReservationClaimOperationRecord<Role:AuthorityOwningRole>",
                    "reserved_observation_import_ref"
                )
                | (
                    "ReservationConsumeAuthorizationBasis<Role,Kind>",
                    "claim_to_applied_use_derivation_proof_ref"
                )
                | (
                    "ReservationDispositionEligibility<Role:AuthorityOwningRole>",
                    "current_observation_import_ref"
                )
                | (
                    "ReservationDispositionEligibility<Role:AuthorityOwningRole>",
                    "usable_evidence_ref"
                )
                | (
                    "ReservationDispositionEligibility<Role:AuthorityOwningRole>",
                    "current_claimed_observation_import_ref"
                )
                | (
                    "ReservationDispositionEligibility<Role:AuthorityOwningRole>",
                    "expired_evidence_ref"
                )
                | (
                    "ReservationDispositionEligibility<Role:AuthorityOwningRole>",
                    "portable_expiry_attestation_ref"
                )
                | (
                    "ReservationUseRecord<Role:AuthorityOwningRole>",
                    "current_reserved_head_ref"
                )
                | (
                    "ReservationUseRecord<Role:AuthorityOwningRole>",
                    "operation_terminal_history_ref"
                )
                | (
                    "ReservationUseRecord<Role:AuthorityOwningRole>",
                    "operation_record_ref"
                )
                | (
                    "ReservationUseRecord<Role:AuthorityOwningRole>",
                    "claimed_head_ref"
                )
                | (
                    "ReservationUseRecord<Role:AuthorityOwningRole>",
                    "claim_receipt_ref"
                )
                | (
                    "ReservationUseRecord<Role:AuthorityOwningRole>",
                    "consumption_receipt_ref"
                )
                | (
                    "ReservationUseRecord<Role:AuthorityOwningRole>",
                    "burn_receipt_ref"
                )
                | (
                    "ReservationUseRecord<Role:AuthorityOwningRole>",
                    "authenticated_winning_successor_ref"
                )
                | (
                    "RestoreDirectCreationAuthorityRecord<Role:AuthorityOwningRole>",
                    "prior_lease_barrier_bootstrap_import_ref"
                )
                | (
                    "RestoreDirectCreationAuthorityRecord<Role:AuthorityOwningRole>",
                    "new_database_and_security_namespace_claim_import_ref"
                )
                | (
                    "RestoreDirectCreationAuthorityRecord<Role:AuthorityOwningRole>",
                    "creation_evidence_ref"
                )
                | (
                    "RestoreDirectCreationAuthorityRecord<Role:AuthorityOwningRole>",
                    "audit_clone_boundary_ref"
                )
                | (
                    "RestoreDirectCreationAuthorityRecord<Role:AuthorityOwningRole>",
                    "allocation_claim_import_ref"
                )
                | (
                    "RestoreReconciliationCompletionProof<Role>",
                    "reconciliation_root_ref"
                )
                | ("RestoreReconciliationStatus<Role>", "proof_ref")
                | (
                    "RestoreShardBootstrapProjectionCertificate",
                    "projection_ref"
                )
                | ("RestoreShardReadyBarrier", "local_projection_result_ref")
                | ("RestoreShardReadyBarrier", "state_root_ref")
                | ("RestoreShardReadyBarrier", "ready_closure_inventory_ref")
                | (
                    "RestoreShardReadyBarrier",
                    "active_meta_projection_root_ref"
                )
                | (
                    "RestoreShardReadyBarrier",
                    "reconciliation_completion_proof_ref"
                )
        )
    };
    // The a18 StrongRef field tranches. These rows are post-erratum additions,
    // so exclude the exact owner/member cohorts from the historical namespace.
    let post_erratum_a18_field_tranche = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            (
                "AuthorityOwningRestoreAbandonmentTombstone<Role:AuthorityOwningRole>",
                "abandon_operation_record_ref"
            ) | (
                "AuthorityOwningRestoreAbandonmentTombstone<Role:AuthorityOwningRole>",
                "pending_pin_owner_ref"
            ) | (
                "CanonicalCatalogRestoreTargetTerminalDisposition<Role,Profile>",
                "receipt_ref"
            ) | (
                "CanonicalCatalogRestoreTargetTerminalDisposition<Role,Profile>",
                "result_catalog_authority_head_ref"
            ) | (
                "GlobalRestoreParticipantPinReleaseCompletionCertificate",
                "meta_terminal_tombstone_ref"
            ) | (
                "GlobalRestoreParticipantPinReleaseCompletionCertificate",
                "post_finalize_global_state_root_ref"
            ) | ("LocalRestoreCompletionEvidence", "cleanup_authority_ref")
                | (
                    "LocalRestoreTerminalTombstone",
                    "release_operation_summary_ref"
                )
                | (
                    "LocalRestoreTerminalTombstone",
                    "source_key_access_cleanup_accumulator_ref"
                )
                | ("MetaRestoreCompletionEvidence", "cleanup_authority_ref")
                | (
                    "MetaRestoreTerminalTombstone",
                    "release_operation_summary_ref"
                )
                | (
                    "MetaRestoreTerminalTombstone",
                    "source_key_access_cleanup_accumulator_ref"
                )
                | ("RecoveryTransformSourceBasis<Role>", "working_set_ref")
                | (
                    "RestoreAbandonOperationRecord<Role:AuthorityOwningRole>",
                    "pending_owner_ref"
                )
                | (
                    "RestoreAbandonOperationRecord<Role:AuthorityOwningRole>",
                    "meta_pending_owner_ref"
                )
                | ("RestoreLeaseOperationTerminalHistory<Role>", "index_ref")
                | (
                    "RestoreLeaseOperationTerminalHistory<Role>",
                    "accumulator_ref"
                )
                | ("RestoreLeaseOperationTerminalRecordRef<Role>", "record_ref")
                | (
                    "RestoreLeaseReleaseEligibility<Role:AuthorityOwningRole>",
                    "current_lease_authority_observation_import_ref"
                )
                | (
                    "RestoreLeaseReleaseEligibility<Role:AuthorityOwningRole>",
                    "usable_evidence_ref"
                )
                | (
                    "RestoreLeaseReleaseEligibility<Role:AuthorityOwningRole>",
                    "expired_evidence_ref"
                )
                | (
                    "RestoreLeaseReleaseEligibility<Role:AuthorityOwningRole>",
                    "portable_expiry_attestation_ref"
                )
                | ("RestoreRetentionAnchor<Role>", "pending_owner_ref")
                | ("RestoreShardAbandonAck", "abandonment_tombstone_ref")
                | ("RestoreShardAbandonAck", "source_access_closure_ref")
                | ("RestoreShardAbandonAck", "post_close_shard_state_root_ref")
                | (
                    "RestoreSourceKeyAccessCleanupProgress<Role>",
                    "source_access_inventory_ref"
                )
                | (
                    "RestoreSourceKeyAccessCleanupProgress<Role>",
                    "lease_bound_source_access_set_ref"
                )
                | (
                    "RestoreSourceKeyAccessCleanupRecord<Role>",
                    "source_access_inventory_ref"
                )
                | (
                    "RestoreSourceKeyAccessCleanupRecord<Role>",
                    "lease_bound_source_access_set_ref"
                )
                | (
                    "RestoreSourceKeyAccessCleanupRecord<Role>",
                    "final_progress_ref"
                )
                | (
                    "RestoreSourceKeyAccessCleanupRecord<Role>",
                    "accumulator_ref"
                )
                | (
                    "RestoreSourceLeaseAuthorityObservationImport<Role:AuthorityOwningRole>",
                    "portable_observation_ref"
                )
                | (
                    "RestoreSourceLeaseAuthorityObservationImport<Role:AuthorityOwningRole>",
                    "time_observation_import_ref"
                )
                | (
                    "RestoreSourceLeaseAuthorityObservationImport<Role:AuthorityOwningRole>",
                    "time_validation_evidence_ref"
                )
                | (
                    "GlobalRestoreAbandonParticipantApplyCertificate",
                    "post_authorization_global_state_root_ref"
                )
                | (
                    "GlobalRestoreAbandonParticipantApplyCertificate",
                    "visibility_certificate_ref"
                )
                | (
                    "GlobalRestoreAbandonParticipantApplyCertificate",
                    "certificate_attempt_ref"
                )
                | (
                    "GlobalRestoreParticipantPinReleaseCompletionCertificate",
                    "visibility_certificate_ref"
                )
                | (
                    "GlobalRestoreParticipantPinReleaseCompletionCertificate",
                    "certificate_attempt_ref"
                )
                | ("RestoreShardAbandonAck", "certificate_attempt_ref")
                | (
                    "RestoreSourceLeaseAuthorityObservationImport<Role:AuthorityOwningRole>",
                    "current_lease_record_ref"
                )
                | (
                    "RestoreTerminalPinReleaseAuthorization",
                    "post_release_meta_state_root_ref"
                )
                | (
                    "RestoreTerminalPinReleaseAuthorization",
                    "visibility_certificate_ref"
                )
                | (
                    "RestoreTerminalPinReleaseAuthorization",
                    "certificate_attempt_ref"
                )
                | (
                    "ShardRestoreAbandonmentTombstone",
                    "participant_apply_authorization_ref"
                )
                | (
                    "RestoreSourceLeaseReleaseOperationSummary<Role:AuthorityOwningRole>",
                    "source_access_cleanup_accumulator_ref"
                )
                | ("RestoreTerminalPinBasis<Role>", "physical_inventory_ref")
                | (
                    "RestoreTerminalPinBasis<Role>",
                    "pin_durability_receipt_ref"
                )
                | (
                    "RestoreTerminalPinBasis<Role>",
                    "retained_recovery_physical_inventory_ref"
                )
                | (
                    "RestoreTerminalPinDurabilityReceipt<Role,Disposition>",
                    "inventory_ref"
                )
                | (
                    "ShardRestoreAbandonmentTombstone",
                    "own_no_target_observation_proof_ref"
                )
                | ("ShardRestoreAbandonmentTombstone", "pending_pin_owner_ref")
        )
    };
    // The A20 promotion sweep was released after the erratum. Keep its exact
    // source-forced members out of the historical namespace witness. The
    // activation union anchor is removed above with its ordinary union.
    let post_erratum_a20_promotion_field_tranche = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            (
                "GlobalRestoreServiceFinalCertificate",
                "finalize_record_ref"
            ) | (
                "GlobalRestoreServiceFinalCertificate",
                "post_apply_global_state_root_ref"
            ) | (
                "GlobalRestoreServiceFinalCertificate",
                "visibility_certificate_ref"
            ) | (
                "GlobalRestoreServiceFinalCertificate",
                "certificate_attempt_ref"
            ) | (
                "LocalRestoreReadyCertificate",
                "current_hidden_state_root_ref"
            ) | (
                "LocalRestoreReadyCertificate",
                "reconciliation_completion_proof_ref"
            ) | (
                "LocalRestoreReadyCertificate",
                "ready_closure_inventory_ref"
            ) | ("LocalRestoreReadyCertificate", "current_configuration_ref")
                | ("LocalRestoreReadyCertificate", "current_config_floor_ref")
                | ("RestoreShardOperationalAck", "post_close_state_root_ref")
                | ("RestoreShardOperationalAck", "source_access_closure_ref")
                | ("RestoreShardOperationalAck", "certificate_attempt_ref")
                | (
                    "GlobalRestoreServiceCompletionSpec",
                    "final_certificate_ref"
                )
                | (
                    "GlobalRestoreServiceCompletionSpec",
                    "exact_sorted_operational_ack_refs"
                )
                | (
                    "GlobalRestoreServiceFinalizeSpec",
                    "prepare_certificate_ref"
                )
                | (
                    "GlobalRestoreServiceFinalizeSpec",
                    "exact_sorted_ready_ack_refs"
                )
                | (
                    "GlobalRestoreServiceFinalizeSpec",
                    "expected_meta_restore_state"
                )
                | ("LocalRestoreActivationSpec", "ready_certificate_ref")
                | ("LocalRestoreActivationSpec", "expected_local_restore_state")
                | ("LocalRestoreServiceCompletionSpec", "promotion_record_ref")
                | ("LocalRestoreServiceCompletionSpec", "promotion_receipt_ref")
                | ("LocalRestoreServicePromotionSpec", "manifest_ref")
                | ("LocalRestoreServicePromotionSpec", "promotion_receipt_ref")
                | (
                    "LocalRestoreServicePromotionSpec",
                    "expected_local_restore_state"
                )
                | ("ShardRestoreReopenConfirmSpec", "final_certificate_ref")
                | ("ShardRestoreServiceOpenSpec", "final_certificate_ref")
                | ("ShardRestoreServiceOpenSpec", "own_ready_ack_ref")
                | (
                    "ShardRestoreServiceOpenSpec",
                    "expected_local_restore_state"
                )
        )
    };
    // The l6xd owner ruling makes these ten embedded AuthorityBoundHeader
    // fields inline. They landed after the erratum and are not part of the
    // historical namespace.
    let post_erratum_a18_inline_authority_headers = |schema: &str, name: &str| {
        name == "authority_bound_header"
            && matches!(
                schema,
                "AuthorityOwningRestoreAbandonmentTombstone<Role:AuthorityOwningRole>"
                    | "RestoreAbandonOperationRecord<Role:AuthorityOwningRole>"
                    | "RestoreLeaseOperationTerminalRecord<Role:AuthorityOwningRole,Kind:RestoreLeaseOperationKind>"
                    | "RestoreSourceAccessRevocationOperationRecord<Role:AuthorityOwningRole,Kind>"
                    | "RestoreSourceKeyAccessCleanupAccumulator<Role>"
                    | "RestoreSourceKeyAccessCleanupProgress<Role>"
                    | "RestoreSourceKeyAccessCleanupRecord<Role>"
                    | "RestoreSourceKeyAccessInventory<Role:AuthorityOwningRole>"
                    | "RestoreSourceLeaseAuthorityObservationImport<Role:AuthorityOwningRole>"
                    | "RestoreTerminalCleanupAuthority<Role:AuthorityOwningRole>"
            )
    };
    // z542 opens the exact consumer closure for three already-registered wire
    // types and adds the corresponding inline fields. They postdate the
    // erratum and therefore stay out of its historical namespace witness.
    let post_erratum_a18_wire_consumer_fields = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            (
                "CatalogTombstoneRestoreTargetReceipt<Contract>",
                "predecessor"
            ) | ("RestoreTerminalPinBasis<Role>", "terminal_disposition")
                | ("RestoreTerminalPinReleaseAuthorization", "body")
        )
    };
    // p2yb extends the exact self-rooted closure for three logical-backed
    // unions and adds their source-forced inline consumers.
    let post_erratum_a18_logical_union_consumer_fields = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("RecoveryIncarnationTransformPlan<Role>", "source_basis")
                | (
                    "RestoreSourceLeaseReleaseOperationSummary<Role:AuthorityOwningRole>",
                    "lease_operation_terminal_history"
                )
                | ("RestoreTerminalPinBasis<Role>", "abandonment_tombstone_ref")
        )
    };
    // j00a replaces five retaining predecessor self-edges with newly catalogued
    // weak generation-adjacency digests. Remove those rows when reconstructing
    // the namespace that predates every post-erratum field increment.
    let post_erratum_j00a_field = |schema: &str, name: &str| {
        name == "nonretaining_predecessor_digest"
            && matches!(
                schema,
                "TxnOutcomeRecord"
                    | "MetaPreparedCommandRecord"
                    | "ShardPreparedPayloadRecord"
                    | "PreparedCommitRecord"
                    | "TimeSubjectIssuanceReservation<Role>"
            )
    };
    // 2lch replaces four mutual-cycle back-links with comparison-only target
    // digests. These rows also postdate the A10 namespace witness.
    let post_erratum_oicl_digest_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            (
                "GlobalTxnOutcomePreparationRecord",
                "expected_registered_outcome_digest"
            ) | ("NoTerminalSignatureOrOrderProof", "freeze_digest")
                | ("KeyEnvelopeNode", "source_root_digest")
                | ("KeyEnvelopeNode", "source_root_ciphertext_digest")
        )
    };
    // The a07 W12 inline tranche postdates the erratum and must not leak into
    // its historical assignment witness.
    let post_erratum_a07_inline_field = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            (
                "AdministrativeAbortAuthorizationRecord<Role:AuthorityOwningRole>",
                "authority_bound_header"
            ) | ("BufferedResultManifest", "owner")
                | ("GlobalAttemptRegistration", "authority_bound_header")
                | ("GlobalBeginReservationRecord", "authority_bound_header")
                | ("GlobalBeginReservationRecord", "applied_control_ref")
                | ("GlobalBeginReservationSpec", "authority_bound_header")
                | ("GlobalControlCommand", "authority_bound_header")
                | (
                    "GlobalFinalCertificationReservation",
                    "authority_bound_header"
                )
                | ("GlobalFinalCertificationReservation", "applied_control_ref")
                | ("GlobalFinalCertificationReservation", "cancel_applied_ref")
                | (
                    "GlobalFinalCertificationReserveSpec",
                    "authority_bound_header"
                )
                | (
                    "GlobalTxnOutcomePreparationRecord",
                    "authority_bound_header"
                )
                | ("GlobalTxnOutcomeRecord", "authority_bound_header")
                | ("GlobalTxnOutcomeRecord", "applied_control_ref")
                | ("GlobalTxnOutcomeRecord", "applied_abort_ref")
                | ("GlobalTxnOutcomeRecord", "completion_state")
                | ("GlobalTxnWorkspaceGeneration", "authority_bound_header")
                | (
                    "LocalFinalCertificationReservation",
                    "authority_bound_header"
                )
                | ("LocalFinalCertificationReservation", "applied_control_ref")
                | ("LocalFinalCertificationReservation", "cancel_applied_ref")
                | (
                    "LocalFinalCertificationReserveSpec",
                    "authority_bound_header"
                )
                | ("LocalOrderAttemptInput", "subject")
                | ("MetaOrderAttemptInput", "subject")
                | ("ResultDeliveryLease", "authority_bound_header")
                | ("ResultDeliveryLease", "activation_applied_ref")
                | (
                    "TerminalAbortAuthority<Role:AuthorityOwningRole>",
                    "authority_bound_header"
                )
        )
    };
    // The checker-clean registered-target StrongRef rows form the second a07
    // W12 field tranche and are likewise outside the historical namespace.
    let post_erratum_a07_strong_field = |schema: &str, name: &str| match schema {
        "AttemptFamilyState" => matches!(name, "registration_ref" | "outcome_ref"),
        "AuditTicketClaimRecord" => matches!(name, "ticket_ref" | "claiming_control_ref"),
        "BufferedResultManifest" => name == "canonical_schema_ref",
        "GlobalBeginIdempotencyIndex" => {
            matches!(
                name,
                "reservation_ref" | "registration_ref" | "terminal_record_ref"
            )
        }
        "GlobalBeginReservationRecord" => name == "source_spec_ref",
        "GlobalControlCommand" => matches!(
            name,
            "typed_payload_ref" | "order_attempt_input_ref" | "authorization_decision_ref"
        ),
        "GlobalFinalCertificationReservation" => {
            matches!(name, "source_spec_ref" | "registration_ref")
        }
        "GlobalFinalCertificationReserveSpec" => name == "registration_ref",
        "GlobalPrepareAdmissionSpec" => matches!(
            name,
            "registration_ref"
                | "expected_workspace_generation_ref"
                | "routing_certificate_ref"
                | "global_authorization_decision_ref"
                | "expected_topology_state_ref"
        ),
        "GlobalReadCloseSpec" => matches!(
            name,
            "registration_ref"
                | "read_routing_certificate_ref"
                | "global_read_authorization_decision_ref"
                | "buffered_result_manifest_ref"
                | "workspace_generation_ref"
                | "distributed_serialization_certificate_ref"
        ),
        "GlobalTxnOutcomePreparationRecord" => matches!(
            name,
            "registration_ref"
                | "routing_certificate_ref"
                | "global_authorization_decision_ref"
                | "preparation_ref"
        ),
        "GlobalTxnOutcomeRecord" => matches!(
            name,
            "registration_ref"
                | "latest_workspace_generation_ref"
                | "workspace_generation_ref"
                | "global_delta_publish_command_ref"
                | "read_close_spec_ref"
        ),
        "GlobalTxnWorkspaceGeneration" => matches!(
            name,
            "registration_ref" | "statement_registration_ref" | "topology_state_ref"
        ),
        "LocalFinalCertificationReservation" => {
            matches!(name, "source_spec_ref" | "registration_ref")
        }
        "LocalFinalCertificationReserveSpec" => name == "registration_ref",
        "LocalOrderAttemptInput" | "MetaOrderAttemptInput" => name == "configuration_ref",
        "ReadParticipantRoutingCertificate" => matches!(
            name,
            "topology_state_ref"
                | "workspace_generation_ref"
                | "partition_derivation_proof_ref"
                | "one_digest_signer_lock_ref"
        ),
        "ResultReleaseEvidence<Meta>" => matches!(name, "receipt_ref" | "expiry_evidence_ref"),
        "ShardReadWitnessBasis" => name == "shard_configuration_ref",
        "TerminalAbortAuthority<Role:AuthorityOwningRole>" => matches!(
            name,
            "administrative_abort_authorization_ref" | "original_operation_authorization_ref"
        ),
        _ => false,
    };
    let post_erratum_a07_weak_field = |schema: &str, name: &str| {
        schema == "NeverRegisteredTerminalRecord" && name == "reservation_identity"
    };
    // A08's lifecycle tranche adds every checker-clean field whose source
    // spelling, containing kind, reference semantics, and construction edge
    // are forced. The gated future/self/cyclic edges are deliberately absent.
    let post_erratum_a08_field = |schema: &str, name: &str| match schema {
        "AttemptCompactionAttestation" => matches!(
            name,
            "build_control_identity"
                | "build_resulting_root_identity"
                | "floor_record_identity"
                | "one_digest_signer_lock_ref"
                | "summary_root_ref"
        ),
        "AuditEventSignerLock" => matches!(name, "attempt_plan_ref" | "terminal_plan_ref"),
        "AuditEventSigningAttemptPlan" => {
            matches!(name, "configuration_ref" | "terminal_plan_ref")
        }
        "AuditResolutionSignerLock" => {
            matches!(name, "attempt_plan_ref" | "terminal_plan_ref")
        }
        "AuditResolutionSigningAttemptPlan" => {
            matches!(name, "configuration_ref" | "terminal_plan_ref")
        }
        "AuditTerminalAttemptRecord" => matches!(
            name,
            "attempt_plan_ref" | "resolution_attempt_plan_ref" | "terminal_plan_ref"
        ),
        "AuditTerminalFreezeRecord" => matches!(
            name,
            "begin_release_applied_ref"
                | "control_terminal_basis_hold_root_ref"
                | "release_applied_ref"
                | "scaffolding_applied_ref"
        ),
        "AuditTerminalSigningPlan" => matches!(name, "event_body_ref" | "resolution_body_ref"),
        "ClosedAttemptCompactionFloorRecord" => matches!(
            name,
            "meta_checkpoint_identity" | "source_command_digest" | "terminal_summary_root_ref"
        ),
        "ClosedAttemptCompactionSpec" => matches!(
            name,
            "candidate_summary_root_ref"
                | "evidence_bundle_ref"
                | "expected_prior_summary_root_ref"
        ),
        "ConstraintReservationRecord" => matches!(
            name,
            "prepare_admission_record_ref" | "registration_ref" | "routing_certificate_ref"
        ),
        "DistributedSerializationCertificate" => matches!(
            name,
            "conflict_index_basis_digest"
                | "prepare_admission_record_ref"
                | "read_routing_certificate_ref"
                | "routing_certificate_ref"
        ),
        "GlobalAttemptCompactionFloor" => {
            matches!(
                name,
                "attestation_ref" | "record_identity" | "summary_root_ref"
            )
        }
        "GlobalAuthorizationDecisionRecord" => matches!(
            name,
            "authority_bound_header"
                | "mutation_permit_ref"
                | "registration_ref"
                | "routing_certificate_ref"
        ),
        "GlobalClosedAttemptFloorPublishSpec" => matches!(
            name,
            "attestation_ref"
                | "expected_prior_authoritative_floor"
                | "expected_summary_root_ref"
                | "floor_record_ref"
        ),
        "GlobalConstraintReservationCertificate" => matches!(name, "reservation_record_ref"),
        "GlobalLogicalDeltaBatch" => matches!(name, "global_txn_record_ref" | "marker_identity"),
        "GlobalTxnCommand" => matches!(
            name,
            "authority_bound_header"
                | "final_certification_reservation_ref"
                | "order_attempt_input_ref"
                | "prepare_admission_record_ref"
                | "registration_ref"
                | "topology_state_ref"
        ),
        "MetaConstraintReservationRecord" => matches!(
            name,
            "admission_spec_ref"
                | "applied_abort_ref"
                | "global_txn_command_ref"
                | "registration_ref"
                | "topology_state_ref"
        ),
        "MetaControlProjectionCertificate" => {
            matches!(
                name,
                "source_global_control_record_ref" | "typed_payload_ref"
            )
        }
        "NeverRegisteredEvidence" => matches!(
            name,
            "begin_reservation_ref" | "no_reservation_binding_proof_ref" | "terminal_record_ref"
        ),
        "NeverRegisteredFloorSpec" => {
            matches!(
                name,
                "candidate_summary_root_ref" | "expected_prior_summary_root_ref"
            )
        }
        "NoTerminalSignatureOrOrderProof" => {
            matches!(
                name,
                "abandonment_ref" | "configuration_ref" | "terminal_plan_ref"
            )
        }
        "ParticipantRoutingCertificate" => matches!(
            name,
            "partition_derivation_proof_ref" | "registration_ref" | "topology_state_ref"
        ),
        "ParticipantRoutingDerivationProof" => matches!(name, "topology_state_ref"),
        "RemoteGrantRetirementPlan" => matches!(
            name,
            "basis_topology_ref" | "old_grant_evidence_ref" | "successor_configuration_ref"
        ),
        "ShardAppliedAck" => matches!(name, "resulting_post_visibility_shard_state_root_ref"),
        "ShardDecisionApplyCommand" => matches!(name, "decision_publish_record_ref"),
        "ShardEffectBasis" => matches!(name, "active_meta_projection_root_ref"),
        "ShardEffectFragment" => matches!(
            name,
            "basis_ref"
                | "global_authorization_ref"
                | "global_outcome_preparation_ref"
                | "prepare_admission_record_ref"
                | "routing_certificate_ref"
        ),
        "ShardGcPreflightEvidence" => matches!(name, "preflight_record_ref"),
        "ShardGcPreflightRecord" => matches!(name, "source_spec_ref"),
        "ShardGcPreflightSpec" => matches!(name, "configuration_ref"),
        "ShardPrepareCommand" => matches!(
            name,
            "active_meta_projection_root_ref"
                | "basis_ref"
                | "expected_shard_configuration_ref"
                | "fragment_ref"
                | "prepare_admission_record_ref"
                | "registration_ref"
                | "routing_certificate_ref"
        ),
        "ShardPrepareRecord" => matches!(
            name,
            "basis_ref" | "fragment_ref" | "shard_configuration_ref"
        ),
        "TopologyAttemptClosureProof" => matches!(
            name,
            "abort_outcome_ref" | "outcome_ref" | "published_abort_ref" | "published_decision_ref"
        ),
        "TopologyRetirementFloor" => matches!(name, "cutover_record_ref"),
        "TopologyTransferAck" => matches!(name, "destination_checkpoint_ref" | "plan_ref"),
        "TopologyTransitionRecord" => matches!(name, "command_ref"),
        "TypedCertifiedMetaProjectionPayload" => {
            matches!(name, "source_global_control_record_ref")
        }
        _ => false,
    };
    // The a09 storage-identity field cohort: IdentityContinuityRecord,
    // IdRangeLease<Role:AuthorityOwningRole>, and TxnAllocationBindingRoot each
    // had ZERO field rows before this landing, so the whole schema is a
    // post-erratum addition and the historical witness reconstructs without it.
    let post_erratum_a09_field = |schema: &str| {
        matches!(
            schema,
            "IdentityContinuityRecord"
                | "IdRangeLease<Role:AuthorityOwningRole>"
                | "TxnAllocationBindingRoot"
        )
    };
    // A16's four exact AuthorityBoundHeader<Role> fields use the already
    // registered generic-free wire family inline. They postdate the erratum
    // and therefore stay out of its historical namespace witness.
    let post_erratum_a16_inline_authority_headers = |schema: &str, name: &str| {
        name == "authority_bound_header"
            && matches!(
                schema,
                "ProtectedErrorReplayTimeBasis<Role>"
                    | "MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>"
                    | "RestoreSourceLeaseRecord<Role:AuthorityOwningRole>"
                    | "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>"
            )
    };
    // l6xd's A03 closeout and its immediate beneficiary add twelve flat
    // AuthorityBoundHeader members plus two flat AppliedControlRef members.
    // The four LocalStatementIndex source occurrences remain committed by
    // exact arm payloads. These rows postdate the A10 namespace erratum and
    // must not contaminate its historical assignment witness.
    let post_erratum_a03_inline_fields = |schema: &str, name: &str| {
        (name == "authority_bound_header"
            && matches!(
                schema,
                "AuthenticatedClientResultAckReceipt<Role:AuthorityOwningRole>"
                    | "AuthenticatedClientResultReleaseReceipt<Role:AuthorityOwningRole>"
                    | "LocalAttemptRegistration"
                    | "LocalBeginReservationSpec"
                    | "LocalTxnWorkspaceGeneration"
                    | "TxnOutcomeRecord"
                    | "LocalBeginReservationRecord"
                    | "LocalBufferedResultManifest"
                    | "ResultDeliveryPolicy<Role>"
                    | "ResultDeliveryLeaseTimeBasis<Role>"
                    | "LocalResultDeliveryLease"
                    | "AdmittedTxnAbortCommand"
            ))
            || matches!(
                (schema, name),
                ("LocalAuditTicketClaimRecord", "claim_applied_ref")
                    | ("LocalBeginReservationRecord", "applied_control_ref")
            )
    };
    // yenh opens five exact A03 wire/ordinary-union consumer closures and
    // lands their source-forced inline fields. They postdate the erratum and
    // therefore stay out of its historical assignment witness.
    let post_erratum_a03_wire_consumer_fields = |schema: &str, name: &str| {
        matches!(
            (schema, name),
            ("LocalAuditTicketClaimRecord", "owner")
                | ("ResultDeliveryPolicy<Role>", "manifest_ref")
                | ("ResultDeliveryPolicy<Role>", "service_authority")
                | ("LocalResultDeliveryLease", "owner")
                | ("LocalResultDeliveryLease", "activation_applied_ref")
        )
    };
    pre_erratum.fields.retain(|field| {
        !post_erratum_a21_field(&field.containing_schema)
            && !post_erratum_union(&field.exact_wire_type)
            && !post_erratum_a01_applied_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a16_reference_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a16_generic_target_family_field(
                &field.containing_schema,
                &field.stable_name,
            )
            && !post_erratum_a15_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a11_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a05_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a04_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a14_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a19_field_tranche(&field.containing_schema, &field.stable_name)
            && !post_erratum_a18_field_tranche(&field.containing_schema, &field.stable_name)
            && !post_erratum_a20_promotion_field_tranche(
                &field.containing_schema,
                &field.stable_name,
            )
            && !post_erratum_a18_inline_authority_headers(
                &field.containing_schema,
                &field.stable_name,
            )
            && !post_erratum_a18_wire_consumer_fields(&field.containing_schema, &field.stable_name)
            && !post_erratum_a18_logical_union_consumer_fields(
                &field.containing_schema,
                &field.stable_name,
            )
            && !post_erratum_a04_field_tranche(&field.containing_schema, &field.stable_name)
            && !post_erratum_a12_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a10_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_j00a_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_oicl_digest_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a07_inline_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a07_strong_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a07_weak_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a08_field(&field.containing_schema, &field.stable_name)
            && !post_erratum_a09_field(&field.containing_schema)
            && !post_erratum_a16_inline_authority_headers(
                &field.containing_schema,
                &field.stable_name,
            )
            && !post_erratum_a03_inline_fields(&field.containing_schema, &field.stable_name)
            && !post_erratum_a03_wire_consumer_fields(&field.containing_schema, &field.stable_name)
    });
    assert_eq!(
        pre_erratum.ordinary_unions.len() + 366,
        current_union_count,
        "the historical witness must remove every post-erratum union through the A20 promotion sweep"
    );
    assert_eq!(
        pre_erratum.fields.len() + 571,
        current_field_count,
        "the historical witness must remove every post-erratum field cohort through the A12 residue tranche"
    );
    rename_logical_command_input_union(&mut pre_erratum, "CommandRef");
    undo_a01_exactness_repair(&mut pre_erratum);
    undo_cq4x_capsule_retarget(&mut pre_erratum);
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
fn idr_a14_gc_decision_and_inventory_union_anchors_are_exact() {
    let identity = real_identity();
    // GcDecisionRecord 15 -> 25 -> 40 (fgdb-oicl). 25 came from a matcher that read
    // a reference by `strip_prefix`, so a repeated member spelled `[StrongRef<T>]`
    // was dropped and the derived floor was too low -- the same under-reading
    // fgdb-suhb traces to a real frozen violation. A balanced scan over every
    // retaining wrapper puts the floor at 40. The field rows carry the containing
    // kind's order by law.
    let expected_fields = [
        (
            "GcDecisionRecord",
            "applied_control_ref",
            0x0002,
            "AppliedControlRef",
            40,
            49,
        ),
        (
            "GcDecisionRecord",
            "decision",
            0x0007,
            "GcDecisionRecordDecision",
            40,
            16_777_216,
        ),
        (
            "MandatoryInventory",
            "role",
            0x0004,
            "MandatoryInventoryRole",
            25,
            16_777_216,
        ),
    ];
    for (schema, name, tag, wire_type, order, max_size) in expected_fields {
        let field = identity
            .fields
            .iter()
            .find(|field| field.containing_schema == schema && field.stable_name == name)
            .unwrap_or_else(|| panic!("{schema}.{name} field exists"));
        assert_eq!(field.field_tag, tag, "{schema}.{name} source-order tag");
        assert_eq!(field.exact_wire_type, wire_type);
        assert_eq!(field.cardinality, "one");
        assert_eq!(field.identity_class, "inline");
        assert_eq!(field.reference_semantics, "none");
        assert_eq!(field.target_schema_id, None);
        assert_eq!(field.construction_order, order);
        assert_eq!(field.role_predicate, "true");
        assert_eq!(field.version_status, "reserved");
        assert_eq!(field.max_size_bytes, max_size);
    }

    for (union_name, schema, tag) in [
        ("GcDecisionRecordDecision", "GcDecisionRecord", 0x0007),
        ("MandatoryInventoryRole", "MandatoryInventory", 0x0004),
    ] {
        let union = identity
            .ordinary_unions
            .iter()
            .find(|union| union.union_name == union_name)
            .unwrap_or_else(|| panic!("{union_name} ordinary union exists"));
        assert_eq!(union.containing_schema, schema);
        assert_eq!(union.field_tag, Some(tag));
        assert_eq!(
            identity
                .fields
                .iter()
                .filter(|field| field.exact_wire_type == union_name)
                .count(),
            1,
            "{union_name} has exactly one matching field anchor"
        );
    }

    let body_digest = identity
        .fields
        .iter()
        .find(|field| {
            field.containing_schema == "MandatoryInventory" && field.stable_name == "body_digest"
        })
        .expect("MandatoryInventory.body_digest field exists");
    assert_eq!(body_digest.field_tag, 0x0007);
    assert_eq!(body_digest.exact_wire_type, "digest256");
    assert_eq!(body_digest.cardinality, "one");
    assert_eq!(body_digest.identity_class, "scalar");
    assert_eq!(body_digest.reference_semantics, "none");
    assert_eq!(body_digest.target_schema_id, None);
    assert_eq!(body_digest.construction_order, 25);
    assert_eq!(body_digest.role_predicate, "true");
    assert_eq!(body_digest.version_status, "reserved");
    assert_eq!(body_digest.max_size_bytes, 32);
    assert_eq!(body_digest.digest_class.as_deref(), Some("body"));
    assert_eq!(
        body_digest.bd_domain_separator.as_deref(),
        Some("fgdb:body:mandatory-inventory:v1")
    );
    assert_eq!(body_digest.bd_schema_major, Some(1));
    assert_eq!(body_digest.bd_included_field_tags, Some(vec![]));
    assert_eq!(body_digest.bd_excluded_field_tags, Some(vec![7]));
    let transcript = bodydigest_transcript(
        "MandatoryInventory",
        "fgdb:body:mandatory-inventory:v1",
        1,
        &[],
        &[7],
    );
    assert_eq!(
        body_digest.recipe_pin.as_deref(),
        Some(bodydigest_pin(&transcript).as_str())
    );

    let catalog = real_appendix_catalog();
    for source_key in [
        "field|GcDecisionRecord|GcDecisionRecord.applied_control_ref|applied_control_ref",
        "field|GcDecisionRecord|GcDecisionRecord.decision|decision",
        "field|MandatoryInventory|MandatoryInventory.role|role",
        "field|MandatoryInventory|MandatoryInventory.body_digest|body_digest",
    ] {
        let targets = catalog
            .targets
            .iter()
            .filter(|target| target.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{source_key} maps exactly once");
        assert_eq!(targets[0].slice_id, "a14");
        assert_eq!(targets[0].target_kind, "field");
        assert_eq!(targets[0].definition_status, "declared");
    }
}

#[test]
fn idr_a12_retention_cut_fields_are_source_ordered_and_exact() {
    let identity = real_identity();
    let expected = [
        (
            "ActivatedRetentionCutSet",
            "activation_applied_ref",
            0x0003,
            "AuthorityAppliedRef",
            "inline",
            "none",
            None,
            49,
            None,
        ),
        (
            "ActivatedRetentionCutSet",
            "provisional_cut_ref",
            0x0001,
            "StrongRef",
            "logical",
            "strong",
            Some("ProvisionalRetentionCutSet"),
            40,
            None,
        ),
        (
            "ProvisionalRetentionCutSet",
            "basis_projection_digest",
            0x0004,
            "WeakDigest",
            "logical",
            "weak_digest",
            None,
            32,
            Some("weak_identity"),
        ),
        (
            "ProvisionalRetentionCutSet",
            "body_ref",
            0x0001,
            "StrongRef",
            "logical",
            "strong",
            Some("RetentionCutBody"),
            40,
            None,
        ),
    ];
    for (
        schema,
        name,
        tag,
        wire_type,
        identity_class,
        reference_semantics,
        target,
        max_size,
        digest_class,
    ) in expected
    {
        let field = identity
            .fields
            .iter()
            .find(|field| field.containing_schema == schema && field.stable_name == name)
            .unwrap_or_else(|| panic!("{schema}.{name} field exists"));
        assert_eq!(field.field_tag, tag, "{schema}.{name} source-order tag");
        assert_eq!(field.exact_wire_type, wire_type);
        assert_eq!(field.cardinality, "one");
        assert_eq!(field.identity_class, identity_class);
        assert_eq!(field.reference_semantics, reference_semantics);
        assert_eq!(field.target_schema_id.as_deref(), target);
        // 20 -> 40 (fgdb-oicl): both cut sets strongly reference RetentionCutBody,
        // whose own body is rooted at 40, so 20 sat below their shared floor. Still
        // one shared literal only because the two owners still share one order -- it
        // must be split per entry the moment they diverge.
        assert_eq!(field.construction_order, 40);
        assert_eq!(field.role_predicate, "true");
        assert_eq!(field.version_status, "reserved");
        assert_eq!(field.max_size_bytes, max_size);
        assert_eq!(field.digest_class.as_deref(), digest_class);
    }

    let catalog = real_appendix_catalog();
    for source_key in [
        "field|ActivatedRetentionCutSet|ActivatedRetentionCutSet.activation_applied_ref|activation_applied_ref",
        "field|ActivatedRetentionCutSet|ActivatedRetentionCutSet.provisional_cut_ref|provisional_cut_ref",
        "field|ProvisionalRetentionCutSet|ProvisionalRetentionCutSet.basis_projection_digest|basis_projection_digest",
        "field|ProvisionalRetentionCutSet|ProvisionalRetentionCutSet.body_ref|body_ref",
    ] {
        let targets = catalog
            .targets
            .iter()
            .filter(|target| target.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{source_key} maps exactly once");
        assert_eq!(targets[0].slice_id, "a12");
        assert_eq!(targets[0].target_kind, "field");
        assert_eq!(targets[0].definition_status, "declared");
    }
}

#[test]
fn idr_a12_residue_promotions_and_fields_are_source_exact() {
    let identity = real_identity();
    let mut catalog = real_appendix_catalog();

    for (name, code, order, role, slice, source_key) in [
        (
            "RecoveryCheckpoint",
            0x03aa,
            30,
            "role-local",
            "a03",
            "top|RecoveryCheckpoint",
        ),
        (
            "ConstraintDefinitionSet",
            0x027b,
            30,
            "true",
            "a12",
            "projection|logical_object_kinds|ConstraintDefinitionSet",
        ),
        (
            "ResourceLimitPolicy",
            0x03d1,
            30,
            "true",
            "a12",
            "projection|logical_object_kinds|ResourceLimitPolicy",
        ),
    ] {
        let logical = identity
            .logical
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("missing reservation-backed A12 target {name}"));
        assert_eq!(logical.object_kind, code);
        assert_eq!(logical.status, "reserved");
        assert_eq!(logical.construction_order, order);
        assert_eq!(logical.role_predicate, role);
        assert_eq!(logical.max_size_bytes, 16_777_216);

        let reservation = catalog
            .reservations
            .iter()
            .find(|row| row.symbol == name)
            .unwrap_or_else(|| panic!("missing reservation for {name}"));
        assert_eq!(reservation.identity_class, "logical");
        assert_eq!(reservation.code_reservation, format!("0x{code:04x}"));
        assert_eq!(reservation.disposition, "existing");

        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{name} must have one projection target");
        assert_eq!(targets[0].slice_id, slice);
        assert_eq!(targets[0].target_kind, "logical-kind");
        assert_eq!(targets[0].definition_status, "declared");
        if name == "RecoveryCheckpoint" {
            let candidate = catalog
                .top_level_candidates
                .iter()
                .find(|row| row.source_key == source_key)
                .expect("RecoveryCheckpoint source candidate exists");
            assert_eq!(candidate.identity_class, "logical");
            assert_eq!(
                targets[0].row_id,
                "a03:target:logical-kind-recovery-checkpoint"
            );
            assert_eq!(
                targets[0].target_row_id,
                "a03:logical-kind:recovery-checkpoint"
            );
        }
    }

    for (name, code, order, role) in [
        ("CheckpointInstallSpec", 0x0566, 30, "role-local"),
        ("ConstraintMutationBatch", 0x0567, 30, "true"),
        ("HistoryCutActivationSpec", 0x0568, 40, "role-local"),
        ("InitialConfigFloorInstallSpec", 0x0569, 30, "role-local"),
        ("ResourceChargeEffect", 0x056a, 15, "true"),
    ] {
        let logical = identity
            .logical
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("missing A12 structural-body kind {name}"));
        assert_eq!(logical.object_kind, code);
        assert!(
            logical.object_kind > i64::from(appendix_a::EXPECTED_RESERVATION_HIGH_WATER),
            "{name} has no StrongRef reservation and must use fresh code space"
        );
        assert_eq!(logical.status, "reserved");
        assert_eq!(logical.construction_order, order);
        assert_eq!(logical.role_predicate, role);
        assert_eq!(logical.max_size_bytes, 16_777_216);
        assert!(
            catalog
                .reservations
                .iter()
                .all(|reservation| reservation.symbol != name),
            "non-reservation family {name} must not acquire a reservation"
        );

        let source_key = format!("top|{name}");
        let candidate = catalog
            .top_level_candidates
            .iter()
            .find(|row| row.source_key == source_key)
            .unwrap_or_else(|| panic!("missing source candidate for {name}"));
        assert_eq!(candidate.identity_class, "logical");
        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{name} must have one source target");
        assert_eq!(targets[0].slice_id, "a12");
        assert_eq!(targets[0].target_kind, "logical-kind");
        assert_eq!(targets[0].definition_status, "declared");
    }

    struct ExpectedField {
        schema: &'static str,
        name: &'static str,
        tag: i64,
        wire_type: &'static str,
        cardinality: &'static str,
        identity_class: &'static str,
        reference_semantics: &'static str,
        target: Option<&'static str>,
        digest_class: Option<&'static str>,
    }
    let expected = [
        ExpectedField {
            schema: "RecoveryCheckpoint",
            name: "basis_payload_digest",
            tag: 0x0005,
            wire_type: "WeakDigest",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "weak_digest",
            target: None,
            digest_class: Some("target"),
        },
        ExpectedField {
            schema: "RecoveryCheckpoint",
            name: "basis_projection_digest",
            tag: 0x0006,
            wire_type: "digest256",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: Some("transcript"),
        },
        ExpectedField {
            schema: "RecoveryCheckpoint",
            name: "nonretaining_predecessor_checkpoint_digest",
            tag: 0x0009,
            wire_type: "digest256",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: Some("weak_identity"),
        },
        ExpectedField {
            schema: "CheckpointInstallSpec",
            name: "basis",
            tag: 0x0001,
            wire_type: "WeakStateIdentity",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: None,
        },
        ExpectedField {
            schema: "CheckpointInstallSpec",
            name: "basis_payload_digest",
            tag: 0x0004,
            wire_type: "WeakDigest",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "weak_digest",
            target: None,
            digest_class: Some("target"),
        },
        ExpectedField {
            schema: "CheckpointInstallSpec",
            name: "basis_projection_digest",
            tag: 0x0005,
            wire_type: "digest256",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: Some("transcript"),
        },
        ExpectedField {
            schema: "CheckpointInstallSpec",
            name: "checkpoint_ref",
            tag: 0x0006,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("RecoveryCheckpoint"),
            digest_class: None,
        },
        ExpectedField {
            schema: "CheckpointInstallSpec",
            name: "checkpoint_state_vector_digest",
            tag: 0x0007,
            wire_type: "digest256",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: Some("target"),
        },
        ExpectedField {
            schema: "CheckpointInstallSpec",
            name: "paired_config_payload_floor_ref",
            tag: 0x0008,
            wire_type: "StrongRef",
            cardinality: "optional",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("ConfigPayloadFloor"),
            digest_class: None,
        },
        ExpectedField {
            schema: "CheckpointInstallSpec",
            name: "retention_cut_body_ref",
            tag: 0x0009,
            wire_type: "StrongRef",
            cardinality: "optional",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("RetentionCutBody"),
            digest_class: None,
        },
        ExpectedField {
            schema: "ConstraintMutationBatch",
            name: "apply_basis",
            tag: 0x0002,
            wire_type: "WeakMarkerIdentity",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: None,
        },
        ExpectedField {
            schema: "ConstraintMutationBatch",
            name: "before_root_ref",
            tag: 0x0003,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("ConstraintStateRoot"),
            digest_class: None,
        },
        ExpectedField {
            schema: "ConstraintMutationBatch",
            name: "after_root_ref",
            tag: 0x0004,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("ConstraintStateRoot"),
            digest_class: None,
        },
        ExpectedField {
            schema: "HistoryCutActivationSpec",
            name: "provisional_cut_ref",
            tag: 0x0001,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("ProvisionalRetentionCutSet"),
            digest_class: None,
        },
        ExpectedField {
            schema: "HistoryCutActivationSpec",
            name: "checkpoint_install_record_ref",
            tag: 0x0002,
            wire_type: "StrongCommandRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("LogicalCommandRecord"),
            digest_class: None,
        },
        ExpectedField {
            schema: "HistoryCutActivationSpec",
            name: "expected_retention_map_basis",
            tag: 0x0003,
            wire_type: "WeakStateIdentity",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: None,
        },
        ExpectedField {
            schema: "InitialConfigFloorInstallSpec",
            name: "checkpoint_installed_state",
            tag: 0x0001,
            wire_type: "WeakStateIdentity",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: None,
        },
        ExpectedField {
            schema: "InitialConfigFloorInstallSpec",
            name: "checkpoint_ref",
            tag: 0x0002,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("RecoveryCheckpoint"),
            digest_class: None,
        },
        ExpectedField {
            schema: "InitialConfigFloorInstallSpec",
            name: "checkpoint_state_vector_digest",
            tag: 0x0003,
            wire_type: "digest256",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: Some("target"),
        },
        ExpectedField {
            schema: "InitialConfigFloorInstallSpec",
            name: "initial_config_payload_floor_ref",
            tag: 0x0004,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("ConfigPayloadFloor"),
            digest_class: None,
        },
        ExpectedField {
            schema: "InitialConfigFloorInstallSpec",
            name: "initial_configuration_ref",
            tag: 0x0005,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("ConfigurationState"),
            digest_class: None,
        },
        ExpectedField {
            schema: "ResourceChargeEffect",
            name: "transition_ref",
            tag: 0x0001,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("ResourceLedgerTransition"),
            digest_class: None,
        },
        ExpectedField {
            schema: "ConstraintStateRoot",
            name: "definition_set_ref",
            tag: 0x0006,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("ConstraintDefinitionSet"),
            digest_class: None,
        },
        ExpectedField {
            schema: "ResourceLedgerState",
            name: "limit_policy_ref",
            tag: 0x0006,
            wire_type: "StrongRef",
            cardinality: "one",
            identity_class: "logical",
            reference_semantics: "strong",
            target: Some("ResourceLimitPolicy"),
            digest_class: None,
        },
        ExpectedField {
            schema: "RetentionCutBody",
            name: "basis_state_identity",
            tag: 0x0002,
            wire_type: "WeakStateIdentity",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: None,
        },
        ExpectedField {
            schema: "ResourceLedgerTransition<Role:AuthorityOwningRole>",
            name: "idempotency_key_digest",
            tag: 0x0003,
            wire_type: "digest256",
            cardinality: "one",
            identity_class: "inline",
            reference_semantics: "none",
            target: None,
            digest_class: Some("weak_identity"),
        },
    ];

    assert_eq!(expected.len(), 26);
    for expected in expected {
        let fields = identity
            .fields
            .iter()
            .filter(|row| {
                row.containing_schema == expected.schema && row.stable_name == expected.name
            })
            .collect::<Vec<_>>();
        assert_eq!(
            fields.len(),
            1,
            "{}.{} must exist exactly once",
            expected.schema,
            expected.name
        );
        let field = fields[0];
        assert_eq!(field.field_tag, expected.tag);
        assert_eq!(field.exact_wire_type, expected.wire_type);
        assert_eq!(field.cardinality, expected.cardinality);
        assert_eq!(field.identity_class, expected.identity_class);
        assert_eq!(field.reference_semantics, expected.reference_semantics);
        assert_eq!(field.target_schema_id.as_deref(), expected.target);
        assert_eq!(field.digest_class.as_deref(), expected.digest_class);
        let containing_kind = identity
            .logical
            .iter()
            .find(|row| row.name == identity::generic_free_family(expected.schema))
            .expect("A12 field has a non-wire logical host");
        assert_eq!(
            field.construction_order, containing_kind.construction_order,
            "field order equals its containing kind"
        );
        assert_eq!(field.role_predicate, containing_kind.role_predicate);
        assert_eq!(field.version_status, "reserved");
        let expected_max_size = match expected.wire_type {
            "StrongRef" | "StrongCommandRef" => 40,
            "WeakDigest" | "digest256" => 32,
            _ => 16_777_216,
        };
        assert_eq!(field.max_size_bytes, expected_max_size);
        if field.reference_semantics == "none" {
            assert_eq!(
                field.identity_class, "inline",
                "every non-reference A12 field is inline"
            );
        }

        let source_key = format!(
            "field|{}|{}.{}|{}",
            expected.schema, expected.schema, expected.name, expected.name
        );
        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(
            targets.len(),
            1,
            "{}.{} must have one source target",
            expected.schema,
            expected.name
        );
        assert_eq!(targets[0].slice_id, "a12");
        assert_eq!(targets[0].target_kind, "field");
        assert_eq!(targets[0].definition_status, "declared");
    }

    let projection_digest = identity
        .fields
        .iter()
        .find(|field| {
            field.containing_schema == "CheckpointInstallSpec"
                && field.stable_name == "basis_projection_digest"
        })
        .expect("basis_projection_digest exists");
    assert_eq!(
        projection_digest.transcript_recipe.as_deref(),
        Some(
            "BLAKE3(\"fgdb:checkpoint-projection:v1\" || canonical(vector reconstruction with exact literals))"
        )
    );
    let recovery_projection_digest = identity
        .fields
        .iter()
        .find(|field| {
            field.containing_schema == "RecoveryCheckpoint"
                && field.stable_name == "basis_projection_digest"
        })
        .expect("RecoveryCheckpoint basis_projection_digest exists");
    assert_eq!(
        recovery_projection_digest.transcript_recipe.as_deref(),
        Some(
            "BLAKE3(\"fgdb:checkpoint-projection:v1\" || canonical(vector reconstruction with exact literals))"
        )
    );

    for (union_name, union_path, expected_arms) in [
        (
            "RecoveryCheckpointCommandBasis",
            "RecoveryCheckpoint.command_basis",
            [
                (
                    0x0001,
                    "Genesis",
                    "genesis",
                    "b8af076b1cc44234b812ad6e773743ec621912a23e5b1e3e0f14b7c8a9e8dd7e",
                ),
                (
                    0x0002,
                    "Ordered",
                    "ordered",
                    "27634e2b4c963a76201929e0bca78dd8cef8676d98cb34e9218695bad370ba92",
                ),
            ],
        ),
        (
            "RecoveryCheckpointMarkerBasis",
            "RecoveryCheckpoint.marker_basis",
            [
                (
                    0x0001,
                    "NoCommittedTransactionAtBasis",
                    "no_committed_transaction_at_basis",
                    "10955adc8034d5f407148ae547939a275c61bfa20b699d22fb4b87a6431f0bf1",
                ),
                (
                    0x0002,
                    "Committed",
                    "committed",
                    "6a8d314ff549e650f854b5265788584f20be6d9ace8d85c2fcfd5cd3325fa1a6",
                ),
            ],
        ),
    ] {
        let union = identity
            .ordinary_unions
            .iter()
            .find(|row| row.union_name == union_name)
            .unwrap_or_else(|| panic!("missing A12 RecoveryCheckpoint union {union_name}"));
        assert_eq!(union.containing_schema, "RecoveryCheckpoint");
        assert_eq!(union.union_path, union_path);
        assert_eq!(
            union.field_tag, None,
            "the source reader represents the union path itself, not a duplicate field anchor"
        );
        assert_eq!(union.tag_wire_type, "u8");
        assert_eq!(union.encoding_context, "closed-tagged");
        assert_eq!(
            union.allowed_containing_schemas,
            ["RecoveryCheckpoint".to_owned()]
        );
        assert_eq!(union.role_predicate, "role-local");
        assert_eq!(union.version_status, "reserved");
        assert_eq!(union.max_size_bytes, 16_777_216);
        assert_eq!(
            union
                .arms
                .iter()
                .map(|arm| {
                    (
                        arm.arm_tag,
                        arm.source_arm_name.as_str(),
                        arm.stable_name.as_str(),
                        arm.payload_sha256.as_deref().expect("payload digest"),
                    )
                })
                .collect::<Vec<_>>(),
            expected_arms,
            "RecoveryCheckpoint arm tags and names follow source order"
        );
        assert!(
            union
                .arms
                .iter()
                .all(|arm| arm.payload_kind == "inline-record"),
            "each RecoveryCheckpoint union arm has a source-committed inline record body"
        );

        for source_key in [
            format!("union|RecoveryCheckpoint|{union_path}"),
            format!("arm|RecoveryCheckpoint|{union_path}|{}", expected_arms[0].1),
            format!("arm|RecoveryCheckpoint|{union_path}|{}", expected_arms[1].1),
        ] {
            let targets = catalog
                .targets
                .iter()
                .filter(|row| row.source_key == source_key)
                .collect::<Vec<_>>();
            assert_eq!(targets.len(), 1, "{source_key} must map exactly once");
            assert_eq!(targets[0].slice_id, "a12");
            assert!(matches!(
                targets[0].target_kind.as_str(),
                "union" | "union-arm"
            ));
            assert_eq!(targets[0].definition_status, "declared");
        }
    }

    let bodyless = catalog
        .top_level_candidates
        .iter()
        .find(|row| row.source_key == "top|ConstraintStateDirectoryRoot")
        .expect("A12 bodyless definition is censused");
    assert_eq!(bodyless.source_kind, "name-only");
    assert_eq!(bodyless.identity_class, "logical");
    assert!(
        identity
            .fields
            .iter()
            .all(|field| field.containing_schema != "ConstraintStateDirectoryRoot"),
        "a definition without a structural body emits no interior field rows"
    );

    let arm_payload = catalog
        .top_level_candidates
        .iter()
        .find(|row| row.source_key == "top|InstallProvisionalCut")
        .expect("InstallProvisionalCut source token is censused");
    assert_eq!(arm_payload.source_kind, "ambiguous");
    assert_eq!(arm_payload.identity_class, "unclassified");
    assert!(
        identity
            .logical
            .iter()
            .all(|row| row.name != "InstallProvisionalCut"),
        "the source-spelled arm payload is not promoted as a standalone schema"
    );

    let mut wrong_order = identity.clone();
    wrong_order
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "CheckpointInstallSpec" && field.stable_name == "basis"
        })
        .expect("control field exists")
        .construction_order += 1;
    assert!(
        codes_without_assignment_drift(&wrong_order)
            .contains(&"field_construction_order_mismatch".to_owned()),
        "negative control: a field order different from its host must fire"
    );

    let mut wire_host = identity.clone();
    wire_host
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "CheckpointInstallSpec" && field.stable_name == "basis"
        })
        .expect("control field exists")
        .containing_schema = "WeakStateIdentity".to_owned();
    assert!(
        codes_without_assignment_drift(&wire_host).contains(&"field_unresolved_schema".to_owned()),
        "negative control: a field on a wire-only host must fire"
    );

    let mut bad_field_class = identity.clone();
    bad_field_class
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "CheckpointInstallSpec" && field.stable_name == "basis"
        })
        .expect("control field exists")
        .identity_class = "wire".to_owned();
    assert!(
        codes_without_assignment_drift(&bad_field_class)
            .contains(&"field_identity_class_not_a_field_class".to_owned()),
        "negative control: wire is not one of the five field identity classes"
    );

    let mut invented_digest_wire = identity.clone();
    invented_digest_wire
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "CheckpointInstallSpec"
                && field.stable_name == "checkpoint_state_vector_digest"
        })
        .expect("control field exists")
        .exact_wire_type = "CheckpointStateVectorDigest".to_owned();
    assert!(
        codes_without_assignment_drift(&invented_digest_wire)
            .contains(&"field_unresolved_wire_type".to_owned()),
        "negative control: a plan-named digest must not become a wire type"
    );

    let mut invented_target = identity.clone();
    invented_target
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "CheckpointInstallSpec"
                && field.stable_name == "checkpoint_ref"
        })
        .expect("control field exists")
        .target_schema_id = Some("FutureRecoveryCheckpoint".to_owned());
    assert!(
        codes_without_assignment_drift(&invented_target)
            .contains(&"ref_target_unresolved".to_owned()),
        "negative control: a fabricated StrongRef target must fire"
    );

    let mut future_checkpoint_state = identity.clone();
    let future_field = future_checkpoint_state
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "RecoveryCheckpoint"
                && field.stable_name == "basis_payload_digest"
        })
        .expect("RecoveryCheckpoint control field exists");
    future_field.stable_name = "checkpoint_state_vector_ref".to_owned();
    future_field.field_tag = 0x0008;
    future_field.exact_wire_type = "StrongRef".to_owned();
    future_field.identity_class = "logical".to_owned();
    future_field.reference_semantics = "strong".to_owned();
    future_field.target_schema_id = Some("CheckpointStateVector".to_owned());
    future_field.max_size_bytes = 40;
    future_field.digest_class = None;
    future_field.transcript_recipe = None;
    assert!(
        codes_without_assignment_drift(&future_checkpoint_state)
            .contains(&"dag_future_result".to_owned()),
        "negative control: RecoveryCheckpoint@30 cannot StrongRef CheckpointStateVector@35"
    );

    catalog
        .reservations
        .iter_mut()
        .find(|row| row.symbol == "RecoveryCheckpoint")
        .expect("RecoveryCheckpoint reservation exists")
        .code_reservation = "0x03ab".to_owned();
    assert!(
        appendix_a::validate_catalog(&catalog)
            .iter()
            .any(|violation| violation.code == "catalog_reservation_existing_mismatch"),
        "negative control: an existing reservation must mint at its exact code"
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
    assert_eq!(fields.len(), 114);
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
fn idr_j00a_predecessors_are_nonretaining_and_current_generations_stay_owned() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();
    let expected = [
        (
            "TxnOutcomeRecord",
            0x0003,
            18,
            "reserved",
            "a03",
            "WeakDigest",
            "logical",
            "weak_digest",
        ),
        (
            "MetaPreparedCommandRecord",
            0x0003,
            35,
            "reserved",
            "a06",
            "WeakDigest",
            "logical",
            "weak_digest",
        ),
        (
            "ShardPreparedPayloadRecord",
            0x0003,
            35,
            "reserved",
            "a06",
            "WeakDigest",
            "logical",
            "weak_digest",
        ),
        (
            "PreparedCommitRecord",
            0x0004,
            12,
            "active",
            "a10",
            "WeakDigest",
            "logical",
            "weak_digest",
        ),
        (
            "TimeSubjectIssuanceReservation<Role>",
            0x0004,
            19,
            "reserved",
            "a16",
            "digest256",
            "inline",
            "none",
        ),
    ];
    for (schema, tag, order, status, slice, wire_type, identity_class, reference_semantics) in
        expected
    {
        let field = identity
            .fields
            .iter()
            .find(|field| {
                field.containing_schema == schema
                    && field.stable_name == "nonretaining_predecessor_digest"
            })
            .unwrap_or_else(|| panic!("{schema} nonretaining predecessor field exists"));
        assert_eq!(field.field_tag, tag);
        assert_eq!(field.exact_wire_type, wire_type);
        assert_eq!(field.cardinality, "one");
        assert_eq!(field.identity_class, identity_class);
        assert_eq!(field.reference_semantics, reference_semantics);
        assert_eq!(field.target_schema_id, None);
        assert_eq!(field.construction_order, order);
        assert_eq!(field.version_status, status);
        assert_eq!(field.max_size_bytes, 32);
        assert_eq!(field.digest_class.as_deref(), Some("weak_identity"));
        assert!(field.retention_and_cut_rule.contains("comparison-only"));
        assert!(field.retention_and_cut_rule.contains("never traversed"));

        let source_key = format!(
            "field|{schema}|{schema}.nonretaining_predecessor_digest|nonretaining_predecessor_digest"
        );
        let target = catalog
            .targets
            .iter()
            .find(|target| target.source_key == source_key)
            .unwrap_or_else(|| panic!("{schema} source field has one catalog target"));
        assert_eq!(target.slice_id, slice);
        assert_eq!(target.target_kind, "field");
    }

    let plan = String::from_utf8(real_plan_source()).expect("plan is UTF-8");
    for old in [
        "predecessor_ref:StrongRef<TxnOutcomeRecord>?",
        "predecessor_ref:StrongRef<MetaPreparedCommandRecord>?",
        "predecessor_ref:StrongRef<ShardPreparedPayloadRecord>?",
        "predecessor_ref:StrongRef<PreparedCommitRecord>?",
        "predecessor_ref:StrongRef<TimeSubjectIssuanceReservation<Role>>?",
    ] {
        assert!(!plan.contains(old), "retaining predecessor survived: {old}");
    }
    for owner in [
        "StrongRef<TxnOutcomeRecord>",
        "StrongRef<MetaPreparedCommandRecord>",
        "StrongRef<ShardPreparedPayloadRecord>",
        "StrongRef<PreparedCommitRecord>",
        "StrongRef<TimeSubjectIssuanceReservation<Role>>",
    ] {
        assert!(
            plan.contains(owner),
            "current-generation owner is not stated: {owner}"
        );
    }
}

#[test]
fn idr_oicl_cycle_backlinks_are_nonretaining_target_digests() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();
    let expected = [
        (
            "GlobalTxnOutcomePreparationRecord",
            "expected_registered_outcome_digest",
            0x0007,
            60,
            "a07",
            "GlobalTxnOutcomePreparationRecord.expected_registered_outcome_digest",
        ),
        (
            "NoTerminalSignatureOrOrderProof",
            "freeze_digest",
            0x0002,
            6,
            "a08",
            "NoTerminalSignatureOrOrderProof.freeze_digest",
        ),
        (
            "KeyEnvelopeNode",
            "source_root_digest",
            0x0016,
            50,
            "a13",
            "KeyEnvelopeNode.inherited_roots.record.source_root_digest",
        ),
        (
            "KeyEnvelopeNode",
            "source_root_ciphertext_digest",
            0x0017,
            50,
            "a13",
            "KeyEnvelopeNode.inherited_roots.record.source_root_ciphertext_digest",
        ),
    ];

    for (schema, stable_name, field_tag, construction_order, slice, source_path) in expected {
        let row = identity
            .fields
            .iter()
            .find(|row| row.containing_schema == schema && row.stable_name == stable_name)
            .unwrap_or_else(|| panic!("{schema}.{stable_name} exists"));
        assert_eq!(row.field_tag, field_tag);
        assert_eq!(row.exact_wire_type, "digest256");
        assert_eq!(row.cardinality, "one");
        assert_eq!(row.identity_class, "inline");
        assert_eq!(row.reference_semantics, "none");
        assert_eq!(row.target_schema_id, None);
        assert_eq!(row.construction_order, construction_order);
        assert_eq!(row.version_status, "reserved");
        assert_eq!(row.max_size_bytes, 32);
        assert_eq!(row.digest_class.as_deref(), Some("target"));
        assert!(row.retention_and_cut_rule.contains("comparison-only"));
        assert!(row.retention_and_cut_rule.contains("never traversed"));

        let source_key = format!("field|{schema}|{source_path}|{stable_name}");
        let target = catalog
            .targets
            .iter()
            .find(|target| target.source_key == source_key)
            .unwrap_or_else(|| panic!("{schema}.{stable_name} has one source target"));
        assert_eq!(target.slice_id, slice);
        assert_eq!(target.target_kind, "field");
        assert_eq!(target.definition_status, "declared");
    }

    let plan = String::from_utf8(real_plan_source()).expect("plan is UTF-8");
    for old_retaining_spelling in [
        "expected_registered_outcome_ref:StrongRef<GlobalTxnOutcomeRecord>",
        "SameGroupCertificateHeader,freeze_ref:StrongRef<AuditTerminalFreezeRecord>",
        "source_root_ref:StrongRef<KeyEnvelopeRoot>",
        "source_root_ciphertext_ref:StrongCiphertextRef<KeyEnvelopeRoot>",
    ] {
        assert!(
            !plan.contains(old_retaining_spelling),
            "retaining cycle back-link survived: {old_retaining_spelling}"
        );
    }

    let mut missing_digest_class = identity;
    missing_digest_class
        .fields
        .iter_mut()
        .find(|row| {
            row.containing_schema == "NoTerminalSignatureOrOrderProof"
                && row.stable_name == "freeze_digest"
        })
        .expect("freeze digest exists")
        .digest_class = None;
    assert!(
        codes(&missing_digest_class).contains(&"digest_missing_class".to_owned()),
        "control must reject a plan-named digest without its target digest class"
    );
}

#[test]
fn idr_a16_authority_bound_headers_are_source_ordered_inline_values() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();
    // The subject of this test is that `authority_bound_header` is an INLINE value
    // (no reference semantics, no target schema, source-ordered tag); the order is
    // row data carried alongside, and a field row's order must equal its containing
    // kind's. Two witnesses move under fgdb-oicl:
    //   RestoreSourceLeaseRecord           45 -> 30. Four referrers cap it at 30
    //     (RecoveryBridgeSourceLeaseBasis, RestoreLeaseState,
    //      RestoreLeaseReleaseEligibility, RestoreSourceLeaseAuthorityObservationImport)
    //     against a floor of 27, so 45 sat 15 above its own ceiling.
    //   TimeAuthorityEpochTransitionRecord 60 -> 30, inside the a16 time-authority
    //     component that collapses to 30.
    let expected = [
        ("ProtectedErrorReplayTimeBasis<Role>", 0x0009, 17),
        (
            "MacaroonRootIssuanceRecord<Role:AuthorityOwningRole>",
            0x0001,
            30,
        ),
        (
            "RestoreSourceLeaseRecord<Role:AuthorityOwningRole>",
            0x0001,
            30,
        ),
        (
            "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
            0x0003,
            30,
        ),
    ];
    for (schema, tag, construction_order) in expected {
        let field = identity
            .fields
            .iter()
            .find(|field| {
                field.containing_schema == schema && field.stable_name == "authority_bound_header"
            })
            .unwrap_or_else(|| panic!("{schema}.authority_bound_header exists"));
        assert_eq!(field.field_tag, tag);
        assert_eq!(field.exact_wire_type, "AuthorityBoundHeader");
        assert_eq!(field.cardinality, "one");
        assert_eq!(field.identity_class, "inline");
        assert_eq!(field.reference_semantics, "none");
        assert_eq!(field.target_schema_id, None);
        assert_eq!(field.construction_order, construction_order);
        assert_eq!(field.version_status, "reserved");
        assert_eq!(field.max_size_bytes, 256);
        let source_key =
            format!("field|{schema}|{schema}.authority_bound_header|authority_bound_header");
        let target = catalog
            .targets
            .iter()
            .find(|target| target.source_key == source_key)
            .unwrap_or_else(|| panic!("{schema}.authority_bound_header has a source target"));
        assert_eq!(target.slice_id, "a16");
        assert_eq!(target.target_kind, "field");
        assert_eq!(target.definition_status, "declared");
    }
}

#[test]
fn idr_a16_generic_strong_targets_use_registered_family_symbols() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();
    // The fourth column is the CONTAINING kind's construction_order, which a field
    // row must equal by law (`field_construction_order_mismatch`). fgdb-oicl
    // collapsed the a16 time-authority component to 30 -- TimeAuthorityRegistry's
    // own window was empty in both directions at [65..23], and 30 is the
    // minimum-churn consistent value for the component -- so every witness whose
    // CONTAINING kind is in that component moves to 30. Entries whose containing
    // kind did not move (ShardTimeBoundSubjectRetirementProof@36,
    // TimeBoundSubjectRetirementProof@61) are deliberately untouched.
    let expected = [
        (
            "ShardTimeBoundSubjectInventoryCertificate",
            "inventory_ref",
            0x0008,
            30,
            "TimeBoundSubjectInventory",
        ),
        (
            "ShardTimeBoundSubjectRetirementProof",
            "shard_inventory_ref",
            0x0002,
            36,
            "TimeBoundSubjectInventory",
        ),
        (
            "TimeAuthorityDrainHold<Role:AuthorityOwningRole>",
            "inventory_closure_ref",
            0x0004,
            30,
            "RoleTimeBoundSubjectInventoryClosure",
        ),
        (
            "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
            "subject_inventory_closure_ref",
            0x000c,
            30,
            "RoleTimeBoundSubjectInventoryClosure",
        ),
        (
            "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
            "drain_hold_ref",
            0x000d,
            30,
            "TimeAuthorityDrainHold",
        ),
        (
            "TimeBoundSubjectInventoryProof<Role>",
            "inventory_ref",
            0x0001,
            30,
            "TimeBoundSubjectInventory",
        ),
        (
            "TimeBoundSubjectRetirementProof<Role:AuthorityOwningRole>",
            "transition_record_ref",
            0x0001,
            61,
            "TimeAuthorityEpochTransitionRecord",
        ),
        (
            "TimeBoundSubjectRetirementProof<Role:AuthorityOwningRole>",
            "current_inventory_ref",
            0x0002,
            61,
            "TimeBoundSubjectInventory",
        ),
    ];
    for (schema, stable_name, field_tag, construction_order, target_schema_id) in expected {
        let row = identity
            .fields
            .iter()
            .find(|field| field.containing_schema == schema && field.stable_name == stable_name)
            .unwrap_or_else(|| panic!("{schema}.{stable_name} exists"));
        assert_eq!(row.field_tag, field_tag);
        assert_eq!(row.exact_wire_type, "StrongRef");
        assert_eq!(row.cardinality, "one");
        assert_eq!(row.identity_class, "logical");
        assert_eq!(row.reference_semantics, "strong");
        assert_eq!(row.target_schema_id.as_deref(), Some(target_schema_id));
        assert_eq!(row.construction_order, construction_order);
        assert_eq!(row.version_status, "reserved");
        assert_eq!(row.max_size_bytes, 40);

        let source_key = format!("field|{schema}|{schema}.{stable_name}|{stable_name}");
        let target = catalog
            .targets
            .iter()
            .find(|target| target.source_key == source_key)
            .unwrap_or_else(|| panic!("{schema}.{stable_name} has one source target"));
        assert_eq!(target.slice_id, "a16");
        assert_eq!(target.target_kind, "field");
        assert_eq!(target.definition_status, "declared");
    }

    let mut signed_target = identity;
    signed_target
        .fields
        .iter_mut()
        .find(|field| {
            field.containing_schema == "ShardTimeBoundSubjectInventoryCertificate"
                && field.stable_name == "inventory_ref"
        })
        .expect("inventory_ref exists")
        .target_schema_id = Some("TimeBoundSubjectInventory<Shard>".to_owned());
    assert!(
        codes(&signed_target).contains(&"ref_target_unresolved".to_owned()),
        "a source-signed instantiation must not replace the registered family identity"
    );
}

#[test]
fn idr_a16_logical_union_consumers_have_exact_self_rooted_closures() {
    let identity = real_identity();
    let expected = [
        (
            "ContinuityAuthorityCurrentBasis<Role>",
            "ContinuityAuthorityObservationImport<Role>",
            "current_basis",
            0x0002,
            56,
        ),
        (
            "ShardRestoreSourceLeaseProjectionSource",
            "ShardRestoreSourceLeaseProjection",
            "source",
            0x0001,
            50,
        ),
        (
            // 60 -> 30 (fgdb-oicl): the consumer TimeAuthorityEpochTransitionRecord
            // is inside the a16 time-authority component that collapses to 30, and
            // a field row's order must equal its containing kind's. The other two
            // consumers did not move and keep their witnesses.
            "RoleTimeAuthorityDrainFloorSet<Role>",
            "TimeAuthorityEpochTransitionRecord<Role:AuthorityOwningRole>",
            "drain_floor_set",
            0x000f,
            30,
        ),
    ];
    for (union_name, consumer, stable_name, field_tag, construction_order) in expected {
        let union = identity
            .ordinary_unions
            .iter()
            .find(|union| union.union_name == union_name)
            .unwrap_or_else(|| panic!("{union_name} exists"));
        assert_eq!(
            union.allowed_containing_schemas,
            vec![union_name.to_owned(), consumer.to_owned()]
        );
        let row = identity
            .fields
            .iter()
            .find(|field| field.containing_schema == consumer && field.stable_name == stable_name)
            .unwrap_or_else(|| panic!("{consumer}.{stable_name} exists"));
        assert_eq!(row.field_tag, field_tag);
        assert_eq!(row.exact_wire_type, union_name);
        assert_eq!(row.cardinality, "one");
        assert_eq!(row.identity_class, "inline");
        assert_eq!(row.reference_semantics, "none");
        assert_eq!(row.target_schema_id, None);
        assert_eq!(row.construction_order, construction_order);
        assert_eq!(row.version_status, "reserved");
        assert_eq!(row.max_size_bytes, 16_777_216);
    }

    let mut missing_consumer = identity.clone();
    missing_consumer
        .ordinary_unions
        .iter_mut()
        .find(|union| union.union_name == "ContinuityAuthorityCurrentBasis<Role>")
        .expect("current-basis union exists")
        .allowed_containing_schemas
        .pop();
    let missing_codes = codes(&missing_consumer);
    assert!(
        missing_codes.contains(&"ordinary_union_logical_contract_mismatch".to_owned())
            && missing_codes.contains(&"ordinary_union_field_mismatch".to_owned()),
        "an actual inline consumer omitted from the closure must fail: {missing_codes:?}"
    );

    let mut unrelated_consumer = identity;
    unrelated_consumer
        .ordinary_unions
        .iter_mut()
        .find(|union| union.union_name == "ContinuityAuthorityCurrentBasis<Role>")
        .expect("current-basis union exists")
        .allowed_containing_schemas
        .push("RootManifest".to_owned());
    assert!(
        codes(&unrelated_consumer).contains(&"ordinary_union_logical_contract_mismatch".to_owned()),
        "an unrelated schema without a matching inline field must not enter the exact closure"
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
fn idr_neg_self_edge_through_generic_family() {
    // j00a corrected the real lineage members to nonretaining digests. Keep the
    // rejected alternative mutation-proven: a field row can still name
    // `Foo<Role>` as its containing schema while the kind is registered under
    // the bare family, and the DAG law must normalize that owner before testing
    // a hypothetical retaining self-edge.
    let mut r = real_identity();
    let mut f = field(
        "TimeSubjectIssuanceReservation<Role>",
        90,
        "predecessor_ref",
        19,
    );
    f.target_schema_id = Some("TimeSubjectIssuanceReservation".into());
    r.fields.push(f);
    let codes = codes(&r);
    assert!(
        codes.contains(&"dag_self_edge".to_string()),
        "a generic-signed containing schema must not hide a self-edge, got {codes:?}"
    );
}

#[test]
fn idr_neg_generic_target_cannot_create_an_unchecked_edge() {
    // The mirror end of the same hazard. A generic-signed *target* is closed by
    // a different law -- `ref_target_unresolved` -- because target_schema_id is
    // resolved by exact name. Asserted here so the DAG law above is known to
    // need normalization on the containing side only: no generic target can
    // reach the DAG loop and quietly contribute an unchecked edge.
    let mut r = real_identity();
    let mut f = field("PreparedCommitRecord", 90, "predecessor_ref", 12);
    f.target_schema_id = Some("PreparedCommitRecord<Local>".into());
    r.fields.push(f);
    let codes = codes(&r);
    assert!(
        codes.contains(&"ref_target_unresolved".to_string()),
        "a generic-signed target must be refused outright, got {codes:?}"
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
        // A field's containing schema resolves by generic-free family, so the
        // FAMILY row is what a generic-signed field row keeps alive — the same
        // treatment ordinary-union containers get below.  `target_schema_id`
        // stays exact: reference-target resolution has no family law.
        load_bearing.insert(identity::generic_free_family(f.containing_schema.as_str()));
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
    // An ordinary union's containing schema is load-bearing too: removing it
    // orphans the union (and, for a whole-schema role union, its logical
    // parent contract).  Resolution is by generic-free family, so the family
    // row is what the union keeps alive.
    for u in &r.ordinary_unions {
        load_bearing.insert(identity::generic_free_family(u.containing_schema.as_str()));
    }
    // Exhaustive single-removal property over every logical kind.
    for victim in r.logical.iter().map(|k| k.name.clone()).collect::<Vec<_>>() {
        let mut mutated = r.clone();
        mutated.logical.retain(|k| k.name != victim);
        let violations = identity::validate_identity(&mutated);
        let resolution_fault = violations.iter().any(|v| {
            matches!(
                v.code.as_str(),
                "union_arm_unresolved"
                    | "ref_target_unresolved"
                    | "field_unresolved_schema"
                    | "ordinary_union_unresolved_schema"
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

/// fgdb-tfow: structural source keys are matched by reconstruction from the
/// typed catalog row, never by splitting the key on `|`.  The separator is also
/// legal inside a generic signature, so `TimeBoundSubjectInventory<Role:Local|
/// Meta|Shard>` is the owner that broke the old fixed-arity parse.
#[test]
fn appendix_a_pipe_bearing_owner_keys_match_by_reconstruction() {
    const PIPE_OWNER: &str = "TimeBoundSubjectInventory<Role:Local|Meta|Shard>";
    const UNION_TARGET: &str =
        "a16:union:time-bound-online-macaroon-root-projection-75ece215ec471511";
    const ARM_TARGET: &str =
        "a16:union-arm:time-bound-online-macaroon-root-projection-local-0a5f193e5c5aaaf4";
    const FIELD_TARGET: &str =
        "a16:field:time-bound-subject-inventory-role-local-meta-shard-online-macaroon-roots";

    // 1. The exact rows verify.  The owner's generic signature carries two
    //    interior pipes, so this is precisely the shape the old parse rejected.
    let baseline = real_appendix_catalog();
    let union_key = baseline
        .targets
        .iter()
        .find(|target| target.target_row_id == UNION_TARGET)
        .expect("pipe-bearing union target exists")
        .source_key
        .clone();
    assert_eq!(
        union_key.matches('|').count(),
        2 + 4,
        "the fixture must keep both the three key separators and the owner's interior pipes"
    );
    assert!(
        !appendix_a::appendix_a_catalog_closure(&baseline)
            .iter()
            .any(|violation| violation.code == "catalog_target_source_identity_mismatch"),
        "an exact pipe-bearing structural row must pass"
    );

    // 2. Altering ANY reconstructed component fails: the owner, the union path,
    //    the arm token, and the field's stable name.
    for (target_row_id, tampered) in [
        (
            UNION_TARGET,
            format!("union|{PIPE_OWNER}|{PIPE_OWNER}.online_macaroon_root"),
        ),
        (
            UNION_TARGET,
            format!(
                "union|TimeBoundSubjectInventory<Role:Local|Meta>|{PIPE_OWNER}.online_macaroon_roots"
            ),
        ),
        (
            ARM_TARGET,
            format!("arm|{PIPE_OWNER}|{PIPE_OWNER}.online_macaroon_roots|Meta"),
        ),
        (
            FIELD_TARGET,
            format!("field|{PIPE_OWNER}|{PIPE_OWNER}.online_macaroon_roots|online_macaroon_root"),
        ),
    ] {
        let mut tampered_catalog = real_appendix_catalog();
        tampered_catalog
            .targets
            .iter_mut()
            .find(|target| target.target_row_id == target_row_id)
            .expect("pipe-bearing structural target exists")
            .source_key = tampered.clone();
        assert!(
            appendix_a::appendix_a_catalog_closure(&tampered_catalog)
                .iter()
                .any(|violation| violation.code == "catalog_target_source_identity_mismatch"),
            "a tampered component escaped reconstruction: {tampered}"
        );
    }

    // 3. A key that re-segments to the SAME `split('|')` parts as the legal key
    //    must still fail: only byte equality against the reconstruction passes,
    //    so swapping the owner and path segments cannot masquerade as exact.
    let mut resegmented = real_appendix_catalog();
    resegmented
        .targets
        .iter_mut()
        .find(|target| target.target_row_id == UNION_TARGET)
        .expect("pipe-bearing union target exists")
        .source_key = format!("union|{PIPE_OWNER}.online_macaroon_roots|{PIPE_OWNER}");
    assert_eq!(
        resegmented
            .targets
            .iter()
            .find(|target| target.target_row_id == UNION_TARGET)
            .expect("target")
            .source_key
            .matches('|')
            .count(),
        union_key.matches('|').count(),
        "the re-segmented key must keep the same pipe count as the legal key"
    );
    assert!(
        appendix_a::appendix_a_catalog_closure(&resegmented)
            .iter()
            .any(|violation| violation.code == "catalog_target_source_identity_mismatch"),
        "a re-segmented key masqueraded as the exact source"
    );
}

#[test]
fn idr_configuration_state_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "ConfigurationState")
        .expect("ConfigurationState logical shell exists");
    assert_eq!(logical.object_kind, 0x0278);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 5);
    // Role-POLYMORPHIC, not role-scoped: a04:1558 carries
    // `group_role:Local|Meta|Shard{shard_id}` inside the schema itself, so the
    // kind is legal in every posture and must not be narrowed to one role.
    assert_eq!(logical.role_predicate, "true");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(logical.golden_corpus, "corpus/logical/configuration_state/");

    // The load-bearing property, and the reason this kind gets its own lock.
    // ConfigurationState has ZERO outbound strong references and 76 inbound
    // `StrongRef<ConfigurationState>` sites across the plan, so it must be
    // constructible before anything that cites it. `dag_future_result` only
    // catches a violation once a consumer field row exists; pinning the global
    // minimum here makes an ordering drift fail immediately and attributably,
    // rather than surfacing later as someone else's unexplained DAG failure.
    let minimum = identity
        .logical
        .iter()
        .map(|kind| kind.construction_order)
        .min()
        .expect("logical registry is non-empty");
    assert_eq!(
        logical.construction_order, minimum,
        "ConfigurationState must remain the unique construction-order floor"
    );
    assert!(
        identity
            .logical
            .iter()
            .filter(|kind| kind.construction_order == minimum)
            .count()
            == 1,
        "no other logical kind may share the floor with ConfigurationState"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "ConfigurationState")
        .expect("ConfigurationState permanent reservation exists");
    assert_eq!(reservation.row_id, "a04:reservation:configuration-state");
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x0278");
    assert_eq!(reservation.disposition, "existing");

    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == "top|ConfigurationState")
        .expect("ConfigurationState source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == "top|ConfigurationState")
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a04:target:logical-kind-configuration-state"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a04:logical-kind:configuration-state"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");

    // The shell must not outrun its field types: a04:1558 members are not yet
    // registered, and no consumer field row may exist before they are.
    assert!(
        !identity
            .fields
            .iter()
            .any(|field| field.containing_schema == "ConfigurationState"),
        "the shell must not outrun its unresolved field types"
    );
}

#[test]
fn idr_portable_restore_archive_acquisition_receipt_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "PortableRestoreArchiveAcquisitionReceipt")
        .expect("PortableRestoreArchiveAcquisitionReceipt logical shell exists");
    assert_eq!(logical.object_kind, 0x038a);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 6);
    assert_eq!(logical.role_predicate, "true");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(
        logical.golden_corpus,
        "corpus/logical/portable_restore_archive_acquisition_receipt/"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "PortableRestoreArchiveAcquisitionReceipt")
        .expect("PortableRestoreArchiveAcquisitionReceipt permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:portable-restore-archive-acquisition-receipt"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x038a");
    assert_eq!(reservation.disposition, "existing");

    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == "top|PortableRestoreArchiveAcquisitionReceipt")
        .expect("PortableRestoreArchiveAcquisitionReceipt source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == "top|PortableRestoreArchiveAcquisitionReceipt")
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-portable-restore-archive-acquisition-receipt"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:portable-restore-archive-acquisition-receipt"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity
            .fields
            .iter()
            .any(|field| { field.containing_schema == "PortableRestoreArchiveAcquisitionReceipt" }),
        "the shell increment must not preempt its receipt-field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "PortableRestoreArchiveAcquisitionReceipt"
                || union.union_name == "PortableRestoreArchiveAcquisitionReceipt"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "PortableRestoreArchiveAcquisitionReceipt"
                || union.union_name == "PortableRestoreArchiveAcquisitionReceipt"
        }),
        "the shell increment must not preempt receipt unions or arms"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|PortableRestoreArchiveAcquisitionReceipt|")
                || row.resolved_source_keys.iter().any(|source_key| {
                    source_key.contains("|PortableRestoreArchiveAcquisitionReceipt|")
                })
        }),
        "receipt shorthand ambiguities must remain open until exact field types are settled"
    );

    let target_row_id = "a17:logical-kind:portable-restore-archive-acquisition-receipt";
    let a17 = catalog
        .slices
        .iter()
        .find(|slice| slice.id == "a17")
        .expect("a17 slice exists");
    assert_eq!(
        a17.definition_status, "declared",
        "coverage must close before completion-layer authoring"
    );
    assert!(
        !catalog
            .annotations
            .iter()
            .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .semantic_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .expansion_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .evidence
                .iter()
                .any(|row| row.target_row_id == target_row_id),
        "a declared shell must not skip coverage-first sequencing with premature completion rows"
    );
}

#[test]
fn idr_canonical_restore_plan_availability_copy_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "CanonicalRestorePlanAvailabilityCopy")
        .expect("CanonicalRestorePlanAvailabilityCopy logical shell exists");
    assert_eq!(logical.object_kind, 0x025d);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 6);
    assert_eq!(logical.role_predicate, "true");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(
        logical.golden_corpus,
        "corpus/logical/canonical_restore_plan_availability_copy/"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "CanonicalRestorePlanAvailabilityCopy")
        .expect("CanonicalRestorePlanAvailabilityCopy permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:canonical-restore-plan-availability-copy"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x025d");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|CanonicalRestorePlanAvailabilityCopy<Role:AuthorityOwningRole>";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == source_key)
        .expect("CanonicalRestorePlanAvailabilityCopy source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-canonical-restore-plan-availability-copy"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:canonical-restore-plan-availability-copy"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity
            .fields
            .iter()
            .any(|field| { field.containing_schema == "CanonicalRestorePlanAvailabilityCopy" }),
        "the shell increment must not preempt its field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "CanonicalRestorePlanAvailabilityCopy"
                || union.union_name == "CanonicalRestorePlanAvailabilityCopy"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "CanonicalRestorePlanAvailabilityCopy"
                || union.union_name == "CanonicalRestorePlanAvailabilityCopy"
        }),
        "the shell increment must not preempt unions or arms"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|CanonicalRestorePlanAvailabilityCopy|")
                || row
                    .resolved_source_keys
                    .iter()
                    .any(|source_key| source_key.contains("|CanonicalRestorePlanAvailabilityCopy|"))
        }),
        "shorthand ambiguities must remain open until exact field types are settled"
    );

    let target_row_id = "a17:logical-kind:canonical-restore-plan-availability-copy";
    let a17 = catalog
        .slices
        .iter()
        .find(|slice| slice.id == "a17")
        .expect("a17 slice exists");
    assert_eq!(
        a17.definition_status, "declared",
        "coverage must close before completion-layer authoring"
    );
    assert!(
        !catalog
            .annotations
            .iter()
            .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .semantic_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .expansion_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .evidence
                .iter()
                .any(|row| row.target_row_id == target_row_id),
        "a declared shell must not skip coverage-first sequencing with premature completion rows"
    );
}

#[test]
fn idr_canonical_restore_source_acquisition_plan_copy_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "CanonicalRestoreSourceAcquisitionPlanCopy")
        .expect("CanonicalRestoreSourceAcquisitionPlanCopy logical shell exists");
    assert_eq!(logical.object_kind, 0x025e);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 7);
    assert_eq!(logical.role_predicate, "true");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(
        logical.golden_corpus,
        "corpus/logical/canonical_restore_source_acquisition_plan_copy/"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "CanonicalRestoreSourceAcquisitionPlanCopy")
        .expect("CanonicalRestoreSourceAcquisitionPlanCopy permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:canonical-restore-source-acquisition-plan-copy"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x025e");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|CanonicalRestoreSourceAcquisitionPlanCopy<Role>";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == source_key)
        .expect("CanonicalRestoreSourceAcquisitionPlanCopy source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-canonical-restore-source-acquisition-plan-copy"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:canonical-restore-source-acquisition-plan-copy"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity.fields.iter().any(|field| {
            field.containing_schema == "CanonicalRestoreSourceAcquisitionPlanCopy"
        }),
        "the shell increment must not preempt its field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "CanonicalRestoreSourceAcquisitionPlanCopy"
                || union.union_name == "CanonicalRestoreSourceAcquisitionPlanCopy"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "CanonicalRestoreSourceAcquisitionPlanCopy"
                || union.union_name == "CanonicalRestoreSourceAcquisitionPlanCopy"
        }),
        "the shell increment must not preempt unions or arms"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|CanonicalRestoreSourceAcquisitionPlanCopy|")
                || row.resolved_source_keys.iter().any(|source_key| {
                    source_key.contains("|CanonicalRestoreSourceAcquisitionPlanCopy|")
                })
        }),
        "shorthand ambiguities must remain open until exact field types are settled"
    );

    let target_row_id = "a17:logical-kind:canonical-restore-source-acquisition-plan-copy";
    let a17 = catalog
        .slices
        .iter()
        .find(|slice| slice.id == "a17")
        .expect("a17 slice exists");
    assert_eq!(
        a17.definition_status, "declared",
        "coverage must close before completion-layer authoring"
    );
    assert!(
        !catalog
            .annotations
            .iter()
            .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .semantic_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .expansion_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .evidence
                .iter()
                .any(|row| row.target_row_id == target_row_id),
        "a declared shell must not skip coverage-first sequencing with premature completion rows"
    );
}

#[test]
fn idr_sealed_prebootstrap_dispatch_import_reserves_its_source_dag_stratum() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "SealedPreBootstrapDispatchJournalImport")
        .expect("SealedPreBootstrapDispatchJournalImport logical shell exists");
    assert_eq!(logical.object_kind, 0x042e);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 10);
    assert_eq!(logical.role_predicate, "true");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(
        logical.golden_corpus,
        "corpus/logical/sealed_pre_bootstrap_dispatch_journal_import/"
    );

    let plan_copy = identity
        .logical
        .iter()
        .find(|logical| logical.name == "CanonicalRestoreSourceAcquisitionPlanCopy")
        .expect("CanonicalRestoreSourceAcquisitionPlanCopy predecessor exists");
    assert_eq!(
        logical.construction_order,
        plan_copy.construction_order + 3,
        "the two generated refinement-copy reference strata separate the plan copy from the sealed import"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "SealedPreBootstrapDispatchJournalImport")
        .expect("SealedPreBootstrapDispatchJournalImport permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:sealed-pre-bootstrap-dispatch-journal-import"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x042e");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|SealedPreBootstrapDispatchJournalImport<Role>";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == source_key)
        .expect("SealedPreBootstrapDispatchJournalImport source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-sealed-pre-bootstrap-dispatch-journal-import"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:sealed-pre-bootstrap-dispatch-journal-import"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity
            .fields
            .iter()
            .any(|field| { field.containing_schema == "SealedPreBootstrapDispatchJournalImport" }),
        "the shell increment must not preempt its generated-reference field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "SealedPreBootstrapDispatchJournalImport"
                || union.union_name == "SealedPreBootstrapDispatchJournalImport"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "SealedPreBootstrapDispatchJournalImport"
                || union.union_name == "SealedPreBootstrapDispatchJournalImport"
        }),
        "the record shell must not manufacture a same-name union or any arms"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|SealedPreBootstrapDispatchJournalImport|")
                || row.resolved_source_keys.iter().any(|source_key| {
                    source_key.contains("|SealedPreBootstrapDispatchJournalImport|")
                })
        }),
        "shorthand ambiguities must remain open until exact field types are settled"
    );

    let target_row_id = "a17:logical-kind:sealed-pre-bootstrap-dispatch-journal-import";
    let a17 = catalog
        .slices
        .iter()
        .find(|slice| slice.id == "a17")
        .expect("a17 slice exists");
    assert_eq!(
        a17.definition_status, "declared",
        "coverage must close before completion-layer authoring"
    );
    assert!(
        !catalog
            .annotations
            .iter()
            .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .semantic_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .expansion_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .evidence
                .iter()
                .any(|row| row.target_row_id == target_row_id),
        "a declared shell must not skip coverage-first sequencing with premature completion rows"
    );
}

#[test]
fn idr_restore_source_acquisition_plan_import_follows_the_sealed_import() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "RestoreSourceAcquisitionPlanImportRecord")
        .expect("RestoreSourceAcquisitionPlanImportRecord logical shell exists");
    assert_eq!(logical.object_kind, 0x03f0);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 11);
    assert_eq!(logical.role_predicate, "true");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(
        logical.golden_corpus,
        "corpus/logical/restore_source_acquisition_plan_import_record/"
    );

    let sealed_import = identity
        .logical
        .iter()
        .find(|logical| logical.name == "SealedPreBootstrapDispatchJournalImport")
        .expect("SealedPreBootstrapDispatchJournalImport predecessor exists");
    assert_eq!(
        logical.construction_order,
        sealed_import.construction_order + 1,
        "the plan import strongly retains the sealed journal import"
    );
    let plan_copy = identity
        .logical
        .iter()
        .find(|logical| logical.name == "CanonicalRestoreSourceAcquisitionPlanCopy")
        .expect("CanonicalRestoreSourceAcquisitionPlanCopy predecessor exists");
    assert_eq!(
        logical.construction_order,
        plan_copy.construction_order + 4,
        "the generated refinement copies and sealed import form four source-DAG strata after the plan copy"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "RestoreSourceAcquisitionPlanImportRecord")
        .expect("RestoreSourceAcquisitionPlanImportRecord permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:restore-source-acquisition-plan-import-record"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x03f0");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|RestoreSourceAcquisitionPlanImportRecord<Role:AuthorityOwningRole>";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == source_key)
        .expect("RestoreSourceAcquisitionPlanImportRecord source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-restore-source-acquisition-plan-import-record"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:restore-source-acquisition-plan-import-record"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity
            .fields
            .iter()
            .any(|field| { field.containing_schema == "RestoreSourceAcquisitionPlanImportRecord" }),
        "the shell increment must not preempt its generated-reference field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "RestoreSourceAcquisitionPlanImportRecord"
                || union.union_name == "RestoreSourceAcquisitionPlanImportRecord"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "RestoreSourceAcquisitionPlanImportRecord"
                || union.union_name == "RestoreSourceAcquisitionPlanImportRecord"
        }),
        "the record shell must not manufacture a same-name union or any arms"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|RestoreSourceAcquisitionPlanImportRecord|")
                || row.resolved_source_keys.iter().any(|source_key| {
                    source_key.contains("|RestoreSourceAcquisitionPlanImportRecord|")
                })
        }),
        "shorthand ambiguities must remain open until exact field types are settled"
    );

    let target_row_id = "a17:logical-kind:restore-source-acquisition-plan-import-record";
    let a17 = catalog
        .slices
        .iter()
        .find(|slice| slice.id == "a17")
        .expect("a17 slice exists");
    assert_eq!(
        a17.definition_status, "declared",
        "coverage must close before completion-layer authoring"
    );
    assert!(
        !catalog
            .annotations
            .iter()
            .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .semantic_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .expansion_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .evidence
                .iter()
                .any(|row| row.target_row_id == target_row_id),
        "a declared shell must not skip coverage-first sequencing with premature completion rows"
    );
}

#[test]
fn idr_restore_source_acquisition_bundle_closes_the_bidirectional_order_interval() {
    let identity = real_identity();
    let logical = |name: &str| {
        identity
            .logical
            .iter()
            .find(|logical| logical.name == name)
            .expect("named logical row exists")
    };
    let bundle = logical("RestoreSourceAcquisitionBundle");
    assert_eq!(bundle.object_kind, 0x03ef);
    assert_eq!(bundle.status, "reserved");
    assert_eq!(bundle.construction_order, 46);
    assert_eq!(bundle.role_predicate, "true");
    assert_eq!(bundle.max_size_bytes, 16_777_216);
    assert_eq!(
        bundle.golden_corpus,
        "corpus/logical/restore_source_acquisition_bundle/"
    );

    for predecessor in [
        "RestoreSourceAcquisitionPlanImportRecord",
        "RestoreCanonicalAcquisitionWorkingSet",
        "RestoreSourceAcquisitionSourceGate",
        "PortableRestoreArchiveAcquisitionReceipt",
        "RestoreSourceLeaseRecord",
    ] {
        assert!(
            logical(predecessor).construction_order < bundle.construction_order,
            "{predecessor} must precede the bundle that strongly retains it"
        );
    }
    // The tight `RestoreSourceLeaseRecord + 1` witness was retired by fgdb-oicl.
    // It held only while the lease record sat at 45, and 45 was itself out of
    // window: four referrers (RecoveryBridgeSourceLeaseBasis, RestoreLeaseState,
    // RestoreLeaseReleaseEligibility, RestoreSourceLeaseAuthorityObservationImport)
    // cap it at 30 against a floor of 27, so the equality encoded the defect rather
    // than a contract. The bundle deliberately does NOT move with it: 46 still
    // satisfies every outbound edge, and the repair moves the minimum set of
    // symbols. What survives is the law this test is named for -- the bundle is
    // bounded below by every target its own body retains.
    let latest_outbound = [
        "RestoreSourceAcquisitionPlanImportRecord",
        "RestoreCanonicalAcquisitionWorkingSet",
        "RestoreSourceAcquisitionSourceGate",
        "PortableRestoreArchiveAcquisitionReceipt",
        "RestoreSourceLeaseRecord",
    ]
    .into_iter()
    .map(|name| logical(name).construction_order)
    .max()
    .expect("the bundle retains at least one ordered target");
    assert!(
        latest_outbound <= bundle.construction_order,
        "the latest outbound strong-reference target ({latest_outbound}) must not \
         follow the bundle that retains it ({})",
        bundle.construction_order
    );

    let bridge_authority = logical("RecoveryBridgeAuthority");
    assert_eq!(
        bridge_authority.construction_order, 51,
        "the Local/Meta and Shard authority arms must follow all of their targets"
    );
    for predecessor in [
        "RestoreDirectCreationAuthorityRecord",
        "RestoreSourceAcquisitionBundle",
        "RestoreSourceLeaseRecord",
        "RestoreShardBootstrapProjectionCertificate",
        "ShardRestoreSourceLeaseProjection",
    ] {
        assert!(
            logical(predecessor).construction_order < bridge_authority.construction_order,
            "{predecessor} must precede RecoveryBridgeAuthority"
        );
    }
    assert_eq!(
        bridge_authority.construction_order,
        logical("RestoreDirectCreationAuthorityRecord")
            .construction_order
            .max(logical("ShardRestoreSourceLeaseProjection").construction_order)
            + 1,
        "the two order-50 authority inputs fix RecoveryBridgeAuthority at order 51"
    );
    let bridge_union = identity
        .ordinary_unions
        .iter()
        .find(|union| union.union_name == "RecoveryBridgeAuthority<Role>")
        .expect("RecoveryBridgeAuthority generic whole-schema union exists");
    assert_eq!(
        bridge_union.union_name, bridge_union.containing_schema,
        "a generic whole-schema union must not substitute its generic-free family"
    );
    assert_eq!(
        bridge_union.union_name, bridge_union.union_path,
        "a generic whole-schema union must preserve the exact source spelling in every shape field"
    );
    assert_eq!(bridge_union.field_tag, None);

    let catalog = real_appendix_catalog();
    let source_key = "top|RestoreSourceAcquisitionBundle<Role:AuthorityOwningRole>";
    let reservation = catalog
        .reservations
        .iter()
        .find(|row| row.symbol == "RestoreSourceAcquisitionBundle")
        .expect("RestoreSourceAcquisitionBundle permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:restore-source-acquisition-bundle"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x03ef");
    assert_eq!(reservation.disposition, "existing");

    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|row| row.source_key == source_key)
        .expect("RestoreSourceAcquisitionBundle confirmed source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|row| row.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-restore-source-acquisition-bundle"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:restore-source-acquisition-bundle"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity.fields.iter().any(|field| {
            field.containing_schema == "RestoreSourceAcquisitionBundle"
                || field
                    .containing_schema
                    .starts_with("RestoreSourceAcquisitionBundle<")
        }),
        "the shell increment must not preempt the shorthand field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "RestoreSourceAcquisitionBundle"
                || union.union_name == "RestoreSourceAcquisitionBundle"
                || union
                    .containing_schema
                    .starts_with("RestoreSourceAcquisitionBundle<")
                || union
                    .union_name
                    .starts_with("RestoreSourceAcquisitionBundle<")
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "RestoreSourceAcquisitionBundle"
                || union.union_name == "RestoreSourceAcquisitionBundle"
                || union
                    .containing_schema
                    .starts_with("RestoreSourceAcquisitionBundle<")
                || union
                    .union_name
                    .starts_with("RestoreSourceAcquisitionBundle<")
        }),
        "the record shell must not manufacture a generic whole-schema union"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|RestoreSourceAcquisitionBundle|")
                || row
                    .resolved_source_keys
                    .iter()
                    .any(|resolved| resolved.contains("|RestoreSourceAcquisitionBundle|"))
        }),
        "shorthand ambiguities must remain open until exact field types are settled"
    );
    let target_row_id = "a17:logical-kind:restore-source-acquisition-bundle";
    assert!(
        !catalog
            .annotations
            .iter()
            .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .semantic_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .expansion_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .evidence
                .iter()
                .any(|row| row.target_row_id == target_row_id),
        "a declared shell must not skip coverage-first sequencing"
    );
}

#[test]
fn idr_pre_bootstrap_dispatch_terminal_accumulator_uses_the_accumulator_stratum() {
    let identity = real_identity();
    let logical = |name: &str| {
        identity
            .logical
            .iter()
            .find(|logical| logical.name == name)
            .expect("named logical row exists")
    };
    let accumulator = logical("PreBootstrapDispatchTerminalAccumulator");
    assert_eq!(accumulator.object_kind, 0x0395);
    assert_eq!(accumulator.status, "reserved");
    assert_eq!(accumulator.construction_order, 20);
    assert_eq!(accumulator.role_predicate, "true");
    assert_eq!(accumulator.max_size_bytes, 16_777_216);
    assert_eq!(
        accumulator.golden_corpus,
        "corpus/logical/pre_bootstrap_dispatch_terminal_accumulator/"
    );

    for precedent in [
        "RestoreLeaseOperationTerminalAccumulator",
        "RestoreSourceKeyAccessCleanupAccumulator",
    ] {
        assert_eq!(
            logical(precedent).construction_order,
            accumulator.construction_order,
            "{precedent} is the existing terminal-accumulator stratum precedent"
        );
    }

    let plan = String::from_utf8(real_plan_source()).expect("the normative plan is UTF-8");
    assert!(
        plan.contains(
            "The spec constructs `PreBootstrapDispatchTerminalAccumulator<Role>`, which compactly authenticates"
        ),
        "the projection fallback is licensed by the substantive normative definition"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|row| row.symbol == "PreBootstrapDispatchTerminalAccumulator")
        .expect("terminal accumulator permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:pre-bootstrap-dispatch-terminal-accumulator"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x0395");
    assert_eq!(reservation.disposition, "existing");

    let source_disposition = catalog
        .source_symbol_dispositions
        .iter()
        .find(|row| row.symbol == "PreBootstrapDispatchTerminalAccumulator")
        .expect("terminal accumulator source disposition exists");
    assert_eq!(
        source_disposition.disposition, "reference-only",
        "the census does not currently produce a structural top-level candidate"
    );
    assert!(
        !catalog
            .top_level_candidates
            .iter()
            .any(|row| row.symbol == "PreBootstrapDispatchTerminalAccumulator"),
        "the shell must use the declared projection fallback until the parser owns the prose shape"
    );

    let targets = catalog
        .targets
        .iter()
        .filter(|row| {
            row.target_row_id == "a17:logical-kind:pre-bootstrap-dispatch-terminal-accumulator"
        })
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "the logical shell maps exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-pre-bootstrap-dispatch-terminal-accumulator"
    );
    assert_eq!(
        targets[0].source_key,
        "projection|logical_object_kinds|PreBootstrapDispatchTerminalAccumulator"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity.fields.iter().any(|field| {
            field.containing_schema == "PreBootstrapDispatchTerminalAccumulator"
                || field
                    .containing_schema
                    .starts_with("PreBootstrapDispatchTerminalAccumulator<")
        }),
        "the shell must not invent fields absent from the shorthand census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "PreBootstrapDispatchTerminalAccumulator"
                || union
                    .containing_schema
                    .starts_with("PreBootstrapDispatchTerminalAccumulator<")
        }),
        "the record-shaped accumulator must not manufacture a union"
    );
}

#[test]
fn idr_compacted_pre_bootstrap_evidence_follows_its_terminal_accumulator() {
    let identity = real_identity();
    let logical = |name: &str| {
        identity
            .logical
            .iter()
            .find(|logical| logical.name == name)
            .expect("named logical row exists")
    };
    let compacted = logical("CompactedPreBootstrapEvidence");
    assert_eq!(compacted.object_kind, 0x0276);
    assert_eq!(compacted.status, "reserved");
    assert_eq!(compacted.construction_order, 20);
    assert_eq!(compacted.role_predicate, "true");
    assert_eq!(compacted.max_size_bytes, 16_777_216);
    assert_eq!(
        compacted.golden_corpus,
        "corpus/logical/compacted_pre_bootstrap_evidence/"
    );
    assert_eq!(
        compacted.construction_order,
        logical("PreBootstrapDispatchTerminalAccumulator").construction_order,
        "the explicit StrongRef fixes the source-derived floor at the established order-20 accumulator stratum"
    );

    let plan = String::from_utf8(real_plan_source()).expect("the normative plan is UTF-8");
    assert!(
        plan.contains(
            "terminal_accumulator_ref:StrongRef<PreBootstrapDispatchTerminalAccumulator<Role>>"
        ),
        "the construction-order edge must remain source-visible"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|row| row.symbol == "CompactedPreBootstrapEvidence")
        .expect("compacted evidence permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:compacted-pre-bootstrap-evidence"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x0276");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|CompactedPreBootstrapEvidence<Role>";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|row| row.source_key == source_key)
        .expect("compacted evidence confirmed source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|row| row.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "the source candidate maps exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-compacted-pre-bootstrap-evidence"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:compacted-pre-bootstrap-evidence"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity.fields.iter().any(|field| {
            field.containing_schema == "CompactedPreBootstrapEvidence"
                || field
                    .containing_schema
                    .starts_with("CompactedPreBootstrapEvidence<")
        }),
        "the logical shell must not preempt the shorthand field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "CompactedPreBootstrapEvidence"
                || union
                    .containing_schema
                    .starts_with("CompactedPreBootstrapEvidence<")
        }),
        "the record-shaped evidence must not manufacture a union"
    );
}

#[test]
fn idr_a17_remaining_journal_shells_exhaust_reserved_identity_codes() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();
    let cases = [
        (
            "CanonicalLocalRefinementCommitReceiptCopy",
            0x025a,
            6,
            "a17:reservation:canonical-local-refinement-commit-receipt-copy",
            "a17:logical-kind:canonical-local-refinement-commit-receipt-copy",
            "a17:target:logical-kind-canonical-local-refinement-commit-receipt-copy",
            "projection|logical_object_kinds|CanonicalLocalRefinementCommitReceiptCopy",
            "reference-only",
            None,
            "corpus/logical/canonical_local_refinement_commit_receipt_copy/",
        ),
        (
            "PreBootstrapJournalCanonicalizationImportRecord",
            0x0396,
            20,
            "a17:reservation:pre-bootstrap-journal-canonicalization-import-record",
            "a17:logical-kind:pre-bootstrap-journal-canonicalization-import-record",
            "a17:target:logical-kind-pre-bootstrap-journal-canonicalization-import-record",
            "top|PreBootstrapJournalCanonicalizationImportRecord<Role>",
            "appendix-ambiguous-structure",
            Some("ambiguous"),
            "corpus/logical/pre_bootstrap_journal_canonicalization_import_record/",
        ),
        (
            "RestoreJournalKeyDisposition",
            0x03d9,
            20,
            "a17:reservation:restore-journal-key-disposition",
            "a17:logical-kind:restore-journal-key-disposition",
            "a17:target:logical-kind-restore-journal-key-disposition",
            "projection|logical_object_kinds|RestoreJournalKeyDisposition",
            "reference-only",
            None,
            "corpus/logical/restore_journal_key_disposition/",
        ),
    ];

    for (
        name,
        object_kind,
        construction_order,
        reservation_row_id,
        logical_row_id,
        target_row_id,
        source_key,
        source_disposition,
        candidate_source_kind,
        corpus,
    ) in cases
    {
        let logical = identity
            .logical
            .iter()
            .find(|row| row.name == name)
            .expect("remaining a17 logical shell exists");
        assert_eq!(logical.object_kind, object_kind);
        assert_eq!(logical.status, "reserved");
        assert_eq!(logical.construction_order, construction_order);
        assert_eq!(logical.role_predicate, "true");
        assert_eq!(logical.max_size_bytes, 16_777_216);
        assert_eq!(logical.golden_corpus, corpus);
        assert!(
            !identity.wire.iter().any(|row| row.name == name),
            "an existing logical reservation must not mint a second wire identity"
        );

        let reservation = catalog
            .reservations
            .iter()
            .find(|row| row.symbol == name)
            .expect("permanent a17 reservation exists");
        assert_eq!(reservation.row_id, reservation_row_id);
        assert_eq!(reservation.row_kind, "logical-kind");
        assert_eq!(reservation.identity_class, "logical");
        assert_eq!(reservation.disposition, "existing");

        let disposition = catalog
            .source_symbol_dispositions
            .iter()
            .find(|row| row.symbol == name)
            .expect("source disposition exists");
        assert_eq!(disposition.disposition, source_disposition);

        match candidate_source_kind {
            Some(source_kind) => {
                let candidate = catalog
                    .top_level_candidates
                    .iter()
                    .find(|row| row.source_key == source_key)
                    .expect("structural source candidate exists");
                assert_eq!(candidate.source_kind, source_kind);
                assert_eq!(candidate.identity_class, "logical");
            }
            None => assert!(
                !catalog
                    .top_level_candidates
                    .iter()
                    .any(|row| row.symbol == name),
                "reference-only source must use the declared projection fallback"
            ),
        }

        let targets = catalog
            .targets
            .iter()
            .filter(|row| row.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{name} maps exactly once");
        assert_eq!(targets[0].row_id, target_row_id);
        assert_eq!(targets[0].target_row_id, logical_row_id);
        assert_eq!(targets[0].target_kind, "logical-kind");
        assert_eq!(targets[0].definition_status, "declared");

        assert!(
            !identity.fields.iter().any(|field| {
                field.containing_schema == name
                    || field.containing_schema.starts_with(&format!("{name}<"))
            }),
            "a shell must not invent fields absent from the shorthand census"
        );
        assert!(
            !identity.ordinary_unions.iter().any(|union| {
                union.containing_schema == name
                    || union.containing_schema.starts_with(&format!("{name}<"))
            }),
            "a shell must not invent ordinary-union rows absent from the census"
        );
    }

    let logical = |name: &str| {
        identity
            .logical
            .iter()
            .find(|row| row.name == name)
            .expect("source-order precedent exists")
    };
    assert_eq!(
        logical("CanonicalLocalRefinementCommitReceiptCopy").construction_order,
        logical("CanonicalRestorePlanAvailabilityCopy").construction_order,
        "the reference-free authenticated receipt copy belongs to the order-6 canonical leaf-copy stratum"
    );
    for name in [
        "PreBootstrapJournalCanonicalizationImportRecord",
        "RestoreJournalKeyDisposition",
    ] {
        assert_eq!(
            logical(name).construction_order,
            logical("PreBootstrapDispatchTerminalAccumulator").construction_order,
            "{name} retains the already-built order-20 terminal accumulator/evidence stratum"
        );
    }

    let plan = String::from_utf8(real_plan_source()).expect("the normative plan is UTF-8");
    assert!(plan.contains(
        "`CanonicalLocalRefinementCommitReceiptCopy<Role>` verifies the original journal MAC during import"
    ));
    assert!(plan.contains(
        "Apply creates `PreBootstrapJournalCanonicalizationImportRecord<Role> {authority_bound_header,restore_id,journal_id,certificate_bytes_and_digest,terminal_head_bytes_digest_and_cas_version,predecessor_active_head_digest_and_cas_version,sealed_import_ref,terminal_accumulator_ref"
    ));
    assert!(plan.contains(
        "Canonical restore uses exact `RestoreJournalKeyDisposition<Role>` with two noninterchangeable states"
    ));

    let still_reserved = catalog
        .reservations
        .iter()
        .filter(|row| row.slice_id == "a17" && row.disposition == "reserved")
        .map(|row| row.symbol.as_str())
        .collect::<Vec<_>>();
    assert!(
        still_reserved.is_empty(),
        "the closeout must exhaust a17's permanent identity reservations: {still_reserved:?}"
    );
}

#[test]
fn idr_a17_outer_artifact_rows_remain_prebootstrap_only() {
    let identity = real_identity();
    let catalog = real_appendix_catalog();
    let cases = [
        ("PortableRestorePlanArtifact", 0x0001, 16_777_216),
        ("RestoreStatusReceiptArtifact", 0x0002, 1_048_576),
        ("AvailabilityCertificateArtifact", 0x0003, 1_048_576),
    ];

    for (name, artifact_kind, max_size_bytes) in cases {
        let row = identity
            .prebootstrap
            .iter()
            .find(|row| row.name == name)
            .expect("outer artifact has a prebootstrap identity row");
        assert_eq!(row.artifact_kind, artifact_kind);
        assert_eq!(row.status, "reserved");
        assert_eq!(row.max_size_bytes, max_size_bytes);
        assert!(
            !identity.logical.iter().any(|other| other.name == name)
                && !identity.physical.iter().any(|other| other.name == name)
                && !identity.bootstrap.iter().any(|other| other.name == name)
                && !identity.wire.iter().any(|other| other.name == name),
            "an outer artifact must inhabit only the prebootstrap class"
        );
        assert!(
            !catalog
                .top_level_candidates
                .iter()
                .any(|candidate| candidate.symbol == name),
            "outer artifact identity comes from its prebootstrap projection, not a fabricated source candidate"
        );

        let source_key = format!("projection|prebootstrap_artifact_kinds|{name}");
        let targets = catalog
            .targets
            .iter()
            .filter(|target| target.source_key == source_key)
            .collect::<Vec<_>>();
        assert_eq!(targets.len(), 1, "{name} maps exactly once");
        assert_eq!(targets[0].target_kind, "prebootstrap-kind");
        assert_eq!(targets[0].definition_status, "declared");
    }
}

#[test]
fn idr_restore_canonical_acquisition_working_set_reserves_pre_freeze_stratum() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "RestoreCanonicalAcquisitionWorkingSet")
        .expect("RestoreCanonicalAcquisitionWorkingSet logical shell exists");
    assert_eq!(logical.object_kind, 0x03d6);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 10);
    assert_eq!(logical.role_predicate, "true");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(
        logical.golden_corpus,
        "corpus/logical/restore_canonical_acquisition_working_set/"
    );

    let plan_copy = identity
        .logical
        .iter()
        .find(|logical| logical.name == "CanonicalRestoreSourceAcquisitionPlanCopy")
        .expect("CanonicalRestoreSourceAcquisitionPlanCopy predecessor exists");
    assert_eq!(
        logical.construction_order,
        plan_copy.construction_order + 3,
        "the generated bootstrap and verified-inventory refs occupy the two strata before the working set"
    );
    let sealed_import = identity
        .logical
        .iter()
        .find(|logical| logical.name == "SealedPreBootstrapDispatchJournalImport")
        .expect("SealedPreBootstrapDispatchJournalImport peer stratum exists");
    assert_eq!(
        logical.construction_order, sealed_import.construction_order,
        "the independent working-set and sealed-import closures share the latest verified-inventory prerequisite"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "RestoreCanonicalAcquisitionWorkingSet")
        .expect("RestoreCanonicalAcquisitionWorkingSet permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:restore-canonical-acquisition-working-set"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x03d6");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|RestoreCanonicalAcquisitionWorkingSet<Role:AuthorityOwningRole>";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == source_key)
        .expect("RestoreCanonicalAcquisitionWorkingSet source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-restore-canonical-acquisition-working-set"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:restore-canonical-acquisition-working-set"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity
            .fields
            .iter()
            .any(|field| { field.containing_schema == "RestoreCanonicalAcquisitionWorkingSet" }),
        "the shell increment must not preempt its shorthand field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "RestoreCanonicalAcquisitionWorkingSet"
                || union.union_name == "RestoreCanonicalAcquisitionWorkingSet"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "RestoreCanonicalAcquisitionWorkingSet"
                || union.union_name == "RestoreCanonicalAcquisitionWorkingSet"
        }),
        "the record shell must not manufacture a same-name union or any arms"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|RestoreCanonicalAcquisitionWorkingSet|")
                || row.resolved_source_keys.iter().any(|source_key| {
                    source_key.contains("|RestoreCanonicalAcquisitionWorkingSet|")
                })
        }),
        "shorthand ambiguities must remain open until exact field types are settled"
    );

    let target_row_id = "a17:logical-kind:restore-canonical-acquisition-working-set";
    let a17 = catalog
        .slices
        .iter()
        .find(|slice| slice.id == "a17")
        .expect("a17 slice exists");
    assert_eq!(
        a17.definition_status, "declared",
        "coverage must close before completion-layer authoring"
    );
    assert!(
        !catalog
            .annotations
            .iter()
            .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .semantic_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .expansion_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .evidence
                .iter()
                .any(|row| row.target_row_id == target_row_id),
        "a declared shell must not skip coverage-first sequencing with premature completion rows"
    );
}

#[test]
fn idr_canonical_pre_bootstrap_evidence_reencryption_owner_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let logical = identity
        .logical
        .iter()
        .find(|logical| logical.name == "CanonicalPreBootstrapEvidenceReencryptionOwner")
        .expect("CanonicalPreBootstrapEvidenceReencryptionOwner logical shell exists");
    assert_eq!(logical.object_kind, 0x025b);
    assert_eq!(logical.status, "reserved");
    assert_eq!(logical.construction_order, 6);
    assert_eq!(logical.role_predicate, "true");
    assert_eq!(logical.max_size_bytes, 16_777_216);
    assert_eq!(
        logical.golden_corpus,
        "corpus/logical/canonical_pre_bootstrap_evidence_reencryption_owner/"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "CanonicalPreBootstrapEvidenceReencryptionOwner")
        .expect("CanonicalPreBootstrapEvidenceReencryptionOwner permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:canonical-pre-bootstrap-evidence-reencryption-owner"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x025b");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|CanonicalPreBootstrapEvidenceReencryptionOwner<Role>";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == source_key)
        .expect("CanonicalPreBootstrapEvidenceReencryptionOwner source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-canonical-pre-bootstrap-evidence-reencryption-owner"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:canonical-pre-bootstrap-evidence-reencryption-owner"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity.fields.iter().any(|field| {
            field.containing_schema == "CanonicalPreBootstrapEvidenceReencryptionOwner"
        }),
        "the shell increment must not preempt its field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "CanonicalPreBootstrapEvidenceReencryptionOwner"
                || union.union_name == "CanonicalPreBootstrapEvidenceReencryptionOwner"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "CanonicalPreBootstrapEvidenceReencryptionOwner"
                || union.union_name == "CanonicalPreBootstrapEvidenceReencryptionOwner"
        }),
        "the shell increment must not preempt unions or arms"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|CanonicalPreBootstrapEvidenceReencryptionOwner|")
                || row.resolved_source_keys.iter().any(|source_key| {
                    source_key.contains("|CanonicalPreBootstrapEvidenceReencryptionOwner|")
                })
        }),
        "shorthand ambiguities must remain open until exact field types are settled"
    );

    let target_row_id = "a17:logical-kind:canonical-pre-bootstrap-evidence-reencryption-owner";
    let a17 = catalog
        .slices
        .iter()
        .find(|slice| slice.id == "a17")
        .expect("a17 slice exists");
    assert_eq!(
        a17.definition_status, "declared",
        "coverage must close before completion-layer authoring"
    );
    assert!(
        !catalog
            .annotations
            .iter()
            .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .semantic_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .expansion_bindings
                .iter()
                .any(|row| row.target_row_id == target_row_id)
            && !catalog
                .evidence
                .iter()
                .any(|row| row.target_row_id == target_row_id),
        "a declared shell must not skip coverage-first sequencing with premature completion rows"
    );
}

#[test]
fn idr_canonical_pre_bootstrap_evidence_reencryption_proof_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let proof = identity
        .logical
        .iter()
        .find(|logical| logical.name == "CanonicalPreBootstrapEvidenceReencryptionProof")
        .expect("CanonicalPreBootstrapEvidenceReencryptionProof logical shell exists");
    let owner = identity
        .logical
        .iter()
        .find(|logical| logical.name == "CanonicalPreBootstrapEvidenceReencryptionOwner")
        .expect("CanonicalPreBootstrapEvidenceReencryptionOwner dependency exists");
    assert_eq!(proof.object_kind, 0x025c);
    assert_eq!(proof.status, "reserved");
    assert_eq!(proof.construction_order, owner.construction_order + 1);
    assert_eq!(proof.construction_order, 7);
    assert_eq!(proof.role_predicate, "true");
    assert_eq!(proof.max_size_bytes, 16_777_216);
    assert_eq!(
        proof.golden_corpus,
        "corpus/logical/canonical_pre_bootstrap_evidence_reencryption_proof/"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "CanonicalPreBootstrapEvidenceReencryptionProof")
        .expect("CanonicalPreBootstrapEvidenceReencryptionProof permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:canonical-pre-bootstrap-evidence-reencryption-proof"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x025c");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|CanonicalPreBootstrapEvidenceReencryptionProof<Role>";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == source_key)
        .expect("CanonicalPreBootstrapEvidenceReencryptionProof source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-canonical-pre-bootstrap-evidence-reencryption-proof"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:canonical-pre-bootstrap-evidence-reencryption-proof"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity.fields.iter().any(|field| {
            field.containing_schema == "CanonicalPreBootstrapEvidenceReencryptionProof"
        }),
        "the shell increment must not preempt its field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "CanonicalPreBootstrapEvidenceReencryptionProof"
                || union.union_name == "CanonicalPreBootstrapEvidenceReencryptionProof"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "CanonicalPreBootstrapEvidenceReencryptionProof"
                || union.union_name == "CanonicalPreBootstrapEvidenceReencryptionProof"
        }),
        "the shell increment must not preempt unions or arms"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|CanonicalPreBootstrapEvidenceReencryptionProof|")
                || row.resolved_source_keys.iter().any(|resolved| {
                    resolved.contains("|CanonicalPreBootstrapEvidenceReencryptionProof|")
                })
        }),
        "shorthand ambiguities must remain open until exact field types are settled"
    );
}

#[test]
fn idr_restore_source_acquisition_gate_is_a_logical_backed_whole_schema_union() {
    let identity = real_identity();
    let gate = identity
        .logical
        .iter()
        .find(|logical| logical.name == "RestoreSourceAcquisitionSourceGate")
        .expect("RestoreSourceAcquisitionSourceGate logical kind exists");
    let reference_free_control = identity
        .logical
        .iter()
        .find(|logical| logical.name == "PortableRestoreArchiveAcquisitionReceipt")
        .expect("known reference-free a17 control exists");
    assert_eq!(gate.object_kind, 0x03f1);
    assert_eq!(gate.status, "reserved");
    assert_eq!(
        gate.construction_order,
        reference_free_control.construction_order
    );
    assert_eq!(gate.construction_order, 6);
    assert_eq!(gate.role_predicate, "true");
    assert_eq!(gate.max_size_bytes, 16_777_216);
    assert_eq!(
        gate.golden_corpus,
        "corpus/logical/restore_source_acquisition_source_gate/"
    );

    let union = identity
        .ordinary_unions
        .iter()
        .find(|union| union.union_name == "RestoreSourceAcquisitionSourceGate")
        .expect("RestoreSourceAcquisitionSourceGate whole-schema union exists");
    assert!(
        identity::ordinary_union_has_top_level_shape(union),
        "whole-schema union name, containing schema, and path must agree exactly"
    );
    assert_eq!(union.field_tag, None);
    assert_eq!(union.tag_wire_type, "u8");
    assert_eq!(union.encoding_context, "closed-tagged");
    assert_eq!(
        union.allowed_containing_schemas,
        ["RestoreSourceAcquisitionSourceGate"]
    );
    assert_eq!(union.role_predicate, "true");
    assert_eq!(union.version_status, "reserved");
    assert_eq!(union.max_size_bytes, 16_777_216);

    let wire_parent_control = identity
        .wire
        .iter()
        .find(|wire| wire.name == "WeakAuthorityAppliedIdentity")
        .expect("known same-name wire-backed union control exists");
    assert_eq!(wire_parent_control.kind, "union");
    assert!(
        identity
            .wire
            .iter()
            .all(|wire| wire.name != "RestoreSourceAcquisitionSourceGate"),
        "the source gate is logical-backed and must not invent a same-name wire parent"
    );

    let source_order = union
        .arms
        .iter()
        .map(|arm| (arm.arm_tag, arm.source_arm_name.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        source_order,
        [
            (1, "RecoverSameIdentity"),
            (2, "CloneDirectoryBound"),
            (3, "CloneExternalCas"),
        ],
        "arm tags follow the source spelling, not the alphabetical census order"
    );
    assert_eq!(
        union
            .arms
            .iter()
            .map(|arm| arm.payload_sha256.as_deref())
            .collect::<Vec<_>>(),
        [
            Some("eecb7071f9eb5a51f31222921c7a16df796f07c9c414cc2c78c2679201eff2ee"),
            Some("28b1768d2aa78eda28de2a288da370d3ddff3b3508fd373ea915b2274a749e34"),
            Some("ce4bc9ad93776471e07b96c4f2d4a618a7727eb2715a8fd5601a54d9ab2d5d11"),
        ]
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "RestoreSourceAcquisitionSourceGate")
        .expect("RestoreSourceAcquisitionSourceGate permanent reservation exists");
    assert_eq!(reservation.code_reservation, "0x03f1");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|RestoreSourceAcquisitionSourceGate";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == source_key)
        .expect("RestoreSourceAcquisitionSourceGate source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");
    assert_eq!(
        catalog
            .targets
            .iter()
            .filter(|target| {
                target.source_key == source_key
                    || target.source_key.starts_with(
                        "union|RestoreSourceAcquisitionSourceGate|RestoreSourceAcquisitionSourceGate",
                    )
                    || target.source_key.starts_with(
                        "arm|RestoreSourceAcquisitionSourceGate|RestoreSourceAcquisitionSourceGate|",
                    )
            })
            .count(),
        5,
        "kind, whole-schema union, and three arms must each have one declared target"
    );
    assert!(
        !identity
            .fields
            .iter()
            .any(|field| field.containing_schema == "RestoreSourceAcquisitionSourceGate"),
        "a whole-schema logical union has no synthetic anchoring field"
    );
}

#[test]
fn idr_restore_journal_key_destruction_summary_reserved_logical_shell_is_exact() {
    let identity = real_identity();
    let summary = identity
        .logical
        .iter()
        .find(|logical| logical.name == "RestoreJournalKeyDestructionSummary")
        .expect("RestoreJournalKeyDestructionSummary logical shell exists");
    let reference_free_control = identity
        .logical
        .iter()
        .find(|logical| logical.name == "PortableRestoreArchiveAcquisitionReceipt")
        .expect("known reference-free a17 control exists");
    assert_eq!(summary.object_kind, 0x03d8);
    assert_eq!(summary.status, "reserved");
    assert_eq!(
        summary.construction_order, reference_free_control.construction_order,
        "the source declares no outgoing strong edge, so the compact summary shares the a17 reference-free leaf order"
    );
    assert_eq!(summary.construction_order, 6);
    assert_eq!(summary.role_predicate, "true");
    assert_eq!(summary.max_size_bytes, 16_777_216);
    assert_eq!(
        summary.golden_corpus,
        "corpus/logical/restore_journal_key_destruction_summary/"
    );

    let catalog = real_appendix_catalog();
    let reservation = catalog
        .reservations
        .iter()
        .find(|reservation| reservation.symbol == "RestoreJournalKeyDestructionSummary")
        .expect("RestoreJournalKeyDestructionSummary permanent reservation exists");
    assert_eq!(
        reservation.row_id,
        "a17:reservation:restore-journal-key-destruction-summary"
    );
    assert_eq!(reservation.row_kind, "logical-kind");
    assert_eq!(reservation.identity_class, "logical");
    assert_eq!(reservation.code_reservation, "0x03d8");
    assert_eq!(reservation.disposition, "existing");

    let source_key = "top|RestoreJournalKeyDestructionSummary<Role>";
    let candidate = catalog
        .top_level_candidates
        .iter()
        .find(|candidate| candidate.source_key == source_key)
        .expect("RestoreJournalKeyDestructionSummary source candidate exists");
    assert_eq!(candidate.source_kind, "confirmed");
    assert_eq!(candidate.identity_class, "logical");

    let targets = catalog
        .targets
        .iter()
        .filter(|target| target.source_key == source_key)
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 1, "source candidate must map exactly once");
    assert_eq!(
        targets[0].row_id,
        "a17:target:logical-kind-restore-journal-key-destruction-summary"
    );
    assert_eq!(
        targets[0].target_row_id,
        "a17:logical-kind:restore-journal-key-destruction-summary"
    );
    assert_eq!(targets[0].target_kind, "logical-kind");
    assert_eq!(targets[0].definition_status, "declared");

    assert!(
        !identity
            .fields
            .iter()
            .any(|field| { field.containing_schema == "RestoreJournalKeyDestructionSummary" }),
        "the shell increment must not preempt its field census"
    );
    assert!(
        !identity.ordinary_unions.iter().any(|union| {
            union.containing_schema == "RestoreJournalKeyDestructionSummary"
                || union.union_name == "RestoreJournalKeyDestructionSummary"
        }) && !identity.unions.iter().any(|union| {
            union.containing_schema == "RestoreJournalKeyDestructionSummary"
                || union.union_name == "RestoreJournalKeyDestructionSummary"
        }),
        "the compact record body is not a closed union and this shell must not invent one"
    );
    assert!(
        !catalog.ambiguity_adjudications.iter().any(|row| {
            row.ambiguity_source_key
                .contains("|RestoreJournalKeyDestructionSummary|")
                || row
                    .resolved_source_keys
                    .iter()
                    .any(|resolved| resolved.contains("|RestoreJournalKeyDestructionSummary|"))
        }),
        "shorthand ambiguities must remain open until exact field types are settled"
    );
}

// ---------------------------------------------------------------------------
// The wire tag declares the reference strength
// (fgdb-refsem-not-forced-by-wire-type-gls4).
//
// Appendix A: "Every ObjectId-bearing edge declares a wire tag: `StrongRef{oid}`
// (always followed) ... `WeakMarkerIdentity{marker_oid,commit_seq}`
// (provenance/identity only) ... `WeakDigest{digest}` (comparison only)", and
// for the W12 wrappers "Strong variants retain, conditional variants stop only
// at a verified matching meta/shard checkpoint cut, and weak variants compare
// only."
//
// That declaration was enforced on Appendix A catalog ANNOTATIONS and on
// nothing else, so a `[[field]]` row typed `StrongRef` could declare
// `reference_semantics = "none"`, keep its target, and pass every gate --
// silently switching off dag_future_result, bare_strong_ref and every generated
// reachability/GC/checkpoint-vector walker for that member, then freezing
// behind the append-only field pin.
// ---------------------------------------------------------------------------

#[test]
fn idr_wire_tag_declares_reference_semantics() {
    let base = real_identity();
    let base_codes = codes_without_assignment_drift(&base);
    for code in [
        "wire_type_reference_semantics_mismatch",
        "reference_semantics_without_reference_type",
        "unclassified_reference_wrapper",
    ] {
        assert!(
            !base_codes.contains(&code.to_string()),
            "the landed corpus must already satisfy the wire-tag law, but {code} fires: \
             {base_codes:?}"
        );
    }

    // NON-VACUITY. An equivalence over an empty population proves nothing, so
    // pin the population the law actually constrains.
    let wrapper_names: BTreeSet<&str> = base
        .wire
        .iter()
        .filter(|w| w.kind == "reference_wrapper")
        .map(|w| w.name.as_str())
        .collect();
    assert!(
        wrapper_names.len() >= 17,
        "reference_wrapper population shrank: {}",
        wrapper_names.len()
    );
    let wrapper_typed = base
        .fields
        .iter()
        .filter(|f| wrapper_names.contains(f.exact_wire_type.as_str()))
        .count();
    assert!(
        wrapper_typed >= 300,
        "wrapper-typed field rows must be a real population, got {wrapper_typed}"
    );

    // The exact subject and spellings the bead measured, on the real corpus.
    let subject = |r: &mut IdentityRegistries| -> usize {
        r.fields
            .iter()
            .position(|f| {
                f.containing_schema == "ResourceLedgerTransition<Role:AuthorityOwningRole>"
                    && f.stable_name == "authorization_decision_ref"
            })
            .expect("the landed StrongRef subject row")
    };

    // (a) weakened to "none" with its target kept -- the spelling that passed.
    let mut kept = real_identity();
    let i = subject(&mut kept);
    assert_eq!(kept.fields[i].exact_wire_type, "StrongRef");
    kept.fields[i].reference_semantics = "none".into();
    kept.fields[i].identity_class = "inline".into();
    assert!(
        codes(&kept).contains(&"wire_type_reference_semantics_mismatch".to_string()),
        "a StrongRef member may not declare reference_semantics = \"none\""
    );

    // (b) weakened AND target dropped -- the other spelling that passed.
    let mut dropped = real_identity();
    let i = subject(&mut dropped);
    dropped.fields[i].reference_semantics = "none".into();
    dropped.fields[i].identity_class = "inline".into();
    dropped.fields[i].target_schema_id = None;
    assert!(
        codes(&dropped).contains(&"wire_type_reference_semantics_mismatch".to_string()),
        "dropping the target does not excuse weakening the tag"
    );

    // (c) strengthened the other way: StrongRef may not claim to be conditional.
    let mut stronger = real_identity();
    let i = subject(&mut stronger);
    stronger.fields[i].reference_semantics = "conditional".into();
    assert!(
        codes(&stronger).contains(&"wire_type_reference_semantics_mismatch".to_string()),
        "the law is an equality, not a floor"
    );

    // (d) the dual direction: a plain scalar promoted to a retaining edge with
    // every NEIGHBOURING guard satisfied (logical class, resolving earlier
    // target), so only the missing wire tag can be what rejects it.
    let mut promoted = real_identity();
    let host = promoted
        .fields
        .iter()
        .position(|f| {
            f.exact_wire_type == "u64"
                && f.reference_semantics == "none"
                && f.target_schema_id.is_none()
                && f.cardinality == "one"
        })
        .expect("a plain u64 control row");
    let host_order = promoted.fields[host].construction_order;
    let target = promoted
        .logical
        .iter()
        .filter(|k| k.construction_order <= host_order)
        .max_by_key(|k| k.construction_order)
        .expect("an earlier logical kind")
        .name
        .clone();
    promoted.fields[host].identity_class = "logical".into();
    promoted.fields[host].reference_semantics = "strong".into();
    promoted.fields[host].target_schema_id = Some(target);
    assert!(
        codes(&promoted).contains(&"reference_semantics_without_reference_type".to_string()),
        "a u64 may not become a retaining edge by declaring one"
    );

    // (e) the completeness guard: a newly minted wrapper whose strength is
    // declared nowhere must be rejected, or the law above fails OPEN on exactly
    // the rows it was written for.
    let mut minted = real_identity();
    let mut wrapper = minted
        .wire
        .iter()
        .find(|w| w.name == "StrongRef")
        .expect("StrongRef is registered")
        .clone();
    wrapper.wire_type_id = 0x7ffe;
    wrapper.name = "UnclassifiedWrapperRef".into();
    minted.wire.push(wrapper);
    assert!(
        codes(&minted).contains(&"unclassified_reference_wrapper".to_string()),
        "an unclassified reference_wrapper leaves its field rows unconstrained"
    );
}

/// The two artifacts share ONE table. Every strength the Appendix A catalog can
/// declare must have a legal field-level spelling, or a row could be required to
/// carry a value `bad_field` rejects.
#[test]
fn idr_declared_reference_strengths_are_field_spellable() {
    let base = real_identity();
    let vocabulary = [
        "none",
        "strong",
        "conditional",
        "weak_digest",
        "locator",
        "external_root",
    ];
    // Drive it through the public verdict: for every registered wrapper, a row
    // carrying each vocabulary value in turn must be accepted for exactly one
    // of them -- which is the law -- and that one must be in the vocabulary.
    let mut checked = 0;
    for name in base
        .wire
        .iter()
        .filter(|w| w.kind == "reference_wrapper")
        .map(|w| w.name.clone())
        .collect::<Vec<_>>()
    {
        let Some(idx) = base.fields.iter().position(|f| f.exact_wire_type == name) else {
            continue;
        };
        let accepted: Vec<&str> = vocabulary
            .iter()
            .copied()
            .filter(|sem| {
                let mut r = base.clone();
                r.fields[idx].reference_semantics = (*sem).into();
                !codes_without_assignment_drift(&r)
                    .contains(&"wire_type_reference_semantics_mismatch".to_string())
            })
            .collect();
        assert_eq!(
            accepted.len(),
            1,
            "wrapper {name} must admit exactly one field spelling, got {accepted:?}"
        );
        checked += 1;
    }
    assert!(
        checked >= 5,
        "non-vacuity: at least the five wrappers with landed rows are checked, got {checked}"
    );
}

// ---------------------------------------------------------------------------
// Field identity classes are a field-domain law, not a wire-shape convention
// (fgdb-identity-class-record-wire-convention-l6xd).
//
// This is intentionally a controlled probe.  The subject differs from the
// known-boring u64 control only in exact_wire_type/max_size_bytes, and both
// carry the construction order of their registered host.  An earlier
// subject-only probe used a malformed order and incorrectly reported that no
// class was accepted; the control makes that failure mode visible.
// ---------------------------------------------------------------------------

#[test]
fn idr_field_identity_class_domain_is_wire_shape_independent() {
    let base = real_identity();
    let host = base
        .logical
        .iter()
        .find(|kind| kind.name == "LocalBeginReservationSpec")
        .expect("the A03 field host is registered")
        .clone();
    let vocabulary = ["logical", "physical", "inline", "wire", "prebootstrap"];

    let accepted = |exact_wire_type: &str, max_size_bytes: i64| -> Vec<&str> {
        vocabulary
            .iter()
            .copied()
            .filter(|class| {
                let mut probe = base.clone();
                probe.fields.retain(|field| {
                    field.containing_schema != host.name
                        || field.stable_name != "authority_bound_header"
                });
                probe.fields.push(FieldRow {
                    containing_schema: host.name.clone(),
                    field_tag: 0x0001,
                    stable_name: "authority_bound_header".into(),
                    exact_wire_type: exact_wire_type.into(),
                    cardinality: "one".into(),
                    identity_class: (*class).into(),
                    reference_semantics: "none".into(),
                    target_schema_id: None,
                    construction_order: host.construction_order,
                    role_predicate: "true".into(),
                    retention_and_cut_rule: "controlled l6xd probe".into(),
                    version_status: "reserved".into(),
                    max_size_bytes,
                    digest_class: None,
                    transcript_recipe: None,
                    bd_domain_separator: None,
                    bd_schema_major: None,
                    bd_included_field_tags: None,
                    bd_excluded_field_tags: None,
                    recipe_pin: None,
                });
                codes_without_assignment_drift(&probe).is_empty()
            })
            .collect()
    };

    let subject = accepted("AuthorityBoundHeader", 256);
    let control = accepted("u64", 8);
    assert_eq!(
        subject, control,
        "record-shaped exact wire types and plain scalars must traverse the same field-class law"
    );
    assert_eq!(
        subject,
        vec!["logical", "physical", "inline"],
        "among top-level durable identity classes, fields admit logical, physical, and inline while rejecting wire and prebootstrap"
    );
}
