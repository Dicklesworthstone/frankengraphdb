//! Optional bounded identity/property roots from a completed leading relation.
//! The original left spool survives unchanged for occurrence-preserving joins.

use super::*;
use crate::buffered_read::BufferedVertexSeeds;
use crate::gql_exec::source::PropertySeedValues;
use crate::query::view::spool::sort::ChargedVec;
use fgdb_strata::tiered::memory::MemoryError;
use fgdb_types::VId;

pub(in crate::query::view::spool::sort) enum Roots {
    Identities(BufferedVertexSeeds),
    Property {
        key: PropertyKeyId,
        values: PropertySeedValues,
    },
}

#[derive(Clone, Copy)]
pub(in crate::query::view::spool::sort) enum SeedPlan<'a> {
    Cross(&'a SpillSetCrossPlan),
    Join(&'a fgdb_gql::spill_set::SpillSetJoinPlan),
}

#[derive(Clone, Copy)]
enum SeedColumn {
    Vertex(usize),
    Identity { column: usize, text: bool },
}

impl SeedColumn {
    fn extract(self, row: &GraphValueRow) -> Option<VId> {
        match self {
            Self::Vertex(column) => match &row.values()[column] {
                GraphValue::Vertex(vid) => Some(*vid),
                _ => None,
            },
            Self::Identity { column, text } => {
                fgdb_gql::spill_set::scalar_to_vid(&row.values()[column], text)
            }
        }
    }
}

impl SeedPlan<'_> {
    fn column(self) -> Option<SeedColumn> {
        match self {
            Self::Cross(plan) => plan
                .vertex_seed_column()
                .map(SeedColumn::Vertex)
                .or_else(|| {
                    plan.identity_seed_column()
                        .map(|(column, text)| SeedColumn::Identity { column, text })
                }),
            Self::Join(plan) => plan
                .vertex_seed_column()
                .map(SeedColumn::Vertex)
                .or_else(|| {
                    plan.identity_seed_column()
                        .map(|(column, text)| SeedColumn::Identity { column, text })
                }),
        }
    }

    fn property_column(self) -> Option<(usize, PropertyKeyId)> {
        match self {
            Self::Cross(plan) => plan.property_seed_column(),
            Self::Join(_) => None,
        }
    }

    fn validate<E>(
        self,
        row: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> core::result::Result<(), E>,
    ) -> core::result::Result<(), SpillSetRowError<E>> {
        match self {
            Self::Cross(plan) => plan.validate_input_with_control(row, 0, control),
            Self::Join(plan) => plan.validate_row_with_control(row, 0, control),
        }
    }
}

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
    plan: SeedPlan<'_>,
    left: &mut NativeResultSpool,
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    source: &mut SpillFile<F>,
    run_rows: usize,
    left_work: &mut u64,
    max_sort_work: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<Option<Roots>>
