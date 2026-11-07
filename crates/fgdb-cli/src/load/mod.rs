//! NDJSON adaptation and durable continuation; all writes use the bulk engine.
//! Source bytes are replayed from a sealed file handle; decoded payloads are
//! retained for only one bounded engine chunk (one row during reconciliation).
//! Key maps and block digests still scale with the admitted input. This is not
//! a claim that the database itself has an external-memory execution engine.
//!
//! Saved V1/V2 checkpoints are lagging resume hints. Publish the initial/recovered
//! prefix, then chunk counts 1, 2, 4, 8, ... and the complete source. Chronicle
//! still commits every chunk. A resumed invocation may replay progress records
//! already delivered; their sequence numbers identify the same durable chunks.
mod checkpoint;
mod input;
use super::{Failure, Options, emit, hex, parameter};
use asupersync::fs::Vfs;
use fgdb::{
    BulkEdge, BulkEndpoint, BulkLoadCheckpoint, BulkLoadErrorKind, BulkLoadPolicy, BulkRow,
    BulkVertex, ChunkFit, Database,
};
use fgdb_delta_types::{DeltaRow, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::GqlParameterValue;
/// The strict, dependency-free JSON grammar now lives with the wire codecs.
pub(crate) use fgdb_protocol::json::{Json, parse_json};
use fgdb_protocol::json::{JsonParseError, parse_json_controlled};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, CommitSeq, EId, PurposeContexts, VId};
use std::{
    collections::{BTreeMap, HashSet},
    io::{self, Write},
};

fn invalid(message: impl std::fmt::Display) -> Failure {
    Failure::query(format!("InvalidResume: {message}"))
}

// Chronicle effects and source rows agree modulo a top-level NULL assignment,
// which means absence. Keep both source rows and their raw hashes untouched;
// nested NULL values and every nonnull property still compare exactly.
fn same_logical_props(
    left: &[(PropertyKeyId, CanonicalPropertyValue)],
    right: &[(PropertyKeyId, CanonicalPropertyValue)],
) -> bool {
    let present = |entry: &&(PropertyKeyId, CanonicalPropertyValue)| {
        !matches!(
            &entry.1,
            CanonicalPropertyValue::Scalar(CanonicalScalar::Null)
        )
    };
    left.iter().filter(present).eq(right.iter().filter(present))
}
fn line_error(line: usize, kind: &str, message: impl std::fmt::Display) -> Failure {
    Failure::query(format!("line {line}: {kind}: {message}"))
}

/// An endpoint in the input's terms: its label and property by binding name,
/// not by internal ID.
fn spelled(endpoint: &BulkEndpoint, options: &Options) -> String {
    let named = |bindings: &BTreeMap<String, u64>, id: u64| {
        bindings
            .iter()
            .find(|(_, bound)| **bound == id)
            .map_or_else(|| format!("#{id}"), |(name, _)| name.clone())
    };
    match endpoint {
        BulkEndpoint::Key(key) => format!("key {key:?}"),
        BulkEndpoint::Match(found) => {
            let label = found
                .label
                .map(|label| format!("label {}, ", named(&options.labels, label.0)))
                .unwrap_or_default();
            format!(
                "{{{label}property {}, value {:?}}}",
                named(&options.properties, found.key.0),
                found.value
            )
        }
    }
}

