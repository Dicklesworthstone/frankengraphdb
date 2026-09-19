//! Final standing-query results must equal the ordinary snapshot result bag.
//! The oracle executes the original definition, including all result clauses;
//! it never reads retained complete groups, projection support or representatives.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    WriteBatch,
};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphValue, IntegerComparison,
};
use fgdb_gql::{
    GqlQueryPolicy, GraphAggregate, GraphAggregateColumn as Column, GraphAggregateFilter,
    GraphAggregateOrder, GraphAggregateRow, GraphAggregateTest, GraphAggregateValue,
    GraphHavingExpression, GraphHavingOp as Having, GraphHavingOperand as Operand,
    GraphIntegerBinary as Binary, GraphIntegerErrorKind, GraphIntegerExpression,
    GraphIntegerOp as Op, GraphSetProjection as Projection, GraphSetValue as Value,
    PreparedGraphAggregate,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const GROUP: PropertyKeyId = PropertyKeyId(1);
const AMOUNT: PropertyKeyId = PropertyKeyId(2);

fn policy() -> GqlQueryPolicy {
    bounded(100_000)
}
fn bounded(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}
fn insert(id: u128, group: i64, amount: i64) -> WriteBatch {
    let mut write = WriteBatch::new(R);
    write.create_vertex(
        VId(id),
        vec![],
        vec![
            (GROUP, CanonicalScalar::Int(group)),
            (AMOUNT, CanonicalScalar::Int(amount)),
        ],
    );
    write
}

fn simple(grouped: bool, offset: u64, count: Option<u64>) -> PreparedGraphAggregate {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder
        .prepare_values(
            &[
                GraphColumn::property("group", "n", GROUP),
                GraphColumn::property("amount", "n", AMOUNT),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    PreparedGraphAggregate::prepare(
        input,
        if grouped { &[0] } else { &[] },
        &[
            GraphAggregate::count_rows("count"),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int("average", 1),
        ],
        offset,
        count,
    )
    .unwrap()
}

fn recomputed(
    db: &Database<MemVfs>,
    definition: &PreparedGraphAggregate,
) -> ZSet<GraphAggregateRow> {
    let vertices = db.vertices().unwrap();
    let edges = db.edges().unwrap();
    let source: BTreeMap<_, _> = vertices.iter().map(|row| (row.vid, row)).collect();
    let result = definition
        .execute_governed(
            (vertices.len() + edges.len()) as u64,
            vertices.iter().map(|row| row.vid),
            edges
                .iter()
                .map(|row| (row.entry.src, row.entry.relation, row.entry.dst)),
            |vid, predicates| {
                let row = source.get(&vid).unwrap();
                Ok::<_, ()>(predicates.iter().all(|predicate| {
                    predicate.matches_borrowed(
                        row.labels.iter().copied(),
                        row.props.iter().map(|(key, value)| (*key, value)),
                    )
                }))
            },
            |vid, key| {
                Ok::<_, ()>(
                    source
                        .get(&vid)
                        .unwrap()
                        .props
                        .iter()
                        .find_map(|(actual, value)| (*actual == key).then_some(value)),
                )
            },
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    ZSet::from_updates(
        result.value.into_iter().map(|row| (row, ZWeight::ONE)),
        LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
    definition: &PreparedGraphAggregate,
) {
    let view = db.standing_query(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert_eq!(view.rows(), &recomputed(db, definition));
}

#[derive(Clone, Copy)]
enum Shape {
    Vertex,
    Edge,
    MultiHop,
    Optional,
    Exists,
    NotExists,
}

fn matrix_definition(
    shape: Shape,
    direction: GlaDirection,
    grouped: bool,
    distinct: bool,
) -> PreparedGraphAggregate {
    let mut root = GraphPatternBuilder::new();
    root.vertex("a").unwrap();
    let endpoint = match shape {
        Shape::Vertex | Shape::Exists | Shape::NotExists => "a",
        Shape::MultiHop => "c",
        _ => "b",
    };
    let columns = [
        GraphColumn::property("group", "a", GROUP),
        GraphColumn::property("amount", endpoint, AMOUNT),
        GraphColumn::vertex("endpoint", endpoint),
    ];
    let input = match shape {
        Shape::Vertex => root.prepare_values(&columns, 0, None).unwrap(),
        Shape::Edge | Shape::MultiHop => {
            root.vertex("b")
                .unwrap()
                .edge("a", R, direction, "b")
                .unwrap();
            if matches!(shape, Shape::MultiHop) {
                root.vertex("c")
                    .unwrap()
                    .edge("b", S, direction, "c")
                    .unwrap();
            }
            root.prepare_values(&columns, 0, None).unwrap()
        }
        Shape::Optional | Shape::Exists | Shape::NotExists => {
            let mut child = GraphPatternBuilder::new();
            child
                .vertex("a")
                .unwrap()
                .vertex("b")
                .unwrap()
                .edge("a", R, direction, "b")
                .unwrap();
            let clause = match shape {
                Shape::Optional => GraphMatchClause::optional(&child),
                Shape::Exists => GraphMatchClause::exists(&child),
                _ => GraphMatchClause::not_exists(&child),
            };
            root.prepare_values_with_clauses(&[clause], &columns, 0, None)
                .unwrap()
        }
    }
    .with_duplicates();
    let doubled = GraphIntegerExpression::prepare(&[
        Op::Column(1),
        Op::Literal(Some(2)),
        Op::Binary(Binary::Multiply),
    ])
    .unwrap();
    // Projected input positions deliberately differ from source positions.
    let base = PreparedGraphAggregate::prepare_projected(
        input,
        vec![
            Projection::new("endpoint", Value::Column(2)),
            Projection::new("group", Value::Column(0)),
            Projection::new("doubled", Value::Integer(doubled)),
        ],
        if grouped { &[1] } else { &[] },
        &[
            GraphAggregate::count_rows("count"),
            GraphAggregate::sum_int("sum", 2),
            GraphAggregate::average_int("average", 2),
            GraphAggregate::min("min", 2),
            GraphAggregate::max("max", 2),
            GraphAggregate::count_distinct("endpoints", 0),
        ],
        0,
        None,
    )
    .unwrap();
    let having = GraphHavingExpression::prepare(&[
        Having::Compare {
            left: Operand::Column(Column::Aggregate(0)),
            comparison: IntegerComparison::GreaterOrEqual,
            right: Operand::Integer(2),
        },
        Having::IsNull {
            operand: Operand::Column(Column::Aggregate(1)),
            is_null: true,
        },
        Having::Or,
    ])
    .unwrap();
    let base = base.with_having_expression(&having).unwrap();
    let result = if grouped {
        let amount =
            GraphIntegerExpression::prepare(&[Op::Column(2), Op::Literal(Some(0)), Op::Coalesce])
                .unwrap();
        base.with_output_projection(vec![
            Projection::new("amount", Value::Integer(amount)),
            Projection::new("average", Value::Column(3)),
            Projection::new("endpoints", Value::Column(6)),
        ])
        .unwrap()
    } else {
        base.with_key_output_columns(&[])
            .unwrap()
            .with_aggregate_output_prefix(3)
            .unwrap()
    };
    result.with_distinct_output(distinct)
}

#[test]
fn projected_outputs_match_snapshot_across_computed_inputs_having_and_join_shapes() {
    let ((), report) = run_async_under_lab(0x6a81, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut registered = Vec::new();
        for shape in [
            Shape::Vertex,
            Shape::Edge,
            Shape::MultiHop,
            Shape::Optional,
            Shape::Exists,
            Shape::NotExists,
        ] {
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                if matches!(shape, Shape::Vertex) && direction != GlaDirection::Forward {
                    continue;
                }
                for grouped in [false, true] {
                    for distinct in [false, true] {
                        let definition = matrix_definition(shape, direction, grouped, distinct);
                        let handle = db
                            .register_standing_query(&query, definition.clone(), policy())
                            .unwrap();
                        check(&db, &query, &handle, &definition);
                        registered.push((handle, definition));
                    }
                }
            }
        }
        assert_eq!(registered.len(), 64);
        let mut seed = WriteBatch::new(R);
        for (id, group, amount) in [
            (1, 1, Some(2)),
            (2, 1, Some(2)),
            (3, 2, Some(2)),
            (4, 2, None),
            (5, 3, Some(-1)),
        ] {
            let mut props = vec![(GROUP, CanonicalScalar::Int(group))];
            if let Some(amount) = amount {
                props.push((AMOUNT, CanonicalScalar::Int(amount)));
            }
            seed.create_vertex(VId(id), vec![], props);
        }
        let mut batches = vec![vec![seed]];
        let mut r = WriteBatch::new(R);
        for (eid, src, dst) in [(1, 1, 2), (2, 1, 2), (3, 2, 3), (4, 3, 3), (5, 5, 2)] {
            r.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        let mut s = WriteBatch::new(S);
        for (eid, src, dst) in [(11, 2, 3), (12, 3, 4), (13, 3, 4), (14, 4, 5)] {
            s.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        batches.push(vec![s, r]);
        let mut properties = WriteBatch::new(R);
        properties.set_vertex_property(VId(1), GROUP, Some(CanonicalScalar::Int(3)));
        properties.set_vertex_property(VId(2), AMOUNT, None);
        properties.set_vertex_property(VId(3), AMOUNT, Some(CanonicalScalar::Int(7)));
        properties.set_vertex_property(VId(4), AMOUNT, Some(CanonicalScalar::Int(5)));
        properties.set_vertex_property(VId(5), AMOUNT, Some(CanonicalScalar::Null));
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
        for id in [1, 2, 4, 5] {
            empty.delete_vertex(VId(id));
        }
        batches.push(vec![empty]);
        for (step, writes) in batches.into_iter().enumerate() {
            let at = db.write_atomic(&commit, writes).await.unwrap();
            for (handle, definition) in &registered {
                check(&db, &query, handle, definition);
                if step == 2 {
                    assert_eq!(
                        db.rebuild_standing_query(&query, handle, policy()).unwrap(),
                        at
                    );
                    check(&db, &query, handle, definition);
                }
            }
            if step == 1 {
                let definition =
                    matrix_definition(Shape::MultiHop, GlaDirection::Forward, true, true);
                let handle = db
                    .register_standing_query(&query, definition.clone(), policy())
                    .unwrap();
                check(&db, &query, &handle, &definition);
                registered.push((handle, definition));
            }
        }
        assert!(db.vertices().unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn deleting_the_first_distinct_group_replaces_the_concrete_numeric_variant() {
    let ((), report) = run_async_under_lab(0x6a82, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = insert(1, 1, 2);
        for (id, group) in [(2, 1), (3, 2), (4, 3)] {
            seed.extend(insert(id, group, 2)).unwrap();
        }
        db.write(&commit, seed).await.unwrap();
        let expression = GraphIntegerExpression::prepare_scalar(&[
            Op::Column(0),
            Op::Literal(Some(1)),
            Op::Compare(IntegerComparison::Equal),
            Op::Column(1),
            Op::Literal(Some(2)),
            Op::Case,
        ])
        .unwrap();
        let all = simple(true, 0, None)
            .with_output_projection(vec![Projection::new("value", Value::Integer(expression))])
            .unwrap();
        let distinct = all.clone().with_distinct_output(true);
        let hd = db
            .register_standing_query(&query, distinct.clone(), bounded(1))
            .unwrap();
        let ha = db
            .register_standing_query(&query, all.clone(), policy())
            .unwrap();
        check(&db, &query, &hd, &distinct);
        assert_eq!(
            db.standing_query(&query, &hd)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .0
                .values(),
            &[GraphAggregateValue::Count(2)]
        );
        let mut remove_first = WriteBatch::new(R);
        remove_first.delete_vertex(VId(1)).delete_vertex(VId(2));
        db.write(&commit, remove_first).await.unwrap();
        for (handle, definition) in [(&hd, &distinct), (&ha, &all)] {
            check(&db, &query, handle, definition);
        }
        assert_eq!(
            db.standing_query(&query, &hd)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .0
                .values(),
            &[GraphAggregateValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(2)
            ))]
        );
        let mut restore = insert(5, 1, 4);
        restore.extend(insert(6, 1, 4)).unwrap();
        db.write(&commit, restore).await.unwrap();
        check(&db, &query, &hd, &distinct);
        assert_eq!(
            db.standing_query(&query, &hd)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .0
                .values(),
            &[GraphAggregateValue::Count(2)]
        );
        let mut empty = WriteBatch::new(R);
        for id in [3, 4, 5, 6] {
            empty.delete_vertex(VId(id));
        }
        db.write(&commit, empty).await.unwrap();
        for (handle, definition) in [(&hd, &distinct), (&ha, &all)] {
            check(&db, &query, handle, definition);
            assert!(db.standing_query(&query, handle).unwrap().rows().is_empty());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projected_bag_quotas_count_occurrences_while_distinct_retains_hidden_group_support() {
    let ((), report) = run_async_under_lab(0x6a83, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=6 {
            seed.extend(insert(id, id as i64, 2)).unwrap();
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let all = simple(true, 0, None)
            .with_key_output_columns(&[])
            .unwrap()
            .with_aggregate_output_prefix(0)
            .unwrap();
        let distinct = all.clone().with_distinct_output(true);
        assert!(matches!(
            db.register_standing_query(&query, all.clone(), bounded(1)),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        let ha = db
            .register_standing_query(&query, all.clone(), bounded(6))
            .unwrap();
        let hd = db
            .register_standing_query(&query, distinct.clone(), bounded(1))
            .unwrap();
        let view = db.standing_query(&query, &ha).unwrap();
        assert_eq!(view.rows().len(), 1);
        let (row, weight) = view.rows().iter().next().unwrap();
        assert!(row.keys().is_empty() && row.values().is_empty());
        assert_eq!(weight.to_i128(), Some(6));
        let at = db.write(&commit, insert(7, 7, 2)).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 7);
        check(&db, &query, &hd, &distinct);
        assert!(matches!(db.standing_query(&query, &ha),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis));
        assert!(matches!(
            db.rebuild_standing_query(&query, &ha, bounded(6)),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert!(matches!(db.standing_query(&query, &ha),
            Err(StandingQueryError::Unavailable { frontier, .. }) if frontier == basis));
        assert_eq!(
            db.rebuild_standing_query(&query, &ha, bounded(7)).unwrap(),
            at
        );
        check(&db, &query, &ha, &all);
        // Insert-before-delete must be admitted at the final seven-row limit.
        let mut swap = insert(8, 8, 3);
        swap.delete_vertex(VId(1));
        db.write(&commit, swap).await.unwrap();
        check(&db, &query, &ha, &all);
        check(&db, &query, &hd, &distinct);
        assert_eq!(
            db.standing_query(&query, &ha)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .1
                .to_i128(),
            Some(7)
        );
        assert_eq!(
            db.standing_query(&query, &hd)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .1,
            &ZWeight::ONE
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn having_precedes_output_errors_and_rebuild_preserves_the_original_projection() {
    let ((), report) = run_async_under_lab(0x6a84, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, insert(1, 1, 0)).await.unwrap();
        let base = simple(true, 0, None);
        let sibling = db
            .register_standing_query(&query, base.clone(), policy())
            .unwrap();
        let reachability = db
            .register_standing_reachability(&query, R, policy())
            .unwrap();
        let quotient = GraphIntegerExpression::prepare(&[
            Op::Literal(Some(10)),
            Op::Column(2),
            Op::Binary(Binary::Divide),
        ])
        .unwrap();
        let definition = base
            .clone()
            .with_result_clauses(
                &[GraphAggregateFilter {
                    column: Column::Aggregate(0),
                    test: GraphAggregateTest::Integer {
                        comparison: IntegerComparison::GreaterOrEqual,
                        value: 2,
                    },
                }],
                &[],
            )
            .unwrap()
            .with_output_projection(vec![
                Projection::new("count", Value::Column(1)),
                Projection::new("quotient", Value::Integer(quotient)),
            ])
            .unwrap()
            .with_distinct_output(true);
        let handle = db
            .register_standing_query(&query, definition.clone(), policy())
            .unwrap();
        assert!(
            db.standing_query(&query, &handle)
                .unwrap()
                .rows()
                .is_empty()
        );
        let at = db.write(&commit, insert(2, 1, 0)).await.unwrap();
        assert_ne!(at, basis);
        assert_eq!(db.vertices().unwrap().len(), 2);
        check(&db, &query, &sibling, &base);
        assert_eq!(
            db.standing_reachability(&query, &reachability)
                .unwrap()
                .frontier(),
            at
        );
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier,
                reason: StandingQueryFailure::OutputExpression { column: 1, error } })
                if frontier == basis && error.kind == GraphIntegerErrorKind::DivisionByZero));
        assert!(matches!(
            db.rebuild_standing_query(&query, &handle, policy()),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::OutputExpression { column: 1, .. }
            ))
        ));
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier, .. }) if frontier == basis));
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(2), AMOUNT, Some(CanonicalScalar::Int(2)));
        let at = db.write(&commit, repair).await.unwrap();
        assert_eq!(
            db.rebuild_standing_query(&query, &handle, policy())
                .unwrap(),
            at
        );
        check(&db, &query, &handle, &definition);
        let view = db.standing_query(&query, &handle).unwrap();
        assert_eq!(
            view.rows().iter().next().unwrap().0.values(),
            &[
                GraphAggregateValue::Count(2),
                GraphAggregateValue::Integer(5)
            ]
        );
        let mut next = WriteBatch::new(R);
        next.set_vertex_property(VId(1), AMOUNT, Some(CanonicalScalar::Int(3)));
        db.write(&commit, next).await.unwrap();
        check(&db, &query, &handle, &definition);
        assert_eq!(
            db.standing_query(&query, &handle)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .0
                .values(),
            &[
                GraphAggregateValue::Count(2),
                GraphAggregateValue::Integer(2)
            ]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn list_output_uses_shared_exact_expression_rules_with_ranked_and_empty_pages() {
    let ((), report) = run_async_under_lab(0x6a85, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let list = Value::List(vec![
            Value::Column(0),
            Value::Value(GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text("kept").unwrap(),
            )),
        ]);
        let definition = simple(false, 0, None)
            .with_output_projection(vec![
                Projection::new("list", list.clone()),
                Projection::new("size", Value::Size(Box::new(list.clone()))),
                Projection::new(
                    "last",
                    Value::Index {
                        list: Box::new(list),
                        index: Box::new(Value::Value(GraphValue::Scalar(CanonicalScalar::Int(-1)))),
                    },
                ),
                Projection::new("average", Value::Column(2)),
            ])
            .unwrap()
            .with_distinct_output(true);
        let handle = db
            .register_standing_query(&query, definition.clone(), policy())
            .unwrap();
        check(&db, &query, &handle, &definition);
        db.write(&commit, insert(1, 1, 3)).await.unwrap();
        db.write(&commit, insert(2, 2, 4)).await.unwrap();
        check(&db, &query, &handle, &definition);
        let view = db.standing_query(&query, &handle).unwrap();
        let row = view.rows().iter().next().unwrap().0;
        assert_eq!(
            row.get(0).unwrap().as_value().unwrap().as_list().unwrap()[0],
            GraphValue::Scalar(CanonicalScalar::Int(2))
        );
        assert_eq!(row.get(3).unwrap().as_average().unwrap().numerator(), 7);
        assert_eq!(row.get(3).unwrap().as_average().unwrap().denominator(), 2);
        for (ranked, expected) in [
            (
                definition
                    .with_result_clauses(
                        &[],
                        &[GraphAggregateOrder::descending(Column::Aggregate(0))],
                    )
                    .unwrap(),
                1,
            ),
            (simple(false, 1, None).with_distinct_output(true), 0),
            (simple(false, 0, Some(0)).with_distinct_output(true), 0),
        ] {
            let handle = db
                .register_standing_query(&query, ranked.clone(), policy())
                .unwrap();
            check(&db, &query, &handle, &ranked);
            assert_eq!(
                db.standing_query(&query, &handle)
                    .unwrap()
                    .ordered_rows()
                    .unwrap()
                    .len(),
                expected
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unrelated_output_classes_do_not_increase_sparse_maintenance_counters() {
    let ((), report) = run_async_under_lab(0x6a86, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let definition = simple(true, 0, None)
            .with_output_projection(vec![Projection::new("amount", Value::Column(2))])
            .unwrap()
            .with_distinct_output(true);
        let mut measured = Vec::new();
        for extra in [0, 500] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = insert(1, 1, 2);
            for id in 10..10 + extra {
                seed.extend(insert(id, id as i64, 100 + id as i64)).unwrap();
            }
            db.write(&commit, seed).await.unwrap();
            let handle = db
                .register_standing_query(&query, definition.clone(), policy())
                .unwrap();
            let mut change = WriteBatch::new(R);
            change.set_vertex_property(VId(1), AMOUNT, Some(CanonicalScalar::Int(3)));
            db.write(&commit, change).await.unwrap();
            check(&db, &query, &handle, &definition);
            measured.push(
                *db.standing_query(&query, &handle)
                    .unwrap()
                    .last_maintenance(),
            );
        }
        assert_eq!(measured[0], measured[1]);
        assert_eq!(measured[0].affected_vertices, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
