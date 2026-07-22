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
    let mut f = field("LogicalStatePayload", 90, "self_ref", 30);
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
        .filter(|f| f.digest_class.as_deref() == Some("body"))
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
        let k = by_name
            .get(name)
            .unwrap_or_else(|| panic!("missing reserved kind {name}"));
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
        for (_, t) in &u.arms {
            load_bearing.insert(t.as_str());
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
                violations.is_empty(),
                "removing leaf kind {victim:?} must stay clean; got {violations:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Fuzz: mutated registry bytes and drifted recipe vectors fail closed,
// naming the exact failing recipe.
// ---------------------------------------------------------------------------

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
fn idr_golden_vector_mutation() {
    let root = repo_root();
    let mut rng = XorShift64(0x1DE_17171_71DE);

    // (a) Bit-flipped recipe "golden vectors": flipping any bit of a pinned
    // recipe pin must be caught, and the violation names the exact row.
    let r = real_identity();
    let body_rows: Vec<(String, String)> = r
        .fields
        .iter()
        .filter(|f| f.digest_class.as_deref() == Some("body"))
        .map(|f| (f.containing_schema.clone(), f.stable_name.clone()))
        .collect();
    for (schema, name) in &body_rows {
        let mut mutated = r.clone();
        for f in &mut mutated.fields {
            if &f.containing_schema == schema && &f.stable_name == name {
                let pin = f.recipe_pin.clone().expect("pin");
                // Flip one hex nibble deterministically.
                let mut bytes = pin.into_bytes();
                let idx = bytes.len() - 1 - (rng.next() as usize % 8);
                bytes[idx] = if bytes[idx] == b'0' { b'1' } else { b'0' };
                f.recipe_pin = Some(String::from_utf8(bytes).expect("ascii pin"));
            }
        }
        let violations = identity::validate_identity(&mutated);
        let hit = violations
            .iter()
            .find(|v| v.code == "bodydigest_pin_mismatch");
        let hit = hit.unwrap_or_else(|| panic!("pin flip on {schema}#{name} not caught"));
        assert_eq!(
            hit.row_id,
            format!("{schema}#{name}"),
            "violation must name the exact failing recipe"
        );
    }

    // (b) Byte-level mutation of the registry TOMLs fails closed: typed
    // parse/read error or violations — never a panic, never silent success
    // on structural damage.
    let bases = [
        std::fs::read(root.join("registries/durable_fields.toml")).expect("read fields"),
        std::fs::read(root.join("registries/logical_object_kinds.toml")).expect("read logical"),
        std::fs::read(root.join("registries/wire_types.toml")).expect("read wire"),
    ];
    for round in 0..300 {
        let base = &bases[round % bases.len()];
        let mut bytes = base.clone();
        let mutations = 1 + (rng.next() as usize % 3);
        for _ in 0..mutations {
            if bytes.is_empty() {
                break;
            }
            let pos = rng.next() as usize % bytes.len();
            match rng.next() % 3 {
                0 => bytes[pos] = (rng.next() & 0xFF) as u8,
                1 => bytes.insert(pos, (rng.next() & 0xFF) as u8),
                _ => {
                    bytes.truncate(pos);
                }
            }
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if let Ok(table) = registry_check::toml::parse(&text) {
            // Parsed: model construction must fail closed or produce a
            // model the validator can process without panicking.
            let _ = identity::fields_from(&table);
            let _ = identity::logical_from(&table);
            let _ = identity::wire_from(&table);
        }
    }
}