pub(super) async fn run<V: Vfs + Clone>(
    db: &mut Database<V>,
    contexts: &PurposeContexts,
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let cx = contexts.query();
    let mut policy = BulkLoadPolicy::new(options.rows_per_chunk.unwrap_or(1), options.coordinate);
    if policy.rows_per_chunk == 0 || policy.rows_per_chunk > BulkLoadPolicy::MAX_ROWS_PER_CHUNK {
        return Err(Failure::usage("invalid rows-per-chunk"));
    }
    if let Some(rows) = options.max_source_rows {
        policy.max_source_rows = rows;
    }
    if let Some(bytes) = options.max_total_key_bytes {
        policy.max_total_key_bytes = bytes;
    }
    // Without --rows-per-chunk, fit chunks to the source (fgdb-hkp7k): every
    // chunk within the preflight's transcript cap, and its capsule, which a
    // V1 capsule holds to 14.4 MB, within ChunkFit::TEMPLATE_BUDGET by each
    // row's exact template bytes. Fitting to half the transcript cap as a
    // proxy for the capsule held road-ny to 32,768-row chunks: 31 commits
    // where 16 suffice, each with ~110 ms of fixed publication cost.
    let mut fit = ChunkFit::new(policy.max_chunk_bytes);
    // Preserve Saved V1's full raw source/binding identity while sealing it
    // without keeping the entire source file or its decoded rows resident.
    let binding = format!(
        "\n{:?}\n{:?}\n{:?}\n{}",
        options.labels, options.relations, options.properties, options.coordinate.0
    );
    let source = input::Input::open(
        &cx,
        options.input.as_ref().expect("required input"),
        binding.as_bytes(),
        policy.max_source_rows,
    )
    .map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidData {
            Failure::query(error.to_string())
        } else {
            Failure::io(error)
        }
    })?;
    let rows = Rows::new(source.reader(), &cx, options, resolver);
    // Validate all format/shape/key failures before touching a checkpoint. Drop
    // these temporary key sets before recovery maps or the engine's preflight.
    // They are only probed, never iterated, so their order reaches nothing: a
    // hashed set spares the string comparisons of an ordered one, most of this
    // pass on a 6M-row source (fgdb-79kyi).
    let key_bytes = {
        let mut keys = HashSet::new();
        let mut vertices = HashSet::new();
        let mut key_bytes = 0usize;
        for (index, row) in rows.clone().enumerate() {
            let row = row.map_err(|e| e.failure())?;
            let key = match &row {
                BulkRow::Vertex(v) => &v.key,
                BulkRow::Edge(e) => &e.key,
            };
            if key.len() > policy.max_key_bytes {
                return Err(line_error(index + 1, "SourceLimit", "key_bytes"));
            }
            if keys.contains(key) {
                return Err(line_error(index + 1, "DuplicateCallerKey", key));
            }
            key_bytes = key_bytes
                .checked_add(key.len())
                .filter(|&bytes| bytes <= policy.max_total_key_bytes)
                .ok_or_else(|| line_error(index + 1, "SourceLimit", "total_key_bytes"))?;
            keys.insert(key.clone());
            if options.rows_per_chunk.is_none() {
                fit.push(&cx, &row).map_err(|kind| match kind {
                    BulkLoadErrorKind::Write(error) => super::execution_failure(error),
                    kind => line_error(index + 1, "BulkLoad", format!("{kind:?}")),
                })?;
            }
            match &row {
                BulkRow::Vertex(v) => {
                    fgdb_strata::vertex::admit_row_content(&v.labels, &v.props)
                        .map_err(|e| line_error(index + 1, "InvalidVertex", format!("{e:?}")))?;
                    vertices.insert(v.key.clone());
                }
                BulkRow::Edge(e) => {
                    // A match endpoint names a committed vertex; the engine's
                    // preflight resolves it before anything commits.
                    for endpoint in [&e.source, &e.destination]
                        .into_iter()
                        .filter_map(|e| e.key())
                    {
                        if endpoint.len() > policy.max_key_bytes {
                            return Err(line_error(index + 1, "SourceLimit", "key_bytes"));
                        }
                        if !vertices.contains(endpoint) {
                            return Err(line_error(index + 1, "DanglingEndpointKey", endpoint));
                        }
                    }
                    fgdb_strata::edge_props::admitted_row_bytes(&e.props)
                        .map_err(|e| line_error(index + 1, "InvalidEdge", format!("{e:?}")))?;
                }
            }
        }
        key_bytes
    };
    let checkpoint_limits =
        checkpoint::Limits::new(source.records(), key_bytes, policy.max_key_bytes)
            .map_err(invalid)?;
    let source_hash = hex(&source.source_hash().0);
    let current = db.frontier().map_err(Failure::io)?;
    let saved = if let Some(path) = &options.checkpoint {
        match checkpoint::read(&cx, path, checkpoint_limits) {
            Ok(Some(text)) => Some(Saved::decode(&text, checkpoint_limits).map_err(invalid)?),
            Ok(None) => None,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => return Err(invalid(e)),
            Err(e) => return Err(Failure::io(e)),
        }
    } else {
        None
    };
    // A checkpoint's committed chunks were cut at its own rows_per_chunk, so
    // a resume without --rows-per-chunk takes it rather than refitting: the
    // fit may differ from the binary that wrote it (the template budget
    // doubled road-ny's chunks). A row past the fitted budget still loads
    // alone when it fits the cap; past the cap, preflight refuses it with its
    // typed chunk_bytes error.
    let rows_per_chunk = match (options.rows_per_chunk, &saved) {
        (Some(rows), _) => rows,
        (None, Some(saved)) => saved.rows_per_chunk,
        (None, None) => fit.rows_per_chunk().unwrap_or(1),
    };
    policy.rows_per_chunk = rows_per_chunk;
    let base = if let Some(saved) = &saved {
        if saved.source_hash != source_hash || saved.rows_per_chunk != rows_per_chunk {
            return Err(invalid("source or chunk policy changed"));
        }
        if saved.checkpoint.frontier.0 > current.0 || saved.base.0 > saved.checkpoint.frontier.0 {
            return Err(invalid("checkpoint frontier is not in recovered history"));
        }
        saved.base
    } else {
        current
    };
    // Checkpoint open retains graph state without eagerly decoding historical
    // capsules. Continuation needs the complete suffix plus the base marker's
    // own batch; make that I/O requirement explicit before replaying key maps.
    db.ensure_delta_window(&contexts.commit(), CommitSeq(base.0.saturating_sub(1)))
        .await
        .map_err(invalid)?;
    let base_marker = match &saved {
        Some(saved) => saved.base_marker.clone(),
        None => marker(db, base)?,
    };
    if marker(db, base)? != base_marker {
        return Err(invalid("base history identity changed"));
    }
    let mut checkpoint = BulkLoadCheckpoint {
        frontier: base,
        ..BulkLoadCheckpoint::default()
    };
    // The checkpoint identity transcript advances once through the sealed
    // source, rather than scanning/serializing all prior maps after each chunk.
    let mut prefix = checkpoint::Prefix::new(rows.clone());
    if let Some(saved) = &saved {
        if saved.checkpoint.frontier == base {
            same_saved_checkpoint(saved, &checkpoint, &mut prefix)?;
        }
        // Reconstruct maps from authenticated creation effects, including the
        // commit -> checkpoint crash window. No checkpoint guesses an identity.
        let mut replay = rows.clone();
        let order = IssueOrder {
            vertices: db.vertex_identity_permutation(),
            edges: db.edge_identity_permutation(),
        };
        for batch in db.delta_since(base).map_err(invalid)? {
            cx.checkpoint().map_err(Failure::io)?;
            reconcile(
                batch,
                &mut replay,
                source.records(),
                rows_per_chunk,
                &mut checkpoint,
                &order,
            )?;
            if checkpoint.frontier == saved.checkpoint.frontier {
                same_saved_checkpoint(saved, &checkpoint, &mut prefix)?;
            }
        }
    }
    let mut save = |cp: &BulkLoadCheckpoint, force: bool| -> io::Result<()> {
        if let Some(path) = &options.checkpoint {
            cx.checkpoint().map_err(io::Error::other)?;
            if !force && !checkpoint::due(cp.committed_chunks, cp.next_row, source.records()) {
                return Ok(());
            }
            let identity_hash = prefix.advance(cp)?;
            let saved = checkpoint::View {
                checkpoint: cp,
                base,
                base_marker: &base_marker,
                source_hash: &source_hash,
                rows_per_chunk,
                identity_hash: Some(&identity_hash),
            };
            checkpoint::persist(&contexts.commit(), path, saved, checkpoint_limits)?;
        }
        Ok(())
    };
    let recovery_start = saved
        .as_ref()
        .map(|saved| saved.checkpoint.committed_chunks);
    let source_rows = source.records();
    // Map/commitment reconciliation alone is not admission: Match endpoints
    // and the complete source's chunk-byte limits are validated by the engine.
    // Do not overwrite a hint or acknowledge reconstructed history until that
    // preflight has accepted it. No second identity map is retained by the hook.
    drop(saved);
    let crash = match std::env::var("FGDB_LOAD_CRASH_CHUNK") {
        Ok(value) => {
            let index = value
                .parse()
                .map_err(|_| Failure::usage("invalid FGDB_LOAD_CRASH_CHUNK"))?;
            let point = match std::env::var("FGDB_LOAD_CRASH_POINT").as_deref() {
                Ok("before-capsule") => fgdb::CrashPoint::BeforeCapsule,
                Ok("after-d1") => fgdb::CrashPoint::AfterD1,
                Ok("after-marker-sync") | Err(_) => {
                    fgdb::CrashPoint::AfterMarkerFileSyncBeforeDirectorySync
                }
                _ => return Err(Failure::usage("invalid FGDB_LOAD_CRASH_POINT")),
            };
            Some((index, point))
        }
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err(Failure::usage("invalid FGDB_LOAD_CRASH_CHUNK")),
    };
    policy.resume = Some(checkpoint);
    // Move a reborrow through the two callbacks, not the caller's reference:
    // final result delivery below still owns `out` after ingestion completes.
    let progress_out = &mut *out;
    let result = db
        .try_bulk_load_with_admission(&cx, &contexts.commit(), rows, policy, crash, |validated| {
            // Force the origin or fully validated recovered prefix before
            // any new write and before replaying its progress. A crash in
            // the first write retains the original import basis as before.
            save(validated, true)?;
            if let Some(start) = recovery_start {
                for chunk in start..validated.committed_chunks {
                    let count = (chunk + 1).saturating_mul(rows_per_chunk).min(source_rows);
                    let seq = base.0 + chunk as u64 + 1;
                    if robot {
                        writeln!(
                            progress_out,
                            "{{\"v\":1,\"event\":\"progress\",\"rows\":{count},\"seq\":{seq}}}"
                        )?;
                    } else {
                        writeln!(progress_out, "loaded {count} rows (seq {seq})")?;
                    }
                    progress_out.flush()?;
                }
            }
            // Ownership passes from admission to per-commit delivery, so
            // both phases share one checkpoint cursor and output writer.
            Ok(move |cp: &BulkLoadCheckpoint| {
                save(cp, false)?;
                if robot {
                    writeln!(
                        progress_out,
                        "{{\"v\":1,\"event\":\"progress\",\"rows\":{},\"seq\":{}}}",
                        cp.next_row, cp.frontier.0
                    )?;
                } else {
                    writeln!(
                        progress_out,
                        "loaded {} rows (seq {})",
                        cp.next_row, cp.frontier.0
                    )?;
                }
                progress_out.flush()
            })
        })
        .await
        .map_err(|e| match e.kind {
            BulkLoadErrorKind::Checkpoint(error) => Failure::io(error),
            BulkLoadErrorKind::Write(error) => super::execution_failure(error),
            BulkLoadErrorKind::Source { row, source } => {
                if let Some(source) = source.downcast_ref::<RowError>() {
                    source.failure()
                } else {
                    line_error(row.saturating_add(1), "Source", source)
                }
            }
            BulkLoadErrorKind::UnresolvedEndpoint {
                row,
                edge,
                endpoint,
            } => line_error(
                row.saturating_add(1),
                "UnresolvedEndpoint",
                format!(
                    "edge {edge}: no committed vertex matches {}",
                    spelled(&endpoint, options)
                ),
            ),
            BulkLoadErrorKind::AmbiguousEndpoint {
                row,
                edge,
                endpoint,
            } => line_error(
                row.saturating_add(1),
                "AmbiguousEndpoint",
                format!(
                    "edge {edge}: several committed vertices match {}",
                    spelled(&endpoint, options)
                ),
            ),
            BulkLoadErrorKind::SourceChanged { row } => line_error(
                row.saturating_add(1),
                "SourceChanged",
                "source replay differs",
            ),
            BulkLoadErrorKind::SourceLimit {
                row,
                dimension,
                limit,
                observed,
            } => line_error(
                row.saturating_add(1),
                "SourceLimit",
                format!("{dimension}: observed {observed}, limit {limit}"),
            ),
            kind => line_error(e.committed.next_row + 1, "BulkLoad", format!("{kind:?}")),
        })?;
    if robot {
        emit(
            out,
            &format!(
                "{{\"v\":1,\"event\":\"result\",\"kind\":\"loaded\",\"seq\":{},\"count\":{},\"statements\":{}}}",
                result.frontier.0, result.next_row, result.committed_chunks
            ),
        )
    } else {
        writeln!(
            out,
            "loaded {} rows in {} chunks (seq {})",
            result.next_row, result.committed_chunks, result.frontier.0
        )
        .map_err(Failure::io)
    }
}

