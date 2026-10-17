//! Keyed residual ON joins with bounded, per-occurrence witness searches.
use super::*;
use fgdb_strata::tiered::memory::spill::PagedSpillWriter;

#[derive(Clone, Copy)]
enum Pass {
    Left,
    RightAbsence,
}
impl Pass {
    fn pair<'a>(
        self,
        outer: &'a GraphValueRow,
        peer: &'a GraphValueRow,
    ) -> (&'a GraphValueRow, &'a GraphValueRow) {
        match self {
            Self::Left => (outer, peer),
            Self::RightAbsence => (peer, outer),
        }
    }
    fn presence(self, kind: RowJoinKind) -> bool {
        matches!(self, Self::RightAbsence)
            || matches!(kind, RowJoinKind::Semi | RowJoinKind::Anti)
    }
    fn selected(self, kind: RowJoinKind, found: bool) -> bool {
        match self {
            Self::RightAbsence => !found,
            Self::Left => match kind {
                RowJoinKind::Semi => found,
                RowJoinKind::Anti | RowJoinKind::Left | RowJoinKind::Full => !found,
                _ => false,
            },
        }
    }
}

// Copy immutable coordinates, not a cached page or a decoded key group. Every
// replay borrows the ordinary authenticated reader and its memory admission.
fn uncached_position(input: &MergeInput) -> MergeInput {
    MergeInput {
        run: input.run.clone(),
        remaining: input.remaining,
        offset: input.offset,
        page: None,
        max_row_bytes: input.max_row_bytes,
    }
}

#[allow(clippy::too_many_arguments)]
fn compare(
    plan: &SpillSetJoinPlan,
    pass: Pass,
    outer: &GraphValueRow,
    peer: &GraphValueRow,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
) -> Result<Ordering> {
    let (left, right) = pass.pair(outer, peer);
    let order = plan
        .compare_keys_with_control(left, right, &mut |value| {
            staged::native_event(cx, policy, evaluator, value)
        })
        .map_err(failure)?;
    Ok(match pass {
        Pass::Left => order,
        Pass::RightAbsence => order.reverse(),
    })
}

#[derive(Default)]
struct Output {
    rows: u64,
    largest: usize,
}
impl Output {
    #[allow(clippy::too_many_arguments)]
    async fn emit<F>(
        &mut self,
        plan: &SpillSetJoinPlan,
        left: Option<&GraphValueRow>,
        right: Option<&GraphValueRow>,
        writer: &mut PagedSpillWriter<'_, F>,
        pool: &MemoryPool,
        policy: GqlQueryPolicy,
        evaluator: &mut GlaExecutionStats,
        max_row_bytes: usize,
        max_input_rows: u64,
        work: &mut Work<'_>,
    ) -> Result<()>
    where
        F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    {
        let next = self.rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        GqlExecutionBudget::result_rows(max_input_rows)
            .check(GqlBudgetDimension::ResultRows, next)
            .map_err(|error| {
                NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
            })?;
        let cx = work.cx;
        let mut charge = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
        let row = plan
            .project_pair_with_control(left, right, &mut |value| {
                event(cx, policy, evaluator, &mut charge, value)
            })
            .map_err(failure)?;
        let row = SpoolRow::buffered((row, charge));
        let encoded = codec::encode(pool, work, &row.row, max_row_bytes)?;
        write_row(writer, &encoded.0, work).await?;
        self.rows = next;
        self.largest = self.largest.max(encoded.0.len());
        Ok(())
    }
}

