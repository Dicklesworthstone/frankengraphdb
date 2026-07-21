//! G0 claim-constitution suites (bead fgdb-g0-claim-registries-myx).
//!
//! Named suites required by the bead's acceptance criteria:
//!   claims_neg_missing_checker, claims_neg_waiver_present,
//!   claims_neg_statistical_in_invariants, claims_neg_unregistered_dependency,
//!   claims_hash_twenty_id_pin, claims_escalation_slo_cannot_justify_invariant,
//!   claims_proof_lane_manifest_resolves, claims_class_lattice_narrowing
//!   (property), claims_registry_toml_fuzz (fuzz).
//!
//! Every suite runs against the real `registries/` content plus targeted
//! in-memory mutations, so a defect in the shipped registries and a defect
//! in the checker are both build breaks.

use registry_check::closure;
use registry_check::hash::id_table_hash;
use registry_check::lint;
use registry_check::model::{self, Manifest, Registries, SloRow};
use registry_check::toml;
use registry_check::validate::{
    self, CANONICAL_CLASSES, check_justification, class_rank, expected_invariant_ids,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // tools/registry-check → repo root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

fn real_registries() -> Registries {
    model::load_registries(&repo_root().join("registries")).expect("real registries load")
}

/// A full invariants.toml text: the twenty-ID spine plus an optional clause
/// snippet appended after FG-INV-20 (so `[[invariant.clause]]` attaches to
/// the last invariant).
fn invariants_text_with(clause_snippet: &str) -> String {
    let mut out = String::from(
        "schema_version = 1\n\
         [registry]\n\
         name = \"invariants\"\n\
         allowed_claim_classes = [\"invariant\", \"proof\", \"bounded_model\"]\n\
         waiver_policy = \"forbidden\"\n\
         twenty_id_hash = \"fnv1a64:204a4b17c8ecc57f\"\n",
    );
    for i in 1..=20 {
        out.push_str(&format!(
            "[[invariant]]\nid = \"FG-INV-{i:02}\"\ntitle = \"spine row {i}\"\n"
        ));
    }
    out.push_str(clause_snippet);
    out
}

/// Default clause snippet under FG-INV-20; callers override single lines.
fn clause_snippet(overrides: &[(&str, &str)]) -> String {
    let mut fields: Vec<(&str, String)> = vec![
        ("key", "\"FG-INV-20.test\"".into()),
        ("claim_class", "\"invariant\"".into()),
        ("exact_statement", "\"test clause statement\"".into()),
        ("activation_predicate", "\"true\"".into()),
        ("dependencies", "[]".into()),
        ("checker_entrypoint", "\"claims_hash_twenty_id_pin\"".into()),
        (
            "negative_test_entrypoint",
            "\"claims_neg_waiver_present\"".into(),
        ),
        ("model_or_proof_scope", "\"n/a (test)\"".into()),
        ("owner", "\"g0-tests\"".into()),
        ("first_gate", "\"G1\"".into()),
        ("status", "\"live\"".into()),
        ("waiver", "\"forbidden\"".into()),
    ];
    for &(key, value) in overrides {
        if let Some(slot) = fields.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value.to_string();
        } else {
            fields.push((key, value.to_string()));
        }
    }
    let mut out = String::from("[[invariant.clause]]\n");
    for (key, value) in fields {
        out.push_str(&format!("{key} = {value}\n"));
    }
    out
}

/// Real registries with invariants replaced by a fixture text.
fn with_invariants_fixture(clause_snippet_text: &str) -> Registries {
    let text = invariants_text_with(clause_snippet_text);
    let table = toml::parse(&text).expect("fixture parses");
    let invariants = model::invariants_from(&table).expect("fixture models");
    Registries {
        invariants,
        ..real_registries()
    }
}

fn codes(r: &Registries) -> Vec<String> {
    validate::validate_all(r, &repo_root())
        .into_iter()
        .map(|v| v.code)
        .collect()
}

// ---------------------------------------------------------------------------
// Baseline: the shipped registries are clean and the closure compiles.
// ---------------------------------------------------------------------------

