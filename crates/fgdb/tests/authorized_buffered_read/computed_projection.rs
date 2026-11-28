//! Real host-pool obligations of the computed cold vertex profile.

use super::*;

#[test]
fn computed_cold_payloads_keep_their_growing_guard_after_source_and_session_drop() {
    let ((), report) = run_async_under_lab(0xa6_0310, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, _, _) = fixture(&commit, false).await;
        let payload = CanonicalScalar::bytes(vec![0x63; 4096]).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(1), P, Some(payload.clone()));
        let at = db.write(&commit, edit).await.unwrap();
        let pool = memory();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let mut reader = session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
        let args = GqlParameters::new();
        // The native input deduplicates n.score to one property column. More
        // output copies than its raw row reservation can cover require the
        // record's growing VM reservation to survive with the returned row.
        let text = format!(
            "MATCH (n) RETURN n, [{}] AS payload LIMIT 1",
            vec!["n.score"; 64].join(",")
        );
        let prepared = reader.prepare(&cx, &text, &args).unwrap();
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        assert_eq!(cursor.columns(), &["n", "payload"]);
        assert_eq!(
            cursor.state(),
            VertexScanState::Open,
            "the suffix is still unevaluated"
        );
        cursor.close();
        drop(cursor);
        assert!(!reader.is_closed());
        drop(reader);
        assert_eq!(
            held.values(),
            &[
                GraphValue::Vertex(VId(1)),
                GraphValue::List(vec![GraphValue::Scalar(payload); 64].into_boxed_slice()),
            ]
        );
        assert!(
            pool.used() >= 64 * 4096,
            "all independently owned payloads remain admitted after record and source drop"
        );
        drop(held);
        assert_eq!(
            pool.used(),
            0,
            "the final row refunds its copy and VM reservations"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_computed_bound_payload_refuses_host_memory_and_can_be_reexecuted() {
    let ((), report) = run_async_under_lab(0xa6_0311, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _db, _, at) = fixture(&commit, false).await;
        let pool = memory();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let mut reader = session(&commit, &cx, &vfs, &pool, &authority, &token, at, || 100).await;
        let payload = CanonicalScalar::bytes(vec![0x64; 32 * 1024]).unwrap();
        let args = GqlParameters::new()
            .with_scalar("payload", payload.clone())
            .unwrap();
        let prepared = reader
            .prepare(
                &cx,
                "MATCH (n) RETURN n, {first:$payload,second:$payload} AS payload LIMIT 1",
                &args,
            )
            .unwrap();
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        // Leave ample room for this fixture's small native row and one cold
        // frame. The two large VM values need their own host-pool admission.
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
        assert!(
            !reader.is_closed(),
            "ordinary resource refusal preserves the session"
        );

        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        assert_eq!(
            held.values(),
            &[
                GraphValue::Vertex(VId(1)),
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
