//! Temporal aggregate text runs against the same retained Chronicle/Strata
//! generations as explicit aggregate `_at` execution. Computed inputs are used
//! deliberately so this covers the real arithmetic/grouping path, not COUNT only.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
    PreparedTemporalGraphAggregateText,
};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 4_000_000, 4_000_000)
}
fn values(result: &fgdb_gql::GqlQueryExecution<fgdb_gql::GraphAggregateRow>) -> (u64, i128) {
    assert_eq!(result.value.len(), 1);
    let values = result.value[0].values();
    let count = values[0].as_count().expect("COUNT(*) result");
    let sum = values[1].as_integer().expect("SUM integer result");
    (count, sum)
}

#[test]
fn temporal_count_and_computed_sum_survive_compaction_reopen_and_pinning() {
    let ((), report) = run_async_under_lab(0x7e4a_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();

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

        let template = PreparedTemporalGraphAggregateText::prepare(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN COUNT(*) AS c,SUM(n.p*2) AS s",
            symbols,
        ).unwrap();
        let bind = |seq: CommitSeq| template.bind_parameters(
            &GqlParameters::new().with_uint64("at", seq.0).unwrap(),
        ).unwrap();

        for (seq, expected) in [
            (seq1, (2, 6)),
            (seq2, (3, 18)),
            (seq3, (2, 14)),
        ] {
            let bound = bind(seq);
            let temporal = db.execute_temporal_graph_aggregate_text_governed(&query, &bound, policy()).unwrap();
            let explicit = db.execute_graph_aggregate_governed_at(
                &query, bound.aggregate(), seq, policy(),
            ).unwrap();
            assert_eq!(temporal, explicit);
            assert_eq!(values(&temporal), expected, "aggregate history mismatch at {seq:?}");
        }

        assert_eq!(values(&pinned.execute_temporal_graph_aggregate_text_governed(
            &query, &bind(seq1), policy()).unwrap()), (2, 6));
        assert_eq!(values(&pinned.execute_temporal_graph_aggregate_text_governed(
            &query, &bind(seq2), policy()).unwrap()), (3, 18));
        assert!(pinned.execute_temporal_graph_aggregate_text_governed(
            &query, &bind(seq3), policy()).is_err(), "pinned view cannot read its future");

        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        for (seq, expected) in [(seq1, (2, 6)), (seq2, (3, 18)), (seq3, (2, 14))] {
            assert_eq!(values(&db.execute_temporal_graph_aggregate_text_governed(
                &query, &bind(seq), policy()).unwrap()), expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_grouping_and_having_rebind_snapshot_without_recompilation() {
    let ((), report) = run_async_under_lab(0x7e4a_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, value) in [(1, -2), (2, 2), (3, 5)] {
            batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
        }
        let seq = db.write(&commit, batch).await.unwrap();
        let template = PreparedTemporalGraphAggregateText::prepare(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN ABS(n.p) AS bucket,COUNT(*) AS c GROUP BY ABS(n.p) HAVING c >= $min ORDER BY bucket",
            symbols,
        ).unwrap();
        let args = GqlParameters::new().with_uint64("at", seq.0).unwrap().with_int64("min", 2).unwrap();
        let bound = template.bind_parameters(&args).unwrap();
        let result = db.execute_temporal_graph_aggregate_text_governed(&query, &bound, policy()).unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].keys()[0].as_scalar(), Some(&CanonicalScalar::Int(2)));
        assert!(matches!(result.value[0].values()[0], GraphAggregateValue::Count(2)));
        let future = template.bind_parameters(&GqlParameters::new()
            .with_uint64("at", seq.0 + 1).unwrap().with_int64("min", 2).unwrap()).unwrap();
        assert!(db.execute_temporal_graph_aggregate_text_governed(&query, &future, policy()).is_err());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
