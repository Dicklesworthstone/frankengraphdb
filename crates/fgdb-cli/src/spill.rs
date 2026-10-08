//! Native external ordering and grouping with bounded scratch and row-wise
//! delivery. A completed authenticated spool is the only result source.

use super::{
    Failure, Options, emit, execution_failure, human_value, quoted, set_decimal, value_cell,
};
use asupersync::fs::{File, Vfs};
use fgdb::{
    BufferedReadView, EmbeddedReadView, NativeAggregateSpool, NativeResultSpool,
    PreparedBufferedAggregate, PreparedBufferedOrder, PreparedNativeRead,
};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
use fgdb_types::{CanonicalScalarResolver, QueryCx};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DEFAULT_MEMORY: u64 = 64 * 1024 * 1024;
const DEFAULT_DISK: u64 = 1024 * 1024 * 1024;
const DEFAULT_INPUT_ROWS: u64 = 1_000_000;
const DEFAULT_SORT_WORK: u64 = 1_000_000_000;
const MAX_ROW_BYTES: usize = 1024 * 1024;
const PAGE_BYTES: usize = 16 * 1024;

#[derive(Default)]
pub(super) struct SpillOptions {
    directory: Option<PathBuf>,
    memory: Option<u64>,
    disk: Option<u64>,
    rows: Option<u64>,
    work: Option<u64>,
}

impl SpillOptions {
    pub(super) const FLAGS: [&str; 5] = [
        "--spill-dir",
        "--spill-memory-bytes",
        "--spill-disk-bytes",
        "--max-spill-rows",
        "--max-sort-work",
    ];

    pub(super) fn enabled(&self) -> bool {
        self.directory.is_some()
    }

    pub(super) fn set(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
        let target = match flag {
            "--spill-dir" if self.directory.is_none() && !raw.is_empty() => {
                self.directory = Some(PathBuf::from(raw));
                return Ok(());
            }
            "--spill-memory-bytes" => &mut self.memory,
            "--spill-disk-bytes" => &mut self.disk,
            "--max-spill-rows" => &mut self.rows,
            "--max-sort-work" => &mut self.work,
            _ => return Err(Failure::usage("invalid or duplicate spill flag")),
        };
        set_decimal(target, flag, raw)
    }

    pub(super) fn validate(&self) -> Result<(), Failure> {
        if !self.enabled()
            && [self.memory, self.disk, self.rows, self.work]
                .iter()
                .any(Option::is_some)
        {
            return Err(Failure::usage("spill limits require --spill-dir"));
        }
        Ok(())
    }
}

// No existing path is opened for writing or removed. Directory creation is
// exclusive; each owned file is recorded only after create_new succeeds.
// Names need uniqueness, not entropy, and a collision never authorizes reuse.
// Close/drop unlinks only these names, including when a query future is dropped.
// A crashed process may leave its private directory; it is never adopted as
// query state or swept by a later invocation.
struct ScratchOwner {
    cx: QueryCx,
    directory: PathBuf,
    files: Vec<PathBuf>,
    closed: bool,
}

