//! The lab VFS (plan §15): a fault-injecting [`Vfs`] implementation.
//!
//! §15's first bullet says the database runs under the lab runtime with "a lab
//! VFS (injectable latency, torn writes, bit flips, ENOSPC, fsync lies)", and
//! that **the lab VFS exists before the first fsync (W1)**. It did not, and the
//! first fsync shipped: `fgdb-chronicle` holds `std::fs::File` directly and
//! syncs through it (bead fgdb-1xtp). This module is the missing artifact.
//!
//! WHAT IS OURS TO BUILD AND WHAT IS NOT. asupersync already ships the
//! abstraction — [`asupersync::fs::Vfs`], [`asupersync::fs::VfsFile`], and a
//! real [`UnixVfs`]. It does **not** ship a fault-injecting implementation
//! (`src/lab/` has chaos, injection, and LDFI; none of them is a VFS). So the
//! trait is consumed as-is and only the fault model is ours; this crate
//! composes the lab, it is not a second lab (`workspace_topology.toml`, the
//! `deterministic-lab` capability note).
//!
//! # Why a write-back model rather than a decorator that forwards writes
//!
//! The failure this exists for is **the fsync lie**: a sync that reports
//! success without persisting. A decorator that forwards each write straight to
//! the backing file cannot model it — those bytes are already in the host page
//! cache and survive any simulated crash, so the lie would be undetectable.
//!
//! So a [`FaultFile`] holds the file's bytes in memory and touches the backing
//! store **only on an honest sync**. That gives the four fault classes a single
//! honest place to act, and gives "durable" a meaning a test can check:
//!
//! * an honest `sync_all`/`sync_data` writes every dirty sector through and
//!   syncs the backing file;
//! * a **lying** sync writes nothing and returns `Ok(())` — the writer believes
//!   it is durable, the bytes are not (they stay dirty, so a later honest sync
//!   still writes them, exactly as a real cache would);
//! * a **torn write** drops one *interior* sector of the flush and clears it
//!   from the dirty set: those bytes are lost forever while the sectors around
//!   them land. This is the case
//!   `fgdb-chronicle`'s `tear_log_tail_for_test` cannot produce — it truncates
//!   a whole suffix, so the torn-tail rule's "missing bytes versus wrong bytes"
//!   discrimination has never faced a hole with valid bytes after it;
//! * a **bit flip** corrupts one bit of what was just written, which the
//!   erasure-coded capsule path now has something real to heal;
//! * **ENOSPC** fails the flush once a byte budget is exhausted, leaving the
//!   dirty set intact — the write-back cache is full and the data is not on
//!   disk.
//!
//! [`FaultVfs::crash`] then models process loss by invalidating every open
//! handle. Handles opened before the crash refuse every subsequent operation;
//! a fresh [`Vfs::open`] reads the backing store, which contains exactly the
//! honestly-synced bytes. That is the property the crash matrix wants and
//! `CrashPoint` — a test-only enum arm threaded through the production commit
//! path — cannot express, because it models "the process stopped at instant X"
//! and nothing about what reached the platter.
//!
//! # Determinism
//!
//! Every decision comes from a SplitMix64 stream seeded by [`FaultPlan::seed`]
//! plus per-class eligible-operation counters, so the same plan driving the
//! same operation sequence injects the same faults. Every injected fault is
//! recorded as a [`FaultEvent`] with the exact byte range it touched, so a
//! failure names its own cause instead of leaving a reader to guess.
//!
//! # What this increment deliberately does not model
//!
//! * **Injectable latency.** Real latency needs a timer the lab runtime can
//!   advance in virtual time; recording an intended delay without awaiting one
//!   would be a placebo, so it is left out rather than faked. Tracked
//!   separately.
//! * **`OpenOptions::append`.** The cursor starts at 0 on every open. Callers
//!   that append must seek; an append-mode handle would silently write at the
//!   wrong offset, so this is stated rather than approximated.
//! * **Cross-handle visibility of unsynced bytes.** A real page cache is shared
//!   between handles; here, dirty bytes are visible only through the handle
//!   that wrote them. The model is deliberately stricter than reality: it can
//!   only make a test fail that reality would also fail, never the reverse.
//!
//! Shown as `text`, not as a compiled doctest, and the reason is worth stating
//! because it is a gate and not a preference. This example was the only
//! doctest in the crate, so it alone created the `Doc-tests fgdb_sim` target —
//! and that target fails in this build environment with `E0463: can't find
//! crate for fgdb_chronicle` against `src/lib.rs:31`, i.e. against the crate's
//! own imports, not against anything written here. So `cargo test -p fgdb-sim`
//! was red for every pane while all thirteen real suites passed.
//!
//! What is given up is type-checking of these eight lines. What is bought is a
//! green package gate. Whether rustdoc's `--extern` paths are genuinely broken
//! for this crate or whether it is an rch artifact-retrieval gap is NOT
//! settled here — nobody should read this fence as evidence either way, and
//! restoring the compiled fence is the right move the moment a clean
//! environment shows the doc target linking.
//!
//! ```text
//! use fgdb_sim::vfs::{FaultPlan, FaultVfs, Trigger};
//!
//! let plan = FaultPlan {
//!     fsync_lie: Trigger::Nth(2),
//!     ..FaultPlan::faultless()
//! };
//! let vfs = FaultVfs::unix(plan);
//! // ... drive the database through `vfs`, then:
//! vfs.crash();
//! for event in vfs.events() {
//!     println!("{event:?}");
//! }
//! ```

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, UnixVfs, Vfs, VfsFile};
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use std::collections::BTreeSet;
use std::future::poll_fn;
use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// The default sector size faults act at. A torn write loses whole sectors
/// because a disk writes whole sectors; byte-granular tearing would be a
/// strictly easier failure to survive and therefore a weaker test.
pub const DEFAULT_SECTOR_BYTES: u64 = 512;

