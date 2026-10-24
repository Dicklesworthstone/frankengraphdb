//! Resumable incidence seeks over the same generation as root-edge lookup.
//! No neighbor vector, new index, or per-query graph representation is built.

use super::*;
use crate::Snapshot;
use crate::gql_exec::source::{AdjacencyIndex, Face, HistoryRef, IndexMap, balanced_middle};
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::edge_stream::EdgeExpansionSourceError;
use fgdb_strata::AdjacencyEntry;

pub(super) fn next<C>(
    source: &SnapshotEdgeSource<'_>,
    endpoint: VId,
    direction: GlaDirection,
    after: Option<EId>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
) -> Result<Option<EId>, EdgeExpansionSourceError<ReadError, C>> {
    next_from_view(&source.view, source.cx, endpoint, direction, after, control)
}

/// Shared immutable lookup for edge-rooted joins and vertex-rooted probes.
/// The caller owns the admitted view and position; no extra pin or index copy.
pub(crate) fn next_from_view<C>(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    endpoint: VId,
    direction: GlaDirection,
    after: Option<EId>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
) -> Result<Option<EId>, EdgeExpansionSourceError<ReadError, C>> {
    cx.with_restriction(|| {
        view.snapshot
            .adjacency_index()
            .next_incident_edge(endpoint, direction, after, control)
            .map_err(|error| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(error)))
    })
}

