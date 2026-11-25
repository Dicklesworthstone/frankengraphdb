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

fn catalog_batch(seq: u64, before: u64, after: u64) -> LogicalDeltaBatch {
    let transition_oid = ObjectId([42; 32]);
    let mut entry = coordinate(
        1,
        vec![DeltaRow::Schema {
            transition_oid,
            before_epoch: SchemaEpoch(before),
            after_epoch: SchemaEpoch(after),
        }],
    );
    entry.schema_epoch = SchemaEpoch(before);
    entry.schema_transition = Some(transition_oid);
    batch(seq, vec![entry])
}

#[test]
fn attested_catalog_ticks_preserve_raw_anchors_and_accept_old_prepared_epochs() {
    with_commit_context(|cx| {
        let (mut state, mut index) = seed();
        let baseline = state.snapshot(LIMBS, &mut allow).unwrap();
        let catalog = catalog_batch(2, 0, 1);
        let proof = CommittedSchemaCompatibility::attest_name_only_catalog(
            cx,
            &catalog,
            GraphId(1),
            BranchId(1),
            SchemaEpoch(1),
        );
        assert!(proof.matches_batch(&catalog));
        assert!(!proof.matches_batch(&catalog.clone()));
        assert_eq!(
            state
                .prepare_committed_successor(cx, &catalog, LIMBS, &mut allow)
                .unwrap_err(),
            EdgeInputError::SchemaChanged
        );
        assert!(
            state
                .prepare_catalog_successor(cx, &proof, LIMBS, &mut allow)
                .unwrap()
                .commit()
                .is_empty()
        );
        assert_eq!(state.frontier(), CommitSeq(2));
        assert_eq!(state.snapshot(LIMBS, &mut allow).unwrap(), baseline);
        index.insert(catalog.clone()).unwrap();
        assert!(state.prepare_next(&index, LIMBS, &mut allow).unwrap().is_none());
        // Neither the marker nor the raw schema template is replaced by an
        // invented empty graph batch to obtain the zero topology derivative.
        let changed = LogicalDeltaBatch::from_parts_for_test(
            catalog.coordinate_entries().to_vec(),
            [99; 32],
            catalog.commit_marker_identity(),
            CommitSeq(2),
            CommitSeq(2),
        );
        let mut fork = seed().1;
        fork.insert(changed).unwrap();
        assert_eq!(
            state.prepare_next(&fork, LIMBS, &mut allow).unwrap_err(),
            EdgeInputError::HistoryChanged { at: CommitSeq(2) }
        );
        // A fresh epoch, then a graph write prepared before registration.
        // Both are compatible; detached consumers still refuse epoch drift.
        for (seq, epoch, retired, created) in [(3, 1, 1, 4), (4, 0, 2, 5)] {
            let mut entry = coordinate(
                1,
                vec![delete(retired), create(created, 1, 7, 8)],
            );
            entry.schema_epoch = SchemaEpoch(epoch);
            let next = batch(seq, vec![entry]);
            let proof = CommittedSchemaCompatibility::attest_name_only_catalog(
                cx,
                &next,
                GraphId(1),
                BranchId(1),
                SchemaEpoch(1),
            );
            assert_eq!(
                state
                    .prepare_committed_successor(cx, &next, LIMBS, &mut allow)
                    .unwrap_err(),
                EdgeInputError::SchemaChanged
            );
            let delta = state
                .prepare_catalog_successor(cx, &proof, LIMBS, &mut allow)
                .unwrap()
                .commit();
            assert_eq!(delta.weight(&(RelationId(1), VId(7), VId(8))), Some(&ZWeight::ONE));
            assert_eq!(state.frontier(), CommitSeq(seq));
            assert_eq!(state.epochs[&RelationId(1)], SchemaEpoch(epoch));
        }
        assert_eq!(state.edge_count(), 3);
        assert_eq!(
            state.snapshot(LIMBS, &mut allow).unwrap().weight(&(
                RelationId(1),
                VId(7),
                VId(8)
            )),
            Some(&ZWeight::from_i128(2))
        );
    });
}

