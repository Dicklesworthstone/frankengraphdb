//! Mixed creation/update/deletion programs reuse the mutation program's meter
//! and the existing per-statement kernels. The database adapter supplies the
//! SAME private workspace rollback guard; no matcher, writer or commit lives here.

use super::*;
use crate::insertion::{
    GraphInsertError, GraphInsertLimitDimension, GraphInsertPolicy, GraphInsertRequest,
    GraphInsertStats, PreparedGraphInsert,
};

/// One already-bound step. No source text or parameter map enters execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphWriteStatement {
    Mutation(PreparedGraphMutation),
    Insert(PreparedGraphInsert),
}
impl GraphWriteStatement {
    #[must_use]
    pub fn relation(&self) -> RelationId {
        match self {
            Self::Mutation(statement) => statement.relation(),
            Self::Insert(statement) => statement.relation(),
        }
    }
}
impl From<PreparedGraphMutation> for GraphWriteStatement {
    fn from(value: PreparedGraphMutation) -> Self { Self::Mutation(value) }
}
impl From<PreparedGraphInsert> for GraphWriteStatement {
    fn from(value: PreparedGraphInsert) -> Self { Self::Insert(value) }
}

/// Identity requests are local to a statement AND its selected occurrence.
/// The host must also distinguish separate program invocations. Rollback never
/// rewinds an external allocator or licenses reusing an already-issued identity.
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
    pub const fn new(query: GqlQueryPolicy, max_mutation_effects: u64,
        max_created_vertices: u64, max_created_edges: u64) -> Self {
        Self {
            mutations: GraphMutationPolicy::new(query, max_mutation_effects),
            max_created_vertices,
            max_created_edges,
        }
    }
    #[must_use]
    pub const fn insertion_policy(self) -> GraphInsertPolicy {
        GraphInsertPolicy::new(self.mutations.query, self.max_created_vertices, self.max_created_edges)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphWriteStepStats {
    Mutation(GraphMutationStats),
    Insert(GraphInsertStats),
}
#[derive(Debug)]
pub enum GraphWriteStepError<E, A, C> {
    Mutation(GqlQueryError<GraphMutationError<E>, C>),
    Insert(GqlQueryError<GraphInsertError<E, A>, C>),
}
impl<E: core::fmt::Display, A: core::fmt::Display, C: core::fmt::Display>
    core::fmt::Display for GraphWriteStepError<E, A, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self { Self::Mutation(error) => error.fmt(f), Self::Insert(error) => error.fmt(f) }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static,
    C: core::error::Error + 'static> core::error::Error for GraphWriteStepError<E, A, C> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self { Self::Mutation(error) => Some(error), Self::Insert(error) => Some(error) }
    }
}

