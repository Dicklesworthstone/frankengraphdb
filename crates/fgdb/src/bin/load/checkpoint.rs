//! Bounded Saved V1 checkpoint I/O. The Chronicle stream remains authoritative.
//!
//! Publication streams borrowed ID maps into an exclusive, private staging
//! file, syncs its contents, atomically renames it, then syncs its directory.
//! Failure before rename preserves the old checkpoint; failure after rename
//! may expose the new hint, which recovery still checks against Chronicle.
//! No buffered writer flushes from Drop. Abandoned staging is never recovered.

use super::super::quoted;
use fgdb::{BulkLoadCheckpoint, BulkLoadPolicy};
use fgdb_types::{CommitCx, CommitSeq, QueryCx};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const BUFFER_BYTES: usize = 64 * 1024;
const MAX_STAGE_ATTEMPTS: usize = 64;
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
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
        let bytes = key_bytes.checked_mul(6)
            .and_then(|n| rows.checked_mul(48).and_then(|entries| n.checked_add(entries)))
            .and_then(|n| n.checked_add(1024))
            .ok_or_else(|| invalid("checkpoint admission overflow"))?;
        Ok(Self { bytes, rows, key_bytes, key_len })
    }
    pub(super) fn values(self) -> usize {
        // A valid V1 object has eleven non-entry JSON values. new()'s larger
        // byte calculation has already checked the stronger overflow bound.
        self.rows + 11
    }
    pub(super) fn token_bytes(self) -> usize { self.key_len.max(64) }
    fn key(self, key: &str, total: &mut usize) -> io::Result<()> {
        if key.is_empty() || key.len() > self.key_len {
            return Err(invalid("checkpoint key length exceeds source admission"));
        }
        *total = total.checked_add(key.len())
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
}
impl View<'_> {
    fn header(self, limits: Limits) -> io::Result<()> {
        let cp = self.checkpoint;
        if self.rows_per_chunk == 0
            || self.rows_per_chunk > BulkLoadPolicy::MAX_ROWS_PER_CHUNK
            || cp.next_row > limits.rows
            || cp.vertices.len().checked_add(cp.edges.len()) != Some(cp.next_row)
            || (cp.next_row < limits.rows && !cp.next_row.is_multiple_of(self.rows_per_chunk))
            || cp.committed_chunks != cp.next_row.div_ceil(self.rows_per_chunk)
        {
            return Err(invalid("checkpoint counts do not describe a source prefix"));
        }
        let chunks = u64::try_from(cp.committed_chunks)
            .map_err(|_| invalid("checkpoint sequence overflow"))?;
        if self.base.0.checked_add(chunks) != Some(cp.frontier.0) {
            return Err(invalid("checkpoint frontier does not match its chunk count"));
        }
        let digest = |value: &str| value.len() == 64
            && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !digest(self.source_hash)
            || (self.base.0 == 0 && !self.base_marker.is_empty())
            || (self.base.0 != 0 && !digest(self.base_marker))
        {
            return Err(invalid("checkpoint digest is not canonical"));
        }
        Ok(())
    }
    pub(super) fn validate(self, limits: Limits) -> io::Result<()> {
        self.header(limits)?;
        let mut bytes = 0;
        for key in self.checkpoint.vertices.keys().chain(self.checkpoint.edges.keys()) {
            limits.key(key, &mut bytes)?;
        }
        if self.checkpoint.vertices.keys().any(|key| self.checkpoint.edges.contains_key(key)) {
            return Err(invalid("checkpoint caller keys overlap"));
        }
        Ok(())
    }
    fn write_to(self, out: &mut impl Write, limits: Limits) -> io::Result<()> {
        self.header(limits)?;
        let cp = self.checkpoint;
        let mut key_bytes = 0;
        out.write_all(b"{\"v\":1,\"vertices\":{")?;
        for (index, (key, id)) in cp.vertices.iter().enumerate() {
            // Validate each bounded key before quoting allocates its temporary.
            limits.key(key, &mut key_bytes)?;
            if cp.edges.contains_key(key) { return Err(invalid("checkpoint caller keys overlap")); }
            if index != 0 { out.write_all(b",")?; }
            write!(out, "{}:\"{}\"", quoted(key), id.0)?;
        }
        out.write_all(b"},\"edges\":{")?;
        for (index, (key, id)) in cp.edges.iter().enumerate() {
            limits.key(key, &mut key_bytes)?;
            if index != 0 { out.write_all(b",")?; }
            write!(out, "{}:\"{}\"", quoted(key), id.0)?;
        }
        // Byte-for-byte identical to the prior Saved::encode(), including its
        // map order, field order, quoted full-width IDs and final newline.
        writeln!(out,
            "}},\"next_row\":{},\"frontier\":{},\"committed_chunks\":{},\"base_frontier\":{},\"base_marker\":{},\"source_hash\":{},\"rows_per_chunk\":{}}}",
            cp.next_row, cp.frontier.0, cp.committed_chunks, self.base.0,
            quoted(self.base_marker), quoted(self.source_hash), self.rows_per_chunk)
    }
}