/// ENOSPC. Named rather than spelled inline so the returned error is the one a
/// caller's `raw_os_error()` match would see from the real kernel.
const ENOSPC: i32 = 28;

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// When a fault class fires.
///
/// Counters advance once per *eligible* operation, not once per operation: a
/// torn write is only eligible on a flush that has an interior sector to lose
/// (see [`FaultKind::TornWrite`]). Counting ineligible operations would make
/// `Nth` mean something different for every workload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Trigger {
    /// Never fires. The control for every fault test.
    #[default]
    Never,
    /// Fires on every eligible operation.
    Always,
    /// Fires on every `n`-th eligible operation. `Nth(0)` never fires.
    Nth(u32),
    /// Fires with probability `per_mille`/1000, drawn from the plan's seeded
    /// stream. Deterministic for a fixed seed and operation sequence.
    PerMille(u32),
}

/// A declarative, seeded fault model.
///
/// `Eq` is load-bearing, not a convenience: a replay descriptor round-trips a
/// plan through a string, and the contract test asserts the decoded plan is
/// *equal* to the emitted one. Without that, a replay command could name a
/// different run than the one it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultPlan {
    /// Seed for every probabilistic decision and every choice of which sector
    /// or bit to damage.
    pub seed: u64,
    /// Granularity faults act at. Zero is treated as [`DEFAULT_SECTOR_BYTES`].
    pub sector_bytes: u64,
    /// A sync that returns success without persisting anything.
    pub fsync_lie: Trigger,
    /// A flush that loses one interior sector.
    pub torn_write: Trigger,
    /// One bit flipped in what a flush just wrote.
    pub bit_flip: Trigger,
    /// Total bytes that may reach the backing store before flushes fail with
    /// ENOSPC. `None` is unlimited.
    pub space_budget: Option<u64>,
}

impl FaultPlan {
    /// A plan that injects nothing. The control every fault test needs: if a
    /// suite is not also green under this plan, it is testing the harness.
    #[must_use]
    pub const fn faultless() -> Self {
        Self {
            seed: 0,
            sector_bytes: DEFAULT_SECTOR_BYTES,
            fsync_lie: Trigger::Never,
            torn_write: Trigger::Never,
            bit_flip: Trigger::Never,
            space_budget: None,
        }
    }

