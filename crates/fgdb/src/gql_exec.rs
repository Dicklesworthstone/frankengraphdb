//! Snapshot admission for the one bounded GLA executor.
//! Source lifetimes, history and policy accounting are independent of the
//! compiler-owned output shape. Tuples do not require a second source scan.

pub(crate) mod property_maps;
pub(crate) mod source;

use crate::{
    Database, EdgeRecord, EmbeddedReadView, GqlCertificate, GqlError, GqlPlanCertificate,
    ReadError, Snapshot, VertexRow,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaIdentityOutput, GlaOutput, GlaPlan, GraphValue, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{BoundPlan, GlaExecution, GlaExecutionError, GlaExecutionLimits, RelationBind};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, CommitSeq, EId, VId};
use source::SourceEvent;
use std::collections::BTreeMap;

pub(crate) trait GqlSnapshotReader {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError>;
    fn gql_edge_at(&self, eid: EId, as_of: CommitSeq) -> Result<Option<EdgeRecord>, ReadError>;
    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError>;
    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError>;
    fn gql_admitted_snapshot(&self, _as_of: CommitSeq) -> Result<Option<&Snapshot>, ReadError> {
        Ok(None)
    }
}
impl<V: Vfs + Clone> GqlSnapshotReader for Database<V> {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        Database::vertex_at(self, vid, as_of)
    }
    fn gql_edge_at(&self, eid: EId, as_of: CommitSeq) -> Result<Option<EdgeRecord>, ReadError> {
        Database::edge_at(self, eid, as_of)
    }
    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        Database::vertices_at(self, as_of)
    }
    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        Database::edges_at(self, as_of)
    }
    fn gql_admitted_snapshot(&self, as_of: CommitSeq) -> Result<Option<&Snapshot>, ReadError> {
        self.ensure_readable()?;
        self.snapshot.check_frontier(as_of)?;
        Ok(Some(&self.snapshot))
    }
}
impl GqlSnapshotReader for EmbeddedReadView {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        EmbeddedReadView::vertex_at(self, vid, as_of)
    }
    fn gql_edge_at(&self, eid: EId, as_of: CommitSeq) -> Result<Option<EdgeRecord>, ReadError> {
        EmbeddedReadView::edge_at(self, eid, as_of)
    }
    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        EmbeddedReadView::vertices_at(self, as_of)
    }
    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        EmbeddedReadView::edges_at(self, as_of)
    }
    fn gql_admitted_snapshot(&self, as_of: CommitSeq) -> Result<Option<&Snapshot>, ReadError> {
        self.snapshot.check_frontier(as_of)?;
        Ok(Some(&self.snapshot))
    }
}
/// One stored property of a vertex or edge at `as_of`, for `x.key` over an
/// element value an expression produced (fgdb-xnd7x). The element came from a
/// pattern at this same cut. Should it be absent there anyway, it reads NULL.
pub(crate) fn element_property_at(
    reader: &(impl GqlSnapshotReader + ?Sized),
    element: fgdb_delta_types::ElementId,
    key: PropertyKeyId,
    as_of: CommitSeq,
) -> Result<Option<CanonicalPropertyValue>, GqlError> {
    if matches!(element, fgdb_delta_types::ElementId::Vertex(_))
        && let Some(snapshot) = reader
            .gql_admitted_snapshot(as_of)
            .map_err(GqlError::Read)?
    {
        return read_element_property(
            element,
            key,
            |vid| snapshot.vertex_row_at(vid, as_of),
            |eid| reader.gql_edge_at(eid, as_of),
        );
    }
    read_element_property(
        element,
        key,
        |vid| reader.gql_vertex_at(vid, as_of),
        |eid| reader.gql_edge_at(eid, as_of),
    )
}

/// The one property row lookup behind every host's element reader: the
/// vertex row or edge record at the host's cut, then its key, if present.
pub(crate) fn read_element_property<Row: std::borrow::Borrow<VertexRow>>(
    element: fgdb_delta_types::ElementId,
    key: PropertyKeyId,
    vertex: impl FnOnce(VId) -> Result<Option<Row>, ReadError>,
    edge: impl FnOnce(EId) -> Result<Option<EdgeRecord>, ReadError>,
) -> Result<Option<CanonicalPropertyValue>, GqlError> {
    let lookup = |props: &[(PropertyKeyId, CanonicalPropertyValue)]| {
        props
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|at| props[at].1.clone())
    };
    Ok(match element {
        fgdb_delta_types::ElementId::Vertex(vid) => vertex(vid)
            .map_err(GqlError::Read)?
            .and_then(|row| lookup(&row.borrow().props)),
        fgdb_delta_types::ElementId::Edge(eid) => edge(eid)
            .map_err(GqlError::Read)?
            .and_then(|record| lookup(&record.props)),
    })
}

/// Metadata for a carried element is resolved at the same snapshot as its
/// source, never against a newer database head. Every output allocation is
/// admitted through the expression evaluator before copying stored payloads.
pub(crate) fn element_metadata_at(
    reader: &(impl GqlSnapshotReader + ?Sized),
    element: fgdb_delta_types::ElementId,
    kind: fgdb_gql::GraphElementMetadata,
    catalog: &fgdb_gql::ReverseSymbolCatalog,
    as_of: CommitSeq,
    control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>,
) -> Result<Option<GraphValue>, fgdb_gql::GraphElementMetadataError<GqlError>> {
    use fgdb_delta_types::ElementId;
    use fgdb_gql::{GlaExecutionEvent, GraphElementMetadataError};
    use property_maps::MetadataRecord;

    let failure = |error| GraphElementMetadataError::Source(GqlError::Read(error));
    let mut control = |event| {
        control(match event {
            SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
            SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
        })
        .map_err(|()| GraphElementMetadataError::Interrupted)
    };
    control(SourceEvent::Work)?;
    if let Some(snapshot) = reader.gql_admitted_snapshot(as_of).map_err(failure)? {
        let record = match element {
            ElementId::Vertex(id) => {
                let Some(row) = source::find_vertex(snapshot, id, as_of, &mut control)? else {
                    return Ok(None);
                };
                MetadataRecord::Vertex(row)
            }
            ElementId::Edge(id) => {
                let Some((block, row)) = snapshot.adjacency_index().statement_at_controlled(
                    &snapshot.blocks,
                    id,
                    as_of,
                    &mut |_| control(SourceEvent::Work),
                )?
                else {
                    return Ok(None);
                };
                MetadataRecord::Edge(
                    snapshot.blocks[block][row].relation,
                    source::edge_properties_at(&snapshot.block_props, block, row),
                )
            }
        };
        return property_maps::metadata(
            record,
            kind,
            catalog,
            |_| true,
            |_| true,
            &mut control,
            &failure,
        )
        .map(Some);
    }
    // Nonresident internal readers retain their authoritative point-lookup
    // contract. The metadata construction itself is shared with the borrowed
    // snapshot path, including field bounds and pre-copy accounting.
    match element {
        ElementId::Vertex(id) => reader
            .gql_vertex_at(id, as_of)
            .map_err(failure)?
            .map(|row| {
                property_maps::metadata(
                    MetadataRecord::Vertex(&row),
                    kind,
                    catalog,
                    |_| true,
                    |_| true,
                    &mut control,
                    &failure,
                )
            })
            .transpose(),
        ElementId::Edge(id) => reader
            .gql_edge_at(id, as_of)
            .map_err(failure)?
            .map(|row| {
                property_maps::metadata(
                    MetadataRecord::Edge(row.entry.relation, &row.props),
                    kind,
                    catalog,
                    |_| true,
                    |_| true,
                    &mut control,
                    &failure,
                )
            })
            .transpose(),
    }
}

