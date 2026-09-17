//! Borrowed scans of an already admitted immutable Snapshot generation.
//! These helpers do not validate raw blocks or bypass snapshot admission.

mod aggregation;

use crate::Snapshot;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_strata::AdjacencyEntry;
use fgdb_strata::vertex::{VertexPatchRows, VertexRow};
use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceEvent {
    Work,
    ScratchEntry,
    SnapshotRecord,
}
/// Admitted topology keeps edge identity so captured paths name real edges.
type IdentifiedEdge = (EId, VId, RelationId, VId);
type VertexCursor = Reverse<(VId, CommitSeq, usize, usize)>;

/// Rebuildable coordinates into one admitted generation, never an authority
/// beside its blocks. Histories retain every version; endpoint lists are in
/// EId order, including both incidences of self loops only once at lookup.
#[derive(Clone, Debug)]
pub(crate) struct AdjacencyIndex {
    histories: Vec<Vec<(CommitSeq, usize, usize)>>,
    outgoing: BTreeMap<VId, Vec<usize>>,
    incoming: BTreeMap<VId, Vec<usize>>,
}

impl AdjacencyIndex {
    pub(crate) fn build(blocks: &[Vec<AdjacencyEntry>]) -> Self {
        let mut by_id = BTreeMap::<EId, Vec<(CommitSeq, usize, usize)>>::new();
        for (block, entries) in blocks.iter().enumerate() {
            for (row, entry) in entries.iter().enumerate() {
                by_id
                    .entry(entry.eid)
                    .or_default()
                    .push((entry.created_at, block, row));
            }
        }
        let mut index = Self {
            histories: Vec::with_capacity(by_id.len()),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
        };
        for mut history in by_id.into_values() {
            let id = index.histories.len();
            // Equal creation sequences use the last block/row, exactly like
            // visit_edges' replacement rule (including retirement images).
            history.sort_unstable();
            for &(_, block, row) in &history {
                let entry = &blocks[block][row];
                for (lists, endpoint) in [
                    (&mut index.outgoing, entry.src),
                    (&mut index.incoming, entry.dst),
                ] {
                    let list = lists.entry(endpoint).or_default();
                    if list.last() != Some(&id) {
                        list.push(id);
                    }
                }
            }
            index.histories.push(history);
        }
        index
    }

    fn visit<'a, E, C>(
        &self,
        blocks: &'a [Vec<AdjacencyEntry>],
        endpoint: VId,
        direction: fgdb_gql::algebra::GlaDirection,
        as_of: CommitSeq,
        control: &mut C,
        mut visit: impl FnMut(&'a AdjacencyEntry, &mut C) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(SourceEvent) -> Result<(), E>,
    {
        use fgdb_gql::algebra::GlaDirection;
        control(SourceEvent::Work)?;
        let outgoing = self.outgoing.get(&endpoint).map_or(&[][..], Vec::as_slice);
        let incoming = self.incoming.get(&endpoint).map_or(&[][..], Vec::as_slice);
        let (mut left, mut right) = match direction {
            GlaDirection::Forward => (outgoing, &[][..]),
            GlaDirection::Reverse => (incoming, &[][..]),
            GlaDirection::Undirected => (outgoing, incoming),
        };
        while !left.is_empty() || !right.is_empty() {
            control(SourceEvent::Work)?;
            let id = match (left.first(), right.first()) {
                (Some(a), Some(b)) => *a.min(b),
                (Some(a), None) | (None, Some(a)) => *a,
                (None, None) => break,
            };
            if left.first() == Some(&id) {
                left = &left[1..];
            }
            if right.first() == Some(&id) {
                right = &right[1..];
            }
            let history = &self.histories[id];
            let end = history.partition_point(|&(created, _, _)| created <= as_of);
            let Some(&(_, block, row)) = end.checked_sub(1).map(|at| &history[at]) else {
                continue;
            };
            let entry = &blocks[block][row];
            let incident = match direction {
                GlaDirection::Forward => entry.src == endpoint,
                GlaDirection::Reverse => entry.dst == endpoint,
                GlaDirection::Undirected => entry.src == endpoint || entry.dst == endpoint,
            };
            if incident && entry.visible_at(as_of) {
                visit(entry, control)?;
            }
        }
        Ok(())
    }
}

/// Rebuildable equality candidates over one admitted generation, never an
/// authority beside its patches (FG-INV-18). Keys are (property key, canonical
/// scalar transcript) seen in ANY version of a vertex's history; candidates
/// are a sorted, deduped superset. The visible winner row at `as_of` is the
/// only authority — the caller re-checks the predicate against it.
#[derive(Clone, Debug)]
pub(crate) struct PropertyEqualityIndex {
    candidates: BTreeMap<(PropertyKeyId, Box<[u8]>), Vec<VId>>,
    histories: BTreeMap<VId, Vec<(CommitSeq, usize, usize)>>,
}

impl PropertyEqualityIndex {
    pub(crate) fn build(patches: &[VertexPatchRows]) -> Self {
        let mut candidates: BTreeMap<(PropertyKeyId, Box<[u8]>), Vec<VId>> = BTreeMap::new();
        let mut histories: BTreeMap<VId, Vec<(CommitSeq, usize, usize)>> = BTreeMap::new();
        for (patch_at, patch) in patches.iter().enumerate() {
            for (row_at, row) in patch.iter().enumerate() {
                histories
                    .entry(row.vid)
                    .or_default()
                    .push((row.created_at, patch_at, row_at));
                for (key, value) in &row.props {
                    if matches!(value, CanonicalScalar::Null) {
                        continue;
                    }
                    let Ok(encoded) = value.encode() else {
                        continue;
                    };
                    candidates
                        .entry((*key, encoded.into_boxed_slice()))
                        .or_default()
                        .push(row.vid);
                }
            }
        }
        for vids in candidates.values_mut() {
            vids.sort_unstable();
            vids.dedup();
        }
        for history in histories.values_mut() {
            history.sort_unstable();
        }
        Self {
            candidates,
            histories,
        }
    }

