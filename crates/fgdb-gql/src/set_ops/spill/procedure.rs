//! Explicit, once-executed procedure leaves for a capable external host.

use super::*;
use crate::PreparedProcedureCall;

/// The original bound call and its YIELD aliases, admitted only by the explicit
/// [`AsyncSpillSetPlan::compile_with_single_procedure`] or
/// [`AsyncSpillSetPlan::compile_with_independent_procedures`] capability.
/// This definition does not confer authority or validate a host's procedure
/// implementation.
///
/// Before opening any source or scratch writer, the host must validate the
/// namespace, signature, graph projection and output profile from `call()`.
/// It must then execute the source at the same pinned cut as every graph and
/// property read, with the current cumulative query and physical allowances.
/// Complete source admission, output-domain validation and source statistics
/// precede downstream rows, including when the enclosing page is empty.
///
/// Source rows have the procedure's YIELD order, named by `columns()`; the
/// initial native stage owns canonical ordering and the original clauses.
/// A ranked call keeps its declared order unless an explicit order replaces
/// it. No graph sort tail or substitute procedure evaluator is synthesized.
#[derive(Clone)]
pub struct SpillSetProcedurePlan {
    pub(super) call: Box<PreparedProcedureCall>,
    pub(super) columns: Vec<String>,
    pub(super) types: Vec<GraphSetColumnType>,
}

impl SpillSetProcedurePlan {
    pub fn call(&self) -> &PreparedProcedureCall {
        &self.call
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }

    pub fn argument_count(&self) -> usize {
        self.call.arguments.len()
    }

    /// Evaluate the original constant arguments once, when this leaf is
    /// reached, against the native unit row. Compilation never evaluates them:
    /// a late argument failure must not move ahead of an earlier child failure.
    /// Named arguments, schema bindings and projection metadata stay in `call`.
    ///
    /// Reserve `argument_count()` value slots before calling, and admit the
    /// vector's metadata on the first ScratchEntry before allocation. Subsequent
    /// native events admit each expression and payload copy. Keep the host's
    /// growing reservation alive through procedure opening. Any
    /// refusal drops the private prefix and returns the original typed error.
    /// This helper emits no ResultRow event and opens no source.
    pub fn evaluate_arguments<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<GraphValue>, SpillSetRowError<E>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        let unit = GraphValueRow::unit();
        let mut arguments = Vec::with_capacity(self.call.arguments.len());
        for (column, argument) in self.call.arguments.iter().enumerate() {
            let value =
                projection::evaluate_value(argument, &unit, column, control).map_err(|error| {
                    match error {
                        projection::ProjectionFailure::Control(error) => {
                            SpillSetRowError::Control(error)
                        }
                        projection::ProjectionFailure::Arithmetic { column, error } => {
                            SpillSetRowError::Native(GraphSetExecutionError::Projection {
                                row: 0,
                                column,
                                error,
                            })
                        }
                    }
                })?;
            arguments.push(value);
        }
        Ok(arguments)
    }
}

impl core::fmt::Debug for SpillSetProcedurePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpillSetProcedurePlan")
            .field("arguments", &self.call.arguments.len())
            .field("columns", &self.columns.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
