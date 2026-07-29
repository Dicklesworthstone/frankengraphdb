//! Laws of the bounded delta window.
//!
//! plan:397 enumerates exactly how an insertion fails — "a **missing,
//! duplicate, gapped, wrong-marker, or wrong-frontier** insertion fails apply"
//! — so this file has one law per named mode and no invented ones.
//!
//! The structural claim underneath them is that the entry and the frontier
//! advance in the SAME transition, so no state exists where a commit has
//! happened and its batch is unreachable. The strongest way to test that is not
//! an assertion at all: it is that the API offers no way to do one without the
//! other. What the tests can add is that every REFUSED insertion leaves the
//! index byte-identical, since a partially-applied refusal would reintroduce
//! exactly the interval the law forbids.

use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{
    CommittedMarker, CoordinateEntry, DeltaRow, IndexError, LabelId, LocalDeltaBatchIndex,
    LogicalDeltaBatch, LogicalDeltaTemplate, PropertyKeyId, RelationId, SchemaEpoch,
};
use fgdb_types::{
    BranchId, CanonicalScalar, CommitCx, CommitSeq, GraphId, MarkerRef, ObjectId, PurposeContexts,
    VId,
};

fn with_commit_cx<T, F>(seed: u64, run: F) -> T
where
    T: Send + 'static,
    F: FnOnce(CommitCx) -> T + Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        run(contexts.commit())
    });
    assert!(
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    output
}

fn template(vid: u128) -> LogicalDeltaTemplate {
    LogicalDeltaTemplate::build(
        ObjectId([0x11; 32]),
        [0x22; 32],
        vec![CoordinateEntry {
            graph: GraphId(1),
            branch: BranchId(1),
            relation: RelationId(3),
            schema_epoch: SchemaEpoch(0),
            schema_transition: None,
            rows: vec![DeltaRow::CreateVertex {
                vid: VId(vid),
                birth_ordinal: vid as u64,
                labels: vec![LabelId(1)],
                props: vec![(PropertyKeyId(1), CanonicalScalar::Int(vid as i64))],
                valid_time: None,
            }],
        }],
    )
    .expect("template builds")
}

/// A well-formed local batch at `seq`: its marker names that sequence and its
/// frontier is that sequence.
fn batch_at(seq: u64, cx: &CommitCx) -> LogicalDeltaBatch {
    LogicalDeltaBatch::order(
        &template(seq as u128),
        [0x33; 32],
        CommittedMarker::attest(
            MarkerRef {
                marker_oid: ObjectId([seq as u8; 32]),
                commit_seq: CommitSeq(seq),
            },
            cx,
        ),
    )
}

#[test]
fn a_fresh_index_is_an_empty_window_at_the_origin() {
    let index = LocalDeltaBatchIndex::new();
    assert_eq!(index.frontier(), CommitSeq(0));
    assert_eq!(index.retained_after_commit_seq(), CommitSeq(0));
    assert!(index.is_empty());
    assert_eq!(index.next_commit_seq(), CommitSeq(1));
    assert_eq!(index.verify(), Ok(()));
}

/// THE STRUCTURAL LAW: inserting a batch advances the frontier in the same
/// call. There is no API that does one without the other.
#[test]
fn insertion_and_the_frontier_advance_together() {
    with_commit_cx(0x1DE1, |cx| {
        let mut index = LocalDeltaBatchIndex::new();
        for seq in 1..=5u64 {
            assert_eq!(index.next_commit_seq(), CommitSeq(seq));
            index.insert(batch_at(seq, &cx)).expect("insert");
            assert_eq!(
                index.frontier(),
                CommitSeq(seq),
                "the frontier reaches the batch that was just inserted"
            );
            assert!(
                index.get(CommitSeq(seq)).is_some(),
                "and the batch is reachable at that sequence"
            );
            assert_eq!(index.verify(), Ok(()), "the window stays exact");
        }
        assert_eq!(index.len(), 5);
    });
}

// ---------------------------------------------------------------------------
// plan:397's five named failure modes
// ---------------------------------------------------------------------------

