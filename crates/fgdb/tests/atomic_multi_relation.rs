//! Multi-relation atomic groups must publish together, or not at all.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, RelationBind, WriteBatch, WriteError, WriteMismatchPolicy,
    WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.expect("database");
    let mut seed = WriteBatch::new(R);
    for vid in [VId(1), VId(2), VId(3), VId(8)] {
        seed.create_vertex(vid, vec![], vec![(P, CanonicalScalar::Int(0))]);
    }
    db.write(cx, seed).await.expect("seed");
    db
}

fn edges() -> Vec<WriteBatch> {
    let mut first = WriteBatch::new(R);
    first.add_edge(EId(10), VId(1), VId(2), vec![]);
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(11), VId(2), VId(3), vec![]);
    // Arrival order is deliberately not canonical coordinate order.
    vec![second, first]
}

#[test]
fn two_relation_groups_publish_one_sequence_and_one_delta() {
    let ((), report) = run_async_under_lab(0xa701_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let seq = db.write_atomic(&cx, edges()).await.expect("atomic write");
        assert_eq!(seq, CommitSeq(basis.0 + 1));
        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_relation("S", S);
        let query = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
        assert_eq!(db.execute_gql(query, &bind).unwrap(), vec![VId(3)]);
        assert!(pinned.execute_gql(query, &bind).unwrap().is_empty());
        assert!(db.execute_gql_at(query, &bind, basis).unwrap().is_empty());
        let batches: Vec<_> = db.delta_since(basis).unwrap().collect();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0]
                .coordinate_entries()
                .iter()
                .map(|c| c.relation)
                .collect::<Vec<_>>(),
            vec![R, S]
        );
        assert_eq!(db.edge(EId(10)).unwrap().unwrap().entry.created_at, seq);
        assert_eq!(db.edge(EId(11)).unwrap().unwrap().entry.created_at, seq);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn per_relation_prefixes_are_preserved_and_birth_ordinals_do_not_collide() {
    let ((), report) = run_async_under_lab(0xa701_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut r1 = WriteBatch::new(R);
        r1.create_vertex(VId(4), vec![], vec![(P, CanonicalScalar::Int(1))]);
        let mut s = WriteBatch::new(S);
        s.create_vertex(VId(5), vec![], vec![]);
        s.add_edge(EId(50), VId(5), VId(3), vec![]);
        let mut r2 = WriteBatch::new(R);
        r2.compare_and_set_vertex_property(
            VId(4),
            P,
            Some(CanonicalScalar::Int(1)),
            CanonicalScalar::Int(2),
            WriteMismatchPolicy::AbortWrite,
        );
        r2.add_edge(EId(40), VId(4), VId(1), vec![]);
        db.write_atomic(&cx, vec![s, r1, r2])
            .await
            .expect("independent groups with ordered prefixes");
        let four = db.vertex(VId(4)).unwrap().unwrap();
        let five = db.vertex(VId(5)).unwrap().unwrap();
        assert_eq!(four.props, vec![(P, CanonicalScalar::Int(2))]);
        assert_eq!(four.birth_ordinal, 1);
        assert_eq!(five.birth_ordinal, 4);
        assert_eq!(db.neighbours(VId(4), R).unwrap(), vec![VId(1)]);
        assert_eq!(db.neighbours(VId(5), S).unwrap(), vec![VId(3)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_or_dependent_group_never_publishes_a_valid_prefix() {
    let ((), report) = run_async_under_lab(0xa701_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for mode in 0..3 {
            let mut db = seeded(&cx).await;
            let before = db.frontier().unwrap();
            let mut r = WriteBatch::new(R);
            let mut s = WriteBatch::new(S);
            match mode {
                0 => {
                    r.add_edge(EId(10), VId(1), VId(2), vec![]);
                    s.add_edge(EId(11), VId(2), VId(999), vec![]);
                }
                1 => {
                    r.delete_vertex(VId(1));
                    s.add_edge(EId(11), VId(2), VId(1), vec![]);
                }
                _ => {
                    r.compare_and_set_vertex_property(
                        VId(1),
                        P,
                        Some(CanonicalScalar::Int(99)),
                        CanonicalScalar::Int(10),
                        WriteMismatchPolicy::NoOp,
                    );
                    s.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
                }
            }
            let result = db.write_atomic(&cx, vec![r, s]).await;
            if mode == 0 {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::Write(WriteError::DanglingEndpoint { .. }))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::AtomicRelationConflict { .. })
                ));
            }
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(0))]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compound_prepared_write_keeps_all_groups_history_dependencies() {
    let ((), report) = run_async_under_lab(0xa701_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for conflicts in [false, true] {
            let mut db = seeded(&cx).await;
            let mut r = WriteBatch::new(R);
            r.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(10)));
            let mut s = WriteBatch::new(S);
            s.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(20)));
            let prepared = db.prepare_atomic_writes(vec![r, s]).unwrap();
            if conflicts {
                let mut winner = WriteBatch::new(S);
                winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(99)));
                db.write(&cx, winner).await.unwrap();
            }
            for value in 1..=2 {
                let mut unrelated = WriteBatch::new(R);
                unrelated.set_vertex_property(VId(8), P, Some(CanonicalScalar::Int(value)));
                db.write(&cx, unrelated).await.unwrap();
            }
            let frontier = db.frontier().unwrap();
            let result = db.commit_prepared(&cx, prepared).await;
            if conflicts {
                assert!(matches!(result, Err(WriteError::FirstCommitterWins { .. })));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!(
                    db.vertex(VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(0))]
                );
                assert_eq!(
                    db.vertex(VId(2)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(99))]
                );
            } else {
                result.expect("disjoint history permits the whole compound write");
                assert_eq!(
                    db.vertex(VId(1)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(10))]
                );
                assert_eq!(
                    db.vertex(VId(2)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(20))]
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_groups_and_foreign_prepared_owners_are_refused() {
    let ((), report) = run_async_under_lab(0xa701_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut foreign = seeded(&cx).await;
        assert!(matches!(
            db.prepare_atomic_writes(vec![]),
            Err(WriteTxnError::Write(WriteError::EmptyBatch))
        ));
        let mut groups = edges();
        groups.push(WriteBatch::new(RelationId(3)));
        assert!(matches!(
            db.prepare_atomic_writes(groups),
            Err(WriteTxnError::Write(WriteError::EmptyBatch))
        ));
        let prepared = db.prepare_atomic_writes(edges()).unwrap();
        assert!(matches!(
            foreign.commit_prepared(&cx, prepared.clone()).await,
            Err(WriteError::ForeignPreparedWrite)
        ));
        db.commit_prepared(&cx, prepared)
            .await
            .expect("owner still accepts unchanged definition");
        assert!(foreign.edges().unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
