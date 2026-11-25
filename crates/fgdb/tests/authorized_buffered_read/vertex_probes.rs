//! Public cold vertex semi/anti joins, including source and output ownership.

use super::*;
use fgdb_gql::{GqlBudgetDimension, GqlQueryError};

#[allow(clippy::too_many_arguments)]
async fn reader<'a>(
    commit: &CommitCx,
    cx: &QueryCx,
    vfs: &GateVfs,
    pool: &MemoryPool,
    authority: &'a Authority,
    token: &CapabilityToken,
    at: CommitSeq,
    records: u64,
) -> AuthorizedReadSession<
    'a,
    impl GraphSymbolResolver + use<>,
    impl FnMut() -> u64 + Send + use<>,
    AuthorizedBufferedReadSource<GateVfs>,
> {
    Database::open_buffered_publication_with_vfs(
        commit,
        vfs.clone(),
        vfs.memory.database_dir(),
        keys(),
        pool.clone(),
        limits(),
    )
    .await
    .unwrap()
    .into_authorized_read_session(
        commit,
        cx,
        authority,
        token,
        "main",
        at,
        GqlQueryPolicy::new(records, 10, 100_000, 100_000),
        || 100,
    )
    .await
    .unwrap()
}

fn rows(ids: &[u128]) -> Vec<GraphValueRow> {
    ids.iter()
        .map(|&id| GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(id))]))
        .collect()
}

