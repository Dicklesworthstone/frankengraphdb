//! Correlated existence uses one snapshot/overlay, not independent child queries.

use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphExistence, GraphPatternBuilder, GraphValue, GraphValueRow,
    IntegerComparison, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlBudgetDimension, GqlQueryError, GqlQueryPolicy, GraphAggregate, PreparedGraphAggregate,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const PERSON: LabelId = LabelId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const META: PropertyKeyId = PropertyKeyId(2);
const HIGH: VId = VId((1_u128 << 100) + 2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa2; 32],
        DatabaseSecurityNamespaceId([0xa3; 32]),
        [0xa4; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 100, 1_000_000, 100_000)
}
fn pattern(anti: bool) -> PreparedGraphPattern<GraphValueRow> {
    let mut outer = GraphPatternBuilder::new();
    outer.vertex("person").unwrap();
    outer
        .filter("person", VertexPredicate::HasLabel(PERSON))
        .unwrap();
    let mut probe = GraphPatternBuilder::new();
    for name in ["person", "bridge", "company"] {
        probe.vertex(name).unwrap();
    }
    probe
        .edge("person", R, GlaDirection::Forward, "bridge")
        .unwrap();
    probe
        .edge("bridge", S, GlaDirection::Forward, "company")
        .unwrap();
    probe
        .filter(
            "company",
            VertexPredicate::IntegerProperty {
                key: SCORE,
                comparison: IntegerComparison::GreaterOrEqual,
                value: 5,
            },
        )
        .unwrap();
    let constraint = if anti {
        GraphExistence::not_exists(&probe)
    } else {
        GraphExistence::exists(&probe)
    };
    outer
        .prepare_values_with_existence(
            &[constraint],
            &[
                GraphColumn::vertex("owner", "person"),
                GraphColumn::property("meta", "person", META),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut first = WriteBatch::new(R);
    for vid in [VId(1), VId(2), VId(3), VId(6), HIGH] {
        first.create_vertex(
            vid,
            vec![PERSON],
            if vid == VId(2) {
                vec![(META, CanonicalScalar::Int(12))]
            } else {
                vec![]
            },
        );
    }
    for (id, score) in [(10, 0), (11, 0), (20, 7), (21, 0)] {
        first.create_vertex(VId(id), vec![], vec![(SCORE, CanonicalScalar::Int(score))]);
    }
    first.add_edge(EId(1), VId(1), VId(10), vec![]);
    first.add_edge(EId(2), VId(1), VId(10), vec![]);
    first.add_edge(EId(3), VId(2), VId(11), vec![]);
    db.write(cx, first).await.unwrap();
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(4), VId(10), VId(20), vec![]);
    second.add_edge(EId(5), VId(10), VId(20), vec![]);
    second.add_edge(EId(6), VId(11), VId(21), vec![]);
    db.write(cx, second).await.unwrap()
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter()
        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
        .collect()
}
// Enumerate concrete edge pairs over ordinary row reads; no compiler slots,
// probe flags, GLA execution, or projected inner result is shared with product.
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord], anti: bool) -> Vec<Vec<GraphValue>> {
    let mut answer = Vec::new();
    for person in vertices.iter().filter(|row| row.labels.contains(&PERSON)) {
        let exists = edges
            .iter()
            .filter(|edge| edge.entry.relation == R && edge.entry.src == person.vid)
            .any(|first| {
                edges
                    .iter()
                    .filter(|edge| edge.entry.relation == S && edge.entry.src == first.entry.dst)
                    .any(|second| {
                        vertices.iter().any(|row| {
                            row.vid == second.entry.dst && row.props.iter().any(|(key, value)| {
                                *key == SCORE
                                    && matches!(value, CanonicalScalar::Int(score) if *score >= 5)
                            })
                        })
                    })
            });
        if exists != anti {
            answer.push(vec![
                GraphValue::Vertex(person.vid),
                GraphValue::Scalar(
                    person
                        .props
                        .iter()
                        .find(|(key, _)| *key == META)
                        .map(|(_, value)| value.clone())
                        .unwrap_or(CanonicalScalar::Null),
                ),
            ]);
        }
    }
    answer.sort();
    answer
}

