//! Compound temporal queries must pin every operand to one exact historical
//! sequence. Computed projections make a live-frontier fallback observable even
//! when both branches happen to return the same vertex identities.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedTemporalGraphSetText,
};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

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
