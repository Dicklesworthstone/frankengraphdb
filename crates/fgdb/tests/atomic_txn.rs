//! Staged multi-relation writes share the ordinary transaction owner and commit.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, RelationBind, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::GlaExecutionLimits;
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32]);
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut seed = WriteBatch::new(R);
    for id in 1..=3 {
        seed.create_vertex(VId(id), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(0))]);
    }
    db.write(cx, seed).await.unwrap();
    db
}

fn path() -> Vec<WriteBatch> {
    let mut r = WriteBatch::new(R);
    r.add_edge(EId(10), VId(1), VId(2), vec![]);
    let mut s = WriteBatch::new(S);
    s.add_edge(EId(20), VId(2), VId(3), vec![]);
    vec![s, r]
}

#[test]
fn staged_path_is_visible_only_to_owner_until_one_atomic_commit() {
    let ((), report) = run_async_under_lab(0xa702_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let baseline = txn_cx.outstanding_obligations();
        let mut db = seeded(&cx).await;
        let before = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        txn.write_atomic(&mut db, path()).unwrap();
        let bind = RelationBind::new().with_relation("R", R).with_relation("S", S);
        let query = txn.prepare_gql_query("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c", &bind).unwrap();
        assert_eq!(txn.execute_prepared_query(&db, &query).unwrap(), vec![VId(3)]);
        assert_eq!(txn.execute_prepared_query_limited(&db, &query,
            GlaExecutionLimits::new(1000, 1000)).unwrap().value, vec![VId(3)]);
        assert!(db.execute_prepared_query(&query).unwrap().is_empty());
        assert!(pinned.execute_prepared_query(&query).unwrap().is_empty());
        let artifact = txn.execute_prepared_query_overlay_artifact(&db, &query).unwrap();
        txn.audit_prepared_query_overlay_artifact(&db, &query, &artifact.to_bytes()).unwrap();

        // Ordinary staging after explicit multi-relation admission must not
        // accidentally concatenate all rows under the first relation.
        let mut append = WriteBatch::new(R);
        append.add_edge(EId(11), VId(3), VId(2), vec![]);
        txn.write(&mut db, append).unwrap();
        assert_eq!(txn.edge(&db, EId(11)).unwrap().unwrap().entry.relation, R);
        assert!(txn.audit_prepared_query_overlay_artifact(&db, &query, &artifact.to_bytes()).is_err());
        let seq = txn.commit(&mut db, &cx).await.unwrap();
        assert_eq!(seq, CommitSeq(before.0 + 1));
        assert_eq!(txn_cx.outstanding_obligations(), baseline);
        assert_eq!(db.delta_since(before).unwrap().count(), 1);
        assert_eq!(db.edges().unwrap().len(), 3);
        assert_eq!(db.execute_prepared_query(&query).unwrap(), vec![VId(3)]);
        assert!(pinned.execute_prepared_query(&query).unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn point_bulk_and_committed_vertices_use_the_same_canonical_births_and_values() {
    let ((), report) = run_async_under_lab(0xa702_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut s = WriteBatch::new(S);
        s.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Int(5))]);
        s.add_edge(EId(50), VId(5), VId(3), vec![]);
        txn.write_atomic(&mut db, vec![s]).unwrap();
        let mut r = WriteBatch::new(R);
        r.ensure_vertex(VId(1), vec![], vec![]); // No-op still consumes a visit.
        r.create_vertex(VId(4), vec![], vec![(P, CanonicalScalar::Int(4))]);
        r.add_edge(EId(40), VId(4), VId(1), vec![]);
        txn.write_atomic(&mut db, vec![r]).unwrap();
        let four = txn.vertex(&db, VId(4)).unwrap().unwrap();
        let five = txn.vertex(&db, VId(5)).unwrap().unwrap();
        assert_eq!(four.birth_ordinal, 2);
        assert_eq!(five.birth_ordinal, 4);
        let bulk = txn.vertices(&db).unwrap();
        assert_eq!(bulk.iter().find(|row| row.vid == VId(4)), Some(&four));
        assert_eq!(bulk.iter().find(|row| row.vid == VId(5)), Some(&five));
        txn.commit(&mut db, &cx).await.unwrap();
        for expected in [four, five] {
            let actual = db.vertex(expected.vid).unwrap().unwrap();
            assert_eq!(actual.birth_ordinal, expected.birth_ordinal);
            assert_eq!(actual.props, expected.props);
            assert_eq!(actual.labels, expected.labels);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_atomic_staging_preserves_prior_effects_and_evidence() {
    let ((), report) = run_async_under_lab(0xa702_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut foreign = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).unwrap();
        txn.write_atomic(&mut db, path()).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        assert!(matches!(txn.write_atomic(&mut foreign, vec![]), Err(WriteTxnError::WrongDatabase)));
        for dependent in [false, true] {
            let mut valid = WriteBatch::new(R);
            valid.create_vertex(VId(99), vec![], vec![]);
            let mut invalid = WriteBatch::new(S);
            if dependent {
                invalid.delete_vertex(VId(1)); // R observes this endpoint.
            } else {
                invalid.add_edge(EId(99), VId(2), VId(999), vec![]);
            }
            let result = txn.write_atomic(&mut db, vec![valid, invalid]);
            if dependent {
                assert!(matches!(result, Err(WriteTxnError::AtomicRelationConflict { .. })));
            } else {
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::DanglingEndpoint { .. }))));
            }
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
            assert!(txn.vertex(&db, VId(99)).unwrap().is_none());
            assert_eq!(txn.edges(&db).unwrap().len(), 2);
        }
        txn.commit(&mut db, &cx).await.unwrap();
        assert_eq!(db.edges().unwrap().len(), 2);
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert!(foreign.edges().unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn legacy_relation_refusal_and_atomic_owner_lifecycle_remain_explicit() {
    let ((), report) = run_async_under_lab(0xa702_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut r = WriteBatch::new(R);
        r.add_edge(EId(10), VId(1), VId(2), vec![]);
        txn.write(&mut db, r).unwrap();
        let mut s = WriteBatch::new(S);
        s.add_edge(EId(20), VId(2), VId(3), vec![]);
        assert!(matches!(txn.write(&mut db, s.clone()), Err(WriteTxnError::RelationMismatch { .. })));
        txn.write_atomic(&mut db, vec![s]).unwrap();
        let mut unrelated = WriteBatch::new(R);
        unrelated.create_vertex(VId(77), vec![], vec![]);
        db.write(&cx, unrelated).await.unwrap();
        assert!(matches!(txn.write_atomic(&mut db, path()), Err(WriteTxnError::SnapshotAdvanced { .. })));
        txn.commit(&mut db, &cx).await.unwrap();
        assert!(matches!(txn.write_atomic(&mut db, path()), Err(WriteTxnError::Finished)));
        assert_eq!(db.edges().unwrap().len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
