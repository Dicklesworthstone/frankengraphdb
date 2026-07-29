//! A chunked region allocator with generational handles.
//!
//! Everything here is safe Rust except two ledgered boundary sites:
//! [`Region::blocks_mut`] (`arena-region-blocks-mut`) and the sealed
//! [`core::alloc::Allocator`] implementation behind [`RegionVec`]
//! (`arena-region-vec-allocator`). Neither site exports its proof obligations:
//! ordinary crates consume only safe byte handles or the safe typed container.

use core::alloc::{AllocError, Layout};
use core::ptr::NonNull;
use std::cell::{Cell, RefCell};
use std::mem;
use std::sync::atomic::{AtomicU64, Ordering};

use fgdb_types::QueryCx;

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
    /// The region's live-logical-byte budget is exhausted: the sum of live
    /// block lengths would exceed `max_live_bytes`.
    RegionFull { requested: usize, remaining: usize },
    /// The region's resident-byte limit is exhausted: the next chunk would
    /// push total chunk capacity past `max_resident_bytes`. Charged per chunk,
    /// before any allocation or metadata mutation, so a refusal changes
    /// nothing.
    ResidentLimitExceeded { requested: usize, remaining: usize },
    /// The process allocator refused backing storage or region metadata.
    BackingAllocationFailed { requested: usize },
    /// The slot table can no longer mint a representable handle.
    SlotSpaceExhausted,
    /// An allocator callback named no live block at the supplied address.
    UnknownAllocation { address: usize },
    /// A private allocator callback re-entered the task-local region.
    AllocatorReentered,
    /// An allocator callback supplied a layout different from the allocation's
    /// exact layout. The callback contract requires an exact match.
    AllocationLayoutMismatch {
        expected_size: usize,
        actual_size: usize,
        expected_align: usize,
        actual_align: usize,
    },
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
            Self::ResidentLimitExceeded {
                requested,
                remaining,
            } => write!(
                f,
                "resident limit exceeded: wanted {requested}, {remaining} remain"
            ),
            Self::BackingAllocationFailed { requested } => {
                write!(f, "backing allocation of {requested} bytes failed")
            }
            Self::SlotSpaceExhausted => write!(f, "region slot space is exhausted"),
            Self::UnknownAllocation { address } => {
                write!(f, "no live allocation starts at address {address:#x}")
            }
            Self::AllocatorReentered => write!(f, "private region allocator was re-entered"),
            Self::AllocationLayoutMismatch {
                expected_size,
                actual_size,
                expected_align,
                actual_align,
            } => write!(
                f,
                "allocation layout mismatch: expected size/alignment \
                 {expected_size}/{expected_align}, got {actual_size}/{actual_align}"
            ),
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
    /// Chunks created over the region's whole life. Chunks are never freed
    /// early — the allocator site's provenance argument rests on their
    /// stability — so this is also the chunk count the region's `Drop`
    /// reclaims.
    pub chunks_allocated: u64,
    /// Largest retained resident footprint the region ever held, in bytes
    /// (the sum of `Vec::capacity()` over live chunks at its maximum).
    /// Resident bytes are monotone over a region's life today, so this equals
    /// the footprint at the end; the field is named for the peak so an
    /// early-reclamation change cannot silently stale its meaning. This is
    /// the number W3 admission control binds to the plan's `resident_bytes`
    /// axis.
    pub peak_resident_bytes: u64,
    /// Total alignment padding consumed inside chunks over the region's
    /// whole life. Padding is charged implicitly — it lives inside charged
    /// chunk capacity — and reported here so internal fragmentation is an
    /// audited quantity rather than an invisible tax.
    pub alignment_padding_bytes: u64,
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
/// This remains the byte-oriented interface for raw arena consumers and
/// differential verification. Typed collection storage does not implement or
/// receive this trait: ART, succinct, and hash buffers use the safe
/// [`RegionVec`] surface so allocator vocabulary cannot cross the unsafe-island
/// boundary.
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
    align: usize,
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
    max_live_bytes: usize,
    max_resident_bytes: usize,
    resident_bytes: usize,
    peak_resident_bytes: u64,
    chunks_allocated: u64,
    alignment_padding_bytes: u64,
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
    /// Open a region whose chunks are `chunk_bytes` each, under two explicit
    /// budgets.
    ///
    /// * `max_live_bytes` is the **live-logical-byte budget**: the sum of
    ///   `len` over live blocks never exceeds it. Releasing a block returns
    ///   its bytes to this budget, so a working set that shrinks can grow
    ///   again. This is the operator-facing working-set contract.
    /// * `max_resident_bytes` is the **resident-byte limit**: the sum of
    ///   `Vec::capacity()` over retained chunks never exceeds it. It is
    ///   charged per chunk at creation — at the capacity the allocator
    ///   actually accepted, which `try_reserve_exact` contractually bounds
    ///   only from below — which inherently charges alignment padding and
    ///   in-chunk slack: they live inside charged chunks. It is *not*
    ///   returned on release: chunks are never freed early, because the
    ///   allocator site's provenance argument rests on chunk stability, so
    ///   resident consumption is monotone over the region's life. This is
    ///   the number admission control binds to the plan's `resident_bytes`
    ///   axis; region metadata and allocator-internal slack beyond
    ///   `Vec::capacity` are outside it by design.
    ///
    /// # Panics
    ///
    /// If the budget triple is not totally ordered as
    /// `0 < chunk_bytes <= max_live_bytes <= max_resident_bytes`. A chunk
    /// larger than the live budget would be unreachable waste, a live budget
    /// above the resident limit could never be reached, and a zero-byte chunk
    /// cannot hold any block; all three are configuration mistakes best named
    /// at construction rather than as a confusing first-allocation refusal.
    #[must_use]
    pub fn with_capacity(
        chunk_bytes: usize,
        max_live_bytes: usize,
        max_resident_bytes: usize,
    ) -> Self {
        assert!(chunk_bytes > 0, "a region needs a non-zero chunk size");
        assert!(
            chunk_bytes <= max_live_bytes,
            "chunk size {chunk_bytes} exceeds the live-byte budget {max_live_bytes}"
        );
        assert!(
            max_live_bytes <= max_resident_bytes,
            "live-byte budget {max_live_bytes} exceeds the resident limit {max_resident_bytes}"
        );
        Self {
            id: NEXT_REGION_ID.fetch_add(1, Ordering::Relaxed),
            chunks: Vec::new(),
            chunk_bytes,
            used_in_last: 0,
            slots: Vec::new(),
            free_slots: Vec::new(),
            max_live_bytes,
            max_resident_bytes,
            resident_bytes: 0,
            peak_resident_bytes: 0,
            chunks_allocated: 0,
            alignment_padding_bytes: 0,
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

    /// The live-logical-byte budget: the most [`Region::live_bytes`] may
    /// reach. Returned in full by release.
    #[must_use]
    pub const fn max_live_bytes(&self) -> usize {
        self.max_live_bytes
    }

    /// Bytes currently held as retained chunk capacity: the sum of
    /// `Vec::capacity()` over this region's chunks, measured at creation.
    /// Charged per chunk and never returned before the region ends, so this
    /// is monotone over the region's life. Region metadata (the chunk table,
    /// slot table, and free list) and allocator-internal slack beyond what
    /// `Vec::capacity` reports are outside this number by design: it is the
    /// admission-control charge for block storage, exactly what the W3
    /// adapter binds to the plan's `resident_bytes` axis.
    #[must_use]
    pub const fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// The resident-byte limit: the most [`Region::resident_bytes`] may
    /// reach. Never returned by release; reclaimed wholesale when the region
    /// ends.
    #[must_use]
    pub const fn max_resident_bytes(&self) -> usize {
        self.max_resident_bytes
    }

    /// Alignment padding consumed inside chunks so far.
    #[must_use]
    pub const fn alignment_padding_bytes(&self) -> u64 {
        self.alignment_padding_bytes
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
            chunks_allocated: self.chunks_allocated,
            peak_resident_bytes: self.peak_resident_bytes,
            alignment_padding_bytes: self.alignment_padding_bytes,
        }
    }

    /// Resolve the exact live block named by an allocator callback.
    ///
    /// This is private because addresses and layouts are boundary vocabulary;
    /// safe consumers use handles or [`RegionVec`]. No pointer is dereferenced
    /// here. The address is compared with the stable base-plus-offset geometry
    /// that allocation recorded in safe code.
    fn handle_for_allocation(&self, address: usize, layout: Layout) -> Result<Handle, ArenaError> {
        let found = self.slots.iter().enumerate().find(|(_, slot)| {
            slot.live
                && self.chunks[slot.chunk]
                    .as_ptr()
                    .addr()
                    .checked_add(slot.start)
                    == Some(address)
        });
        let Some((index, slot)) = found else {
            return Err(ArenaError::UnknownAllocation { address });
        };
        if slot.len != layout.size() || slot.align != layout.align() {
            return Err(ArenaError::AllocationLayoutMismatch {
                expected_size: slot.len,
                actual_size: layout.size(),
                expected_align: slot.align,
                actual_align: layout.align(),
            });
        }
        let slot = u32::try_from(index).map_err(|_| ArenaError::SlotSpaceExhausted)?;
        Ok(Handle {
            region: self.id,
            slot,
            generation: self.slots[index].generation,
        })
    }

    fn release_allocation(&mut self, address: usize, layout: Layout) -> Result<usize, ArenaError> {
        let handle = self.handle_for_allocation(address, layout)?;
        self.release(handle)
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
    /// LEDGER ROW `arena-region-blocks-mut`. This is one of two unsafe sites in
    /// the island, and the byte-view operation safe Rust cannot express:
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
        let remaining = self.max_live_bytes.saturating_sub(self.live_bytes);
        if len > remaining {
            return Err(ArenaError::RegionFull {
                requested: len,
                remaining,
            });
        }

        // Plan every fallible growth before mutating region-visible state.
        // In particular, the backing chunk is built as a local value and the
        // slot/chunk metadata reserves are completed before either vector is
        // changed. A refused allocation therefore cannot leave an empty chunk
        // or a half-created slot behind.
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
                placed = Some((self.chunks.len() - 1, self.used_in_last + pad, pad));
            }
        }
        let new_chunk = match placed {
            Some(_) => None,
            None => {
                // The resident charge is checked before the chunk exists:
                // refusal here is deterministic and leaves the region
                // byte-identical. Chunks are never freed early, so this limit
                // — not the live budget — is what bounds the region's actual
                // footprint under allocate/release churn.
                let resident_remaining =
                    self.max_resident_bytes.saturating_sub(self.resident_bytes);
                if self.chunk_bytes > resident_remaining {
                    return Err(ArenaError::ResidentLimitExceeded {
                        requested: self.chunk_bytes,
                        remaining: resident_remaining,
                    });
                }
                let mut chunk = Vec::new();
                chunk.try_reserve_exact(self.chunk_bytes).map_err(|_| {
                    ArenaError::BackingAllocationFailed {
                        requested: self.chunk_bytes,
                    }
                })?;
                chunk.resize(self.chunk_bytes, 0_u8);
                // `try_reserve_exact` contractually guarantees only
                // `capacity() >= chunk_bytes` — the allocator may accept a
                // larger layout — so the retained charge is the capacity the
                // vector actually reports, measured before the chunk becomes
                // region-visible. Under the pinned toolchain `try_reserve_exact`
                // records exactly the requested capacity, so this equals
                // `chunk_bytes` today; the measurement is what keeps the hard
                // limit honest if that implementation detail ever moves. A
                // refusal drops the chunk while it is still a local: nothing
                // region-visible has mutated, and the transient local is
                // outside the retained bound by construction.
                let accepted = chunk.capacity();
                if accepted > resident_remaining {
                    return Err(ArenaError::ResidentLimitExceeded {
                        requested: accepted,
                        remaining: resident_remaining,
                    });
                }
                let base = chunk.as_ptr().addr();
                let pad = (align - (base % align)) % align;
                if pad + len > self.chunk_bytes {
                    return Err(ArenaError::BlockLargerThanChunk {
                        len: pad + len,
                        chunk_bytes: self.chunk_bytes,
                    });
                }
                self.chunks
                    .try_reserve(1)
                    .map_err(|_| ArenaError::BackingAllocationFailed {
                        requested: mem::size_of::<Vec<u8>>(),
                    })?;
                Some((chunk, pad, accepted))
            }
        };

        // A generation at u32::MAX is retired rather than wrapped: wrapping
        // would eventually make a stale handle current again. Retired slots
        // are small metadata tombstones and never name live storage.
        let reusable = loop {
            match self.free_slots.pop() {
                Some(index) if self.slots[index as usize].generation == u32::MAX => continue,
                other => break other,
            }
        };
        if reusable.is_none() {
            if self.slots.len() >= u32::MAX as usize {
                return Err(ArenaError::SlotSpaceExhausted);
            }
            self.slots
                .try_reserve(1)
                .map_err(|_| ArenaError::BackingAllocationFailed {
                    requested: mem::size_of::<Slot>(),
                })?;
            let release_capacity = self
                .slots
                .len()
                .checked_add(1)
                .and_then(|needed| needed.checked_sub(self.free_slots.len()))
                .ok_or(ArenaError::SlotSpaceExhausted)?;
            self.free_slots.try_reserve(release_capacity).map_err(|_| {
                ArenaError::BackingAllocationFailed {
                    requested: release_capacity.saturating_mul(mem::size_of::<u32>()),
                }
            })?;
        }

        let (chunk_index, start, pad) = match (placed, new_chunk) {
            (Some(place), None) => place,
            (None, Some((chunk, pad, accepted))) => {
                let index = self.chunks.len();
                self.chunks.push(chunk);
                self.resident_bytes += accepted;
                self.chunks_allocated += 1;
                self.peak_resident_bytes = self.peak_resident_bytes.max(self.resident_bytes as u64);
                (index, pad, pad)
            }
            _ => unreachable!("placement planning must choose exactly one chunk"),
        };
        self.used_in_last = start + len;
        self.alignment_padding_bytes += pad as u64;

        let slot_index = match reusable {
            Some(index) => {
                let slot = &mut self.slots[index as usize];
                slot.chunk = chunk_index;
                slot.start = start;
                slot.len = len;
                slot.align = align;
                slot.generation += 1;
                slot.live = true;
                index
            }
            None => {
                let index =
                    u32::try_from(self.slots.len()).map_err(|_| ArenaError::SlotSpaceExhausted)?;
                self.slots.push(Slot {
                    chunk: chunk_index,
                    start,
                    len,
                    align,
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
        debug_assert!(
            self.free_slots.len() < self.free_slots.capacity(),
            "slot creation reserves one release entry per slot"
        );
        self.slots[index].live = false;
        self.free_slots.push(handle.slot);
        self.bytes_reclaimed += len as u64;
        self.blocks_released += 1;
        self.live_bytes -= len;
        Ok(len)
    }
}

/// Why a safe typed-region operation was refused.
///
/// Allocation refusals never mutate the vector's element sequence. Capacity
/// may grow before a later, impossible-under-the-contract allocator callback
/// fault is detected, but initialized values remain owned and accessible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionVecError {
    /// The query context refused work at its cancellation/budget checkpoint.
    CheckpointRefused,
    /// `T` asks for an alignment the region kernel does not support.
    UnsupportedAlignment { align: usize, maximum: usize },
    /// `len + additional`, or the corresponding byte layout, overflowed.
    CapacityOverflow,
    /// An insertion index exceeded the current length.
    IndexOutOfBounds { index: usize, len: usize },
    /// The task-local owner counter cannot represent another typed container.
    OwnerCounterExhausted,
    /// The byte-region kernel refused the allocation.
    Arena(ArenaError),
    /// A private allocator callback violated its exact address/layout contract.
    AllocatorFault(ArenaError),
}

impl std::fmt::Display for RegionVecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CheckpointRefused => write!(f, "query checkpoint refused the allocation"),
            Self::UnsupportedAlignment { align, maximum } => {
                write!(f, "alignment {align} exceeds region maximum {maximum}")
            }
            Self::CapacityOverflow => write!(f, "region vector capacity overflow"),
            Self::IndexOutOfBounds { index, len } => {
                write!(f, "insert index {index} exceeds vector length {len}")
            }
            Self::OwnerCounterExhausted => write!(f, "typed region owner counter exhausted"),
            Self::Arena(source) => write!(f, "region allocation refused: {source}"),
            Self::AllocatorFault(source) => {
                write!(f, "private allocator callback fault: {source}")
            }
        }
    }
}

