//! Awaitable intake for the SAME compiled probe and candidate predicate kernel.
//! Only the active local binding owns records. A first TRUE witness finishes;
//! anti joins complement successful exhaustion, never a source/control error.

use super::super::asynchronous::{admit, check_admission};
use super::*;
use crate::edge_stream::AsyncGraphWalkSource;
use crate::stream::AsyncVertexScanRecord;

/// A loan of the already admitted outer binding. The mutable record loan
/// makes the future Send without requiring its guarded payload to be Sync.
/// Local incidence records remain separately owned by the probe itself.
pub(super) trait Outer<S: AsyncEdgeJoinSource>:
    EdgeScanSource<Error = S::Error> + Send
{
    fn observe(&self, source: &S, event: GlaExecutionEvent) -> Result<(), S::Error>;
}

impl Probe {
    pub(in crate::edge_stream::join) async fn accepts_async<S, F, C>(
        &self,
        outer: &Traversal,
        outer_records: &mut [S::Record],
        source: &mut S,
        meter: &mut Meter<F>,
        seq: CommitSeq,
    ) -> ScanResult<bool, S::Error, C>
    where
        S: AsyncEdgeJoinSource,
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        let frame = EdgeOuter::<S> {
            records: outer_records,
            choices: &outer.choices,
            seq,
        };
        self.accepts_async_outer(&outer.bindings, frame, source, meter)
            .await
    }

    pub(crate) async fn accepts_vertex_async<S, F, C>(
        &self,
        vid: VId,
        root: &mut S::RootRecord,
        source: &mut S,
        meter: &mut Meter<F>,
        seq: CommitSeq,
    ) -> ScanResult<bool, S::Error, C>
    where
        S: AsyncGraphWalkSource,
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        let frame = VertexOuter::<S> { vid, root, seq };
        self.accepts_async_outer(&[Some(vid)], frame, source, meter)
            .await
    }

    async fn accepts_async_outer<S, O, F, C>(
        &self,
        bindings: &[Option<VId>],
        mut outer: O,
        source: &mut S,
        meter: &mut Meter<F>,
    ) -> ScanResult<bool, S::Error, C>
    where
        S: AsyncEdgeJoinSource,
        O: Outer<S>,
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        debug_assert_eq!(bindings.len(), self.outer_width);
        // Admission precedes vectors and payloads. This guard covers fixed
        // slots; every awaited record brings its own source payload guard.
        // Declare the guard first so local records/vectors drop before refund.
        let _guard = flatten(
            source.reserve_traversal(self.outer_width + self.steps.len() - 1, &mut |event| {
                meter.event(event)
            }),
        )?;
        for _ in 0..self.outer_width + 4 * self.steps.len() {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
        }
        let mut ids = Vec::with_capacity(self.outer_width + self.steps.len());
        for id in bindings {
            meter.event(GlaExecutionEvent::Work)?;
            ids.push(*id);
        }
        let mut positions: Vec<_> = self
            .steps
            .iter()
            .map(|step| Position::new(step.binding))
            .collect();
        let mut records: Vec<Option<S::Record>> = (0..self.steps.len()).map(|_| None).collect();
        let mut walks = BTreeMap::<usize, walk::Endpoints>::new();
        let mut depth = 0;
        loop {
            meter.event(GlaExecutionEvent::Work)?;
            if depth == self.steps.len() {
                return Ok(!self.anti);
            }
            // Retrying this binding drops its previous candidate, not any
            // accepted ancestor. Never keep a sibling's fields through descent.
            records[depth] = None;
            let step = &self.steps[depth];
            let candidate = match step.binding {
                Binding::Copy {
                    source: from,
                    nullable,
                } => {
                    if matches!(positions[depth], Position::Copy(true)) {
                        None
                    } else {
                        positions[depth] = Position::Copy(true);
                        let value = ids[from];
                        if value.is_none() && !nullable {
                            None
                        } else {
                            Some(value)
                        }
                    }
                }
                Binding::Expand(expansion) => {
                    if let Some(from) = ids[expansion.source] {
                        let Position::Edge(mut after) = positions[depth] else {
                            unreachable!("compiled fixed expansion position");
                        };
                        let mut admitted = None;
                        let result = source
                            .next_incident_candidate(
                                from,
                                expansion.relation,
                                expansion.direction,
                                after,
                                &mut |event| admit(meter, &mut after, &mut admitted, event),
                            )
                            .await;
                        let candidate = match result {
                            Ok(candidate) => candidate,
                            Err(EdgeExpansionSourceError::Read(error)) => flatten(Err(error))?,
                            Err(EdgeExpansionSourceError::Unavailable) => {
                                return Err(GqlQueryError::Source(
                                    EdgeScanError::ExpansionUnavailable,
                                ));
                            }
                        };
                        check_admission(&candidate, admitted)?;
                        positions[depth] = Position::Edge(after);
                        if let Some(candidate) = candidate {
                            // Invisible identities still advance and consume
                            // candidate admission. They are not NULL witnesses.
                            let Some(record) = candidate.record else {
                                continue;
                            };
                            meter.event(GlaExecutionEvent::Work)?;
                            let edge = record.edge();
                            if !expansion.relation.matches(edge.relation) {
                                continue;
                            }
                            let to = match expansion.direction {
                                GlaDirection::Forward if edge.source == from => edge.target,
                                GlaDirection::Reverse if edge.target == from => edge.source,
                                GlaDirection::Undirected if edge.source == from => edge.target,
                                GlaDirection::Undirected if edge.target == from => edge.source,
                                _ => continue,
                            };
                            for vid in [from, to] {
                                meter.event(GlaExecutionEvent::Work)?;
                                if record.vertex(vid).is_none() {
                                    return Err(GqlQueryError::Source(
                                        EdgeScanError::DanglingEndpoint,
                                    ));
                                }
                            }
                            records[depth] = Some(record);
                            Some(Some(to))
                        } else {
                            None
                        }
                    } else {
                        None // A NULL capture cannot seed a positive edge match.
                    }
                }
                Binding::Walk {
                    expansion,
                    bounds,
                    search,
                } => match ids[expansion.source] {
                    None => None, // An identity path cannot manufacture a NULL seed.
                    Some(from) => {
                        if let std::collections::btree_map::Entry::Vacant(slot) = walks.entry(depth)
                        {
                            // Validate even an identity path without opening
                            // an index. Existing local/outer records are the
                            // only authority for this correlated seed.
                            {
                                let loan = Records {
                                    outer: &outer,
                                    records: &records,
                                    positions: &positions,
                                };
                                vertex(&loan, from, &mut |event| meter.event(event))?;
                            }
                            let walk = walk::Endpoints::from_admitted_start(
                                from,
                                bounds,
                                search,
                                &mut |event| walk::async_event(source, &outer, meter, event),
                            )?;
                            walk::async_event(
                                source,
                                &outer,
                                meter,
                                GlaExecutionEvent::ScratchEntry,
                            )?;
                            slot.insert(walk);
                        }
                        let candidate = walks
                            .get_mut(&depth)
                            .expect("initialized path support")
                            .next_async(expansion, source, meter, &mut outer)
                            .await?;
                        candidate.map(|(vertex, record)| {
                            records[depth] = record;
                            Some(vertex)
                        })
                    }
                },
                // Physical admission rejects independent scans before source
                // construction; do not reinterpret a missing owner as EOF.
                Binding::Scan => {
                    return Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable));
                }
            };
            let Some(candidate) = candidate else {
                walks.remove(&depth);
                if depth == 0 {
                    return Ok(self.anti);
                }
                depth -= 1;
                ids.pop();
                continue;
            };
            ids.push(candidate);
            let passed = {
                let loan = Records {
                    outer: &outer,
                    records: &records,
                    positions: &positions,
                };
                self.test_candidate(&step.predicates, &ids, &positions, &loan, &mut |event| {
                    meter.event(event)?;
                    // Candidate-local temporaries die before backtracking.
                    // Charge the deepest retained local record when possible,
                    // rather than accumulating every sibling's charge on the
                    // outer row. A copy-only prefix still has an outer owner.
                    let observed = match records.iter().rev().flatten().next() {
                        Some(owner) => source.evaluation_event(owner, event),
                        None => outer.observe(source, event),
                    };
                    observed.map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
                })?
            };
            if !passed {
                ids.pop();
                continue;
            }
            depth += 1;
            if depth < self.steps.len() {
                positions[depth] = Position::new(self.steps[depth].binding);
                walks.remove(&depth); // Each new local prefix owns fresh history.
            }
        }
    }
}