impl AdjacencyIndex {
    /// Resolve one visible coordinate with control over BOTH search paths.
    /// No history ranks, candidate vector, payload copies or cursor allocation
    /// are needed. Even an absent identity pays for the directory nodes it
    /// inspected, and every history predecessor comparison is interruptible.
    ///
    /// This borrows the admitted generation, just like statement_at(). The
    /// caller still owns health/cut admission and any logical record charges;
    /// authorized callers must poll physical work without billing hidden rows.
    pub(crate) fn statement_at_controlled<E>(
        &self,
        blocks: &[impl AsRef<[AdjacencyEntry]>],
        eid: EId,
        as_of: CommitSeq,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<Option<(usize, usize)>, E> {
        control(SourceEvent::Work)?;
        let mut charge = || control(SourceEvent::Work);
        let Some(history) = self.history_charged(eid, &mut charge)? else {
            return Ok(None);
        };
        let coordinate = history.latest_charged(as_of, &mut charge)?;
        // The newest statement can be a retirement. Never search backwards
        // again to substitute an older live statement.
        Ok(coordinate.filter(|&(block, row)| blocks[block].as_ref()[row].visible_at(as_of)))
    }

    /// Visit exact-cut winners directly from the admitted historical index.
    /// The ordered cursor retains at most the AVL height in borrowed pointers;
    /// no all-edge winner map or candidate vector is allocated before output.
    /// Caller controls cover cursor admission and every history predecessor.
    /// This is an internal source primitive, not snapshot/visibility authority.
    fn visit_all_coordinates<'a, E, C>(
        &'a self,
        blocks: &'a [impl AsRef<[AdjacencyEntry]>],
        as_of: CommitSeq,
        control: &mut C,
        mut visit: impl FnMut(&'a AdjacencyEntry, usize, usize, &mut C) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(SourceEvent) -> Result<(), E>,
    {
        control(SourceEvent::Work)?;
        // Reserve the maximum live cursor depth up front, as a descent of
        // the identity trees would hold it.
        for _ in 0..self.identity_height() {
            control(SourceEvent::ScratchEntry)?;
        }
        let mut overlay = self.histories.iter().peekable();
        let mut at = 0;
        loop {
            // The lower identity of the base and the overlay; the overlay
            // carries the whole history of an identity both hold.
            let base = self.base.ids.get(at).copied();
            let history = match (base, overlay.peek()) {
                (Some(eid), Some(&(&tree, history))) if tree <= eid => {
                    overlay.next();
                    if tree == eid {
                        at += 1;
                    }
                    HistoryRef::Tree(history)
                }
                (Some(_), _) => {
                    at += 1;
                    HistoryRef::Flat(self.base.history(at - 1))
                }
                (None, Some(_)) => {
                    let (_, history) = overlay.next().expect("peeked");
                    HistoryRef::Tree(history)
                }
                (None, None) => break,
            };
            control(SourceEvent::Work)?;
            if let Some((block, row)) =
                history.latest_charged(as_of, &mut || control(SourceEvent::Work))?
            {
                let entry = &blocks[block].as_ref()[row];
                if entry.visible_at(as_of) {
                    visit(entry, block, row, control)?;
                }
            }
        }
        Ok(())
    }

    /// Seek the next incident identity without materializing the graph or an
    /// incidence list. Shared by query streams and transaction overlay reads.
    ///
    /// These are historical candidates, not live rows. Every caller must
    /// resolve the winning statement at its admitted cut and recheck relation
    /// and incidence before treating the identity as an observation. This
    /// method grants no snapshot or query authority and owns no cursor state.
    pub(crate) fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, C> {
        match direction {
            GlaDirection::Forward => self.face_successor(Face::Outgoing, endpoint, after, control),
            GlaDirection::Reverse => self.face_successor(Face::Incoming, endpoint, after, control),
            GlaDirection::Undirected => {
                let left = self.face_successor(Face::Outgoing, endpoint, after, control)?;
                let right = self.face_successor(Face::Incoming, endpoint, after, control)?;
                // Strict > after on both sides deduplicates the same EId in
                // both faces (self loops) without suppressing parallel edges.
                Ok(match (left, right) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                })
            }
        }
    }
}

impl AdjacencyIndex {
    /// The least identity above `after` in `face` at `endpoint`: the
    /// overlay's descent, then the base's (each balanced level one Work).
    fn face_successor<C>(
        &self,
        face: Face,
        endpoint: VId,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, C> {
        let (base, overlay) = match face {
            Face::Outgoing => (&self.base.outgoing, &self.outgoing),
            Face::Incoming => (&self.base.incoming, &self.incoming),
        };
        let mut next = successor(overlay, endpoint, after, control)?;
        let (mut low, mut high) = (0, base.vertices.len());
        let mut found = None;
        while low < high {
            control(GlaExecutionEvent::Work)?;
            let middle = balanced_middle(low, high);
            match endpoint.cmp(&base.vertices[middle]) {
                core::cmp::Ordering::Less => high = middle,
                core::cmp::Ordering::Greater => low = middle + 1,
                core::cmp::Ordering::Equal => {
                    found = Some(middle);
                    break;
                }
            }
        }
        let Some(at) = found else {
            return Ok(next);
        };
        let (members, _) = base.members_at(at);
        let (mut low, mut high) = (0, members.len());
        while low < high {
            control(GlaExecutionEvent::Work)?;
            let middle = balanced_middle(low, high);
            let eid = members[middle];
            if after.is_none_or(|after| eid > after) {
                next = Some(next.map_or(eid, |next: EId| next.min(eid)));
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        Ok(next)
    }
}

impl Snapshot {
    /// Borrow topology and properties from the SAME winning coordinate of an
    /// already admitted immutable generation. The caller validates its cut and
    /// supplies physical-scan controls separately from logical row admission.
    /// Authorized callers poll here and charge only after scope filtering.
    pub(crate) fn visit_indexed_edges<'a, E, C>(
        &'a self,
        as_of: CommitSeq,
        control: &mut C,
        mut visit: impl FnMut(
            &'a AdjacencyEntry,
            &'a [(PropertyKeyId, fgdb_types::CanonicalPropertyValue)],
            &mut C,
        ) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(SourceEvent) -> Result<(), E>,
    {
        self.adjacency_index().visit_all_coordinates(
            &self.blocks,
            as_of,
            control,
            |entry, block, row, control| {
                visit(
                    entry,
                    edge_properties_at(&self.block_props, block, row),
                    control,
                )
            },
        )
    }
}

/// Only a face's identities are read; each entry's carried history is not.
fn successor<V, C>(
    face: &IndexMap<VId, IndexMap<EId, V>>,
    endpoint: VId,
    after: Option<EId>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
) -> Result<Option<EId>, C> {
    let mut node = face.0.as_deref();
    let mut incidence = None;
    while let Some(current) = node {
        control(GlaExecutionEvent::Work)?;
        match endpoint.cmp(&current.key) {
            core::cmp::Ordering::Less => node = current.left.0.as_deref(),
            core::cmp::Ordering::Greater => node = current.right.0.as_deref(),
            core::cmp::Ordering::Equal => {
                incidence = Some(&current.value);
                break;
            }
        }
    }
    let Some(incidence) = incidence else {
        return Ok(None);
    };
    let mut node = incidence.0.as_deref();
    let mut next = None;
    while let Some(current) = node {
        control(GlaExecutionEvent::Work)?;
        if after.is_none_or(|after| current.key > after) {
            next = Some(current.key);
            node = current.left.0.as_deref();
        } else {
            node = current.right.0.as_deref();
        }
    }
    // Historical candidates are resolved and rechecked by the existing edge
    // reader. A retirement winner is never replaced with an older live row.
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    #[test]
    fn every_incidence_seek_checkpoint_is_fallible_and_positions_are_caller_owned() {
        let ((), report) = run_async_under_lab(0x6a6f_6904, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let keys = crate::DatabaseKeys::new(
                [0x12; 32],
                DatabaseSecurityNamespaceId([0x23; 32]),
                [0x34; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut batch = crate::WriteBatch::new(RelationId(1));
            for v in 0..8 {
                batch.create_vertex(VId(v), vec![], vec![]);
            }
            for i in 0..32 {
                batch.add_edge(EId(i), VId(i % 8), VId((i + 1) % 8), vec![]);
            }
            batch.add_edge(EId(u128::MAX), VId(0), VId(0), vec![]);
            db.write(&commit, batch).await.unwrap();
            let source = SnapshotEdgeSource {
                view: db.read_session().unwrap(),
                cx: &cx,
                as_of: db.frontier().unwrap(),
                scan: IdentityScanCursor::default(),
            };
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                for after in [None, Some(EId(0)), Some(EId(31)), Some(EId(u128::MAX))] {
                    let mut total = 0;
                    let expected = next(&source, VId(0), direction, after, &mut |_| {
                        total += 1;
                        Ok::<_, usize>(())
                    })
                    .unwrap();
                    assert!(total > 0);
                    for stop in 1..=total {
                        let mut calls = 0;
                        let result = next(&source, VId(0), direction, after, &mut |_| {
                            calls += 1;
                            if calls == stop { Err(stop) } else { Ok(()) }
                        });
                        assert!(
                            matches!(result, Err(EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(at))) if at == stop)
                        );
                        assert_eq!(calls, stop);
                        assert_eq!(
                            next(&source, VId(0), direction, after, &mut |_| Ok::<_, usize>(
                                ()
                            ))
                            .unwrap(),
                            expected
                        );
                        assert!(
                            !source.scan.initialized,
                            "nested seeks cannot initialize or advance the root cursor"
                        );
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn indexed_candidates_preserve_identity_boundaries_and_skip_unrelated_graph() {
        use fgdb_strata::AdjacencyEntry;

        let entry = |eid, src, dst| AdjacencyEntry {
            eid: EId(eid),
            src: VId(src),
            dst: VId(dst),
            relation: RelationId(1),
            created_at: CommitSeq(1),
            retired_at: None,
        };
        let mut rows = vec![
            entry(0, 0, 1),
            entry(7, 0, 1),
            entry(8, 2, 0),
            entry(u128::MAX, 0, 0),
        ];
        for id in 100..4196 {
            rows.push(entry(id, id, id + 1));
        }
        let index = AdjacencyIndex::build(&[rows]);
        for (direction, expected) in [
            (GlaDirection::Forward, vec![EId(0), EId(7), EId(u128::MAX)]),
            (GlaDirection::Reverse, vec![EId(8), EId(u128::MAX)]),
            (
                GlaDirection::Undirected,
                vec![EId(0), EId(7), EId(8), EId(u128::MAX)],
            ),
        ] {
            let mut after = None;
            let mut actual = Vec::new();
            let mut work = 0;
            loop {
                let next = index
                    .next_incident_edge(VId(0), direction, after, &mut |_| {
                        work += 1;
                        Ok::<_, core::convert::Infallible>(())
                    })
                    .unwrap();
                let Some(eid) = next else { break };
                assert!(after.is_none_or(|previous| previous < eid));
                actual.push(eid);
                after = Some(eid);
            }
            assert_eq!(actual, expected);
            assert!(
                work < 256,
                "an incidence seek scanned unrelated rows: {work}"
            );
        }
        assert_eq!(
            index
                .next_incident_edge(
                    VId(u128::MAX),
                    GlaDirection::Undirected,
                    None,
                    &mut |_| Ok::<_, core::convert::Infallible>(()),
                )
                .unwrap(),
            None
        );
    }
}

#[cfg(test)]
mod indexed_scan_tests {
    use super::*;
    use crate::gql_exec::source;
    use fgdb_delta_types::RelationId;
    use fgdb_types::CanonicalScalar;

    fn histories(count: u128) -> Vec<Vec<AdjacencyEntry>> {
        let mut blocks = Vec::new();
        for seq in 1..=4 {
            let mut rows = Vec::new();
            for id in (0..count).chain([u128::MAX]) {
                rows.push(AdjacencyEntry {
                    eid: EId(id),
                    src: VId(id % 7),
                    dst: VId(id % 11),
                    relation: match id % 3 {
                        0 => RelationId(1),
                        1 => RelationId(2),
                        _ => RelationId(3),
                    },
                    created_at: CommitSeq(seq),
                    retired_at: (id % 4 == 0 && seq >= 3).then_some(CommitSeq(3)),
                });
            }
            // Equal creation sequences in later blocks are intentional: the
            // locator and retirement winner must agree with the full scan.
            blocks.push(rows);
        }
        let mut restated = blocks[1].clone();
        for entry in &mut restated {
            if entry.eid.0 % 5 == 0 {
                entry.retired_at = Some(CommitSeq(2));
            }
        }
        blocks.push(restated);
        blocks
    }

    /// A built generation charges exactly what the balanced trees the flat
    /// base replaced charged: a point lookup, one Work, the identity tree,
    /// then the history tree; an incidence seek, the endpoint tree, then the
    /// incidence tree. The trees are rebuilt here from the same sorted items
    /// with the same constructor, and descended as the old lookups did.
    #[test]
    fn a_built_index_charges_exactly_what_its_balanced_trees_did() {
        for count in [0, 1, 6, 31, 200] {
            let blocks = histories(count);
            let index = AdjacencyIndex::build(&blocks);
            let mut work = 0;
            let statements = source::edge_statements(&blocks);
            let trees: IndexMap<EId, IndexMap<(CommitSeq, usize, usize), ()>> =
                IndexMap::from_sorted(
                    statements
                        .chunk_by(|a, b| a.0 == b.0)
                        .map(|group| (group[0].0, source::statement_history(group, &mut work)))
                        .collect(),
                    &mut work,
                );
            let face = |end: fn(&AdjacencyEntry) -> VId| {
                let mut pairs: Vec<(VId, EId)> = blocks
                    .iter()
                    .flatten()
                    .map(|entry| (end(entry), entry.eid))
                    .collect();
                pairs.sort_unstable();
                pairs.dedup();
                let mut work = 0;
                let vertices = pairs
                    .chunk_by(|a, b| a.0 == b.0)
                    .map(|group| {
                        let ids = group.iter().map(|&(_, eid)| (eid, ())).collect();
                        (group[0].0, IndexMap::from_sorted(ids, &mut work))
                    })
                    .collect();
                IndexMap::<VId, IndexMap<EId, ()>>::from_sorted(vertices, &mut work)
            };
            let (outgoing, incoming) = (face(|entry| entry.src), face(|entry| entry.dst));

            for cut in [0, 1, 2, 3, 4, u64::MAX] {
                let at = CommitSeq(cut);
                for id in (0..=count + 1).chain([u128::MAX - 1, u128::MAX]) {
                    let eid = EId(id);
                    // The old lookup: one Work, then each node of each descent.
                    let mut expected = 1;
                    let mut node = trees.0.as_deref();
                    while let Some(current) = node {
                        expected += 1;
                        match eid.cmp(&current.key) {
                            core::cmp::Ordering::Less => node = current.left.0.as_deref(),
                            core::cmp::Ordering::Greater => node = current.right.0.as_deref(),
                            core::cmp::Ordering::Equal => {
                                let mut node = current.value.0.as_deref();
                                while let Some(current) = node {
                                    expected += 1;
                                    node = if current.key.0 <= at {
                                        current.right.0.as_deref()
                                    } else {
                                        current.left.0.as_deref()
                                    };
                                }
                                break;
                            }
                        }
                    }
                    let mut charged = 0;
                    index
                        .statement_at_controlled(&blocks, eid, at, &mut |_| {
                            charged += 1;
                            Ok::<_, ()>(())
                        })
                        .unwrap();
                    assert_eq!(charged, expected, "count={count} cut={cut} eid={id}");
                }
            }
            for vertex in 0..13 {
                for after in [
                    None,
                    Some(EId(0)),
                    Some(EId(count / 2)),
                    Some(EId(u128::MAX)),
                ] {
                    for (direction, tree) in [
                        (GlaDirection::Forward, &outgoing),
                        (GlaDirection::Reverse, &incoming),
                    ] {
                        let mut expected = 0;
                        let old = successor(tree, VId(vertex), after, &mut |_| {
                            expected += 1;
                            Ok::<_, ()>(())
                        })
                        .unwrap();
                        let mut charged = 0;
                        let new = index
                            .next_incident_edge(VId(vertex), direction, after, &mut |_| {
                                charged += 1;
                                Ok::<_, ()>(())
                            })
                            .unwrap();
                        assert_eq!(new, old, "count={count} vertex={vertex} {after:?}");
                        assert_eq!(charged, expected, "count={count} vertex={vertex} {after:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn controlled_point_history_agrees_with_independent_scan_and_legacy_lookup() {
        for count in [0, 1, 7, 31] {
            let blocks = histories(count);
            let index = AdjacencyIndex::build(&blocks);
            for cut in [0, 1, 2, 3, 4, 5, u64::MAX] {
                let at = CommitSeq(cut);
                let mut expected = Vec::new();
                source::visit_edges(&blocks, at, &mut |_| Ok::<_, ()>(()), |row, _| {
                    expected.push(row);
                    Ok(())
                })
                .unwrap();
                for id in (0..=count).chain([u128::MAX - 1, u128::MAX]) {
                    let eid = EId(id);
                    let mut work = 0;
                    let coordinate = index
                        .statement_at_controlled(&blocks, eid, at, &mut |event| {
                            assert_eq!(event, SourceEvent::Work);
                            work += 1;
                            Ok::<_, ()>(())
                        })
                        .unwrap();
                    let actual = coordinate.map(|(block, row)| &blocks[block][row]);
                    let wanted = expected.iter().copied().find(|row| row.eid == eid);
                    assert_eq!(actual, wanted, "count={count} cut={cut} eid={id}");
                    if let (Some(actual), Some(wanted)) = (actual, wanted) {
                        assert!(std::ptr::eq(actual, wanted));
                    }
                    assert_eq!(coordinate, index.statement_at(&blocks, eid, at));
                    let history_height = index.history_height(eid);
                    assert!(work > 0);
                    assert!(work <= 1 + index.identity_height() + history_height);
                }
            }
        }
    }

    #[test]
    fn every_point_directory_and_version_step_can_refuse_without_a_partial_winner() {
        let mut blocks = histories(127);
        // A long single-identity chain tests history work independently of
        // the directory size. The final retirement must not reveal an older
        // live row when the last control callback refuses.
        for seq in 5..=68 {
            blocks.push(vec![AdjacencyEntry {
                eid: EId(0),
                src: VId(0),
                dst: VId(0),
                relation: RelationId(1),
                created_at: CommitSeq(seq),
                retired_at: (seq == 68).then_some(CommitSeq(69)),
            }]);
        }
        let index = AdjacencyIndex::build(&blocks);
        for eid in [EId(0), EId(63), EId(128), EId(u128::MAX)] {
            for cut in [0, 1, 67, 68, 69, u64::MAX] {
                let run = |stop| {
                    let mut calls = 0;
                    let result =
                        index.statement_at_controlled(&blocks, eid, CommitSeq(cut), &mut |event| {
                            assert_eq!(event, SourceEvent::Work);
                            calls += 1;
                            if calls == stop { Err(stop) } else { Ok(()) }
                        });
                    (result, calls)
                };
                let (expected, total) = run(usize::MAX);
                assert!(total > 1);
                if eid == EId(0) {
                    assert!(total > 7, "version traversal disappeared from the controls");
                }
                for stop in 1..=total {
                    assert_eq!(run(stop), (Err(stop), stop));
                }
                assert_eq!(run(usize::MAX), (expected, total));
            }
        }
        let empty_blocks: &[Vec<AdjacencyEntry>] = &[];
        let empty = AdjacencyIndex::build(empty_blocks);
        assert_eq!(
            empty.statement_at_controlled(empty_blocks, EId(0), CommitSeq(0), &mut |_| Err(7)),
            Err(7)
        );
    }

    #[test]
    fn indexed_all_edge_scan_matches_independent_history_fold_at_every_cut() {
        for count in [0, 1, 7, 31] {
            let blocks = histories(count);
            let index = AdjacencyIndex::build(&blocks);
            for seq in 0..=5 {
                let mut expected = Vec::new();
                source::visit_edges(
                    &blocks,
                    CommitSeq(seq),
                    &mut |_| Ok::<_, ()>(()),
                    |row, _| {
                        expected.push(row);
                        Ok(())
                    },
                )
                .unwrap();
                let mut actual = Vec::new();
                index
                    .visit_all_coordinates(
                        &blocks,
                        CommitSeq(seq),
                        &mut |_| Ok::<_, ()>(()),
                        |row, block, at, _| {
                            assert!(std::ptr::eq(row, &blocks[block][at]));
                            actual.push(row);
                            Ok(())
                        },
                    )
                    .unwrap();
                assert_eq!(actual, expected, "count={count} cut={seq}");
                assert!(actual.windows(2).all(|pair| pair[0].eid < pair[1].eid));
            }
        }
    }

    #[test]
    fn first_row_refusal_visits_a_cursor_prefix_not_the_whole_history() {
        let blocks = histories(4096);
        let index = AdjacencyIndex::build(&blocks);
        let mut new_calls = 0;
        let mut new_rows = 0;
        let result = index.visit_all_coordinates(
            &blocks,
            CommitSeq(1),
            &mut |_| {
                new_calls += 1;
                Ok(())
            },
            |row, _, _, _| {
                assert_eq!(row.eid, EId(0));
                new_rows += 1;
                Err(17)
            },
        );
        assert_eq!(result, Err(17));
        assert_eq!(new_rows, 1);
        assert!(
            new_calls < 64,
            "indexed prefix traversed {new_calls} events"
        );
        let mut old_calls = 0;
        let mut old_rows = 0;
        let result = source::visit_edges(
            &blocks,
            CommitSeq(1),
            &mut |_| {
                old_calls += 1;
                Ok(())
            },
            |row, _| {
                assert_eq!(row.eid, EId(0));
                old_rows += 1;
                Err(17)
            },
        );
        assert_eq!(result, Err(17));
        assert_eq!(old_rows, 1);
        assert!(old_calls > 4096, "the live incumbent was not exercised");
    }

    #[test]
    fn indexed_cursor_admission_and_every_predecessor_step_are_fallible() {
        let blocks = histories(16);
        let index = AdjacencyIndex::build(&blocks);
        let run = |stop| {
            let mut calls = 0;
            let mut rows = Vec::new();
            let result = index.visit_all_coordinates(
                &blocks,
                CommitSeq(2),
                &mut |_| {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                },
                |row, _, _, _| {
                    rows.push(row.eid);
                    Ok(())
                },
            );
            (result, calls, rows)
        };
        let (result, total, rows) = run(usize::MAX);
        assert_eq!(result, Ok(()));
        assert!(!rows.is_empty());
        for stop in 1..=total {
            let (result, calls, prefix) = run(stop);
            assert_eq!(result, Err(stop));
            assert_eq!(calls, stop);
            assert!(rows.starts_with(&prefix));
            if stop <= 1 + usize::from(index.histories.height()) {
                assert!(prefix.is_empty(), "output preceded cursor admission");
            }
        }
        assert_eq!(run(usize::MAX), (Ok(()), total, rows));
        let empty_blocks: &[Vec<AdjacencyEntry>] = &[];
        let empty = AdjacencyIndex::build(empty_blocks);
        assert_eq!(
            empty.visit_all_coordinates(
                empty_blocks,
                CommitSeq(0),
                &mut |_| Err(19),
                |_, _, _, _| Ok(())
            ),
            Err(19)
        );
    }

    #[test]
    fn indexed_topology_and_properties_share_the_winner_across_compaction_and_reopen() {
        use asupersync::lab::run_async_under_lab;
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
        fn check(snapshot: &Snapshot, at: CommitSeq) {
            let mut expected = Vec::new();
            source::visit_edges_with_properties(
                snapshot,
                at,
                &mut |_| Ok::<_, ()>(()),
                |entry, props, _| {
                    expected.push((entry, props));
                    Ok(())
                },
            )
            .unwrap();
            let mut actual = Vec::new();
            snapshot
                .visit_indexed_edges(at, &mut |_| Ok::<_, ()>(()), |entry, props, _| {
                    actual.push((entry, props));
                    Ok(())
                })
                .unwrap();
            assert_eq!(actual, expected);
            for ((a, ap), (b, bp)) in actual.iter().zip(&expected) {
                assert!(std::ptr::eq(*a, *b));
                assert!(std::ptr::eq(*ap, *bp));
                let (block, row) = snapshot
                    .adjacency_index()
                    .statement_at_controlled(&snapshot.blocks, a.eid, at, &mut |_| Ok::<_, ()>(()))
                    .unwrap()
                    .unwrap();
                assert!(std::ptr::eq(*a, &snapshot.blocks[block][row]));
                assert!(std::ptr::eq(
                    *ap,
                    edge_properties_at(&snapshot.block_props, block, row)
                ));
            }
        }
        let ((), report) = run_async_under_lab(0xa710, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = crate::MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let keys = || {
                crate::DatabaseKeys::new(
                    [0xb7; 32],
                    DatabaseSecurityNamespaceId([0xb8; 32]),
                    [0xb9; 32],
                )
            };
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let p = PropertyKeyId(1);
            let mut batch = crate::WriteBatch::new(RelationId(1));
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(0), VId(1), VId(2), vec![(p, CanonicalScalar::Int(1))]);
            batch.add_edge(EId(1), VId(1), VId(2), vec![(p, CanonicalScalar::Int(2))]);
            batch.add_edge(EId(u128::MAX), VId(1), VId(1), vec![]);
            let basis = db.write(&cx, batch).await.unwrap();
            let pinned = db.read_session().unwrap();
            for step in 0..3 {
                let mut batch = crate::WriteBatch::new(RelationId(1));
                match step {
                    0 => {
                        batch.set_edge_property(EId(0), p, Some(CanonicalScalar::Int(10)));
                    }
                    1 => {
                        batch.delete_edge(EId(1));
                    }
                    _ => {
                        batch.delete_vertex(VId(2));
                    }
                }
                db.write(&cx, batch).await.unwrap();
                check(&db.snapshot, basis);
                check(&db.snapshot, db.frontier().unwrap());
                check(&pinned.snapshot, basis);
            }
            let frontier = db.frontier().unwrap();
            db.compact(&cx).await.unwrap();
            check(&db.snapshot, frontier);
            check(&pinned.snapshot, basis);
            drop(db);
            let db = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .unwrap();
            check(&db.snapshot, frontier);
            assert_eq!(db.edges().unwrap().len(), 1);
            assert_eq!(db.edges().unwrap()[0].entry.eid, EId(u128::MAX));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
