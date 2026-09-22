//! Regression at the public database boundary, not an alternate graph model.
use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::IntegerComparison;
use fgdb_gql::{
    GqlParameters, GraphSetOperand, GraphSetPredicateOp, GraphSymbol, GraphSymbolKind,
    PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::collections::BTreeMap;

const K: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const LIMBS: LimbLimit = LimbLimit::new(4);
type Bag = BTreeMap<GraphValueRow, i128>;

fn policy() -> GqlQueryPolicy {
    // 64-by-64 raw product exceeds this result allowance, selected rows do not.
    GqlQueryPolicy::new(10_000, 256, 10_000_000, 10_000_000)
}
fn keys(tag: u8) -> DatabaseKeys {
    DatabaseKeys::new([tag; 32], DatabaseSecurityNamespaceId([tag; 32]), [tag; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "R") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Label, "M") => Some(GraphSymbol::Label(LabelId(3))),
        (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(K)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn leaf(label: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(&format!("MATCH (n:{label}) RETURN n.k AS k, n.p AS p"), symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap().with_duplicates().into()
}
fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a), comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(b),
    }
}
fn seed(count: u128) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for at in 0..count {
        for (id, label, value) in [(at + 1, 1, at as i64), (at + 10_001, 2, at as i64 + 1000)] {
            batch.create_vertex(VId(id), vec![LabelId(label)], vec![
                (K, CanonicalScalar::Int(at as i64)), (P, CanonicalScalar::Int(value)),
            ]);
        }
    }
    batch
}
fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter().map(|(row, count)| (row.clone(), count.to_i128().unwrap())).collect()
}
fn check<V: Vfs + Clone>(
    db: &Database<V>, cx: &QueryCx, query: &PreparedGraphSet, handle: &StandingQueryHandle,
) -> Bag {
    let snapshot = db.execute_graph_set_governed(cx, query, policy()).unwrap().value;
    let mut expected = Bag::new();
    for row in snapshot { *expected.entry(row).or_default() += 1; }
    // The public read does the owner/health/frontier/delivery admission first.
    assert!(db.standing_native_query(cx, handle, policy()).is_ok());
    let retained = &db.standing_queries[handle.index];
    assert_eq!(retained.status().1, db.frontier().unwrap());
    assert_eq!(sets::columns(retained).unwrap(), query.columns());
    assert_eq!(plain(sets::rows(retained).unwrap()), expected);
    expected
}

