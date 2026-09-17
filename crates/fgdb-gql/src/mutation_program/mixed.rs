//! Mixed creation/update/deletion/MERGE programs reuse the mutation program's
//! meter and existing per-statement kernels. The database adapter supplies the
//! SAME private workspace rollback guard; no matcher, writer or commit lives here.

mod merges;

use super::*;
use crate::insertion::{
    GraphInsertError, GraphInsertLimitDimension, GraphInsertPolicy, GraphInsertRequest,
    GraphInsertStats, PreparedGraphInsert,
};
use crate::{
    GraphDeleteError, GraphDeletePolicy, GraphDeleteStats, GraphEdgeMergeError,
    GraphEdgeMergePolicy, GraphEdgeMergeStats, GraphEdgeUpsertError, GraphEdgeUpsertPolicy,
    GraphEdgeUpsertStats, GraphVertexMergeError, GraphVertexMergePolicy, GraphVertexMergeStats,
    GraphVertexUpsertError, GraphVertexUpsertPolicy, GraphVertexUpsertStats, PreparedGraphDelete,
    PreparedGraphEdgeMerge, PreparedGraphEdgeUpsert, PreparedGraphVertexMerge,
    PreparedGraphVertexUpsert,
};

/// One already-bound step. No source text or parameter map enters execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphWriteStatement {
    Mutation(PreparedGraphMutation),
    Insert(PreparedGraphInsert),
    VertexMerge(PreparedGraphVertexMerge),
    VertexUpsert(PreparedGraphVertexUpsert),
    EdgeMerge(PreparedGraphEdgeMerge),
    EdgeUpsert(PreparedGraphEdgeUpsert),
    /// Non-detaching deletion; the host must validate canonical incidence.
    Delete(PreparedGraphDelete),
}
impl GraphWriteStatement {
    #[must_use]
    pub fn relation(&self) -> RelationId {
        match self {
            Self::Mutation(statement) => statement.relation(),
            Self::Insert(statement) => statement.relation(),
            Self::VertexMerge(statement) => statement.relation(),
            Self::VertexUpsert(statement) => statement.merge().relation(),
            Self::EdgeMerge(statement) => statement.relation(),
            Self::EdgeUpsert(statement) => statement.merge().relation(),
            Self::Delete(statement) => statement.relation(),
        }
    }
}
impl From<PreparedGraphMutation> for GraphWriteStatement {
    fn from(value: PreparedGraphMutation) -> Self {
        Self::Mutation(value)
    }
}
impl From<PreparedGraphInsert> for GraphWriteStatement {
    fn from(value: PreparedGraphInsert) -> Self {
        Self::Insert(value)
    }
}
impl From<PreparedGraphVertexMerge> for GraphWriteStatement {
    fn from(value: PreparedGraphVertexMerge) -> Self {
        Self::VertexMerge(value)
    }
}
impl From<PreparedGraphVertexUpsert> for GraphWriteStatement {
    fn from(value: PreparedGraphVertexUpsert) -> Self {
        Self::VertexUpsert(value)
    }
}
impl From<PreparedGraphEdgeMerge> for GraphWriteStatement {
    fn from(value: PreparedGraphEdgeMerge) -> Self {
        Self::EdgeMerge(value)
    }
}
impl From<PreparedGraphEdgeUpsert> for GraphWriteStatement {
    fn from(value: PreparedGraphEdgeUpsert) -> Self {
        Self::EdgeUpsert(value)
    }
}
impl From<PreparedGraphDelete> for GraphWriteStatement {
    fn from(value: PreparedGraphDelete) -> Self {
        Self::Delete(value)
    }
}

/// Identity requests are local to a statement AND its selected occurrence.
/// The host must also distinguish separate program invocations. Rollback never
/// rewinds an external allocator or licenses reusing an already-issued identity.
/// A vertex MERGE creation requests Vertex { row: 0, vertex: 0 } exactly once;
/// an edge MERGE requests Edge { row: 0, edge: 0 }. Matched and NoInput branches
/// never call the allocator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphWriteIdentityRequest {
    pub statement: usize,
    pub request: GraphInsertRequest,
}

