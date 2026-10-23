//! Completed row pipelines enter the ordinary simultaneous mutation reducer.
//! There is still one graph source, one frozen selection and one staging path.

use super::*;
use crate::PreparedGraphSet;

impl PreparedGraphMutation {
    /// Select targets and RHS values from a completed single-source relation.
    /// All projections, filters, DISTINCT and pages run before assignments.
    /// Action indices address the final relation, never the graph leaf. Element
    /// identities must survive as vertex/edge-typed columns; scalar values cannot
    /// fabricate targets. Null targets retain the ordinary no-assignment rule.
    ///
    /// Existing WriteTxn, autocommit and atomic-program mutation entrypoints
    /// execute this definition unchanged. Selection and proposals share one
    /// work/scratch allowance. Only the final selection uses the selected-row
    /// quota; hidden rows remain charged private scratch, not public results.
    /// This bounded materialized path is not a streaming or spill implementation.
    pub fn prepare_relation(
        input: PreparedGraphSet,
        relation: RelationId,
        actions: Vec<GraphMutationAction>,
    ) -> Result<Self, GraphMutationBuildError> {
        input
            .check_parent_depth()
            .map_err(GraphMutationBuildError::RelationalInput)?;
        let selection = input
            .single_pattern_input()
            .ok_or(GraphMutationBuildError::RequiresSingleGraphSource)?
            .clone();
        Self::prepare_input(Some(selection), Some(input), relation, actions)
    }

    /// A chain step's SET, REMOVE or DELETE over the previous write's rows
    /// (fgdb-cpny9): `input` reads only its one owned leaf, never the graph,
    /// and every action addresses its row. It runs only through
    /// [`Self::execute_governed_over`] with those rows.
    pub(crate) fn prepare_owned(
        input: PreparedGraphSet,
        relation: RelationId,
        actions: Vec<GraphMutationAction>,
    ) -> Result<Self, GraphMutationBuildError> {
        input
            .check_parent_depth()
            .map_err(GraphMutationBuildError::RelationalInput)?;
        if input.reads_graph() {
            return Err(GraphMutationBuildError::RequiresSingleGraphSource);
        }
        Self::prepare_input(None, Some(input), relation, actions)
    }

    #[must_use]
    pub fn input_relation(&self) -> Option<&PreparedGraphSet> {
        self.input_relation.as_ref()
    }

    /// `owned` are a chain step's input rows, read by the input relation's
    /// one owned leaf; a definition with no such leaf refuses them, never
    /// drops them.
    pub(super) fn select_governed<E, C>(
        &self,
        policy: GqlQueryPolicy,
        owned: Option<Vec<GraphValueRow>>,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphMutationError<E>, C>> {
        let owned_unavailable = || {
            GqlQueryError::Source(GraphMutationError::InputRelation(
                crate::GraphSetExecutionError::OwnedRowsUnavailable,
            ))
        };
        let Some(input) = &self.input_relation else {
            let (Some(selection), None) = (&self.selection, owned) else {
                return Err(owned_unavailable());
            };
            return source(selection, policy)
                .map_err(|error| error.map_source(GraphMutationError::Source));
        };
        let mut source = Some(source);
        let mut patterns = |pattern: &PreparedGraphPattern<GraphValueRow>,
                            remaining: GqlQueryPolicy| {
            source.take().expect("preparation admitted one graph leaf")(pattern, remaining)
        };
        match owned {
            None => input.execute_governed(policy, patterns, checkpoint),
            Some(rows) => input.execute_with_source(
                policy,
                &mut crate::set_ops::WithOwnedRows {
                    source: &mut patterns,
                    rows: Some(rows),
                },
                checkpoint,
            ),
        }
        .map_err(|error| error.map_source(GraphMutationError::InputRelation))
    }
}
