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
use registry_check::model::{self, Manifest, Registries, ScriptDisposition, SloRow};
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
         twenty_id_hash = \"fnv1a64:204a4b17c8ecc57f\"\n\
         expected_enforced_clauses = 0\n\
         expected_enforced_invariants = 0\n\
         capability_atoms = [\"feature-x\", \"feature-y\"]\n",
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

#[test]
fn claims_script_closure_reaches_nested_deliverables() {
    // Concurrent runs may share this fixture safely: they write the same bytes,
    // validation is read-only, and no run deletes the retained evidence.
    let root = std::env::temp_dir().join("fgdb-fknh-nested-script");
    let nested = root.join("scripts/lib/unclaimed.sh");
    std::fs::create_dir_all(nested.parent().expect("nested script parent"))
        .expect("nested script directory");
    std::fs::write(&nested, "#!/usr/bin/env bash\n").expect("nested script fixture");

    let mut registries = real_registries();
    let exact_violation = |registries: &Registries| {
        validate::validate_all(registries, &root)
            .into_iter()
            .any(|v| v.code == "script_undeclared" && v.row_id == "scripts/lib/unclaimed.sh")
    };

    assert!(
        exact_violation(&registries),
        "an undeclared nested script must be inside the file-to-row closure"
    );

    registries.script_dispositions.push(ScriptDisposition {
        path: "scripts/lib/unclaimed.sh".into(),
        role: "library".into(),
        reason: "fixture source library; its caller owns the assertions".into(),
    });
    assert!(
        !exact_violation(&registries),
        "a declared nested source library is the conformant control"
    );
    let bad_role = validate::validate_all(&registries, &root)
        .into_iter()
        .any(|v| v.code == "bad_field" && v.row_id == "scripts/lib/unclaimed.sh");
    assert!(!bad_role, "library must be a closed-vocabulary role");
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

/// The second law in `proof_lanes.toml`'s header, which was prose and no code:
/// "A proof-class clause may cite a declared lane **only while its own status is
/// `stub`**" (`fgdb-proof-lane-checked-is-only-file-existence-0f1l`).
///
/// A `declared` lane's artifact does not exist yet — the registry says so in the
/// same sentence. A clause promoted off `stub` while citing one has been
/// promoted against a proof nobody has written, and `validate` checked only that
/// the lane id RESOLVED.
///
/// The control comes first and it is the load-bearing half: all ten clauses that
/// cite a lane are `stub` today, so a test that only asserted the shipped tree is
/// clean would pass with the rule deleted.
#[test]
fn claims_proof_lane_declared_requires_a_stub_clause() {
    let real = real_registries();
    let citing: Vec<_> = real
        .invariants
        .invariants
        .iter()
        .flat_map(|invariant| invariant.clauses.iter())
        .filter(|clause| clause.proof_lane.is_some())
        .collect();
    assert!(
        !citing.is_empty(),
        "control: no shipped clause cites a proof lane at all, so this law is \
         quantified over nothing"
    );
    assert!(
        real.proof_lanes
            .iter()
            .any(|lane| lane.status == "declared"),
        "control: no shipped lane is \"declared\", so the illegal combination cannot be \
         formed from real rows"
    );
    assert!(
        !codes(&real).contains(&"proof_lane_declared_while_clause_promoted".to_string()),
        "the shipped tree must satisfy the law it states"
    );

    // The mutation, in both illegal directions. `dormant` is in the clause status
    // vocabulary and is not `stub`, so it is illegal too — a rule written as
    // `!= "live"` would pass the first mutant and miss the second.
    for status in ["live", "dormant"] {
        let mut promoted = real_registries();
        let mut mutated = None;
        'outer: for invariant in promoted.invariants.invariants.iter_mut() {
            for clause in invariant.clauses.iter_mut() {
                let cites_declared = clause.proof_lane.as_ref().is_some_and(|id| {
                    real.proof_lanes
                        .iter()
                        .any(|lane| &lane.id == id && lane.status == "declared")
                });
                if cites_declared && clause.status == "stub" {
                    clause.status = status.to_string();
                    mutated = Some(clause.key.clone());
                    break 'outer;
                }
            }
        }
        let key = mutated.expect("a stub clause citing a declared lane exists to mutate");
        assert!(
            codes(&promoted).contains(&"proof_lane_declared_while_clause_promoted".to_string()),
            "promoting {key} to {status:?} while its lane is still \"declared\" must fail \
             CI, got {:?}",
            codes(&promoted)
        );
    }

    // The other direction: promoting the LANE instead of the clause is legal, so
    // the rule must not fire on it. A rule that rejected every non-stub clause
    // citing any lane would pass every assertion above and block Genesis.
    let mut both_promoted = real_registries();
    let mut lane_id = None;
    'outer: for invariant in both_promoted.invariants.invariants.iter_mut() {
        for clause in invariant.clauses.iter_mut() {
            if let Some(id) = clause.proof_lane.clone() {
                clause.status = "live".into();
                lane_id = Some(id);
                break 'outer;
            }
        }
    }
    let lane_id = lane_id.expect("a clause citing a lane exists");
    for lane in both_promoted.proof_lanes.iter_mut() {
        if lane.id == lane_id {
            lane.status = "checked".into();
        }
    }
    assert!(
        !codes(&both_promoted).contains(&"proof_lane_declared_while_clause_promoted".to_string()),
        "a live clause citing a CHECKED lane is the legal combination and must not fire \
         this rule"
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
    let (hits, census) = lint::run(&root, &config, &registered).expect("lint runs");
    assert!(
        hits.is_empty(),
        "claims-lint hits on the shipped prose: {hits:?}"
    );

    // A green lint proves nothing until it says what it opened; every number
    // below was measured on this tree (6 scanned files, 117 markers, 12 prose
    // artifacts, 13 gate rows). They are FLOORS, not pins: another pane adding
    // a citation or a document must not turn this suite red.
    assert!(
        census.files_scanned >= 6,
        "scan set shrank below the measured six artifacts: {census:?}"
    );
    assert!(
        census.markers_seen >= 100,
        "marker scan collapsed — 117 markers were measured on this tree: {census:?}"
    );
    assert!(
        census.prose_files_seen >= 12,
        "closure walk collapsed — 12 prose artifacts were measured: {census:?}"
    );
    assert!(
        census.gate_rows_read >= 13,
        "the README gate table lost rows — 13 were measured: {census:?}"
    );
    assert_eq!(
        census.gate_rows_read,
        census.gate_rows_marked + census.gate_rows_unmarked,
        "every gate row is marked or unmarked: {census:?}"
    );

    // The measured gap, as a CEILING that can only close. slo.toml holds zero
    // `slo`/`benchmark` rows today, so all thirteen README gate rows cite no
    // marker (fgdb-claims-lint-one-directional-unmarked-budgets-sdpv). Minting
    // a row and citing it lowers this number; nothing can raise it without
    // failing here, and adding a fourteenth unmarked row fails in the lint
    // itself rather than here.
    assert!(
        census.gate_rows_unmarked <= 13,
        "unmarked gate budgets grew past the measured thirteen: {census:?}"
    );
}

