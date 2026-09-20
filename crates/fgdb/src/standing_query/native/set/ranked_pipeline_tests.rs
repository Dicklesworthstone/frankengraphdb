//! Nested selection is compared to complete snapshot execution after real writes.
use super::*;
use asupersync::lab::run_async_under_lab;
use crate::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, IntegerComparison};
use fgdb_gql::{GqlScalarParameter, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
    GraphSetOperand, GraphSetPredicateOp, GraphSetProjection, GraphSetValue,
    GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const P: PropertyKeyId = PropertyKeyId(1);
const LIMBS: LimbLimit = LimbLimit::new(4);
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000) }
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x81; 32], DatabaseSecurityNamespaceId([0x82; 32]), [0x83; 32]) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) { (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)), _ => None }
}
fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap().into()
}
fn ranked(input: PreparedGraphSet, count: u64) -> PreparedGraphSet {
    input.with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(0, Some(count))
}
fn below_nine(input: PreparedGraphSet) -> PreparedGraphSet {
    input.filter(&[GraphSetPredicateOp::Compare { left: GraphSetOperand::Column(0),
        comparison: IntegerComparison::Less,
        right: GraphSetOperand::Literal(GqlScalarParameter::new(CanonicalScalar::Int(9)).unwrap()),
    }]).unwrap()
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value) in [(1, 9), (2, 7), (3, 7), (4, 5), (5, 2)] {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    batch
}
fn expected(values: &[i64]) -> QueryResult {
    QueryResult::Rows { columns: vec!["p".into()], rows: values.iter()
        .map(|value| vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(*value)))]).collect() }
}
fn snapshot<V: Vfs + Clone>(db: &Database<V>, cx: &QueryCx, query: &PreparedGraphSet) -> QueryResult {
    let at = db.frontier().unwrap();
    let result = query.execute_governed(policy(), |pattern, allowance| {
        db.execute_graph_pattern_governed_at(cx, pattern, at, allowance)
    }, || cx.checkpoint()).unwrap();
    QueryResult::Rows { columns: query.columns().to_vec(), rows: result.value.iter()
        .map(|row| row.values().iter().cloned().map(QueryValue::Value).collect()).collect() }
}

