//! Bounded Saved V1/V2 checkpoint I/O. Chronicle remains authoritative.
//!
//! V2 replaces repeated full ID maps with a source-ordered identity commitment.
//! Its producer advances through only newly acknowledged rows; a checkpoint
//! never supplies identities to recovery. V1 maps remain readable and checked.
//! Publication uses an exclusive, private staging file, syncs its contents,
//! atomically renames it, then syncs its directory.
//! Failure before rename preserves the old checkpoint; failure after rename
//! may expose the new hint, which recovery still checks against Chronicle.
//! No buffered writer flushes from Drop. Abandoned staging is never recovered.

use super::super::quoted;
use fgdb::{BulkLoadCheckpoint, BulkLoadPolicy, BulkRow};
use fgdb_types::{CommitCx, CommitSeq, QueryCx};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const BUFFER_BYTES: usize = 64 * 1024;
pub(super) const MAX_COMPACT_BYTES: usize = 1024;
const IDENTITY_PREFIX_DOMAIN: &[u8] = b"fgdb.cli.load.identity-prefix.v2\0";
const MAX_STAGE_ATTEMPTS: usize = 64;
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// One independent cursor over the sealed, prevalidated source. No duplicate
/// key map is retained. On a successful pass each source row is visited once,
/// across all checkpoint publications (including a resumed prefix). A failed
/// preparation can be retried without losing its source position.
pub(super) struct Prefix<R> {
    rows: R,
    next_row: usize,
    digest: [u8; 32],
}

impl<R> Prefix<R> {
    pub(super) fn new(rows: R) -> Self {
        Self {
            rows,
            next_row: 0,
            digest: fgdb_crypto::hash(IDENTITY_PREFIX_DOMAIN).0,
        }
    }
}

// Length framing, kind tagging and fixed-width IDs make the transcript
// unambiguous even for NUL/Unicode keys, overlapping ID domains and wide IDs.
// Chaining is source-order, not BTreeMap or permuted allocation-ID order.
fn identity_step(previous: &[u8; 32], kind: u8, key: &str, id: u128) -> io::Result<[u8; 32]> {
    let len = u64::try_from(key.len()).map_err(|_| invalid("checkpoint key length overflow"))?;
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(IDENTITY_PREFIX_DOMAIN);
    hash.update(previous);
    hash.update(&[kind]);
    hash.update(&len.to_be_bytes());
    hash.update(key.as_bytes());
    hash.update(&id.to_be_bytes());
    Ok(hash.finalize().0)
}

impl<R, E> Prefix<R>
where
    R: Iterator<Item = Result<BulkRow, E>> + Clone,
    E: std::fmt::Display,
{
    pub(super) fn advance(&mut self, cp: &BulkLoadCheckpoint) -> io::Result<String> {
        if cp.next_row < self.next_row
            || cp.vertices.len().checked_add(cp.edges.len()) != Some(cp.next_row)
        {
            return Err(invalid(
                "checkpoint identity prefix regressed or has missing rows",
            ));
        }
        // The private caller supplies append-only, engine-produced or fully
        // reconciled maps. Untrusted Saved maps NEVER feed this producer.
        // Rows clones have independent positions over the SAME sealed input.
        // Publish cursor and digest together; a source error, missing ID or
        // interruption cannot skip an entry when the operation is retried.
        let mut rows = self.rows.clone();
        let mut digest = self.digest;
        for _ in self.next_row..cp.next_row {
            let row = rows
                .next()
                .ok_or_else(|| invalid("source shorter than checkpoint"))?
                .map_err(|error| io::Error::other(error.to_string()))?;
            let (kind, key, id) = match &row {
                BulkRow::Vertex(row) => (0, &row.key, cp.vertices.get(&row.key).map(|id| id.0)),
                BulkRow::Edge(row) => (1, &row.key, cp.edges.get(&row.key).map(|id| id.0)),
            };
            let id = id.ok_or_else(|| invalid("checkpoint is missing a source identity"))?;
            digest = identity_step(&digest, kind, key, id)?;
        }
        self.rows = rows;
        self.next_row = cp.next_row;
        self.digest = digest;
        Ok(super::hex(&digest))
    }
}