/// GAPPED (and MISSING, which is the same defect seen from the other side): a
/// sequence beyond `frontier + 1` would leave a hole, making "the deltas since
/// N" unanswerable across it.
#[test]
fn a_gapped_insertion_is_refused_and_changes_nothing() {
    with_commit_cx(0x1DE2, |cx| {
        let mut index = LocalDeltaBatchIndex::new();
        index.insert(batch_at(1, &cx)).expect("first");
        let settled = index.clone();

        let result = index.insert(batch_at(3, &cx));
        assert_eq!(
            result,
            Err(IndexError::Gapped {
                expected: CommitSeq(2),
                found: CommitSeq(3),
            })
        );
        assert_eq!(
            index, settled,
            "a refused insertion leaves the index intact"
        );

        // And the sequence it wanted still works.
        index.insert(batch_at(2, &cx)).expect("the gap closes");
        index.insert(batch_at(3, &cx)).expect("then 3 fits");
        assert_eq!(index.frontier(), CommitSeq(3));
    });
}

/// DUPLICATE: re-inserting a covered sequence must be refused rather than
/// replace. Replacing is the worse outcome — the index would stop agreeing with
/// the commit stream while still looking gap-free.
#[test]
fn a_duplicate_insertion_is_refused_and_does_not_replace() {
    with_commit_cx(0x1DE3, |cx| {
        let mut index = LocalDeltaBatchIndex::new();
        index.insert(batch_at(1, &cx)).expect("first");
        index.insert(batch_at(2, &cx)).expect("second");
        let settled = index.clone();

        assert_eq!(
            index.insert(batch_at(2, &cx)),
            Err(IndexError::Duplicate {
                frontier: CommitSeq(2),
                found: CommitSeq(2),
            })
        );
        assert_eq!(
            index.insert(batch_at(1, &cx)),
            Err(IndexError::Duplicate {
                frontier: CommitSeq(2),
                found: CommitSeq(1),
            })
        );
        assert_eq!(index, settled);
    });
}

/// WRONG-FRONTIER: a local batch must declare its own commit sequence as its
/// frontier (plan:1926, `frontier: Local{commit_seq}`). `order` guarantees
/// that, so this law needs a batch built the way a DECODER would build one —
/// from parts, with no construction guarantee behind it.
#[test]
fn a_batch_whose_frontier_is_not_its_own_sequence_is_refused() {
    with_commit_cx(0x1DE4, |cx| {
        let well_formed = batch_at(1, &cx);
        let malformed = LogicalDeltaBatch::from_parts_for_test(
            well_formed.coordinate_entries().to_vec(),
            *well_formed.source_template_digest(),
            well_formed.commit_marker_identity(),
            CommitSeq(1),
            CommitSeq(9), // a frontier that is not this batch's sequence
        );

        let mut index = LocalDeltaBatchIndex::new();
        let settled = index.clone();
        assert_eq!(
            index.insert(malformed),
            Err(IndexError::WrongFrontier {
                commit_seq: CommitSeq(1),
                frontier: CommitSeq(9),
            })
        );
        assert_eq!(index, settled, "a refused insertion changes nothing");

        // The well-formed batch at the same sequence still inserts, so the
        // refusal was about the defect and not about the sequence.
        index.insert(well_formed).expect("well-formed inserts");
        assert_eq!(index.frontier(), CommitSeq(1));
    });
}

/// WRONG-MARKER: the batch's marker must name the batch's own sequence. If they
/// disagree the index cannot tell which is right, so it refuses rather than
/// pick — and picking is what a lenient index would do, leaving the delta
/// stream and the commit stream describing different histories.
#[test]
fn a_batch_whose_marker_names_another_sequence_is_refused() {
    with_commit_cx(0x1DE5, |cx| {
        let well_formed = batch_at(1, &cx);
        let malformed = LogicalDeltaBatch::from_parts_for_test(
            well_formed.coordinate_entries().to_vec(),
            *well_formed.source_template_digest(),
            MarkerRef {
                marker_oid: ObjectId([0xEE; 32]),
                commit_seq: CommitSeq(77), // names a different commit
            },
            CommitSeq(1),
            CommitSeq(1),
        );

        let mut index = LocalDeltaBatchIndex::new();
        let settled = index.clone();
        assert_eq!(
            index.insert(malformed),
            Err(IndexError::WrongMarker {
                batch_commit_seq: CommitSeq(1),
                marker_commit_seq: CommitSeq(77),
            })
        );
        assert_eq!(index, settled);
    });
}

