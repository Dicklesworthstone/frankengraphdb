//! MERGE branches participate in the existing program meter without inventing
//! MATCH rows for a create branch or losing its separate creation allowance.

use super::*;
use crate::{GraphEdgeUpsertBranch, GraphVertexUpsertBranch};

impl MixedMeter {
    pub(super) fn absorb_vertex_merge<E, A, C>(
        &mut self,
        statement: usize,
        stats: GraphVertexMergeStats,
    ) -> Result<(), GraphWriteProgramError<E, A, C>> {
        if stats.created_vertices > 1
            || (stats.created_vertices == 0 && stats.match_selection.result_rows == 0)
        {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        }
        let vertices = self.add_creation(
            statement, GraphInsertLimitDimension::Vertices, stats.created_vertices,
        )?;
        self.common.absorb(statement, 0, GraphMutationStats {
            selection: stats.match_selection,
            evaluator: stats.evaluator,
            target_vertices: 0,
            effects: 0,
        })?;
        self.vertices = vertices;
        Ok(())
    }

    pub(super) fn absorb_vertex_upsert<E, A, C>(
        &mut self,
        statement: usize,
        input: &PreparedGraphVertexUpsert,
        stats: GraphVertexUpsertStats,
    ) -> Result<(), GraphWriteProgramError<E, A, C>> {
        let (actions, created) = match stats.branch {
            GraphVertexUpsertBranch::Match => (input.on_match(), 0),
            GraphVertexUpsertBranch::Create => (input.on_create(), 1),
        };
        if stats.merge.created_vertices != created
            || stats.action_effects as u128 != actions.len() as u128
        {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        }
        // A create branch can apply actions despite selecting ZERO MATCH rows.
        // Validate its exact branch above instead of applying the ordinary
        // mutation law effects <= selected_rows * actions to fabricated rows.
        let effects = self.common.add(
            statement, GraphMutationProgramDimension::Effects, stats.action_effects,
        )?;
        let targets = self.common.stats.target_vertex_visits
            .checked_add(u64::from(stats.action_effects != 0))
            .ok_or(GraphMutationProgramError::InvalidStatistics { statement })?;
        // All action counters are checked before the common successor changes;
        // absorb_vertex_merge itself installs counters only after all checks.
        self.absorb_vertex_merge(statement, stats.merge)?;
        self.common.stats.effects = effects;
        self.common.stats.target_vertex_visits = targets;
        Ok(())
    }

    pub(super) fn absorb_edge_merge<E, A, C>(
        &mut self,
        statement: usize,
        stats: GraphEdgeMergeStats,
    ) -> Result<(), GraphWriteProgramError<E, A, C>> {
        let no_input = stats.match_selection.result_rows == 0;
        if stats.created_edges > 1
            || (no_input && (stats.overlay_edges != 0 || stats.created_edges != 0))
            || (!no_input && stats.created_edges == 0 && stats.overlay_edges == 0)
        {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        }
        // The existence scan is additional source work, not part of MATCH's
        // snapshot count. Charge it exactly once, including read-only matches.
        // Widen BEFORE adding so hostile stats cannot wrap at u64::MAX.
        let records = u128::from(stats.match_selection.snapshot_records) + u128::from(stats.overlay_edges);
        let dimension = GraphMutationProgramDimension::SnapshotRecords;
        let (used, limit) = self.common.counter(dimension);
        let observed = u128::from(used) + records;
        if observed > u128::from(limit) {
            return Err(GraphMutationProgramError::Budget {
                statement, dimension, limit, observed,
            }.into());
        }
        let edges = self.add_creation(statement, GraphInsertLimitDimension::Edges, stats.created_edges)?;
        self.common.absorb(statement, 0, GraphMutationStats {
            selection: GqlExecutionStats {
                snapshot_records: records as u64,
                result_rows: stats.match_selection.result_rows,
            },
            evaluator: stats.evaluator,
            target_vertices: 0,
            effects: 0,
        })?;
        self.edges = edges;
        Ok(())
    }

    pub(super) fn absorb_edge_upsert<E, A, C>(
        &mut self,
        statement: usize,
        input: &PreparedGraphEdgeUpsert,
        stats: GraphEdgeUpsertStats,
    ) -> Result<(), GraphWriteProgramError<E, A, C>> {
        let (branch, actions) = if stats.merge.match_selection.result_rows == 0 {
            (GraphEdgeUpsertBranch::NoInput, &[][..])
        } else if stats.merge.created_edges == 1 {
            (GraphEdgeUpsertBranch::Create, input.on_create())
        } else {
            (GraphEdgeUpsertBranch::Match, input.on_match())
        };
        let effects = u128::from(stats.action_effects);
        // One owned entry/work event per action and a final acceptance event.
        // Widen before addition; wrapped or underreported totals are invalid.
        if stats.branch != branch || effects != actions.len() as u128
            || u128::from(stats.evaluator.work_units)
                != u128::from(stats.merge.evaluator.work_units) + effects + 1
            || u128::from(stats.evaluator.scratch_entries)
                != u128::from(stats.merge.evaluator.scratch_entries) + effects
        {
            return Err(GraphMutationProgramError::InvalidStatistics { statement }.into());
        }
        let effects = self.common.add(
            statement, GraphMutationProgramDimension::Effects, stats.action_effects,
        )?;
        // The total already includes MERGE. Reuse its source/creation validation
        // with the cumulative evaluator, never charge the nested work twice.
        self.absorb_edge_merge(statement, GraphEdgeMergeStats {
            evaluator: stats.evaluator, ..stats.merge
        })?;
        self.common.stats.effects = effects;
        // Updating an edge is not a vertex visit.
        Ok(())
    }

