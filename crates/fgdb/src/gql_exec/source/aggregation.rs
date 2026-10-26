//! Aggregate, set, and bounded shortest-walk execution over one admitted snapshot source.

mod cheapest_path;
mod edge_stream;
pub use edge_stream::SnapshotEdgeSource;
mod shortest_admission;
mod shortest_any;
mod vertex_stream;
pub use vertex_stream::SnapshotVertexSource;

use crate::gql_exec::{AdmissionUsage, AdmittedGqlSnapshot, GqlSnapshotReader};
use crate::{Database, EmbeddedReadView, GqlError, ReadError, Snapshot};
use asupersync::fs::Vfs;
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::{
    GlaExecutionEvent, GlaExecutionLimits, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded,
    GqlBudgetDimension, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregateError, GraphAggregateRow, GraphCountSource, GraphShortestWalkCursor,
    GraphWalkBounds, PreparedGraphAggregate,
};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::collections::BTreeMap;

type AggregateResult<E> = Result<
    GqlQueryExecution<GraphAggregateRow>,
    GqlQueryError<GraphAggregateError<E>, Box<asupersync::error::Error>>,
>;

impl<V: Vfs + Clone> Database<V> {
    /// Summarize one live snapshot. Ordinary inputs keep streaming/factorized
    /// execution; captured paths retain real edge IDs in bounded input rows.
    /// Admission, path copies, grouping and final output share one policy.
    pub fn execute_graph_aggregate_governed(
        &self,
        cx: &QueryCx,
        aggregate: &PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        let as_of = self.frontier().map_err(|error| {
            GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
        })?;
        self.execute_graph_aggregate_governed_at(cx, aggregate, as_of, policy)
    }

    /// Health and exact-sequence fences precede cancellation/resource refusal.
    /// A count, including zero, is never obtained by skipping source admission.
    pub fn execute_graph_aggregate_governed_at(
        &self,
        cx: &QueryCx,
        aggregate: &PreparedGraphAggregate,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(|error| {
                GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
            })?;
        cx.with_restriction(|| execute_at(self, aggregate, as_of, policy, || cx.checkpoint()))
    }

    pub fn execute_temporal_graph_aggregate_text_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::BoundTemporalGraphAggregateQuery,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.execute_graph_aggregate_governed_at(cx, query.aggregate(), query.as_of(), policy)
    }
}

impl EmbeddedReadView {
    pub fn execute_graph_aggregate_governed(
        &self,
        cx: &QueryCx,
        aggregate: &PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.execute_graph_aggregate_governed_at(cx, aggregate, self.frontier(), policy)
    }

    pub fn execute_graph_aggregate_governed_at(
        &self,
        cx: &QueryCx,
        aggregate: &PreparedGraphAggregate,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.snapshot.check_frontier(as_of).map_err(|error| {
            GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
        })?;
        cx.with_restriction(|| execute_at(self, aggregate, as_of, policy, || cx.checkpoint()))
    }

    pub fn execute_temporal_graph_aggregate_text_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::BoundTemporalGraphAggregateQuery,
        policy: GqlQueryPolicy,
    ) -> AggregateResult<GqlError> {
        self.execute_graph_aggregate_governed_at(cx, query.aggregate(), query.as_of(), policy)
    }
}