/// One shared source/work/scratch allowance, plus independent cumulative caps
/// for mutation intents, new vertices and new edges. These count proposals,
/// including creations later deleted, not final net effects or commit costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphWriteProgramPolicy {
    pub mutations: GraphMutationPolicy,
    pub max_created_vertices: u64,
    pub max_created_edges: u64,
}
impl GraphWriteProgramPolicy {
    #[must_use]
    pub const fn new(
        query: GqlQueryPolicy,
        max_mutation_effects: u64,
        max_created_vertices: u64,
        max_created_edges: u64,
    ) -> Self {
        Self {
            mutations: GraphMutationPolicy::new(query, max_mutation_effects),
            max_created_vertices,
            max_created_edges,
        }
    }
    #[must_use]
    pub const fn insertion_policy(self) -> GraphInsertPolicy {
        GraphInsertPolicy::new(
            self.mutations.query,
            self.max_created_vertices,
            self.max_created_edges,
        )
    }
    #[must_use]
    pub const fn vertex_merge_policy(self) -> GraphVertexMergePolicy {
        GraphVertexMergePolicy::new(self.mutations.query)
            .with_creation_limit(self.max_created_vertices)
    }
    #[must_use]
    pub const fn vertex_upsert_policy(self) -> GraphVertexUpsertPolicy {
        GraphVertexUpsertPolicy::new(self.vertex_merge_policy(), self.mutations.max_effects)
    }
    #[must_use]
    pub const fn edge_merge_policy(self) -> GraphEdgeMergePolicy {
        GraphEdgeMergePolicy::new(self.mutations.query).with_creation_limit(self.max_created_edges)
    }
    #[must_use]
    pub const fn edge_upsert_policy(self) -> GraphEdgeUpsertPolicy {
        GraphEdgeUpsertPolicy::new(self.edge_merge_policy(), self.mutations.max_effects)
    }
    /// Each distinct plain-DELETE target is one mutation effect.
    #[must_use]
    pub const fn deletion_policy(self) -> GraphDeletePolicy {
        GraphDeletePolicy::new(self.mutations.query, self.mutations.max_effects)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphWriteStepStats {
    Mutation(GraphMutationStats),
    Insert(GraphInsertStats),
    VertexMerge(GraphVertexMergeStats),
    VertexUpsert(GraphVertexUpsertStats),
    EdgeMerge(GraphEdgeMergeStats),
    EdgeUpsert(GraphEdgeUpsertStats),
    Delete(GraphDeleteStats),
}
#[derive(Debug)]
pub enum GraphWriteStepError<E, A, C> {
    Mutation(GqlQueryError<GraphMutationError<E>, C>),
    Insert(GqlQueryError<GraphInsertError<E, A>, C>),
    VertexMerge(GqlQueryError<GraphVertexMergeError<E, A>, C>),
    VertexUpsert(GqlQueryError<GraphVertexUpsertError<E, A>, C>),
    EdgeMerge(GqlQueryError<GraphEdgeMergeError<E, A>, C>),
    EdgeUpsert(GqlQueryError<GraphEdgeUpsertError<E, A>, C>),
    Delete(GqlQueryError<GraphDeleteError<E>, C>),
}
impl<E: core::fmt::Display, A: core::fmt::Display, C: core::fmt::Display> core::fmt::Display
    for GraphWriteStepError<E, A, C>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mutation(error) => error.fmt(f),
            Self::Insert(error) => error.fmt(f),
            Self::VertexMerge(error) => error.fmt(f),
            Self::VertexUpsert(error) => error.fmt(f),
            Self::EdgeMerge(error) => error.fmt(f),
            Self::EdgeUpsert(error) => error.fmt(f),
            Self::Delete(error) => error.fmt(f),
        }
    }
}
impl<
    E: core::error::Error + 'static,
    A: core::error::Error + 'static,
    C: core::error::Error + 'static,
> core::error::Error for GraphWriteStepError<E, A, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Mutation(error) => Some(error),
            Self::Insert(error) => Some(error),
            Self::VertexMerge(error) => Some(error),
            Self::VertexUpsert(error) => Some(error),
            Self::EdgeMerge(error) => Some(error),
            Self::EdgeUpsert(error) => Some(error),
            Self::Delete(error) => Some(error),
        }
    }
}

