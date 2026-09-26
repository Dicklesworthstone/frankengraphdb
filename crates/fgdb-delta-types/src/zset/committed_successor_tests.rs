// Included in the existing input tests: both lanes use exactly those fixtures.
fn with_commit_context(run: impl FnOnce(&CommitCx)) {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
    run(&contexts.commit());
}

#[test]
fn live_successors_match_indexed_ticks_and_keep_indexed_ancestry_checks() {
    with_commit_context(|cx| {
        let mut index = LocalDeltaBatchIndex::new();
        let mut indexed = input();
        let mut pushed = input();
        let batches = [
            batch(
                1,
                vec![coordinate(1, vec![create(1, 1, 1, 2), create(2, 1, 1, 2)])],
            ),
            batch(2, vec![coordinate(1, vec![delete(1), create(3, 1, 2, 1)])]),
            batch(3, vec![coordinate(2, vec![create(4, 2, 2, 3)])]),
            batch(
                4,
                vec![coordinate(1, vec![cascade(2, &[2, 3, 4]), delete(2)])],
            ),
        ];
        for batch in batches {
            index.insert(batch.clone()).unwrap();
            let mut read_events = Vec::new();
            let a = indexed
                .prepare_next(&index, LIMBS, &mut |event| {
                    read_events.push(event);
                    Ok::<_, usize>(())
                })
                .unwrap()
                .unwrap()
                .commit();
            let mut write_events = Vec::new();
            let b = pushed
                .prepare_committed_successor(cx, &batch, LIMBS, &mut |event| {
                    write_events.push(event);
                    Ok::<_, usize>(())
                })
                .unwrap()
                .commit();
            assert_eq!(a, b);
            assert_eq!(read_events, write_events);
            assert_eq!(indexed, pushed);
        }
        assert!(
            pushed
                .prepare_next(&index, LIMBS, &mut allow)
                .unwrap()
                .is_none()
        );
        let at = pushed.frontier();
        let original = index.get(at).unwrap();
        let changed = LogicalDeltaBatch::from_parts_for_test(
            original.coordinate_entries().to_vec(),
            [99; 32],
            original.commit_marker_identity(),
            at,
            at,
        );
        let fork = LocalDeltaBatchIndex::from_parts_for_test(
            CommitSeq::ORIGIN,
            at,
            index
                .since(CommitSeq::ORIGIN)
                .unwrap()
                .map(|batch| {
                    (
                        batch.commit_seq(),
                        if batch.commit_seq() == at {
                            changed.clone()
                        } else {
                            batch.clone()
                        },
                    )
                })
                .collect(),
        );
        assert_eq!(
            pushed.prepare_next(&fork, LIMBS, &mut allow).unwrap_err(),
            EdgeInputError::HistoryChanged { at }
        );
        assert_eq!(pushed, indexed);
    });
}

#[test]
fn live_successor_refusals_and_drops_never_acknowledge_the_batch() {
    with_commit_context(|cx| {
        let (before, _) = seed();
        let next = batch(
            2,
            vec![
                coordinate(1, vec![delete(1), create(4, 1, 4, 5)]),
                coordinate(2, vec![cascade(2, &[2, 3])]),
            ],
        );
        let mut success = seed().0;
        let mut calls = 0;
        let wanted = success
            .prepare_committed_successor(cx, &next, LIMBS, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .commit();
        for stop in 1..=calls {
            let mut candidate = seed().0;
            let mut seen = 0;
            assert_eq!(
                candidate
                    .prepare_committed_successor(cx, &next, LIMBS, &mut |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    })
                    .unwrap_err(),
                EdgeInputError::Delta(ZSetError::Control(stop))
            );
            assert_eq!(seen, stop);
            assert_eq!(candidate, before);
            let pending = candidate
                .prepare_committed_successor(cx, &next, LIMBS, &mut allow)
                .unwrap();
            assert_eq!(pending.delta(), &wanted);
            drop(pending);
            assert_eq!(candidate, before);
            candidate
                .prepare_committed_successor(cx, &next, LIMBS, &mut allow)
                .unwrap()
                .commit();
            assert_eq!(candidate, success);
        }
        for seq in [0, 1, 3, u64::MAX] {
            let mut candidate = seed().0;
            let wrong = batch(seq, vec![coordinate(1, vec![create(4, 1, 4, 5)])]);
            assert!(matches!(
                candidate.prepare_committed_successor(cx, &wrong, LIMBS, &mut allow),
                Err(EdgeInputError::Index(IndexError::WrongEntryKey { .. }))
            ));
            assert_eq!(candidate, before);
        }
        for wrong_marker in [false, true] {
            let bad = LogicalDeltaBatch::from_parts_for_test(
                next.coordinate_entries().to_vec(),
                [9; 32],
                MarkerRef {
                    marker_oid: ObjectId([9; 32]),
                    commit_seq: CommitSeq(if wrong_marker { 3 } else { 2 }),
                },
                CommitSeq(2),
                CommitSeq(if wrong_marker { 2 } else { 3 }),
            );
            let mut candidate = seed().0;
            assert!(matches!(
                candidate.prepare_committed_successor(cx, &bad, LIMBS, &mut allow),
                Err(EdgeInputError::Index(IndexError::WrongMarker { .. }))
                    | Err(EdgeInputError::Index(IndexError::WrongFrontier { .. }))
            ));
            assert_eq!(candidate, before);
        }
    });
}
