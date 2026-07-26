//! Invariant-spine suites (bead fgdb-g0-invariant-spine-tmm).
//!
//! Named suites required by the bead's acceptance criteria:
//!   spine_exactly_twenty_ids, spine_hash_pin,
//!   spine_clause_entrypoints_resolve, spine_dag_acyclic,
//!   spine_neg_twenty_first_id, spine_neg_inactive_reachable_clause,
//!   spine_gate_table_classes, spine_clause_key_stability (property).
//!
//! The registry payload under test is the REAL `registries/invariants.toml`
//! (Appendix F materialized verbatim), plus targeted in-memory mutations.

use registry_check::closure;
use registry_check::hash::id_table_hash;
use registry_check::model::{self, Manifest, Registries};
use registry_check::toml;
use registry_check::validate::{self, expected_invariant_ids};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const PIN: &str = "fnv1a64:204a4b17c8ecc57f";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

fn real_registries() -> Registries {
    model::load_registries(&repo_root().join("registries")).expect("real registries load")
}

fn manifest(features: &[&str]) -> Manifest {
    Manifest {
        name: "spine-test".into(),
        features: features.iter().map(|s| s.to_string()).collect(),
        postures: vec![],
        roles: vec![],
    }
}

#[test]
fn spine_exactly_twenty_ids() {
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
    // The spine is materialized: every top-level ID carries at least one
    // clause with a non-empty verbatim statement and enforcement column.
    for inv in &r.invariants.invariants {
        assert!(
            !inv.clauses.is_empty(),
            "{} has no materialized clause",
            inv.id
        );
        for c in &inv.clauses {
            assert!(
                !c.exact_statement.trim().is_empty(),
                "{} clause {} has empty statement",
                inv.id,
                c.key
            );
            assert!(
                c.enforcement
                    .as_deref()
                    .is_some_and(|e| !e.trim().is_empty()),
                "{} clause {} is missing the Appendix F enforcement column",
                inv.id,
                c.key
            );
            assert_eq!(c.waiver, "forbidden", "{} waiver must be forbidden", c.key);
        }
    }
}

#[test]
fn spine_hash_pin() {
    let r = real_registries();
    let ids: Vec<String> = r
        .invariants
        .invariants
        .iter()
        .map(|i| i.id.clone())
        .collect();
    assert_eq!(id_table_hash(&ids), PIN);
    assert_eq!(r.invariants.twenty_id_hash, PIN);
}

#[test]
fn spine_clause_entrypoints_resolve() {
    let r = real_registries();
    let symbols: BTreeSet<&str> = r.checker_index.iter().map(|c| c.symbol.as_str()).collect();
    for inv in &r.invariants.invariants {
        for c in &inv.clauses {
            assert!(
                symbols.contains(c.checker_entrypoint.as_str()),
                "{}: checker {} not registered",
                c.key,
                c.checker_entrypoint
            );
            assert!(
                symbols.contains(c.negative_test_entrypoint.as_str()),
                "{}: negative test {} not registered",
                c.key,
                c.negative_test_entrypoint
            );
        }
    }
    // And the validator agrees end-to-end.
    let violations = validate::validate_all(&r, &repo_root());
    assert!(
        !violations.iter().any(|v| v.code == "missing_checker"),
        "missing_checker on shipped spine: {violations:?}"
    );
}

#[test]
fn spine_dag_acyclic() {
    // Shipped spine: no dependency cycle.
    let r = real_registries();
    let violations = validate::validate_all(&r, &repo_root());
    assert!(
        !violations.iter().any(|v| v.code == "dependency_cycle"),
        "shipped spine has a dependency cycle: {violations:?}"
    );
    // Mutation: closing a cycle (03 -> 04 while 04 -> 03) must be reported.
    let mut mutated = r.clone();
    for inv in &mut mutated.invariants.invariants {
        if inv.id == "FG-INV-03" {
            for c in &mut inv.clauses {
                c.dependencies.push("FG-INV-04.core".into());
            }
        }
    }
    let violations = validate::validate_all(&mutated, &repo_root());
    assert!(
        violations.iter().any(|v| v.code == "dependency_cycle"),
        "expected dependency_cycle, got {violations:?}"
    );
}

#[test]
fn spine_neg_twenty_first_id() {
    let path = repo_root().join("registries/invariants.toml");
    let mut text = std::fs::read_to_string(&path).expect("read invariants.toml");
    text.push_str("\n[[invariant]]\nid = \"FG-INV-21\"\ntitle = \"illegal extra row\"\n");
    let table = toml::parse(&text).expect("fixture parses");
    let invariants = model::invariants_from(&table).expect("fixture models");
    let mutated = Registries {
        invariants,
        ..real_registries()
    };
    let codes: Vec<String> = validate::validate_all(&mutated, &repo_root())
        .into_iter()
        .map(|v| v.code)
        .collect();
    assert!(
        codes.contains(&"twenty_id_violation".to_string()),
        "got {codes:?}"
    );
    assert!(
        codes.contains(&"hash_mismatch".to_string()),
        "got {codes:?}"
    );
}

