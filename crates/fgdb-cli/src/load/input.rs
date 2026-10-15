//! Replayed file input with independent cursors and bounded payload buffering.
//!
//! The opened handle, length and one BLAKE3 digest per 64 KiB block are retained.
//! Every newly fetched block must match its seal; clones never share seek state
//! or trust a reopened path. This is an invocation-local checked source image,
//! not a filesystem snapshot, external authentication or durable import state.
//!
//! A leading '[' selects a JSON array of the same graph-row objects accepted
//! by NDJSON. Array separators/indentation are not records; object bytes are
//! bounded even across physical lines. Both formats use the same sealed input,
//! source digest, row decoder, chunk writer and durable continuation protocol.

mod array;

use fgdb_crypto::{Digest, Hasher, hash};
use fgdb_types::QueryCx;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

const BLOCK_BYTES: usize = 64 * 1024;
pub(super) const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// NDJSON: raw bytes excluding LF, including CR in a CRLF terminator.
/// JSON arrays: the complete object, including braces and internal whitespace.
pub(super) const MAX_RECORD_BYTES: usize = 1024 * 1024;

struct Image {
    file: Mutex<File>,
    len: u64,
    blocks: Box<[Digest]>,
    record_limit: usize,
    array: bool,
}
pub(super) struct Input {
    image: Arc<Image>,
    source_hash: Digest,
    records: usize,
}

