//! The exported multi-component fixture state machine (plan §15.1, `fgdb-qd2s`).
//!
//! q97e's acceptance requires "the exported fixture state machine boots under
//! the lab with virtual time/disk/network and a pinned seed". This module is
//! that machine: a producer and a consumer, connected by an in-memory
//! [`VirtualTcpStream`] (network), each payload made durable through the
//! [`crate::vfs::FaultVfs`] write-back model (disk), with the producer pacing
//! itself on [`sleep`] (time — virtual under the lab's auto-advance, wall-clock
//! under the live runtime).
//!
//! The SAME futures run under both runtimes. That is the load-bearing design
//! decision: each side obtains its context via `Cx::current()` from whatever
//! runtime scheduled it, so the lab-vs-live dual run in [`crate::dual_run`]
//! diffs two executions of one program, not two programs that claim to agree.
//!
//! # What the trace is
//!
//! Every step appends one fixed-width [`FixtureEvent`] to a shared
//! [`TraceHandle`]. [`TraceHandle::to_bytes`] serializes the whole run —
//! timestamps, global interleaving, per-event chain digests — into a canonical
//! byte string. Two same-seed lab runs must produce **byte-identical** trace
//! strings; that is the two-runs-one-seed determinism gate. The live runtime
//! is entitled to different timestamps and a different interleaving, so the
//! dual run compares [`FixtureSemantics`] (digests and counters, no clocks)
//! rather than trace bytes.
//!
//! # The chain digest
//!
//! Producer and consumer independently fold every payload into a running
//! BLAKE3 chain. The consumer only sees payloads that survived the network,
//! so `producer_chain == consumer_chain` is the end-to-end integrity claim,
//! and it is asserted as a semantic counter rather than trusted silently.

use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use asupersync::Cx;
use asupersync::fs::{OpenOptions, Vfs, VfsFile};
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use asupersync::net::tcp::VirtualTcpStream;
use asupersync::time::sleep;
use fgdb_crypto::{Digest, Hasher};

use crate::vfs::{FaultEvent, FaultPlan, FaultVfs};

/// Domain separator for the fixture's chain digests.
const CHAIN_DOMAIN: &[u8] = b"fgdb.sim.fixture.chain.v1";

/// Keep the exported fixture bounded: it is a deterministic verification
/// workload, not a general allocation API. The upper bound is deliberately
/// above the foundation virtual-TCP window so tests can force real partial
/// writes/backpressure without making an accidental configuration an OOM.
pub const MAX_FIXTURE_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Maximum number of actions in one exported fixture workload.
///
/// The fixture is verification infrastructure, not a bulk-ingest API. A
/// fixed bound keeps canonical decoding and later workload minimization
/// finite even when bytes originate in a persisted failure artifact.
pub const MAX_FIXTURE_WORKLOAD_ACTIONS: usize = 4_096;

/// Maximum aggregate payload bytes in one exported fixture workload.
pub const MAX_FIXTURE_WORKLOAD_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Maximum pacing delay encoded by one fixture action (60 seconds).
pub const MAX_FIXTURE_ACTION_DELAY_NANOS: u64 = 60 * 1_000_000_000;

/// Maximum aggregate pacing delay in one fixture workload (one hour).
pub const MAX_FIXTURE_WORKLOAD_DELAY_NANOS: u64 = 60 * MAX_FIXTURE_ACTION_DELAY_NANOS;

const FIXTURE_WORKLOAD_MAGIC: &[u8; 8] = b"FGDBFWL1";
const FIXTURE_WORKLOAD_DIGEST_DOMAIN: &[u8] = b"fgdb.sim.fixture.workload.v1";
const FIXTURE_WORKLOAD_HEADER_BYTES: usize = 8 + 8 + 4;
const FIXTURE_ACTION_HEADER_BYTES: usize = 4 + 8 + 4;

/// Process-global tick used by [`FixtureConfig::entropy_probe`]. Each fixture
/// run that has the probe enabled consumes one tick, so two same-seed runs in
/// one process observe different values — a nondeterminism source the
/// determinism gate MUST catch. This is the control that can fire: a gate that
/// never sees it fire has not been shown to measure anything.
static ENTROPY_PROBE_TICKS: AtomicU64 = AtomicU64::new(0);

/// Configuration for one fixture run.
#[derive(Clone, Debug)]
pub struct FixtureConfig {
    /// Seed for the payload stream (and, at the driver layer, the scheduler).
    pub seed: u64,
    /// How many records the producer emits. Must not exceed
    /// [`MAX_FIXTURE_WORKLOAD_ACTIONS`].
    pub rounds: u32,
    /// Producer pacing between records; virtual under the lab. One action and
    /// the aggregate workload must remain within the exported delay bounds.
    pub tick: Duration,
    /// Deliberately mix process-global state into the first payload so the
    /// run is NOT deterministic. Exists so tests can prove the determinism
    /// gate fires; never enable it outside a control.
    pub entropy_probe: bool,
    /// Deterministic lab-VFS chaos plan used by the producer's durable leg.
    pub fault_plan: FaultPlan,
    /// Bytes per record. Values above the virtual-TCP window exercise real
    /// partial-write/backpressure behavior. Must be in
    /// `1..=MAX_FIXTURE_PAYLOAD_BYTES`; `rounds * payload_bytes` must not
    /// exceed [`MAX_FIXTURE_WORKLOAD_PAYLOAD_BYTES`].
    pub payload_bytes: usize,
}