/// Common program refusals keep the existing mutation-program vocabulary.
/// Creation limits report WHOLE-program counts, not a step's residual quota.
#[derive(Debug)]
pub enum GraphWriteProgramError<E, A, C> {
    Program(GraphMutationProgramError<E, C>),
    Insert {
        statement: usize,
        source: GqlQueryError<GraphInsertError<E, A>, C>,
    },
    VertexMerge {
        statement: usize,
        source: GqlQueryError<GraphVertexMergeError<E, A>, C>,
    },
    VertexUpsert {
        statement: usize,
        source: GqlQueryError<GraphVertexUpsertError<E, A>, C>,
    },
    EdgeMerge {
        statement: usize,
        source: GqlQueryError<GraphEdgeMergeError<E, A>, C>,
    },
    EdgeUpsert {
        statement: usize,
        source: GqlQueryError<GraphEdgeUpsertError<E, A>, C>,
    },
    Delete {
        statement: usize,
        source: GqlQueryError<GraphDeleteError<E>, C>,
    },
    CreationBudget {
        statement: usize,
        dimension: GraphInsertLimitDimension,
        limit: u64,
        observed: u128,
    },
}
impl<E, A, C> From<GraphMutationProgramError<E, C>> for GraphWriteProgramError<E, A, C> {
    fn from(error: GraphMutationProgramError<E, C>) -> Self {
        Self::Program(error)
    }
}
impl<E: core::fmt::Display, A: core::fmt::Display, C: core::fmt::Display> core::fmt::Display
    for GraphWriteProgramError<E, A, C>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Program(error) => error.fmt(f),
            Self::Insert { statement, source } => {
                write!(f, "write program creation step {statement}: {source}")
            }
            Self::VertexMerge { statement, source } => {
                write!(f, "write program vertex MERGE step {statement}: {source}")
            }
            Self::VertexUpsert { statement, source } => {
                write!(f, "write program vertex upsert step {statement}: {source}")
            }
            Self::EdgeMerge { statement, source } => write!(
                f,
                "write program relationship MERGE step {statement}: {source}"
            ),
            Self::EdgeUpsert { statement, source } => write!(
                f,
                "write program relationship upsert step {statement}: {source}"
            ),
            Self::Delete { statement, source } => {
                write!(f, "write program plain DELETE step {statement}: {source}")
            }
            Self::CreationBudget {
                statement,
                dimension,
                limit,
                observed,
            } => write!(
                f,
                "write program step {statement} created {dimension:?}: {observed} > {limit}"
            ),
        }
    }
}
impl<
    E: core::error::Error + 'static,
    A: core::error::Error + 'static,
    C: core::error::Error + 'static,
