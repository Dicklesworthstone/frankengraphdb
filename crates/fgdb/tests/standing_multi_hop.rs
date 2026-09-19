//! Multi-hop standing results must equal the ordinary GLA snapshot evaluator.
//! These tests use real database commits, not a fabricated delta or an alternate
//! storage engine. The cost check counts logical work, not wall-clock time.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    StandingQueryStats, WriteBatch,
};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphPatternBuilder, IntegerComparison};
use fgdb_gql::{GqlQueryPolicy, GraphAggregate, PreparedGraphAggregate};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const GROUP: PropertyKeyId = PropertyKeyId(1);
const VALUE: PropertyKeyId = PropertyKeyId(2);
const RANK: PropertyKeyId = PropertyKeyId(3);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

fn definition(
    edges: &[(&str, RelationId, GlaDirection, &str)],
    grouped: bool,
    predicate: bool,
) -> PreparedGraphAggregate {
    let names: BTreeSet<_> = edges.iter().flat_map(|(a, _, _, b)| [*a, *b]).collect();
    let mut builder = GraphPatternBuilder::new();
    for name in names {
        builder.vertex(name).unwrap();
    }
    for &(a, relation, direction, b) in edges {
        builder.edge(a, relation, direction, b).unwrap();
    }
    if predicate {
        // RANK is neither returned nor aggregated. It must still be retained
        // and invalidate bindings when either operand changes.
        builder
            .compare_properties("a", RANK, IntegerComparison::LessOrEqual, "c", RANK)
            .unwrap();
    }
    let input = builder
        .prepare_values(
            &[
                GraphColumn::property("group", "a", GROUP),
                GraphColumn::property("value", "c", VALUE),
                GraphColumn::vertex("endpoint", "c"),
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
            GraphAggregate::min("minimum", 1),
            GraphAggregate::count_rows("paths"),
            GraphAggregate::sum_int("sum", 1),
            GraphAggregate::average_int_distinct("distinct_average", 1),
            GraphAggregate::count_distinct("endpoints", 2),
            GraphAggregate::count("nonnull", 1),
            GraphAggregate::max("maximum", 1),
        ],
        0,
        None,
    )
    .unwrap()
}

fn chain(grouped: bool) -> PreparedGraphAggregate {
    definition(
        &[
            ("a", R, GlaDirection::Forward, "b"),
            ("b", S, GlaDirection::Forward, "c"),
        ],
        grouped,
        false,
    )
}

fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    definition: &PreparedGraphAggregate,
    handle: &StandingQueryHandle,
    at: CommitSeq,
) -> StandingQueryStats {
    let vertices = db.vertices().unwrap();
    let edges = db.edges().unwrap();
    let rows: BTreeMap<_, _> = vertices.iter().map(|row| (row.vid, row)).collect();
    let expected = definition
        .execute_governed(
            (vertices.len() + edges.len()) as u64,
            vertices.iter().map(|row| row.vid),
            edges
                .iter()
                .map(|row| (row.entry.src, row.entry.relation, row.entry.dst)),
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
    let expected = ZSet::from_updates(
        expected.value.into_iter().map(|row| (row, ZWeight::ONE)),
        LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let actual = db.standing_query(cx, handle).unwrap();
    assert_eq!(actual.frontier(), at);
    assert_eq!(actual.rows(), &expected);
    *actual.last_maintenance()
}

fn vertex(batch: &mut WriteBatch, id: u128, group: i64) {
    batch.create_vertex(
        VId(id),
        vec![],
        vec![
            (GROUP, CanonicalScalar::Int(group)),
            (VALUE, CanonicalScalar::Int((id % 7) as i64 - 3)),
            (RANK, CanonicalScalar::Int((id % 5) as i64)),
        ],
    );
}

#[test]
fn committed_joins_match_snapshots_for_orientations_self_joins_branches_and_cycles() {
    let ((), report) = run_async_under_lab(0x6a30, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut views = Vec::new();
        let directions = [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ];
        for left in directions {
            for right in directions {
                for second in [R, S] {
                    for grouped in [false, true] {
                        let definition = definition(
                            &[("a", R, left, "b"), ("b", second, right, "c")],
                            grouped,
                            true,
                        );
                        let handle = db
                            .register_standing_query(&query, definition.clone(), policy())
                            .unwrap();
                        check(&db, &query, &definition, &handle, db.frontier().unwrap());
                        views.push((definition, handle));
                    }
                }
            }
        }
        // Declaration order differs from traversal order in the branching case.
        // Cycle closure introduces a fresh slot plus VertexIdentity, not a walk.
        for edges in [
            vec![
                ("a", R, GlaDirection::Forward, "b"),
                ("c", S, GlaDirection::Reverse, "d"),
                ("b", S, GlaDirection::Forward, "c"),
            ],
            vec![
                ("a", R, GlaDirection::Forward, "b"),
                ("a", S, GlaDirection::Undirected, "c"),
                ("b", R, GlaDirection::Reverse, "d"),
            ],
            vec![
                ("a", R, GlaDirection::Undirected, "b"),
                ("b", S, GlaDirection::Forward, "c"),
                ("c", R, GlaDirection::Forward, "a"),
            ],
        ] {
            let definition = definition(&edges, true, false);
            let handle = db
                .register_standing_query(&query, definition.clone(), policy())
                .unwrap();
            views.push((definition, handle));
        }
        let mut seed = WriteBatch::new(R);
        for id in 1..=4 {
            vertex(&mut seed, id, (id % 2) as i64);
        }
        let at = db.write(&commit, seed).await.unwrap();
        for (definition, handle) in &views {
            check(&db, &query, definition, handle, at);
        }
        let mut r = WriteBatch::new(R);
        for (id, a, b) in [
            (1, 1, 2),
            (2, 1, 2),
            (3, 2, 2),
            (4, 2, 3),
            (5, 3, 1),
            (6, 4, 3),
        ] {
            r.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        let mut s = WriteBatch::new(S);
        for (id, a, b) in [(11, 2, 3), (12, 2, 3), (13, 3, 4), (14, 1, 1), (15, 4, 2)] {
            s.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        let at = db.write_atomic(&commit, vec![s, r]).await.unwrap();
        for (definition, handle) in &views {
            check(&db, &query, definition, handle, at);
        }
        let late = chain(true);
        let handle = db
            .register_standing_query(&query, late.clone(), policy())
            .unwrap();
        check(&db, &query, &late, &handle, at);
        views.push((late, handle));

        let mut r = WriteBatch::new(R);
        r.delete_edge(EId(1));
        r.add_edge(EId(7), VId(3), VId(2), vec![]);
        let mut s = WriteBatch::new(S);
        s.delete_edge(EId(12));
        s.add_edge(EId(16), VId(2), VId(1), vec![]);
        let at = db.write_atomic(&commit, vec![r, s]).await.unwrap();
        for (definition, handle) in &views {
            check(&db, &query, definition, handle, at);
        }
        let mut props = WriteBatch::new(R);
        props.set_vertex_property(VId(2), VALUE, Some(CanonicalScalar::Null));
        props.set_vertex_property(VId(3), GROUP, Some(CanonicalScalar::Int(0)));
        props.set_vertex_property(VId(1), RANK, Some(CanonicalScalar::Int(99)));
        let at = db.write(&commit, props).await.unwrap();
        for (definition, handle) in &views {
            check(&db, &query, definition, handle, at);
        }
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(2));
        let at = db.write(&commit, cascade).await.unwrap();
        for (definition, handle) in &views {
            check(&db, &query, definition, handle, at);
            assert_eq!(
                db.rebuild_standing_query(&query, handle, policy()).unwrap(),
                at
            );
            check(&db, &query, definition, handle, at);
        }
        let mut r = WriteBatch::new(R);
        r.add_edge(EId(8), VId(1), VId(3), vec![]);
        let mut s = WriteBatch::new(S);
        s.add_edge(EId(17), VId(3), VId(4), vec![]);
        let at = db.write_atomic(&commit, vec![s, r]).await.unwrap();
        for (definition, handle) in &views {
            check(&db, &query, definition, handle, at);
        }
        let mut empty = WriteBatch::new(R);
        for id in [1, 3, 4] {
            empty.delete_vertex(VId(id));
        }
        let at = db.write(&commit, empty).await.unwrap();
        for (definition, handle) in &views {
            check(&db, &query, definition, handle, at);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sparse_commit_work_does_not_scan_unrelated_complete_join_components() {
    let ((), report) = run_async_under_lab(0x6a31, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut measurements = Vec::new();
        for extra in [0, 100_u128] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for id in 1..=3 {
                vertex(&mut seed, id, 0);
            }
            for component in 0..extra {
                for offset in 0..3 {
                    vertex(
                        &mut seed,
                        100 + component * 3 + offset,
                        component as i64 + 1,
                    );
                }
            }
            db.write(&commit, seed).await.unwrap();
            let mut r = WriteBatch::new(R);
            let mut s = WriteBatch::new(S);
            r.add_edge(EId(1), VId(1), VId(2), vec![]);
            s.add_edge(EId(2), VId(2), VId(3), vec![]);
            for component in 0..extra {
                let start = 100 + component * 3;
                r.add_edge(
                    EId(1000 + component * 2),
                    VId(start),
                    VId(start + 1),
                    vec![],
                );
                s.add_edge(
                    EId(1001 + component * 2),
                    VId(start + 1),
                    VId(start + 2),
                    vec![],
                );
            }
            db.write_atomic(&commit, vec![r, s]).await.unwrap();
            let definition = chain(true);
            let handle = db
                .register_standing_query(&query, definition.clone(), policy())
                .unwrap();
            let mut update = WriteBatch::new(R);
            update.set_vertex_property(VId(3), VALUE, Some(CanonicalScalar::Int(7)));
            let at = db.write(&commit, update).await.unwrap();
            measurements.push(check(&db, &query, &definition, &handle, at));
        }
        assert_eq!(measurements[0].affected_edges, 1);
        assert_eq!(measurements[0], measurements[1]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_join_view_does_not_undo_commit_and_rebuild_resumes_incremental_updates() {
    let ((), report) = run_async_under_lab(0x6a32, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=4 {
            vertex(&mut seed, id, id as i64);
        }
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();
        let mut s = WriteBatch::new(S);
        s.add_edge(EId(2), VId(2), VId(3), vec![]);
        let basis = db.write(&commit, s).await.unwrap();
        let definition = chain(true);
        let bounded = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let handle = db
            .register_standing_query(&query, definition.clone(), bounded)
            .unwrap();
        check(&db, &query, &definition, &handle, basis);
        let mut add = WriteBatch::new(R);
        add.add_edge(EId(3), VId(4), VId(2), vec![]);
        let at = db.write(&commit, add).await.unwrap();
        assert!(db.edge(EId(3)).unwrap().is_some());
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget })
                if frontier == basis));
        assert!(matches!(
            db.rebuild_standing_query(&query, &handle, bounded),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert!(matches!(db.standing_query(&query, &handle),
            Err(StandingQueryError::Unavailable { frontier, reason: StandingQueryFailure::ResultBudget })
                if frontier == basis));
        assert_eq!(
            db.rebuild_standing_query(&query, &handle, policy())
                .unwrap(),
            at
        );
        check(&db, &query, &definition, &handle, at);
        let mut remove = WriteBatch::new(R);
        remove.delete_edge(EId(3));
        let at = db.write(&commit, remove).await.unwrap();
        check(&db, &query, &definition, &handle, at);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn disconnected_patterns_remain_explicitly_unsupported() {
    let ((), report) = run_async_under_lab(0x6a33, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let disconnected = definition(
            &[
                ("a", R, GlaDirection::Forward, "b"),
                ("c", S, GlaDirection::Forward, "d"),
            ],
            true,
            false,
        );
        assert!(matches!(
            db.register_standing_query(&contexts.query(), disconnected, policy()),
            Err(StandingQueryError::Unsupported)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
