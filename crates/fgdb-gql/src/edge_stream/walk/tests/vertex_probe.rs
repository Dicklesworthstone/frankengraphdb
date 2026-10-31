//! The real-root probe cursor uses the same non-Sync guarded fixture as paths.
//! Whole-edge scans panic in that fixture, so every probe must use incidence.

use super::*;
use crate::stream::{AsyncVertexProbeCursor, AsyncVertexProbePlan, VertexScanState};

fn plan(text: &str) -> AsyncVertexProbePlan<GraphValueRow> {
    AsyncVertexProbePlan::compile(prepared(text).plan()).unwrap()
}

const ANTI: &str = "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) WHERE b.p=999 } RETURN a";

#[test]
fn each_root_is_admitted_once_and_only_reached_probes_open_incidence() {
    for (text, expected_seeks, expected_rows) in [
        ("MATCH (a) WHERE EXISTS { MATCH (a) } RETURN a", 0, 4),
        (
            "MATCH (a) WHERE EXISTS { MATCH WALK (a)-[:R*0..0]->(b) } RETURN a",
            0,
            4,
        ),
        (
            "MATCH (a) WHERE a.p<0 AND EXISTS { MATCH (a)-[:R]->(b) } RETURN a",
            0,
            0,
        ),
    ] {
        let mut source = Source::new(31);
        source.edge_fault = Fault::Io;
        let counts = source.counts.clone();
        let mut cursor = AsyncVertexProbeCursor::new(source, plan(text), wide(), ok);
        let mut rows = 0;
        while let Some(row) = run(cursor.next()) {
            drop(row.unwrap());
            rows += 1;
        }
        assert_eq!(rows, expected_rows);
        assert_eq!(cursor.row_stats().snapshot_records, 4);
        assert_eq!(counts.seeks.load(Ordering::SeqCst), expected_seeks);
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
    let source = Source::new(31);
    let counts = source.counts.clone();
    let mut cursor = AsyncVertexProbeCursor::new(
        source,
        plan("MATCH (a) WHERE a.p=1 AND EXISTS { MATCH (a)-[:R]->(b) } RETURN a"),
        wide(),
        ok,
    );
    assert_eq!(
        run(cursor.next()).unwrap().unwrap().row().values(),
        &[GraphValue::Vertex(IDS[0])]
    );
    assert!(run(cursor.next()).is_none());
    assert_eq!(
        counts.seeks.load(Ordering::SeqCst),
        1,
        "the first witness ends a positive probe"
    );
    assert!(
        counts.peak.load(Ordering::SeqCst) <= 2,
        "no sibling incidence bag is retained"
    );
}

#[test]
fn source_protocol_and_memory_failures_never_turn_into_a_negative_witness() {
    for roots in [false, true] {
        for fault in [
            Fault::Io,
            Fault::Missing,
            Fault::Twice,
            Fault::Mismatch,
            Fault::FalseEof,
            Fault::Repeat,
        ] {
            let mut source = Source::new(31);
            if roots {
                source.root_fault = fault;
            } else {
                source.edge_fault = fault;
            }
            let counts = source.counts.clone();
            let mut cursor = AsyncVertexProbeCursor::new(source, plan(ANTI), wide(), ok);
            let mut failed = false;
            while let Some(row) = run(cursor.next()) {
                if row.is_err() {
                    failed = true;
                    break;
                }
            }
            assert!(failed, "root fault {roots}, fault {}", fault as u8);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(run(cursor.next()).is_none());
            assert_eq!(counts.records.load(Ordering::SeqCst), 0);
            assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
            assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
        }
    }
    for fault in [Fault::Unavailable, Fault::Dangling] {
        let mut source = Source::new(31);
        source.edge_fault = fault;
        let mut cursor = AsyncVertexProbeCursor::new(source, plan(ANTI), wide(), ok);
        assert!(run(cursor.next()).unwrap().is_err());
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(run(cursor.next()).is_none());
    }
    for kind in 0..4 {
        let mut source = Source::new(31);
        match kind {
            0 => source.refuse_backing = true,
            1 => source.refuse_output = true,
            2 => source.refuse_evaluation = true,
            _ => source.scratch_limit = 0,
        }
        let counts = source.counts.clone();
        let mut cursor = AsyncVertexProbeCursor::new(source, plan(ANTI), wide(), ok);
        assert!(run(cursor.next()).unwrap().is_err());
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn every_live_checkpoint_and_exact_cumulative_budget_covers_roots_and_probes() {
    let plan = plan(ANTI);
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    let mut cursor = AsyncVertexProbeCursor::new(Source::new(1), plan.clone(), wide(), move || {
        count.fetch_add(1, Ordering::SeqCst);
        Ok::<_, usize>(())
    });
    while let Some(row) = run(cursor.next()) {
        drop(row.unwrap());
    }
    let checkpoints = seen.load(Ordering::SeqCst);
    let rows = cursor.row_stats();
    let evaluator = cursor.evaluator_stats();
    for fail_at in 0..checkpoints {
        let source = Source::new(1);
        let counts = source.counts.clone();
        let mut called = 0;
        let mut cursor = AsyncVertexProbeCursor::new(source, plan.clone(), wide(), move || {
            let at = called;
            called += 1;
            if at == fail_at { Err(at) } else { Ok(()) }
        });
        let mut error = None;
        while let Some(row) = run(cursor.next()) {
            if let Err(failure) = row {
                error = Some(failure);
                break;
            }
        }
        assert!(matches!(error, Some(GqlQueryError::Interrupted(at)) if at == fail_at));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(run(cursor.next()).is_none());
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
    }
    let exact = [
        rows.snapshot_records,
        rows.result_rows,
        evaluator.work_units,
        evaluator.scratch_entries,
    ];
    assert!(
        rows.snapshot_records > 4,
        "the incidence candidate shares root admission counters"
    );
    for dimension in 0..5 {
        let mut limits = exact;
        if dimension < 4 {
            limits[dimension] -= 1;
        }
        let policy = GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]);
        let mut cursor = AsyncVertexProbeCursor::new(Source::new(1), plan.clone(), policy, ok);
        let mut failed = false;
        while let Some(row) = run(cursor.next()) {
            if row.is_err() {
                failed = true;
                break;
            }
        }
        assert_eq!(failed, dimension < 4, "budget dimension {dimension}");
    }
}

#[test]
fn dropping_a_polled_probe_future_refunds_records_and_preserves_debited_counters() {
    for root_pending in [false, true] {
        let mut source = Source::new(31);
        if root_pending {
            source.root_fault = Fault::Pending;
        } else {
            source.edge_fault = Fault::Pending;
        }
        let counts = source.counts.clone();
        let mut cursor = AsyncVertexProbeCursor::new(source, plan(ANTI), wide(), ok);
        drop(cursor.next::<usize>());
        assert_eq!(cursor.state(), VertexScanState::Open);
        let mut future = Box::pin(cursor.next::<usize>());
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&future);
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(future);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(run(cursor.next()).is_none());
        if !root_pending {
            assert_eq!(cursor.row_stats().snapshot_records, 1);
            assert!(
                cursor.evaluator_stats().scratch_entries > 1,
                "probe meter loan must restore its charges even when the await is dropped"
            );
        }
        assert_eq!(counts.records.load(Ordering::SeqCst), 0);
        assert_eq!(counts.guards.load(Ordering::SeqCst), 0);
        assert_eq!(counts.drops.load(Ordering::SeqCst), 1);
    }
}
