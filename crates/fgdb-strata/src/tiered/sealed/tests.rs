use super::*;
use crate::edge_props::{BlockProps, EdgePropertyRow};
use fgdb_types::{EId, ObjectId};

fn scope(floor: u64) -> SealedScope {
    SealedScope { source_root: PartitionRootVersion(ObjectId([7; 32])), graph: GraphId(1),
        branch: BranchId(2), partition: 3, floor: CommitSeq(floor), publication: CommitSeq(10) }
}

fn entry(src: u128, dst: u128, eid: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry { src: VId(src), relation: RelationId(1), dst: VId(dst), eid: EId(eid),
        created_at: CommitSeq(created), retired_at: retired.map(CommitSeq) }
}

fn seal(blocks: &[Vec<AdjacencyEntry>], properties: &[Option<BlockProps>], floor: u64) -> SealedPartition {
    let compacted = crate::compact::compact_with_props(blocks, properties, CommitSeq(floor)).unwrap();
    let image = image::build(compacted, SealedLimits::default(), &mut || Ok(())).unwrap();
    SealedPartition::finish(scope(floor), image, SealedLimits::default(), &mut || Ok(())).unwrap()
}

fn collect(partition: &SealedPartition, src: u128, as_of: u64, lower: Option<u128>) -> Vec<(AdjacencyEntry, EdgePropertyRow)> {
    partition.anchor.authorize(CommitSeq(as_of)).unwrap();
    let row = partition.image.find_row(VId(src), RelationId(1));
    let position = match (row, lower) {
        (Some(row), Some(dst)) => row.lower_bound(&partition.image, VId(dst)),
        _ => 0,
    };
    let mut cursor = SealedCursor { image: &partition.image, row, position, as_of: CommitSeq(as_of), finished: false };
    let mut entries = Vec::new();
    while let Some(edge) = cursor.next_inner(&mut || Ok(())).unwrap() {
        entries.push((edge.entry, edge.properties.to_vec()));
    }
    entries
}

fn bytes(partition: &SealedPartition) -> Vec<u8> {
    wire::encode(&partition.image, SealedLimits::default(), &mut || Ok(())).unwrap()
}

#[test]
fn real_compaction_to_mixed_tiers_preserves_every_retained_snapshot() {
    let large: Vec<_> = (0..40).map(|i| entry(1, i / 3, i + 1, 1 + (i % 3) as u64,
        if i % 2 == 0 { Some(8) } else { None })).collect();
    let small: Vec<_> = (0..4).map(|i| entry(2, i, 100 + i, 1, None)).collect();
    let source = vec![large.clone(), small.clone()];
    let partition = seal(&source, &[None, None], 3);
    assert_eq!(partition.storage_kind(VId(1), RelationId(1)), Some(RowStorageKind::SealedCsr));
    assert_eq!(partition.storage_kind(VId(2), RelationId(1)), Some(RowStorageKind::Inline));
    assert_eq!(partition.stats().incidences, 44);
    for as_of in 3..=10 {
        for (src, entries) in [(1, &large), (2, &small)] {
            let expected: Vec<_> = entries.iter().copied().filter(|entry| entry.visible_at(CommitSeq(as_of))).collect();
            let actual: Vec<_> = collect(&partition, src, as_of, None).into_iter().map(|(entry, _)| entry).collect();
            assert_eq!(actual, expected);
        }
    }
    assert!(partition.anchor.authorize(CommitSeq(2)).is_err());
    assert!(partition.anchor.authorize(CommitSeq(11)).is_err());
}

