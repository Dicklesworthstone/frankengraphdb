//! Completed row pipelines enter the ordinary simultaneous mutation reducer.
//! Every graph source shares one frozen selection and one staging path.

use super::*;
use crate::PreparedGraphSet;
use std::{collections::BTreeMap, sync::Arc};

/// Build metadata from the actual leaves. A conditional body contributes its
/// names even when execution has no outer row; admission never guesses a
/// representative graph or manufactures a graph source for owned/unit input.
pub(super) fn input_catalog(
    input: &PreparedGraphSet,
) -> Result<Option<Arc<crate::ReverseSymbolCatalog>>, GraphMutationBuildError> {
    let mut merged: Option<Arc<crate::ReverseSymbolCatalog>> = None;
    input.admit_patterns(&mut |pattern| {
        let Some(catalog) = &pattern.plan().reverse_catalog else {
            return Ok(());
        };
        let Some(current) = merged.as_mut() else {
            merged = Some(Arc::clone(catalog));
            return Ok(());
        };
        if current.as_ref() == catalog.as_ref() {
            return Ok(());
        }
        let current = Arc::make_mut(current);
        merge_names(&mut current.labels, &catalog.labels)?;
        merge_names(&mut current.relations, &catalog.relations)?;
        merge_names(&mut current.properties, &catalog.properties)
    })?;
    Ok(merged)
}

/// Computed-map actions encode only their property catalog. Its checked names
/// join the actual input catalogs, without introducing label/relation names
/// that the action's canonical identity does not own.
pub(super) fn merge_property_catalog(
    merged: &mut Option<Arc<crate::ReverseSymbolCatalog>>,
    catalog: &crate::ReverseSymbolCatalog,
) -> Result<(), GraphMutationBuildError> {
    let Some(current) = merged.as_mut() else {
        *merged = Some(Arc::new(crate::ReverseSymbolCatalog {
            properties: catalog.properties.clone(),
            ..crate::ReverseSymbolCatalog::default()
        }));
        return Ok(());
    };
    if current.properties == catalog.properties {
        return Ok(());
    }
    merge_names(&mut Arc::make_mut(current).properties, &catalog.properties)
}

pub(super) fn merge_names<K: Copy + Ord>(
    merged: &mut BTreeMap<K, String>,
    names: &BTreeMap<K, String>,
) -> Result<(), GraphMutationBuildError> {
    for (key, name) in names {
        if merged.get(key).is_some_and(|previous| previous != name)
            || merged
                .iter()
                .any(|(previous_key, previous)| previous_key != key && previous == name)
        {
            return Err(GraphMutationBuildError::IncompatibleSourceCatalog);
        }
        merged.entry(*key).or_insert_with(|| name.clone());
    }
    Ok(())
}

impl PreparedGraphMutation {
    /// Select targets and RHS values from a completed relation. Graph joins,
    /// Apply bodies, owned/unit input, projections, filters, DISTINCT and pages
    /// all run before assignments.
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
        let selection = input.single_pattern_input().cloned();
        Self::prepare_input(selection, Some(input), relation, actions)
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

    /// Admit every real graph leaf before a host begins a write chain. This
    /// includes both sides of Apply, regardless of whether its body later has
    /// any row. No source is executed and no owned identity gains authority.
    pub fn admit_patterns<E>(
        &self,
        mut visitor: impl FnMut(&PreparedGraphPattern<GraphValueRow>) -> Result<(), E>,
    ) -> Result<(), E> {
        match (&self.input_relation, &self.selection) {
            (Some(input), _) => input.admit_patterns(&mut visitor),
            (None, Some(selection)) => visitor(selection),
            (None, None) => Ok(()),
        }
    }