fn bind_plan(statement: &str, bind: &RelationBind) -> Result<BoundPlan, GqlError> {
    bind.bind(statement).map_err(|error| match error {
        fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
        unbound => GqlError::Bind(unbound),
    })
}

impl<V: Vfs + Clone> Database<V> {
    pub fn prepare_gql_plan(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<BoundPlan, GqlError> {
        bind_plan(statement, bind)
    }
    pub fn execute_prepared_gql(&self, plan: &BoundPlan) -> Result<Vec<VId>, GqlError> {
        let as_of = self.frontier().map_err(GqlError::Read)?;
        self.execute_prepared_gql_at(plan, as_of)
    }
    pub fn execute_prepared_gql_at(
        &self,
        plan: &BoundPlan,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, GqlError> {
        execute_at(plan, self, as_of).map_err(GqlError::Read)
    }
    pub fn execute_prepared_gql_certified(
        &self,
        plan: &BoundPlan,
    ) -> Result<(Vec<VId>, GqlPlanCertificate), GqlError> {
        let as_of = self.frontier().map_err(GqlError::Read)?;
        self.execute_prepared_gql_certified_at(plan, as_of)
    }
    pub fn execute_prepared_gql_certified_at(
        &self,
        plan: &BoundPlan,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlPlanCertificate), GqlError> {
        let rows = self.execute_prepared_gql_at(plan, as_of)?;
        Ok((rows, crate::gql_cert::certify(plan, as_of)))
    }
    pub fn read_session(&self) -> Result<EmbeddedReadView, ReadError> {
        self.pinned_read_view()
    }

    /// Execute a scalar or multi-column connected pattern with one source and
    /// one policy. The prepared type determines the output, without flattening
    /// correlated bindings or issuing a legacy scalar query certificate.
    pub fn execute_graph_pattern_governed<Row: GlaOutput>(
        &self,
        cx: &fgdb_types::QueryCx,
        pattern: &PreparedGraphPattern<Row>,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<Row>,
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    > {
        let as_of = self
            .frontier()
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        self.execute_graph_pattern_governed_at(cx, pattern, as_of, policy)
    }
    pub fn execute_graph_pattern_governed_at<Row: GlaOutput>(
        &self,
        cx: &fgdb_types::QueryCx,
        pattern: &PreparedGraphPattern<Row>,
        as_of: CommitSeq,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<Row>,
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    > {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        cx.with_restriction(|| execute_pattern_at(self, pattern, as_of, policy, || cx.checkpoint()))
    }
}

impl EmbeddedReadView {
    pub fn prepare_gql_plan(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<BoundPlan, GqlError> {
        bind_plan(statement, bind)
    }
    pub fn execute_prepared_gql(&self, plan: &BoundPlan) -> Result<Vec<VId>, GqlError> {
        self.execute_prepared_gql_at(plan, self.frontier())
    }
    pub fn execute_prepared_gql_at(
        &self,
        plan: &BoundPlan,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, GqlError> {
        execute_at(plan, self, as_of).map_err(GqlError::Read)
    }
    pub fn execute_gql(&self, statement: &str, bind: &RelationBind) -> Result<Vec<VId>, GqlError> {
        self.execute_gql_at(statement, bind, as_of_for_view(self))
    }
    pub fn execute_gql_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, GqlError> {
        let plan = bind_plan(statement, bind)?;
        self.execute_prepared_gql_at(&plan, as_of)
    }
    pub fn execute_gql_certified(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<(Vec<VId>, GqlCertificate), GqlError> {
        self.execute_gql_certified_at(statement, bind, self.frontier())
    }
    pub fn execute_gql_certified_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlCertificate), GqlError> {
        let rows = self.execute_gql_at(statement, bind, as_of)?;
        Ok((
            rows,
            GqlCertificate {
                snapshot_seq: as_of,
                statement_digest: crate::gql_cert::digest_statement(statement),
                bind_digest: crate::gql_cert::digest_bind(bind),
            },
        ))
    }
    pub fn execute_prepared_gql_certified(
        &self,
        plan: &BoundPlan,
    ) -> Result<(Vec<VId>, GqlPlanCertificate), GqlError> {
        self.execute_prepared_gql_certified_at(plan, self.frontier())
    }
    pub fn execute_prepared_gql_certified_at(
        &self,
        plan: &BoundPlan,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, GqlPlanCertificate), GqlError> {
        let rows = self.execute_prepared_gql_at(plan, as_of)?;
        Ok((rows, crate::gql_cert::certify(plan, as_of)))
    }
    #[must_use]
    pub fn prepared_gql_plan_certificate(&self, plan: &BoundPlan) -> GqlPlanCertificate {
        crate::gql_cert::certify(plan, self.frontier())
    }

    pub fn execute_graph_pattern_governed<Row: GlaOutput>(
        &self,
        cx: &fgdb_types::QueryCx,
        pattern: &PreparedGraphPattern<Row>,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<Row>,
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    > {
        self.execute_graph_pattern_governed_at(cx, pattern, self.frontier(), policy)
    }
    pub fn execute_graph_pattern_governed_at<Row: GlaOutput>(
        &self,
        cx: &fgdb_types::QueryCx,
        pattern: &PreparedGraphPattern<Row>,
        as_of: CommitSeq,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<Row>,
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    > {
        self.snapshot
            .check_frontier(as_of)
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        cx.with_restriction(|| execute_pattern_at(self, pattern, as_of, policy, || cx.checkpoint()))
    }
}

fn execute_pattern_at<R: GqlSnapshotReader + ?Sized, C, Row: GlaOutput>(
    reader: &R,
    pattern: &PreparedGraphPattern<Row>,
    as_of: CommitSeq,
    policy: fgdb_gql::GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<GqlError, C>> {
    execute_seeded_pattern_at(reader, pattern, None, as_of, policy, checkpoint)
}

/// [`execute_pattern_at`] for a pattern whose one vertex root a leading bag
/// binds by identity (fgdb-2acff): with `seeds`, a single-root plan admits
/// only those roots by point lookup. Other shapes, and a reader without a
/// borrowed snapshot, admit as before; their rows are a superset the set
/// executor's identity join narrows, exactly as for the unseeded leaf.
pub(crate) fn execute_seeded_pattern_at<R: GqlSnapshotReader + ?Sized, C, Row: GlaOutput>(
    reader: &R,
    pattern: &PreparedGraphPattern<Row>,
    seeds: Option<&[VId]>,
    as_of: CommitSeq,
    policy: fgdb_gql::GqlQueryPolicy,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<GqlError, C>> {
    checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
    let mut admitted = AdmittedGqlSnapshot::admit_logical(pattern.plan().clone(), reader, as_of)
        .map_err(GqlError::Read)
        .map_err(fgdb_gql::GqlQueryError::Source)?;
    admitted.seeds = seeds.map(Box::from);
    admitted
        .execute_governed(policy, checkpoint)
        .map_err(|error| error.map_source(GqlError::Read))
}
fn as_of_for_view(view: &EmbeddedReadView) -> CommitSeq {
    view.frontier()
}

/// One exact reader/sequence/plan and its admitted source. Output shape changes
/// neither the input rows nor the borrowed generation's authority boundary.
pub(crate) struct AdmittedGqlSnapshot<'a, R: ?Sized, Row = VId> {
    reader: &'a R,
    as_of: CommitSeq,
    logical: GlaPlan<Row>,
    snapshot: Option<&'a Snapshot>,
    borrowed: Option<source::BorrowedTables<'a>>,
    /// Root identities a leading bag binds (fgdb-2acff); see admit_seeded.
    seeds: Option<Box<[VId]>>,
    vertices: BTreeMap<VId, VertexRow>,
    edges: Vec<EdgeRecord>,
    snapshot_records: u64,
    cached_labels: BTreeMap<VId, Vec<GraphValue>>,
    cached_types: BTreeMap<EId, CanonicalScalar>,
    property_maps: property_maps::PropertyMaps,
}
impl<'a, R: GqlSnapshotReader + ?Sized> AdmittedGqlSnapshot<'a, R> {
    pub(crate) fn admit(
        plan: &BoundPlan,
        reader: &'a R,
        as_of: CommitSeq,
    ) -> Result<Self, ReadError> {
        Self::admit_logical(GlaPlan::lower(plan), reader, as_of)
    }
}
impl<'a, R: GqlSnapshotReader + ?Sized, Row: GlaOutput> AdmittedGqlSnapshot<'a, R, Row> {
    fn admit_logical(
        logical: GlaPlan<Row>,
        reader: &'a R,
        as_of: CommitSeq,
    ) -> Result<Self, ReadError> {
        let mut vertices = BTreeMap::new();
        let mut edges = Vec::new();
        let snapshot = reader.gql_admitted_snapshot(as_of)?;
        let count = if snapshot.is_some() {
            0
        } else if logical.scans_edges() {
            edges = reader.gql_edges_at(as_of)?;
            // The independently fallible owned-reader seam also supplies real
            // projection values. Never silently return null because it has no
            // private Snapshot. The production path borrows selected endpoints.
            if logical.projects_properties() || logical.projects_labels() {
                vertices.extend(
                    reader
                        .gql_vertices_at(as_of)?
                        .into_iter()
                        .map(|row| (row.vid, row)),
                );
            }
            edges.len() as u64
        } else {
            let rows = reader.gql_vertices_at(as_of)?;
            let count = rows.len() as u64;
            vertices.extend(rows.into_iter().map(|row| (row.vid, row)));
            if logical.reads_edges() {
                edges = reader.gql_edges_at(as_of)?;
            }
            count + edges.len() as u64
        };
        Ok(Self {
            reader,
            as_of,
            logical,
            snapshot,
            borrowed: None,
            seeds: None,
            vertices,
            edges,
            snapshot_records: count,
            cached_labels: BTreeMap::new(),
            cached_types: BTreeMap::new(),
            property_maps: property_maps::PropertyMaps::default(),
        })
    }
    fn materialize<E>(
        &mut self,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.borrowed.is_none()
            && let Some(snapshot) = self.snapshot
        {
            let seeded = match self.seeds.as_deref() {
                Some(seeds) => {
                    source::admit_seeded(snapshot, &self.logical, self.as_of, seeds, control)?
                }
                None => None,
            };
            let tables = match seeded {
                Some(tables) => tables,
                None => source::admit(snapshot, &self.logical, self.as_of, control)?,
            };
            self.snapshot_records = tables.snapshot_records;
            self.borrowed = Some(tables);
        }
        Ok(())
    }
    fn vertex_ids(&self) -> impl Iterator<Item = VId> + '_ {
        self.borrowed
            .iter()
            .flat_map(|tables| tables.vertices.iter().map(|row| row.vid))
            .chain(self.vertices.keys().copied())
    }
    fn edge_triples(&self) -> impl Iterator<Item = (VId, RelationId, VId)> + '_ {
        self.identified_edges()
            .map(|(_, src, relation, dst)| (src, relation, dst))
    }
    fn identified_edges(&self) -> impl Iterator<Item = (EId, VId, RelationId, VId)> + '_ {
        self.borrowed
            .iter()
            .flat_map(|tables| tables.edges.iter().map(|(edge, _)| *edge))
            .chain(self.edges.iter().map(|record| {
                (
                    record.entry.eid,
                    record.entry.src,
                    record.entry.relation,
                    record.entry.dst,
                )
            }))
    }
    fn matches(&self, vid: VId, predicates: &[VertexPredicate]) -> Result<bool, ReadError> {
        if let Some(tables) = &self.borrowed {
            Ok(tables.matches(vid, predicates))
        } else if self.logical.scans_edges() && !self.logical.projects_properties() {
            let row = self.reader.gql_vertex_at(vid, self.as_of)?;
            Ok(row.is_some_and(|row| {
                predicates
                    .iter()
                    .all(|p| p.matches(&row.labels, &row.props))
            }))
        } else {
            Ok(self.vertices.get(&vid).is_some_and(|row| {
                predicates
                    .iter()
                    .all(|p| p.matches(&row.labels, &row.props))
            }))
        }
    }
    fn property(&self, vid: VId, key: PropertyKeyId) -> Option<&CanonicalPropertyValue> {
        if let Some(tables) = &self.borrowed {
            return tables.property(vid, key);
        }
        let row = self.vertices.get(&vid)?;
        row.props
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|at| &row.props[at].1)
    }
    fn edge_property(&self, eid: EId, key: PropertyKeyId) -> Option<&CanonicalPropertyValue> {
        if let Some(tables) = &self.borrowed {
            return tables.edge_property(eid, key);
        }
        let row = self.edges.iter().find(|record| record.entry.eid == eid)?;
        row.props
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|at| &row.props[at].1)
    }
    pub(crate) fn execute_budgeted(
        mut self,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<Row>>, fgdb_gql::BudgetedGqlError<ReadError>>
    where
        Row: GlaIdentityOutput,
    {
        // Refuse the first excess record before the borrowed source retains
        // it, rather than materializing the full table and then counting it.
        // The evaluator still checks the final count for owned-reader inputs.
        self.materialize(&mut snapshot_record_budget::<ReadError>(budget))?;
        self.logical.execute_budgeted(
            self.snapshot_records,
            self.vertex_ids(),
            self.edge_triples(),
            |vid, predicates| self.matches(vid, predicates),
            budget,
        )
    }
    pub(crate) fn execute(mut self) -> Result<Vec<Row>, ReadError>
    where
        Row: GlaIdentityOutput,
    {
        self.materialize(&mut |_| Ok::<_, ReadError>(()))?;
        self.logical
            .execute(self.vertex_ids(), self.edge_triples(), |vid, predicates| {
                self.matches(vid, predicates)
            })
    }
    pub(crate) fn execute_limited(
        mut self,
        limits: GlaExecutionLimits,
    ) -> Result<GlaExecution<Row>, GlaExecutionError<ReadError>>
    where
        Row: GlaIdentityOutput,
    {
        self.materialize(&mut |_| Ok::<_, ReadError>(()))
            .map_err(GlaExecutionError::Source)?;
        self.logical.execute_with_limits(
            self.vertex_ids(),
            self.edge_triples(),
            |vid, predicates| self.matches(vid, predicates),
            limits,
        )
    }
    fn validate_and_cache_catalog_symbols(&mut self) -> Result<(), ReadError> {
        if self.logical.projects_labels() {
            let reverse = self.logical.reverse_catalog.as_deref();
            let check_vertex = |labels: &[LabelId]| -> Result<Vec<GraphValue>, ReadError> {
                let mut sorted_labels = labels.to_vec();
                sorted_labels.sort();
                sorted_labels.dedup();
                let mut names = Vec::with_capacity(sorted_labels.len());
                for id in sorted_labels {
                    let name = reverse
                        .and_then(|catalog| catalog.labels.get(&id))
                        .ok_or(ReadError::UnmappedLabel(id))?;
                    let text = CanonicalScalar::ucs_basic_text(name)
                        .map_err(|_| ReadError::UnmappedLabel(id))?;
                    names.push(GraphValue::Scalar(text));
                }
                Ok(names)
            };

            if let Some(tables) = &self.borrowed {
                for row in &tables.vertices {
                    let names = check_vertex(&row.labels)?;
                    self.cached_labels.insert(row.vid, names);
                }
            } else {
                for row in self.vertices.values() {
                    let names = check_vertex(&row.labels)?;
                    self.cached_labels.insert(row.vid, names);
                }
            }
        }

        if self.logical.projects_types() {
            let reverse = self.logical.reverse_catalog.as_deref();
            let check_edge = |relation: RelationId| -> Result<CanonicalScalar, ReadError> {
                let name = reverse
                    .and_then(|catalog| catalog.relations.get(&relation))
                    .ok_or(ReadError::UnmappedRelation(relation))?;
                CanonicalScalar::ucs_basic_text(name)
                    .map_err(|_| ReadError::UnmappedRelation(relation))
            };

            if let Some(tables) = &self.borrowed {
                for &((eid, _, relation, _), _) in &tables.edges {
                    let text = check_edge(relation)?;
                    self.cached_types.insert(eid, text);
                }
            } else {
                for record in &self.edges {
                    let text = check_edge(record.entry.relation)?;
                    self.cached_types.insert(record.entry.eid, text);
                }
            }
        }

        Ok(())
    }

    fn cache_property_maps<E>(
        &mut self,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
        failure: impl Fn(ReadError) -> E,
    ) -> Result<(), E> {
        let catalog = self.logical.reverse_catalog.as_deref();
        if self.logical.projects_vertex_property_maps() {
            let rows = self
                .borrowed
                .iter()
                .flat_map(|tables| tables.vertices.iter().copied())
                .chain(self.vertices.values());
            for row in rows {
                let map = property_maps::collect(
                    row.props.iter().map(|(key, value)| (*key, value)),
                    catalog,
                    |_| true,
                    control,
                    &failure,
                )?;
                control(SourceEvent::ScratchEntry)?;
                self.property_maps.vertices.insert(row.vid, map);
            }
        }
        if self.logical.projects_edge_property_maps() {
            let rows = self
                .borrowed
                .iter()
                .flat_map(|tables| {
                    tables
                        .edges
                        .iter()
                        .map(|(edge, properties)| (edge.0, *properties))
                })
                .chain(
                    self.edges
                        .iter()
                        .map(|row| (row.entry.eid, row.props.as_slice())),
                );
            for (eid, properties) in rows {
                let map = property_maps::collect(
                    properties.iter().map(|(key, value)| (*key, value)),
                    catalog,
                    |_| true,
                    control,
                    &failure,
                )?;
                control(SourceEvent::ScratchEntry)?;
                self.property_maps.edges.insert(eid, map);
            }
        }
        Ok(())
    }

    pub(crate) fn execute_governed<C>(
        mut self,
        policy: fgdb_gql::GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<ReadError, C>> {
        let mut usage = AdmissionUsage::default();
        self.materialize(&mut |event| {
            checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
            usage.observe::<ReadError, C>(policy, event)
        })?;
        self.validate_and_cache_catalog_symbols()
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        self.cache_property_maps(
            &mut |event| {
                checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
                usage.observe::<ReadError, C>(policy, event)
            },
            fgdb_gql::GqlQueryError::Source,
        )?;
        let result = self.logical.execute_governed_with_element_maps(
            self.snapshot_records,
            self.vertex_ids(),
            self.identified_edges(),
            |vid, predicates| self.matches(vid, predicates),
            |vid, key| Ok(self.property(vid, key)),
            |eid, key| Ok(self.edge_property(eid, key)),
            |vid| Ok(self.cached_labels.get(&vid).map(|v| v.as_slice())),
            |eid| Ok(self.cached_types.get(&eid)),
            |vid| Ok(self.property_maps.vertices.get(&vid)),
            |eid| Ok(self.property_maps.edges.get(&eid)),
            usage.remaining(policy),
            checkpoint,
        );
        usage.finish(policy, result)
    }
}
pub(crate) fn execute<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
) -> Result<Vec<VId>, ReadError> {
    execute_at(plan, db, db.frontier()?)
}
pub(crate) fn execute_at<R: GqlSnapshotReader + ?Sized>(
    plan: &BoundPlan,
    reader: &R,
    as_of: CommitSeq,
) -> Result<Vec<VId>, ReadError> {
    AdmittedGqlSnapshot::admit(plan, reader, as_of)?.execute()
}

/// Row-only admission uses the same record events as governed execution.
/// History traversal, scratch entries and evaluator work remain outside this
/// API's contract; this is not an allocator-byte or whole-process memory cap.
pub(crate) fn snapshot_record_budget<E>(
    budget: fgdb_gql::GqlExecutionBudget,
) -> impl FnMut(SourceEvent) -> Result<(), fgdb_gql::BudgetedGqlError<E>> {
    let mut records = 0_u64;
    move |event| {
        if event == SourceEvent::SnapshotRecord {
            let next = records
                .checked_add(1)
                .expect("source record count fits u64");
            budget
                .check(fgdb_gql::GqlBudgetDimension::SnapshotRecords, next)
                .map_err(fgdb_gql::BudgetedGqlError::Budget)?;
            records = next;
        }
        Ok(())
    }
}

/// Source and evaluator consume one allowance, never independent full budgets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AdmissionUsage {
    work_units: u64,
    scratch_entries: u64,
    records: u64,
}
impl AdmissionUsage {
    /// Charge a registered kernel's admitted work estimate to the SAME
    /// allowance as its source, projection and later result conversion.
    /// The kernel must first cap its own execution at `remaining(policy)`.
    pub(crate) fn charge_work<E, C>(
        &mut self,
        policy: fgdb_gql::GqlQueryPolicy,
        units: u64,
    ) -> Result<(), fgdb_gql::GqlQueryError<E, C>> {
        let observed = u128::from(self.work_units) + u128::from(units);
        if observed > u128::from(policy.evaluator.max_work_units) {
            return Err(fgdb_gql::GqlQueryError::Evaluator(
                fgdb_gql::GlaLimitExceeded {
                    dimension: fgdb_gql::GlaLimitDimension::WorkUnits,
                    limit: policy.evaluator.max_work_units,
                    observed,
                },
            ));
        }
        self.work_units = observed as u64;
        Ok(())
    }

    /// `n` Work events in one step: the same admitted total, and the same
    /// refusal, as `n` calls of [`Self::observe`]. Every unit up to the
    /// limit is admitted and the first past it refuses, observing limit + 1.
    pub(crate) fn observe_work<E, C>(
        &mut self,
        policy: fgdb_gql::GqlQueryPolicy,
        n: u64,
    ) -> Result<(), fgdb_gql::GqlQueryError<E, C>> {
        let limit = policy.evaluator.max_work_units;
        let observed = u128::from(self.work_units) + u128::from(n);
        if n > 0 && observed > u128::from(limit) {
            self.work_units = self.work_units.max(limit);
            return Err(fgdb_gql::GqlQueryError::Evaluator(
                fgdb_gql::GlaLimitExceeded {
                    dimension: fgdb_gql::GlaLimitDimension::WorkUnits,
                    limit,
                    observed: u128::from(self.work_units) + 1,
                },
            ));
        }
        self.work_units = observed as u64;
        Ok(())
    }

    pub(crate) fn observe<E, C>(
        &mut self,
        policy: fgdb_gql::GqlQueryPolicy,
        event: SourceEvent,
    ) -> Result<(), fgdb_gql::GqlQueryError<E, C>> {
        use fgdb_gql::{GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlQueryError};
        let mut next = *self;
        if event == SourceEvent::SnapshotRecord {
            next.records = next
                .records
                .checked_add(1)
                .expect("source record count fits u64");
            policy
                .rows
                .check(GqlBudgetDimension::SnapshotRecords, next.records)
                .map_err(GqlQueryError::Rows)?;
        }
        for (value, limit, dimension, active) in [
            (
                &mut next.work_units,
                policy.evaluator.max_work_units,
                GlaLimitDimension::WorkUnits,
                true,
            ),
            (
                &mut next.scratch_entries,
                policy.evaluator.max_scratch_entries,
                GlaLimitDimension::ScratchEntries,
                event == SourceEvent::ScratchEntry,
            ),
        ] {
            if !active {
                continue;
            }
            let observed = u128::from(*value) + 1;
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }));
            }
            *value = observed as u64;
        }
        *self = next;
        Ok(())
    }
    /// Whether `records` more snapshot records, `work` more work units and
    /// `scratch` more scratch entries all fit `policy`: charged in any order,
    /// no event among them could refuse.
    pub(crate) fn absorbs(
        &self,
        policy: fgdb_gql::GqlQueryPolicy,
        records: u64,
        work: u64,
        scratch: u64,
    ) -> bool {
        let fits = |used: u64, more: u64, limit: u64| {
            used.checked_add(more).is_some_and(|total| total <= limit)
        };
        self.records.checked_add(records).is_some_and(|total| {
            policy
                .rows
                .check(fgdb_gql::GqlBudgetDimension::SnapshotRecords, total)
                .is_ok()
        }) && fits(self.work_units, work, policy.evaluator.max_work_units)
            && fits(
                self.scratch_entries,
                scratch,
                policy.evaluator.max_scratch_entries,
            )
    }

