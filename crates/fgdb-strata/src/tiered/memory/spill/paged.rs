//! Incremental, authenticated result/sort scratch. An append-only binary tree
//! lives beside its pages; only a bounded merge frontier stays resident. This
//! is query-private scratch, never a database format or recovery authority.

use super::*;

const LEVELS: usize = usize::BITS as usize;
const NODE_BYTES: usize = 64;
const BRANCH_BYTES: usize = 2 * NODE_BYTES;
const WRITER_METADATA: usize = core::mem::size_of::<[Option<PageNode>; LEVELS]>() + BRANCH_BYTES;

#[derive(Clone, Copy)]
struct PageNode {
    offset: u64,
    first: u64,
    pages: u64,
    len: u64,
    digest: [u8; 32],
}

impl PageNode {
    fn encode(self, bytes: &mut [u8]) {
        for (slot, value) in [self.offset, self.first, self.pages, self.len]
            .into_iter()
            .enumerate()
        {
            bytes[slot * 8..slot * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
        bytes[32..NODE_BYTES].copy_from_slice(&self.digest);
    }

    // Called only after the containing branch has authenticated, with an
    // exactly sized slice. No file-controlled count drives an allocation.
    fn decode(bytes: &[u8]) -> Self {
        let word = |slot: usize| {
            let mut value = [0; 8];
            value.copy_from_slice(&bytes[slot * 8..slot * 8 + 8]);
            u64::from_le_bytes(value)
        };
        let mut digest = [0; 32];
        digest.copy_from_slice(&bytes[32..NODE_BYTES]);
        Self { offset: word(0), first: word(1), pages: word(2), len: word(3), digest }
    }

    fn stored_len(self) -> u64 {
        if self.pages == 1 { self.len } else { BRANCH_BYTES as u64 }
    }
}

fn page_hash(id: u64, start: u64, page_bytes: usize, node: PageNode, bytes: &[u8]) -> [u8; 32] {
    let mut hash = fgdb_crypto::Hasher::new();
    let domain: &[u8] = if node.pages == 1 {
        b"fgdb.strata.paged-spill.leaf.v1"
    } else {
        b"fgdb.strata.paged-spill.branch.v1"
    };
    hash.update(domain);
    for value in [id, start, page_bytes as u64, node.offset, node.first, node.pages, node.len] {
        hash.update(&value.to_le_bytes());
    }
    hash.update(bytes);
    hash.finalize().0
}

/// Process-local authority for one completely accepted paged run. A clone
/// retains constant-size coordinates and a root digest, not a page catalog.
/// No public constructor/deserializer can assert a root over caller bytes.
#[derive(Clone)]
pub struct PagedSpillRun {
    owner: Arc<()>,
    id: u64,
    start: u64,
    end: u64,
    page_bytes: usize,
    root: Option<PageNode>,
}

impl PagedSpillRun {
    pub fn len(&self) -> usize { self.root.map_or(0, |root| root.len as usize) }
    pub fn is_empty(&self) -> bool { self.root.is_none() }
    pub fn page_count(&self) -> u64 { self.root.map_or(0, |root| root.pages) }
    pub fn page_bytes(&self) -> usize { self.page_bytes }

    pub fn page_len(&self, page: u64) -> Option<usize> {
        let root = self.root?;
        if page >= root.pages { return None; }
        let offset = page.checked_mul(self.page_bytes as u64)?;
        Some(root.len.checked_sub(offset)?.min(self.page_bytes as u64) as usize)
    }

    fn validate_node(&self, node: PageNode) -> Result<(), SpillError> {
        let last = node.first.checked_add(node.pages).ok_or(SpillError::InvalidRun)?;
        let end = node.offset.checked_add(node.stored_len()).ok_or(SpillError::InvalidRun)?;
        if node.pages == 0 || last > self.page_count() || node.offset < self.start || end > self.end {
            return Err(SpillError::InvalidRun);
        }
        let expected = if last == self.page_count() {
            (self.len() as u64).checked_sub(node.first.checked_mul(self.page_bytes as u64)
                .ok_or(SpillError::InvalidRun)?)
        } else {
            node.pages.checked_mul(self.page_bytes as u64)
        }.ok_or(SpillError::InvalidRun)?;
        if node.len != expected { return Err(SpillError::InvalidRun); }
        Ok(())
    }
}

impl fmt::Debug for PagedSpillRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PagedSpillRun")
            .field("bytes", &self.len()).field("pages", &self.page_count())
            .field("page_bytes", &self.page_bytes).finish_non_exhaustive()
    }
}

