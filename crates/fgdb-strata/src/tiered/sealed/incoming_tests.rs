use super::super::{SealedLimits, SealedScope, image, wire};
use super::*;
use crate::compact::Compaction;
use crate::edge_props::{BlockProps, EdgePropertyRow};
use crate::{AdjacencyEntry, PartitionRootVersion};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{BranchId, CanonicalPropertyValue, CanonicalScalar, EId, GraphId, ObjectId};

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
                        CanonicalPropertyValue::Scalar(CanonicalScalar::Int(
                            entry.created_at.0 as i64,
                        )),
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
                                                .into()
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
                    vec![(
                        PropertyKeyId(1),
                        CanonicalPropertyValue::Scalar(CanonicalScalar::Int(1))
                    )]
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
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(if cut < 5 { 1 } else { 5 }))
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

#[derive(Clone, Copy, Debug)]
enum HistoryShape {
    AllVisible,
    AllHidden,
    Alternating,
    LongGaps,
}

fn history_fixture(count: usize, shape: HistoryShape) -> (SealedPartition, Vec<AdjacencyEntry>) {
    let entries: Vec<_> = (0..count)
        .map(|at| {
            let visible = match shape {
                HistoryShape::AllVisible => true,
                HistoryShape::AllHidden => false,
                HistoryShape::Alternating => !at.is_multiple_of(2),
                HistoryShape::LongGaps => {
                    at + 1 == count || [63, 64, 65, 255, 256, 257, 4095].contains(&at)
                }
            };
            let mut edge = entry(
                (1_u128 << 80) + 17,
                u128::MAX,
                (1_u128 << 112) + at as u128,
                1,
            );
            edge.created_at = CommitSeq(if visible { 3 } else { 1 });
            edge.retired_at = (!visible).then_some(CommitSeq(3));
            edge
        })
        .collect();
    (seal(&entries), entries)
}