    /// Sorted candidate VIds whose history ever carried this exact canonical
    /// value under `key`, or an empty slice when none did.
    pub(crate) fn lookup(&self, key: PropertyKeyId, value: &CanonicalScalar) -> &[VId] {
        let Ok(encoded) = value.encode() else {
            return &[];
        };
        self.candidates
            .get(&(key, encoded.into_boxed_slice()))
            .map_or(&[][..], Vec::as_slice)
    }

    /// Latest statement at the cut, with later patches winning equal creation
    /// sequences exactly as in visit_vertices. Retirements remain authoritative.
    fn visible_row<'a, E>(
        &self,
        patches: &'a [VertexPatchRows],
        vid: VId,
        as_of: CommitSeq,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<Option<&'a VertexRow>, E> {
        let Some(history) = self.histories.get(&vid) else {
            return Ok(None);
        };
        let (mut low, mut high) = (0, history.len());
        while low < high {
            control(SourceEvent::Work)?;
            let middle = low + (high - low) / 2;
            if history[middle].0 <= as_of {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low.checked_sub(1).and_then(|at| {
            let (_, patch, row) = history[at];
            let row = &patches[patch][row];
            row.visible_at(as_of).then_some(row)
        }))
    }
}

/// Serve an equality-bound vertex-only plan from the equality index. Returns
/// `None` unless the plan is a vertex scan whose prefix constrains slot 0 (or
/// slot 1 with an identical value bound through the join) with an equality
/// predicate; every other shape keeps the scan path verbatim.
fn bound_vertices<'a, E, Row>(
    snapshot: &'a Snapshot,
    logical: &fgdb_gql::algebra::GlaPlan<Row>,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<Vec<&'a VertexRow>>, E> {
    use fgdb_gql::algebra::{GlaOperator, IntegerComparison, VertexPredicate};
    if !matches!(logical.operators().first(), Some(GlaOperator::ScanVertices)) {
        return Ok(None);
    }
    let prefix = &logical.operators()[1..];
    let mut equality: Option<(PropertyKeyId, CanonicalScalar)> = None;
    let mut bound_predicates: &[VertexPredicate] = &[];
    for op in prefix {
        match op {
            GlaOperator::Select { slot, predicates } if slot.ordinal() < 2 => {
                let mut found = None;
                for predicate in predicates {
                    let (key, value) = match predicate {
                        VertexPredicate::IntegerProperty {
                            key,
                            comparison: IntegerComparison::Equal,
                            value,
                        } => (*key, CanonicalScalar::Int(*value)),
                        VertexPredicate::ScalarProperty { key, predicate }
                            if predicate.comparison() == IntegerComparison::Equal =>
                        {
                            // Equality on a non-integer canonical scalar; a
                            // stored Null is never an equality candidate.
                            match predicate.value() {
                                CanonicalScalar::Null => continue,
                                scalar => (*key, scalar.clone()),
                            }
                        }
                        _ => continue,
                    };
                    found = Some((key, value));
                    break;
                }
                if let Some((key, value)) = found {
                    equality = Some((key, value));
                    bound_predicates = predicates;
                    break;
                }
            }
            // Only a leading run of selections is index-served; anything
            // structural after the scan keeps the scan path.
            GlaOperator::Project { .. } | GlaOperator::Distinct | GlaOperator::OrderByVertexId => {}
            _ => return Ok(None),
        }
    }
    let Some((key, value)) = equality else {
        return Ok(None);
    };
    let mut rows = Vec::new();
    for vid in snapshot.property_index.lookup(key, &value) {
        control(SourceEvent::Work)?;
        // Resolve only this candidate's history, not every snapshot patch.
        // The visible row remains the authority for the complete predicate.
        if let Some(row) =
            snapshot
                .property_index
                .visible_row(&snapshot.patches, *vid, as_of, control)?
        {
            if bound_predicates
                .iter()
                .all(|predicate| predicate.matches(&row.labels, &row.props))
            {
                control(SourceEvent::SnapshotRecord)?;
                control(SourceEvent::ScratchEntry)?;
                rows.push(row);
            }
        }
    }
    Ok(Some(rows))
}

