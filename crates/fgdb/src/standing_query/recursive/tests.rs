use super::*;
use asupersync::lab::run_async_under_lab;
use crate::{DatabaseKeys, WriteBatch};
use fgdb_types::{DatabaseSecurityNamespaceId, EId, PurposeContexts};

fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000) }

fn seeded(cx: &CommitCx, initial: &LogicalDeltaBatch) -> State {
    let policy = policy();
    let mut state = State {
        input: CommittedReachability::new(crate::GRAPH, crate::BRANCH, RelationId(1)),
        rows: ZSet::new(), policy, frontier: CommitSeq::ORIGIN,
        stats: StandingQueryStats::default(), failure: None,
    };
    let mut checkpoint = || Ok(());
    let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
    state.maintain(cx, initial, &mut meter).unwrap();
    state.frontier = initial.commit_seq();
    state.stats = meter.stats;
    state
}

#[test]
fn every_recursive_maintenance_checkpoint_is_atomic_and_retryable() {
    let ((), report) = run_async_under_lab(0x6a71, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut first = WriteBatch::new(RelationId(1));
        for id in 1..=4 { first.create_vertex(VId(id), vec![], vec![]); }
        first.add_edge(EId(1), VId(1), VId(2), vec![]);
        first.add_edge(EId(2), VId(1), VId(2), vec![]);
        first.add_edge(EId(3), VId(2), VId(3), vec![]);
        let basis = db.write(&commit, first).await.unwrap();
        let initial = db.delta_index().unwrap().get(basis).unwrap().clone();
        let mut second = WriteBatch::new(RelationId(1));
        second.delete_edge(EId(1));
        second.delete_edge(EId(2));
        second.add_edge(EId(4), VId(3), VId(1), vec![]);
        second.add_edge(EId(5), VId(4), VId(2), vec![]);
        let at = db.write(&commit, second).await.unwrap();
        let batch = db.delta_index().unwrap().get(at).unwrap().clone();
        let before = seeded(&commit, &initial);
        let mut success = seeded(&commit, &initial);
        let mut calls = 0;
        {
            let mut checkpoint = || { calls += 1; Ok(()) };
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            success.maintain(&commit, &batch, &mut meter).unwrap();
            success.frontier = at;
        }
        for stop in 1..=calls {
            let mut state = seeded(&commit, &initial);
            let mut seen = 0;
            {
                let mut checkpoint = || {
                    seen += 1;
                    if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
                };
                let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                assert_eq!(state.maintain(&commit, &batch, &mut meter), Err(StandingQueryFailure::Interrupted));
            }
            assert_eq!(seen, stop);
            assert_eq!(state.input, before.input);
            assert_eq!(state.rows, before.rows);
            assert_eq!(state.frontier, before.frontier);
            assert_eq!(state.stats, before.stats);
            assert_eq!(state.failure, before.failure);
            let mut checkpoint = || Ok(());
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            state.maintain(&commit, &batch, &mut meter).unwrap();
            state.frontier = at;
            assert_eq!(state.input, success.input);
            assert_eq!(state.rows, success.rows);
        }
        let mut limited = seeded(&commit, &initial);
        let mut checkpoint = || Ok(());
        let mut meter = Meter { policy: GqlQueryPolicy::new(10_000, 0, 1_000_000, 1_000_000),
            stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
        assert_eq!(limited.maintain(&commit, &batch, &mut meter), Err(StandingQueryFailure::ResultBudget));
        assert_eq!(limited.input, before.input);
        assert_eq!(limited.rows, before.rows);
        assert_eq!(limited.frontier, before.frontier);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
