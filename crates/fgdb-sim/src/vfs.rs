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
//! # Injectable latency (fgdb-milt)
//!
//! The fifth §15 fault class. A delay that is recorded but not awaited is a
//! placebo — every test passes and none is stronger — so the model only
//! ships it *awaited*: an eligible sync (a file flush with dirty sectors, or
//! a directory sync with pending dirent operations) that
//! [`FaultPlan::latency`] selects awaits [`FaultPlan::latency_micros`] of
//! timer time **before** the sync's fault-or-write logic runs, and the
//! [`FaultKind::Latency`] event records the delay actually awaited.
//!
//! The clock question is answered by construction, not ambience: a plan with
//! latency enabled must be built with [`FaultVfs::with_clock`] /
//! [`FaultVfs::unix_with_clock`], which hold a `Cx` whose timer driver
//! supplies `now` — virtual under the lab runtime (where the scheduler
//! advances it), real under a live one. The plain constructors refuse a
//! latency-enabled plan outright rather than degrading to no-delay, which
//! would be the placebo again. Reads and writes are served from the
//! in-memory image and stay undelayed: they are `poll` implementations that
//! cannot await a timer, and pretending otherwise is the same placebo.
//!
//! # What this increment deliberately does not model
//! * **`OpenOptions::append`.** The cursor starts at 0 on every open. Callers
//!   that append must seek; an append-mode handle would silently write at the
//!   wrong offset, so this is stated rather than approximated.
//! * **Cross-handle visibility of unsynced bytes.** A real page cache is shared
//!   between handles; here, dirty bytes are visible only through the handle
//!   that wrote them. The model is deliberately stricter than reality: it can
//!   only make a test fail that reality would also fail, never the reverse.
//! * **Dirent tearing.** Namespace loss is modelled whole-operation (see
//!   below): a crash loses an unsynced dirent operation entirely or not at
//!   all. A half-applied rename (both names present, or neither) is a
//!   journal-implementation artifact this model does not represent.
//! * **`create_dir`/`remove_dir`/`hard_link` durability.** Directory-tree
//!   shape and extra links are applied straight to the backing store and
//!   survive every crash; only *file* dirents (create, rename, remove) are
//!   modelled. Chronicle creates its database directory once at open and
//!   never links; modelling those would add surface no landed campaign can
//!   drive.
//!
//! # Dirent durability (fgdb-3a3u)
//!
//! A file sync makes an inode's *contents* durable; it does not make the
//! *name* by which recovery finds that inode durable — that is the parent
//! directory's sync. So namespace operations are tracked as **pending dirent
//! operations** against their parent directory:
//!
//! * creating a file (an open that brings it into existence, including
//!   [`Vfs::write`]), renaming, and removing all record a pending operation
//!   owing a sync to the affected parent directory (both parents, for a
//!   cross-directory rename);
//! * the backing store is updated immediately — pre-crash readers see the
//!   new namespace exactly as a page cache would show it;
//! * an **honest directory sync** settles every pending operation owing that
//!   directory (and syncs the backing directory);
//! * a **lying directory sync** ([`FaultPlan::dirent_lie`]) reports success
//!   and settles nothing — the names are still volatile;
//! * at [`FaultVfs::crash`], each still-pending operation is put to
//!   [`FaultPlan::dirent_loss`]: a loser is rolled back against the backing
//!   store (a created file's name vanishes even if its contents were
//!   honestly synced — the classic fsync-the-file, forget-the-directory bug;
//!   a rename reverts, restoring any clobbered destination's durable bytes;
//!   a removed file reappears with its durable bytes), while a survivor was
//!   committed by the journal anyway — the measured common case, and why an
//!   ENOSPC-refused capsule's residue outlives a faultless crash.
//!
//! A directory sync with nothing pending stays an honest no-op consuming no
//! trigger counts, so the landed campaigns' `Nth` arithmetic over file syncs
//! is untouched: `dirent_lie` and `dirent_loss` have their own eligibility
//! counters.
//!
//! (This fence was briefly demoted to `text` when the doc target failed with
//! `E0463: can't find crate for fgdb_chronicle` against `src/lib.rs:31` — the
//! crate's own imports, not this example. That was diagnosed during a
//! disk-full outage and under remote execution; with the filesystem restored
//! the doc target links and the example compiles, so the demotion was
//! treating a build-environment symptom. Restored, and left compiled so it
//! keeps being type-checked.)
//!
//! ```no_run
//! use fgdb_sim::vfs::{FaultPlan, FaultVfs, Trigger};
//!
//! let plan = FaultPlan {
//!     fsync_lie: Trigger::Nth(2),
//!     ..FaultPlan::faultless()
//! };
//! let vfs = FaultVfs::unix(plan);
//! // ... drive the database through `vfs`, then:
//! # async fn drive(vfs: fgdb_sim::vfs::FaultVfs) -> std::io::Result<()> {
//! vfs.crash().await?;
//! # Ok(())
//! # }
//! for event in vfs.events() {
//!     println!("{event:?}");
//! }
//! ```

