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

// The incumbent reader keeps independent positional accesses for both the
// incoming permutation and the original CSR image. Keep this test-only oracle
// live beside the streaming reader so result, fuel and guard traces can be
// compared in one invocation, including the charged end of every EF chunk.
struct PositionalIncomingCursor<'a> {
    image: &'a Image,
    chunks: &'a [EliasFano],
    chunk: usize,
    at: usize,
    as_of: CommitSeq,
    finished: bool,
}

impl<'a> PositionalIncomingCursor<'a> {
    fn open(
        index: &'a SealedIncomingIndex,
        destination: VId,
        relation: RelationId,
        as_of: CommitSeq,
        lower_source: Option<VId>,
    ) -> Self {
        index.source.anchor.authorize(as_of).unwrap();
        let image = &index.source.image;
        let chunks = index.find(destination, relation).map_or(&[][..], |row| {
            &index.index.chunks[row.first_chunk..row.end_chunk]
        });
        let lower = match lower_source {
            None => 0,
            Some(source) => {
                let row = image.rows.partition_point(|row| row.key.src < source);
                image.offsets.select(row).unwrap()
            }
        };
        let chunk =
            chunks.partition_point(|chunk| chunk.max_value().is_some_and(|max| max < lower));
        let at = chunks.get(chunk).map_or(0, |chunk| chunk.rank_lt(lower));
        Self {
            image,
            chunks,
            chunk,
            at,
            as_of,
            finished: false,
        }
    }