// ---------------------------------------------------------------------------
// claims-lint direction 2 and the closure laws
// (bead fgdb-claims-lint-one-directional-unmarked-budgets-sdpv).
//
// Every negative below is a MUTATION of one fixture whose base is green, so a
// hit is attributable to the mutation and to nothing else. The base itself is
// asserted for content, not just emptiness: an equivalence over two vacuous
// runs proves nothing, which is the exact defect this direction exists to fix.
// ---------------------------------------------------------------------------

const LINT_FIXTURE_README: &str = "\
# Fixture

## Performance

Numbers below are provisional CI gates on the reference machine (32-core, 256 GB RAM).

| Domain | Gate |
|---|---|
| Cold bulk load | ≥ 40M edges/s sustained |
| Point reads | ≥ 8M lookups/s; p99 < 15 µs warm |
| Branch create | O(1), < 100 µs (FG-SLO-01) |

## Determinism

Everything here is prose, and prose is not a gate table.
";

const LINT_FIXTURE_GUIDE: &str = "A scanned guide that cites FG-SLO-01.\n";
const LINT_FIXTURE_HISTORY: &str = "A historical draft citing FG-INV-27, which was never minted.\n";

/// One fixture tree + config. Mutations are applied by the caller between
/// `build` and `run`, so the base is provably the only difference.
struct LintFixture {
    root: PathBuf,
}

impl LintFixture {
    /// `tag` must be unique per test: `cargo test` runs these in parallel and
    /// the builder opens a fixture by destroying it.
    fn build(tag: &str) -> LintFixture {
        let root =
            std::env::temp_dir().join(format!("fgdb-claims-lint-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs")).expect("fixture docs dir");
        std::fs::create_dir_all(root.join("registries")).expect("fixture registries dir");
        std::fs::write(root.join("README.md"), LINT_FIXTURE_README).expect("fixture README");
        std::fs::write(root.join("docs/GUIDE.md"), LINT_FIXTURE_GUIDE).expect("fixture guide");
        std::fs::write(root.join("HISTORY.md"), LINT_FIXTURE_HISTORY).expect("fixture history");
        let f = LintFixture { root };
        f.write_config(
            &["README.md", "docs/GUIDE.md"],
            "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n",
            &["Cold bulk load", "Point reads"],
        );
        f
    }

    fn write_config(&self, scan: &[&str], excludes: &str, unmarked_rows: &[&str]) {
        self.write_config_roots(&[".", "docs"], scan, excludes, unmarked_rows);
    }

    /// Same, with the closure roots under the test's control. Only the roots
    /// law needs this; every other test wants the default pair.
    fn write_config_roots(
        &self,
        roots: &[&str],
        scan: &[&str],
        excludes: &str,
        unmarked_rows: &[&str],
    ) {
        let quoted = |v: &[&str]| {
            v.iter()
                .map(|s| format!("  {:?},\n", s))
                .collect::<String>()
        };
        let roots_toml = roots
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!(
            "schema_version = 1\n\n[lint]\nmarker_pattern = \"{}\"\nscan = [\n{}]\nclosure_dirs = [{roots_toml}]\n\n[[gate_table]]\nfile = \"README.md\"\nheading = \"## Performance\"\nowner_bead = \"fgdb-fixture\"\nunmarked_rows = [\n{}]\n\n{excludes}",
            lint::SUPPORTED_MARKER_PATTERN,
            quoted(scan),
            quoted(unmarked_rows),
        );
        std::fs::write(self.root.join("registries/claims_lint.toml"), text)
            .expect("fixture config");
    }

    fn config(&self) -> Result<lint::LintConfig, lint::LintError> {
        lint::load_config(&self.root.join("registries/claims_lint.toml"))
    }

    fn run(&self) -> Result<(Vec<lint::LintHit>, lint::LintCensus), lint::LintError> {
        let registered: std::collections::BTreeSet<String> =
            ["FG-SLO-01".to_string()].into_iter().collect();
        lint::run(
            &self.root,
            &self.config().expect("fixture config"),
            &registered,
        )
    }

    fn hits_of(&self, kind: lint::HitKind) -> Vec<lint::LintHit> {
        let (hits, _) = self.run().expect("fixture lint runs");
        hits.into_iter().filter(|h| h.kind == kind).collect()
    }
}

#[test]
fn claims_lint_fixture_base_is_green_and_not_vacuous() {
    let f = LintFixture::build("base");
    let (hits, census) = f.run().expect("base lint runs");
    assert!(hits.is_empty(), "base fixture is not green: {hits:?}");
    // Named base numbers. Every negative below is stated as a delta from these.
    assert_eq!(census.files_scanned, 2, "{census:?}");
    assert_eq!(census.gate_rows_read, 3, "{census:?}");
    assert_eq!(census.gate_rows_marked, 1, "{census:?}");
    assert_eq!(census.gate_rows_unmarked, 2, "{census:?}");
    assert_eq!(census.prose_files_seen, 3, "{census:?}");
}

#[test]
fn claims_neg_lint_region_that_examines_nothing_is_a_hard_error() {
    // THE VACUITY CONTROL. The failure mode this whole direction exists to
    // catch is a check that passes because it examined nothing, so the reader
    // losing its region must be a loud error and never a clean pass. Two
    // mutations, both meaning "the gate table is gone".
    let f = LintFixture::build("vacuous");

    // (a) heading present, table deleted.
    std::fs::write(
        f.root.join("README.md"),
        "# Fixture\n\n## Performance\n\nNo table here at all.\n\n## Determinism\n\nProse.\n",
    )
    .expect("mutate README");
    let err = f.run().expect_err("a region with no table must not pass");
    assert!(
        err.to_string().contains("no table between them"),
        "unexpected error: {err}"
    );

    // (b) heading present, table present, zero data rows.
    std::fs::write(
        f.root.join("README.md"),
        "# Fixture\n\n## Performance\n\n| Domain | Gate |\n|---|---|\n\n## Determinism\n",
    )
    .expect("mutate README");
    let err = f.run().expect_err("an empty gate table must not pass");
    assert!(
        err.to_string().contains("would examine nothing"),
        "unexpected error: {err}"
    );

    // (c) the heading itself moved.
    std::fs::write(
        f.root.join("README.md"),
        "# Fixture\n\n## Speed\n\nProse.\n",
    )
    .expect("mutate README");
    let err = f.run().expect_err("a missing region must not pass");
    assert!(
        err.to_string().contains("does not exist"),
        "unexpected error: {err}"
    );
}

#[test]
fn claims_neg_unmarked_gate_row() {
    // Direction 2. A fourth gate row states a budget and cites nothing: 3 rows
    // → 4, 0 hits → 1, and the hit names the row.
    let f = LintFixture::build("unmarked-row");
    let mutated = LINT_FIXTURE_README.replace(
        "| Branch create |",
        "| Recovery | < 30 s to first query |\n| Branch create |",
    );
    std::fs::write(f.root.join("README.md"), &mutated).expect("mutate README");
    let (hits, census) = f.run().expect("lint runs");
    assert_eq!(census.gate_rows_read, 4, "{census:?}");
    assert_eq!(census.gate_rows_unmarked, 3, "{census:?}");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].kind, lint::HitKind::UnmarkedGateRow);
    assert_eq!(hits[0].subject, "Recovery");
    assert_eq!(hits[0].file, "README.md");
    assert_eq!(
        hits[0].line, 11,
        "the hit must carry the source line: {hits:?}"
    );
}

