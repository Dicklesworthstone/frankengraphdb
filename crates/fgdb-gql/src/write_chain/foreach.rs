//! Native FOREACH is a scoped side-effect iteration, not an UNWIND rewrite.
//! The host retains one transaction, permit and cumulative execution meter.

use super::{GraphWriteChainStep, PreparedGraphWriteChain, PreparedGraphWriteChainText};
use crate::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use crate::set_ops::WithOwnedRows;
use crate::set_text::multipart::BoundReadInput;
use crate::{
    GlaExecutionEvent, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphIntegerError,
    GraphIntegerErrorKind, GraphSetColumnType, GraphSetExecutionError, GraphSetSource,
    PreparedGraphSet,
};
use fgdb_delta_types::PropertyKeyId;

#[derive(Clone)]
pub(crate) struct PreparedGraphForeachText {
    pub(crate) input: BoundReadInput,
    pub(crate) collection: BoundReadInput,
    pub(crate) output: Option<BoundReadInput>,
    pub(crate) body: PreparedGraphWriteChainText,
    pub(crate) outer_columns: Vec<String>,
    pub(crate) outer_types: Vec<GraphSetColumnType>,
    /// The visible outer columns imported by the body. A shadowed loop name
    /// is omitted here, while its original outer value remains preserved.
    pub(crate) imports: Vec<usize>,
    pub(crate) read_keys: Vec<PropertyKeyId>,
    pub(crate) returning: bool,
}

/// A prepared scoped iteration. Its input selects outer occurrences; a separate
/// projection evaluates the collection immediately before each occurrence.
/// The body imports visible columns and one member, keeping all local names
/// out of the outer namespace.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphForeach {
    pub(crate) input: PreparedGraphSet,
    pub(crate) collection: PreparedGraphSet,
    pub(crate) output: Option<PreparedGraphSet>,
    pub(crate) body: PreparedGraphWriteChain,
    pub(crate) outer_columns: Vec<String>,
    pub(crate) outer_types: Vec<GraphSetColumnType>,
    pub(crate) imports: Vec<usize>,
    pub(crate) read_keys: Vec<PropertyKeyId>,
    pub(crate) returning: bool,
}

impl core::fmt::Debug for PreparedGraphForeach {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphForeach")
            .field("body_steps", &self.body.steps().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphForeach {
    pub fn input(&self) -> &PreparedGraphSet {
        &self.input
    }
    pub fn collection(&self) -> &PreparedGraphSet {
        &self.collection
    }
    pub fn output(&self) -> Option<&PreparedGraphSet> {
        self.output.as_ref()
    }
    pub fn body(&self) -> &PreparedGraphWriteChain {
        &self.body
    }
    pub fn columns(&self) -> &[String] {
        self.output
            .as_ref()
            .map_or(&self.outer_columns, PreparedGraphSet::columns)
    }
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        self.output
            .as_ref()
            .map_or(&self.outer_types, PreparedGraphSet::column_types)
    }
    pub fn read_element_keys(&self) -> &[PropertyKeyId] {
        &self.read_keys
    }
    pub fn returns_rows(&self) -> bool {
        self.returning
    }
    pub fn requires_read(&self) -> bool {
        self.input.reads_graph()
            || self.input.reads_elements()
            || self.collection.reads_graph()
            || self.collection.reads_elements()
            || self
                .output
                .as_ref()
                .is_some_and(|output| output.reads_graph() || output.reads_elements())
    }
    /// Visit the loop's own source and tail. Hosts separately preflight every
    /// body step, even when the input collection will be empty.
    pub fn admit_patterns<E>(
        &self,
        mut visitor: impl FnMut(&PreparedGraphPattern<GraphValueRow>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.input.admit_patterns(&mut visitor)?;
        self.collection.admit_patterns(&mut visitor)?;
        if let Some(output) = &self.output {
            output.admit_patterns(&mut visitor)?;
        }
        Ok(())
    }
    pub fn execute_input_governed<E, C>(
        &self,
        input: Option<Vec<GraphValueRow>>,
        policy: GqlQueryPolicy,
        mut source: impl GraphSetSource<E, C>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.input.execute_governed_from_source(
            policy,
            WithOwnedRows {
                source: &mut source,
                rows: input,
            },
            checkpoint,
        )
    }
    pub fn execute_output_governed<E, C>(
        &self,
        input: Option<Vec<GraphValueRow>>,
        policy: GqlQueryPolicy,
        mut source: impl GraphSetSource<E, C>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.output
            .as_ref()
            .expect("the cursor emits only a prepared output tail")
            .execute_governed_from_source(
                policy,
                WithOwnedRows {
                    source: &mut source,
                    rows: input,
                },
                checkpoint,
            )
    }
    pub fn execute_collection_governed<E, C>(
        &self,
        input: Option<Vec<GraphValueRow>>,
        policy: GqlQueryPolicy,
        mut source: impl GraphSetSource<E, C>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.collection.execute_governed_from_source(
            policy,
            WithOwnedRows {
                source: &mut source,
                rows: input,
            },
            checkpoint,
        )
    }
}

/// One physical action. Every body write remains an ordinary native write;
/// only an outer terminal RETURN can have `delivers == true`.
#[derive(Debug)]
pub enum GraphWriteChainTask<'a> {
    Write {
        step: &'a GraphWriteChainStep,
        continues: bool,
        delivers: bool,
    },
    ForeachInput {
        foreach: &'a PreparedGraphForeach,
    },
    ForeachCollection {
        foreach: &'a PreparedGraphForeach,
    },
    ForeachOutput {
        foreach: &'a PreparedGraphForeach,
        delivers: bool,
    },
}

struct Iteration<'a> {
    foreach: &'a PreparedGraphForeach,
    incoming: std::vec::IntoIter<GraphValueRow>,
    outer: Option<GraphValueRow>,
    member: usize,
    body_at: usize,
    body_running: bool,
    collection_running: bool,
    restored: Vec<GraphValueRow>,
}

