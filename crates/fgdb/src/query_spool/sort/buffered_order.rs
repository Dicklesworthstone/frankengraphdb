//! Bind and admit the buffered source before storage opening, then feed the
//! existing native external order engine without an intermediate resident view.

use super::*;
use crate::BufferedReadView;
use fgdb_gql::edge_stream::AsyncEdgeScanPlan;
use fgdb_gql::spill_set::{AsyncSpillSetPlan, AsyncSpillSetSourcePlan};
use fgdb_gql::stream::AsyncVertexScanPlan;
use fgdb_types::CanonicalScalarResolver;

#[derive(Clone)]
enum Input {
    Vertex(AsyncVertexScanPlan<GraphValueRow>),
    Edge(AsyncEdgeScanPlan),
    Set(AsyncSpillSetPlan),
}

/// A parameter-bound local query whose complete ORDER BY/DISTINCT/window is
/// executed over a buffered source. Preparation reads no graph or scratch.
/// The sealed physical plan owns its clauses and exact historical selector;
/// an unsupported shape refuses before a database needs to be opened.
#[derive(Clone)]
pub struct PreparedBufferedOrder {
    columns: Vec<String>,
    input: Input,
    tail: ScanSortTail,
    as_of: Option<CommitSeq>,
}
impl core::fmt::Debug for PreparedBufferedOrder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PreparedBufferedOrder([REDACTED])")
    }
}

impl PreparedNativeRead {
    /// Bind once and admit a vertex or single-edge local source for external
    /// ordering. Property-only output, local predicates, hidden ordering keys,
    /// edge orientation and row DISTINCT use their existing GLA semantics.
    /// Native unary Project/Filter/Scope relations preserve every intermediate
    /// canonicalization, DISTINCT, order and window through external stages.
    /// Expansion, probes, joins, UNWIND and aggregate shapes refuse here.
    /// Numeric aggregate plans have their own preparation.
    pub fn prepare_buffered_order(&self, params: &GqlParameters) -> Result<PreparedBufferedOrder> {
        let prepare = |error| NativeSpoolError::Prepare(Box::new(error));
        let set = match self {
            Self::Set(template) => Some((
                template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::SetText(error)))?,
                None,
            )),
            Self::TemporalSet(template) => {
                let bound = template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::TemporalSetText(error)))?;
                Some((bound.query().clone(), Some(bound.as_of())))
            }
            _ => None,
        };
        if let Some((query, as_of)) = set {
            let plan = AsyncSpillSetPlan::compile(&query).map_err(|_| {
                prepare(QueryError::StreamingUnsupported {
                    facade: self.facade_class(),
                })
            })?;
            return Ok(PreparedBufferedOrder {
                columns: plan.columns().to_vec(),
                tail: plan.source_tail().clone(),
                input: Input::Set(plan),
                as_of,
            });
        }
        let (query, as_of) = match self {
            Self::Pattern(template) => (
                template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::PatternText(error)))?,
                None,
            ),
            Self::TemporalPattern(template) => {
                let bound = template
                    .bind_parameters(params)
                    .map_err(|error| prepare(QueryError::TemporalText(error)))?;
                (bound.pattern().clone(), Some(bound.as_of()))
            }
            _ => {
                return Err(prepare(QueryError::StreamingUnsupported {
                    facade: self.facade_class(),
                }));
            }
        };
        let (input, tail) = if matches!(
            query.plan().operators().first(),
            Some(GlaOperator::ScanEdges { .. })
        ) {
            let (plan, tail) =
                AsyncEdgeScanPlan::compile_sort_input(query.plan()).map_err(|error| {
                    prepare(QueryError::EdgeStream(GqlQueryError::Source(
                        EdgeScanError::Plan(error),
                    )))
                })?;
            (Input::Edge(plan), tail)
        } else {
            let (plan, tail) =
                AsyncVertexScanPlan::compile_sort_input(query.plan()).map_err(|error| {
                    prepare(QueryError::Stream(GqlQueryError::Source(
                        VertexScanError::Plan(error),
                    )))
                })?;
            (Input::Vertex(plan), tail)
        };
        Ok(PreparedBufferedOrder {
            columns: query.columns().to_vec(),
            input,
            tail,
            as_of,
        })
    }
}

impl PreparedBufferedOrder {
    /// Number of native relational barriers after graph-source evaluation.
    /// Every barrier has finite external sort/window/copy work; hosts may use
    /// this bound to admit scratch metadata before opening the database.
    pub fn stage_count(&self) -> usize {
        match &self.input {
            Input::Set(plan) => plan.stages().len(),
            _ => 0,
        }
    }