    /// `owned` are a chain step's input rows, read by the input relation's
    /// one owned leaf; a definition with no such leaf refuses them, never
    /// drops them. `elements` reads `x.key` of an element the relation
    /// computes (fgdb-694ga); without it that read stays a typed error.
    pub(super) fn select_governed<E, C>(
        &self,
        policy: GqlQueryPolicy,
        owned: Option<Vec<GraphValueRow>>,
        source: super::collect::ErasedSource<'_, E, C>,
        elements: Option<&mut super::collect::MutationElementReader<'_, E>>,
        checkpoint: &mut impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphMutationError<E>, C>> {
        let owned_unavailable = || {
            GqlQueryError::Source(GraphMutationError::InputRelation(
                crate::GraphSetExecutionError::OwnedRowsUnavailable,
            ))
        };
        // Split a seeded reader off before the pattern closure borrows the
        // source; the relation below offers it to a joined MATCH leaf.
        let (mut source, mut seeded) = match source {
            super::collect::ErasedSource::Seeded { patterns, seeded } => (
                super::collect::ErasedSource::Reusable(patterns),
                Some(seeded),
            ),
            source => (source, None),
        };
        let mut patterns = |pattern: &PreparedGraphPattern<GraphValueRow>,
                            remaining: GqlQueryPolicy| {
            let selected = match &mut source {
                super::collect::ErasedSource::Once(source) => {
                    let Some(source) = source.take() else {
                        return Err(GqlQueryError::Source(SourceError::Reused));
                    };
                    source(pattern, remaining)
                }
                super::collect::ErasedSource::Reusable(source)
                | super::collect::ErasedSource::Seeded {
                    patterns: source, ..
                } => source(pattern, remaining),
            };
            selected.map_err(|error| error.map_source(SourceError::Host))
        };
        let Some(input) = &self.input_relation else {
            let (Some(selection), None) = (&self.selection, owned) else {
                return Err(owned_unavailable());
            };
            return patterns(selection, policy).map_err(|error| {
                error.map_source(|error| match error {
                    SourceError::Host(error) => GraphMutationError::Source(error),
                    SourceError::Reused => GraphMutationError::ReusableSourceRequired,
                })
            });
        };
        let mut elements = elements.map(|read| crate::set_ops::projection::MappedElements {
            read,
            error: SourceError::Host,
        });
        let mut combined;
        let host: &mut dyn crate::GraphSetSource<SourceError<E>, C> = match seeded.as_mut() {
            Some(reader) => {
                combined = crate::set_ops::WithSeededPatterns {
                    source: &mut patterns,
                    seeded: |pattern: &PreparedGraphPattern<GraphValueRow>,
                             seeds: crate::GraphSeeds<'_>,
                             remaining: GqlQueryPolicy| {
                        reader(pattern, seeds, remaining).map(|result| {
                            result.map_err(|error| error.map_source(SourceError::Host))
                        })
                    },
                };
                &mut combined
            }
            None => &mut patterns,
        };
        let mut source = crate::set_ops::BorrowedSource {
            source: host,
            elements: elements
                .as_mut()
                .map(|read| read as &mut super::collect::MutationElementReader<'_, SourceError<E>>),
        };
        match owned {
            None => input.execute_with_source(policy, &mut source, checkpoint),
            Some(rows) => input.execute_with_source(
                policy,
                &mut crate::set_ops::WithOwnedRows {
                    source: &mut source,
                    rows: Some(rows),
                },
                checkpoint,
            ),
        }
        .map_err(|error| {
            error.map_source(|error| match unwrap_relation(error) {
                Ok(error) => GraphMutationError::InputRelation(error),
                Err(()) => GraphMutationError::ReusableSourceRequired,
            })
        })
    }
}

enum SourceError<E> {
    Host(E),
    Reused,
}

/// Preserve the existing nested host/error shape. Only an exhausted legacy
/// callback becomes ReusableSourceRequired, including inside aggregate input.
fn unwrap_relation<E>(
    error: crate::GraphSetExecutionError<SourceError<E>>,
) -> Result<crate::GraphSetExecutionError<E>, ()> {
    use crate::GraphSetExecutionError as Error;
    Ok(match error {
        Error::Source(SourceError::Host(error)) => Error::Source(error),
        Error::Source(SourceError::Reused) => return Err(()),
        Error::Join(error) => Error::Join(error),
        Error::Aggregate(error) => Error::Aggregate(Box::new(unwrap_aggregate(*error)?)),
        Error::InputSchema { operand } => Error::InputSchema { operand },
        Error::InvalidSourceStatistics { operand } => Error::InvalidSourceStatistics { operand },
        Error::AccountingOverflow { dimension } => Error::AccountingOverflow { dimension },
        Error::Projection { row, column, error } => Error::Projection { row, column, error },
        Error::ProcedureUnavailable { operand } => Error::ProcedureUnavailable { operand },
        Error::PatternFactorsUnavailable { operand } => {
            Error::PatternFactorsUnavailable { operand }
        }
        Error::OwnedRowsUnavailable => Error::OwnedRowsUnavailable,
        Error::ArgumentsUnavailable => Error::ArgumentsUnavailable,
    })
}

fn unwrap_aggregate<E>(
    error: crate::GraphAggregateError<SourceError<E>>,
) -> Result<crate::GraphAggregateError<E>, ()> {
    use crate::GraphAggregateError as Error;
    Ok(match error {
        Error::Source(SourceError::Host(error)) => Error::Source(error),
        Error::Source(SourceError::Reused) => return Err(()),
        Error::InputRelation(error) => Error::InputRelation(unwrap_relation(error)?),
        Error::NonIntegerSum { aggregate } => Error::NonIntegerSum { aggregate },
        Error::NonIntegerAverage { aggregate } => Error::NonIntegerAverage { aggregate },
        Error::ArithmeticOverflow { aggregate } => Error::ArithmeticOverflow { aggregate },
        Error::NonIntegerHaving { predicate } => Error::NonIntegerHaving { predicate },
        Error::InputExpression { row, column, error } => {
            Error::InputExpression { row, column, error }
        }
        Error::OutputExpression { column, error } => Error::OutputExpression { column, error },
        Error::ResultCountOverflow => Error::ResultCountOverflow,
        Error::MultiplicityUnavailable => Error::MultiplicityUnavailable,
        Error::NonMonotonicGroups => Error::NonMonotonicGroups,
        Error::InvalidReductionInput => Error::InvalidReductionInput,
    })
}
