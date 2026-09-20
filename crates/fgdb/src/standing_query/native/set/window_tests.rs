//! Terminal ranked selection is native computation, not presentation sorting.
use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, IntegerComparison};
use fgdb_gql::{GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
    GraphSetOperand, GraphSetPredicateOp, GraphSetProjection, GraphSetValue,
    GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const P: PropertyKeyId = PropertyKeyId(1);
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000) }
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x31; 32], DatabaseSecurityNamespaceId([0x32; 32]), [0x33; 32]) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) { (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)), _ => None }
}
fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap().into()
}
fn ordered(input: PreparedGraphSet, offset: u64, count: u64) -> PreparedGraphSet {
    input.with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(offset, Some(count))
}
fn seed() -> WriteBatch {
    let mut seed = WriteBatch::new(RelationId(1));
    for (id, value) in [(1, 9), (2, 7), (3, 7), (4, 2)] {
        seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    seed
}
fn definition() -> PreparedGraphSet {
    ordered(leaf().combine(GraphSetOperation::Union, GraphSetQuantifier::All, leaf()).unwrap(), 1, 3)
}
fn scalar_result(values: impl IntoIterator<Item = i64>) -> QueryResult {
    QueryResult::Rows { columns: vec!["p".into()], rows: values.into_iter()
        .map(|v| vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(v)))]).collect() }
}
fn oracle<V: Vfs + Clone>(db: &Database<V>) -> QueryResult {
    let mut values = Vec::new();
    for vertex in db.vertices_at(db.frontier().unwrap()).unwrap() {
        let (_, CanonicalScalar::Int(value)) = &vertex.props[0] else { panic!("integer fixture"); };
        values.extend([*value, *value]);
    }
    values.sort_unstable_by(|a, b| b.cmp(a));
    scalar_result(values.into_iter().skip(1).take(3))
}

#[test]
fn text_and_bound_terminal_windows_deliver_exact_ranked_rows_after_updates_and_rebuild() {
    let ((), report) = run_async_under_lab(0x7769_0201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "(MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p) ORDER BY p DESC SKIP 1 LIMIT 3";
        let params = GqlParameters::new();
        let native = db.register_standing_native(&cx, text, &params, symbols, policy()).unwrap();
        let bound = db.register_standing_relation(&cx, &definition(), policy()).unwrap();
        assert_eq!(db.standing_native_query(&cx, &native, policy()).unwrap().1, scalar_result([9, 7, 7]));
        for tick in 0..3 {
            let mut edit = WriteBatch::new(RelationId(1));
            match tick {
                0 => { edit.delete_vertex(VId(1)); }
                1 => { edit.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(1))); }
                _ => { edit.create_vertex(VId(u128::MAX), vec![], vec![(P, CanonicalScalar::Int(8))]); }
            }
            let at = db.write(&commit, edit).await.unwrap();
            for handle in [&native, &bound] {
                let actual = db.standing_native_query(&cx, handle, policy()).unwrap();
                assert_eq!(actual, (at, oracle(&db)));
                assert_eq!(actual.1, db.query(&cx, text, &params, symbols, policy()).unwrap());
                assert_eq!(db.standing_native_columns(&cx, handle).unwrap(), &["p"]);
            }
            db.rebuild_standing_query(&cx, &native, policy()).unwrap();
            assert_eq!(db.standing_native_query(&cx, &native, policy()).unwrap().1, oracle(&db));
        }
        let before = db.standing_native_query(&cx, &bound, policy()).unwrap();
        let small = GqlQueryPolicy::new(0, 2, 20_000_000, 20_000_000);
        assert!(matches!(db.standing_native_query(&cx, &bound, small),
            Err(StandingQueryError::Delivery(StandingQueryFailure::ResultBudget))));
        assert_eq!(db.standing_native_query(&cx, &bound, policy()).unwrap(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ranked_products_preserve_projection_filter_distinct_and_complete_row_ties() {
    let ((), report) = run_async_under_lab(0x7769_0202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
            let query = leaf().cross_join(leaf()).unwrap().project(vec![
                GraphSetProjection::new("left_value", GraphSetValue::Column(0)),
                GraphSetProjection::new("right_value", GraphSetValue::Column(1)),
            ], quantifier).unwrap().filter(&[GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(0), comparison: IntegerComparison::Greater,
                right: GraphSetOperand::Column(1),
            }]).unwrap().with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(1, Some(4));
            let handle = db.register_standing_relation(&cx, &query, policy()).unwrap();
            let mut expected = Vec::new();
            for a in [9, 7, 7, 2] { for b in [9, 7, 7, 2] {
                if a > b { expected.push((a, b)); }
            }}
            // ORDER BY left DESC; the complete tuple breaks equal-left ties.
            expected.sort_by(|a, b| b.0.cmp(&a.0).then(a.cmp(b)));
            if quantifier == GraphSetQuantifier::Distinct { expected.dedup(); }
            let rows = expected.into_iter().skip(1).take(4).map(|(a, b)| [a, b].into_iter()
                .map(|v| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(v)))).collect()).collect();
            assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap().1,
                QueryResult::Rows { columns: vec!["left_value".into(), "right_value".into()], rows });
            let snapshot = query.execute_governed(policy(), |pattern, allowance| {
                db.execute_graph_pattern_governed_at(&cx, pattern, db.frontier().unwrap(), allowance)
            }, || cx.checkpoint()).unwrap();
            let rows = snapshot.value.iter().map(|row| row.values().iter().cloned().map(QueryValue::Value).collect()).collect();
            assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap().1,
                QueryResult::Rows { columns: query.columns().to_vec(), rows });
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