impl std::error::Error for RegionVecError {}

#[derive(Debug)]
struct RegionScopeState {
    region: RefCell<Option<Region>>,
    owners: Cell<usize>,
    last_allocation_error: Cell<Option<ArenaError>>,
    allocator_fault: Cell<Option<ArenaError>>,
}

impl RegionScopeState {
    fn claim_owner(&self) -> Result<(), RegionVecError> {
        let owners = self
            .owners
            .get()
            .checked_add(1)
            .ok_or(RegionVecError::OwnerCounterExhausted)?;
        self.owners.set(owners);
        Ok(())
    }

    fn release_owner(&self) {
        let owners = self.owners.get();
        debug_assert!(owners > 0, "RegionVec owner counter underflow");
        self.owners.set(owners.saturating_sub(1));
    }

    fn record_allocator_fault(&self, fault: ArenaError) {
        if self.allocator_fault.get().is_none() {
            self.allocator_fault.set(Some(fault));
        }
    }
}

/// A task-local owner for typed region storage.
///
/// Vectors borrow this value, so ordinary Rust code cannot close or cancel the
/// region while a [`RegionVec`] is still accessible. A dynamic owner count
/// closes the `mem::forget` escape: finalization returns
/// [`RegionFinishError`] and retains the storage rather than reporting a
/// balanced audit over live typed owners.
#[derive(Debug)]
pub struct RegionScope {
    state: RegionScopeState,
}