use asupersync::Cx;
use asupersync::cx::cap;
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
    /// A directory sync that returns success while settling none of the
    /// pending dirent operations owing that directory. Eligible only when at
    /// least one operation is pending, on its own counter — file syncs and
    /// directory syncs never share `Nth` arithmetic.
    pub dirent_lie: Trigger,
    /// A pending dirent operation actually lost at [`FaultVfs::crash`],
    /// decided per pending operation on its own counter. `Never` (the
    /// default) models a journal that happened to commit the namespace
    /// update anyway — the measured common case, and what keeps a faultless
    /// plan namespace-transparent. An operation an honest directory sync
    /// settled is immune: durable names cannot be lost.
    pub dirent_loss: Trigger,
    /// An eligible sync delayed by [`FaultPlan::latency_micros`] of awaited
    /// timer time before its fault-or-write logic runs. Own counter; a plan
    /// enabling this requires a clock-bearing constructor
    /// ([`FaultVfs::with_clock`]) or construction refuses.
    pub latency: Trigger,
    /// How long a selected sync is delayed, in microseconds.
    pub latency_micros: u64,
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
            dirent_lie: Trigger::Never,
            dirent_loss: Trigger::Never,
            latency: Trigger::Never,
            latency_micros: 0,
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
    /// A directory sync returned success while settling none of the dirent
    /// operations owing that directory. They stay pending: a later honest
    /// sync still settles them, and a crash can still lose them.
    DirentSyncLie {
        /// Pending dirent operations the caller believed durable that are not.
        pending_ops: u64,
    },
    /// A pending dirent operation was lost at crash and rolled back against
    /// the backing store. The event's path is the name that vanished.
    DirentLoss {
        /// Which namespace operation was lost: "created", "renamed", or
        /// "removed".
        op: &'static str,
    },
    /// An eligible sync was delayed. Records the delay actually awaited —
    /// this event exists only after the timer completed, never before.
    Latency {
        /// The awaited delay, in microseconds.
        micros: u64,
    },
}

impl FaultKind {
    /// The fault class this belongs to, as a stable name.
    ///
    /// Stable because it is reported in graded replay bundles
    /// (`crate::completeness`), where a renamed class silently changes what a
    /// bundle claims was reproduced.
    #[must_use]
    pub const fn class(self) -> &'static str {
        match self {
            Self::FsyncLie { .. } => "fsync-lie",
            Self::TornWrite { .. } => "torn-write",
            Self::BitFlip { .. } => "bit-flip",
            Self::OutOfSpace { .. } => "out-of-space",
            Self::DirentSyncLie { .. } => "dirent-sync-lie",
            Self::DirentLoss { .. } => "dirent-loss",
            Self::Latency { .. } => "latency",
        }
    }
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
    DirentLie = 3,
    DirentLoss = 4,
    Latency = 5,
}

/// One namespace operation applied to the backing store whose durability is
/// still owed to one or two parent-directory syncs.
struct NamespaceOp {
    /// Parent directories that must honestly sync before this operation is
    /// durable. Two entries only for a cross-directory rename.
    owing: Vec<PathBuf>,
    kind: NamespaceKind,
}