impl FixtureConfig {
    /// A small, fast configuration: 6 records at 2ms ticks.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            rounds: 6,
            tick: Duration::from_millis(2),
            entropy_probe: false,
            fault_plan: FaultPlan {
                seed,
                ..FaultPlan::faultless()
            },
            payload_bytes: 24,
        }
    }
}

/// Caller-owned admission limits for a canonical fixture workload.
///
/// These limits are checked before allocation and while reading payloads. A
/// persisted artifact must not choose the amount of decoder work it receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureWorkloadDecodeLimits {
    /// Maximum complete encoded byte length.
    pub max_encoded_bytes: usize,
    /// Maximum number of actions.
    pub max_actions: usize,
    /// Maximum aggregate payload bytes.
    pub max_payload_bytes: usize,
    /// Maximum pacing delay for one action.
    pub max_action_delay_nanos: u64,
    /// Maximum checked sum of every action delay.
    pub max_total_delay_nanos: u64,
}

impl Default for FixtureWorkloadDecodeLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: FIXTURE_WORKLOAD_HEADER_BYTES
                + MAX_FIXTURE_WORKLOAD_ACTIONS * FIXTURE_ACTION_HEADER_BYTES
                + MAX_FIXTURE_WORKLOAD_PAYLOAD_BYTES,
            max_actions: MAX_FIXTURE_WORKLOAD_ACTIONS,
            max_payload_bytes: MAX_FIXTURE_WORKLOAD_PAYLOAD_BYTES,
            max_action_delay_nanos: MAX_FIXTURE_ACTION_DELAY_NANOS,
            max_total_delay_nanos: MAX_FIXTURE_WORKLOAD_DELAY_NANOS,
        }
    }
}

/// Why fixture workload construction or canonical decoding failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixtureWorkloadError {
    /// The magic/version prefix is not the one codec this build accepts.
    WrongMagic,
    /// The byte stream ended before a declared field or payload did.
    Truncated,
    /// Canonical bytes had data after the declared action sequence.
    TrailingBytes,
    /// The complete byte string exceeds the caller's admission limit.
    EncodedBytesExceeded { actual: usize, limit: usize },
    /// The action count exceeds either the fixture or caller bound.
    ActionCountExceeded { actual: usize, limit: usize },
    /// Aggregate payload bytes exceed either the fixture or caller bound.
    PayloadBytesExceeded { actual: usize, limit: usize },
    /// One action's pacing delay exceeds either the fixture or caller bound.
    ActionDelayExceeded {
        action: u32,
        actual: u64,
        limit: u64,
    },
    /// The checked sum of action delays exceeds the admitted bound.
    TotalDelayExceeded { actual: u64, limit: u64 },
    /// An action payload is empty or exceeds the per-action fixture bound.
    InvalidPayloadLength { action: u32, length: usize },
    /// Action ordinals must be exactly `0..count`, without gaps or reorder.
    NonContiguousAction { expected: u32, actual: u32 },
    /// A duration or size cannot be represented by the stable codec.
    IntegerOverflow,
    /// The decoder could not reserve its bounded action inventory.
    AllocationRefused,
    /// An explicit workload was paired with a different fixture seed.
    SeedMismatch { config: u64, workload: u64 },
}

impl core::fmt::Display for FixtureWorkloadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::WrongMagic => f.write_str("wrong fixture workload magic/version"),
            Self::Truncated => f.write_str("truncated fixture workload"),
            Self::TrailingBytes => f.write_str("trailing fixture workload bytes"),
            Self::EncodedBytesExceeded { actual, limit } => {
                write!(f, "fixture workload bytes {actual} exceed limit {limit}")
            }
            Self::ActionCountExceeded { actual, limit } => {
                write!(f, "fixture action count {actual} exceeds limit {limit}")
            }
            Self::PayloadBytesExceeded { actual, limit } => {
                write!(f, "fixture payload bytes {actual} exceed limit {limit}")
            }
            Self::ActionDelayExceeded {
                action,
                actual,
                limit,
            } => write!(
                f,
                "fixture action {action} delay {actual}ns exceeds limit {limit}ns"
            ),
            Self::TotalDelayExceeded { actual, limit } => {
                write!(f, "fixture total delay {actual}ns exceeds limit {limit}ns")
            }
            Self::InvalidPayloadLength { action, length } => {
                write!(
                    f,
                    "fixture action {action} has invalid payload length {length}"
                )
            }
            Self::NonContiguousAction { expected, actual } => write!(
                f,
                "fixture action ordinal {actual} does not match expected {expected}"
            ),
            Self::IntegerOverflow => f.write_str("fixture workload integer overflow"),
            Self::AllocationRefused => f.write_str("fixture workload allocation refused"),
            Self::SeedMismatch { config, workload } => write!(
                f,
                "fixture config seed {config:#x} does not match workload seed {workload:#x}"
            ),
        }
    }
}

impl std::error::Error for FixtureWorkloadError {}