#[cfg(test)]
mod indexed_tests {
    use super::*;
    use fgdb_gql::algebra::GlaDirection;

    #[test]
    fn indexed_history_matches_scan_before_and_after_retirement() {
        for seed in [3_u128, 17, 91] {
            let mut blocks = Vec::new();
            for seq in 1..=6 {
                let mut block = Vec::new();
                for id in 1..=30 {
                    block.push(AdjacencyEntry {
                        src: VId((id * seed) % 7),
                        dst: VId((id * seed + id / 3) % 7),
                        relation: RelationId(1),
                        eid: EId(id),
                        created_at: CommitSeq(seq),
                        retired_at: (id % 4 == 0 && seq >= 4).then_some(CommitSeq(4)),
                    });
                }
                blocks.push(block);
            }
            let index = AdjacencyIndex::build(&blocks);
            let mut nonempty = false;
            let mut deleted = false;
            for at in 0..=7 {
                for endpoint in (0..7).map(VId) {
                    for direction in [
                        GlaDirection::Forward,
                        GlaDirection::Reverse,
                        GlaDirection::Undirected,
                    ] {
                        let mut expected = Vec::new();
                        visit_edges(
                            &blocks,
                            CommitSeq(at),
                            &mut |_| Ok::<_, ()>(()),
                            |entry, _| {
                                let incident = match direction {
                                    GlaDirection::Forward => entry.src == endpoint,
                                    GlaDirection::Reverse => entry.dst == endpoint,
                                    GlaDirection::Undirected => {
                                        entry.src == endpoint || entry.dst == endpoint
                                    }
                                };
                                if incident {
                                    expected.push(entry);
                                }
                                Ok(())
                            },
                        )
                        .unwrap();
                        let mut actual = Vec::new();
                        index
                            .visit(
                                &blocks,
                                endpoint,
                                direction,
                                CommitSeq(at),
                                &mut |_| Ok::<_, ()>(()),
                                |entry, _| {
                                    actual.push(entry);
                                    Ok(())
                                },
                            )
                            .unwrap();
                        assert_eq!(
                            actual, expected,
                            "seed={seed} at={at} endpoint={endpoint:?} direction={direction:?}"
                        );
                        nonempty |= !actual.is_empty();
                        if at >= 4 {
                            assert!(actual.iter().all(|entry| entry.eid.0 % 4 != 0));
                            deleted = true;
                        }
                    }
                }
            }
            assert!(nonempty && deleted);
        }
    }

