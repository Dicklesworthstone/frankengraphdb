//! Compile bound set expressions into the existing dependency-ordered registry.
//! A private append suffix is protected by one guard; no handle, callback that
//! can borrow the database, await, or commit can expose a partially built circuit.
//! Rebuild prepares a complete replacement suffix before swapping any old node.

use super::*;
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_gql::row_join::RowJoinKind;
use fgdb_gql::row_projection::RowProjectionSpec;
use fgdb_gql::row_window::RowWindowSpec;
use fgdb_gql::{GraphSetOperation, GraphSetQuantifier, PreparedGraphSet};
use fgdb_gql::PreparedGraphSetAggregate;

struct Staging<'a, V: Vfs + Clone> {
    database: &'a mut Database<V>,
    first: usize,
    accepted: bool,
}
impl<'a, V: Vfs + Clone> Staging<'a, V> {
    fn new(database: &'a mut Database<V>) -> Self {
        Self {
            first: database.standing_queries.len(),
            database,
            accepted: false,
        }
    }
    fn append(&mut self, query: StandingQuery) -> usize {
        self.database.store_standing_query(query).index
    }

    // Internal consumers need the selected bag; native delivery also needs
    // inherited rank after scopes/filters. Restore that rank with an ordinary
    // compressed window only when a finite occurrence bound is proved.
    fn compile_root(
        &mut self,
        cx: &QueryCx,
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
        checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
    ) -> Result<usize, StandingQueryError> {
        checkpoint()?;
        // A source-free root has an exact, immutable sequence. Freeze the
        // complete subtree before any rank/page decomposition; its positional
        // selection never needs a dynamic canonical-order approximation.
        if query.operand_count() == 0 {
            return self.compile(cx, query, policy, checkpoint);
        }
        // Keep the existing refusal for an implicitly ordered positional root
        // mixed with pages. Explicit/proved ordering can safely consume those
        // bags; every internal page must independently prove its own rank.
        if query.incremental_result_order().is_none()
            && !query.incremental_window_sequence_compatible()
        {
            return Err(StandingQueryError::Unsupported);
        }
        let index = self.compile(cx, query, policy, checkpoint)?;
        let Some((order, Some(bound))) = query.incremental_result_order() else {
            return Ok(index);
        };
        if order.is_empty()
            || matches!(
                &self.database.standing_queries[index],
                StandingQuery::Window(_)
            )
        {
            return Ok(index);
        }
        let spec = RowWindowSpec::new(
            query.column_types().to_vec(),
            order.to_vec(),
            GraphSetQuantifier::All,
            0,
            bound,
        )
        .map_err(StandingQueryError::WindowSchema)?;
        let query = self.database.prepare_standing_window(
            cx,
            index,
            spec,
            policy,
            self.database.standing_queries.len(),
        )?;
        let index = self.append(StandingQuery::Window(Box::new(query)));
        checkpoint()?;
        Ok(index)
    }
    fn compile(
        &mut self,
        cx: &QueryCx,
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
        checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
    ) -> Result<usize, StandingQueryError> {
        checkpoint()?;
        let index = if query.operand_count() == 0 {
            let state = self.database.prepare_standing_constant(cx, query.clone(), policy)?;
            self.append(StandingQuery::Constant(Box::new(state)))
        } else if let Some((input, order, offset, count)) = query.incremental_ordered_window()
        {
            // Peel just this scope. Never move a page across DISTINCT, a
            // filter, or an expression, including when the page is empty.
            let spec = RowWindowSpec::new(
                input.column_types().to_vec(),
                order.to_vec(),
                GraphSetQuantifier::All,
                offset,
                count,
            )
            .map_err(StandingQueryError::WindowSchema)?;
            let input = self.compile(cx, &input, policy, checkpoint)?;
            let state = self.database.prepare_standing_window(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Window(Box::new(state)))
        } else if let Some((input, spec)) = query
            .incremental_window()
            .map_err(StandingQueryError::WindowSchema)?
        {
            let input = self.compile(cx, &input, policy, checkpoint)?;
            let state = self.database.prepare_standing_window(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Window(Box::new(state)))
        } else if let Some(pattern) = query.incremental_pattern() {
            let query = self
                .database
                .prepare_registered_rows(cx, pattern.clone(), policy)?;
            self.append(query)
        } else if let Some(input) = query.incremental_scope() {
            return self.compile(cx, input, policy, checkpoint);
        } else if let Some((input, projection, quantifier)) = query.incremental_projection() {
            let spec = RowProjectionSpec::new(
                input.column_types().to_vec(),
                projection.to_vec(),
                quantifier,
            )
            .map_err(StandingQueryError::ProjectionSchema)?;
            let input = self.compile(cx, input, policy, checkpoint)?;
            let query = self.database.prepare_standing_projection(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Projection(Box::new(query)))
        } else if let Some((input, predicate)) = query.incremental_filter() {
            // Keep the complete child scope upstream, including DISTINCT and
            // source paging. Identity projection preserves all native cells.
            let spec = RowProjectionSpec::selection(
                input.column_types().to_vec(),
                input.columns().to_vec(),
                predicate,
            )
            .map_err(StandingQueryError::ProjectionSchema)?;
            let input = self.compile(cx, input, policy, checkpoint)?;
            let query = self.database.prepare_standing_projection(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Projection(Box::new(query)))
        } else if let Some((input, name, value)) = query.incremental_unwind() {
            let spec = RowProjectionSpec::unwind(
                input.column_types().to_vec(),
                input.columns().to_vec(),
                name.to_owned(),
                value.clone(),
            )
            .map_err(StandingQueryError::ProjectionSchema)?;
            let input = self.compile(cx, input, policy, checkpoint)?;
            let query = self.database.prepare_standing_projection(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Projection(Box::new(query)))
        } else if let Some((left, right)) = query.incremental_cross_join() {
            // Never short-circuit the other operand when one result is empty:
            // its schema, definition and failures are still part of the query.
            let left = self.compile(cx, left, policy, checkpoint)?;
            let right = self.compile(cx, right, policy, checkpoint)?;
            let query = self.database.prepare_standing_join(
                cx,
                [left, right],
                &[],
                RowJoinKind::Inner,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Join(Box::new(query)))
        } else if let Some((operation, quantifier, left, right)) = query.incremental_binary() {
            let left = self.compile(cx, left, policy, checkpoint)?;
            let right = self.compile(cx, right, policy, checkpoint)?;
            let query = self.database.prepare_standing_set(
                cx,
                [left, right],
                operation_of(operation, quantifier),
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Set(Box::new(query)))
        } else {
            return Err(StandingQueryError::Unsupported);
        };
        checkpoint()?;
        Ok(index)
    }
}
impl<V: Vfs + Clone> Drop for Staging<'_, V> {
    fn drop(&mut self) {
        if !self.accepted {
            // Private append suffix only. Existing entries and their indexes
            // are unchanged on every recoverable preparation failure/unwind.
            self.database.standing_queries.truncate(self.first);
        }
    }
}
fn operation_of(operation: GraphSetOperation, quantifier: GraphSetQuantifier) -> SetOperation {
    match (operation, quantifier) {
        (GraphSetOperation::Union, GraphSetQuantifier::All) => SetOperation::UnionAll,
        (GraphSetOperation::Union, GraphSetQuantifier::Distinct) => SetOperation::UnionDistinct,
        (GraphSetOperation::Intersect, GraphSetQuantifier::All) => SetOperation::IntersectAll,
        (GraphSetOperation::Intersect, GraphSetQuantifier::Distinct) => {
            SetOperation::IntersectDistinct
        }
        (GraphSetOperation::Except, GraphSetQuantifier::All) => SetOperation::ExceptAll,
        (GraphSetOperation::Except, GraphSetQuantifier::Distinct) => SetOperation::ExceptDistinct,
    }
}