fn execute_at<R: GqlSnapshotReader + ?Sized, C>(
    reader: &R,
    aggregate: &PreparedGraphAggregate,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<GqlError>, C>> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let mut admitted =
        AdmittedGqlSnapshot::admit_logical(aggregate.input_pattern().plan().clone(), reader, as_of)
            .map_err(|error| {
                GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
            })?;
    let mut usage = AdmissionUsage::default();
    let elements = |element, key| {
        crate::gql_exec::read_element_property(
            element,
            key,
            |vid| reader.gql_vertex_at(vid, as_of),
            |eid| reader.gql_edge_at(eid, as_of),
        )
    };
    // Exact metadata is available only on the private unrestricted source at
    // its own frontier. A label count has a separate cold path: its predicate
    // still runs through the ordinary executor, preserving source errors and
    // interleaved scratch/work events when no exact cardinality is available.
    let mut label_builder = None;
    if let (Some(snapshot), Some(source)) = (admitted.snapshot, aggregate.count_metadata_source())
        && as_of == snapshot.frontier
    {
        let matching = snapshot
            .exact_counts
            .lookup(source, &mut |event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                usage.observe::<GraphAggregateError<ReadError>, C>(policy, event)
            })
            .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
        if let Some(matching) = matching {
            let count = aggregate
                .prepare_count_metadata(source, matching)
                .expect("the source shape was checked before the exact metadata lookup");
            let result = count.execute_governed_with_element_reads(
                1,
                elements,
                usage.remaining(policy),
                checkpoint,
            );
            return usage.finish(policy, result);
        }
        if matches!(source, GraphCountSource::Label(_)) {
            label_builder = snapshot.exact_counts.label_builder();
        }
    }
    // The same MVCC visitors can admit a plain count without retaining their
    // winning rows a second time. Shape admission excludes every input reader,
    // filter and metadata projection; the original aggregate finalizer still
    // owns output expressions, HAVING, ordering and pages. An independently
    // fallible reader keeps its ordinary materialized source.
    if let (Some(snapshot), Some(source)) = (admitted.snapshot, aggregate.count_summary_source()) {
        let current = as_of == snapshot.frontier;
        let mut builder = if current && matches!(source, GraphCountSource::Relation(_)) {
            snapshot.exact_counts.relation_builder()
        } else {
            None
        };
        let (examined, matching) = count_input_with_builder(
            snapshot,
            aggregate.input_pattern().plan(),
            source,
            as_of,
            builder.as_mut(),
            &mut |event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                usage.observe::<GraphAggregateError<ReadError>, C>(policy, event)
            },
        )
        .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
        let relations = builder
            .map(|builder| {
                builder.finish(&mut |event| {
                    checkpoint().map_err(GqlQueryError::Interrupted)?;
                    usage.observe::<GraphAggregateError<ReadError>, C>(policy, event)
                })
            })
            .transpose()
            .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
        let count = aggregate
            .prepare_count_summary(source, examined, matching)
            .expect("the checked source counts only its admitted live occurrences");
        let result = count.execute_governed_with_element_reads(
            examined,
            elements,
            usage.remaining(policy),
            checkpoint,
        );
        let result = usage.finish(policy, result);
        if result.is_ok() && current {
            if source == GraphCountSource::Vertices {
                snapshot.exact_counts.publish_vertices(matching);
            }
            if let Some(relations) = relations {
                snapshot.exact_counts.publish_relations(relations);
            }
        }
        return result;
    }
    admitted
        .materialize(&mut |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            usage.observe::<GraphAggregateError<ReadError>, C>(policy, event)
        })
        .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
    // A single-label count admits every live vertex, then evaluates its one
    // label predicate in GLA. Build from that complete admitted population,
    // never from matching output rows or an independently pruned candidate set.
    // Each label membership and bounded-map allocation is governed before use.
    let labels = if let Some(mut builder) = label_builder {
        let tables = admitted
            .borrowed
            .as_ref()
            .expect("exact label metadata requires the private snapshot source");
        let mut control = |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            usage.observe::<GraphAggregateError<ReadError>, C>(policy, event)
        };
        for vertex in &tables.vertices {
            control(super::SourceEvent::Work)
                .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
            for label in &vertex.labels {
                control(super::SourceEvent::Work)
                    .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
                builder
                    .observe(*label, &mut control)
                    .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
            }
        }
        Some(
            builder
                .finish(&mut control)
                .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?,
        )
    } else {
        None
    };
    // labels(n) and type(r) keys, arguments and row stages read the catalog
    // names, as a pattern read does; an unmapped symbol refuses, never NULL.
    admitted
        .validate_and_cache_catalog_symbols()
        .map_err(|error| {
            GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(error)))
        })?;
    admitted
        .cache_property_maps(
            &mut |event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                usage.observe::<GraphAggregateError<ReadError>, C>(policy, event)
            },
            |error| GqlQueryError::Source(GraphAggregateError::Source(error)),
        )
        .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
    let result = aggregate.execute_governed_with_element_maps_and_reads(
        admitted.snapshot_records,
        admitted.vertex_ids(),
        admitted.identified_edges(),
        |vid, predicates| admitted.matches(vid, predicates).map_err(GqlError::Read),
        |vid, key| Ok(admitted.property(vid, key)),
        |eid, key| Ok(admitted.edge_property(eid, key)),
        |vid| Ok(admitted.cached_labels.get(&vid).map(Vec::as_slice)),
        |eid| Ok(admitted.cached_types.get(&eid)),
        |vid| Ok(admitted.property_maps.vertices.get(&vid)),
        |eid| Ok(admitted.property_maps.edges.get(&eid)),
        elements,
        usage.remaining(policy),
        checkpoint,
    );
    let result = usage.finish(policy, result);
    if result.is_ok()
        && let (Some(snapshot), Some(labels)) = (admitted.snapshot, labels)
    {
        snapshot.exact_counts.publish_labels(labels);
    }
    result
}