#[test]
fn all_five_read_surfaces_keep_isolated_vertices_and_count_both_admitted_tables() {
    let ((), report) = run_async_under_lab(0xe715_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        for anti in [false, true] {
            let query = pattern(anti);
            let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), anti);
            assert_eq!(query.required_vertex_label(), Some(PERSON));
            assert!(!query.plan().scans_edges());
            assert!(query.plan().reads_edges());
            for run in [
                db.execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap(),
                db.execute_graph_pattern_governed_at(&cx, &query, basis, policy())
                    .unwrap(),
                pinned
                    .execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap(),
                pinned
                    .execute_graph_pattern_governed_at(&cx, &query, basis, policy())
                    .unwrap(),
                txn.execute_graph_pattern_governed(&db, &cx, &query, policy())
                    .unwrap(),
            ] {
                assert_eq!(plain(&run.value), expected);
                assert_eq!(run.rows.snapshot_records, 15);
                assert_eq!(run.rows.result_rows, if anti { 4 } else { 1 });
            }
        }
        let query = pattern(true);
        let full = db
            .execute_graph_pattern_governed(&cx, &query, policy())
            .unwrap();
        assert_eq!(ids(&full.value), vec![VId(2), VId(3), VId(6), HIGH]);
        let exact = GqlQueryPolicy::new(
            15,
            4,
            full.evaluator.work_units,
            full.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_pattern_governed(&cx, &query, exact)
                .unwrap(),
            full
        );
        for cap in [
            GqlQueryPolicy::new(15, 4, full.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(15, 4, u64::MAX, full.evaluator.scratch_entries - 1),
        ] {
            assert!(
                matches!(db.execute_graph_pattern_governed(&cx, &query, cap),
                Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1)
            );
        }
        assert!(matches!(db.execute_graph_pattern_governed(&cx, &query,
            GqlQueryPolicy::new(14, 4, u64::MAX, u64::MAX)),
            Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords && error.observed == 15));
        assert!(matches!(db.execute_graph_pattern_governed(&cx, &query,
            GqlQueryPolicy::new(15, 3, u64::MAX, u64::MAX)),
            Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 4));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn canonical_staging_moves_rows_between_exists_and_not_exists_without_rebasing_history() {
    let ((), report) = run_async_under_lab(0xe715_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let query = pattern(true);
        let before = db
            .execute_graph_pattern_governed(&cx, &query, policy())
            .unwrap()
            .value;
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.delete_edge(EId(1));
        stage.delete_edge(EId(2));
        stage.ensure_edge_by_triple(EId(999), VId(2), VId(11), vec![]);
        stage.add_edge(EId(7), VId(3), VId(10), vec![]);
        stage.set_vertex_property(VId(21), SCORE, Some(CanonicalScalar::Int(9)));
        stage.set_vertex_property(VId(1), META, Some(CanonicalScalar::Int(900)));
        stage.delete_vertex(VId(6));
        txn.write(&mut db, stage).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap(), true);
        let staged = txn
            .execute_graph_pattern_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(plain(&staged.value), expected);
        assert_eq!(ids(&staged.value), vec![VId(1), HIGH]);
        assert_eq!(staged.rows.snapshot_records, 13);
        let exact = GqlQueryPolicy::new(
            13,
            2,
            staged.evaluator.work_units,
            staged.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_pattern_governed(&db, &cx, &query, exact)
                .unwrap(),
            staged
        );
        assert_eq!(
            db.execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap()
                .value,
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            plain(
                &db.execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            reopened
                .execute_graph_pattern_governed_at(&cx, &query, basis, policy())
                .unwrap()
                .value,
            before
        );
        assert_eq!(
            pinned
                .execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap()
                .value,
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn absence_is_a_read_dependency_even_after_output_refusal_but_unlabeled_insertions_are_disjoint() {
    let ((), report) = run_async_under_lab(0xe715_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        for refused in [false, true] {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let query = pattern(true);
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R);
                stage.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let result = txn.execute_graph_pattern_governed(
                    &db,
                    &cx,
                    &query,
                    GqlQueryPolicy::new(100, if refused { 0 } else { 100 }, 1_000_000, 100_000),
                );
                if refused {
                    assert!(matches!(result, Err(GqlQueryError::Rows(_))));
                } else {
                    assert!(ids(&result.unwrap().value).contains(&VId(3)));
                }
                let mut winner = WriteBatch::new(R);
                match change {
                    0 => winner.create_vertex(VId(88), vec![], vec![]),
                    1 => winner.add_edge(EId(8), VId(3), VId(10), vec![]),
                    _ => winner.set_vertex_property(VId(21), SCORE, Some(CanonicalScalar::Int(9))),
                };
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if change == 0 {
                    result.unwrap();
                    assert!(db.vertex(VId(99)).unwrap().is_some());
                } else {
                    assert!(matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(99)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_root_correlations_and_streaming_aggregation_reuse_the_same_probe() {
    let ((), report) = run_async_under_lab(0xe715_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut outer = GraphPatternBuilder::new();
        for name in ["person", "bridge"] {
            outer.vertex(name).unwrap();
        }
        outer
            .edge("person", R, GlaDirection::Forward, "bridge")
            .unwrap();
        let mut probe = GraphPatternBuilder::new();
        for name in ["bridge", "company"] {
            probe.vertex(name).unwrap();
        }
        probe
            .edge("bridge", S, GlaDirection::Forward, "company")
            .unwrap();
        probe
            .filter(
                "company",
                VertexPredicate::IntegerProperty {
                    key: SCORE,
                    comparison: IntegerComparison::GreaterOrEqual,
                    value: 5,
                },
            )
            .unwrap();
        let query = outer
            .prepare_values_with_existence(
                &[GraphExistence::exists(&probe)],
                &[GraphColumn::vertex("owner", "person")],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let rows = db
            .execute_graph_pattern_governed(&cx, &query, policy())
            .unwrap();
        assert_eq!(rows.rows.snapshot_records, 6);
        assert_eq!(ids(&rows.value), vec![VId(1), VId(1)]);
        let summary = PreparedGraphAggregate::prepare(
            query,
            &[],
            &[GraphAggregate::count_rows("qualified")],
            0,
            None,
        )
        .unwrap();
        assert_eq!(
            db.execute_graph_aggregate_governed(&cx, &summary, policy())
                .unwrap()
                .value[0]
                .get(0)
                .unwrap()
                .as_count(),
            Some(2)
        );
        let summary = PreparedGraphAggregate::prepare(
            pattern(true),
            &[],
            &[GraphAggregate::count_rows("missing")],
            0,
            None,
        )
        .unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let rows = txn
            .execute_graph_aggregate_governed(&db, &cx, &summary, policy())
            .unwrap();
        assert_eq!(rows.rows.snapshot_records, 15);
        assert_eq!(rows.value[0].get(0).unwrap().as_count(), Some(4));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn wrong_owner_future_snapshot_and_cancellation_never_become_successful_absence() {
    let ((), report) = run_async_under_lab(0xe715_0005, |root| async move {
        // Cancel the query child, keeping the lab supervisor available to join it.
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let cx = contexts.query();
                let txn_cx = contexts.txn();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let foreign = Database::open_memory(&commit, keys()).await.unwrap();
                let txn = db.begin(&txn_cx).unwrap();
                let query = pattern(true);
                root.cancel_with(CancelKind::User, Some("correlated existence cancellation"));
                assert!(matches!(
                    txn.execute_graph_pattern_governed(&foreign, &cx, &query, policy()),
                    Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))
                ));
                let future = CommitSeq(db.frontier().unwrap().0 + 1);
                assert!(matches!(
                    db.execute_graph_pattern_governed_at(&cx, &query, future, policy()),
                    Err(GqlQueryError::Source(GqlError::Read(
                        ReadError::BeyondFrontier { .. }
                    )))
                ));
                assert!(matches!(
                    db.execute_graph_pattern_governed(&cx, &query, policy()),
                    Err(GqlQueryError::Interrupted(_))
                ));
                txn.abort();
            })
            .expect("lab query task can be spawned");
        assert_eq!(handle.join(&root).await, Ok(()));
        assert!(root.checkpoint().is_ok(), "the supervisor remains live");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