#[test]
fn claims_neg_dead_gate_exemption_when_the_row_is_gone() {
    // The ledger's converse, half one: an entry naming a row that no longer
    // exists is stale, and a stale entry is a free pass waiting for a row of
    // the same name.
    let f = LintFixture::build("dead-exemption");
    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n",
        &["Cold bulk load", "Point reads", "Vector search"],
    );
    let hits = f.hits_of(lint::HitKind::DeadGateExemption);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].subject, "Vector search");
    assert!(hits[0].text.contains("no such row"), "{hits:?}");
}

#[test]
fn claims_neg_dead_gate_exemption_when_the_row_is_now_marked() {
    // The ledger's converse, half two — and the direction that makes progress
    // visible. Citing a marker on a ledgered row without deleting its entry
    // must fail, so the measured gap can only move deliberately.
    let f = LintFixture::build("now-marked");
    let mutated = LINT_FIXTURE_README.replace(
        "| Cold bulk load | ≥ 40M edges/s sustained |",
        "| Cold bulk load | ≥ 40M edges/s sustained (FG-SLO-01) |",
    );
    std::fs::write(f.root.join("README.md"), &mutated).expect("mutate README");
    let (hits, census) = f.run().expect("lint runs");
    assert_eq!(census.gate_rows_marked, 2, "{census:?}");
    assert_eq!(census.gate_rows_unmarked, 1, "{census:?}");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].kind, lint::HitKind::DeadGateExemption);
    assert_eq!(hits[0].subject, "Cold bulk load");
    assert!(hits[0].text.contains("FG-SLO-01"), "{hits:?}");
}

#[test]
fn claims_neg_unclaimed_prose() {
    // The closure law. A new artifact in a closure directory that neither list
    // names is unclaimed: 3 prose files → 4, 0 hits → 1.
    let f = LintFixture::build("unclaimed");
    std::fs::write(
        f.root.join("docs/NEW_NOTE.md"),
        "A new normative-looking document nobody pointed the lint at.\n",
    )
    .expect("add prose");
    let (hits, census) = f.run().expect("lint runs");
    assert_eq!(census.prose_files_seen, 4, "{census:?}");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].kind, lint::HitKind::UnclaimedProse);
    assert_eq!(hits[0].subject, "docs/NEW_NOTE.md");

    // Hidden entries are not deliverables, and the `._*.md` AppleDouble forks
    // beside the real plan documents must not become closure obligations.
    std::fs::write(f.root.join("._SHADOW.md"), "resource fork\n").expect("add fork");
    let (hits, census2) = f.run().expect("lint runs");
    assert_eq!(
        census2.prose_files_seen, 4,
        "hidden entries are skipped: {census2:?}"
    );
    assert_eq!(hits.len(), 1, "{hits:?}");
}

#[test]
fn claims_neg_unclaimed_prose_below_the_closure_root() {
    // THE DEPTH OF THE LAW, which is what fgdb-claims-lint-scan-set-not-total-nldg
    // is actually about. The closure walk used to read each root one level deep.
    // MEASURED 2026-07-26 at b77982e that made it total over the corpus as it
    // stood -- all 11 tracked `.md` sit in `.` or `docs/` -- and blind
    // everywhere else: the repository has 50 tracked directories below depth 1,
    // and `crates/fgdb-bigint/README.md` carrying the exact text this lint
    // exists to catch left `registry-check all` at `failures: 0, outcome: pass`,
    // exit 0. A law that is total only by where files happen to sit today is a
    // coincidence, not a law.
    let f = LintFixture::build("unclaimed-deep");
    std::fs::create_dir_all(f.root.join("crates/demo/notes")).expect("nested dirs");
    std::fs::write(
        f.root.join("crates/demo/notes/DESIGN.md"),
        "This guarantees FG-INV-01 and is proven correct.\n",
    )
    .expect("add nested prose");
    let (hits, census) = f.run().expect("lint runs");
    // 3 prose files -> 4, 0 hits -> 1, and the hit names the nested path.
    assert_eq!(census.prose_files_seen, 4, "{census:?}");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].kind, lint::HitKind::UnclaimedProse);
    assert_eq!(hits[0].subject, "crates/demo/notes/DESIGN.md");

    // Naming it settles it, exactly as it does at the root: the file becomes a
    // scanned artifact and the corpus is accounted for again.
    f.write_config(
        &["README.md", "docs/GUIDE.md", "crates/demo/notes/DESIGN.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let (hits, census) = f.run().expect("lint runs");
    assert_eq!(census.prose_files_seen, 4, "{census:?}");
    assert_eq!(census.files_scanned, 3, "{census:?}");
    assert!(
        hits.iter().all(|h| h.kind != lint::HitKind::UnclaimedProse),
        "{hits:?}"
    );
}

#[test]
fn claims_closure_prune_narrows_the_walk_and_must_stay_live() {
    // A recursive walk needs one declared escape or a build-output tree drags
    // every vendored dependency's prose into the obligation. `[[closure_prune]]`
    // is that escape, and it is held to the denylist's discipline: it carries a
    // reason, and a `presence = "required"` prune whose directory is gone is a
    // dead rule. Both halves are proven here off the SAME fixture, so the flag
    // is the whole difference.
    let f = LintFixture::build("closure-prune");
    std::fs::create_dir_all(f.root.join("build/doc")).expect("build dir");
    std::fs::write(
        f.root.join("build/doc/VENDORED.md"),
        "Vendored dependency prose that guarantees FG-INV-01.\n",
    )
    .expect("add vendored prose");

    // Without a prune the walk reaches it and it is unclaimed: 3 -> 4, 0 -> 1.
    let (hits, census) = f.run().expect("lint runs");
    assert_eq!(census.prose_files_seen, 4, "{census:?}");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].subject, "build/doc/VENDORED.md");

    // With it, the subtree is not walked at all: back to 3 prose files, 0 hits.
    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n\
         [[closure_prune]]\ndir = \"build\"\nreason = \"build output\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let (hits, census) = f.run().expect("lint runs");
    assert_eq!(census.prose_files_seen, 3, "{census:?}");
    assert!(hits.is_empty(), "{hits:?}");

    // A required prune that names nothing narrows nothing, and says so.
    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n\
         [[closure_prune]]\ndir = \"build\"\nreason = \"build output\"\n\n\
         [[closure_prune]]\ndir = \"vanished\"\nreason = \"a directory that is gone\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let hits = f.hits_of(lint::HitKind::DeadPrune);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].subject, "vanished");
    assert_eq!(hits[0].kind.code(), "dead_closure_prune");

    // presence = "optional" is the declared escape, same absent path.
    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n\
         [[closure_prune]]\ndir = \"build\"\nreason = \"build output\"\n\n\
         [[closure_prune]]\ndir = \"vanished\"\npresence = \"optional\"\n\
         reason = \"absent in a clean checkout\"\n",
        &["Cold bulk load", "Point reads"],
    );
    assert!(f.hits_of(lint::HitKind::DeadPrune).is_empty());
}