/// And the safe constructor cannot produce either defect — which is why the two
/// laws above need `from_parts_for_test` at all.
#[test]
fn the_safe_constructor_cannot_build_an_inconsistent_batch() {
    with_commit_cx(0x1DEA, |cx| {
        for seq in 1..=3u64 {
            let batch = batch_at(seq, &cx);
            assert_eq!(
                batch.commit_marker_identity().commit_seq,
                batch.commit_seq()
            );
            assert_eq!(batch.frontier(), batch.commit_seq());
        }
    });
}

// ---------------------------------------------------------------------------
// Retirement keeps the window exact
// ---------------------------------------------------------------------------

/// Retiring a prefix moves the floor and hands back exactly what it dropped —
/// the consumer owes a commitment over precisely those batches and is the only
/// layer that can hash them.
#[test]
fn retiring_a_prefix_returns_what_it_dropped_and_keeps_the_window_exact() {
    with_commit_cx(0x1DE6, |cx| {
        let mut index = LocalDeltaBatchIndex::new();
        for seq in 1..=6u64 {
            index.insert(batch_at(seq, &cx)).expect("insert");
        }

        let retired = index.retire_prefix(CommitSeq(3)).expect("retire");
        let retired_seqs: Vec<u64> = retired.iter().map(|b| b.commit_seq().0).collect();
        assert_eq!(retired_seqs, vec![1, 2, 3], "in commit order");

        assert_eq!(index.retained_after_commit_seq(), CommitSeq(3));
        assert_eq!(index.frontier(), CommitSeq(6));
        assert_eq!(index.len(), 3, "the window is (3, 6]");
        assert!(index.get(CommitSeq(3)).is_none());
        assert!(index.get(CommitSeq(4)).is_some());
        assert_eq!(index.verify(), Ok(()));

        // The frontier is untouched, so insertion continues from where it was.
        assert_eq!(index.next_commit_seq(), CommitSeq(7));
        index.insert(batch_at(7, &cx)).expect("still writable");
        assert_eq!(index.verify(), Ok(()));
    });
}

#[test]
fn retiring_past_the_frontier_or_backwards_is_refused() {
    with_commit_cx(0x1DE7, |cx| {
        let mut index = LocalDeltaBatchIndex::new();
        for seq in 1..=3u64 {
            index.insert(batch_at(seq, &cx)).expect("insert");
        }
        index.retire_prefix(CommitSeq(2)).expect("retire to 2");
        let settled = index.clone();

        assert!(matches!(
            index.retire_prefix(CommitSeq(9)),
            Err(IndexError::UnretirableInterval { .. })
        ));
        assert!(
            matches!(
                index.retire_prefix(CommitSeq(1)),
                Err(IndexError::UnretirableInterval { .. })
            ),
            "the retained floor must not move backwards"
        );
        assert_eq!(index, settled);
    });
}

/// Retiring the whole window leaves an empty but coherent index that still
/// accepts the next commit — the empty case is not a special state.
#[test]
fn retiring_everything_leaves_a_coherent_empty_window() {
    with_commit_cx(0x1DE8, |cx| {
        let mut index = LocalDeltaBatchIndex::new();
        for seq in 1..=4u64 {
            index.insert(batch_at(seq, &cx)).expect("insert");
        }
        let retired = index.retire_prefix(CommitSeq(4)).expect("retire all");
        assert_eq!(retired.len(), 4);
        assert!(index.is_empty());
        assert_eq!(index.retained_after_commit_seq(), CommitSeq(4));
        assert_eq!(index.frontier(), CommitSeq(4));
        assert_eq!(index.verify(), Ok(()));

        index.insert(batch_at(5, &cx)).expect("still writable");
        assert_eq!(index.len(), 1);
        assert_eq!(index.verify(), Ok(()));
    });
}