fn refused(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn stopped(cx: &QueryCx) -> io::Result<()> {
    cx.checkpoint().map_err(io::Error::other)
}
fn read_exact_controlled(
    file: &mut File,
    mut bytes: &mut [u8],
    control: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    while !bytes.is_empty() {
        control()?;
        match file.read(bytes) {
            Ok(0) => return Err(refused("SourceChanged: truncated input")),
            Ok(n) => bytes = &mut bytes[n..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    control()
}
fn check_length(file: &File, len: u64) -> io::Result<()> {
    if file.metadata()?.len() != len {
        return Err(refused("SourceChanged: input length changed"));
    }
    Ok(())
}

/// Inspect only through the first non-whitespace byte, using the sealing
/// buffer and the same opened handle. This chooses a grammar, not trusted
/// content: sealing and replay must each accept that grammar independently.
fn is_array(
    file: &mut File,
    len: u64,
    buffer: &mut [u8],
    control: &mut impl FnMut() -> io::Result<()>,
) -> io::Result<bool> {
    let mut remaining = len;
    let mut array = false;
    while remaining != 0 {
        control()?;
        check_length(file, len)?;
        let size = remaining.min(buffer.len() as u64) as usize;
        read_exact_controlled(file, &mut buffer[..size], control)?;
        if let Some(&byte) = buffer[..size]
            .iter()
            .find(|&&byte| !array::whitespace(byte))
        {
            array = byte == b'[';
            break;
        }
        remaining -= size as u64;
    }
    control()?;
    check_length(file, len)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(array)
}

impl Input {
    pub(super) fn open(
        cx: &QueryCx,
        path: &Path,
        binding: &[u8],
        max_records: usize,
    ) -> io::Result<Self> {
        Self::open_controlled(
            path,
            binding,
            MAX_INPUT_BYTES,
            MAX_RECORD_BYTES,
            max_records,
            &mut || stopped(cx),
        )
    }

    fn open_controlled(
        path: &Path,
        binding: &[u8],
        max_bytes: u64,
        record_limit: usize,
        max_records: usize,
        control: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<Self> {
        control()?;
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(refused("input must be a regular file"));
        }
        let len = metadata.len();
        if len > max_bytes {
            return Err(refused("SourceLimit: input_bytes"));
        }
        let count = usize::try_from(len.div_ceil(BLOCK_BYTES as u64))
            .map_err(|_| refused("SourceLimit: input_blocks"))?;
        let mut blocks = Vec::new();
        blocks.try_reserve_exact(count).map_err(io::Error::other)?;
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(BLOCK_BYTES)
            .map_err(io::Error::other)?;
        buffer.resize(BLOCK_BYTES, 0);
        let array = is_array(&mut file, len, &mut buffer, control)?;
        let mut framing = array.then(|| array::Array::new(record_limit, max_records));
        let mut digest = Hasher::new();
        let mut remaining = len;
        let mut records = 0usize;
        let mut record_bytes = 0usize;
        while remaining != 0 {
            control()?;
            check_length(&file, len)?;
            let size = remaining.min(BLOCK_BYTES as u64) as usize;
            let bytes = &mut buffer[..size];
            read_exact_controlled(&mut file, bytes, control)?;
            // A record cap is checked during sealing, before ANY record buffer
            // or JSON tree can allocate according to hostile source lengths.
            if let Some(framing) = &mut framing {
                for &byte in bytes.iter() {
                    framing.push(byte)?;
                }
            } else {
                for part in bytes.split_inclusive(|&byte| byte == b'\n') {
                    let terminated = part.last() == Some(&b'\n');
                    record_bytes = record_bytes
                        .checked_add(part.len() - usize::from(terminated))
                        .ok_or_else(|| refused("SourceLimit: record_bytes"))?;
                    if record_bytes > record_limit {
                        return Err(refused("SourceLimit: record_bytes"));
                    }
                    if terminated {
                        records = records
                            .checked_add(1)
                            .ok_or_else(|| refused("SourceLimit: source_rows"))?;
                        if records > max_records {
                            return Err(refused("SourceLimit: source_rows"));
                        }
                        record_bytes = 0;
                    }
                }
            }
            digest.update(bytes);
            blocks.push(hash(bytes));
            remaining -= size as u64;
        }
        if let Some(framing) = framing {
            framing.finish()?;
            records = framing.records();
        } else if record_bytes != 0 {
            records = records
                .checked_add(1)
                .ok_or_else(|| refused("SourceLimit: source_rows"))?;
            if records > max_records {
                return Err(refused("SourceLimit: source_rows"));
            }
        }
        control()?;
        check_length(&file, len)?;
        // Keep Saved V1's exact raw-file-plus-binding transcript. Hashing a
        // digest of the file here would silently invalidate existing resumes.
        digest.update(binding);
        Ok(Self {
            image: Arc::new(Image {
                file: Mutex::new(file),
                len,
                blocks: blocks.into(),
                record_limit,
                array,
            }),
            source_hash: digest.finalize(),
            records,
        })
    }
    pub(super) fn source_hash(&self) -> &Digest {
        &self.source_hash
    }
    pub(super) fn records(&self) -> usize {
        self.records
    }
    pub(super) fn reader(&self) -> Reader {
        Reader {
            image: self.image.clone(),
            offset: 0,
            line: 1,
            cache: Vec::new(),
            block: None,
            ended: false,
            array: self
                .image
                .array
                .then(|| array::Array::new(self.image.record_limit, self.records)),
        }
    }
}

pub(super) struct Reader {
    image: Arc<Image>,
    offset: u64,
    line: usize,
    cache: Vec<u8>,
    block: Option<usize>,
    ended: bool,
    array: Option<array::Array>,
}
impl Clone for Reader {
    fn clone(&self) -> Self {
        // Reload the current block at the exact cursor offset. No payload copy
        // or fallible file-open operation is hidden inside iterator cloning.
        Self {
            image: self.image.clone(),
            offset: self.offset,
            line: self.line,
            cache: Vec::new(),
            block: None,
            ended: self.ended,
            array: self.array.clone(),
        }
    }
}
impl Reader {
    pub(super) fn line(&self) -> usize {
        // Array input reports its one-based logical object ordinal, not a
        // physical newline. The existing row/error/checkpoint owners use it.
        self.line
    }
    fn file(&self) -> io::Result<MutexGuard<'_, File>> {
        self.image
            .file
            .lock()
            .map_err(|_| refused("input handle lock poisoned"))
    }
    pub(super) fn next_line(&mut self, cx: &QueryCx) -> io::Result<Option<String>> {
        self.next_controlled(&mut || stopped(cx))
    }
    fn next_controlled(
        &mut self,
        control: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<Option<String>> {
        if self.ended {
            return Ok(None);
        }
        let result = if self.array.is_some() {
            self.array_controlled(control)
        } else {
            self.line_controlled(control)
        };
        if !matches!(&result, Ok(Some(_))) {
            self.ended = true;
        }
        result
    }

    fn fetch_block(&mut self, control: &mut impl FnMut() -> io::Result<()>) -> io::Result<()> {
        let block = (self.offset / BLOCK_BYTES as u64) as usize;
        if self.block != Some(block) {
            let start = block as u64 * BLOCK_BYTES as u64;
            let size = (self.image.len - start).min(BLOCK_BYTES as u64) as usize;
            // Buffer allocation and file seek/read form one private fetch.
            // Refusal never publishes partially read bytes into the cache.
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(size).map_err(io::Error::other)?;
            bytes.resize(size, 0);
            {
                let mut file = self.file()?;
                check_length(&file, self.image.len)?;
                file.seek(SeekFrom::Start(start))?;
                read_exact_controlled(&mut file, &mut bytes, control)?;
                check_length(&file, self.image.len)?;
            }
            if self.image.blocks.get(block) != Some(&hash(&bytes)) {
                return Err(refused("SourceChanged: input block changed"));
            }
            self.cache = bytes;
            self.block = Some(block);
        }
        Ok(())
    }

    fn array_controlled(
        &mut self,
        control: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<Option<String>> {
        let mut row = Vec::new();
        loop {
            control()?;
            if self.offset == self.image.len {
                {
                    let file = self.file()?;
                    check_length(&file, self.image.len)?;
                }
                self.array.as_ref().expect("array reader").finish()?;
                return Ok(None);
            }
            self.fetch_block(control)?;
            let start = (self.offset % BLOCK_BYTES as u64) as usize;
            let mut part = None;
            let mut end = self.cache.len();
            let mut complete = false;
            for index in start..self.cache.len() {
                let piece = self
                    .array
                    .as_mut()
                    .expect("array reader")
                    .push(self.cache[index])?;
                if piece != array::Piece::Skip {
                    part.get_or_insert(index);
                }
                if piece == array::Piece::Last {
                    end = index + 1;
                    complete = true;
                    break;
                }
            }
            if let Some(part) = part {
                // The framer admitted every raw object byte before this copy.
                row.try_reserve_exact(end - part)
                    .map_err(io::Error::other)?;
                row.extend_from_slice(&self.cache[part..end]);
            }
            self.offset += (end - start) as u64;
            if complete {
                let text = String::from_utf8(row).map_err(|_| refused("input must be UTF-8"))?;
                control()?;
                self.line += 1;
                return Ok(Some(text));
            }
        }
    }
    fn line_controlled(
        &mut self,
        control: &mut impl FnMut() -> io::Result<()>,
    ) -> io::Result<Option<String>> {
        control()?;
        if self.offset == self.image.len {
            let file = self.file()?;
            check_length(&file, self.image.len)?;
            return Ok(None);
        }
        let mut row = Vec::new();
        loop {
            control()?;
            self.fetch_block(control)?;
            let start = (self.offset % BLOCK_BYTES as u64) as usize;
            let suffix = &self.cache[start..];
            let newline = suffix.iter().position(|&byte| byte == b'\n');
            let size = newline.unwrap_or(suffix.len());
            let total = row
                .len()
                .checked_add(size)
                .ok_or_else(|| refused("SourceLimit: record_bytes"))?;
            if total > self.image.record_limit {
                return Err(refused("SourceLimit: record_bytes"));
            }
            row.try_reserve_exact(size).map_err(io::Error::other)?;
            row.extend_from_slice(&suffix[..size]);
            self.offset += size as u64;
            if newline.is_some() {
                self.offset += 1;
                if row.last() == Some(&b'\r') {
                    row.pop();
                }
                break;
            }
            if self.offset == self.image.len {
                break;
            }
        }
        let text = String::from_utf8(row).map_err(|_| refused("input must be UTF-8"))?;
        control()?;
        self.line += 1;
        Ok(Some(text))
    }
}

#[cfg(test)]
mod tests;