/// Scheduler for a checked chain inside the host's existing rollback guard.
/// Call `next` with the preceding task's output in `rows`. On exhaustion,
/// `rows` holds the final outer result, even when no body iteration executed.
pub struct GraphWriteChainCursor<'a> {
    chain: &'a PreparedGraphWriteChain,
    at: usize,
    deliver_final: bool,
    awaiting_input: Option<&'a PreparedGraphForeach>,
    iteration: Option<Iteration<'a>>,
}

impl<'a> GraphWriteChainCursor<'a> {
    pub fn new(chain: &'a PreparedGraphWriteChain, deliver_final: bool) -> Self {
        Self {
            chain,
            at: 0,
            deliver_final,
            awaiting_input: None,
            iteration: None,
        }
    }

    pub fn next<E: From<GraphIntegerError>>(
        &mut self,
        rows: &mut Option<Vec<GraphValueRow>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphWriteChainTask<'a>>, E> {
        use GlaExecutionEvent::{ScratchEntry, Work};
        let invalid = || GraphIntegerError {
            instruction: 0,
            kind: GraphIntegerErrorKind::IncompatibleOperands,
        };
        if let Some(foreach) = self.awaiting_input.take() {
            control(Work)?;
            self.iteration = Some(Iteration {
                foreach,
                incoming: rows.take().ok_or_else(invalid)?.into_iter(),
                outer: None,
                member: 0,
                body_at: 0,
                body_running: false,
                collection_running: false,
                restored: Vec::new(),
            });
        }
        loop {
            if let Some(iteration) = self.iteration.as_mut() {
                if iteration.collection_running {
                    control(Work)?;
                    let mut evaluated = rows.take().ok_or_else(invalid)?.into_iter();
                    let outer = evaluated.next().ok_or_else(invalid)?;
                    if evaluated.next().is_some()
                        || outer.len() != iteration.foreach.outer_types.len() + 1
                    {
                        return Err(invalid().into());
                    }
                    iteration.outer = Some(outer);
                    iteration.collection_running = false;
                }
                if iteration.body_running && iteration.body_at < iteration.foreach.body.steps.len()
                {
                    control(Work)?;
                    let step = &iteration.foreach.body.steps[iteration.body_at];
                    iteration.body_at += 1;
                    return Ok(Some(GraphWriteChainTask::Write {
                        step,
                        continues: iteration.body_at < iteration.foreach.body.steps.len(),
                        delivers: false,
                    }));
                }
                // Body output is local, even if the last native write emitted
                // zero or several occurrences. Its outer row remains intact.
                if iteration.body_running {
                    *rows = None;
                    iteration.body_running = false;
                }
                if iteration.outer.is_none() {
                    control(Work)?;
                    iteration.member = 0;
                    if let Some(outer) = iteration.incoming.next() {
                        if outer.len() != iteration.foreach.outer_types.len() {
                            return Err(invalid().into());
                        }
                        for (value, kind) in
                            outer.values().iter().zip(&iteration.foreach.outer_types)
                        {
                            control(Work)?;
                            if !kind.accepts(value) {
                                return Err(invalid().into());
                            }
                        }
                        control(ScratchEntry)?;
                        *rows = Some(vec![outer]);
                        iteration.collection_running = true;
                        return Ok(Some(GraphWriteChainTask::ForeachCollection {
                            foreach: iteration.foreach,
                        }));
                    }
                }
                if let Some(outer) = &iteration.outer {
                    control(Work)?;
                    let collection = outer.values().last().ok_or_else(invalid)?;
                    let length = match collection {
                        GraphValue::Scalar(fgdb_types::CanonicalScalar::Null) => 0,
                        GraphValue::List(values) => values.len(),
                        GraphValue::Vertices(values) => values.len(),
                        GraphValue::Edges(values) => values.len(),
                        _ => 1,
                    };
                    if iteration.member < length {
                        control(Work)?; // includes a body that later produces no effects
                        control(ScratchEntry)?;
                        let mut values = Vec::new();
                        for &column in &iteration.foreach.imports {
                            control(ScratchEntry)?;
                            values.push(
                                outer
                                    .get(column)
                                    .ok_or_else(invalid)?
                                    .copy_with_control(control)?,
                            );
                        }
                        control(ScratchEntry)?;
                        let value = match collection {
                            GraphValue::List(values) => {
                                values[iteration.member].copy_with_control(control)?
                            }
                            GraphValue::Vertices(values) => {
                                GraphValue::Vertex(values[iteration.member])
                            }
                            GraphValue::Edges(values) => GraphValue::Edge(values[iteration.member]),
                            value => value.copy_with_control(control)?,
                        };
                        values.push(value);
                        control(ScratchEntry)?;
                        *rows = Some(vec![GraphValueRow::from_owned_values(values)]);
                        iteration.member += 1;
                        iteration.body_at = 0;
                        iteration.body_running = true;
                        if iteration.foreach.body.steps.is_empty() {
                            return Err(invalid().into());
                        }
                        continue;
                    }
                    // Moving an admitted row needs no payload clone. Reserve
                    // its new occurrence slot before growing the output bag.
                    control(ScratchEntry)?;
                    iteration.restored.push(
                        iteration
                            .outer
                            .take()
                            .expect("outer present")
                            .into_prefix(iteration.foreach.outer_types.len()),
                    );
                    continue;
                }
                let iteration = self.iteration.take().expect("iteration present");
                *rows = Some(iteration.restored);
                if iteration.foreach.output.is_some() {
                    return Ok(Some(GraphWriteChainTask::ForeachOutput {
                        foreach: iteration.foreach,
                        delivers: self.deliver_final
                            && self.at == self.chain.steps.len()
                            && iteration.foreach.returns_rows(),
                    }));
                }
            }
            let Some(step) = self.chain.steps.get(self.at) else {
                return Ok(None);
            };
            self.at += 1;
            match step {
                GraphWriteChainStep::Foreach(foreach) => {
                    control(Work)?;
                    self.awaiting_input = Some(foreach);
                    return Ok(Some(GraphWriteChainTask::ForeachInput { foreach }));
                }
                step => {
                    return Ok(Some(GraphWriteChainTask::Write {
                        step,
                        continues: self.at < self.chain.steps.len(),
                        delivers: self.deliver_final
                            && self.at == self.chain.steps.len()
                            && self.chain.returns_rows(),
                    }));
                }
            }
        }
    }
}
