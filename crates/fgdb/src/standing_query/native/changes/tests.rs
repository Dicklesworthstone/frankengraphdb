//! The derivative is over final native cells, not source occurrences or groups.
use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue};
use fgdb_gql::{GraphAggregate, GraphExactAverage, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "g") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn scalar(value: i64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)))
}
fn bag<T: Ord>(rows: impl IntoIterator<Item = (T, ZWeight)>) -> ZSet<T> {
    ZSet::from_updates(rows, LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn copied<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn run<T: Ord>(
    rows: &ZSet<T>,
    signed: bool,
    width: usize,
    policy: GqlQueryPolicy,
    mut project: impl FnMut(&T) -> Vec<QueryValue>,
) -> Result<ZSet<Vec<QueryValue>>, StandingQueryFailure> {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy,
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    collect_bag(rows, signed, width, &mut meter, |row, _| Ok(project(row)))
}

#[test]
fn native_projection_commutes_with_exact_small_bag_derivatives() {
    // An independent per-slot array oracle deliberately merges three input
    // support keys into one visible tuple and repeats a RETURN column.
    for before in 0..81_usize {
        for after in 0..81_usize {
            let counts = |mut code: usize| {
                let mut result = [0_i128; 4];
                for count in &mut result {
                    *count = (code % 3) as i128;
                    code /= 3;
                }
                result
            };
            let a = counts(before);
            let b = counts(after);
            let delta =
                bag((0..4_u8)
                    .map(|key| (key, ZWeight::from_i128(b[key as usize] - a[key as usize]))));
            let mapped = run(&delta, true, 2, policy(), |key| {
                vec![scalar(i64::from(*key / 3)); 2]
            })
            .unwrap();
            let expected = bag((0..2).map(|class| {
                let count: i128 = (0..4)
                    .filter(|key| key / 3 == class)
                    .map(|key| b[key] - a[key])
                    .sum();
                (vec![scalar(class as i64); 2], ZWeight::from_i128(count))
            }));
            assert_eq!(mapped, expected);
            let source = bag((0..4_u8).map(|key| (key, ZWeight::from_i128(a[key as usize]))));
            let mut baseline = run(&source, false, 2, policy(), |key| {
                vec![scalar(i64::from(*key / 3)); 2]
            })
            .unwrap();
            baseline
                .integrate(&mapped, LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            let source = bag((0..4_u8).map(|key| (key, ZWeight::from_i128(b[key as usize]))));
            assert_eq!(
                baseline,
                run(&source, false, 2, policy(), |key| {
                    vec![scalar(i64::from(*key / 3)); 2]
                })
                .unwrap()
            );
        }
    }
}

#[test]
fn promoted_signed_weights_never_expand_and_quota_follows_consolidation() {
    let huge = ZWeight::from_i128(i128::MAX)
        .checked_add(&ZWeight::ONE, LIMBS)
        .unwrap();
    let positive = bag([(1_u8, huge)]);
    let bounded = GqlQueryPolicy::new(0, 1, 1000, 1000);
    let output = run(&positive, false, 1, bounded, |_| vec![scalar(7)]).unwrap();
    assert_eq!(output.weight(&vec![scalar(7)]), positive.weight(&1));
    assert!(output.weight(&vec![scalar(7)]).unwrap().to_i128().is_none());
    let negative = bag([(1_u8, ZWeight::from_i128(i128::MIN))]);
    assert_eq!(
        run(&negative, false, 1, bounded, |_| panic!(
            "negative baseline must refuse"
        )),
        Err(StandingQueryFailure::InvalidDelta)
    );
    let retraction = run(&negative, true, 1, bounded, |_| vec![scalar(7)]).unwrap();
    assert_eq!(
        retraction.weight(&vec![scalar(7)]).unwrap().to_i128(),
        Some(i128::MIN)
    );
    let cancellation = bag([
        (
            1_u8,
            positive.weight(&1).unwrap().checked_clone(LIMBS).unwrap(),
        ),
        (2_u8, ZWeight::from_i128(i128::MIN)),
    ]);
    assert!(
        run(
            &cancellation,
            true,
            1,
            GqlQueryPolicy::new(0, 0, 1000, 1000),
            |_| vec![scalar(7)]
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        run(&positive, false, 2, bounded, |_| vec![scalar(7)]),
        Err(StandingQueryFailure::InvalidDelta)
    );
    assert_eq!(
        run(
            &positive,
            false,
            1,
            GqlQueryPolicy::new(0, 0, 1000, 1000),
            |_| vec![scalar(7)]
        ),
        Err(StandingQueryFailure::ResultBudget)
    );
}

#[test]
fn exact_aggregate_cells_follow_repeated_and_permuted_return_slots() {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input = builder
        .prepare_values(
            &[
                GraphColumn::vertex("id", "n"),
                GraphColumn::property("p", "n", PropertyKeyId(1)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let definition = PreparedGraphAggregate::prepare(
        input,
        &[0],
        &[
            GraphAggregate::count_rows("c"),
            GraphAggregate::sum_int("s", 1),
            GraphAggregate::average_int("a", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let average = GraphExactAverage::new(i128::MIN + 1, u64::MAX).unwrap();
    let row = definition
        .materialize_incremental_row(
            vec![GraphValue::Vertex(VId(u128::MAX))],
            vec![
                QueryValue::Count(u64::MAX),
                QueryValue::Integer(i128::MIN),
                QueryValue::Average(average),
            ],
        )
        .unwrap();
    let slots = [
        GraphAggregateTextSlot::Aggregate(2),
        GraphAggregateTextSlot::GroupKey(0),
        GraphAggregateTextSlot::Aggregate(0),
        GraphAggregateTextSlot::Aggregate(1),
        GraphAggregateTextSlot::Aggregate(2),
    ];
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    assert_eq!(
        aggregate_cells(&row, &slots, &mut meter).unwrap(),
        vec![
            QueryValue::Average(average),
            QueryValue::Value(GraphValue::Vertex(VId(u128::MAX))),
            QueryValue::Count(u64::MAX),
            QueryValue::Integer(i128::MIN),
            QueryValue::Average(average),
        ]
    );
    assert_eq!(
        aggregate_cells(&row, &[GraphAggregateTextSlot::Aggregate(3)], &mut meter),
        Err(StandingQueryFailure::InvalidDelta)
    );
}

#[test]
fn every_delivery_checkpoint_and_exact_allowance_preserves_the_source_bag() {
    let source = bag([
        (1_u8, ZWeight::from_i128(-2)),
        (2, ZWeight::ONE),
        (3, ZWeight::ONE),
    ]);
    let before = copied(&source);
    let attempt = |stop, policy| {
        let mut seen = 0;
        let mut checkpoint = || {
            seen += 1;
            if seen == stop {
                Err(StandingQueryFailure::Interrupted)
            } else {
                Ok(())
            }
        };
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let result = collect_bag(&source, true, 1, &mut meter, |row, meter| {
            meter.charge(ZSetEvent::Work)?;
            meter.charge(ZSetEvent::ScratchEntry)?;
            Ok(vec![scalar(i64::from(*row))])
        });
        let stats = meter.stats;
        (result, stats, seen)
    };
    let (expected, stats, calls) = attempt(usize::MAX, policy());
    assert!(calls > 0);
    for stop in 1..=calls {
        let (result, _, seen) = attempt(stop, policy());
        assert_eq!(result, Err(StandingQueryFailure::Interrupted));
        assert_eq!(seen, stop);
        assert_eq!(source, before);
    }
    assert_eq!(
        attempt(
            usize::MAX,
            GqlQueryPolicy::new(0, 3, stats.work_units, stats.scratch_entries)
        )
        .0,
        expected
    );
    for (work, scratch, rows, error) in [
        (
            stats.work_units - 1,
            stats.scratch_entries,
            3,
            StandingQueryFailure::WorkBudget,
        ),
        (
            stats.work_units,
            stats.scratch_entries - 1,
            3,
            StandingQueryFailure::ScratchBudget,
        ),
        (
            stats.work_units,
            stats.scratch_entries,
            2,
            StandingQueryFailure::ResultBudget,
        ),
    ] {
        assert_eq!(
            attempt(usize::MAX, GqlQueryPolicy::new(0, rows, work, scratch)).0,
            Err(error)
        );
    }
    for stop in 1..=calls {
        let mut seen = 0;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut checkpoint = || {
                seen += 1;
                assert_ne!(seen, stop);
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            collect_bag(&source, true, 1, &mut meter, |row, meter| {
                meter.charge(ZSetEvent::Work)?;
                meter.charge(ZSetEvent::ScratchEntry)?;
                Ok(vec![scalar(i64::from(*row))])
            })
        }));
        assert!(result.is_err());
        assert_eq!(source, before);
    }
    assert_eq!(attempt(usize::MAX, policy()).0, expected);
}

fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, p, group) in [(1, 3, 1), (2, 3, 1), (3, 7, 2), (4, -1, 2)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(p)),
                (PropertyKeyId(2), CanonicalScalar::Int(group)),
            ],
        );
    }
    batch
}
fn result_bag(result: QueryResult) -> ZSet<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read result expected")
    };
    bag(rows.into_iter().map(|row| (row, ZWeight::ONE)))
}

#[test]
fn native_bags_and_derivatives_integrate_across_operator_and_aggregate_shapes() {
    let ((), report) = run_async_under_lab(0x6dde_11, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let first = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let texts = [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN DISTINCT n.p AS p",
            "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2",
            "MATCH (n) WITH n.p AS p WHERE p > 0 RETURN p",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            "MATCH (n) WITH [n.p,n.p] AS xs UNWIND xs AS x RETURN x",
            "MATCH (n) RETURN COUNT(*) AS c,SUM(n.p) AS s,AVG(n.p) AS a",
            "MATCH (n) RETURN n.g AS g,COUNT(*) AS c GROUP BY n.g",
            "MATCH (n) WITH n.g AS g,n.p AS p RETURN SUM(p) AS s,g AS key,COUNT(*) AS c GROUP BY g HAVING s > 0 ORDER BY s DESC LIMIT 2",
            "RETURN 7 AS fixed",
        ];
        let mut handles = Vec::new();
        let mut bags = Vec::new();
        for text in texts {
            let handle = db
                .register_standing_native(&cx, text, &params, resolve, policy())
                .unwrap();
            let (at, baseline) = db.standing_native_bag(&cx, &handle, policy()).unwrap();
            assert_eq!(at, first);
            assert_eq!(
                baseline,
                result_bag(db.query(&cx, text, &params, resolve, policy()).unwrap())
            );
            assert!(matches!(
                db.standing_native_delta(&cx, &handle, CommitSeq(first.0 - 1), policy()),
                Err(StandingQueryError::DeltaUnavailable { .. })
            ));
            handles.push(handle);
            bags.push(baseline);
        }
        let mut from = first;
        for tick in 0..3 {
            let mut batch = WriteBatch::new(RelationId(1));
            if tick == 0 {
                batch.delete_vertex(VId(1));
                batch.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(2)));
            } else {
                batch.set_vertex_property(
                    VId(2),
                    PropertyKeyId(99),
                    Some(CanonicalScalar::Int(tick)),
                );
            }
            let at = db.write(&commit, batch).await.unwrap();
            for (index, handle) in handles.iter().enumerate() {
                let (frontier, delta) = db
                    .standing_native_delta(&cx, handle, from, policy())
                    .unwrap();
                assert_eq!(frontier, at);
                if tick > 0 || index == texts.len() - 1 {
                    assert!(delta.is_empty());
                }
                let retry = db
                    .standing_native_delta(&cx, handle, from, policy())
                    .unwrap();
                assert_eq!(retry, (at, copied(&delta)));
                bags[index]
                    .integrate(&delta, LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(
                    &bags[index],
                    &db.standing_native_bag(&cx, handle, policy()).unwrap().1
                );
                assert_eq!(
                    bags[index],
                    result_bag(
                        db.query(&cx, texts[index], &params, resolve, policy())
                            .unwrap()
                    )
                );
                assert!(matches!(
                    db.standing_native_delta(&cx, handle, at, policy()),
                    Err(StandingQueryError::DeltaUnavailable { .. })
                ));
            }
            from = at;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn missed_frontiers_rebuild_baselines_and_delivery_refusals_are_not_empty_ticks() {
    let ((), report) = run_async_under_lab(0x6dde_12, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let first = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let handle = db
            .register_standing_native(&cx, "MATCH (n) RETURN n.p AS p", &params, resolve, policy())
            .unwrap();
        let (_, baseline) = db
            .standing_native_bag(&cx, &handle, GqlQueryPolicy::new(0, 3, 100_000, 100_000))
            .unwrap();
        assert_eq!(baseline.len(), 3); // four occurrences fit three support slots
        assert!(matches!(
            db.standing_native_bag(&cx, &handle, GqlQueryPolicy::new(0, 2, 100_000, 100_000)),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        let mut batch = WriteBatch::new(RelationId(1));
        batch.delete_vertex(VId(1));
        let second = db.write(&commit, batch).await.unwrap();
        let (_, delta) = db
            .standing_native_delta(&cx, &handle, first, policy())
            .unwrap();
        assert_eq!(delta.weight(&vec![scalar(3)]).unwrap().to_i128(), Some(-1));
        assert!(matches!(
            db.standing_native_delta(
                &cx,
                &handle,
                first,
                GqlQueryPolicy::new(0, 0, 100_000, 100_000)
            ),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert_eq!(
            delta,
            db.standing_native_delta(&cx, &handle, first, policy())
                .unwrap()
                .1
        );
        assert!(
            db.rebuild_standing_query(&cx, &handle, GqlQueryPolicy::new(0, 0, 0, 0))
                .is_err()
        );
        assert_eq!(
            delta,
            db.standing_native_delta(&cx, &handle, first, policy())
                .unwrap()
                .1
        );
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert!(
            matches!(db.standing_native_delta(&cx, &handle, first, policy()),
            Err(StandingQueryError::DeltaUnavailable { from, frontier }) if from == first && frontier == second)
        );
        for id in [2, 3] {
            let mut batch = WriteBatch::new(RelationId(1));
            batch.set_vertex_property(VId(id), PropertyKeyId(99), Some(CanonicalScalar::Int(1)));
            db.write(&commit, batch).await.unwrap();
        }
        assert!(matches!(
            db.standing_native_delta(&cx, &handle, second, policy()),
            Err(StandingQueryError::DeltaUnavailable { .. })
        ));
        assert!(matches!(
            db.standing_native_delta(&cx, &handle, CommitSeq(u64::MAX), policy()),
            Err(StandingQueryError::DeltaUnavailable { .. })
        ));
        assert!(db.standing_native_bag(&cx, &handle, policy()).is_ok());
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_native_delta(&cx, &handle, second, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unavailable_aggregate_does_not_export_an_old_derivative_after_durable_write() {
    let ((), report) = run_async_under_lab(0x6dde_13, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let first = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let handle = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN SUM(n.p) AS s",
                &params,
                resolve,
                policy(),
            )
            .unwrap();
        let mut invalid = WriteBatch::new(RelationId(1));
        invalid.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Bool(true)));
        let second = db.write(&commit, invalid).await.unwrap();
        assert_eq!(db.frontier().unwrap(), second);
        assert!(matches!(
            db.standing_native_delta(&cx, &handle, first, policy()),
            Err(StandingQueryError::Unavailable { .. })
        ));
        assert!(matches!(
            db.standing_native_bag(&cx, &handle, policy()),
            Err(StandingQueryError::Unavailable { .. })
        ));
        let mut repaired = WriteBatch::new(RelationId(1));
        repaired.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(3)));
        db.write(&commit, repaired).await.unwrap();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert!(db.standing_native_bag(&cx, &handle, policy()).is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
