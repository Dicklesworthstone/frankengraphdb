//! Ranked standing results must equal the original snapshot result SEQUENCE.
//! A bag-only oracle cannot detect rank-only changes, wrong DISTINCT
//! representatives, stale page boundaries, or missing deletion refills.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure,
    StandingQueryHandle, WriteBatch};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder,
    GraphValue, IntegerComparison};
use fgdb_gql::{GqlQueryPolicy, GraphAggregate, GraphAggregateColumn as Column,
    GraphAggregateFilter, GraphAggregateOrder, GraphAggregateRow, GraphAggregateTest,
    GraphAggregateValue, GraphHavingExpression, GraphHavingOp as Having,
    GraphHavingOperand as Operand, GraphIntegerBinary as Binary, GraphIntegerErrorKind,
    GraphIntegerExpression, GraphIntegerOp as Op, GraphNullPlacement,
    GraphSetProjection as Projection, GraphSetValue as Value, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const GROUP: PropertyKeyId = PropertyKeyId(1);
const SCORE: PropertyKeyId = PropertyKeyId(2);

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}
fn insert(id: u128, group: i64, score: i64) -> WriteBatch {
    let mut write = WriteBatch::new(R);
    write.create_vertex(VId(id), vec![], vec![
        (GROUP, CanonicalScalar::Int(group)), (SCORE, CanonicalScalar::Int(score)),
    ]);
    write
}
fn simple(offset: u64, count: Option<u64>) -> PreparedGraphAggregate {
    let mut source = GraphPatternBuilder::new();
    source.vertex("n").unwrap();
    let input = source.prepare_values(&[
        GraphColumn::property("group", "n", GROUP),
        GraphColumn::property("score", "n", SCORE),
    ], 0, None).unwrap().with_duplicates();
    PreparedGraphAggregate::prepare(input, &[0], &[
        GraphAggregate::count_rows("count"), GraphAggregate::sum_int("sum", 1),
        GraphAggregate::average_int("average", 1),
    ], offset, count).unwrap()
}
fn descending(query: PreparedGraphAggregate) -> PreparedGraphAggregate {
    query.with_result_clauses(&[], &[GraphAggregateOrder::descending(Column::Aggregate(1))]).unwrap()
}
fn identity_output(query: PreparedGraphAggregate) -> PreparedGraphAggregate {
    query.with_output_projection(vec![Projection::new("id", Value::Column(0))]).unwrap()
}
fn expected(db: &Database<MemVfs>, definition: &PreparedGraphAggregate) -> Vec<GraphAggregateRow> {
    let vertices = db.vertices().unwrap();
    let edges = db.edges().unwrap();
    let source: BTreeMap<_, _> = vertices.iter().map(|row| (row.vid, row)).collect();
    definition.execute_governed(
        (vertices.len() + edges.len()) as u64,
        vertices.iter().map(|row| row.vid),
        edges.iter().map(|row| (row.entry.src, row.entry.relation, row.entry.dst)),
        |vid, predicates| {
            let row = source.get(&vid).unwrap();
            Ok::<_, ()>(predicates.iter().all(|predicate| predicate.matches_borrowed(
                row.labels.iter().copied(), row.props.iter().map(|(key, value)| (*key, value)),
            )))
        },
        |vid, key| Ok::<_, ()>(source.get(&vid).unwrap().props.iter()
            .find_map(|(actual, value)| (*actual == key).then_some(value))),
        policy(100_000), || Ok::<_, ()>(()),
    ).unwrap().value
}
fn check(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle,
    definition: &PreparedGraphAggregate) -> Vec<GraphAggregateRow>
{
    let expected = expected(db, definition);
    let view = db.standing_query(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    let actual: Vec<_> = view.ordered_rows().expect("ranked result stage").cloned().collect();
    assert_eq!(actual, expected);
    let bag = ZSet::from_updates(expected.into_iter().map(|row| (row, ZWeight::ONE)),
        LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
    assert_eq!(view.rows(), &bag);
    actual
}
fn ids(rows: &[GraphAggregateRow]) -> Vec<i64> {
    rows.iter().map(|row| match row.get(0).unwrap().as_value().unwrap().as_scalar().unwrap() {
        CanonicalScalar::Int(value) => *value,
        _ => panic!("identity output"),
    }).collect()
}

#[derive(Clone, Copy)]
enum Shape { Vertex, Edge, MultiHop, Optional, Exists, NotExists }

fn matrix_definition(shape: Shape, direction: GlaDirection, grouped: bool, distinct: bool) -> PreparedGraphAggregate {
    let mut root = GraphPatternBuilder::new();
    root.vertex("a").unwrap();
    let endpoint = match shape {
        Shape::Vertex | Shape::Exists | Shape::NotExists => "a",
        Shape::MultiHop => "c",
        _ => "b",
    };
    let columns = [GraphColumn::property("group", "a", GROUP),
        GraphColumn::property("score", endpoint, SCORE)];
    let input = match shape {
        Shape::Vertex => root.prepare_values(&columns, 0, None).unwrap(),
        Shape::Edge | Shape::MultiHop => {
            root.vertex("b").unwrap().edge("a", R, direction, "b").unwrap();
            if matches!(shape, Shape::MultiHop) {
                root.vertex("c").unwrap().edge("b", S, direction, "c").unwrap();
            }
            root.prepare_values(&columns, 0, None).unwrap()
        }
        Shape::Optional | Shape::Exists | Shape::NotExists => {
            let mut child = GraphPatternBuilder::new();
            child.vertex("a").unwrap().vertex("b").unwrap().edge("a", R, direction, "b").unwrap();
            let clause = match shape {
                Shape::Optional => GraphMatchClause::optional(&child),
                Shape::Exists => GraphMatchClause::exists(&child),
                _ => GraphMatchClause::not_exists(&child),
            };
            root.prepare_values_with_clauses(&[clause], &columns, 0, None).unwrap()
        }
    }.with_duplicates();
    let doubled = GraphIntegerExpression::prepare(&[
        Op::Column(1), Op::Literal(Some(2)), Op::Binary(Binary::Multiply),
    ]).unwrap();
    let base = PreparedGraphAggregate::prepare_projected(input, vec![
        Projection::new("score", Value::Integer(doubled)),
        Projection::new("group", Value::Column(0)),
    ], if grouped { &[1] } else { &[] }, &[
        GraphAggregate::count_rows("count"), GraphAggregate::sum_int("sum", 0),
        GraphAggregate::average_int("average", 0), GraphAggregate::count_distinct("distinct_scores", 0),
    ], u64::from(grouped), Some(2)).unwrap();
    let base = base.with_result_clauses(&[], &[
        GraphAggregateOrder { column: Column::Aggregate(2), descending: true,
            nulls: if direction == GlaDirection::Reverse { GraphNullPlacement::First } else { GraphNullPlacement::Last } },
        GraphAggregateOrder::ascending(Column::Aggregate(0)),
    ]).unwrap();
    let having = GraphHavingExpression::prepare(&[
        Having::Compare { left: Operand::Column(Column::Aggregate(0)),
            comparison: IntegerComparison::GreaterOrEqual, right: Operand::Integer(2) },
        Having::IsNull { operand: Operand::Column(Column::Aggregate(1)), is_null: true }, Having::Or,
    ]).unwrap();
    let key_width = usize::from(grouped);
    let amount = GraphIntegerExpression::prepare(&[
        Op::Column(key_width + 1), Op::Literal(Some(0)), Op::Coalesce,
    ]).unwrap();
    base.with_having_expression(&having).unwrap().with_output_projection(vec![
        Projection::new("count", Value::Column(key_width)),
        Projection::new("amount", Value::Integer(amount)),
    ]).unwrap().with_distinct_output(distinct)
}

#[test]
fn ranked_sequences_match_snapshot_across_computed_having_and_join_shapes() {
    let ((), report) = run_async_under_lab(0x6a91, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut registered = Vec::new();
        for shape in [Shape::Vertex, Shape::Edge, Shape::MultiHop, Shape::Optional, Shape::Exists, Shape::NotExists] {
            for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
                if matches!(shape, Shape::Vertex) && direction != GlaDirection::Forward { continue; }
                for grouped in [false, true] {
                    for distinct in [false, true] {
                        let definition = matrix_definition(shape, direction, grouped, distinct);
                        let handle = db.register_standing_query(&query, definition.clone(), policy(2)).unwrap();
                        check(&db, &query, &handle, &definition);
                        registered.push((handle, definition));
                    }
                }
            }
        }
        assert_eq!(registered.len(), 64);
        let mut seed = WriteBatch::new(R);
        for id in 1..=6 {
            let mut props = vec![(GROUP, CanonicalScalar::Int((id % 3) as i64))];
            if id != 4 { props.push((SCORE, CanonicalScalar::Int((id % 2) as i64))); }
            seed.create_vertex(VId(id), vec![], props);
        }
        let mut batches = vec![vec![seed]];
        let mut r = WriteBatch::new(R);
        for (eid, src, dst) in [(1,1,2), (2,1,2), (3,2,3), (4,3,3), (5,6,4)] {
            r.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        let mut s = WriteBatch::new(S);
        for (eid, src, dst) in [(11,2,3), (12,3,4), (13,3,4), (14,4,5)] {
            s.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        batches.push(vec![s, r]);
        let mut properties = WriteBatch::new(R);
        properties.set_vertex_property(VId(1), GROUP, Some(CanonicalScalar::Int(4)));
        properties.set_vertex_property(VId(2), SCORE, None);
        properties.set_vertex_property(VId(3), SCORE, Some(CanonicalScalar::Int(9)));
        properties.set_vertex_property(VId(4), SCORE, Some(CanonicalScalar::Int(7)));
        batches.push(vec![properties]);
        let mut r = WriteBatch::new(R);
        r.delete_edge(EId(1));
        r.add_edge(EId(6), VId(5), VId(1), vec![]);
        let mut s = WriteBatch::new(S);
        s.delete_edge(EId(12));
        batches.push(vec![r, s]);
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
                    assert_eq!(db.rebuild_standing_query(&query, handle, policy(2)).unwrap(), at);
                    check(&db, &query, handle, definition);
                }
            }
            if step == 1 {
                let definition = matrix_definition(Shape::MultiHop, GlaDirection::Forward, true, true);
                let handle = db.register_standing_query(&query, definition.clone(), policy(2)).unwrap();
                registered.push((handle, definition));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rank_only_changes_reorder_equal_bags_and_deletions_refill_from_outside_page() {
    let ((), report) = run_async_under_lab(0x6a92, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = insert(1, 1, 20);
        seed.extend(insert(2, 2, 10)).unwrap();
        seed.extend(insert(3, 3, 5)).unwrap();
        db.write(&commit, seed).await.unwrap();
        let definition = identity_output(descending(simple(0, Some(2))));
        let handle = db.register_standing_query(&query, definition.clone(), policy(2)).unwrap();
        let before = check(&db, &query, &handle, &definition);
        assert_eq!(ids(&before), vec![1, 2]);
        let mut swap = WriteBatch::new(R);
        swap.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(10)));
        swap.set_vertex_property(VId(2), SCORE, Some(CanonicalScalar::Int(20)));
        db.write(&commit, swap).await.unwrap();
        let after = check(&db, &query, &handle, &definition);
        assert_eq!(ids(&after), vec![2, 1]);
        assert_eq!(before.iter().collect::<std::collections::BTreeSet<_>>(),
            after.iter().collect::<std::collections::BTreeSet<_>>());
        let mut remove = WriteBatch::new(R);
        remove.delete_vertex(VId(2));
        db.write(&commit, remove).await.unwrap();
        assert_eq!(ids(&check(&db, &query, &handle, &definition)), vec![1, 3]);
        let mut demote = WriteBatch::new(R);
        demote.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(0)));
        db.write(&commit, demote).await.unwrap();
        assert_eq!(ids(&check(&db, &query, &handle, &definition)), vec![3, 1]);
        db.write(&commit, insert(4, 4, 9)).await.unwrap();
        assert_eq!(ids(&check(&db, &query, &handle, &definition)), vec![4, 3]);
        // Natural ordering with only a page, and unbounded explicit ranking,
        // both use the same complete-key tiebreak and original output window.
        for definition in [simple(1, Some(2)), descending(simple(0, None)), simple(u64::MAX, None)] {
            let handle = db.register_standing_query(&query, definition.clone(), policy(10)).unwrap();
            check(&db, &query, &handle, &definition);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_representatives_follow_hidden_rank_and_refresh_unchanged_rank_keys() {
    let ((), report) = run_async_under_lab(0x6a93, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = insert(1, 1, 2);
        seed.extend(insert(11, 1, 2)).unwrap();
        seed.extend(insert(2, 2, 9)).unwrap();
        seed.extend(insert(3, 3, 8)).unwrap();
        db.write(&commit, seed).await.unwrap();
        let expression = GraphIntegerExpression::prepare_scalar(&[
            Op::Column(0), Op::Literal(Some(1)), Op::Compare(IntegerComparison::Equal),
            Op::Column(1), Op::Literal(Some(2)), Op::Case,
        ]).unwrap();
        let definition = descending(simple(0, Some(1))).with_output_projection(vec![
            Projection::new("value", Value::Integer(expression)),
        ]).unwrap().with_distinct_output(true);
        let handle = db.register_standing_query(&query, definition.clone(), policy(1)).unwrap();
        let rows = check(&db, &query, &handle, &definition);
        assert_eq!(rows[0].values(), &[GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2)))]);
        let mut promote = WriteBatch::new(R);
        promote.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
        db.write(&commit, promote).await.unwrap();
        assert_eq!(check(&db, &query, &handle, &definition)[0].values(), &[GraphAggregateValue::Count(2)]);
        // Same hidden sum/rank, but a changed count and DISTINCT class. The
        // retained full group key MUST refresh or the next retraction is stale.
        db.write(&commit, insert(12, 1, 0)).await.unwrap();
        assert_eq!(check(&db, &query, &handle, &definition)[0].values(), &[GraphAggregateValue::Count(3)]);
        let mut remove = WriteBatch::new(R);
        remove.delete_vertex(VId(12));
        db.write(&commit, remove).await.unwrap();
        assert_eq!(check(&db, &query, &handle, &definition)[0].values(), &[GraphAggregateValue::Count(2)]);
        let mut demote = WriteBatch::new(R);
        demote.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(0)));
        db.write(&commit, demote).await.unwrap();
        assert_eq!(check(&db, &query, &handle, &definition)[0].values(),
            &[GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2)))]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_and_out_of_range_pages_do_not_hide_output_errors_and_rebuild_keeps_windows() {
    let ((), report) = run_async_under_lab(0x6a94, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, insert(1, 1, 0)).await.unwrap();
        let sibling = db.register_standing_query(&query, simple(0, None), policy(10)).unwrap();
        let mut registered = Vec::new();
        for (offset, count) in [(0, Some(0)), (u64::MAX, Some(1))] {
            let quotient = GraphIntegerExpression::prepare(&[
                Op::Literal(Some(10)), Op::Column(2), Op::Binary(Binary::Divide),
            ]).unwrap();
            let definition = simple(offset, count).with_result_clauses(&[GraphAggregateFilter {
                column: Column::Aggregate(0), test: GraphAggregateTest::Integer {
                    comparison: IntegerComparison::GreaterOrEqual, value: 2,
                },
            }], &[GraphAggregateOrder::descending(Column::Aggregate(1))]).unwrap()
                .with_output_projection(vec![Projection::new("quotient", Value::Integer(quotient))]).unwrap();
            let handle = db.register_standing_query(&query, definition.clone(), policy(0)).unwrap();
            assert!(check(&db, &query, &handle, &definition).is_empty());
            registered.push((handle, definition));
        }
        let at = db.write(&commit, insert(2, 1, 0)).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(db.standing_query(&query, &sibling).unwrap().frontier(), at);
        assert!(db.standing_query(&query, &sibling).unwrap().ordered_rows().is_none());
        for (handle, _) in &registered {
            assert!(matches!(db.standing_query(&query, handle),
                Err(StandingQueryError::Unavailable { frontier,
                    reason: StandingQueryFailure::OutputExpression { column: 0, error } })
                    if frontier == basis && error.kind == GraphIntegerErrorKind::DivisionByZero));
            assert!(matches!(db.rebuild_standing_query(&query, handle, policy(0)),
                Err(StandingQueryError::Maintenance(StandingQueryFailure::OutputExpression { .. }))));
            assert!(matches!(db.standing_query(&query, handle),
                Err(StandingQueryError::Unavailable { frontier, .. }) if frontier == basis));
        }
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(2), SCORE, Some(CanonicalScalar::Int(2)));
        let at = db.write(&commit, repair).await.unwrap();
        for (handle, definition) in &registered {
            assert_eq!(db.rebuild_standing_query(&query, handle, policy(0)).unwrap(), at);
            assert!(check(&db, &query, handle, definition).is_empty());
        }
        let mut next = WriteBatch::new(R);
        next.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(3)));
        db.write(&commit, next).await.unwrap();
        for (handle, definition) in &registered { check(&db, &query, handle, definition); }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_occurrence_quota_ignores_candidates_but_fences_real_page_growth() {
    let ((), report) = run_async_under_lab(0x6a95, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, insert(1, 1, 10)).await.unwrap();
        let definition = identity_output(descending(simple(0, Some(2))));
        let handle = db.register_standing_query(&query, definition.clone(), policy(1)).unwrap();
        let at = db.write(&commit, insert(2, 2, 9)).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis));
        assert!(matches!(db.rebuild_standing_query(&query, &handle, policy(1)),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::ResultBudget))));
        assert_eq!(db.rebuild_standing_query(&query, &handle, policy(2)).unwrap(), at);
        for id in 3..=6 { db.write(&commit, insert(id, id as i64, 0)).await.unwrap(); }
        assert_eq!(ids(&check(&db, &query, &handle, &definition)), vec![1, 2]);
        let all = descending(simple(1, Some(2))).with_key_output_columns(&[]).unwrap()
            .with_aggregate_output_prefix(0).unwrap();
        assert!(matches!(db.register_standing_query(&query, all.clone(), policy(1)),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::ResultBudget))));
        let bag = db.register_standing_query(&query, all.clone(), policy(2)).unwrap();
        assert_eq!(check(&db, &query, &bag, &all).len(), 2);
        assert_eq!(db.standing_query(&query, &bag).unwrap().rows().iter().next().unwrap().1.to_i128(), Some(2));
        let distinct = all.with_distinct_output(true);
        let unique = db.register_standing_query(&query, distinct.clone(), policy(0)).unwrap();
        assert!(check(&db, &query, &unique, &distinct).is_empty()); // one class, then OFFSET 1
        let mut remove = WriteBatch::new(R);
        remove.delete_vertex(VId(1));
        db.write(&commit, remove).await.unwrap();
        assert_eq!(ids(&check(&db, &query, &handle, &definition)), vec![2, 3]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unrelated_rank_suffixes_do_not_increase_finite_page_maintenance_visits() {
    let ((), report) = run_async_under_lab(0x6a96, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let definition = identity_output(descending(simple(0, Some(2))));
        let mut measured = Vec::new();
        for extra in [0, 500] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = insert(1, 1, 100);
            seed.extend(insert(2, 2, 90)).unwrap();
            seed.extend(insert(3, 3, 80)).unwrap();
            for id in 10..10 + extra { seed.extend(insert(id, id as i64, -(id as i64))).unwrap(); }
            db.write(&commit, seed).await.unwrap();
            let handle = db.register_standing_query(&query, definition.clone(), policy(2)).unwrap();
            let mut change = WriteBatch::new(R);
            change.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(1)));
            db.write(&commit, change).await.unwrap();
            assert_eq!(ids(&check(&db, &query, &handle, &definition)), vec![2, 3]);
            measured.push(*db.standing_query(&query, &handle).unwrap().last_maintenance());
            let mut unrelated = WriteBatch::new(R);
            unrelated.set_vertex_property(VId(1), PropertyKeyId(99), Some(CanonicalScalar::Int(1)));
            db.write(&commit, unrelated).await.unwrap();
            check(&db, &query, &handle, &definition);
            assert_eq!(db.standing_query(&query, &handle).unwrap().last_maintenance().affected_vertices, 0);
        }
        assert_eq!(measured[0], measured[1]);
        assert_eq!(measured[0].affected_vertices, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
