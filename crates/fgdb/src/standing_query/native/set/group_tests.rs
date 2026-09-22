//! Complete relational inputs feed the production maintained-group registry.
//! Independent property/bag arithmetic supplements ordinary eager execution.

use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::{
    GraphColumn, GraphPatternBuilder, GraphValue, GraphValueOrder, IntegerComparison,
};
use fgdb_gql::{
    GraphAggregate, GraphAggregateColumn, GraphAggregateFilter, GraphAggregateOrder,
    GraphAggregateTest, GraphExactAverage, GraphIntegerBinary, GraphIntegerExpression,
    GraphIntegerOp, GraphSetProjection, GraphSetValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphPipelineAggregateText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::collections::{BTreeMap, BTreeSet};

const B: PropertyKeyId = PropertyKeyId(1);
const N: PropertyKeyId = PropertyKeyId(2);
const LIMBS: LimbLimit = LimbLimit::new(4);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 50_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa4; 32],
        DatabaseSecurityNamespaceId([0xa5; 32]),
        [0xa6; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "bucket") => Some(GraphSymbol::Property(B)),
        (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(N)),
        _ => None,
    }
}
fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, bucket, amount) in [
        (1, Some(1), Some(3)),
        (2, Some(1), Some(3)),
        (3, Some(1), None),
        (4, Some(2), Some(-2)),
        (5, Some(2), Some(7)),
        (u128::MAX, None, Some(5)),
    ] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (
                    B,
                    bucket.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                ),
                (
                    N,
                    amount.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                ),
            ],
        );
    }
    batch
}
fn leaf() -> PreparedGraphSet {
    let mut graph = GraphPatternBuilder::new();
    graph.vertex("n").unwrap();
    // Group schema deliberately differs from the first graph leaf's schema.
    let source: PreparedGraphSet = graph
        .prepare_values(
            &[
                GraphColumn::property("raw_amount", "n", N),
                GraphColumn::property("raw_bucket", "n", B),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into();
    source
        .project(
            vec![
                GraphSetProjection::new("bucket", GraphSetValue::Column(1)),
                GraphSetProjection::new("amount", GraphSetValue::Column(0)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
}
fn input(operation: GraphSetOperation, quantifier: GraphSetQuantifier) -> PreparedGraphSet {
    leaf()
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, leaf())
        .unwrap()
        .combine(operation, quantifier, leaf())
        .unwrap()
}
fn definition(
    input: PreparedGraphSet,
    groups: bool,
    count: Option<u64>,
) -> PreparedGraphSetAggregate {
    PreparedGraphSetAggregate::prepare(
        input,
        if groups { &[0] } else { &[] },
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 1),
            GraphAggregate::count_distinct("unique", 1),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::sum_int_distinct("unique_sum", 1),
            GraphAggregate::min("min", 1),
            GraphAggregate::max("max", 1),
            GraphAggregate::average_int("avg", 1),
            GraphAggregate::average_int_distinct("unique_avg", 1),
        ],
        0,
        count,
    )
    .unwrap()
}
fn snapshot<V: Vfs + Clone>(
    db: &Database<V>,
    cx: &QueryCx,
    query: &PreparedGraphSetAggregate,
) -> QueryResult {
    let rows = query
        .execute_governed(
            policy(),
            |pattern, allowance| {
                db.execute_graph_pattern_governed_at(cx, pattern, db.frontier().unwrap(), allowance)
            },
            || cx.checkpoint(),
        )
        .unwrap()
        .value;
    QueryResult::Rows {
        columns: query
            .key_columns()
            .iter()
            .chain(query.aggregate_columns())
            .cloned()
            .collect(),
        rows: rows
            .into_iter()
            .map(|row| {
                row.keys()
                    .iter()
                    .cloned()
                    .map(QueryValue::Value)
                    .chain(row.values().iter().cloned())
                    .collect()
            })
            .collect(),
    }
}
fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn summaries(values: &[Option<i64>]) -> Vec<QueryValue> {
    let nonnull: Vec<_> = values.iter().flatten().copied().collect();
    let distinct: BTreeSet<_> = nonnull.iter().copied().collect();
    let sum: i128 = nonnull.iter().copied().map(i128::from).sum();
    let unique_sum: i128 = distinct.iter().copied().map(i128::from).sum();
    let null = || QueryValue::Value(scalar(None));
    vec![
        QueryValue::Count(values.len() as u64),
        QueryValue::Count(nonnull.len() as u64),
        QueryValue::Count(distinct.len() as u64),
        if nonnull.is_empty() {
            null()
        } else {
            QueryValue::Integer(sum)
        },
        if distinct.is_empty() {
            null()
        } else {
            QueryValue::Integer(unique_sum)
        },
        QueryValue::Value(scalar(nonnull.iter().min().copied())),
        QueryValue::Value(scalar(nonnull.iter().max().copied())),
        if nonnull.is_empty() {
            null()
        } else {
            QueryValue::Average(GraphExactAverage::new(sum, nonnull.len() as u64).unwrap())
        },
        if distinct.is_empty() {
            null()
        } else {
            QueryValue::Average(GraphExactAverage::new(unique_sum, distinct.len() as u64).unwrap())
        },
    ]
}
fn independent<V: Vfs + Clone>(
    db: &Database<V>,
    op: GraphSetOperation,
    q: GraphSetQuantifier,
) -> QueryResult {
    let mut source = BTreeMap::new();
    for vertex in db.vertices_at(db.frontier().unwrap()).unwrap() {
        let get = |key| match vertex.props.iter().find(|(k, _)| *k == key).map(|(_, v)| v) {
            Some(CanonicalScalar::Int(n)) => Some(*n),
            None | Some(CanonicalScalar::Null) => None,
            _ => panic!("integer fixture"),
        };
        *source.entry((get(B), get(N))).or_insert(0_usize) += 1;
    }
    let mut groups: BTreeMap<GraphValue, Vec<Option<i64>>> = BTreeMap::new();
    for ((key, value), multiplicity) in source {
        let count = match (op, q) {
            (GraphSetOperation::Union, GraphSetQuantifier::All) => 3 * multiplicity,
            (GraphSetOperation::Intersect | GraphSetOperation::Except, GraphSetQuantifier::All) => {
                multiplicity
            }
            (
                GraphSetOperation::Union | GraphSetOperation::Intersect,
                GraphSetQuantifier::Distinct,
            ) => 1,
            (GraphSetOperation::Except, GraphSetQuantifier::Distinct) => 0,
        };
        if count != 0 {
            groups
                .entry(scalar(key))
                .or_default()
                .extend(std::iter::repeat_n(value, count));
        }
    }
    QueryResult::Rows {
        columns: [
            "bucket",
            "rows",
            "nonnull",
            "unique",
            "sum",
            "unique_sum",
            "min",
            "max",
            "avg",
            "unique_avg",
        ]
        .map(str::to_owned)
        .to_vec(),
        rows: groups
            .into_iter()
            .map(|(key, values)| {
                std::iter::once(QueryValue::Value(key))
                    .chain(summaries(&values))
                    .collect()
            })
            .collect(),
    }
}
fn metadata(query: &PreparedGraphSetAggregate) -> (Vec<String>, Vec<GraphAggregateTextSlot>) {
    (
        query
            .key_columns()
            .iter()
            .chain(query.aggregate_columns())
            .cloned()
            .collect(),
        (0..query.key_columns().len())
            .map(GraphAggregateTextSlot::GroupKey)
            .chain((0..query.aggregate_columns().len()).map(GraphAggregateTextSlot::Aggregate))
            .collect(),
    )
}

#[test]
fn all_set_laws_group_the_completed_relation_and_publish_exact_native_deltas() {
    let ((), report) = run_async_under_lab(0x6772_1011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut cases = Vec::new();
        for op in [
            GraphSetOperation::Union,
            GraphSetOperation::Intersect,
            GraphSetOperation::Except,
        ] {
            for q in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
                let def = definition(input(op, q), true, None);
                let handle = db
                    .register_standing_relation_aggregate(&cx, &def, policy())
                    .unwrap();
                assert!(db.standing_group_delta(&cx, &handle).unwrap().is_none());
                cases.push((op, q, def, handle));
            }
        }
        for tick in 0..5 {
            let mut previous = Vec::new();
            for (op, q, def, handle) in &cases {
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    independent(&db, *op, *q)
                );
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    snapshot(&db, &cx, def)
                );
                assert_eq!(
                    db.standing_group_definition(&cx, handle)
                        .unwrap()
                        .canonical_bytes(),
                    def.canonical_bytes()
                );
                previous.push(copy(db.standing_query(&cx, handle).unwrap().rows()));
            }
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.set_vertex_property(VId(2), B, Some(CanonicalScalar::Int(2)));
                }
                1 => {
                    change.delete_vertex(VId(1));
                }
                2 => {
                    change.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(-5)));
                }
                3 => {
                    change.set_vertex_property(VId(u128::MAX), N, None);
                }
                _ => {
                    change.set_vertex_property(
                        VId(5),
                        PropertyKeyId(99),
                        Some(CanonicalScalar::Bool(true)),
                    );
                }
            }
            let at = db.write(&commit, change).await.unwrap();
            for ((op, q, _, handle), mut integrated) in cases.iter().zip(previous) {
                let delta = db.standing_group_delta(&cx, handle).unwrap().unwrap();
                assert_eq!(delta.frontier(), at);
                if tick == 4 {
                    assert!(delta.rows().is_empty());
                }
                integrated
                    .integrate(delta.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&integrated, db.standing_query(&cx, handle).unwrap().rows());
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap(),
                    (at, independent(&db, *op, *q))
                );
            }
        }
        for (op, q, _, handle) in &cases {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
            assert_eq!(
                db.standing_native_query(&cx, handle, policy()).unwrap().1,
                independent(&db, *op, *q)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_with_grouping_preserves_alias_slots_frozen_parameters_having_and_input_distinct() {
    let ((), report) = run_async_under_lab(0x6772_1012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (n) WITH DISTINCT n.bucket AS bucket,n.amount AS amount WHERE amount >= $floor \
            RETURN SUM(amount) AS total,bucket AS key,COUNT(*) AS copies GROUP BY bucket \
            HAVING total >= $minimum ORDER BY total DESC LIMIT 2";
        let args = GqlParameters::new()
            .with_int64("floor", 0)
            .unwrap()
            .with_int64("minimum", 0)
            .unwrap();
        let template = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
        let h = template
            .register_standing(&mut db, &cx, &args, policy())
            .unwrap();
        let bound = PreparedGraphPipelineAggregateText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&args)
            .unwrap();
        assert!(bound.input_relation().is_some());
        // The typed aggregate entrypoint must route the entire relation too.
        let typed = db.register_standing_query(&cx, bound, policy()).unwrap();
        drop(template);
        assert_eq!(
            db.standing_native_columns(&cx, &h).unwrap(),
            &["total", "key", "copies"]
        );
        let expected = QueryResult::Rows {
            columns: vec!["total".into(), "key".into(), "copies".into()],
            rows: vec![
                vec![
                    QueryValue::Integer(7),
                    QueryValue::Value(scalar(Some(2))),
                    QueryValue::Count(1),
                ],
                vec![
                    QueryValue::Integer(5),
                    QueryValue::Value(scalar(None)),
                    QueryValue::Count(1),
                ],
            ],
        };
        assert_eq!(
            db.standing_native_query(&cx, &h, policy()).unwrap().1,
            expected
        );
        for step in 0..3 {
            assert_eq!(
                db.standing_native_query(&cx, &h, policy()).unwrap().1,
                db.query(&cx, text, &args, symbols, policy()).unwrap()
            );
            assert_eq!(
                db.standing_query(&cx, &h).unwrap().rows(),
                db.standing_query(&cx, &typed).unwrap().rows()
            );
            let mut change = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    change.set_vertex_property(VId(1), N, Some(CanonicalScalar::Int(20)));
                }
                1 => {
                    change.delete_vertex(VId(2));
                }
                _ => {
                    change.set_vertex_property(VId(4), N, Some(CanonicalScalar::Int(30)));
                }
            }
            db.write(&commit, change).await.unwrap();
        }
        db.rebuild_standing_query(&cx, &h, policy()).unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &h, policy()).unwrap().1,
            db.query(&cx, text, &args, symbols, policy()).unwrap()
        );
        assert!(db.standing_group_delta(&cx, &h).unwrap().is_none());
        let strict = GqlParameters::new()
            .with_int64("floor", 100)
            .unwrap()
            .with_int64("minimum", 0)
            .unwrap();
        let separate = db
            .register_standing_native(&cx, text, &strict, symbols, policy())
            .unwrap();
        assert!(db.standing_query(&cx, &separate).unwrap().rows().is_empty());
        assert!(!db.standing_query(&cx, &h).unwrap().rows().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn private_input_allowance_and_final_group_pages_survive_rebuild_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x6772_1013, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let def = definition(
            input(GraphSetOperation::Union, GraphSetQuantifier::All),
            false,
            None,
        );
        let one = GqlQueryPolicy::new(100_000, 1, 50_000_000, 50_000_000);
        let handle = db
            .register_standing_relation_aggregate(&cx, &def, one)
            .unwrap();
        let empty = db.standing_query(&cx, &handle).unwrap();
        assert_eq!(empty.rows().len(), 1);
        assert_eq!(
            empty.rows().iter().next().unwrap().0.values(),
            summaries(&[])
        );
        db.write(&commit, seed()).await.unwrap();
        assert_eq!(
            db.standing_query(&cx, &handle)
                .unwrap()
                .rows()
                .iter()
                .next()
                .unwrap()
                .0
                .values()[0]
                .as_count(),
            Some(18)
        );
        assert_eq!(
            db.standing_native_query(&cx, &handle, one).unwrap().1,
            snapshot(&db, &cx, &def)
        );
        // A group page of one does not cap pre-HAVING groups or input tuples.
        let ranked = definition(leaf(), true, Some(1))
            .with_result_clauses(
                &[],
                &[GraphAggregateOrder::descending(
                    GraphAggregateColumn::Aggregate(3),
                )],
            )
            .unwrap();
        let page = db
            .register_standing_relation_aggregate(&cx, &ranked, one)
            .unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &page, one).unwrap().1,
            snapshot(&db, &cx, &ranked)
        );
        db.rebuild_standing_query(&cx, &handle, one).unwrap();
        db.rebuild_standing_query(&cx, &page, one).unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.create_vertex(
            VId(99),
            vec![],
            vec![(B, CanonicalScalar::Int(3)), (N, CanonicalScalar::Int(100))],
        );
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &page, one).unwrap().1,
            snapshot(&db, &cx, &ranked)
        );
        db.compact(&commit).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &handle, one).unwrap().1,
            snapshot(&db, &cx, &def)
        );
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_query(&cx, &handle),
            Err(StandingQueryError::ForeignHandle)
        ));
        let fresh = db
            .register_standing_relation_aggregate(&cx, &def, one)
            .unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &fresh, one).unwrap().1,
            snapshot(&db, &cx, &def)
        );
        assert!(db.standing_group_delta(&cx, &fresh).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[derive(Debug, PartialEq, Eq)]
struct Saved {
    address: usize,
    status: (GqlQueryPolicy, CommitSeq, Option<StandingQueryFailure>),
    rows: Option<ZSet<GraphValueRow>>,
    groups: Option<ZSet<GraphAggregateRow>>,
    ordered: Option<Vec<GraphAggregateRow>>,
}
fn saved(queries: &[StandingQuery]) -> Vec<Saved> {
    queries
        .iter()
        .map(|query| match query {
            StandingQuery::Group(group) => Saved {
                address: std::ptr::from_ref(group.rows()) as usize,
                status: query.status(),
                rows: None,
                groups: Some(copy(group.rows())),
                ordered: group
                    .ordered_rows()
                    .map(|rows| rows.iter().map(|row| row.as_ref().clone()).collect()),
            },
            _ => {
                let rows = sets::rows(query).unwrap();
                Saved {
                    address: std::ptr::from_ref(rows) as usize,
                    status: query.status(),
                    rows: Some(copy(rows)),
                    groups: None,
                    ordered: None,
                }
            }
        })
        .collect()
}

#[test]
fn every_group_circuit_admission_and_rebuild_checkpoint_preserves_the_old_registry() {
    let ((), report) = run_async_under_lab(0x6772_1014, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let sibling = db
            .register_standing_relation(&cx, &leaf(), policy())
            .unwrap();
        let selected = input(GraphSetOperation::Union, GraphSetQuantifier::Distinct)
            .with_order_by(&[GraphValueOrder::descending(1)])
            .unwrap()
            .with_page(0, Some(4));
        let def = definition(selected, true, Some(1))
            .with_result_clauses(
                &[],
                &[GraphAggregateOrder::descending(
                    GraphAggregateColumn::Aggregate(3),
                )],
            )
            .unwrap()
            .with_key_output_columns(&[])
            .unwrap()
            .with_aggregate_output_prefix(1)
            .unwrap()
            .with_distinct_output(true);
        let (names, slots) = metadata(&def);
        let one = GqlQueryPolicy::new(100_000, 1, 50_000_000, 50_000_000);
        let mut calls = 0;
        let h = register_group_checked(&mut db, &cx, &def, &names, &slots, one, &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        let before = saved(&db.standing_queries);
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(
                register_group_checked(&mut db, &cx, &def, &names, &slots, one, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryError::Maintenance(
                            StandingQueryFailure::Interrupted,
                        ))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert_eq!(seen, stop);
            assert_eq!(saved(&db.standing_queries), before);
        }
        let first = match h.native.as_deref().unwrap() {
            Layout::GroupCircuit { first, .. } => *first,
            _ => unreachable!(),
        };
        let mut calls = 0;
        rebuild_checked(&mut db, &cx, first, h.index, one, &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        let accepted = saved(&db.standing_queries);
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(
                rebuild_checked(&mut db, &cx, first, h.index, one, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryError::Maintenance(
                            StandingQueryFailure::Interrupted,
                        ))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert_eq!(seen, stop);
            assert_eq!(saved(&db.standing_queries), accepted);
        }
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(5));
        db.write(&commit, edit).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &h, one).unwrap().1,
            snapshot(&db, &cx, &def)
        );
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn output_errors_dependency_failure_and_quotas_fence_only_the_affected_circuit_and_repair() {
    let ((), report) = run_async_under_lab(0x6772_1015, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let sibling = db
            .register_standing_relation(&cx, &leaf(), policy())
            .unwrap();
        // 12/SUM: a final LIMIT 0 still evaluates output for every qualifying group.
        let expression = GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Column(4),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap();
        let make = |having: bool| {
            let query = definition(leaf(), true, Some(0))
                .with_output_projection(vec![GraphSetProjection::new(
                    "ratio",
                    GraphSetValue::Integer(expression.clone()),
                )])
                .unwrap();
            if having {
                query
                    .with_result_clauses(
                        &[GraphAggregateFilter {
                            column: GraphAggregateColumn::Aggregate(3),
                            test: GraphAggregateTest::Integer {
                                comparison: IntegerComparison::NotEqual,
                                value: 0,
                            },
                        }],
                        &[],
                    )
                    .unwrap()
            } else {
                query
            }
        };
        let bad = db
            .register_standing_relation_aggregate(&cx, &make(false), policy())
            .unwrap();
        let guarded = db
            .register_standing_relation_aggregate(&cx, &make(true), policy())
            .unwrap();
        let global = definition(leaf(), false, None);
        let healthy = db
            .register_standing_relation_aggregate(&cx, &global, policy())
            .unwrap();
        let mut zero = WriteBatch::new(RelationId(1));
        zero.set_vertex_property(VId(5), N, Some(CanonicalScalar::Int(2)));
        let at = db.write(&commit, zero).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(matches!(
            db.standing_query(&cx, &bad),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::OutputExpression { .. },
                ..
            })
        ));
        assert!(db.standing_query(&cx, &guarded).unwrap().rows().is_empty());
        db.standing_query(&cx, &healthy).unwrap();
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
        assert!(db.rebuild_standing_query(&cx, &bad, policy()).is_err());
        let mut repair = WriteBatch::new(RelationId(1));
        repair.set_vertex_property(VId(5), N, Some(CanonicalScalar::Int(9)));
        db.write(&commit, repair).await.unwrap();
        db.rebuild_standing_query(&cx, &bad, policy()).unwrap();
        assert!(db.standing_query(&cx, &bad).unwrap().rows().is_empty());
        // A nonnumeric contribution refuses after a durable write, without a
        // partial count/sum. Rebuild starts from current rows, not stale deltas.
        let mut invalid = WriteBatch::new(RelationId(1));
        invalid.set_vertex_property(VId(5), N, Some(CanonicalScalar::Bool(true)));
        db.write(&commit, invalid).await.unwrap();
        assert!(matches!(
            db.standing_query(&cx, &healthy),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::NonIntegerAggregate { column: 1 },
                ..
            })
        ));
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
        let mut repair = WriteBatch::new(RelationId(1));
        repair.set_vertex_property(VId(5), N, Some(CanonicalScalar::Int(9)));
        db.write(&commit, repair).await.unwrap();
        db.rebuild_standing_query(&cx, &healthy, policy()).unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &healthy, policy()).unwrap().1,
            snapshot(&db, &cx, &global)
        );
        assert!(db.standing_group_delta(&cx, &healthy).unwrap().is_none());
        let before = saved(&db.standing_queries);
        assert!(
            db.register_standing_relation_aggregate(
                &cx,
                &definition(leaf(), true, None),
                GqlQueryPolicy::new(100_000, 1, 50_000_000, 50_000_000)
            )
            .is_err()
        );
        assert_eq!(saved(&db.standing_queries), before);
        assert!(matches!(
            db.standing_native_query(
                &cx,
                &healthy,
                GqlQueryPolicy::new(0, 0, 50_000_000, 50_000_000)
            ),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        db.standing_query(&cx, &healthy).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_domains_sources_and_bad_metadata_never_publish_a_partial_group_circuit() {
    let ((), report) = run_async_under_lab(0x6772_1016, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let sibling = db
            .register_standing_relation(&cx, &leaf(), policy())
            .unwrap();
        let before = saved(&db.standing_queries);
        let collect = PreparedGraphSetAggregate::prepare(
            leaf(),
            &[],
            &[GraphAggregate::collect("values", 1)],
            0,
            Some(0),
        )
        .unwrap();
        assert!(
            db.register_standing_relation_aggregate(&cx, &collect, policy())
                .is_err()
        );
        let list = leaf()
            .project(
                vec![GraphSetProjection::new("n", GraphSetValue::List(vec![]))],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let unsupported_domain =
            PreparedGraphSetAggregate::prepare(list, &[], &[GraphAggregate::sum("n", 0)], 0, None)
                .unwrap();
        assert!(
            db.register_standing_relation_aggregate(&cx, &unsupported_domain, policy())
                .is_err()
        );
        let unsupported_source = leaf()
            .with_page(1, None)
            .with_order_by(&order(true))
            .unwrap();
        let unsupported = PreparedGraphSetAggregate::prepare(
            unsupported_source,
            &[],
            &[GraphAggregate::count_rows("n")],
            0,
            None,
        )
        .unwrap();
        assert!(
            db.register_standing_relation_aggregate(&cx, &unsupported, policy())
                .is_err()
        );
        let good = definition(leaf(), false, None);
        assert!(
            register_group(
                &mut db,
                &cx,
                &good,
                &["bad".into()],
                &[GraphAggregateTextSlot::GroupKey(0)],
                policy()
            )
            .is_err()
        );
        assert_eq!(saved(&db.standing_queries), before);
        // Empty parent data cannot conceal a declared domain mismatch.
        let mut graph = GraphPatternBuilder::new();
        graph.vertex("n").unwrap();
        let identities: PreparedGraphSet = graph
            .prepare_values(
                &[
                    GraphColumn::vertex("bucket", "n"),
                    GraphColumn::property("amount", "n", N),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates()
            .into();
        let parent = db
            .register_standing_relation(&cx, &identities, policy())
            .unwrap();
        assert!(matches!(
            db.prepare_standing_group(&cx, parent.index, good, policy(), db.standing_queries.len()),
            Err(StandingQueryError::GroupSchema(_))
        ));
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
        // Foreign ownership is checked before attempting to read any output.
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_group_delta(&cx, &parent),
            Err(StandingQueryError::ForeignHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn input_expression_failure_fences_the_group_and_rebuilds_the_entire_owned_input() {
    let ((), report) = run_async_under_lab(0x6772_1017, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let divide = GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Column(1),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap();
        let projected = leaf()
            .project(
                vec![
                    GraphSetProjection::new("bucket", GraphSetValue::Column(0)),
                    GraphSetProjection::new("amount", GraphSetValue::Integer(divide)),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let def = definition(projected, true, None);
        let h = db
            .register_standing_relation_aggregate(&cx, &def, policy())
            .unwrap();
        let ordinary = db
            .register_standing_relation_aggregate(&cx, &definition(leaf(), true, None), policy())
            .unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), N, Some(CanonicalScalar::Int(0)));
        db.write(&commit, change).await.unwrap();
        assert!(matches!(
            db.standing_query(&cx, &h),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::DependencyUnavailable,
                ..
            })
        ));
        db.standing_query(&cx, &ordinary).unwrap();
        let before = saved(&db.standing_queries);
        assert!(db.rebuild_standing_query(&cx, &h, policy()).is_err());
        assert_eq!(saved(&db.standing_queries), before);
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), N, Some(CanonicalScalar::Int(4)));
        db.write(&commit, change).await.unwrap();
        db.rebuild_standing_query(&cx, &h, policy()).unwrap();
        assert!(db.standing_group_delta(&cx, &h).unwrap().is_none());
        assert_eq!(
            db.standing_native_query(&cx, &h, policy()).unwrap().1,
            snapshot(&db, &cx, &def)
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(4));
        db.write(&commit, change).await.unwrap();
        assert!(db.standing_group_delta(&cx, &h).unwrap().is_some());
        assert_eq!(
            db.standing_native_query(&cx, &h, policy()).unwrap().1,
            snapshot(&db, &cx, &def)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_product_inputs_group_full_width_identities_after_their_own_page() {
    let ((), report) = run_async_under_lab(0x6772_1018, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut graph = GraphPatternBuilder::new();
        graph.vertex("n").unwrap();
        let identities: PreparedGraphSet = graph
            .prepare_values(
                &[
                    GraphColumn::vertex("owner", "n"),
                    GraphColumn::property("unused", "n", N),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates()
            .into();
        let selected = identities
            .with_order_by(&[GraphValueOrder::descending(0)])
            .unwrap()
            .with_page(0, Some(3));
        let product = selected
            .cross_join(leaf())
            .unwrap()
            .project(
                vec![
                    GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                    GraphSetProjection::new("amount", GraphSetValue::Column(3)),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap()
            .with_order_by(&[
                GraphValueOrder {
                    column: 1,
                    descending: true,
                    nulls_first: false,
                },
                GraphValueOrder::descending(0),
            ])
            .unwrap()
            .with_page(0, Some(4));
        let def = definition(product, true, None);
        let allowance = GqlQueryPolicy::new(100_000, 3, 50_000_000, 50_000_000);
        let h = db
            .register_standing_relation_aggregate(&cx, &def, allowance)
            .unwrap();
        // Eighteen product occurrences become four selected occurrences, then
        // three groups. The final group quota must not cap either private bag.
        let mut rows = Vec::new();
        for (id, values) in [
            (4, vec![Some(7)]),
            (5, vec![Some(7)]),
            (u128::MAX, vec![Some(7), Some(5)]),
        ] {
            rows.push(
                std::iter::once(QueryValue::Value(GraphValue::Vertex(VId(id))))
                    .chain(summaries(&values))
                    .collect(),
            );
        }
        let expected = QueryResult::Rows {
            columns: metadata(&def).0,
            rows,
        };
        assert_eq!(
            db.standing_native_query(&cx, &h, allowance).unwrap().1,
            expected
        );
        for step in 0..3 {
            assert_eq!(
                db.standing_native_query(&cx, &h, allowance).unwrap().1,
                snapshot(&db, &cx, &def)
            );
            let mut integrated = copy(db.standing_query(&cx, &h).unwrap().rows());
            let mut edit = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    edit.set_vertex_property(VId(5), N, Some(CanonicalScalar::Int(-10)));
                }
                1 => {
                    edit.delete_vertex(VId(u128::MAX));
                }
                _ => {
                    edit.create_vertex(
                        VId(u128::MAX - 1),
                        vec![],
                        vec![(N, CanonicalScalar::Int(11))],
                    );
                }
            }
            db.write(&commit, edit).await.unwrap();
            integrated
                .integrate(
                    db.standing_group_delta(&cx, &h).unwrap().unwrap().rows(),
                    LIMBS,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
            assert_eq!(&integrated, db.standing_query(&cx, &h).unwrap().rows());
        }
        db.rebuild_standing_query(&cx, &h, allowance).unwrap();
        assert!(db.standing_group_delta(&cx, &h).unwrap().is_none());
        assert_eq!(
            db.standing_native_query(&cx, &h, allowance).unwrap().1,
            snapshot(&db, &cx, &def)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
