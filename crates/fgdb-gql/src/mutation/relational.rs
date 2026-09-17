//! Completed row pipelines enter the ordinary simultaneous mutation reducer.
//! There is still one graph source, one frozen selection and one staging path.

use super::*;
use crate::PreparedGraphSet;

impl PreparedGraphMutation {
    /// Select targets and RHS values from a completed single-source relation.
    /// All projections, filters, DISTINCT and pages run before assignments.
    /// Action indices address the final relation, never the graph leaf. Vertex
    /// identities must survive as vertex-typed columns; scalar values cannot
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
        input.check_parent_depth().map_err(GraphMutationBuildError::RelationalInput)?;
        let selection = input.single_pattern_input()
            .ok_or(GraphMutationBuildError::RequiresSingleGraphSource)?.clone();
        Self::prepare_input(selection, Some(input), relation, actions)
    }

    #[must_use]
    pub fn input_relation(&self) -> Option<&PreparedGraphSet> {
        self.input_relation.as_ref()
    }

    pub(super) fn select_governed<E, C>(
        &self,
        policy: GqlQueryPolicy,
        source: impl FnOnce(&PreparedGraphPattern<GraphValueRow>, GqlQueryPolicy)
            -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphMutationError<E>, C>> {
        let Some(input) = &self.input_relation else {
            return source(&self.selection, policy)
                .map_err(|error| error.map_source(GraphMutationError::Source));
        };
        let mut source = Some(source);
        input.execute_governed(policy, |pattern, remaining| {
            source.take().expect("preparation admitted one graph leaf")(pattern, remaining)
        }, checkpoint).map_err(|error| error.map_source(GraphMutationError::InputRelation))
    }
}
