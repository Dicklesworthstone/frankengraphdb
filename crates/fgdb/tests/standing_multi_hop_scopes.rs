//! Correlated multi-hop scopes through the public committed-write lifecycle.
//! Expected results execute the original GQL definition against the snapshot;
//! the oracle never reads the standing query's retained witness counts.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure,
    StandingQueryHandle, WriteBatch};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{GlaDirection, GraphBooleanExpression, GraphBooleanOp as B,
    GraphBooleanOperand as Arg, GraphColumn, GraphMatchClause, GraphPatternBuilder,
    GraphValue, IntegerComparison};
use fgdb_gql::{GqlQueryPolicy, GraphAggregate, GraphAggregateColumn as Column,
    GraphAggregateOrder, GraphAggregateRow, GraphHavingExpression, GraphHavingOp as H,
    GraphHavingOperand as A, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp as I,
    GraphSetProjection as Projection, GraphSetValue as Value, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const AMOUNT: PropertyKeyId = PropertyKeyId(1);
const GROUP: PropertyKeyId = PropertyKeyId(2);
const ROOT_GATE: PropertyKeyId = PropertyKeyId(3);
const CHILD_GATE: PropertyKeyId = PropertyKeyId(4);
const THRESHOLD: PropertyKeyId = PropertyKeyId(5);

#[derive(Clone, Copy, Debug)]
enum Mode { Optional, Exists, Anti }
#[derive(Clone, Copy, Debug)]
enum Shape { Chain, Branch, Cycle, SelfJoin }

fn policy() -> GqlQueryPolicy { bounded(100_000) }
fn bounded(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}
fn put(write: &mut WriteBatch, id: u128, amount: Option<i64>) {
    let mut props = Vec::new();
    if let Some(amount) = amount { props.push((AMOUNT, CanonicalScalar::Int(amount))); }
    props.extend([
        (GROUP, CanonicalScalar::Int((id % 3) as i64)),
        (ROOT_GATE, CanonicalScalar::Bool(true)),
        (CHILD_GATE, CanonicalScalar::Bool(true)),
        (THRESHOLD, CanonicalScalar::Int(id as i64)),
    ]);
    write.create_vertex(VId(id), vec![], props);
}
fn gate(variable: &str, key: PropertyKeyId) -> GraphBooleanExpression {
    let yes = CanonicalScalar::Bool(true);
    GraphBooleanExpression::prepare(&[B::Compare {
        left: Arg::Property { variable, key }, comparison: IntegerComparison::Equal,
        right: Arg::Literal(&yes),
    }]).unwrap()
}

fn definition(mode: Mode, shape: Shape, direction: GlaDirection, grouped: bool, decorate: bool)
    -> PreparedGraphAggregate
{
    let mut root = GraphPatternBuilder::new();
    root.vertex("a").unwrap().filter_boolean(&gate("a", ROOT_GATE)).unwrap();
    let mut child = GraphPatternBuilder::new();
    child.vertex("a").unwrap().vertex("b").unwrap().vertex("c").unwrap();
    child.edge("a", R, direction, "b").unwrap();
    child.edge(if matches!(shape, Shape::Branch) { "a" } else { "b" },
        if matches!(shape, Shape::SelfJoin) { R } else { S }, direction, "c").unwrap();
    if matches!(shape, Shape::Cycle) { child.edge("c", R, direction, "a").unwrap(); }
    child.filter_boolean(&gate("b", CHILD_GATE)).unwrap();
    child.compare_properties("a", THRESHOLD, IntegerComparison::LessOrEqual, "c", THRESHOLD).unwrap();
    let clause = match mode {
        Mode::Optional => GraphMatchClause::optional(&child),
        Mode::Exists => GraphMatchClause::exists(&child),
        Mode::Anti => GraphMatchClause::not_exists(&child),
    };
    let (middle, end) = if matches!(mode, Mode::Optional) { ("b", "c") } else { ("a", "a") };
    let input = root.prepare_values_with_clauses(&[clause], &[
        GraphColumn::vertex("root", "a"), GraphColumn::vertex("middle", middle),
        GraphColumn::vertex("endpoint", end), GraphColumn::property("amount", end, AMOUNT),
        GraphColumn::property("group", "a", GROUP),
    ], 0, None).unwrap().with_duplicates();
    if !decorate {
        return PreparedGraphAggregate::prepare(input, if grouped { &[0] } else { &[] }, &[
            GraphAggregate::count_rows("rows"), GraphAggregate::count("middles", 1),
            GraphAggregate::count("endpoints", 2), GraphAggregate::sum_int("sum", 3),
            GraphAggregate::average_int("average", 3), GraphAggregate::min("minimum", 3),
            GraphAggregate::count_distinct("distinct_endpoints", 2),
        ], 0, None).unwrap();
    }
    let doubled = GraphIntegerExpression::prepare(&[
        I::Column(3), I::Literal(Some(2)), I::Binary(GraphIntegerBinary::Multiply),
    ]).unwrap();
    let base = PreparedGraphAggregate::prepare_projected(input, vec![
        Projection::new("group", Value::Column(4)), Projection::new("amount", Value::Integer(doubled)),
        Projection::new("middle", Value::Column(1)), Projection::new("end", Value::Column(2)),
        Projection::new("root", Value::Column(0)),
    ], if grouped { &[0] } else { &[] }, &[
        GraphAggregate::count_rows("rows"), GraphAggregate::count("middles", 2),
        GraphAggregate::count("endpoints", 3), GraphAggregate::sum_int("sum", 1),
        GraphAggregate::average_int("average", 1), GraphAggregate::min("minimum", 1),
        GraphAggregate::count_distinct("distinct_endpoints", 3),
    ], u64::from(grouped), Some(2)).unwrap();
    let having = GraphHavingExpression::prepare(&[
        H::Compare { left: A::Column(Column::Aggregate(0)),
            comparison: IntegerComparison::GreaterOrEqual, right: A::Integer(2) },
        H::IsNull { operand: A::Column(Column::Aggregate(3)), is_null: true }, H::Or,
    ]).unwrap();
    let at = usize::from(grouped);
    base.with_result_clauses(&[], &[
        GraphAggregateOrder::descending(Column::Aggregate(4)),
        GraphAggregateOrder::ascending(Column::Aggregate(0)),
    ]).unwrap().with_having_expression(&having).unwrap()
        .with_output_projection(vec![
            Projection::new("rows", Value::Column(at)), Projection::new("sum", Value::Column(at + 3)),
            Projection::new("average", Value::Column(at + 4)),
            Projection::new("distinct_endpoints", Value::Column(at + 6)),
        ]).unwrap().with_distinct_output(matches!(shape, Shape::Branch | Shape::Cycle))
}

fn recomputed(db: &Database<MemVfs>, definition: &PreparedGraphAggregate) -> Vec<GraphAggregateRow> {
    let vertices = db.vertices().unwrap();
    let edges = db.edges().unwrap();
    let source: BTreeMap<_, _> = vertices.iter().map(|row| (row.vid, row)).collect();
    definition.execute_governed(
        (vertices.len() + edges.len()) as u64, vertices.iter().map(|row| row.vid),
        edges.iter().map(|row| (row.entry.src, row.entry.relation, row.entry.dst)),
        |vid, predicates| {
            let row = source.get(&vid).unwrap();
            Ok::<_, ()>(predicates.iter().all(|predicate| predicate.matches_borrowed(
                row.labels.iter().copied(), row.props.iter().map(|(key, value)| (*key, value)),
            )))
        },
        |vid, key| Ok::<_, ()>(source.get(&vid).unwrap().props.iter()
            .find_map(|(actual, value)| (*actual == key).then_some(value))),
        policy(), || Ok::<_, ()>(()),
    ).unwrap().value
}
fn check(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle, definition: &PreparedGraphAggregate) {
    let expected = recomputed(db, definition);
    let view = db.standing_query(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    if let Some(ordered) = view.ordered_rows() {
        assert_eq!(ordered.collect::<Vec<_>>(), expected.iter().collect::<Vec<_>>());
    }
    let bag = ZSet::from_updates(expected.into_iter().map(|row| (row, ZWeight::ONE)),
        LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(view.rows(), &bag);
}

#[test]
fn multi_hop_scopes_match_snapshot_sequences_across_shapes_and_result_stages() {
    let ((), report) = run_async_under_lab(0x6ab3, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut registered = Vec::new();
        for mode in [Mode::Optional, Mode::Exists, Mode::Anti] {
            for shape in [Shape::Chain, Shape::Branch, Shape::Cycle, Shape::SelfJoin] {
                for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
                    for grouped in [false, true] {
                        let definition = definition(mode, shape, direction, grouped, true);
                        let handle = db.register_standing_query(&query, definition.clone(), policy()).unwrap();
                        assert!(db.standing_query(&query, &handle).unwrap().ordered_rows().is_some());
                        check(&db, &query, &handle, &definition);
                        registered.push((handle, definition));
                    }
                }
            }
        }
        assert_eq!(registered.len(), 72);
        let mut seed = WriteBatch::new(R);
        for id in 1..=6 { put(&mut seed, id, (id != 4).then_some(id as i64)); }
        seed.set_vertex_property(VId(6), ROOT_GATE, Some(CanonicalScalar::Bool(false)));
        let mut batches = vec![vec![seed]];
        let mut r = WriteBatch::new(R);
        for (eid, src, dst) in [(1,1,2), (2,1,2), (3,2,3), (4,3,3), (5,4,2), (6,6,1), (7,3,1)] {
            r.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        let mut s = WriteBatch::new(S);
        for (eid, src, dst) in [(11,2,3), (12,2,3), (13,3,1), (14,1,4), (15,3,3), (16,5,6)] {
            s.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        batches.push(vec![s, r]);
        let mut hidden = WriteBatch::new(R);
        hidden.set_vertex_property(VId(2), CHILD_GATE, Some(CanonicalScalar::Bool(false)));
        hidden.set_vertex_property(VId(3), THRESHOLD, None);
        hidden.set_vertex_property(VId(4), ROOT_GATE, Some(CanonicalScalar::Bool(false)));
        hidden.set_vertex_property(VId(1), GROUP, Some(CanonicalScalar::Int(9)));
        hidden.set_vertex_property(VId(5), AMOUNT, Some(CanonicalScalar::Int(9)));
        batches.push(vec![hidden]);
        let mut restore = WriteBatch::new(R);
        restore.set_vertex_property(VId(2), CHILD_GATE, Some(CanonicalScalar::Bool(true)));
        restore.set_vertex_property(VId(3), THRESHOLD, Some(CanonicalScalar::Int(3)));
        restore.set_vertex_property(VId(4), ROOT_GATE, Some(CanonicalScalar::Bool(true)));
        restore.set_vertex_property(VId(6), ROOT_GATE, Some(CanonicalScalar::Bool(true)));
        restore.set_vertex_property(VId(3), CHILD_GATE, Some(CanonicalScalar::Null));
        restore.delete_edge(EId(1));
        let mut s = WriteBatch::new(S);
        s.delete_edge(EId(11));
        s.add_edge(EId(17), VId(2), VId(5), vec![]);
        batches.push(vec![restore, s]);
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(3));
        batches.push(vec![cascade]);
        let mut empty = WriteBatch::new(R);
        for id in [1, 2, 4, 5, 6] { empty.delete_vertex(VId(id)); }
        batches.push(vec![empty]);
        for (step, writes) in batches.into_iter().enumerate() {
            let at = db.write_atomic(&commit, writes).await.unwrap();
            for (handle, definition) in &registered {
                check(&db, &query, handle, definition);
                if step == 2 {
                    assert_eq!(db.rebuild_standing_query(&query, handle, policy()).unwrap(), at);
                    check(&db, &query, handle, definition);
                }
            }
            if step == 1 {
                let definition = definition(Mode::Optional, Shape::Cycle, GlaDirection::Undirected, true, true);
                let handle = db.register_standing_query(&query, definition.clone(), policy()).unwrap();
                check(&db, &query, &handle, &definition);
                registered.push((handle, definition));
            }
        }
        assert!(db.vertices().unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn whole_path_replacement_and_result_refusal_preserve_public_lifecycle() {
    let ((), report) = run_async_under_lab(0x6ab4, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut r = WriteBatch::new(R);
        for id in 1..=5 { put(&mut r, id, Some(id as i64)); }
        r.add_edge(EId(1), VId(1), VId(2), vec![]);
        let mut s = WriteBatch::new(S);
        s.add_edge(EId(11), VId(2), VId(3), vec![]);
        db.write_atomic(&commit, vec![r, s]).await.unwrap();
        let exists = definition(Mode::Exists, Shape::Chain, GlaDirection::Forward, true, false);
        let optional = definition(Mode::Optional, Shape::Chain, GlaDirection::Forward, true, false);
        let anti = definition(Mode::Anti, Shape::Chain, GlaDirection::Forward, true, false);
        let he = db.register_standing_query(&query, exists.clone(), bounded(1)).unwrap();
        let ho = db.register_standing_query(&query, optional.clone(), policy()).unwrap();
        let ha = db.register_standing_query(&query, anti.clone(), policy()).unwrap();
        let reachability = db.register_standing_reachability(&query, R, policy()).unwrap();
        let before = recomputed(&db, &exists);
        let mut r = WriteBatch::new(R);
        r.delete_edge(EId(1));
        r.add_edge(EId(2), VId(1), VId(4), vec![]);
        let mut s = WriteBatch::new(S);
        s.delete_edge(EId(11));
        s.add_edge(EId(12), VId(4), VId(5), vec![]);
        let basis = db.write_atomic(&commit, vec![s, r]).await.unwrap();
        assert_eq!(recomputed(&db, &exists), before);
        for (handle, definition) in [(&he, &exists), (&ho, &optional), (&ha, &anti)] {
            check(&db, &query, handle, definition);
        }
        let mut another = WriteBatch::new(R);
        another.add_edge(EId(3), VId(3), VId(4), vec![]);
        let at = db.write(&commit, another).await.unwrap();
        assert_eq!(recomputed(&db, &exists).len(), 2);
        assert_eq!(db.standing_reachability(&query, &reachability).unwrap().frontier(), at);
        check(&db, &query, &ho, &optional);
        check(&db, &query, &ha, &anti);
        assert!(matches!(db.standing_query(&query, &he),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget })
                if frontier == basis));
        assert!(matches!(db.rebuild_standing_query(&query, &he, bounded(1)),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::ResultBudget))));
        assert!(matches!(db.standing_query(&query, &he),
            Err(StandingQueryError::Unavailable { frontier, .. }) if frontier == basis));
        assert_eq!(db.rebuild_standing_query(&query, &he, bounded(2)).unwrap(), at);
        check(&db, &query, &he, &exists);
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(4));
        db.write(&commit, cascade).await.unwrap();
        for (handle, definition) in [(&he, &exists), (&ho, &optional), (&ha, &anti)] {
            check(&db, &query, handle, definition);
        }
        assert!(db.standing_query(&query, &he).unwrap().rows().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn self_join_occurrences_preserve_parallel_edges_and_nonnull_identity_witnesses() {
    let ((), report) = run_async_under_lab(0x6ab5, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        put(&mut seed, 1, None); // A real witness can have a NULL scalar payload.
        db.write(&commit, seed).await.unwrap();
        let mut registered = Vec::new();
        for mode in [Mode::Optional, Mode::Exists, Mode::Anti] {
            let definition = definition(mode, Shape::SelfJoin, GlaDirection::Undirected, true, false);
            let handle = db.register_standing_query(&query, definition.clone(), policy()).unwrap();
            check(&db, &query, &handle, &definition);
            registered.push((mode, handle, definition));
        }
        let mut first = WriteBatch::new(R);
        first.add_edge(EId(1), VId(1), VId(1), vec![]);
        let mut second = WriteBatch::new(R);
        second.add_edge(EId(2), VId(1), VId(1), vec![]);
        let mut remove = WriteBatch::new(R);
        remove.delete_edge(EId(1));
        let mut replace = WriteBatch::new(R);
        replace.delete_edge(EId(2));
        replace.add_edge(EId(3), VId(1), VId(1), vec![]);
        replace.add_edge(EId(4), VId(1), VId(1), vec![]);
        let mut empty = WriteBatch::new(R);
        empty.delete_edge(EId(3)).delete_edge(EId(4));
        for (write, paths) in [(first, 1_u64), (second, 4), (remove, 1), (replace, 4), (empty, 0)] {
            db.write(&commit, write).await.unwrap();
            for (mode, handle, definition) in &registered {
                check(&db, &query, handle, definition);
                let view = db.standing_query(&query, handle).unwrap();
                match mode {
                    Mode::Optional => {
                        let row = view.rows().iter().next().unwrap().0;
                        assert_eq!(row.get(0).unwrap().as_count(), Some(paths.max(1)));
                        assert_eq!(row.get(1).unwrap().as_count(), Some(paths));
                        assert_eq!(row.get(2).unwrap().as_count(), Some(paths));
                        assert_eq!(row.get(6).unwrap().as_count(), Some(u64::from(paths != 0)));
                        assert!(row.get(3).unwrap().is_null());
                    }
                    Mode::Exists => assert_eq!(view.rows().len(), usize::from(paths != 0)),
                    Mode::Anti => assert_eq!(view.rows().len(), usize::from(paths == 0)),
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_middle_property_updates_do_not_scan_unrelated_components() {
    let ((), report) = run_async_under_lab(0x6ab6, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        for mode in [Mode::Optional, Mode::Exists, Mode::Anti] {
            let definition = definition(mode, Shape::Chain, GlaDirection::Forward, true, false);
            let mut measured = Vec::new();
            for extra in [0_u128, 200] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut r = WriteBatch::new(R);
                let mut s = WriteBatch::new(S);
                for id in 1..=3 { put(&mut r, id, Some(id as i64)); }
                r.add_edge(EId(1), VId(1), VId(2), vec![]);
                s.add_edge(EId(2), VId(2), VId(3), vec![]);
                for component in 0..extra {
                    let base = 10 + 3 * component;
                    for id in base..base + 3 { put(&mut r, id, Some(id as i64)); }
                    r.add_edge(EId(base), VId(base), VId(base + 1), vec![]);
                    s.add_edge(EId(base + 1), VId(base + 1), VId(base + 2), vec![]);
                }
                db.write_atomic(&commit, vec![r, s]).await.unwrap();
                let handle = db.register_standing_query(&query, definition.clone(), policy()).unwrap();
                let mut hidden = WriteBatch::new(R);
                hidden.set_vertex_property(VId(2), CHILD_GATE, Some(CanonicalScalar::Bool(false)));
                db.write(&commit, hidden).await.unwrap();
                check(&db, &query, &handle, &definition);
                measured.push(*db.standing_query(&query, &handle).unwrap().last_maintenance());
            }
            assert_eq!(measured[0], measured[1]);
            assert_eq!(measured[0].affected_vertices, 1);
            assert_eq!(measured[0].affected_edges, 2);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_and_multiple_scopes_remain_explicitly_unsupported() {
    let ((), report) = run_async_under_lab(0x6ab7, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut outer = GraphPatternBuilder::new();
        outer.vertex("a").unwrap();
        let mut independent = GraphPatternBuilder::new();
        independent.vertex("x").unwrap().vertex("y").unwrap().vertex("z").unwrap();
        independent.edge("x", R, GlaDirection::Forward, "y").unwrap();
        independent.edge("y", S, GlaDirection::Forward, "z").unwrap();
        let mut correlated = GraphPatternBuilder::new();
        correlated.vertex("a").unwrap().vertex("b").unwrap().vertex("c").unwrap();
        correlated.edge("a", R, GlaDirection::Forward, "b").unwrap();
        correlated.edge("b", S, GlaDirection::Forward, "c").unwrap();
        for clauses in [
            vec![GraphMatchClause::optional(&independent)],
            vec![GraphMatchClause::exists(&correlated), GraphMatchClause::not_exists(&correlated)],
        ] {
            let input = outer.prepare_values_with_clauses(&clauses,
                &[GraphColumn::vertex("root", "a")], 0, None).unwrap().with_duplicates();
            let definition = PreparedGraphAggregate::prepare(input, &[0],
                &[GraphAggregate::count_rows("rows")], 0, None).unwrap();
            assert!(matches!(db.register_standing_query(&query, definition, policy()),
                Err(StandingQueryError::Unsupported)));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