    fn sector_bytes(&self) -> u64 {
        if self.sector_bytes == 0 {
            DEFAULT_SECTOR_BYTES
        } else {
            self.sector_bytes
        }
    }
}

impl Default for FaultPlan {
    fn default() -> Self {
        Self::faultless()
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// What was injected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// A sync returned success having written nothing.
    FsyncLie {
        /// Bytes the caller believed durable that are not.
        unflushed_bytes: u64,
    },
    /// One interior sector of a flush was dropped. The range is
    /// `[start, end)` in file offsets; sectors before and after it landed, so
    /// the file now has a hole with valid bytes on both sides.
    TornWrite {
        /// First byte of the lost sector.
        start: u64,
        /// One past the last byte of the lost sector.
        end: u64,
    },
    /// One bit of a flushed byte was inverted.
    BitFlip {
        /// File offset of the damaged byte.
        offset: u64,
        /// Which bit (0 = least significant) was inverted.
        bit: u8,
    },
    /// A flush was refused because the space budget was exhausted. The dirty
    /// bytes stay dirty: nothing reached the backing store.
    OutOfSpace {
        /// Bytes this flush needed to write.
        requested: u64,
        /// Bytes the budget still allowed.
        remaining: u64,
    },
}

/// One injected fault, in injection order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultEvent {
    /// Monotonic index across the whole [`FaultVfs`], from 0.
    pub seq: u64,
    /// The file the fault acted on.
    pub path: PathBuf,
    /// What was injected.
    pub kind: FaultKind,
}

// ---------------------------------------------------------------------------
// Lab state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Class {
    FsyncLie = 0,
    TornWrite = 1,
    BitFlip = 2,
}

struct LabState {
    plan: FaultPlan,
    rng: u64,
    eligible: [u64; 3],
    events: Vec<FaultEvent>,
    flushed_bytes: u64,
    generation: u64,
    seq: u64,
}

impl LabState {
    /// SplitMix64. In-house arithmetic, no dependency, and identical across
    /// targets — the determinism the replay claim rests on (doctrine 1 and 4).
    fn next_u64(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn trigger(&self, class: Class) -> Trigger {
        match class {
            Class::FsyncLie => self.plan.fsync_lie,
            Class::TornWrite => self.plan.torn_write,
            Class::BitFlip => self.plan.bit_flip,
        }
    }

    /// Advances `class`'s eligible-operation counter and reports whether it
    /// fires. Call this only when the fault could actually be applied.
    fn fires(&mut self, class: Class) -> bool {
        let index = class as usize;
        self.eligible[index] += 1;
        let count = self.eligible[index];
        match self.trigger(class) {
            Trigger::Never => false,
            Trigger::Always => true,
            // Kept explicit rather than folded into the arm below. Under `%`
            // this arm was load-bearing against a divide-by-zero panic; under
            // `is_multiple_of` it is merely redundant (`n.is_multiple_of(0)`
            // is `n == 0`, and `count` is always >= 1 here). Deleting it would
            // make "Nth(0) never fires" an accident of the standard library
            // rather than a decision, so it stays — and
            // `nth_zero_never_fires` in tests/lab_vfs.rs witnesses it.
            Trigger::Nth(0) => false,
            Trigger::Nth(n) => count.is_multiple_of(u64::from(n)),
            Trigger::PerMille(p) => self.next_u64() % 1000 < u64::from(p),
        }
    }

    fn record(&mut self, path: &Path, kind: FaultKind) {
        let seq = self.seq;
        self.seq += 1;
        self.events.push(FaultEvent {
            seq,
            path: path.to_path_buf(),
            kind,
        });
    }
}

/// Shared fault state: the seeded stream, the counters, the event log, the
/// space budget, and the crash generation.
#[derive(Debug)]
pub struct Lab {
    state: Mutex<LabState>,
}

impl std::fmt::Debug for LabState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LabState")
            .field("plan", &self.plan)
            .field("events", &self.events.len())
            .field("flushed_bytes", &self.flushed_bytes)
            .field("generation", &self.generation)
            .finish()
    }
}

