//! Resumable incidence seeks over the same generation as root-edge lookup.
//! No neighbor vector, new index, or per-query graph representation is built.

use super::*;
use crate::gql_exec::source::{AdjacencyIndex, IndexMap};
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::edge_stream::EdgeExpansionSourceError;

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
            .map_err(|error| {
                EdgeExpansionSourceError::Read(EdgeScanSourceError::Control(error))
            })
    })
}

impl AdjacencyIndex {
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
            assert!(work < 256, "an incidence seek scanned unrelated rows: {work}");
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