// Preserve scan_vertices/scan_edges' full admission transcript, including the
// logical scratch charge for each live row, while retaining just two counts.
// The existing edge visitor still owns its bounded-by-policy MVCC winner merge;
// this removes the second live-edge table, not the source's history work.
#[cfg(test)]
fn count_input<E>(
    snapshot: &Snapshot,
    plan: &fgdb_gql::algebra::GlaPlan<fgdb_gql::algebra::GraphValueRow>,
    source: GraphCountSource,
    as_of: CommitSeq,
    control: &mut impl FnMut(super::SourceEvent) -> Result<(), E>,
) -> Result<(u64, u64), E> {
    count_input_with_builder(snapshot, plan, source, as_of, None, control)
}

fn count_input_with_builder<E>(
    snapshot: &Snapshot,
    plan: &fgdb_gql::algebra::GlaPlan<fgdb_gql::algebra::GraphValueRow>,
    source: GraphCountSource,
    as_of: CommitSeq,
    mut builder: Option<&mut super::exact_counts::RelationBuilder>,
    control: &mut impl FnMut(super::SourceEvent) -> Result<(), E>,
) -> Result<(u64, u64), E> {
    let mut examined = 0;
    let mut matching = 0;
    match source {
        GraphCountSource::Vertices => {
            // Ordinary vertex admission first checks index eligibility. A
            // validated filterless count has no seek predicate, but that
            // bounded plan probe still owns work and refusal checkpoints.
            if super::PROPERTY_INDEX_SERVING {
                let _ = super::bound_vertices(snapshot, plan, as_of, control)?;
            }
            super::visit_vertices(&snapshot.patches, as_of, control, |_, control| {
                control(super::SourceEvent::SnapshotRecord)?;
                control(super::SourceEvent::ScratchEntry)?;
                examined += 1;
                Ok(())
            })?;
            matching = examined;
        }
        GraphCountSource::Label(_) => {
            unreachable!("label predicates use ordinary admission when metadata is unavailable")
        }
        GraphCountSource::Relation(relation) => {
            super::visit_edges(&snapshot.blocks, as_of, control, |entry, control| {
                control(super::SourceEvent::SnapshotRecord)?;
                control(super::SourceEvent::ScratchEntry)?;
                examined += 1;
                if entry.relation == relation {
                    matching += 1;
                }
                if let Some(builder) = builder.as_deref_mut() {
                    builder.observe(entry.relation, control)?;
                }
                Ok(())
            })?;
        }
    }
    Ok((examined, matching))
}

