//! Native scoped writes share one scheduler, transaction and execution meter.
//! FOREACH invokes its body per member; unit CALL invokes it per outer row.

use super::{
    GraphWriteChainStep, PreparedGraphUnitCall, PreparedGraphWriteChain,
    PreparedGraphWriteChainText,
};
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
    /// A scoped input, collection expression or outer continuation. All
    /// execute against the host's current staged graph and owned input bag.
    Read(GraphWriteChainRead<'a>),
}

/// One relational phase of a native scoped write. Only the final outer
/// continuation may spend the public delivered-row allowance.
pub struct GraphWriteChainRead<'a> {
    plan: &'a PreparedGraphSet,
    phase: GraphWriteChainReadPhase,
    delivers: bool,
}

/// The read's role in the native scoped scheduler. Collection phases occur
/// only for FOREACH; a unit CALL proceeds directly from input to its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphWriteChainReadPhase {
    Input,
    Collection,
    Output,
}

impl core::fmt::Debug for GraphWriteChainRead<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphWriteChainRead")
            .field("phase", &self.phase)
            .field("delivers", &self.delivers)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl GraphWriteChainRead<'_> {
    pub fn phase(&self) -> GraphWriteChainReadPhase {
        self.phase
    }

    pub fn delivers_rows(&self) -> bool {
        self.delivers
    }

    pub fn execute_governed<E, C>(
        &self,
        input: Option<Vec<GraphValueRow>>,
        policy: GqlQueryPolicy,
        mut source: impl GraphSetSource<E, C>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.plan.execute_governed_from_source(
            policy,
            WithOwnedRows {
                source: &mut source,
                rows: input,
            },
            checkpoint,
        )
    }
}

#[derive(Clone, Copy)]
enum ScopedWrite<'a> {
    Foreach(&'a PreparedGraphForeach),
    UnitCall(&'a PreparedGraphUnitCall),
}

impl<'a> ScopedWrite<'a> {
    fn from_step(step: &'a GraphWriteChainStep) -> Option<Self> {
        match step {
            GraphWriteChainStep::Foreach(foreach) => Some(Self::Foreach(foreach)),
            GraphWriteChainStep::UnitCall(call) => Some(Self::UnitCall(call)),
            _ => None,
        }
    }

    fn input(self) -> &'a PreparedGraphSet {
        match self {
            Self::Foreach(foreach) => &foreach.input,
            Self::UnitCall(call) => &call.input,
        }
    }

    fn output(self) -> Option<&'a PreparedGraphSet> {
        match self {
            Self::Foreach(foreach) => foreach.output.as_ref(),
            Self::UnitCall(call) => call.output.as_ref(),
        }
    }

    fn body(self) -> &'a PreparedGraphWriteChain {
        match self {
            Self::Foreach(foreach) => &foreach.body,
            Self::UnitCall(call) => &call.body,
        }
    }

    fn outer_types(self) -> &'a [GraphSetColumnType] {
        match self {
            Self::Foreach(foreach) => &foreach.outer_types,
            Self::UnitCall(call) => &call.outer_types,
        }
    }

    fn imports(self) -> &'a [usize] {
        match self {
            Self::Foreach(foreach) => &foreach.imports,
            Self::UnitCall(call) => &call.imports,
        }
    }

    fn returns_rows(self) -> bool {
        match self {
            Self::Foreach(foreach) => foreach.returning,
            Self::UnitCall(call) => call.returning,
        }
    }
}

struct Iteration<'a> {
    scope: ScopedWrite<'a>,
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
    awaiting_input: Option<ScopedWrite<'a>>,
    iterations: Vec<Iteration<'a>>,
}

