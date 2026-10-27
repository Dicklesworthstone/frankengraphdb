//! Fold order-preserving relational stages without retaining their output bag.
//!
//! Graph leaves and child sorting barriers keep the ordinary executor. Set
//! merges, UNWIND, Cartesian expansion, filters and order-preserving ALL
//! projections forward owned rows directly. Pages drain their input: LIMIT 0
//! cannot hide failures. Set inputs still sort/deduplicate in their own bags.
//! This is query-local physical execution, not a new logical algebra or spill.

use super::*;

mod cardinality;
mod repeated;

type Consumer<'a, S, Checkpoint, E, C> =
    dyn FnMut(GraphValueRow, &mut S, &mut Meter<Checkpoint>) -> SetResult<(), E, C> + 'a;

impl PreparedGraphSet {
    /// A terminal expansion can be folded even if its children need a sort.
    /// Never elide a projection's implicit canonicalization or explicit order.
    #[cfg(test)]
    pub(crate) fn has_foldable_expansion(&self) -> bool {
        self.has_foldable_expansion_for_source(true)
    }

    pub(crate) fn has_foldable_expansion_for_source(&self, procedure_rows: bool) -> bool {
        self.order.is_empty() && self.has_foldable_node_for_source(procedure_rows)
    }

    /// Ignore ONLY this node's tail when a caller owns its exact ordering/page.
    /// Every descendant must still retain its own sequence boundaries.
    pub(super) fn has_foldable_node(&self) -> bool {
        self.has_foldable_node_for_source(true)
    }

    pub(super) fn has_foldable_node_for_source(&self, procedure_rows: bool) -> bool {
        match &self.node {
            SetNode::ProcedureCall(call) => procedure_rows && canonical_vertex_call(call),
            SetNode::Unwind { .. } | SetNode::CrossJoin { .. } | SetNode::Binary { .. } => true,
            SetNode::Scope(input) | SetNode::Filter { input, .. } => {
                input.has_foldable_expansion_for_source(procedure_rows)
            }
            SetNode::Project {
                input,
                quantifier: GraphSetQuantifier::All,
                projection,
                ..
            } if input.preserves_row_order()
                || (procedure_rows
                    && input.has_canonical_vertex_prefix()
                    && projection
                        .first()
                        .is_some_and(|item| matches!(item.value(), GraphSetValue::Column(0)))) =>
            {
                input.has_foldable_expansion_for_source(procedure_rows)
            }
            _ => false,
        }
    }

    /// A leading unique vertex key proves tuple order independently of all
    /// later scalar cells. Preserve that key through ALL projections only;
    /// dropping/reordering it retains the ordinary canonical sorting barrier.
    fn has_canonical_vertex_prefix(&self) -> bool {
        if !self.order.is_empty() {
            return false;
        }
        match &self.node {
            SetNode::ProcedureCall(call) => canonical_vertex_call(call),
            SetNode::Scope(input) | SetNode::Filter { input, .. } => {
                input.has_canonical_vertex_prefix()
            }
            SetNode::Project {
                input,
                projection,
                quantifier: GraphSetQuantifier::All,
                ..
            } => {
                input.has_canonical_vertex_prefix()
                    && projection
                        .first()
                        .is_some_and(|item| matches!(item.value(), GraphSetValue::Column(0)))
            }
            _ => false,
        }
    }

    /// Internal terminal fold. The sink's state is tentative until success.
    /// Source admission and every scalable stage share one cumulative meter.
    /// No intermediate occurrence spends the public result-row allowance.
    /// A semantic sink failure is deferred while upstream validation completes;
    /// interruption and resource refusal stop immediately. No partial sink
    /// state is returned by this method on failure.
    pub(crate) fn fold_governed<E, C>(
        &self,
        policy: GqlQueryPolicy,
        mut source: impl GraphSetSource<E, C>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
        mut consume: impl FnMut(
            GraphValueRow,
            &mut dyn FnMut(GlaExecutionEvent) -> SetResult<(), E, C>,
        ) -> SetResult<(), E, C>,
    ) -> SetResult<(GqlExecutionStats, GlaExecutionStats), E, C> {
        let source: &mut dyn GraphSetSource<E, C> = &mut source;
        let checkpoint: &mut dyn FnMut() -> Result<(), C> = &mut checkpoint;
        let mut meter = Meter {
            policy,
            checkpoint,
            arguments: Vec::new(),
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
        };
        let mut operand = 0;
        visit(
            self,
            &mut *source,
            &mut meter,
            &mut operand,
            &mut |row, _, meter| consume(row, &mut |event| meter.event(event)),
        )?;
        meter.event(GlaExecutionEvent::Work)?;
        Ok((meter.rows, meter.evaluator))
    }
}