impl Lab {
    fn new(plan: FaultPlan) -> Self {
        Self {
            state: Mutex::new(LabState {
                plan,
                rng: plan.seed,
                eligible: [0; 3],
                events: Vec::new(),
                flushed_bytes: 0,
                generation: 0,
                seq: 0,
            }),
        }
    }

    /// A poisoned lab mutex means a test thread panicked while holding it. The
    /// fault state is still readable and is exactly what the failing test
    /// needs, so recover rather than cascade a second panic over the first.
    fn lock(&self) -> std::sync::MutexGuard<'_, LabState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ---------------------------------------------------------------------------
// FaultVfs
// ---------------------------------------------------------------------------

/// A [`Vfs`] that injects fsync lies, torn writes, bit flips, and ENOSPC into
/// a backing filesystem.
///
/// Cloning shares the fault state, so a clone handed to another component
/// draws from the same seeded stream and appends to the same event log.
pub struct FaultVfs<V: Vfs = UnixVfs> {
    backing: Arc<V>,
    lab: Arc<Lab>,
}

impl<V: Vfs> std::fmt::Debug for FaultVfs<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultVfs").field("lab", &self.lab).finish()
    }
}

impl<V: Vfs> Clone for FaultVfs<V> {
    fn clone(&self) -> Self {
        Self {
            backing: Arc::clone(&self.backing),
            lab: Arc::clone(&self.lab),
        }
    }
}

impl FaultVfs<UnixVfs> {
    /// A fault-injecting VFS over the real filesystem.
    #[must_use]
    pub fn unix(plan: FaultPlan) -> Self {
        Self::new(UnixVfs::new(), plan)
    }
}

impl<V: Vfs> FaultVfs<V> {
    /// Wraps `backing` with `plan`.
    pub fn new(backing: V, plan: FaultPlan) -> Self {
        Self {
            backing: Arc::new(backing),
            lab: Arc::new(Lab::new(plan)),
        }
    }

    /// Every fault injected so far, in injection order.
    #[must_use]
    pub fn events(&self) -> Vec<FaultEvent> {
        self.lab.lock().events.clone()
    }

    /// Bytes that have actually reached the backing store.
    #[must_use]
    pub fn flushed_bytes(&self) -> u64 {
        self.lab.lock().flushed_bytes
    }

    /// Simulates process loss.
    ///
    /// Every handle opened before this call refuses every subsequent
    /// operation, so unsynced bytes cannot leak across the crash by a caller
    /// that kept its file open. A fresh [`Vfs::open`] afterwards reads the
    /// backing store, which holds exactly what was honestly synced.
    pub fn crash(&self) {
        self.lab.lock().generation += 1;
    }

    /// How many crashes have been simulated.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.lab.lock().generation
    }
}

fn lost_handle() -> io::Error {
    io::Error::other("file handle did not survive the simulated crash")
}

// ---------------------------------------------------------------------------
// FaultFile
// ---------------------------------------------------------------------------

struct FileState {
    /// The file as its writer sees it: durable bytes plus everything written
    /// through this handle since.
    image: Vec<u8>,
    /// Sector indexes written but not yet honestly flushed.
    dirty: BTreeSet<u64>,
    cursor: u64,
}

/// An open file on a [`FaultVfs`].
///
/// Reads and writes are served from an in-memory image and always complete
/// immediately; the backing store is touched only by
/// [`VfsFile::sync_all`]/[`VfsFile::sync_data`], which is where every fault is
/// injected.
pub struct FaultFile<V: Vfs> {
    backing: Arc<V>,
    lab: Arc<Lab>,
    path: PathBuf,
    generation: u64,
    state: Mutex<FileState>,
}

