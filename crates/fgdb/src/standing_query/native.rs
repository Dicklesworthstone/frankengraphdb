//! Native preparation and lossless delivery over the existing standing engines.
//! No parser, evaluator, graph source or per-commit re-execution lives here.

use super::*;
use crate::{PreparedNativeRead, QueryError, QueryResult, QueryValue};
use fgdb_delta_types::ZWeight;
use fgdb_gql::{GqlParameters, GraphAggregateTextSlot, GraphSymbolResolver};

pub(super) mod set;

pub(super) enum Layout {
    Rows {
        columns: Vec<String>,
    },
    Aggregate {
        columns: Vec<String>,
        slots: Vec<GraphAggregateTextSlot>,
    },
    Circuit {
        columns: Vec<String>,
        first: usize,
    },
}
impl Layout {
    fn columns(&self) -> &[String] {
        match self {
            Self::Rows { columns }
            | Self::Aggregate { columns, .. }
            | Self::Circuit { columns, .. } => columns,
        }
    }
}
impl core::fmt::Debug for Layout {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeStandingLayout")
            .field("columns", &self.columns().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
fn prepare_error(error: QueryError) -> StandingQueryError {
    StandingQueryError::NativePrepare(Box::new(error))
}

impl PreparedNativeRead {
    /// Bind once into a database-owned maintained query. Later writes use the
    /// existing derivative engines, never this template or an eager fallback.
    /// Reusing this template with another argument map creates an independent
    /// registration. Dropping/changing the text, resolver, template or parameters
    /// cannot change an accepted definition. Handles remain session-local.
    ///
    /// Ordinary patterns, aggregates, WITH-aggregate pipelines and unadorned
    /// binary set/product circuits with projections, filters and UNWIND are admitted
    /// only where the existing standing engines support their bound operators.
    /// Historical selectors refuse: a fixed historical answer is not a current
    /// maintained view. There is no new SUBSCRIBE grammar or durable delivery.
    /// Set registration owns a bounded row/set/join/projection tree; admission and
    /// maintenance policies apply PER NODE, not to their aggregate footprint.
    /// Ranked windows compose at any depth, including around products and
    /// UNWIND when each selected scope proves its comparator. ORDER BY/OFFSET
    /// without LIMIT is admitted only with a proven finite upstream bound.
    /// Scopes/filters preserve inherited rank; projections and sets follow the
    /// snapshot canonicalization rules. The existing product-free finite-page
    /// profile also admits canonical bare pages. Unknown positional paging or
    /// a positional unranked root mixed with pages still refuses. Genuinely
    /// unbounded sorting and unsupported descendants refuse even under LIMIT 0.
    /// Unwindowed compound delivery is canonical bag order, not implicit
    /// left-major enumeration. Policies still apply per node.
    /// The normal rebuild API repairs the complete owned circuit atomically.
    pub fn register_standing<V: Vfs + Clone>(
        &self,
        database: &mut Database<V>,
        cx: &QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        database
            .ensure_readable()
            .map_err(StandingQueryError::Read)?;
        cx.with_restriction(|| {
            if let Self::Set(prepared) = self {
                let bound = prepared
                    .bind_parameters(params)
                    .map_err(|e| prepare_error(QueryError::SetText(e)))?;
                return set::register(database, cx, &bound, policy);
            }
            let (query, layout) = match self {
                Self::Pattern(prepared) => {
                    let bound = prepared
                        .bind_parameters(params)
                        .map_err(|e| prepare_error(QueryError::PatternText(e)))?;
                    let layout = Layout::Rows {
                        columns: bound.columns().to_vec(),
                    };
                    (database.prepare_registered_rows(cx, bound, policy)?, layout)
                }
                Self::Aggregate(prepared) => {
                    let bound = prepared
                        .bind_parameters(params)
                        .map_err(|e| prepare_error(QueryError::PatternText(e)))?;
                    let layout = Layout::Aggregate {
                        columns: prepared.columns().to_vec(),
                        slots: prepared.output_slots().to_vec(),
                    };
                    (
                        database.prepare_registered_aggregate(cx, bound, policy)?,
                        layout,
                    )
                }
                Self::PipelineAggregate(prepared) => {
                    let bound = prepared
                        .bind_parameters(params)
                        .map_err(|e| prepare_error(QueryError::PipelineText(e)))?;
                    let layout = Layout::Aggregate {
                        columns: prepared.columns().to_vec(),
                        slots: prepared.output_slots().to_vec(),
                    };
                    (
                        database.prepare_registered_aggregate(cx, bound, policy)?,
                        layout,
                    )
                }
                _ => {
                    return Err(StandingQueryError::NativeClassUnsupported {
                        facade: self.facade_class(),
                    });
                }
            };
            // Metadata is private too. No registration escapes if its final
            // admission is cancelled after successful source initialization.
            let layout = Arc::new(layout);
            cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
            let mut handle = database.store_standing_query(query);
            handle.native = Some(layout);
            Ok(handle)
        })
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Classify native GQL once and register its bound maintained definition.
    /// Health/cancellation admission precedes resolver callbacks. Preparation
    /// errors keep their native typed source; unsupported maintenance refuses.
    pub fn register_standing_native(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        cx.with_restriction(|| {
            PreparedNativeRead::prepare(text, params, resolver)
                .map_err(prepare_error)?
                .register_standing(self, cx, params, policy)
        })
    }

    /// Register an already bound relational definition through the SAME circuit
    /// compiler as native GQL. This permits typed composition of supported
    /// patterns, projections, filters, UNWIND, sets and Cartesian products without new
    /// text syntax or resolver callbacks. Accepted definitions are owned by the
    /// registry; dropping the caller's tree cannot change a live registration.
    ///
    /// Read through standing_native_query/standing_native_columns. The one
    /// returned handle owns an atomically admitted circuit and rebuilds it as a
    /// unit. Explicit terminal ORDER BY with finite LIMIT is ranked; otherwise
    /// delivery uses canonical bag order, not implicit left-major enumeration.
    /// Nested ranked windows keep complete child selection before later stages.
    /// A comparator and finite bound may be inherited through prior scopes.
    /// Explicitly ranked product/UNWIND trees may contain nested ranked pages;
    /// unknown implicit enumeration never substitutes for a proved comparator.
    /// Bare finite pages remain available in the product-free profile. Unknown
    /// positional pages and genuinely unbounded sorting still refuse.
    /// Each node keeps its own allowance; no eager per-commit fallback is used,
    /// including for a zero terminal limit.
    /// Session-local and in-memory, not a durable subscription or spill engine.
    pub fn register_standing_relation(
        &mut self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        cx.with_restriction(|| set::register(self, cx, query, policy))
    }

    /// The native output names attached at registration, after owner/health/
    /// freshness admission. An ordinary typed-only handle has no native layout.
    pub fn standing_native_columns<'a>(
        &self,
        cx: &QueryCx,
        handle: &'a StandingQueryHandle,
    ) -> Result<&'a [String], StandingQueryError> {
        self.admitted_standing_query(cx, handle)?;
        Ok(handle
            .native
            .as_deref()
            .ok_or(StandingQueryError::Unsupported)?
            .columns())
    }

    /// Materialize the current maintained answer in the SAME lossless cells and
    /// column order as Database::query, together with its exact commit frontier.
    /// Reads no graph records and reruns no query. Ordered occurrences take
    /// precedence over Z-set key order; ALL collisions retain multiplicities.
    ///
    /// Delivery has a separate cumulative result/work/scratch allowance. Payload
    /// reservations precede cell clones and every occurrence is interruptible.
    /// max_snapshot_records is unused because there is no base-source scan.
    /// A refusal returns no partial answer and never fences the maintained view.
    /// This is a collected in-process result, not a cursor/backlog or byte bound.
    pub fn standing_native_query(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        policy: GqlQueryPolicy,
    ) -> Result<(CommitSeq, QueryResult), StandingQueryError> {
        self.admitted_standing_query(cx, handle)?;
        let layout = handle
            .native
            .as_deref()
            .ok_or(StandingQueryError::Unsupported)?;
        cx.with_restriction(|| {
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let root = self.admitted_standing_query(cx, handle)?;
            let (at, rows) = if let StandingQuery::Window(query) = root {
                let rows = collect_runs(
                    query.ordered().map(|(row, count)| (row, Some(count))),
                    layout.columns().len(),
                    &mut meter,
                    |row, meter| {
                        let mut cells = Vec::new();
                        for value in row.values() {
                            reserve_value(value, meter)?;
                            cells.push(QueryValue::Value(value.clone()));
                        }
                        Ok(cells)
                    },
                )
                .map_err(StandingQueryError::Delivery)?;
                (query.frontier, rows)
            } else {
                match layout {
                    Layout::Rows { .. } | Layout::Circuit { .. } => {
                        let mut view = match self.admitted_standing_query(cx, handle)? {
                            StandingQuery::Rows { .. } => self.standing_rows(cx, handle)?,
                            StandingQuery::Set(_) => self.standing_set(cx, handle)?,
                            StandingQuery::Join(_) => self.standing_join(cx, handle)?,
                            StandingQuery::Projection(_) => self.standing_projection(cx, handle)?,
                            _ => return Err(StandingQueryError::Unsupported),
                        };
                        if matches!(layout, Layout::Circuit { .. }) {
                            // PreparedGraphSet canonicalizes a pattern's selected
                            // bag before set composition. Do not leak the leaf's
                            // pre-wrapper ordering from a transparent root scope.
                            view.ordered = None;
                        }
                        let rows =
                            collect(&view, layout.columns().len(), &mut meter, |row, meter| {
                                let mut cells = Vec::new();
                                for value in row.values() {
                                    reserve_value(value, meter)?;
                                    cells.push(QueryValue::Value(value.clone()));
                                }
                                Ok(cells)
                            })
                            .map_err(StandingQueryError::Delivery)?;
                        (view.frontier(), rows)
                    }
                    Layout::Aggregate { slots, .. } => {
                        let view = self.standing_query(cx, handle)?;
                        let rows =
                            collect(&view, layout.columns().len(), &mut meter, |row, meter| {
                                let mut cells = Vec::new();
                                for slot in slots {
                                    match *slot {
                                        GraphAggregateTextSlot::GroupKey(at) => {
                                            let value = row
                                                .keys()
                                                .get(at)
                                                .ok_or(StandingQueryFailure::InvalidDelta)?;
                                            reserve_value(value, meter)?;
                                            cells.push(QueryValue::Value(value.clone()));
                                        }
                                        GraphAggregateTextSlot::Aggregate(at) => {
                                            let value = row
                                                .values()
                                                .get(at)
                                                .ok_or(StandingQueryFailure::InvalidDelta)?;
                                            match value {
                                                QueryValue::Value(value) => {
                                                    reserve_value(value, meter)?
                                                }
                                                _ => {
                                                    meter.charge(ZSetEvent::Work)?;
                                                    meter.charge(ZSetEvent::ScratchEntry)?;
                                                }
                                            }
                                            cells.push(value.clone());
                                        }
                                    }
                                }
                                Ok(cells)
                            })
                            .map_err(StandingQueryError::Delivery)?;
                        (view.frontier(), rows)
                    }
                }
            };
            let mut columns = Vec::new();
            for name in layout.columns() {
                meter
                    .charge(ZSetEvent::Work)
                    .map_err(StandingQueryError::Delivery)?;
                meter
                    .units(ZSetEvent::ScratchEntry, 1 + name.len().div_ceil(64))
                    .map_err(StandingQueryError::Delivery)?;
                columns.push(name.clone());
            }
            (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
            Ok((at, QueryResult::Rows { columns, rows }))
        })
    }
}

fn reserve_value(
    value: &fgdb_gql::algebra::GraphValue,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    let units = value
        .payload_units()
        .checked_add(1)
        .ok_or(StandingQueryFailure::ScratchBudget)?;
    meter.units(ZSetEvent::ScratchEntry, units)
}

fn collect<Row: Ord>(
    view: &StandingQueryView<'_, Row>,
    width: usize,
    meter: &mut Meter<'_>,
    project: impl FnMut(&Row, &mut Meter<'_>) -> Result<Vec<QueryValue>, StandingQueryFailure>,
) -> Result<Vec<Vec<QueryValue>>, StandingQueryFailure> {
    if let Some(ordered) = view.ordered_rows() {
        collect_runs(ordered.map(|row| (row, None)), width, meter, project)
    } else {
        collect_runs(
            view.rows().iter().map(|(row, weight)| (row, Some(weight))),
            width,
            meter,
            project,
        )
    }
}

// Ordered unit occurrences, canonical bags and compressed ranked runs share
// exactly one delivery allowance. Expansion happens only here, after checking
// the next complete run against the caller's cumulative result-row limit.
fn collect_runs<'a, Row: Ord + 'a>(
    runs: impl IntoIterator<Item = (&'a Row, Option<&'a ZWeight>)>,
    width: usize,
    meter: &mut Meter<'_>,
    mut project: impl FnMut(&Row, &mut Meter<'_>) -> Result<Vec<QueryValue>, StandingQueryFailure>,
) -> Result<Vec<Vec<QueryValue>>, StandingQueryFailure> {
    let mut rows = Vec::new();
    let mut delivered = 0_u64;
    for (row, weight) in runs {
        let count = if let Some(weight) = weight {
            meter.charge(ZSetEvent::Work)?;
            if weight <= &ZWeight::ZERO {
                return Err(StandingQueryFailure::InvalidDelta);
            }
            weight
                .to_i128()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or(StandingQueryFailure::ResultBudget)?
        } else {
            1
        };
        let final_count = delivered
            .checked_add(count)
            .ok_or(StandingQueryFailure::ResultBudget)?;
        if meter
            .policy
            .rows
            .max_result_rows()
            .is_some_and(|limit| final_count > limit)
            || usize::try_from(final_count).is_err()
        {
            return Err(StandingQueryFailure::ResultBudget);
        }
        for _ in 0..count {
            meter.charge(ZSetEvent::Work)?;
            meter.charge(ZSetEvent::ScratchEntry)?;
            let cells = project(row, meter)?;
            if cells.len() != width {
                return Err(StandingQueryFailure::InvalidDelta);
            }
            rows.push(cells);
        }
        delivered = final_count;
    }
    (meter.checkpoint)()?;
    Ok(rows)
}

#[cfg(test)]
mod tests;