pub(super) fn register<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
) -> Result<StandingQueryHandle, StandingQueryError> {
    let mut checkpoint = || cx.checkpoint().map_err(StandingQueryError::Interrupted);
    register_checked(database, cx, query, policy, &mut checkpoint)
}
fn register_checked<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
) -> Result<StandingQueryHandle, StandingQueryError> {
    let mut staged = Staging::new(database);
    let index = staged.compile_root(cx, query, policy, checkpoint)?;
    let layout = Arc::new(Layout::Circuit {
        columns: query.columns().to_vec(),
        first: staged.first,
    });
    let handle = StandingQueryHandle {
        owner: Arc::clone(&staged.database.handle_owner),
        index,
        native: Some(layout),
    };
    checkpoint()?;
    staged.accepted = true;
    Ok(handle)
}

/// One complete row circuit followed by one native aggregate. The immutable
/// aggregate owner retains its real relational input throughout admission.
/// No first-leaf conversion or fallible work occurs after acceptance.
pub(in crate::standing_query) fn register_group<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    definition: &PreparedGraphSetAggregate,
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    policy: GqlQueryPolicy,
) -> Result<StandingQueryHandle, StandingQueryError> {
    let mut checkpoint = || cx.checkpoint().map_err(StandingQueryError::Interrupted);
    register_group_checked(database, cx, definition, columns, slots, policy, &mut checkpoint)
}