impl ScratchOwner {
    fn new(cx: &QueryCx, parent: &Path) -> Result<Self, Failure> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..128 {
            cx.checkpoint().map_err(Failure::query)?;
            let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
            let directory = parent.join(format!("fgdb-spill-{}-{ordinal}", std::process::id()));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match cx.with_restriction(|| builder.create(&directory)) {
                Ok(()) => {
                    return Ok(Self {
                        cx: cx.clone(),
                        directory,
                        files: Vec::with_capacity(3),
                        closed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(Failure::io(error)),
            }
        }
        Err(Failure::io("could not reserve a private spill directory"))
    }

    fn file(&mut self, name: &str) -> Result<File, Failure> {
        self.cx.checkpoint().map_err(Failure::query)?;
        let path = self.directory.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = self
            .cx
            .with_restriction(|| options.open(&path))
            .map_err(Failure::io)?;
        self.files.push(path);
        Ok(File::from_std(file))
    }

    fn close(&mut self) -> Result<(), Failure> {
        if self.closed {
            return Ok(());
        }
        let result = self.cx.with_restriction(|| {
            let mut error = None;
            for file in &self.files {
                if let Err(found) = std::fs::remove_file(file)
                    && found.kind() != std::io::ErrorKind::NotFound
                {
                    error.get_or_insert(found);
                }
            }
            if let Err(found) = std::fs::remove_dir(&self.directory)
                && found.kind() != std::io::ErrorKind::NotFound
            {
                error.get_or_insert(found);
            }
            error.map_or(Ok(()), Err)
        });
        self.closed = result.is_ok();
        result.map_err(Failure::io)
    }
}

impl Drop for ScratchOwner {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

// Resident and buffered sources share these exact spill limits, owned files,
// result readers and retirement rules. File fields drop before their owner.
struct Scratch {
    source: SpillFile<File>,
    partition: Option<SpillFile<File>>,
    destination: SpillFile<File>,
    run_rows: usize,
    max_partitions: usize,
    max_runs: usize,
    input_rows: u64,
    owner: ScratchOwner,
}

impl Scratch {
    async fn new(cx: &QueryCx, options: &Options, aggregate: bool) -> Result<Self, Failure> {
        let limits = &options.spill;
        let directory = limits
            .directory
            .as_deref()
            .ok_or_else(|| Failure::usage("--spill-dir required"))?;
        let memory = usize::try_from(limits.memory.unwrap_or(DEFAULT_MEMORY))
            .map_err(|_| Failure::usage("spill memory limit exceeds this platform"))?;
        let file_count = if aggregate { 3 } else { 2 };
        let per_file = limits.disk.unwrap_or(DEFAULT_DISK) / file_count;
        if memory == 0 || per_file == 0 {
            return Err(Failure::query(
                "ResourceExhausted: spill needs resident memory and a nonzero allowance for every file",
            ));
        }
        let pool = MemoryPool::new(memory, 0).map_err(execution_failure)?;
        let input_rows = limits.rows.unwrap_or(DEFAULT_INPUT_ROWS);
        let run_rows = (memory / (4 * MAX_ROW_BYTES)).clamp(1, 256);
        let max_runs = usize::try_from(input_rows.div_ceil(run_rows as u64).max(1))
            .map_err(|_| Failure::usage("spill run count exceeds this platform"))?;
        // Initial runs, pairwise merge passes, parity copy and final window all
        // spend append attempts; the allowance remains finite even for empty input.
        let append_runs = u64::try_from(max_runs)
            .ok()
            .and_then(|runs| runs.checked_mul(4))
            .and_then(|runs| runs.checked_add(16))
            .ok_or_else(|| Failure::usage("spill append count exceeds this platform"))?;
        let max_partitions = usize::try_from(append_runs)
            .map_err(|_| Failure::usage("spill partition count exceeds this platform"))?;
        let append_runs = if aggregate {
            // Completed-group clauses can sort canonical keys, DISTINCT equality
            // classes, and final representative rank. Every pass retains the same
            // byte/work quotas; only the finite append-attempt envelope expands.
            append_runs
                .checked_mul(3)
                .and_then(|runs| runs.checked_add(32))
                .and_then(|runs| runs.checked_add(u64::try_from(max_partitions).ok()?))
                .ok_or_else(|| Failure::usage("spill append count exceeds this platform"))?
        } else {
            append_runs
        };
        let file_limits = SpillLimits {
            max_file_bytes: per_file,
            max_runs: append_runs,
            max_run_bytes: usize::try_from(per_file).unwrap_or(usize::MAX),
        };
        let mut owner = ScratchOwner::new(cx, directory)?;
        // Declaration order matters: dropping this async frame drops the files
        // before their namespace owner, including cancellation during an append.
        let source = SpillFile::new(cx, owner.file("scratch")?, pool.clone(), file_limits)
            .await
            .map_err(execution_failure)?;
        let partition = if aggregate {
            Some(
                SpillFile::new(cx, owner.file("partitions")?, pool.clone(), file_limits)
                    .await
                    .map_err(execution_failure)?,
            )
        } else {
            None
        };
        let destination = SpillFile::new(cx, owner.file("result")?, pool, file_limits)
            .await
            .map_err(execution_failure)?;
        Ok(Self {
            source,
            partition,
            destination,
            run_rows,
            max_partitions,
            max_runs,
            input_rows,
            owner,
        })
    }

    fn complete(
        self,
        outcome: Result<(u64, u64), Failure>,
        cx: &QueryCx,
        robot: bool,
        out: &mut impl Write,
    ) -> Result<(), Failure> {
        let Self {
            source,
            partition,
            destination,
            mut owner,
            ..
        } = self;
        // Success is withheld until scratch retirement succeeds. A transport or
        // query error still retires every file, preserving that original error.
        drop(destination);
        drop(partition);
        drop(source);
        let cleanup = owner.close();
        let (seq, sent) = outcome?;
        cleanup?;
        cx.checkpoint().map_err(Failure::query)?;
        if robot {
            emit(
                out,
                &format!(
                    r#"{{"v":1,"event":"result","kind":"rows","stream":true,"seq":{seq},"count":{sent}}}"#
                ),
            )?;
        } else {
            writeln!(out, "{sent} row(s) (external query complete at seq {seq})")
                .map_err(Failure::io)?;
        }
        out.flush().map_err(Failure::io)
    }
}

pub(super) async fn run(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    options: &Options,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let prepared = PreparedNativeRead::prepare(&options.text, &options.params, options)
        .map_err(execution_failure)?;
    let aggregate = is_aggregate(&prepared);
    let mut scratch = Scratch::new(cx, options, aggregate).await?;
    let outcome = async {
        if let Some(partition) = scratch.partition.as_mut() {
            return run_aggregate(
                &prepared,
                view,
                cx,
                options,
                resolver,
                &mut scratch.source,
                partition,
                &mut scratch.destination,
                scratch.run_rows,
                scratch.max_partitions,
                scratch.max_runs,
                scratch.input_rows,
                robot,
                out,
            )
            .await;
        }
        let (spool, _) = prepared
            .spool_ordered_in_view(
                view,
                cx,
                &options.params,
                options.budget.policy(),
                &mut scratch.source,
                &mut scratch.destination,
                scratch.run_rows,
                scratch.max_runs,
                PAGE_BYTES,
                MAX_ROW_BYTES,
                scratch.input_rows,
                options.spill.work.unwrap_or(DEFAULT_SORT_WORK),
            )
            .await
            .map_err(execution_failure)?;
        deliver_ordered(&spool, &mut scratch.destination, cx, resolver, robot, out).await
    }
    .await;
    scratch.complete(outcome, cx, robot, out)
}

fn is_aggregate(prepared: &PreparedNativeRead) -> bool {
    matches!(
        prepared,
        PreparedNativeRead::Aggregate(_)
            | PreparedNativeRead::TemporalAggregate(_)
            | PreparedNativeRead::PipelineAggregate(_)
    )
}

/// Bind and admit the external physical definition before storage opens. The
/// native statement class selects one compiler; errors never trigger retries.
pub(super) enum PreparedBuffered {
    Ordered(PreparedBufferedOrder),
    Aggregate(PreparedBufferedAggregate),
}

pub(super) fn prepare_buffered(options: &Options) -> Result<PreparedBuffered, Failure> {
    let prepared = PreparedNativeRead::prepare(&options.text, &options.params, options)
        .map_err(execution_failure)?;
    if is_aggregate(&prepared) {
        prepared
            .prepare_buffered_aggregate(&options.params)
            .map(PreparedBuffered::Aggregate)
            .map_err(execution_failure)
    } else {
        prepared
            .prepare_buffered_order(&options.params)
            .map(PreparedBuffered::Ordered)
            .map_err(execution_failure)
    }
}

pub(super) async fn run_buffered<V: Vfs + Clone>(
    view: &mut BufferedReadView<V>,
    cx: &QueryCx,
    options: &Options,
    prepared: &PreparedBuffered,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let aggregate = matches!(prepared, PreparedBuffered::Aggregate(_));
    let mut scratch = Scratch::new(cx, options, aggregate).await?;
    let outcome = async {
        match prepared {
            PreparedBuffered::Ordered(prepared) => {
                let (spool, _) = prepared
                    .spool_in_view(
                        view,
                        cx,
                        options.budget.policy(),
                        &mut scratch.source,
                        &mut scratch.destination,
                        scratch.run_rows,
                        scratch.max_runs,
                        PAGE_BYTES,
                        MAX_ROW_BYTES,
                        scratch.input_rows,
                        options.spill.work.unwrap_or(DEFAULT_SORT_WORK),
                    )
                    .await
                    .map_err(execution_failure)?;
                deliver_ordered(&spool, &mut scratch.destination, cx, resolver, robot, out).await
            }
            PreparedBuffered::Aggregate(prepared) => {
                let partition = scratch.partition.as_mut().ok_or_else(|| {
                    Failure::query("aggregate spill is missing its private partition file")
                })?;
                let (spool, _) = prepared
                    .spool_in_view(
                        view,
                        cx,
                        options.budget.policy(),
                        &mut scratch.source,
                        partition,
                        &mut scratch.destination,
                        scratch.run_rows,
                        scratch.max_partitions,
                        scratch.run_rows,
                        scratch.max_runs,
                        PAGE_BYTES,
                        MAX_ROW_BYTES,
                        scratch.input_rows,
                        options.spill.work.unwrap_or(DEFAULT_SORT_WORK),
                        resolver,
                    )
                    .await
                    .map_err(execution_failure)?;
                deliver_aggregate(&spool, &mut scratch.destination, cx, resolver, robot, out).await
            }
        }
    }
    .await;
    scratch.complete(outcome, cx, robot, out)
}

async fn deliver_ordered(
    spool: &NativeResultSpool,
    destination: &mut SpillFile<File>,
    cx: &QueryCx,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
    robot: bool,
    out: &mut impl Write,
) -> Result<(u64, u64), Failure> {
    let seq = spool.snapshot_seq().0;
    cx.checkpoint().map_err(Failure::query)?;
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"columns","stream":true,"seq":{seq},"columns":[{}]}}"#,
                spool
                    .columns()
                    .iter()
                    .map(|name| quoted(name))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        )?;
    } else {
        emit(
            out,
            &spool
                .columns()
                .iter()
                .map(|name| {
                    name.chars()
                        .flat_map(char::escape_default)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\t"),
        )?;
    }
    out.flush().map_err(Failure::io)?;
    let mut reader = spool.reader(destination);
    let mut sent = 0_u64;
    loop {
        cx.checkpoint().map_err(Failure::query)?;
        let Some(frame) = reader.next_row(cx).await.map_err(execution_failure)? else {
            break;
        };
        let row = match resolver {
            Some(resolver) => {
                GraphValueRow::decode_canonical_with_resolver(frame.as_ref(), resolver)
            }
            None => GraphValueRow::decode_canonical(frame.as_ref()),
        }
        .map_err(execution_failure)?;
        if row.values().len() != spool.columns().len() {
            return Err(Failure::query("spill row does not match its native layout"));
        }
        let cells = row
            .values()
            .iter()
            .map(|value| {
                cx.checkpoint().map_err(Failure::query)?;
                if robot {
                    value_cell(value)
                } else {
                    human_value(value)
                }
            })
            .collect::<Result<Vec<_>, Failure>>()?;
        cx.checkpoint().map_err(Failure::query)?;
        if robot {
            emit(
                out,
                &format!(r#"{{"v":1,"event":"row","cells":[{}]}}"#, cells.join(",")),
            )?;
        } else {
            emit(out, &cells.join("\t"))?;
        }
        out.flush().map_err(Failure::io)?;
        sent = sent
            .checked_add(1)
            .ok_or_else(|| Failure::query("spill delivery counter overflow"))?;
    }
    if sent != spool.row_count() {
        return Err(Failure::query("incomplete spill result"));
    }
    Ok((seq, sent))
}

#[allow(clippy::too_many_arguments)]
async fn run_aggregate(
    prepared: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &QueryCx,
    options: &Options,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
    source: &mut SpillFile<File>,
    partition: &mut SpillFile<File>,
    destination: &mut SpillFile<File>,
    group_capacity: usize,
    max_partitions: usize,
    max_runs: usize,
    input_rows: u64,
    robot: bool,
    out: &mut impl Write,
) -> Result<(u64, u64), Failure> {
    let (spool, _) = prepared
        .spool_aggregate_in_view(
            view,
            cx,
            &options.params,
            options.budget.policy(),
            source,
            partition,
            destination,
            group_capacity,
            max_partitions,
            group_capacity,
            max_runs,
            PAGE_BYTES,
            MAX_ROW_BYTES,
            input_rows,
            options.spill.work.unwrap_or(DEFAULT_SORT_WORK),
            resolver,
        )
        .await
        .map_err(execution_failure)?;
    deliver_aggregate(&spool, destination, cx, resolver, robot, out).await
}

async fn deliver_aggregate(
    spool: &NativeAggregateSpool,
    destination: &mut SpillFile<File>,
    cx: &QueryCx,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
    robot: bool,
    out: &mut impl Write,
) -> Result<(u64, u64), Failure> {
    let seq = spool.snapshot_seq().0;
    let columns = spool.columns();
    let slots = spool.output_slots();
    if columns.len() != slots.len() {
        return Err(Failure::query(
            "aggregate spill has an invalid native layout",
        ));
    }
    cx.checkpoint().map_err(Failure::query)?;
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"columns","stream":true,"seq":{seq},"columns":[{}]}}"#,
                columns
                    .iter()
                    .map(|name| quoted(name))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        )?;
    } else {
        emit(
            out,
            &columns
                .iter()
                .map(|name| {
                    name.chars()
                        .flat_map(char::escape_default)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\t"),
        )?;
    }
    out.flush().map_err(Failure::io)?;
    let mut reader = spool.reader(destination, resolver);
    let mut sent = 0_u64;
    loop {
        cx.checkpoint().map_err(Failure::query)?;
        let Some(row) = reader.next_row(cx).await.map_err(execution_failure)? else {
            break;
        };
        let mut cells = Vec::with_capacity(slots.len());
        for slot in slots {
            cx.checkpoint().map_err(Failure::query)?;
            cells.push(super::stream::aggregate_cell(&row, slot, robot)?);
        }
        cx.checkpoint().map_err(Failure::query)?;
        if robot {
            emit(
                out,
                &format!(r#"{{"v":1,"event":"row","cells":[{}]}}"#, cells.join(",")),
            )?;
        } else {
            emit(out, &cells.join("\t"))?;
        }
        out.flush().map_err(Failure::io)?;
        sent = sent
            .checked_add(1)
            .ok_or_else(|| Failure::query("spill delivery counter overflow"))?;
    }
    if sent != spool.row_count() {
        return Err(Failure::query("incomplete aggregate spill result"));
    }
    Ok((seq, sent))
}

#[cfg(test)]
mod tests;
