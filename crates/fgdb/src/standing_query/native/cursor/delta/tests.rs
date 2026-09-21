use super::*;
use fgdb_gql::algebra::GraphValue;
use fgdb_types::CanonicalScalar;

fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(0, 1000, 1_000_000, 1_000_000) }
fn row(n: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(n))])
}
fn metadata() -> Arc<Layout> { Arc::new(Layout::Rows { columns: vec!["value".into()] }) }
fn make(rows: &ZSet<GraphValueRow>, policy: GqlQueryPolicy) -> Pull<'_> {
    Pull::new(view_runs(rows, None, NativeRow::Values), metadata(), CommitSeq(7), policy,
        StandingQueryStats { work_units: 1, scratch_entries: 1, ..StandingQueryStats::default() })
}
fn drain(pull: &mut Pull<'_>) -> Result<Vec<(Vec<QueryValue>, ZWeight)>, StandingQueryFailure> {
    let mut result = Vec::new();
    while let Some(change) = pull.advance(&mut || Ok(()), pull_change) { result.push(change?); }
    Ok(result)
}

#[test]
fn all_small_signed_bags_deliver_one_exact_frame_per_changed_tuple() {
    for code in 0..625_usize {
        let delta = ZSet::from_updates((0..4).map(|i| (row(i as i64),
            ZWeight::from_i128(((code / 5usize.pow(i)) % 5) as i128 - 2))),
            LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
        let before = delta.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
        let mut pull = make(&delta, policy());
        let actual = drain(&mut pull).unwrap();
        assert_eq!(actual.len(), delta.len());
        for ((cells, weight), (row, expected)) in actual.iter().zip(delta.iter()) {
            assert_eq!(cells, &vec![QueryValue::Value(row.values()[0].clone())]);
            assert_eq!(weight, expected);
        }
        assert_eq!(pull.delivered, delta.len() as u64);
        assert_eq!(pull.state, VertexScanState::Exhausted);
        assert!(pull.advance(&mut || panic!("EOF called control"), pull_change).is_none());
        pull.close(); assert_eq!(pull.state, VertexScanState::Exhausted);
        assert_eq!(delta, before);
    }
}

#[test]
fn huge_positive_and_negative_weights_remain_compressed_under_two_frame_allowance() {
    let huge = ZWeight::from_i128(i128::MAX)
        .checked_add(&ZWeight::from_i128(i128::MAX), LimbLimit::new(4)).unwrap();
    let negative = ZWeight::from_i128(i128::MIN)
        .checked_add(&ZWeight::from_i128(i128::MIN), LimbLimit::new(4)).unwrap();
    let delta = ZSet::from_updates([(row(1), huge), (row(2), negative)], LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(())).unwrap();
    let mut pull = make(&delta, GqlQueryPolicy::new(0, 2, 100, 100));
    let frames = drain(&mut pull).unwrap();
    assert_eq!(frames.len(), 2);
    assert!(frames[0].1 > ZWeight::ZERO && frames[1].1 < ZWeight::ZERO);
    assert_eq!(frames[0].1.to_i128(), None); assert_eq!(frames[1].1.to_i128(), None);
    assert_eq!(pull.delivered, 2);
    for ((_, got), (_, expected)) in frames.iter().zip(delta.iter()) { assert_eq!(got, expected); }
    let mut limited = make(&delta, GqlQueryPolicy::new(0, 1, 100, 100));
    assert!(limited.advance(&mut || Ok(()), pull_change).unwrap().is_ok());
    assert_eq!(limited.advance(&mut || Ok(()), pull_change), Some(Err(StandingQueryFailure::ResultBudget)));
    assert_eq!(limited.delivered, 1);
}

#[test]
fn every_delta_checkpoint_and_panic_preserves_input_and_fuses_incomplete_delivery() {
    let delta = ZSet::from_updates([(row(1), ZWeight::from_i128(-2)), (row(3), ZWeight::from_i128(2))],
        LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
    let before = delta.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
    let mut good = make(&delta, policy()); let mut calls = 0; let mut expected = Vec::new();
    while let Some(change) = good.advance(&mut || { calls += 1; Ok(()) }, pull_change) {
        expected.push(change.unwrap());
    }
    for stop in 1..=calls {
        let mut candidate = make(&delta, policy()); let mut seen = 0; let mut prefix = Vec::new();
        loop {
            match candidate.advance(&mut || {
                seen += 1;
                if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
            }, pull_change) {
                Some(Ok(frame)) => prefix.push(frame),
                Some(Err(error)) => { assert_eq!(error, StandingQueryFailure::Interrupted); break; }
                None => panic!("missed interruption"),
            }
        }
        assert_eq!(seen, stop); assert_eq!(prefix, expected[..prefix.len()]);
        assert_eq!(candidate.delivered, prefix.len() as u64);
        assert_eq!(candidate.state, VertexScanState::Failed);
        assert!(candidate.runs.is_none() && candidate.pending.is_none());
        assert!(candidate.advance(&mut || panic!("failed cursor called control"), pull_change).is_none());
        assert_eq!(delta, before);
        assert_eq!(drain(&mut make(&delta, policy())).unwrap(), expected);
    }
    for stop in [1, calls / 2, calls] {
        let mut candidate = make(&delta, policy()); let mut seen = 0;
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            while candidate.advance(&mut || {
                seen += 1; assert_ne!(seen, stop, "injected delta unwind"); Ok(())
            }, pull_change).is_some() {}
        })).is_err());
        assert_eq!(candidate.state, VertexScanState::Failed);
        assert!(candidate.runs.is_none() && candidate.pending.is_none());
        assert_eq!(delta, before);
    }
    let usage = good.stats;
    let exact = GqlQueryPolicy::new(0, 2, usage.work_units, usage.scratch_entries);
    assert_eq!(drain(&mut make(&delta, exact)).unwrap(), expected);
    for (work, scratch, cause) in [
        (usage.work_units - 1, usage.scratch_entries, StandingQueryFailure::WorkBudget),
        (usage.work_units, usage.scratch_entries - 1, StandingQueryFailure::ScratchBudget),
    ] {
        assert_eq!(drain(&mut make(&delta, GqlQueryPolicy::new(0, 2, work, scratch))), Err(cause));
    }
}