#[test]
fn claims_real_registries_validate() {
    let r = real_registries();
    let violations = validate::validate_all(&r, &repo_root());
    assert!(
        violations.is_empty(),
        "shipped registries must validate cleanly, found: {violations:?}"
    );
    // The shipped sample manifest compiles to a satisfied closure.
    let manifest =
        model::load_manifest(&repo_root().join("registries/sample_capability_manifest.toml"))
            .expect("sample manifest loads");
    let report = closure::compute(&r, &manifest);
    assert!(
        report.ok(),
        "sample-manifest closure must be satisfied: {report:?}"
    );
}

// ---------------------------------------------------------------------------
// Negative fixtures.
// ---------------------------------------------------------------------------

#[test]
fn claims_neg_missing_checker() {
    let r = with_invariants_fixture(&clause_snippet(&[(
        "checker_entrypoint",
        "\"no_such_symbol_anywhere\"",
    )]));
    let codes = codes(&r);
    assert!(
        codes.contains(&"missing_checker".to_string()),
        "expected missing_checker, got {codes:?}"
    );
}

#[test]
fn claims_neg_waiver_present() {
    let r = with_invariants_fixture(&clause_snippet(&[("waiver", "\"granted-until-2027\"")]));
    let codes = codes(&r);
    assert!(
        codes.contains(&"waiver_present".to_string()),
        "expected waiver_present, got {codes:?}"
    );
}

#[test]
fn claims_neg_statistical_in_invariants() {
    let r = with_invariants_fixture(&clause_snippet(&[("claim_class", "\"statistical\"")]));
    let codes = codes(&r);
    assert!(
        codes.contains(&"class_not_allowed".to_string()),
        "expected class_not_allowed, got {codes:?}"
    );
}

#[test]
fn claims_neg_unregistered_dependency() {
    let r = with_invariants_fixture(&clause_snippet(&[(
        "dependencies",
        "[\"FG-INV-99.ghost-clause\"]",
    )]));
    let codes = codes(&r);
    assert!(
        codes.contains(&"unregistered_dependency".to_string()),
        "expected unregistered_dependency, got {codes:?}"
    );
}

// ---------------------------------------------------------------------------
// The twenty-ID table hash pin.
// ---------------------------------------------------------------------------

#[test]
fn claims_hash_twenty_id_pin() {
    // The pinned constant. Changing the invariant spine REQUIRES changing
    // this pin in the same change, with review of the exact ID diff.
    const PIN: &str = "fnv1a64:204a4b17c8ecc57f";
    let r = real_registries();
    let ids: Vec<String> = r
        .invariants
        .invariants
        .iter()
        .map(|i| i.id.clone())
        .collect();
    assert_eq!(
        ids,
        expected_invariant_ids(),
        "spine must be FG-INV-01..20 in order"
    );
    assert_eq!(
        id_table_hash(&ids),
        PIN,
        "twenty-ID table hash pin mismatch"
    );
    assert_eq!(
        r.invariants.twenty_id_hash, PIN,
        "registry-declared pin mismatch"
    );

    // A twenty-first ID must fail with twenty_id_violation + hash_mismatch.
    let mut text = invariants_text_with("");
    text.push_str("[[invariant]]\nid = \"FG-INV-21\"\ntitle = \"illegal extra row\"\n");
    let table = toml::parse(&text).expect("fixture parses");
    let invariants = model::invariants_from(&table).expect("fixture models");
    let mutated = Registries {
        invariants,
        ..real_registries()
    };
    let codes = codes(&mutated);
    assert!(
        codes.contains(&"twenty_id_violation".to_string()),
        "expected twenty_id_violation, got {codes:?}"
    );
    assert!(
        codes.contains(&"hash_mismatch".to_string()),
        "expected hash_mismatch, got {codes:?}"
    );
}

// ---------------------------------------------------------------------------
// Cross-class escalation: an slo row can never justify an invariant clause.
// ---------------------------------------------------------------------------