/// `verify` must actually CATCH a broken window, not merely pass on good ones.
///
/// The interesting case is a window whose frontier claims more than its entries
/// hold — which is what a decoder would produce from bytes where an entry was
/// lost. Built here from parts, since the safe API cannot reach that state.
#[test]
fn verify_catches_a_window_whose_frontier_outruns_its_entries() {
    with_commit_cx(0x1DE9, |cx| {
        let mut index = LocalDeltaBatchIndex::new();
        for seq in 1..=3u64 {
            index.insert(batch_at(seq, &cx)).expect("insert");
        }
        assert_eq!(index.verify(), Ok(()), "the coherent window verifies");

        // Retiring is coherent and stays verifiable.
        index.retire_prefix(CommitSeq(1)).expect("retire");
        assert_eq!(index.len(), 2);
        assert_eq!(index.verify(), Ok(()));

        // A window that lost an interior entry must NOT verify. There is no
        // safe way to reach this, so the control is that verify() distinguishes
        // it from the coherent window above.
        let broken = LocalDeltaBatchIndex::from_parts_for_test(
            CommitSeq(1),
            CommitSeq(3),
            vec![batch_at(3, &cx)], // seq 2 is missing
        );
        assert!(
            matches!(broken.verify(), Err(IndexError::Gapped { .. })),
            "a window missing an interior entry must not verify"
        );
    });
}

/// An empty interval is coherent only when its two endpoints are equal.
/// `saturating_sub` would turn an inverted `(4, 3]` window into length zero
/// and let this decoder-facing corruption pass as a valid empty index.
#[test]
fn verify_refuses_an_inverted_empty_window() {
    let broken = LocalDeltaBatchIndex::from_parts_for_test(CommitSeq(4), CommitSeq(3), Vec::new());
    assert_eq!(
        broken.verify(),
        Err(IndexError::UnretirableInterval {
            retained_after: CommitSeq(4),
            frontier: CommitSeq(3),
            requested: CommitSeq(4),
        })
    );
}

/// Exact key coverage is not enough: durable decoding bypasses `insert`, so
/// `verify` must re-check that the retained batch's marker names its own
/// sequence even when the window has the expected key and cardinality.
#[test]
fn verify_refuses_a_same_cardinality_batch_with_the_wrong_marker() {
    with_commit_cx(0x1DEB, |cx| {
        let well_formed = batch_at(1, &cx);
        let malformed = LogicalDeltaBatch::from_parts_for_test(
            well_formed.coordinate_entries().to_vec(),
            *well_formed.source_template_digest(),
            MarkerRef {
                marker_oid: ObjectId([0xEE; 32]),
                commit_seq: CommitSeq(77),
            },
            CommitSeq(1),
            CommitSeq(1),
        );
        let broken =
            LocalDeltaBatchIndex::from_parts_for_test(CommitSeq(0), CommitSeq(1), vec![malformed]);
        assert_eq!(
            broken.verify(),
            Err(IndexError::WrongMarker {
                batch_commit_seq: CommitSeq(1),
                marker_commit_seq: CommitSeq(77),
            })
        );
    });
}

/// The same decoder-facing control for a batch whose own frontier disagrees
/// with its sequence. The index key/count still exactly cover `(0, 1]`, so
/// only intrinsic batch validation can catch it.
#[test]
fn verify_refuses_a_same_cardinality_batch_with_the_wrong_frontier() {
    with_commit_cx(0x1DEC, |cx| {
        let well_formed = batch_at(1, &cx);
        let malformed = LogicalDeltaBatch::from_parts_for_test(
            well_formed.coordinate_entries().to_vec(),
            *well_formed.source_template_digest(),
            well_formed.commit_marker_identity(),
            CommitSeq(1),
            CommitSeq(9),
        );
        let broken =
            LocalDeltaBatchIndex::from_parts_for_test(CommitSeq(0), CommitSeq(1), vec![malformed]);
        assert_eq!(
            broken.verify(),
            Err(IndexError::WrongFrontier {
                commit_seq: CommitSeq(1),
                frontier: CommitSeq(9),
            })
        );
    });
}
