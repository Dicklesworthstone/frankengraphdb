//! Query-selected non-detaching vertex deletion.
//!
//! This kernel freezes one prepared MATCH relation and reduces target columns to
//! distinct vertex identities. It deliberately does not decide whether a target
//! has incident relationships: only the write-capable host can answer that
//! against the canonical transaction overlay. The host must refuse such a
//! target before staging any delete. No storage effect or cascade lives here.

use crate::algebra::{GraphValueRow, PreparedGraphPattern, ValueProjection};
use crate::{
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension,
    GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
};
use fgdb_delta_types::RelationId;
use fgdb_types::VId;
use std::collections::BTreeSet;

pub const MAX_GRAPH_DELETE_TARGETS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphDeleteBuildError {
    EmptyTargets,
    TooManyTargets { limit: usize, observed: usize },
    TargetColumn { target: usize, column: usize },
}
impl core::fmt::Display for GraphDeleteBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph DELETE definition: {self:?}")
    }
}
impl core::error::Error for GraphDeleteBuildError {}

#[derive(Debug)]
pub enum GraphDeleteError<E> {
    Source(E),
    InvalidSourceStatistics,
    InputSchema {
        row: usize,
        column: usize,
    },
    TargetLimit {
        limit: u64,
        observed: u128,
    },
    /// The storage adapter observed at least one live incident relationship for
    /// a requested target in the exact transaction overlay. IDs remain redacted.
    IncidentRelationships,
}
impl<E: core::fmt::Display> core::fmt::Display for GraphDeleteError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::InvalidSourceStatistics => f.write_str("DELETE source returned inconsistent statistics"),
            Self::InputSchema { row, column } => write!(f, "DELETE row {row} has incompatible column {column}"),
            Self::TargetLimit { limit, observed } => write!(f, "DELETE target limit exceeded: {observed} > {limit}"),
            Self::IncidentRelationships => f.write_str("plain DELETE target has incident relationships; use DETACH DELETE or remove them first"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphDeleteError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphDeletePolicy {
    pub query: GqlQueryPolicy,
    pub max_targets: u64,
}
impl GraphDeletePolicy {
    #[must_use]
    pub const fn new(query: GqlQueryPolicy, max_targets: u64) -> Self {
        Self { query, max_targets }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphDeleteStats {
    pub selection: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
    pub target_vertices: u64,
}

#[derive(Debug)]
pub struct GraphDeleteProposal {
    targets: Vec<VId>,
    stats: GraphDeleteStats,
}
impl GraphDeleteProposal {
    #[must_use]
    pub fn targets(&self) -> &[VId] {
        &self.targets
    }
    #[must_use]
    pub const fn stats(&self) -> GraphDeleteStats {
        self.stats
    }
    #[must_use]
    pub fn into_targets(self) -> Vec<VId> {
        self.targets
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphDelete {
    selection: PreparedGraphPattern<GraphValueRow>,
    relation: RelationId,
    targets: Vec<usize>,
}
impl core::fmt::Debug for PreparedGraphDelete {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphDelete")
            .field("targets", &self.targets.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphDelete {
    pub fn prepare(
        selection: PreparedGraphPattern<GraphValueRow>,
        relation: RelationId,
        targets: Vec<usize>,
    ) -> Result<Self, GraphDeleteBuildError> {
        if targets.is_empty() {
            return Err(GraphDeleteBuildError::EmptyTargets);
        }
        if targets.len() > MAX_GRAPH_DELETE_TARGETS {
            return Err(GraphDeleteBuildError::TooManyTargets {
                limit: MAX_GRAPH_DELETE_TARGETS,
                observed: targets.len(),
            });
        }
        let columns = selection.value_columns();
        let mut seen = BTreeSet::new();
        for (at, &column) in targets.iter().enumerate() {
            if !matches!(columns.get(column), Some(ValueProjection::Vertex { .. })) {
                return Err(GraphDeleteBuildError::TargetColumn { target: at, column });
            }
            if !seen.insert(column) {
                // Repeated target columns are definition noise and would make
                // per-target accounting ambiguous. Treat them as the same
                // invalid target location rather than silently deduplicating.
                return Err(GraphDeleteBuildError::TargetColumn { target: at, column });
            }
        }
        Ok(Self {
            selection,
            relation,
            targets,
        })
    }

    #[must_use]
    pub fn selection(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.selection
    }
    #[must_use]
    pub const fn relation(&self) -> RelationId {
        self.relation
    }
    #[must_use]
    pub fn target_columns(&self) -> &[usize] {
        &self.targets
    }

    pub fn execute_governed<E, C>(
        &self,
        policy: GraphDeletePolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphDeleteProposal, GqlQueryError<GraphDeleteError<E>, C>> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        let selected = source(&self.selection, policy.query)
            .map_err(|error| error.map_source(GraphDeleteError::Source))?;
        if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
            return Err(GqlQueryError::Source(
                GraphDeleteError::InvalidSourceStatistics,
            ));
        }
        for (dimension, observed) in [
            (
                GqlBudgetDimension::SnapshotRecords,
                selected.rows.snapshot_records,
            ),
            (GqlBudgetDimension::ResultRows, selected.rows.result_rows),
        ] {
            policy
                .query
                .rows
                .check(dimension, observed)
                .map_err(GqlQueryError::Rows)?;
        }
        for (observed, limit, dimension) in [
            (
                selected.evaluator.work_units,
                policy.query.evaluator.max_work_units,
                GlaLimitDimension::WorkUnits,
            ),
            (
                selected.evaluator.scratch_entries,
                policy.query.evaluator.max_scratch_entries,
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            if observed > limit {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed: u128::from(observed),
                }));
            }
        }

        let mut evaluator = selected.evaluator;
        let mut event =
            |kind: GlaExecutionEvent| -> Result<(), GqlQueryError<GraphDeleteError<E>, C>> {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                let work = u128::from(evaluator.work_units) + 1;
                let scratch = u128::from(evaluator.scratch_entries)
                    + u128::from(kind == GlaExecutionEvent::ScratchEntry);
                for (observed, limit, dimension) in [
                    (
                        work,
                        policy.query.evaluator.max_work_units,
                        GlaLimitDimension::WorkUnits,
                    ),
                    (
                        scratch,
                        policy.query.evaluator.max_scratch_entries,
                        GlaLimitDimension::ScratchEntries,
                    ),
                ] {
                    if observed > u128::from(limit) {
                        return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                            dimension,
                            limit,
                            observed,
                        }));
                    }
                }
                evaluator.work_units = work as u64;
                evaluator.scratch_entries = scratch as u64;
                Ok(())
            };

        event(GlaExecutionEvent::Work)?;
        let columns = self.selection.value_columns();
        let mut targets = BTreeSet::new();
        for (row_at, row) in selected.value.iter().enumerate() {
            event(GlaExecutionEvent::Work)?;
            if row.len() != columns.len() {
                return Err(GqlQueryError::Source(GraphDeleteError::InputSchema {
                    row: row_at,
                    column: row.len().min(columns.len()),
                }));
            }
            for &column in &self.targets {
                event(GlaExecutionEvent::Work)?;
                let value = &row.values()[column];
                if value.is_null() {
                    continue;
                }
                let Some(vertex) = value.as_vertex() else {
                    return Err(GqlQueryError::Source(GraphDeleteError::InputSchema {
                        row: row_at,
                        column,
                    }));
                };
                if targets.contains(&vertex) {
                    continue;
                }
                let observed = targets.len() as u128 + 1;
                if observed > u128::from(policy.max_targets) {
                    return Err(GqlQueryError::Source(GraphDeleteError::TargetLimit {
                        limit: policy.max_targets,
                        observed,
                    }));
                }
                event(GlaExecutionEvent::ScratchEntry)?;
                targets.insert(vertex);
            }
        }
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        let targets = targets.into_iter().collect::<Vec<_>>();
        let stats = GraphDeleteStats {
            selection: selected.rows,
            evaluator,
            target_vertices: targets.len() as u64,
        };
        Ok(GraphDeleteProposal { targets, stats })
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:plain-graph-delete:v1\0".to_vec();
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        let input = self.selection.canonical_bytes();
        bytes.extend_from_slice(&(input.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&input);
        bytes.extend_from_slice(&(self.targets.len() as u64).to_be_bytes());
        for target in &self.targets {
            bytes.extend_from_slice(&(*target as u64).to_be_bytes());
        }
        bytes
    }
}
