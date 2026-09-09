//! Borrowed scans of an already admitted, immutable Snapshot generation.
//!
//! These are not raw-block validators. Snapshot construction owns content,
//! topology and version-chain admission; callers cannot construct a Snapshot.
//! Never use these helpers to admit untrusted blocks or skip that boundary.

use crate::Snapshot;
use fgdb_delta_types::RelationId;
use fgdb_strata::AdjacencyEntry;
use fgdb_strata::vertex::{VertexPatchRows, VertexRow};
use fgdb_types::{CommitSeq, EId, VId};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

/// Emitted before the corresponding work or allocation. Scratch counts new
/// logical entries, not allocator bytes; reused heap slots are not recharged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceEvent {
    Work,
    ScratchEntry,
    SnapshotRecord,
}

type EdgeTriple = (VId, RelationId, VId);
type VertexCursor = Reverse<(VId, CommitSeq, usize, usize)>;

/// One candidate per EId, not per historical content version. Property
/// sidecars are never read or cloned: bounded MATCH consumes topology only.
fn scan_edges<E>(
    blocks: &[Vec<AdjacencyEntry>],
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Vec<EdgeTriple>, E> {
    let mut winners: BTreeMap<EId, &AdjacencyEntry> = BTreeMap::new();
    for block in blocks {
        control(SourceEvent::Work)?;
        for entry in block {
            control(SourceEvent::Work)?;
            if entry.created_at > as_of {
                continue;
            }
            match winners.get(&entry.eid) {
                Some(previous) if previous.created_at > entry.created_at => continue,
                Some(_) => {}
                None => control(SourceEvent::ScratchEntry)?,
            }
            // Equal creation sequences are restatements; later publication
            // wins, including a retirement. Do NOT filter retirement first.
            winners.insert(entry.eid, entry);
        }
    }
    let mut rows = Vec::new();
    for entry in winners.into_values() {
        control(SourceEvent::Work)?;
        if entry.visible_at(as_of) {
            control(SourceEvent::SnapshotRecord)?;
            control(SourceEvent::ScratchEntry)?;
            rows.push((entry.src, entry.relation, entry.dst));
        }
    }
    Ok(rows)
}

/// Each typed patch is sorted by (VId, creation sequence). Merge with one heap
/// slot per nonempty patch, replacing its slot after every pop. Only visible
/// winners are retained, as references into the pinned generation.
pub(crate) fn scan_vertices<'a, E>(
    patches: &'a [VertexPatchRows],
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Vec<&'a VertexRow>, E> {
    let mut heap: BinaryHeap<VertexCursor> = BinaryHeap::new();
    for (patch_at, patch) in patches.iter().enumerate() {
        control(SourceEvent::Work)?;
        if let Some(row) = patch.first() {
            control(SourceEvent::ScratchEntry)?;
            heap.push(Reverse((row.vid, row.created_at, patch_at, 0)));
        }
    }
    let mut rows = Vec::new();
    let mut group = None;
    let mut winner: Option<&VertexRow> = None;
    while let Some(Reverse((vid, _, patch_at, row_at))) = heap.pop() {
        control(SourceEvent::Work)?;
        if group != Some(vid) {
            emit_vertex(winner.take(), as_of, &mut rows, control)?;
            group = Some(vid);
        }
        let patch = &patches[patch_at];
        let row = &patch[row_at];
        // Heap order visits newer statements and later publications last.
        if row.created_at <= as_of {
            winner = Some(row);
        }
        if let Some(next) = patch.get(row_at + 1) {
            heap.push(Reverse((next.vid, next.created_at, patch_at, row_at + 1)));
        }
    }
    emit_vertex(winner, as_of, &mut rows, control)?;
    Ok(rows)
}

fn emit_vertex<'a, E>(
    winner: Option<&'a VertexRow>,
    as_of: CommitSeq,
    rows: &mut Vec<&'a VertexRow>,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<(), E> {
    if let Some(row) = winner.filter(|row| row.visible_at(as_of)) {
        control(SourceEvent::SnapshotRecord)?;
        control(SourceEvent::ScratchEntry)?;
        rows.push(row);
    }
    Ok(())
}

