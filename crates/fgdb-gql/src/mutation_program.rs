//! Ordered prepared mutations with one cumulative admission contract.
//!
//! A program is one embedded operation, not a durable statement protocol or a
//! public savepoint. Each statement selects from its predecessor's workspace;
//! assignments WITHIN one statement retain their simultaneous semantics.
//! The fgdb WriteTxn adapter owns all-or-nothing workspace installation.

pub(crate) mod mixed;

use crate::{
    GlaExecutionStats, GlaLimitDimension, GqlBudgetDimension, GqlExecutionStats, GqlQueryError,
    GqlQueryPolicy, GraphMutationError, GraphMutationPolicy, GraphMutationStats,
    PreparedGraphMutation,
};
use fgdb_delta_types::RelationId;

pub const MAX_GRAPH_MUTATION_STATEMENTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphMutationProgramBuildError {
    Empty,
    TooManyStatements { limit: usize, observed: usize },
    MixedRelation { statement: usize },
}
impl core::fmt::Display for GraphMutationProgramBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "mutation program definition: {self:?}")
    }
}
impl core::error::Error for GraphMutationProgramBuildError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphMutationProgramDimension {
    SnapshotRecords,
    SelectedRows,
    WorkUnits,
    ScratchEntries,
    Effects,
}

/// A zero-based statement index identifies a refused step. An index equal to
/// statement_count identifies the final acceptance boundary. No query text,
/// field identity or operand value is added to the underlying error.
#[derive(Debug)]
pub enum GraphMutationProgramError<E, C> {
    Preflight(E),
    Statement {
        statement: usize,
        source: GqlQueryError<GraphMutationError<E>, C>,
    },
    Interrupted {
        completed_statements: usize,
        source: C,
    },
    Budget {
        statement: usize,
        dimension: GraphMutationProgramDimension,
        limit: u64,
        observed: u128,
    },
    InvalidStatistics {
        statement: usize,
    },
}
impl<E: core::fmt::Display, C: core::fmt::Display> core::fmt::Display
    for GraphMutationProgramError<E, C>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Preflight(source) => write!(f, "mutation program preflight: {source}"),
            Self::Statement { statement, source } => {
                write!(f, "mutation program statement {statement}: {source}")
            }
            Self::Interrupted {
                completed_statements,
                source,
            } => {
                write!(
                    f,
                    "mutation program interrupted after {completed_statements} statements: {source}"
                )
            }
            Self::Budget {
                statement,
                dimension,
                limit,
                observed,
            } => {
                write!(
                    f,
                    "mutation program statement {statement} {dimension:?}: {observed} > {limit}"
                )
            }
            Self::InvalidStatistics { statement } => {
                write!(
                    f,
                    "mutation program statement {statement} returned inconsistent statistics"
                )
            }
        }
    }
}
impl<E: core::error::Error + 'static, C: core::error::Error + 'static> core::error::Error
    for GraphMutationProgramError<E, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Preflight(source) => Some(source),
            Self::Statement { source, .. } => Some(source),
            Self::Interrupted { source, .. } => Some(source),
            Self::Budget { .. } | Self::InvalidStatistics { .. } => None,
        }
    }
}

/// Whole-program counters. Selected rows, target visits and proposed effects
/// are SUMS across statements, not unique elements or the final canonical net
/// effect. An update followed by its inverse still consumes both allowances.
/// Like GraphMutationStats, these exclude storage preparation and commit costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphMutationProgramStats {
    pub completed_statements: usize,
    pub selection: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
    pub target_vertex_visits: u64,
    pub effects: u64,
}
impl Default for GraphMutationProgramStats {
    fn default() -> Self {
        Self {
            completed_statements: 0,
            selection: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
            target_vertex_visits: 0,
            effects: 0,
        }
    }
}