impl RegionScope {
    /// Create a typed region over the W1 byte-allocation kernel.
    ///
    /// # Panics
    ///
    /// Panics if the budget triple is not totally ordered as
    /// `0 < chunk_bytes <= max_live_bytes <= max_resident_bytes`, matching
    /// [`Region::with_capacity`].
    #[must_use]
    pub fn with_capacity(
        chunk_bytes: usize,
        max_live_bytes: usize,
        max_resident_bytes: usize,
    ) -> Self {
        Self {
            state: RegionScopeState {
                region: RefCell::new(Some(Region::with_capacity(
                    chunk_bytes,
                    max_live_bytes,
                    max_resident_bytes,
                ))),
                owners: Cell::new(0),
                last_allocation_error: Cell::new(None),
                allocator_fault: Cell::new(None),
            },
        }
    }

    /// Number of live typed containers.
    #[must_use]
    pub fn owners(&self) -> usize {
        self.state.owners.get()
    }

    /// Bytes handed out by the underlying region over its lifetime.
    #[must_use]
    pub fn bytes_allocated(&self) -> u64 {
        self.state
            .region
            .borrow()
            .as_ref()
            .map_or(0, Region::bytes_allocated)
    }

    /// Bytes already returned by typed-container reallocation or drop.
    #[must_use]
    pub fn bytes_reclaimed(&self) -> u64 {
        self.state
            .region
            .borrow()
            .as_ref()
            .map_or(0, Region::bytes_reclaimed)
    }

