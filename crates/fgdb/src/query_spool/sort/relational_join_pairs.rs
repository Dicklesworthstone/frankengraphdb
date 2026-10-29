//! Bounded equality-only pair joins over authenticated, key-sorted runs.
use super::*;

// Private coordinates over a completed immutable run, not a clone of its
// resident page or a public cursor restart. Replays authenticate pages through
// the ordinary reader and spend the existing work and memory allowances.
fn uncached_position(input: &MergeInput) -> MergeInput {
    MergeInput {
        run: input.run.clone(),
        remaining: input.remaining,
        offset: input.offset,
        page: None,
        max_row_bytes: input.max_row_bytes,
    }
}

fn compare_keys(
    plan: &SpillSetJoinPlan,
    left: &GraphValueRow,
    right: &GraphValueRow,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    evaluator: &mut ControlledStats<'_>,
) -> Result<Ordering> {
    plan.compare_keys_with_control(left, right, &mut |event| {
        staged::native_event(cx, policy, evaluator, event)
    })
    .map_err(failure)
}

// Merge disjoint keys monotonically; replay only the matching right group for
// duplicate left occurrences. Retain constant-size coordinates, not a decoded
// group or match bitmap. At most a group head and one replay/lookahead row stay
// reserved across writes. Row visits are O(left + right + emitted pairs) after
// sorting; payload work, authentication and sorting are charged separately.
// The enclosing native Join stage restores result order before pagination.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute<A, B, C>(
    plan: &SpillSetJoinPlan,
    left: &NativeResultSpool,
    right: &NativeResultSpool,
    snapshot: CommitSeq,
    snapshot_records: u64,
    mut evaluator: ControlledStats<'_>,
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
    let cx = work.cx;
    let pool = scratch.memory_pool().clone();
    let mut left_input = cross::position(&left);
    let mut right_input = cross::position(&right);
    let mut left_row = pull_sorted(
        &mut left_input,
        scratch,
        &pool,
        policy,
        &mut evaluator,
        work,
        resolver,
    )
    .await?;
    let mut right_row = pull_sorted(
        &mut right_input,
        auxiliary,
        &pool,
        policy,
        &mut evaluator,
        work,
        resolver,
    )
    .await?;
    let preserve_left = matches!(plan.kind(), RowJoinKind::Left | RowJoinKind::Full);
    let preserve_right = matches!(plan.kind(), RowJoinKind::Right | RowJoinKind::Full);
    let mut writer = destination.paged_writer(cx, page_bytes)?;
    let mut rows = 0_u64;
    let mut largest = 0;
    macro_rules! emit {
        ($left:expr, $right:expr) => {{
            let next = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            GqlExecutionBudget::result_rows(max_input_rows)
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(|error| {
                    NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Rows(error)))
                })?;
            let mut charge = pool.reserve(cx, 0).map_err(SpillError::Memory)?;
            let row = plan
                .project_pair_with_control($left, $right, &mut |value| {
                    event(cx, policy, &mut evaluator, &mut charge, value)
                })
                .map_err(failure)?;
            let row = SpoolRow::buffered((row, charge));
            let encoded = codec::encode(&pool, work, &row.row, max_row_bytes)?;
            write_row(&mut writer, &encoded.0, work).await?;
            largest = largest.max(encoded.0.len());
            rows = next;
        }};
    }
    macro_rules! advance {
        ($head:ident, $input:ident, $source:ident) => {{
            // Refund the old decoded payload before admitting its successor.
            drop($head.take());
            $head = pull_sorted(
                &mut $input,
                $source,
                &pool,
                policy,
                &mut evaluator,
                work,
                resolver,
            )
            .await?;
        }};
    }
    while let Some(current_left) = left_row.as_ref() {
        let Some(current_right) = right_row.as_ref() else {
            if preserve_left {
                emit!(Some(&current_left.row), None);
            }
            advance!(left_row, left_input, scratch);
            continue;
        };
        match compare_keys(
            plan,
            &current_left.row,
            &current_right.row,
            cx,
            policy,
            &mut evaluator,
        )? {
            Ordering::Less => {
                if preserve_left {
                    emit!(Some(&current_left.row), None);
                }
                advance!(left_row, left_input, scratch);
            }
            Ordering::Greater => {
                if preserve_right {
                    emit!(None, Some(&current_right.row));
                }
                advance!(right_row, right_input, auxiliary);
            }
            Ordering::Equal => {
                if !matches_pair(
                    plan,
                    &current_left.row,
                    &current_right.row,
                    cx,
                    policy,
                    &mut evaluator,
                    &pool,
                )? {
                    // Equal tuples containing NULL are not matches. Advance
                    // only left; right occurrences are padded independently
                    // when their turn arrives, never multiplied by NULL rows.
                    if preserve_left {
                        emit!(Some(&current_left.row), None);
                    }
                    advance!(left_row, left_input, scratch);
                    continue;
                }
                let group_head = right_row.take().expect("matched right head");
                let after_head = uncached_position(&right_input);
                // The bookmark holds no page. Refund the original cache
                // before the replay admits another authenticated page.
                drop(right_input.page.take());
                loop {
                    let current_left = left_row.as_ref().expect("matched left head");
                    emit!(Some(&current_left.row), Some(&group_head.row));
                    let mut probe = uncached_position(&after_head);
                    let next_right = loop {
                        let Some(candidate) = pull_sorted(
                            &mut probe,
                            auxiliary,
                            &pool,
                            policy,
                            &mut evaluator,
                            work,
                            resolver,
                        )
                        .await?
                        else {
                            break None;
                        };
                        match compare_keys(
                            plan,
                            &current_left.row,
                            &candidate.row,
                            cx,
                            policy,
                            &mut evaluator,
                        )? {
                            Ordering::Equal => {
                                // The head proved nonnull equality. Equal
                                // tuples are witnesses without a residual ON.
                                emit!(Some(&current_left.row), Some(&candidate.row));
                            }
                            Ordering::Less => break Some(candidate),
                            Ordering::Greater => return Err(SpillError::InvalidRun.into()),
                        }
                    };
                    advance!(left_row, left_input, scratch);
                    let repeat = match left_row.as_ref() {
                        Some(next_left) => {
                            compare_keys(
                                plan,
                                &next_left.row,
                                &group_head.row,
                                cx,
                                policy,
                                &mut evaluator,
                            )? == Ordering::Equal
                        }
                        None => false,
                    };
                    if !repeat {
                        // Every right occurrence in this group matched. Keep
                        // only its successor, not an unmatched group member.
                        right_row = next_right;
                        right_input = probe;
                        break;
                    }
                    // Refund lookahead and its page before the next replay.
                    // No earlier or unrelated right keys are rescanned.
                    drop(next_right);
                    drop(probe);
                }
            }
        }
    }
    // INNER/LEFT must consume the unselected suffix too: unread corrupt pages,
    // refused allocations and cancellation cannot become successful joins.
    while let Some(current_right) = right_row.as_ref() {
        if preserve_right {
            emit!(None, Some(&current_right.row));
        }
        advance!(right_row, right_input, auxiliary);
    }
    drop(left_input);
    drop(right_input);
    staged::native_event(cx, policy, &mut evaluator, GlaExecutionEvent::Work)?;
    work.charge(1)?;
    let run = writer.finish(cx).await?;
    Ok(NativeResultSpool {
        columns: plan.columns().to_vec().into(),
        encoded_columns: plan.columns().len(),
        snapshot,
        kind: ScanKind::Set,
        rows: GqlExecutionStats {
            snapshot_records,
            result_rows: rows,
        },
        evaluator: evaluator.stats,
        max_row_bytes: largest,
        run,
    })
}