    /// Charge counts [`Self::absorbs`] admitted.
    pub(crate) fn absorb(&mut self, records: u64, work: u64, scratch: u64) {
        self.records += records;
        self.work_units += work;
        self.scratch_entries += scratch;
    }

    /// Snapshot records admitted so far, for a source that reports its own
    /// execution stats (an authorized procedure call).
    pub(crate) fn snapshot_records(&self) -> u64 {
        self.records
    }
    pub(crate) fn remaining(
        self,
        mut policy: fgdb_gql::GqlQueryPolicy,
    ) -> fgdb_gql::GqlQueryPolicy {
        policy.evaluator.max_work_units -= self.work_units;
        policy.evaluator.max_scratch_entries -= self.scratch_entries;
        policy
    }
    pub(crate) fn finish<E, C, Row>(
        self,
        policy: fgdb_gql::GqlQueryPolicy,
        result: Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<E, C>>,
    ) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<E, C>> {
        use fgdb_gql::{GlaLimitDimension, GqlQueryError};
        match result {
            Ok(mut execution) => {
                execution.evaluator.work_units += self.work_units;
                execution.evaluator.scratch_entries += self.scratch_entries;
                Ok(execution)
            }
            Err(GqlQueryError::Evaluator(mut exceeded)) => {
                match exceeded.dimension {
                    GlaLimitDimension::WorkUnits => {
                        exceeded.limit = policy.evaluator.max_work_units;
                        exceeded.observed += u128::from(self.work_units);
                    }
                    GlaLimitDimension::ScratchEntries => {
                        exceeded.limit = policy.evaluator.max_scratch_entries;
                        exceeded.observed += u128::from(self.scratch_entries);
                    }
                }
                Err(GqlQueryError::Evaluator(exceeded))
            }
            Err(error) => Err(error),
        }
    }
}