/// Exclusively borrows the scratch file until finish/drop. Any failed write,
/// unwind or abandoned writer is terminal and fences the file. There is no
/// observable partial run. Buffered bytes and merge-frontier memory are charged
/// to the file's existing pool; the producer's own allocations are separate.
pub struct PagedSpillWriter<'a, F> {
    scratch: &'a mut SpillFile<F>,
    buffer: TrackedBytes,
    frontier: [Option<PageNode>; LEVELS],
    id: u64,
    start: u64,
    len: usize,
    filled: usize,
    pages: u64,
    failed: bool,
}

impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin> SpillFile<F> {
    /// Start an unknown-length paged run. page_bytes must be in 1..=64 KiB.
    /// The run's logical ceiling is max_run_bytes; max_file_bytes also charges
    /// every 128-byte branch record. max_runs counts this attempt, not pages.
    /// Allocation/initial-control refusal occurs before reserving an attempt.
    ///
    /// finish computes the root from hashes retained from actual writes, NEVER
    /// from unverified disk rereads. At most usize::BITS merge nodes are live.
    /// Successful reads later cost one payload plus O(log pages) branch reads.
    /// Old append/append_from/restore formats and admission remain unchanged.
    pub fn paged_writer(&mut self, cx: &QueryCx, page_bytes: usize)
        -> Result<PagedSpillWriter<'_, F>, SpillError>
    {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(SpillError::Interrupted)?;
            self.paged_writer_inner(page_bytes)
        })
    }

    fn paged_writer_inner(&mut self, page_bytes: usize) -> Result<PagedSpillWriter<'_, F>, SpillError> {
        if self.io_pending { return Err(SpillError::PoisonedFile); }
        if page_bytes == 0 || page_bytes > IO_CHUNK_BYTES { return Err(SpillError::InvalidLimits); }
        if self.stats.reserved_runs >= self.limits.max_runs {
            return Err(SpillError::RunLimit { limit: self.limits.max_runs });
        }
        let buffer = self.pool.allocate_inner(page_bytes, WRITER_METADATA)?;
        let id = self.stats.reserved_runs + 1;
        let start = self.stats.reserved_bytes;
        self.stats.reserved_runs = id;
        self.io_pending = true;
        Ok(PagedSpillWriter {
            scratch: self, buffer, frontier: [None; LEVELS], id, start,
            len: 0, filled: 0, pages: 0, failed: false,
        })
    }

    /// Authenticate a single page against this run's retained root. No page
    /// bytes escape until every traversed branch and the leaf verify. This
    /// authenticates THIS page, not unread pages or a complete database result.
    /// In-order complete consumption verifies the complete payload.
    ///
    /// Allocate only the page plus a fixed branch buffer under the existing
    /// hierarchical pool. Foreign/out-of-range requests perform no I/O. Partial
    /// I/O/cancellation/drop fences the file; complete authentication failure
    /// refunds output without poisoning otherwise quiescent I/O. Each accepted
    /// page increments restored_runs, like one accepted restore_window call.
    pub async fn restore_page(&mut self, cx: &QueryCx, run: &PagedSpillRun, page: u64)
        -> Result<TrackedBytes, SpillError>
    {
        cx.with_restriction_async(self.restore_page_inner(run, page, || {
            cx.checkpoint().map_err(SpillError::Interrupted)
        })).await
    }

    async fn restore_page_inner(&mut self, run: &PagedSpillRun, page: u64,
        mut checkpoint: impl FnMut() -> Result<(), SpillError>) -> Result<TrackedBytes, SpillError>
    {
        checkpoint()?;
        if self.io_pending { return Err(SpillError::PoisonedFile); }
        if !Arc::ptr_eq(&self.owner, &run.owner) { return Err(SpillError::ForeignRun); }
        if run.id == 0 || run.id > self.stats.reserved_runs || run.end > self.stats.reserved_bytes
            || run.start > run.end || run.page_bytes == 0 || run.page_bytes > IO_CHUNK_BYTES
            || run.len() > self.limits.max_run_bytes
        { return Err(SpillError::InvalidRun); }
        let len = run.page_len(page).ok_or(SpillError::InvalidRun)?;
        let mut node = run.root.ok_or(SpillError::InvalidRun)?;
        run.validate_node(node)?;
        let mut output = self.pool.allocate_inner(len, BRANCH_BYTES)?;
        let mut branch = [0; BRANCH_BYTES];
        self.io_pending = true;
        for _ in 0..=LEVELS {
            checkpoint()?;
            let actual = self.file.seek(SeekFrom::Start(node.offset)).await?;
            if actual != node.offset {
                return Err(SpillError::UnexpectedPosition { expected: node.offset, actual });
            }
            checkpoint()?;
            if node.pages == 1 {
                if node.first != page || node.len != len as u64 {
                    self.io_pending = false;
                    return Err(SpillError::InvalidRun);
                }
                self.file.read_exact(output.as_mut()).await?;
                self.io_pending = false;
                if page_hash(run.id, run.start, run.page_bytes, node, output.as_ref()) != node.digest {
                    return Err(SpillError::ChecksumMismatch);
                }
                checkpoint()?;
                self.stats.restored_runs = self.stats.restored_runs.saturating_add(1);
                return Ok(output);
            }
            self.file.read_exact(&mut branch).await?;
            if page_hash(run.id, run.start, run.page_bytes, node, &branch) != node.digest {
                self.io_pending = false;
                return Err(SpillError::ChecksumMismatch);
            }
            let left = PageNode::decode(&branch[..NODE_BYTES]);
            let right = PageNode::decode(&branch[NODE_BYTES..]);
            let valid = (|| {
                run.validate_node(left)?;
                run.validate_node(right)?;
                if left.first != node.first
                    || left.first.checked_add(left.pages) != Some(right.first)
                    || left.pages.checked_add(right.pages) != Some(node.pages)
                    || left.len.checked_add(right.len) != Some(node.len)
                    || left.offset >= right.offset
                    || left.offset.checked_add(left.stored_len()).is_none_or(|end| end > right.offset)
                    || right.offset.checked_add(right.stored_len()).is_none_or(|end| end > node.offset)
                { return Err(SpillError::InvalidRun); }
                Ok(())
            })();
            if let Err(error) = valid { self.io_pending = false; return Err(error); }
            node = if page < right.first { left } else { right };
        }
        self.io_pending = false;
        Err(SpillError::InvalidRun)
    }
}

impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin> PagedSpillWriter<'_, F> {
    pub fn len(&self) -> usize { self.len }
    pub fn is_empty(&self) -> bool { self.len == 0 }

    /// Append an arbitrary fragment. Fragment boundaries do not change page
    /// contents or tree shape. No hidden source/EOF probe or input copy beyond
    /// the one page buffer occurs. Failure makes this writer unfinishable.
    pub async fn write(&mut self, cx: &QueryCx, bytes: &[u8]) -> Result<(), SpillError> {
        cx.with_restriction_async(self.write_inner(bytes, &mut || {
            cx.checkpoint().map_err(SpillError::Interrupted)
        })).await
    }

    async fn write_inner(&mut self, mut bytes: &[u8], checkpoint: &mut impl FnMut() -> Result<(), SpillError>)
        -> Result<(), SpillError>
    {
        if self.failed { return Err(SpillError::PoisonedFile); }
        self.failed = true;
        checkpoint()?;
        let total = self.len.checked_add(bytes.len()).ok_or(SpillError::SizeOverflow)?;
        if total > self.scratch.limits.max_run_bytes {
            return Err(SpillError::RunTooLarge { bytes: total, limit: self.scratch.limits.max_run_bytes });
        }
        while !bytes.is_empty() {
            checkpoint()?;
            let count = bytes.len().min(self.buffer.len() - self.filled);
            self.buffer.as_mut()[self.filled..self.filled + count].copy_from_slice(&bytes[..count]);
            self.filled += count;
            self.len += count;
            bytes = &bytes[count..];
            if self.filled == self.buffer.len() { self.flush_leaf(checkpoint).await?; }
        }
        checkpoint()?;
        self.failed = false;
        Ok(())
    }

    fn reserve_extent(&mut self, bytes: usize) -> Result<u64, SpillError> {
        let bytes = u64::try_from(bytes).map_err(|_| SpillError::SizeOverflow)?;
        let available = self.scratch.limits.max_file_bytes - self.scratch.stats.reserved_bytes;
        if bytes > available { return Err(SpillError::FileLimit { requested: bytes, available }); }
        let offset = self.scratch.stats.reserved_bytes;
        self.scratch.stats.reserved_bytes += bytes;
        Ok(offset)
    }

    async fn seek(&mut self, offset: u64) -> Result<(), SpillError> {
        let actual = self.scratch.file.seek(SeekFrom::Start(offset)).await?;
        if actual != offset { return Err(SpillError::UnexpectedPosition { expected: offset, actual }); }
        Ok(())
    }

    async fn combine(&mut self, left: PageNode, right: PageNode,
        checkpoint: &mut impl FnMut() -> Result<(), SpillError>) -> Result<PageNode, SpillError>
    {
        checkpoint()?;
        let mut bytes = [0; BRANCH_BYTES];
        left.encode(&mut bytes[..NODE_BYTES]);
        right.encode(&mut bytes[NODE_BYTES..]);
        let mut node = PageNode {
            offset: self.reserve_extent(BRANCH_BYTES)?, first: left.first,
            pages: left.pages.checked_add(right.pages).ok_or(SpillError::SizeOverflow)?,
            len: left.len.checked_add(right.len).ok_or(SpillError::SizeOverflow)?, digest: [0; 32],
        };
        node.digest = page_hash(self.id, self.start, self.buffer.len(), node, &bytes);
        self.seek(node.offset).await?;
        checkpoint()?;
        self.scratch.file.write_all(&bytes).await?;
        Ok(node)
    }

    async fn flush_leaf(&mut self, checkpoint: &mut impl FnMut() -> Result<(), SpillError>)
        -> Result<(), SpillError>
    {
        checkpoint()?;
        let mut node = PageNode { offset: self.reserve_extent(self.filled)?, first: self.pages,
            pages: 1, len: self.filled as u64, digest: [0; 32] };
        node.digest = page_hash(self.id, self.start, self.buffer.len(), node, &self.buffer.as_ref()[..self.filled]);
        self.seek(node.offset).await?;
        checkpoint()?;
        self.scratch.file.write_all(&self.buffer.as_ref()[..self.filled]).await?;
        self.filled = 0;
        self.pages = self.pages.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        for level in 0..LEVELS {
            if let Some(left) = self.frontier[level].take() {
                node = self.combine(left, node, checkpoint).await?;
            } else {
                self.frontier[level] = Some(node);
                return Ok(());
            }
        }
        Err(SpillError::SizeOverflow)
    }

    /// Accept the complete run only after all pages, branch records, flush and
    /// final control succeed. No durable barrier or persistent catalog is made.
    pub async fn finish(self, cx: &QueryCx) -> Result<PagedSpillRun, SpillError> {
        cx.with_restriction_async(self.finish_inner(&mut || {
            cx.checkpoint().map_err(SpillError::Interrupted)
        })).await
    }

    async fn finish_inner(mut self, checkpoint: &mut impl FnMut() -> Result<(), SpillError>)
        -> Result<PagedSpillRun, SpillError>
    {
        if self.failed { return Err(SpillError::PoisonedFile); }
        checkpoint()?;
        if self.filled != 0 { self.flush_leaf(checkpoint).await?; }
        let mut root = None;
        for level in 0..LEVELS {
            if let Some(left) = self.frontier[level].take() {
                root = Some(match root {
                    Some(right) => self.combine(left, right, checkpoint).await?,
                    None => left,
                });
            }
        }
        checkpoint()?;
        self.scratch.file.flush().await?;
        self.scratch.io_pending = false;
        checkpoint()?;
        self.scratch.stats.published_runs += 1;
        Ok(PagedSpillRun {
            owner: Arc::clone(&self.scratch.owner), id: self.id, start: self.start,
            end: self.scratch.stats.reserved_bytes, page_bytes: self.buffer.len(), root,
        })
    }
}

#[cfg(test)]
mod tests;
