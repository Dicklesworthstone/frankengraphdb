//! A chunked region allocator with generational handles.
//!
//! Everything here is safe Rust except [`Region::blocks_mut`], which is the
//! single ledgered site in this island (`arena-region-blocks-mut`). The
//! disjointness proof that site relies on is *not* written inside the unsafe
//! block: it lives in [`Region::plan_batch`], which is safe, path-independent,
//! and used by the safe fallback as well — so the check that licenses the
//! unsafe view is exercised by every test that touches either path.

use std::sync::atomic::{AtomicU64, Ordering};

/// Region identities are minted from a process-wide counter so a handle from
/// one region cannot resolve against another. Without this a foreign handle
/// would carry a slot index and an offset that are meaningless here, and the
/// exclusive-view site would be forming a pointer from another region's
/// geometry. The counter never wraps in any realistic process, and a wrap would
/// produce a collision rather than unsoundness — a colliding handle still has
/// to pass the slot, generation, and bounds checks.
static NEXT_REGION_ID: AtomicU64 = AtomicU64::new(1);

/// The largest alignment a block may request.
///
/// Bounded on purpose: alignment is achieved by padding within a chunk, so an
/// unbounded request would let one block waste a whole chunk. 64 covers a cache
/// line, which is the widest thing any planned consumer asks for.
pub const MAX_BLOCK_ALIGN: usize = 64;

/// A generational handle to one block.
///
/// Copy and comparable so consumers can key maps by it. The fields are private:
/// a handle is a capability to reach a block through its region, not a
/// description of where the block is, and exposing the offset would make every
/// consumer a participant in the bounds argument.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Handle {
    region: u64,
    slot: u32,
    generation: u32,
}

impl Handle {
    /// The region this handle was minted by.
    #[must_use]
    pub const fn region_id(self) -> u64 {
        self.region
    }

    /// The generation this handle was minted at. A block released and its slot
    /// reused mints the next generation, so the old handle is refused.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation
    }
}

/// Why an arena operation was refused.
///
/// Every variant is a *refusal*, never a silent fallback: the exclusive-view
/// site's safety argument rests on these checks, so a check that quietly
/// degraded into "do something reasonable" would be removing a load-bearing
/// wall to improve the ergonomics of a corridor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArenaError {
    /// The region has been closed or cancelled; its blocks no longer exist.
    RegionNotOpen,
    /// The handle was minted by a different region.
    ForeignHandle {
        handle_region: u64,
        this_region: u64,
    },
    /// The handle names a slot this region never had.
    UnknownSlot { slot: u32 },
    /// The slot exists but has been released, or reused by a later block.
    StaleHandle {
        slot: u32,
        handle: u32,
        current: u32,
    },
    /// Zero-length blocks are refused: they would make the disjointness proof
    /// trivially true for a block that can still be handed out twice.
    EmptyBlock,
    /// Alignment must be a power of two and at most [`MAX_BLOCK_ALIGN`].
    BadAlignment { align: usize },
    /// The block, plus the padding its alignment needs, cannot fit a chunk.
    BlockLargerThanChunk { len: usize, chunk_bytes: usize },
    /// The region's byte budget is exhausted.
    RegionFull { requested: usize, remaining: usize },
    /// An edit reaches past the end of its block.
    EditOutOfBounds {
        at: usize,
        len: usize,
        block_len: usize,
    },
    /// Two entries in one batch name the same block, or two blocks that
    /// overlap. Either would make the returned views alias.
    AliasedBatch { first: usize, second: usize },
}

