//! Optional bounded identity roots from a fully completed leading relation.
//! The original left spool survives unchanged for occurrence-preserving joins.

use super::*;
use crate::buffered_read::BufferedVertexSeeds;
use crate::query::view::spool::sort::ChargedVec;
use fgdb_strata::tiered::memory::MemoryError;
use fgdb_types::VId;

// A fallible in-place heap sort keeps comparison/swap work cancellable without
// another allocation or an infallible comparator hiding a control refusal.
fn sort(values: &mut [VId], control: &mut impl FnMut() -> Result<()>) -> Result<()> {
    fn sift(
        values: &mut [VId],
        mut root: usize,
        end: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        while root < end / 2 {
            control()?;
            let mut child = root * 2 + 1;
            if child + 1 < end && values[child] < values[child + 1] {
                child += 1;
            }
            if values[root] >= values[child] {
                break;
            }
            values.swap(root, child);
            root = child;
        }
        Ok(())
    }
    let len = values.len();
    for root in (0..len / 2).rev() {
        sift(values, root, len, control)?;
    }
    for end in (1..len).rev() {
        control()?;
        values.swap(0, end);
        sift(values, 0, end, control)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(in crate::query::view::spool::sort) async fn prepare<H, F>(
    plan: &SpillSetCrossPlan,
    left: &mut NativeResultSpool,
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    source: &mut SpillFile<F>,
    run_rows: usize,
    left_work: &mut u64,
    max_sort_work: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<Option<BufferedVertexSeeds>>
where
    H: ExternalOrderHost,
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let Some(column) = plan.vertex_seed_column() else {
        return Ok(None);
    };
    if !host.supports_vertex_seeds() || u128::from(left.row_count()) > run_rows as u128 {
        return Ok(None);
    }
    host.checkpoint()?;
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    let count = usize::try_from(left.row_count()).map_err(|_| SpillError::SizeOverflow)?;
    let pool = source.memory_pool().clone();
    // Empty/singleton seed sets, including repeated copies of one identity,
    // need no allocation. A larger set reserves at most the admitted batch.
    let mut single = None;
    let mut seeds: Option<ChargedVec<VId>> = None;
    let mut work = Work {
        cx,
        used: *left_work,
        limit: max_sort_work,
    };
    let mut input = position(left);
    let mut seen = 0_u64;
    while let Some(bytes) = input.next(source, cx).await? {
        seen = seen.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        if seen > left.row_count() {
            return Err(NativeSpoolError::IncompleteCursor);
        }
        let row = decode(bytes, &pool, &mut work, resolver)?;
        plan.validate_input_with_control(&row.row, 0, &mut |event| {
            host::event(host, cx, policy, &mut left.evaluator, event)
        })
        .map_err(|error| match error {
            SpillSetRowError::Control(error) => error,
            SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
        })?;
        if let GraphValue::Vertex(vid) = &row.row.values()[column] {
            if seeds.is_none() && single.is_some_and(|first| first != *vid) {
                // Decide optional capacity before any right-source effect.
                // Only this affordability refusal keeps the ordinary lane;
                // already spent seed work is never erased, and no failure
                // after a point source starts can retry another source.
                let mut batch = match ChargedVec::<VId>::new(&pool, cx, count) {
                    Ok(batch) => batch,
                    Err(NativeSpoolError::Spill(SpillError::Memory(
                        MemoryError::ResourceExhausted { .. },
                    ))) => {
                        *left_work = work.used;
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                };
                if count < 2 {
                    return Err(NativeSpoolError::IncompleteCursor);
                }
                batch
                    .values
                    .push(single.take().expect("a second distinct seed"));
                seeds = Some(batch);
            }
            if seeds.is_none() && single == Some(*vid) {
                continue;
            }
            host::event(
                host,
                cx,
                policy,
                &mut left.evaluator,
                GlaExecutionEvent::ScratchEntry,
            )?;
            // Capacity is bounded by the advertised complete left count. A
            // malformed extra occurrence must not grow an unadmitted vector.
            if let Some(seeds) = &mut seeds {
                if seeds.values.len() == count {
                    return Err(NativeSpoolError::IncompleteCursor);
                }
                seeds.values.push(*vid);
            } else {
                single = Some(*vid);
            }
        }
    }
    if seen != left.row_count() {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    let Some(mut seeds) = seeds else {
        *left_work = work.used;
        return Ok(Some(
            single.map_or(BufferedVertexSeeds::Empty, BufferedVertexSeeds::One),
        ));
    };
    let mut step = || {
        work.charge(1)?;
        host::event(
            host,
            cx,
            policy,
            &mut left.evaluator,
            GlaExecutionEvent::Work,
        )
    };
    sort(&mut seeds.values, &mut step)?;
    let mut distinct = 0;
    for at in 0..seeds.values.len() {
        step()?;
        if distinct == 0 || seeds.values[at] != seeds.values[distinct - 1] {
            seeds.values[distinct] = seeds.values[at];
            distinct += 1;
        }
    }
    seeds.values.truncate(distinct);
    *left_work = work.used;
    let ChargedVec {
        values,
        _charge,
        _extra,
    } = seeds;
    Ok(Some(BufferedVertexSeeds::Many {
        remaining: values.into_iter(),
        _charge,
        _extra,
    }))
}
