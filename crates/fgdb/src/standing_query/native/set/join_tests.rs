//! Complete prepared joins through public snapshot and maintained-circuit APIs.
use super::*;
use crate::{DatabaseKeys, QueryResult, QueryValue, WriteBatch, WriteTxnError};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, IntegerComparison};
use fgdb_gql::row_join::{RowJoinKind, RowJoinSpec};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphSetColumnType, GraphSetExecutionError, GraphSetOperand, GraphSetPredicateOp,
    GraphSetProjection, GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const K: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const KINDS: [RowJoinKind; 6] = [
    RowJoinKind::Inner, RowJoinKind::Left, RowJoinKind::Right,
    RowJoinKind::Full, RowJoinKind::Semi, RowJoinKind::Anti,
];
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000)
}
fn keys(tag: u8) -> DatabaseKeys {
    DatabaseKeys::new([tag; 32], DatabaseSecurityNamespaceId([tag; 32]), [tag; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "R") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(K)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn leaf(label: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(
        &format!("MATCH (n:{label}) RETURN n.k AS k, n.p AS p"), symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap().with_duplicates().into()
}
fn query(kind: RowJoinKind, keyed: bool) -> PreparedGraphSet {
    let types = [GraphSetColumnType::Scalar; 2];
    let spec = if keyed {
        RowJoinSpec::new(&types, &types, &[(0, 0)])
    } else {
        RowJoinSpec::cross(&types, &types)
    }.unwrap().with_kind(kind).with_predicate(&[GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(1), comparison: IntegerComparison::Less,
        right: GraphSetOperand::Column(3),
    }]).unwrap();
    leaf("L").join(leaf("R"), spec).unwrap()
}
fn vertex(batch: &mut WriteBatch, id: u128, label: u64, key: i64, value: i64) {
    batch.create_vertex(VId(id), vec![LabelId(label)], vec![
        (K, CanonicalScalar::Int(key)), (P, CanonicalScalar::Int(value)),
    ]);
}
fn native(at: CommitSeq, query: &PreparedGraphSet, rows: &[GraphValueRow]) -> (CommitSeq, QueryResult) {
    (at, QueryResult::Rows {
        columns: query.columns().to_vec(),
        rows: rows.iter().map(|row| row.values().iter().cloned().map(QueryValue::Value).collect()).collect(),
    })
}
fn row(values: &[Option<i64>]) -> GraphValueRow {
    GraphValueRow::from_owned_values(values.iter().map(|value| {
        GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
    }).collect())
}

#[test]
fn all_six_prepared_join_circuits_publish_same_rows_and_deltas_as_current_snapshots() {
    let ((), report) = run_async_under_lab(0x6f6e_0501, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xa1)).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, label, key, value) in [
            (1, 1, 1, 10), (2, 1, 1, 10), (3, 1, 1, 30),
            (4, 2, 1, 20), (5, 2, 2, 40),
        ] { vertex(&mut seed, id, label, key, value); }
        db.write(&commit, seed).await.unwrap();
        let mut queries = Vec::new();
        for kind in KINDS {
            for keyed in [true, false] {
                let definition = query(kind, keyed);
                let handle = db.register_standing_relation(&cx, &definition, policy()).unwrap();
                let rows = db.execute_graph_set_governed(&cx, &definition, policy()).unwrap().value;
                assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap(),
                    native(db.frontier().unwrap(), &definition, &rows));
                assert_eq!(db.standing_join_spec(&cx, &handle).unwrap(), definition.incremental_join().unwrap().2);
                queries.push((definition, handle));
            }
        }
        for tick in 0..4 {
            let before: Vec<_> = queries.iter().map(|(_, handle)| {
                db.standing_join(&cx, handle).unwrap().rows()
                    .checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap()
            }).collect();
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(35)));
                    change.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(40)));
                }
                1 => { change.set_vertex_property(VId(5), K, None); }
                2 => { change.delete_vertex(VId(2)); vertex(&mut change, 6, 2, 1, 15); }
                _ => { change.set_vertex_property(VId(3), P, None); }
            }
            let at = db.write(&commit, change).await.unwrap();
            for ((definition, handle), mut integrated) in queries.iter().zip(before) {
                let rows = db.execute_graph_set_governed(&cx, definition, policy()).unwrap().value;
                assert_eq!(db.standing_native_query(&cx, handle, policy()).unwrap(), native(at, definition, &rows));
                integrated.integrate(
                    db.standing_join_delta(&cx, handle).unwrap().unwrap().rows(),
                    LimbLimit::new(4), &mut |_| Ok::<_, ()>(()),
                ).unwrap();
                assert_eq!(&integrated, db.standing_join(&cx, handle).unwrap().rows());
            }
        }
        for (definition, handle) in queries {
            let expected = db.standing_native_query(&cx, &handle, policy()).unwrap();
            db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
            assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap(), expected);
            assert_eq!(db.standing_join_spec(&cx, &handle).unwrap(), definition.incremental_join().unwrap().2);
            assert!(db.standing_join_delta(&cx, &handle).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn live_historical_pinned_and_staged_readers_share_join_semantics_without_crossing_frontiers() {
    let ((), report) = run_async_under_lab(0x6f6e_0502, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query(); let commit = c.commit(); let txcx = c.txn();
        let mut db = Database::open_memory(&commit, keys(0xa2)).await.unwrap();
        let other = Database::open_memory(&commit, keys(0xa3)).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        vertex(&mut seed, 1, 1, 1, 10); vertex(&mut seed, 2, 1, 1, 30); vertex(&mut seed, 3, 2, 1, 20);
        let basis = db.write(&commit, seed).await.unwrap();
        let definition = query(RowJoinKind::Full, true);
        let handle = db.register_standing_relation(&cx, &definition, policy()).unwrap();
        let pinned = db.read_session().unwrap();
        let before = vec![row(&[Some(1), Some(10), Some(1), Some(20)]), row(&[Some(1), Some(30), None, None])];
        assert_eq!(db.execute_graph_set_governed(&cx, &definition, policy()).unwrap().value, before);
        let mut tx = db.begin(&txcx).unwrap();
        assert!(matches!(tx.execute_graph_set_governed(&other, &cx, &definition, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(WriteTxnError::WrongDatabase)))));
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(40)));
        tx.write(&mut db, change).unwrap();
        let staged = vec![row(&[Some(1), Some(10), Some(1), Some(40)]), row(&[Some(1), Some(30), Some(1), Some(40)])];
        assert_eq!(tx.execute_graph_set_governed(&db, &cx, &definition, policy()).unwrap().value, staged);
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap(), native(basis, &definition, &before));
        assert_eq!(pinned.execute_graph_set_governed(&cx, &definition, policy()).unwrap().value, before);
        tx.finish(&mut db, &commit).await.unwrap();
        let after = db.frontier().unwrap();
        assert!(after > basis);
        assert_eq!(db.execute_graph_set_governed(&cx, &definition, policy()).unwrap().value, staged);
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap(), native(after, &definition, &staged));
        assert_eq!(db.execute_graph_set_governed_at(&cx, &definition, basis, policy()).unwrap().value, before);
        assert_eq!(pinned.execute_graph_set_governed_at(&cx, &definition, basis, policy()).unwrap().value, before);
        assert!(pinned.execute_graph_set_governed_at(&cx, &definition, after, policy()).is_err());
        assert_eq!(txcx.outstanding_obligations(), 0);
        drop(db);
        assert_eq!(pinned.execute_graph_set_governed(&cx, &definition, policy()).unwrap().value, before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

type Saved = (GqlQueryPolicy, CommitSeq, Option<StandingQueryFailure>, ZSet<GraphValueRow>, Option<RowJoinSpec>);
fn saved(queries: &[StandingQuery]) -> Vec<Saved> {
    queries.iter().map(|query| {
        let (policy, frontier, failure) = query.status();
        let rows = sets::rows(query).unwrap().checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
        let spec = if let StandingQuery::Join(query) = query { Some(query.spec().clone()) } else { None };
        (policy, frontier, failure, rows, spec)
    }).collect()
}
fn pipeline() -> PreparedGraphSet {
    query(RowJoinKind::Full, true).filter(&[GraphSetPredicateOp::IsNull {
        operand: GraphSetOperand::Column(0), is_null: false,
    }]).unwrap().project(vec![
        GraphSetProjection::new("left_value", GraphSetValue::Column(1)),
        GraphSetProjection::new("right_value", GraphSetValue::Column(3)),
    ], GraphSetQuantifier::All).unwrap().with_order_by(&[GraphValueOrder::descending(1)]).unwrap().with_page(0, Some(2))
}

#[test]
fn join_projection_window_registration_and_rebuild_are_atomic_at_every_circuit_stage() {
    let ((), report) = run_async_under_lab(0x6f6e_0503, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xa4)).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        vertex(&mut seed, 1, 1, 1, 10); vertex(&mut seed, 2, 1, 1, 30); vertex(&mut seed, 3, 2, 1, 20);
        vertex(&mut seed, 5, 2, 1, 5); // unmatched-right row must be filtered AFTER the join
        db.write(&commit, seed).await.unwrap();
        let sibling = db.register_standing_relation(&cx, &leaf("L"), policy()).unwrap();
        let before = saved(&db.standing_queries);
        let definition = pipeline();
        let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged.compile(&cx, &definition, policy(), &mut || { calls += 1; Ok(()) }).unwrap();
        }
        assert!(calls > 0);
        assert_eq!(saved(&db.standing_queries), before);
        for stop in 1..=calls {
            let mut seen = 0;
            {
                let mut staged = Staging::new(&mut db);
                assert!(staged.compile(&cx, &definition, policy(), &mut || {
                    seen += 1;
                    if seen == stop { Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted)) } else { Ok(()) }
                }).is_err());
            }
            assert_eq!(seen, stop);
            assert_eq!(saved(&db.standing_queries), before);
        }
        let handle = db.register_standing_relation(&cx, &definition, policy()).unwrap();
        let Some(Layout::Circuit { first, .. }) = handle.native.as_deref() else { panic!() };
        let first = *first;
        let mut change = WriteBatch::new(RelationId(1));
        vertex(&mut change, 4, 2, 1, 40);
        db.write(&commit, change).await.unwrap();
        let rows = db.execute_graph_set_governed(&cx, &definition, policy()).unwrap().value;
        let expected = native(db.frontier().unwrap(), &definition, &rows);
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap(), expected);
        // Measure a completed rebuild, then cancel independently at every phase.
        let mut calls = 0;
        rebuild_checked(&mut db, &cx, first, handle.index, policy(), &mut || { calls += 1; Ok(()) }).unwrap();
        let before = saved(&db.standing_queries);
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(rebuild_checked(&mut db, &cx, first, handle.index, policy(), &mut || {
                seen += 1;
                if seen == stop { Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted)) } else { Ok(()) }
            }).is_err());
            assert_eq!(seen, stop);
            assert_eq!(saved(&db.standing_queries), before);
        }
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap(), expected);
        assert!(db.standing_native_query(&cx, &sibling, policy()).is_ok());
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(4));
        db.write(&commit, change).await.unwrap();
        let rows = db.execute_graph_set_governed(&cx, &definition, policy()).unwrap().value;
        assert_eq!(db.standing_native_query(&cx, &handle, policy()).unwrap(), native(db.frontier().unwrap(), &definition, &rows));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
