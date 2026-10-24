use super::*;
use fgdb_gql::algebra::GraphValue;
use fgdb_types::CanonicalScalar;

fn budget(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, rows, 10_000_000, 10_000_000)
}
fn row(value: i64) -> Vec<QueryValue> {
    vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
        value,
    )))]
}
fn bag(counts: [i128; 2]) -> ZSet<Vec<QueryValue>> {
    ZSet::from_updates(
        counts
            .into_iter()
            .enumerate()
            .map(|(i, count)| (row(i as i64), ZWeight::from_i128(count))),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn open<'a>(
    sources: &'a [ZSet<Vec<QueryValue>>],
    policy: GqlQueryPolicy,
) -> Pull<'a, impl Iterator<Item = Entry<'a>> + use<'a>> {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy,
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let merge = SortedMerge::new(sources.iter().map(ZSet::iter), &mut meter).unwrap();
    Pull {
        merge: Some(merge),
        delivered: 0,
        policy,
        stats: meter.stats,
    }
}
fn drain<'a, I: Iterator<Item = Entry<'a>>>(
    pull: &mut Pull<'a, I>,
) -> Result<Vec<(Vec<QueryValue>, i128)>, StandingQueryError> {
    let mut result = Vec::new();
    while let Some(item) = pull.advance(&mut || Ok(())) {
        let (row, weight) = item?;
        result.push((row.to_vec(), weight.to_i128().unwrap()));
    }
    Ok(result)
}

#[test]
fn signed_prefixes_deliver_exact_borrowed_keys_without_a_collected_output() {
    for mut code in 0..15_625_u32 {
        let mut counts = [[0_i128; 2]; 3];
        for tick in &mut counts {
            for count in tick {
                *count = i128::from(code % 5) - 2;
                code /= 5;
            }
        }
        let sources = counts.into_iter().map(bag).collect::<Vec<_>>();
        let expected = (0..2)
            .filter_map(|key| {
                let sum: i128 = counts.iter().map(|tick| tick[key]).sum();
                (sum != 0).then(|| (row(key as i64), sum))
            })
            .collect::<Vec<_>>();
        let mut pull = open(&sources, budget(2));
        assert_eq!(drain(&mut pull).unwrap(), expected);
        assert_eq!(pull.delivered, expected.len() as u64);
        assert!(pull.merge.is_none());
        assert!(
            pull.advance(&mut || panic!("EOF touched control"))
                .is_none()
        );
    }
}

#[test]
fn each_pull_checkpoint_including_final_eof_fuses_refusal_and_allows_fresh_retry() {
    let sources = [bag([7, -1]), bag([-2, 5])];
    let mut good = open(&sources, budget(2));
    let mut calls = 0;
    let mut expected = Vec::new();
    while let Some(item) = good.advance(&mut || {
        calls += 1;
        Ok(())
    }) {
        let (row, weight) = item.unwrap();
        expected.push((row.to_vec(), weight.to_i128().unwrap()));
    }
    assert!(calls > 1);
    for stop in 1..=calls {
        let mut candidate = open(&sources, budget(2));
        let mut visited = 0;
        let mut prefix = Vec::new();
        loop {
            match candidate.advance(&mut || {
                visited += 1;
                if visited == stop {
                    Err(StandingQueryFailure::Interrupted)
                } else {
                    Ok(())
                }
            }) {
                Some(Ok((row, weight))) => prefix.push((row.to_vec(), weight.to_i128().unwrap())),
                Some(Err(StandingQueryError::Delivery(StandingQueryFailure::Interrupted))) => break,
                other => panic!("missed injected refusal: {other:?}"),
            }
        }
        assert_eq!(visited, stop);
        assert_eq!(prefix, expected[..prefix.len()]);
        assert_eq!(candidate.delivered, prefix.len() as u64);
        assert!(candidate.merge.is_none());
        let stats = candidate.stats;
        assert!(
            candidate
                .advance(&mut || panic!("failed pull touched control"))
                .is_none()
        );
        assert_eq!(candidate.stats, stats);
        assert_eq!(drain(&mut open(&sources, budget(2))).unwrap(), expected);
        if stop == calls {
            // All rows were handed out, but completion itself was refused.
            assert_eq!(prefix, expected);
        }
    }
    for stop in [1, calls / 2, calls] {
        let mut candidate = open(&sources, budget(2));
        let mut visited = 0;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while candidate
                    .advance(&mut || {
                        visited += 1;
                        assert_ne!(visited, stop, "injected unwind");
                        Ok(())
                    })
                    .is_some()
                {}
            }))
            .is_err()
        );
        assert!(candidate.merge.is_none());
        assert!(
            candidate
                .advance(&mut || panic!("unwound pull touched control"))
                .is_none()
        );
    }
    let exact = GqlQueryPolicy::new(0, 2, good.stats.work_units, good.stats.scratch_entries);
    assert_eq!(drain(&mut open(&sources, exact)).unwrap(), expected);
    for (work, scratch, rows, cause) in [
        (
            exact.evaluator.max_work_units - 1,
            exact.evaluator.max_scratch_entries,
            2,
            StandingQueryFailure::WorkBudget,
        ),
        (
            exact.evaluator.max_work_units,
            exact.evaluator.max_scratch_entries - 1,
            2,
            StandingQueryFailure::ScratchBudget,
        ),
        (
            exact.evaluator.max_work_units,
            exact.evaluator.max_scratch_entries,
            1,
            StandingQueryFailure::ResultBudget,
        ),
    ] {
        let mut candidate = open(&sources, GqlQueryPolicy::new(0, rows, work, scratch));
        assert!(
            matches!(drain(&mut candidate), Err(StandingQueryError::Delivery(error)) if error == cause)
        );
        assert!(candidate.merge.is_none());
    }
}