/// Lines one refill reads before parsing them: one sequential pass of block
/// reads, their hash checks and cancellation polls, then the whole batch's
/// JSON parsed across worker threads, in order. Parsing every row on the
/// loading thread, once in each of a load's three passes over its source,
/// was about a twelfth of a road-ny load (fgdb-load-beats-memgraph-xf2n8).
const PARSE_BATCH: usize = 16_384;

/// At most this many threads parse one batch; fewer lines parse inline.
const PARSE_WORKERS: usize = 8;
const PARSE_INLINE_BELOW: usize = 1_024;
/// Live control bounds a decoding run by both rows and source bytes. A single
/// larger record runs alone, never behind a full run of other records, and its
/// JSON decoder requests fresh control within source-byte windows. Semantic
/// row conversion retains the admitted 1 MiB record bound. These limits do not
/// change source or engine admission.
const PARSE_RUN_ROWS: usize = 64;
const PARSE_RUN_BYTES: usize = 64 * 1024;

/// Cloneable decoder over independently positioned, verified input blocks.
/// Only the owned row/error escapes next(); no borrowed source payload reaches
/// an await. A decoder or read error is terminal, never translated into EOF.
/// Rows are read and parsed a batch ahead; the first error in source order
/// ends the rows exactly where reading them one by one would have.
#[derive(Clone)]
struct Rows<'a> {
    reader: input::Reader,
    cx: &'a fgdb_types::QueryCx,
    options: &'a Options,
    resolver: Option<&'a fgdb::PinnedTzdb>,
    /// Parsed rows not yet yielded, in source order.
    ready: std::collections::VecDeque<BulkRow>,
    /// What follows `ready`.
    after: After,
    failed: bool,
}
/// What follows a batch's parsed rows: another batch, the end, or the first
/// error in source order. A read error is shared only so a clone of a
/// decoder holding one can be made; the yielded error is the original.
#[derive(Clone)]
enum After {
    More,
    End,
    Decode {
        line: usize,
        message: String,
    },
    Read {
        line: usize,
        error: std::sync::Arc<io::Error>,
    },
    Interrupted {
        line: usize,
        error: std::sync::Arc<asupersync::error::Error>,
    },
}
impl<'a> Rows<'a> {
    fn new(
        reader: input::Reader,
        cx: &'a fgdb_types::QueryCx,
        options: &'a Options,
        resolver: Option<&'a fgdb::PinnedTzdb>,
    ) -> Self {
        Self {
            reader,
            cx,
            options,
            resolver,
            ready: std::collections::VecDeque::new(),
            after: After::More,
            failed: false,
        }
    }

    /// Read up to [`PARSE_BATCH`] lines, parse them, and queue the rows up
    /// to the first error in source order, which becomes `after`.
    fn refill(&mut self) {
        let cx = self.cx;
        self.refill_with_control(|| cx.checkpoint());
    }

    fn refill_with_control(
        &mut self,
        control: impl FnMut() -> Result<(), Box<asupersync::error::Error>>,
    ) {
        let mut lines = Vec::new();
        let mut after = After::More;
        while lines.len() < PARSE_BATCH {
            let line = self.reader.line();
            match self.reader.next_line(self.cx) {
                Ok(None) => {
                    after = After::End;
                    break;
                }
                Ok(Some(text)) => lines.push((line, text)),
                Err(error) => {
                    after = After::Read {
                        line,
                        error: std::sync::Arc::new(error),
                    };
                    break;
                }
            }
        }
        let parsed = match parse_rows(&lines, self.options, self.resolver, control) {
            Ok(parsed) => parsed,
            Err(error) => {
                // No decoded prefix from an interrupted batch escapes. This
                // retains the typed cause, rather than reporting cancellation
                // as malformed input or a successful end of the source.
                self.after = After::Interrupted {
                    line: lines.first().map_or(self.reader.line(), |(line, _)| *line),
                    error: error.into(),
                };
                return;
            }
        };
        for ((line, _), result) in lines.iter().zip(parsed) {
            match result {
                Ok(row) => self.ready.push_back(row),
                Err(message) => {
                    after = After::Decode {
                        line: *line,
                        message,
                    };
                    break;
                }
            }
        }
        self.after = after;
    }
}

/// `parse_row` over every line, in order, across up to [`PARSE_WORKERS`]
/// scoped threads. Live control and its error stay on the calling thread;
/// all workers join before a result, interruption or panic escapes.
fn parse_rows<C>(
    lines: &[(usize, String)],
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
    control: impl FnMut() -> Result<(), C>,
) -> Result<Vec<Result<BulkRow, String>>, C> {
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(PARSE_WORKERS);
    parse_rows_controlled(
        lines,
        workers,
        |text, poll| parse_row_controlled(text, options, resolver, poll),
        control,
    )
}

fn parse_run_length(lines: &[(usize, String)]) -> usize {
    let mut bytes = 0usize;
    let mut rows = 0;
    for (_, text) in lines.iter().take(PARSE_RUN_ROWS) {
        if rows != 0
            && (bytes >= PARSE_RUN_BYTES || text.len() > PARSE_RUN_BYTES.saturating_sub(bytes))
        {
            break;
        }
        bytes = bytes.saturating_add(text.len());
        rows += 1;
    }
    rows
}

fn parse_wait(spins: &mut usize) {
    if *spins < 64 {
        *spins += 1;
        std::hint::spin_loop();
    } else {
        std::thread::yield_now();
    }
}

// Worker-owned progress lines avoid one contended counter in the decode loop.
#[repr(align(64))]
struct ParseWorker {
    requested: std::sync::atomic::AtomicUsize,
    admitted: std::sync::atomic::AtomicUsize,
    done: std::sync::atomic::AtomicBool,
}