where
    H: ExternalOrderHost,
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    if u128::from(left.row_count()) > run_rows as u128 {
        return Ok(None);
    }
    let Some(column) = plan.column().filter(|_| host.supports_vertex_seeds()) else {
        if host.supports_property_seeds()
            && let Some((column, key)) = plan.property_column()
        {
            return prepare_properties(
                plan,
                column,
                key,
                left,
                host,
                cx,
                policy,
                source,
                left_work,
                max_sort_work,
                resolver,
            )
            .await;
        }
        return Ok(None);
    };
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
        plan.validate(&row.row, &mut |event| {
            host::event(host, cx, policy, &mut left.evaluator, event)
        })
        .map_err(|error| match error {
            SpillSetRowError::Control(error) => error,
            SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
        })?;
        if let Some(vid) = column.extract(&row.row) {
            if seeds.is_none() && single.is_some_and(|first| first != vid) {
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
            if seeds.is_none() && single == Some(vid) {
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
                seeds.values.push(vid);
            } else {
                single = Some(vid);
            }
        }
    }
    if seen != left.row_count() {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    let Some(mut seeds) = seeds else {
        *left_work = work.used;
        return Ok(Some(Roots::Identities(
            single.map_or(BufferedVertexSeeds::Empty, BufferedVertexSeeds::One),
        )));
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
    Ok(Some(Roots::Identities(BufferedVertexSeeds::Many {
        remaining: values.into_iter(),
        _charge,
        _extra,
    })))
}

#[allow(clippy::too_many_arguments)]
async fn prepare_properties<H, F>(
    plan: SeedPlan<'_>,
    column: usize,
    key: PropertyKeyId,
    left: &mut NativeResultSpool,
    host: &mut H,
    cx: &QueryCx,
    policy: GqlQueryPolicy,
    source: &mut SpillFile<F>,
    left_work: &mut u64,
    max_sort_work: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<Option<Roots>>
where
    H: ExternalOrderHost,
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    host.checkpoint()?;
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    let count = usize::try_from(left.row_count()).map_err(|_| SpillError::SizeOverflow)?;
    let pool = source.memory_pool().clone();
    let mut values = match ChargedVec::<fgdb_types::CanonicalScalar>::new(&pool, cx, count) {
        Ok(values) => values,
        Err(NativeSpoolError::Spill(SpillError::Memory(MemoryError::ResourceExhausted {
            ..
        }))) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut work = Work {
        cx,
        used: *left_work,
        limit: max_sort_work,
    };
    let mut input = position(left);
    let mut seen = 0_u64;
    loop {
        let bytes = match input.next(source, cx).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(NativeSpoolError::Spill(SpillError::Memory(MemoryError::ResourceExhausted {
                ..
            }))) => {
                // This optional seed owner can make an otherwise affordable
                // page/frame decode exceed the pool. The complete left run
                // still exists and no right source has started, so release
                // the seeds and retain the ordinary lane and all spent work.
                *left_work = work.used;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        seen = seen.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        if seen > left.row_count() {
            return Err(NativeSpoolError::IncompleteCursor);
        }
        let row = match decode(bytes, &pool, &mut work, resolver) {
            Ok(row) => row,
            Err(NativeSpoolError::Spill(SpillError::Memory(MemoryError::ResourceExhausted {
                ..
            }))) => {
                *left_work = work.used;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        plan.validate(&row.row, &mut |event| {
            host::event(host, cx, policy, &mut left.evaluator, event)
        })
        .map_err(|error| match error {
            SpillSetRowError::Control(error) => error,
            SpillSetRowError::Native(error) => NativeSpoolError::SetExecution(error),
        })?;
        match &row.row.values()[column] {
            GraphValue::Scalar(fgdb_types::CanonicalScalar::Null) => {}
            GraphValue::Scalar(value) => {
                let bytes = value.owned_heap_bytes().ok_or(SpillError::SizeOverflow)?;
                match values._charge.grow(cx, bytes) {
                    Ok(()) => {}
                    Err(MemoryError::ResourceExhausted { .. }) => {
                        *left_work = work.used;
                        return Ok(None);
                    }
                    Err(error) => return Err(SpillError::Memory(error).into()),
                }
                host::event(
                    host,
                    cx,
                    policy,
                    &mut left.evaluator,
                    GlaExecutionEvent::ScratchEntry,
                )?;
                if values.values.len() == count {
                    return Err(NativeSpoolError::IncompleteCursor);
                }
                values.values.push(value.clone());
            }
            GraphValue::List(_) | GraphValue::Map { .. } => {
                // Stored collections have no scalar posting. Keep the exact
                // ordinary leaf instead of silently losing a possible match.
                *left_work = work.used;
                return Ok(None);
            }
            _ => {}
        }
    }
    if seen != left.row_count() {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    *left_work = work.used;
    let ChargedVec {
        values,
        _charge,
        _extra,
    } = values;
    Ok(Some(Roots::Property {
        key,
        values: PropertySeedValues {
            values,
            _charge,
            _extra,
        },
    }))
}