#[test]
fn catalog_and_epoch_rebinding_cancel_or_drop_without_partial_publication() {
    with_commit_context(|cx| {
        let mut newer = coordinate(1, vec![delete(1), create(4, 1, 7, 8)]);
        newer.schema_epoch = SchemaEpoch(1);
        for next in [catalog_batch(2, 0, 1), batch(2, vec![newer])] {
            let proof = CommittedSchemaCompatibility::attest_name_only_catalog(
                cx,
                &next,
                GraphId(1),
                BranchId(1),
                SchemaEpoch(1),
            );
            let before = seed().0;
            let mut success = seed().0;
            let mut calls = 0;
            let expected = success
                .prepare_catalog_successor(cx, &proof, LIMBS, &mut |_| {
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
                        .prepare_catalog_successor(cx, &proof, LIMBS, &mut |_| {
                            seen += 1;
                            if seen == stop { Err(stop) } else { Ok(()) }
                        })
                        .unwrap_err(),
                    EdgeInputError::Delta(ZSetError::Control(stop))
                );
                assert_eq!(candidate, before);
                let pending = candidate
                    .prepare_catalog_successor(cx, &proof, LIMBS, &mut allow)
                    .unwrap();
                assert_eq!(pending.delta(), &expected);
                drop(pending);
                assert_eq!(candidate, before);
                candidate
                    .prepare_catalog_successor(cx, &proof, LIMBS, &mut allow)
                    .unwrap()
                    .commit();
                assert_eq!(candidate, success);
            }
        }
    });
}

#[test]
fn catalog_attestation_cannot_admit_future_epochs_foreign_scopes_or_other_schema_shapes() {
    with_commit_context(|cx| {
        let good = catalog_batch(2, 0, 1);
        let mut malformed = Vec::new();
        let mut missing = good.coordinate_entries().to_vec();
        missing[0].schema_transition = None;
        malformed.push(missing);
        let mut different = good.coordinate_entries().to_vec();
        different[0].schema_transition = Some(ObjectId([43; 32]));
        malformed.push(different);
        let mut mixed = good.coordinate_entries().to_vec();
        mixed[0].rows.push(create(4, 1, 7, 8));
        malformed.push(mixed);
        let mut another = good.coordinate_entries().to_vec();
        another.push(coordinate(2, vec![create(4, 2, 7, 8)]));
        malformed.push(another);
        let mut wrong_epoch = good.coordinate_entries().to_vec();
        wrong_epoch[0].schema_epoch = SchemaEpoch(1);
        malformed.push(wrong_epoch);
        malformed.push(vec![coordinate(1, vec![DeltaRow::Constraint {
            before_schema_root: ObjectId([1; 32]),
            after_schema_root: ObjectId([2; 32]),
            before_constraint_root: ObjectId([3; 32]),
            after_constraint_root: ObjectId([4; 32]),
        }])]);
        let mut future = coordinate(1, vec![create(4, 1, 7, 8)]);
        future.schema_epoch = SchemaEpoch(2);
        malformed.push(vec![future]);
        for entries in malformed {
            let wrong = LogicalDeltaBatch::from_parts_for_test(
                entries,
                *good.source_template_digest(),
                good.commit_marker_identity(),
                CommitSeq(2),
                CommitSeq(2),
            );
            let proof = CommittedSchemaCompatibility::attest_name_only_catalog(
                cx,
                &wrong,
                GraphId(1),
                BranchId(1),
                SchemaEpoch(1),
            );
            let mut state = seed().0;
            assert_eq!(
                state
                    .prepare_catalog_successor(cx, &proof, LIMBS, &mut allow)
                    .unwrap_err(),
                EdgeInputError::SchemaChanged
            );
            assert_eq!(state, seed().0);
        }
        for (graph, branch, epoch) in [
            (GraphId(2), BranchId(1), SchemaEpoch(1)),
            (GraphId(1), BranchId(2), SchemaEpoch(1)),
            (GraphId(1), BranchId(1), SchemaEpoch(0)),
        ] {
            let proof = CommittedSchemaCompatibility::attest_name_only_catalog(
                cx, &good, graph, branch, epoch,
            );
            let mut state = seed().0;
            assert_eq!(
                state
                    .prepare_catalog_successor(cx, &proof, LIMBS, &mut allow)
                    .unwrap_err(),
                EdgeInputError::SchemaChanged
            );
            assert_eq!(state, seed().0);
        }
    });
}