/// Immutable bounded composition of already-bound mutation plans. Preparation
/// does not read a database or perform an effect. One relation coordinate keeps
/// the ordinary WriteTxn composition law explicit; this is not cross-relation
/// dependency reordering or a replacement for the independent-group API.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphMutationProgram {
    statements: Box<[PreparedGraphMutation]>,
}
impl core::fmt::Debug for PreparedGraphMutationProgram {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphMutationProgram")
            .field("statements", &self.statements.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphMutationProgram {
    pub fn prepare(
        statements: Vec<PreparedGraphMutation>,
    ) -> Result<Self, GraphMutationProgramBuildError> {
        if statements.is_empty() {
            return Err(GraphMutationProgramBuildError::Empty);
        }
        if statements.len() > MAX_GRAPH_MUTATION_STATEMENTS {
            return Err(GraphMutationProgramBuildError::TooManyStatements {
                limit: MAX_GRAPH_MUTATION_STATEMENTS,
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
    pub fn statements(&self) -> &[PreparedGraphMutation] {
        &self.statements
    }
    #[must_use]
    pub fn relation(&self) -> RelationId {
        self.statements[0].relation()
    }

    /// Explicit value-bearing application transcript. Statement order, duplicate
    /// steps and every complete bound definition participate in identity.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:ordered-mutation-program:v1\0".to_vec();
        bytes.extend_from_slice(&(self.statements.len() as u64).to_be_bytes());
        for statement in &self.statements {
            let input = statement.canonical_bytes();
            bytes.extend_from_slice(&(input.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&input);
        }
        bytes
    }

    /// Execute in source order through a TRUSTED workspace adapter. The adapter
    /// must expose prior successful steps to the next selection, retain all read
    /// observations and restore the starting workspace on ANY returned failure
    /// or unwind, including failure at the final checkpoint. This callback API
    /// cannot supply rollback for an arbitrary caller. fgdb's WriteTxn entrypoint
    /// enforces the private workspace guard structurally; no callback may commit.
    ///
    /// Each step receives only the remaining allowance. Empty matches do not
    /// terminate a program. No later step runs after a refusal. A final charged
    /// checkpoint precedes acceptance, never follows workspace installation.
    pub fn execute_governed<E, C>(
        &self,
        policy: GraphMutationPolicy,
        mut stage: impl FnMut(
            &PreparedGraphMutation,
            GraphMutationPolicy,
        )
            -> Result<GraphMutationStats, GqlQueryError<GraphMutationError<E>, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationProgramStats, GraphMutationProgramError<E, C>> {
        let mut meter = ProgramMeter {
            policy,
            stats: GraphMutationProgramStats::default(),
        };
        for (statement, input) in self.statements.iter().enumerate() {
            checkpoint().map_err(|source| GraphMutationProgramError::Interrupted {
                completed_statements: statement,
                source,
            })?;
            meter.boundary(statement)?;
            let stats = stage(input, meter.remaining())
                .map_err(|source| meter.translate(statement, source))?;
            meter.absorb(statement, input.actions().len(), stats)?;
        }
        checkpoint().map_err(|source| GraphMutationProgramError::Interrupted {
            completed_statements: self.statements.len(),
            source,
        })?;
        meter.boundary(self.statements.len())?;
        Ok(meter.stats)
    }
}

struct ProgramMeter {
    policy: GraphMutationPolicy,
    stats: GraphMutationProgramStats,
}
impl ProgramMeter {
    fn counter(&self, dimension: GraphMutationProgramDimension) -> (u64, u64) {
        use GraphMutationProgramDimension as D;
        match dimension {
            D::SnapshotRecords => (
                self.stats.selection.snapshot_records,
                self.policy
                    .query
                    .rows
                    .max_snapshot_records()
                    .unwrap_or(u64::MAX),
            ),
            D::SelectedRows => (
                self.stats.selection.result_rows,
                self.policy.query.rows.max_result_rows().unwrap_or(u64::MAX),
            ),
            D::WorkUnits => (
                self.stats.evaluator.work_units,
                self.policy.query.evaluator.max_work_units,
            ),
            D::ScratchEntries => (
                self.stats.evaluator.scratch_entries,
                self.policy.query.evaluator.max_scratch_entries,
            ),
            D::Effects => (self.stats.effects, self.policy.max_effects),
        }
    }
    fn add<E, C>(
        &self,
        statement: usize,
        dimension: GraphMutationProgramDimension,
        amount: u64,
    ) -> Result<u64, GraphMutationProgramError<E, C>> {
        let (used, limit) = self.counter(dimension);
        let observed = u128::from(used) + u128::from(amount);
        if observed > u128::from(limit) {
            return Err(GraphMutationProgramError::Budget {
                statement,
                dimension,
                limit,
                observed,
            });
        }
        Ok(observed as u64)
    }
    fn boundary<E, C>(&mut self, statement: usize) -> Result<(), GraphMutationProgramError<E, C>> {
        self.stats.evaluator.work_units =
            self.add(statement, GraphMutationProgramDimension::WorkUnits, 1)?;
        Ok(())
    }
    fn remaining(&self) -> GraphMutationPolicy {
        use GraphMutationProgramDimension as D;
        let remainder = |dimension| {
            let (used, limit) = self.counter(dimension);
            limit - used
        };
        GraphMutationPolicy::new(
            GqlQueryPolicy::new(
                remainder(D::SnapshotRecords),
                remainder(D::SelectedRows),
                remainder(D::WorkUnits),
                remainder(D::ScratchEntries),
            ),
            remainder(D::Effects),
        )
    }
    fn absorb<E, C>(
        &mut self,
        statement: usize,
        actions: usize,
        stats: GraphMutationStats,
    ) -> Result<(), GraphMutationProgramError<E, C>> {
        use GraphMutationProgramDimension as D;
        if stats.target_vertices > stats.effects
            || (stats.target_vertices == 0) != (stats.effects == 0)
            || u128::from(stats.effects) > u128::from(stats.selection.result_rows) * actions as u128
        {
            return Err(GraphMutationProgramError::InvalidStatistics { statement });
        }
        // Build the complete successor before mutating ANY counter. Even a late
        // dimension overflow leaves the successful prefix's accounting intact.
        let next = GraphMutationProgramStats {
            completed_statements: self.stats.completed_statements + 1,
            selection: GqlExecutionStats {
                snapshot_records: self.add(
                    statement,
                    D::SnapshotRecords,
                    stats.selection.snapshot_records,
                )?,
                result_rows: self.add(statement, D::SelectedRows, stats.selection.result_rows)?,
            },
            evaluator: GlaExecutionStats {
                work_units: self.add(statement, D::WorkUnits, stats.evaluator.work_units)?,
                scratch_entries: self.add(
                    statement,
                    D::ScratchEntries,
                    stats.evaluator.scratch_entries,
                )?,
            },
            // Each target visit owes an effect, whose checked sum below bounds
            // this addition. Check it independently rather than relying on order.
            target_vertex_visits: self
                .stats
                .target_vertex_visits
                .checked_add(stats.target_vertices)
                .ok_or(GraphMutationProgramError::InvalidStatistics { statement })?,
            effects: self.add(statement, D::Effects, stats.effects)?,
        };
        self.stats = next;
        Ok(())
    }
    fn translate<E, C>(
        &self,
        statement: usize,
        source: GqlQueryError<GraphMutationError<E>, C>,
    ) -> GraphMutationProgramError<E, C> {
        use GraphMutationProgramDimension as D;
        let (dimension, local) = match source {
            GqlQueryError::Rows(error) => (
                match error.dimension {
                    GqlBudgetDimension::SnapshotRecords => D::SnapshotRecords,
                    GqlBudgetDimension::ResultRows => D::SelectedRows,
                },
                u128::from(error.observed),
            ),
            GqlQueryError::Evaluator(error) => (
                match error.dimension {
                    GlaLimitDimension::WorkUnits => D::WorkUnits,
                    GlaLimitDimension::ScratchEntries => D::ScratchEntries,
                },
                error.observed,
            ),
            GqlQueryError::Source(GraphMutationError::EffectLimit { observed, .. }) => {
                (D::Effects, observed)
            }
            source => return GraphMutationProgramError::Statement { statement, source },
        };
        let (used, limit) = self.counter(dimension);
        match local.checked_add(u128::from(used)) {
            Some(observed) if observed > u128::from(limit) => GraphMutationProgramError::Budget {
                statement,
                dimension,
                limit,
                observed,
            },
            _ => GraphMutationProgramError::InvalidStatistics { statement },
        }
    }
}