#[test]
fn native_with_pipelines_keep_nested_pages_distinct_and_parameters_through_commits() {
    let ((), report) = run_async_under_lab(0x7769_0301, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT $top WHERE p < $ceiling RETURN p ORDER BY p ASC";
        let original = GqlParameters::new().with_uint64("top", 4).unwrap().with_int64("ceiling", 9).unwrap();
        let changed = GqlParameters::new().with_uint64("top", 2).unwrap().with_int64("ceiling", 9).unwrap();
        let mut cases = vec![(text, original), (text, changed)];
        for text in [
            "MATCH (n) WITH DISTINCT n.p AS p ORDER BY p DESC LIMIT 3 RETURN p ORDER BY p DESC",
            "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT 3 WITH DISTINCT p RETURN p ORDER BY p DESC",
            "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT 4 WHERE p < 9 WITH p ORDER BY p ASC SKIP 1 LIMIT 2 RETURN p ORDER BY p DESC",
            "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT 4 UNWIND [p,p] AS x RETURN x ORDER BY x ASC LIMIT 5",
        ] { cases.push((text, GqlParameters::new())); }
        let handles: Vec<_> = cases.iter().map(|(text, params)| {
            db.register_standing_native(&cx, text, params, symbols, policy()).unwrap()
        }).collect();
        assert_eq!(db.standing_native_query(&cx, &handles[0], policy()).unwrap().1, expected(&[5, 7, 7]));
        assert_eq!(db.standing_native_query(&cx, &handles[1], policy()).unwrap().1, expected(&[7]));
        assert_eq!(db.standing_native_query(&cx, &handles[2], policy()).unwrap().1, expected(&[9, 7, 5]));
        assert_eq!(db.standing_native_query(&cx, &handles[3], policy()).unwrap().1, expected(&[9, 7]));
        for tick in 0..6 {
            for (handle, (text, params)) in handles.iter().zip(&cases) {
                assert_eq!(db.standing_native_query(&cx, handle, policy()).unwrap(),
                    (db.frontier().unwrap(), db.query(&cx, text, params, symbols, policy()).unwrap()));
            }
            let mut edit = WriteBatch::new(RelationId(1));
            match tick {
                0 => { edit.delete_vertex(VId(1)); }
                1 => { edit.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(1))); }
                2 => { edit.create_vertex(VId(u128::MAX), vec![], vec![]); }
                3 => { edit.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(11))); }
                4 => { edit.set_vertex_property(VId(u128::MAX), P, Some(CanonicalScalar::Int(8))); }
                _ => { edit.delete_vertex(VId(4)); }
            }
            db.write(&commit, edit).await.unwrap();
            if tick == 2 { for handle in &handles { db.rebuild_standing_query(&cx, handle, policy()).unwrap(); } }
        }
        for (handle, (text, params)) in handles.iter().zip(&cases) {
            assert_eq!(db.standing_native_query(&cx, handle, policy()).unwrap().1,
                db.query(&cx, text, params, symbols, policy()).unwrap());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filters_scopes_and_bare_pages_preserve_inherited_rank_but_projection_resets_it() {
    let ((), report) = run_async_under_lab(0x7769_0302, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let filtered = below_nine(ranked(leaf(), 4)).nested().unwrap();
        let combined = ranked(leaf(), 2).combine(GraphSetOperation::Union,
            GraphSetQuantifier::All, filtered.clone()).unwrap();
        let queries = [filtered.clone(), filtered.clone().with_page(1, Some(2)),
            filtered.clone().with_page(1, None),
            filtered.clone().with_order_by(&[GraphValueOrder::ascending(0)]).unwrap(),
            filtered.clone().project(vec![GraphSetProjection::new("p", GraphSetValue::Column(0))],
                GraphSetQuantifier::All).unwrap(),
            combined.clone(), combined.with_order_by(&[GraphValueOrder::descending(0)]).unwrap()];
        let handles: Vec<_> = queries.iter().map(|query| db.register_standing_relation(&cx, query, policy()).unwrap()).collect();
        for (at, values) in [&[7, 7, 5][..], &[7, 5], &[7, 5], &[5, 7, 7], &[5, 7, 7],
            &[5, 7, 7, 7, 9], &[9, 7, 7, 7, 5]]
            .into_iter().enumerate() {
            assert_eq!(db.standing_native_query(&cx, &handles[at], policy()).unwrap().1, expected(values));
        }
        // The transparent filtered root has a maintained rank sink, not a
        // per-read re-sort or an expanded Vec<Arc<Row>> presentation cache.
        assert!(matches!(&db.standing_queries[handles[0].index], StandingQuery::Window(_)));
        let mut integrated = db.standing_window(&cx, &handles[0]).unwrap().rows()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        let mut edit = WriteBatch::new(RelationId(1)); edit.delete_vertex(VId(2));
        db.write(&commit, edit).await.unwrap();
        integrated.integrate(db.standing_window_delta(&cx, &handles[0]).unwrap().unwrap().rows(),
            LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(&integrated, db.standing_window(&cx, &handles[0]).unwrap().rows());
        for (handle, query) in handles.iter().zip(&queries) {
            assert_eq!(db.standing_native_query(&cx, handle, policy()).unwrap().1, snapshot(&db, &cx, query));
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert_eq!(db.standing_native_query(&cx, handle, policy()).unwrap().1, snapshot(&db, &cx, query));
        }
        let accepted = db.standing_native_query(&cx, &handles[0], policy()).unwrap();
        assert!(matches!(db.standing_native_query(&cx, &handles[0],
            GqlQueryPolicy::new(0, 1, 20_000_000, 20_000_000)),
            Err(StandingQueryError::Delivery(StandingQueryFailure::ResultBudget))));
        assert_eq!(db.standing_native_query(&cx, &handles[0], policy()).unwrap(), accepted);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

type Saved = (usize, GqlQueryPolicy, CommitSeq, Option<StandingQueryFailure>, ZSet<GraphValueRow>);
fn saved(queries: &[StandingQuery]) -> Vec<Saved> {
    queries.iter().map(|query| {
        let rows = sets::rows(query).unwrap(); let (policy, at, failure) = query.status();
        (std::ptr::from_ref(rows) as usize, policy, at, failure,
            rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap())
    }).collect()
}
fn nested() -> PreparedGraphSet {
    let input = ranked(leaf(), 4).combine(GraphSetOperation::Union, GraphSetQuantifier::All,
        ranked(leaf(), 2)).unwrap();
    below_nine(ranked(input, 5)).nested().unwrap()
}

#[test]
fn every_nested_registration_and_rebuild_boundary_keeps_the_old_circuit_intact() {
    let ((), report) = run_async_under_lab(0x7769_0303, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let sibling = db.register_standing_relation(&cx, &leaf(), policy()).unwrap();
        let before = saved(&db.standing_queries); let query = nested(); let mut calls = 0;
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
        let first = match handle.native.as_deref().unwrap() { Layout::Circuit { first, .. } => *first, _ => unreachable!() };
        let size = db.standing_queries.len(); let mut calls = 0;
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
        assert_eq!(db.standing_queries.len(), size);
        let mut edit = WriteBatch::new(RelationId(1)); edit.delete_vertex(VId(1));
        db.write(&commit, edit).await.unwrap();
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap().1, snapshot(&db, &cx, &query));
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rank_before_expression_excludes_off_page_errors_then_fences_and_repairs_when_they_enter() {
    let ((), report) = run_async_under_lab(0x7769_0304, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, 4), (2, 2), (3, 0)] {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
        }
        db.write(&commit, seed).await.unwrap();
        let divide = GraphIntegerExpression::prepare(&[GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Column(0), GraphIntegerOp::Binary(GraphIntegerBinary::Divide)]).unwrap();
        let query = ranked(leaf(), 2).project(vec![GraphSetProjection::new("p", GraphSetValue::Integer(divide))],
            GraphSetQuantifier::All).unwrap().with_order_by(&[GraphValueOrder::descending(0)]).unwrap();
        let handle = db.register_standing_relation(&cx, &query, policy()).unwrap();
        let dependent = db.register_standing_set(&cx, &handle, &handle, SetOperation::UnionAll, policy()).unwrap();
        let sibling = db.register_standing_relation(&cx, &leaf(), policy()).unwrap();
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap().1, expected(&[6, 3]));
        let mut edit = WriteBatch::new(RelationId(1)); edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(-1)));
        let at = db.write(&commit, edit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(matches!(db.standing_native_query(&cx, &handle, policy()),
            Err(StandingQueryError::Unavailable { reason: StandingQueryFailure::DependencyUnavailable, .. })));
        assert!(matches!(db.standing_set(&cx, &dependent),
            Err(StandingQueryError::Unavailable { reason: StandingQueryFailure::DependencyUnavailable, .. })));
        assert!(db.standing_queries.iter().any(|query| matches!(query.status().2,
            Some(StandingQueryFailure::OutputExpression { .. }))));
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
        let before = saved(&db.standing_queries);
        assert!(db.rebuild_standing_query(&cx, &handle, policy()).is_err());
        assert_eq!(saved(&db.standing_queries), before);
        let mut fix = WriteBatch::new(RelationId(1)); fix.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(1)));
        db.write(&commit, fix).await.unwrap();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert!(db.standing_window_delta(&cx, &handle).unwrap().is_none());
        db.rebuild_standing_query(&cx, &dependent, policy()).unwrap();
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap().1, snapshot(&db, &cx, &query));
        let mut next = WriteBatch::new(RelationId(1)); next.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(6)));
        db.write(&commit, next).await.unwrap();
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap().1, snapshot(&db, &cx, &query));
        assert_eq!(db.standing_set_total(&cx, &dependent).unwrap().to_i128(), Some(4));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_ranked_pages_do_not_hide_bad_descendants_or_unknown_enumeration() {
    let ((), report) = run_async_under_lab(0x7769_0305, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = saved(&db.standing_queries);
        let expanded = ranked(leaf(), 2).unwind("x".into(), GraphSetValue::List(vec![
            GraphSetValue::Column(0), GraphSetValue::Column(0),
        ])).unwrap().with_page(0, Some(1));
        let unknown = ranked(expanded.nested().unwrap(), 0);
        assert!(matches!(db.register_standing_relation(&cx, &unknown, policy()), Err(StandingQueryError::Unsupported)));
        assert_eq!(saved(&db.standing_queries), before);
        let mut zero = WriteBatch::new(RelationId(1));
        zero.create_vertex(VId(6), vec![], vec![(P, CanonicalScalar::Int(0))]);
        db.write(&commit, zero).await.unwrap();
        let divide = GraphIntegerExpression::prepare(&[GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Column(0), GraphIntegerOp::Binary(GraphIntegerBinary::Divide)]).unwrap();
        let bad = ranked(leaf(), 6).project(vec![GraphSetProjection::new("p", GraphSetValue::Integer(divide))],
            GraphSetQuantifier::All).unwrap();
        assert!(matches!(db.register_standing_relation(&cx, &ranked(bad, 0), policy()),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::OutputExpression { .. }))));
        assert_eq!(saved(&db.standing_queries), before);
        let query = below_nine(ranked(leaf(), 3)).with_page(0, Some(0));
        assert!(matches!(db.register_standing_relation(&cx, &query,
            GqlQueryPolicy::new(0, 100, 20_000_000, 20_000_000)),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget))));
        assert_eq!(saved(&db.standing_queries), before);
        let handle = db.register_standing_relation(&cx, &query, policy()).unwrap();
        assert_eq!(db.standing_native_query(&cx, &handle,
            GqlQueryPolicy::new(0, 0, 20_000_000, 20_000_000)).unwrap().1, expected(&[]));
        let mut edit = WriteBatch::new(RelationId(1)); edit.delete_vertex(VId(1));
        db.write(&commit, edit).await.unwrap();
        assert!(db.standing_window_delta(&cx, &handle).unwrap().unwrap().rows().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