impl std::fmt::Display for ArenaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RegionNotOpen => write!(f, "region is not open"),
            Self::ForeignHandle {
                handle_region,
                this_region,
            } => write!(
                f,
                "handle belongs to region {handle_region}, not region {this_region}"
            ),
            Self::UnknownSlot { slot } => write!(f, "no such slot {slot}"),
            Self::StaleHandle {
                slot,
                handle,
                current,
            } => write!(
                f,
                "slot {slot} is at generation {current}, handle carries {handle}"
            ),
            Self::EmptyBlock => write!(f, "a zero-length block is refused"),
            Self::BadAlignment { align } => write!(
                f,
                "alignment {align} is not a power of two at most {MAX_BLOCK_ALIGN}"
            ),
            Self::BlockLargerThanChunk { len, chunk_bytes } => write!(
                f,
                "a {len}-byte block cannot fit a {chunk_bytes}-byte chunk"
            ),
            Self::RegionFull {
                requested,
                remaining,
            } => write!(f, "region full: wanted {requested}, {remaining} remain"),
            Self::EditOutOfBounds { at, len, block_len } => write!(
                f,
                "edit [{at}, {at}+{len}) reaches past a {block_len}-byte block"
            ),
            Self::AliasedBatch { first, second } => {
                write!(f, "batch entries {first} and {second} alias the same bytes")
            }
        }
    }
}

impl std::error::Error for ArenaError {}

/// One write into one block: `bytes` are copied to `at` within the block.
#[derive(Clone, Copy, Debug)]
pub struct Edit<'a> {
    /// The block to write into.
    pub handle: Handle,
    /// Offset within the block.
    pub at: usize,
    /// The bytes to write.
    pub bytes: &'a [u8],
}

/// Which implementation [`Region::apply`] runs.
///
/// The same pair-shape as `fgdb-unsafe-simd`'s `DispatchPath`, and for the same
/// reason: a harness that can only reach the path the build happens to select
/// proves one path. Both are compiled on every target — there is no `cfg` on
/// either — so the matrix is the same everywhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditPath {
    /// The safe specification: one single-borrow `block_mut` call per edit,
    /// applied in order. No unsafe anywhere on this path.
    Sequential,
    /// The ledgered path: every block borrowed exclusively and simultaneously
    /// through [`Region::blocks_mut`], then written through the views.
    Exclusive,
}

/// Every compiled [`EditPath`], for harnesses that iterate the matrix.
pub const COMPILED_EDIT_PATHS: &[EditPath] = &[EditPath::Sequential, EditPath::Exclusive];

/// How a region ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionOutcome {
    /// [`Region::close`] — the ordinary end of a region's life.
    Closed,
    /// [`Region::cancel`] — the obligation was cancelled mid-flight. The
    /// reclamation claim is identical; that is the point of stating it.
    Cancelled,
}

/// The reclamation record a region produces when it ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegionAudit {
    /// How the region ended.
    pub outcome: RegionOutcome,
    /// Total bytes handed out over the region's whole life.
    pub bytes_allocated: u64,
    /// Total bytes taken back, by explicit release or at the end.
    pub bytes_reclaimed: u64,
    /// Blocks handed out.
    pub blocks_allocated: u64,
    /// Blocks taken back.
    pub blocks_released: u64,
    /// Blocks still live when the region ended, reclaimed by the end itself.
    pub blocks_live_at_end: u64,
}

impl RegionAudit {
    /// The claim the bead names: `bytes_reclaimed == bytes_allocated`, on both
    /// the close and the cancel path.
    #[must_use]
    pub const fn balanced(&self) -> bool {
        self.bytes_reclaimed == self.bytes_allocated
            && self.blocks_released == self.blocks_allocated
    }
}

/// The allocator seam.
///
/// ART, succinct, and hash storage are meant to be generic over this rather
/// than over a concrete region, so "which allocator" is a type parameter. It is
/// byte-oriented because the durable node encodings are byte images already.
///
/// No consumer is wired to it yet; that is consumer integration on
/// `fgdb-w1-unsafe-islands-eqrq`, and the ledger row says so.
pub trait RegionAlloc {
    /// Hand out a block of `len` bytes aligned to `align`.
    fn alloc_block(&mut self, len: usize, align: usize) -> Result<Handle, ArenaError>;
    /// Borrow a block immutably.
    fn block(&self, handle: Handle) -> Result<&[u8], ArenaError>;
    /// Borrow one block mutably.
    fn block_mut(&mut self, handle: Handle) -> Result<&mut [u8], ArenaError>;
    /// Give a block back, returning its length.
    fn release(&mut self, handle: Handle) -> Result<usize, ArenaError>;
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    chunk: usize,
    start: usize,
    len: usize,
    generation: u32,
    live: bool,
}