#[test]
fn claims_escalation_slo_cannot_justify_invariant() {
    let mut r = with_invariants_fixture(&clause_snippet(&[("justified_by", "[\"FG-SLO-91\"]")]));
    r.slo.rows.push(SloRow {
        id: "FG-SLO-91".into(),
        claim_class: "slo".into(),
        kind: None,
        qualified_claim: "synthetic latency budget".into(),
        required_disclosures: vec!["fixture".into()],
        operation_class: Some("SnapshotQuery".into()),
        posture: Some("quorum-one".into()),
        audit_class: Some("NotRequired".into()),
    });
    let codes = codes(&r);
    assert!(
        codes.contains(&"class_escalation".to_string()),
        "an slo row justifying an invariant clause must fail CI, got {codes:?}"
    );
}

// ---------------------------------------------------------------------------
// Proof-lane manifests.
// ---------------------------------------------------------------------------

#[test]
fn claims_proof_lane_manifest_resolves() {
    // Every shipped lane row is well-formed (validated in the baseline
    // suite); here: a proof-class clause citing a registered lane passes,
    // an unknown lane fails, and a "checked" lane without its artifact fails.
    let ok = with_invariants_fixture(&clause_snippet(&[
        ("claim_class", "\"proof\""),
        ("proof_lane", "\"lean-mvcc-visibility\""),
    ]));
    assert!(
        !codes(&ok).contains(&"proof_lane_unresolved".to_string()),
        "registered lane must resolve"
    );

    let missing_lane = with_invariants_fixture(&clause_snippet(&[
        ("claim_class", "\"proof\""),
        ("proof_lane", "\"no-such-lane\""),
    ]));
    assert!(
        codes(&missing_lane).contains(&"proof_lane_unresolved".to_string()),
        "unknown lane must fail"
    );

    let no_lane = with_invariants_fixture(&clause_snippet(&[("claim_class", "\"proof\"")]));
    assert!(
        codes(&no_lane).contains(&"proof_lane_unresolved".to_string()),
        "proof-class clause without a lane must fail"
    );

    let mut checked_missing_artifact = real_registries();
    if let Some(lane) = checked_missing_artifact.proof_lanes.first_mut() {
        lane.status = "checked".into();
        lane.artifact = "formal/lean/DoesNotExistYet.lean".into();
    }
    assert!(
        codes(&checked_missing_artifact).contains(&"artifact_missing".to_string()),
        "checked lane with missing artifact must fail"
    );
}

// ---------------------------------------------------------------------------
// Property: the class lattice admits only weaker-informs-stronger; an
// enforce/justify edge from a weaker class to a stronger one is never
// representable without a violation.
// ---------------------------------------------------------------------------

#[test]
fn claims_class_lattice_narrowing() {
    for (claim_class, claim_rank) in CANONICAL_CLASSES {
        for (justifier_class, justifier_rank) in CANONICAL_CLASSES {
            let mut ranks = BTreeMap::new();
            ranks.insert("J-ROW".to_string(), justifier_rank);
            let mut out = Vec::new();
            check_justification(
                "FG-INV-01.property",
                claim_class,
                &["J-ROW".to_string()],
                &ranks,
                "invariants",
                &mut out,
            );
            let escalated = out.iter().any(|v| v.code == "class_escalation");
            if justifier_rank < claim_rank {
                assert!(
                    escalated,
                    "{justifier_class} (rank {justifier_rank}) must not justify {claim_class} (rank {claim_rank})"
                );
            } else {
                assert!(
                    !escalated,
                    "{justifier_class} (rank {justifier_rank}) may justify {claim_class} (rank {claim_rank})"
                );
            }
        }
    }
    // Rank table sanity: the canonical order from §1.11.
    assert_eq!(class_rank("invariant"), Some(6));
    assert_eq!(class_rank("benchmark"), Some(1));
    assert_eq!(class_rank("nonsense"), None);
}