    /// Bytes held by currently live backing allocations.
    #[must_use]
    pub fn live_bytes(&self) -> usize {
        self.state
            .region
            .borrow()
            .as_ref()
            .map_or(0, Region::live_bytes)
    }

    /// Bytes currently held as chunk capacity. Monotone over the region's
    /// life; the number W3 admission control binds to the plan's
    /// `resident_bytes` axis.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.state
            .region
            .borrow()
            .as_ref()
            .map_or(0, Region::resident_bytes)
    }

    /// End the region normally after every typed container has dropped.
    pub fn close(self) -> Result<RegionAudit, RegionFinishError> {
        self.finish(RegionOutcome::Closed)
    }

    /// End the region on cancellation after every typed container has dropped.
    pub fn cancel(self) -> Result<RegionAudit, RegionFinishError> {
        self.finish(RegionOutcome::Cancelled)
    }

    fn finish(mut self, outcome: RegionOutcome) -> Result<RegionAudit, RegionFinishError> {
        let owners = self.state.owners.get();
        let allocator_fault = self.state.allocator_fault.get();
        if owners != 0 || allocator_fault.is_some() {
            return Err(RegionFinishError {
                scope: Box::new(self),
                owners,
                allocator_fault,
            });
        }
        let region = self
            .state
            .region
            .get_mut()
            .take()
            .expect("a RegionScope is finalized at most once");
        Ok(match outcome {
            RegionOutcome::Closed => region.close(),
            RegionOutcome::Cancelled => region.cancel(),
        })
    }
}