/// Evaluate the physical factors of one already admitted original source.
/// Source records are reported once. Admission, both evaluators and retained
/// rows share one work/scratch allowance, even when either factor is empty.
/// The caller retains the original admission and witnesses until both succeed.
pub(crate) fn execute_pattern_factors<E, C>(
    request: &fgdb_gql::PreparedGraphPatternFactors<'_>,
    snapshot_records: u64,
    policy: fgdb_gql::GqlQueryPolicy,
    mut usage: AdmissionUsage,
    mut evaluate: impl FnMut(
        &PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<E, C>,
    >,
) -> Result<fgdb_gql::GraphPatternFactorExecution, fgdb_gql::GqlQueryError<E, C>> {
    let mut value = [Vec::new(), Vec::new()];
    let mut result_rows = 0_u64;
    for (factor, rows) in request.factors().iter().zip(&mut value) {
        let result = evaluate(factor, usage.remaining(policy));
        let execution = usage.finish(policy, result)?;
        debug_assert_eq!(execution.rows.snapshot_records, snapshot_records);
        debug_assert_eq!(execution.rows.result_rows, execution.value.len() as u64);
        // Both vectors coexist in this address space; their row count fits u64.
        result_rows = result_rows
            .checked_add(execution.value.len() as u64)
            .expect("retained factor rows fit u64");
        usage.work_units = execution.evaluator.work_units;
        usage.scratch_entries = execution.evaluator.scratch_entries;
        *rows = execution.value;
    }
    policy
        .rows
        .check(fgdb_gql::GqlBudgetDimension::ResultRows, result_rows)
        .map_err(fgdb_gql::GqlQueryError::Rows)?;
    Ok(fgdb_gql::GraphPatternFactorExecution {
        value,
        rows: fgdb_gql::GqlExecutionStats {
            snapshot_records,
            result_rows,
        },
        evaluator: fgdb_gql::GlaExecutionStats {
            work_units: usage.work_units,
            scratch_entries: usage.scratch_entries,
        },
    })
}

#[cfg(test)]
mod admission_meter_tests {
    use super::*;
    use fgdb_gql::{
        GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlExecutionStats, GqlQueryError,
        GqlQueryExecution, GqlQueryPolicy,
    };

    #[test]
    fn element_properties_borrow_retained_history_and_preserve_pinned_cuts() {
        use asupersync::lab::run_async_under_lab;
        use fgdb_delta_types::ElementId;
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

        // Observe the owned-row hook, not allocator bytes or elapsed time.
        // Both routes delegate to the same real committed read view.
        struct ObservedReader<'a> {
            view: &'a EmbeddedReadView,
            borrow_snapshot: bool,
            owned_reads: std::cell::Cell<usize>,
        }
        impl GqlSnapshotReader for ObservedReader<'_> {
            fn gql_vertex_at(
                &self,
                vid: VId,
                at: CommitSeq,
            ) -> Result<Option<VertexRow>, ReadError> {
                self.owned_reads.set(self.owned_reads.get() + 1);
                self.view.vertex_at(vid, at)
            }
            fn gql_edge_at(
                &self,
                eid: EId,
                at: CommitSeq,
            ) -> Result<Option<EdgeRecord>, ReadError> {
                self.view.edge_at(eid, at)
            }
            fn gql_vertices_at(&self, at: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
                self.view.vertices_at(at)
            }
            fn gql_edges_at(&self, at: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
                self.view.edges_at(at)
            }
            fn gql_admitted_snapshot(&self, at: CommitSeq) -> Result<Option<&Snapshot>, ReadError> {
                if self.borrow_snapshot {
                    self.view.gql_admitted_snapshot(at)
                } else {
                    Ok(None)
                }
            }
        }

        let ((), report) = run_async_under_lab(0x518a_d487, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = crate::DatabaseKeys::new(
                [0x51; 32],
                DatabaseSecurityNamespaceId([0x8a; 32]),
                [0xd4; 32],
            );
            let mut database = Database::open_memory(&commit, keys).await.unwrap();
            let key = PropertyKeyId(1);
            let mut create = crate::WriteBatch::new(RelationId(1));
            create.create_vertex(
                VId(1),
                vec![LabelId(1)],
                vec![
                    (key, CanonicalScalar::Int(11)),
                    // Encoded under the 16 KiB per-row ceiling (eb4bf6ec):
                    // 16,384 payload bytes encode to an 18,541-byte row.
                    (
                        PropertyKeyId(2),
                        CanonicalScalar::bytes(vec![0x5a; 8_192]).unwrap(),
                    ),
                ],
            );
            assert_eq!(database.write(&commit, create).await.unwrap(), CommitSeq(1));
            let pinned = database.read_session().unwrap();
            let mut update = crate::WriteBatch::new(RelationId(1));
            update.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(22)));
            assert_eq!(database.write(&commit, update).await.unwrap(), CommitSeq(2));
            let mut delete = crate::WriteBatch::new(RelationId(1));
            delete.delete_vertex(VId(1));
            assert_eq!(database.write(&commit, delete).await.unwrap(), CommitSeq(3));

            let element = ElementId::Vertex(VId(1));
            for (cut, expected) in [(0, None), (1, Some(11)), (2, Some(22)), (3, None)] {
                let at = CommitSeq(cut);
                assert_eq!(
                    element_property_at(&database, element, key, at).unwrap(),
                    expected
                        .map(|value| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value))),
                );
                if let Some(row) = database.snapshot.vertex_row_at(VId(1), at).unwrap() {
                    assert!(
                        database
                            .snapshot
                            .patches
                            .iter()
                            .any(|patch| patch.iter().any(|stored| std::ptr::eq(stored, row))),
                        "the borrowed row must be retained patch storage, not a copied row"
                    );
                    assert_eq!(row.props.len(), 2);
                }
            }
            let current = database.read_session().unwrap();
            for borrow_snapshot in [false, true] {
                let reader = ObservedReader {
                    view: &current,
                    borrow_snapshot,
                    owned_reads: std::cell::Cell::new(0),
                };
                for (cut, expected) in [(0, None), (1, Some(11)), (2, Some(22)), (3, None)] {
                    assert_eq!(
                        element_property_at(&reader, element, key, CommitSeq(cut)).unwrap(),
                        expected.map(|value| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(
                            value
                        ))),
                    );
                }
                assert_eq!(
                    reader.owned_reads.get(),
                    if borrow_snapshot { 0 } else { 4 }
                );
            }
            assert_eq!(
                element_property_at(&pinned, element, key, CommitSeq(1)).unwrap(),
                Some(CanonicalPropertyValue::Scalar(CanonicalScalar::Int(11))),
            );
            assert_eq!(
                element_property_at(&database, element, PropertyKeyId(3), CommitSeq(1)).unwrap(),
                None,
            );
            assert!(matches!(
                element_property_at(&database, element, key, CommitSeq(4)),
                Err(GqlError::Read(ReadError::BeyondFrontier {
                    asked: CommitSeq(4),
                    frontier: CommitSeq(3)
                }))
            ));
            assert!(matches!(
                element_property_at(&pinned, element, key, CommitSeq(2)),
                Err(GqlError::Read(ReadError::BeyondFrontier {
                    asked: CommitSeq(2),
                    frontier: CommitSeq(1)
                }))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    /// **N WORK UNITS IN ONE STEP ARE N WORK EVENTS.** From every starting
    /// count around the limit, `observe_work(n)` leaves the usage and returns
    /// the result that `n` single Work events would: everything up to the
    /// limit is admitted, and the refusal observes limit + 1.
    #[test]
    fn work_in_one_step_admits_and_refuses_as_single_events() {
        type Outcome = (AdmissionUsage, Result<(), GqlQueryError<(), ()>>);
        for limit in [0, 1, 5, 64, 130] {
            let policy = GqlQueryPolicy::new(1_000, 1_000, limit, 1_000);
            for start in 0..=limit {
                for n in [0, 1, 2, 3, 63, 64, 65, 129, 200] {
                    let mut base = AdmissionUsage::default();
                    base.charge_work::<(), ()>(policy, start).unwrap();
                    let single: Outcome = {
                        let mut usage = base;
                        let result = (0..n)
                            .try_for_each(|_| usage.observe::<(), ()>(policy, SourceEvent::Work));
                        (usage, result)
                    };
                    let batched: Outcome = {
                        let mut usage = base;
                        let result = usage.observe_work::<(), ()>(policy, n);
                        (usage, result)
                    };
                    assert_eq!(batched, single, "limit {limit} start {start} n {n}");
                }
            }
        }
    }

    #[test]
    fn row_only_admission_ignores_work_and_does_not_charge_a_refused_record() {
        use fgdb_gql::{BudgetedGqlError, GqlBudgetDimension, GqlExecutionBudget};
        let mut control = snapshot_record_budget::<()>(GqlExecutionBudget::new(1, 0));
        for event in [
            SourceEvent::Work,
            SourceEvent::ScratchEntry,
            SourceEvent::SnapshotRecord,
        ] {
            control(event).unwrap();
        }
        for _ in 0..2 {
            assert!(matches!(control(SourceEvent::SnapshotRecord),
                Err(BudgetedGqlError::Budget(error))
                    if error.dimension == GqlBudgetDimension::SnapshotRecords
                        && error.limit == 1 && error.observed == 2));
        }
    }

    #[test]
    fn budgeted_snapshot_admission_stops_at_the_first_excess_record() {
        use asupersync::lab::run_async_under_lab;
        use fgdb_gql::{
            BudgetedGqlError, GqlBudgetDimension, GqlExecutionBudget, PreparedGqlQuery,
        };
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
        let ((), report) = run_async_under_lab(0xb0d6_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = crate::DatabaseKeys::new(
                [0xa1; 32],
                DatabaseSecurityNamespaceId([0xa2; 32]),
                [0xa3; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let bind = RelationBind::new()
                .with_label("L", LabelId(1))
                .with_relation("R", RelationId(1));
            let nodes = PreparedGqlQuery::prepare("MATCH (a:L) RETURN a", &bind).unwrap();
            let empty = db
                .execute_prepared_query_budgeted(&nodes, GqlExecutionBudget::new(0, 0))
                .unwrap();
            assert!(empty.value.is_empty());
            assert_eq!(empty.stats.snapshot_records, 0);

            let mut seed = crate::WriteBatch::new(RelationId(1));
            for id in 1..=6 {
                seed.create_vertex(VId(id), vec![LabelId(1)], vec![]);
                if id > 1 {
                    seed.add_edge(EId(id - 1), VId(id - 1), VId(id), vec![]);
                }
            }
            let basis = db.write(&commit, seed).await.unwrap();
            let pinned = db.read_session().unwrap();
            let mut deletion = crate::WriteBatch::new(RelationId(1));
            deletion.delete_vertex(VId(6));
            let live = db.write(&commit, deletion).await.unwrap();

            // The historical and pinned cuts retain six vertices/five edges;
            // the live cut has five vertices/four edges after the cascade.
            let cuts: [(&dyn GqlSnapshotReader, CommitSeq, u64, u64); 3] = [
                (&db, basis, 6, 5),
                (&pinned, basis, 6, 5),
                (&db, live, 5, 4),
            ];
            for (reader, as_of, vertex_count, edge_count) in cuts {
                for (statement, source_count) in [
                    ("MATCH (a:L) RETURN a", vertex_count),
                    ("MATCH (a)-[:R]->(b) RETURN b", edge_count),
                    // A LIMIT never reduces source admission. The legacy
                    // binder refuses LIMIT 0 by design (parser.rs pins it), so
                    // the smallest admitted page carries the same property.
                    ("MATCH (a:L) RETURN a LIMIT 1", vertex_count),
                ] {
                    let query = PreparedGqlQuery::prepare(statement, &bind).unwrap();
                    for limit in [0, 1] {
                        let refused = AdmittedGqlSnapshot::admit(query.plan(), reader, as_of)
                            .unwrap()
                            .execute_budgeted(GqlExecutionBudget::new(limit, 100));
                        // An after-materialization check reports source_count,
                        // not limit + 1. This exercises the real source owner.
                        assert!(
                            matches!(refused, Err(BudgetedGqlError::Budget(error))
                            if error.dimension == GqlBudgetDimension::SnapshotRecords
                                && error.limit == limit && error.observed == limit + 1),
                            "{statement} at {as_of:?}, limit {limit}"
                        );
                    }
                    let expected = execute_at(query.plan(), reader, as_of).unwrap();
                    let exact = AdmittedGqlSnapshot::admit(query.plan(), reader, as_of)
                        .unwrap()
                        .execute_budgeted(GqlExecutionBudget::new(
                            source_count,
                            expected.len() as u64,
                        ))
                        .unwrap();
                    assert_eq!(exact.value, expected);
                    assert_eq!(exact.stats.snapshot_records, source_count);
                    assert_eq!(exact.stats.result_rows, expected.len() as u64);
                }
            }

            // Exercise all public durable adapters, not only the private seam.
            let small = GqlExecutionBudget::new(1, 100);
            for result in [
                db.execute_prepared_query_budgeted(&nodes, small),
                db.execute_prepared_query_budgeted_at(&nodes, basis, small),
                pinned.execute_prepared_query_budgeted(&nodes, small),
            ] {
                assert!(matches!(result, Err(BudgetedGqlError::Budget(error))
                    if error.dimension == GqlBudgetDimension::SnapshotRecords
                        && error.limit == 1 && error.observed == 2));
            }
            let result_limited =
                db.execute_prepared_query_budgeted(&nodes, GqlExecutionBudget::new(5, 1));
            assert!(
                matches!(result_limited, Err(BudgetedGqlError::Budget(error))
                if error.dimension == GqlBudgetDimension::ResultRows
                    && error.limit == 1 && error.observed == 2)
            );
            for result in [
                db.execute_prepared_query_budgeted_at(
                    &nodes,
                    CommitSeq(live.0 + 1),
                    GqlExecutionBudget::new(0, 0),
                ),
                pinned.execute_prepared_query_budgeted_at(
                    &nodes,
                    live,
                    GqlExecutionBudget::new(0, 0),
                ),
            ] {
                assert!(matches!(
                    result,
                    Err(BudgetedGqlError::Execution(GqlError::Read(
                        ReadError::BeyondFrontier { .. }
                    )))
                ));
            }
            assert_eq!(db.frontier().unwrap(), live);
            assert_eq!(pinned.frontier(), basis);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn phases_share_one_allowance_and_refusals_report_original_limits() {
        let policy = GqlQueryPolicy::new(1, 1, 3, 2);
        let mut usage = AdmissionUsage::default();
        usage
            .observe::<(), ()>(policy, SourceEvent::ScratchEntry)
            .unwrap();
        usage
            .observe::<(), ()>(policy, SourceEvent::SnapshotRecord)
            .unwrap();
        let remaining = usage.remaining(policy);
        assert_eq!(remaining.evaluator.max_work_units, 1);
        assert_eq!(remaining.evaluator.max_scratch_entries, 1);
        let execution = GqlQueryExecution {
            value: vec![],
            rows: GqlExecutionStats {
                snapshot_records: 1,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats {
                work_units: 1,
                scratch_entries: 1,
            },
        };
        let success = usage.finish::<(), (), VId>(policy, Ok(execution)).unwrap();
        assert_eq!(success.evaluator.work_units, 3);
        assert_eq!(success.evaluator.scratch_entries, 2);
        let refused = usage.finish::<(), (), VId>(
            policy,
            Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension: GlaLimitDimension::WorkUnits,
                limit: 1,
                observed: 2,
            })),
        );
        assert!(matches!(
            refused,
            Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension: GlaLimitDimension::WorkUnits,
                limit: 3,
                observed: 4
            }))
        ));
        let before = usage;
        assert!(
            matches!(usage.observe::<(), ()>(policy, SourceEvent::SnapshotRecord), Err(GqlQueryError::Rows(error)) if error.observed == 2)
        );
        assert_eq!(usage, before);
    }
    #[test]
    fn source_work_never_wraps_and_refused_events_do_not_change_counters() {
        let policy = GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX);
        let mut usage = AdmissionUsage {
            work_units: u64::MAX,
            ..AdmissionUsage::default()
        };
        let before = usage;
        assert!(matches!(usage.observe::<(), ()>(policy, SourceEvent::Work),
            Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(u64::MAX) + 1));
        assert_eq!(usage, before);
    }
}