/// Limits come from the completely validated SOURCE, never a checkpoint's own
/// declared counters. Six bytes per raw key byte covers JSON escaping; 48 per
/// entry covers the full u128 ID, quotes and delimiters; 1024 covers the header.
/// Recovery still owns a bounded JSON buffer/tree and the decoded ID maps.
#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub(super) bytes: usize,
    pub(super) rows: usize,
    pub(super) key_bytes: usize,
    pub(super) key_len: usize,
}
impl Limits {
    pub(super) fn new(rows: usize, key_bytes: usize, key_len: usize) -> io::Result<Self> {
        let bytes = key_bytes
            .checked_mul(6)
            .and_then(|n| {
                rows.checked_mul(48)
                    .and_then(|entries| n.checked_add(entries))
            })
            .and_then(|n| n.checked_add(1024))
            .ok_or_else(|| invalid("checkpoint admission overflow"))?;
        Ok(Self {
            bytes,
            rows,
            key_bytes,
            key_len,
        })
    }
    pub(super) fn values(self) -> usize {
        // A valid V1 object has eleven non-entry JSON values. new()'s larger
        // byte calculation has already checked the stronger overflow bound.
        self.rows + 11
    }
    pub(super) fn token_bytes(self) -> usize {
        self.key_len.max(64)
    }
    fn key(self, key: &str, total: &mut usize) -> io::Result<()> {
        if key.is_empty() || key.len() > self.key_len {
            return Err(invalid("checkpoint key length exceeds source admission"));
        }
        *total = total
            .checked_add(key.len())
            .filter(|&n| n <= self.key_bytes)
            .ok_or_else(|| invalid("checkpoint key bytes exceed source admission"))?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct View<'a> {
    pub(super) checkpoint: &'a BulkLoadCheckpoint,
    pub(super) base: CommitSeq,
    pub(super) base_marker: &'a str,
    pub(super) source_hash: &'a str,
    pub(super) rows_per_chunk: usize,
    /// None selects the durable V1 map format. V2 is a receipt over a prefix
    /// derived from Chronicle/engine output, never caller-supplied identities.
    pub(super) identity_hash: Option<&'a str>,
}
impl View<'_> {
    fn header(self, limits: Limits) -> io::Result<()> {
        let cp = self.checkpoint;
        if self.rows_per_chunk == 0
            || self.rows_per_chunk > BulkLoadPolicy::MAX_ROWS_PER_CHUNK
            || cp.next_row > limits.rows
            || (self.identity_hash.is_none()
                && cp.vertices.len().checked_add(cp.edges.len()) != Some(cp.next_row))
            || (cp.next_row < limits.rows && !cp.next_row.is_multiple_of(self.rows_per_chunk))
            || cp.committed_chunks != cp.next_row.div_ceil(self.rows_per_chunk)
        {
            return Err(invalid("checkpoint counts do not describe a source prefix"));
        }
        let chunks = u64::try_from(cp.committed_chunks)
            .map_err(|_| invalid("checkpoint sequence overflow"))?;
        if self.base.0.checked_add(chunks) != Some(cp.frontier.0) {
            return Err(invalid(
                "checkpoint frontier does not match its chunk count",
            ));
        }
        let digest = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if !digest(self.source_hash)
            || self.identity_hash.is_some_and(|value| !digest(value))
            || (self.base.0 == 0 && !self.base_marker.is_empty())
            || (self.base.0 != 0 && !digest(self.base_marker))
        {
            return Err(invalid("checkpoint digest is not canonical"));
        }
        Ok(())
    }
    pub(super) fn validate(self, limits: Limits) -> io::Result<()> {
        self.header(limits)?;
        if self.identity_hash.is_some() {
            // Decoded V2 checkpoints deliberately have no maps. Only the
            // recovered source prefix can establish their identity commitment.
            return Ok(());
        }
        let mut bytes = 0;
        for key in self
            .checkpoint
            .vertices
            .keys()
            .chain(self.checkpoint.edges.keys())
        {
            limits.key(key, &mut bytes)?;
        }
        if self
            .checkpoint
            .vertices
            .keys()
            .any(|key| self.checkpoint.edges.contains_key(key))
        {
            return Err(invalid("checkpoint caller keys overlap"));
        }
        Ok(())
    }
    fn write_to(self, out: &mut impl Write, limits: Limits) -> io::Result<()> {
        self.header(limits)?;
        let cp = self.checkpoint;
        if let Some(identity_hash) = self.identity_hash {
            return writeln!(
                out,
                "{{\"v\":2,\"next_row\":{},\"frontier\":{},\"committed_chunks\":{},\"base_frontier\":{},\"base_marker\":{},\"source_hash\":{},\"rows_per_chunk\":{},\"identity_hash\":{}}}",
                cp.next_row,
                cp.frontier.0,
                cp.committed_chunks,
                self.base.0,
                quoted(self.base_marker),
                quoted(self.source_hash),
                self.rows_per_chunk,
                quoted(identity_hash),
            );
        }
        let mut key_bytes = 0;
        out.write_all(b"{\"v\":1,\"vertices\":{")?;
        for (index, (key, id)) in cp.vertices.iter().enumerate() {
            // Validate each bounded key before quoting allocates its temporary.
            limits.key(key, &mut key_bytes)?;
            if cp.edges.contains_key(key) {
                return Err(invalid("checkpoint caller keys overlap"));
            }
            if index != 0 {
                out.write_all(b",")?;
            }
            write!(out, "{}:\"{}\"", quoted(key), id.0)?;
        }
        out.write_all(b"},\"edges\":{")?;
        for (index, (key, id)) in cp.edges.iter().enumerate() {
            limits.key(key, &mut key_bytes)?;
            if index != 0 {
                out.write_all(b",")?;
            }
            write!(out, "{}:\"{}\"", quoted(key), id.0)?;
        }
        // Byte-for-byte identical to the prior Saved::encode(), including its
        // map order, field order, quoted full-width IDs and final newline.
        writeln!(
            out,
            "}},\"next_row\":{},\"frontier\":{},\"committed_chunks\":{},\"base_frontier\":{},\"base_marker\":{},\"source_hash\":{},\"rows_per_chunk\":{}}}",
            cp.next_row,
            cp.frontier.0,
            cp.committed_chunks,
            self.base.0,
            quoted(self.base_marker),
            quoted(self.source_hash),
            self.rows_per_chunk
        )
    }
}