#[test]
fn claims_neg_uncovered_closure_root() {
    // THE ROOTS ARE DATA TOO. fd7d169 made the walk recursive, so everything
    // under a root is accounted for. It said nothing about WHICH roots, and
    // `closure_dirs` is the one scan root in this crate that a registry edit can
    // narrow: MEASURED 2026-07-27, of seven `read_dir` sites, five take their
    // root from `Cargo.toml [workspace] members` (removing one member reds
    // topology-check with `active_not_a_member`) and one is the code constant
    // `scripts/`, guarded against its own emptiness by `script_scan_empty`.
    //
    // The emptiness guard already in this walk does NOT cover this: a root set
    // that holds prose passes it while covering the wrong thing. On the real
    // tree, `closure_dirs = ["docs"]` left `registry-check lint` at exit 0,
    // hits 0, prose_files_seen 3 -- with AGENTS.md, README.md and the 1.7MB
    // merged plan silently outside the closure.
    let f = LintFixture::build("uncovered-root");
    std::fs::create_dir_all(f.root.join("crates/demo")).expect("nested dirs");

    // Roots that reach neither the repository root's own prose nor `crates/`.
    // `docs` holds prose, so the pre-existing emptiness guard is satisfied and
    // this law is the only thing that can fire.
    f.write_config_roots(
        &["docs"],
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let hits = f.hits_of(lint::HitKind::UncoveredClosureRoot);
    let named: Vec<&str> = hits.iter().map(|h| h.subject.as_str()).collect();
    // README.md and HISTORY.md are root prose no root reaches; `crates` and
    // `registries` are top-level directories no root reaches and no prune
    // excuses. `docs` is absent from the list because it IS a root.
    assert_eq!(
        named,
        vec!["HISTORY.md", "README.md", "crates", "registries"],
        "{hits:?}"
    );
    assert_eq!(hits[0].kind.code(), "uncovered_closure_root");

    // A prune is the declared excuse for a directory, exactly as it is for the
    // walk: both directories drop out, the two root documents do not, because a
    // prune names directories and a file at the repository root is reachable
    // only from a `.` root.
    f.write_config_roots(
        &["docs"],
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n\
         [[closure_prune]]\ndir = \"crates\"\nreason = \"not prose\"\n\n\
         [[closure_prune]]\ndir = \"registries\"\nreason = \"not prose\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let named: Vec<String> = f
        .hits_of(lint::HitKind::UncoveredClosureRoot)
        .into_iter()
        .map(|h| h.subject)
        .collect();
    assert_eq!(named, vec!["HISTORY.md", "README.md"]);

    // Restoring the repository root as a closure root settles all of it: 3 of 3
    // named entries drop to none, and no other law starts complaining.
    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let (hits, census) = f.run().expect("lint runs");
    assert!(
        hits.iter()
            .all(|h| h.kind != lint::HitKind::UncoveredClosureRoot),
        "{hits:?}"
    );
    assert_eq!(census.prose_files_seen, 3, "{census:?}");
}

#[test]
fn claims_neg_dead_exclude() {
    // The denylist's own liveness. An exclusion is a narrowing of the lint;
    // one that matches nothing narrows it invisibly, which is how an allowlist
    // rots. `presence = "optional"` is the one declared escape, and this test
    // proves the flag is the whole difference: same absent path, both verdicts.
    let f = LintFixture::build("dead-exclude");
    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n\
         [[exclude]]\npath = \"DELETED_REVIEW.md\"\nreason = \"a review that was removed\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let hits = f.hits_of(lint::HitKind::DeadExclude);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].subject, "DELETED_REVIEW.md");

    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n\
         [[exclude]]\npath = \"DELETED_REVIEW.md\"\npresence = \"optional\"\nreason = \"gitignored working document\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let (hits, _) = f.run().expect("lint runs");
    assert!(
        hits.is_empty(),
        "presence = optional must be the only thing that changed: {hits:?}"
    );
}

#[test]
fn claims_neg_unreachable_exclude() {
    // `dead_exclude` asks only whether the path EXISTS, and existing is not the
    // same as narrowing anything: `unclaimed_prose` accuses only files the
    // closure walk FOUND, so an exclusion the walk never reaches excuses a hit
    // that could not have been raised. Hidden directories are skipped by the
    // walk, so the same file is reachable or not purely by where it sits —
    // which makes location the only difference between the two verdicts below.
    //
    // MEASURED 2026-07-27 on the real tree: appending a `target/PHANTOM.md`
    // exclusion (a pruned subtree) to the shipped config left the verdict at
    // exactly its base — 1 hit before, 1 hit after — with nothing accusing the
    // inert row.
    let f = LintFixture::build("unreachable-exclude");
    std::fs::create_dir_all(f.root.join(".private")).expect("hidden dir");
    std::fs::write(
        f.root.join(".private/OLD.md"),
        "A hidden historical note.\n",
    )
    .expect("hidden file");
    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n\
         [[exclude]]\npath = \".private/OLD.md\"\nreason = \"a note the walk cannot see\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let dead = f.hits_of(lint::HitKind::DeadExclude);
    assert!(
        dead.is_empty(),
        "the file exists, so the existence law must not be what fires: {dead:?}"
    );
    let hits = f.hits_of(lint::HitKind::UnreachableExclude);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].subject, ".private/OLD.md");

    // Paired control: the same file, the same exclusion, moved somewhere the
    // walk reaches. If this still fired, the law would be accusing every row.
    std::fs::write(f.root.join("OLD.md"), "A historical note.\n").expect("visible file");
    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n\
         [[exclude]]\npath = \"OLD.md\"\nreason = \"a note the walk can see\"\n",
        &["Cold bulk load", "Point reads"],
    );
    let (hits, _) = f.run().expect("lint runs");
    assert!(
        hits.is_empty(),
        "location must be the only thing that changed: {hits:?}"
    );
}

#[test]
fn claims_neg_unregistered_marker_survives_the_second_direction() {
    // Direction 1 regression guard: adding direction 2 must not cost the
    // direction that already worked.
    let f = LintFixture::build("unregistered");
    let mutated = LINT_FIXTURE_README.replace("(FG-SLO-01)", "(FG-SLO-99)");
    std::fs::write(f.root.join("README.md"), &mutated).expect("mutate README");
    let (hits, _) = f.run().expect("lint runs");
    let unregistered: Vec<_> = hits
        .iter()
        .filter(|h| h.kind == lint::HitKind::UnregisteredMarker)
        .collect();
    assert_eq!(unregistered.len(), 1, "{hits:?}");
    assert_eq!(unregistered[0].subject, "FG-SLO-99");
    assert_eq!(unregistered[0].file, "README.md");
    assert_eq!(unregistered[0].line, 11, "{hits:?}");
}