fn parse_rows_controlled<T: Send, C>(
    lines: &[(usize, String)],
    workers: usize,
    parse: impl Fn(&str, &mut dyn FnMut() -> bool) -> Result<T, JsonParseError<()>> + Sync,
    mut control: impl FnMut() -> Result<(), C>,
) -> Result<Vec<Result<T, String>>, C> {
    let workers = workers.clamp(1, PARSE_WORKERS);
    if workers < 2 || lines.len() < PARSE_INLINE_BELOW {
        let mut parsed = Vec::with_capacity(lines.len());
        let mut remaining = lines;
        while !remaining.is_empty() {
            control()?;
            let (run, rest) = remaining.split_at(parse_run_length(remaining));
            for (_, text) in run {
                let mut interrupted = None;
                let result = parse(text, &mut || {
                    if interrupted.is_some() {
                        return false;
                    }
                    match control() {
                        Ok(()) => true,
                        Err(error) => {
                            interrupted = Some(error);
                            false
                        }
                    }
                });
                if let Some(error) = interrupted {
                    return Err(error);
                }
                match result {
                    Ok(row) => parsed.push(Ok(row)),
                    Err(JsonParseError::Invalid(message)) => parsed.push(Err(message)),
                    Err(JsonParseError::Interrupted(())) => {
                        unreachable!("decoder interruption requires a refused control poll")
                    }
                }
            }
            remaining = rest;
        }
        control()?;
        return Ok(parsed);
    }

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    control()?;
    let stop = AtomicBool::new(false);
    let progress: [ParseWorker; PARSE_WORKERS] = std::array::from_fn(|_| ParseWorker {
        requested: AtomicUsize::new(0),
        admitted: AtomicUsize::new(0),
        done: AtomicBool::new(false),
    });
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    struct DoneOnDrop<'a> {
        done: &'a AtomicBool,
        stop: &'a AtomicBool,
    }
    impl Drop for DoneOnDrop<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                self.stop.store(true, Ordering::Release);
            }
            self.done.store(true, Ordering::Release);
        }
    }
    std::thread::scope(|scope| {
        // Drop inside the scope closure, BEFORE scope joins on a host error
        // or unwind. Otherwise a worker awaiting admission could wait forever.
        let _stop_on_exit = StopOnDrop(&stop);
        let share = lines.len().div_ceil(workers);
        let parts: Vec<_> = lines
            .chunks(share)
            .zip(&progress)
            .map(|(part, worker)| {
                let parse = &parse;
                let stop = &stop;
                scope.spawn(move || {
                    let _done_on_exit = DoneOnDrop {
                        done: &worker.done,
                        stop,
                    };
                    let mut parsed = Vec::with_capacity(part.len());
                    let mut remaining = part;
                    let mut requested = 0;
                    let mut poll = || {
                        if stop.load(Ordering::Acquire) {
                            return false;
                        }
                        requested += 1;
                        worker.requested.store(requested, Ordering::Release);
                        let mut spins = 0;
                        while worker.admitted.load(Ordering::Acquire) < requested {
                            if stop.load(Ordering::Acquire) {
                                return false;
                            }
                            parse_wait(&mut spins);
                        }
                        !stop.load(Ordering::Acquire)
                    };
                    while !remaining.is_empty() {
                        if !poll() {
                            return parsed;
                        }
                        let (run, rest) = remaining.split_at(parse_run_length(remaining));
                        for (_, text) in run {
                            if stop.load(Ordering::Acquire) {
                                return parsed;
                            }
                            match parse(text, &mut poll) {
                                Ok(row) => parsed.push(Ok(row)),
                                Err(JsonParseError::Invalid(message)) => parsed.push(Err(message)),
                                Err(JsonParseError::Interrupted(())) => {
                                    assert!(stop.load(Ordering::Acquire));
                                    return parsed;
                                }
                            }
                        }
                        remaining = rest;
                    }
                    parsed
                })
            })
            .collect();

        let mut admitted = [0; PARSE_WORKERS];
        let mut idle_spins = 0;
        loop {
            let mut requested = [0; PARSE_WORKERS];
            let mut pending = false;
            let mut done = true;
            for (at, worker) in progress[..parts.len()].iter().enumerate() {
                requested[at] = worker.requested.load(Ordering::Acquire);
                pending |= requested[at] != admitted[at];
                done &= worker.done.load(Ordering::Acquire);
            }
            if pending || done {
                idle_spins = 0;
                control()?;
                // Only requests observed BEFORE this live poll are granted.
                // New requests must wait for the next checkpoint, not inherit
                // an acknowledgement from before their preceding decode run.
                for (at, worker) in progress[..parts.len()].iter().enumerate() {
                    worker.admitted.store(requested[at], Ordering::Release);
                    admitted[at] = requested[at];
                }
            }
            if done {
                break;
            }
            if !pending {
                parse_wait(&mut idle_spins);
            }
        }

        // A partial worker result exists only after a parent interruption
        // (which returned above) or a peer panic (which is rethrown here).
        // Source order, including the first malformed row, is unchanged.
        Ok(parts
            .into_iter()
            .flat_map(|part| {
                part.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect())
    })
}
#[derive(Debug)]
struct RowError {
    line: usize,
    detail: RowErrorDetail,
}
#[derive(Debug)]
enum RowErrorDetail {
    Read(io::Error),
    Decode(String),
    Interrupted(std::sync::Arc<asupersync::error::Error>),
}
impl std::fmt::Display for RowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            RowErrorDetail::Read(error) => std::fmt::Display::fmt(error, f),
            RowErrorDetail::Decode(error) => f.write_str(error),
            RowErrorDetail::Interrupted(error) => {
                write!(f, "load parsing interrupted: {error}")
            }
        }
    }
}
impl std::error::Error for RowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.detail {
            RowErrorDetail::Read(error) => Some(error),
            RowErrorDetail::Interrupted(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}
impl RowError {
    fn failure(&self) -> Failure {
        match &self.detail {
            RowErrorDetail::Interrupted(error) => super::execution_failure(error.clone()),
            _ => line_error(self.line, "MalformedRow", self),
        }
    }
}
impl Iterator for Rows<'_> {
    type Item = Result<BulkRow, RowError>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.failed {
                return None;
            }
            if let Some(row) = self.ready.pop_front() {
                return Some(Ok(row));
            }
            match std::mem::replace(&mut self.after, After::More) {
                After::More => self.refill(),
                After::End => {
                    self.after = After::End;
                    return None;
                }
                After::Decode { line, message } => {
                    self.failed = true;
                    return Some(Err(RowError {
                        line,
                        detail: RowErrorDetail::Decode(message),
                    }));
                }
                After::Read { line, error } => {
                    self.failed = true;
                    let error = std::sync::Arc::try_unwrap(error)
                        .unwrap_or_else(|shared| io::Error::new(shared.kind(), shared.to_string()));
                    return Some(Err(RowError {
                        line,
                        detail: RowErrorDetail::Read(error),
                    }));
                }
                After::Interrupted { line, error } => {
                    self.failed = true;
                    return Some(Err(RowError {
                        line,
                        detail: RowErrorDetail::Interrupted(error),
                    }));
                }
            }
        }
    }
}
impl std::iter::FusedIterator for Rows<'_> {}

fn marker<V: Vfs + Clone>(db: &Database<V>, seq: CommitSeq) -> Result<String, Failure> {
    if seq.0 == 0 {
        return Ok(String::new());
    }
    let batch = db
        .delta_since(CommitSeq(seq.0 - 1))
        .map_err(invalid)?
        .next()
        .ok_or_else(|| invalid("missing history marker"))?;
    if batch.commit_seq() != seq {
        return Err(invalid("missing history marker"));
    }
    Ok(hex(&batch.commit_marker_identity().marker_oid.0))
}
fn same_saved_checkpoint(
    saved: &Saved,
    b: &BulkLoadCheckpoint,
    prefix: &mut checkpoint::Prefix<Rows<'_>>,
) -> Result<(), Failure> {
    let a = &saved.checkpoint;
    if a.next_row != b.next_row
        || a.frontier != b.frontier
        || a.committed_chunks != b.committed_chunks
    {
        return Err(invalid("checkpoint does not match recovered source prefix"));
    }
    match &saved.identity_hash {
        Some(expected) => {
            if prefix.advance(b).map_err(invalid)? != *expected {
                return Err(invalid(
                    "checkpoint identity commitment does not match recovered source prefix",
                ));
            }
        }
        None => same_checkpoint(a, b)?,
    }
    Ok(())
}
// V1 remains a checked durable input format. Keep its exact identity-map law
// separate from V2's recomputed prefix commitment.
fn same_checkpoint(a: &BulkLoadCheckpoint, b: &BulkLoadCheckpoint) -> Result<(), Failure> {
    if a.vertices != b.vertices
        || a.edges != b.edges
        || a.next_row != b.next_row
        || a.frontier != b.frontier
        || a.committed_chunks != b.committed_chunks
    {
        return Err(invalid("checkpoint does not match recovered source prefix"));
    }
    Ok(())
}
/// The engine's identity permutations, for ordering a chunk's creations by
/// issue (fgdb-hxgm1 channel 2).
struct IssueOrder {
    vertices: fgdb::IdentityPermutation,
    edges: fgdb::IdentityPermutation,
}

