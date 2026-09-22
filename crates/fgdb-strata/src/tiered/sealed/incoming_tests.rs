use super::super::{SealedLimits, SealedScope, image, wire};
use super::*;
use crate::compact::Compaction;
use crate::edge_props::{BlockProps, EdgePropertyRow};
use crate::{AdjacencyEntry, PartitionRootVersion};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, ObjectId};

fn entry(source: u128, target: u128, id: u128, relation: u64) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(source),
        dst: VId(target),
        eid: EId(id),
        relation: RelationId(relation),
        created_at: CommitSeq(1),
        retired_at: None,
    }
}

fn seal(entries: &[AdjacencyEntry]) -> SealedPartition {
    let mut blocks = Vec::new();
    let mut block_props = Vec::new();
    for chunk in entries.chunks(120) {
        blocks.push(chunk.to_vec());
        block_props.push(Some(BlockProps {
            locators: (1..=chunk.len()).map(|value| value as u8).collect(),
            rows: chunk
                .iter()
                .map(|entry| {
                    vec![(
                        PropertyKeyId(1),
                        CanonicalScalar::Int(entry.created_at.0 as i64),
                    )]
                })
                .collect(),
        }));
    }
    let image = image::build(
        Compaction {
            blocks,
            block_props,
            dropped: 0,
            superseded: 0,
        },
        SealedLimits::default(),
        &mut || Ok(()),
    )
    .unwrap();
    SealedPartition::finish(
        SealedScope {
            source_root: PartitionRootVersion(ObjectId([7; 32])),
            graph: GraphId(1),
            branch: BranchId(2),
            partition: 3,
            floor: CommitSeq(1),
            publication: CommitSeq(10),
        },
        image,
        SealedLimits::default(),
        &mut || Ok(()),
    )
    .unwrap()
}

fn index(source: &SealedPartition) -> SealedIncomingIndex {
    SealedIncomingIndex::build(source, IncomingIndexLimits::default(), &mut || Ok(())).unwrap()
}

fn collect(
    index: &SealedIncomingIndex,
    destination: VId,
    relation: RelationId,
    cut: CommitSeq,
    lower: Option<VId>,
) -> Vec<(AdjacencyEntry, EdgePropertyRow)> {
    let mut cursor = index
        .open(destination, relation, cut, lower, &mut || Ok(()))
        .unwrap();
    let mut rows = Vec::new();
    while let Some(edge) = cursor.next_inner(&mut || Ok(())).unwrap() {
        rows.push((edge.entry, edge.properties.to_vec()));
    }
    rows
}

#[test]
fn every_small_topology_transposes_exactly_and_preserves_original_edge_orientation() {
    let ids = [0, 17, u128::MAX];
    for mask in 0u16..512 {
        let entries: Vec<_> = (0..9)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| entry(ids[bit / 3], ids[bit % 3], bit as u128, 1))
            .collect();
        let source = seal(&entries);
        let incoming = index(&source);
        assert_eq!(incoming.source_anchor(), source.anchor());
        assert!(incoming.source().shares_image_with(&source));
        assert_eq!(incoming.stats().incidences, entries.len());
        for &target in &ids {
            let mut expected: Vec<_> = entries
                .iter()
                .copied()
                .filter(|entry| entry.dst == VId(target))
                .collect();
            expected.sort_by_key(|entry| (entry.src, entry.eid, entry.created_at));
            let actual = collect(&incoming, VId(target), RelationId(1), CommitSeq(3), None);
            assert_eq!(
                actual.iter().map(|(entry, _)| *entry).collect::<Vec<_>>(),
                expected,
                "mask={mask} target={target}"
            );
            for (entry, properties) in actual {
                assert_eq!(entry.dst, VId(target));
                assert_eq!(
                    properties,
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(1))]
                );
            }
        }
        // The permutation covers every retained source incidence exactly once.
        let mut positions: Vec<_> = incoming
            .index
            .chunks
            .iter()
            .flat_map(|chunk| (0..chunk.len()).map(|at| chunk.select(at).unwrap()))
            .collect();
        positions.sort_unstable();
        assert_eq!(positions, (0..entries.len() as u64).collect::<Vec<_>>());
    }
}