type Saved = (usize, GqlQueryPolicy, CommitSeq, Option<StandingQueryFailure>, ZSet<GraphValueRow>);
fn saved(queries: &[StandingQuery]) -> Vec<Saved> {
    queries.iter().map(|query| {
        let rows = sets::rows(query).unwrap(); let (policy, at, failure) = query.status();
        (std::ptr::from_ref(rows) as usize, policy, at, failure,
            rows.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap())
    }).collect()
}

#[test]
fn every_window_circuit_stage_refusal_preserves_old_nodes_and_rebuild_rebases_the_window() {
    let ((), report) = run_async_under_lab(0x7769_0203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let sibling = db.register_standing_relation(&cx, &leaf(), policy()).unwrap();
        let before = saved(&db.standing_queries); let query = definition(); let mut calls = 0;
        { let mut staged = Staging::new(&mut db);
            staged.compile_root(&cx, &query, policy(), &mut || { calls += 1; Ok(()) }).unwrap(); }
        assert_eq!(saved(&db.standing_queries), before);
        for stop in 1..=calls {
            let mut seen = 0;
            { let mut staged = Staging::new(&mut db);
                assert!(staged.compile_root(&cx, &query, policy(), &mut || {
                    seen += 1; if seen == stop { Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted)) }
                    else { Ok(()) }
                }).is_err()); }
            assert_eq!(seen, stop); assert_eq!(saved(&db.standing_queries), before);
        }
        let handle = db.register_standing_relation(&cx, &query, policy()).unwrap();
        let first = match handle.native.as_deref().unwrap() {
            Layout::Circuit { first, .. } => *first, _ => unreachable!(),
        };
        let mut calls = 0;
        rebuild_checked(&mut db, &cx, first, handle.index, policy(), &mut || { calls += 1; Ok(()) }).unwrap();
        let accepted = saved(&db.standing_queries);
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(rebuild_checked(&mut db, &cx, first, handle.index, policy(), &mut || {
                seen += 1; if seen == stop { Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted)) }
                else { Ok(()) }
            }).is_err());
            assert_eq!(seen, stop); assert_eq!(saved(&db.standing_queries), accepted);
        }
        let mut edit = WriteBatch::new(RelationId(1)); edit.delete_vertex(VId(1));
        db.write(&commit, edit).await.unwrap();
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap().1, oracle(&db));
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn terminal_zero_windows_admit_every_child_and_do_not_erase_nested_sequence_semantics() {
    let ((), report) = run_async_under_lab(0x7769_0204, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1)); seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(0))]);
        db.write(&commit, seed).await.unwrap();
        let sibling = db.register_standing_relation(&cx, &leaf(), policy()).unwrap();
        let before = saved(&db.standing_queries);
        let divide = GraphIntegerExpression::prepare(&[GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Column(0), GraphIntegerOp::Binary(GraphIntegerBinary::Divide)]).unwrap();
        let bad = leaf().project(vec![GraphSetProjection::new("p", GraphSetValue::Integer(divide))],
            GraphSetQuantifier::All).unwrap();
        assert!(matches!(db.register_standing_relation(&cx, &ordered(bad, 0, 0), policy()),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::OutputExpression { .. }))));
        let product = || leaf().cross_join(leaf()).unwrap();
        for query in [product().with_page(0, Some(0)),
            product().with_order_by(&[GraphValueOrder::descending(0)]).unwrap(),
            ordered(leaf().nested().unwrap().with_page(1, None).nested().unwrap(), 0, 0)] {
            assert!(matches!(db.register_standing_relation(&cx, &query, policy()), Err(StandingQueryError::Unsupported)));
            assert_eq!(saved(&db.standing_queries), before);
        }
        assert!(matches!(db.register_standing_relation(&cx, &ordered(leaf(), 0, 0),
            GqlQueryPolicy::new(0, 100, 20_000_000, 20_000_000)),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget))));
        assert_eq!(saved(&db.standing_queries), before);
        let empty = db.register_standing_relation(&cx, &ordered(leaf(), 0, 0), policy()).unwrap();
        assert_eq!(db.standing_native_query(&cx, &empty,
            GqlQueryPolicy::new(0, 0, 20_000_000, 20_000_000)).unwrap().1, scalar_result([]));
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
