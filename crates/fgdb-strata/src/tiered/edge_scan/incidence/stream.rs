//! Persistent incidence positions over the existing authenticated history merge.

use super::*;

// The private count lane returns only visibility. A projected endpoint can
// never escape through the public complete-record cursor as an empty property.
// Keep the existing complete-record footprint inline; boxing would add an
// unmetered allocation to every property-bearing pull.
#[allow(clippy::large_enum_variant)]
enum IncidenceProjection {
    Endpoints(BufferedEdgeEndpoints),
    Visible,
}

/// A forward-only incident-edge stream in full-width EId order at one cut.
///
/// The first pull seeks the shared authenticated descriptor directory and
/// seeds one head per routed block, without constructing graph routing. Later
/// pulls retain those heads and advance the SAME history merge; they do not
/// rebuild the routed heap for every returned edge. Refaults filter bounded
/// position permutations but keep original row indices as property locators.
/// All versions of a matching identity remain present until history resolution.
///
/// This retains merge heads and one bounded decoded block, not an adjacency
/// result vector. The directory is charged once during partition admission,
/// with one outgoing key and incoming interval per nonempty block. Broad
/// destination ranges can admit unrelated payload rows. The query's merge
/// remains governed by the MemoryPool and cumulative work ceiling. This is not
/// an optimal sequential-I/O or whole-database memory bound.
///
/// Returned edges and endpoints own independent reservations. An undirected
/// self-loop appears once. Endpoints retain their stored orientation, even for
/// an incoming request. Missing endpoints are errors, not absent matches.
/// Close, any error, or dropping a POLLED pending pull releases private state
/// and permanently fuses the stream. Earlier delivered rows cannot be retracted;
/// successful completion requires exhausting the stream. An unpolled future
/// performs no work and leaves the cursor usable. No durable resume/GC lease or
/// capability grant is created by this owner-level storage API.
pub struct BufferedEdgeIncidenceScan<'source, V: Vfs> {
    driver: Option<Driver<'source, V>>,
    state: Option<State>,
    request: Incidence,
    as_of: CommitSeq,
    work: usize,
}

impl<'source, V: Vfs> BufferedEdgeScan<'source, V> {
    /// Consume an active admitted source into an independently positioned
    /// incidence stream. No routing or payload I/O occurs here. `after` is an
    /// exclusive EId lower bound, not a numeric increment or a durable token;
    /// zero and u128::MAX remain ordinary identities. The stream keeps the
    /// source's selected cut and cumulative work allowance. An already closed,
    /// failed or exhausted source stays terminal and cannot be revived.
    pub fn into_incidence_scan(
        mut self,
        endpoint: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
        after: Option<EId>,
    ) -> BufferedEdgeIncidenceScan<'source, V> {
        let active = self.state.take().is_some();
        BufferedEdgeIncidenceScan {
            as_of: self.as_of,
            work: self.work.used,
            request: Incidence {
                endpoint,
                relation,
                direction,
                after,
            },
            state: None,
            driver: active.then_some(Driver { root: self }),
        }
    }
}

impl<V: Vfs> core::fmt::Debug for BufferedEdgeIncidenceScan<'_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BufferedEdgeIncidenceScan")
            .field("snapshot", &self.as_of)
            .field("finished", &self.is_finished())
            .field("work", &self.work)
            .field("request", &"[REDACTED]")
            .finish()
    }
}