/// A chunked bump region with generational handles.
///
/// Chunks are allocated at their full size and never resized, so every chunk's
/// base address is stable for the region's life. That stability is what lets
/// alignment be computed in safe code — [`Region::alloc_block`] reads the base
/// with `as_ptr().addr()` and pads to the next aligned offset — and it is also
/// the first obligation the exclusive-view site relies on.
#[derive(Debug)]
pub struct Region {
    id: u64,
    chunks: Vec<Vec<u8>>,
    chunk_bytes: usize,
    used_in_last: usize,
    slots: Vec<Slot>,
    free_slots: Vec<u32>,
    max_bytes: usize,
    bytes_allocated: u64,
    bytes_reclaimed: u64,
    blocks_allocated: u64,
    blocks_released: u64,
    live_bytes: usize,
    open: bool,
}

/// One resolved batch entry: where the block is, and what part of it an edit
/// touches. Produced by the safe planner and consumed by both paths.
#[derive(Clone, Copy, Debug)]
struct Resolved {
    chunk: usize,
    start: usize,
    len: usize,
    at: usize,
    write_len: usize,
}

impl Region {
    /// Open a region whose chunks are `chunk_bytes` each and which will hand
    /// out at most `max_bytes` in total.
    ///
    /// # Panics
    ///
    /// If `chunk_bytes` is zero. A zero-byte chunk cannot hold any block, and
    /// every subsequent allocation would fail with a confusing error rather
    /// than at the mistake.
    #[must_use]
    pub fn with_capacity(chunk_bytes: usize, max_bytes: usize) -> Self {
        assert!(chunk_bytes > 0, "a region needs a non-zero chunk size");
        Self {
            id: NEXT_REGION_ID.fetch_add(1, Ordering::Relaxed),
            chunks: Vec::new(),
            chunk_bytes,
            used_in_last: 0,
            slots: Vec::new(),
            free_slots: Vec::new(),
            max_bytes,
            bytes_allocated: 0,
            bytes_reclaimed: 0,
            blocks_allocated: 0,
            blocks_released: 0,
            live_bytes: 0,
            open: true,
        }
    }

    /// This region's identity, carried by every handle it mints.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Whether the region is still open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Bytes handed out so far.
    #[must_use]
    pub const fn bytes_allocated(&self) -> u64 {
        self.bytes_allocated
    }

    /// Bytes taken back so far.
    #[must_use]
    pub const fn bytes_reclaimed(&self) -> u64 {
        self.bytes_reclaimed
    }

    /// Bytes currently held by live blocks.
    #[must_use]
    pub const fn live_bytes(&self) -> usize {
        self.live_bytes
    }

    /// End the region normally, reclaiming every live block.
    #[must_use]
    pub fn close(self) -> RegionAudit {
        self.finish(RegionOutcome::Closed)
    }

    /// End the region because the work was cancelled, reclaiming every live
    /// block. The reclamation guarantee is identical to [`Region::close`], and
    /// stating it separately is the point: an arena that balances only on the
    /// happy path leaks exactly when a query is cancelled mid-build.
    #[must_use]
    pub fn cancel(self) -> RegionAudit {
        self.finish(RegionOutcome::Cancelled)
    }