#[test]
fn history_relations_parallel_edges_and_self_loops_keep_source_properties() {
    let mut old = entry(1, 9, 7, 1);
    old.retired_at = Some(CommitSeq(5));
    let mut new = old;
    new.created_at = CommitSeq(5);
    new.retired_at = None;
    let entries = [
        old,
        new,
        entry(1, 9, 8, 1),
        entry(2, 9, 9, 2),
        entry(9, 9, 10, 1),
    ];
    let source = seal(&entries);
    let incoming = index(&source);
    assert_eq!(incoming.retained_row_len(VId(9), RelationId(1)), 4);
    for cut in 1..=10 {
        let actual = collect(&incoming, VId(9), RelationId(1), CommitSeq(cut), None);
        assert_eq!(actual.len(), 3);
        assert_eq!(actual[0].0.eid, EId(7));
        assert_eq!(
            actual[0].1,
            vec![(
                PropertyKeyId(1),
                CanonicalScalar::Int(if cut < 5 { 1 } else { 5 })
            )]
        );
        let mut cursor = incoming
            .open(VId(9), RelationId(1), CommitSeq(cut), None, &mut || Ok(()))
            .unwrap();
        while let Some(edge) = cursor.next_inner(&mut || Ok(())).unwrap() {
            let row = source
                .image
                .find_row(edge.entry.src, edge.entry.relation)
                .unwrap();
            let at = (0..row.len())
                .find(|&at| row.incidence(&source.image, at).unwrap().0 == edge.entry)
                .unwrap();
            let (_, locator) = row.incidence(&source.image, at).unwrap();
            assert!(
                std::ptr::eq(
                    edge.properties,
                    source.image.properties[locator as usize - 1].as_slice()
                ),
                "incoming properties must borrow original storage, not cloned sidecars"
            );
        }
    }
    assert_eq!(
        collect(&incoming, VId(9), RelationId(2), CommitSeq(5), None).len(),
        1
    );
    assert!(collect(&incoming, VId(9), RelationId(99), CommitSeq(5), None).is_empty());
}

#[test]
fn full_width_lower_bounds_seek_across_ef_chunk_boundaries() {
    let mut entries: Vec<_> = (0..600u128)
        .rev()
        .map(|i| entry(i * 17, u128::MAX, i, 1))
        .collect();
    entries.extend((0..40u128).map(|i| entry(u128::MAX, i, 1000 + i, 2)));
    let source = seal(&entries);
    let incoming = index(&source);
    let all = collect(&incoming, VId(u128::MAX), RelationId(1), CommitSeq(5), None);
    assert_eq!(all.len(), 600);
    let row = incoming.find(VId(u128::MAX), RelationId(1)).unwrap();
    assert_eq!(row.end_chunk - row.first_chunk, 3);
    for lower in [
        0,
        1,
        17,
        255 * 17,
        256 * 17,
        256 * 17 + 1,
        511 * 17,
        512 * 17,
        599 * 17,
        600 * 17,
        u128::MAX,
    ] {
        let expected: Vec<_> = all
            .iter()
            .filter(|(entry, _)| entry.src >= VId(lower))
            .cloned()
            .collect();
        assert_eq!(
            collect(
                &incoming,
                VId(u128::MAX),
                RelationId(1),
                CommitSeq(5),
                Some(VId(lower))
            ),
            expected
        );
    }
    for chunk in &incoming.index.chunks {
        assert!(chunk.len() <= CHUNK_ENTRIES);
        let logical = chunk.logical_storage_words() * size_of::<u64>();
        let charged = ef_bytes(chunk.len(), chunk.max_value().unwrap_or(0)).unwrap();
        assert!(logical >= charged && logical - charged < size_of::<u64>());
    }
}