/// Each creation keyed by its allocation counter, ascending: the order the
/// engine issued the identities, which is source order within a kind.
fn by_issue<I: Copy, R>(
    created: BTreeMap<I, R>,
    counter: impl Fn(I) -> Option<u64>,
) -> Result<Vec<(I, R)>, Failure> {
    let mut issued = created
        .into_iter()
        .map(|(id, row)| {
            counter(id)
                .map(|issue| (issue, id, row))
                .ok_or_else(|| invalid("creation identity was not engine-issued"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // A permutation is a bijection, so no two creations share a counter.
    issued.sort_unstable_by_key(|(issue, _, _)| *issue);
    Ok(issued.into_iter().map(|(_, id, row)| (id, row)).collect())
}

fn reconcile(
    batch: &fgdb_delta_types::LogicalDeltaBatch,
    rows: &mut Rows<'_>,
    source_rows: usize,
    size: usize,
    cp: &mut BulkLoadCheckpoint,
    order: &IssueOrder,
) -> Result<(), Failure> {
    let count = source_rows
        .checked_sub(cp.next_row)
        .ok_or_else(|| invalid("source shorter than history"))?
        .min(size);
    let end = cp
        .next_row
        .checked_add(count)
        .ok_or_else(|| invalid("row counter overflow"))?;
    let next_chunk = cp
        .committed_chunks
        .checked_add(1)
        .ok_or_else(|| invalid("chunk counter overflow"))?;
    if count == 0 || Some(batch.commit_seq().0) != cp.frontier.0.checked_add(1) {
        return Err(invalid("unexpected history after import"));
    }
    let mut vs = BTreeMap::new();
    let mut es = BTreeMap::new();
    for entry in batch.coordinate_entries() {
        for row in &entry.rows {
            match row {
                DeltaRow::CreateVertex {
                    vid,
                    labels,
                    props,
                    valid_time: None,
                    ..
                } => {
                    if vs.insert(*vid, (labels, props)).is_some() {
                        return Err(invalid("duplicate vertex creation"));
                    }
                }
                DeltaRow::CreateEdge {
                    eid,
                    src,
                    dst,
                    relation,
                    props,
                    canonical_key: None,
                    valid_time: None,
                    ..
                } => {
                    if es.insert(*eid, (*src, *dst, *relation, props)).is_some() {
                        return Err(invalid("duplicate edge creation"));
                    }
                }
                _ => return Err(invalid("history is not a bulk creation chunk")),
            }
        }
    }
    if vs.len() + es.len() != count {
        return Err(invalid("history chunk row count changed"));
    }
    // The engine issues each kind's identities in source order, from a
    // counter it permutes, so creations pair with source rows in counter
    // order. Identity order would scramble them.
    let mut vs = by_issue(vs, |vid: VId| {
        order.vertices.invert(u64::try_from(vid.0).ok()?)
    })?
    .into_iter();
    let mut es = by_issue(es, |eid: EId| {
        order.edges.invert(u64::try_from(eid.0).ok()?)
    })?
    .into_iter();
    for _ in 0..count {
        let row = rows
            .next()
            .ok_or_else(|| invalid("source shorter than history"))?
            .map_err(|error| error.failure())?;
        match row {
            BulkRow::Vertex(v) => {
                let (id, (labels, props)) = vs
                    .next()
                    .ok_or_else(|| invalid("missing vertex creation"))?;
                if labels != &v.labels || !same_logical_props(props, &v.props) {
                    return Err(invalid("vertex source prefix changed"));
                }
                cp.vertices.insert(v.key.clone(), id);
            }
            BulkRow::Edge(e) => {
                let (id, (src, dst, relation, props)) =
                    es.next().ok_or_else(|| invalid("missing edge creation"))?;
                // A match endpoint is re-resolved and checked against this
                // creation by the engine's resume preflight.
                let keyed = |endpoint: &fgdb::BulkEndpoint, id| {
                    endpoint
                        .key()
                        .is_none_or(|key| cp.vertices.get(key) == Some(&id))
                };
                if !keyed(&e.source, src)
                    || !keyed(&e.destination, dst)
                    || relation != e.relation
                    || !same_logical_props(props, &e.props)
                {
                    return Err(invalid("edge source prefix changed"));
                }
                cp.edges.insert(e.key.clone(), id);
            }
        }
    }
    cp.next_row = end;
    cp.committed_chunks = next_chunk;
    cp.frontier = batch.commit_seq();
    Ok(())
}

struct Saved {
    checkpoint: BulkLoadCheckpoint,
    base: CommitSeq,
    base_marker: String,
    source_hash: String,
    rows_per_chunk: usize,
    identity_hash: Option<String>,
}
impl Saved {
    fn view(&self) -> checkpoint::View<'_> {
        checkpoint::View {
            checkpoint: &self.checkpoint,
            base: self.base,
            base_marker: &self.base_marker,
            source_hash: &self.source_hash,
            rows_per_chunk: self.rows_per_chunk,
            identity_hash: self.identity_hash.as_deref(),
        }
    }
    fn decode(text: &str, limits: checkpoint::Limits) -> Result<Self, String> {
        if text.len() > limits.bytes {
            return Err("checkpoint file exceeds source admission".into());
        }
        let json = parse_json(text, limits.values(), limits.token_bytes())?;
        let Json::Object(mut fields) = json else {
            return Err("expected object".into());
        };
        let version = number(field(&fields, "v")?)?;
        if !matches!((version, fields.len()), (1, 10) | (2, 9)) {
            return Err("unknown checkpoint format".into());
        }
        if version == 2 && text.len() > checkpoint::MAX_COMPACT_BYTES {
            return Err("compact checkpoint exceeds format bound".into());
        }
        let identity_hash = if version == 2 {
            Some(string(field(&fields, "identity_hash")?)?.to_owned())
        } else {
            None
        };
        let id = |value: &Json| -> Result<u128, String> {
            let text = string(value)?;
            let value: u128 = text.parse().map_err(|_| "invalid identity")?;
            if text != value.to_string() {
                return Err("noncanonical identity".into());
            }
            Ok(value)
        };
        let usize_field = |name| {
            usize::try_from(number(field(&fields, name)?)?)
                .map_err(|_| "checkpoint counter overflow".to_owned())
        };
        let next_row = usize_field("next_row")?;
        let committed_chunks = usize_field("committed_chunks")?;
        let rows_per_chunk = usize_field("rows_per_chunk")?;
        let frontier = CommitSeq(number(field(&fields, "frontier")?)?);
        let base = CommitSeq(number(field(&fields, "base_frontier")?)?);
        let base_marker = string(field(&fields, "base_marker")?)?.to_owned();
        let source_hash = string(field(&fields, "source_hash")?)?.to_owned();
        // Consume decoded object keys instead of cloning the complete map's
        // String payload while the JSON tree still owns another full copy.
        let mut take_object = |name: &str| match fields.remove(name) {
            Some(Json::Object(map)) => Ok(map),
            _ => Err("expected checkpoint identity map".to_owned()),
        };
        let (vertices, edges) = if version == 1 {
            let vertices = take_object("vertices")?
                .into_iter()
                .map(|(k, v)| Ok((k, VId(id(&v)?))))
                .collect::<Result<_, String>>()?;
            let edges = take_object("edges")?
                .into_iter()
                .map(|(k, v)| Ok((k, EId(id(&v)?))))
                .collect::<Result<_, String>>()?;
            (vertices, edges)
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
        let saved = Self {
            checkpoint: BulkLoadCheckpoint {
                vertices,
                edges,
                next_row,
                frontier,
                committed_chunks,
            },
            base,
            base_marker,
            source_hash,
            rows_per_chunk,
            identity_hash,
        };
        saved.view().validate(limits).map_err(|e| e.to_string())?;
        Ok(saved)
    }
}

/// One typed property value (`int:1`, `text:Ada`, ...) as a storable scalar.
fn storable(value: &Json, resolver: Option<&fgdb::PinnedTzdb>) -> Result<CanonicalScalar, String> {
    let value = parameter(string(value)?, resolver)
        .map_err(|e| format!("InvalidProperty: {}", e.message))?;
    match value {
        GqlParameterValue::Int64(value) => Ok(CanonicalScalar::Int(value)),
        GqlParameterValue::Scalar(value) => Ok(value.value().clone()),
        GqlParameterValue::UInt64(_) | GqlParameterValue::List(_) | GqlParameterValue::Map(_) => {
            Err("InvalidProperty: value is not a storable canonical scalar".into())
        }
    }
}

/// An edge endpoint: a caller key of this input, or
/// `{"label": <name>?, "property": <name>, "value": "<type>:<value>"}` naming
/// the one vertex committed before this load that carries that label and
/// property value.
fn endpoint(
    json: &Json,
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
) -> Result<BulkEndpoint, String> {
    if let Json::String(key) = json {
        return Ok(BulkEndpoint::Key(key.clone()));
    }
    let fields =
        object(json).map_err(|_| "an endpoint is a key or {label?, property, value}".to_owned())?;
    reject_extra(fields, &["label", "property", "value"])?;
    let label = match fields.get("label") {
        None => None,
        Some(name) => {
            let name = string(name)?;
            let id = options
                .labels
                .get(name)
                .ok_or_else(|| format!("UnknownBinding: label {name}"))?;
            Some(LabelId(*id))
        }
    };
    let name = string(field(fields, "property")?)?;
    let key = options
        .properties
        .get(name)
        .ok_or_else(|| format!("UnknownBinding: property {name}"))?;
    let value = storable(field(fields, "value")?, resolver)?;
    if matches!(value, CanonicalScalar::Null) {
        return Err("InvalidEndpoint: a null value matches no vertex".into());
    }
    Ok(BulkEndpoint::matching(label, PropertyKeyId(*key), value))
}

#[cfg(test)]
fn parse_row(
    text: &str,
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
) -> Result<BulkRow, String> {
    let json = parse_json(text, usize::MAX, text.len())?;
    bind_row(&json, options, resolver)
}

fn parse_row_controlled(
    text: &str,
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
    poll: &mut dyn FnMut() -> bool,
) -> Result<BulkRow, JsonParseError<()>> {
    let json = parse_json_within_row(text, poll)?;
    bind_row(&json, options, resolver).map_err(JsonParseError::Invalid)
}

fn parse_json_within_row(
    text: &str,
    poll: &mut dyn FnMut() -> bool,
) -> Result<Json, JsonParseError<()>> {
    parse_json_controlled(text, usize::MAX, text.len(), |offset| {
        // The containing row run was already admitted, and the next run (or
        // final batch check) polls again. Avoid extra host polls per short row;
        // every interior JSON checkpoint still needs a fresh live grant.
        if offset == 0 || offset == text.len() || poll() {
            Ok(())
        } else {
            Err(())
        }
    })
}

fn bind_row(
    json: &Json,
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
) -> Result<BulkRow, String> {
    let fields = object(json)?;
    let key = string(field(fields, "key")?)?.to_owned();
    if key.is_empty() {
        return Err("caller key must not be empty".into());
    }
    let mut props: Vec<(PropertyKeyId, fgdb_types::CanonicalPropertyValue)> = Vec::new();
    if let Some(values) = fields.get("props") {
        for (name, value) in object(values)? {
            let id = options
                .properties
                .get(name)
                .ok_or_else(|| format!("UnknownBinding: property {name}"))?;
            props.push((PropertyKeyId(*id), storable(value, resolver)?.into()));
        }
    }
    props.sort_by_key(|(id, _)| *id);
    match string(field(fields, "kind")?)? {
        "vertex" => {
            reject_extra(fields, &["kind", "key", "labels", "props"])?;
            let Json::Array(names) = field(fields, "labels")? else {
                return Err("labels must be an array".into());
            };
            let mut labels = Vec::new();
            for name in names {
                let name = string(name)?;
                let id = options
                    .labels
                    .get(name)
                    .ok_or_else(|| format!("UnknownBinding: label {name}"))?;
                labels.push(LabelId(*id));
            }
            labels.sort();
            if labels.windows(2).any(|w| w[0] == w[1]) {
                return Err("duplicate label".into());
            }
            Ok(BulkRow::Vertex(BulkVertex { key, labels, props }))
        }
        "edge" => {
            reject_extra(
                fields,
                &["kind", "key", "source", "destination", "relation", "props"],
            )?;
            let relation = string(field(fields, "relation")?)?;
            let relation = options
                .relations
                .get(relation)
                .ok_or_else(|| format!("UnknownBinding: relation {relation}"))?;
            Ok(BulkRow::Edge(BulkEdge {
                key,
                source: endpoint(field(fields, "source")?, options, resolver)?,
                destination: endpoint(field(fields, "destination")?, options, resolver)?,
                relation: RelationId(*relation),
                props,
            }))
        }
        _ => Err("kind must be vertex or edge".into()),
    }
}
fn reject_extra(fields: &BTreeMap<String, Json>, names: &[&str]) -> Result<(), String> {
    if fields.keys().any(|k| !names.contains(&k.as_str())) {
        return Err("unknown row field".into());
    }
    Ok(())
}
fn object(json: &Json) -> Result<&BTreeMap<String, Json>, String> {
    if let Json::Object(v) = json {
        Ok(v)
    } else {
        Err("expected object".into())
    }
}
fn string(json: &Json) -> Result<&str, String> {
    if let Json::String(v) = json {
        Ok(v)
    } else {
        Err("expected string".into())
    }
}
fn number(json: &Json) -> Result<u64, String> {
    let Json::Number(v) = json else {
        return Err("expected unsigned number".into());
    };
    let value = v.parse::<u64>().map_err(|_| "expected unsigned number")?;
    if value.to_string() != *v {
        return Err("noncanonical unsigned number".into());
    }
    Ok(value)
}
fn field<'a>(fields: &'a BTreeMap<String, Json>, name: &str) -> Result<&'a Json, String> {
    fields.get(name).ok_or_else(|| format!("missing {name}"))
}

#[cfg(test)]
mod parse_tests {
    use super::*;
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    fn options() -> Options {
        crate::parse(
            &[
                "--db",
                "unused",
                "--key-file",
                "unused",
                "--input",
                "unused",
                "--label",
                "Node=4294967298",
                "--label",
                "Person=1",
                "--relation",
                "R=18446744073709551615",
                "--property",
                "name=18446744073709551615",
                "--property",
                "age=7",
            ]
            .map(str::to_owned),
            "load",
        )
        .unwrap()
    }

    fn lines(count: usize) -> Vec<(usize, String)> {
        (0..count)
            .map(|i| {
                let text = if i % 2 == 0 {
                    format!(
                        r#"{{"kind":"vertex","key":"v{i}","labels":["Node","Person"],"props":{{"name":"text:λ\"\n💡","age":"int:-{i}"}}}}"#
                    )
                } else {
                    format!(
                        r#"{{"kind":"edge","key":"e{i}","source":"v0","destination":{{"label":"Person","property":"age","value":"int:0"}},"relation":"R","props":{{"name":"text:edge {i}"}}}}"#
                    )
                };
                (i + 1, text)
            })
            .collect()
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Captured {
        Vertex(String, Vec<LabelId>, Vec<(PropertyKeyId, Vec<u8>)>),
        Edge(
            String,
            BulkEndpoint,
            BulkEndpoint,
            RelationId,
            Vec<(PropertyKeyId, Vec<u8>)>,
        ),
    }

    fn captured(row: Result<BulkRow, String>) -> Result<Captured, String> {
        let properties = |props: Vec<(PropertyKeyId, CanonicalPropertyValue)>| {
            props
                .into_iter()
                .map(|(key, value)| (key, value.encode().unwrap()))
                .collect()
        };
        row.map(|row| match row {
            BulkRow::Vertex(v) => Captured::Vertex(v.key, v.labels, properties(v.props)),
            BulkRow::Edge(e) => Captured::Edge(
                e.key,
                e.source,
                e.destination,
                e.relation,
                properties(e.props),
            ),
        })
    }

    #[test]
    fn parallel_decoding_keeps_scalar_rows_canonical_bytes_and_error_order() {
        let options = options();
        for count in [0, 1, 63, 64, 65, 1023, 1024, 1031, 4097] {
            let mut source = lines(count);
            for (at, malformed) in [
                (11, r#"{"kind":"vertex","kind":"edge"}"#),
                (64, r#"{"kind":"vertex","key":"bad","labels":["missing"]}"#),
                (1024, r#"{"kind":"edge","key":"broken"}"#),
            ] {
                if let Some((_, text)) = source.get_mut(at) {
                    *text = malformed.to_owned();
                }
            }
            let expected = source
                .iter()
                .map(|(_, text)| captured(parse_row(text, &options, None)))
                .collect::<Vec<_>>();
            for workers in [1, 2, 3, PARSE_WORKERS] {
                let actual = parse_rows_controlled(
                    &source,
                    workers,
                    |text, poll| parse_row_controlled(text, &options, None, poll),
                    || Ok::<_, Infallible>(()),
                )
                .unwrap()
                .into_iter()
                .map(captured)
                .collect::<Vec<_>>();
                assert_eq!(actual, expected, "rows={count}, workers={workers}");
            }
            if count > 11 {
                assert_eq!(
                    expected.iter().position(Result::is_err),
                    Some(11),
                    "later parallel errors must not replace the first source error"
                );
            }
        }
    }

    #[test]
    fn live_decode_runs_bound_both_rows_and_bytes_and_isolate_large_records() {
        let options = options();
        let mut source = lines(PARSE_RUN_ROWS * 3);
        for (index, size) in [
            (2, PARSE_RUN_BYTES - 7),
            (3, PARSE_RUN_BYTES),
            (4, input::MAX_RECORD_BYTES),
        ] {
            let text = &mut source[index].1;
            text.push_str(&" ".repeat(size - text.len()));
        }
        let rows = AtomicUsize::new(0);
        let bytes = AtomicUsize::new(0);
        let mut previous = (0, 0);
        let mut oversized = 0;
        let result = parse_rows_controlled(
            &source,
            1,
            |text, poll| {
                let row = parse_row_controlled(text, &options, None, poll);
                assert!(row.is_ok());
                rows.fetch_add(1, Ordering::Relaxed);
                bytes.fetch_add(text.len(), Ordering::Relaxed);
                row
            },
            || {
                let now = (rows.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed));
                let delta = (now.0 - previous.0, now.1 - previous.1);
                assert!(delta.0 <= PARSE_RUN_ROWS);
                if delta.1 > PARSE_RUN_BYTES {
                    assert_eq!(delta, (1, input::MAX_RECORD_BYTES));
                    oversized += 1;
                }
                previous = now;
                Ok::<_, Infallible>(())
            },
        )
        .unwrap();
        assert_eq!(result.len(), source.len());
        assert_eq!(previous.0, source.len(), "final work is polled too");
        assert_eq!(oversized, 1);
    }

    struct Active<'a>(&'a AtomicUsize);
    impl<'a> Active<'a> {
        fn new(active: &'a AtomicUsize) -> Self {
            active.fetch_add(1, Ordering::AcqRel);
            Self(active)
        }
    }
    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn cancellation_after_real_decoding_keeps_non_send_control_on_parent_and_joins() {
        let options = options();
        let source = lines(PARSE_BATCH);
        for workers in [1, 2, PARSE_WORKERS] {
            let active = AtomicUsize::new(0);
            let completed = AtomicUsize::new(0);
            let cause = Rc::new(Cell::new(0));
            let parent = std::thread::current().id();
            let result = parse_rows_controlled(
                &source,
                workers,
                |text, poll| {
                    let _active = Active::new(&active);
                    let row = parse_row_controlled(text, &options, None, poll);
                    assert!(row.is_ok());
                    completed.fetch_add(1, Ordering::Release);
                    row
                },
                || {
                    assert_eq!(std::thread::current().id(), parent);
                    let seen = completed.load(Ordering::Acquire);
                    if seen >= PARSE_RUN_ROWS {
                        cause.set(seen);
                        Err(cause.clone())
                    } else {
                        Ok(())
                    }
                },
            );
            let error = result.expect_err("actual decoded work must observe cancellation");
            assert!(Rc::ptr_eq(&error, &cause), "preserve the original cause");
            assert_eq!(active.load(Ordering::Acquire), 0, "all decoders joined");
            let after = completed.load(Ordering::Acquire);
            assert!(after >= PARSE_RUN_ROWS);
            assert!(after <= cause.get() + workers * PARSE_RUN_ROWS);
            assert!(
                after < source.len(),
                "the rest of the batch was not decoded"
            );
        }
    }

    #[test]
    fn parser_workers_join_and_stop_peers_on_parent_or_worker_unwind() {
        let options = options();
        let source = lines(PARSE_BATCH);
        for host_panics in [true, false] {
            let active = AtomicUsize::new(0);
            let completed = AtomicUsize::new(0);
            let panicked = AtomicBool::new(false);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                parse_rows_controlled(
                    &source,
                    PARSE_WORKERS,
                    |text, poll| {
                        let _active = Active::new(&active);
                        let row = parse_row_controlled(text, &options, None, poll);
                        assert!(row.is_ok());
                        let seen = completed.fetch_add(1, Ordering::AcqRel) + 1;
                        if !host_panics
                            && seen >= PARSE_RUN_ROWS
                            && !panicked.swap(true, Ordering::AcqRel)
                        {
                            std::panic::resume_unwind(Box::new("worker decode panic"));
                        }
                        row
                    },
                    || {
                        if host_panics && completed.load(Ordering::Acquire) >= PARSE_RUN_ROWS {
                            std::panic::resume_unwind(Box::new("host decode panic"));
                        }
                        Ok::<_, Infallible>(())
                    },
                )
            }));
            let panic = result.expect_err("the originating panic must escape");
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&if host_panics {
                    "host decode panic"
                } else {
                    "worker decode panic"
                })
            );
            assert_eq!(active.load(Ordering::Acquire), 0);
            assert!(completed.load(Ordering::Acquire) < source.len());
        }
    }

    fn input(source: &[(usize, String)], cx: &fgdb_types::QueryCx) -> input::Input {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fgdb-parse-control-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        for (_, text) in source {
            writeln!(file, "{text}").unwrap();
        }
        drop(file);
        input::Input::open(cx, &path, b"parse control", source.len()).unwrap()
    }

    #[test]
    fn row_decoder_returns_first_source_error_then_fuses() {
        let runtime = fgdb::runtime_builder().build().unwrap();
        let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let options = options();
        let mut source = lines(4097);
        source[11].1.insert_str(1, &" ".repeat(3 * PARSE_RUN_BYTES));
        source[13].1 = r#"{"kind":"vertex","key":"bad","labels":["missing"]}"#.to_owned();
        source[2049].1 = "{broken}".to_owned();
        let input = input(&source, &cx);
        let mut rows = Rows::new(input.reader(), &cx, &options, None);
        for (_, text) in &source[..13] {
            assert_eq!(
                captured(Ok(rows.next().unwrap().unwrap())),
                captured(parse_row(text, &options, None))
            );
        }
        let error = rows.next().unwrap().unwrap_err();
        assert_eq!(error.line, 14);
        assert_eq!(error.to_string(), "UnknownBinding: label missing");
        assert!(
            error
                .failure()
                .message
                .starts_with("line 14: MalformedRow:")
        );
        assert!(rows.next().is_none());
        assert!(rows.clone().next().is_none());
    }

    #[test]
    fn interrupted_large_json_refill_keeps_context_error_and_discards_every_private_row() {
        use std::error::Error as _;

        for malformed_prefix in [false, true] {
            let runtime = fgdb::runtime_builder().build().unwrap();
            let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let options = options();
            let mut source = lines(3);
            if malformed_prefix {
                source[0].1 = "{broken}".into();
            }
            let padding = input::MAX_RECORD_BYTES - source[1].1.len();
            source[1].1.insert_str(1, &" ".repeat(padding));
            let input = input(&source, &cx);
            let mut rows = Rows::new(input.reader(), &cx, &options, None);
            let mut polls = 0;
            rows.refill_with_control(|| {
                polls += 1;
                if polls == 3 {
                    // First poll admits the preceding small row, second admits
                    // the large row, and the third is inside that JSON record.
                    root.set_cancel_reason(asupersync::types::CancelReason::user(
                        "cancel inside loader JSON",
                    ));
                }
                cx.checkpoint()
            });
            assert_eq!(polls, 3);
            assert!(rows.ready.is_empty());
            let mut cloned = rows.clone();
            for rows in [&mut rows, &mut cloned] {
                let error = rows.next().unwrap().unwrap_err();
                assert!(matches!(&error.detail, RowErrorDetail::Interrupted(_)));
                assert!(error.source().unwrap().is::<asupersync::error::Error>());
                let failure = error.failure();
                assert_eq!(failure.code, 3);
                assert!(failure.message.contains("cancel inside loader JSON"));
                assert!(!failure.message.contains("MalformedRow"));
                assert!(rows.next().is_none());
            }
        }
    }

    #[test]
    fn interrupted_refill_discards_private_rows_retains_typed_cause_and_fuses() {
        use std::error::Error as _;

        let runtime = fgdb::runtime_builder().build().unwrap();
        let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let options = options();
        let source = lines(2 * PARSE_RUN_ROWS);
        let input = input(&source, &cx);
        let mut rows = Rows::new(input.reader(), &cx, &options, None);
        let mut polls = 0;
        rows.refill_with_control(|| {
            polls += 1;
            if polls == 2 {
                // Inline parsing has completed the first real 64-row run.
                root.set_cancel_reason(asupersync::types::CancelReason::user(
                    "cancel during loader decoding",
                ));
            }
            cx.checkpoint()
        });
        assert_eq!(polls, 2);
        assert!(
            rows.ready.is_empty(),
            "no interrupted prefix is publishable"
        );
        let mut cloned = rows.clone();
        for rows in [&mut rows, &mut cloned] {
            let error = rows.next().unwrap().unwrap_err();
            assert!(matches!(&error.detail, RowErrorDetail::Interrupted(_)));
            assert!(error.source().unwrap().is::<asupersync::error::Error>());
            let failure = error.failure();
            assert_eq!(failure.code, 3);
            assert!(failure.message.contains("cancel during loader decoding"));
            assert!(!failure.message.contains("MalformedRow"));
            assert!(rows.next().is_none());
        }
    }
}