#[test]
fn tombstone_precedence_and_content_version_properties_use_the_existing_compactor() {
    let initial: Vec<_> = (1..=9).map(|eid| entry(1, 7, eid, 1, None)).collect();
    let initial_props = BlockProps { locators: (1..=9).collect(), rows: (1..=9)
        .map(|eid| vec![(PropertyKeyId(1), CanonicalScalar::Int(eid))]).collect() };
    let change = vec![entry(1, 7, 1, 1, Some(5)), entry(1, 7, 1, 5, None)];
    let changed_props = BlockProps { locators: vec![1, 2], rows: vec![
        vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(99))],
    ] };
    let partition = seal(&[initial, change], &[Some(initial_props), Some(changed_props)], 1);
    assert_eq!(partition.stats().incidences, 10);
    assert_eq!(collect(&partition, 1, 4, None)[0].1, vec![(PropertyKeyId(1), CanonicalScalar::Int(1))]);
    assert_eq!(collect(&partition, 1, 5, None)[0].1, vec![(PropertyKeyId(1), CanonicalScalar::Int(99))]);
    for cut in [4, 5, 10] { assert_eq!(collect(&partition, 1, cut, None).len(), 9); }
    let serialized = bytes(&partition);
    let reloaded = SealedPartition::reload_inner(partition.anchor, &serialized,
        SealedLimits::default(), None, &mut || Ok(())).unwrap();
    for cut in 1..=10 { assert_eq!(collect(&partition, 1, cut, None), collect(&reloaded, 1, cut, None)); }
}

#[test]
fn floor_drops_can_demote_a_row_without_erasing_observable_history() {
    let entries: Vec<_> = (1..=9).map(|eid| entry(1, eid, eid, 1, (eid == 1).then_some(5))).collect();
    let before = seal(core::slice::from_ref(&entries), &[None], 4);
    let after = seal(&[entries], &[None], 5);
    assert_eq!(before.storage_kind(VId(1), RelationId(1)), Some(RowStorageKind::SealedCsr));
    assert_eq!(after.storage_kind(VId(1), RelationId(1)), Some(RowStorageKind::Inline));
    for cut in 5..=10 { assert_eq!(collect(&before, 1, cut, None), collect(&after, 1, cut, None)); }
    assert_eq!(collect(&before, 1, 4, None).len(), 9);
    assert!(after.anchor.authorize(CommitSeq(4)).is_err());
}

#[test]
fn lower_bound_keeps_all_parallel_incidence_ranges_on_both_tiers() {
    let large: Vec<_> = (0..30).map(|i| entry(1, i / 3, i + 1, 1, None)).collect();
    let small: Vec<_> = (0..6).map(|i| entry(2, i / 3, i + 100, 1, None)).collect();
    let partition = seal(&[large, small], &[None, None], 1);
    for src in [1, 2] {
        let all = collect(&partition, src, 5, None);
        for lower in 0..=11 {
            let expected: Vec<_> = all.iter().filter(|(entry, _)| entry.dst >= VId(lower)).cloned().collect();
            assert_eq!(collect(&partition, src, 5, Some(lower)), expected);
        }
    }
}

#[test]
fn canonical_bytes_round_trip_and_corruption_never_mints_source_authority() {
    let partition = seal(&[vec![entry(1, 2, 1, 1, None)]], &[None], 1);
    let encoded = bytes(&partition);
    let reloaded = SealedPartition::reload_inner(partition.anchor, &encoded,
        SealedLimits::default(), None, &mut || Ok(())).unwrap();
    assert_eq!(bytes(&reloaded), encoded);
    assert_eq!(reloaded.anchor, partition.anchor);
    for at in 0..encoded.len() {
        let mut damaged = encoded.clone();
        damaged[at] ^= 1;
        assert!(matches!(SealedPartition::reload_inner(partition.anchor, &damaged,
            SealedLimits::default(), None, &mut || Ok(())), Err(SealedError::ImageMismatch)));
        assert!(wire::decode(&encoded[..at], SealedLimits::default(), None, &mut || Ok(())).is_err());
    }
    let mut extra = encoded.clone();
    extra.push(0);
    assert!(matches!(wire::decode(&extra, SealedLimits::default(), None, &mut || Ok(())), Err(SealedError::TrailingBytes)));
    let mut forged = encoded;
    forged[6..10].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(wire::decode(&forged, SealedLimits::default(), None, &mut || Ok(())), Err(SealedError::Limit { .. })));
}