> core::error::Error for GraphWriteProgramError<E, A, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Program(error) => Some(error),
            Self::Insert { source, .. } => Some(source),
            Self::VertexMerge { source, .. } => Some(source),
            Self::VertexUpsert { source, .. } => Some(source),
            Self::EdgeMerge { source, .. } => Some(source),
            Self::EdgeUpsert { source, .. } => Some(source),
            Self::Delete { source, .. } => Some(source),
            Self::CreationBudget { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphWriteProgramStats {
    pub completed_statements: usize,
    /// Source records and selected occurrences summed over all statements.
    /// Includes relationship MERGE's existence and plain DELETE's incidence
    /// scans, not only MATCH. Vertex MERGE's internal creation unit is not a
    /// fabricated MATCH result.
    pub selection: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
    /// Distinct updated/deleted vertex visits summed over statements, including
    /// one visit for each nonempty selected vertex-upsert action branch.
    /// Relationship-property actions do not count as vertex visits.
    pub target_vertex_visits: u64,
    /// Distinct updated/deleted edge visits summed over statements.
    pub target_edge_visits: u64,
    pub mutation_effects: u64,
    pub created_vertices: u64,
    pub created_edges: u64,
}
impl GraphWriteProgramStats {
    /// The sum uses u128 even when all three individual u64 caps are maximal.
    #[must_use]
    pub fn proposed_effects(&self) -> u128 {
        u128::from(self.mutation_effects)
            + u128::from(self.created_vertices)
            + u128::from(self.created_edges)
    }
}

/// Source-ordered mixed graph writes at one relation coordinate. Later MATCH
/// and MERGE steps observe successful earlier creations, updates and deletes in
/// the canonical transaction overlay. This is not cross-relation sequencing.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphWriteProgram {
    statements: Box<[GraphWriteStatement]>,
}
impl core::fmt::Debug for PreparedGraphWriteProgram {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphWriteProgram")
            .field("statements", &self.statements.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphWriteProgram {
    pub fn prepare(
        statements: Vec<GraphWriteStatement>,
    ) -> Result<Self, GraphMutationProgramBuildError> {
        Self::prepare_with_statement_limit(statements, MAX_GRAPH_MUTATION_STATEMENTS)
    }

    // Only the native parameter-batch binder may request a larger admission.
    // It checks the full expansion before binding any record. Reuse ALL other
    // definition laws; execution still has one meter and one acceptance point.
    pub(crate) fn prepare_with_statement_limit(
        statements: Vec<GraphWriteStatement>,
        limit: usize,
    ) -> Result<Self, GraphMutationProgramBuildError> {
        let limit = limit.min(crate::PreparedGraphWriteScript::MAX_BATCH_STATEMENTS);
        if statements.is_empty() {
            return Err(GraphMutationProgramBuildError::Empty);
        }
        if statements.len() > limit {
            return Err(GraphMutationProgramBuildError::TooManyStatements {
                limit,
                observed: statements.len(),
            });
        }
        let relation = statements[0].relation();
        for (statement, input) in statements.iter().enumerate() {
            if input.relation() != relation {
                return Err(GraphMutationProgramBuildError::MixedRelation { statement });
            }
        }
        Ok(Self {
            statements: statements.into_boxed_slice(),
        })
    }
    #[must_use]
    pub fn statements(&self) -> &[GraphWriteStatement] {
        &self.statements
    }

    // Transfer bound definitions into an admitted batch without deep-cloning
    // each record's program a second time.
    pub(crate) fn into_statements(self) -> Box<[GraphWriteStatement]> {
        self.statements
    }

    #[must_use]
    pub fn relation(&self) -> RelationId {
        self.statements[0].relation()
    }

    /// The transcript binds ordered statement kinds and definitions. It is not
    /// a durable operation ID, identity-allocation log or commit acknowledgment.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:mixed-write-program:v1\0".to_vec();
        bytes.extend_from_slice(&(self.statements.len() as u64).to_be_bytes());
        for statement in &self.statements {
            let (kind, value) = match statement {
                GraphWriteStatement::Mutation(value) => (0, value.canonical_bytes()),
                GraphWriteStatement::Insert(value) => (1, value.canonical_bytes()),
                GraphWriteStatement::VertexMerge(value) => (2, value.canonical_bytes()),
                GraphWriteStatement::VertexUpsert(value) => (3, value.canonical_bytes()),
                GraphWriteStatement::EdgeMerge(value) => (4, value.canonical_bytes()),
                GraphWriteStatement::EdgeUpsert(value) => (5, value.canonical_bytes()),
                GraphWriteStatement::Delete(value) => (6, value.canonical_bytes()),
            };
            bytes.push(kind);
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&value);
        }
        bytes
    }

