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

use crate::vfs::{FaultPlan, FaultVfs};

/// Domain separator for the fixture's chain digests.
const CHAIN_DOMAIN: &[u8] = b"fgdb.sim.fixture.chain.v1";

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
    /// How many records the producer emits.
    pub rounds: u32,
    /// Producer pacing between records; virtual under the lab.
    pub tick: Duration,
    /// Deliberately mix process-global state into the first payload so the
    /// run is NOT deterministic. Exists so tests can prove the determinism
    /// gate fires; never enable it outside a control.
    pub entropy_probe: bool,
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
        }
    }
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

    /// Canonical byte serialization of the run: header, fixed-width events,
    /// then both final chain digests. Two same-seed lab runs must agree on
    /// every byte, timestamps included.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let events = self.snapshot();
        let mut out = Vec::with_capacity(HEADER_SIZE + events.len() * EVENT_SIZE + 64);
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

async fn write_all_net(stream: &mut VirtualTcpStream, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0usize;
    while written < bytes.len() {
        let n = poll_fn(|cx| Pin::new(&mut *stream).poll_write(cx, &bytes[written..])).await?;
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

/// The producer half: per round, sleep one tick, derive the payload, make it
/// durable, frame it onto the network; then send the zero-length terminator.
async fn producer(
    cfg: FixtureConfig,
    vfs: Arc<FaultVfs>,
    dir: PathBuf,
    mut net: VirtualTcpStream,
    trace: TraceHandle,
) {
    let cx = Cx::current().expect("fixture producer runs under a runtime");
    let mut rng = cfg.seed;
    let mut chain = chain_seed(cfg.seed);
    for round in 0..cfg.rounds {
        sleep(cx.now_for_observability(), cfg.tick).await;
        trace.record(Component::Producer, EventKind::Slept, round, &chain);

        let mut payload = [0u8; 24];
        for (i, word) in payload.as_chunks_mut::<8>().0.iter_mut().enumerate() {
            let mut value = split_mix_next(&mut rng);
            if cfg.entropy_probe && round == 0 && i == 0 {
                value ^= ENTROPY_PROBE_TICKS
                    .fetch_add(1, Ordering::SeqCst)
                    .wrapping_add(1);
            }
            word.copy_from_slice(&value.to_le_bytes());
        }
        chain = chain_fold(&chain, &payload);

        write_durable(
            vfs.as_ref(),
            &dir.join(format!("record-{round:04}.bin")),
            &payload,
        )
        .await
        .expect("fixture durable write");
        trace
            .0
            .durable_bytes
            .fetch_add(payload.len() as u64, Ordering::SeqCst);
        trace.record(Component::Producer, EventKind::Durable, round, &chain);

        let len = u32::try_from(payload.len()).expect("payload fits u32");
        write_all_net(&mut net, &len.to_le_bytes())
            .await
            .expect("fixture frame header");
        write_all_net(&mut net, &payload)
            .await
            .expect("fixture frame body");
        trace.record(Component::Producer, EventKind::Sent, round, &chain);
    }
    write_all_net(&mut net, &0u32.to_le_bytes())
        .await
        .expect("fixture terminator");
    *trace.0.producer_chain.lock().expect("producer chain lock") = Some(chain);
    trace.record(
        Component::Producer,
        EventKind::Terminated,
        cfg.rounds,
        &chain,
    );
}

/// The consumer half: read frames until the terminator, folding each payload
/// into an independent chain.
async fn consumer(cfg: FixtureConfig, mut net: VirtualTcpStream, trace: TraceHandle) {
    let _cx = Cx::current().expect("fixture consumer runs under a runtime");
    let mut chain = chain_seed(cfg.seed);
    let mut applied = 0u32;
    loop {
        let mut len_bytes = [0u8; 4];
        read_exact_net(&mut net, &mut len_bytes)
            .await
            .expect("fixture frame header");
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len == 0 {
            break;
        }
        let mut payload = vec![0u8; len];
        read_exact_net(&mut net, &mut payload)
            .await
            .expect("fixture frame body");
        chain = chain_fold(&chain, &payload);
        trace.record(Component::Consumer, EventKind::Applied, applied, &chain);
        applied += 1;
    }
    *trace.0.consumer_chain.lock().expect("consumer chain lock") = Some(chain);
    trace.record(Component::Consumer, EventKind::Terminated, applied, &chain);
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
    impl std::future::Future<Output = ()> + Send + 'static,
    impl std::future::Future<Output = ()> + Send + 'static,
    TraceHandle,
) {
    std::fs::create_dir_all(scratch_dir).expect("fixture scratch dir");
    let trace = TraceHandle::new(cfg.seed);
    let (producer_end, consumer_end) = VirtualTcpStream::pair(
        SocketAddr::from(([127, 0, 0, 1], 9101)),
        SocketAddr::from(([127, 0, 0, 1], 9102)),
    );
    let vfs = Arc::new(FaultVfs::unix(FaultPlan::faultless()));
    let producer_fut = producer(
        cfg.clone(),
        vfs,
        scratch_dir.to_path_buf(),
        producer_end,
        trace.clone(),
    );
    let consumer_fut = consumer(cfg.clone(), consumer_end, trace.clone());
    (producer_fut, consumer_fut, trace)
}