#[test]
fn selective_circuit_avoids_cartesian_admission_and_keeps_exact_committed_deltas() {
    let ((), report) = run_async_under_lab(0x6f6e_0601, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xb1)).await.unwrap();
        db.write(&commit, seed(64)).await.unwrap();
        let product = leaf("L").cross_join(leaf("R")).unwrap();
        let first = db.standing_queries.len();
        assert!(matches!(db.register_standing_relation(&cx, &product, policy()),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::ResultBudget))));
        assert_eq!(db.standing_queries.len(), first);
        let query = product.filter(&[equal(0, 2)]).unwrap();
        let frozen = query.canonical_bytes();
        let handle = db.register_standing_relation(&cx, &query, policy()).unwrap();
        assert_eq!(db.standing_queries.len(), first + 4);
        let StandingQuery::Join(join) = &db.standing_queries[first + 2] else { panic!("missing selected join") };
        assert_eq!(join.spec().keys(), &[(0, 0)]);
        assert!(join.spec().predicate().is_some());
        assert_eq!(check(&db, &cx, &query, &handle).values().sum::<i128>(), 64);
        let mut before = sets::rows(&db.standing_queries[handle.index]).unwrap()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), K, Some(CanonicalScalar::Int(1)));
        change.set_vertex_property(VId(10_001), K, Some(CanonicalScalar::Int(1)));
        db.write(&commit, change).await.unwrap();
        // Key zero disappears, while key one now has two-by-two witnesses.
        assert_eq!(check(&db, &cx, &query, &handle).values().sum::<i128>(), 66);
        before.integrate(sets::delta(&db.standing_queries[handle.index]).unwrap(),
            LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(&before, sets::rows(&db.standing_queries[handle.index]).unwrap());
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(10_001));
        db.write(&commit, change).await.unwrap();
        assert_eq!(check(&db, &cx, &query, &handle).values().sum::<i128>(), 64);
        let expected = plain(sets::rows(&db.standing_queries[handle.index]).unwrap());
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(check(&db, &cx, &query, &handle), expected);
        assert!(sets::delta(&db.standing_queries[handle.index]).is_none());
        let StandingQuery::Join(join) = &db.standing_queries[first + 2] else { panic!() };
        assert_eq!(join.spec().keys(), &[(0, 0)]);
        assert!(join.spec().predicate().is_some());
        assert_eq!(query.canonical_bytes(), frozen);
        // An unrelated durable tick must remain a real empty successor delta.
        let mut unrelated = WriteBatch::new(RelationId(1));
        unrelated.create_vertex(VId(99_999), vec![], vec![]);
        db.write(&commit, unrelated).await.unwrap();
        assert_eq!(check(&db, &cx, &query, &handle), expected);
        assert!(sets::delta(&db.standing_queries[handle.index]).unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn false_on_discards_pairs_but_empty_left_cannot_hide_unsupported_right_input() {
    let ((), report) = run_async_under_lab(0x6f6e_0602, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xb2)).await.unwrap();
        db.write(&commit, seed(64)).await.unwrap();
        let query = leaf("L").cross_join(leaf("R")).unwrap()
            .filter(&[GraphSetPredicateOp::Truth(Some(false))]).unwrap();
        let sibling = db.register_standing_relation(&cx, &query, policy()).unwrap();
        assert!(check(&db, &cx, &query, &sibling).is_empty());
        let before = db.standing_queries.len();
        // The right operand's unbounded offset has no admitted derivative.
        let invalid = leaf("M").cross_join(leaf("R").with_page(1, None)).unwrap()
            .filter(&[GraphSetPredicateOp::Truth(Some(false))]).unwrap();
        assert!(matches!(db.register_standing_relation(&cx, &invalid, policy()),
            Err(StandingQueryError::Unsupported)));
        assert_eq!(db.standing_queries.len(), before);
        assert!(check(&db, &cx, &query, &sibling).is_empty());
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Bool(true)));
        db.write(&commit, change).await.unwrap();
        assert!(check(&db, &cx, &query, &sibling).is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn projected_query() -> PreparedGraphSet {
    use fgdb_gql::{GraphSetProjection, GraphSetValue};
    leaf("L").cross_join(leaf("R")).unwrap().project(vec![
        GraphSetProjection::new("right_value", GraphSetValue::Column(3)),
        GraphSetProjection::new("left_key", GraphSetValue::Column(0)),
        GraphSetProjection::new("left_value", GraphSetValue::Column(1)),
        GraphSetProjection::new("right_key", GraphSetValue::Column(2)),
        GraphSetProjection::new("again", GraphSetValue::Column(1)),
    ], GraphSetQuantifier::All).unwrap().filter(&[equal(1, 3), GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(2), comparison: IntegerComparison::Less,
        right: GraphSetOperand::Column(0),
    }, GraphSetPredicateOp::And]).unwrap()
}

