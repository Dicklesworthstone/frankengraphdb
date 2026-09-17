//! Compound exact summaries use real snapshot and canonical transaction readers.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, MemVfs, ReadError, VertexRow,
    WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::IntegerComparison;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphAggregateColumn,
    GraphAggregateError, GraphAggregateFilter, GraphAggregateRow, GraphAggregateTest,
    GraphIntegerErrorKind, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    PreparedGraphSet, PreparedGraphSetAggregate, PreparedGraphSetText,
};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId,
    EId, EmbeddedTxnCompletion, PurposeContexts, VId};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const A: LabelId = LabelId(1);
const B: LabelId = LabelId(2);
const SOURCE: LabelId = LabelId(3);
const R: RelationId = RelationId(1);
const TOP: &str = "(MATCH (n:A) RETURN n.p AS amount ORDER BY amount DESC LIMIT $left) UNION ALL (MATCH (n:B) WITH n.p AS amount WHERE amount IS NOT NULL RETURN amount ORDER BY amount DESC LIMIT $right)";
const UNION: &str = "MATCH (n:A) RETURN n.p AS amount UNION ALL MATCH (n:B) RETURN n.p AS amount";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc1; 32], DatabaseSecurityNamespaceId([0xc2; 32]), [0xc3; 32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(A)),
        (GraphSymbolKind::Label, "B") => Some(GraphSymbol::Label(B)),
        (GraphSymbolKind::Label, "Source") => Some(GraphSymbol::Label(SOURCE)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn relation(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn summary(input: PreparedGraphSet, grouped: bool, count: Option<u64>) -> PreparedGraphSetAggregate {
    PreparedGraphSetAggregate::prepare(input, if grouped { &[0] } else { &[] }, &[
        GraphAggregate::count_rows("rows"), GraphAggregate::sum_int("total", 0),
        GraphAggregate::average_int("mean", 0),
    ], 0, count).unwrap()
}
fn grouped(rows: &[GraphAggregateRow]) -> Vec<(i64, u64, i128)> {
    rows.iter().map(|row| {
        let Some(CanonicalScalar::Int(key)) = row.keys()[0].as_scalar() else { panic!("integer key") };
        (*key, row.values()[0].as_count().unwrap(), row.values()[1].as_integer().unwrap())
    }).collect()
}
fn top_oracle(vertices: &[VertexRow]) -> Vec<(i64, u64, i128)> {
    // Raw storage rows, independent sorting and grouping. No query/set/summary
    // engine contributes to this oracle. Every duplicate remains an occurrence.
    let mut selected = Vec::new();
    for (label, count) in [(A, 2), (B, 1)] {
        let mut values = vertices.iter().filter(|row| row.labels.contains(&label))
            .filter_map(|row| row.props.iter().find_map(|(key, value)| match value {
                CanonicalScalar::Int(value) if *key == P => Some(*value), _ => None,
            })).collect::<Vec<_>>();
        values.sort_by(|a, b| b.cmp(a)); values.truncate(count); selected.extend(values);
    }
    let mut groups = BTreeMap::<i64, u64>::new();
    for value in selected { *groups.entry(value).or_default() += 1; }
    groups.into_iter().map(|(value, count)| (value, count, i128::from(value) * i128::from(count))).collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, label, value) in [(1, A, 2), (2, A, 5), (3, A, 5), (4, B, 5), (5, B, 9)] {
        batch.create_vertex(VId(id), vec![label], vec![(P, CanonicalScalar::Int(value))]);
    }
    batch.create_vertex(VId(6), vec![B], vec![(P, CanonicalScalar::Null)]);
    db.write(cx, batch).await.unwrap()
}

#[test]
fn compound_local_top_k_groups_survive_staging_compaction_history_and_reopen() {
    let ((), report) = run_async_under_lab(0xc057_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let mut catalog_calls = 0;
        let template = PreparedGraphSetText::prepare(TOP, |kind, name| {
            catalog_calls += 1; symbols(kind, name)
        }).unwrap();
        assert_eq!(catalog_calls, 3);
        let arguments = GqlParameters::new().with_uint64("left", 2).unwrap().with_uint64("right", 1).unwrap();
        let query = summary(template.bind_parameters(&arguments).unwrap(), true, None);
        let identity = query.canonical_bytes();
        let old = top_oracle(&db.vertices().unwrap());
        assert_eq!(old, vec![(5, 2, 10), (9, 1, 9)]);
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        for result in [
            db.execute_graph_set_aggregate_governed(&cx, &query, wide()).unwrap(),
            db.execute_graph_set_aggregate_governed_at(&cx, &query, basis, wide()).unwrap(),
            pinned.execute_graph_set_aggregate_governed(&cx, &query, wide()).unwrap(),
            pinned.execute_graph_set_aggregate_governed_at(&cx, &query, basis, wide()).unwrap(),
            txn.execute_graph_set_aggregate_governed(&db, &cx, &query, wide()).unwrap(),
        ] { assert_eq!(grouped(&result.value), old); }
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(20)));
        changes.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(4)));
        txn.write(&mut db, changes).unwrap();
        let new = top_oracle(&txn.vertices(&db).unwrap());
        assert_eq!(new, vec![(5, 2, 10), (20, 1, 20)]);
        assert_eq!(grouped(&txn.execute_graph_set_aggregate_governed(&db, &cx, &query, wide()).unwrap().value), new);
        assert_eq!(grouped(&db.execute_graph_set_aggregate_governed(&cx, &query, wide()).unwrap().value), old);
        let committed = txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(committed.0, basis.0 + 1);
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(grouped(&reopened.execute_graph_set_aggregate_governed(&cx, &query, wide()).unwrap().value), new);
        assert_eq!(grouped(&reopened.execute_graph_set_aggregate_governed_at(&cx, &query, basis, wide()).unwrap().value), old);
        assert_eq!(grouped(&pinned.execute_graph_set_aggregate_governed(&cx, &query, wide()).unwrap().value), old);
        assert_eq!(identity, summary(template.bind_parameters(&arguments).unwrap(), true, None).canonical_bytes());
        assert_eq!(catalog_calls, 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_operand_admission_and_grouping_use_one_exact_budget_in_each_reader() {
    let ((), report) = run_async_under_lab(0xc057_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let query = summary(relation(UNION), false, None);
        let set = db.execute_graph_set_governed(&cx, query.input(), wide()).unwrap();
        assert_eq!(set.value.len(), 6);
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        for mode in 0..3 {
            let execute = |policy| match mode {
                0 => db.execute_graph_set_aggregate_governed(&cx, &query, policy)
                    .map_err(|error| format!("{error:?}")),
                1 => pinned.execute_graph_set_aggregate_governed(&cx, &query, policy)
                    .map_err(|error| format!("{error:?}")),
                _ => txn.execute_graph_set_aggregate_governed(&db, &cx, &query, policy)
                    .map_err(|error| format!("{error:?}")),
            };
            let measured = execute(wide()).unwrap();
            if mode != 2 { assert_eq!(measured.rows.snapshot_records, set.rows.snapshot_records); }
            let row = &measured.value[0];
            assert_eq!(row.values()[0].as_count(), Some(6));
            assert_eq!(row.values()[1].as_integer(), Some(26));
            let average = row.values()[2].as_average().unwrap();
            assert_eq!((average.numerator(), average.denominator()), (26, 5));
            let caps = [measured.rows.snapshot_records, measured.rows.result_rows,
                measured.evaluator.work_units, measured.evaluator.scratch_entries];
            assert_eq!(caps[1], 1);
            assert_eq!(execute(GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])).unwrap(), measured);
            for dimension in 0..4 {
                let mut cap = caps; cap[dimension] -= 1;
                assert!(execute(GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3])).is_err());
            }
        }
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn second_operand_negative_and_canceled_rows_still_conflict_after_empty_or_hidden_groups() {
    let ((), report) = run_async_under_lab(0xc057_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        for operation in ["EXCEPT", "INTERSECT"] {
            for absent in [false, true] {
                for mode in 0..3 {
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    let mut base = WriteBatch::new(R);
                    base.create_vertex(VId(1), vec![A], vec![(P, CanonicalScalar::Int(7))]);
                    if !absent { base.create_vertex(VId(2), vec![B], vec![(P, CanonicalScalar::Int(7))]); }
                    db.write(&commit, base).await.unwrap();
                    let mut txn = db.begin(&txcx).unwrap();
                    let mut prefix = WriteBatch::new(R); prefix.create_vertex(VId(777), vec![], vec![]);
                    txn.write(&mut db, prefix).unwrap();
                    let input = relation(&format!("MATCH (a:A) RETURN a.p AS amount {operation} DISTINCT MATCH (b:B) RETURN b.p AS amount"));
                    let mut query = summary(input, false, if mode == 1 { Some(0) } else { None });
                    if mode == 2 {
                        query = query.with_result_clauses(&[GraphAggregateFilter {
                            column: GraphAggregateColumn::Aggregate(0), test: GraphAggregateTest::Integer {
                                comparison: IntegerComparison::Greater, value: 100,
                            },
                        }], &[]).unwrap();
                    }
                    let result = txn.execute_graph_set_aggregate_governed(&db, &cx, &query, wide()).unwrap();
                    if mode != 0 { assert!(result.value.is_empty()); }
                    let mut winner = WriteBatch::new(R);
                    if absent { winner.create_vertex(VId(2), vec![B], vec![(P, CanonicalScalar::Int(7))]); }
                    else { winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(9))); }
                    db.write(&commit, winner).await.unwrap();
                    let frontier = db.frontier().unwrap();
                    // No intervening transaction query is allowed to repair a
                    // missing second-operand witness before this validation.
                    assert!(matches!(txn.commit(&mut db, &commit).await,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                }
            }
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_second_operand_arithmetic_failure_preserves_outer_effects_and_observations() {
    let ((), report) = run_async_under_lab(0xc057_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        for concurrent_change in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut base = WriteBatch::new(R);
            base.create_vertex(VId(1), vec![A], vec![(P, CanonicalScalar::Int(5))]);
            base.create_vertex(VId(2), vec![B], vec![(P, CanonicalScalar::Int(0))]);
            let basis = db.write(&commit, base).await.unwrap();
            let query = summary(relation("MATCH (a:A) RETURN a.p AS amount UNION ALL MATCH (b:B) RETURN 10/b.p AS amount"), false, Some(0));
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R); prefix.create_vertex(VId(777), vec![], vec![]);
            txn.write(&mut db, prefix).unwrap();
            assert!(matches!(txn.execute_graph_set_aggregate_governed(&db, &cx, &query, wide()),
                Err(GqlQueryError::Source(GraphAggregateError::InputRelation(GraphSetExecutionError::Projection { error, .. })))
                    if error.kind == GraphIntegerErrorKind::DivisionByZero));
            assert_eq!(db.frontier().unwrap(), basis);
            if concurrent_change {
                let mut winner = WriteBatch::new(R);
                winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(2)));
                db.write(&commit, winner).await.unwrap();
                assert!(matches!(txn.commit(&mut db, &commit).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                assert!(db.vertex(VId(777)).unwrap().is_none());
            } else {
                assert_eq!(txn.commit(&mut db, &commit).await.unwrap().0, basis.0 + 1);
                assert!(db.vertex(VId(777)).unwrap().is_some());
                assert_eq!(db.vertex(VId(2)).unwrap().unwrap().props[0].1, CanonicalScalar::Int(0));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_shortest_selectors_in_separate_arms_observe_the_same_staged_topology() {
    let ((), report) = run_async_under_lab(0xc057_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut base = WriteBatch::new(R);
        base.create_vertex(VId(1), vec![SOURCE], vec![]);
        base.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(5))]);
        base.create_vertex(VId(3), vec![], vec![(P, CanonicalScalar::Int(9))]);
        base.add_edge(EId(10), VId(1), VId(2), vec![]);
        base.add_edge(EId(11), VId(1), VId(2), vec![]);
        base.add_edge(EId(12), VId(2), VId(3), vec![]);
        let basis = db.write(&commit, base).await.unwrap();
        let query = summary(relation("MATCH ALL SHORTEST WALK (a:Source)-[:R*1..2]->(b) RETURN b.p AS amount UNION ALL MATCH ANY SHORTEST WALK (a:Source)-[:R*1..2]->(b) RETURN b.p AS amount"), false, None);
        let old = db.execute_graph_set_aggregate_governed(&cx, &query, wide()).unwrap();
        assert_eq!(old.value[0].values()[0].as_count(), Some(6));
        assert_eq!(old.value[0].values()[1].as_integer(), Some(42));
        let mut txn = db.begin(&txcx).unwrap();
        let mut shortcut = WriteBatch::new(R); shortcut.add_edge(EId(13), VId(1), VId(3), vec![]);
        txn.write(&mut db, shortcut).unwrap();
        let new = txn.execute_graph_set_aggregate_governed(&db, &cx, &query, wide()).unwrap();
        assert_eq!(new.value[0].values()[0].as_count(), Some(5));
        assert_eq!(new.value[0].values()[1].as_integer(), Some(33));
        let average = new.value[0].values()[2].as_average().unwrap();
        assert_eq!((average.numerator(), average.denominator()), (33, 5));
        txn.abort();
        assert_eq!(db.execute_graph_set_aggregate_governed(&cx, &query, wide()).unwrap(), old);
        assert_eq!(db.frontier().unwrap(), basis);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn wrong_owner_finished_transaction_and_future_history_precede_zero_budgets() {
    let ((), report) = run_async_under_lab(0xc057_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let query = summary(relation(UNION), false, Some(0));
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        let pinned = db.read_session().unwrap();
        for result in [
            db.execute_graph_set_aggregate_governed_at(&cx, &query, CommitSeq(basis.0 + 1), zero),
            pinned.execute_graph_set_aggregate_governed_at(&cx, &query, CommitSeq(basis.0 + 1), zero),
        ] { assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source(
            GqlError::Read(ReadError::BeyondFrontier { .. })))))); }
        let mut txn = db.begin(&txcx).unwrap();
        assert!(matches!(txn.execute_graph_set_aggregate_governed(&other, &cx, &query, zero),
            Err(GqlQueryError::Source(GraphAggregateError::Source(WriteTxnError::WrongDatabase)))));
        assert!(txn.execute_graph_set_aggregate_governed(&db, &cx, &query, wide()).unwrap().value.is_empty());
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::ReadClosed { .. }));
        assert!(matches!(txn.execute_graph_set_aggregate_governed(&db, &cx, &query, zero),
            Err(GqlQueryError::Source(GraphAggregateError::Source(WriteTxnError::Finished)))));
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