    #[test]
    fn indexed_lookup_refuses_at_every_source_event() {
        let blocks = vec![vec![AdjacencyEntry {
            src: VId(1),
            dst: VId(2),
            relation: RelationId(1),
            eid: EId(1),
            created_at: CommitSeq(1),
            retired_at: None,
        }]];
        let index = AdjacencyIndex::build(&blocks);
        let run = |stop| {
            let mut seen = 0;
            let result = index.visit(
                &blocks,
                VId(1),
                GlaDirection::Forward,
                CommitSeq(1),
                &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                },
                |_, control| {
                    control(SourceEvent::SnapshotRecord)?;
                    control(SourceEvent::ScratchEntry)
                },
            );
            (result, seen)
        };
        let (result, total) = run(usize::MAX);
        assert_eq!(result, Ok(()));
        assert!(total >= 4);
        for stop in 1..=total {
            assert_eq!(run(stop), (Err(stop), stop));
        }
    }

    #[test]
    fn property_index_candidates_are_a_superset_of_visible_winners() {
        let row = |vid: u64, created: u64, retired: Option<u64>, value: i64| VertexRow {
            vid: VId(vid as u128),
            birth_ordinal: vid,
            created_at: CommitSeq(created),
            retired_at: retired.map(CommitSeq),
            labels: vec![fgdb_delta_types::LabelId(1)],
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        };
        let patch = |rows: &[VertexRow]| {
            let bytes = fgdb_strata::vertex::encode_patch(rows).unwrap();
            fgdb_strata::vertex::decode_patch(&bytes).unwrap()
        };
        let patches = vec![
            // v1: value 7 at creation, changed to 8; v2 created with 7 later.
            patch(&[VertexRow {
                ..row(1, 1, None, 7)
            }]),
            patch(&[
                VertexRow {
                    retired_at: Some(CommitSeq(2)),
                    ..row(1, 1, None, 7)
                },
                row(1, 2, None, 8),
                row(2, 2, None, 7),
            ]),
            // v2 deleted at seq 3.
            patch(&[VertexRow {
                retired_at: Some(CommitSeq(3)),
                ..row(2, 2, None, 7)
            }]),
        ];
        let index = PropertyEqualityIndex::build(&patches);
        // Candidates cover every history carrier of the value; they are a
        // superset of the visible winners at any single cut.
        let candidates: Vec<VId> = index
            .lookup(PropertyKeyId(1), &CanonicalScalar::Int(7))
            .to_vec();
        assert_eq!(candidates, vec![VId(1), VId(2)]);
        let mut ever_nonempty = false;
        for at in 0..=4 {
            let visible = scan_vertices(&patches, CommitSeq(at), &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .into_iter()
                .filter(|row| {
                    row.props.iter().any(|(key, value)| {
                        *key == PropertyKeyId(1) && *value == CanonicalScalar::Int(7)
                    })
                })
                .map(|row| row.vid)
                .collect::<Vec<_>>();
            // At every cut the re-checked winner set equals the scan answer.
            let mut winners = Vec::new();
            for vid in index.lookup(PropertyKeyId(1), &CanonicalScalar::Int(7)) {
                if let Some(row) = index
                    .visible_row(&patches, *vid, CommitSeq(at), &mut |_| Ok::<_, ()>(()))
                    .unwrap()
                {
                    if row.props.iter().any(|(key, value)| {
                        *key == PropertyKeyId(1) && *value == CanonicalScalar::Int(7)
                    }) {
                        winners.push(row.vid);
                    }
                }
            }
            assert_eq!(winners, visible, "at={at}: recheck must equal the scan");
            ever_nonempty |= !visible.is_empty();
        }
        assert!(ever_nonempty);
    }
}

pub(crate) fn visit_edges<'a, E, C>(
    blocks: &'a [Vec<AdjacencyEntry>],
    as_of: CommitSeq,
    control: &mut C,
    mut visit: impl FnMut(&'a AdjacencyEntry, &mut C) -> Result<(), E>,
) -> Result<(), E>
where
    C: FnMut(SourceEvent) -> Result<(), E>,
{
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
            winners.insert(entry.eid, entry);
        }
    }
    for entry in winners.into_values() {
        control(SourceEvent::Work)?;
        if entry.visible_at(as_of) {
            visit(entry, control)?;
        }
    }
    Ok(())
}
fn scan_edges<E>(
    blocks: &[Vec<AdjacencyEntry>],
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Vec<IdentifiedEdge>, E> {
    let mut rows = Vec::new();
    visit_edges(blocks, as_of, control, |entry, control| {
        control(SourceEvent::SnapshotRecord)?;
        control(SourceEvent::ScratchEntry)?;
        rows.push((entry.eid, entry.src, entry.relation, entry.dst));
        Ok(())
    })?;
    Ok(rows)
}

/// One reusable heap cursor per nonempty patch; winning rows remain borrowed.
pub(crate) fn visit_vertices<'a, E, C>(
    patches: &'a [VertexPatchRows],
    as_of: CommitSeq,
    control: &mut C,
    mut visit: impl FnMut(&'a VertexRow, &mut C) -> Result<(), E>,
) -> Result<(), E>
where
    C: FnMut(SourceEvent) -> Result<(), E>,
{
    let mut heap: BinaryHeap<VertexCursor> = BinaryHeap::new();
    for (patch_at, patch) in patches.iter().enumerate() {
        control(SourceEvent::Work)?;
        if let Some(row) = patch.first() {
            control(SourceEvent::ScratchEntry)?;
            heap.push(Reverse((row.vid, row.created_at, patch_at, 0)));
        }
    }
    let mut group = None;
    let mut winner: Option<&VertexRow> = None;
    while let Some(Reverse((vid, _, patch_at, row_at))) = heap.pop() {
        control(SourceEvent::Work)?;
        if group != Some(vid) {
            if let Some(row) = winner.take().filter(|row| row.visible_at(as_of)) {
                visit(row, control)?;
            }
            group = Some(vid);
        }
        let patch = &patches[patch_at];
        let row = &patch[row_at];
        if row.created_at <= as_of {
            winner = Some(row);
        }
        if let Some(next) = patch.get(row_at + 1) {
            heap.push(Reverse((next.vid, next.created_at, patch_at, row_at + 1)));
        }
    }
    if let Some(row) = winner.filter(|row| row.visible_at(as_of)) {
        visit(row, control)?;
    }
    Ok(())
}
pub(crate) fn scan_vertices<'a, E>(
    patches: &'a [VertexPatchRows],
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Vec<&'a VertexRow>, E> {
    let mut rows = Vec::new();
    visit_vertices(patches, as_of, control, |row, control| {
        control(SourceEvent::SnapshotRecord)?;
        control(SourceEvent::ScratchEntry)?;
        rows.push(row);
        Ok(())
    })?;
    Ok(rows)
}