/// Stable execution stage for one fixture task I/O failure.
///
/// This is deliberately smaller than an `io::Error`: persisted shrink
/// decisions compare the operation and [`io::ErrorKind`], never unstable OS
/// prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureTaskStage {
    /// Make one producer payload durable through the fault VFS.
    DurableWrite,
    /// Send a producer frame length.
    FrameHeaderWrite,
    /// Send a producer frame payload.
    FrameBodyWrite,
    /// Send the zero-length end-of-stream marker.
    TerminatorWrite,
    /// Read a consumer frame length.
    FrameHeaderRead,
    /// Read a consumer frame payload.
    FrameBodyRead,
}

/// Typed failure returned by an executing fixture component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureTaskError {
    stage: FixtureTaskStage,
    action: Option<u32>,
    kind: io::ErrorKind,
}

impl FixtureTaskError {
    const fn new(stage: FixtureTaskStage, action: Option<u32>, kind: io::ErrorKind) -> Self {
        Self {
            stage,
            action,
            kind,
        }
    }

    /// Operation that failed.
    #[must_use]
    pub const fn stage(self) -> FixtureTaskStage {
        self.stage
    }

    /// Workload action being executed, or `None` for the terminator.
    #[must_use]
    pub const fn action(self) -> Option<u32> {
        self.action
    }

    /// Stable I/O category, excluding platform-specific message text.
    #[must_use]
    pub const fn kind(self) -> io::ErrorKind {
        self.kind
    }
}

impl core::fmt::Display for FixtureTaskError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "fixture {:?} failed at action {:?}: {:?}",
            self.stage, self.action, self.kind
        )
    }
}

impl std::error::Error for FixtureTaskError {}

/// One immutable producer action in the exported fixture workload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureWorkloadAction {
    ordinal: u32,
    delay_nanos: u64,
    payload: Arc<[u8]>,
}

impl FixtureWorkloadAction {
    /// Stable zero-based action ordinal.
    #[must_use]
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// Pacing delay before this action executes.
    #[must_use]
    pub const fn delay_nanos(&self) -> u64 {
        self.delay_nanos
    }

    /// Exact payload made durable and sent by this action.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Versioned, canonical workload executed by the exported fixture.
///
/// Payload generation happens once, before either runtime starts. The
/// producer consumes these exact actions; it does not regenerate equivalent-
/// looking data from configuration while it runs. This makes workload
/// identity retainable and provides a real input for future minimization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureWorkload {
    seed: u64,
    actions: Arc<[FixtureWorkloadAction]>,
}