// Loan only records already admitted by the real async source, at one cut.
// It performs no I/O and has no incidence/index implementation. This is the
// probe analogue of FrameSource, not a second graph snapshot or evaluator.
struct Records<'a, O, R> {
    outer: &'a O,
    records: &'a [Option<R>],
    positions: &'a [Position],
}
impl<O: EdgeScanSource, R: AsyncEdgeScanRecord> EdgeScanSource for Records<'_, O, R> {
    type Error = O::Error;
    fn snapshot_seq(&self) -> CommitSeq {
        self.outer.snapshot_seq()
    }
    fn next_edge<C>(
        &mut self,
        _control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        unreachable!("admitted probe records are not a graph source")
    }
    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        for (position, record) in self.positions.iter().zip(self.records) {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            if matches!(position, Position::Edge(Some(current)) if *current == eid)
                && let Some(record) = record
            {
                return Ok(Some(record.edge()));
            }
        }
        self.outer.edge(eid, control)
    }
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        for record in self.records {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            if let Some(record) = record {
                let edge = record.edge();
                if vid == edge.source || vid == edge.target {
                    return Ok(record.vertex(vid));
                }
            }
        }
        self.outer.vertex(vid, control)
    }
}

struct EdgeOuter<'a, S: AsyncEdgeJoinSource> {
    records: &'a mut [S::Record],
    choices: &'a [Choice],
    seq: CommitSeq,
}

