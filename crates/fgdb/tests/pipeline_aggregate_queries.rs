//! WITH aggregates execute against real snapshots and canonical staged effects.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, GqlError, MemVfs, ReadError, VertexRow, WriteBatch, WriteError,
    WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphIntegerErrorKind, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphPipelineAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const TOP_GROUPS: &str = "MATCH (n) WITH n AS owner,n.p AS value ORDER BY value DESC NULLS LAST LIMIT $take WHERE value IS NOT NULL WITH value%2 AS bucket,value RETURN bucket,COUNT(*) AS count,SUM(value) AS total,AVG(value) AS mean GROUP BY bucket HAVING count >= $minimum ORDER BY total DESC";
type Summary = (i64, u64, i128, (i128, u64));
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn parameters() -> GqlParameters {
    GqlParameters::new()
        .with_uint64("take", 3)
        .unwrap()
        .with_int64("minimum", 1)
        .unwrap()
}
fn query(text: &str) -> PreparedGraphAggregate {
    PreparedGraphPipelineAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn top_groups() -> PreparedGraphAggregate {
    PreparedGraphPipelineAggregateText::prepare(TOP_GROUPS, symbols)
        .unwrap()
        .bind_parameters(&parameters())
        .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(1, 2), (2, 5), (3, 5), (4, 9)] {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    batch.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Null)]);
    db.write(cx, batch).await.unwrap()
}
fn fraction(sum: i128, count: u64) -> (i128, u64) {
    let (mut a, mut b) = (sum.unsigned_abs(), u128::from(count));
    while b != 0 {
        (a, b) = (b, a % b);
    }
    (sum / a as i128, count / a as u64)
}
fn oracle(vertices: &[VertexRow], take: usize) -> Vec<Summary> {
    // Independent record sorting, pagination and grouping; no GLA, scalar VM,
    // prepared statement or aggregate accumulator contributes to the oracle.
    let mut rows = vertices
        .iter()
        .map(|row| {
            let value = row.props.iter().find_map(|(key, value)| match value {
                CanonicalScalar::Int(value) if *key == P => Some(*value),
                _ => None,
            });
            (row.vid, value)
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        a.1.is_none()
            .cmp(&b.1.is_none())
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut groups: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for (_, value) in rows.into_iter().take(take) {
        if let Some(value) = value {
            groups.entry(value % 2).or_default().push(value);
        }
    }
    let mut result = groups
        .into_iter()
        .map(|(bucket, values)| {
            let count = values.len() as u64;
            let sum = values.iter().map(|value| i128::from(*value)).sum();
            (bucket, count, sum, fraction(sum, count))
        })
        .collect::<Vec<_>>();
    result.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    result
}
fn summaries(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter()
        .map(|row| {
            let CanonicalScalar::Int(bucket) = row.keys()[0].as_scalar().unwrap() else {
                panic!("integer grouping key")
            };
            let mean = row.values()[2].as_average().unwrap();
            (
                *bucket,
                row.values()[0].as_count().unwrap(),
                row.values()[1].as_integer().unwrap(),
                (mean.numerator(), mean.denominator()),
            )
        })
        .collect()
}

#[test]
fn top_k_then_grouping_uses_live_historical_and_pinned_data_through_reopen() {
    let ((), report) = run_async_under_lab(0xa661_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let mut calls = 0;
        let template = PreparedGraphPipelineAggregateText::prepare(TOP_GROUPS, |kind, name| {
            calls += 1;
            symbols(kind, name)
        })
        .unwrap();
        let query = template.bind_parameters(&parameters()).unwrap();
        let frozen = query.canonical_bytes();
        let old = oracle(&db.vertices().unwrap(), 3);
        assert_eq!(old, vec![(1, 3, 19, (19, 3))]);
        assert_eq!(
            summaries(
                &db.execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        let mut update = WriteBatch::new(R);
        update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(20)));
        update.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(3)));
        db.write(&commit, update).await.unwrap();
        let current = oracle(&db.vertices().unwrap(), 3);
        assert_eq!(current, vec![(0, 1, 20, (20, 1)), (1, 2, 14, (7, 1))]);
        assert_eq!(
            summaries(
                &db.execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            current
        );
        assert_eq!(
            summaries(
                &db.execute_graph_aggregate_governed_at(&cx, &query, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            summaries(
                &pinned
                    .execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            summaries(
                &pinned
                    .execute_graph_aggregate_governed_at(&cx, &query, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            summaries(
                &db.execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            current
        );
        assert_eq!(
            summaries(
                &db.execute_graph_aggregate_governed_at(&cx, &query, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            summaries(
                &pinned
                    .execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        let selective = template
            .bind_parameters(
                &GqlParameters::new()
                    .with_uint64("take", 3)
                    .unwrap()
                    .with_int64("minimum", 2)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            summaries(
                &db.execute_graph_aggregate_governed(&cx, &selective, policy())
                    .unwrap()
                    .value
            ),
            vec![(1, 2, 14, (7, 1))]
        );
        assert_eq!(query.canonical_bytes(), frozen);
        assert_eq!(
            template
                .bind_parameters(&parameters())
                .unwrap()
                .canonical_bytes(),
            frozen
        );
        assert_eq!(calls, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_values_change_input_ranking_without_publishing_or_rewriting_effects() {
    let ((), report) = run_async_under_lab(0xa661_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let query = top_groups();
        let old = oracle(&db.vertices().unwrap(), 3);
        let mut txn = db.begin(&txcx).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(30))]);
        changes.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(1)));
        txn.write(&mut db, changes).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let expected = oracle(&txn.vertices(&db).unwrap(), 3);
        assert_eq!(expected, vec![(0, 1, 30, (30, 1)), (1, 2, 10, (5, 1))]);
        let rows = txn
            .execute_graph_aggregate_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(summaries(&rows.value), expected);
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(
            summaries(
                &db.execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        let committed = db.frontier().unwrap();
        assert_eq!(committed.0, basis.0 + 1);
        assert_eq!(
            summaries(
                &db.execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        let mut reader = db.begin(&txcx).unwrap();
        let _ = reader
            .execute_graph_aggregate_governed(&db, &cx, &query, policy())
            .unwrap();
        assert!(matches!(
            reader.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), committed);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn input_pages_empty_groups_having_and_errors_keep_conflict_observations() {
    let ((), report) = run_async_under_lab(0xa661_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let texts = [
            "MATCH (n) WITH n.p AS value ORDER BY value DESC LIMIT 1 RETURN SUM(value) AS total",
            "MATCH (n) WITH n.p AS value WHERE value < 0 RETURN COUNT(*) AS n",
            "MATCH (n) WITH n.p AS value WITH 10/(value-5) AS reciprocal RETURN SUM(reciprocal) AS total LIMIT 0",
            "MATCH (n) WITH n.p AS value RETURN COUNT(*) AS n LIMIT 0",
            "MATCH (n) WITH n AS owner RETURN owner,COUNT(*) AS n GROUP BY owner HAVING n > 100",
        ];
        for (mode, text) in texts.iter().enumerate() {
            for insertion in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txcx).unwrap();
                let mut prefix = WriteBatch::new(R);
                prefix.create_vertex(VId(1000), vec![], vec![]);
                txn.write(&mut db, prefix).unwrap();
                let digest = txn.staged_effect_digest().unwrap();
                let result = txn.execute_graph_aggregate_governed(&db, &cx, &query(text), policy());
                match mode {
                    2 => assert!(
                        matches!(result, Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
                        GraphSetExecutionError::Projection { error, .. }))) if error.kind == GraphIntegerErrorKind::DivisionByZero)
                    ),
                    3 | 4 => assert!(result.unwrap().value.is_empty()),
                    1 => assert_eq!(result.unwrap().value[0].values()[0].as_count(), Some(0)),
                    _ => assert_eq!(result.unwrap().value[0].values()[0].as_integer(), Some(9)),
                }
                assert_eq!(txn.staged_effect_digest().unwrap(), digest);
                let mut winner = WriteBatch::new(R);
                if insertion {
                    winner.create_vertex(VId(77), vec![], vec![(P, CanonicalScalar::Int(50))]);
                } else {
                    winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(50)));
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // No follow-up read may repair a witness omitted by the query.
                assert!(matches!(
                    txn.finish(&mut db, &commit).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(1000)).unwrap().is_none());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_pipeline_and_groups_share_limits_and_preflight_precedes_zero_budgets() {
    let ((), report) = run_async_under_lab(0xa661_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let query = top_groups();
        let measured = db
            .execute_graph_aggregate_governed(&cx, &query, policy())
            .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            caps[1], 1,
            "private input rows do not spend the public group quota"
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(
                &cx,
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            let mut cap = caps;
            cap[dimension] -= 1;
            assert!(
                db.execute_graph_aggregate_governed(
                    &cx,
                    &query,
                    GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3])
                )
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), basis);
        }
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(
            db.execute_graph_aggregate_governed_at(&cx, &query, CommitSeq(basis.0 + 1), zero),
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        let pinned = db.read_session().unwrap();
        assert!(matches!(
            pinned.execute_graph_aggregate_governed_at(&cx, &query, CommitSeq(basis.0 + 1), zero),
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txcx).unwrap();
        assert!(matches!(
            txn.execute_graph_aggregate_governed(&other, &cx, &query, zero),
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                WriteTxnError::WrongDatabase
            )))
        ));
        let measured = txn
            .execute_graph_aggregate_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(
            txn.execute_graph_aggregate_governed(
                &db,
                &cx,
                &query,
                GqlQueryPolicy::new(
                    measured.rows.snapshot_records,
                    measured.rows.result_rows,
                    measured.evaluator.work_units,
                    measured.evaluator.scratch_entries
                )
            )
            .unwrap(),
            measured
        );
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_pipeline_failure_preserves_an_outer_write_that_can_still_commit() {
    let ((), report) = run_async_under_lab(0xa661_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(42))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let fails =
            query("MATCH (n) WITH n.p AS value WITH 10/(value-5) AS q RETURN SUM(q) AS total");
        assert!(
            matches!(txn.execute_graph_aggregate_governed(&db, &cx, &fails, policy()),
            Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
                GraphSetExecutionError::Projection { error, .. }))) if error.kind == GraphIntegerErrorKind::DivisionByZero)
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(txn.vertex(&db, VId(99)).unwrap().is_some());
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert_eq!(db.frontier().unwrap(), basis);
        let safe = query(
            "MATCH (n) WITH n.p AS value WHERE value <> 5 WITH 10/(value-5) AS q RETURN SUM(q) AS total",
        );
        let result = txn
            .execute_graph_aggregate_governed(&db, &cx, &safe, policy())
            .unwrap();
        assert_eq!(result.value[0].values()[0].as_integer(), Some(-1));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(
            db.vertex(VId(2)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(5))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_shortcuts_change_shortest_multiplicity_before_grouping_not_afterward() {
    let ((), report) = run_async_under_lab(0xa661_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut topology = WriteBatch::new(R);
        for (id, source, target) in [(10, 1, 2), (11, 1, 2), (12, 2, 3)] {
            topology.add_edge(EId(id), VId(source), VId(target), vec![]);
        }
        db.write(&commit, topology).await.unwrap();
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut shortcut = WriteBatch::new(R);
        shortcut.add_edge(EId(13), VId(1), VId(3), vec![]);
        txn.write(&mut db, shortcut).unwrap();
        for (mode, old_count, new_count) in [("ALL", 4, 3), ("ANY", 2, 2)] {
            let query = query(&format!(
                "MATCH (a) OPTIONAL MATCH {mode} SHORTEST WALK (a)-[:R*1..2]->(b) WITH a AS owner,b AS peer,b.p AS value RETURN owner,COUNT(*) AS rows,COUNT(peer) AS matched,SUM(value) AS total GROUP BY owner ORDER BY owner"
            ));
            let old = db
                .execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap();
            let staged = txn
                .execute_graph_aggregate_governed(&db, &cx, &query, policy())
                .unwrap();
            for (rows, count) in [(&old.value, old_count), (&staged.value, new_count)] {
                assert_eq!(rows.len(), 5);
                assert_eq!(rows[0].keys()[0].as_vertex(), Some(VId(1)));
                assert_eq!(rows[0].values()[0].as_count(), Some(count));
                assert_eq!(rows[0].values()[1].as_count(), Some(count));
                assert_eq!(
                    rows[0].values()[2].as_integer(),
                    Some(i128::from(count) * 5)
                );
                assert_eq!(rows[4].values()[0].as_count(), Some(1));
                assert_eq!(rows[4].values()[1].as_count(), Some(0));
                assert!(rows[4].values()[2].is_null());
            }
        }
        txn.abort();
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.edge(EId(13)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
