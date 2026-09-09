//! Snapshot admission for the one bounded GLA executor.
//! Source lifetimes, history and policy accounting are independent of the
//! compiler-owned output shape. Tuples do not require a second source scan.

pub(crate) mod source;

use crate::{Database, EdgeRecord, EmbeddedReadView, GqlCertificate, GqlError,
    GqlPlanCertificate, ReadError, Snapshot, VertexRow};
use asupersync::fs::Vfs;
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::{GlaOutput, GlaPlan, PreparedGraphPattern, VertexPredicate};
use fgdb_gql::{BoundPlan, GlaExecution, GlaExecutionError, GlaExecutionLimits, RelationBind};
use fgdb_types::{CommitSeq, VId};
use source::SourceEvent;
use std::collections::BTreeMap;

pub(crate) trait GqlSnapshotReader {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError>;
    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError>;
    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError>;
    fn gql_admitted_snapshot(&self, _as_of: CommitSeq) -> Result<Option<&Snapshot>, ReadError> { Ok(None) }
}
impl<V: Vfs + Clone> GqlSnapshotReader for Database<V> {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> { Database::vertex_at(self, vid, as_of) }
    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> { Database::vertices_at(self, as_of) }
    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> { Database::edges_at(self, as_of) }
    fn gql_admitted_snapshot(&self, as_of: CommitSeq) -> Result<Option<&Snapshot>, ReadError> {
        self.ensure_readable()?; self.snapshot.check_frontier(as_of)?; Ok(Some(&self.snapshot))
    }
}
impl GqlSnapshotReader for EmbeddedReadView {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> { EmbeddedReadView::vertex_at(self, vid, as_of) }
    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> { EmbeddedReadView::vertices_at(self, as_of) }
    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> { EmbeddedReadView::edges_at(self, as_of) }
    fn gql_admitted_snapshot(&self, as_of: CommitSeq) -> Result<Option<&Snapshot>, ReadError> {
        self.snapshot.check_frontier(as_of)?; Ok(Some(&self.snapshot))
    }
}
fn bind_plan(statement: &str, bind: &RelationBind) -> Result<BoundPlan, GqlError> {
    bind.bind(statement).map_err(|error| match error {
        fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse), unbound => GqlError::Bind(unbound),
    })
}

impl<V: Vfs + Clone> Database<V> {
    pub fn prepare_gql_plan(&self, statement: &str, bind: &RelationBind) -> Result<BoundPlan, GqlError> { bind_plan(statement, bind) }
    pub fn execute_prepared_gql(&self, plan: &BoundPlan) -> Result<Vec<VId>, GqlError> {
        let as_of = self.frontier().map_err(GqlError::Read)?; self.execute_prepared_gql_at(plan, as_of)
    }
    pub fn execute_prepared_gql_at(&self, plan: &BoundPlan, as_of: CommitSeq) -> Result<Vec<VId>, GqlError> {
        execute_at(plan, self, as_of).map_err(GqlError::Read)
    }
    pub fn execute_prepared_gql_certified(&self, plan: &BoundPlan) -> Result<(Vec<VId>, GqlPlanCertificate), GqlError> {
        let as_of = self.frontier().map_err(GqlError::Read)?; self.execute_prepared_gql_certified_at(plan, as_of)
    }
    pub fn execute_prepared_gql_certified_at(&self, plan: &BoundPlan, as_of: CommitSeq) -> Result<(Vec<VId>, GqlPlanCertificate), GqlError> {
        let rows = self.execute_prepared_gql_at(plan, as_of)?; Ok((rows, crate::gql_cert::certify(plan, as_of)))
    }
    pub fn read_session(&self) -> Result<EmbeddedReadView, ReadError> { self.pinned_read_view() }