#[test]
fn claims_neg_broken_path_claim() {
    // The path-claim tripwire (fgdb-g0-doc-sync-usq.1). In the real corpus it
    // is EXPECTED to match nothing — README no longer mentions the absent
    // installer — so this fixture is the only witness that the law fires.
    let f = LintFixture::build("pathclaim");
    f.write_config(
        &["README.md", "docs/GUIDE.md"],
        "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n\
         [[path_claim]]\npath = \"scripts/install.sh\"\nfiles = [\"README.md\"]\n\
         reason = \"fixture installer claim\"\n",
        &["Cold bulk load", "Point reads"],
    );
    // Armed but silent: the claim is declared, nothing mentions the path.
    assert!(f.hits_of(lint::HitKind::BrokenPathClaim).is_empty());

    // The exact shipped defect: an install instruction naming a path that
    // does not exist. One mention, one hit, located at its line.
    // LINT_FIXTURE_README ends with a newline, so the appended instruction is
    // exactly one line past its last.
    let mutated = format!(
        "{LINT_FIXTURE_README}curl -fsSL https://example.invalid/scripts/install.sh | bash\n"
    );
    std::fs::write(f.root.join("README.md"), &mutated).expect("mutate README");
    let hits = f.hits_of(lint::HitKind::BrokenPathClaim);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].file, "README.md");
    assert_eq!(hits[0].subject, "scripts/install.sh");
    assert_eq!(
        hits[0].line,
        LINT_FIXTURE_README.lines().count() + 1,
        "{hits:?}"
    );

    // The claim binds only to its named files: descriptive prose in another
    // scanned artifact may NAME the absent path without a hit (the shipped
    // corpus does exactly this in the reality-check document).
    std::fs::write(
        f.root.join("docs/GUIDE.md"),
        "A scanned guide that cites FG-SLO-01 and describes the scripts/install.sh defect.\n",
    )
    .expect("mutate GUIDE");
    let hits = f.hits_of(lint::HitKind::BrokenPathClaim);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].file, "README.md", "{hits:?}");

    // Land the file: the same mention is now an honest instruction and the
    // claim is satisfied everywhere at once.
    std::fs::create_dir_all(f.root.join("scripts")).expect("scripts dir");
    std::fs::write(f.root.join("scripts/install.sh"), "#!/usr/bin/env bash\n")
        .expect("fixture installer");
    assert!(f.hits_of(lint::HitKind::BrokenPathClaim).is_empty());
}

#[test]
fn claims_neg_path_claim_without_reason_is_a_schema_error() {
    let f = LintFixture::build("pathclaim-schema");
    for (fragment, expect) in [
        (
            "[[path_claim]]\npath = \"scripts/install.sh\"\n",
            "path_claim[0]",
        ),
        (
            "[[path_claim]]\npath = \"scripts/install.sh\"\nreason = \"  \"\n",
            "without a reason",
        ),
        (
            "[[path_claim]]\npath = \"../outside\"\nreason = \"escapes the root\"\n",
            "non-empty relative path",
        ),
        (
            "[[path_claim]]\npath = \"a.sh\"\nfiles = [\"README.md\"]\nreason = \"once\"\n\n\
             [[path_claim]]\npath = \"a.sh\"\nfiles = [\"README.md\"]\nreason = \"twice\"\n",
            "claimed twice",
        ),
        (
            "[[path_claim]]\npath = \"a.sh\"\nfiles = []\nreason = \"unbound\"\n",
            "bound to nothing",
        ),
        (
            "[[path_claim]]\npath = \"a.sh\"\nfiles = [\"HISTORY.md\"]\nreason = \"unscanned\"\n",
            "not in lint.scan",
        ),
        (
            "[[path_claim]]\npath = \"a.sh\"\nfiles = [\"README.md\", \"README.md\"]\nreason = \"dup file\"\n",
            "listed twice",
        ),
    ] {
        f.write_config(
            &["README.md", "docs/GUIDE.md"],
            &format!(
                "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"historical draft\"\n\n{fragment}"
            ),
            &["Cold bulk load", "Point reads"],
        );
        let err = f.config().expect_err(fragment);
        assert!(err.msg.contains(expect), "{fragment:?} -> {err:?}");
    }
}

#[test]
fn claims_neg_lint_config_cannot_declare_a_vacuous_scope() {
    // A config that disarms the lint must be rejected at load, not obeyed. Each
    // mutation below is a different way to make the check examine nothing.
    let f = LintFixture::build("vacuous-config");
    let base = std::fs::read_to_string(f.root.join("registries/claims_lint.toml")).expect("read");
    let cfg = f.root.join("registries/claims_lint.toml");

    for (mutation, expect) in [
        (
            base.replace(
                "scan = [\n  \"README.md\",\n  \"docs/GUIDE.md\",\n]",
                "scan = []",
            ),
            "scans nothing",
        ),
        (
            base.replace("closure_dirs = [\".\", \"docs\"]", "closure_dirs = []"),
            "closure_dirs is empty",
        ),
        (
            base.replace("[[gate_table]]", "[[unused_table]]"),
            "declares no [[gate_table]]",
        ),
        (
            base.replace(
                "path = \"HISTORY.md\"\nreason",
                "path = \"README.md\"\nreason",
            ),
            "both scanned and excluded",
        ),
        (
            base.replace(
                "path = \"HISTORY.md\"\nreason",
                "path = \"HISTORY.md\"\npresence = \"whenever\"\nreason",
            ),
            "not one of",
        ),
        (
            base.replace(
                "\"Cold bulk load\",\n  \"Point reads\",\n",
                "\"Cold bulk load\",\n  \"Cold bulk load\",\n",
            ),
            "is listed twice",
        ),
        (
            base.replace(
                "  \"docs/GUIDE.md\",\n",
                "  \"docs/GUIDE.md\",\n  \"docs/GUIDE.md\",\n",
            ),
            "twice, which would double every count",
        ),
        (
            base.replace(
                "[[exclude]]\npath = \"HISTORY.md\"",
                "[[exclude]]\npath = \"HISTORY.md\"\nreason = \"first\"\n\n[[exclude]]\npath = \"HISTORY.md\"",
            ),
            "excluded twice",
        ),
    ] {
        assert_ne!(
            mutation, base,
            "mutation {expect:?} did not change the config"
        );
        std::fs::write(&cfg, &mutation).expect("write mutated config");
        let err = f
            .config()
            .expect_err(&format!("config must be rejected: {expect}"));
        assert!(
            err.to_string().contains(expect),
            "expected {expect:?} in: {err}"
        );
    }
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
        expected_reachable_clauses: 0,
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
        expected_reachable_clauses: 0,
    };
    assert!(closure::compute(&r, &empty).ok());
}

