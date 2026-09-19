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