#[test]
fn cloning_shares_payloads_but_retaining_an_anchor_does_not_pin_them() {
    let partition = seal(&[vec![entry(1, 2, 1, 1, None)]], &[None], 1);
    let cloned = partition.clone();
    assert!(partition.shares_image_with(&cloned));
    let weak = Arc::downgrade(&partition.image);
    let anchor = partition.anchor();
    let encoded = bytes(&partition);
    drop(partition);
    assert!(weak.upgrade().is_some());
    drop(cloned);
    assert!(weak.upgrade().is_none());
    let reloaded = SealedPartition::reload_inner(anchor, &encoded,
        SealedLimits::default(), None, &mut || Ok(())).unwrap();
    assert_eq!(collect(&reloaded, 1, 5, None).len(), 1);
}

#[test]
fn property_chunks_cross_the_existing_255_row_boundary_without_locator_loss() {
    let mut blocks = Vec::new();
    let mut props = Vec::new();
    for chunk in 0..5 {
        blocks.push((0..120).map(|i| entry(1, (chunk * 120 + i) as u128,
            (chunk * 120 + i + 1) as u128, 1, None)).collect());
        props.push(Some(BlockProps { locators: (1..=120).collect(), rows: (0..120)
            .map(|i| vec![(PropertyKeyId(1), CanonicalScalar::Int(chunk * 120 + i))]).collect() }));
    }
    let partition = seal(&blocks, &props, 1);
    assert_eq!(partition.stats().property_rows, 600);
    let encoded = bytes(&partition);
    let reloaded = SealedPartition::reload_inner(partition.anchor, &encoded,
        SealedLimits::default(), None, &mut || Ok(())).unwrap();
    let rows = collect(&reloaded, 1, 10, None);
    assert_eq!(rows.len(), 600);
    for (index, (_, properties)) in rows.iter().enumerate() {
        assert_eq!(properties, &vec![(PropertyKeyId(1), CanonicalScalar::Int(index as i64))]);
    }
}

#[test]
fn empty_image_is_canonical_and_resource_limits_remain_explicit() {
    let empty = seal(&[], &[], 0);
    let encoded = bytes(&empty);
    let reloaded = SealedPartition::reload_inner(empty.anchor, &encoded,
        SealedLimits::default(), None, &mut || Ok(())).unwrap();
    assert_eq!(reloaded.stats().incidences, 0);
    assert!(collect(&reloaded, 9, 0, None).is_empty());
    assert!(matches!(wire::encode(&empty.image, SealedLimits { max_image_bytes: encoded.len() - 1,
        ..SealedLimits::default() }, &mut || Ok(())), Err(SealedError::Limit { .. })));
    let one = seal(&[vec![entry(1, 2, 1, 1, None)]], &[None], 1);
    assert!(matches!(wire::decode(&bytes(&one), SealedLimits { max_incidences: 0,
        ..SealedLimits::default() }, None, &mut || Ok(())), Err(SealedError::Limit { .. })));
}

#[test]
fn interrupted_decode_and_scan_do_not_report_a_successful_prefix() {
    let partition = seal(&[vec![entry(1, 2, 1, 1, None)]], &[None], 1);
    assert!(wire::decode(&bytes(&partition), SealedLimits::default(), None,
        &mut || Err(SealedError::InvalidFloor)).is_err());
    let mut cursor = SealedCursor { image: &partition.image,
        row: partition.image.find_row(VId(1), RelationId(1)), position: 0,
        as_of: CommitSeq(5), finished: false };
    assert!(cursor.next_inner(&mut || Err(SealedError::InvalidFloor)).is_err());
    assert!(cursor.finished);
    assert_eq!(cursor.position, 0);
}