/// Compare each pull against the pre-filter sequential reader and check its
/// answer against independent logical records. Exact guard/fuel/error order
/// matters even for a call that returns no visible incidence.
fn compare_history_pulls(
    incoming: &SealedIncomingIndex,
    entries: &[AdjacencyEntry],
    cut: CommitSeq,
    allowances: &[usize],
    stop: Option<usize>,
) -> usize {
    let mut old = incoming
        .open(entries[0].dst, entries[0].relation, cut, None, &mut || {
            Ok(())
        })
        .unwrap();
    let mut new = incoming
        .open(entries[0].dst, entries[0].relation, cut, None, &mut || {
            Ok(())
        })
        .unwrap();
    let expected: Vec<_> = entries
        .iter()
        .filter(|entry| entry.visible_at(cut))
        .collect();
    let (mut old_calls, mut new_calls, mut seen, mut pulls) = (0, 0, 0, 0);
    let (mut position, mut visible_prefix) = (0, 0);
    loop {
        let allowance = allowances[pulls % allowances.len()];
        pulls += 1;
        assert!(pulls <= 3 * entries.len() + 64, "scan must make progress");
        let mut old_fuel = SealedScanBudget::new(allowance);
        let mut new_fuel = SealedScanBudget::new(allowance);
        let old_step = old.next_decoding_history(&mut old_fuel, &mut || {
            old_calls += 1;
            if stop == Some(old_calls) {
                Err(SealedError::InvalidFloor)
            } else {
                Ok(())
            }
        });
        let new_step = new.next_budgeted_inner(&mut new_fuel, &mut || {
            new_calls += 1;
            if stop == Some(new_calls) {
                Err(SealedError::InvalidFloor)
            } else {
                Ok(())
            }
        });
        assert_eq!(new_calls, old_calls);
        assert_eq!(new_fuel.remaining(), old_fuel.remaining());
        assert_eq!(new.finished, old.finished);
        assert_eq!(new.chunk, old.chunk);
        assert_eq!(new.source_row.is_some(), old.source_row.is_some());
        if let (Some(old_row), Some(new_row)) = (&old.source_row, &new.source_row) {
            assert_eq!(new_row.index, old_row.index);
            assert_eq!(new_row.scan.position, old_row.scan.position);
            assert_eq!(old_row.scan.decoded_incidences(), old_row.scan.position);
            visible_prefix += entries[position..new_row.scan.position]
                .iter()
                .filter(|entry| entry.visible_at(cut))
                .count();
            position = new_row.scan.position;
            assert_eq!(
                new_row.scan.decoded_incidences(),
                if incoming.source.image.rows[new_row.index].kind()
                    == super::super::RowStorageKind::Inline
                {
                    new_row.scan.position
                } else {
                    visible_prefix
                },
            );
        }
        match (old_step, new_step) {
            (Ok(SealedScanStep::Item(old_edge)), Ok(SealedScanStep::Item(new_edge))) => {
                assert_eq!(new_edge.entry, old_edge.entry);
                assert_eq!(new_edge.entry, *expected[seen]);
                assert!(std::ptr::eq(new_edge.properties, old_edge.properties));
                assert_eq!(
                    new_edge.properties,
                    &[(
                        PropertyKeyId(1),
                        CanonicalScalar::Int(new_edge.entry.created_at.0 as i64).into(),
                    )],
                );
                seen += 1;
            }
            (Ok(SealedScanStep::Yield), Ok(SealedScanStep::Yield)) => {}
            (Ok(SealedScanStep::End), Ok(SealedScanStep::End)) => {
                assert!(stop.is_none());
                assert_eq!(seen, expected.len());
                break;
            }
            (Err(SealedError::InvalidFloor), Err(SealedError::InvalidFloor)) => {
                assert_eq!(Some(new_calls), stop);
                break;
            }
            (old, new) => panic!("incoming history steps differ: {old:?} vs {new:?}"),
        }
    }
    let mut zero = SealedScanBudget::new(0);
    assert!(matches!(
        new.next_budgeted_inner(&mut zero, &mut || -> Result<(), SealedError> {
            panic!("terminal cursor resumed")
        }),
        Ok(SealedScanStep::End)
    ));
    assert!(matches!(
        old.next_decoding_history(&mut zero, &mut || -> Result<(), SealedError> {
            panic!("terminal incumbent resumed")
        }),
        Ok(SealedScanStep::End)
    ));
    new_calls
}

#[test]
fn incoming_hidden_history_preserves_every_fuel_boundary_and_refusal() {
    for count in [1, 8, 9, 65, 257] {
        for shape in [
            HistoryShape::AllVisible,
            HistoryShape::AllHidden,
            HistoryShape::Alternating,
            HistoryShape::LongGaps,
        ] {
            let (source, entries) = history_fixture(count, shape);
            let incoming = index(&source);
            for cut in [CommitSeq(1), CommitSeq(2), CommitSeq(3), CommitSeq(10)] {
                let allowances = [0, 1, 0, 7, 63, 64, 65, 256, 257];
                let total = compare_history_pulls(&incoming, &entries, cut, &allowances, None);
                for stop in 1..=total {
                    compare_history_pulls(&incoming, &entries, cut, &allowances, Some(stop));
                }
            }
        }
    }
}

