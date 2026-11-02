//! Compound temporal queries must pin every operand to one exact historical
//! sequence. Computed projections make a live-frontier fallback observable even
//! when both branches happen to return the same vertex identities.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedTemporalGraphSetText,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(20_000, 20_000, 5_000_000, 5_000_000)
}
fn ints(result: &fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>) -> Vec<i64> {
    result
        .value
        .iter()
        .map(|row| match row.values()[0].as_scalar() {
            Some(CanonicalScalar::Int(value)) => *value,
            other => panic!("expected integer set value, got {other:?}"),
        })
        .collect()
}

#[test]
fn a_with_stage_after_the_system_time_clause_runs_at_the_selected_sequence() {
    let ((), report) = run_async_under_lab(0x7e45_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        first.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
        let seq1 = db.write(&commit, first).await.unwrap();
        let mut second = WriteBatch::new(R);
        second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(3)));
        second.create_vertex(VId(3), vec![], vec![(P, CanonicalScalar::Int(4))]);
        let seq2 = db.write(&commit, second).await.unwrap();
        let mut third = WriteBatch::new(R);
        third.delete_vertex(VId(2));
        let seq3 = db.write(&commit, third).await.unwrap();

        // The clause closes the first MATCH and a WITH stage follows it, as
        // the aggregate facade already allows for its pipelines.
        let template = PreparedTemporalGraphSetText::prepare(
            "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at WITH a.p AS x WHERE x >= 2 RETURN x*2 AS p ORDER BY p",
            symbols,
        )
        .unwrap();
        for (seq, expected) in [(seq1, vec![4]), (seq2, vec![4, 6, 8]), (seq3, vec![6, 8])] {
            let bound = template
                .bind_parameters(&GqlParameters::new().with_uint64("at", seq.0).unwrap())
                .unwrap();
            let temporal = db
                .execute_temporal_graph_set_text_governed(&query, &bound, policy())
                .unwrap();
            let explicit = db
                .execute_graph_set_governed_at(&query, bound.query(), seq, policy())
                .unwrap();
            assert_eq!(temporal, explicit);
            assert_eq!(ints(&temporal), expected, "WITH stage at {seq:?}");
        }
        // A correlated CALL body may follow the clause too, and it reads at the
        // cut: at seq1 a body reading the live head would count 2 above both.
        let template = PreparedTemporalGraphSetText::prepare(
            "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at CALL (a) { MATCH (b) WHERE b.p > a.p \
             RETURN count(*) AS above } RETURN above ORDER BY above",
            symbols,
        )
        .unwrap();
        for (seq, expected) in [
            (seq1, vec![0, 1]),
            (seq2, vec![0, 1, 2]),
            (seq3, vec![0, 1]),
        ] {
            let bound = template
                .bind_parameters(&GqlParameters::new().with_uint64("at", seq.0).unwrap())
                .unwrap();
            let temporal = db
                .execute_temporal_graph_set_text_governed(&query, &bound, policy())
                .unwrap();
            let explicit = db
                .execute_graph_set_governed_at(&query, bound.query(), seq, policy())
                .unwrap();
            assert_eq!(temporal, explicit);
            assert_eq!(ints(&temporal), expected, "CALL body at {seq:?}");
        }
        // A historical read cannot turn into a write after the selector.
        assert!(matches!(
            PreparedTemporalGraphSetText::prepare(
                "MATCH (a) FOR SYSTEM_TIME AS OF SEQ 1 SET a.p=2 RETURN a",
                symbols,
            )
            .unwrap_err()
            .kind,
            fgdb_gql::GraphTemporalSetTextErrorKind::InvalidSystemTimePosition
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_unwind_reads_paths_and_correlated_bodies_at_one_cut() {
    let ((), report) = run_async_under_lab(0x7e45_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut first = WriteBatch::new(R);
        for (id, value) in [(1, 10), (2, 20), (3, 30)] {
            first.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
        }
        first.add_edge(
            EId(11),
            VId(1),
            VId(2),
            vec![(P, CanonicalScalar::Int(100))],
        );
        first.add_edge(
            EId(12),
            VId(2),
            VId(3),
            vec![(P, CanonicalScalar::Int(200))],
        );
        let before = db.write(&commit, first).await.unwrap();
        let pinned = db.read_session().unwrap();

        // Both the topology and properties differ at the live head. Using
        // the old identities with live property readers cannot pass these laws.
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(21)));
        changes.set_edge_property(EId(11), P, Some(CanonicalScalar::Int(101)));
        changes.delete_edge(EId(12));
        changes.create_vertex(VId(4), vec![], vec![(P, CanonicalScalar::Int(40))]);
        changes.add_edge(
            EId(13),
            VId(2),
            VId(4),
            vec![(P, CanonicalScalar::Int(300))],
        );
        let after = db.write(&commit, changes).await.unwrap();
        let cases = [
            (
                "MATCH walk=(a)-[:R*1..2]->(b) FOR SYSTEM_TIME AS OF SEQ $at \
                 UNWIND nodes(walk) AS item RETURN item.p AS value ORDER BY value",
                vec![10, 10, 20, 20, 20, 30, 30],
                vec![10, 10, 21, 21, 21, 40, 40],
            ),
            (
                "MATCH walk=(a)-[:R*1..2]->(b) FOR SYSTEM_TIME AS OF SEQ $at \
                 UNWIND relationships(walk) AS item RETURN item.p AS value ORDER BY value",
                vec![100, 100, 200, 200],
                vec![101, 101, 300, 300],
            ),
            (
                "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at UNWIND [a,a] AS carried \
                 CALL (carried) { OPTIONAL MATCH (carried)-[:R]->(next) \
                 RETURN coalesce(next.p,-1) AS neighbor } \
                 RETURN carried.p*100+neighbor AS value ORDER BY value",
                vec![1020, 1020, 2030, 2030, 2999, 2999],
                vec![1021, 1021, 2140, 2140, 2999, 2999, 3999, 3999],
            ),
        ];
        for (text, old, new) in &cases {
            let template = PreparedTemporalGraphSetText::prepare(text, symbols).unwrap();
            for (seq, expected) in [(before, old), (after, new)] {
                let parameters = GqlParameters::new().with_uint64("at", seq.0).unwrap();
                let bound = template.bind_parameters(&parameters).unwrap();
                let found = db
                    .execute_temporal_graph_set_text_governed(&query, &bound, policy())
                    .unwrap();
                assert_eq!(ints(&found), *expected, "{text} at {seq:?}");
                let QueryResult::Rows { columns, rows } = db
                    .query(&query, text, &parameters, symbols, policy())
                    .unwrap()
                else {
                    panic!("a temporal UNWIND is a read");
                };
                assert_eq!(columns, ["value"]);
                assert_eq!(
                    rows,
                    expected
                        .iter()
                        .map(|value| vec![QueryValue::Value(GraphValue::Scalar(
                            CanonicalScalar::Int(*value)
                        ))])
                        .collect::<Vec<_>>(),
                    "native query dispatch: {text} at {seq:?}",
                );
            }
            let old_bound = template
                .bind_parameters(&GqlParameters::new().with_uint64("at", before.0).unwrap())
                .unwrap();
            assert_eq!(
                ints(
                    &pinned
                        .execute_temporal_graph_set_text_governed(&query, &old_bound, policy())
                        .unwrap()
                ),
                *old,
            );
            let new_bound = template
                .bind_parameters(&GqlParameters::new().with_uint64("at", after.0).unwrap())
                .unwrap();
            assert!(matches!(
                pinned.execute_temporal_graph_set_text_governed(&query, &new_bound, policy()),
                Err(fgdb_gql::GqlQueryError::Source(
                    fgdb_gql::GraphSetExecutionError::Source(fgdb::GqlError::Read(
                        fgdb::ReadError::BeyondFrontier { .. }
                    ))
                ))
            ));
        }

        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (text, old, new) in &cases {
            let template = PreparedTemporalGraphSetText::prepare(text, symbols).unwrap();
            for (seq, expected) in [(before, old), (after, new)] {
                let bound = template
                    .bind_parameters(&GqlParameters::new().with_uint64("at", seq.0).unwrap())
                    .unwrap();
                assert_eq!(
                    ints(
                        &db.execute_temporal_graph_set_text_governed(&query, &bound, policy())
                            .unwrap()
                    ),
                    *expected,
                    "compacted and reopened: {text} at {seq:?}",
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_unwind_rebinds_lists_and_windows_without_skipping_snapshot_admission() {
    let ((), report) = run_async_under_lab(0x7e45_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(10))]);
        first.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
        let before = db.write(&commit, first).await.unwrap();
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(11)));
        let after = db.write(&commit, changes).await.unwrap();
        let template = PreparedTemporalGraphSetText::prepare_with_parameter_types(
            "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at UNWIND $copies AS copy \
             RETURN a.p+copy AS value ORDER BY value SKIP $skip LIMIT $take",
            &[("copies", GqlParameterType::List)],
            symbols,
        )
        .unwrap();
        let parameters = |seq: CommitSeq, copies: &[i64], skip, take| {
            GqlParameters::new()
                .with_uint64("at", seq.0)
                .unwrap()
                .with_list(
                    "copies",
                    copies
                        .iter()
                        .map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value)))
                        .collect(),
                )
                .unwrap()
                .with_uint64("skip", skip)
                .unwrap()
                .with_uint64("take", take)
                .unwrap()
        };
        for (seq, copies, skip, take, expected) in [
            (before, vec![0, 0, 100], 1, 4, vec![10, 20, 20, 110]),
            (after, vec![0, 0, 100], 1, 4, vec![11, 20, 20, 111]),
            (before, vec![1], 0, 10, vec![11, 21]),
            (before, vec![], 0, 10, vec![]),
            (before, vec![0], 0, 0, vec![]),
        ] {
            let bound = template
                .bind_parameters(&parameters(seq, &copies, skip, take))
                .unwrap();
            assert_eq!(
                ints(
                    &db.execute_temporal_graph_set_text_governed(&query, &bound, policy())
                        .unwrap()
                ),
                expected,
            );
        }
        // An empty expansion or LIMIT 0 cannot admit a nonexistent revision.
        for (copies, take) in [(vec![], 10), (vec![0], 0)] {
            let bound = template
                .bind_parameters(&parameters(CommitSeq(after.0 + 1), &copies, 0, take))
                .unwrap();
            assert!(matches!(
                db.execute_temporal_graph_set_text_governed(&query, &bound, policy()),
                Err(fgdb_gql::GqlQueryError::Source(
                    fgdb_gql::GraphSetExecutionError::Source(fgdb::GqlError::Read(
                        fgdb::ReadError::BeyondFrontier { .. }
                    ))
                ))
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn union_uses_one_historical_sequence_for_every_operand_and_survives_reopen() {
    let ((), report) = run_async_under_lab(0x7e45_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();

        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        first.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
        let seq1 = db.write(&commit, first).await.unwrap();

        let mut second = WriteBatch::new(R);
        second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(3)));
        second.create_vertex(VId(3), vec![], vec![(P, CanonicalScalar::Int(4))]);
        let seq2 = db.write(&commit, second).await.unwrap();
        let pinned = db.read_session().unwrap();

        let mut third = WriteBatch::new(R);
        third.delete_vertex(VId(2));
        let seq3 = db.write(&commit, third).await.unwrap();

        let template = PreparedTemporalGraphSetText::prepare(
            "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at WHERE a.p >= 2 RETURN a.p*2 AS p UNION DISTINCT MATCH (b) WHERE b.p <= 3 RETURN b.p*2 AS p ORDER BY p",
            symbols,
        ).unwrap();
        let bind = |seq: CommitSeq| {
            template
                .bind_parameters(&GqlParameters::new().with_uint64("at", seq.0).unwrap())
                .unwrap()
        };
        for (seq, expected) in [
            (seq1, vec![2, 4]),
            (seq2, vec![4, 6, 8]),
            (seq3, vec![6, 8]),
        ] {
            let bound = bind(seq);
            let temporal = db
                .execute_temporal_graph_set_text_governed(&query, &bound, policy())
                .unwrap();
            let explicit = db
                .execute_graph_set_governed_at(&query, bound.query(), seq, policy())
                .unwrap();
            assert_eq!(temporal, explicit);
            assert_eq!(ints(&temporal), expected, "set history mismatch at {seq:?}");
        }

        assert_eq!(
            ints(
                &pinned
                    .execute_temporal_graph_set_text_governed(&query, &bind(seq1), policy())
                    .unwrap()
            ),
            vec![2, 4]
        );
        assert_eq!(
            ints(
                &pinned
                    .execute_temporal_graph_set_text_governed(&query, &bind(seq2), policy())
                    .unwrap()
            ),
            vec![4, 6, 8]
        );
        assert!(
            pinned
                .execute_temporal_graph_set_text_governed(&query, &bind(seq3), policy())
                .is_err(),
            "pinned set view cannot read its future"
        );

        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (seq, expected) in [
            (seq1, vec![2, 4]),
            (seq2, vec![4, 6, 8]),
            (seq3, vec![6, 8]),
        ] {
            assert_eq!(
                ints(
                    &db.execute_temporal_graph_set_text_governed(&query, &bind(seq), policy())
                        .unwrap()
                ),
                expected
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn except_and_intersect_preserve_operand_execution_at_selected_snapshot() {
    let ((), report) = run_async_under_lab(0x7e45_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, value) in [(1, 1), (2, 2), (3, 3), (4, 4)] {
            batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
        }
        let seq = db.write(&commit, batch).await.unwrap();
        // INTERSECT binds more tightly than EXCEPT in the shared set parser, so
        // this is (A INTERSECT B) EXCEPT C without hiding the temporal selector
        // inside a parenthesized nested scope.
        let template = PreparedTemporalGraphSetText::prepare(
            "MATCH (a) FOR SYSTEM_TIME AS OF SEQ $at WHERE a.p >= 2 RETURN a.p AS p INTERSECT MATCH (b) WHERE b.p <= 3 RETURN b.p AS p EXCEPT MATCH (c) WHERE c.p = 2 RETURN c.p AS p ORDER BY p",
            symbols,
        ).unwrap();
        let bound = template
            .bind_parameters(&GqlParameters::new().with_uint64("at", seq.0).unwrap())
            .unwrap();
        assert_eq!(
            ints(
                &db.execute_temporal_graph_set_text_governed(&query, &bound, policy())
                    .unwrap()
            ),
            vec![3]
        );
        let future = template
            .bind_parameters(&GqlParameters::new().with_uint64("at", seq.0 + 1).unwrap())
            .unwrap();
        assert!(
            db.execute_temporal_graph_set_text_governed(&query, &future, policy())
                .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