    /// Evaluate the complete buffered source, externally order all occurrences,
    /// apply DISTINCT, then SKIP/LIMIT and the visible projection. LIMIT zero
    /// still evaluates the full source and preserves late failures. A handle
    /// escapes only after every phase succeeds; it always belongs to destination.
    ///
    /// Source rows and expressions use the view's MemoryPool. Intermediate
    /// native-stage decode and expression allocations use the destination spill
    /// pool. Row reservations survive canonical encoding and all awaited
    /// appends. The native encoder's complete row/cell/scalar overlap is admitted
    /// to the spill pool before allocation, with max_row_bytes checked against
    /// an allocation-free shape first. Scratch pages, sort runs and merge metadata
    /// use the same scratch-file pools. Returned bytes must match that preflight.
    /// Hosts may use children of a common pool for an aggregate resident bound,
    /// or separately account source and spill allowances. Initial recovery,
    /// bound plans/catalogs and caller-owned copies have their existing bounds.
    ///
    /// Source record/evaluator quotas are cumulative; max_input_rows governs
    /// private input occurrences, and the original result quota governs only
    /// final selected rows. max_sort_work cumulatively covers input encoding and
    /// the existing sort/window work; no phase restarts the allowance.
    /// Both scratch files must be distinct and query-private. No allowance is
    /// reset or refunded for appended intermediates, errors or cancellation.
    /// The exact temporal cut is checked against this view's publication.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_in_view<'q, V, A, B>(
        &self,
        view: &'q mut BufferedReadView<V>,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        V: Vfs + Clone + 'q,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        self.spool_in_view_with_resolver(
            view,
            cx,
            policy,
            scratch,
            destination,
            run_rows,
            max_runs,
            page_bytes,
            max_row_bytes,
            max_input_rows,
            max_sort_work,
            None,
        )
    }

    /// Execute the same checked buffered program with the native resolver used
    /// to decode artifact-bound text and timestamp values between stages.
    /// The supplied resolver is carried through every scratch barrier; no host
    /// locale, timezone, or replacement artifact can be chosen by this executor.
    /// All quotas and memory guarantees of [`Self::spool_in_view`] apply.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_in_view_with_resolver<'q, V, A, B>(
        &self,
        view: &'q mut BufferedReadView<V>,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_sort_work: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        V: Vfs + Clone + 'q,
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        let as_of = self.as_of.unwrap_or_else(|| view.frontier());
        let columns = self.columns.clone();
        let tail = self.tail.clone();
        let input_policy = input_policy(policy, max_input_rows);
        match &self.input {
            Input::Set(plan) => {
                let plan = plan.clone();
                let source = Self {
                    columns: plan.source_columns().to_vec(),
                    input: match plan.source() {
                        AsyncSpillSetSourcePlan::Vertex(source) => Input::Vertex(source.clone()),
                        AsyncSpillSetSourcePlan::Edge(source) => Input::Edge(source.clone()),
                    },
                    tail,
                    as_of: Some(as_of),
                };
                // Source rows and every intermediate page have their own
                // allowance. Only the last native stage spends final rows.
                Box::pin(async move {
                    let (input, work) = source
                        .spool_in_view_with_resolver(
                            view,
                            cx,
                            input_policy,
                            &mut *scratch,
                            &mut *destination,
                            run_rows,
                            max_runs,
                            page_bytes,
                            max_row_bytes,
                            max_input_rows,
                            max_sort_work,
                            resolver,
                        )
                        .await?;
                    super::staged::execute(
                        plan,
                        input,
                        cx,
                        policy,
                        scratch,
                        destination,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_input_rows,
                        max_sort_work,
                        work,
                        resolver,
                    )
                    .await
                })
            }
            Input::Vertex(plan) => {
                let opened = view
                    .open_vertex_input(cx, plan.clone(), as_of, input_policy)
                    .map_err(|error| {
                        NativeSpoolError::BufferedExecute(Box::new(
                            error.map_source(ScanError::Vertex),
                        ))
                    });
                Box::pin(async move {
                    evaluate(
                        cx,
                        columns,
                        opened?,
                        tail,
                        policy,
                        scratch,
                        destination,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_sort_work,
                    )
                    .await
                })
            }
            Input::Edge(plan) => {
                let opened = view
                    .open_edge_input(cx, plan.clone(), as_of, input_policy)
                    .map_err(|error| {
                        NativeSpoolError::BufferedExecute(Box::new(
                            error.map_source(ScanError::Edge),
                        ))
                    });
                Box::pin(async move {
                    evaluate(
                        cx,
                        columns,
                        opened?,
                        tail,
                        policy,
                        scratch,
                        destination,
                        run_rows,
                        max_runs,
                        page_bytes,
                        max_row_bytes,
                        max_sort_work,
                    )
                    .await
                })
            }
        }
    }
}