// ---------------------------------------------------------------------------
// The promotion law (`fgdb-clause-promotion-to-live-is-unguarded-nllh`).
// ---------------------------------------------------------------------------
//
// `invariants.toml`'s own header states it: "Workstream beads flip status
// stub -> live in the same change that LANDS THE CHECKER, never before."
// AGENTS.md rests every G1-G4 exit gate on it. Nothing implemented it.
//
// MEASURED BEFORE THE LAW WAS WRITTEN, against the shipped registries:
//   - all 20 clauses are `stub`, and all 40 entrypoints resolve to
//     `checker_index` rows that are themselves `stub`, pointing into crates that
//     do not exist;
//   - promoting one clause to `live` and changing NOTHING else produced ZERO
//     violations;
//   - so did the degenerate case, a live clause whose `negative_test_entrypoint`
//     IS its `checker_entrypoint`.
// The whole law for both fields was "the string resolves to a row", which is why
// [`legacy_entrypoint_resolves`] below is kept verbatim: every mutant is
// asserted to satisfy it, so each test states that the old law could not tell
// the cases apart.
//
// One measurement corrected the bead that filed this. It proposed requiring the
// checker's and the negative test's ARTIFACTS to be distinct. All twenty shipped
// clauses share an artifact between the two (0 share a symbol), and `claims.rs`
// itself holds both `claims_hash_twenty_id_pin` and `claims_neg_waiver_present`,
// so that rule would reject the shipped shape and the house style with it.
// Distinct SYMBOLS is the real rule.
//
// WHAT THE PIN IS WORTH, MEASURED (2026-07-26), on depth-matched scratch copies:
//
//   * Deleting the promotion law turns FIVE tests red, all of them here:
//     claims_promotion_law_is_vacuous_today_and_this_is_what_licenses_it,
//     claims_neg_clause_promoted_without_live_checker,
//     claims_promotion_delegates_to_the_liveness_reader,
//     claims_neg_negative_test_is_its_own_checker, and
//     claims_clause_status_vocabulary_has_one_reader. 14 of 19 stay green here;
//     metamorphic 43/43 and spine 8/8 stay fully green, which is the reading
//     that matters — the reds are the missing law, not a broken harness.
//
//   * Splitting the single status vocabulary back into two spellings (a
//     `matches!` for the schema and an inline `== "live"` for the law) turns
//     ZERO tests red. That is stated rather than hidden: with exactly the three
//     statuses the old inline spelling already had, both implementations agree
//     on every input, so no test CAN distinguish them today.
//     claims_clause_status_vocabulary_has_one_reader is a prospective guard —
//     it iterates `CLAUSE_STATUS_ENFORCED` instead of restating it, so it fires
//     the moment a fourth status is added and the second spelling does not
//     follow, which is exactly when the fail-open would otherwise ship. A guard
//     that cannot fire on today's input is worth having and is NOT worth
//     reporting as though it had been witnessed firing; see
//     fgdb-validator-laws-never-witnessed-firing-xnxy.

/// The pre-fix promotion predicate, verbatim: the symbol resolves to a row.
/// Status was not consulted, and neither was anything about the row.
fn legacy_entrypoint_resolves(r: &Registries, symbol: &str) -> bool {
    r.checker_index.iter().any(|row| row.symbol == symbol)
}

/// Promote the first clause to `status`, optionally repointing its entrypoints.
/// Returns the mutated registries and the clause key.
fn promote_first_clause(
    status: &str,
    checker: Option<&str>,
    negative_test: Option<&str>,
) -> (Registries, String) {
    let mut r = real_registries();
    let mut key = String::new();
    for invariant in r.invariants.invariants.iter_mut() {
        if let Some(clause) = invariant.clauses.first_mut() {
            clause.status = status.to_string();
            if let Some(symbol) = checker {
                clause.checker_entrypoint = symbol.to_string();
            }
            if let Some(symbol) = negative_test {
                clause.negative_test_entrypoint = symbol.to_string();
            }
            key = clause.key.clone();
            break;
        }
    }
    (r, key)
}

/// THE VACUITY CONTROL, and it fires.
///
/// Zero of the twenty shipped clauses is `live`, so a sweep asserting "every
/// live clause has a live checker" is quantified over the empty set — it would
/// pass just as loudly with the law deleted. A zero result is not a result
/// without a control, so this test MAKES one: it promotes a real shipped clause
/// exactly as a workstream bead would at G1 and requires the law to produce a
/// real defect from real registry data.
///
/// Written to survive G1: when clauses start promoting, the shipped-cohort half
/// takes over and neither the count nor the emptiness of the live cohort is
/// pinned.
#[test]
fn claims_promotion_law_is_vacuous_today_and_this_is_what_licenses_it() {
    let r = real_registries();
    let clauses: Vec<_> = r
        .invariants
        .invariants
        .iter()
        .flat_map(|invariant| invariant.clauses.iter())
        .collect();
    assert!(
        !clauses.is_empty(),
        "control: the clause roster came back empty, so every verdict about it is \
         quantified over nothing"
    );

    // Half one: whatever IS shipped enforced must have a live checker. Vacuous
    // today by construction; the licence for that zero is half two.
    assert!(
        !codes(&r).contains(&"clause_promoted_without_live_checker".to_string()),
        "the shipped tree must satisfy the law its own registry header states"
    );

    // Half two: the licence. If this comes back clean the law is not merely
    // unexercised, it is dead, and half one proved nothing.
    let (promoted, key) = promote_first_clause("live", None, None);
    let observed = codes(&promoted);
    assert!(
        observed.contains(&"clause_promoted_without_live_checker".to_string()),
        "control: promoting shipped clause {key} to \"live\" produced no promotion \
         defect at all, so the law cannot tell an enforced invariant from an \
         unenforced one; got {observed:?}"
    );
}

/// A clause may not be enforced by a checker that is not itself live.
#[test]
fn claims_neg_clause_promoted_without_live_checker() {
    let (promoted, key) = promote_first_clause("live", None, None);
    // The witness: the pre-fix law accepted this, because both symbols resolve.
    let clause = promoted
        .invariants
        .invariants
        .iter()
        .flat_map(|invariant| invariant.clauses.iter())
        .find(|clause| clause.key == key)
        .expect("promoted clause");
    for symbol in [&clause.checker_entrypoint, &clause.negative_test_entrypoint] {
        assert!(
            legacy_entrypoint_resolves(&promoted, symbol),
            "witness: the pre-fix law must accept {symbol:?}, or this relation proves \
             nothing about the fix"
        );
    }
    let observed = codes(&promoted);
    assert!(
        observed.contains(&"clause_promoted_without_live_checker".to_string()),
        "{observed:?}"
    );

    // BOTH fields are held to the bar, not just the checker. The negative test's
    // whole purpose is to prove the checker can go red; one that is itself a
    // stub proves nothing, and a law that checked only `checker_entrypoint`
    // would pass this.
    let (checker_only, _) = promote_first_clause("live", Some("claims_hash_twenty_id_pin"), None);
    assert!(
        codes(&checker_only).contains(&"clause_promoted_without_live_checker".to_string()),
        "a live checker with a stub negative test is not a promoted clause: {:?}",
        codes(&checker_only)
    );

    // THE OTHER DIRECTION, which is the one a too-strict law fails: two real
    // live rows, in the SAME artifact (`claims.rs` holds both), is the legal
    // shape and must not fire.
    let (legal, _) = promote_first_clause(
        "live",
        Some("claims_hash_twenty_id_pin"),
        Some("claims_neg_waiver_present"),
    );
    let legal_codes = codes(&legal);
    assert!(
        !legal_codes.contains(&"clause_promoted_without_live_checker".to_string()),
        "a clause whose two entrypoints are distinct LIVE rows is legal even when they \
         share an artifact — all twenty shipped clauses share one: {legal_codes:?}"
    );
    assert!(
        !legal_codes.contains(&"clause_negative_test_is_its_own_checker".to_string()),
        "{legal_codes:?}"
    );
}