impl Drop for RegionScope {
    fn drop(&mut self) {
        let Some(region) = self.state.region.get_mut().take() else {
            return;
        };
        if self.state.owners.get() != 0 || self.state.allocator_fault.get().is_some() {
            // A forgotten owner or allocator-contract fault means typed storage
            // might still be logically live. Freeing it would turn a detected
            // lifecycle violation into a dangling allocation, so retain it.
            mem::forget(region);
        } else {
            let _ = region.cancel();
        }
    }
}

/// A fail-closed typed-region finalization refusal.
#[derive(Debug)]
pub struct RegionFinishError {
    scope: Box<RegionScope>,
    owners: usize,
    allocator_fault: Option<ArenaError>,
}

impl RegionFinishError {
    /// Typed owners that prevented finalization.
    #[must_use]
    pub fn owners_remaining(&self) -> usize {
        self.owners
    }

    /// Allocator callback fault that prevented a trustworthy balanced audit.
    #[must_use]
    pub fn allocator_fault(&self) -> Option<ArenaError> {
        self.allocator_fault
    }

    /// Recover the retained scope for inspection or a later retry.
    #[must_use]
    pub fn into_scope(self) -> RegionScope {
        *self.scope
    }
}

impl std::fmt::Display for RegionFinishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.owners, self.allocator_fault) {
            (owners, Some(fault)) => write!(
                f,
                "region finalization refused with {owners} typed owner(s) and allocator fault: \
                 {fault}"
            ),
            (owners, None) => {
                write!(
                    f,
                    "region finalization refused with {owners} typed owner(s)"
                )
            }
        }
    }
}

impl std::error::Error for RegionFinishError {}

#[repr(align(64))]
struct MaximumAlignedZero;

static MAXIMUM_ALIGNED_ZERO: MaximumAlignedZero = MaximumAlignedZero;

/// The allocator boundary is deliberately unnameable outside this module.
///
/// Moving or copying this adapter preserves every allocation because it holds
/// a borrow of the immovable-for-that-borrow [`RegionScopeState`]. The region's
/// chunk buffers never resize, released ranges are never reused, and newly
/// allocated ranges are pairwise disjoint.
#[derive(Clone, Copy)]
struct PrivateRegionAllocator<'region> {
    state: &'region RegionScopeState,
}

#[allow(unsafe_code)]
// SAFETY: the allocator borrow keeps `RegionScopeState` alive, chunks never
// resize, released ranges are never reused, every nonzero allocation is
// aligned and disjoint, pointers come from `Vec::as_mut_ptr` without forming a
// backing-slice reference, deallocation validates exact pointer/layout
// identity, and a live or unwinding owner makes finalization retain the region.
unsafe impl core::alloc::Allocator for PrivateRegionAllocator<'_> {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.align() > MAX_BLOCK_ALIGN {
            let error = ArenaError::BadAlignment {
                align: layout.align(),
            };
            self.state.last_allocation_error.set(Some(error));
            return Err(AllocError);
        }
        if layout.size() == 0 {
            let pointer = NonNull::from(&MAXIMUM_ALIGNED_ZERO).cast::<u8>();
            return Ok(NonNull::slice_from_raw_parts(pointer, 0));
        }
        let mut region_slot = match self.state.region.try_borrow_mut() {
            Ok(region) => region,
            Err(_) => {
                self.state
                    .last_allocation_error
                    .set(Some(ArenaError::AllocatorReentered));
                return Err(AllocError);
            }
        };
        let Some(region) = region_slot.as_mut() else {
            self.state
                .last_allocation_error
                .set(Some(ArenaError::RegionNotOpen));
            return Err(AllocError);
        };
        let handle = match region.alloc_block(layout.size(), layout.align()) {
            Ok(handle) => handle,
            Err(error) => {
                self.state.last_allocation_error.set(Some(error));
                return Err(AllocError);
            }
        };
        let index = match region.resolve(handle) {
            Ok(index) => index,
            Err(error) => {
                self.state.record_allocator_fault(error);
                self.state.last_allocation_error.set(Some(error));
                return Err(AllocError);
            }
        };
        let slot = region.slots[index];
        // `Vec::as_mut_ptr` is specifically guaranteed not to materialize a
        // reference to the backing slice. That matters here: forming
        // `&mut chunk[start..end]` would uniquely retag the whole byte chunk
        // and invalidate pointers previously handed to other RegionVecs.
        let pointer = region.chunks[slot.chunk]
            .as_mut_ptr()
            .wrapping_add(slot.start);
        let Some(pointer) = NonNull::new(pointer) else {
            let error = ArenaError::BackingAllocationFailed {
                requested: layout.size(),
            };
            self.state.record_allocator_fault(error);
            self.state.last_allocation_error.set(Some(error));
            return Err(AllocError);
        };
        Ok(NonNull::slice_from_raw_parts(pointer, layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        let mut region_slot = match self.state.region.try_borrow_mut() {
            Ok(region) => region,
            Err(_) => {
                self.state
                    .record_allocator_fault(ArenaError::AllocatorReentered);
                return;
            }
        };
        let Some(region) = region_slot.as_mut() else {
            self.state.record_allocator_fault(ArenaError::RegionNotOpen);
            return;
        };
        if let Err(error) = region.release_allocation(ptr.as_ptr().addr(), layout) {
            self.state.record_allocator_fault(error);
        }
    }
}