/// Borrow one predicate source without materializing a history map or cloning
/// properties. Every patch and binary-search comparison is a checkpoint.
fn find_vertex<'a, E>(
    patches: &'a [VertexPatchRows],
    vid: VId,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<&'a VertexRow>, E> {
    let mut winner: Option<&VertexRow> = None;
    for patch in patches {
        control(SourceEvent::Work)?;
        let (mut low, mut high) = (0, patch.len());
        while low < high {
            control(SourceEvent::Work)?;
            let middle = low + (high - low) / 2;
            let row = &patch[middle];
            if (row.vid, row.created_at) <= (vid, as_of) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if low > 0 {
            let row = &patch[low - 1];
            if row.vid == vid && winner.is_none_or(|old| old.created_at <= row.created_at) {
                winner = Some(row);
            }
        }
    }
    Ok(winner.filter(|row| row.visible_at(as_of)))
}

/// Payload references are tied to one admitted generation. Edge property
/// sidecars are not requested, and no graph scalar is cloned during admission.
pub(super) struct BorrowedTables<'a> {
    pub(super) vertices: Vec<&'a VertexRow>,
    pub(super) edges: Vec<EdgeTriple>,
    pub(super) snapshot_records: u64,
}

pub(super) fn admit<'a, E>(
    snapshot: &'a Snapshot,
    logical: &fgdb_gql::algebra::GlaPlan,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<BorrowedTables<'a>, E> {
    use fgdb_gql::algebra::GlaOperator;
    if !logical.scans_edges() {
        let vertices = scan_vertices(&snapshot.patches, as_of, control)?;
        return Ok(BorrowedTables {
            snapshot_records: vertices.len() as u64,
            vertices,
            edges: Vec::new(),
        });
    }
    let edges = scan_edges(&snapshot.blocks, as_of, control)?;
    let mut vertices = Vec::new();
    if logical
        .operators()
        .iter()
        .any(|operator| matches!(operator, GlaOperator::Select { .. }))
    {
        let mut candidates = std::collections::BTreeSet::new();
        for &(src, relation, dst) in &edges {
            control(SourceEvent::Work)?;
            let requested = logical.operators().iter().any(|operator| match operator {
                GlaOperator::ScanEdges {
                    relation: required, ..
                }
                | GlaOperator::Expand {
                    relation: required, ..
                } => *required == relation,
                _ => false,
            });
            if !requested {
                continue;
            }
            for vid in [src, dst] {
                if !candidates.contains(&vid) {
                    control(SourceEvent::ScratchEntry)?;
                    candidates.insert(vid);
                }
            }
        }
        // Predicate sources are not base-table admission records. Their work,
        // candidate set and retained references are still scratch-metered.
        for vid in candidates {
            if let Some(row) = find_vertex(&snapshot.patches, vid, as_of, control)? {
                control(SourceEvent::ScratchEntry)?;
                vertices.push(row);
            }
        }
    }
    Ok(BorrowedTables {
        snapshot_records: edges.len() as u64,
        vertices,
        edges,
    })
}