#[test]
fn spine_neg_inactive_reachable_clause() {
    // Enabling a capability whose clause is still a stub must fail the
    // closure NAMING the exact clause — a reachable non-live clause cannot
    // count as covered, and the capability is absent.
    let r = real_registries();
    let report = closure::compute(&r, &manifest(&["mvcc-visibility"]));
    assert!(!report.ok(), "stub clause reachable yet closure passed");
    assert!(
        report.absent.contains("FG-INV-04.core"),
        "absent set must name FG-INV-04.core: {:?}",
        report.absent
    );
    // Dependency expansion pulls in the (also stub) dependency clauses.
    assert!(
        report.absent.contains("FG-INV-03.core") && report.absent.contains("FG-INV-08.core"),
        "dependency closure must surface FG-INV-03/08 stubs: {:?}",
        report.absent
    );
    // Capability attribution names the offending atom.
    assert!(
        report
            .absent_capabilities
            .get("mvcc-visibility")
            .is_some_and(|cs| cs.contains("FG-INV-04.core")),
        "capability attribution missing: {:?}",
        report.absent_capabilities
    );
    // The empty (pre-Genesis) manifest remains satisfied.
    assert!(closure::compute(&r, &manifest(&[])).ok());
}

#[test]
fn spine_gate_table_classes() {
    let r = real_registries();
    // FG-CAL-01..03 and FG-EVID-01..05 in evidence.toml: statistical class
    // with their required-disclosure fields. FG-EVID-05 (leakage/channel-capacity
    // measurement) was added by fgdb-g0-threat-model-nqd, which needed a carrier
    // for the derived-state/observable-metadata asset and found none of 01..04
    // fit. This frozen list is the "same-commit-update" half of that change.
    let expected_evidence = [
        "FG-CAL-01",
        "FG-CAL-02",
        "FG-CAL-03",
        "FG-EVID-01",
        "FG-EVID-02",
        "FG-EVID-03",
        "FG-EVID-04",
        "FG-EVID-05",
    ];
    let actual: Vec<&str> = r.evidence.rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(actual, expected_evidence, "evidence gate table drifted");
    for row in &r.evidence.rows {
        assert_eq!(row.claim_class, "statistical", "{} class", row.id);
        assert!(
            !row.required_disclosures.is_empty(),
            "{} disclosures",
            row.id
        );
    }
    // FG-CFG-01..04 in slo.toml: configuration-model claims, never invariants.
    let expected_cfg = ["FG-CFG-01", "FG-CFG-02", "FG-CFG-03", "FG-CFG-04"];
    let actual: Vec<&str> = r.slo.rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        actual, expected_cfg,
        "configuration-model gate table drifted"
    );
    for row in &r.slo.rows {
        assert_eq!(row.claim_class, "bounded_model", "{} class", row.id);
        assert_eq!(
            row.kind.as_deref(),
            Some("configuration_model"),
            "{} kind",
            row.id
        );
        assert!(
            !row.required_disclosures.is_empty(),
            "{} disclosures",
            row.id
        );
    }
}

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
fn spine_clause_key_stability() {
    // Property: randomized subordinate-clause additions never mint a new
    // top-level ID — the ID set and its hash pin are invariant under any
    // number of legal clause additions.
    let base = std::fs::read_to_string(repo_root().join("registries/invariants.toml"))
        .expect("read invariants.toml");
    let mut rng = XorShift64(0x5917E_C1A05E);
    for round in 0..40 {
        // Regenerate the file text with random clause additions appended
        // under random invariants (text-level, honoring array-of-tables
        // attachment: insert immediately after the chosen [[invariant]]
        // block's existing clauses — appending at end attaches to FG-INV-20,
        // so rotate through end-of-file additions with distinct keys).
        let mut text = base.clone();
        let additions = 1 + (rng.next() as usize % 4);
        for a in 0..additions {
            let n = 20; // appended clauses attach to the last invariant
            let key = format!("FG-INV-{n:02}.rand-{round}-{a}");
            text.push_str(&format!(
                "\n[[invariant.clause]]\n\
                 key = \"{key}\"\n\
                 claim_class = \"invariant\"\n\
                 exact_statement = \"randomized subordinate clause (property fixture)\"\n\
                 activation_predicate = \"rand-feature-{round}\"\n\
                 dependencies = []\n\
                 checker_entrypoint = \"fg_inv_20_core_checker\"\n\
                 negative_test_entrypoint = \"fg_inv_20_core_negative\"\n\
                 model_or_proof_scope = \"property fixture\"\n\
                 owner = \"g0-tests\"\n\
                 first_gate = \"G1\"\n\
                 status = \"stub\"\n\
                 waiver = \"forbidden\"\n"
            ));
        }
        let table = toml::parse(&text).expect("property fixture parses");
        let invariants = model::invariants_from(&table).expect("property fixture models");
        let ids: Vec<String> = invariants.invariants.iter().map(|i| i.id.clone()).collect();
        assert_eq!(
            ids,
            expected_invariant_ids(),
            "clause additions minted a new ID"
        );
        assert_eq!(id_table_hash(&ids), PIN, "clause additions changed the pin");
        let mutated = Registries {
            invariants,
            ..real_registries()
        };
        let violations = validate::validate_all(&mutated, &repo_root());
        assert!(
            !violations
                .iter()
                .any(|v| v.code == "twenty_id_violation" || v.code == "hash_mismatch"),
            "round {round}: spine violation from clause additions: {violations:?}"
        );
    }
}