#[test]
fn large_outputs_stay_borrowed_and_drop_does_not_drain_or_acknowledge() {
    let values = ZSet::from_updates(
        (0..16_384_i64).map(|n| (row(n), ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let sources = [values];
    let mut pull = open(&sources, budget(16_384));
    let initial = pull.stats;
    let (first, weight) = pull.advance(&mut || Ok(())).unwrap().unwrap();
    assert_eq!(weight, ZWeight::ONE);
    assert!(std::ptr::eq(
        first,
        sources[0].iter().next().unwrap().0.as_slice()
    ));
    let merge = pull.merge.as_ref().unwrap();
    assert_eq!((merge.sources.len(), merge.heads.len()), (1, 1));
    assert_eq!(pull.delivered, 1);
    assert!(pull.stats.work_units - initial.work_units < 20);
    drop(pull);
    // Returned references borrow retained history, not cursor-owned copies.
    assert_eq!(first, row(0).as_slice());
    assert_eq!(sources[0].len(), 16_384);
    let mut retry = open(&sources, budget(16_384));
    assert_eq!(retry.advance(&mut || Ok(())).unwrap().unwrap().0, first);
    let cancelled = [bag([9, -8]), bag([-9, 8])];
    let mut empty = open(&cancelled, budget(0));
    assert!(empty.advance(&mut || Ok(())).is_none());
    assert_eq!(empty.delivered, 0);
    let mut scratch = Vec::new();
    for bytes in [1, 65_536] {
        let text = vec![QueryValue::Value(GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(&"x".repeat(bytes)).unwrap(),
        ))];
        let source =
            ZSet::from_updates([(text, ZWeight::ONE)], LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        let sources = [source];
        let mut pull = open(&sources, budget(1));
        let (row, _) = pull.advance(&mut || Ok(())).unwrap().unwrap();
        assert!(std::ptr::eq(
            row,
            sources[0].iter().next().unwrap().0.as_slice()
        ));
        assert!(pull.advance(&mut || Ok(())).is_none());
        scratch.push(pull.stats.scratch_entries);
    }
    assert_eq!(scratch[0], scratch[1]);
}

#[test]
fn late_arithmetic_failure_never_becomes_successful_exhaustion() {
    let mut huge = ZWeight::from_i128(i128::MAX);
    while let Ok(next) = huge.checked_add(&huge, LIMBS) {
        huge = next;
    }
    let first = ZSet::from_updates(
        [
            (row(0), ZWeight::ONE),
            (row(1), huge.checked_clone(LIMBS).unwrap()),
        ],
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    let second = ZSet::from_updates([(row(1), huge)], LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
    let sources = [first, second];
    let mut pull = open(&sources, budget(2));
    assert_eq!(
        pull.advance(&mut || Ok(())).unwrap().unwrap().0,
        row(0).as_slice()
    );
    assert!(matches!(
        pull.advance(&mut || Ok(())),
        Some(Err(StandingQueryError::Delivery(
            StandingQueryFailure::Arithmetic
        )))
    ));
    assert_eq!(pull.delivered, 1);
    assert!(pull.merge.is_none());
    assert!(
        pull.advance(&mut || panic!("failed pull touched control"))
            .is_none()
    );
}

#[test]
fn public_cursor_pins_the_selected_cut_preserves_gaps_and_integrates_to_native_reads() {
    use crate::{DatabaseKeys, MemVfs, QueryResult, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
    use std::collections::BTreeMap;

    let ((), report) = run_async_under_lab(0x726d_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = || DatabaseKeys::new([91; 32], DatabaseSecurityNamespaceId([92; 32]), [93; 32]);
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let maintenance = GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000);
        let symbols = |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        };
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, 1), (2, 2), (3, 2)] {
            seed.create_vertex(
                VId(id),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
            );
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let text = "MATCH (n) RETURN n.p AS p";
        let params = GqlParameters::new();
        let handle = db
            .register_standing_native(&cx, text, &params, symbols, maintenance)
            .unwrap();
        let replay = db
            .register_standing_replay(&cx, &handle, 8, 1000, 10_000, maintenance)
            .unwrap();
        let snapshot = |db: &Database<MemVfs>| {
            let QueryResult::Rows { rows, .. } =
                db.query(&cx, text, &params, symbols, maintenance).unwrap()
            else {
                panic!("rows");
            };
            let mut bag = BTreeMap::<Vec<QueryValue>, i128>::new();
            for row in rows {
                *bag.entry(row).or_default() += 1;
            }
            bag
        };
        let mut accepted = snapshot(&db);
        for value in [8, 1] {
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(value)));
            db.write(&commit, edit).await.unwrap();
        }
        let empty_end = db.frontier().unwrap();
        {
            let (end, mut cursor) = db
                .standing_replay_coalesced_cursor(&cx, &replay, basis, limit(2), budget(0))
                .unwrap()
                .unwrap();
            assert_eq!(end, empty_end);
            assert!(cursor.next().is_none());
            assert!(cursor.next().is_none());
        }
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
        let at = db.write(&commit, edit).await.unwrap();
        let copy = replay.clone();
        let (end, mut cursor) = db
            .standing_replay_coalesced_cursor(&cx, &copy, empty_end, limit(8), budget(2))
            .unwrap()
            .unwrap();
        drop(copy); // This borrow must not be captured by the returned iterator.
        assert_eq!(end, at);
        for item in cursor.by_ref() {
            let (row, weight) = item.unwrap();
            *accepted.entry(row.to_vec()).or_default() += weight.to_i128().unwrap();
        }
        assert!(cursor.next().is_none());
        drop(cursor);
        accepted.retain(|_, count| *count != 0);
        assert_eq!(accepted, snapshot(&db));
        {
            let (_, mut short) = db
                .standing_replay_coalesced_cursor(&cx, &replay, empty_end, limit(8), budget(1))
                .unwrap()
                .unwrap();
            assert!(short.next().unwrap().is_ok());
            assert!(matches!(
                short.next(),
                Some(Err(StandingQueryError::Delivery(
                    StandingQueryFailure::ResultBudget
                )))
            ));
            assert!(short.next().is_none());
        }
        assert_eq!(snapshot(&db), accepted);
        assert!(
            db.standing_replay_coalesced_cursor(&cx, &replay, at, limit(1), budget(0))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            db.standing_replay_coalesced_cursor(
                &cx,
                &replay,
                CommitSeq(u64::MAX),
                limit(1),
                budget(2)
            ),
            Err(StandingQueryError::ReplayGap { .. })
        ));
        assert!(matches!(
            db.standing_replay_coalesced_cursor(&cx, &handle, basis, limit(1), budget(2)),
            Err(StandingQueryError::Unsupported)
        ));
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            other.standing_replay_coalesced_cursor(&cx, &replay, basis, limit(1), budget(2)),
            Err(StandingQueryError::ForeignHandle)
        ));
        db.rebuild_standing_query(&cx, &replay, maintenance)
            .unwrap();
        assert!(matches!(
            db.standing_replay_coalesced_cursor(&cx, &replay, basis, limit(8), budget(2)),
            Err(StandingQueryError::ReplayGap { .. })
        ));
        let fragile = db
            .register_standing_replay(&cx, &handle, 8, 1, 10_000, maintenance)
            .unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(6)));
        let durable = db.write(&commit, edit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), durable);
        assert!(matches!(
            db.standing_replay_coalesced_cursor(&cx, &fragile, at, limit(8), budget(2)),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::ResultBudget,
                ..
            })
        ));
        assert_eq!(
            db.standing_native_query(&cx, &handle, maintenance)
                .unwrap()
                .0,
            durable
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn limit(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