pub(crate) fn find_vertex<'a, E>(
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

pub(super) struct BorrowedTables<'a> {
    pub(super) vertices: Vec<&'a VertexRow>,
    pub(super) edges: Vec<IdentifiedEdge>,
    pub(super) snapshot_records: u64,
}

/// Admit a conservative edge closure for a predicate-bound root. The algebra
/// still evaluates every predicate/join and owns multiplicity and ordering.
/// Unbound scans retain the original source and its accounting verbatim.
fn bound_edges<E, Row>(
    snapshot: &Snapshot,
    logical: &fgdb_gql::algebra::GlaPlan<Row>,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<Vec<IdentifiedEdge>>, E> {
    use fgdb_gql::algebra::{GlaDirection, GlaOperator};
    let Some(GlaOperator::ScanEdges {
        relation,
        direction,
    }) = logical.operators().first()
    else {
        return Ok(None);
    };
    let prefix = &logical.operators()[1..];
    let predicate = prefix
        .iter()
        .take_while(|op| {
            !matches!(
                op,
                GlaOperator::Expand { .. }
                    | GlaOperator::VarLengthExpand { .. }
                    | GlaOperator::Probe { .. }
                    | GlaOperator::Optional { .. }
                    | GlaOperator::ScanVertices
            )
        })
        .find_map(|op| match op {
            GlaOperator::Select { slot, predicates } if slot.ordinal() < 2 => {
                Some((slot.ordinal(), predicates))
            }
            _ => None,
        });
    let Some((slot, predicates)) = predicate else {
        return Ok(None);
    };
    let lookup_direction = if slot == 0 {
        *direction
    } else {
        match direction {
            GlaDirection::Forward => GlaDirection::Reverse,
            GlaDirection::Reverse => GlaDirection::Forward,
            GlaDirection::Undirected => GlaDirection::Undirected,
        }
    };
    let mut selected = BTreeMap::<EId, &AdjacencyEntry>::new();
    let mut frontier = std::collections::BTreeSet::new();
    visit_vertices(&snapshot.patches, as_of, control, |row, control| {
        control(SourceEvent::Work)?;
        for predicate in predicates {
            for _ in 0..predicate.comparison_work_units() {
                control(SourceEvent::Work)?;
            }
        }
        if predicates
            .iter()
            .all(|p| p.matches(&row.labels, &row.props))
        {
            snapshot.adjacency_index.visit(
                &snapshot.blocks,
                row.vid,
                lookup_direction,
                as_of,
                control,
                |entry, control| {
                    if entry.relation == *relation && !selected.contains_key(&entry.eid) {
                        control(SourceEvent::SnapshotRecord)?;
                        control(SourceEvent::ScratchEntry)?;
                        selected.insert(entry.eid, entry);
                        for endpoint in [entry.src, entry.dst] {
                            if !frontier.contains(&endpoint) {
                                control(SourceEvent::ScratchEntry)?;
                                frontier.insert(endpoint);
                            }
                        }
                    }
                    Ok(())
                },
            )?;
        }
        Ok(())
    })?;
    // Fixed-hop plans consume at most one new adjacency per Expand. Using
    // both endpoints and both directions is a superset even for correlations
    // and cycle closures; no source-level join can discard a valid witness.
    let hops = prefix
        .iter()
        .filter(|op| matches!(op, GlaOperator::Expand { .. }))
        .count();
    let mut visited = std::collections::BTreeSet::new();
    for _ in 0..hops {
        let current = std::mem::take(&mut frontier);
        for endpoint in current {
            control(SourceEvent::Work)?;
            if visited.contains(&endpoint) {
                continue;
            }
            control(SourceEvent::ScratchEntry)?;
            visited.insert(endpoint);
            snapshot.adjacency_index.visit(
                &snapshot.blocks,
                endpoint,
                GlaDirection::Undirected,
                as_of,
                control,
                |entry, control| {
                    if !selected.contains_key(&entry.eid) {
                        control(SourceEvent::SnapshotRecord)?;
                        control(SourceEvent::ScratchEntry)?;
                        selected.insert(entry.eid, entry);
                        for endpoint in [entry.src, entry.dst] {
                            if !frontier.contains(&endpoint) {
                                control(SourceEvent::ScratchEntry)?;
                                frontier.insert(endpoint);
                            }
                        }
                    }
                    Ok(())
                },
            )?;
        }
    }
    Ok(Some(
        selected
            .into_values()
            .map(|entry| (entry.eid, entry.src, entry.relation, entry.dst))
            .collect(),
    ))
}

/// Source selection is shared by scalar and tuple plans. Output columns do
/// not change what snapshot generation or topology is admitted.
pub(super) fn admit<'a, E, Row>(
    snapshot: &'a Snapshot,
    logical: &fgdb_gql::algebra::GlaPlan<Row>,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<BorrowedTables<'a>, E> {
    use fgdb_gql::algebra::GlaOperator;
    if !logical.scans_edges() {
        // Equality-bound vertex scans serve from the per-generation index;
        // every other node shape keeps the O(|V|) scan verbatim.
        let vertices = match bound_vertices(snapshot, logical, as_of, control)? {
            Some(vertices) => vertices,
            None => scan_vertices(&snapshot.patches, as_of, control)?,
        };
        // A node-root semijoin needs both base tables. Keep isolated outer
        // vertices, admit topology once, and charge every base record to the
        // same allowance. Probe execution never rereads either source table.
        let edges = if logical.reads_edges() {
            scan_edges(&snapshot.blocks, as_of, control)?
        } else {
            Vec::new()
        };
        return Ok(BorrowedTables {
            snapshot_records: vertices.len() as u64 + edges.len() as u64,
            vertices,
            edges,
        });
    }
    let edges = match bound_edges(snapshot, logical, as_of, control)? {
        Some(edges) => edges,
        None => scan_edges(&snapshot.blocks, as_of, control)?,
    };
    let mut vertices = Vec::new();
    // Projection-only properties need admitted vertex rows even with no WHERE.
    if logical.needs_vertex_values() {
        let mut candidates = std::collections::BTreeSet::new();
        for &(_, src, relation, dst) in &edges {
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
    pub(super) fn property(&self, vid: VId, key: PropertyKeyId) -> Option<&CanonicalScalar> {
        let row = self.vertices[self
            .vertices
            .binary_search_by_key(&vid, |row| row.vid)
            .ok()?];
        row.props
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|at| &row.props[at].1)
    }
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

/// Execute a bound `FOR SYSTEM_TIME AS OF SEQ ...` query through the same
/// historical snapshot admission path as the explicit `_at` API. The temporal
/// text layer selects only the sequence; it does not create another reader,
/// authorization boundary, budget meter or result contract.
impl<V: asupersync::fs::Vfs + Clone> crate::Database<V> {
    pub fn execute_temporal_graph_text_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::BoundTemporalGraphQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<crate::GqlError, Box<asupersync::error::Error>>,
    > {
        self.execute_graph_pattern_governed_at(cx, query.pattern(), query.as_of(), policy)
    }
}

impl crate::EmbeddedReadView {
    pub fn execute_temporal_graph_text_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::BoundTemporalGraphQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<crate::GqlError, Box<asupersync::error::Error>>,
    > {
        self.execute_graph_pattern_governed_at(cx, query.pattern(), query.as_of(), policy)
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
                    .map(|(entry, _)| (entry.eid, entry.src, entry.relation, entry.dst))
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