/// A typed vector whose actual element buffer is allocated from a
/// [`RegionScope`].
///
/// The private allocator and `Vec<T, A>` type never appear in this public
/// surface. Methods that can allocate require [`QueryCx`], checkpoint before
/// touching the element sequence, and reserve before mutation. This type does
/// not implement `Deref`, `Clone`, `Extend`, or `FromIterator`, because those
/// routes would expose allocating operations without the purpose context.
pub struct RegionVec<'region, T> {
    inner: Option<Vec<T, PrivateRegionAllocator<'region>>>,
    state: &'region RegionScopeState,
}

impl<'region, T> RegionVec<'region, T> {
    /// Open an empty typed container in `scope`.
    pub fn new_in(scope: &'region RegionScope) -> Result<Self, RegionVecError> {
        let align = mem::align_of::<T>();
        if align > MAX_BLOCK_ALIGN {
            return Err(RegionVecError::UnsupportedAlignment {
                align,
                maximum: MAX_BLOCK_ALIGN,
            });
        }
        scope.state.claim_owner()?;
        Ok(Self {
            inner: Some(Vec::new_in(PrivateRegionAllocator {
                state: &scope.state,
            })),
            state: &scope.state,
        })
    }

    /// Open a container and reserve at least `capacity` elements.
    pub fn with_capacity_in(
        scope: &'region RegionScope,
        cx: &QueryCx,
        capacity: usize,
    ) -> Result<Self, RegionVecError> {
        let mut vector = Self::new_in(scope)?;
        if let Err(error) = vector.try_reserve_exact(cx, capacity) {
            drop(vector);
            return Err(error);
        }
        Ok(vector)
    }

