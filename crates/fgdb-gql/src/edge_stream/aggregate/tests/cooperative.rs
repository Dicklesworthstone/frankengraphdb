use super::*;
use core::task::{Context, Poll, Waker};

fn large_source() -> Source {
    let mut source = source(0);
    for at in 0..257 {
        let eid = EId(1000 + at);
        let (from, to, relation) = if at == 0 {
            (VId(1), VId(u128::MAX), S)
        } else {
            (VId(0), VId(1), R)
        };
        source.edges.insert(
            eid,
            (
                from,
                relation,
                to,
                vec![(P, CanonicalScalar::Int((at % 7) as i64).into())],
            ),
        );
        for key in [(from, 0), (to, 1), (from, 2), (to, 2)] {
            source.incident.entry(key).or_default().insert(eid);
        }
    }
    source
}

#[test]
fn cooperative_join_intake_matches_native_groups_and_counters_without_restarting() {
    for text in [
        "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN r.p AS k,COUNT(*) AS count,SUM(r.p) AS sum,AVG(DISTINCT r.p) AS avg GROUP BY r.p ORDER BY count DESC",
        "MATCH (a)-[r:R]->(b) WHERE r.p < 0 RETURN COUNT(*) AS count",
    ] {
        let q = prepare(text);
        let wide = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let mut direct = run(&q, large_source(), wide);
        let expected: Vec<_> = direct.by_ref().map(Result::unwrap).collect();
        let input = large_source();
        let reads = Arc::clone(&input.reads);
        let drops = Arc::clone(&input.drops);
        let mut cursor = run(&q, input, wide);
        let mut context = Context::from_waker(Waker::noop());
        assert!(cursor.poll_next(&mut context).is_pending(), "{text}");
        assert!(reads.load(Ordering::SeqCst) <= 64);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let mut got = Vec::new();
        loop {
            match cursor.poll_next(&mut context) {
                Poll::Pending => assert_eq!(cursor.row_stats().result_rows, 0),
                Poll::Ready(Some(Ok(row))) => got.push(row),
                Poll::Ready(Some(Err(error))) => panic!("{text}: {error:?}"),
                Poll::Ready(None) => break,
            }
        }
        assert_eq!(got, expected, "{text}");
        assert_eq!(cursor.row_stats(), direct.row_stats(), "{text}");
        assert_eq!(cursor.evaluator_stats(), direct.evaluator_stats(), "{text}");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn close_after_a_pending_join_releases_source_and_unfinished_groups() {
    let q = prepare("MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(*) AS count");
    let input = large_source();
    let reads = Arc::clone(&input.reads);
    let drops = Arc::clone(&input.drops);
    let mut cursor = run(&q, input, policy());
    let mut context = Context::from_waker(Waker::noop());
    assert!(cursor.poll_next(&mut context).is_pending());
    let read = reads.load(Ordering::SeqCst);
    cursor.close();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(cursor.poll_next(&mut context), Poll::Ready(None)));
    assert_eq!(reads.load(Ordering::SeqCst), read);
    assert_eq!(cursor.row_stats().result_rows, 0);
}