impl<V: Vfs> BufferedEdgeIncidenceScan<'_, V> {
    pub const fn snapshot_seq(&self) -> CommitSeq {
        self.as_of
    }

    pub const fn work_used(&self) -> usize {
        self.work
    }

    pub fn is_finished(&self) -> bool {
        self.driver.is_none()
    }

    pub fn close(&mut self) {
        self.state = None;
        self.driver = None;
    }

    /// Pull one complete historical identity and resolve its visible endpoints.
    /// An invisible identity has `row: None` but is still admitted once through
    /// Identity; it is not EOF. No unrelated identity is reported merely because
    /// it shares a routed block. Work and controls cover initial routing,
    /// permutations, history merging and endpoint reads cumulatively.
    pub async fn next_candidate_with<C: Send>(
        &mut self,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        self.next_candidate_with_access(cx, EdgePropertyAccess::Read, observe)
            .await
            .map(|candidate| {
                candidate.map(|candidate| BufferedEdgeCandidate {
                    eid: candidate.eid,
                    row: candidate.row.map(|row| match row {
                        IncidenceProjection::Endpoints(row) => row,
                        IncidenceProjection::Visible => unreachable!("complete-record projection"),
                    }),
                })
            })
    }

    async fn next_candidate_with_access<C: Send>(
        &mut self,
        cx: &QueryCx,
        properties: EdgePropertyAccess,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<IncidenceProjection>>, BufferedScanError<C>> {
        let Some(mut driver) = self.driver.take() else {
            return Ok(None);
        };
        // Both ownership roots move into the pending future before its first
        // await. A cancelled future cannot leave heads without their source.
        let mut state = self.state.take();
        let request = self.request;
        let used = &mut self.work;
        let result = async {
            let mut observe = |event| {
                observe(event)?;
                if event == BufferedEdgeScanEvent::Work {
                    *used = used.saturating_add(1);
                }
                Ok(())
            };
            if state.is_none() {
                state = Some(
                    driver
                        .prepare_incident(cx, request, true, properties, &mut observe)
                        .await?,
                );
            }
            let state = state.as_mut().expect("complete incidence heads");
            let Some(candidate) = driver
                .root
                .advance(state, cx, properties, &mut observe)
                .await?
            else {
                return Ok(None);
            };
            let candidate = match properties {
                EdgePropertyAccess::Read => {
                    let candidate = driver
                        .root
                        .resolve_endpoints(cx, candidate, request.relation, &mut observe)
                        .await?;
                    BufferedEdgeCandidate {
                        eid: candidate.eid,
                        row: candidate.row.map(IncidenceProjection::Endpoints),
                    }
                }
                EdgePropertyAccess::Skip => {
                    let candidate = driver
                        .root
                        .resolve_endpoint_visibility(cx, candidate, request.relation, &mut observe)
                        .await?;
                    BufferedEdgeCandidate {
                        eid: candidate.eid,
                        row: candidate.row.map(|()| IncidenceProjection::Visible),
                    }
                }
            };
            Ok(Some(candidate))
        }
        .await;
        self.work = driver.root.work.used;
        if matches!(result, Ok(Some(_)))
            && state.as_ref().is_some_and(|state| !state.heads.is_empty())
        {
            self.state = state;
            self.driver = Some(driver);
        }
        result
    }

    /// Demand one visible edge with its same-cut endpoints, skipping invisible
    /// historical candidates without buffering a neighborhood. Callers retaining
    /// multiple outputs also retain and pay their independent reservations.
    pub async fn next(
        &mut self,
        cx: &QueryCx,
    ) -> Result<Option<BufferedEdgeEndpoints>, BufferedReadError> {
        loop {
            match self
                .next_candidate_with(cx, &mut |_| Ok::<_, core::convert::Infallible>(()))
                .await
            {
                Ok(Some(candidate)) => {
                    if let Some(row) = candidate.row {
                        return Ok(Some(row));
                    }
                }
                Ok(None) => return Ok(None),
                Err(BufferedScanError::Read(error)) => return Err(error),
                Err(BufferedScanError::Control(never)) => match never {},
            }
        }
    }

    /// Consume the remaining stream and count visible edge identities without
    /// loading edge-property sidecars or decoding endpoint properties. Both
    /// endpoint histories are resolved through the admitted topology decoder
    /// at this exact cut, retaining dangling-endpoint and retirement refusals.
    /// Direction matching and self-loop de-duplication use the full-record
    /// history merge. No partial count escapes a source, memory, work or
    /// cancellation refusal. Initial root admission continues to authenticate
    /// every property object and validate every vertex property value.
    pub async fn count_visible(mut self, cx: &QueryCx) -> Result<u64, BufferedReadError> {
        let mut count = 0_u64;
        loop {
            match self
                .next_candidate_with_access(cx, EdgePropertyAccess::Skip, &mut |_| {
                    Ok::<_, core::convert::Infallible>(())
                })
                .await
            {
                Ok(Some(candidate)) => {
                    if candidate.row.is_some() {
                        count = count
                            .checked_add(1)
                            .ok_or(BufferedReadError::SizeOverflow)?;
                    }
                }
                Ok(None) => return Ok(count),
                Err(BufferedScanError::Read(error)) => return Err(error),
                Err(BufferedScanError::Control(never)) => match never {},
            }
        }
    }
}

#[cfg(test)]
mod tests;