fn canonical_vertex_call(call: &PreparedProcedureCall) -> bool {
    call.namespace() == "fnx"
        && call
            .outputs()
            .first()
            .is_some_and(|name| name == "vertex" || name == "node")
}

/// Selection is downstream of a stage's own validation. Remembering a sink
/// error must NOT suppress a later error in this stage or one of its children.
struct Window<E> {
    skip: u64,
    remaining: Option<u64>,
    downstream: Option<GraphSetExecutionError<E>>,
}
impl<E> Window<E> {
    fn new(skip: u64, remaining: Option<u64>) -> Self {
        Self {
            skip,
            remaining,
            downstream: None,
        }
    }

    fn push<C, S: ?Sized, Checkpoint>(
        &mut self,
        row: GraphValueRow,
        source: &mut S,
        meter: &mut Meter<Checkpoint>,
        consume: &mut Consumer<'_, S, Checkpoint, E, C>,
    ) -> SetResult<(), E, C>
    where
        Checkpoint: FnMut() -> Result<(), C>,
    {
        meter.event(GlaExecutionEvent::Work)?;
        if self.skip != 0 {
            self.skip -= 1;
            return Ok(());
        }
        if self.remaining == Some(0) {
            return Ok(());
        }
        if let Some(remaining) = &mut self.remaining {
            *remaining -= 1;
        }
        if self.downstream.is_none() {
            remember(consume(row, source, meter), &mut self.downstream)?;
        }
        Ok(())
    }

    fn finish<C>(self) -> SetResult<(), E, C> {
        match self.downstream {
            Some(error) => Err(GqlQueryError::Source(error)),
            None => Ok(()),
        }
    }
}

fn remember<E, C>(
    result: SetResult<(), E, C>,
    deferred: &mut Option<GraphSetExecutionError<E>>,
) -> SetResult<(), E, C> {
    match result {
        Err(GqlQueryError::Source(error)) => {
            *deferred = Some(error);
            Ok(())
        }
        result => result,
    }
}

fn projected<E, C>(
    failure: projection::ProjectionFailure<GqlQueryError<GraphSetExecutionError<E>, C>>,
    row: usize,
) -> GqlQueryError<GraphSetExecutionError<E>, C> {
    match failure {
        projection::ProjectionFailure::Control(error) => error,
        projection::ProjectionFailure::Arithmetic { column, error } => {
            GqlQueryError::Source(GraphSetExecutionError::Projection { row, column, error })
        }
    }
}

pub(super) fn visit<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
    consume: &mut Consumer<'_, S, Checkpoint, E, C>,
) -> SetResult<(), E, C>
where
    S: GraphSetSource<E, C> + ?Sized,
    Checkpoint: FnMut() -> Result<(), C>,
{
    // Nonfoldable sorting/projection barriers keep their complete selected
    // output in its declared order. Binary sets use the same canonical merge
    // below, without retaining a second bag for the terminal consumer.
    if !query.has_foldable_expansion_for_source(source.supports_procedure_rows()) {
        let rows = run(query, source, meter, operand)?;
        let mut forward = Window::new(0, None);
        for row in rows {
            forward.push(row, source, meter, consume)?;
        }
        return forward.finish();
    }

    visit_expansion(
        query,
        source,
        meter,
        operand,
        Window::new(query.offset, query.count),
        consume,
    )
}

/// Feed this node's complete output into a separately owned ranked page. No
/// definition is cloned or mutated and no child sort/page is peeled away.
pub(super) fn visit_unwindowed<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
    consume: &mut Consumer<'_, S, Checkpoint, E, C>,
) -> SetResult<(), E, C>
where
    S: GraphSetSource<E, C> + ?Sized,
    Checkpoint: FnMut() -> Result<(), C>,
{
    debug_assert!(query.has_foldable_node_for_source(source.supports_procedure_rows()));
    visit_expansion(query, source, meter, operand, Window::new(0, None), consume)
}