/// Only absence is None. Truncation, growth, malformed UTF-8, nonregular files
/// and over-budget input are errors; none is reinterpreted as a new import.
pub(super) fn read(cx: &QueryCx, path: &Path, limits: Limits) -> io::Result<Option<String>> {
    read_controlled(path, limits, &mut || cx.checkpoint().map_err(io::Error::other))
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
    buffer.try_reserve_exact(BUFFER_BYTES).map_err(io::Error::other)?;
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
        let next = bytes.len().checked_add(n).filter(|&n| n <= limits.bytes)
            .ok_or_else(|| invalid("checkpoint file exceeds source admission"))?;
        bytes.try_reserve_exact(next - bytes.len()).map_err(io::Error::other)?;
        bytes.extend_from_slice(&buffer[..n]);
    }
    control()?;
    if file.metadata()?.len() != metadata.len() || bytes.len() as u64 != metadata.len() {
        return Err(invalid("checkpoint length changed during read"));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| invalid("checkpoint must be UTF-8"))
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
        buffer.try_reserve_exact(BUFFER_BYTES).map_err(io::Error::other)?;
        Ok(Self { sink, control, buffer, bytes: 0, limit, failed: false })
    }
    fn checkpoint(&mut self) -> io::Result<()> {
        if self.failed { return Err(invalid("checkpoint output already failed")); }
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
        if self.bytes.checked_add(bytes.len()).is_none_or(|n| n > self.limit) {
            self.failed = true;
            return Err(invalid("checkpoint output exceeds source admission"));
        }
        if self.buffer.len() == BUFFER_BYTES { self.flush()?; }
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
                Err(error) => { self.failed = true; return Err(error); }
            }
        }
        self.buffer.clear();
        self.checkpoint()?;
        match self.sink.flush() {
            Ok(()) => Ok(()),
            Err(error) => { self.failed = true; Err(error) }
        }
    }
}

fn parent(path: &Path) -> &Path {
    path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."))
}
fn create_stage(
    path: &Path,
    next: &mut impl FnMut() -> io::Result<u64>,
    control: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<(File, PathBuf)> {
    let name = path.file_name().ok_or_else(|| invalid("checkpoint needs a file name"))?;
    for _ in 0..MAX_STAGE_ATTEMPTS {
        control()?;
        let mut temp_name = name.to_os_string();
        temp_name.push(format!(".tmp.{}.{}", std::process::id(), next()?));
        let temp = parent(path).join(temp_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        match options.open(&temp) {
            Ok(file) => return Ok((file, temp)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "checkpoint staging namespace exhausted"))
}

pub(super) fn persist(cx: &CommitCx, path: &Path, view: View<'_>, limits: Limits) -> io::Result<()> {
    persist_controlled(path, view, limits, &mut || cx.checkpoint().map_err(io::Error::other))
}
fn persist_controlled(
    path: &Path,
    view: View<'_>,
    limits: Limits,
    control: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    control()?;
    view.header(limits)?;
    let (mut file, temp) = create_stage(path, &mut || {
        NEXT_STAGE.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| invalid("checkpoint staging counter exhausted"))
    }, control)?;
    {
        let mut out = Buffer::new(&mut file, control, limits.bytes)?;
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

#[cfg(test)]
mod tests;
