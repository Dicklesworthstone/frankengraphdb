//! Selected-property reads for carried identities at the cold session's cut.
//! Topology is reauthorized on every reached operand. A prior graph root or
//! captured row never supplies authority for a later property lookup.

use super::*;
use fgdb_delta_types::LabelId;
use fgdb_gql::{GlaExecutionEvent, GqlBudgetDimension, GqlExecutionBudget};
use fgdb_strata::store::{BufferedScanError, BufferedVertexProperties};
use fgdb_strata::tiered::edge_scan::{
    BufferedEdgeProperties, BufferedEdgeScan, BufferedEdgeScanState,
};

// The lexical borrower drops before this owner parks the backend. No mutex
// guard spans a source read, user callback or backend destructor.
pub(super) struct Checkout<V: Vfs> {
    backend: Option<AuthorizedBufferedReadSource<V>>,
    returned: Returned<V>,
}

impl<V: Vfs> Checkout<V> {
    pub(super) fn new(returned: Returned<V>) -> Result<Self> {
        let backend = take_returned(&returned).ok_or(NativeSpoolError::IncompleteCursor)?;
        Ok(Self {
            backend: Some(backend),
            returned,
        })
    }

    pub(super) fn view(&mut self) -> &mut BufferedReadView<V> {
        &mut self.backend.as_mut().expect("checkout owns backend").view
    }
}

impl<V: Vfs> Drop for Checkout<V> {
    fn drop(&mut self) {
        if let Some(backend) = self.backend.take() {
            let previous = {
                self.returned
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .replace(backend)
            };
            debug_assert!(previous.is_none());
            drop(previous);
        }
    }
}

// Only one selected key enters either owner. The evaluator admits its copy
// before Borrow is used, while these authenticated source reservations remain
// live. No value is cloned into an uncharged intermediate allocation.
pub(super) enum Property {
    Vertex(BufferedVertexProperties),
    Edge(BufferedEdgeProperties),
}

impl std::borrow::Borrow<CanonicalPropertyValue> for Property {
    fn borrow(&self) -> &CanonicalPropertyValue {
        match self {
            Self::Vertex(row) => &row.properties()[0].1,
            Self::Edge(row) => &row.properties()[0].1,
        }
    }
}

pub(super) struct Elements<'v, 'q, V: Vfs, C> {
    scan: BufferedEdgeScan<'v, V>,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    budget: GqlExecutionBudget,
    records: u64,
    _metadata: MemoryCharge,
}

fn read_error(error: BufferedReadError) -> NativeSpoolError {
    NativeSpoolError::BufferedRead(Box::new(error))
}

fn scan_error(error: BufferedScanError<NativeSpoolError>) -> NativeSpoolError {
    match error {
        BufferedScanError::Read(error) => read_error(error),
        BufferedScanError::Control(error) => error,
    }
}

fn admit_record(records: &mut u64, budget: GqlExecutionBudget) -> Result<()> {
    let next = records.checked_add(1).ok_or(SpillError::SizeOverflow)?;
    budget
        .check(GqlBudgetDimension::SnapshotRecords, next)
        .map_err(|error| authorized(QueryError::Set(GqlQueryError::Rows(error))))?;
    *records = next;
    Ok(())
}

fn admit_vertex<C: FnMut() -> u64>(
    execution: &Shared<'_, C>,
    labels: &[LabelId],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<()>,
) -> Result<()> {
    control(GlaExecutionEvent::Work)?;
    for &label in labels {
        let visible = {
            let mut live = execution.borrow_mut();
            live.poll_fresh().map_err(authorized)?;
            live.permit.predicates().allows_label(label)
        };
        if visible {
            control(GlaExecutionEvent::Work)?;
        }
    }
    // Do not use vertex_node: an earlier root cannot exempt a carried or
    // repeated identity from this new lookup's signed node admission.
    execution.borrow_mut().node().map_err(authorized)
}

impl<'v, 'q, V: Vfs + Clone, C: FnMut() -> u64 + Send> Elements<'v, 'q, V, C> {
    pub(super) fn new(
        view: &'v mut BufferedReadView<V>,
        state: Option<BufferedEdgeScanState>,
        cx: &'q QueryCx,
        at: CommitSeq,
        execution: Shared<'q, C>,
        budget: GqlExecutionBudget,
        records: u64,
    ) -> Result<Self> {
        let metadata = view
            .memory_pool()
            .reserve(cx, 1024 + size_of::<Self>())
            .map_err(BufferedReadError::Memory)
            .map_err(read_error)?;
        let scan = match state {
            Some(state) => view.partition_mut().resume_edge_scan(cx, state),
            None => view.partition_mut().edge_scan(cx, at),
        }
        .map_err(read_error)?;
        if scan.snapshot_seq() != at {
            return Err(NativeSpoolError::IncompleteCursor);
        }
        Ok(Self {
            scan,
            cx,
            execution,
            budget,
            records,
            _metadata: metadata,
        })
    }