// The peer head advances monotonically between key groups. Each outer
// occurrence replays ONLY its equal-key group. A witness belongs to that
// occurrence, never its whole group. Presence passes stop at the first TRUE;
// pair passes emit every TRUE. FALSE/UNKNOWN never establish a witness.
//
// The independent peer position is not advanced by a replay. It subsequently
// traverses the group once when moving to the next key, or while draining the
// suffix. This bounds non-candidate visits linearly and ensures an early
// witness cannot conceal a later corrupt frame, quota refusal or cancellation.
#[allow(clippy::too_many_arguments)]
async fn sweep<A, B, C>(
    plan: &SpillSetJoinPlan,
    pass: Pass,
    outer: &NativeResultSpool,
    peer: &NativeResultSpool,
    outer_file: &mut SpillFile<A>,
    peer_file: &mut SpillFile<B>,
    writer: &mut PagedSpillWriter<'_, C>,
    output: &mut Output,
    pool: &MemoryPool,
    policy: GqlQueryPolicy,
    evaluator: &mut GlaExecutionStats,
    max_row_bytes: usize,
    max_input_rows: u64,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<()>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let cx = work.cx;
    let presence = pass.presence(plan.kind());
    let mut outer_input = cross::position(outer);
    let mut peer_input = cross::position(peer);
    let mut head = pull_sorted(
        &mut peer_input,
        peer_file,
        pool,
        policy,
        evaluator,
        work,
        resolver,
    )
    .await?;
    while let Some(row) = pull_sorted(
        &mut outer_input,
        outer_file,
        pool,
        policy,
        evaluator,
        work,
        resolver,
    )
    .await?
    {
        // NULL keys can never match. Do not replay a large all-NULL group or
        // evaluate its ON program, but still drain both physical inputs.
        let mut nonnull = true;
        for &(left, right) in plan.keys() {
            staged::native_event(cx, policy, evaluator, GlaExecutionEvent::Work)?;
            let column = match pass {
                Pass::Left => left,
                Pass::RightAbsence => right,
            };
            if row.row.values()[column].is_null() {
                nonnull = false;
                break;
            }
        }
        let mut found = false;
        if nonnull {
            loop {
                let Some(peer_row) = head.as_ref() else {
                    break;
                };
                match compare(plan, pass, &row.row, &peer_row.row, cx, policy, evaluator)? {
                    Ordering::Less => break,
                    Ordering::Greater => {
                        drop(head.take());
                        head = pull_sorted(
                            &mut peer_input,
                            peer_file,
                            pool,
                            policy,
                            evaluator,
                            work,
                            resolver,
                        )
                        .await?;
                    }
                    Ordering::Equal => {
                        let (left, right) = pass.pair(&row.row, &peer_row.row);
                        found = matches_pair(plan, left, right, cx, policy, evaluator, pool)?;
                        if found && !presence {
                            output
                                .emit(
                                    plan,
                                    Some(left),
                                    Some(right),
                                    writer,
                                    pool,
                                    policy,
                                    evaluator,
                                    max_row_bytes,
                                    max_input_rows,
                                    work,
                                )
                                .await?;
                        }
                        if !presence || !found {
                            let mut probe = uncached_position(&peer_input);
                            // The bookmark carries no page charge. Refund the
                            // forward cache before the replay admits another page.
                            drop(peer_input.page.take());
                            while let Some(candidate) = pull_sorted(
                                &mut probe,
                                peer_file,
                                pool,
                                policy,
                                evaluator,
                                work,
                                resolver,
                            )
                            .await?
                            {
                                match compare(
                                    plan,
                                    pass,
                                    &row.row,
                                    &candidate.row,
                                    cx,
                                    policy,
                                    evaluator,
                                )? {
                                    Ordering::Less => break,
                                    Ordering::Greater => return Err(SpillError::InvalidRun.into()),
                                    Ordering::Equal => {}
                                }
                                let (left, right) = pass.pair(&row.row, &candidate.row);
                                if matches_pair(plan, left, right, cx, policy, evaluator, pool)? {
                                    found = true;
                                    if presence {
                                        break;
                                    }
                                    output
                                        .emit(
                                            plan,
                                            Some(left),
                                            Some(right),
                                            writer,
                                            pool,
                                            policy,
                                            evaluator,
                                            max_row_bytes,
                                            max_input_rows,
                                            work,
                                        )
                                        .await?;
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
        if pass.selected(plan.kind(), found) {
            let (left, right) = match pass {
                Pass::Left => (Some(&row.row), None),
                Pass::RightAbsence => (None, Some(&row.row)),
            };
            output
                .emit(
                    plan,
                    left,
                    right,
                    writer,
                    pool,
                    policy,
                    evaluator,
                    max_row_bytes,
                    max_input_rows,
                    work,
                )
                .await?;
        }
    }
    drop(head);
    while pull_sorted(
        &mut peer_input,
        peer_file,
        pool,
        policy,
        evaluator,
        work,
        resolver,
    )
    .await?
    .is_some()
    {}
    Ok(())
}

// Children have already completed and validated. Reuse their original native
// matcher/projection and the ordinary key sorter; no new scalar semantics,
// group materialization, witness bitmap, scratch owner or retry strategy.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<A, B, C>(
    plan: &SpillSetJoinPlan,
    left: &NativeResultSpool,
    right: &NativeResultSpool,
    snapshot: CommitSeq,
    snapshot_records: u64,
    mut evaluator: GlaExecutionStats,
    policy: GqlQueryPolicy,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    auxiliary: &mut SpillFile<C>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let left = order_keys(
        plan,
        0,
        left,
        destination,
        scratch,
        run_rows,
        max_runs,
        page_bytes,
        work,
    )
    .await?;
    let right = order_keys(
        plan,
        1,
        right,
        destination,
        auxiliary,
        run_rows,
        max_runs,
        page_bytes,
        work,
    )
    .await?;
    let pool = scratch.memory_pool().clone();
    let mut output = Output::default();
    let mut writer = destination.paged_writer(work.cx, page_bytes)?;
    sweep(
        plan,
        Pass::Left,
        &left,
        &right,
        scratch,
        auxiliary,
        &mut writer,
        &mut output,
        &pool,
        policy,
        &mut evaluator,
        max_row_bytes,
        max_input_rows,
        work,
        resolver,
    )
    .await?;
    if matches!(plan.kind(), RowJoinKind::Right | RowJoinKind::Full) {
        // Recheck absence PER right occurrence with operands in their original
        // schema order. A match for one right row cannot suppress another's
        // null extension. No duplicate matched pairs are emitted in this pass.
        // This is a read-only sweep of the same sorted runs, not another sort
        // or output run. All allowances and the output cap remain cumulative.
        sweep(
            plan,
            Pass::RightAbsence,
            &right,
            &left,
            auxiliary,
            scratch,
            &mut writer,
            &mut output,
            &pool,
            policy,
            &mut evaluator,
            max_row_bytes,
            max_input_rows,
            work,
            resolver,
        )
        .await?;
    }
    staged::native_event(work.cx, policy, &mut evaluator, GlaExecutionEvent::Work)?;
    work.charge(1)?;
    let run = writer.finish(work.cx).await?;
    Ok(NativeResultSpool {
        columns: plan.columns().to_vec().into(),
        encoded_columns: plan.columns().len(),
        snapshot,
        kind: ScanKind::Set,
        rows: GqlExecutionStats {
            snapshot_records,
            result_rows: output.rows,
        },
        evaluator,
        max_row_bytes: output.largest,
        run,
    })
}
