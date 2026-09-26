//! Prefix admission is checked against an independent storage scan and write-history ledger.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlBudgetDimension, GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphSymbol, GraphSymbolKind, PreparedGraphText, PreparedTemporalGraphText,
};
use fgdb_types::{
    CanonicalScalar, CanonicalScalarKind, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts,
    QueryCx, VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const NAME: PropertyKeyId = PropertyKeyId(1);
const WITNESSES: &[&str] = &[
    "",
    "a",
    "ab",
    "ab-tail",
    "ab-reject",
    "ac",
    "z",
    "zz",
    "\u{ff}",
    "\u{ff}-tail",
    "\u{100}",
    "\u{7ff}",
    "\u{7ff}-tail",
    "\u{800}",
    "é",
    "éclair",
    "e\u{301}",
    "e\u{301}clair",
    "東京",
    "東京駅",
    "z\u{10ffff}",
    "z\u{10ffff}tail",
    "\u{10ffff}",
    "\u{10ffff}tail",
    "\u{10ffff}\u{10ffff}",
    "\u{10ffff}\u{10ffff}tail",
];

type History = BTreeMap<VId, Vec<String>>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000_000, 100_000_000, 100_000_000)
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn query(statement: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(statement, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    let mut result: Vec<_> = rows
        .iter()
        .map(|r| r.get(0).unwrap().as_vertex().unwrap())
        .collect();
    result.sort_unstable();
    result
}

fn remember(history: &mut History, id: VId, value: &Option<CanonicalScalar>) {
    if let Some(CanonicalScalar::Text(value)) = value {
        history
            .entry(id)
            .or_default()
            .push(value.as_str().to_owned());
    }
}

fn candidates(history: &History, prefix: &str) -> u64 {
    // Count IDs, not index entries, including deleted/changed/future-at-the-cut IDs.
    // This ledger comes exclusively from fixture writes, not engine counters.
    history
        .values()
        .filter(|values| values.iter().any(|v| v.starts_with(prefix)))
        .count() as u64
}

struct Case {
    prefix: Option<&'static str>,
    residual: bool,
    indexed: PreparedGraphPattern<GraphValueRow>,
    scan: PreparedGraphPattern<GraphValueRow>,
}

impl Case {
    fn new(prefix: Option<&'static str>, literal: bool, residual: bool) -> Self {
        let value = prefix.map_or(CanonicalScalar::Null, text);
        let operand = if literal {
            prefix.map_or_else(
                || "NULL".to_owned(),
                |p| format!("'{}'", p.replace('\'', "''")),
            )
        } else {
            "$prefix".to_owned()
        };
        let predicate = format!(
            "n.name STARTS WITH {operand}{}",
            if residual {
                " AND n.name <> 'ab-reject'"
            } else {
                ""
            }
        );
        let args = if literal {
            GqlParameters::new()
        } else {
            GqlParameters::new()
                .with_scalar("prefix", value.clone())
                .unwrap()
        };
        let declarations = if literal {
            vec![]
        } else {
            vec![(
                "prefix",
                GqlParameterType::Scalar(CanonicalScalarKind::of(&value)),
            )]
        };
        let bind = |statement: String| {
            PreparedGraphText::prepare_with_parameter_types(&statement, &declarations, symbols)
                .unwrap()
                .bind_parameters(&args)
                .unwrap()
        };
        Self {
            prefix,
            residual,
            indexed: bind(format!("MATCH (n) WHERE {predicate} RETURN n")),
            // An independent vertex domain forbids index admission. Identity
            // correlation and DISTINCT preserve exactly the outer n rows.
            scan: bind(format!(
                "MATCH (n) WHERE {predicate} OPTIONAL MATCH (other) WHERE other=n RETURN DISTINCT n"
            )),
        }
    }
}

fn cases() -> Vec<Case> {
    let mut result: Vec<_> = [
        "",
        "ab",
        "\u{ff}",
        "\u{7ff}",
        "é",
        "e\u{301}",
        "東京",
        "z\u{10ffff}",
        "\u{10ffff}",
        "\u{10ffff}\u{10ffff}",
        "absent",
    ]
    .into_iter()
    .map(|prefix| Case::new(Some(prefix), false, false))
    .collect();
    result.extend([
        Case::new(Some("ab"), true, false),
        Case::new(Some("\u{ff}"), true, false),
        Case::new(Some("\u{7ff}"), true, false),
        Case::new(Some("ab"), false, true),
        Case::new(None, false, false),
        Case::new(None, true, false),
    ]);
    result
}

fn oracle(db: &Database<MemVfs>, at: CommitSeq, case: &Case) -> Vec<VId> {
    db.vertices_at(at)
        .unwrap()
        .into_iter()
        .filter(|row| {
            row.props.iter().any(|(key, value)| {
                *key == NAME
                    && matches!(value, CanonicalScalar::Text(value)
                if case.prefix.is_some_and(|prefix| value.as_str().starts_with(prefix))
                    && (!case.residual || value.as_str() != "ab-reject"))
            })
        })
        .map(|row| row.vid)
        .collect()
}

fn boundary(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    at: CommitSeq,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    expected: &[VId],
    charge: u64,
) {
    let exact = db
        .execute_graph_pattern_governed_at(
            cx,
            pattern,
            at,
            GqlQueryPolicy::new(charge, 1_000_000, u64::MAX, u64::MAX),
        )
        .unwrap();
    assert_eq!(ids(&exact.value), expected);
    assert_eq!(exact.rows.snapshot_records, charge);
    if charge != 0 {
        assert!(matches!(
            db.execute_graph_pattern_governed_at(cx, pattern, at,
                GqlQueryPolicy::new(charge - 1, 1_000_000, u64::MAX, u64::MAX)),
            Err(GqlQueryError::Rows(error))
                if error.dimension == GqlBudgetDimension::SnapshotRecords
                    && error.limit == charge - 1 && error.observed == charge
        ));
    }
}

struct Rng(u64);
impl Rng {
    fn value(&mut self) -> Option<CanonicalScalar> {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        let choice = (self.0 >> 32) as usize % (WITNESSES.len() + 2);
        match WITNESSES.get(choice) {
            Some(value) => Some(text(value)),
            None if choice == WITNESSES.len() => Some(CanonicalScalar::Null),
            None => None,
        }
    }
}

#[test]
fn prefix_answers_and_resolved_charges_follow_history_pinning_reopen_and_as_of() {
    for seed in [11_u64, 23, 0xdead_f00d] {
        let ((), report) = run_async_under_lab(0xd10_0000 + seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut history = History::new();
            let mut rng = Rng(seed);
            let mut initial = WriteBatch::new(R);
            for (offset, value) in WITNESSES.iter().enumerate() {
                let id = VId(offset as u128 + 1);
                let value = text(value);
                remember(&mut history, id, &Some(value.clone()));
                initial.create_vertex(id, vec![], vec![(NAME, value)]);
            }
            initial.create_vertex(VId(90), vec![], vec![]);
            initial.create_vertex(VId(91), vec![], vec![(NAME, CanonicalScalar::Null)]);
            for id in 100..124 {
                let value = rng.value();
                remember(&mut history, VId(id), &value);
                initial.create_vertex(
                    VId(id),
                    vec![],
                    value.into_iter().map(|v| (NAME, v)).collect(),
                );
            }
            for id in 150..154 {
                let value = text("ab-moving");
                remember(&mut history, VId(id), &Some(value.clone()));
                initial.create_vertex(VId(id), vec![], vec![(NAME, value)]);
            }
            let old = db.write(&commit, initial).await.unwrap();
            let pinned = db.read_session().unwrap();
            let pinned_history = history.clone();
            let cases = cases();
            let original: Vec<_> = cases.iter().map(|case| oracle(&db, old, case)).collect();
            let original_scan_count = db.vertices_at(old).unwrap().len() as u64;
            let mut cuts = vec![CommitSeq(0), old];
            let temporal = PreparedTemporalGraphText::prepare(
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WHERE n.name STARTS WITH 'ab' RETURN n",
                symbols,
            )
            .unwrap();
            let temporal_case = Case::new(Some("ab"), true, false);
            let original_temporal = oracle(&db, old, &temporal_case);
            for step in 0..=4_u64 {
                if step != 0 {
                    let mut batch = WriteBatch::new(R);
                    for id in 100..124 {
                        let value = rng.value();
                        remember(&mut history, VId(id), &value);
                        batch.set_vertex_property(VId(id), NAME, value);
                    }
                    // Re-entry, explicit NULL, missing property and tombstone
                    // must not erase prior candidate membership in the generation.
                    for (id, value) in [
                        (
                            150,
                            Some(text(if step % 2 == 0 {
                                "ab-moving"
                            } else {
                                "outside"
                            })),
                        ),
                        (
                            151,
                            if step % 2 == 0 {
                                Some(text("ab-moving"))
                            } else {
                                Some(CanonicalScalar::Null)
                            },
                        ),
                        (
                            152,
                            if step % 2 == 0 {
                                Some(text("ab-moving"))
                            } else {
                                None
                            },
                        ),
                    ] {
                        remember(&mut history, VId(id), &value);
                        batch.set_vertex_property(VId(id), NAME, value);
                    }
                    if step == 2 {
                        batch.delete_vertex(VId(153));
                    }
                    let id = VId(u128::from(200 + step));
                    let value = text("ab-new");
                    remember(&mut history, id, &Some(value.clone()));
                    batch.create_vertex(id, vec![], vec![(NAME, value)]);
                    cuts.push(db.write(&commit, batch).await.unwrap());
                }
                if step == 3 {
                    drop(db);
                    db = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                }
                for (case, before) in cases.iter().zip(&original) {
                    for at in &cuts {
                        let expected = oracle(&db, *at, case);
                        let scan_count = db.vertices_at(*at).unwrap().len() as u64;
                        let charge = case.prefix.map_or(scan_count, |p| candidates(&history, p));
                        boundary(&db, &cx, *at, &case.indexed, &expected, charge);
                        boundary(&db, &cx, *at, &case.scan, &expected, scan_count);
                    }
                    let retained = pinned
                        .execute_graph_pattern_governed(&cx, &case.indexed, policy())
                        .unwrap();
                    assert_eq!(
                        ids(&retained.value),
                        *before,
                        "seed={seed} step={step} prefix={:?}",
                        case.prefix
                    );
                    assert_eq!(
                        retained.rows.snapshot_records,
                        case.prefix
                            .map_or(original_scan_count, |p| candidates(&pinned_history, p))
                    );
                    let retained_scan = pinned
                        .execute_graph_pattern_governed(&cx, &case.scan, policy())
                        .unwrap();
                    assert_eq!(ids(&retained_scan.value), *before);
                    assert_eq!(retained_scan.rows.snapshot_records, original_scan_count);
                }
                for at in &cuts {
                    let args = GqlParameters::new().with_uint64("at", at.0).unwrap();
                    let bound = temporal.bind_parameters(&args).unwrap();
                    let run = db
                        .execute_temporal_graph_text_governed(&cx, &bound, policy())
                        .unwrap();
                    assert_eq!(ids(&run.value), oracle(&db, *at, &temporal_case));
                    assert_eq!(run.rows.snapshot_records, candidates(&history, "ab"));
                }
                let bound = temporal
                    .bind_parameters(&GqlParameters::new().with_uint64("at", old.0).unwrap())
                    .unwrap();
                let run = pinned
                    .execute_temporal_graph_text_governed(&cx, &bound, policy())
                    .unwrap();
                assert_eq!(ids(&run.value), original_temporal);
                assert_eq!(run.rows.snapshot_records, candidates(&pinned_history, "ab"));
            }
        });
        assert!(report.lab_test_passed(), "seed={seed}: {report:?}");
    }
}

