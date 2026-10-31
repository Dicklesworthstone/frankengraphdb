//! Explicit procedure leaves for a capable external host.

use super::*;
use crate::PreparedProcedureCall;

/// The original bound call and its YIELD aliases, admitted only by the explicit
/// [`AsyncSpillSetPlan::compile_with_single_procedure`] or
/// [`AsyncSpillSetPlan::compile_with_independent_procedures`] capability, or an
/// explicit repeated-procedure capability for sources inside Apply bodies.
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
    pub(super) reads_elements: bool,
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

    /// Evaluate the original arguments once per reached invocation. Constant
    /// calls use the unit row; correlated calls require their owning Apply's
    /// imported frame. Compilation never evaluates them:
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
        frame: Option<&GraphValueRow>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<GraphValue>, SpillSetRowError<E>> {
        let unit = GraphValueRow::unit();
        let row = self
            .call
            .argument_input(frame, &unit)
            .map_err(SpillSetRowError::Native)?;
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        let mut arguments = Vec::with_capacity(self.call.arguments.len());
        for (column, argument) in self.call.arguments.iter().enumerate() {
            let value = projection::evaluate_value(argument, row, column, control)
                .map_err(argument_failure)?;
            arguments.push(value);
        }
        Ok(arguments)
    }

    /// The same evaluation with reached element reads at the invocation's
    /// pinned cut. Reader-free calls retain the synchronous path. The host
    /// owns the imported row and all ScratchEntry reservations through source
    /// opening; reserve_frame admits each recursive future before allocation.
    /// Property-read records spend the same residual allowance as the call.
    pub async fn evaluate_arguments_with_async_elements<E, S, M, G>(
        &self,
        frame: Option<&GraphValueRow>,
        control: &mut (impl FnMut(GlaExecutionEvent) -> Result<(), E> + Send),
        source: &mut S,
        reserve_frame: &M,
    ) -> Result<Vec<GraphValue>, SpillSetRowError<E>>
    where
        E: Send,
        S: AsyncSpillElementSource<Error = E>,
        M: Fn(usize) -> Result<G, E> + Sync,
        G: Send,
    {
        if !self.reads_elements {
            return self.evaluate_arguments(frame, control);
        }
        let unit = GraphValueRow::unit();
        let row = self
            .call
            .argument_input(frame, &unit)
            .map_err(SpillSetRowError::Native)?;
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        let mut arguments = Vec::with_capacity(self.call.arguments.len());
        for (column, argument) in self.call.arguments.iter().enumerate() {
            let value = projection::evaluate_value_async(
                argument,
                row,
                column,
                None,
                control,
                source,
                reserve_frame,
            )
            .await
            .map_err(argument_failure)?;
            arguments.push(value);
        }
        Ok(arguments)
    }
}

fn argument_failure<E>(error: projection::ProjectionFailure<E>) -> SpillSetRowError<E> {
    match error {
        projection::ProjectionFailure::Control(error) => SpillSetRowError::Control(error),
        projection::ProjectionFailure::Arithmetic { column, error } => {
            SpillSetRowError::Native(GraphSetExecutionError::Projection {
                row: 0,
                column,
                error,
            })
        }
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
