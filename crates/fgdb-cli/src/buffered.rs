//! CLI demand and transport over the public extent-buffered query source.
//! Native preparation and physical admission happen before opening storage;
//! each returned row keeps its resident guard until its output is flushed.

use super::{Failure, Options, emit, execution_failure, set_decimal, stream};
use asupersync::fs::Vfs;
use fgdb::{
    BufferedOpenError, BufferedReadLimits, BufferedReadView, MemoryPool, PreparedNativeRead,
};
use fgdb_gql::GqlQueryPolicy;
use fgdb_gql::algebra::{GlaOperator, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::edge_stream::AsyncEdgeScanPlan;
use fgdb_gql::stream::AsyncVertexScanPlan;
use fgdb_types::{CommitSeq, QueryCx};
use std::io::Write;

const DEFAULT_MEMORY: u64 = 64 * 1024 * 1024;
const DEFAULT_SOURCE: u64 = 1024 * 1024 * 1024;

#[derive(Default)]
pub(super) struct BufferedOptions {
    enabled: bool,
    memory: Option<u64>,
    source: Option<u64>,
}

impl BufferedOptions {
    pub(super) const FLAGS: [&str; 2] = ["--buffer-memory-bytes", "--buffer-source-bytes"];

    pub(super) fn enable(&mut self) -> Result<(), Failure> {
        if self.enabled {
            return Err(Failure::usage("--buffered must be supplied only once"));
        }
        self.enabled = true;
        Ok(())
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(super) fn set(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
        let value = match flag {
            "--buffer-memory-bytes" => &mut self.memory,
            "--buffer-source-bytes" => &mut self.source,
            _ => return Err(Failure::usage("unknown buffered-read limit")),
        };
        set_decimal(value, flag, raw)
    }

    pub(super) fn validate(&self) -> Result<(), Failure> {
        if !self.enabled && (self.memory.is_some() || self.source.is_some()) {
            return Err(Failure::usage("buffer limits require --buffered"));
        }
        Ok(())
    }

    pub(super) fn admission(
        &self,
        policy: GqlQueryPolicy,
    ) -> Result<(MemoryPool, BufferedReadLimits), Failure> {
        let size = |value, name| {
            usize::try_from(value).map_err(|_| {
                Failure::usage(format!("{name} exceeds this platform's byte/work range"))
            })
        };
        let memory = size(
            self.memory.unwrap_or(DEFAULT_MEMORY),
            "--buffer-memory-bytes",
        )?;
        let root = (memory / 64).min(1024 * 1024);
        let source = size(
            self.source.unwrap_or(DEFAULT_SOURCE),
            "--buffer-source-bytes",
        )?;
        let work = size(policy.evaluator.max_work_units, "--max-work-units")?;
        Ok((
            MemoryPool::new(memory, 0).map_err(Failure::open)?,
            BufferedReadLimits {
                max_root_bytes: root,
                max_source_bytes: source,
                // Every reference consumes more than one encoded byte. These
                // explicit count bounds precede V4 root-segment flattening.
                max_blocks: root,
                max_vertex_patches: root,
                max_work: work,
                buffer: fgdb::BufferLimits {
                    max_frames: 32,
                    max_ghost_entries: 64,
                    max_extent_bytes: 16 * 1024,
                },
            },
        ))
    }
}

pub(super) struct Prepared {
    pattern: PreparedGraphPattern<GraphValueRow>,
    as_of: Option<CommitSeq>,
    edge: bool,
}

pub(super) fn prepare(options: &Options) -> Result<Prepared, Failure> {
    let native = PreparedNativeRead::prepare(&options.text, &options.params, options)
        .map_err(execution_failure)?;
    let (pattern, as_of) = match native {
        PreparedNativeRead::Pattern(template) => (
            template
                .bind_parameters(&options.params)
                .map_err(execution_failure)?,
            None,
        ),
        PreparedNativeRead::TemporalPattern(template) => {
            let bound = template
                .bind_parameters(&options.params)
                .map_err(execution_failure)?;
            (bound.pattern().clone(), Some(bound.as_of()))
        }
        _ => {
            return Err(Failure::query(
                "--buffered supports native single-vertex and single-edge scans, not aggregate or set queries",
            ));
        }
    };
    let edge = matches!(
        pattern.plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    );
    // Validate the whole physical definition before graph recovery/open. LIMIT
    // 0 cannot hide an unsupported probe, join, projection or ordering.
    if edge {
        AsyncEdgeScanPlan::compile(pattern.plan()).map_err(execution_failure)?;
    } else {
        AsyncVertexScanPlan::compile(pattern.plan()).map_err(execution_failure)?;
    }
    Ok(Prepared {
        pattern,
        as_of,
        edge,
    })
}

pub(super) fn open_failure(error: BufferedOpenError) -> Failure {
    match error {
        BufferedOpenError::Open(error) => super::open_failure(error),
        error => Failure::open(error),
    }
}

pub(super) async fn run<V: Vfs + Clone>(
    view: &mut BufferedReadView<V>,
    cx: &QueryCx,
    options: &Options,
    prepared: &Prepared,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let seq = prepared.as_of.unwrap_or(view.frontier());
    let columns = prepared.pattern.columns();
    if prepared.edge {
        let mut cursor = view
            .stream_graph_edges_governed_at(cx, &prepared.pattern, seq, options.budget.policy())
            .map_err(execution_failure)?;
        let result = deliver(
            columns,
            seq.0,
            async || cursor.next().await,
            robot,
            out,
            || cx.checkpoint().map_err(Failure::query),
        )
        .await;
        cursor.close();
        result
    } else {
        let mut cursor = view
            .stream_graph_values_governed_at(cx, &prepared.pattern, seq, options.budget.policy())
            .map_err(execution_failure)?;
        let result = deliver(
            columns,
            seq.0,
            async || cursor.next().await,
            robot,
            out,
            || cx.checkpoint().map_err(Failure::query),
        )
        .await;
        cursor.close();
        result
    }
}

// This is a delivery seam, not a new graph source. The closure makes exactly
// one pull from the selected native async cursor. AsRef borrows the value from
// its owner; the owner and its MemoryCharge live through encoding and flush.
async fn deliver<Row: AsRef<GraphValueRow>, E: std::error::Error + 'static>(
    columns: &[String],
    seq: u64,
    mut next: impl AsyncFnMut() -> Option<Result<Row, E>>,
    robot: bool,
    out: &mut impl Write,
    mut checkpoint: impl FnMut() -> Result<(), Failure>,
) -> Result<(), Failure> {
    let mut sent = 0u64;
    let result = async {
        checkpoint()?;
        emit(out, &stream::header_line(columns, seq, robot))?;
        out.flush().map_err(Failure::io)?;
        loop {
            checkpoint()?;
            let Some(row) = next().await else { break };
            let row = row.map_err(execution_failure)?;
            if row.as_ref().values().len() != columns.len() {
                return Err(Failure::query(
                    "stream row width does not match native columns",
                ));
            }
            let count = sent
                .checked_add(1)
                .ok_or_else(|| Failure::query("stream delivery counter overflow"))?;
            let line = stream::row_line(row.as_ref(), robot, &mut checkpoint)?;
            checkpoint()?;
            emit(out, &line)?;
            out.flush().map_err(Failure::io)?;
            sent = count;
            drop(row);
        }
        checkpoint()?;
        emit(out, &stream::summary_line(seq, sent, robot))?;
        out.flush().map_err(Failure::io)
    }
    .await;
    result.map_err(|error| stream::incomplete(error, sent))
}

#[cfg(test)]
mod tests;