    fn inner(&self) -> &Vec<T, PrivateRegionAllocator<'region>> {
        let Some(inner) = self.inner.as_ref() else {
            unreachable!("RegionVec inner storage exists until Drop")
        };
        inner
    }

    fn inner_mut(&mut self) -> &mut Vec<T, PrivateRegionAllocator<'region>> {
        let Some(inner) = self.inner.as_mut() else {
            unreachable!("RegionVec inner storage exists until Drop")
        };
        inner
    }

    fn checkpoint(cx: &QueryCx) -> Result<(), RegionVecError> {
        cx.checkpoint()
            .map_err(|_| RegionVecError::CheckpointRefused)
    }

    fn reserve_with(
        &mut self,
        cx: &QueryCx,
        additional: usize,
        exact: bool,
    ) -> Result<(), RegionVecError> {
        Self::checkpoint(cx)?;
        self.state.last_allocation_error.set(None);
        let result = if exact {
            self.inner_mut().try_reserve_exact(additional)
        } else {
            self.inner_mut().try_reserve(additional)
        };
        match result {
            Ok(()) => match self.state.allocator_fault.get() {
                Some(error) => Err(RegionVecError::AllocatorFault(error)),
                None => Ok(()),
            },
            Err(_) => match self.state.last_allocation_error.take() {
                Some(error) => Err(RegionVecError::Arena(error)),
                None => Err(RegionVecError::CapacityOverflow),
            },
        }
    }

    /// Number of initialized elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner().len()
    }

    /// Whether there are no initialized elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner().is_empty()
    }

    /// Number of elements the current region allocation can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner().capacity()
    }

    /// Borrow the initialized element sequence.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        self.inner().as_slice()
    }

    /// Borrow the initialized element sequence exclusively.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.inner_mut().as_mut_slice()
    }

    #[must_use]
    pub fn get(&self, index: usize) -> Option<&T> {
        self.inner().get(index)
    }

    #[must_use]
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.inner_mut().get_mut(index)
    }

    pub fn try_reserve(&mut self, cx: &QueryCx, additional: usize) -> Result<(), RegionVecError> {
        self.reserve_with(cx, additional, false)
    }

    pub fn try_reserve_exact(
        &mut self,
        cx: &QueryCx,
        additional: usize,
    ) -> Result<(), RegionVecError> {
        self.reserve_with(cx, additional, true)
    }

    pub fn try_push(&mut self, cx: &QueryCx, value: T) -> Result<(), RegionVecError> {
        self.reserve_with(cx, 1, false)?;
        self.inner_mut().push(value);
        Ok(())
    }

    pub fn try_insert(
        &mut self,
        cx: &QueryCx,
        index: usize,
        value: T,
    ) -> Result<(), RegionVecError> {
        let len = self.len();
        if index > len {
            return Err(RegionVecError::IndexOutOfBounds { index, len });
        }
        self.reserve_with(cx, 1, false)?;
        self.inner_mut().insert(index, value);
        Ok(())
    }

    /// Extend atomically with respect to structured allocation refusal.
    ///
    /// Items are first staged in the same region. If staging or final reserve
    /// fails, `self` is unchanged; on success, `Vec::append` only moves already
    /// initialized `T` values into capacity reserved under `cx`.
    pub fn try_extend<I>(&mut self, cx: &QueryCx, values: I) -> Result<(), RegionVecError>
    where
        I: IntoIterator<Item = T>,
    {
        Self::checkpoint(cx)?;
        self.state.claim_owner()?;
        let mut staged = Self {
            inner: Some(Vec::new_in(PrivateRegionAllocator { state: self.state })),
            state: self.state,
        };
        for value in values {
            staged.try_push(cx, value)?;
        }
        self.reserve_with(cx, staged.len(), false)?;
        self.inner_mut().append(staged.inner_mut());
        Ok(())
    }

    pub fn try_extend_from_slice(
        &mut self,
        cx: &QueryCx,
        values: &[T],
    ) -> Result<(), RegionVecError>
    where
        T: Clone,
    {
        self.try_extend(cx, values.iter().cloned())
    }

    pub fn try_resize(
        &mut self,
        cx: &QueryCx,
        new_len: usize,
        value: T,
    ) -> Result<(), RegionVecError>
    where
        T: Clone,
    {
        Self::checkpoint(cx)?;
        if new_len <= self.len() {
            self.truncate(new_len);
            return Ok(());
        }
        self.try_extend(cx, std::iter::repeat_n(value, new_len - self.len()))
    }

    pub fn try_resize_with(
        &mut self,
        cx: &QueryCx,
        new_len: usize,
        mut value: impl FnMut() -> T,
    ) -> Result<(), RegionVecError> {
        Self::checkpoint(cx)?;
        if new_len <= self.len() {
            self.truncate(new_len);
            return Ok(());
        }
        let additional = new_len - self.len();
        self.try_extend(cx, (0..additional).map(|_| value()))
    }

    pub fn try_clone(&self, cx: &QueryCx) -> Result<Self, RegionVecError>
    where
        T: Clone,
    {
        Self::checkpoint(cx)?;
        self.state.claim_owner()?;
        let mut cloned = Self {
            inner: Some(Vec::new_in(PrivateRegionAllocator { state: self.state })),
            state: self.state,
        };
        if let Err(error) = cloned.try_extend(cx, self.as_slice().iter().cloned()) {
            drop(cloned);
            return Err(error);
        }
        Ok(cloned)
    }

    pub fn pop(&mut self) -> Option<T> {
        self.inner_mut().pop()
    }

    pub fn truncate(&mut self, len: usize) {
        self.inner_mut().truncate(len);
    }

    pub fn clear(&mut self) {
        self.inner_mut().clear();
    }

    pub fn remove(&mut self, index: usize) -> T {
        self.inner_mut().remove(index)
    }

    pub fn swap_remove(&mut self, index: usize) -> T {
        self.inner_mut().swap_remove(index)
    }

    pub fn replace(&mut self, index: usize, value: T) -> Result<T, RegionVecError> {
        let len = self.len();
        let Some(slot) = self.get_mut(index) else {
            return Err(RegionVecError::IndexOutOfBounds { index, len });
        };
        Ok(mem::replace(slot, value))
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.as_mut_slice().iter_mut()
    }
}

impl<T> Drop for RegionVec<'_, T> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        // Drop glue and the allocator deallocation callback both complete
        // before the owner lease is released. If T::drop panics, the lease
        // deliberately remains live and RegionScope will retain the storage.
        drop(inner);
        self.state.release_owner();
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for RegionVec<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: PartialEq> PartialEq for RegionVec<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq> Eq for RegionVec<'_, T> {}