fn visit_expansion<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
    mut window: Window<E>,
    consume: &mut Consumer<'_, S, Checkpoint, E, C>,
) -> SetResult<(), E, C>
where
    S: GraphSetSource<E, C> + ?Sized,
    Checkpoint: FnMut() -> Result<(), C>,
{
    meter.event(GlaExecutionEvent::Work)?;
    if !source.supports_procedure_rows()
        && matches!(&query.node, SetNode::Project { input, .. } if !input.preserves_row_order())
    {
        // An ordinary custom procedure host need not promise unique leading
        // vertices. Preserve its projection's canonical sort and duplicate
        // semantics; only the checked cursor capability proves the shortcut.
        for row in run_node(query, source, meter, operand)? {
            window.push(row, source, meter, consume)?;
        }
        return window.finish();
    }
    if let Some(inputs @ (left, right, code, projection)) = query.filtered_cross_inputs() {
        // The materialized and folded paths share the exact selected-pair walk.
        // Complete both original children first; even LIMIT 0 cannot hide a
        // late source or child-expression failure. Never retain the joined bag.
        let left = run(left, source, meter, operand)?;
        let right = match selected_cross::seeded::collect(&left, inputs, source, meter, operand)? {
            Some(rows) => rows,
            None => run(right, source, meter, operand)?,
        };
        let columns = selected_cross::columns(projection, &mut |event| meter.event(event))?;
        selected_cross::visit_with_context(
            &left,
            &right,
            code,
            columns.as_deref(),
            meter,
            |meter, event| meter.event(event),
            |left, right, meter| {
                let row =
                    selected_cross::copy_pair(left, right, columns.as_deref(), &mut |event| {
                        meter.event(event)
                    })?;
                // Keep the ordinary window and deferred downstream-error law.
                // The probe walk drains even after the output page is full.
                window.push(row, source, meter, consume)
            },
        )?;
        return window.finish();
    }
    let mut local = None;
    match &query.node {
        SetNode::ProcedureCall(call) => {
            let at = *operand;
            *operand += 1;
            let unit = GraphValueRow::unit();
            let mut arguments = Vec::with_capacity(call.arguments.len());
            for (column, argument) in call.arguments.iter().enumerate() {
                arguments.push(
                    projection::evaluate_value(argument, &unit, column, &mut |event| {
                        meter.event(event)
                    })
                    .map_err(|error| projected(error, 0))?,
                );
            }
            if source.supports_procedure_rows() {
                let execution = source
                    .procedure_rows(call, &arguments, meter.remaining())
                    .ok_or(GqlQueryError::Source(
                        GraphSetExecutionError::InvalidSourceStatistics { operand: at },
                    ))?;
                let mut execution = execution.map_err(|error| meter.source_error(error))?;
                let count = execution.rows.result_rows;
                // Complete source admission is charged before the first
                // reducer/comparator/property read, never after callbacks.
                meter.absorb_statistics(execution.rows, execution.evaluator, Some(count), at)?;
                let mut consumed = 0_u64;
                let mut previous = None;
                while let Some(row) = execution
                    .cursor
                    .next_row(&mut meter.checkpoint)
                    .map_err(|error| meter.source_error(error))?
                {
                    check_source_rows(core::slice::from_ref(&row), &query.types, at, meter)?;
                    let Some(GraphValue::Vertex(vertex)) = row.values().first() else {
                        return Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
                            operand: at,
                        }));
                    };
                    if previous.is_some_and(|prior| prior >= *vertex) || consumed == count {
                        return Err(GqlQueryError::Source(
                            GraphSetExecutionError::InvalidSourceStatistics { operand: at },
                        ));
                    }
                    previous = Some(*vertex);
                    consumed += 1;
                    window.push(row, source, meter, consume)?;
                }
                if consumed != count {
                    return Err(GqlQueryError::Source(
                        GraphSetExecutionError::InvalidSourceStatistics { operand: at },
                    ));
                }
            } else {
                // Unsupported hosts retain their source implementation and
                // canonical sorting. Arguments and the source execute once.
                let execution = source
                    .procedure(call, &arguments, meter.remaining())
                    .ok_or(GqlQueryError::Source(
                        GraphSetExecutionError::ProcedureUnavailable { operand: at },
                    ))?
                    .map_err(|error| meter.source_error(error))?;
                for row in admit_source_rows(query, execution, at, meter)? {
                    window.push(row, source, meter, consume)?;
                }
            }
        }
        SetNode::Binary {
            operation,
            quantifier,
            left,
            right,
        } => {
            // Complete BOTH original children before forwarding any row,
            // including beneath LIMIT 0. Child pages, source error order,
            // schema admission and the shared allowance remain unchanged.
            let left = run(left, source, meter, operand)?;
            let right = run(right, source, meter, operand)?;
            let mut merge = merge::SortedMerge::new(
                left,
                right,
                *operation,
                *quantifier,
                &mut |event| meter.event(event),
                &mut |a, b, control| compare_rows(a, b, &[], control),
            )?;
            loop {
                let row = merge
                    .next_with_control(&mut |event| meter.event(event), &mut |a, b, control| {
                        compare_rows(a, b, &[], control)
                    })?;
                let Some(row) = row else {
                    break;
                };
                // Move the occurrence directly into the ordinary window/sink.
                // A full page or deferred semantic sink failure still drains
                // the merge; interruption/resource refusal stops immediately.
                window.push(row, source, meter, consume)?;
            }
        }
        SetNode::CrossJoin { left, right } => {
            // Both children execute once, left-to-right, even if either is
            // empty. Their own sort/page and source-failure order are intact.
            let left = run(left, source, meter, operand)?;
            let right = run(right, source, meter, operand)?;
            for a in &left {
                for b in &right {
                    let row = copy_cross_pair(a, b, &mut |event| meter.event(event))?;
                    window.push(row, source, meter, consume)?;
                }
            }
        }
        SetNode::Unwind { input, value } => {
            let mut row_at = 0_usize;
            visit(input, source, meter, operand, &mut |row, source, meter| {
                if local.is_some() {
                    return Ok(());
                }
                let result = (|| {
                    // The same host reader as Project, so an UNWIND list can
                    // read carried elements' properties. It is released before
                    // the window takes the source back.
                    let mut source_elements = projection::SourceElements::new(source);
                    let mut read = projection::MappedElements {
                        read: &mut source_elements,
                        error: |error| GqlQueryError::Source(GraphSetExecutionError::Source(error)),
                    };
                    let mut rows = projection::UnwindRows::new(
                        row,
                        value,
                        &mut |event| meter.event(event),
                        &mut read,
                    )
                    .map_err(|error| projected(error, row_at))?;
                    while let Some((row, next)) =
                        rows.next_with_control(&mut |event| meter.event(event))?
                    {
                        rows = next;
                        window.push(row, source, meter, consume)?;
                    }
                    Ok(())
                })();
                remember(result, &mut local)?;
                row_at = row_at.checked_add(1).ok_or_else(|| {
                    GqlQueryError::Source(GraphSetExecutionError::AccountingOverflow {
                        dimension: GqlBudgetDimension::ResultRows,
                    })
                })?;
                Ok(())
            })?;
        }
        SetNode::Scope(input) => {
            visit(input, source, meter, operand, &mut |row, source, meter| {
                window.push(row, source, meter, consume)
            })?;
        }
        SetNode::Filter { input, predicate } => {
            visit(input, source, meter, operand, &mut |row, source, meter| {
                meter.event(GlaExecutionEvent::Work)?;
                if predicate.evaluate(&row, &mut |event| meter.event(event))? {
                    window.push(row, source, meter, consume)?;
                }
                Ok(())
            })?;
        }
        SetNode::Project {
            input, projection, ..
        } => {
            // Admission proves ALL and order preservation. Implicit canonical
            // projection sorting is never bypassed just because the sink sums.
            let mut row_at = 0_usize;
            visit(input, source, meter, operand, &mut |row, source, meter| {
                if local.is_some() {
                    return Ok(());
                }
                meter.event(GlaExecutionEvent::Work)?;
                // Numbered like project_rows, so rand() draws the same here.
                // Carry the same host through the fold so aliases keep its
                // saved snapshot, masking and element-read error semantics.
                let mut source_elements = projection::SourceElements::new(source);
                let mut read = projection::MappedElements {
                    read: &mut source_elements,
                    error: |error| GqlQueryError::Source(GraphSetExecutionError::Source(error)),
                };
                let result = projection::evaluate(
                    &row,
                    projection,
                    Some(row_at as u64),
                    &mut |event| meter.event(event),
                    &mut read,
                )
                .map_err(|error| projected(error, row_at))
                .and_then(|row| window.push(row, source, meter, consume));
                remember(result, &mut local)?;
                row_at = row_at.checked_add(1).ok_or_else(|| {
                    GqlQueryError::Source(GraphSetExecutionError::AccountingOverflow {
                        dimension: GqlBudgetDimension::ResultRows,
                    })
                })?;
                Ok(())
            })?;
        }
        _ => unreachable!("fold admission is the closed terminal operator profile"),
    }
    if let Some(error) = local {
        return Err(GqlQueryError::Source(error));
    }
    window.finish()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod selected_tests;

#[cfg(test)]
mod set_tests;