enum NamespaceKind {
    /// `path` came into existence. Loss-undo removes it: the name was never
    /// durable, even if the file's contents were honestly synced.
    Created { path: PathBuf },
    /// `from` became `to`. Loss-undo renames back and, when the rename
    /// clobbered an existing `to`, restores that file's durable bytes.
    Renamed {
        from: PathBuf,
        to: PathBuf,
        replaced: Option<Vec<u8>>,
    },
    /// `path` was unlinked. Loss-undo restores its durable bytes.
    Removed { path: PathBuf, durable: Vec<u8> },
}

impl NamespaceKind {
    /// The stable operation name a [`FaultKind::DirentLoss`] event reports.
    const fn op_name(&self) -> &'static str {
        match self {
            Self::Created { .. } => "created",
            Self::Renamed { .. } => "renamed",
            Self::Removed { .. } => "removed",
        }
    }

    /// The name the loss makes vanish (or reappear, for a removal).
    fn vanished_path(&self) -> &Path {
        match self {
            Self::Created { path } | Self::Removed { path, .. } => path,
            Self::Renamed { to, .. } => to,
        }
    }
}

struct LabState {
    plan: FaultPlan,
    rng: u64,
    eligible: [u64; 6],
    events: Vec<FaultEvent>,
    namespace: Vec<NamespaceOp>,
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
            Class::DirentLie => self.plan.dirent_lie,
            Class::DirentLoss => self.plan.dirent_loss,
            Class::Latency => self.plan.latency,
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
/// space budget, the crash generation, and (when latency is enabled) the
/// clock whose timer supplies awaited delays.
pub struct Lab {
    state: Mutex<LabState>,
    clock: Option<Cx<cap::All>>,
}

impl std::fmt::Debug for Lab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lab")
            .field("state", &self.state)
            .field("has_clock", &self.clock.is_some())
            .finish()
    }
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
    fn new(plan: FaultPlan, clock: Option<Cx<cap::All>>) -> Self {
        Self {
            clock,
            state: Mutex::new(LabState {
                plan,
                rng: plan.seed,
                eligible: [0; 6],
                events: Vec::new(),
                namespace: Vec::new(),
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

    /// [`FaultVfs::unix`] with a clock, for latency-enabled plans.
    #[must_use]
    pub fn unix_with_clock(plan: FaultPlan, clock: Cx<cap::All>) -> Self {
        Self::with_clock(UnixVfs::new(), plan, clock)
    }
}

impl<V: Vfs> FaultVfs<V> {
    /// Wraps `backing` with `plan`.
    ///
    /// # Panics
    ///
    /// Refuses a latency-enabled plan: without a clock the delay could only
    /// be recorded, not awaited, and an unawaited delay is a placebo. Use
    /// [`FaultVfs::with_clock`].
    pub fn new(backing: V, plan: FaultPlan) -> Self {
        assert!(
            plan.latency == Trigger::Never,
            "a latency-enabled plan needs a clock: use FaultVfs::with_clock, \
             which awaits the delay through the Cx's timer (virtual under the \
             lab runtime) instead of silently not delaying"
        );
        Self {
            backing: Arc::new(backing),
            lab: Arc::new(Lab::new(plan, None)),
        }
    }

    /// Wraps `backing` with `plan` and the clock that supplies awaited
    /// delays — virtual time under the lab runtime's `Cx`, real time under a
    /// live one.
    ///
    /// # Panics
    ///
    /// Refuses a latency-enabled plan whose `clock` carries no timer driver:
    /// degrading to no-delay would be the placebo this class exists to avoid.
    pub fn with_clock(backing: V, plan: FaultPlan, clock: Cx<cap::All>) -> Self {
        assert!(
            plan.latency == Trigger::Never || clock.timer_driver().is_some(),
            "the supplied Cx has no timer driver; a latency-enabled plan \
             cannot await its delays through it"
        );
        Self {
            backing: Arc::new(backing),
            lab: Arc::new(Lab::new(plan, Some(clock))),
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
    ///
    /// Each pending dirent operation — a namespace change whose parent
    /// directory never honestly synced — is put to [`FaultPlan::dirent_loss`],
    /// oldest first so the eligibility counter is stable. Losers are rolled
    /// back against the backing store, newest first; survivors were committed
    /// by the journal anyway and are durable from here on (see the module's
    /// dirent-durability section).
    ///
    /// # Errors
    ///
    /// Returns an error if the backing store refuses a rollback operation.
    /// That is harness breakage, not a simulated fault, and swallowing it
    /// would hand later assertions a namespace neither durable nor volatile.
    pub async fn crash(&self) -> io::Result<()> {
        let lost = {
            let mut lab = self.lab.lock();
            lab.generation += 1;
            let pending = std::mem::take(&mut lab.namespace);
            let mut lost = Vec::new();
            for op in pending {
                if lab.fires(Class::DirentLoss) {
                    lab.record(
                        op.kind.vanished_path(),
                        FaultKind::DirentLoss {
                            op: op.kind.op_name(),
                        },
                    );
                    lost.push(op);
                }
            }
            lost
        };
        for op in lost.into_iter().rev() {
            match op.kind {
                NamespaceKind::Created { path } => {
                    match self.backing.remove_file(&path).await {
                        Ok(()) => {}
                        // Already absent (e.g. a later, also-lost removal was
                        // rolled back first): the name is gone either way.
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                NamespaceKind::Renamed { from, to, replaced } => {
                    self.backing.rename(&to, &from).await?;
                    if let Some(durable) = replaced {
                        self.backing.write(&to, &durable).await?;
                    }
                }
                NamespaceKind::Removed { path, durable } => {
                    self.backing.write(&path, &durable).await?;
                }
            }
        }
        Ok(())
    }

    /// Dirent operations recorded and not yet settled by an honest sync of
    /// every parent directory owing them.
    #[must_use]
    pub fn pending_dirent_ops(&self) -> usize {
        self.lab.lock().namespace.len()
    }

    fn record_namespace(&self, owing: Vec<PathBuf>, kind: NamespaceKind) {
        // An operation with no parent to sync (a root-level path) has no
        // barrier to model; it is durable the moment the backing store
        // applied it, which the caller already did.
        if owing.is_empty() {
            return;
        }
        self.lab.lock().namespace.push(NamespaceOp { owing, kind });
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
    is_dir: bool,
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

    /// Awaits the plan's latency for one eligible sync, when selected. The
    /// event is recorded only after the timer completes, so it reports a
    /// delay that was awaited, never one that was merely intended.
    async fn maybe_delay(&self) -> io::Result<()> {
        let micros = {
            let mut lab = self.lab.lock();
            if lab.fires(Class::Latency) {
                Some(lab.plan.latency_micros)
            } else {
                None
            }
        };
        let Some(micros) = micros else {
            return Ok(());
        };
        let Some(clock) = self.lab.clock.as_ref() else {
            // Unreachable by construction — the clockless constructors
            // refuse a latency-enabled plan — but if it ever becomes
            // reachable, refusing beats silently not delaying.
            return Err(io::Error::other(
                "latency fired with no clock to await it through",
            ));
        };
        // Registered against the CONSTRUCTION clock's driver, not resolved
        // ambiently: `Sleep`'s ambient `Cx::current()` lookup is documented
        // to return `None` inside a polled future on at least one path, and
        // its wall-clock fallback cannot observe the lab's virtual time —
        // measured here as a sleep that never completed (quiescence stall at
        // the step budget). Explicit registration makes the delay virtual
        // under the lab clock and real under a live one, by construction.
        let Some(driver) = clock.timer_driver() else {
            return Err(io::Error::other(
                "latency fired but the clock lost its timer driver",
            ));
        };
        let deadline = asupersync::types::Time::from_nanos(
            clock
                .now()
                .as_nanos()
                .saturating_add(micros.saturating_mul(1_000)),
        );
        let mut registered: Option<asupersync::time::TimerHandle> = None;
        poll_fn(|task_cx| {
            if clock.now().as_nanos() >= deadline.as_nanos() {
                return Poll::Ready(());
            }
            if registered.is_none() {
                registered = Some(driver.register(deadline, task_cx.waker().clone()));
            }
            Poll::Pending
        })
        .await;
        self.lab
            .lock()
            .record(&self.path, FaultKind::Latency { micros });
        Ok(())
    }

    /// The directory half of the sync surface: settle or lie about the
    /// pending dirent operations owing this directory.
    async fn sync_dirents(&self) -> io::Result<()> {
        let pending_ops = {
            let lab = self.lab.lock();
            lab.namespace
                .iter()
                .filter(|op| op.owing.contains(&self.path))
                .count() as u64
        };
        if pending_ops == 0 {
            // Nothing owes this directory, so no fault class is eligible: an
            // honest no-op must not consume a trigger count. This is also
            // what keeps the landed campaigns' file-sync arithmetic intact.
            return Ok(());
        }
        self.maybe_delay().await?;

        {
            let mut lab = self.lab.lock();
            if lab.fires(Class::DirentLie) {
                lab.record(&self.path, FaultKind::DirentSyncLie { pending_ops });
                // Return success having settled nothing: the operations stay
                // pending, exactly as an fsync lie leaves sectors dirty.
                return Ok(());
            }
        }

        // Honest: every pending operation stops owing this directory, and an
        // operation owing nothing further is durable. Matching is by the
        // path this handle was opened with — the same spelling callers pass
        // when they create under a directory and then sync it.
        {
            let mut lab = self.lab.lock();
            for op in &mut lab.namespace {
                op.owing.retain(|dir| dir != &self.path);
            }
            lab.namespace.retain(|op| !op.owing.is_empty());
        }
        let directory = self
            .backing
            .open(&self.path, &OpenOptions::new().read(true))
            .await?;
        directory.sync_all().await
    }

    async fn flush_through(&self) -> io::Result<()> {
        self.alive()?;
        if self.is_dir {
            return self.sync_dirents().await;
        }

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

        // --- injectable latency ----------------------------------------------
        // Before the fault-or-write logic: the sync is slow first, and only
        // then honest, lying, torn, or refused — a real device's ordering.
        self.maybe_delay().await?;

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
        // Existence is sampled before the delegated open: `OpenOptions` has
        // no getters, so "did this open create the file" is observable only
        // as before-absent/after-present. A directory opens only to be
        // synced (`sync_directory` is how a dirent barrier is expressed over
        // a `Vfs`); its handle carries an empty image, nothing can go dirty
        // through it, and its sync is the dirent settle/lie surface. Reading
        // a directory as bytes would be EISDIR, which is why it cannot share
        // the file arm below.
        let before = self.backing.metadata(path).await;
        let existed = before.is_ok();
        let is_directory = before
            .map(|metadata| metadata.file_type().is_dir())
            .unwrap_or(false);
        // Delegate the open itself so create/create_new/truncate/mode all keep
        // their real semantics, including their real errors.
        drop(self.backing.open(path, opts).await?);
        if !existed && self.backing.metadata(path).await.is_ok() {
            // The open brought the file into existence: its dirent owes the
            // parent directory a sync before the name is durable.
            self.record_namespace(
                path.parent().map(Path::to_path_buf).into_iter().collect(),
                NamespaceKind::Created {
                    path: path.to_path_buf(),
                },
            );
        }
        let image = if is_directory {
            Vec::new()
        } else {
            match self.backing.read(path).await {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(error),
            }
        };
        let generation = self.lab.lock().generation;
        Ok(FaultFile {
            backing: Arc::clone(&self.backing),
            lab: Arc::clone(&self.lab),
            path: path.to_path_buf(),
            generation,
            is_dir: is_directory,
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
        // The durable bytes are sampled before the unlink so a crash can
        // restore them: an unsettled removal never happened, namespace-wise.
        let durable = match self.backing.read(path).await {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        self.backing.remove_file(path).await?;
        if let Some(durable) = durable {
            self.record_namespace(
                path.parent().map(Path::to_path_buf).into_iter().collect(),
                NamespaceKind::Removed {
                    path: path.to_path_buf(),
                    durable,
                },
            );
        }
        Ok(())
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        // A rename touches two dirents: the unlink under `from`'s parent and
        // the link under `to`'s parent. It is durable only once every
        // distinct parent involved has honestly synced. A clobbered `to` has
        // its durable bytes sampled first, so a crash restores both names.
        let replaced = match self.backing.read(to).await {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        self.backing.rename(from, to).await?;
        let mut owing: Vec<PathBuf> = from
            .parent()
            .into_iter()
            .chain(to.parent())
            .map(Path::to_path_buf)
            .collect();
        owing.dedup();
        self.record_namespace(
            owing,
            NamespaceKind::Renamed {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                replaced,
            },
        );
        Ok(())
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
