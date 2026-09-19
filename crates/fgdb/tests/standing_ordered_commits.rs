//! Public-API regression coverage for ordered standing aggregates.
//!
//! Exercise registration, the real write/commit publication path and explicit
//! rebuild. The independent expected result uses the existing snapshot GLA
//! evaluator, never the standing query's retained aggregate or support maps.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, StandingQueryError, StandingQueryFailure, VertexRow, WriteBatch,
};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
use fgdb_gql::{GraphAggregate, GraphAggregateRow, GqlQueryPolicy, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::collections::BTreeMap;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn definition(grouped: bool, numeric: bool) -> PreparedGraphAggregate {
    let mut input = GraphPatternBuilder::new();
    input.vertex("n").unwrap();
    let input = input
        .prepare_values(
            &[
                GraphColumn::property("group", "n", PropertyKeyId(1)),
                GraphColumn::property("value", "n", PropertyKeyId(2)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    // Put an ordered aggregate FIRST: group existence must not depend on a
    // COUNT or numeric aggregate occupying slot zero.
    let mut aggregates = vec![
        GraphAggregate::min("minimum", 1),
        GraphAggregate::count_distinct("distinct", 1),
        GraphAggregate::count_rows("rows"),
        GraphAggregate::max("maximum", 1),
        GraphAggregate::count("nonnull", 1),
    ];
    if numeric {
        aggregates.extend([
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::sum_int_distinct("distinct_sum", 1),
            GraphAggregate::average_int("average", 1),
            GraphAggregate::average_int_distinct("distinct_average", 1),
        ]);
    }
    PreparedGraphAggregate::prepare(
        input,
        if grouped { &[0] } else { &[] },
        &aggregates,
        0,
        None,
    )
    .unwrap()
}

fn recomputed(
    definition: &PreparedGraphAggregate,
    vertices: &[VertexRow],
) -> ZSet<GraphAggregateRow> {
    let rows: BTreeMap<_, _> = vertices.iter().map(|row| (row.vid, row)).collect();
    let result = definition
        .execute_governed(
            vertices.len() as u64,
            vertices.iter().map(|row| row.vid),
            std::iter::empty::<(VId, RelationId, VId)>(),
            |vid, predicates| {
                let row = rows.get(&vid).unwrap();
                Ok::<_, ()>(predicates.iter().all(|predicate| {
                    predicate.matches_borrowed(
                        row.labels.iter().copied(),
                        row.props.iter().map(|(key, value)| (*key, value)),
                    )
                }))
            },
            |vid, key| {
                Ok::<_, ()>(
                    rows.get(&vid)
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

#[test]
fn registered_ordered_views_follow_commits_and_match_snapshot_evaluation() {
    let ((), report) = run_async_under_lab(0x6a20, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let grouped_definition = definition(true, false);
        let global_definition = definition(false, false);
        let grouped = db
            .register_standing_query(&query, grouped_definition.clone(), policy())
            .unwrap();
        let global = db
            .register_standing_query(&query, global_definition.clone(), policy())
            .unwrap();
        assert!(db.standing_query(&query, &grouped).unwrap().rows().is_empty());
        assert_eq!(
            db.standing_query(&query, &global).unwrap().rows(),
            &recomputed(&global_definition, &[]),
        );

        let mut batches = Vec::new();
        let mut initial = WriteBatch::new(RelationId(1));
        for (id, group, value) in [
            (1, 10, Some(2)),
            (2, 10, Some(2)),
            (3, 10, Some(9)),
            (4, 20, None),
        ] {
            let mut props = vec![(PropertyKeyId(1), CanonicalScalar::Int(group))];
            if let Some(value) = value {
                props.push((PropertyKeyId(2), CanonicalScalar::Int(value)));
            }
            initial.create_vertex(VId(id), vec![], props);
        }
        batches.push(initial);
        let mut replace_duplicate = WriteBatch::new(RelationId(1));
        replace_duplicate.set_vertex_property(
            VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(5)),
        );
        batches.push(replace_duplicate);
        let mut remove_extrema = WriteBatch::new(RelationId(1));
        remove_extrema.delete_vertex(VId(2));
        remove_extrema.delete_vertex(VId(3));
        batches.push(remove_extrema);
        let mut move_and_change_kind = WriteBatch::new(RelationId(1));
        move_and_change_kind.set_vertex_property(
            VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(20)),
        );
        move_and_change_kind.set_vertex_property(
            VId(1), PropertyKeyId(2), Some(CanonicalScalar::Bool(true)),
        );
        move_and_change_kind.set_vertex_property(
            VId(4), PropertyKeyId(2), Some(CanonicalScalar::Int(5)),
        );
        batches.push(move_and_change_kind);
        let mut stored_null = WriteBatch::new(RelationId(1));
        stored_null.set_vertex_property(
            VId(1), PropertyKeyId(2), Some(CanonicalScalar::Null),
        );
        stored_null.set_vertex_property(VId(4), PropertyKeyId(2), None);
        batches.push(stored_null);
        let mut delete_all = WriteBatch::new(RelationId(1));
        delete_all.delete_vertex(VId(4));
        delete_all.delete_vertex(VId(1));
        batches.push(delete_all);

        let mut late = None;
        for (step, write) in batches.into_iter().enumerate() {
            let at = db.write(&commit, write).await.unwrap();
            // Also bootstrap over an existing snapshot and ensure this handle
            // subsequently follows the same incremental commit publication.
            if step == 0 {
                late = Some(
                    db.register_standing_query(&query, grouped_definition.clone(), policy())
                        .unwrap(),
                );
            }
            let vertices = db.vertices().unwrap();
            let expected_grouped = recomputed(&grouped_definition, &vertices);
            let expected_global = recomputed(&global_definition, &vertices);
            for handle in [&grouped, late.as_ref().unwrap()] {
                let view = db.standing_query(&query, handle).unwrap();
                assert_eq!(view.frontier(), at);
                assert_eq!(view.rows(), &expected_grouped, "commit step {step}");
            }
            let view = db.standing_query(&query, &global).unwrap();
            assert_eq!(view.frontier(), at);
            assert_eq!(view.rows(), &expected_global, "commit step {step}");
        }
        assert!(db.standing_query(&query, &grouped).unwrap().rows().is_empty());
        assert_eq!(db.standing_query(&query, &global).unwrap().rows().len(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordered_result_refusal_preserves_durable_write_and_rebuild_resumes_maintenance() {
    let ((), report) = run_async_under_lab(0x6a21, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut initial = WriteBatch::new(RelationId(1));
        for id in [1, 2] {
            initial.create_vertex(
                VId(id),
                vec![],
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(1)),
                    (PropertyKeyId(2), CanonicalScalar::Int(2)),
                ],
            );
        }
        let basis = db.write(&commit, initial).await.unwrap();
        let definition = definition(true, true);
        let bounded = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let handle = db
            .register_standing_query(&query, definition.clone(), bounded)
            .unwrap();
        assert_eq!(
            db.standing_query(&query, &handle).unwrap().rows(),
            &recomputed(&definition, &db.vertices().unwrap()),
        );

        let mut next = WriteBatch::new(RelationId(1));
        next.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(2)));
        next.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(10)));
        next.set_vertex_property(VId(2), PropertyKeyId(2), Some(CanonicalScalar::Int(5)));
        next.create_vertex(
            VId(3),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(3)),
                (PropertyKeyId(2), CanonicalScalar::Int(9)),
            ],
        );
        // The authoritative commit succeeds even though its derived result
        // exceeds the registered one-group limit.
        let at = db.write(&commit, next).await.unwrap();
        assert_ne!(at, basis);
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert!(matches!(
            db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable {
                frontier,
                reason: StandingQueryFailure::ResultBudget,
            }) if frontier == basis
        ));
        assert!(matches!(
            db.rebuild_standing_query(&query, &handle, bounded),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::ResultBudget))
        ));
        // A refused rebuild must preserve the failed generation and its
        // frontier rather than expose a partially rebuilt current result.
        assert!(matches!(
            db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable {
                frontier,
                reason: StandingQueryFailure::ResultBudget,
            }) if frontier == basis
        ));
        assert_eq!(db.rebuild_standing_query(&query, &handle, policy()).unwrap(), at);
        let view = db.standing_query(&query, &handle).unwrap();
        assert_eq!(view.frontier(), at);
        assert_eq!(view.rows(), &recomputed(&definition, &db.vertices().unwrap()));

        let mut resumed = WriteBatch::new(RelationId(1));
        resumed.delete_vertex(VId(1));
        resumed.set_vertex_property(VId(2), PropertyKeyId(2), Some(CanonicalScalar::Int(9)));
        resumed.create_vertex(
            VId(4),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(3)),
                (PropertyKeyId(2), CanonicalScalar::Int(9)),
            ],
        );
        let resumed_at = db.write(&commit, resumed).await.unwrap();
        let view = db.standing_query(&query, &handle).unwrap();
        assert_eq!(view.frontier(), resumed_at);
        assert_eq!(view.rows(), &recomputed(&definition, &db.vertices().unwrap()));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