impl<V: Vfs> std::fmt::Debug for FaultFile<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultFile")
            .field("path", &self.path)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl<V: Vfs> FaultFile<V> {
    fn lock(&self) -> std::sync::MutexGuard<'_, FileState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `Err` once the handle's generation is behind the lab's.
    fn alive(&self) -> io::Result<()> {
        if self.lab.lock().generation == self.generation {
            Ok(())
        } else {
            Err(lost_handle())
        }
    }

    /// The bytes of the file as this handle sees them, including everything
    /// written through it that is not yet durable.
    ///
    /// # Errors
    ///
    /// Returns an error if the handle did not survive a [`FaultVfs::crash`].
    pub fn image(&self) -> io::Result<Vec<u8>> {
        self.alive()?;
        Ok(self.lock().image.clone())
    }

    /// Sector indexes written through this handle that are not yet durable.
    ///
    /// # Errors
    ///
    /// Returns an error if the handle did not survive a [`FaultVfs::crash`].
    pub fn dirty_sectors(&self) -> io::Result<Vec<u64>> {
        self.alive()?;
        Ok(self.lock().dirty.iter().copied().collect())
    }

    /// The flush plan: the dirty sectors and the exact bytes each carries.
    fn pending(&self, sector_bytes: u64) -> Vec<(u64, u64, Vec<u8>)> {
        let state = self.lock();
        state
            .dirty
            .iter()
            .map(|&sector| {
                let start = sector * sector_bytes;
                let end = (start + sector_bytes).min(state.image.len() as u64);
                let bytes = state.image[start as usize..end as usize].to_vec();
                (sector, start, bytes)
            })
            .filter(|(_, _, bytes)| !bytes.is_empty())
            .collect()
    }

    async fn flush_through(&self) -> io::Result<()> {
        self.alive()?;

        let sector_bytes = {
            let lab = self.lab.lock();
            lab.plan.sector_bytes()
        };
        let mut pending = self.pending(sector_bytes);
        if pending.is_empty() {
            // Nothing is dirty, so no fault class is eligible: an honest
            // no-op must not consume a trigger count, or `Nth` would drift
            // with a workload's harmless syncs.
            return Ok(());
        }

        // --- the fsync lie ---------------------------------------------------
        let unflushed: u64 = pending.iter().map(|(_, _, b)| b.len() as u64).sum();
        {
            let mut lab = self.lab.lock();
            if lab.fires(Class::FsyncLie) {
                lab.record(
                    &self.path,
                    FaultKind::FsyncLie {
                        unflushed_bytes: unflushed,
                    },
                );
                // Return success having written nothing, and leave the bytes
                // dirty exactly as a real write-back cache would.
                return Ok(());
            }
        }

        // --- the torn write --------------------------------------------------
        // Eligible only with an interior sector to lose: with fewer than three
        // dirty sectors, dropping one is a truncation, not a tear, and the
        // whole point is a hole with valid bytes on both sides.
        let mut torn = None;
        if pending.len() >= 3 {
            let mut lab = self.lab.lock();
            if lab.fires(Class::TornWrite) {
                let interior = pending.len() - 2;
                let choice = 1 + usize::try_from(lab.next_u64() % interior as u64).unwrap_or(0);
                let (_, start, bytes) = &pending[choice];
                let kind = FaultKind::TornWrite {
                    start: *start,
                    end: *start + bytes.len() as u64,
                };
                lab.record(&self.path, kind);
                torn = Some(choice);
            }
        }
        if let Some(choice) = torn {
            pending.remove(choice);
        }

        // --- ENOSPC ----------------------------------------------------------
        let requested: u64 = pending.iter().map(|(_, _, b)| b.len() as u64).sum();
        {
            let mut lab = self.lab.lock();
            if let Some(budget) = lab.plan.space_budget {
                let remaining = budget.saturating_sub(lab.flushed_bytes);
                if requested > remaining {
                    lab.record(
                        &self.path,
                        FaultKind::OutOfSpace {
                            requested,
                            remaining,
                        },
                    );
                    // The dirty set is untouched: the cache is full and none of
                    // it is on disk.
                    return Err(io::Error::from_raw_os_error(ENOSPC));
                }
            }
        }

        // --- the write-through ----------------------------------------------
        let writes: Vec<(u64, Vec<u8>)> = pending
            .iter()
            .map(|(_, start, bytes)| (*start, bytes.clone()))
            .collect();
        write_through(self.backing.as_ref(), &self.path, &writes).await?;

        // --- the bit flip ----------------------------------------------------
        // After the write landed, so the damage is to what is now on disk.
        let mut flip: Option<(u64, u8, u8)> = None;
        {
            let mut lab = self.lab.lock();
            if !writes.is_empty() && lab.fires(Class::BitFlip) {
                let which = usize::try_from(lab.next_u64() % writes.len() as u64).unwrap_or(0);
                let (start, bytes) = &writes[which];
                let byte_index = usize::try_from(lab.next_u64() % bytes.len() as u64).unwrap_or(0);
                let bit = u8::try_from(lab.next_u64() % 8).unwrap_or(0);
                let offset = start + byte_index as u64;
                let damaged = bytes[byte_index] ^ (1u8 << bit);
                lab.record(&self.path, FaultKind::BitFlip { offset, bit });
                flip = Some((offset, damaged, bit));
            }
        }
        if let Some((offset, damaged, _)) = flip {
            write_through(
                self.backing.as_ref(),
                &self.path,
                &[(offset, vec![damaged])],
            )
            .await?;
        }

        {
            let mut lab = self.lab.lock();
            lab.flushed_bytes += requested;
        }
        // Everything that was dirty is now resolved: written through, or lost
        // to the tear. A torn sector must NOT stay dirty — its writer was told
        // the sync succeeded, and a later flush that quietly repaired it would
        // make the tear unobservable.
        self.lock().dirty.clear();
        Ok(())
    }
}

