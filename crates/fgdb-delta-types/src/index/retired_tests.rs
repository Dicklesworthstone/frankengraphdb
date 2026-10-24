use super::*;
use fgdb_types::{MarkerRef, ObjectId};

fn batch(seq: u64) -> LogicalDeltaBatch {
    LogicalDeltaBatch::from_parts_for_test(
        vec![],
        [seq as u8; 32],
        MarkerRef {
            marker_oid: ObjectId([seq as u8; 32]),
            commit_seq: CommitSeq(seq),
        },
        CommitSeq(seq),
        CommitSeq(seq),
    )
}

#[test]
fn boundary_identity_survives_empty_window_noop_clone_and_future_insertion() {
    let mut index = LocalDeltaBatchIndex::new();
    for at in 1..=3 {
        index.insert(batch(at)).unwrap();
    }
    let expected = (
        batch(2).format(),
        batch(2).commit_marker_identity(),
        [2; 32],
    );
    let removed = index.retire_prefix(CommitSeq(2)).unwrap();
    assert_eq!(removed.len(), 2);
    assert_eq!(index.retired_boundary_identity(), Some(expected));
    assert_eq!(index.len(), 1);
    assert!(index.get(CommitSeq(2)).is_none());
    index.verify().unwrap();
    assert!(index.retire_prefix(CommitSeq(2)).unwrap().is_empty());
    assert_eq!(index.clone().retired_boundary_identity(), Some(expected));
    let original = index.clone();
    assert!(index.retire_prefix(CommitSeq(4)).is_err());
    assert_eq!(index, original);
    index.retire_prefix(CommitSeq(3)).unwrap();
    assert!(index.is_empty());
    let expected = index.retired_boundary_identity();
    index.insert(batch(4)).unwrap();
    assert_eq!(index.retired_boundary_identity(), expected);
    assert_eq!(index.since(CommitSeq(3)).unwrap().count(), 1);
    assert!(matches!(
        index.since(CommitSeq(2)),
        Err(IndexError::CursorRetired { .. })
    ));
    index.verify().unwrap();
}

#[test]
fn bare_floor_and_malformed_boundaries_cannot_mint_retired_evidence() {
    let mut bare = LocalDeltaBatchIndex::from_parts_for_test(CommitSeq(7), CommitSeq(7), vec![]);
    assert!(bare.retire_prefix(CommitSeq(7)).unwrap().is_empty());
    assert!(bare.retired_boundary_identity().is_none());
    for entries in [vec![], vec![(CommitSeq(2), batch(1))]] {
        let mut malformed =
            LocalDeltaBatchIndex::from_parts_for_test(CommitSeq(0), CommitSeq(2), entries);
        let before = malformed.clone();
        assert!(malformed.retire_prefix(CommitSeq(2)).is_err());
        assert_eq!(malformed, before);
        assert!(malformed.retired_boundary_identity().is_none());
    }
    let bad = LogicalDeltaBatch::from_parts_for_test(
        vec![],
        [2; 32],
        batch(1).commit_marker_identity(),
        CommitSeq(2),
        CommitSeq(2),
    );
    let mut malformed = LocalDeltaBatchIndex::from_parts_for_test(
        CommitSeq(0),
        CommitSeq(2),
        vec![(CommitSeq(1), batch(1)), (CommitSeq(2), bad)],
    );
    let before = malformed.clone();
    assert!(matches!(
        malformed.retire_prefix(CommitSeq(2)),
        Err(IndexError::WrongMarker { .. })
    ));
    assert_eq!(malformed, before);
}

#[test]
fn authenticated_empty_boundary_matches_retirement_without_fabricating_rows() {
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::PurposeContexts;

    let ((), report) = run_async_under_lab(0xa670, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let anchor = batch(7);
        let mut recovered = LocalDeltaBatchIndex::empty_at_committed(
            crate::CommittedMarker::attest(anchor.commit_marker_identity(), &cx),
            *anchor.source_template_digest(),
        )
        .unwrap();
        assert_eq!(recovered.frontier(), CommitSeq(7));
        assert_eq!(recovered.retained_after_commit_seq(), CommitSeq(7));
        assert_eq!(
            recovered.retired_boundary_identity(),
            Some((anchor.format(), anchor.commit_marker_identity(), [7; 32]))
        );
        assert!(recovered.get(CommitSeq(7)).is_none());
        assert!(recovered.is_empty());
        assert_eq!(recovered.since(CommitSeq(7)).unwrap().count(), 0);
        assert!(matches!(
            recovered.since(CommitSeq(6)),
            Err(IndexError::CursorRetired { .. })
        ));
        recovered.insert(batch(8)).unwrap();
        recovered.verify().unwrap();
        assert_eq!(recovered.since(CommitSeq(7)).unwrap().count(), 1);
        assert!(matches!(
            LocalDeltaBatchIndex::empty_at_committed(
                crate::CommittedMarker::attest(batch(0).commit_marker_identity(), &cx),
                [0; 32],
            ),
            Err(IndexError::OriginAnchor)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