#[test]
fn workspace_history_and_descriptor_limits_are_exact_and_checked_before_publication() {
    let source = seal(&[entry(1, 7, 1, 1), entry(2, 8, 2, 1), entry(3, 8, 3, 1)]);
    let expected = index(&source).stats();
    let exact = IncomingIndexLimits {
        max_rows: 2,
        max_incidences: 3,
        max_workspace_bytes: expected.charged_workspace_bytes,
    };
    assert_eq!(
        SealedIncomingIndex::build(&source, exact, &mut || Ok(()))
            .unwrap()
            .stats(),
        expected
    );
    for limits in [
        IncomingIndexLimits {
            max_rows: 1,
            ..exact
        },
        IncomingIndexLimits {
            max_incidences: 2,
            ..exact
        },
        IncomingIndexLimits {
            max_workspace_bytes: exact.max_workspace_bytes - 1,
            ..exact
        },
    ] {
        assert!(matches!(
            SealedIncomingIndex::build(&source, limits, &mut || Ok(())),
            Err(SealedError::Limit { .. })
        ));
    }
    assert!(mul(usize::MAX, 2).is_err());
    assert!(add(usize::MAX, 1).is_err());
    assert!(ef_bytes(usize::MAX, u64::MAX).is_err());
    let empty = seal(&[]);
    let empty = SealedIncomingIndex::build(
        &empty,
        IncomingIndexLimits {
            max_rows: 0,
            max_incidences: 0,
            max_workspace_bytes: 0,
        },
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(empty.stats().charged_workspace_bytes, 0);
    assert!(
        empty
            .open(VId(0), RelationId(1), CommitSeq(0), None, &mut || Ok(()))
            .is_err()
    );
    assert!(
        empty
            .open(VId(0), RelationId(1), CommitSeq(11), None, &mut || Ok(()))
            .is_err()
    );
}

#[test]
fn every_build_and_pull_checkpoint_refuses_without_resumable_prefixes() {
    let mut entries: Vec<_> = (0..24).map(|i| entry(i, 7, i, 1)).collect();
    for entry in &mut entries[..12] {
        entry.retired_at = Some(CommitSeq(5));
    }
    let source = seal(&entries);
    let mut total = 0;
    SealedIncomingIndex::build(&source, IncomingIndexLimits::default(), &mut || {
        total += 1;
        Ok(())
    })
    .unwrap();
    for stop in 1..=total {
        let mut calls = 0;
        let result =
            SealedIncomingIndex::build(&source, IncomingIndexLimits::default(), &mut || {
                calls += 1;
                if calls == stop {
                    Err(SealedError::InvalidFloor)
                } else {
                    Ok(())
                }
            });
        assert!(matches!(result, Err(SealedError::InvalidFloor)));
        assert_eq!(calls, stop);
    }
    let incoming = index(&source);
    let mut cursor = incoming
        .open(VId(7), RelationId(1), CommitSeq(5), None, &mut || Ok(()))
        .unwrap();
    total = 0;
    while cursor
        .next_inner(&mut || {
            total += 1;
            Ok(())
        })
        .unwrap()
        .is_some()
    {}
    for stop in 1..=total {
        let mut cursor = incoming
            .open(VId(7), RelationId(1), CommitSeq(5), None, &mut || Ok(()))
            .unwrap();
        let mut calls = 0;
        loop {
            let result = cursor.next_inner(&mut || {
                calls += 1;
                if calls == stop {
                    Err(SealedError::InvalidFloor)
                } else {
                    Ok(())
                }
            });
            if matches!(result, Err(SealedError::InvalidFloor)) {
                break;
            }
            assert!(result.unwrap().is_some());
        }
        assert_eq!(calls, stop);
        assert!(
            cursor
                .next_inner(&mut || panic!("failed cursor resumed"))
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        collect(&incoming, VId(7), RelationId(1), CommitSeq(5), None).len(),
        12
    );
}

#[test]
fn clones_pin_the_same_source_and_authenticated_reload_rebuilds_identical_indices() {
    let source = seal(&[entry(1, 2, 1, 1), entry(3, 2, 2, 1)]);
    let bytes = wire::encode(&source.image, SealedLimits::default(), &mut || Ok(())).unwrap();
    let anchor = source.anchor();
    let incoming = index(&source);
    let cloned = incoming.clone();
    assert!(incoming.shares_index_with(&cloned));
    assert!(incoming.source().shares_image_with(cloned.source()));
    let weak = Arc::downgrade(&source.image);
    drop(source);
    drop(incoming);
    assert!(weak.upgrade().is_some());
    let expected = collect(&cloned, VId(2), RelationId(1), CommitSeq(5), None);
    drop(cloned);
    assert!(weak.upgrade().is_none());
    let source = SealedPartition::reload_inner(
        anchor,
        &bytes,
        SealedLimits::default(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(
        collect(&index(&source), VId(2), RelationId(1), CommitSeq(5), None),
        expected
    );
    let mut damaged = bytes;
    damaged[0] ^= 1;
    assert!(matches!(
        SealedPartition::reload_inner(anchor, &damaged, SealedLimits::default(), None, &mut || Ok(
            ()
        )),
        Err(SealedError::ImageMismatch)
    ));
}

#[test]
fn cooperative_sort_matches_std_on_duplicate_and_adversarial_keys() {
    for length in 0..100 {
        for seed in 0..20usize {
            let mut values: Vec<_> = (0..length).map(|i| (i * 17 + seed * 13) % 19).collect();
            let mut expected = values.clone();
            expected.sort_unstable();
            sort(&mut values, &mut || Ok(())).unwrap();
            assert_eq!(values, expected);
        }
    }
}