    /// TRUSTED workspace seam: stage must retain read observations, use the
    /// existing kernels, expose each successful prefix to the next step and
    /// restore the starting workspace on any error or unwind. This callback
    /// cannot make an arbitrary external writer atomic. WriteTxn implements
    /// these obligations using the existing private MutationProgramWorkspace.
    /// Identity allocation is external and cannot be rolled back. No callback
    /// may commit; the final charged checkpoint still precedes acceptance.
    pub fn execute_governed<E, A, C>(
        &self,
        policy: GraphWriteProgramPolicy,
        mut stage: impl FnMut(
            usize,
            &GraphWriteStatement,
            GraphWriteProgramPolicy,
        ) -> Result<GraphWriteStepStats, GraphWriteStepError<E, A, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphWriteProgramStats, GraphWriteProgramError<E, A, C>> {
        let mut meter = MixedMeter {
            common: ProgramMeter {
                policy: policy.mutations,
                stats: GraphMutationProgramStats::default(),
            },
            policy,
            vertices: 0,
            edges: 0,
        };
        for (statement, input) in self.statements.iter().enumerate() {
            checkpoint().map_err(|source| GraphMutationProgramError::Interrupted {
                completed_statements: statement,
                source,
            })?;
            meter.common.boundary(statement)?;
            let stats =
                stage(statement, input, meter.remaining()).map_err(|source| match source {
                    GraphWriteStepError::Mutation(error) => {
                        meter.common.translate(statement, error).into()
                    }
                    GraphWriteStepError::Insert(error) => meter.insert_failure(statement, error),
                    GraphWriteStepError::VertexMerge(error) => {
                        meter.vertex_merge_failure(statement, error)
                    }
                    GraphWriteStepError::VertexUpsert(error) => {
                        meter.vertex_upsert_failure(statement, error)
                    }
                    GraphWriteStepError::EdgeMerge(error) => {
                        meter.edge_merge_failure(statement, error)
                    }
                    GraphWriteStepError::EdgeUpsert(error) => {
                        meter.edge_upsert_failure(statement, error)
                    }
                    GraphWriteStepError::Delete(error) => meter.delete_failure(statement, error),
                })?;
            match (input, stats) {
                (GraphWriteStatement::Mutation(input), GraphWriteStepStats::Mutation(stats)) => {
                    meter
                        .common
                        .absorb(statement, input.actions().len(), stats)?;
                }
                (GraphWriteStatement::Insert(input), GraphWriteStepStats::Insert(stats)) => {
                    meter.absorb_insert(statement, input, stats)?;
                }
                (GraphWriteStatement::VertexMerge(_), GraphWriteStepStats::VertexMerge(stats)) => {
                    meter.absorb_vertex_merge(statement, stats)?;
                }
                (
                    GraphWriteStatement::VertexUpsert(input),
                    GraphWriteStepStats::VertexUpsert(stats),
                ) => {
                    meter.absorb_vertex_upsert(statement, input, stats)?;
                }
                (GraphWriteStatement::EdgeMerge(_), GraphWriteStepStats::EdgeMerge(stats)) => {
                    meter.absorb_edge_merge(statement, stats)?;
                }
                (
                    GraphWriteStatement::EdgeUpsert(input),
                    GraphWriteStepStats::EdgeUpsert(stats),
                ) => {
                    meter.absorb_edge_upsert(statement, input, stats)?;
                }
                (GraphWriteStatement::Delete(input), GraphWriteStepStats::Delete(stats)) => {
                    meter.absorb_delete(statement, input, stats)?;
                }
                _ => return Err(GraphMutationProgramError::InvalidStatistics { statement }.into()),
            }
        }
        checkpoint().map_err(|source| GraphMutationProgramError::Interrupted {
            completed_statements: self.statements.len(),
            source,
        })?;
        meter.common.boundary(self.statements.len())?;
        let stats = meter.common.stats;
        Ok(GraphWriteProgramStats {
            completed_statements: stats.completed_statements,
            selection: stats.selection,
            evaluator: stats.evaluator,
            target_vertex_visits: stats.target_vertex_visits,
            target_edge_visits: stats.target_edge_visits,
            mutation_effects: stats.effects,
            created_vertices: meter.vertices,
            created_edges: meter.edges,
        })
    }
}

struct MixedMeter {
    common: ProgramMeter,
    policy: GraphWriteProgramPolicy,
    vertices: u64,
    edges: u64,
}
impl MixedMeter {
    fn remaining(&self) -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy {
            mutations: self.common.remaining(),
            max_created_vertices: self.policy.max_created_vertices - self.vertices,
            max_created_edges: self.policy.max_created_edges - self.edges,
        }
    }
    fn creation_counter(&self, dimension: GraphInsertLimitDimension) -> (u64, u64) {
        match dimension {
            GraphInsertLimitDimension::Vertices => {
                (self.vertices, self.policy.max_created_vertices)
            }
            GraphInsertLimitDimension::Edges => (self.edges, self.policy.max_created_edges),
        }
    }
    fn add_creation<E, A, C>(
        &self,
        statement: usize,
        dimension: GraphInsertLimitDimension,
        count: u64,
    ) -> Result<u64, GraphWriteProgramError<E, A, C>> {
        let (used, limit) = self.creation_counter(dimension);
        let observed = u128::from(used) + u128::from(count);
        if observed > u128::from(limit) {
            return Err(GraphWriteProgramError::CreationBudget {
                statement,
                dimension,
                limit,
                observed,
            });
        }
        Ok(observed as u64)
    }
    fn absorb_insert<E, A, C>(
        &mut self,
        statement: usize,
        input: &PreparedGraphInsert,
        stats: GraphInsertStats,
    ) -> Result<(), GraphWriteProgramError<E, A, C>> {
        let rows = u128::from(stats.selection.result_rows);
        if u128::from(stats.created_vertices) != rows * input.vertices_per_row() as u128
            || u128::from(stats.created_edges) != rows * input.edges_per_row() as u128
            || (input.selection().is_none()
                && (stats.selection.result_rows != 1 || stats.selection.snapshot_records != 0))
        {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        }
        let vertices = self.add_creation(
            statement,
            GraphInsertLimitDimension::Vertices,
            stats.created_vertices,
        )?;
        let edges = self.add_creation(
            statement,
            GraphInsertLimitDimension::Edges,
            stats.created_edges,
        )?;
        // Creation has zero update/delete intents. Feed its REAL query work and
        // rows to the SAME checked meter; count creation effects separately.
        self.common.absorb(
            statement,
            0,
            GraphMutationStats {
                selection: stats.selection,
                evaluator: stats.evaluator,
                target_vertices: 0,
                target_edges: 0,
                effects: 0,
            },
        )?;
        self.vertices = vertices;
        self.edges = edges;
        Ok(())
    }
    fn insert_failure<E, A, C>(
        &self,
        statement: usize,
        source: GqlQueryError<GraphInsertError<E, A>, C>,
    ) -> GraphWriteProgramError<E, A, C> {
        match source {
            GqlQueryError::Rows(error) => self
                .common
                .translate(statement, GqlQueryError::Rows(error))
                .into(),
            GqlQueryError::Evaluator(error) => self
                .common
                .translate(statement, GqlQueryError::Evaluator(error))
                .into(),
            GqlQueryError::Source(GraphInsertError::Limit {
                dimension,
                observed: local,
                ..
            }) => self.creation_failure(statement, dimension, local),
            source => GraphWriteProgramError::Insert { statement, source },
        }
    }
    fn creation_failure<E, A, C>(
        &self,
        statement: usize,
        dimension: GraphInsertLimitDimension,
        local: u128,
    ) -> GraphWriteProgramError<E, A, C> {
        let (used, limit) = self.creation_counter(dimension);
        match local.checked_add(u128::from(used)) {
            Some(observed) if observed > u128::from(limit) => {
                GraphWriteProgramError::CreationBudget {
                    statement,
                    dimension,
                    limit,
                    observed,
                }
            }
            _ => GraphMutationProgramError::InvalidStatistics { statement }.into(),
        }
    }