#[test]
fn reordered_and_repeated_with_columns_survive_commits_and_whole_circuit_rebuild() {
    let ((), report) = run_async_under_lab(0x6f6e_0603, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xb3)).await.unwrap();
        db.write(&commit, seed(64)).await.unwrap();
        let query = projected_query();
        let first = db.standing_queries.len();
        let handle = db.register_standing_relation(&cx, &query, policy()).unwrap();
        assert_eq!(db.standing_queries.len(), first + 4);
        assert_eq!(query.columns(), &["right_value", "left_key", "left_value", "right_key", "again"]);
        assert_eq!(check(&db, &cx, &query, &handle).values().sum::<i128>(), 64);
        let StandingQuery::Join(join) = &db.standing_queries[first + 2] else { panic!() };
        assert_eq!(join.spec().keys(), &[(0, 0)]);
        let frozen_spec = join.spec().clone();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(10_008), P, Some(CanonicalScalar::Int(-1)));
        db.write(&commit, change).await.unwrap();
        assert_eq!(check(&db, &cx, &query, &handle).values().sum::<i128>(), 63);
        let expected = plain(sets::rows(&db.standing_queries[handle.index]).unwrap());
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(check(&db, &cx, &query, &handle), expected);
        let StandingQuery::Join(join) = &db.standing_queries[first + 2] else { panic!() };
        assert_eq!(join.spec(), &frozen_spec);
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(10_008), P, Some(CanonicalScalar::Int(1007)));
        db.write(&commit, change).await.unwrap();
        assert_eq!(check(&db, &cx, &query, &handle).values().sum::<i128>(), 64);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_unwind_match_with_where_uses_selected_maintenance_without_new_syntax() {
    use fgdb_gql::PreparedGraphSetText;
    use fgdb_gql::algebra::GraphValue;
    let value_row = |cells: &[i64]| GraphValueRow::from_owned_values(cells.iter()
        .map(|&cell| GraphValue::Scalar(CanonicalScalar::Int(cell))).collect());
    let ((), report) = run_async_under_lab(0x6f6e_0604, move |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xb4)).await.unwrap();
        db.write(&commit, seed(200)).await.unwrap();
        // A raw product has 600 occurrences and fails the 256-row policy;
        // both the 200-row graph child and selected three-row bag fit it.
        let query = PreparedGraphSetText::prepare(
            "UNWIND [2, 1, 2] AS wanted MATCH (n:L) WITH n.k AS actual, wanted, n.p AS payload WHERE wanted = actual RETURN actual, wanted, payload",
            symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let handle = db.register_standing_relation(&cx, &query, policy()).unwrap();
        let expected = Bag::from([(value_row(&[1, 1, 1]), 1), (value_row(&[2, 2, 2]), 2)]);
        assert_eq!(check(&db, &cx, &query, &handle), expected);
        let joins: Vec<_> = db.standing_queries.iter().filter_map(|state| {
            if let StandingQuery::Join(join) = state { Some(join) } else { None }
        }).collect();
        assert_eq!(joins.len(), 1);
        assert!(joins[0].spec().predicate().is_some());
        // The UNWIND domain remains Any; it is not silently narrowed to Scalar.
        assert_eq!(joins[0].spec().left_types(), &[fgdb_gql::GraphSetColumnType::Any]);
        assert!(joins[0].spec().keys().is_empty());
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(3), K, Some(CanonicalScalar::Int(777)));
        db.write(&commit, change).await.unwrap();
        assert_eq!(check(&db, &cx, &query, &handle), Bag::from([(value_row(&[1, 1, 1]), 1)]));
        let mut change = WriteBatch::new(RelationId(1));
        change.create_vertex(VId(500_000), vec![LabelId(1)], vec![
            (K, CanonicalScalar::Int(2)), (P, CanonicalScalar::Int(99)),
        ]);
        db.write(&commit, change).await.unwrap();
        let expected = Bag::from([(value_row(&[1, 1, 1]), 1), (value_row(&[2, 2, 99]), 2)]);
        assert_eq!(check(&db, &cx, &query, &handle), expected);
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(check(&db, &cx, &query, &handle), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

type SavedState = (GqlQueryPolicy, CommitSeq, Option<StandingQueryFailure>, Bag, Vec<usize>);
fn saved(states: &[StandingQuery]) -> Vec<SavedState> {
    states.iter().map(|state| {
        let (policy, frontier, failure) = state.status();
        let inputs = match state {
            StandingQuery::Join(join) => join.inputs.to_vec(),
            StandingQuery::Projection(projection) => vec![projection.input],
            StandingQuery::Window(window) => vec![window.input],
            _ => Vec::new(),
        };
        (policy, frontier, failure, plain(sets::rows(state).unwrap()), inputs)
    }).collect()
}

#[test]
fn every_selected_circuit_registration_and_rebuild_checkpoint_preserves_previous_states() {
    let ((), report) = run_async_under_lab(0x6f6e_0605, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query(); let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xb5)).await.unwrap();
        db.write(&commit, seed(8)).await.unwrap();
        let sibling = db.register_standing_relation(&cx, &leaf("R"), policy()).unwrap();
        let query = projected_query();
        let before = saved(&db.standing_queries);
        let mut total = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged.compile_root(&cx, &query, policy(), &mut || {
                total += 1; Ok(())
            }).unwrap();
            // No accepted handle; Drop must discard the complete private suffix.
        }
        assert_eq!(saved(&db.standing_queries), before);
        // register_checked has one final checkpoint after compile_root.
        for stop in 1..=total + 1 {
            let mut seen = 0;
            assert!(matches!(register_checked(&mut db, &cx, &query, policy(), &mut || {
                seen += 1;
                if seen == stop {
                    Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted))
                } else { Ok(()) }
            }), Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted))));
            assert_eq!(seen, stop);
            assert_eq!(saved(&db.standing_queries), before);
        }
        let first = db.standing_queries.len();
        let handle = db.register_standing_relation(&cx, &query, policy()).unwrap();
        let expected = check(&db, &cx, &query, &handle);
        let mut total = 0;
        rebuild_checked(&mut db, &cx, first, handle.index, policy(), &mut || {
            total += 1; Ok(())
        }).unwrap();
        let before = saved(&db.standing_queries);
        for stop in 1..=total {
            let mut seen = 0;
            assert!(matches!(rebuild_checked(&mut db, &cx, first, handle.index, policy(), &mut || {
                seen += 1;
                if seen == stop {
                    Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted))
                } else { Ok(()) }
            }), Err(StandingQueryError::Maintenance(StandingQueryFailure::Interrupted))));
            assert_eq!(seen, stop);
            assert_eq!(saved(&db.standing_queries), before);
        }
        assert_eq!(check(&db, &cx, &query, &handle), expected);
        assert!(db.standing_native_query(&cx, &sibling, policy()).is_ok());
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(10_001));
        db.write(&commit, change).await.unwrap();
        assert_eq!(check(&db, &cx, &query, &handle).values().sum::<i128>(), 7);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
