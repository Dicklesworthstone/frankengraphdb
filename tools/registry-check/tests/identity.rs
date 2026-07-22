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