#[cfg(test)]
mod count_tests;

type SetResult<E> = Result<
    GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    GqlQueryError<fgdb_gql::GraphSetExecutionError<E>, Box<asupersync::error::Error>>,
>;

impl<V: Vfs + Clone> Database<V> {
    pub fn execute_graph_set_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        let as_of = self.frontier().map_err(|error| {
            GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(GqlError::Read(
                error,
            )))
        })?;
        self.execute_graph_set_governed_at(cx, query, as_of, policy)
    }

    pub fn execute_graph_set_governed_at(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.execute_graph_set_evidenced_at(cx, query, as_of, policy, &mut Vec::new())
    }

    /// [`Self::execute_graph_set_governed_at`], appending each executed
    /// procedure's Prism certificate to `evidence` in execution order, for a
    /// certified read (fgdb-3b1v7).
    pub(crate) fn execute_graph_set_evidenced_at(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
        evidence: &mut Vec<fgdb_prism::FnxCertificate>,
    ) -> SetResult<GqlError> {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(GqlError::Read)
            .map_err(|error| {
                GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(error))
            })?;
        // A CALL stage runs in Prism at this same snapshot, so its rows and
        // every pattern leaf read one generation.
        cx.with_restriction(|| {
            query.execute_governed_with_seeded_elements(
                policy,
                |pattern, allowance| {
                    crate::gql_exec::execute_pattern_at(self, pattern, as_of, allowance, || {
                        cx.checkpoint()
                    })
                },
                |pattern, seeds, allowance| {
                    Some(crate::gql_exec::execute_seeded_pattern_at(
                        self,
                        pattern,
                        Some(seeds),
                        as_of,
                        allowance,
                        || cx.checkpoint(),
                    ))
                },
                |call, arguments, remaining| {
                    if crate::query::is_hybrid(call) {
                        return crate::query::hybrid_procedure_with_registry(
                            &self.snapshot,
                            Some(self.maintained_beacon.query_indexes()),
                            as_of,
                            call,
                            arguments,
                            remaining,
                            cx,
                        );
                    }
                    crate::query::fnx_procedure(
                        call,
                        arguments,
                        as_of,
                        remaining,
                        evidence,
                        |spec, options| self.execute_fnx(cx, spec, options),
                    )
                },
                |element, key| crate::gql_exec::element_property_at(self, element, key, as_of),
                || cx.checkpoint(),
            )
        })
    }

    pub fn execute_temporal_graph_set_text_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::BoundTemporalGraphSetQuery,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.execute_graph_set_governed_at(cx, query.query(), query.as_of(), policy)
    }
}

impl EmbeddedReadView {
    pub fn execute_graph_set_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.execute_graph_set_governed_at(cx, query, self.frontier(), policy)
    }

    pub fn execute_graph_set_governed_at(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.snapshot.check_frontier(as_of).map_err(|error| {
            GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(GqlError::Read(
                error,
            )))
        })?;
        cx.with_restriction(|| {
            query.execute_governed_with_seeded_elements(
                policy,
                |pattern, allowance| {
                    crate::gql_exec::execute_pattern_at(self, pattern, as_of, allowance, || {
                        cx.checkpoint()
                    })
                },
                |pattern, seeds, allowance| {
                    Some(crate::gql_exec::execute_seeded_pattern_at(
                        self,
                        pattern,
                        Some(seeds),
                        as_of,
                        allowance,
                        || cx.checkpoint(),
                    ))
                },
                |call, arguments, remaining| {
                    if crate::query::is_hybrid(call) {
                        return crate::query::hybrid_procedure(
                            &self.snapshot,
                            Some(&self.beacon_indexes),
                            as_of,
                            call,
                            arguments,
                            remaining,
                            cx,
                        );
                    }
                    crate::query::fnx_procedure(
                        call,
                        arguments,
                        as_of,
                        remaining,
                        &mut Vec::new(),
                        |spec, options| self.execute_fnx(cx, spec, options),
                    )
                },
                |element, key| crate::gql_exec::element_property_at(self, element, key, as_of),
                || cx.checkpoint(),
            )
        })
    }

    pub fn execute_temporal_graph_set_text_governed(
        &self,
        cx: &QueryCx,
        query: &fgdb_gql::BoundTemporalGraphSetQuery,
        policy: GqlQueryPolicy,
    ) -> SetResult<GqlError> {
        self.execute_graph_set_governed_at(cx, query.query(), query.as_of(), policy)
    }
}