impl FixtureWorkload {
    /// Materializes the deterministic action stream described by `cfg`.
    pub fn try_from_config(cfg: &FixtureConfig) -> Result<Self, FixtureWorkloadError> {
        let action_count =
            usize::try_from(cfg.rounds).map_err(|_| FixtureWorkloadError::IntegerOverflow)?;
        if action_count > MAX_FIXTURE_WORKLOAD_ACTIONS {
            return Err(FixtureWorkloadError::ActionCountExceeded {
                actual: action_count,
                limit: MAX_FIXTURE_WORKLOAD_ACTIONS,
            });
        }
        if !(1..=MAX_FIXTURE_PAYLOAD_BYTES).contains(&cfg.payload_bytes) {
            return Err(FixtureWorkloadError::InvalidPayloadLength {
                action: 0,
                length: cfg.payload_bytes,
            });
        }
        let total_payload_bytes = action_count
            .checked_mul(cfg.payload_bytes)
            .ok_or(FixtureWorkloadError::IntegerOverflow)?;
        if total_payload_bytes > MAX_FIXTURE_WORKLOAD_PAYLOAD_BYTES {
            return Err(FixtureWorkloadError::PayloadBytesExceeded {
                actual: total_payload_bytes,
                limit: MAX_FIXTURE_WORKLOAD_PAYLOAD_BYTES,
            });
        }
        let delay_nanos = u64::try_from(cfg.tick.as_nanos())
            .map_err(|_| FixtureWorkloadError::IntegerOverflow)?;
        if delay_nanos > MAX_FIXTURE_ACTION_DELAY_NANOS {
            return Err(FixtureWorkloadError::ActionDelayExceeded {
                action: 0,
                actual: delay_nanos,
                limit: MAX_FIXTURE_ACTION_DELAY_NANOS,
            });
        }
        let total_delay_nanos = delay_nanos
            .checked_mul(u64::from(cfg.rounds))
            .ok_or(FixtureWorkloadError::IntegerOverflow)?;
        if total_delay_nanos > MAX_FIXTURE_WORKLOAD_DELAY_NANOS {
            return Err(FixtureWorkloadError::TotalDelayExceeded {
                actual: total_delay_nanos,
                limit: MAX_FIXTURE_WORKLOAD_DELAY_NANOS,
            });
        }
        let mut actions = Vec::new();
        actions
            .try_reserve_exact(action_count)
            .map_err(|_| FixtureWorkloadError::AllocationRefused)?;
        let mut rng = cfg.seed;
        for ordinal in 0..cfg.rounds {
            let mut payload = Vec::new();
            payload
                .try_reserve_exact(cfg.payload_bytes)
                .map_err(|_| FixtureWorkloadError::AllocationRefused)?;
            payload.resize(cfg.payload_bytes, 0);
            for (chunk_index, chunk) in payload.chunks_mut(8).enumerate() {
                let mut value = split_mix_next(&mut rng);
                if cfg.entropy_probe && ordinal == 0 && chunk_index == 0 {
                    value ^= ENTROPY_PROBE_TICKS
                        .fetch_add(1, Ordering::SeqCst)
                        .wrapping_add(1);
                }
                let bytes = value.to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            actions.push(FixtureWorkloadAction {
                ordinal,
                delay_nanos,
                payload: payload.into(),
            });
        }
        Ok(Self {
            seed: cfg.seed,
            actions: actions.into(),
        })
    }

    /// Strictly decodes one canonical version-1 workload under caller limits.
    pub fn try_from_canonical_bytes(
        bytes: &[u8],
        limits: FixtureWorkloadDecodeLimits,
    ) -> Result<Self, FixtureWorkloadError> {
        if bytes.len() > limits.max_encoded_bytes {
            return Err(FixtureWorkloadError::EncodedBytesExceeded {
                actual: bytes.len(),
                limit: limits.max_encoded_bytes,
            });
        }
        let mut cursor = 0usize;
        let magic = take_workload_bytes(bytes, &mut cursor, FIXTURE_WORKLOAD_MAGIC.len())?;
        if magic != FIXTURE_WORKLOAD_MAGIC {
            return Err(FixtureWorkloadError::WrongMagic);
        }
        let seed = read_workload_u64(bytes, &mut cursor)?;
        let action_count = usize::try_from(read_workload_u32(bytes, &mut cursor)?)
            .map_err(|_| FixtureWorkloadError::IntegerOverflow)?;
        let action_limit = limits.max_actions.min(MAX_FIXTURE_WORKLOAD_ACTIONS);
        if action_count > action_limit {
            return Err(FixtureWorkloadError::ActionCountExceeded {
                actual: action_count,
                limit: action_limit,
            });
        }
        let payload_limit = limits
            .max_payload_bytes
            .min(MAX_FIXTURE_WORKLOAD_PAYLOAD_BYTES);
        let action_delay_limit = limits
            .max_action_delay_nanos
            .min(MAX_FIXTURE_ACTION_DELAY_NANOS);
        let total_delay_limit = limits
            .max_total_delay_nanos
            .min(MAX_FIXTURE_WORKLOAD_DELAY_NANOS);
        let mut actions = Vec::new();
        actions
            .try_reserve_exact(action_count)
            .map_err(|_| FixtureWorkloadError::AllocationRefused)?;
        let mut total_payload_bytes = 0usize;
        let mut total_delay_nanos = 0u64;
        for expected_index in 0..action_count {
            let expected =
                u32::try_from(expected_index).map_err(|_| FixtureWorkloadError::IntegerOverflow)?;
            let actual = read_workload_u32(bytes, &mut cursor)?;
            if actual != expected {
                return Err(FixtureWorkloadError::NonContiguousAction { expected, actual });
            }
            let delay_nanos = read_workload_u64(bytes, &mut cursor)?;
            if delay_nanos > action_delay_limit {
                return Err(FixtureWorkloadError::ActionDelayExceeded {
                    action: actual,
                    actual: delay_nanos,
                    limit: action_delay_limit,
                });
            }
            total_delay_nanos = total_delay_nanos
                .checked_add(delay_nanos)
                .ok_or(FixtureWorkloadError::IntegerOverflow)?;
            if total_delay_nanos > total_delay_limit {
                return Err(FixtureWorkloadError::TotalDelayExceeded {
                    actual: total_delay_nanos,
                    limit: total_delay_limit,
                });
            }
            let payload_len = usize::try_from(read_workload_u32(bytes, &mut cursor)?)
                .map_err(|_| FixtureWorkloadError::IntegerOverflow)?;
            if !(1..=MAX_FIXTURE_PAYLOAD_BYTES).contains(&payload_len) {
                return Err(FixtureWorkloadError::InvalidPayloadLength {
                    action: actual,
                    length: payload_len,
                });
            }
            total_payload_bytes = total_payload_bytes
                .checked_add(payload_len)
                .ok_or(FixtureWorkloadError::IntegerOverflow)?;
            if total_payload_bytes > payload_limit {
                return Err(FixtureWorkloadError::PayloadBytesExceeded {
                    actual: total_payload_bytes,
                    limit: payload_limit,
                });
            }
            let payload_bytes = take_workload_bytes(bytes, &mut cursor, payload_len)?;
            let mut payload = Vec::new();
            payload
                .try_reserve_exact(payload_len)
                .map_err(|_| FixtureWorkloadError::AllocationRefused)?;
            payload.extend_from_slice(payload_bytes);
            actions.push(FixtureWorkloadAction {
                ordinal: actual,
                delay_nanos,
                payload: payload.into(),
            });
        }
        if cursor != bytes.len() {
            return Err(FixtureWorkloadError::TrailingBytes);
        }
        Ok(Self {
            seed,
            actions: actions.into(),
        })
    }

    /// Stable seed that initializes both producer and consumer digest chains.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Exact ordered action sequence.
    #[must_use]
    pub fn actions(&self) -> &[FixtureWorkloadAction] {
        &self.actions
    }

    /// Strict canonical version-1 bytes.
    #[must_use]
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        let payload_bytes = self
            .actions
            .iter()
            .map(|action| action.payload.len())
            .sum::<usize>();
        let capacity = FIXTURE_WORKLOAD_HEADER_BYTES
            + self.actions.len() * FIXTURE_ACTION_HEADER_BYTES
            + payload_bytes;
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(FIXTURE_WORKLOAD_MAGIC);
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(self.actions.len())
                .expect("validated fixture action count fits u32")
                .to_le_bytes(),
        );
        for action in self.actions.iter() {
            out.extend_from_slice(&action.ordinal.to_le_bytes());
            out.extend_from_slice(&action.delay_nanos.to_le_bytes());
            out.extend_from_slice(
                &u32::try_from(action.payload.len())
                    .expect("validated fixture payload length fits u32")
                    .to_le_bytes(),
            );
            out.extend_from_slice(&action.payload);
        }
        out
    }

    /// Domain-separated digest of the strict canonical bytes.
    #[must_use]
    pub fn canonical_digest_hex(&self) -> String {
        let mut hasher = Hasher::new();
        hasher.update(FIXTURE_WORKLOAD_DIGEST_DOMAIN);
        hasher.update(&self.to_canonical_bytes());
        hasher.finalize().to_hex()
    }

    /// Rebuilds a canonical workload from a retained action subsequence.
    ///
    /// Action ordinals are identities within one encoded workload, so a
    /// shrink candidate is re-numbered to `0..len` rather than serializing a
    /// gap. The action fields are private; callers cannot synthesize an
    /// unvalidated action through this seam.
    pub(crate) fn try_from_retained_actions(
        seed: u64,
        retained: &[FixtureWorkloadAction],
    ) -> Result<Self, FixtureWorkloadError> {
        if retained.len() > MAX_FIXTURE_WORKLOAD_ACTIONS {
            return Err(FixtureWorkloadError::ActionCountExceeded {
                actual: retained.len(),
                limit: MAX_FIXTURE_WORKLOAD_ACTIONS,
            });
        }
        let mut total_payload_bytes = 0usize;
        let mut total_delay_nanos = 0u64;
        let mut actions = Vec::new();
        actions
            .try_reserve_exact(retained.len())
            .map_err(|_| FixtureWorkloadError::AllocationRefused)?;
        for (index, action) in retained.iter().enumerate() {
            total_payload_bytes = total_payload_bytes
                .checked_add(action.payload.len())
                .ok_or(FixtureWorkloadError::IntegerOverflow)?;
            if total_payload_bytes > MAX_FIXTURE_WORKLOAD_PAYLOAD_BYTES {
                return Err(FixtureWorkloadError::PayloadBytesExceeded {
                    actual: total_payload_bytes,
                    limit: MAX_FIXTURE_WORKLOAD_PAYLOAD_BYTES,
                });
            }
            total_delay_nanos = total_delay_nanos
                .checked_add(action.delay_nanos)
                .ok_or(FixtureWorkloadError::IntegerOverflow)?;
            if total_delay_nanos > MAX_FIXTURE_WORKLOAD_DELAY_NANOS {
                return Err(FixtureWorkloadError::TotalDelayExceeded {
                    actual: total_delay_nanos,
                    limit: MAX_FIXTURE_WORKLOAD_DELAY_NANOS,
                });
            }
            actions.push(FixtureWorkloadAction {
                ordinal: u32::try_from(index).map_err(|_| FixtureWorkloadError::IntegerOverflow)?,
                delay_nanos: action.delay_nanos,
                payload: Arc::clone(&action.payload),
            });
        }
        Ok(Self {
            seed,
            actions: actions.into(),
        })
    }

    fn validate_seed(&self, cfg: &FixtureConfig) -> Result<(), FixtureWorkloadError> {
        if self.seed != cfg.seed {
            return Err(FixtureWorkloadError::SeedMismatch {
                config: cfg.seed,
                workload: self.seed,
            });
        }
        Ok(())
    }
}

