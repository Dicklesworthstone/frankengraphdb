// Included in vertex_scan_read_tests: exercise the public aggregate path with
// its ordinary transaction fixtures, not a mock accumulator or validator.
use fgdb_gql::{GraphAggregate, GraphAggregateError, GqlQueryError, PreparedGraphAggregate};
use fgdb_gql::algebra::GraphColumn;

fn aggregate_for(
    value: i64,
    keys: &[usize],
    aggregates: &[GraphAggregate<'_>],
    count: Option<u64>,
) -> PreparedGraphAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.filter("n", VertexPredicate::HasLabel(LABEL)).unwrap();
    builder.filter("n", VertexPredicate::IntegerProperty {
        key: P,
        comparison: IntegerComparison::Equal,
        value,
    }).unwrap();
    let input = builder.prepare_values(
        &[
            GraphColumn::property("value", "n", P),
            GraphColumn::property("group_key", "n", Q),
        ],
        0,
        None,
    ).unwrap().with_duplicates();
    PreparedGraphAggregate::prepare(input, keys, aggregates, 0, count).unwrap()
}

#[test]
fn zero_count_then_insert_allows_different_keys_but_rejects_the_same_key_race() {
    let ((), report) = run_async_under_lab(0x7653_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for same_key in [false, true] {
            let mut database = fixture(&cx).await;
            let mut first = database.begin(&txcx).unwrap();
            let mut second = database.begin(&txcx).unwrap();
            let second_key = if same_key { 42 } else { 43 };
            for (transaction, value) in [(&first, 42), (&second, second_key)] {
                let query = aggregate_for(value, &[], &[GraphAggregate::count_rows("count")], None);
                let result = transaction.execute_graph_aggregate_governed(
                    &database, &qcx, &query, policy(),
                ).unwrap();
                assert_eq!(result.value.len(), 1);
                assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(0));
                assert!(transaction.read_set.borrow().is_empty());
                assert!(!transaction.scanned_vertices.get());
                assert!(transaction.scanned_vertex_labels.borrow().is_empty());
                assert!(transaction.point_reads.borrow().2[0].complete);
            }
            let mut a = WriteBatch::new(RelationId(1));
            a.create_vertex(VId(50), vec![LABEL], vec![(P, CanonicalScalar::Int(42))]);
            first.write(&mut database, a).unwrap();
            let mut b = WriteBatch::new(RelationId(1));
            b.create_vertex(VId(51), vec![LABEL], vec![(P, CanonicalScalar::Int(second_key))]);
            second.write(&mut database, b).unwrap();
            let before = database.frontier().unwrap();
            first.commit(&mut database, &cx).await.unwrap();
            let result = second.finish(&mut database, &cx).await;
            if same_key {
                assert!(is_read_conflict(result));
                assert!(database.vertex(VId(51)).unwrap().is_none());
            } else {
                assert!(matches!(result, Ok(EmbeddedTxnCompletion::WriteCommitted { .. })));
                assert_eq!(database.vertex(VId(51)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(43))]);
            }
            assert!(database.vertex(VId(50)).unwrap().is_some());
            assert_eq!(database.delta_since(before).unwrap().count(), if same_key { 1 } else { 2 });
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouping_and_exact_sums_keep_values_without_observing_nonmembers() {
    let ((), report) = run_async_under_lab(0x7653_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut seed = WriteBatch::new(RelationId(1));
        seed.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(2)));
        seed.create_vertex(VId(5), vec![LABEL], vec![
            (P, CanonicalScalar::Int(7)), (Q, CanonicalScalar::Int(3)),
        ]);
        database.write(&cx, seed).await.unwrap();
        let mut transaction = database.begin(&txcx).unwrap();
        let query = aggregate_for(7, &[1], &[
            GraphAggregate::count_rows("count"), GraphAggregate::sum_int("total", 0),
        ], None);
        let result = transaction.execute_graph_aggregate_governed(
            &database, &qcx, &query, policy(),
        ).unwrap();
        assert_eq!(result.value.len(), 2);
        for (row, key) in result.value.iter().zip([2, 3]) {
            assert_eq!(row.keys(), &[fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(key))]);
            assert_eq!(row.get(0).unwrap().as_count(), Some(1));
            assert_eq!(row.get(1).unwrap().as_integer(), Some(7));
        }
        assert_eq!(transaction.read_set.borrow().iter().copied().collect::<Vec<_>>(),
            vec![ElementId::Vertex(VId(1)), ElementId::Vertex(VId(5))]);
        let mut outside = WriteBatch::new(RelationId(1));
        outside.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(4)));
        outside.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(9)));
        outside.create_vertex(VId(6), vec![LABEL], vec![(P, CanonicalScalar::Int(8))]);
        database.write(&cx, outside).await.unwrap();
        assert!(matches!(transaction.finish(&mut database, &cx).await,
            Ok(EmbeddedTxnCompletion::ReadClosed { .. })));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_output_group_page_does_not_erase_the_input_predicate_domain() {
    let ((), report) = run_async_under_lab(0x7653_0103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        let query = aggregate_for(7, &[], &[GraphAggregate::count_rows("count")], Some(0));
        let result = transaction.execute_graph_aggregate_governed(
            &database, &qcx, &query, policy(),
        ).unwrap();
        assert!(result.value.is_empty());
        assert!(transaction.point_reads.borrow().2[0].complete);
        let mut winner = WriteBatch::new(RelationId(1));
        winner.create_vertex(VId(5), vec![LABEL], vec![(P, CanonicalScalar::Int(7))]);
        database.write(&cx, winner).await.unwrap();
        assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_data_and_final_result_failures_leave_pending_witnesses_after_retry() {
    let ((), report) = run_async_under_lab(0x7653_0104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for data_error in [false, true] {
            let mut database = fixture(&cx).await;
            let mut seed = WriteBatch::new(RelationId(1));
            seed.set_vertex_property(VId(1), Q,
                Some(CanonicalScalar::ucs_basic_text("not an integer").unwrap()));
            database.write(&cx, seed).await.unwrap();
            let mut transaction = database.begin(&txcx).unwrap();
            transaction.savepoint(&database, "before-aggregate").unwrap();
            let aggregate = if data_error {
                GraphAggregate::sum_int("total", 1)
            } else {
                GraphAggregate::count_rows("count")
            };
            let query = aggregate_for(7, &[], &[aggregate], None);
            let limits = if data_error { policy() } else {
                GqlQueryPolicy::new(10_000, 0, 1_000_000, 1_000_000)
            };
            let failure = transaction.execute_graph_aggregate_governed(
                &database, &qcx, &query, limits,
            );
            if data_error {
                assert!(matches!(failure,
                    Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 0 }))));
            } else {
                assert!(matches!(failure, Err(GqlQueryError::Rows(_))));
            }
            assert!(!transaction.point_reads.borrow().2[0].complete);
            let count = aggregate_for(7, &[], &[GraphAggregate::count_rows("count")], None);
            let result = transaction.execute_graph_aggregate_governed(
                &database, &qcx, &count, policy(),
            ).unwrap();
            assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(1));
            assert_eq!(transaction.point_reads.borrow().2.iter().map(|scan| scan.complete)
                .collect::<Vec<_>>(), vec![false, true]);
            transaction.rollback_to_savepoint(&database, "before-aggregate").unwrap();
            let mut outside = WriteBatch::new(RelationId(1));
            outside.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(9)));
            database.write(&cx, outside).await.unwrap();
            assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_over_staged_values_keeps_dependencies_after_rollback() {
    let ((), report) = run_async_under_lab(0x7653_0105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        transaction.savepoint(&database, "original").unwrap();
        let mut staged = WriteBatch::new(RelationId(1));
        staged.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(7)));
        transaction.write(&mut database, staged).unwrap();
        let query = aggregate_for(7, &[], &[
            GraphAggregate::count_rows("count"), GraphAggregate::sum_int("total", 0),
        ], None);
        let result = transaction.execute_graph_aggregate_governed(
            &database, &qcx, &query, policy(),
        ).unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(2));
        assert_eq!(result.value[0].get(1).unwrap().as_integer(), Some(14));
        transaction.rollback_to_savepoint(&database, "original").unwrap();
        assert!(transaction.prepared.is_none());
        assert!(transaction.read_set.borrow().contains(&ElementId::Vertex(VId(2))));
        let mut winner = WriteBatch::new(RelationId(1));
        winner.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(9)));
        database.write(&cx, winner).await.unwrap();
        assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
        assert_eq!(database.vertex(VId(2)).unwrap().unwrap().props[0],
            (P, CanonicalScalar::Int(2)));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_with_independent_roots_keeps_the_conservative_source_profile() {
    let ((), report) = run_async_under_lab(0x7653_0106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut database = fixture(&cx).await;
        let mut transaction = database.begin(&txcx).unwrap();
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        builder.vertex("m").unwrap();
        builder.filter("n", VertexPredicate::HasLabel(LABEL)).unwrap();
        builder.filter("n", VertexPredicate::IntegerProperty {
            key: P, comparison: IntegerComparison::Equal, value: 7,
        }).unwrap();
        let input = builder.prepare_values(&[GraphColumn::vertex("n", "n")], 0, None)
            .unwrap().with_duplicates();
        let query = PreparedGraphAggregate::prepare(
            input, &[], &[GraphAggregate::count_rows("count")], 0, None,
        ).unwrap();
        let result = transaction.execute_graph_aggregate_governed(
            &database, &qcx, &query, policy(),
        ).unwrap();
        assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(4));
        assert!(transaction.point_reads.borrow().2.is_empty());
        assert!(transaction.scanned_vertices.get());
        let mut winner = WriteBatch::new(RelationId(1));
        winner.create_vertex(VId(5), vec![OTHER], vec![]);
        database.write(&cx, winner).await.unwrap();
        assert!(is_read_conflict(transaction.finish(&mut database, &cx).await));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