type ShortestResult =
    Result<GqlQueryExecution<VId>, GqlQueryError<GqlError, Box<asupersync::error::Error>>>;

#[derive(Clone, Copy)]
enum ShortestMultiplicity {
    All,
    Any,
}

impl<V: Vfs + Clone> Database<V> {
    /// Return every shortest WALK occurrence from one existing source to each
    /// reachable endpoint inside the finite hop interval. Equal-length routes
    /// and parallel edges retain multiplicity; longer alternatives to the same
    /// source/end partition are pruned. Results are sorted by endpoint identity.
    /// This endpoint API is the execution primitive for ALL SHORTEST; it does
    /// not claim path-value capture or intermediate/path-expression predicates.
    ///
    /// Source admission follows the exact-cut incidence index only from vertices
    /// reachable before the maximum hop count. Each distinct live matching edge
    /// costs one snapshot-record unit, even if both undirected faces are used.
    /// Disconnected edges and other relations are not retained or record-charged;
    /// historical candidates still consume work. The lower hop bound is applied
    /// only by the native shortest-walk cursor, never by source pruning.
    /// Source scratch and enumeration share one policy. This bounds logical
    /// entries, not allocator bytes; resident indexes and synchronous lookups
    /// remain, and the reachable closure is not a spill-backed representation.
    pub fn execute_all_shortest_walk_governed(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        let as_of = self
            .frontier()
            .map_err(GqlError::Read)
            .map_err(GqlQueryError::Source)?;
        self.execute_all_shortest_walk_governed_at(
            cx, source, relation, direction, bounds, as_of, policy,
        )
    }

    // The governed-read argument family: the non-`_at` form plus an explicit
    // `as_of`, kept identical to the EmbeddedReadView form below.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_all_shortest_walk_governed_at(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(GqlError::Read)
            .map_err(GqlQueryError::Source)?;
        cx.with_restriction(|| {
            execute_shortest_at(
                &self.snapshot,
                source,
                relation,
                direction,
                bounds,
                as_of,
                ShortestMultiplicity::All,
                policy,
                || cx.checkpoint(),
            )
        })
    }
}

impl EmbeddedReadView {
    pub fn execute_all_shortest_walk_governed(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        self.execute_all_shortest_walk_governed_at(
            cx,
            source,
            relation,
            direction,
            bounds,
            self.frontier(),
            policy,
        )
    }

    // Same governed-read argument family as the Database form above.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_all_shortest_walk_governed_at(
        &self,
        cx: &QueryCx,
        source: VId,
        relation: RelationId,
        direction: GlaDirection,
        bounds: GraphWalkBounds,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> ShortestResult {
        self.snapshot
            .check_frontier(as_of)
            .map_err(GqlError::Read)
            .map_err(GqlQueryError::Source)?;
        cx.with_restriction(|| {
            execute_shortest_at(
                &self.snapshot,
                source,
                relation,
                direction,
                bounds,
                as_of,
                ShortestMultiplicity::All,
                policy,
                || cx.checkpoint(),
            )
        })
    }
}