#[test]
fn cold_vertex_probes_preserve_history_directions_masks_and_final_root() {
    let ((), report) = run_async_under_lab(0xa6_0301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let authority = issuer();
        let token = authority.issue_at(&grant(1000), 100).unwrap();
        for hidden in [false, true] {
            let (vfs, mut db, _, _) = fixture(&commit, hidden).await;
            let mut extra = WriteBatch::new(R);
            extra.create_vertex(VId(3), vec![L, H], vec![(P, CanonicalScalar::Int(3))]);
            extra.create_vertex(
                VId(u128::MAX),
                vec![L, H],
                vec![(P, CanonicalScalar::Int(4))],
            );
            extra.add_edge(EId(102), VId(1), VId(2), vec![(P, CanonicalScalar::Int(9))]);
            extra.add_edge(EId(103), VId(2), VId(2), vec![(P, CanonicalScalar::Int(9))]);
            extra.add_edge(
                EId(u128::MAX),
                VId(u128::MAX),
                VId(1),
                vec![(P, CanonicalScalar::Int(8))],
            );
            let old = db.write(&commit, extra).await.unwrap();
            let mut edit = WriteBatch::new(R);
            edit.delete_edge(EId(103));
            edit.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(21)));
            let current = db.write(&commit, edit).await.unwrap();

            for at in [old, current] {
                let pool = memory();
                let mut reader =
                    reader(&commit, &cx, &vfs, &pool, &authority, &token, at, 100).await;
                let first = if at == old { 11 } else { 21 };
                let expected = if at == old { vec![1, 2] } else { vec![1] };
                for relation in ["R", "R|S", ""] {
                    let relation = if relation.is_empty() {
                        String::new()
                    } else {
                        format!(":{relation}")
                    };
                    let query = format!(
                        "MATCH (n) WHERE EXISTS {{ MATCH (n)-[r{relation}]->(m) \
                         WHERE r.score >= 7 AND m.score = 2 \
                         AND r.secret IS NULL AND m.secret IS NULL }} RETURN n"
                    );
                    assert_eq!(collect(&mut reader, &cx, &query).await, rows(&expected));
                }
                let absent = if at == old { vec![3] } else { vec![2, 3] };
                for (text, expected) in [
                    (
                        "MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } RETURN n",
                        absent,
                    ),
                    (
                        "MATCH (n) WHERE EXISTS { MATCH (n)<-[:R]-(m) } RETURN n",
                        vec![1, 2],
                    ),
                    (
                        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]-(m) } RETURN n",
                        vec![1, 2, u128::MAX],
                    ),
                    (
                        "MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:S]->(m) } RETURN n",
                        vec![1, 2, 3, u128::MAX],
                    ),
                ] {
                    assert_eq!(collect(&mut reader, &cx, text).await, rows(&expected));
                }
                // The final root keeps its incidence driver after full-width
                // root enumeration reaches its end.
                assert_eq!(
                    collect(
                        &mut reader,
                        &cx,
                        &format!(
                            "MATCH (n) WHERE EXISTS {{ MATCH (n)-[:R]->(m) \
                             WHERE m.score = {first} }} RETURN n,n.score,n.secret"
                        ),
                    )
                    .await,
                    vec![GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(VId(u128::MAX)),
                        GraphValue::Scalar(CanonicalScalar::Int(4)),
                        GraphValue::Scalar(CanonicalScalar::Null),
                    ])],
                );
                assert!(
                    collect(
                        &mut reader,
                        &cx,
                        "MATCH (n:H) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n",
                    )
                    .await
                    .is_empty()
                );
                drop(reader);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_vertex_probes_share_exact_node_and_record_limits_with_their_witnesses() {
    let ((), report) = run_async_under_lab(0xa6_0302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let authority = issuer();
        let args = GqlParameters::new();
        let text = "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n";
        for hidden in [false, true] {
            let (vfs, _db, at, _) = fixture(&commit, hidden).await;
            // Root 1, its two endpoint witness, and root 2 require four
            // signed nodes and three native records, regardless of hidden data.
            for (nodes, records) in [(3, 3), (4, 2), (4, 3)] {
                let token = authority.issue_at(&grant(nodes), 100).unwrap();
                let pool = memory();
                let mut reader =
                    reader(&commit, &cx, &vfs, &pool, &authority, &token, at, records).await;
                let prepared = reader.prepare(&cx, text, &args).unwrap();
                let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
                assert_eq!(
                    cursor.next().await.unwrap().unwrap().as_ref().values(),
                    &[GraphValue::Vertex(VId(1))],
                );
                let end = cursor.next().await;
                if nodes == 3 {
                    assert!(matches!(
                        end,
                        Some(Err(AuthorizedBufferedError::Query(
                            QueryError::Authorization(Error::LimitExceeded(LimitDimension::Nodes))
                        )))
                    ));
                } else if records == 2 {
                    assert!(matches!(
                        end,
                        Some(Err(AuthorizedBufferedError::Query(QueryError::Stream(
                            GqlQueryError::Rows(error)
                        )))) if error.dimension == GqlBudgetDimension::SnapshotRecords
                    ));
                } else {
                    assert!(end.is_none());
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                }
                assert!(cursor.next().await.is_none());
                drop(cursor);
                assert!(!reader.is_closed());
                assert_eq!(
                    collect(&mut reader, &cx, &format!("{text} LIMIT 1")).await,
                    rows(&[1]),
                );
                drop(reader);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_vertex_anti_probe_source_failures_are_errors_and_pending_drop_closes_owner() {
    let ((), report) = run_async_under_lab(0xa6_0303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _db, at, _) = fixture(&commit, false).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(10), 100).unwrap();
        let pool = memory();
        let mut reader = reader(&commit, &cx, &vfs, &pool, &authority, &token, at, 10).await;
        let args = GqlParameters::new();
        let prepared = reader
            .prepare(
                &cx,
                "MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } RETURN n",
                &args,
            )
            .unwrap();
        let baseline = pool.used();
        vfs.reset();
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        drop(cursor.next());
        cursor.close();
        drop(cursor);
        assert_eq!(vfs.reads.load(Ordering::SeqCst), 0);
        assert_eq!(pool.used(), baseline);
        assert!(!reader.is_closed());

        vfs.refuse.store(true, Ordering::SeqCst);
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        assert!(matches!(
            cursor.next().await.unwrap(),
            Err(AuthorizedBufferedError::Read(_))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert!(!reader.is_closed());
        vfs.refuse.store(false, Ordering::SeqCst);

        vfs.pause.store(true, Ordering::SeqCst);
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        let mut pending = Box::pin(cursor.next());
        poll_fn(|task| {
            assert!(pending.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(pending);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert!(reader.is_closed());
        assert_eq!(pool.used(), 0);
        vfs.pause.store(false, Ordering::SeqCst);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_vertex_probe_projection_retains_root_payload_after_session_drop() {
    let ((), report) = run_async_under_lab(0xa6_0304, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, _, _) = fixture(&commit, false).await;
        let value = CanonicalScalar::bytes(vec![0x7a; 4096]).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(1), P, Some(value.clone()));
        let at = db.write(&commit, edit).await.unwrap();
        let authority = issuer();
        let token = authority.issue_at(&grant(3), 100).unwrap();
        let pool = memory();
        let mut reader = reader(&commit, &cx, &vfs, &pool, &authority, &token, at, 2).await;
        let args = GqlParameters::new();
        let prepared = reader
            .prepare(
                &cx,
                "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n,n.score LIMIT 1",
                &args,
            )
            .unwrap();
        let mut cursor = reader.stream(&cx, &prepared, &args).unwrap();
        let row = cursor.next().await.unwrap().unwrap();
        assert_eq!(
            row.as_ref().values(),
            &[GraphValue::Vertex(VId(1)), GraphValue::Scalar(value)],
        );
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(reader);
        assert!(
            pool.used() >= 4096,
            "the delivered root copy remains governed"
        );
        drop(row);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_vertex_probes_enumerate_isolates_when_no_edge_exists() {
    let ((), report) = run_async_under_lab(0xa6_0305, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = GateVfs::new();
        let mut db =
            Database::create_with_vfs(&commit, vfs.clone(), vfs.memory.database_dir(), keys())
                .await
                .unwrap();
        let mut catalog = SymbolCatalog::default();
        catalog
            .bind_all([
                ("L", CatalogSymbol::Label(L)),
                ("H", CatalogSymbol::Label(H)),
                ("R", CatalogSymbol::Relation(R)),
            ])
            .unwrap();
        db.register_symbols(&commit, &catalog).await.unwrap();
        let mut birth = WriteBatch::new(R);
        birth.create_vertex(VId(1), vec![L], vec![]);
        birth.create_vertex(VId(2), vec![H], vec![]);
        birth.create_vertex(VId(u128::MAX), vec![L], vec![]);
        let at = db.write(&commit, birth).await.unwrap();
        let authority = issuer();
        let token = authority.issue_at(&grant(2), 100).unwrap();
        let pool = memory();
        let mut reader = reader(&commit, &cx, &vfs, &pool, &authority, &token, at, 2).await;
        assert_eq!(
            collect(
                &mut reader,
                &cx,
                "MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } RETURN n",
            )
            .await,
            rows(&[1, u128::MAX]),
        );
        assert!(
            collect(
                &mut reader,
                &cx,
                "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n",
            )
            .await
            .is_empty()
        );
        drop(reader);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