// ---------------------------------------------------------------------------
// Fuzz: mutated registry bytes fail closed with a typed error, never a panic.
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
fn claims_registry_toml_fuzz() {
    // Deterministic seed: the fuzz corpus is replayable by construction.
    let mut rng = XorShift64(0x00F6_DB01_C1A1_5EED);
    let bases = [
        std::fs::read(repo_root().join("registries/invariants.toml")).expect("read invariants"),
        std::fs::read(repo_root().join("registries/constitution.toml")).expect("read constitution"),
        std::fs::read(repo_root().join("registries/evidence.toml")).expect("read evidence"),
    ];
    let mut parsed_ok = 0u32;
    let mut parse_err = 0u32;
    for round in 0..600 {
        let base = &bases[round % bases.len()];
        let mut bytes = base.clone();
        // 1–4 byte-level mutations: overwrite, insert, or truncate.
        let mutations = 1 + (rng.next() as usize % 4);
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
        // Must return Ok or a typed error — a panic aborts the test.
        match toml::parse(&text) {
            Ok(table) => {
                parsed_ok += 1;
                // Model construction over mutated-but-parsable input must
                // also fail closed, never panic.
                let _ = model::invariants_from(&table);
                let _ = model::constitution_from(&table);
                let _ = model::evidence_from(&table);
            }
            Err(e) => {
                parse_err += 1;
                assert!(e.line > 0, "typed error carries a line number");
            }
        }
    }
    // Sanity: the corpus exercised both outcomes.
    assert!(
        parsed_ok > 0,
        "fuzz corpus never parsed — mutations too destructive"
    );
    assert!(
        parse_err > 0,
        "fuzz corpus never failed — mutations too gentle"
    );
}

// ---------------------------------------------------------------------------
// claims-lint marker extraction and the shipped prose corpus.
// ---------------------------------------------------------------------------

#[test]
fn claims_lint_marker_extraction() {
    assert_eq!(
        lint::markers_in_line("see FG-INV-04 and FG-CAL-01."),
        vec!["FG-INV-04".to_string(), "FG-CAL-01".to_string()]
    );
    // Wildcards and over-long digit runs are not claim citations.
    assert!(lint::markers_in_line("the FG-INV-* namespace").is_empty());
    assert!(lint::markers_in_line("FG-INV-012 is not a marker").is_empty());
    // Alphanumeric left boundary suppresses a match.
    assert!(lint::markers_in_line("XFG-INV-01").is_empty());
    // Slash-continued lists yield exactly the leading marker.
    assert_eq!(
        lint::markers_in_line("verification (FG-INV-08/09/10)"),
        vec!["FG-INV-08".to_string()]
    );
}

#[test]
fn claims_lint_shipped_prose_is_clean() {
    let root = repo_root();
    let r = real_registries();
    let config = lint::load_config(&root.join("registries/claims_lint.toml")).expect("config");
    let registered = lint::registered_markers(&r);
    let hits = lint::run(&root, &config, &registered).expect("lint runs");
    assert!(
        hits.is_empty(),
        "normative prose cites unregistered claim markers: {hits:?}"
    );
}

#[test]
fn claims_closure_absent_capability_is_attributed() {
    // A stub clause guarded by a feature: enabling the feature must surface
    // the exact clause behind the absent capability.
    let r = with_invariants_fixture(&clause_snippet(&[
        ("activation_predicate", "\"feature-x\""),
        ("status", "\"stub\""),
    ]));
    let manifest = Manifest {
        name: "test".into(),
        features: vec!["feature-x".into()],
        postures: vec![],
        roles: vec![],
    };
    let report = closure::compute(&r, &manifest);
    assert!(!report.ok());
    assert!(report.absent.contains("FG-INV-20.test"));
    let attributed = report
        .absent_capabilities
        .get("feature-x")
        .expect("capability attributed");
    assert!(attributed.contains("FG-INV-20.test"));

    // Without the feature the clause is unreachable and the closure holds.
    let empty = Manifest {
        name: "empty".into(),
        features: vec![],
        postures: vec![],
        roles: vec![],
    };
    assert!(closure::compute(&r, &empty).ok());
}