fn take_workload_bytes<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], FixtureWorkloadError> {
    let end = cursor
        .checked_add(length)
        .ok_or(FixtureWorkloadError::IntegerOverflow)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or(FixtureWorkloadError::Truncated)?;
    *cursor = end;
    Ok(value)
}

fn read_workload_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, FixtureWorkloadError> {
    let raw: [u8; 4] = take_workload_bytes(bytes, cursor, 4)?
        .try_into()
        .map_err(|_| FixtureWorkloadError::Truncated)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_workload_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, FixtureWorkloadError> {
    let raw: [u8; 8] = take_workload_bytes(bytes, cursor, 8)?
        .try_into()
        .map_err(|_| FixtureWorkloadError::Truncated)?;
    Ok(u64::from_le_bytes(raw))
}

/// Which component recorded an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Component {
    /// The paced writer: sleep, durable write, send.
    Producer = 0,
    /// The reader: receive, apply.
    Consumer = 1,
}

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EventKind {
    /// Producer finished its pacing sleep for this round.
    Slept = 0,
    /// Producer's payload survived `sync_all` through the lab VFS.
    Durable = 1,
    /// Producer wrote the framed payload to the network.
    Sent = 2,
    /// Consumer applied a payload to its chain.
    Applied = 3,
    /// The component finished.
    Terminated = 4,
}

