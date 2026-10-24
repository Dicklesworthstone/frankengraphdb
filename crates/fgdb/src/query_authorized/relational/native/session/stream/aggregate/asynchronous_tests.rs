use super::*;
use core::future::Future;
use core::task::{Context, Poll, Waker};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

async fn large_database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = database(cx).await;
    let mut batch = WriteBatch::new(RelationId(1));
    // Hidden history must yield without entering either resource transcript.
    for id in 1000..1192 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(99)],
            vec![(P, CanonicalScalar::Int(999))],
        );
    }
    for id in 2000..2192 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![
                (P, CanonicalScalar::Int((id % 7) as i64)),
                (H, CanonicalScalar::Bool(true)),
            ],
        );
        if id != 2000 {
            batch.add_edge(EId(id), VId(id - 1), VId(id), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    db
}

async fn read_all(cursor: &mut AuthorizedAsyncAggregateCursor<'_>) -> QueryResult {
    let slots = cursor.output_slots().to_vec();
    let columns = cursor.columns().to_vec();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next_row().await.unwrap() {
        rows.push(
            slots
                .iter()
                .map(|slot| match *slot {
                    GraphAggregateTextSlot::GroupKey(at) => {
                        QueryValue::Value(row.keys()[at].clone())
                    }
                    GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                })
                .collect(),
        );
    }
    QueryResult::Rows { columns, rows }
}

#[test]
fn cooperative_groups_match_synchronous_masked_vertex_and_join_aggregates() {
    let ((), report) = run_async_under_lab(0x5ec0_84a1, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = large_database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "host-main", symbols, policy(), || 100)
            .unwrap();
        for text in [
            "MATCH (n) RETURN n.p AS k,COUNT(*) AS count,SUM(n.p) AS sum,AVG(n.p) AS avg,COUNT(DISTINCT n.p) AS distinct_count,COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS support GROUP BY n.p ORDER BY count DESC LIMIT 4",
            "MATCH (n) WHERE n.hidden IS NOT NULL RETURN COUNT(*) AS count",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN b.p AS k,COUNT(*) AS count,SUM(b.p) AS sum,AVG(DISTINCT b.p) AS avg GROUP BY b.p ORDER BY count DESC",
            "MATCH (a)-[r:R]->(b) WHERE a.hidden IS NOT NULL RETURN COUNT(*) AS count",
        ] {
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let expected = native(&mut session.stream_aggregate(&cx, &prepared, &args).unwrap());
            let mut cursor = session
                .stream_aggregate_async(&cx, &prepared, &args)
                .unwrap();
            fn is_send<T: Send>(_: &T) {}
            is_send(&cursor);
            assert_eq!(read_all(&mut cursor).await, expected, "{text}");
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert!(cursor.next_row().await.unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pending_source_pull_keeps_its_snapshot_and_checks_expiry_before_resuming() {
    let ((), report) = run_async_under_lab(0x5ec0_84a2, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = large_database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let now = Arc::new(AtomicU64::new(100));
        let clock = Arc::clone(&now);
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "host-main",
                symbols,
                policy(),
                move || clock.load(Ordering::SeqCst),
            )
            .unwrap();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN COUNT(*) AS count", &args)
            .unwrap();
        let mut cursor = session
            .stream_aggregate_async(&cx, &prepared, &args)
            .unwrap();
        let at = cursor.snapshot_seq();
        let mut future = Box::pin(cursor.next_row());
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let mut write = WriteBatch::new(RelationId(1));
        write.create_vertex(VId(3000), vec![LabelId(1)], vec![]);
        db.write(&c.commit(), write).await.unwrap();
        let first = future.await.unwrap().unwrap();
        assert_eq!(first.values(), &[QueryValue::Count(196)]);
        assert_eq!(cursor.snapshot_seq(), at);
        drop(cursor);

        let mut cursor = session
            .stream_aggregate_async(&cx, &prepared, &args)
            .unwrap();
        let mut future = Box::pin(cursor.next_row());
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        now.store(1001, Ordering::SeqCst);
        assert!(matches!(
            future.await,
            Err(QueryError::Authorization(Denied::Expired))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next_row().await.unwrap().is_none());
        drop(cursor);
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_pending_input_fuses_the_cursor_and_releases_the_session_pin() {
    let ((), report) = run_async_under_lab(0x5ec0_84a3, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = large_database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        // Each input must still be scanning at the first poll. A direct
        // `n.p < 0` is now answered from the range candidates in one poll, so
        // the vertex input compares an expression no candidate source reads.
        for text in [
            "MATCH (n) WHERE n.p + 0 < 0 RETURN COUNT(*) AS count",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN COUNT(*) AS count",
        ] {
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "host-main",
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let mut cursor = session
                .stream_aggregate_async(&cx, &prepared, &args)
                .unwrap();
            {
                let mut future = Box::pin(cursor.next_row());
                assert!(matches!(
                    future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Pending
                ));
            }
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next_row().await.unwrap().is_none());
            cursor.close();
            drop(cursor);
            assert!(session.is_closed());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