/// Common program refusals keep the existing mutation-program vocabulary.
/// Creation limits report WHOLE-program counts, not a step's residual quota.
#[derive(Debug)]
pub enum GraphWriteProgramError<E, A, C> {
    Program(GraphMutationProgramError<E, C>),
    Insert { statement: usize, source: GqlQueryError<GraphInsertError<E, A>, C> },
    CreationBudget {
        statement: usize,
        dimension: GraphInsertLimitDimension,
        limit: u64,
        observed: u128,
    },
}
impl<E, A, C> From<GraphMutationProgramError<E, C>> for GraphWriteProgramError<E, A, C> {
    fn from(error: GraphMutationProgramError<E, C>) -> Self { Self::Program(error) }
}
impl<E: core::fmt::Display, A: core::fmt::Display, C: core::fmt::Display>
    core::fmt::Display for GraphWriteProgramError<E, A, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Program(error) => error.fmt(f),
            Self::Insert { statement, source } => write!(f, "write program creation step {statement}: {source}"),
            Self::CreationBudget { statement, dimension, limit, observed } =>
                write!(f, "write program step {statement} created {dimension:?}: {observed} > {limit}"),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static,
    C: core::error::Error + 'static> core::error::Error for GraphWriteProgramError<E, A, C> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Program(error) => Some(error),
            Self::Insert { source, .. } => Some(source),
            Self::CreationBudget { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphWriteProgramStats {
    pub completed_statements: usize,
    pub selection: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
    /// Distinct updated/deleted vertex visits, summed over mutation statements.
    pub target_vertex_visits: u64,
    pub mutation_effects: u64,
    pub created_vertices: u64,
    pub created_edges: u64,
}
impl GraphWriteProgramStats {
    /// The sum uses u128 even when all three individual u64 caps are maximal.
    #[must_use]
    pub fn proposed_effects(&self) -> u128 {
        u128::from(self.mutation_effects) + u128::from(self.created_vertices) + u128::from(self.created_edges)
    }
}

/// Source-ordered mixed graph writes at one relation coordinate. A later MATCH
/// observes successful earlier creations, updates and deletes in the canonical
/// transaction overlay. This is not cross-relation sequencing or autocommit.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphWriteProgram {
    statements: Box<[GraphWriteStatement]>,
}
impl core::fmt::Debug for PreparedGraphWriteProgram {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphWriteProgram").field("statements", &self.statements.len())
            .field("definition", &"[REDACTED]").finish()
    }
}
impl PreparedGraphWriteProgram {
    pub fn prepare(statements: Vec<GraphWriteStatement>) -> Result<Self, GraphMutationProgramBuildError> {
        if statements.is_empty() { return Err(GraphMutationProgramBuildError::Empty); }
        if statements.len() > MAX_GRAPH_MUTATION_STATEMENTS {
            return Err(GraphMutationProgramBuildError::TooManyStatements {
                limit: MAX_GRAPH_MUTATION_STATEMENTS, observed: statements.len(),
            });
        }
        let relation = statements[0].relation();
        for (statement, input) in statements.iter().enumerate() {
            if input.relation() != relation { return Err(GraphMutationProgramBuildError::MixedRelation { statement }); }
        }
        Ok(Self { statements: statements.into_boxed_slice() })
    }
    #[must_use]
    pub fn statements(&self) -> &[GraphWriteStatement] { &self.statements }
    #[must_use]
    pub fn relation(&self) -> RelationId { self.statements[0].relation() }

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
        &self, policy: GraphWriteProgramPolicy,
        mut stage: impl FnMut(usize, &GraphWriteStatement, GraphWriteProgramPolicy)
            -> Result<GraphWriteStepStats, GraphWriteStepError<E, A, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphWriteProgramStats, GraphWriteProgramError<E, A, C>> {
        let mut meter = MixedMeter {
            common: ProgramMeter { policy: policy.mutations, stats: GraphMutationProgramStats::default() },
            policy, vertices: 0, edges: 0,
        };
        for (statement, input) in self.statements.iter().enumerate() {
            checkpoint().map_err(|source| GraphMutationProgramError::Interrupted {
                completed_statements: statement, source,
            })?;
            meter.common.boundary(statement)?;
            let stats = stage(statement, input, meter.remaining()).map_err(|source| match source {
                GraphWriteStepError::Mutation(error) => meter.common.translate(statement, error).into(),
                GraphWriteStepError::Insert(error) => meter.insert_failure(statement, error),
            })?;
            match (input, stats) {
                (GraphWriteStatement::Mutation(input), GraphWriteStepStats::Mutation(stats)) => {
                    meter.common.absorb(statement, input.actions().len(), stats)?;
                }
                (GraphWriteStatement::Insert(input), GraphWriteStepStats::Insert(stats)) => {
                    meter.absorb_insert(statement, input, stats)?;
                }
                _ => return Err(GraphMutationProgramError::InvalidStatistics { statement }.into()),
            }
        }
        checkpoint().map_err(|source| GraphMutationProgramError::Interrupted {
            completed_statements: self.statements.len(), source,
        })?;
        meter.common.boundary(self.statements.len())?;
        let stats = meter.common.stats;
        Ok(GraphWriteProgramStats {
            completed_statements: stats.completed_statements, selection: stats.selection,
            evaluator: stats.evaluator, target_vertex_visits: stats.target_vertex_visits,
            mutation_effects: stats.effects, created_vertices: meter.vertices, created_edges: meter.edges,
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
            GraphInsertLimitDimension::Vertices => (self.vertices, self.policy.max_created_vertices),
            GraphInsertLimitDimension::Edges => (self.edges, self.policy.max_created_edges),
        }
    }
    fn add_creation<E, A, C>(&self, statement: usize, dimension: GraphInsertLimitDimension, count: u64)
        -> Result<u64, GraphWriteProgramError<E, A, C>> {
        let (used, limit) = self.creation_counter(dimension);
        let observed = u128::from(used) + u128::from(count);
        if observed > u128::from(limit) {
            return Err(GraphWriteProgramError::CreationBudget { statement, dimension, limit, observed });
        }
        Ok(observed as u64)
    }
    fn absorb_insert<E, A, C>(&mut self, statement: usize, input: &PreparedGraphInsert, stats: GraphInsertStats)
        -> Result<(), GraphWriteProgramError<E, A, C>> {
        let rows = u128::from(stats.selection.result_rows);
        if u128::from(stats.created_vertices) != rows * input.vertices_per_row() as u128
            || u128::from(stats.created_edges) != rows * input.edges_per_row() as u128
            || (input.selection().is_none() && (stats.selection.result_rows != 1 || stats.selection.snapshot_records != 0))
        {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        }
        let vertices = self.add_creation(statement, GraphInsertLimitDimension::Vertices, stats.created_vertices)?;
        let edges = self.add_creation(statement, GraphInsertLimitDimension::Edges, stats.created_edges)?;
        // Creation has zero update/delete intents. Feed its REAL query work and
        // rows to the SAME checked meter; count creation effects separately.
        self.common.absorb(statement, 0, GraphMutationStats {
            selection: stats.selection, evaluator: stats.evaluator, target_vertices: 0, effects: 0,
        })?;
        self.vertices = vertices;
        self.edges = edges;
        Ok(())
    }
    fn insert_failure<E, A, C>(&self, statement: usize, source: GqlQueryError<GraphInsertError<E, A>, C>)
        -> GraphWriteProgramError<E, A, C> {
        match source {
            GqlQueryError::Rows(error) => self.common.translate(statement, GqlQueryError::Rows(error)).into(),
            GqlQueryError::Evaluator(error) => self.common.translate(statement, GqlQueryError::Evaluator(error)).into(),
            GqlQueryError::Source(GraphInsertError::Limit { dimension, observed: local, .. }) => {
                let (used, limit) = self.creation_counter(dimension);
                match local.checked_add(u128::from(used)) {
                    Some(observed) if observed > u128::from(limit) =>
                        GraphWriteProgramError::CreationBudget { statement, dimension, limit, observed },
                    _ => GraphMutationProgramError::InvalidStatistics { statement }.into(),
                }
            }
            source => GraphWriteProgramError::Insert { statement, source },
        }
    }
}
