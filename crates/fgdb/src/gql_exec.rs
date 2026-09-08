//! Snapshot admission for the one bounded GLA executor.
//!
//! Text, bound, owned-prepared, historical, pinned and limited reads converge
//! here. Traversal, predicates, projection, distinct ordering and pagination
//! belong to fgdb-gql's lowered operators, never a second inline MATCH engine.

use crate::{
    Database, EdgeRecord, EmbeddedReadView, GqlCertificate, GqlError, GqlPlanCertificate,
    ReadError, VertexRow,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::{GlaPlan, VertexPredicate};
use fgdb_gql::{BoundPlan, GlaExecution, GlaExecutionError, GlaExecutionLimits, RelationBind};
use fgdb_types::{CommitSeq, VId};
use std::collections::BTreeMap;

/// Immutable read contract. The implementing surface owns retention and handle
/// fences; this trait does not manufacture authorization or a storage backend.
pub(crate) trait GqlSnapshotReader {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError>;
    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError>;
    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError>;
}

impl<V: Vfs + Clone> GqlSnapshotReader for Database<V> {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        Database::vertex_at(self, vid, as_of)
    }

    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        Database::vertices_at(self, as_of)
    }

    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        Database::edges_at(self, as_of)
    }
}

impl GqlSnapshotReader for EmbeddedReadView {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        EmbeddedReadView::vertex_at(self, vid, as_of)
    }

    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        EmbeddedReadView::vertices_at(self, as_of)
    }

    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        EmbeddedReadView::edges_at(self, as_of)
    }
}

fn bind_plan(statement: &str, bind: &RelationBind) -> Result<BoundPlan, GqlError> {
    bind.bind(statement).map_err(|error| match error {
        fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
        unbound => GqlError::Bind(unbound),
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Prepare the bounded language without retaining mutable parser state.
    pub fn prepare_gql_plan(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<BoundPlan, GqlError> {
        bind_plan(statement, bind)
    }

    /// Execute an already prepared plan at one live frontier read.
    pub fn execute_prepared_gql(&self, plan: &BoundPlan) -> Result<Vec<VId>, GqlError> {
        let as_of = self.frontier().map_err(GqlError::Read)?;
        self.execute_prepared_gql_at(plan, as_of)
    }

    /// Execute at an exact retained sequence, preserving ordinary read refusals.
    pub fn execute_prepared_gql_at(
        &self,
        plan: &BoundPlan,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, GqlError> {
        execute_at(plan, self, as_of).map_err(GqlError::Read)
    }

    /// Execute and certify at the same live frontier. No certificate on failure.
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

    /// Acquire one read-only immutable generation, not a new authority context.
    pub fn read_session(&self) -> Result<EmbeddedReadView, ReadError> {
        self.pinned_read_view()
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

    /// The pinned generation refuses sequences beyond its own frontier.
    pub fn execute_prepared_gql_at(
        &self,
        plan: &BoundPlan,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, GqlError> {
        execute_at(plan, self, as_of).map_err(GqlError::Read)
    }

    pub fn execute_gql(&self, statement: &str, bind: &RelationBind) -> Result<Vec<VId>, GqlError> {
        self.execute_gql_at(statement, bind, self.frontier())
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
}

/// Own the admitted table and bind it to the exact plan, reader and sequence.
/// A budget check can inspect the count and then execute these SAME rows;
/// it cannot substitute another plan or accidentally re-read a different cut.
/// At most one of vertices and edges contains data.
pub(crate) struct AdmittedGqlSnapshot<'a, R: ?Sized> {
    reader: &'a R,
    as_of: CommitSeq,
    logical: GlaPlan,
    vertices: BTreeMap<VId, VertexRow>,
    edges: Vec<EdgeRecord>,
    snapshot_records: u64,
}

impl<'a, R: GqlSnapshotReader + ?Sized> AdmittedGqlSnapshot<'a, R> {
    pub(crate) fn admit(
        plan: &BoundPlan,
        reader: &'a R,
        as_of: CommitSeq,
    ) -> Result<Self, ReadError> {
        let logical = GlaPlan::lower(plan);
        let mut vertices = BTreeMap::new();
        let mut edges = Vec::new();
        // Even a logically empty forged plan must cross the ordinary source
        // fence. Future/fenced snapshots cannot become successful empty reads.
        let count = if logical.scans_edges() {
            edges = reader.gql_edges_at(as_of)?;
            edges.len()
        } else {
            let rows = reader.gql_vertices_at(as_of)?;
            let count = rows.len();
            vertices.extend(rows.into_iter().map(|row| (row.vid, row)));
            count
        };
        Ok(Self {
            reader,
            as_of,
            logical,
            vertices,
            edges,
            snapshot_records: u64::try_from(count).unwrap_or(u64::MAX),
        })
    }

    pub(crate) fn snapshot_records(&self) -> u64 {
        self.snapshot_records
    }

    fn matches(&self, vid: VId, predicates: &[VertexPredicate]) -> Result<bool, ReadError> {
        if self.logical.scans_edges() {
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

    pub(crate) fn execute(self) -> Result<Vec<VId>, ReadError> {
        self.logical.execute(
            self.vertices.keys().copied(),
            self.edges.iter().map(edge_triple),
            |vid, predicates| self.matches(vid, predicates),
        )
    }

    pub(crate) fn execute_limited(
        self,
        limits: GlaExecutionLimits,
    ) -> Result<GlaExecution, GlaExecutionError<ReadError>> {
        self.logical.execute_with_limits(
            self.vertices.keys().copied(),
            self.edges.iter().map(edge_triple),
            |vid, predicates| self.matches(vid, predicates),
            limits,
        )
    }
}

fn edge_triple(record: &EdgeRecord) -> (VId, RelationId, VId) {
    (record.entry.src, record.entry.relation, record.entry.dst)
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
