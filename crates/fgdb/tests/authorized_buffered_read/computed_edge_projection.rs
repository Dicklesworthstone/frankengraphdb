//! Real cold storage and host reservations for computed edge rows.

use super::*;

#[test]
fn computed_undirected_rows_retain_vm_memory_after_the_shared_record_is_released() {
    let ((), report) = run_async_under_lab(0xa6_0320, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, _, _) = fixture(&commit, false).await;
        let payload = CanonicalScalar::bytes(vec![0x65; 4096]).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(EId(101), P, Some(payload.clone()));
        let at = db.write(&commit, edit).await.unwrap();
        // This positive case retains both orientations' cumulative VM scratch;
        // the separate refusal case below constrains available host memory.
        let pool = MemoryPool::new(64 * 1024 * 1024, 0).unwrap();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let mut reader = session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
        let args = GqlParameters::new();
        let text = format!(
            "MATCH (a)-[r:R]-(b) RETURN r,a,[{}] AS copies",
            vec!["r.score"; 64].join(",")
        );
        let prepared = reader.prepare(&cx, &text, &args).unwrap();
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        assert_eq!(cursor.columns(), &["r", "a", "copies"]);
        let first = cursor.next().await.unwrap().unwrap();
        let second = cursor.next().await.unwrap().unwrap();
        for (row, from) in [(&first, VId(1)), (&second, VId(2))] {
            assert_eq!(
                row.values(),
                &[
                    GraphValue::Edge(EId(101)),
                    GraphValue::Vertex(from),
                    GraphValue::List(
                        vec![GraphValue::Scalar(payload.clone()); 64].into_boxed_slice()
                    ),
                ]
            );
        }
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        drop(cursor);
        drop(reader);
        // The native source deduplicates r.score to one property column; its
        // raw reservations cannot cover these 128 independently owned copies.
        assert!(
            pool.used() >= 128 * 4096,
            "both orientation outputs retain VM admission"
        );
        drop(first);
        assert!(
            pool.used() >= 64 * 4096,
            "the second output still owns the shared growing charge"
        );
        drop(second);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_cold_edge_memory_refusal_is_typed_fused_and_reusable() {
    let ((), report) = run_async_under_lab(0xa6_0321, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _db, _, at) = fixture(&commit, false).await;
        let pool = memory();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let mut reader = session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
        let payload = CanonicalScalar::bytes(vec![0x66; 32 * 1024]).unwrap();
        let args = GqlParameters::new()
            .with_scalar("payload", payload.clone())
            .unwrap();
        let prepared = reader
            .prepare(
                &cx,
                "MATCH (a)-[r:R]->(b) RETURN r,{first:$payload,second:$payload} AS copies LIMIT 1",
                &args,
            )
            .unwrap();
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        let occupied = pool.reserve(&cx, pool.available() - 96 * 1024).unwrap();
        assert!(matches!(
            cursor.next().await.unwrap(),
            Err(AuthorizedBufferedError::Read(
                fgdb::BufferedReadError::Memory(_)
            ))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(occupied);
        assert!(!reader.is_closed());
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        assert_eq!(
            held.values(),
            &[
                GraphValue::Edge(EId(101)),
                GraphValue::map(vec![
                    ("first".into(), GraphValue::Scalar(payload.clone())),
                    ("second".into(), GraphValue::Scalar(payload)),
                ])
                .unwrap(),
            ]
        );
        cursor.close();
        drop(cursor);
        drop(reader);
        assert!(pool.used() >= 64 * 1024);
        drop(held);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
