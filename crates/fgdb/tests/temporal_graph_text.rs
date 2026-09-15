//! Temporal text selects the real retained snapshot generation. These tests use
//! Chronicle/Strata history and compare the text wrapper with the explicit `_at`
//! executor; no temporal graph model or rewritten result oracle is substituted.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    BoundTemporalGraphQuery, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol,
    GraphSymbolKind, PreparedTemporalGraphText,
};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000)
}
fn ints(result: &fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>) -> Vec<i64> {
    result.value.iter().map(|row| match row.values()[0].as_scalar() {
        Some(CanonicalScalar::Int(value)) => *value,
        other => panic!("expected integer temporal value, got {other:?}"),
    }).collect()
}
fn bind(template: &PreparedTemporalGraphText, seq: CommitSeq) -> BoundTemporalGraphQuery {
    template.bind_parameters(&GqlParameters::new().with_uint64("at", seq.0).unwrap()).unwrap()
}

#[test]
fn temporal_text_reads_each_retained_generation_and_matches_explicit_as_of() {
    let ((), report) = run_async_under_lab(0x7e4d_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();

        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(10))]);
        first.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
        let seq1 = db.write(&commit, first).await.unwrap();

        let mut second = WriteBatch::new(R);
        second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(11)));
        second.create_vertex(VId(3), vec![], vec![(P, CanonicalScalar::Int(30))]);
        let seq2 = db.write(&commit, second).await.unwrap();
        let pinned = db.read_session().unwrap();
        assert_eq!(pinned.frontier(), seq2);

        let mut third = WriteBatch::new(R);
        third.delete_vertex(VId(2));
        let seq3 = db.write(&commit, third).await.unwrap();

        let template = PreparedTemporalGraphText::prepare(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p ORDER BY p", symbols,
        ).unwrap();
        for (seq, expected) in [
            (seq1, vec![10, 20]),
            (seq2, vec![11, 20, 30]),
            (seq3, vec![11, 30]),
        ] {
            let bound = bind(&template, seq);
            let via_text = db.execute_temporal_graph_text_governed(&query, &bound, policy()).unwrap();
            let explicit = db.execute_graph_pattern_governed_at(
                &query, bound.pattern(), seq, policy(),
            ).unwrap();
            assert_eq!(via_text, explicit, "text selector must be only an as_of binding");
            assert_eq!(ints(&via_text), expected, "history mismatch at {seq:?}");
        }

        let at_seq1 = bind(&template, seq1);
        assert_eq!(ints(&pinned.execute_temporal_graph_text_governed(&query, &at_seq1, policy()).unwrap()), vec![10, 20]);
        let at_seq2 = bind(&template, seq2);
        assert_eq!(ints(&pinned.execute_temporal_graph_text_governed(&query, &at_seq2, policy()).unwrap()), vec![11, 20, 30]);
        let at_seq3 = bind(&template, seq3);
        assert!(pinned.execute_temporal_graph_text_governed(&query, &at_seq3, policy()).is_err(),
            "a pinned view cannot time-travel beyond its admitted frontier");

        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        for (seq, expected) in [(seq1, vec![10, 20]), (seq2, vec![11, 20, 30]), (seq3, vec![11, 30])] {
            let bound = bind(&template, seq);
            assert_eq!(ints(&db.execute_temporal_graph_text_governed(&query, &bound, policy()).unwrap()), expected,
                "compaction/reopen changed temporal answer at {seq:?}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn future_sequence_refuses_before_result_release_and_parameters_rebind_without_catalog_access() {
    let ((), report) = run_async_under_lab(0x7e4d_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(7))]);
        let live = db.write(&commit, batch).await.unwrap();

        let mut resolutions = 0;
        let template = PreparedTemporalGraphText::prepare(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WHERE n.p >= $floor RETURN n.p AS p",
            |kind, name| { resolutions += 1; symbols(kind, name) },
        ).unwrap();
        assert_eq!(resolutions, 1);
        let args = |at| GqlParameters::new().with_uint64("at", at).unwrap().with_int64("floor", 0).unwrap();
        let current = template.bind_parameters(&args(live.0)).unwrap();
        assert_eq!(ints(&db.execute_temporal_graph_text_governed(&query, &current, policy()).unwrap()), vec![7]);
        let future = template.bind_parameters(&args(live.0 + 1)).unwrap();
        let failed = db.execute_temporal_graph_text_governed(&query, &future, policy());
        assert!(matches!(failed, Err(GqlQueryError::Source(GqlError::Read(_)))));
        assert_eq!(resolutions, 1, "rebinding temporal parameters must not re-enter the catalog");
        assert_ne!(current.canonical_bytes(), future.canonical_bytes());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