impl BorrowedTables<'_> {
    pub(super) fn matches(
        &self,
        vid: VId,
        predicates: &[fgdb_gql::algebra::VertexPredicate],
    ) -> bool {
        self.vertices
            .binary_search_by_key(&vid, |row| row.vid)
            .ok()
            .is_some_and(|at| {
                let row = self.vertices[at];
                predicates
                    .iter()
                    .all(|predicate| predicate.matches(&row.labels, &row.props))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::CanonicalScalar;

    fn row(id: u128, created: u64, retired: Option<u64>, value: i64) -> VertexRow {
        VertexRow {
            vid: VId(id),
            birth_ordinal: id as u64,
            created_at: CommitSeq(created),
            retired_at: retired.map(CommitSeq),
            labels: vec![LabelId(1)],
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        }
    }

    fn patch(rows: &[VertexRow]) -> VertexPatchRows {
        let bytes = fgdb_strata::vertex::encode_patch(rows).unwrap();
        fgdb_strata::vertex::decode_patch(&bytes).unwrap()
    }

    #[test]
    fn borrowed_vertex_scan_and_lookup_match_the_independent_storage_merge() {
        let high = (1_u128 << 100) + 3;
        let patches = vec![
            patch(&[row(1, 1, None, 1), row(high, 1, None, 8)]),
            patch(&[row(1, 1, Some(2), 1), row(1, 2, None, 2)]),
            patch(&[row(1, 2, Some(4), 2), row(2, 3, None, 3)]),
            patch(&[row(high, 1, Some(5), 8)]),
        ];
        for at in 0..=6 {
            let expected = fgdb_strata::vertex::merge_all_vertices(&patches, CommitSeq(at));
            let actual = scan_vertices(&patches, CommitSeq(at), &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(actual, expected.iter().collect::<Vec<_>>());
            for vid in [VId(1), VId(2), VId(99), VId(high)] {
                let actual =
                    find_vertex(&patches, vid, CommitSeq(at), &mut |_| Ok::<_, ()>(())).unwrap();
                let expected = fgdb_strata::vertex::merge_vertex(&patches, vid, CommitSeq(at));
                assert_eq!(actual, expected.as_ref());
            }
            assert!(actual.iter().all(|found| {
                patches
                    .iter()
                    .any(|patch| patch.iter().any(|original| std::ptr::eq(*found, original)))
            }));
        }
    }

    fn edge(id: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
        AdjacencyEntry {
            src: VId(1),
            relation: RelationId(1),
            dst: VId(2),
            eid: EId(id),
            created_at: CommitSeq(created),
            retired_at: retired.map(CommitSeq),
        }
    }

    #[test]
    fn edge_candidates_preserve_parallel_ids_and_retirements_without_properties() {
        let blocks = vec![
            vec![edge(1, 1, None), edge(2, 1, None)],
            vec![edge(1, 1, Some(2)), edge(1, 2, None)],
            vec![edge(1, 2, Some(3))],
        ];
        let props = vec![None; blocks.len()];
        for at in 0..=4 {
            let expected =
                fgdb_strata::root::merge_all_edges_with_props(&blocks, &props, CommitSeq(at))
                    .unwrap()
                    .into_iter()
                    .map(|(entry, _)| (entry.src, entry.relation, entry.dst))
                    .collect::<Vec<_>>();
            let actual = scan_edges(&blocks, CommitSeq(at), &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(actual, expected);
        }
        let mut allocations = 0;
        scan_edges(&blocks, CommitSeq(3), &mut |event| {
            allocations += usize::from(event == SourceEvent::ScratchEntry);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(
            allocations, 3,
            "two EId candidates plus one visible triple, not one per version"
        );
    }

    #[test]
    fn every_scan_checkpoint_can_refuse_without_returning_partial_rows() {
        let patches = vec![patch(&[row(1, 1, None, 1), row(2, 1, None, 2)])];
        let blocks = vec![vec![edge(1, 1, None), edge(2, 1, None)]];
        for vertices in [false, true] {
            let run = |stop: usize| {
                let mut events = 0;
                let mut control = |_| {
                    events += 1;
                    if events == stop { Err(stop) } else { Ok(()) }
                };
                let result = if vertices {
                    scan_vertices(&patches, CommitSeq(1), &mut control).map(|rows| rows.len())
                } else {
                    scan_edges(&blocks, CommitSeq(1), &mut control).map(|rows| rows.len())
                };
                (result, events)
            };
            let (success, total) = run(usize::MAX);
            assert_eq!(success, Ok(2));
            for stop in 1..=total {
                assert_eq!(run(stop), (Err(stop), stop));
            }
        }
    }
}