#[test]
fn prefix_boolean_and_multiple_domain_boundaries_preserve_scan_semantics() {
    let ((), report) = run_async_under_lab(0xd10_0040, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, name) in [(1, "ab"), (2, "ab-reject"), (3, "z"), (4, "other")] {
            batch.create_vertex(VId(id), vec![], vec![(NAME, text(name))]);
        }
        batch.create_vertex(VId(5), vec![], vec![]);
        batch.create_vertex(VId(6), vec![], vec![(NAME, CanonicalScalar::Null)]);
        let at = db.write(&commit, batch).await.unwrap();
        let and = Case::new(Some("ab"), true, true);
        boundary(&db, &cx, at, &and.indexed, &[VId(1)], 2);
        boundary(&db, &cx, at, &and.scan, &[VId(1)], 6);
        for (statement, expected) in [
            (
                "MATCH (n) WHERE n.name STARTS WITH 'ab' OR n.name = 'z' RETURN n",
                vec![VId(1), VId(2), VId(3)],
            ),
            (
                "MATCH (n) WHERE NOT (n.name STARTS WITH 'ab') RETURN n",
                vec![VId(3), VId(4)],
            ),
            (
                "MATCH (n) WHERE (n.name STARTS WITH 'ab' OR n.name = 'z') AND n.name <> 'ab-reject' RETURN n",
                vec![VId(1), VId(3)],
            ),
            (
                "MATCH (n) OPTIONAL MATCH (other) WHERE other=n AND other.name STARTS WITH 'ab' RETURN DISTINCT n",
                (1..=6).map(VId).collect(),
            ),
            (
                "MATCH (n) WHERE n.name STARTS WITH 'ab' OPTIONAL MATCH (other) WHERE other.name='z' RETURN DISTINCT other",
                vec![VId(3)],
            ),
        ] {
            boundary(&db, &cx, at, &query(statement), &expected, 6);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prefix_nontext_properties_preserve_scan_unknowns_including_null_prefix() {
    let ((), report) = run_async_under_lab(0xd10_0041, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        for value in [
            CanonicalScalar::Int(7),
            CanonicalScalar::Bool(true),
            CanonicalScalar::bytes(vec![97, 98]).unwrap(),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![], vec![(NAME, text("ab"))]);
            batch.create_vertex(VId(2), vec![], vec![(NAME, value)]);
            let at = db.write(&commit, batch).await.unwrap();
            for prefix in [Some("ab"), Some(""), None] {
                let case = Case::new(prefix, false, false);
                // The Boolean wrapper converts a scalar NonText failure into
                // UNKNOWN, so mixed properties do not escape as query errors.
                let expected = if prefix.is_some() {
                    vec![VId(1)]
                } else {
                    vec![]
                };
                boundary(&db, &cx, at, &case.indexed, &expected, 2);
                boundary(&db, &cx, at, &case.scan, &expected, 2);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn narrow_prefix_charges_ignore_growth_from_one_thousand_to_fifty_thousand() {
    let mut charges = Vec::new();
    for (seed, total) in [(0xd10_0042_u64, 1_000_usize), (0xd10_0043, 50_000)] {
        let (charged, report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut at = CommitSeq(0);
            for start in (0..total).step_by(1_000) {
                let mut batch = WriteBatch::new(R);
                for id in start..(start + 1_000).min(total) {
                    // Ten stable candidates; unrelated growth lies on both
                    // sides, so a one-sided lower-bound scan cannot pass.
                    let name = if id < 10 {
                        format!("narrow-{id}")
                    } else if id % 2 == 0 {
                        format!("before-{id}")
                    } else {
                        format!("outside-{id}")
                    };
                    batch.create_vertex(VId(id as u128), vec![], vec![(NAME, text(&name))]);
                }
                at = db.write(&commit, batch).await.unwrap();
            }
            let case = Case::new(Some("narrow-"), false, false);
            let expected: Vec<_> = (0..10).map(VId).collect();
            boundary(&db, &cx, at, &case.indexed, &expected, 10);
            let run = db
                .execute_graph_pattern_governed_at(&cx, &case.indexed, at, policy())
                .unwrap();
            (run.rows.snapshot_records, run.evaluator.work_units)
        });
        assert!(report.lab_test_passed(), "total={total}: {report:?}");
        charges.push(charged);
    }
    assert_eq!(charges[0].0, 10);
    assert_eq!(charges[1].0, 10);
    assert_eq!(
        charges[0].1, charges[1].1,
        "prefix work must not grow with unrelated vertices"
    );
}
