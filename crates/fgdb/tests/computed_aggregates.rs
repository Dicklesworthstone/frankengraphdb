//! Computed aggregate inputs through real storage and the canonical workspace.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphIntegerErrorKind, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const OWNER: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a:Owner) OPTIONAL MATCH WALK (a)-[:R*1..2]->(b) \
    WHERE b.p >= $floor RETURN ABS(a.p) AS bucket,COUNT(*) AS walks,COUNT(b) AS present, \
    SUM(COALESCE(b.p,0)*$scale) AS total,AVG(COALESCE(b.p,0)) AS mean, \
    COUNT(DISTINCT ABS(b.p)) AS unique GROUP BY ABS(a.p) HAVING total>=0 ORDER BY bucket";
type Summary = (Option<i64>, u64, u64, i128, (i128, u64), u64);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 1_000, 5_000_000, 2_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn parameters() -> GqlParameters {
    GqlParameters::new()
        .with_int64("floor", 0)
        .unwrap()
        .with_int64("scale", 2)
        .unwrap()
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(1, -2), (2, 2), (3, 5)] {
        batch.create_vertex(VId(id), vec![OWNER], vec![(P, CanonicalScalar::Int(value))]);
    }
    batch.create_vertex(VId(11), vec![], vec![(P, CanonicalScalar::Int(3))]);
    batch.create_vertex(VId(12), vec![], vec![(P, CanonicalScalar::Int(7))]);
    batch.create_vertex(VId(13), vec![], vec![(P, CanonicalScalar::Null)]);
    for (id, source, target) in [
        (101, 1, 11),
        (102, 1, 11),
        (103, 11, 12),
        (104, 2, 12),
        (105, 2, 13),
        (106, 13, 12),
    ] {
        batch.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn integer(row: &VertexRow) -> Option<i64> {
    row.props.iter().find_map(|(key, value)| match value {
        CanonicalScalar::Int(value) if *key == P => Some(*value),
        _ => None,
    })
}
fn fraction(numerator: i128, denominator: u64) -> (i128, u64) {
    let (mut a, mut b) = (numerator.unsigned_abs(), u128::from(denominator));
    while b != 0 {
        (a, b) = (b, a % b);
    }
    (numerator / a as i128, denominator / a as u64)
}
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Summary> {
    // Raw-record walk enumeration, with per-owner OPTIONAL extension. No GLA,
    // expression VM, aggregate state or graph query entrypoint is used here.
    let mut groups: BTreeMap<Option<i64>, Vec<Option<i64>>> = BTreeMap::new();
    for owner in vertices.iter().filter(|row| row.labels.contains(&OWNER)) {
        let mut layer = vec![owner.vid];
        let mut matches = Vec::new();
        for _ in 0..2 {
            let mut next = Vec::new();
            for source in layer {
                for edge in edges
                    .iter()
                    .filter(|edge| edge.entry.relation == R && edge.entry.src == source)
                {
                    next.push(edge.entry.dst);
                    let target = vertices
                        .iter()
                        .find(|row| row.vid == edge.entry.dst)
                        .unwrap();
                    if let Some(value) = integer(target).filter(|value| *value >= 0) {
                        matches.push(Some(value));
                    }
                }
            }
            layer = next;
        }
        if matches.is_empty() {
            matches.push(None);
        }
        groups
            .entry(integer(owner).map(i64::abs))
            .or_default()
            .extend(matches);
    }
    let mut result: Vec<_> = groups
        .into_iter()
        .map(|(bucket, values)| {
            let count = values.len() as u64;
            let present = values.iter().flatten().count() as u64;
            let sum: i128 = values
                .iter()
                .map(|value| i128::from(value.unwrap_or(0)))
                .sum();
            let unique = values
                .iter()
                .flatten()
                .map(|value| value.abs())
                .collect::<BTreeSet<_>>()
                .len() as u64;
            (
                bucket,
                count,
                present,
                sum * 2,
                fraction(sum, count),
                unique,
            )
        })
        .collect();
    result.sort_by_key(|row| (row.0.is_none(), row.0));
    result
}
fn summaries(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter()
        .map(|row| {
            let bucket = match row.keys()[0].as_scalar().unwrap() {
                CanonicalScalar::Int(value) => Some(*value),
                CanonicalScalar::Null => None,
                _ => panic!("unexpected grouping domain"),
            };
            let mean = row.get(3).unwrap().as_average().unwrap();
            (
                bucket,
                row.get(0).unwrap().as_count().unwrap(),
                row.get(1).unwrap().as_count().unwrap(),
                row.get(2).unwrap().as_integer().unwrap(),
                (mean.numerator(), mean.denominator()),
                row.get(4).unwrap().as_count().unwrap(),
            )
        })
        .collect()
}

#[test]
fn computed_walk_groups_reuse_live_historical_pinned_and_staged_snapshots() {
    let ((), report) = run_async_under_lab(0xa991_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let calls = std::cell::Cell::new(0);
        let template = PreparedGraphAggregateText::prepare(QUERY, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        assert_eq!(calls.get(), 3);
        let plan = template.bind_parameters(&parameters()).unwrap();
        let frozen = plan.canonical_bytes();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(
            old,
            vec![
                (Some(2), 6, 6, 68, (17, 3), 2),
                (Some(5), 1, 0, 0, (0, 1), 0)
            ]
        );
        let mut txn = db.begin(&txcx).unwrap();
        for result in [
            db.execute_graph_aggregate_governed(&cx, &plan, policy())
                .unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &plan, basis, policy())
                .unwrap(),
            pinned
                .execute_graph_aggregate_governed(&cx, &plan, policy())
                .unwrap(),
            pinned
                .execute_graph_aggregate_governed_at(&cx, &plan, basis, policy())
                .unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &plan, policy())
                .unwrap(),
        ] {
            assert_eq!(summaries(&result.value), old);
        }
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(102));
        changes.set_vertex_property(VId(11), P, Some(CanonicalScalar::Int(-1)));
        changes.set_vertex_property(VId(12), P, Some(CanonicalScalar::Int(4)));
        changes.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(8)));
        changes.add_edge(EId(107), VId(3), VId(13), vec![]);
        txn.write(&mut db, changes).unwrap();
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(
            expected,
            vec![
                (Some(2), 1, 1, 8, (4, 1), 1),
                (Some(5), 1, 1, 8, (4, 1), 1),
                (Some(8), 2, 2, 16, (4, 1), 1)
            ]
        );
        assert_eq!(
            summaries(
                &txn.execute_graph_aggregate_governed(&db, &cx, &plan, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            summaries(
                &db.execute_graph_aggregate_governed(&cx, &plan, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        let sequence = txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(sequence.0, basis.0 + 1);
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            summaries(
                &reopened
                    .execute_graph_aggregate_governed(&cx, &plan, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            summaries(
                &reopened
                    .execute_graph_aggregate_governed_at(&cx, &plan, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            summaries(
                &pinned
                    .execute_graph_aggregate_governed(&cx, &plan, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            frozen,
            template
                .bind_parameters(&parameters())
                .unwrap()
                .canonical_bytes()
        );
        assert_eq!(calls.get(), 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_aggregate_failures_and_empty_output_preserve_negative_read_dependencies() {
    let ((), report) = run_async_under_lab(0xa991_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for mode in 0..5 {
            for insertion in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut setup = WriteBatch::new(R);
                setup.set_vertex_property(VId(11), P, Some(CanonicalScalar::Int(1)));
                setup.set_vertex_property(VId(13), P, Some(CanonicalScalar::Int(-1)));
                db.write(&commit, setup).await.unwrap();
                let mut txn = db.begin(&txcx).unwrap();
                let mut prefix = WriteBatch::new(R);
                prefix.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, prefix).unwrap();
                let expression = if mode == 1 { "1/(b.p-1)" } else { "b.p+1" };
                let floor = if mode == 4 { 100 } else { 0 };
                let page = if mode == 3 { " LIMIT 0" } else { "" };
                let plan = prepare(&format!(
                    "MATCH (a:Owner)-[:R]->(b) WHERE b.p>={floor} \
                    RETURN COUNT(*) AS c,SUM({expression}) AS total{page}"
                ));
                let cap = if mode == 2 {
                    GqlQueryPolicy::new(10_000, 0, 5_000_000, 2_000_000)
                } else {
                    policy()
                };
                let result = txn.execute_graph_aggregate_governed(&db, &cx, &plan, cap);
                match mode {
                    1 => assert!(
                        matches!(result, Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. }))
                        if error.kind == GraphIntegerErrorKind::DivisionByZero)
                    ),
                    2 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                    3 => assert!(result.unwrap().value.is_empty()),
                    4 => {
                        let rows = result.unwrap().value;
                        assert_eq!(rows[0].get(0).unwrap().as_count(), Some(0));
                        assert!(rows[0].get(1).unwrap().is_null());
                    }
                    _ => assert_eq!(result.unwrap().value.len(), 1),
                }
                let mut winner = WriteBatch::new(R);
                if insertion {
                    winner.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(101))]);
                    winner.add_edge(EId(199), VId(2), VId(99), vec![]);
                } else {
                    winner.set_vertex_property(VId(13), P, Some(CanonicalScalar::Int(101)));
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // Do not run another transaction read that could repair the
                // query's missing observation before final validation.
                assert!(matches!(
                    txn.commit(&mut db, &commit).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_source_projection_and_output_share_exact_cumulative_limits() {
    let ((), report) = run_async_under_lab(0xa991_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let plan = PreparedGraphAggregateText::prepare(QUERY, symbols)
            .unwrap()
            .bind_parameters(&parameters())
            .unwrap();
        let measured = db
            .execute_graph_aggregate_governed(&cx, &plan, policy())
            .unwrap();
        let exact = GqlQueryPolicy::new(
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(&cx, &plan, exact)
                .unwrap(),
            measured
        );
        assert_eq!(measured.rows.result_rows, 2);
        for budget in [
            GqlQueryPolicy::new(measured.rows.snapshot_records - 1, 100, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(10_000, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(10_000, 100, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(
                10_000,
                100,
                u64::MAX,
                measured.evaluator.scratch_entries - 1,
            ),
        ] {
            assert!(
                db.execute_graph_aggregate_governed(&cx, &plan, budget)
                    .is_err()
            );
        }
        assert_eq!(db.frontier().unwrap(), basis);
        let future = CommitSeq(basis.0 + 1);
        assert!(matches!(
            db.execute_graph_aggregate_governed_at(
                &cx,
                &plan,
                future,
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        let pinned = db.read_session().unwrap();
        assert!(matches!(
            pinned.execute_graph_aggregate_governed_at(
                &cx,
                &plan,
                future,
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_computed_aggregate_overflow_refuses_without_changing_prior_workspace() {
    let ((), report) = run_async_under_lab(0xa991_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(777), vec![], vec![]);
        staged.set_vertex_property(VId(12), P, Some(CanonicalScalar::Int(i64::MAX)));
        txn.write(&mut db, staged).unwrap();
        let before = txn.vertices(&db).unwrap();
        for tail in ["HAVING SUM(b.p*2)>0", "ORDER BY SUM(b.p*2) LIMIT 0"] {
            let plan = prepare(&format!(
                "MATCH (a:Owner)-[:R]->(b) RETURN COUNT(*) AS c {tail}"
            ));
            assert!(
                matches!(txn.execute_graph_aggregate_governed(&db, &cx, &plan, policy()),
                Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. }))
                    if error.kind == GraphIntegerErrorKind::Overflow)
            );
            assert_eq!(txn.vertices(&db).unwrap(), before);
            // The same definition against the original live snapshot succeeds.
            db.execute_graph_aggregate_governed(&cx, &plan, policy())
                .unwrap();
        }
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(777)).unwrap().is_some());
        assert_eq!(
            integer(&db.vertex(VId(12)).unwrap().unwrap()),
            Some(i64::MAX)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