/// One fixed-width trace entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureEvent {
    /// Observability clock at record time: virtual nanoseconds under the lab,
    /// wall nanoseconds under live. Part of the byte trace on purpose — the
    /// determinism gate must cover virtual TIME, not just payload bytes.
    pub now_nanos: u64,
    /// Round (producer) or apply index (consumer).
    pub round: u32,
    /// Who recorded it.
    pub component: Component,
    /// What it was.
    pub kind: EventKind,
    /// Low 8 bytes of the recording component's running chain digest at this
    /// point, so a divergence pinpoints WHERE the streams departed.
    pub chain: u64,
}

/// Serialized size of one event in [`TraceHandle::to_bytes`].
pub const EVENT_SIZE: usize = 8 + 4 + 1 + 1 + 8;

/// Serialized header size: version, seed, event count.
pub const HEADER_SIZE: usize = 4 + 8 + 4;

struct TraceInner {
    events: Mutex<Vec<FixtureEvent>>,
    producer_chain: Mutex<Option<Digest>>,
    consumer_chain: Mutex<Option<Digest>>,
    durable_bytes: AtomicU64,
    network_backpressure_events: AtomicU64,
    fault_events: Mutex<Vec<FaultEvent>>,
    seed: u64,
}

/// Shared, append-only record of one fixture run.
#[derive(Clone)]
pub struct TraceHandle(Arc<TraceInner>);

impl TraceHandle {
    /// A fresh, empty trace for the given seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(Arc::new(TraceInner {
            events: Mutex::new(Vec::new()),
            producer_chain: Mutex::new(None),
            consumer_chain: Mutex::new(None),
            durable_bytes: AtomicU64::new(0),
            network_backpressure_events: AtomicU64::new(0),
            fault_events: Mutex::new(Vec::new()),
            seed,
        }))
    }

    fn record(&self, component: Component, kind: EventKind, round: u32, chain: &Digest) {
        let now_nanos = Cx::current()
            .map(|cx| cx.now_for_observability().as_nanos())
            .unwrap_or(0);
        let chain = u64::from_le_bytes(chain.0[..8].try_into().expect("digest has 8 bytes"));
        self.0
            .events
            .lock()
            .expect("fixture trace lock")
            .push(FixtureEvent {
                now_nanos,
                round,
                component,
                kind,
                chain,
            });
    }

    /// The events recorded so far, in global append order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<FixtureEvent> {
        self.0.events.lock().expect("fixture trace lock").clone()
    }

    fn record_faults(&self, root: &Path, mut events: Vec<FaultEvent>) {
        for event in &mut events {
            if let Ok(relative) = event.path.strip_prefix(root) {
                event.path = relative.to_path_buf();
            }
        }
        *self.0.fault_events.lock().expect("fixture fault log lock") = events;
    }

    /// Exact normalized fault-injection log from the durable fixture leg.
    #[must_use]
    pub fn fault_events(&self) -> Vec<FaultEvent> {
        self.0
            .fault_events
            .lock()
            .expect("fixture fault log lock")
            .clone()
    }

    /// Canonical byte serialization of the run: header, fixed-width events,
    /// then both final chain digests. Two same-seed lab runs must agree on
    /// every byte, timestamps included.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let events = self.snapshot();
        let mut out = Vec::with_capacity(HEADER_SIZE + events.len() * EVENT_SIZE + 64 + 8);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&self.0.seed.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(events.len())
                .expect("fixture traces are small")
                .to_le_bytes(),
        );
        for event in &events {
            out.extend_from_slice(&event.now_nanos.to_le_bytes());
            out.extend_from_slice(&event.round.to_le_bytes());
            out.push(event.component as u8);
            out.push(event.kind as u8);
            out.extend_from_slice(&event.chain.to_le_bytes());
        }
        let zero = Digest([0u8; 32]);
        let producer = self.0.producer_chain.lock().expect("producer chain lock");
        let consumer = self.0.consumer_chain.lock().expect("consumer chain lock");
        out.extend_from_slice(&producer.as_ref().unwrap_or(&zero).0);
        out.extend_from_slice(&consumer.as_ref().unwrap_or(&zero).0);
        let mut fault_hasher = Hasher::new();
        fault_hasher.update(b"fgdb.sim.fixture.faults.v1");
        for event in self.fault_events() {
            fault_hasher.update(format!("{event:?}").as_bytes());
        }
        out.extend_from_slice(&fault_hasher.finalize().0);
        out.extend_from_slice(
            &self
                .0
                .network_backpressure_events
                .load(Ordering::SeqCst)
                .to_le_bytes(),
        );
        out
    }

    /// The clock-free semantic projection the dual run compares.
    #[must_use]
    pub fn semantics(&self) -> FixtureSemantics {
        let events = self.snapshot();
        let produced = events
            .iter()
            .filter(|e| e.component == Component::Producer && e.kind == EventKind::Sent)
            .count() as i64;
        let consumed = events
            .iter()
            .filter(|e| e.component == Component::Consumer && e.kind == EventKind::Applied)
            .count() as i64;
        let producer_chain = *self.0.producer_chain.lock().expect("producer chain lock");
        let consumer_chain = *self.0.consumer_chain.lock().expect("consumer chain lock");
        FixtureSemantics {
            chain_intact: matches!((&producer_chain, &consumer_chain),
                (Some(p), Some(c)) if p.0 == c.0),
            final_digest_hex: consumer_chain.map_or_else(String::new, Digest::to_hex),
            producer_digest_hex: producer_chain.map_or_else(String::new, Digest::to_hex),
            produced,
            consumed,
            durable_bytes: self.0.durable_bytes.load(Ordering::SeqCst) as i64,
            injected_faults: self.fault_events().len() as i64,
            network_backpressure_events: self.0.network_backpressure_events.load(Ordering::SeqCst)
                as i64,
        }
    }
}

