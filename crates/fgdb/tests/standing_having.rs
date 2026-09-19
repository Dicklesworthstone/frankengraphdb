//! HAVING is a result-stage derivative, not an input filter.
//! Public committed writes must preserve hidden groups, enforce final visible
//! result limits, and agree with ordinary snapshot aggregate evaluation.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    WriteBatch,
};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder, IntegerComparison,
};
use fgdb_gql::{
    GqlQueryPolicy, GraphAggregate, GraphAggregateColumn as Column, GraphAggregateFilter,
    GraphAggregateOrder, GraphAggregateRow, GraphAggregateTest, GraphHavingExpression,
    GraphHavingOp as Op, GraphHavingOperand as Arg, PreparedGraphAggregate,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const GROUP: PropertyKeyId = PropertyKeyId(1);
const VALUE: PropertyKeyId = PropertyKeyId(2);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
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
    let columns = [
        GraphColumn::property("group", "a", GROUP),
        GraphColumn::property("value", endpoint, VALUE),
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
    PreparedGraphAggregate::prepare(
        input,
        if grouped { &[0] } else { &[] },
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 1),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int("average", 1),
            GraphAggregate::average_int_distinct("distinct_average", 1),
            GraphAggregate::min("minimum", 1),
            GraphAggregate::max("maximum", 1),
            GraphAggregate::count_distinct("endpoints", 2),
        ],
        0,
        None,
    )
    .unwrap()
}

fn threshold(
    query: PreparedGraphAggregate,
    comparison: IntegerComparison,
    value: i128,
) -> PreparedGraphAggregate {
    query
        .with_result_clauses(
            &[GraphAggregateFilter {
                column: Column::Aggregate(0),
                test: GraphAggregateTest::Integer { comparison, value },
            }],
            &[],
        )
        .unwrap()
}