fn register_group_checked<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    definition: &PreparedGraphSetAggregate,
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
) -> Result<StandingQueryHandle, StandingQueryError> {
    checkpoint()?;
    if columns.len() != slots.len() || columns.len() > fgdb_gql::algebra::MAX_PATTERN_VERTICES
        || slots.iter().any(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(at) => at >= definition.key_columns().len(),
            GraphAggregateTextSlot::Aggregate(at) => at >= definition.aggregate_columns().len(),
        })
    {
        return Err(StandingQueryError::Unsupported);
    }
    let mut staged = Staging::new(database);
    // The nine admitted aggregates consume a bag, not an inherited order trace.
    // Every input-local page must still be admitted by the recursive compiler.
    let input = staged.compile(cx, definition.input(), group::input_policy(policy), checkpoint)?;
    checkpoint()?;
    let state = staged.database.prepare_standing_group(cx, input, definition.clone(), policy,
        staged.database.standing_queries.len())?;
    let index = staged.append(StandingQuery::Group(Box::new(state)));
    let layout = Arc::new(Layout::GroupCircuit {
        columns: columns.to_vec(), slots: slots.to_vec(), first: staged.first,
    });
    let handle = StandingQueryHandle {
        owner: Arc::clone(&staged.database.handle_owner), index, native: Some(layout),
    };
    checkpoint()?;
    staged.accepted = true;
    Ok(handle)
}