    fn finish(mut self, outcome: RegionOutcome) -> RegionAudit {
        let mut live = 0_u64;
        let mut reclaimed = 0_usize;
        for slot in &mut self.slots {
            if slot.live {
                slot.live = false;
                reclaimed += slot.len;
                live += 1;
            }
        }
        self.bytes_reclaimed += reclaimed as u64;
        self.blocks_released += live;
        self.live_bytes -= reclaimed;
        self.open = false;
        RegionAudit {
            outcome,
            bytes_allocated: self.bytes_allocated,
            bytes_reclaimed: self.bytes_reclaimed,
            blocks_allocated: self.blocks_allocated,
            blocks_released: self.blocks_released,
            blocks_live_at_end: live,
        }
    }

    /// Resolve a handle to its slot index, refusing every way it can be wrong.
    fn resolve(&self, handle: Handle) -> Result<usize, ArenaError> {
        if !self.open {
            return Err(ArenaError::RegionNotOpen);
        }
        if handle.region != self.id {
            return Err(ArenaError::ForeignHandle {
                handle_region: handle.region,
                this_region: self.id,
            });
        }
        let index = handle.slot as usize;
        let slot = self
            .slots
            .get(index)
            .ok_or(ArenaError::UnknownSlot { slot: handle.slot })?;
        if !slot.live || slot.generation != handle.generation {
            return Err(ArenaError::StaleHandle {
                slot: handle.slot,
                handle: handle.generation,
                current: slot.generation,
            });
        }
        Ok(index)
    }