#[test]
fn gap_checks_are_exact_including_sequence_exhaustion_and_zero() {
    for frontier in [0, 1, 2, 19, u64::MAX] {
        for after in [0, 1, 18, 19, u64::MAX - 1, u64::MAX] {
            let got = require_predecessor(CommitSeq(after), CommitSeq(frontier));
            if after.checked_add(1) == Some(frontier) { assert!(got.is_ok()); }
            else { assert!(matches!(got, Err(StandingQueryError::DeltaGap { after: a, frontier: f })
                if a == CommitSeq(after) && f == CommitSeq(frontier))); }
        }
    }
}

#[test]
fn zero_weights_missing_signed_support_and_partial_schema_never_escape() {
    let r = row(1); let zero = ZWeight::ZERO;
    for weight in [None, Some(&zero)] {
        let mut pull = Pull::new(Box::new(std::iter::once(Run { row: NativeRow::Values(&r), weight })),
            metadata(), CommitSeq(7), policy(), StandingQueryStats::default());
        assert_eq!(pull.advance(&mut || Ok(()), pull_change), Some(Err(StandingQueryFailure::InvalidDelta)));
        assert_eq!(pull.delivered, 0);
    }
    let r = GraphValueRow::from_owned_values(vec![]); let one = ZWeight::ONE;
    let mut pull = Pull::new(Box::new(std::iter::once(Run { row: NativeRow::Values(&r), weight: Some(&one) })),
        metadata(), CommitSeq(7), policy(), StandingQueryStats::default());
    assert_eq!(pull.advance(&mut || Ok(()), pull_change), Some(Err(StandingQueryFailure::InvalidDelta)));
    assert_eq!(pull.delivered, 0);
}

#[path = "database_tests.rs"]
mod database;