fn charge_shortest<C>(
    stats: &mut GlaExecutionStats,
    limits: GlaExecutionLimits,
    event: GlaExecutionEvent,
    checkpoint: &mut impl FnMut() -> Result<(), C>,
) -> Result<(), GqlQueryError<GqlError, C>> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let mut next = *stats;
    let work = u128::from(next.work_units) + 1;
    if work > u128::from(limits.max_work_units) {
        return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
            dimension: GlaLimitDimension::WorkUnits,
            limit: limits.max_work_units,
            observed: work,
        }));
    }
    next.work_units = work as u64;
    if event == GlaExecutionEvent::ScratchEntry {
        let scratch = u128::from(next.scratch_entries) + 1;
        if scratch > u128::from(limits.max_scratch_entries) {
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension: GlaLimitDimension::ScratchEntries,
                limit: limits.max_scratch_entries,
                observed: scratch,
            }));
        }
        next.scratch_entries = scratch as u64;
    }
    *stats = next;
    Ok(())
}

// Private worker for both governed-read forms: their arguments plus the
// checkpoint closure.
#[allow(clippy::too_many_arguments)]
fn execute_shortest_at<C>(
    snapshot: &Snapshot,
    source: VId,
    relation: RelationId,
    direction: GlaDirection,
    bounds: GraphWalkBounds,
    as_of: CommitSeq,
    multiplicity: ShortestMultiplicity,
    policy: GqlQueryPolicy,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GqlQueryExecution<VId>, GqlQueryError<GqlError, C>> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let mut usage = AdmissionUsage::default();
    let exists = super::find_vertex(snapshot, source, as_of, &mut |event| {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        usage.observe::<GqlError, C>(policy, event)
    })?
    .is_some();
    if !exists {
        return usage.finish(
            policy,
            Ok(GqlQueryExecution {
                value: Vec::new(),
                rows: GqlExecutionStats {
                    snapshot_records: usage.records,
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
            }),
        );
    }

    let adjacency = shortest_admission::collect(
        snapshot.adjacency_index(),
        &snapshot.blocks,
        shortest_admission::Scope {
            source,
            relation,
            direction,
            as_of,
            maximum: bounds.maximum(),
        },
        &mut |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            usage.observe::<GqlError, C>(policy, event)
        },
    )?;

    let remaining = usage.remaining(policy);
    let result = (|| {
        let mut evaluator = GlaExecutionStats::default();
        let mut control =
            |event| charge_shortest(&mut evaluator, remaining.evaluator, event, &mut checkpoint);
        // ANY must coalesce ties DURING native traversal, not enumerate the
        // potentially exponential ALL bag and deduplicate its final output.
        let mut cursor = match multiplicity {
            ShortestMultiplicity::All => {
                GraphShortestWalkCursor::new(source, bounds, Some(&adjacency), &mut control)?
            }
            ShortestMultiplicity::Any => {
                GraphShortestWalkCursor::new_any(source, bounds, Some(&adjacency), &mut control)?
            }
        };
        let mut counts = BTreeMap::<VId, u64>::new();
        let mut occurrences = 0_u64;
        while let Some(endpoint) = cursor.next_with_control(&mut control)? {
            let next = occurrences
                .checked_add(1)
                .expect("in-memory result count fits u64");
            policy
                .rows
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(GqlQueryError::Rows)?;
            occurrences = next;
            // Price the logical retained occurrence even though equal endpoints
            // are compressed into one counter until deterministic release.
            control(GlaExecutionEvent::ScratchEntry)?;
            control(GlaExecutionEvent::Work)?;
            if !counts.contains_key(&endpoint) {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
            let count = counts.entry(endpoint).or_default();
            *count = count
                .checked_add(1)
                .expect("bounded result multiplicity fits u64");
        }
        let mut value = Vec::new();
        for (endpoint, multiplicity) in counts {
            for _ in 0..multiplicity {
                control(GlaExecutionEvent::ResultRow)?;
                value.push(endpoint);
            }
        }
        Ok(GqlQueryExecution {
            value,
            rows: GqlExecutionStats {
                snapshot_records: usage.records,
                result_rows: occurrences,
            },
            evaluator,
        })
    })();
    usage.finish(policy, result)
}