    /// Plan a batch: resolve every handle, bound every edit, and prove the
    /// blocks pairwise disjoint.
    ///
    /// This is the whole safety argument for [`Region::blocks_mut`], and it is
    /// deliberately here in safe code rather than inside the unsafe block. Both
    /// [`EditPath`]s call it, so the two paths refuse identical batches for
    /// identical reasons — without that, the "bit-identical fallback" claim
    /// would hold only for inputs both paths accept, which is the easy half.
    ///
    /// Disjointness is checked pairwise, not argued from the bump allocator's
    /// construction. Blocks carved by a bump allocator never overlap, so the
    /// only way two entries can alias is a repeated handle — but "never
    /// overlaps by construction" is exactly the kind of claim that survives a
    /// refactor it should not have survived, and the site's soundness rests on
    /// it. The quadratic scan is affordable: a batch is a handful of nodes.
    fn plan_batch(&self, edits: &[Edit<'_>]) -> Result<Vec<Resolved>, ArenaError> {
        let mut resolved = Vec::with_capacity(edits.len());
        for edit in edits {
            let index = self.resolve(edit.handle)?;
            let slot = self.slots[index];
            let end = edit
                .at
                .checked_add(edit.bytes.len())
                .ok_or(ArenaError::EditOutOfBounds {
                    at: edit.at,
                    len: edit.bytes.len(),
                    block_len: slot.len,
                })?;
            if end > slot.len {
                return Err(ArenaError::EditOutOfBounds {
                    at: edit.at,
                    len: edit.bytes.len(),
                    block_len: slot.len,
                });
            }
            resolved.push(Resolved {
                chunk: slot.chunk,
                start: slot.start,
                len: slot.len,
                at: edit.at,
                write_len: edit.bytes.len(),
            });
        }
        for (i, a) in resolved.iter().enumerate() {
            for (j, b) in resolved.iter().enumerate().skip(i + 1) {
                if a.chunk == b.chunk && a.start < b.start + b.len && b.start < a.start + a.len {
                    return Err(ArenaError::AliasedBatch {
                        first: i,
                        second: j,
                    });
                }
            }
        }
        Ok(resolved)
    }

    /// Borrow several disjoint blocks mutably at once.
    ///
    /// LEDGER ROW `arena-region-blocks-mut`. This is the only unsafe site in
    /// the island, and the only operation here that safe Rust cannot express:
    /// the borrow checker cannot see that two byte ranges carved from one chunk
    /// do not overlap, so [`Region::plan_batch`] proves it at runtime first.
    ///
    /// The batch is refused whole if any handle is foreign, stale, unknown, or
    /// repeated, or if any two named blocks overlap. There is no partial
    /// success: a caller that got three of five views would have to reason
    /// about which two are missing, and that reasoning would be part of the
    /// safety argument.
    ///
    /// # Errors
    ///
    /// [`ArenaError::AliasedBatch`] if two entries name overlapping bytes, plus
    /// every handle-resolution error.
    #[allow(unsafe_code)]
    pub fn blocks_mut(&mut self, handles: &[Handle]) -> Result<Vec<&mut [u8]>, ArenaError> {
        let edits: Vec<Edit<'_>> = handles
            .iter()
            .map(|&handle| Edit {
                handle,
                at: 0,
                bytes: &[],
            })
            .collect();
        let plan = self.plan_batch(&edits)?;

        // Base pointers first, one per chunk, taken in safe code. Each carries
        // provenance for its own chunk buffer and nothing else.
        let bases: Vec<*mut u8> = self.chunks.iter_mut().map(Vec::as_mut_ptr).collect();

        let mut views = Vec::with_capacity(plan.len());
        for entry in &plan {
            // SAFETY: four obligations, all discharged before this line.
            //
            // 1. IN BOUNDS. `entry.chunk` indexes `bases`, which was built from
            //    `self.chunks`, and `entry.start + entry.len <= chunk_bytes`
            //    was established when the block was allocated: `alloc_block`
            //    only records a slot after bump-checking the offset against the
            //    chunk it carves from, and a chunk is created at its full
            //    length and never resized. So `add(start)` stays inside the
            //    chunk's allocation and the `len` bytes that follow are part of
            //    the same object.
            // 2. INITIALIZED. Chunks are created as `vec![0u8; chunk_bytes]`,
            //    so every byte in range is an initialized `u8`. There is no
            //    `MaybeUninit` anywhere in this crate.
            // 3. NO ALIASING. `plan_batch` proved the planned ranges pairwise
            //    disjoint, so no two views produced by this loop overlap. It
            //    also refused every stale, foreign, and unknown handle, so no
            //    entry describes geometry that is not this region's.
            // 4. NO OTHER LIVE BORROW. The views borrow `self` mutably for the
            //    caller-visible lifetime: `&mut self` is exclusive for that
            //    span, `bases` holds pointers rather than references, and the
            //    only references handed out are these. Nothing here reads or
            //    writes through a pointer inside the block.
            let view = unsafe {
                core::slice::from_raw_parts_mut(bases[entry.chunk].add(entry.start), entry.len)
            };
            views.push(view);
        }
        Ok(views)
    }

    /// Apply a batch of edits along a named path.
    ///
    /// The two paths must produce a bit-identical region image and identical
    /// refusals; that is the property `tests/edit_path_differential.rs`
    /// asserts over a seeded op script, and it is why both paths plan through
    /// the same safe [`Region::plan_batch`].
    ///
    /// # Errors
    ///
    /// Whatever [`Region::plan_batch`] refuses. On any error nothing is
    /// written, on either path.
    pub fn apply(&mut self, edits: &[Edit<'_>], path: EditPath) -> Result<(), ArenaError> {
        // Planned first on BOTH paths, so a refused batch mutates nothing on
        // either. A sequential path that wrote three edits and then hit a bad
        // fourth would not be a fallback for the exclusive path, it would be a
        // different function that usually agrees.
        let plan = self.plan_batch(edits)?;
        match path {
            EditPath::Sequential => {
                for (entry, edit) in plan.iter().zip(edits) {
                    let from = entry.start + entry.at;
                    self.chunks[entry.chunk][from..from + entry.write_len]
                        .copy_from_slice(edit.bytes);
                }
            }
            EditPath::Exclusive => {
                let handles: Vec<Handle> = edits.iter().map(|e| e.handle).collect();
                let views = self.blocks_mut(&handles)?;
                for ((view, entry), edit) in views.into_iter().zip(&plan).zip(edits) {
                    view[entry.at..entry.at + entry.write_len].copy_from_slice(edit.bytes);
                }
            }
        }
        Ok(())
    }
}

impl RegionAlloc for Region {
    fn alloc_block(&mut self, len: usize, align: usize) -> Result<Handle, ArenaError> {
        if !self.open {
            return Err(ArenaError::RegionNotOpen);
        }
        if len == 0 {
            return Err(ArenaError::EmptyBlock);
        }
        if !align.is_power_of_two() || align > MAX_BLOCK_ALIGN {
            return Err(ArenaError::BadAlignment { align });
        }
        if len > self.chunk_bytes {
            return Err(ArenaError::BlockLargerThanChunk {
                len,
                chunk_bytes: self.chunk_bytes,
            });
        }
        let remaining = self.max_bytes.saturating_sub(self.live_bytes);
        if len > remaining {
            return Err(ArenaError::RegionFull {
                requested: len,
                remaining,
            });
        }

        // Alignment in safe code. The chunk's base address is read with
        // `as_ptr().addr()`, which is a plain integer question about a pointer
        // we already hold; the chunk was created at full length and is never
        // resized, so that address is stable for the region's life and the
        // padded offset stays aligned. No unsafe is needed to hand out an
        // aligned block, and pretending otherwise would have been the easiest
        // way to make this island look busier than it is.
        let mut placed = None;
        if let Some(chunk) = self.chunks.last() {
            let base = chunk.as_ptr().addr();
            let pad = (align - ((base + self.used_in_last) % align)) % align;
            if self
                .used_in_last
                .checked_add(pad)
                .and_then(|s| s.checked_add(len))
                .is_some_and(|end| end <= self.chunk_bytes)
            {
                placed = Some((self.chunks.len() - 1, self.used_in_last + pad));
            }
        }
        let (chunk_index, start) = match placed {
            Some(place) => place,
            None => {
                self.chunks.push(vec![0_u8; self.chunk_bytes]);
                let chunk = self.chunks.last().expect("just pushed");
                let base = chunk.as_ptr().addr();
                let pad = (align - (base % align)) % align;
                if pad + len > self.chunk_bytes {
                    // A fresh chunk cannot even hold this block once aligned.
                    // Report it as too large rather than looping forever on new
                    // chunks that will fail the same way.
                    return Err(ArenaError::BlockLargerThanChunk {
                        len: pad + len,
                        chunk_bytes: self.chunk_bytes,
                    });
                }
                (self.chunks.len() - 1, pad)
            }
        };
        self.used_in_last = start + len;

        let slot_index = match self.free_slots.pop() {
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                slot.chunk = chunk_index;
                slot.start = start;
                slot.len = len;
                slot.generation = slot.generation.wrapping_add(1);
                slot.live = true;
                index
            }
            None => {
                let index = u32::try_from(self.slots.len()).expect("slot count fits u32");
                self.slots.push(Slot {
                    chunk: chunk_index,
                    start,
                    len,
                    generation: 1,
                    live: true,
                });
                index
            }
        };

        self.bytes_allocated += len as u64;
        self.blocks_allocated += 1;
        self.live_bytes += len;
        Ok(Handle {
            region: self.id,
            slot: slot_index,
            generation: self.slots[slot_index as usize].generation,
        })
    }

    fn block(&self, handle: Handle) -> Result<&[u8], ArenaError> {
        let index = self.resolve(handle)?;
        let slot = self.slots[index];
        Ok(&self.chunks[slot.chunk][slot.start..slot.start + slot.len])
    }

    fn block_mut(&mut self, handle: Handle) -> Result<&mut [u8], ArenaError> {
        let index = self.resolve(handle)?;
        let slot = self.slots[index];
        Ok(&mut self.chunks[slot.chunk][slot.start..slot.start + slot.len])
    }

    fn release(&mut self, handle: Handle) -> Result<usize, ArenaError> {
        let index = self.resolve(handle)?;
        let len = self.slots[index].len;
        self.slots[index].live = false;
        self.free_slots.push(handle.slot);
        self.bytes_reclaimed += len as u64;
        self.blocks_released += 1;
        self.live_bytes -= len;
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArenaError, COMPILED_EDIT_PATHS, Edit, EditPath, MAX_BLOCK_ALIGN, Region, RegionAlloc,
        RegionAudit, RegionOutcome,
    };

    #[test]
    fn blocks_are_aligned_as_requested() {
        let mut region = Region::with_capacity(4096, 1 << 20);
        for align in [1_usize, 2, 4, 8, 16, 32, MAX_BLOCK_ALIGN] {
            // A one-byte block between each aligned one, so the bump offset is
            // odd as often as not and the padding actually has to do work.
            let _filler = region.alloc_block(1, 1).expect("filler");
            let handle = region.alloc_block(24, align).expect("aligned block");
            let address = region.block(handle).expect("live").as_ptr().addr();
            assert_eq!(address % align, 0, "align {align} produced {address:#x}");
        }
    }

    #[test]
    fn a_released_handle_is_refused_even_after_its_slot_is_reused() {
        let mut region = Region::with_capacity(256, 4096);
        let first = region.alloc_block(16, 1).expect("first");
        assert_eq!(region.release(first), Ok(16));
        let second = region.alloc_block(16, 1).expect("second");
        assert_eq!(
            second.generation(),
            first.generation() + 1,
            "slot reuse must mint the next generation"
        );
        assert!(matches!(
            region.block(first),
            Err(ArenaError::StaleHandle { .. })
        ));
        assert!(region.block(second).is_ok());
    }

    #[test]
    fn a_handle_from_another_region_is_refused() {
        let mut a = Region::with_capacity(256, 4096);
        let mut b = Region::with_capacity(256, 4096);
        let from_a = a.alloc_block(16, 1).expect("block");
        assert_ne!(a.id(), b.id());
        assert!(matches!(
            b.block(from_a),
            Err(ArenaError::ForeignHandle { .. })
        ));
        // And the exclusive site refuses it too, which is the one that matters:
        // a foreign handle carries another region's geometry.
        assert!(matches!(
            b.blocks_mut(&[from_a]),
            Err(ArenaError::ForeignHandle { .. })
        ));
    }

    #[test]
    fn a_repeated_handle_is_refused_rather_than_aliased() {
        let mut region = Region::with_capacity(256, 4096);
        let handle = region.alloc_block(16, 1).expect("block");
        assert!(matches!(
            region.blocks_mut(&[handle, handle]),
            Err(ArenaError::AliasedBatch {
                first: 0,
                second: 1
            })
        ));
    }

    #[test]
    fn disjoint_blocks_are_borrowed_simultaneously() {
        let mut region = Region::with_capacity(256, 4096);
        let a = region.alloc_block(8, 1).expect("a");
        let b = region.alloc_block(8, 8).expect("b");
        let c = region.alloc_block(8, 1).expect("c");
        {
            let mut views = region.blocks_mut(&[a, b, c]).expect("three views");
            views[0].fill(0xAA);
            views[1].fill(0xBB);
            views[2].fill(0xCC);
            assert_eq!(views.len(), 3);
        }
        assert_eq!(region.block(a).expect("a"), &[0xAA; 8]);
        assert_eq!(region.block(b).expect("b"), &[0xBB; 8]);
        assert_eq!(region.block(c).expect("c"), &[0xCC; 8]);
    }

    #[test]
    fn an_out_of_bounds_edit_writes_nothing_on_either_path() {
        for &path in COMPILED_EDIT_PATHS {
            let mut region = Region::with_capacity(256, 4096);
            let a = region.alloc_block(4, 1).expect("a");
            let b = region.alloc_block(4, 1).expect("b");
            let edits = [
                Edit {
                    handle: a,
                    at: 0,
                    bytes: &[1, 2, 3, 4],
                },
                Edit {
                    handle: b,
                    at: 2,
                    bytes: &[9, 9, 9],
                },
            ];
            assert!(
                matches!(
                    region.apply(&edits, path),
                    Err(ArenaError::EditOutOfBounds { .. })
                ),
                "{path:?} must refuse the batch"
            );
            assert_eq!(
                region.block(a).expect("a"),
                &[0, 0, 0, 0],
                "{path:?} wrote through a refused batch"
            );
        }
    }

    #[test]
    fn close_and_cancel_both_balance_with_live_blocks_outstanding() {
        for (outcome, end) in [
            (
                RegionOutcome::Closed,
                Region::close as fn(Region) -> RegionAudit,
            ),
            (
                RegionOutcome::Cancelled,
                Region::cancel as fn(Region) -> RegionAudit,
            ),
        ] {
            let mut region = Region::with_capacity(128, 4096);
            let keep = region.alloc_block(32, 8).expect("keep");
            let drop_me = region.alloc_block(48, 1).expect("drop");
            assert_eq!(region.release(drop_me), Ok(48));
            assert_eq!(region.live_bytes(), 32);
            let _ = keep;
            let audit = end(region);
            assert_eq!(audit.outcome, outcome);
            assert_eq!(audit.bytes_allocated, 80);
            assert!(
                audit.balanced(),
                "{outcome:?} left {} of {} bytes unreclaimed",
                audit.bytes_allocated - audit.bytes_reclaimed,
                audit.bytes_allocated
            );
            assert_eq!(audit.blocks_live_at_end, 1);
        }
    }

    #[test]
    fn a_closed_region_refuses_everything() {
        let mut region = Region::with_capacity(128, 4096);
        let handle = region.alloc_block(8, 1).expect("block");
        let audit = region.close();
        assert!(audit.balanced());
        // The handle outlives the region value only in the test's own hands;
        // what matters is that a region cannot be reopened, which the type
        // system already guarantees by consuming `self`.
        let reopened = Region::with_capacity(128, 4096);
        assert!(matches!(
            reopened.block(handle),
            Err(ArenaError::ForeignHandle { .. })
        ));
    }

    #[test]
    fn the_budget_is_enforced_and_release_returns_it() {
        let mut region = Region::with_capacity(64, 96);
        let a = region.alloc_block(64, 1).expect("a");
        assert!(matches!(
            region.alloc_block(64, 1),
            Err(ArenaError::RegionFull {
                requested: 64,
                remaining: 32
            })
        ));
        assert_eq!(region.release(a), Ok(64));
        assert!(region.alloc_block(64, 1).is_ok(), "released bytes return");
    }

    #[test]
    fn refusals_are_identical_on_both_paths() {
        // The half of "bit-identical fallback" that is easy to skip: agreement
        // on inputs both paths accept proves nothing about the inputs one of
        // them would have waved through.
        let mut region = Region::with_capacity(256, 4096);
        let a = region.alloc_block(8, 1).expect("a");
        let stale = region.alloc_block(8, 1).expect("stale");
        region.release(stale).expect("release");
        let cases: [Vec<Edit<'_>>; 3] = [
            vec![Edit {
                handle: stale,
                at: 0,
                bytes: &[1],
            }],
            vec![
                Edit {
                    handle: a,
                    at: 0,
                    bytes: &[1],
                },
                Edit {
                    handle: a,
                    at: 4,
                    bytes: &[2],
                },
            ],
            vec![Edit {
                handle: a,
                at: 7,
                bytes: &[1, 2],
            }],
        ];
        for case in &cases {
            let sequential = region.apply(case, EditPath::Sequential);
            let exclusive = region.apply(case, EditPath::Exclusive);
            assert_eq!(
                sequential, exclusive,
                "paths disagreed on a refusal: {case:?}"
            );
            assert!(sequential.is_err(), "case was meant to be refused");
        }
    }
}