    fn next_budgeted(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, SealedError> {
        if self.finished {
            return Ok(SealedScanStep::End);
        }
        let result = self.pull(budget, checkpoint);
        if result.is_err() || matches!(&result, Ok(SealedScanStep::End)) {
            self.finished = true;
        }
        result
    }

    fn pull(
        &mut self,
        budget: &mut SealedScanBudget,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<SealedScanStep<SealedEdge<'a>>, SealedError> {
        checkpoint()?;
        while let Some(chunk) = self.chunks.get(self.chunk) {
            if !budget.spend() {
                return Ok(SealedScanStep::Yield);
            }
            checkpoint()?;
            let Some(position) = chunk.select(self.at) else {
                self.chunk += 1;
                self.at = 0;
                continue;
            };
            self.at += 1;
            let row_index = self
                .image
                .offsets
                .rank_le(position)
                .checked_sub(1)
                .ok_or(SealedError::NonCanonical)?;
            let row = self
                .image
                .rows
                .get(row_index)
                .ok_or(SealedError::NonCanonical)?;
            let offset = position
                .checked_sub(
                    self.image
                        .offsets
                        .select(row_index)
                        .ok_or(SealedError::NonCanonical)?,
                )
                .ok_or(SealedError::NonCanonical)?;
            let (entry, locator) = row
                .incidence(
                    self.image,
                    usize::try_from(offset).map_err(|_| SealedError::SizeOverflow)?,
                )
                .ok_or(SealedError::NonCanonical)?;
            if entry.visible_at(self.as_of) {
                let properties = if locator == 0 {
                    &[][..]
                } else {
                    self.image
                        .properties
                        .get(locator as usize - 1)
                        .ok_or(SealedError::NonCanonical)?
                        .as_slice()
                };
                return Ok(SealedScanStep::Item(SealedEdge { entry, properties }));
            }
        }
        Ok(SealedScanStep::End)
    }
}

#[test]
fn incoming_streaming_matches_positional_work_and_logical_oracle_on_hubs_and_sparse_seeks() {
    let dense_target = VId(1_u128 << 96);
    let sparse_target = VId(u128::MAX);
    let sources: Vec<_> = (0..513_u128)
        .map(|at| {
            if at == 512 {
                VId(u128::MAX)
            } else {
                VId((at << 80) + 17 * at)
            }
        })
        .collect();
    let mut entries = Vec::new();
    for (at, &source) in sources.iter().enumerate() {
        let mut add = |target: VId, relation: u64, copies: usize| {
            for copy in 0..copies {
                let mut edge = entry(
                    source.0,
                    target.0,
                    (1_u128 << 112) + entries.len() as u128,
                    relation,
                );
                edge.created_at = CommitSeq(1 + (copy % 3) as u64);
                edge.retired_at = (copy % 4 == 0).then_some(CommitSeq(7));
                entries.push(edge);
            }
        };
        add(dense_target, 1, if at % 64 == 0 { 270 } else { 1 });
        add(VId(dense_target.0 + 2 + at as u128), 1, 2);
        if at % 97 == 0 || at == 512 {
            add(sparse_target, 1, if at == 0 { 257 } else { 2 });
        }
        if at % 7 == 0 {
            add(dense_target, 2, 3);
        }
    }
    let source = seal(&entries);
    let incoming = index(&source);
    for target in [dense_target, sparse_target, VId(0)] {
        for relation in [RelationId(1), RelationId(2)] {
            for cut in [1, 3, 6, 7, 10] {
                for lower in [
                    None,
                    Some(sources[1]),
                    Some(VId(sources[255].0 + 1)),
                    Some(sources[512]),
                ] {
                    let mut expected: Vec<_> = entries
                        .iter()
                        .copied()
                        .filter(|edge| {
                            edge.dst == target
                                && edge.relation == relation
                                && edge.visible_at(CommitSeq(cut))
                                && lower.is_none_or(|lower| edge.src >= lower)
                        })
                        .collect();
                    expected.sort_unstable_by_key(|edge| (edge.src, edge.eid, edge.created_at));
                    for quantum in [1, 7, 256, 257] {
                        let mut actual = incoming
                            .open(target, relation, CommitSeq(cut), lower, &mut || Ok(()))
                            .unwrap();
                        let mut incumbent = PositionalIncomingCursor::open(
                            &incoming,
                            target,
                            relation,
                            CommitSeq(cut),
                            lower,
                        );
                        let mut emitted = Vec::new();
                        let mut turn = 0;
                        loop {
                            // Inject empty fuel between successful pulls. It
                            // cannot advance a hidden edge or a chunk boundary.
                            let fuel = if turn % 3 == 0 { 0 } else { quantum };
                            turn += 1;
                            let mut a_fuel = SealedScanBudget::new(fuel);
                            let mut b_fuel = SealedScanBudget::new(fuel);
                            let mut a_calls = 0;
                            let mut b_calls = 0;
                            let a = actual
                                .next_budgeted_inner(&mut a_fuel, &mut || {
                                    a_calls += 1;
                                    Ok::<_, SealedError>(())
                                })
                                .unwrap();
                            let b = incumbent
                                .next_budgeted(&mut b_fuel, &mut || {
                                    b_calls += 1;
                                    Ok(())
                                })
                                .unwrap();
                            assert_eq!(a_calls, b_calls);
                            assert_eq!(a_fuel.remaining(), b_fuel.remaining());
                            match (a, b) {
                                (SealedScanStep::Item(a), SealedScanStep::Item(b)) => {
                                    assert_eq!(a.entry, b.entry);
                                    assert_eq!(a.properties, b.properties);
                                    assert!(std::ptr::eq(a.properties, b.properties));
                                    assert_eq!(
                                        a.properties,
                                        &[(
                                            PropertyKeyId(1),
                                            CanonicalScalar::Int(a.entry.created_at.0 as i64)
                                        )]
                                    );
                                    emitted.push(a.entry);
                                }
                                (SealedScanStep::Yield, SealedScanStep::Yield) => {}
                                (SealedScanStep::End, SealedScanStep::End) => break,
                                (a, b) => panic!("incoming cursor trace differs: {a:?} vs {b:?}"),
                            }
                        }
                        assert_eq!(emitted, expected);
                        assert!(matches!(
                            actual
                                .next_budgeted_inner(
                                    &mut SealedScanBudget::new(1),
                                    &mut || -> Result<(), SealedError> {
                                        panic!("exhausted cursor resumed")
                                    }
                                )
                                .unwrap(),
                            SealedScanStep::End
                        ));
                    }
                }
            }
        }
    }
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

#[test]
#[ignore = "manual release-mode positional-versus-streaming incoming scan benchmark"]
fn benchmark_sealed_incoming_stream_against_positional() {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    fn fold(sum: u128, edge: &SealedEdge<'_>) -> u128 {
        sum.wrapping_mul(31)
            .wrapping_add(edge.entry.src.0)
            .wrapping_add(edge.entry.dst.0.rotate_left(23))
            .wrapping_add(edge.entry.eid.0.rotate_left(47))
            .wrapping_add(u128::from(edge.entry.created_at.0))
            .wrapping_add(edge.properties.len() as u128)
    }

    fn scan(index: &SealedIncomingIndex, keys: &[(VId, RelationId)], streaming: bool) -> u128 {
        let mut sum = 0;
        for &(destination, relation) in keys {
            let mut fuel = SealedScanBudget::new(usize::MAX);
            if streaming {
                let mut cursor = index
                    .open(destination, relation, CommitSeq(5), None, &mut || Ok(()))
                    .unwrap();
                loop {
                    match cursor
                        .next_budgeted_inner(&mut fuel, &mut || Ok::<_, SealedError>(()))
                        .unwrap()
                    {
                        SealedScanStep::Item(edge) => sum = fold(sum, &edge),
                        SealedScanStep::End => break,
                        SealedScanStep::Yield => panic!("unbounded benchmark fuel exhausted"),
                    }
                }
            } else {
                let mut cursor = PositionalIncomingCursor::open(
                    index,
                    destination,
                    relation,
                    CommitSeq(5),
                    None,
                );
                loop {
                    match cursor.next_budgeted(&mut fuel, &mut || Ok(())).unwrap() {
                        SealedScanStep::Item(edge) => sum = fold(sum, &edge),
                        SealedScanStep::End => break,
                        SealedScanStep::Yield => panic!("unbounded benchmark fuel exhausted"),
                    }
                }
            }
        }
        black_box(sum)
    }

    fn measure(repetitions: usize, expected: u128, mut scan: impl FnMut() -> u128) -> Duration {
        let start = Instant::now();
        let mut observed = 0_u128;
        for _ in 0..repetitions {
            observed = observed.wrapping_add(black_box(scan()));
        }
        let duration = start.elapsed();
        assert_eq!(observed, expected.wrapping_mul(repetitions as u128));
        duration
    }

    const ROUNDS: usize = 9;
    const INCIDENCES_PER_ROUND: usize = 262_144;
    eprintln!(
        "sealed_incoming_scan: same-process incumbent; source sealing, index construction, \
         QueryCx and durable I/O excluded; physical cursor controls retained; \
         empirical_gate_activated=false; old_cursor_bytes={} new_cursor_bytes={} \
         ef_sequential_cursor_bytes={}",
        size_of::<PositionalIncomingCursor<'_>>(),
        size_of::<SealedIncomingCursor<'_>>(),
        size_of::<EliasFanoSequentialCursor<'_>>()
    );
    for (name, vertices, degree) in [
        ("adjacent_fan_in", 16_384_usize, 1_usize),
        ("regular", 4_096, 32),
        ("parallel_history", 1_024, 256),
        ("sparse_fan_in", 65_536, 1),
    ] {
        let mut entries = Vec::new();
        for source in 0..vertices {
            for neighbor in 0..degree {
                let destination = match name {
                    "adjacent_fan_in" | "parallel_history" => 0,
                    "regular" => (source + neighbor + 1) % vertices,
                    "sparse_fan_in" => {
                        if source % 1_024 == 0 {
                            0
                        } else {
                            source
                        }
                    }
                    _ => unreachable!(),
                };
                let mut edge = entry(
                    (1_u128 << 96) + source as u128,
                    (1_u128 << 96) + destination as u128,
                    (1_u128 << 112) + entries.len() as u128,
                    1,
                );
                if name == "parallel_history" {
                    edge.created_at = CommitSeq(1 + (neighbor % 3) as u64);
                    edge.retired_at = (neighbor % 4 == 0).then_some(CommitSeq(4));
                }
                entries.push(edge);
            }
        }
        let source = seal(&entries);
        let index = index(&source);
        let keys: Vec<_> = index
            .index
            .rows
            .iter()
            .filter(|row| name != "sparse_fan_in" || row.destination == VId(1_u128 << 96))
            .map(|row| (row.destination, row.relation))
            .collect();
        let retained: usize = keys
            .iter()
            .map(|&(destination, relation)| index.retained_row_len(destination, relation))
            .sum();
        // Compare every emitted entry and property before timing. The checksum
        // is only a timed consumption sink, not the result-equality oracle.
        for &(destination, relation) in &keys {
            let mut a = index
                .open(destination, relation, CommitSeq(5), None, &mut || Ok(()))
                .unwrap();
            let mut b =
                PositionalIncomingCursor::open(&index, destination, relation, CommitSeq(5), None);
            loop {
                let left = a
                    .next_budgeted_inner(&mut SealedScanBudget::new(usize::MAX), &mut || {
                        Ok::<_, SealedError>(())
                    })
                    .unwrap();
                let right = b
                    .next_budgeted(&mut SealedScanBudget::new(usize::MAX), &mut || Ok(()))
                    .unwrap();
                match (left, right) {
                    (SealedScanStep::Item(a), SealedScanStep::Item(b)) => {
                        assert_eq!(a.entry, b.entry);
                        assert_eq!(a.properties, b.properties);
                    }
                    (SealedScanStep::End, SealedScanStep::End) => break,
                    (a, b) => panic!("benchmark preflight differs: {a:?} vs {b:?}"),
                }
            }
        }
        let expected = scan(&index, &keys, false);
        assert_eq!(scan(&index, &keys, true), expected);
        let repetitions = INCIDENCES_PER_ROUND.div_ceil(retained);
        let mut old = [Duration::ZERO; ROUNDS];
        let mut new = [Duration::ZERO; ROUNDS];
        for round in 0..ROUNDS {
            if round % 2 == 0 {
                old[round] = measure(repetitions, expected, || scan(&index, &keys, false));
                new[round] = measure(repetitions, expected, || scan(&index, &keys, true));
            } else {
                new[round] = measure(repetitions, expected, || scan(&index, &keys, true));
                old[round] = measure(repetitions, expected, || scan(&index, &keys, false));
            }
        }
        old.sort_unstable();
        new.sort_unstable();
        let incidences = (retained * repetitions) as f64;
        let ns = |elapsed: Duration| elapsed.as_secs_f64() * 1e9 / incidences;
        eprintln!(
            "shape={name} vertices={vertices} source_incidences={} scanned_incidences={retained} \
             rounds={ROUNDS} old_ns={:.3} old_min={:.3} old_max={:.3} \
             new_ns={:.3} new_min={:.3} new_max={:.3} self_speedup={:.3}",
            entries.len(),
            ns(old[ROUNDS / 2]),
            ns(old[0]),
            ns(old[ROUNDS - 1]),
            ns(new[ROUNDS / 2]),
            ns(new[0]),
            ns(new[ROUNDS - 1]),
            old[ROUNDS / 2].as_secs_f64() / new[ROUNDS / 2].as_secs_f64(),
        );
    }
}