#[cfg(test)]
mod json_parse_tests {
    use super::*;
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Active<'a>(&'a AtomicUsize);
    impl<'a> Active<'a> {
        fn new(active: &'a AtomicUsize) -> Self {
            active.fetch_add(1, Ordering::AcqRel);
            Self(active)
        }
    }
    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn large_first_records(workers: usize) -> Vec<(usize, String)> {
        let mut source: Vec<_> = (0..PARSE_INLINE_BELOW)
            .map(|line| (line + 1, "null".to_owned()))
            .collect();
        let large = format!("\"{}\"", "é".repeat((1024 * 1024 - 2) / 2));
        assert_eq!(large.len(), 1024 * 1024);
        let share = source.len().div_ceil(workers);
        for part in source.chunks_mut(share) {
            part[0].1.clone_from(&large);
        }
        source
    }

    #[test]
    fn json_batches_keep_values_and_first_error_order_across_worker_counts() {
        let mut source: Vec<_> = (0..PARSE_INLINE_BELOW + 1)
            .map(|row| (row + 1, format!(r#"{{"key":{row},"value":"é🦀"}}"#)))
            .collect();
        source[3].1 = "{broken}".into();
        source[7].1 = format!("null{}?", " ".repeat(2 * PARSE_RUN_BYTES));
        for (index, text) in [
            (1, "é".repeat(PARSE_RUN_BYTES)),
            (127, r"\uD834\uDD1E".repeat(PARSE_RUN_BYTES / 3)),
            (512, "🦀".repeat(PARSE_RUN_BYTES)),
            (1024, "x".repeat(1024 * 1024 - 2)),
        ] {
            source[index].1 = format!("\"{text}\"");
        }
        let expected: Vec<_> = source
            .iter()
            .map(|(_, text)| parse_json(text, usize::MAX, text.len()))
            .collect();
        assert_eq!(expected.iter().position(Result::is_err), Some(3));
        for workers in [1, 2, 3, PARSE_WORKERS] {
            let interior = AtomicUsize::new(0);
            let polls = Rc::new(Cell::new(0));
            let owner = std::thread::current().id();
            let result = parse_rows_controlled(
                &source,
                workers,
                |text, poll| {
                    parse_json_within_row(text, &mut || {
                        interior.fetch_add(1, Ordering::Relaxed);
                        poll()
                    })
                },
                || {
                    assert_eq!(std::thread::current().id(), owner);
                    polls.set(polls.get() + 1);
                    Ok::<(), Infallible>(())
                },
            )
            .unwrap();
            assert_eq!(result, expected, "workers={workers}");
            assert_eq!(result.iter().position(Result::is_err), Some(3));
            assert!(interior.load(Ordering::Relaxed) > 0);
            assert!(polls.get() > 0);
        }
    }

    #[test]
    fn json_cancellation_inside_first_record_keeps_parent_cause_and_joins() {
        for workers in [1, 2, PARSE_WORKERS] {
            let source = large_first_records(workers);
            let active = AtomicUsize::new(0);
            let interior = AtomicUsize::new(0);
            let completed = AtomicUsize::new(0);
            let cause = Rc::new(Cell::new(0));
            let owner = std::thread::current().id();
            let result = parse_rows_controlled(
                &source,
                workers,
                |text, poll| {
                    let _active = Active::new(&active);
                    let parsed = parse_json_within_row(text, &mut || {
                        interior.fetch_add(1, Ordering::Release);
                        poll()
                    });
                    if parsed.is_ok() {
                        completed.fetch_add(1, Ordering::Release);
                    }
                    parsed
                },
                || {
                    assert_eq!(std::thread::current().id(), owner);
                    let seen = interior.load(Ordering::Acquire);
                    if seen == 0 {
                        Ok(())
                    } else {
                        cause.set(seen);
                        Err(Rc::clone(&cause))
                    }
                },
            );
            let error = result.expect_err("actual work inside the JSON record cancels");
            assert!(Rc::ptr_eq(&error, &cause));
            assert!(cause.get() > 0);
            assert!(interior.load(Ordering::Acquire) <= workers);
            assert_eq!(completed.load(Ordering::Acquire), 0);
            assert_eq!(active.load(Ordering::Acquire), 0, "every decoder joined");
        }
    }

    #[test]
    fn json_workers_join_when_inside_record_control_or_decoder_unwinds() {
        for host_panics in [true, false] {
            let source = large_first_records(PARSE_WORKERS);
            let active = AtomicUsize::new(0);
            let interior = AtomicUsize::new(0);
            let completed = AtomicUsize::new(0);
            let worker_panicked = AtomicBool::new(false);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                parse_rows_controlled(
                    &source,
                    PARSE_WORKERS,
                    |text, poll| {
                        let _active = Active::new(&active);
                        let parsed = parse_json_within_row(text, &mut || {
                            interior.fetch_add(1, Ordering::Release);
                            if !host_panics && !worker_panicked.swap(true, Ordering::AcqRel) {
                                std::panic::resume_unwind(Box::new("inside JSON worker"));
                            }
                            poll()
                        });
                        if parsed.is_ok() {
                            completed.fetch_add(1, Ordering::Release);
                        }
                        parsed
                    },
                    || {
                        if host_panics && interior.load(Ordering::Acquire) != 0 {
                            std::panic::resume_unwind(Box::new("inside JSON parent"));
                        }
                        Ok::<(), Infallible>(())
                    },
                )
            }));
            let error = result.expect_err("panic inside the record must escape after joins");
            assert_eq!(
                error.downcast_ref::<&str>(),
                Some(&if host_panics {
                    "inside JSON parent"
                } else {
                    "inside JSON worker"
                })
            );
            assert!(interior.load(Ordering::Acquire) > 0);
            if host_panics {
                assert_eq!(completed.load(Ordering::Acquire), 0);
            } else {
                assert!(worker_panicked.load(Ordering::Acquire));
                assert!(completed.load(Ordering::Acquire) < source.len());
            }
            assert_eq!(active.load(Ordering::Acquire), 0, "every decoder joined");
        }
    }
}