    pub(super) fn into_state(self) -> (BufferedEdgeScanState, u64) {
        (self.scan.into_state(), self.records)
    }

    async fn read(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
        control: &mut (impl FnMut(GlaExecutionEvent) -> Result<()> + Send),
    ) -> Result<Option<Property>> {
        {
            let mut live = self.execution.borrow_mut();
            live.poll_fresh().map_err(authorized)?;
            // A denied key needs no topology or payload fault, and cannot
            // spend a visible node, record or candidate allowance.
            if !live.permit.predicates().allows_property(key) {
                return Ok(None);
            }
        }
        let execution = &self.execution;
        let records = &mut self.records;
        let budget = self.budget;
        let mut observe = |_| execution.borrow_mut().poll_fresh().map_err(authorized);
        let property = match element {
            ElementId::Vertex(vid) => {
                let mut admit = |_, labels: &[LabelId]| {
                    {
                        let mut live = execution.borrow_mut();
                        live.poll_fresh().map_err(authorized)?;
                        if !live.permit.predicates().allows_vertex(labels) {
                            return Ok(false);
                        }
                    }
                    admit_vertex(execution, labels, control)?;
                    admit_record(records, budget)?;
                    control(GlaExecutionEvent::Work)?;
                    Ok(true)
                };
                let candidate = self
                    .scan
                    .vertex_admitted_properties_candidate_with(
                        self.cx,
                        vid,
                        PropertySelection::one(key),
                        &mut admit,
                        &mut observe,
                    )
                    .await
                    .map_err(scan_error)?
                    .ok_or(NativeSpoolError::IncompleteCursor)?;
                candidate
                    .row
                    .and_then(|row| (!row.properties().is_empty()).then_some(Property::Vertex(row)))
            }
            ElementId::Edge(eid) => {
                let mut admit =
                    |row: &fgdb_strata::tiered::edge_scan::BufferedEdgeTopologyEndpoints| {
                        let edge = row.edge().entry();
                        {
                            let mut live = execution.borrow_mut();
                            live.poll_fresh().map_err(authorized)?;
                            let scope = live.permit.predicates();
                            if !scope.allows_relation(edge.relation)
                                || !scope.allows_vertex(row.source_vertex().labels())
                                || !scope.allows_vertex(row.target_vertex().labels())
                            {
                                return Ok(false);
                            }
                        }
                        // Both endpoint predicates precede every logical charge.
                        control(GlaExecutionEvent::Work)?;
                        admit_vertex(execution, row.source_vertex().labels(), control)?;
                        if edge.src != edge.dst {
                            admit_vertex(execution, row.target_vertex().labels(), control)?;
                        }
                        admit_record(records, budget)?;
                        control(GlaExecutionEvent::Work)?;
                        Ok(true)
                    };
                let candidate = self
                    .scan
                    .edge_admitted_properties_candidate_with(
                        self.cx,
                        eid,
                        PropertySelection::one(key),
                        &mut admit,
                        &mut observe,
                    )
                    .await
                    .map_err(scan_error)?
                    .ok_or(NativeSpoolError::IncompleteCursor)?;
                candidate.row.and_then(|row| {
                    let edge = row.into_edge();
                    (!edge.properties().is_empty()).then_some(Property::Edge(edge))
                })
            }
        };
        self.execution
            .borrow_mut()
            .poll_fresh()
            .map_err(authorized)?;
        if property.is_some() {
            control(GlaExecutionEvent::Work)?;
        }
        Ok(property)
    }
}

impl<V: Vfs + Clone, C: FnMut() -> u64 + Send> AsyncSpillElementSource for Elements<'_, '_, V, C> {
    type Error = NativeSpoolError;
    type Property = Property;

    async fn element_property(
        &mut self,
        _element: ElementId,
        _key: PropertyKeyId,
    ) -> Option<Result<Option<Property>>> {
        // The evaluator control owns visible work and scratch admission.
        // Omitting it cannot open a separate unmetered property route.
        None
    }

    async fn element_property_with_control(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
        control: &mut (impl FnMut(GlaExecutionEvent) -> Result<()> + Send),
    ) -> Option<Result<Option<Property>>> {
        let cx = self.cx;
        Some(
            cx.with_restriction_async(self.read(element, key, control))
                .await,
        )
    }
}