#[test]
fn incoming_large_hidden_history_elides_decodes_after_authenticated_reload() {
    for shape in [
        HistoryShape::AllVisible,
        HistoryShape::AllHidden,
        HistoryShape::Alternating,
        HistoryShape::LongGaps,
    ] {
        let (source, entries) = history_fixture(8193, shape);
        let bytes = wire::encode(&source.image, SealedLimits::default(), &mut || Ok(())).unwrap();
        let reloaded = SealedPartition::reload_inner(
            source.anchor(),
            &bytes,
            SealedLimits::default(),
            None,
            &mut || Ok(()),
        )
        .unwrap();
        for partition in [&source, &reloaded] {
            let incoming = index(partition);
            assert!(incoming.stats().chunks > 1);
            compare_history_pulls(
                &incoming,
                &entries,
                CommitSeq(3),
                &[0, 1, 63, 64, 65, 255, 256, 257, 4096],
                None,
            );
            let mut cursor = incoming
                .open(
                    entries[0].dst,
                    entries[0].relation,
                    CommitSeq(3),
                    None,
                    &mut || Ok(()),
                )
                .unwrap();
            while cursor.next_inner(&mut || Ok(())).unwrap().is_some() {}
            let scan = &cursor.source_row.as_ref().unwrap().scan;
            let visible = entries
                .iter()
                .filter(|edge| edge.visible_at(CommitSeq(3)))
                .count();
            assert_eq!(scan.decoded_incidences(), visible);
            match shape {
                HistoryShape::AllHidden => {
                    assert_eq!(scan.decoded_incidences(), 0);
                    assert_eq!(scan.resumed_seeks(), 0);
                }
                HistoryShape::AllVisible | HistoryShape::Alternating => {
                    assert_eq!(scan.resumed_seeks(), 0);
                }
                HistoryShape::LongGaps => {
                    assert_eq!(visible, 8);
                    assert!(scan.resumed_seeks() <= visible);
                }
            }
        }
    }
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
#[ignore = "manual release-mode incoming hidden-history decode-elision benchmark"]
fn benchmark_sealed_incoming_history_decode_elision_against_unfiltered() {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    const ROUNDS: usize = 9;
    eprintln!(
        "incoming_history_scan: same-process pre-filter sequential reader; sealing, \
         index construction, wire admission, QueryCx and durable I/O excluded; \
         empirical_gate_activated=false"
    );
    for count in [4096, 65536] {
        for shape in [
            HistoryShape::AllVisible,
            HistoryShape::AllHidden,
            HistoryShape::Alternating,
            HistoryShape::LongGaps,
        ] {
            let (source, entries) = history_fixture(count, shape);
            let incoming = index(&source);
            compare_history_pulls(&incoming, &entries, CommitSeq(3), &[usize::MAX], None);
            let scan = |filtered| {
                let mut cursor = incoming
                    .open(
                        entries[0].dst,
                        entries[0].relation,
                        CommitSeq(3),
                        None,
                        &mut || Ok(()),
                    )
                    .unwrap();
                let mut fuel = SealedScanBudget::new(usize::MAX);
                let (mut sum, mut visible, mut polls) = (0_u128, 0_usize, 0_usize);
                loop {
                    let mut guard = || {
                        polls += 1;
                        Ok::<_, SealedError>(())
                    };
                    let step = if filtered {
                        cursor.next_budgeted_inner(&mut fuel, &mut guard)
                    } else {
                        cursor.next_decoding_history(&mut fuel, &mut guard)
                    }
                    .unwrap();
                    match step {
                        SealedScanStep::Item(edge) => {
                            let edge = black_box(edge);
                            sum = sum
                                .wrapping_mul(31)
                                .wrapping_add(edge.entry.eid.0)
                                .wrapping_add(edge.entry.dst.0);
                            black_box(edge.properties);
                            visible += 1;
                        }
                        SealedScanStep::End => break,
                        SealedScanStep::Yield => panic!("unbounded scan exhausted its fuel"),
                    }
                }
                (sum, visible, polls, usize::MAX - fuel.remaining())
            };
            let expected = scan(false);
            assert_eq!(scan(true), expected);
            let repetitions = 262_144_usize.div_ceil(count);
            let measure = |filtered| {
                let start = Instant::now();
                for _ in 0..repetitions {
                    assert_eq!(black_box(scan(filtered)), expected);
                }
                start.elapsed()
            };
            let mut old = [Duration::ZERO; ROUNDS];
            let mut new = [Duration::ZERO; ROUNDS];
            for round in 0..ROUNDS {
                if round.is_multiple_of(2) {
                    old[round] = measure(false);
                    new[round] = measure(true);
                } else {
                    new[round] = measure(true);
                    old[round] = measure(false);
                }
            }
            old.sort_unstable();
            new.sort_unstable();
            let ns = |elapsed: Duration| elapsed.as_secs_f64() * 1e9 / (count * repetitions) as f64;
            eprintln!(
                "count={count} shape={shape:?} visible={} polls={} fuel={} rounds={ROUNDS} \
                 old_ns={:.3} old_min={:.3} old_max={:.3} new_ns={:.3} new_min={:.3} \
                 new_max={:.3} self_speedup={:.3}",
                expected.1,
                expected.2,
                expected.3,
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

// Sort laws exercise the shared immediate/cooperative implementation. Checkpoint
// counts follow the chosen algorithm; refusal handling and permutation safety
// must hold at every checkpoint, including after genuine suspension.
fn drive_sort_future<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(result) = future.as_mut().poll(&mut cx) {
            return result;
        }
    }
}
#[test]
fn exhaustive_duplicate_inputs_preserve_complete_order() {
    for n in 0..=9_u32 {
        for encoded in 0..3_usize.pow(n) {
            let mut code = encoded;
            let mut values: Vec<_> = (0..n)
                .map(|_| {
                    let value = code % 3;
                    code /= 3;
                    value
                })
                .collect();
            let mut expected = values.clone();
            expected.sort_unstable();
            construction::sort_sync(&mut values, &mut || Ok(())).unwrap();
            assert_eq!(values, expected);
        }
    }
}
#[test]
fn full_width_tuples_and_forced_heap_fallback_match_std() {
    for n in [
        0_usize, 1, 2, 15, 16, 17, 31, 32, 33, 83, 127, 128, 129, 257, 4097,
    ] {
        for shape in 0..5 {
            let input: Vec<_> = (0..n)
                .map(|i| {
                    let key = match shape {
                        0 => (i.wrapping_mul(19997) % (n + 1)) as u128,
                        1 => (n - i) as u128,
                        2 => i.min(n - i) as u128,
                        3 => (i % 5) as u128,
                        _ => {
                            if i == 0 {
                                u64::MAX as u128
                            } else {
                                i as u128
                            }
                        }
                    };
                    ((key << 64) | (i % 3) as u128, (i % 7) as u64, i as u64)
                })
                .collect();
            let mut expected = input.clone();
            expected.sort_unstable();
            let mut values = input.clone();
            construction::sort_sync(&mut values, &mut || Ok(())).unwrap();
            assert_eq!(values, expected);
            let mut incumbent = input.clone();
            construction::heap_sort_sync(&mut incumbent, &mut || Ok(())).unwrap();
            assert_eq!(incumbent, expected);
            for depth in [0, 1, 2] {
                let mut values = input.clone();
                construction::sort_at_depth_sync(&mut values, depth, &mut || Ok(())).unwrap();
                assert_eq!(values, expected);
            }
        }
    }
}
#[test]
fn every_refusal_keeps_each_item_exactly_once() {
    let input: Vec<_> = (0..191).map(|i| (i * 73) % 191).collect();
    let mut expected = input.clone();
    expected.sort_unstable();
    let mut successful = input.clone();
    let mut total = 0;
    construction::sort_sync(&mut successful, &mut || {
        total += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(successful, expected);
    for stop in 1..=total {
        let mut values = input.clone();
        let mut calls = 0;
        let result = construction::sort_sync(&mut values, &mut || {
            calls += 1;
            if calls == stop {
                Err(SealedError::NonCanonical)
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(SealedError::NonCanonical)));
        assert_eq!(calls, stop);
        values.sort_unstable();
        assert_eq!(values, expected);
    }
}
struct Yielding {
    calls: usize,
    quantum: usize,
    yields: usize,
    stop: Option<usize>,
}
impl construction::Checkpoint for Yielding {
    type Error = SealedError;
    async fn check(&mut self) -> Result<(), Self::Error> {
        self.calls += 1;
        if self.stop == Some(self.calls) {
            return Err(SealedError::NonCanonical);
        }
        if self.calls.is_multiple_of(self.quantum) {
            self.yields += 1;
            let mut pending = true;
            std::future::poll_fn(|cx| {
                if pending {
                    pending = false;
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready(())
                }
            })
            .await;
        }
        Ok(())
    }
}
#[test]
fn real_pending_futures_preserve_results_and_exact_refusals() {
    let input: Vec<_> = (0..257).map(|i| (i * 139) % 257).collect();
    let mut expected = input.clone();
    expected.sort_unstable();
    for quantum in [1, 2, 7, 64, 256] {
        let mut values = input.clone();
        let mut control = Yielding {
            calls: 0,
            quantum,
            yields: 0,
            stop: None,
        };
        drive_sort_future(construction::sort(&mut values, &mut control)).unwrap();
        assert_eq!(values, expected);
        assert_eq!(control.yields, control.calls / quantum);
        for stop in [1, 2, 16, 257, control.calls / 2, control.calls] {
            let mut values = input.clone();
            let mut control = Yielding {
                calls: 0,
                quantum,
                yields: 0,
                stop: Some(stop),
            };
            assert!(matches!(
                drive_sort_future(construction::sort(&mut values, &mut control)),
                Err(SealedError::NonCanonical)
            ));
            assert_eq!(control.calls, stop);
            values.sort_unstable();
            assert_eq!(values, expected);
        }
    }
}

#[test]
fn ninther_duplicate_heavy_samples_match_std_at_thresholds() {
    for n in [127_usize, 128, 129, 257, 4096] {
        for seed in 0..16 {
            let mut values: Vec<_> = (0..n).map(|i| (i * 17 + seed * 13) % 31).collect();
            let mut expected = values.clone();
            expected.sort_unstable();
            construction::sort_sync(&mut values, &mut || Ok(())).unwrap();
            assert_eq!(values, expected, "length={n} seed={seed}");
        }
    }
}

mod incumbent {
    use super::SealedError;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    pub(super) trait Checkpoint {
        type Error: From<SealedError>;
        async fn check(&mut self) -> Result<(), Self::Error>;
    }

    struct Immediate<G>(G);
    impl<E, G> Checkpoint for Immediate<G>
    where
        E: From<SealedError>,
        G: FnMut() -> Result<(), E>,
    {
        type Error = E;
        async fn check(&mut self) -> Result<(), E> {
            (self.0)()
        }
    }

    fn immediate<T, E: From<SealedError>>(
        future: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(result) => result,
            Poll::Pending => Err(SealedError::NonCanonical.into()),
        }
    }

    #[cfg(test)]
    pub(super) fn sort_sync<T: Ord>(
        values: &mut [T],
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<(), SealedError> {
        immediate(sort(values, &mut Immediate(checkpoint)))
    }

    // Constant-scratch heapsort. No allocation or uninterruptible whole-population
    // sort hides between the surrounding checkpoints. Input keys are total/unique.
    async fn sort<T: Ord, C: Checkpoint>(
        values: &mut [T],
        control: &mut C,
    ) -> Result<(), C::Error> {
        let mut ordered = true;
        for pair in values.windows(2) {
            control.check().await?;
            if pair[0] > pair[1] {
                ordered = false;
            }
        }
        if ordered {
            return Ok(());
        }
        for root in (0..values.len() / 2).rev() {
            sift(values, root, control).await?;
        }
        for end in (1..values.len()).rev() {
            control.check().await?;
            values.swap(0, end);
            sift(&mut values[..end], 0, control).await?;
        }
        Ok(())
    }

    async fn sift<T: Ord, C: Checkpoint>(
        values: &mut [T],
        mut root: usize,
        control: &mut C,
    ) -> Result<(), C::Error> {
        while root < values.len() / 2 {
            control.check().await?;
            let left = 2 * root + 1;
            let right = left + 1;
            let child = if right < values.len() && values[right] > values[left] {
                right
            } else {
                left
            };
            if values[root] >= values[child] {
                break;
            }
            values.swap(root, child);
            root = child;
        }
        Ok(())
    }
}

#[test]
#[ignore = "paired retained-incidence sort-stage measurement"]
fn paired_sort_stage_measurement() {
    use std::hint::black_box;
    use std::time::{Duration, Instant};
    type Ref = (u128, u64, u64);
    fn mix(mut x: u64) -> u64 {
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }
    fn id(vertex: u64) -> u128 {
        (u128::from(mix(vertex.wrapping_add(0x9e37_79b9_7f4a_7c15))) << 64)
            | u128::from(mix(vertex ^ 0xa076_1d64_78bd_642f))
    }
    fn measure(input: &[Ref], expected: &[Ref], old: bool) -> (Duration, usize) {
        let mut values = input.to_vec();
        let mut polls = 0usize;
        let start = Instant::now();
        if old {
            incumbent::sort_sync(black_box(&mut values), &mut || {
                polls += 1;
                Ok(())
            })
            .unwrap();
        } else {
            construction::sort_sync(black_box(&mut values), &mut || {
                polls += 1;
                Ok(())
            })
            .unwrap();
        }
        let elapsed = start.elapsed();
        assert_eq!(values, expected);
        black_box(&values);
        (elapsed, polls)
    }
    for n in [100_000usize, 250_000] {
        for shape in 0..6 {
            let name = [
                "uniform",
                "powerlaw",
                "single_hub_ordered",
                "ordered",
                "reverse",
                "organ_pipe",
            ][shape];
            let vertices = n / 16;
            let mut input: Vec<Ref> = (0..n)
                .map(|position| {
                    let vertex = match shape {
                        0 => mix(position as u64) as usize % vertices,
                        1 if position % 5 != 0 => {
                            mix(position as u64) as usize % (vertices / 100).max(1)
                        }
                        1 => mix(position as u64) as usize % vertices,
                        2 => 0,
                        _ => position,
                    };
                    let key = if shape >= 3 {
                        (vertex as u128) << 64
                    } else {
                        id(vertex as u64)
                    };
                    (
                        key,
                        if shape == 2 { 0 } else { (position % 4) as u64 },
                        position as u64,
                    )
                })
                .collect();
            if shape == 4 {
                input.reverse();
            }
            if shape == 5 {
                for (i, value) in input.iter_mut().enumerate() {
                    value.0 = ((i.min(n - i)) as u128) << 64;
                }
            }
            let mut expected = input.clone();
            expected.sort_unstable();
            measure(&input, &expected, true);
            measure(&input, &expected, false);
            let mut old = [Duration::ZERO; 5];
            let mut new = [Duration::ZERO; 5];
            let mut old_polls = 0;
            let mut new_polls = 0;
            for round in 0..5 {
                if round % 2 == 0 {
                    (old[round], old_polls) = measure(&input, &expected, true);
                    (new[round], new_polls) = measure(&input, &expected, false);
                } else {
                    (new[round], new_polls) = measure(&input, &expected, false);
                    (old[round], old_polls) = measure(&input, &expected, true);
                }
            }
            old.sort_unstable();
            new.sort_unstable();
            println!(
                "shape={name} n={n} bytes_per_ref={} old_ms={:.3} old_min={:.3} old_max={:.3} new_ms={:.3} new_min={:.3} new_max={:.3} ratio={:.3} old_polls={old_polls} new_polls={new_polls}",
                std::mem::size_of::<Ref>(),
                old[2].as_secs_f64() * 1000.0,
                old[0].as_secs_f64() * 1000.0,
                old[4].as_secs_f64() * 1000.0,
                new[2].as_secs_f64() * 1000.0,
                new[0].as_secs_f64() * 1000.0,
                new[4].as_secs_f64() * 1000.0,
                old[2].as_secs_f64() / new[2].as_secs_f64()
            );
        }
    }
}