    pub(super) fn vertex_merge_failure<E, A, C>(
        &self,
        statement: usize,
        source: GqlQueryError<GraphVertexMergeError<E, A>, C>,
    ) -> GraphWriteProgramError<E, A, C> {
        match source {
            GqlQueryError::Rows(error) => self.common.translate(statement, GqlQueryError::Rows(error)).into(),
            GqlQueryError::Evaluator(error) => self.common.translate(statement, GqlQueryError::Evaluator(error)).into(),
            GqlQueryError::Source(GraphVertexMergeError::Creation(GraphInsertError::Limit {
                dimension, observed, ..
            })) => self.creation_failure(statement, dimension, observed),
            source => GraphWriteProgramError::VertexMerge { statement, source },
        }
    }

    pub(super) fn vertex_upsert_failure<E, A, C>(
        &self,
        statement: usize,
        source: GqlQueryError<GraphVertexUpsertError<E, A>, C>,
    ) -> GraphWriteProgramError<E, A, C> {
        match source {
            GqlQueryError::Rows(error) => self.common.translate(statement, GqlQueryError::Rows(error)).into(),
            GqlQueryError::Evaluator(error) => self.common.translate(statement, GqlQueryError::Evaluator(error)).into(),
            GqlQueryError::Source(GraphVertexUpsertError::Merge(
                GraphVertexMergeError::Creation(GraphInsertError::Limit { dimension, observed, .. }),
            )) => self.creation_failure(statement, dimension, observed),
            GqlQueryError::Source(GraphVertexUpsertError::ActionLimit { observed: local, .. }) => {
                let dimension = GraphMutationProgramDimension::Effects;
                let (used, limit) = self.common.counter(dimension);
                match local.checked_add(u128::from(used)) {
                    Some(observed) if observed > u128::from(limit) =>
                        GraphMutationProgramError::Budget { statement, dimension, limit, observed }.into(),
                    _ => GraphMutationProgramError::InvalidStatistics { statement }.into(),
                }
            }
            source => GraphWriteProgramError::VertexUpsert { statement, source },
        }
    }

    pub(super) fn edge_merge_failure<E, A, C>(
        &self,
        statement: usize,
        source: GqlQueryError<GraphEdgeMergeError<E, A>, C>,
    ) -> GraphWriteProgramError<E, A, C> {
        match source {
            GqlQueryError::Rows(error) => self.common.translate(statement, GqlQueryError::Rows(error)).into(),
            GqlQueryError::Evaluator(error) => self.common.translate(statement, GqlQueryError::Evaluator(error)).into(),
            GqlQueryError::Source(GraphEdgeMergeError::CreationLimit { observed, .. }) =>
                self.creation_failure(statement, GraphInsertLimitDimension::Edges, observed),
            source => GraphWriteProgramError::EdgeMerge { statement, source },
        }
    }

    pub(super) fn edge_upsert_failure<E, A, C>(
        &self,
        statement: usize,
        source: GqlQueryError<GraphEdgeUpsertError<E, A>, C>,
    ) -> GraphWriteProgramError<E, A, C> {
        match source {
            GqlQueryError::Rows(error) => self.common.translate(statement, GqlQueryError::Rows(error)).into(),
            GqlQueryError::Evaluator(error) => self.common.translate(statement, GqlQueryError::Evaluator(error)).into(),
            GqlQueryError::Source(GraphEdgeUpsertError::Merge(GraphEdgeMergeError::CreationLimit { observed, .. })) =>
                self.creation_failure(statement, GraphInsertLimitDimension::Edges, observed),
            GqlQueryError::Source(GraphEdgeUpsertError::ActionLimit { observed: local, .. }) => {
                let dimension = GraphMutationProgramDimension::Effects;
                let (used, limit) = self.common.counter(dimension);
                match local.checked_add(u128::from(used)) {
                    Some(observed) if observed > u128::from(limit) =>
                        GraphMutationProgramError::Budget { statement, dimension, limit, observed }.into(),
                    _ => GraphMutationProgramError::InvalidStatistics { statement }.into(),
                }
            }
            source => GraphWriteProgramError::EdgeUpsert { statement, source },
        }
    }
}
