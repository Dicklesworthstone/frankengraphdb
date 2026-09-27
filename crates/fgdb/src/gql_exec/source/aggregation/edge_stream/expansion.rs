//! Resumable incidence seeks over the same generation as root-edge lookup.
//! No neighbor vector, new index, or per-query graph representation is built.

use super::*;
use crate::gql_exec::source::{AdjacencyIndex, IndexMap};
use crate::Snapshot;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::edge_stream::EdgeExpansionSourceError;
use fgdb_strata::AdjacencyEntry;
use fgdb_types::CanonicalScalar;

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
            .adjacency_index
            .next_incident_edge(endpoint, direction, after, control)
            .map_err(|error| EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(error)))
    })
}

impl AdjacencyIndex {
    /// Visit exact-cut winners directly from the admitted historical index.
    /// The ordered cursor retains at most the AVL height in borrowed pointers;
    /// no all-edge winner map or candidate vector is allocated before output.
    /// Caller controls cover cursor admission and every history predecessor.
    /// This is an internal source primitive, not snapshot/visibility authority.
    fn visit_all_coordinates<'a, E, C>(
        &'a self,
        blocks: &'a [Vec<AdjacencyEntry>],
        as_of: CommitSeq,
        control: &mut C,
        mut visit: impl FnMut(&'a AdjacencyEntry, usize, usize, &mut C) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(SourceEvent) -> Result<(), E>,
    {
        control(SourceEvent::Work)?;
        // Reserve the maximum live cursor depth BEFORE iter() allocates its
        // initial stack. Subsequent descent reuses those logical slots.
        for _ in 0..self.histories.height() {
            control(SourceEvent::ScratchEntry)?;
        }
        for (_, history) in self.histories.iter() {
            control(SourceEvent::Work)?;
            let mut node = history.0.as_deref();
            let mut winner = None;
            while let Some(current) = node {
                control(SourceEvent::Work)?;
                // Rightmost (created_at, block, row) <= this sequence. A
                // retirement restatement wins too: never revive an older row.
                if current.key.0 <= as_of {
                    winner = Some(current.key);
                    node = current.right.0.as_deref();
                } else {
                    node = current.left.0.as_deref();
                }
            }
            if let Some((_, block, row)) = winner {
                let entry = &blocks[block][row];
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
            GlaDirection::Forward => successor(&self.outgoing, endpoint, after, control),
            GlaDirection::Reverse => successor(&self.incoming, endpoint, after, control),
            GlaDirection::Undirected => {
                let left = successor(&self.outgoing, endpoint, after, control)?;
                let right = successor(&self.incoming, endpoint, after, control)?;
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
            &'a [(PropertyKeyId, CanonicalScalar)],
            &mut C,
        ) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(SourceEvent) -> Result<(), E>,
    {
        self.adjacency_index.visit_all_coordinates(
            &self.blocks, as_of, control, |entry, block, row, control| {
                visit(entry, edge_properties_at(&self.block_props, block, row), control)
            },
        )
    }
}

fn successor<C>(
    face: &IndexMap<VId, IndexMap<EId, ()>>,
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
                after: None,
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
                        assert_eq!(
                            source.after, None,
                            "nested seeks cannot advance the root cursor"
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

    #[test]
    fn indexed_all_edge_scan_matches_independent_history_fold_at_every_cut() {
        for count in [0, 1, 7, 31] {
            let blocks = histories(count);
            let index = AdjacencyIndex::build(&blocks);
            for seq in 0..=5 {
                let mut expected = Vec::new();
                source::visit_edges(&blocks, CommitSeq(seq), &mut |_| Ok::<_, ()>(()), |row, _| {
                    expected.push(row);
                    Ok(())
                }).unwrap();
                let mut actual = Vec::new();
                index.visit_all_coordinates(&blocks, CommitSeq(seq), &mut |_| Ok::<_, ()>(()),
                    |row, block, at, _| {
                        assert!(std::ptr::eq(row, &blocks[block][at]));
                        actual.push(row);
                        Ok(())
                    },
                ).unwrap();
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
        let result = index.visit_all_coordinates(&blocks, CommitSeq(1), &mut |_| {
            new_calls += 1;
            Ok(())
        }, |row, _, _, _| {
            assert_eq!(row.eid, EId(0));
            new_rows += 1;
            Err(17)
        });
        assert_eq!(result, Err(17));
        assert_eq!(new_rows, 1);
        assert!(new_calls < 64, "indexed prefix traversed {new_calls} events");
        let mut old_calls = 0;
        let mut old_rows = 0;
        let result = source::visit_edges(&blocks, CommitSeq(1), &mut |_| {
            old_calls += 1;
            Ok(())
        }, |row, _| {
            assert_eq!(row.eid, EId(0));
            old_rows += 1;
            Err(17)
        });
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
            let result = index.visit_all_coordinates(&blocks, CommitSeq(2), &mut |_| {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            }, |row, _, _, _| {
                rows.push(row.eid);
                Ok(())
            });
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
        let empty = AdjacencyIndex::build(&[]);
        assert_eq!(empty.visit_all_coordinates(&[], CommitSeq(0), &mut |_| Err(19),
            |_, _, _, _| Ok(())), Err(19));
    }

    #[test]
    fn indexed_topology_and_properties_share_the_winner_across_compaction_and_reopen() {
        use asupersync::lab::run_async_under_lab;
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
        fn check(snapshot: &Snapshot, at: CommitSeq) {
            let mut expected = Vec::new();
            source::visit_edges_with_properties(snapshot, at, &mut |_| Ok::<_, ()>(()),
                |entry, props, _| { expected.push((entry, props)); Ok(()) },
            ).unwrap();
            let mut actual = Vec::new();
            snapshot.visit_indexed_edges(at, &mut |_| Ok::<_, ()>(()),
                |entry, props, _| { actual.push((entry, props)); Ok(()) },
            ).unwrap();
            assert_eq!(actual, expected);
            for ((a, ap), (b, bp)) in actual.iter().zip(&expected) {
                assert!(std::ptr::eq(*a, *b));
                assert!(std::ptr::eq(*ap, *bp));
            }
        }
        let ((), report) = run_async_under_lab(0xa710, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = crate::MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let keys = || crate::DatabaseKeys::new(
                [0xb7; 32], DatabaseSecurityNamespaceId([0xb8; 32]), [0xb9; 32],
            );
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys()).await.unwrap();
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
                    0 => { batch.set_edge_property(EId(0), p, Some(CanonicalScalar::Int(10))); }
                    1 => { batch.delete_edge(EId(1)); }
                    _ => { batch.delete_vertex(VId(2)); }
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
            let db = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
            check(&db.snapshot, frontier);
            assert_eq!(db.edges().unwrap().len(), 1);
            assert_eq!(db.edges().unwrap()[0].entry.eid, EId(u128::MAX));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