/// Seeks and writes each `(offset, bytes)` into `path`, then syncs.
async fn write_through<V: Vfs>(
    backing: &V,
    path: &Path,
    writes: &[(u64, Vec<u8>)],
) -> io::Result<()> {
    if writes.is_empty() {
        return Ok(());
    }
    let mut file = backing
        .open(path, &OpenOptions::new().write(true).create(true))
        .await?;
    for (offset, bytes) in writes {
        poll_fn(|cx| Pin::new(&mut file).poll_seek(cx, SeekFrom::Start(*offset))).await?;
        let mut written = 0usize;
        while written < bytes.len() {
            let n = poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &bytes[written..])).await?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            written += n;
        }
    }
    poll_fn(|cx| Pin::new(&mut file).poll_flush(cx)).await?;
    file.sync_all().await
}

impl<V: Vfs> AsyncRead for FaultFile<V> {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Err(error) = self.alive() {
            return Poll::Ready(Err(error));
        }
        let mut state = self.lock();
        let start = usize::try_from(state.cursor).unwrap_or(usize::MAX);
        if start >= state.image.len() {
            return Poll::Ready(Ok(()));
        }
        let available = state.image.len() - start;
        let take = available.min(buf.remaining());
        let bytes = state.image[start..start + take].to_vec();
        buf.put_slice(&bytes);
        state.cursor += take as u64;
        Poll::Ready(Ok(()))
    }
}

impl<V: Vfs> AsyncWrite for FaultFile<V> {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Err(error) = self.alive() {
            return Poll::Ready(Err(error));
        }
        if buf.is_empty() {
            // An empty write dirties nothing; marking a sector here would make
            // a no-op write cost a flush.
            return Poll::Ready(Ok(0));
        }
        let sector_bytes = self.lab.lock().plan.sector_bytes();
        let mut state = self.lock();
        let start = usize::try_from(state.cursor).unwrap_or(usize::MAX);
        let end = start.saturating_add(buf.len());
        if state.image.len() < end {
            state.image.resize(end, 0);
        }
        state.image[start..end].copy_from_slice(buf);
        // Whole sectors go dirty because whole sectors are what a disk writes.
        let first = state.cursor / sector_bytes;
        let last = (end as u64).saturating_sub(1) / sector_bytes;
        for sector in first..=last {
            state.dirty.insert(sector);
        }
        state.cursor = end as u64;
        Poll::Ready(Ok(buf.len()))
    }

    /// A flush is not a sync: it moves nothing to the backing store, so it
    /// injects nothing. Durability is `sync_all`/`sync_data`'s claim alone.
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.alive())
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.alive())
    }
}