/// Only absence is None. Truncation, growth, malformed UTF-8, nonregular files
/// and over-budget input are errors; none is reinterpreted as a new import.
pub(super) fn read(cx: &QueryCx, path: &Path, limits: Limits) -> io::Result<Option<String>> {
    read_controlled(path, limits, &mut || {
        cx.checkpoint().map_err(io::Error::other)
    })
}
fn read_controlled(
    path: &Path,
    limits: Limits,
    control: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<Option<String>> {
    control()?;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    control()?;
    let metadata = file.metadata()?;
    let maximum = u64::try_from(limits.bytes).map_err(|_| invalid("checkpoint size overflow"))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(invalid("checkpoint file exceeds source admission"));
    }
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(BUFFER_BYTES)
        .map_err(io::Error::other)?;
    buffer.resize(BUFFER_BYTES, 0);
    let mut bytes = Vec::new();
    loop {
        control()?;
        // The extra byte detects growth at the exact cap without a large read.
        let allowance = limits.bytes.saturating_sub(bytes.len()).saturating_add(1);
        let take = allowance.min(BUFFER_BYTES);
        let n = match file.read(&mut buffer[..take]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let next = bytes
            .len()
            .checked_add(n)
            .filter(|&n| n <= limits.bytes)
            .ok_or_else(|| invalid("checkpoint file exceeds source admission"))?;
        bytes
            .try_reserve_exact(next - bytes.len())
            .map_err(io::Error::other)?;
        bytes.extend_from_slice(&buffer[..n]);
    }
    control()?;
    if file.metadata()?.len() != metadata.len() || bytes.len() as u64 != metadata.len() {
        return Err(invalid("checkpoint length changed during read"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| invalid("checkpoint must be UTF-8"))
}

/// A bounded sink whose drop NEVER flushes. Failed/cancelled writes fuse the
/// sink: retrying cannot duplicate a partially written buffer into staging.
struct Buffer<'a, W, F> {
    sink: &'a mut W,
    control: &'a mut F,
    buffer: Vec<u8>,
    bytes: usize,
    limit: usize,
    failed: bool,
}
impl<'a, W: Write, F: FnMut() -> io::Result<()>> Buffer<'a, W, F> {
    fn new(sink: &'a mut W, control: &'a mut F, limit: usize) -> io::Result<Self> {
        control()?;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(BUFFER_BYTES)
            .map_err(io::Error::other)?;
        Ok(Self {
            sink,
            control,
            buffer,
            bytes: 0,
            limit,
            failed: false,
        })
    }
    fn checkpoint(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(invalid("checkpoint output already failed"));
        }
        if let Err(error) = (self.control)() {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }
}
impl<W: Write, F: FnMut() -> io::Result<()>> Write for Buffer<'_, W, F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.checkpoint()?;
        if self
            .bytes
            .checked_add(bytes.len())
            .is_none_or(|n| n > self.limit)
        {
            self.failed = true;
            return Err(invalid("checkpoint output exceeds source admission"));
        }
        if self.buffer.len() == BUFFER_BYTES {
            self.flush()?;
        }
        let n = bytes.len().min(BUFFER_BYTES - self.buffer.len());
        self.buffer.extend_from_slice(&bytes[..n]);
        self.bytes += n;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.checkpoint()?;
        let mut written = 0;
        while written < self.buffer.len() {
            self.checkpoint()?;
            match self.sink.write(&self.buffer[written..]) {
                Ok(0) => {
                    self.failed = true;
                    return Err(io::Error::from(io::ErrorKind::WriteZero));
                }
                Ok(n) => written += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
        self.buffer.clear();
        self.checkpoint()?;
        match self.sink.flush() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}
fn create_stage(
    path: &Path,
    next: &mut impl FnMut() -> io::Result<u64>,
    control: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<(File, PathBuf)> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid("checkpoint needs a file name"))?;
    for _ in 0..MAX_STAGE_ATTEMPTS {
        control()?;
        let mut temp_name = name.to_os_string();
        temp_name.push(format!(".tmp.{}.{}", std::process::id(), next()?));
        let temp = parent(path).join(temp_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp) {
            Ok(file) => return Ok((file, temp)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "checkpoint staging namespace exhausted",
    ))
}

pub(super) fn persist(
    cx: &CommitCx,
    path: &Path,
    view: View<'_>,
    limits: Limits,
) -> io::Result<()> {
    persist_controlled(path, view, limits, &mut || {
        cx.checkpoint().map_err(io::Error::other)
    })
}
fn persist_controlled(
    path: &Path,
    view: View<'_>,
    limits: Limits,
    control: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    control()?;
    view.header(limits)?;
    let (mut file, temp) = create_stage(
        path,
        &mut || {
            NEXT_STAGE
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| invalid("checkpoint staging counter exhausted"))
        },
        control,
    )?;
    {
        let maximum = if view.identity_hash.is_some() {
            limits.bytes.min(MAX_COMPACT_BYTES)
        } else {
            limits.bytes
        };
        let mut out = Buffer::new(&mut file, control, maximum)?;
        view.write_to(&mut out, limits)?;
        out.flush()?;
    }
    control()?;
    file.sync_all()?;
    drop(file);
    control()?;
    std::fs::rename(&temp, path)?;
    control()?;
    let directory = File::open(parent(path))?;
    control()?;
    directory.sync_all()
}

/// Whether a committed load prefix needs another complete saved receipt.
///
/// The driver separately publishes the origin/reconciled prefix before work
/// resumes. Ordinary callbacks publish at chunk counts 1, 2, 4, 8, ... and at
/// the final (possibly partial) chunk. For V1 and a fixed chunk size, the sum of map
/// entries encoded in one uninterrupted load is less than three times the
/// source row count, rather than quadratic in the number of chunks. Encoded
/// bytes have the same bound using the already-admitted maximum key length.
/// V2 bounds each image independently of maps and advances its commitment only
/// over newly covered rows. Neither changes Chronicle commits or validation.
///
/// No checkpoint is authoritative: resume verifies saved maps/commitments and
/// reconstructs EVERY committed creation since the original base. Progress
/// describes durable chunks and may run ahead of this hint; replayed progress
/// retains its original sequence number. The final image is always required
/// before successful completion. No multiplying threshold can wrap at usize::MAX.
pub(super) fn due(committed_chunks: usize, next_row: usize, source_rows: usize) -> bool {
    committed_chunks.is_power_of_two() || next_row == source_rows
}

#[cfg(test)]
mod cadence_tests {
    use super::*;
    use fgdb_types::{EId, VId};

    #[derive(Default)]
    struct CountBytes(usize);
    impl Write for CountBytes {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.checked_add(bytes.len()).expect("fixture byte count");
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Cost {
        old_bytes: usize,
        new_bytes: usize,
        old_entries: usize,
        new_entries: usize,
        old_images: usize,
        new_images: usize,
    }

    fn view<'a>(
        checkpoint: &'a BulkLoadCheckpoint,
        base_marker: &'a str,
        source_hash: &'a str,
        rows_per_chunk: usize,
    ) -> View<'a> {
        View {
            checkpoint,
            base: CommitSeq(7),
            base_marker,
            source_hash,
            rows_per_chunk,
            identity_hash: None,
        }
    }

    // Both arms invoke the incumbent V1 encoder on the SAME actual key maps.
    // Only publication cadence differs. Full-width IDs, escaping, Unicode and
    // both key kinds keep this from being a fixed-width byte-counting formula.
    fn compare(total: usize, chunk: usize) -> Cost {
        let mut cp = BulkLoadCheckpoint {
            frontier: CommitSeq(7),
            ..BulkLoadCheckpoint::default()
        };
        let limits = Limits::new(total, total * 32, 32).unwrap();
        let base_marker = "a".repeat(64);
        let source_hash = "b".repeat(64);
        let mut old = CountBytes::default();
        let mut new = CountBytes::default();
        let mut cost = Cost {
            old_bytes: 0,
            new_bytes: 0,
            old_entries: 0,
            new_entries: 0,
            old_images: 1,
            new_images: 1,
        };
        view(&cp, &base_marker, &source_hash, chunk)
            .write_to(&mut old, limits)
            .unwrap();
        view(&cp, &base_marker, &source_hash, chunk)
            .write_to(&mut new, limits)
            .unwrap();
        for first in (0..total).step_by(chunk) {
            let end = (first + chunk).min(total);
            for row in first..end {
                let key = format!("key\"\\\nλ{row:08}");
                let identity = u128::MAX - row as u128;
                if row.is_multiple_of(3) {
                    cp.vertices.insert(key, VId(identity));
                } else {
                    cp.edges.insert(key, EId(identity));
                }
            }
            cp.next_row = end;
            cp.committed_chunks += 1;
            cp.frontier.0 += 1;
            view(&cp, &base_marker, &source_hash, chunk)
                .write_to(&mut old, limits)
                .unwrap();
            cost.old_images += 1;
            cost.old_entries += end;
            if due(cp.committed_chunks, end, total) {
                view(&cp, &base_marker, &source_hash, chunk)
                    .write_to(&mut new, limits)
                    .unwrap();
                cost.new_images += 1;
                cost.new_entries += end;
            }
        }
        let mut final_bytes = Vec::new();
        view(&cp, &base_marker, &source_hash, chunk)
            .write_to(&mut final_bytes, limits)
            .unwrap();
        let saved = super::super::Saved::decode(std::str::from_utf8(&final_bytes).unwrap(), limits)
            .unwrap();
        super::super::same_checkpoint(&saved.checkpoint, &cp).unwrap();
        cost.old_bytes = old.0;
        cost.new_bytes = new.0;
        cost
    }

    #[test]
    fn cadence_keeps_the_first_two_checkpoints_and_every_final_prefix() {
        let checkpoints: Vec<_> = (1..=9)
            .filter(|&chunk| due(chunk, (chunk * 11).min(97), 97))
            .collect();
        assert_eq!(checkpoints, [1, 2, 4, 8, 9]);
        assert!(due(0, 0, 0));
        assert!(!due(0, 0, 1));
        // Completion need not coincide with a power of two or a full chunk.
        assert!(due(3, 17, 17));
        assert!(!due(3, 17, 18));
    }

    #[test]
    fn cadence_is_independent_of_restart_state_and_cannot_overflow() {
        // An origin checkpoint and reconciliation are forced by the driver.
        // Resuming from a non-geometric prefix does not reset chunk ordinals.
        let remaining: Vec<_> = (6..=10).filter(|&n| due(n, n * 7, 70)).collect();
        assert_eq!(remaining, [8, 10]);
        let high = 1usize << (usize::BITS - 1);
        assert!(due(high, high, usize::MAX));
        assert!(!due(high + 1, high + 1, usize::MAX));
        assert!(!due(usize::MAX - 1, usize::MAX - 1, usize::MAX));
        assert!(due(usize::MAX, usize::MAX, usize::MAX));
    }

    #[test]
    fn real_v1_encoder_visits_less_than_three_source_prefixes() {
        for total in [0, 1, 7, 31, 257, 1025] {
            for chunk in [1, 2, 7, 32, 256] {
                let cost = compare(total, chunk);
                assert!(
                    cost.new_bytes <= cost.old_bytes,
                    "{total}/{chunk}: {cost:?}"
                );
                assert!(cost.new_images <= cost.old_images, "{cost:?}");
                if total != 0 {
                    assert!(cost.new_entries < 3 * total, "{total}/{chunk}: {cost:?}");
                } else {
                    assert_eq!(cost.new_entries, 0);
                }
                if total.div_ceil(chunk) <= 3 {
                    assert_eq!(cost.new_bytes, cost.old_bytes, "small loads stay unchanged");
                }
            }
        }
    }

    #[test]
    fn real_v1_encoder_reduces_repeated_mapping_work_at_fixed_source_size() {
        let cost = compare(1025, 1);
        assert_eq!(cost.old_entries, 1025 * 1026 / 2);
        assert_eq!(cost.new_entries, 3072);
        assert_eq!((cost.old_images, cost.new_images), (1026, 13));
        assert!(cost.new_bytes * 100 < cost.old_bytes, "{cost:?}");
    }

    #[test]
    #[ignore = "same-invocation V1 encoder comparison; not native database latency"]
    fn benchmark_checkpoint_mapping_serialization() {
        for (rows, chunk) in [(24_000, 256), (240_000, 8192), (4096, 1)] {
            let cost = compare(rows, chunk);
            eprintln!("rows={rows} chunk={chunk} {cost:?}");
            assert!(cost.new_bytes <= cost.old_bytes);
            assert!(cost.new_entries < 3 * rows);
        }
    }
}

#[cfg(test)]
mod tests;