/// THE DELEGATION. The law does not re-derive what a live checker is; it asks
/// `liveness`, and the verdict carries that reader's own words.
///
/// `claims_hash_twenty_id_pin` is a genuinely live row. Repointing it at a file
/// that exists but that `cargo test --workspace` never compiles leaves
/// `status = "live"` and a present artifact — everything short of the full
/// liveness read still says yes. A parallel implementation of "is this checker
/// live" would pass every other test in this file and fail this one.
#[test]
fn claims_promotion_delegates_to_the_liveness_reader() {
    let (mut mutated, _) = promote_first_clause(
        "live",
        Some("claims_hash_twenty_id_pin"),
        Some("claims_neg_waiver_present"),
    );
    assert!(
        !codes(&mutated).contains(&"clause_promoted_without_live_checker".to_string()),
        "control: this shape must be legal before the mutation below means anything"
    );
    for row in mutated.checker_index.iter_mut() {
        if row.symbol == "claims_hash_twenty_id_pin" {
            // Still `status = "live"`, still a file that exists.
            row.artifact = "README.md".to_string();
        }
    }
    let violations = validate::validate_all(&mutated, &repo_root());
    let promotion: Vec<&validate::Violation> = violations
        .iter()
        .filter(|v| v.code == "clause_promoted_without_live_checker")
        .collect();
    assert!(
        !promotion.is_empty(),
        "a clause enforced by a row no gate compiles is not promoted: {:?}",
        violations.iter().map(|v| &v.code).collect::<Vec<_>>()
    );
    assert!(
        promotion.iter().any(|v| v
            .msg
            .contains("is not an integration test target of a workspace member")),
        "the clause verdict must CARRY the liveness reader's own finding — a second \
         implementation of \"is this checker live\" would pass every other assertion \
         here. Got: {:?}",
        promotion.iter().map(|v| &v.msg).collect::<Vec<_>>()
    );
}

/// A checker cannot be the evidence that it can go red.
#[test]
fn claims_neg_negative_test_is_its_own_checker() {
    let (same, _) = promote_first_clause(
        "live",
        Some("claims_hash_twenty_id_pin"),
        Some("claims_hash_twenty_id_pin"),
    );
    assert!(
        legacy_entrypoint_resolves(&same, "claims_hash_twenty_id_pin"),
        "witness: the pre-fix law accepted this — the symbol resolves, twice"
    );
    assert!(
        codes(&same).contains(&"clause_negative_test_is_its_own_checker".to_string()),
        "{:?}",
        codes(&same)
    );

    // Not enforced, not the law's business: a stub clause may still be drafting.
    let (stubbed, _) = promote_first_clause(
        "stub",
        Some("claims_hash_twenty_id_pin"),
        Some("claims_hash_twenty_id_pin"),
    );
    assert!(
        !codes(&stubbed).contains(&"clause_negative_test_is_its_own_checker".to_string()),
        "the promotion law applies to enforced clauses only: {:?}",
        codes(&stubbed)
    );
}

/// THE COMPLETENESS GUARD. One vocabulary, one enforcement answer.
///
/// The status vocabulary used to be spelled inline in a `matches!`, and the
/// promotion law would have been a second `== "live"` beside it. Two spellings
/// of one vocabulary is how a status added later arrives enforced by nothing —
/// the schema check accepts it and the law that gives `live` its meaning
/// silently skips it, which is this whole bug family. This test pins that the
/// schema check and the promotion law read the SAME list.
#[test]
fn claims_clause_status_vocabulary_has_one_reader() {
    assert!(
        !validate::CLAUSE_STATUS_ENFORCED.is_empty(),
        "control: an empty vocabulary makes every assertion below vacuous"
    );
    assert!(
        validate::CLAUSE_STATUS_ENFORCED
            .iter()
            .any(|(_, enforced)| *enforced),
        "control: no status enforces, so the promotion law can never run"
    );

    for (status, enforced) in validate::CLAUSE_STATUS_ENFORCED {
        // Every status the law knows about is a status the schema accepts.
        let (r, key) = promote_first_clause(
            status,
            Some("claims_hash_twenty_id_pin"),
            Some("claims_hash_twenty_id_pin"),
        );
        let observed = codes(&r);
        assert!(
            !observed.contains(&"bad_field".to_string()),
            "status {status:?} is in the enforcement table but the schema check rejects \
             it as a bad field: {observed:?}"
        );
        // And the enforcement answer is the one the law acts on.
        assert_eq!(
            observed.contains(&"clause_negative_test_is_its_own_checker".to_string()),
            *enforced,
            "clause {key} at status {status:?}: the enforcement table says {enforced}, \
             the promotion law behaved otherwise; got {observed:?}"
        );
    }

    // A status outside the vocabulary is rejected, never silently sorted into
    // one side or the other.
    let (unknown, _) = promote_first_clause("enforced", None, None);
    let observed = codes(&unknown);
    assert!(
        observed.contains(&"bad_field".to_string()),
        "an unregistered clause status must be reported: {observed:?}"
    );
}

// ---------------------------------------------------------------------------
// The enforcement ledger (`fgdb-fginv-spine-zero-live-checkers-v05b`).
// ---------------------------------------------------------------------------
//
// AGENTS.md: "CI cross-checks that every ID has a live checker." All 20 clauses
// are stub and all 40 entrypoints resolve to stub rows (measured under nllh), so
// that cross-check quantified over an EMPTY SET and passed — the purest form of
// the family this suite has been closing, and an exit code indistinguishable
// from a fully enforced spine.
//
// The pre-fix predicate is not a function to keep verbatim here: there WAS no
// predicate. That is the whole finding, and [`claims_enforcement_ledger_control`]
// states it by asserting the shipped tree really does enforce nothing, so every
// number below is read against a known-empty base.
//
// WHAT THE PIN IS WORTH, MEASURED (2026-07-26), on a depth-matched scratch copy:
// removing the ledger's call from `validate_all` turns TWO tests red —
// claims_enforcement_ledger_control and claims_neg_enforcement_coverage_drift
// (claims 20/22; metamorphic 43/43 and spine 8/8 stay fully green, so the reds
// are the missing law and not a broken harness).
//
// claims_enforcement_ledger_delegates_to_the_liveness_reader stays GREEN under
// that reversion, and that is stated rather than hidden: it asserts the ABSENCE
// of drift, which is trivially true when the ledger does not run. It guards a
// different edit — a ledger that counted `status == "live"` instead of asking
// the liveness reader — and it fires on that one. A test that cannot distinguish
// "correct" from "absent" is worth having and is NOT worth reporting as though
// it had been witnessed firing; same reading as
// claims_clause_status_vocabulary_has_one_reader above, and the population is
// fgdb-validator-laws-never-witnessed-firing-xnxy.