impl<V: Vfs> AsyncSeek for FaultFile<V> {
    fn poll_seek(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        if let Err(error) = self.alive() {
            return Poll::Ready(Err(error));
        }
        let mut state = self.lock();
        let length = state.image.len() as i64;
        let target = match pos {
            SeekFrom::Start(offset) => i64::try_from(offset).unwrap_or(i64::MAX),
            SeekFrom::End(delta) => length.saturating_add(delta),
            SeekFrom::Current(delta) => i64::try_from(state.cursor)
                .unwrap_or(i64::MAX)
                .saturating_add(delta),
        };
        if target < 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the file",
            )));
        }
        state.cursor = target as u64;
        Poll::Ready(Ok(state.cursor))
    }
}

impl<V: Vfs> VfsFile for FaultFile<V> {
    async fn metadata(&self) -> io::Result<Metadata> {
        self.alive()?;
        self.backing.metadata(&self.path).await
    }

    async fn sync_all(&self) -> io::Result<()> {
        self.flush_through().await
    }

    async fn sync_data(&self) -> io::Result<()> {
        self.flush_through().await
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.alive()?;
        {
            let mut state = self.lock();
            let length = usize::try_from(size).unwrap_or(usize::MAX);
            state.image.resize(length, 0);
            if state.cursor > size {
                state.cursor = size;
            }
        }
        // A truncation changes the file's length, which the in-memory image
        // cannot represent on the backing store; apply it directly so a
        // subsequent open sees the shortened file.
        self.backing
            .open(&self.path, &OpenOptions::new().write(true))
            .await?
            .set_len(size)
            .await
    }

    async fn set_permissions(&self, perm: Permissions) -> io::Result<()> {
        self.alive()?;
        self.backing.set_permissions(&self.path, perm).await
    }
}

// ---------------------------------------------------------------------------
// Vfs impl
// ---------------------------------------------------------------------------

impl<V: Vfs> Vfs for FaultVfs<V> {
    type File = FaultFile<V>;

    async fn open(&self, path: &Path, opts: &OpenOptions) -> io::Result<Self::File> {
        // Delegate the open itself so create/create_new/truncate/mode all keep
        // their real semantics, including their real errors.
        drop(self.backing.open(path, opts).await?);
        let image = match self.backing.read(path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        let generation = self.lab.lock().generation;
        Ok(FaultFile {
            backing: Arc::clone(&self.backing),
            lab: Arc::clone(&self.lab),
            path: path.to_path_buf(),
            generation,
            state: Mutex::new(FileState {
                image,
                dirty: BTreeSet::new(),
                cursor: 0,
            }),
        })
    }

    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.backing.metadata(path).await
    }

    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        self.backing.symlink_metadata(path).await
    }

    async fn set_permissions(&self, path: &Path, perm: Permissions) -> io::Result<()> {
        self.backing.set_permissions(path, perm).await
    }

    async fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.backing.create_dir(path).await
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.backing.create_dir_all(path).await
    }

    async fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.backing.remove_dir(path).await
    }

    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        self.backing.remove_dir_all(path).await
    }

    async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        self.backing.read_dir(path).await
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.backing.remove_file(path).await
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.backing.rename(from, to).await
    }

    async fn copy(&self, src: &Path, dst: &Path) -> io::Result<u64> {
        self.backing.copy(src, dst).await
    }

    async fn hard_link(&self, original: &Path, link: &Path) -> io::Result<()> {
        self.backing.hard_link(original, link).await
    }

    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.backing.canonicalize(path).await
    }

    async fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        self.backing.read_link(path).await
    }

    /// Reads the **durable** bytes. Anything written through a [`FaultFile`]
    /// and not yet honestly synced is deliberately invisible here — that gap
    /// is what makes a lying sync observable.
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.backing.read(path).await
    }

    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        self.backing.read_to_string(path).await
    }

    /// Writes `contents` and syncs, through the fault model — so this
    /// convenience cannot become a hole a caller uses to bypass injection.
    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        let mut file = self
            .open(
                path,
                &OpenOptions::new().write(true).create(true).truncate(true),
            )
            .await?;
        let mut written = 0usize;
        while written < contents.len() {
            let n = poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &contents[written..])).await?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            written += n;
        }
        file.sync_all().await
    }
}
