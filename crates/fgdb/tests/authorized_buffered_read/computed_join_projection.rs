//! Independent output reservations over real cold fixed-hop sources.

use super::*;

#[test]
fn computed_join_outputs_keep_copied_ancestor_and_child_payloads_after_backtracking() {
    let ((), report) = run_async_under_lab(0xa6_0330, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, _, _) = fixed_join_fixture(&commit, false).await;
        let payload = CanonicalScalar::bytes(vec![0x67; 4096]).unwrap();
        let mut edit = WriteBatch::new(R);
        for eid in [EId(101), EId(102), EId(105)] {
            edit.set_edge_property(eid, P, Some(payload.clone()));
        }
        let at = db.write(&commit, edit).await.unwrap();
        let pool = MemoryPool::new(64 * 1024 * 1024, 0).unwrap();
        let authority = issuer();
        let token = authority.issue_at(&grant(100), 100).unwrap();
        let mut reader = Database::open_buffered_publication_with_vfs(
            &commit,
            vfs.clone(),
            vfs.memory.database_dir(),
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap()
        .into_authorized_read_session(
            &commit,
            &cx,
            &authority,
            &token,
            "main",
            at,
            GqlQueryPolicy::new(1000, 10, 1_000_000, 1_000_000),
            || 100,
        )
        .await
        .unwrap();
        let args = GqlParameters::new();
        let text = format!(
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,[{}] AS copies LIMIT 2",
            vec!["r.score,s.score"; 32].join(",")
        );
        let prepared = reader.prepare(&cx, &text, &args).unwrap();
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        assert_eq!(cursor.columns(), &["r", "s", "copies"]);
        let first = cursor.next().await.unwrap().unwrap();
        // Advancing replaces the first child with its next indexed sibling.
        // The copied payload must remain owned by the already-delivered row.
        let second = cursor.next().await.unwrap().unwrap();
        for (row, child) in [(&first, EId(102)), (&second, EId(105))] {
            assert_eq!(
                row.values(),
                &[
                    GraphValue::Edge(EId(101)),
                    GraphValue::Edge(child),
                    GraphValue::List(
                        vec![GraphValue::Scalar(payload.clone()); 64].into_boxed_slice()
                    ),
                ]
            );
        }
        cursor.close();
        drop(cursor);
        drop(reader);
        // The native row has only two identities and two property columns.
        // Its raw copy bound cannot cover these 128 distinct payload copies.
        let both = pool.used();
        assert!(both >= 128 * 4096, "both outputs retain full VM admission");
        drop(first);
        assert!(pool.used() < both, "join outputs have independent owners");
        assert!(
            pool.used() >= 64 * 4096,
            "the second row keeps its own charge"
        );
        drop(second);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_join_host_memory_refusal_releases_the_frame_and_permits_reuse() {
    let ((), report) = run_async_under_lab(0xa6_0331, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _db, _, at) = fixed_join_fixture(&commit, false).await;
        let pool = memory();
        let authority = issuer();
        let token = authority.issue_at(&grant(100), 100).unwrap();
        let mut reader = Database::open_buffered_publication_with_vfs(
            &commit,
            vfs.clone(),
            vfs.memory.database_dir(),
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap()
        .into_authorized_read_session(
            &commit,
            &cx,
            &authority,
            &token,
            "main",
            at,
            GqlQueryPolicy::new(1000, 10, 1_000_000, 1_000_000),
            || 100,
        )
        .await
        .unwrap();
        let payload = CanonicalScalar::bytes(vec![0x68; 32 * 1024]).unwrap();
        let args = GqlParameters::new()
            .with_scalar("payload", payload.clone())
            .unwrap();
        let prepared = reader
            .prepare(
                &cx,
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,{first:$payload,second:$payload} AS copies LIMIT 1",
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
                GraphValue::Edge(EId(102)),
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