fn combined_having(query: PreparedGraphAggregate) -> PreparedGraphAggregate {
    let having = GraphHavingExpression::prepare(&[
        Op::Compare {
            left: Arg::Column(Column::Aggregate(0)),
            comparison: IntegerComparison::GreaterOrEqual,
            right: Arg::Integer(2),
        },
        Op::Compare {
            left: Arg::Column(Column::Aggregate(3)),
            comparison: IntegerComparison::GreaterOrEqual,
            right: Arg::Integer(2),
        },
        Op::And,
        Op::IsNull {
            operand: Arg::Column(Column::Aggregate(2)),
            is_null: true,
        },
        Op::Or,
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
fn having_matches_snapshot_across_grouped_global_and_all_admitted_join_shapes() {
    let ((), report) = run_async_under_lab(0x6a51, |root| async move {
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
                    let definition = combined_having(definition(shape, direction, grouped));
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
        for id in 1..=5 {
            let mut props = vec![(GROUP, CanonicalScalar::Int((id % 2) as i64))];
            if id != 4 {
                props.push((VALUE, CanonicalScalar::Int(id as i64)));
            }
            seed.create_vertex(VId(id), vec![], props);
        }
        let mut batches = vec![vec![seed]];
        let mut r = WriteBatch::new(R);
        for (eid, src, dst) in [(1, 1, 2), (2, 1, 2), (3, 2, 3), (4, 3, 3)] {
            r.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        let mut s = WriteBatch::new(S);
        for (eid, src, dst) in [(11, 2, 3), (12, 3, 4), (13, 3, 4), (14, 4, 5)] {
            s.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        batches.push(vec![s, r]);
        let mut properties = WriteBatch::new(R);
        properties.set_vertex_property(VId(1), GROUP, Some(CanonicalScalar::Int(3)));
        properties.set_vertex_property(VId(2), VALUE, None);
        properties.set_vertex_property(VId(3), VALUE, Some(CanonicalScalar::Int(7)));
        properties.set_vertex_property(VId(4), VALUE, Some(CanonicalScalar::Int(5)));
        batches.push(vec![properties]);
        let mut r = WriteBatch::new(R);
        r.delete_edge(EId(1));
        r.add_edge(EId(5), VId(5), VId(1), vec![]);
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
        for writes in batches {
            db.write_atomic(&commit, writes).await.unwrap();
            for (handle, definition) in &registered {
                check(&db, &query, handle, definition);
            }
        }
        assert!(db.vertices().unwrap().is_empty());
        for (handle, definition) in &registered {
            let expected = usize::from(definition.group_key_columns().is_empty());
            assert_eq!(
                db.standing_query(&query, handle).unwrap().rows().len(),
                expected
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn global_empty_rows_and_late_registration_cross_having_without_phantom_retractions() {
    let ((), report) = run_async_under_lab(0x6a52, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let zero = threshold(
            definition(Shape::Vertex, GlaDirection::Forward, false),
            IntegerComparison::Equal,
            0,
        );
        let two = threshold(
            definition(Shape::Vertex, GlaDirection::Forward, false),
            IntegerComparison::GreaterOrEqual,
            2,
        );
        let h0 = db
            .register_standing_query(&query, zero.clone(), policy())
            .unwrap();
        let h2 = db
            .register_standing_query(&query, two.clone(), policy())
            .unwrap();
        assert_eq!(db.standing_query(&query, &h0).unwrap().rows().len(), 1);
        assert!(db.standing_query(&query, &h2).unwrap().rows().is_empty());
        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, first).await.unwrap();
        let late = db
            .register_standing_query(&query, zero.clone(), policy())
            .unwrap();
        assert!(db.standing_query(&query, &late).unwrap().rows().is_empty());
        let mut unrelated = WriteBatch::new(R);
        unrelated.set_vertex_property(VId(1), PropertyKeyId(99), Some(CanonicalScalar::Int(7)));
        let mut second = WriteBatch::new(R);
        second.create_vertex(VId(2), vec![], vec![]);
        let mut remove_first = WriteBatch::new(R);
        remove_first.delete_vertex(VId(1));
        let mut remove_second = WriteBatch::new(R);
        remove_second.delete_vertex(VId(2));
        for (write, counts) in [
            (unrelated, (0, 0)),
            (second, (0, 1)),
            (remove_first, (0, 0)),
            (remove_second, (1, 0)),
        ] {
            db.write(&commit, write).await.unwrap();
            check(&db, &query, &h0, &zero);
            check(&db, &query, &late, &zero);
            check(&db, &query, &h2, &two);
            assert_eq!(
                db.standing_query(&query, &h0).unwrap().rows().len(),
                counts.0
            );
            assert_eq!(
                db.standing_query(&query, &h2).unwrap().rows().len(),
                counts.1
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn visible_budget_allows_hidden_groups_and_atomic_swaps_then_rebuilds_after_refusal() {
    let ((), report) = run_async_under_lab(0x6a53, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=6 {
            seed.create_vertex(
                VId(id),
                vec![],
                vec![
                    (GROUP, CanonicalScalar::Int(id as i64)),
                    (VALUE, CanonicalScalar::Int(id as i64)),
                ],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let definition = threshold(
            definition(Shape::Vertex, GlaDirection::Forward, true),
            IntegerComparison::GreaterOrEqual,
            2,
        );
        let bounded = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let handle = db
            .register_standing_query(&query, definition.clone(), bounded)
            .unwrap();
        assert!(
            db.standing_query(&query, &handle)
                .unwrap()
                .rows()
                .is_empty()
        );
        let add = |id, group| {
            let mut write = WriteBatch::new(R);
            write.create_vertex(
                VId(id),
                vec![],
                vec![
                    (GROUP, CanonicalScalar::Int(group)),
                    (VALUE, CanonicalScalar::Int(10)),
                ],
            );
            write
        };
        db.write(&commit, add(100, 1)).await.unwrap();
        check(&db, &query, &handle, &definition);
        assert_eq!(db.standing_query(&query, &handle).unwrap().rows().len(), 1);
        // The group entering the output and the group leaving it are one
        // publication; there must not be a transient two-row budget refusal.
        let mut swap = add(101, 2);
        swap.delete_vertex(VId(100));
        let basis = db.write(&commit, swap).await.unwrap();
        check(&db, &query, &handle, &definition);
        let at = db.write(&commit, add(102, 3)).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 8);
        assert_eq!(recomputed(&db, &definition).len(), 2);
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis));
        assert!(matches!(
            db.rebuild_standing_query(&query, &handle, bounded),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis));
        assert_eq!(
            db.rebuild_standing_query(&query, &handle, policy())
                .unwrap(),
            at
        );
        check(&db, &query, &handle, &definition);
        let mut remove = WriteBatch::new(R);
        remove.delete_vertex(VId(101));
        db.write(&commit, remove).await.unwrap();
        check(&db, &query, &handle, &definition);
        assert_eq!(db.standing_query(&query, &handle).unwrap().rows().len(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_having_domains_are_not_hidden_by_false_filters_or_true_disjunctions() {
    let ((), report) = run_async_under_lab(0x6a54, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(GROUP, CanonicalScalar::Int(1))]);
        let basis = db.write(&commit, seed).await.unwrap();
        let base = definition(Shape::Vertex, GlaDirection::Forward, true);
        let ordinary = base
            .clone()
            .with_result_clauses(
                &[
                    GraphAggregateFilter {
                        column: Column::Aggregate(0),
                        test: GraphAggregateTest::Integer {
                            comparison: IntegerComparison::Less,
                            value: 0,
                        },
                    },
                    GraphAggregateFilter {
                        column: Column::GroupKey(0),
                        test: GraphAggregateTest::Integer {
                            comparison: IntegerComparison::Equal,
                            value: 1,
                        },
                    },
                ],
                &[],
            )
            .unwrap();
        let boolean = base
            .with_having_expression(
                &GraphHavingExpression::prepare(&[
                    Op::Truth(Some(true)),
                    Op::Compare {
                        left: Arg::Column(Column::GroupKey(0)),
                        comparison: IntegerComparison::Equal,
                        right: Arg::Integer(1),
                    },
                    Op::Or,
                ])
                .unwrap(),
            )
            .unwrap();
        let mut registered = Vec::new();
        for definition in [ordinary, boolean] {
            let handle = db
                .register_standing_query(&query, definition.clone(), policy())
                .unwrap();
            check(&db, &query, &handle, &definition);
            registered.push((handle, definition));
        }
        let mut invalid = WriteBatch::new(R);
        invalid.set_vertex_property(VId(1), GROUP, Some(CanonicalScalar::Bool(true)));
        let bad_at = db.write(&commit, invalid).await.unwrap();
        assert_ne!(bad_at, basis);
        for (handle, definition) in &registered {
            assert!(matches!(db.standing_query(&query, handle),
                Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::NonIntegerHaving }) if frontier == basis));
            assert!(matches!(
                db.rebuild_standing_query(&query, handle, policy()),
                Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::NonIntegerHaving
                ))
            ));
            assert!(matches!(
                db.register_standing_query(&query, definition.clone(), policy()),
                Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::NonIntegerHaving
                ))
            ));
        }
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(1), GROUP, Some(CanonicalScalar::Int(1)));
        let at = db.write(&commit, repair).await.unwrap();
        for (handle, definition) in &registered {
            // Advancing the source does not silently skip the failed tick.
            assert!(matches!(
                db.standing_query(&query, handle),
                Err(StandingQueryError::Unavailable { .. })
            ));
            assert_eq!(
                db.rebuild_standing_query(&query, handle, policy()).unwrap(),
                at
            );
            check(&db, &query, handle, definition);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unrelated_hidden_groups_do_not_increase_having_maintenance_work() {
    let ((), report) = run_async_under_lab(0x6a55, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let definition = threshold(
            definition(Shape::Vertex, GlaDirection::Forward, true),
            IntegerComparison::GreaterOrEqual,
            2,
        );
        let mut measured = Vec::new();
        for extra in [0, 500] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for id in 1..=2 {
                seed.create_vertex(
                    VId(id),
                    vec![],
                    vec![
                        (GROUP, CanonicalScalar::Int(0)),
                        (VALUE, CanonicalScalar::Int(1)),
                    ],
                );
            }
            for id in 10..10 + extra {
                seed.create_vertex(
                    VId(id),
                    vec![],
                    vec![
                        (GROUP, CanonicalScalar::Int(id as i64)),
                        (VALUE, CanonicalScalar::Int(1)),
                    ],
                );
            }
            db.write(&commit, seed).await.unwrap();
            let handle = db
                .register_standing_query(&query, definition.clone(), policy())
                .unwrap();
            let mut change = WriteBatch::new(R);
            change.set_vertex_property(VId(1), VALUE, Some(CanonicalScalar::Int(2)));
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

#[test]
fn having_preserves_projection_and_admits_ranked_output() {
    let ((), report) = run_async_under_lab(0x6a56, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let base = combined_having(definition(Shape::Vertex, GlaDirection::Forward, true));
        for definition in [
            base.clone().with_distinct_output(true),
            base.clone().with_key_output_columns(&[]).unwrap(),
            base.clone().with_aggregate_output_prefix(1).unwrap(),
        ] {
            let handle = db
                .register_standing_query(&query, definition.clone(), policy())
                .unwrap();
            check(&db, &query, &handle, &definition);
        }
        let ordered = base
            .with_result_clauses(
                &[],
                &[GraphAggregateOrder::descending(Column::Aggregate(0))],
            )
            .unwrap();
        let handle = db
            .register_standing_query(&query, ordered.clone(), policy())
            .unwrap();
        check(&db, &query, &handle, &ordered);
        assert_eq!(
            db.standing_query(&query, &handle)
                .unwrap()
                .ordered_rows()
                .unwrap()
                .len(),
            0
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
