//! Computed input must run before grouping, DISTINCT arguments and HAVING.
//! Compare the public committed-delta path with ordinary snapshot evaluation,
//! preserving source-column dependencies and projected-column result schemas.

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
    GraphAggregateRow, GraphAggregateTest, GraphHavingExpression, GraphHavingOp as Having,
    GraphHavingOperand as Operand, GraphIntegerBinary, GraphIntegerErrorKind,
    GraphIntegerExpression, GraphIntegerOp as Op, GraphSetProjection, GraphSetValue,
    PreparedGraphAggregate,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const CATEGORY: PropertyKeyId = PropertyKeyId(1);
const QUANTITY: PropertyKeyId = PropertyKeyId(2);
const PRICE: PropertyKeyId = PropertyKeyId(3);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

#[derive(Clone, Copy, Debug)]
enum Shape {
    Vertex,
    Edge,
    MultiHop,
    Optional,
    Exists,
    NotExists,
}

fn definition(shape: Shape, direction: GlaDirection, grouped: bool) -> PreparedGraphAggregate {
    let mut root = GraphPatternBuilder::new();
    root.vertex("a").unwrap();
    let endpoint = match shape {
        Shape::Vertex | Shape::Exists | Shape::NotExists => "a",
        Shape::MultiHop => "c",
        _ => "b",
    };
    // Source column zero is a VERTEX, but projected column zero is a SCALAR.
    // The projected endpoint also moves from source position four to two.
    let columns = [
        GraphColumn::vertex("owner", "a"),
        GraphColumn::property("category", "a", CATEGORY),
        GraphColumn::property("quantity", endpoint, QUANTITY),
        GraphColumn::property("price", "a", PRICE),
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
            child.vertex("a").unwrap().vertex("b").unwrap();
            child.edge("a", R, direction, "b").unwrap();
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
    let cost = GraphIntegerExpression::prepare(&[
        Op::Column(2),
        Op::Literal(Some(0)),
        Op::Coalesce,
        Op::Column(3),
        Op::Literal(Some(1)),
        Op::Coalesce,
        Op::Binary(GraphIntegerBinary::Multiply),
    ])
    .unwrap();
    let category =
        GraphIntegerExpression::prepare_scalar(&[Op::ScalarColumn(1), Op::Lower]).unwrap();
    let query = PreparedGraphAggregate::prepare_projected(
        input,
        vec![
            GraphSetProjection::new("cost", GraphSetValue::Integer(cost)),
            GraphSetProjection::new("category", GraphSetValue::Integer(category)),
            GraphSetProjection::new("endpoint", GraphSetValue::Column(4)),
            GraphSetProjection::new(
                "one",
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
            ),
        ],
        if grouped { &[1] } else { &[] },
        &[
            GraphAggregate::min("minimum", 0),
            GraphAggregate::max("maximum", 0),
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::sum_int_distinct("distinct_sum", 0),
            GraphAggregate::average_int("average", 0),
            GraphAggregate::average_int_distinct("distinct_average", 0),
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull_cost", 0),
            GraphAggregate::count_distinct("costs", 0),
            GraphAggregate::count("nonnull_endpoint", 2),
            GraphAggregate::count_distinct("endpoints", 2),
            GraphAggregate::min("first_endpoint", 2),
            GraphAggregate::sum_int("constant_sum", 3),
        ],
        0,
        None,
    )
    .unwrap();
    let having = GraphHavingExpression::prepare(&[
        Having::Compare {
            left: Operand::Column(Column::Aggregate(6)),
            comparison: IntegerComparison::GreaterOrEqual,
            right: Operand::Integer(2),
        },
        Having::Compare {
            left: Operand::Column(Column::Aggregate(2)),
            comparison: IntegerComparison::Greater,
            right: Operand::Integer(3),
        },
        Having::Or,
    ])
    .unwrap();
    query.with_having_expression(&having).unwrap()
}

fn recomputed(db: &Database<MemVfs>, query: &PreparedGraphAggregate) -> ZSet<GraphAggregateRow> {
    let vertices = db.vertices().unwrap();
    let edges = db.edges().unwrap();
    let source: BTreeMap<_, _> = vertices.iter().map(|row| (row.vid, row)).collect();
    let result = query
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
    query: &PreparedGraphAggregate,
) {
    let view = db.standing_query(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert_eq!(view.rows(), &recomputed(db, query));
}

#[test]
fn computed_inputs_match_snapshot_across_all_admitted_shapes_and_orientations() {
    let ((), report) = run_async_under_lab(0x6a61, |root| async move {
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
                    let definition = definition(shape, direction, grouped);
                    let handle = db
                        .register_standing_query(&query, definition.clone(), policy())
                        .unwrap();
                    check(&db, &query, &handle, &definition);
                    registered.push((handle, definition));
                }
            }
        }
        assert_eq!(registered.len(), 32);
        let mut seed = WriteBatch::new(R);
        for (id, category, quantity, price) in [
            (1, Some("Books"), Some(2), Some(3)),
            (2, Some("BOOKS"), Some(2), Some(3)),
            (3, Some("Tools"), None, Some(1)),
            (4, Some("tools"), Some(-1), Some(2)),
            (5, None, Some(3), None),
        ] {
            let mut props = Vec::new();
            if let Some(category) = category {
                props.push((CATEGORY, text(category)));
            }
            if let Some(quantity) = quantity {
                props.push((QUANTITY, CanonicalScalar::Int(quantity)));
            }
            if let Some(price) = price {
                props.push((PRICE, CanonicalScalar::Int(price)));
            }
            seed.create_vertex(VId(id), vec![], props);
        }
        let mut batches = vec![vec![seed]];
        let mut r = WriteBatch::new(R);
        for (eid, src, dst) in [(10, 1, 2), (11, 1, 2), (12, 2, 2), (13, 2, 3), (14, 4, 5)] {
            r.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        let mut s = WriteBatch::new(S);
        for (eid, src, dst) in [(20, 2, 3), (21, 2, 4), (22, 3, 5), (23, 5, 1)] {
            s.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        batches.push(vec![s, r]);
        let mut values = WriteBatch::new(R);
        values.set_vertex_property(VId(2), QUANTITY, Some(CanonicalScalar::Int(4)));
        values.set_vertex_property(VId(1), PRICE, Some(CanonicalScalar::Int(2)));
        values.set_vertex_property(VId(1), CATEGORY, Some(text("Tools")));
        batches.push(vec![values]);
        let mut r = WriteBatch::new(R);
        r.delete_edge(EId(10));
        r.add_edge(EId(15), VId(1), VId(3), vec![]);
        let mut s = WriteBatch::new(S);
        s.delete_edge(EId(20));
        s.add_edge(EId(24), VId(3), VId(4), vec![]);
        batches.push(vec![s, r]);
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(2));
        batches.push(vec![cascade]);
        let mut nulls = WriteBatch::new(R);
        nulls.set_vertex_property(VId(1), PRICE, Some(CanonicalScalar::Null));
        nulls.set_vertex_property(VId(3), QUANTITY, Some(CanonicalScalar::Null));
        nulls.set_vertex_property(VId(4), CATEGORY, None);
        batches.push(vec![nulls]);
        let mut delete = WriteBatch::new(R);
        for id in [1, 3, 4, 5] {
            delete.delete_vertex(VId(id));
        }
        batches.push(vec![delete]);
        for (step, batches) in batches.into_iter().enumerate() {
            db.write_atomic(&commit, batches).await.unwrap();
            if step == 1 {
                let definition = definition(Shape::Optional, GlaDirection::Undirected, true);
                let handle = db
                    .register_standing_query(&query, definition.clone(), policy())
                    .unwrap();
                registered.push((handle, definition));
            }
            for (handle, definition) in &registered {
                check(&db, &query, handle, definition);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn quotient_definition() -> PreparedGraphAggregate {
    let mut input = GraphPatternBuilder::new();
    input.vertex("n").unwrap();
    let input = input
        .prepare_values(
            &[
                GraphColumn::vertex("id", "n"),
                GraphColumn::property("quantity", "n", QUANTITY),
                GraphColumn::property("divisor", "n", PRICE),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let quotient = GraphIntegerExpression::prepare(&[
        Op::Column(1),
        Op::Column(2),
        Op::Binary(GraphIntegerBinary::Divide),
    ])
    .unwrap();
    PreparedGraphAggregate::prepare_projected(
        input,
        vec![
            GraphSetProjection::new(
                "group",
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
            ),
            GraphSetProjection::new("quotient", GraphSetValue::Integer(quotient)),
        ],
        &[0],
        &[
            GraphAggregate::min("minimum", 1),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::count_rows("count"),
        ],
        0,
        None,
    )
    .unwrap()
    .with_result_clauses(
        &[GraphAggregateFilter {
            column: Column::Aggregate(2),
            test: GraphAggregateTest::Integer {
                comparison: IntegerComparison::GreaterOrEqual,
                value: 3,
            },
        }],
        &[],
    )
    .unwrap()
}

#[test]
fn hidden_expression_failure_preserves_durable_write_and_rebuild_restores_maintenance() {
    let ((), report) = run_async_under_lab(0x6a62, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for (id, quantity) in [(1, 10), (2, 8)] {
            seed.create_vertex(
                VId(id),
                vec![],
                vec![
                    (QUANTITY, CanonicalScalar::Int(quantity)),
                    (PRICE, CanonicalScalar::Int(2)),
                ],
            );
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let definition = quotient_definition();
        let handle = db
            .register_standing_query(&query, definition.clone(), policy())
            .unwrap();
        assert!(
            db.standing_query(&query, &handle)
                .unwrap()
                .rows()
                .is_empty()
        );
        let mut bad = WriteBatch::new(R);
        bad.set_vertex_property(VId(1), QUANTITY, Some(CanonicalScalar::Int(20)));
        bad.set_vertex_property(VId(2), PRICE, Some(CanonicalScalar::Int(0)));
        let durable = db.write(&commit, bad).await.unwrap();
        assert_ne!(durable, basis);
        assert_eq!(db.frontier().unwrap(), durable);
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier,
                reason: StandingQueryFailure::InputExpression { column: 1, error } })
                if frontier == basis && error.kind == GraphIntegerErrorKind::DivisionByZero));
        assert!(
            matches!(db.rebuild_standing_query(&query, &handle, policy()),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::InputExpression {
                column: 1, error,
            })) if error.kind == GraphIntegerErrorKind::DivisionByZero)
        );
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier, .. }) if frontier == basis));
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(2), PRICE, Some(CanonicalScalar::Int(4)));
        repair.create_vertex(
            VId(3),
            vec![],
            vec![
                (QUANTITY, CanonicalScalar::Int(6)),
                (PRICE, CanonicalScalar::Int(3)),
            ],
        );
        let repaired = db.write(&commit, repair).await.unwrap();
        assert!(matches!(
            db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { .. })
        ));
        assert_eq!(
            db.rebuild_standing_query(&query, &handle, policy())
                .unwrap(),
            repaired
        );
        check(&db, &query, &handle, &definition);
        assert_eq!(
            db.standing_query(&query, &handle)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .0
                .get(1)
                .unwrap()
                .as_integer(),
            Some(14)
        );
        let mut resumed = WriteBatch::new(R);
        resumed.set_vertex_property(VId(1), QUANTITY, Some(CanonicalScalar::Int(30)));
        db.write(&commit, resumed).await.unwrap();
        check(&db, &query, &handle, &definition);
        assert_eq!(
            db.standing_query(&query, &handle)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .0
                .get(1)
                .unwrap()
                .as_integer(),
            Some(19)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unused_computed_column_executes_but_unselected_coalesce_fallback_does_not() {
    let ((), report) = run_async_under_lab(0x6a63, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(QUANTITY, CanonicalScalar::Int(8))]);
        let basis = db.write(&commit, seed).await.unwrap();
        let mut input = GraphPatternBuilder::new();
        input.vertex("n").unwrap();
        let input = input
            .prepare_values(&[GraphColumn::property("quantity", "n", QUANTITY)], 0, None)
            .unwrap()
            .with_duplicates();
        let fallback = GraphIntegerExpression::prepare(&[
            Op::Column(0),
            Op::Literal(Some(1)),
            Op::Literal(Some(0)),
            Op::Binary(GraphIntegerBinary::Divide),
            Op::Coalesce,
        ])
        .unwrap();
        let definition = PreparedGraphAggregate::prepare_projected(
            input,
            vec![GraphSetProjection::new(
                "unused",
                GraphSetValue::Integer(fallback),
            )],
            &[],
            &[GraphAggregate::count_rows("count")],
            0,
            None,
        )
        .unwrap();
        let handle = db
            .register_standing_query(&query, definition.clone(), policy())
            .unwrap();
        check(&db, &query, &handle, &definition);
        let mut clear = WriteBatch::new(R);
        clear.set_vertex_property(VId(1), QUANTITY, None);
        db.write(&commit, clear).await.unwrap();
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier,
                reason: StandingQueryFailure::InputExpression { column: 0, error } })
                if frontier == basis && error.kind == GraphIntegerErrorKind::DivisionByZero));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_input_work_is_independent_of_unrelated_groups_and_properties() {
    let ((), report) = run_async_under_lab(0x6a64, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let definition = definition(Shape::Vertex, GlaDirection::Forward, true);
        let mut measured = Vec::new();
        for extra in [0, 500] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for id in [1, 2] {
                seed.create_vertex(
                    VId(id),
                    vec![],
                    vec![
                        (CATEGORY, text("books")),
                        (QUANTITY, CanonicalScalar::Int(2)),
                        (PRICE, CanonicalScalar::Int(3)),
                    ],
                );
            }
            for id in 10..10 + extra {
                seed.create_vertex(
                    VId(id),
                    vec![],
                    vec![
                        (CATEGORY, text(&format!("other{id}"))),
                        (QUANTITY, CanonicalScalar::Int(2)),
                        (PRICE, CanonicalScalar::Int(3)),
                    ],
                );
            }
            db.write(&commit, seed).await.unwrap();
            let handle = db
                .register_standing_query(&query, definition.clone(), policy())
                .unwrap();
            let mut change = WriteBatch::new(R);
            change.set_vertex_property(VId(1), QUANTITY, Some(CanonicalScalar::Int(4)));
            db.write(&commit, change).await.unwrap();
            check(&db, &query, &handle, &definition);
            measured.push(
                *db.standing_query(&query, &handle)
                    .unwrap()
                    .last_maintenance(),
            );
            let mut unrelated = WriteBatch::new(R);
            unrelated.set_vertex_property(
                VId(1),
                PropertyKeyId(99),
                Some(CanonicalScalar::Int(123)),
            );
            db.write(&commit, unrelated).await.unwrap();
            check(&db, &query, &handle, &definition);
            assert_eq!(
                db.standing_query(&query, &handle)
                    .unwrap()
                    .last_maintenance()
                    .affected_vertices,
                0
            );
        }
        assert_eq!(measured[0], measured[1]);
        assert_eq!(measured[0].affected_vertices, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_computed_collections_refuse_before_registering_even_on_empty_input() {
    let ((), report) = run_async_under_lab(0x6a65, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut input = GraphPatternBuilder::new();
        input.vertex("n").unwrap();
        let input = input
            .prepare_values(&[GraphColumn::property("quantity", "n", QUANTITY)], 0, None)
            .unwrap()
            .with_duplicates();
        let definition = PreparedGraphAggregate::prepare_projected(
            input,
            vec![GraphSetProjection::new(
                "unused",
                GraphSetValue::List(vec![GraphSetValue::Column(0)]),
            )],
            &[],
            &[GraphAggregate::count_rows("count")],
            0,
            None,
        )
        .unwrap();
        assert!(matches!(
            db.register_standing_query(&query, definition, policy()),
            Err(StandingQueryError::Unsupported)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