/// THE VACUITY CONTROL, and it fires.
///
/// The ledger's own conclusion is "0 enforced, 0 declared, pass". A zero result
/// is not a result without a control, so this makes three: the accounting really
/// examined all twenty ids, the base really is empty, and — the half that
/// matters — a spine with nothing in it FAILS rather than passes.
#[test]
fn claims_enforcement_ledger_control() {
    let r = real_registries();
    assert_eq!(
        r.invariants.invariants.len(),
        20,
        "control: the ledger must have twenty ids to account for"
    );
    let clauses: usize = r
        .invariants
        .invariants
        .iter()
        .map(|invariant| invariant.clauses.len())
        .sum();
    assert!(
        clauses > 0,
        "control: zero clauses would make every count below quantified over nothing"
    );
    // RE-DERIVED, not re-pinned (fgdb-1sto). The base stopped being empty when
    // `FG-INV-12.canonical-scalar-coherence` was promoted, so the count alone is
    // no longer the interesting fact: WHICH clause is enforced is. Asserting the
    // identity means a future promotion cannot slip in under an unchanged
    // number, and a regression of this clause to stub names itself here instead
    // of arriving as an off-by-one.
    let enforced: Vec<String> = r
        .invariants
        .invariants
        .iter()
        .flat_map(|invariant| invariant.clauses.iter())
        .filter(|clause| {
            validate::clause_status_is_enforced(&clause.status) == Some(true)
                && registry_check::liveness::assess_clause(
                    repo_root().as_path(),
                    clause,
                    &r.checker_index,
                )
                .is_empty()
        })
        .map(|clause| clause.key.clone())
        .collect();
    assert_eq!(
        enforced,
        vec![
            "FG-INV-04.pinned-snapshot-visibility".to_string(),
            "FG-INV-05.first-committer-wins".to_string(),
            "FG-INV-09.four-layer-identity-recomputation".to_string(),
            "FG-INV-12.canonical-scalar-coherence".to_string(),
            "FG-INV-19.replay-grade-monotonicity".to_string(),
        ],
        "the shipped tree enforces exactly these clauses; if this changes, the \
         numbers below are read against a different base and this test must be \
         re-derived, not re-pinned"
    );
    assert_eq!(
        r.invariants.expected_enforced_clauses as usize,
        enforced.len(),
        "the declaration must equal the measured enforced-clause set"
    );
    assert_eq!(
        r.invariants.expected_enforced_invariants, 0,
        "no ID is fully enforced: FG-INV-12.core is still stub, and an ID counts \
         only when every clause under it is enforced"
    );
    assert!(
        !codes(&r).contains(&"enforcement_coverage_drift".to_string()),
        "the shipped declaration must match the shipped tree"
    );

    // THE GUARD THIS BEAD EXISTS FOR: an empty spine must FAIL, not pass.
    // Before this law, emptying the registry left "every ID has a live checker"
    // trivially true and the exit code unchanged.
    let mut emptied = real_registries();
    emptied.invariants.invariants.clear();
    let observed = codes(&emptied);
    assert!(
        observed.contains(&"enforcement_coverage_empty".to_string()),
        "a spine with no clauses must be a violation, never a pass: {observed:?}"
    );

    // And a spine that merely SHRANK — still non-empty, so the emptiness guard
    // does not fire — must be caught by the completeness guard.
    let mut shrunk = real_registries();
    shrunk.invariants.invariants.truncate(19);
    let observed = codes(&shrunk);
    assert!(
        observed.contains(&"enforcement_coverage_incomplete".to_string()),
        "a ledger that accounted for 19 of 20 ids has stopped looking, not passed: \
         {observed:?}"
    );
}

/// The declared count is checked in BOTH directions.
#[test]
fn claims_neg_enforcement_coverage_drift() {
    // Too many: a clause promoted with a genuinely live apparatus, without the
    // declaration bump. This is the G1 case the doctrine's gate review exists
    // for, and nothing computed it before.
    let (promoted, key) = promote_first_clause(
        "live",
        Some("claims_hash_twenty_id_pin"),
        Some("claims_neg_waiver_present"),
    );
    let observed = codes(&promoted);
    assert!(
        !observed.contains(&"clause_promoted_without_live_checker".to_string()),
        "control: this promotion is legal under the promotion law, so the drift below \
         is the ledger's finding and not nllh's: {observed:?}"
    );
    assert!(
        observed.contains(&"enforcement_coverage_drift".to_string()),
        "promoting {key} with a live apparatus and no declaration bump must fail: \
         {observed:?}"
    );

    // Too few: the declaration claims enforcement the tree does not have. This
    // is the direction that catches a clause silently regressing to stub.
    let mut overclaimed = real_registries();
    overclaimed.invariants.expected_enforced_clauses =
        real_registries().invariants.expected_enforced_clauses + 1;
    assert!(
        codes(&overclaimed).contains(&"enforcement_coverage_drift".to_string()),
        "a declaration one above the measured count must fail: {:?}",
        codes(&overclaimed)
    );

    // And the other side of the same direction: declaring zero while a clause
    // IS enforced. Before fgdb-1sto the measured base was zero and this mutant
    // could not be written at all.
    let mut underclaimed = real_registries();
    underclaimed.invariants.expected_enforced_clauses = 0;
    assert!(
        codes(&underclaimed).contains(&"enforcement_coverage_drift".to_string()),
        "a declaration of 0 against a measured 1 must fail: {:?}",
        codes(&underclaimed)
    );

    // The ID count is its own fact: promoting a clause moves the clause count,
    // and a law that only pinned ids would pass the first mutant above.
    let mut ids_only = real_registries();
    ids_only.invariants.expected_enforced_invariants = 3;
    assert!(
        codes(&ids_only).contains(&"enforcement_coverage_drift".to_string()),
        "{:?}",
        codes(&ids_only)
    );
}

/// THE DELEGATION. The ledger does not re-derive "is this apparatus live"; it
/// asks `liveness::Prover::assess_clause`, the same reader the promotion law
/// uses.
///
/// A clause promoted with a checker row that is `status = "live"` over a file
/// that exists but that `cargo test` never compiles must NOT count as enforced —
/// so the measured count stays 0, matches the declaration, and the ledger stays
/// quiet while the promotion law does the talking. A ledger that counted
/// `status == "live"` alone would report 1 enforced and drift.
#[test]
fn claims_enforcement_ledger_delegates_to_the_liveness_reader() {
    let (mut mutated, _) = promote_first_clause(
        "live",
        Some("claims_hash_twenty_id_pin"),
        Some("claims_neg_waiver_present"),
    );
    for row in mutated.checker_index.iter_mut() {
        if row.symbol == "claims_hash_twenty_id_pin" {
            row.artifact = "README.md".to_string();
        }
    }
    let observed = codes(&mutated);
    assert!(
        observed.contains(&"clause_promoted_without_live_checker".to_string()),
        "control: the promotion law must reject this shape: {observed:?}"
    );
    assert!(
        !observed.contains(&"enforcement_coverage_drift".to_string()),
        "a clause whose checker is live in name only is NOT enforced, so the ledger's \
         count must stay 0 and match the declaration. A ledger reading `status == \
         \"live\"` instead of asking the liveness reader would report drift here: \
         {observed:?}"
    );
}