impl<S: AsyncEdgeJoinSource> Outer<S> for EdgeOuter<'_, S> {
    fn observe(&self, source: &S, event: GlaExecutionEvent) -> Result<(), S::Error> {
        source.evaluation_event(self.records.last().expect("outer record"), event)
    }
}

impl<S: AsyncEdgeJoinSource> EdgeScanSource for EdgeOuter<'_, S> {
    type Error = S::Error;

    fn snapshot_seq(&self) -> CommitSeq {
        self.seq
    }

    fn next_edge<C>(
        &mut self,
        _control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        unreachable!("an outer binding is never a graph scan")
    }

    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        for (choice, record) in self.choices.iter().zip(self.records.iter()) {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            if choice.eid == eid {
                return Ok(Some(record.edge()));
            }
        }
        Ok(None)
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        for record in self.records.iter() {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            let edge = record.edge();
            if vid == edge.source || vid == edge.target {
                return Ok(record.vertex(vid));
            }
        }
        Ok(None)
    }
}

struct VertexOuter<'a, S: AsyncGraphWalkSource> {
    vid: VId,
    root: &'a mut S::RootRecord,
    seq: CommitSeq,
}

impl<S: AsyncGraphWalkSource> Outer<S> for VertexOuter<'_, S> {
    fn observe(&self, source: &S, event: GlaExecutionEvent) -> Result<(), S::Error> {
        source.root_evaluation_event(self.root, event)
    }
}

impl<S: AsyncGraphWalkSource> EdgeScanSource for VertexOuter<'_, S> {
    type Error = S::Error;

    fn snapshot_seq(&self) -> CommitSeq {
        self.seq
    }

    fn next_edge<C>(
        &mut self,
        _control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        unreachable!("an outer vertex is never a graph scan")
    }

    fn edge<'a, C>(
        &'a self,
        _eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok(None)
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        Ok((vid == self.vid).then(|| self.root.as_row()))
    }
}

#[cfg(test)]
mod tests;
