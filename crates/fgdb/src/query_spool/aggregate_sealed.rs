//! Terminal exact aggregates over one sealed analytics source. The input uses
//! the existing external relational host; completed groups retain the native
//! aggregate domain and never pass through a scalar-row conversion.

use super::*;
use crate::PreparedBufferedSealedOrder;
use fgdb_delta_types::RelationId;
use fgdb_prism::FnxMemoryLimits;
use fgdb_strata::tiered::sealed::SealedLimits;

/// A single non-correlated sealed FnX source feeding native external grouping.
/// Wide sums, exact averages and ordered collections retain their native output
/// domains. The input may include supported external relational stages, and
/// both input and final aggregate output must complete before a handle escapes.
///
/// Sealed images, projections, kernel vectors and each individual collection
/// retain their independent memory limits. This owner does not make those
/// allocations external-memory or bypass any ordinary aggregate refusal.
#[derive(Clone)]
pub struct PreparedBufferedSealedAggregate {
    input: PreparedBufferedSealedOrder,
    definition: SpillAggregateDefinition,
    columns: Vec<String>,
    slots: Vec<GraphAggregateTextSlot>,
}

impl core::fmt::Debug for PreparedBufferedSealedAggregate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PreparedBufferedSealedAggregate([REDACTED])")
    }
}

impl PreparedNativeRead {
    /// Explicit privileged admission for terminal aggregates over exactly one
    /// sealed procedure source. Ordinary buffered preparation remains unchanged.
    /// Binding and registry-shape admission perform no graph or scratch IO;
    /// argument evaluation remains at the reached procedure source.
    pub fn prepare_buffered_sealed_aggregate(
        &self,
        params: &GqlParameters,
        relation: RelationId,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
        max_source_incidences: usize,
    ) -> Result<PreparedBufferedSealedAggregate> {
        let (plan, columns, slots, as_of) = match self {
            Self::PipelineAggregate(template) => {
                let aggregate = template.bind_relation_parameters(params).map_err(|error| {
                    NativeAggregateSpoolError::Prepare(Box::new(QueryError::PipelineText(error)))
                })?;
                (
                    AsyncSpillAggregatePlan::compile_relation_with_single_procedure(&aggregate)
                        .map_err(|_| NativeAggregateSpoolError::Unsupported)?,
                    template.columns().to_vec(),
                    template.output_slots().to_vec(),
                    None,
                )
            }
            _ => {
                let BoundAggregate {
                    definition,
                    columns,
                    slots,
                    as_of,
                } = bind(self, params)?;
                (
                    AsyncSpillAggregatePlan::compile_with_single_procedure(&definition)
                        .map_err(|_| NativeAggregateSpoolError::Unsupported)?,
                    columns,
                    slots,
                    as_of,
                )
            }
        };
        let AsyncSpillAggregatePlan::Relation { input, definition } = plan else {
            return Err(NativeAggregateSpoolError::Unsupported);
        };
        if !crate::query::aggregate_stream::valid_layout(
            &columns,
            &slots,
            definition.key_columns(),
            definition.aggregate_columns(),
        ) {
            return Err(NativeAggregateSpoolError::Unsupported);
        }
        let input = PreparedBufferedSealedOrder::from_relation(
            *input,
            as_of,
            relation,
            memory,
            sealing,
            max_source_incidences,
        )
        .ok_or(NativeAggregateSpoolError::Unsupported)?;
        Ok(PreparedBufferedSealedAggregate {
            input,
            definition,
            columns,
            slots,
        })
    }
}

impl PreparedBufferedSealedAggregate {
    /// Relational barriers before the terminal aggregate reduction.
    pub fn stage_count(&self) -> usize {
        self.input.stage_count()
    }

    /// Terminal reduction plus every grouping stage in the input relation.
    pub fn aggregate_count(&self) -> usize {
        1 + self.input.aggregate_count()
    }

    /// Select an exact historical cut without replacing a statement selector.
    pub fn with_as_of(
        mut self,
        requested: CommitSeq,
    ) -> core::result::Result<Self, crate::SealedQueryError> {
        self.input = self.input.with_as_of(requested)?;
        Ok(self)
    }

    /// Complete the sealed input and its external stages, then reduce the
    /// authenticated relation with the ordinary native aggregate engine.
    ///
    /// The three files are distinct, query-private owners. Input occurrences
    /// use `max_input_rows`; only selected final groups spend the query's result
    /// limit. Source/evaluator usage and physical spill work remain cumulative
    /// across the handoff, including cache hits and final `LIMIT 0` queries.
    /// The completed result belongs to `destination`.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_in_view<'q, V, A, B, C>(
        &self,
        view: &'q mut BufferedReadView<V>,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        source: &'q mut SpillFile<A>,
        partition: &'q mut SpillFile<B>,
        destination: &'q mut SpillFile<C>,
        group_capacity: usize,
        max_partitions: usize,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_work_units: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeAggregateSpool, u64)>>
    where
        V: Vfs + Clone + 'q,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        let input = self.input.clone();
        let definition = self.definition.clone();
        let columns = self.columns.clone();
        let slots = self.slots.clone();
        Box::pin(async move {
            validate_limits(
                group_capacity,
                max_partitions,
                run_rows,
                max_runs,
                page_bytes,
                max_row_bytes,
            )?;
            let private_policy = GqlQueryPolicy {
                rows: GqlExecutionBudget::new(
                    policy.rows.max_snapshot_records().unwrap_or(u64::MAX),
                    max_input_rows,
                ),
                evaluator: policy.evaluator,
            };
            // The sealed relational runner writes to its second file. Its
            // completed result must become the reducer's source, leaving the
            // final destination free for native aggregate output.
            let (initial, prior_work) = input
                .spool_in_view_with_resolver(
                    view,
                    cx,
                    private_policy,
                    partition,
                    source,
                    destination,
                    group_capacity,
                    max_partitions,
                    run_rows,
                    max_runs,
                    page_bytes,
                    max_row_bytes,
                    max_input_rows,
                    max_work_units,
                    resolver,
                )
                .await?;
            let opened = Opened {
                input: Box::new(CompletedRelation::new(cx, policy, &initial)),
                definition,
                columns,
                slots,
            };
            execute(
                opened,
                Some(initial),
                prior_work,
                cx,
                source,
                partition,
                destination,
                group_capacity,
                max_partitions,
                run_rows,
                max_runs,
                page_bytes,
                max_row_bytes,
                max_input_rows,
                max_work_units,
                resolver,
            )
            .await
        })
    }
}