    /// Execute a scalar or multi-column connected pattern with one source and
    /// one policy. The prepared type determines the output, without flattening
    /// correlated bindings or issuing a legacy scalar query certificate.
    pub fn execute_graph_pattern_governed<Row: GlaOutput>(
        &self, cx: &fgdb_types::QueryCx, pattern: &PreparedGraphPattern<Row>, policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>> {
        let as_of = self.frontier().map_err(GqlError::Read).map_err(fgdb_gql::GqlQueryError::Source)?;
        self.execute_graph_pattern_governed_at(cx, pattern, as_of, policy)
    }
    pub fn execute_graph_pattern_governed_at<Row: GlaOutput>(
        &self, cx: &fgdb_types::QueryCx, pattern: &PreparedGraphPattern<Row>, as_of: CommitSeq, policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>> {
        self.ensure_readable().and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(GqlError::Read).map_err(fgdb_gql::GqlQueryError::Source)?;
        cx.with_restriction(|| execute_pattern_at(self, pattern, as_of, policy, || cx.checkpoint()))
    }
}

impl EmbeddedReadView {
    pub fn prepare_gql_plan(&self, statement: &str, bind: &RelationBind) -> Result<BoundPlan, GqlError> { bind_plan(statement, bind) }
    pub fn execute_prepared_gql(&self, plan: &BoundPlan) -> Result<Vec<VId>, GqlError> { self.execute_prepared_gql_at(plan, self.frontier()) }
    pub fn execute_prepared_gql_at(&self, plan: &BoundPlan, as_of: CommitSeq) -> Result<Vec<VId>, GqlError> {
        execute_at(plan, self, as_of).map_err(GqlError::Read)
    }
    pub fn execute_gql(&self, statement: &str, bind: &RelationBind) -> Result<Vec<VId>, GqlError> {
        self.execute_gql_at(statement, bind, as_of_for_view(self))
    }
    pub fn execute_gql_at(&self, statement: &str, bind: &RelationBind, as_of: CommitSeq) -> Result<Vec<VId>, GqlError> {
        let plan = bind_plan(statement, bind)?; self.execute_prepared_gql_at(&plan, as_of)
    }
    pub fn execute_gql_certified(&self, statement: &str, bind: &RelationBind) -> Result<(Vec<VId>, GqlCertificate), GqlError> {
        self.execute_gql_certified_at(statement, bind, self.frontier())
    }
    pub fn execute_gql_certified_at(&self, statement: &str, bind: &RelationBind, as_of: CommitSeq) -> Result<(Vec<VId>, GqlCertificate), GqlError> {
        let rows = self.execute_gql_at(statement, bind, as_of)?;
        Ok((rows, GqlCertificate { snapshot_seq: as_of,
            statement_digest: crate::gql_cert::digest_statement(statement), bind_digest: crate::gql_cert::digest_bind(bind) }))
    }
    pub fn execute_prepared_gql_certified(&self, plan: &BoundPlan) -> Result<(Vec<VId>, GqlPlanCertificate), GqlError> {
        self.execute_prepared_gql_certified_at(plan, self.frontier())
    }
    pub fn execute_prepared_gql_certified_at(&self, plan: &BoundPlan, as_of: CommitSeq) -> Result<(Vec<VId>, GqlPlanCertificate), GqlError> {
        let rows = self.execute_prepared_gql_at(plan, as_of)?; Ok((rows, crate::gql_cert::certify(plan, as_of)))
    }
    #[must_use]
    pub fn prepared_gql_plan_certificate(&self, plan: &BoundPlan) -> GqlPlanCertificate { crate::gql_cert::certify(plan, self.frontier()) }

    pub fn execute_graph_pattern_governed<Row: GlaOutput>(
        &self, cx: &fgdb_types::QueryCx, pattern: &PreparedGraphPattern<Row>, policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>> {
        self.execute_graph_pattern_governed_at(cx, pattern, self.frontier(), policy)
    }
    pub fn execute_graph_pattern_governed_at<Row: GlaOutput>(
        &self, cx: &fgdb_types::QueryCx, pattern: &PreparedGraphPattern<Row>, as_of: CommitSeq, policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>> {
        self.snapshot.check_frontier(as_of).map_err(GqlError::Read).map_err(fgdb_gql::GqlQueryError::Source)?;
        cx.with_restriction(|| execute_pattern_at(self, pattern, as_of, policy, || cx.checkpoint()))
    }
}

fn execute_pattern_at<R: GqlSnapshotReader + ?Sized, C, Row: GlaOutput>(
    reader: &R, pattern: &PreparedGraphPattern<Row>, as_of: CommitSeq,
    policy: fgdb_gql::GqlQueryPolicy, mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<GqlError, C>> {
    checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
    AdmittedGqlSnapshot::admit_logical(pattern.plan().clone(), reader, as_of)
        .map_err(GqlError::Read).map_err(fgdb_gql::GqlQueryError::Source)?
        .execute_governed(policy, checkpoint).map_err(|error| error.map_source(GqlError::Read))
}
fn as_of_for_view(view: &EmbeddedReadView) -> CommitSeq { view.frontier() }

/// One exact reader/sequence/plan and its admitted source. Output shape changes
/// neither the input rows nor the borrowed generation's authority boundary.
pub(crate) struct AdmittedGqlSnapshot<'a, R: ?Sized, Row = VId> {
    reader: &'a R,
    as_of: CommitSeq,
    logical: GlaPlan<Row>,
    snapshot: Option<&'a Snapshot>,
    borrowed: Option<source::BorrowedTables<'a>>,
    vertices: BTreeMap<VId, VertexRow>,
    edges: Vec<EdgeRecord>,
    snapshot_records: u64,
}
impl<'a, R: GqlSnapshotReader + ?Sized> AdmittedGqlSnapshot<'a, R> {
    pub(crate) fn admit(plan: &BoundPlan, reader: &'a R, as_of: CommitSeq) -> Result<Self, ReadError> {
        Self::admit_logical(GlaPlan::lower(plan), reader, as_of)
    }
}
impl<'a, R: GqlSnapshotReader + ?Sized, Row: GlaOutput> AdmittedGqlSnapshot<'a, R, Row> {
    fn admit_logical(logical: GlaPlan<Row>, reader: &'a R, as_of: CommitSeq) -> Result<Self, ReadError> {
        let mut vertices = BTreeMap::new(); let mut edges = Vec::new();
        let snapshot = reader.gql_admitted_snapshot(as_of)?;
        let count = if snapshot.is_some() { 0 } else if logical.scans_edges() {
            edges = reader.gql_edges_at(as_of)?; edges.len() as u64
        } else {
            let rows = reader.gql_vertices_at(as_of)?; let count = rows.len() as u64;
            vertices.extend(rows.into_iter().map(|row| (row.vid, row))); count
        };
        Ok(Self { reader, as_of, logical, snapshot, borrowed: None, vertices, edges, snapshot_records: count })
    }
    fn materialize<E>(&mut self, control: &mut impl FnMut(SourceEvent) -> Result<(), E>) -> Result<(), E> {
        if self.borrowed.is_none() && let Some(snapshot) = self.snapshot {
            let tables = source::admit(snapshot, &self.logical, self.as_of, control)?;
            self.snapshot_records = tables.snapshot_records; self.borrowed = Some(tables);
        }
        Ok(())
    }
    fn vertex_ids(&self) -> impl Iterator<Item = VId> + '_ {
        self.borrowed.iter().flat_map(|tables| tables.vertices.iter().map(|row| row.vid)).chain(self.vertices.keys().copied())
    }
    fn edge_triples(&self) -> impl Iterator<Item = (VId, RelationId, VId)> + '_ {
        self.borrowed.iter().flat_map(|tables| tables.edges.iter().copied()).chain(self.edges.iter().map(edge_triple))
    }
    fn matches(&self, vid: VId, predicates: &[VertexPredicate]) -> Result<bool, ReadError> {
        if let Some(tables) = &self.borrowed { Ok(tables.matches(vid, predicates)) }
        else if self.logical.scans_edges() {
            let row = self.reader.gql_vertex_at(vid, self.as_of)?;
            Ok(row.is_some_and(|row| predicates.iter().all(|p| p.matches(&row.labels, &row.props))))
        } else {
            Ok(self.vertices.get(&vid).is_some_and(|row| predicates.iter().all(|p| p.matches(&row.labels, &row.props))))
        }
    }
    pub(crate) fn execute_budgeted(mut self, budget: fgdb_gql::GqlExecutionBudget)
        -> Result<fgdb_gql::BudgetedGqlExecution<Vec<Row>>, fgdb_gql::BudgetedGqlError<ReadError>> {
        self.materialize(&mut |_| Ok::<_, ReadError>(())).map_err(fgdb_gql::BudgetedGqlError::Execution)?;
        self.logical.execute_budgeted(self.snapshot_records, self.vertex_ids(), self.edge_triples(),
            |vid, predicates| self.matches(vid, predicates), budget)
    }
    pub(crate) fn execute(mut self) -> Result<Vec<Row>, ReadError> {
        self.materialize(&mut |_| Ok::<_, ReadError>(()))?;
        self.logical.execute(self.vertex_ids(), self.edge_triples(), |vid, predicates| self.matches(vid, predicates))
    }
    pub(crate) fn execute_limited(mut self, limits: GlaExecutionLimits) -> Result<GlaExecution<Row>, GlaExecutionError<ReadError>> {
        self.materialize(&mut |_| Ok::<_, ReadError>(())).map_err(GlaExecutionError::Source)?;
        self.logical.execute_with_limits(self.vertex_ids(), self.edge_triples(), |vid, predicates| self.matches(vid, predicates), limits)
    }
    pub(crate) fn execute_governed<C>(mut self, policy: fgdb_gql::GqlQueryPolicy, mut checkpoint: impl FnMut() -> Result<(), C>)
        -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<ReadError, C>> {
        let mut usage = AdmissionUsage::default();
        self.materialize(&mut |event| {
            checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?; usage.observe::<ReadError, C>(policy, event)
        })?;
        let result = self.logical.execute_governed(self.snapshot_records, self.vertex_ids(), self.edge_triples(),
            |vid, predicates| self.matches(vid, predicates), usage.remaining(policy), checkpoint);
        usage.finish(policy, result)
    }
}
fn edge_triple(record: &EdgeRecord) -> (VId, RelationId, VId) { (record.entry.src, record.entry.relation, record.entry.dst) }
pub(crate) fn execute<V: Vfs + Clone>(plan: &BoundPlan, db: &Database<V>) -> Result<Vec<VId>, ReadError> { execute_at(plan, db, db.frontier()?) }
pub(crate) fn execute_at<R: GqlSnapshotReader + ?Sized>(plan: &BoundPlan, reader: &R, as_of: CommitSeq) -> Result<Vec<VId>, ReadError> {
    AdmittedGqlSnapshot::admit(plan, reader, as_of)?.execute()
}

/// Source and evaluator consume one allowance, never independent full budgets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AdmissionUsage { work_units: u64, scratch_entries: u64, records: u64 }
impl AdmissionUsage {
    pub(crate) fn observe<E, C>(&mut self, policy: fgdb_gql::GqlQueryPolicy, event: SourceEvent) -> Result<(), fgdb_gql::GqlQueryError<E, C>> {
        use fgdb_gql::{GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlQueryError};
        let mut next = *self;
        if event == SourceEvent::SnapshotRecord {
            next.records = next.records.checked_add(1).expect("source record count fits u64");
            policy.rows.check(GqlBudgetDimension::SnapshotRecords, next.records).map_err(GqlQueryError::Rows)?;
        }
        for (value, limit, dimension, active) in [
            (&mut next.work_units, policy.evaluator.max_work_units, GlaLimitDimension::WorkUnits, true),
            (&mut next.scratch_entries, policy.evaluator.max_scratch_entries, GlaLimitDimension::ScratchEntries, event == SourceEvent::ScratchEntry),
        ] {
            if !active { continue; }
            let observed = u128::from(*value) + 1;
            if observed > u128::from(limit) { return Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension, limit, observed })); }
            *value = observed as u64;
        }
        *self = next; Ok(())
    }
    pub(crate) fn remaining(self, mut policy: fgdb_gql::GqlQueryPolicy) -> fgdb_gql::GqlQueryPolicy {
        policy.evaluator.max_work_units -= self.work_units; policy.evaluator.max_scratch_entries -= self.scratch_entries; policy
    }
    pub(crate) fn finish<E, C, Row>(self, policy: fgdb_gql::GqlQueryPolicy,
        result: Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<E, C>>)
        -> Result<fgdb_gql::GqlQueryExecution<Row>, fgdb_gql::GqlQueryError<E, C>> {
        use fgdb_gql::{GlaLimitDimension, GqlQueryError};
        match result {
            Ok(mut execution) => {
                execution.evaluator.work_units += self.work_units; execution.evaluator.scratch_entries += self.scratch_entries; Ok(execution)
            }
            Err(GqlQueryError::Evaluator(mut exceeded)) => {
                match exceeded.dimension {
                    GlaLimitDimension::WorkUnits => { exceeded.limit = policy.evaluator.max_work_units; exceeded.observed += u128::from(self.work_units); }
                    GlaLimitDimension::ScratchEntries => { exceeded.limit = policy.evaluator.max_scratch_entries; exceeded.observed += u128::from(self.scratch_entries); }
                }
                Err(GqlQueryError::Evaluator(exceeded))
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod admission_meter_tests {
    use super::*;
    use fgdb_gql::{GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlExecutionStats,
        GqlQueryError, GqlQueryExecution, GqlQueryPolicy};

    #[test]
    fn phases_share_one_allowance_and_refusals_report_original_limits() {
        let policy = GqlQueryPolicy::new(1, 1, 3, 2); let mut usage = AdmissionUsage::default();
        usage.observe::<(), ()>(policy, SourceEvent::ScratchEntry).unwrap();
        usage.observe::<(), ()>(policy, SourceEvent::SnapshotRecord).unwrap();
        let remaining = usage.remaining(policy);
        assert_eq!(remaining.evaluator.max_work_units, 1); assert_eq!(remaining.evaluator.max_scratch_entries, 1);
        let execution = GqlQueryExecution { value: vec![], rows: GqlExecutionStats { snapshot_records: 1, result_rows: 0 },
            evaluator: GlaExecutionStats { work_units: 1, scratch_entries: 1 } };
        let success = usage.finish::<(), (), VId>(policy, Ok(execution)).unwrap();
        assert_eq!(success.evaluator.work_units, 3); assert_eq!(success.evaluator.scratch_entries, 2);
        let refused = usage.finish::<(), (), VId>(policy, Err(GqlQueryError::Evaluator(GlaLimitExceeded {
            dimension: GlaLimitDimension::WorkUnits, limit: 1, observed: 2 })));
        assert!(matches!(refused, Err(GqlQueryError::Evaluator(GlaLimitExceeded {
            dimension: GlaLimitDimension::WorkUnits, limit: 3, observed: 4 }))));
        let before = usage;
        assert!(matches!(usage.observe::<(), ()>(policy, SourceEvent::SnapshotRecord), Err(GqlQueryError::Rows(error)) if error.observed == 2));
        assert_eq!(usage, before);
    }
    #[test]
    fn source_work_never_wraps_and_refused_events_do_not_change_counters() {
        let policy = GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX);
        let mut usage = AdmissionUsage { work_units: u64::MAX, ..AdmissionUsage::default() }; let before = usage;
        assert!(matches!(usage.observe::<(), ()>(policy, SourceEvent::Work),
            Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(u64::MAX) + 1));
        assert_eq!(usage, before);
    }
}