/// What a run MEANS, with every clock removed: the fields two executions on
/// different schedulers must still agree on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureSemantics {
    /// Consumer's final chain digest, hex.
    pub final_digest_hex: String,
    /// Producer's final chain digest, hex.
    pub producer_digest_hex: String,
    /// Whether the two chains match — the network-integrity claim.
    pub chain_intact: bool,
    /// Records sent by the producer.
    pub produced: i64,
    /// Records applied by the consumer.
    pub consumed: i64,
    /// Bytes acknowledged durable through the lab VFS.
    pub durable_bytes: i64,
    /// Deterministic VFS injections observed by this run.
    pub injected_faults: i64,
    /// Number of virtual-TCP writes that observed a full finite channel and
    /// yielded `Pending` until the consumer drained it.
    pub network_backpressure_events: i64,
}

/// First byte at which two serialized traces differ, with the event index it
/// falls inside when it lands in the event region. `None` when equal.
#[must_use]
pub fn first_divergence(a: &[u8], b: &[u8]) -> Option<(usize, Option<usize>)> {
    let shorter = a.len().min(b.len());
    let byte = (0..shorter)
        .find(|&i| a[i] != b[i])
        .or_else(|| (a.len() != b.len()).then_some(shorter))?;
    let event = (byte >= HEADER_SIZE).then(|| (byte - HEADER_SIZE) / EVENT_SIZE);
    Some((byte, event))
}

fn chain_seed(seed: u64) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(CHAIN_DOMAIN);
    hasher.update(&seed.to_le_bytes());
    hasher.finalize()
}

fn chain_fold(chain: &Digest, payload: &[u8]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(&chain.0);
    hasher.update(payload);
    hasher.finalize()
}

/// SplitMix64 — the same generator the lab VFS fault stream uses, so the
/// fixture adds no second RNG convention to the crate.
fn split_mix_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

async fn write_all_net(
    stream: &mut VirtualTcpStream,
    bytes: &[u8],
    trace: &TraceHandle,
) -> io::Result<()> {
    let mut written = 0usize;
    while written < bytes.len() {
        let n = poll_fn(
            |cx| match Pin::new(&mut *stream).poll_write(cx, &bytes[written..]) {
                Poll::Pending => {
                    trace
                        .0
                        .network_backpressure_events
                        .fetch_add(1, Ordering::SeqCst);
                    Poll::Pending
                }
                ready => ready,
            },
        )
        .await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "virtual stream refused bytes",
            ));
        }
        written += n;
    }
    Ok(())
}

async fn read_exact_net(stream: &mut VirtualTcpStream, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = poll_fn(|cx| {
            let mut read_buf = ReadBuf::new(&mut buf[filled..]);
            match Pin::new(&mut *stream).poll_read(cx, &mut read_buf) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(read_buf.filled().len())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "virtual stream closed mid-frame",
            ));
        }
        filled += n;
    }
    Ok(())
}

/// Writes `bytes` durably through the fault VFS, mirroring the write-and-sync
/// discipline the lab VFS tests pin down.
async fn write_durable<V: Vfs>(vfs: &FaultVfs<V>, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = vfs
        .open(
            path,
            &OpenOptions::new().write(true).create(true).truncate(true),
        )
        .await?;
    let mut written = 0usize;
    while written < bytes.len() {
        let n = poll_fn(|cx| Pin::new(&mut file).poll_write(cx, &bytes[written..])).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "vfs refused bytes",
            ));
        }
        written += n;
    }
    VfsFile::sync_all(&file).await
}