impl<T> AsRef<[T]> for RegionVec<'_, T> {
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> AsMut<[T]> for RegionVec<'_, T> {
    fn as_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
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
        let mut region = Region::with_capacity(4096, 1 << 20, 1 << 21);
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
        let mut region = Region::with_capacity(256, 4096, 4096);
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
        let mut a = Region::with_capacity(256, 4096, 4096);
        let mut b = Region::with_capacity(256, 4096, 4096);
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
        let mut region = Region::with_capacity(256, 4096, 4096);
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
        let mut region = Region::with_capacity(256, 4096, 4096);
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
            let mut region = Region::with_capacity(256, 4096, 4096);
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
            let mut region = Region::with_capacity(128, 4096, 4096);
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
            // The resident side of the ledger: one 128-byte chunk held both
            // blocks, the first block's alignment pad is under its alignment,
            // and the peak never exceeded the configured limit.
            assert_eq!(audit.chunks_allocated, 1);
            assert_eq!(audit.peak_resident_bytes, 128);
            assert!(audit.alignment_padding_bytes < 8);
            assert!(audit.peak_resident_bytes <= 4096);
        }
    }

    #[test]
    fn a_closed_region_refuses_everything() {
        let mut region = Region::with_capacity(128, 4096, 4096);
        let handle = region.alloc_block(8, 1).expect("block");
        let audit = region.close();
        assert!(audit.balanced());
        // The handle outlives the region value only in the test's own hands;
        // what matters is that a region cannot be reopened, which the type
        // system already guarantees by consuming `self`.
        let reopened = Region::with_capacity(128, 4096, 4096);
        assert!(matches!(
            reopened.block(handle),
            Err(ArenaError::ForeignHandle { .. })
        ));
    }

    #[test]
    fn the_budget_is_enforced_and_release_returns_it() {
        let mut region = Region::with_capacity(64, 96, 128);
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
        let mut region = Region::with_capacity(256, 4096, 4096);
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

    #[test]
    #[should_panic(expected = "a region needs a non-zero chunk size")]
    fn construction_refuses_a_zero_chunk() {
        let _ = Region::with_capacity(0, 64, 64);
    }

    #[test]
    #[should_panic(expected = "exceeds the live-byte budget")]
    fn construction_refuses_a_chunk_larger_than_the_live_budget() {
        // The bead's named case: `chunk_bytes` greater than the live budget
        // can never be anything but unreachable waste, so the refusal happens
        // at construction, not as a surprising first-allocation failure.
        let _ = Region::with_capacity(128, 64, 128);
    }

    #[test]
    #[should_panic(expected = "exceeds the resident limit")]
    fn construction_refuses_a_live_budget_above_the_resident_limit() {
        // Live bytes occupy resident chunk capacity, so a live budget above
        // the resident limit is unreachable: a configuration mistake, named
        // at construction.
        let _ = Region::with_capacity(64, 128, 64);
    }

    #[test]
    fn resident_limit_is_charged_per_chunk_and_refusals_mutate_nothing() {
        // The hole this contract closes: released ranges are never reused, so
        // under allocate/release churn the live budget stays open while chunk
        // capacity grows without bound. The resident limit is what refuses,
        // and it refuses before anything mutates.
        let mut region = Region::with_capacity(64, 96, 128);
        let first = region.alloc_block(64, 1).expect("first");
        assert_eq!(region.release(first), Ok(64));
        let second = region.alloc_block(64, 1).expect("second: a fresh chunk");
        assert_eq!(region.release(second), Ok(64));
        // Under the pinned toolchain `try_reserve_exact` records exactly the
        // requested capacity, so the accepted charge equals `chunk_bytes`.
        // If that implementation detail ever moves, this assertion turns red
        // on purpose: the accounting changed meaningfully and the audit
        // contract must be re-examined, not quietly re-baselined.
        assert_eq!(region.resident_bytes(), 128);
        assert_eq!(region.live_bytes(), 0);

        let before = (
            region.bytes_allocated(),
            region.bytes_reclaimed(),
            region.live_bytes(),
            region.resident_bytes(),
        );
        assert!(matches!(
            region.alloc_block(64, 1),
            Err(ArenaError::ResidentLimitExceeded {
                requested: 64,
                remaining: 0
            })
        ));
        let after = (
            region.bytes_allocated(),
            region.bytes_reclaimed(),
            region.live_bytes(),
            region.resident_bytes(),
        );
        assert_eq!(
            before, after,
            "a resident refusal mutated region accounting"
        );
        // The live budget alone would have waved this allocation through;
        // the two budgets answer different questions, and that is the point.
        assert!(64 <= region.max_live_bytes() - region.live_bytes());

        let audit = region.close();
        assert!(audit.balanced());
        assert_eq!(audit.chunks_allocated, 2);
        assert_eq!(audit.peak_resident_bytes, 128);
    }

    #[test]
    fn alignment_padding_is_charged_and_audited() {
        let mut region = Region::with_capacity(256, 4096, 4096);
        let a = region.alloc_block(8, 1).expect("a");
        let b = region.alloc_block(8, MAX_BLOCK_ALIGN).expect("b");
        let address_a = region.block(a).expect("a live").as_ptr().addr();
        let address_b = region.block(b).expect("b live").as_ptr().addr();
        assert_eq!(address_b % MAX_BLOCK_ALIGN, 0);
        // `a` needs no padding at align 1, so the exact pad before `b` is
        // observable from the two addresses.
        let expected_pad = address_b - (address_a + 8);
        assert!(expected_pad < MAX_BLOCK_ALIGN);
        assert_eq!(region.alignment_padding_bytes(), expected_pad as u64);
        // Padding lives inside charged chunk capacity: part of the resident
        // footprint, never part of live bytes.
        assert_eq!(region.live_bytes(), 16);
        assert_eq!(region.resident_bytes(), 256);
        assert!(16 + expected_pad <= region.resident_bytes());

        let audit = region.close();
        assert!(audit.balanced());
        assert_eq!(audit.alignment_padding_bytes, expected_pad as u64);
        assert_eq!(audit.chunks_allocated, 1);
        assert_eq!(audit.peak_resident_bytes, 256);
    }
}