    fn absorb_delete<E, A, C>(
        &mut self,
        statement: usize,
        input: &PreparedGraphDelete,
        stats: GraphDeleteStats,
    ) -> Result<(), GraphWriteProgramError<E, A, C>> {
        // Distinct targets each owe exactly one deletion intent. The common
        // meter checks target bounds, all cumulative dimensions and overflow.
        // Host stats already include incidence records/work: never add twice.
        let targets = stats.target_vertices.checked_add(stats.target_edges)
            .ok_or(GraphMutationProgramError::InvalidStatistics { statement })?;
        self.common.absorb(
            statement,
            input.target_columns().len(),
            GraphMutationStats {
                selection: stats.selection,
                evaluator: stats.evaluator,
                target_vertices: stats.target_vertices,
                target_edges: stats.target_edges,
                effects: targets,
            },
        )?;
        Ok(())
    }

    fn delete_failure<E, A, C>(
        &self,
        statement: usize,
        source: GqlQueryError<GraphDeleteError<E>, C>,
    ) -> GraphWriteProgramError<E, A, C> {
        match source {
            GqlQueryError::Rows(error) => self
                .common
                .translate(statement, GqlQueryError::Rows(error))
                .into(),
            GqlQueryError::Evaluator(error) => self
                .common
                .translate(statement, GqlQueryError::Evaluator(error))
                .into(),
            GqlQueryError::Source(GraphDeleteError::TargetLimit { limit, observed }) => self
                .common
                .translate(
                    statement,
                    GqlQueryError::Source(GraphMutationError::EffectLimit { limit, observed }),
                )
                .into(),
            source => GraphWriteProgramError::Delete { statement, source },
        }
    }
}
