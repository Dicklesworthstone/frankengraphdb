//! Native external ordering and grouping with bounded scratch and row-wise
//! delivery. A completed authenticated spool is the only result source.

use super::{
    Failure, Options, emit, execution_failure, human_value, quoted, set_decimal, value_cell,
};
use asupersync::fs::File;
use fgdb::{EmbeddedReadView, PreparedNativeRead};
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
    let aggregate = matches!(
        &prepared,
        PreparedNativeRead::Aggregate(_)
            | PreparedNativeRead::TemporalAggregate(_)
            | PreparedNativeRead::PipelineAggregate(_)
    );
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
        append_runs
            .checked_add(
                u64::try_from(max_partitions)
                    .map_err(|_| Failure::usage("spill partition count exceeds this platform"))?,
            )
            .and_then(|runs| runs.checked_add(32))
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
    let mut scratch = SpillFile::new(cx, owner.file("scratch")?, pool.clone(), file_limits)
        .await
        .map_err(execution_failure)?;
    let mut partition = if aggregate {
        Some(
            SpillFile::new(cx, owner.file("partitions")?, pool.clone(), file_limits)
                .await
                .map_err(execution_failure)?,
        )
    } else {
        None
    };
    let mut destination = SpillFile::new(cx, owner.file("result")?, pool, file_limits)
        .await
        .map_err(execution_failure)?;
    let outcome = async {
        if let Some(partition) = partition.as_mut() {
            return run_aggregate(
                &prepared,
                view,
                cx,
                options,
                resolver,
                &mut scratch,
                partition,
                &mut destination,
                run_rows,
                max_partitions,
                max_runs,
                input_rows,
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
                &mut scratch,
                &mut destination,
                run_rows,
                max_runs,
                PAGE_BYTES,
                MAX_ROW_BYTES,
                input_rows,
                limits.work.unwrap_or(DEFAULT_SORT_WORK),
            )
            .await
            .map_err(execution_failure)?;
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
        let mut reader = spool.reader(&mut destination);
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
        Ok::<_, Failure>((seq, sent))
    }
    .await;
    // Success is withheld until scratch retirement succeeds. A transport or
    // query error still retires every file, preserving that original error.
    drop(destination);
    drop(partition);
    drop(scratch);
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