/// The producer half: execute each explicit workload action, then send the
/// zero-length terminator.
async fn producer(
    cfg: FixtureConfig,
    workload: FixtureWorkload,
    dir: PathBuf,
    mut net: VirtualTcpStream,
    trace: TraceHandle,
) -> Result<(), FixtureTaskError> {
    let cx = Cx::current().expect("fixture producer runs under a runtime");
    let vfs = FaultVfs::unix_with_clock(cfg.fault_plan, cx.clone());
    let mut chain = chain_seed(workload.seed);
    for action in workload.actions() {
        let round = action.ordinal;
        sleep(
            cx.now_for_observability(),
            Duration::from_nanos(action.delay_nanos),
        )
        .await;
        trace.record(Component::Producer, EventKind::Slept, round, &chain);

        let payload = action.payload();
        chain = chain_fold(&chain, payload);

        if let Err(error) =
            write_durable(&vfs, &dir.join(format!("record-{round:04}.bin")), payload).await
        {
            trace.record_faults(&dir, vfs.events());
            return Err(FixtureTaskError::new(
                FixtureTaskStage::DurableWrite,
                Some(round),
                error.kind(),
            ));
        }
        trace
            .0
            .durable_bytes
            .fetch_add(payload.len() as u64, Ordering::SeqCst);
        trace.record(Component::Producer, EventKind::Durable, round, &chain);

        let len = u32::try_from(payload.len()).expect("payload fits u32");
        if let Err(error) = write_all_net(&mut net, &len.to_le_bytes(), &trace).await {
            trace.record_faults(&dir, vfs.events());
            return Err(FixtureTaskError::new(
                FixtureTaskStage::FrameHeaderWrite,
                Some(round),
                error.kind(),
            ));
        }
        if let Err(error) = write_all_net(&mut net, payload, &trace).await {
            trace.record_faults(&dir, vfs.events());
            return Err(FixtureTaskError::new(
                FixtureTaskStage::FrameBodyWrite,
                Some(round),
                error.kind(),
            ));
        }
        trace.record(Component::Producer, EventKind::Sent, round, &chain);
    }
    if let Err(error) = write_all_net(&mut net, &0u32.to_le_bytes(), &trace).await {
        trace.record_faults(&dir, vfs.events());
        return Err(FixtureTaskError::new(
            FixtureTaskStage::TerminatorWrite,
            None,
            error.kind(),
        ));
    }
    trace.record_faults(&dir, vfs.events());
    *trace.0.producer_chain.lock().expect("producer chain lock") = Some(chain);
    trace.record(
        Component::Producer,
        EventKind::Terminated,
        u32::try_from(workload.actions.len()).expect("validated fixture action count fits u32"),
        &chain,
    );
    Ok(())
}

/// The consumer half: read frames until the terminator, folding each payload
/// into an independent chain.
async fn consumer(
    cfg: FixtureConfig,
    mut net: VirtualTcpStream,
    trace: TraceHandle,
) -> Result<(), FixtureTaskError> {
    let _cx = Cx::current().expect("fixture consumer runs under a runtime");
    let mut chain = chain_seed(cfg.seed);
    let mut applied = 0u32;
    loop {
        let mut len_bytes = [0u8; 4];
        if let Err(error) = read_exact_net(&mut net, &mut len_bytes).await {
            return Err(FixtureTaskError::new(
                FixtureTaskStage::FrameHeaderRead,
                Some(applied),
                error.kind(),
            ));
        }
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len == 0 {
            break;
        }
        let mut payload = vec![0u8; len];
        if let Err(error) = read_exact_net(&mut net, &mut payload).await {
            return Err(FixtureTaskError::new(
                FixtureTaskStage::FrameBodyRead,
                Some(applied),
                error.kind(),
            ));
        }
        chain = chain_fold(&chain, &payload);
        trace.record(Component::Consumer, EventKind::Applied, applied, &chain);
        applied += 1;
    }
    *trace.0.consumer_chain.lock().expect("consumer chain lock") = Some(chain);
    trace.record(Component::Consumer, EventKind::Terminated, applied, &chain);
    Ok(())
}

/// Builds one run's pair of component futures plus the trace they share.
///
/// The caller decides HOW they run: the lab driver spawns each as its own
/// task under the deterministic scheduler; the live driver polls them jointly
/// inside `block_on`. Both futures acquire their `Cx` ambiently, so they are
/// runtime-agnostic by construction.
pub fn fixture_futures(
    cfg: &FixtureConfig,
    scratch_dir: &Path,
) -> (
    impl std::future::Future<Output = Result<(), FixtureTaskError>> + Send + 'static,
    impl std::future::Future<Output = Result<(), FixtureTaskError>> + Send + 'static,
    TraceHandle,
    FixtureWorkload,
) {
    let workload = FixtureWorkload::try_from_config(cfg)
        .expect("fixture configuration must materialize a bounded workload");
    fixture_futures_for_workload(cfg, workload, scratch_dir)
        .expect("generated fixture workload must match its configuration")
}

/// Builds the fixture futures from an already materialized workload.
///
/// This is the real execution seam used by the dual-run adapter and future
/// workload minimization. The producer consumes `workload` directly; it does
/// not consult `rounds`, `tick`, `payload_bytes`, or `entropy_probe` again.
pub(crate) fn fixture_futures_for_workload(
    cfg: &FixtureConfig,
    workload: FixtureWorkload,
    scratch_dir: &Path,
) -> Result<
    (
        impl std::future::Future<Output = Result<(), FixtureTaskError>> + Send + 'static,
        impl std::future::Future<Output = Result<(), FixtureTaskError>> + Send + 'static,
        TraceHandle,
        FixtureWorkload,
    ),
    FixtureWorkloadError,
> {
    workload.validate_seed(cfg)?;
    std::fs::create_dir_all(scratch_dir).expect("fixture scratch dir");
    let trace = TraceHandle::new(cfg.seed);
    let (producer_end, consumer_end) = VirtualTcpStream::pair(
        SocketAddr::from(([127, 0, 0, 1], 9101)),
        SocketAddr::from(([127, 0, 0, 1], 9102)),
    );
    let producer_fut = producer(
        cfg.clone(),
        workload.clone(),
        scratch_dir.to_path_buf(),
        producer_end,
        trace.clone(),
    );
    let consumer_fut = consumer(cfg.clone(), consumer_end, trace.clone());
    Ok((producer_fut, consumer_fut, trace, workload))
}