impl<'a> GraphWriteChainCursor<'a> {
    pub fn new(chain: &'a PreparedGraphWriteChain, deliver_final: bool) -> Self {
        Self {
            chain,
            at: 0,
            deliver_final,
            awaiting_input: None,
            iterations: Vec::new(),
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
        if let Some(scope) = self.awaiting_input.take() {
            control(Work)?;
            if self.iterations.len() >= crate::MAX_GRAPH_SET_DEPTH {
                return Err(invalid().into());
            }
            // Reserve the live frame before moving its input or growing the
            // stack. A child shares the host's cumulative scratch allowance.
            control(ScratchEntry)?;
            self.iterations.push(Iteration {
                scope,
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
            let depth = self.iterations.len();
            if let Some(iteration) = self.iterations.last_mut() {
                if iteration.collection_running {
                    control(Work)?;
                    let mut evaluated = rows.take().ok_or_else(invalid)?.into_iter();
                    let outer = evaluated.next().ok_or_else(invalid)?;
                    if evaluated.next().is_some()
                        || outer.len() != iteration.scope.outer_types().len() + 1
                    {
                        return Err(invalid().into());
                    }
                    iteration.outer = Some(outer);
                    iteration.collection_running = false;
                }
                if iteration.body_running && iteration.body_at < iteration.scope.body().steps.len()
                {
                    control(Work)?;
                    let step = &iteration.scope.body().steps[iteration.body_at];
                    iteration.body_at += 1;
                    if let Some(scope) = ScopedWrite::from_step(step) {
                        if depth >= crate::MAX_GRAPH_SET_DEPTH {
                            return Err(invalid().into());
                        }
                        // The parent's next step already follows this child.
                        // Its row, member and restored bag stay on the stack
                        // while the child uses the same native host phases.
                        self.awaiting_input = Some(scope);
                        return Ok(Some(GraphWriteChainTask::Read(GraphWriteChainRead {
                            plan: scope.input(),
                            phase: GraphWriteChainReadPhase::Input,
                            delivers: false,
                        })));
                    }
                    return Ok(Some(GraphWriteChainTask::Write {
                        step,
                        continues: iteration.body_at < iteration.scope.body().steps.len(),
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
                        if outer.len() != iteration.scope.outer_types().len() {
                            return Err(invalid().into());
                        }
                        for (value, kind) in
                            outer.values().iter().zip(iteration.scope.outer_types())
                        {
                            control(Work)?;
                            if !kind.accepts(value) {
                                return Err(invalid().into());
                            }
                        }
                        match iteration.scope {
                            ScopedWrite::Foreach(foreach) => {
                                control(ScratchEntry)?;
                                *rows = Some(vec![outer]);
                                iteration.collection_running = true;
                                return Ok(Some(GraphWriteChainTask::Read(GraphWriteChainRead {
                                    plan: &foreach.collection,
                                    phase: GraphWriteChainReadPhase::Collection,
                                    delivers: false,
                                })));
                            }
                            ScopedWrite::UnitCall(_) => {
                                iteration.outer = Some(outer);
                            }
                        }
                    }
                }
                if let Some(outer) = &iteration.outer {
                    control(Work)?;
                    // Unit CALL has one invocation and no collection or loop
                    // member. Its imported row is the native body input.
                    let collection = match iteration.scope {
                        ScopedWrite::Foreach(_) => Some(outer.values().last().ok_or_else(invalid)?),
                        ScopedWrite::UnitCall(_) => None,
                    };
                    let length = match collection {
                        Some(GraphValue::Scalar(fgdb_types::CanonicalScalar::Null)) => 0,
                        Some(GraphValue::List(values)) => values.len(),
                        Some(GraphValue::Vertices(values)) => values.len(),
                        Some(GraphValue::Edges(values)) => values.len(),
                        _ => 1,
                    };
                    if iteration.member < length {
                        control(Work)?; // includes a body that later produces no effects
                        control(ScratchEntry)?;
                        let mut values = Vec::new();
                        for &column in iteration.scope.imports() {
                            control(ScratchEntry)?;
                            values.push(
                                outer
                                    .get(column)
                                    .ok_or_else(invalid)?
                                    .copy_with_control(control)?,
                            );
                        }
                        if let Some(collection) = collection {
                            control(ScratchEntry)?;
                            let value = match collection {
                                GraphValue::List(values) => {
                                    values[iteration.member].copy_with_control(control)?
                                }
                                GraphValue::Vertices(values) => {
                                    GraphValue::Vertex(values[iteration.member])
                                }
                                GraphValue::Edges(values) => {
                                    GraphValue::Edge(values[iteration.member])
                                }
                                value => value.copy_with_control(control)?,
                            };
                            values.push(value);
                        }
                        control(ScratchEntry)?;
                        *rows = Some(vec![GraphValueRow::from_owned_values(values)]);
                        iteration.member += 1;
                        iteration.body_at = 0;
                        iteration.body_running = true;
                        if iteration.scope.body().steps.is_empty() {
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
                            .into_prefix(iteration.scope.outer_types().len()),
                    );
                    continue;
                }
                let iteration = self.iterations.pop().expect("iteration present");
                *rows = Some(iteration.restored);
                if let Some(output) = iteration.scope.output() {
                    return Ok(Some(GraphWriteChainTask::Read(GraphWriteChainRead {
                        plan: output,
                        phase: GraphWriteChainReadPhase::Output,
                        delivers: self.deliver_final
                            && self.iterations.is_empty()
                            && self.at == self.chain.steps.len()
                            && iteration.scope.returns_rows(),
                    })));
                }
                // A completed child restores the parent's current local row
                // bag. Resume that body before advancing the root statement.
                continue;
            }
            let Some(step) = self.chain.steps.get(self.at) else {
                return Ok(None);
            };
            self.at += 1;
            if let Some(scope) = ScopedWrite::from_step(step) {
                control(Work)?;
                self.awaiting_input = Some(scope);
                return Ok(Some(GraphWriteChainTask::Read(GraphWriteChainRead {
                    plan: scope.input(),
                    phase: GraphWriteChainReadPhase::Input,
                    delivers: false,
                })));
            }
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
