//! Exact-cut resident graph leaves for the shared external relational engine.

use super::*;
use fgdb_gql::spill_set::AsyncSpillSetSourcePlan;
use fgdb_types::CanonicalScalarResolver;

pub(super) enum ResidentOrder<I> {
    Native(OpenedOrder<I>),
    Relation {
        plan: AsyncSpillSetPlan,
        view: EmbeddedReadView,
        snapshot: CommitSeq,
    },
}

/// Parameter binding, physical admission and source pinning happen before the
/// future is returned. Neither a later writer nor parameter rebinding can move
/// a correlated body's graph or element reads to another generation/cut.
pub(super) fn open<'q>(
    prepared: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    params: &GqlParameters,
    policy: GqlQueryPolicy,
    max_input_rows: u64,
) -> core::result::Result<ResidentOrder<impl SpoolInput + 'q + use<'q>>, QueryError> {
    let captured = view.capture_statement_parameters(cx, prepared.statement(), params)?;
    let params = captured.as_ref();
    let (set, snapshot) = match prepared {
        PreparedNativeRead::Set(template) => (
            template
                .bind_parameters(params)
                .map_err(QueryError::SetText)?,
            view.frontier(),
        ),
        PreparedNativeRead::TemporalSet(template) => {
            let bound = template
                .bind_parameters(params)
                .map_err(QueryError::TemporalSetText)?;
            (bound.query().clone(), bound.as_of())
        }
        _ => {
            return super::open(prepared, view, cx, params, policy, max_input_rows, None)
                .map(ResidentOrder::Native);
        }
    };
    view.snapshot.check_frontier(snapshot).map_err(|error| {
        QueryError::Stream(GqlQueryError::Source(VertexScanError::Source(error)))
    })?;
    if set.ordered_row_pull().is_some() || set.ordered_graph_row_pull().is_some() {
        // Retain the native compiler's proven projection fusion and its exact
        // source counters for already admitted linear plans. Binding remains
        // a single synchronous operation; only nonfusible relations recurse.
        return super::open(
            prepared,
            view,
            cx,
            params,
            policy,
            max_input_rows,
            Some((set, snapshot)),
        )
        .map(ResidentOrder::Native);
    }
    let plan = AsyncSpillSetPlan::compile(&set).map_err(|_| QueryError::StreamingUnsupported {
        facade: prepared.facade_class(),
    })?;
    Ok(ResidentOrder::Relation {
        plan,
        view: view.clone(),
        snapshot,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<I, A, B, C>(
    opened: ResidentOrder<I>,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    auxiliary: &mut SpillFile<C>,
    limits: ExternalOrderLimits,
) -> Result<(NativeResultSpool, u64)>
where
    I: SpoolInput,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    match opened {
        ResidentOrder::Native(opened) => {
            execute_opened(
                opened,
                cx,
                policy,
                scratch,
                destination,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                limits.max_row_bytes,
                limits.max_input_rows,
                limits.max_sort_work,
            )
            .await
        }
        ResidentOrder::Relation {
            plan,
            view,
            snapshot,
        } => {
            let mut host = ResidentHost { view };
            execute_relation(
                &plan,
                None,
                snapshot,
                &mut host,
                cx,
                policy,
                scratch,
                destination,
                auxiliary,
                limits,
                None,
            )
            .await
            .map_err(resident_stage_error)
        }
    }
}

struct ResidentHost {
    view: EmbeddedReadView,
}

impl ExternalOrderHost for ResidentHost {
    fn check_snapshot(&mut self, snapshot: CommitSeq) -> Result<()> {
        self.view
            .snapshot
            .check_frontier(snapshot)
            .map_err(|error| {
                NativeSpoolError::Execute(Box::new(GqlQueryError::Source(ScanError::Vertex(
                    VertexScanError::Source(error),
                ))))
            })
    }

    fn checkpoint(&mut self) -> Result<()> {
        Ok(())
    }

    fn native_event(&mut self, _event: GlaExecutionEvent) -> Result<()> {
        Ok(())
    }

    fn supports_property_seeds(&self) -> bool {
        true
    }

    fn seeded_property_leaf<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        key: PropertyKeyId,
        values: crate::gql_exec::source::PropertySeedValues,
        snapshot: CommitSeq,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        Box::pin(async move {
            self.check_snapshot(snapshot)?;
            if !matches!(plan.source(), AsyncSpillSetSourcePlan::Vertex(_)) {
                return Err(SpillError::InvalidRun.into());
            }
            let query = plan.source_pattern().ok_or(SpillError::InvalidRun)?;
            let tail = plan.source_tail().cloned().ok_or(SpillError::InvalidRun)?;
            let source = self
                .view
                .vertex_scan_source(cx, snapshot)
                .map_err(|error| {
                    NativeSpoolError::Execute(Box::new(GqlQueryError::Source(ScanError::Vertex(
                        VertexScanError::Source(error),
                    ))))
                })?
                .with_property_seeds(key, values);
            let (source_plan, _) =
                VertexScanPlan::compile_sort_input(query.plan()).map_err(|error| {
                    NativeSpoolError::Execute(Box::new(GqlQueryError::Source(ScanError::Vertex(
                        VertexScanError::Plan(error),
                    ))))
                })?;
            let cursor = VertexScanCursor::new(source, source_plan, policy, move || {
                cx.with_restriction(|| cx.checkpoint())
            });
            ordered_input(
                cx,
                plan.source_columns().to_vec(),
                cursor,
                tail,
                policy,
                scratch,
                destination,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                limits.max_row_bytes,
                limits.max_sort_work,
            )
            .await
        })
    }

    fn leaf<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        _arguments: Option<&'q GraphValueRow>,
        snapshot: CommitSeq,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
        _resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        Box::pin(async move {
            self.check_snapshot(snapshot)?;
            let query = plan.source_pattern().ok_or(SpillError::InvalidRun)?;
            let tail = plan.source_tail().cloned().ok_or(SpillError::InvalidRun)?;
            let cursor = if matches!(
                query.plan().operators().first(),
                Some(GlaOperator::ScanEdges { .. })
            ) {
                let source = self.view.edge_scan_source(cx, snapshot).map_err(|error| {
                    NativeSpoolError::Execute(Box::new(GqlQueryError::Source(ScanError::Edge(
                        EdgeScanError::Source(error),
                    ))))
                })?;
                let (source_plan, _) =
                    EdgeScanPlan::compile_sort_input(query.plan()).map_err(|error| {
                        NativeSpoolError::Execute(Box::new(GqlQueryError::Source(ScanError::Edge(
                            EdgeScanError::Plan(error),
                        ))))
                    })?;
                ScanCursor::Edge(Box::new(EdgeScanCursor::new(
                    source,
                    source_plan,
                    policy,
                    move || cx.with_restriction(|| cx.checkpoint()),
                )))
            } else {
                let source = self
                    .view
                    .vertex_scan_source(cx, snapshot)
                    .map_err(|error| {
                        NativeSpoolError::Execute(Box::new(GqlQueryError::Source(
                            ScanError::Vertex(VertexScanError::Source(error)),
                        )))
                    })?;
                let (source_plan, _) =
                    VertexScanPlan::compile_sort_input(query.plan()).map_err(|error| {
                        NativeSpoolError::Execute(Box::new(GqlQueryError::Source(
                            ScanError::Vertex(VertexScanError::Plan(error)),
                        )))
                    })?;
                ScanCursor::Vertex(Box::new(VertexScanCursor::new(
                    source,
                    source_plan,
                    policy,
                    move || cx.with_restriction(|| cx.checkpoint()),
                )))
            };
            ordered_input(
                cx,
                plan.source_columns().to_vec(),
                cursor,
                tail,
                policy,
                scratch,
                destination,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                limits.max_row_bytes,
                limits.max_sort_work,
            )
            .await
        })
    }

    fn stages<'q, A, B>(
        &'q mut self,
        plan: &'q AsyncSpillSetPlan,
        input: NativeResultSpool,
        cx: &'q QueryCx,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<A>,
        destination: &'q mut SpillFile<B>,
        limits: ExternalOrderLimits,
        prior_work: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> crate::SendFuture<'q, Result<(NativeResultSpool, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'q,
    {
        Box::pin(async move {
            let mut elements = ResidentElements {
                view: &self.view,
                as_of: input.snapshot_seq(),
                budget: policy.rows,
                records: input.row_stats().snapshot_records,
            };
            let (mut output, work) = staged::execute_with_elements(
                plan,
                input,
                &mut elements,
                cx,
                policy,
                scratch,
                destination,
                limits.run_rows,
                limits.max_runs,
                limits.page_bytes,
                limits.max_row_bytes,
                limits.max_input_rows,
                limits.max_sort_work,
                prior_work,
                resolver,
            )
            .await
            .map_err(resident_stage_error)?;
            output.rows.snapshot_records = elements.records;
            Ok((output, work))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::io::ReadBuf;
    use asupersync::lab::run_async_under_lab;
    use fgdb_gql::algebra::{GraphValue, IntegerComparison};
    use fgdb_gql::{
        GraphSetOperand, GraphSetPredicateOp, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
        PreparedGraphText,
    };
    use fgdb_strata::tiered::memory::SpillLimits;
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
    use std::io::{self, Cursor, Seek, SeekFrom};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    // Only the asynchronous file surface is supplied here; frame admission,
    // authentication, byte ownership and decoding are the production owners.
    #[derive(Default)]
    struct File(Cursor<Vec<u8>>);
    impl AsyncRead for File {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, out)
        }
    }
    impl AsyncWrite for File {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, bytes)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }
    impl AsyncSeek for File {
        fn poll_seek(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            at: SeekFrom,
        ) -> Poll<io::Result<u64>> {
            Poll::Ready(self.get_mut().0.seek(at))
        }
    }

    #[test]
    fn optional_property_seeds_release_pool_pressure_and_keep_completed_left_work() {
        let ((), report) = run_async_under_lab(0x5eed_7402, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let database = Database::open_memory(
                &contexts.commit(),
                crate::DatabaseKeys::new(
                    [0x71; 32],
                    DatabaseSecurityNamespaceId([0x72; 32]),
                    [0x73; 32],
                ),
            )
            .await
            .unwrap();
            let mut host = ResidentHost {
                view: database.pinned_read_view().unwrap(),
            };
            let pattern = |text| -> PreparedGraphSet {
                PreparedGraphText::prepare(text, |kind, name: &str| match (kind, name) {
                    (GraphSymbolKind::Property, "p") => {
                        Some(GraphSymbol::Property(PropertyKeyId(1)))
                    }
                    _ => None,
                })
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .with_duplicates()
                .into()
            };
            let query = pattern("MATCH (a) RETURN a.p AS wanted")
                .cross_join(pattern("MATCH (b) RETURN b.p AS value"))
                .unwrap()
                .filter(&[GraphSetPredicateOp::Compare {
                    left: GraphSetOperand::Column(0),
                    comparison: IntegerComparison::Equal,
                    right: GraphSetOperand::Column(1),
                }])
                .unwrap();
            let physical = AsyncSpillSetPlan::compile(&query).unwrap();
            let AsyncSpillSetSourcePlan::Cross(plan) = physical.source() else {
                panic!("expected a selected external product");
            };
            assert_eq!(plan.property_seed_column(), Some((0, PropertyKeyId(1))));
            let policy = GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000);
            let pool = MemoryPool::new(128_000, 0).unwrap();
            let mut file = SpillFile::new(
                &cx,
                File::default(),
                pool.clone(),
                SpillLimits {
                    max_file_bytes: 128_000,
                    max_run_bytes: 64_000,
                    max_runs: 8,
                },
            )
            .await
            .unwrap();
            let wanted = [
                CanonicalScalar::Int(7),
                CanonicalScalar::ucs_basic_text(&"x".repeat(4096)).unwrap(),
            ];
            let frames: Vec<_> = wanted
                .iter()
                .map(|value| {
                    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(value.clone())])
                        .canonical_bytes()
                        .unwrap()
                })
                .collect();
            let mut writer = file.paged_writer(&cx, 8192).unwrap();
            for frame in &frames {
                writer
                    .write(&cx, &(frame.len() as u64).to_be_bytes())
                    .await
                    .unwrap();
                writer.write(&cx, frame).await.unwrap();
            }
            let spool = NativeResultSpool {
                columns: vec!["wanted".into()].into(),
                encoded_columns: 1,
                snapshot: host.view.frontier(),
                kind: ScanKind::Vertex,
                rows: GqlExecutionStats {
                    snapshot_records: 17,
                    result_rows: 2,
                },
                evaluator: GlaExecutionStats {
                    work_units: 19,
                    scratch_entries: 23,
                },
                max_row_bytes: frames.iter().map(Vec::len).max().unwrap(),
                run: writer.finish(&cx).await.unwrap(),
            };
            assert_eq!(pool.used(), 0);

            // Observe the real reader's live bytes, without searching for a
            // magic pool size or including optional seed capacity in its cost.
            let mut probe = cross::position(&spool);
            drop(probe.next(&mut file, &cx).await.unwrap().unwrap());
            let second = probe.next(&mut file, &cx).await.unwrap().unwrap();
            let second_read_bytes = pool.used();
            let second_decoded_bytes = codec::decoded(second.as_ref(), &cx).unwrap();
            drop(second);
            drop(probe);
            let capacity = ChargedVec::<CanonicalScalar>::new(&pool, &cx, 2).unwrap();
            let seed_bytes = pool.used();
            drop(capacity);
            assert_eq!(pool.used(), 0);

            for (at, available) in [
                seed_bytes - 1,
                seed_bytes,
                seed_bytes + second_read_bytes + second_decoded_bytes - 1,
            ]
            .into_iter()
            .enumerate()
            {
                let held_bytes = pool.limit() - available;
                let hold = pool.reserve(&cx, held_bytes).unwrap();
                let before_file = file.stats();
                let mut left = spool.clone();
                let mut work = 31;
                let result = cross::seeds::prepare(
                    cross::seeds::SeedPlan::Cross(plan),
                    &mut left,
                    &mut host,
                    &cx,
                    policy,
                    &mut file,
                    2,
                    &mut work,
                    1_000_000,
                    None,
                )
                .await
                .unwrap();
                assert!(result.is_none(), "capacity case {at}");
                assert_eq!(pool.used(), held_bytes, "capacity case {at}");
                assert_eq!(left.row_count(), spool.row_count());
                if at < 2 {
                    assert_eq!(file.stats(), before_file, "no scratch input read");
                    assert_eq!(work, 31);
                    assert_eq!(left.evaluator, spool.evaluator);
                } else {
                    assert_eq!(
                        work,
                        31 + frames.iter().map(|frame| frame.len() as u64).sum::<u64>()
                    );
                    assert!(left.evaluator.scratch_entries > spool.evaluator.scratch_entries);
                }
                drop(hold);
                assert_eq!(pool.used(), 0);
            }

            let mut left = spool.clone();
            let mut work = 31;
            let roots = cross::seeds::prepare(
                cross::seeds::SeedPlan::Cross(plan),
                &mut left,
                &mut host,
                &cx,
                policy,
                &mut file,
                2,
                &mut work,
                1_000_000,
                None,
            )
            .await
            .unwrap()
            .unwrap();
            let cross::seeds::Roots::Property { key, values } = roots else {
                panic!("property seed owner");
            };
            assert_eq!(key, PropertyKeyId(1));
            assert_eq!(values.values, wanted);
            assert!(pool.used() >= seed_bytes + wanted[1].owned_heap_bytes().unwrap());
            drop(values);
            assert_eq!(pool.used(), 0);

            // Physical capacity may decline the optimization; a work refusal
            // is a terminal error and must never silently retry another lane.
            let mut left = spool.clone();
            let mut work = 31;
            assert!(matches!(
                cross::seeds::prepare(
                    cross::seeds::SeedPlan::Cross(plan),
                    &mut left,
                    &mut host,
                    &cx,
                    policy,
                    &mut file,
                    2,
                    &mut work,
                    31,
                    None,
                )
                .await,
                Err(NativeSpoolError::SortWorkLimit { .. })
            ));
            assert_eq!(pool.used(), 0);
            let mut reader = spool.reader(&mut file);
            for frame in frames {
                assert_eq!(reader.next_row(&cx).await.unwrap().unwrap().as_ref(), frame);
            }
            assert!(reader.next_row(&cx).await.unwrap().is_none());
            assert_eq!(pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