/// Called only after ordinary owner/health admission. One native handle owns a
/// contiguous, topologically ordered circuit. Original definitions are used;
/// no text, parameters, resolver or hidden handle is needed to repair it.
/// Per-node preparation uses the ordinary source, set and projection engines.
pub(in crate::standing_query) fn rebuild<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    first: usize,
    root: usize,
    policy: GqlQueryPolicy,
) -> Result<CommitSeq, StandingQueryError> {
    let mut checkpoint = || cx.checkpoint().map_err(StandingQueryError::Interrupted);
    rebuild_checked(database, cx, first, root, policy, &mut checkpoint)
}
fn rebuild_checked<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    first: usize,
    root: usize,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
) -> Result<CommitSeq, StandingQueryError> {
    if first > root || root >= database.standing_queries.len() {
        return Err(StandingQueryError::UnknownHandle);
    }
    // Only the terminal group owns the public result quota. Rebuilding its
    // private input must preserve the same policy partition as registration.
    let input_policy = if matches!(&database.standing_queries[root], StandingQuery::Group(_)) {
        group::input_policy(policy)
    } else {
        policy
    };
    let mut staged = Staging::new(database);
    let at = staged.database.snapshot.frontier;
    for old in first..=root {
        checkpoint()?;
        let policy = if old == root { policy } else { input_policy };
        let replacement = match &staged.database.standing_queries[old] {
            StandingQuery::Constant(query) => StandingQuery::Constant(Box::new(
                staged.database.prepare_standing_constant(cx, query.definition.clone(), policy)?,
            )),
            StandingQuery::Rows { output, .. } => {
                staged
                    .database
                    .prepare_registered_rows(cx, output.definition().clone(), policy)?
            }
            StandingQuery::Set(query) => {
                let mut inputs = [0; 2];
                for (next, input) in inputs.iter_mut().zip(query.inputs) {
                    if input < first || input >= old {
                        return Err(StandingQueryError::Unsupported);
                    }
                    *next = staged
                        .first
                        .checked_add(input - first)
                        .ok_or(StandingQueryError::Unsupported)?;
                }
                StandingQuery::Set(Box::new(staged.database.prepare_standing_set(
                    cx,
                    inputs,
                    query.operation(),
                    policy,
                    staged.database.standing_queries.len(),
                )?))
            }
            StandingQuery::Join(query) => {
                let mut inputs = [0; 2];
                for (next, input) in inputs.iter_mut().zip(query.inputs) {
                    if input < first || input >= old {
                        return Err(StandingQueryError::Unsupported);
                    }
                    *next = staged
                        .first
                        .checked_add(input - first)
                        .ok_or(StandingQueryError::Unsupported)?;
                }
                StandingQuery::Join(Box::new(staged.database.prepare_standing_join(
                    cx,
                    inputs,
                    query.spec().keys(),
                    query.spec().kind(),
                    policy,
                    staged.database.standing_queries.len(),
                )?))
            }
            StandingQuery::Projection(query) => {
                if query.input < first || query.input >= old {
                    return Err(StandingQueryError::Unsupported);
                }
                let input = staged
                    .first
                    .checked_add(query.input - first)
                    .ok_or(StandingQueryError::Unsupported)?;
                StandingQuery::Projection(Box::new(staged.database.prepare_standing_projection(
                    cx,
                    input,
                    query.spec().clone(),
                    policy,
                    staged.database.standing_queries.len(),
                )?))
            }
            StandingQuery::Window(query) => {
                if query.input < first || query.input >= old {
                    return Err(StandingQueryError::Unsupported);
                }
                let input = staged
                    .first
                    .checked_add(query.input - first)
                    .ok_or(StandingQueryError::Unsupported)?;
                StandingQuery::Window(Box::new(staged.database.prepare_standing_window(
                    cx,
                    input,
                    query.spec().clone(),
                    policy,
                    staged.database.standing_queries.len(),
                )?))
            }
            StandingQuery::Group(query) => {
                if query.input < first || query.input >= old {
                    return Err(StandingQueryError::Unsupported);
                }
                let input = staged.first.checked_add(query.input - first)
                    .ok_or(StandingQueryError::Unsupported)?;
                StandingQuery::Group(Box::new(staged.database.prepare_standing_group(
                    cx, input, query.definition().clone(), policy, staged.database.standing_queries.len(),
                )?))
            }
            _ => return Err(StandingQueryError::Unsupported),
        };
        staged.append(replacement);
        checkpoint()?;
    }
    // Rebase private dependency indexes before any old state is replaced.
    // The original circuit is never altered on arithmetic/admission refusal.
    for query in &mut staged.database.standing_queries[staged.first..] {
        checkpoint()?;
        if let StandingQuery::Set(query) = query {
            for input in &mut query.inputs {
                *input = input
                    .checked_sub(staged.first)
                    .and_then(|offset| first.checked_add(offset))
                    .ok_or(StandingQueryError::Unsupported)?;
            }
        } else if let StandingQuery::Join(query) = query {
            for input in &mut query.inputs {
                *input = input
                    .checked_sub(staged.first)
                    .and_then(|offset| first.checked_add(offset))
                    .ok_or(StandingQueryError::Unsupported)?;
            }
        } else if let StandingQuery::Projection(query) = query {
            query.input = query
                .input
                .checked_sub(staged.first)
                .and_then(|offset| first.checked_add(offset))
                .ok_or(StandingQueryError::Unsupported)?;
        } else if let StandingQuery::Window(query) = query {
            query.input = query
                .input
                .checked_sub(staged.first)
                .and_then(|offset| first.checked_add(offset))
                .ok_or(StandingQueryError::Unsupported)?;
        } else if let StandingQuery::Group(query) = query {
            query.input = query.input.checked_sub(staged.first)
                .and_then(|offset| first.checked_add(offset))
                .ok_or(StandingQueryError::Unsupported)?;
        }
    }
    checkpoint()?;
    // No callbacks/fallible work between node swaps. Old states move to the
    // private suffix and are released by Drop; public indexes stay unchanged.
    for offset in 0..=root - first {
        staged
            .database
            .standing_queries
            .swap(first + offset, staged.first + offset);
    }
    Ok(at)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod projection_tests;

#[cfg(test)]
mod filter_tests;

#[cfg(test)]
mod cross_tests;

#[cfg(test)]
mod unwind_tests;

#[cfg(test)]
mod window_tests;

#[cfg(test)]
mod nested_window_tests;

#[cfg(test)]
mod ranked_pipeline_tests;

#[cfg(test)]
mod group_tests;

#[cfg(test)]
mod constant_tests;
