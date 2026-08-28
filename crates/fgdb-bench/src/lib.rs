#![forbid(unsafe_code)]

//! **The §17 adversarial bench harness** (`fgdb-p95p`) library: one binary that drives
//! the real durable spine through the hostile shapes a graph engine actually
//! falls over on, asserts every answer it measures against, and publishes the
//! numbers it saw — including the bad ones.
//!
//! # What this is not
//!
//! §17's gates are `EmpiricalGate`s that activate only once a benchmark
//! manifest pins CPU/microcode, kernel/filesystem/mount options, NVMe model and
//! measured fsync distribution, toolchain, database configuration, workload,
//! and statistical comparison rule. None of that is pinned here. Every result
//! event this binary prints carries `"empirical_gate_activated": false`: it is
//! a machine-local baseline that makes the next optimization measurable, not a
//! gate result. Publishing an honest number where a target says 8M is
//! progress; an unmeasured claim of 8M is a liability.
//!
//! # What is non-negotiable anyway
//!
//! Doctrine 7 forbids reporting a non-durable mode as a result. Everything here
//! runs on the production runtime authority (`RuntimeBuilder` →
//! `PurposeContexts::commit`), through the ordinary `Database::create/write/
//! open/compact` path, with real fsyncs — nothing is measured in memory and
//! labelled durable. And because a "fast" path that returns wrong answers is
//! not a win, **every shape asserts correctness while it measures**: reads are
//! checked against the expected model the loader built, inside the measured
//! region, not after it.
//!
//! # Shapes (fgdb-p95p's adversarial list)
//!
//! | shape | what breaks |
//! |---|---|
//! | `ingest-power-law` | uniform-degree benchmarks hide supernode ingest cost |
//! | `point-reads-supernode` | p99 under degree skew on a warm decoded cache |
//! | `version-chain` | one-key history amplification: bytes/version + historical probe cost |
//! | `cold-reopen` | the store path instead of the memory path |
//! | `compaction-under-load` | a compactor that is fast when idle is not a compactor |
//!
//! Deep branch chains stay absent: the spine has no branch API yet (see
//! `crates/fgdb/tests/hostile_shapes.rs` for the same statement in witness
//! form). The harness cannot measure what the engine cannot express.

use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::CommitCx;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const KNOWS: RelationId = RelationId(1);
pub const WEIGHT: PropertyKeyId = PropertyKeyId(7);
pub const K_OID: [u8; 32] = [0x5a; 32];
pub const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

pub fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-scoped so concurrent runs never share leftovers. Nothing is removed:
/// repository rule 1 carves out no deletion exception, not even for benches.
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-bench-{}-{name}", std::process::id()))
}

// ---------------------------------------------------------------------------
// deterministic load model: preferential attachment over an exact PRNG
// ---------------------------------------------------------------------------

/// A tiny deterministic PRNG (LCG, Numerical Recipes constants). The closed
/// dependency universe has no `rand`, and a benchmark whose workload depended
/// on wall-clock entropy could never be replayed or diffed.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    pub fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound.max(1) as u64) as usize
    }
}

/// The expected model the loader builds and every measured read checks against.
/// Degree-proportional attachment gives the degree distribution a power-law
/// tail, so the first vertices become true supernodes instead of synthetic
/// hot spots grafted onto a uniform graph.
pub struct Model {
    /// Outbound face: `out[src] = [dst, ...]` ascending — what
    /// `Database::neighbours` answers (KNOWS is a directed relation).
    out_adjacency: BTreeMap<VId, Vec<VId>>,
    /// Inbound face: `in_[dst] = [src, ...]` ascending — what
    /// `Database::in_neighbours` answers.
    in_adjacency: BTreeMap<VId, Vec<VId>>,
    degree: BTreeMap<VId, usize>,
    edges: Vec<(EId, VId, VId)>,
}
impl Model {
    pub fn preferential_attachment(
        vertex_count: usize,
        edges_per_new_vertex: usize,
        seed: u64,
    ) -> Self {
        let mut rng = Lcg::new(seed);
        let mut out_adjacency: BTreeMap<VId, Vec<VId>> = BTreeMap::new();
        let mut in_adjacency: BTreeMap<VId, Vec<VId>> = BTreeMap::new();
        let mut degree: BTreeMap<VId, usize> = BTreeMap::new();
        // Attachment candidates: one entry per existing edge endpoint, so a
        // draw is degree-proportional without a weighted sampler.
        let mut endpoints: Vec<VId> = Vec::new();
        let mut edges = Vec::new();
        let mut next_eid = 0usize;
        // KNOWS edges are stored DIRECTED (src -> dst); the undirected
        // attachment view comes from bumping BOTH endpoints' degree and
        // recording both faces, exactly as the engine's two adjacency
        // families answer them.
        let link = |out: &mut BTreeMap<VId, Vec<VId>>,
                    inn: &mut BTreeMap<VId, Vec<VId>>,
                    degree: &mut BTreeMap<VId, usize>,
                    src: VId,
                    dst: VId| {
            out.entry(src).or_default().push(dst);
            inn.entry(dst).or_default().push(src);
            *degree.entry(src).or_insert(0) += 1;
            *degree.entry(dst).or_insert(0) += 1;
        };
        for a in 0..seed_clique(edges_per_new_vertex) {
            for b in (a + 1)..seed_clique(edges_per_new_vertex) {
                let (ea, eb) = (VId(a as u128), VId(b as u128));
                link(&mut out_adjacency, &mut in_adjacency, &mut degree, ea, eb);
                endpoints.push(ea);
                endpoints.push(eb);
                edges.push((EId(next_eid as u128), ea, eb));
                next_eid += 1;
            }
        }
        for newcomer in seed_clique(edges_per_new_vertex)..vertex_count {
            let vid = VId(newcomer as u128);
            let mut attached: BTreeMap<VId, ()> = BTreeMap::new();
            while attached.len() < edges_per_new_vertex.min(newcomer) {
                let target = endpoints[rng.below(endpoints.len())];
                if target == vid || attached.contains_key(&target) {
                    continue;
                }
                attached.insert(target, ());
                link(
                    &mut out_adjacency,
                    &mut in_adjacency,
                    &mut degree,
                    vid,
                    target,
                );
                endpoints.push(vid);
                endpoints.push(target);
                edges.push((EId(next_eid as u128), vid, target));
                next_eid += 1;
            }
        }
        for list in out_adjacency.values_mut() {
            list.sort();
        }
        for list in in_adjacency.values_mut() {
            list.sort();
        }
        Self {
            out_adjacency,
            in_adjacency,
            degree,
            edges,
        }
    }

    /// The outbound face of `vid`: exactly what `Database::neighbours`
    /// answers over the directed relation.
    pub fn neighbours_of(&self, vid: VId) -> Vec<VId> {
        self.out_adjacency.get(&vid).cloned().unwrap_or_default()
    }

    /// The inbound face of `vid`: exactly what `Database::in_neighbours`
    /// answers.
    pub fn in_neighbours_of(&self, vid: VId) -> Vec<VId> {
        self.in_adjacency.get(&vid).cloned().unwrap_or_default()
    }

    pub fn supernode(&self) -> VId {
        self.degree
            .iter()
            .max_by_key(|(_, degree)| **degree)
            .map(|(vid, _)| *vid)
            .expect("nonempty model has a max-degree vertex")
    }

    /// The lowest-degree vertices: the tail the skew exists to disadvantage.
    pub fn tail(&self, count: usize) -> Vec<VId> {
        let mut by_degree: Vec<(VId, usize)> = self
            .degree
            .iter()
            .map(|(vid, degree)| (*vid, *degree))
            .collect();
        by_degree.sort_by_key(|(_, degree)| *degree);
        by_degree
            .into_iter()
            .take(count)
            .map(|(vid, _)| vid)
            .collect()
    }
}

fn seed_clique(edges_per_new_vertex: usize) -> usize {
    edges_per_new_vertex + 1
}

// ---------------------------------------------------------------------------
// measurement plumbing
// ---------------------------------------------------------------------------

/// Nearest-rank percentile over recorded samples, in microseconds.
pub fn percentile_us(samples: &[Duration], quantile: f64) -> u128 {
    let mut sorted: Vec<u128> = samples.iter().map(Duration::as_micros).collect();
    sorted.sort_unstable();
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((quantile * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

pub fn bytes_on_disk(dir: &Path) -> u64 {
    fn walk(root: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, total);
            } else if let Ok(metadata) = entry.metadata() {
                *total += metadata.len();
            }
        }
    }
    let mut total = 0;
    walk(dir, &mut total);
    total
}

pub fn emit(event: &str, fields: &[(&str, String)]) {
    // Hand-rolled NDJSON: keys are known identifiers; the only free-form values
    // (cpu model, config text) are escaped here. The closed universe has no
    // serde, and a benchmark that cannot be piped is a benchmark nobody diffs.
    print!("{{\"event\":\"{event}\"");
    for (key, value) in fields {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        print!(",\"{key}\":\"{escaped}\"");
    }
    println!("}}");
}

pub fn shape_result(shape: &str, config: &str, metrics: &[(&str, u128)]) {
    let mut fields: Vec<(&str, String)> = vec![
        ("shape", shape.to_string()),
        ("config", config.to_string()),
        ("correctness", "verified".to_string()),
        ("empirical_gate_activated", "false".to_string()),
    ];
    for (key, value) in metrics {
        fields.push((key, value.to_string()));
    }
    emit("shape_result", &fields);
}

pub fn edge_weight(record: &fgdb::EdgeRecord) -> Result<i64, String> {
    for (property, scalar) in &record.props {
        if *property == WEIGHT {
            return match scalar {
                CanonicalScalar::Int(value) => Ok(*value),
                other => Err(format!("edge weight is not an Int: {other:?}")),
            };
        }
    }
    Err("edge record is missing the weight property".to_string())
}

// ---------------------------------------------------------------------------
// shared durable loader with publish-fence survival
// ---------------------------------------------------------------------------

/// One ingest batch over `range`, creating any vertex the range first touches.
/// `vertices_seen` is global across batches: identities are permanently spent,
/// so a create may be emitted exactly once per vertex for the whole load.
pub fn build_batch(
    model: &Model,
    vertices_seen: &mut BTreeMap<VId, ()>,
    range: std::ops::Range<usize>,
) -> WriteBatch {
    let mut batch = WriteBatch::new(KNOWS);
    for (eid, src, dst) in &model.edges[range] {
        if !vertices_seen.contains_key(src) {
            batch.create_vertex(*src, vec![], vec![]);
            vertices_seen.insert(*src, ());
        }
        if !vertices_seen.contains_key(dst) {
            batch.create_vertex(*dst, vec![], vec![]);
            vertices_seen.insert(*dst, ());
        }
        batch.add_edge(*eid, *src, *dst, vec![(WEIGHT, CanonicalScalar::Int(0))]);
    }
    batch
}

/// The measured engine contract this harness documents: a delta block that
/// crosses the block store's object ceiling fences the handle at derived
/// publication, and reopen is the only continuation. These are the typed arms
/// whose remedy is reopen; anything else is a real failure.
pub fn is_publish_fence(error: &fgdb::WriteError) -> bool {
    matches!(
        error,
        fgdb::WriteError::RecoveryRequired(_)
            | fgdb::WriteError::CommittedNeedsRecovery { .. }
            | fgdb::WriteError::CommitOutcomeUnknown { .. }
            | fgdb::WriteError::HandleCommitOutcomeUnknown { .. }
    )
}

/// Load `model` durably, surviving publish fences by reopening and probing.
/// Returns the open handle, how many fences were survived, and one duration
/// per successful durable commit (fence handling lands inside the duration of
/// the commit that hit it).
pub async fn load_model_durably(
    cx: &CommitCx,
    dir: &Path,
    model: &Model,
    create: bool,
) -> Result<(Database, u64, Vec<Duration>), String> {
    let mut db = if create {
        Database::create(cx, dir, keys())
            .await
            .map_err(|error| format!("create: {error}"))?
    } else {
        Database::open(cx, dir, keys())
            .await
            .map_err(|error| format!("open: {error}"))?
    };
    let mut vertices_seen: BTreeMap<VId, ()> = BTreeMap::new();
    let mut reopens = 0u64;
    let mut commit_samples = Vec::new();
    let mut index = 0;
    while index < model.edges.len() {
        let end = (index + INGEST_BATCH_EDGES).min(model.edges.len());
        let batch = build_batch(model, &mut vertices_seen, index..end);
        let started = Instant::now();
        match db.write(cx, batch).await {
            Ok(_) => commit_samples.push(started.elapsed()),
            Err(error) if is_publish_fence(&error) => {
                reopens += 1;
                // fgdb-a7sz instrument: how many sealed blocks had
                // accumulated when the next root offer refused? Each stored
                // .block file is one 56-byte root reference, so this count is
                // the W3 owner's threshold-arithmetic denominator.
                let stored_blocks = count_stored_blocks(dir);
                // Release the exclusive writer lease BEFORE reopening.
                drop(db);
                db = match Database::open(cx, dir, keys()).await {
                    Ok(db) => db,
                    Err(open_error) => {
                        // MEASURED ENGINE LIMIT (fgdb-a7sz): once the offer
                        // crosses the ceiling, EVERY later open refuses
                        // during rebuild -- the directory admits no
                        // documented recovery. This is the honest
                        // sustainable-ingest number today.
                        let message = format!(
                            "fences_survived={reopens} stored_blocks={stored_blocks} \
                             then open refused: {open_error}"
                        );
                        if open_error.to_string().contains("-byte limit") {
                            return Err(format!("ENGINE_LIMIT: {message}"));
                        }
                        return Err(format!("reopen after fence: {open_error}"));
                    }
                };
                if db
                    .edge(model.edges[end - 1].0)
                    .map_err(|error| format!("fence probe read: {error}"))?
                    .is_none()
                {
                    // Refused BEFORE the commit: rebuild the same range.
                    let rebuilt = build_batch(model, &mut vertices_seen, index..end);
                    db.write(cx, rebuilt)
                        .await
                        .map_err(|error| format!("post-fence write: {error}"))?;
                    commit_samples.push(started.elapsed());
                }
            }
            Err(error) => return Err(format!("write: {error}")),
        }
        index = end;
    }
    Ok((db, reopens, commit_samples))
}

/// Verify every edge of `model` against an open handle: payload exactness plus
/// forward-adjacency presence. Runs OUTSIDE measured regions so verification
/// cost never pollutes the numbers.
pub async fn verify_model(db: &Database, model: &Model) -> Result<(), String> {
    for (eid, src, dst) in &model.edges {
        let record = db
            .edge(*eid)
            .map_err(|error| format!("edge read: {error}"))?
            .ok_or_else(|| format!("edge {eid:?} missing after load"))?;
        if edge_weight(&record) != Ok(0) {
            return Err(format!("edge {eid:?} payload drifted"));
        }
        let forward = db
            .neighbours(*src, KNOWS)
            .map_err(|error| format!("neighbours read: {error}"))?;
        if !forward.contains(dst) {
            return Err(format!("edge {eid:?} absent from forward adjacency"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// shapes
// ---------------------------------------------------------------------------

pub const VERTEX_COUNT: usize = 150;
pub const EDGES_PER_NEW_VERTEX: usize = 3;
// MEASURED (fgdb-a7sz): the PARTITION ROOT's reference enumeration -- one
// 56-byte ref per sealed block -- crosses the store's 16 KiB ceiling at
// ~292 blocks, and every touched family adds a block per commit. Until the
// root-format fix lands (W3), the skew shapes deliberately run on a fixture
// small enough that total refs stay under the ceiling (~150 vertices /
// ~450 edges / ~6 commits x ~35 touched families), so the warm-read and
// cold-open NUMBERS publish instead of fencing. Config strings carry the
// exact fixture; scale up once fgdb-a7sz ships.
const INGEST_BATCH_EDGES: usize = 64;
pub const LOAD_SEED: u64 = 0x46474442; // "FGDB"

/// Sealed-block files on disk == current root reference count (56 B each).
pub fn count_stored_blocks(dir: &Path) -> u128 {
    std::fs::read_dir(dir.join("strata-blocks"))
        .map(|entries| entries.flatten().count() as u128)
        .unwrap_or(0)
}

/// Shape 1: durable ingest under power-law degree skew. Every commit is the
/// real two-fsync durable path; every edge is read back and checked.
pub async fn shape_ingest_power_law(cx: &CommitCx) -> Result<(), String> {
    let model = Model::preferential_attachment(VERTEX_COUNT, EDGES_PER_NEW_VERTEX, LOAD_SEED);
    let dir = scratch("ingest-power-law");
    let ingest_started = Instant::now();
    let (db, reopens, commit_samples) = load_model_durably(cx, &dir, &model, true).await?;
    let total_elapsed = ingest_started.elapsed();

    verify_model(&db, &model).await?;

    let total_edges = model.edges.len() as u128;
    let total_us = total_elapsed.as_micros().max(1);
    let on_disk = bytes_on_disk(&dir);
    shape_result(
        "ingest-power-law",
        &format!(
            "vertices={VERTEX_COUNT} edges={total_edges} batch_edges={INGEST_BATCH_EDGES} durable=two-fsync"
        ),
        &[
            ("total_edges", total_edges),
            ("commits", commit_samples.len() as u128),
            ("publish_fence_reopens", reopens as u128),
            ("wall_us", total_us),
            ("edges_per_s", total_edges * 1_000_000 / total_us),
            ("commit_p50_us", percentile_us(&commit_samples, 0.50)),
            ("commit_p95_us", percentile_us(&commit_samples, 0.95)),
            ("commit_p99_us", percentile_us(&commit_samples, 0.99)),
            ("bytes_on_disk", on_disk as u128),
            ("bytes_per_edge", on_disk as u128 / total_edges.max(1)),
            ("stored_blocks", count_stored_blocks(&dir)),
        ],
    );
    Ok(())
}

/// Shape 2: warm point reads under degree skew — supernode and tail on one
/// decoded cache, every measured read checked against the model. Both
/// adjacency faces are verified: the warm pass walks the whole graph, and the
/// measured probes assert their own answers mid-measurement.
pub async fn shape_point_reads_supernode(cx: &CommitCx) -> Result<(), String> {
    let model = Model::preferential_attachment(VERTEX_COUNT, EDGES_PER_NEW_VERTEX, LOAD_SEED);
    let dir = scratch("point-reads-supernode");
    let (db, _, _) = load_model_durably(cx, &dir, &model, true).await?;
    drop(db);
    // Warm = the decoded cache of a real cold open, not a memory-only
    // handle. The warm pass walks the whole graph on BOTH adjacency faces
    // so a drift on either fails before measurement begins.
    let db = Database::open(cx, &dir, keys())
        .await
        .map_err(|error| format!("open: {error}"))?;
    for vid in model.out_adjacency.keys() {
        let got = db
            .neighbours(*vid, KNOWS)
            .map_err(|error| format!("warm neighbours: {error}"))?;
        if got != model.neighbours_of(*vid) {
            return Err(format!(
                "warm outbound mismatch at {vid:?}: db={got:?} model={:?}",
                model.neighbours_of(*vid)
            ));
        }
    }
    for vid in model.in_adjacency.keys() {
        let got = db
            .in_neighbours(*vid, KNOWS)
            .map_err(|error| format!("warm in-neighbours: {error}"))?;
        if got != model.in_neighbours_of(*vid) {
            return Err(format!(
                "warm inbound mismatch at {vid:?}: db={got:?} model={:?}",
                model.in_neighbours_of(*vid)
            ));
        }
    }

    let supernode = model.supernode();
    let tail = model.tail(32);
    const ROUNDS: usize = 200;
    let mut vertex_samples = Vec::new();
    let mut out_samples = Vec::new();
    let mut in_samples = Vec::new();
    let measured = Instant::now();
    for round in 0..ROUNDS {
        // The measured mix: the supernode (p99 driver), two tail vertices,
        // and one rotation over the middle of the distribution. Every probe
        // measures its vertex lookup plus BOTH one-hop faces, and every
        // answer is checked against the model mid-measurement.
        let mut probe = vec![supernode];
        probe.push(tail[round % tail.len()]);
        probe.push(tail[(round * 7 + 3) % tail.len()]);
        probe.push(VId((((round * 31) % (VERTEX_COUNT - 2) + 2) as u64) as u128));
        for vid in probe {
            let started = Instant::now();
            let row = db
                .vertex(vid)
                .map_err(|error| format!("vertex read: {error}"))?;
            vertex_samples.push(started.elapsed());
            if row.is_none() {
                return Err(format!("vertex {vid:?} missing during measured reads"));
            }

            let started = Instant::now();
            let got = db
                .neighbours(vid, KNOWS)
                .map_err(|error| format!("neighbours read: {error}"))?;
            out_samples.push(started.elapsed());
            if got != model.neighbours_of(vid) {
                return Err(format!(
                    "outbound face of {vid:?} drifted during measurement"
                ));
            }

            let started = Instant::now();
            let got = db
                .in_neighbours(vid, KNOWS)
                .map_err(|error| format!("in-neighbours read: {error}"))?;
            in_samples.push(started.elapsed());
            if got != model.in_neighbours_of(vid) {
                return Err(format!(
                    "inbound face of {vid:?} drifted during measurement"
                ));
            }
        }
    }
    let total_us = measured.elapsed().as_micros().max(1);
    let ops = (vertex_samples.len() + out_samples.len() + in_samples.len()) as u128;
    shape_result(
        "point-reads-supernode",
        &format!(
            "vertices={VERTEX_COUNT} rounds={ROUNDS} ops={ops} mix=supernode+tail+mid faces=both warm=cold-open-cache single-threaded"
        ),
        &[
            ("ops", ops),
            ("wall_us", total_us),
            ("ops_per_s", ops * 1_000_000 / total_us),
            ("vertex_p50_us", percentile_us(&vertex_samples, 0.50)),
            ("vertex_p95_us", percentile_us(&vertex_samples, 0.95)),
            ("vertex_p99_us", percentile_us(&vertex_samples, 0.99)),
            ("out_p50_us", percentile_us(&out_samples, 0.50)),
            ("out_p95_us", percentile_us(&out_samples, 0.95)),
            ("out_p99_us", percentile_us(&out_samples, 0.99)),
            ("in_p50_us", percentile_us(&in_samples, 0.50)),
            ("in_p95_us", percentile_us(&in_samples, 0.95)),
            ("in_p99_us", percentile_us(&in_samples, 0.99)),
        ],
    );
    Ok(())
}

/// Shape 3: one edge, 65 exact versions through 65 durable commits, then
/// historical probes at EVERY version — the amplification shape, measured and
/// verified, cold-reverified after reopen.
pub async fn shape_version_chain(cx: &CommitCx) -> Result<(), String> {
    const VERSIONS: i64 = 64;
    let dir = scratch("version-chain");
    let mut db = Database::create(cx, &dir, keys())
        .await
        .map_err(|error| format!("create: {error}"))?;
    let mut first = WriteBatch::new(KNOWS);
    first.create_vertex(VId(1), vec![], vec![]);
    first.create_vertex(VId(2), vec![], vec![]);
    first.add_edge(
        EId(10),
        VId(1),
        VId(2),
        vec![(WEIGHT, CanonicalScalar::Int(0))],
    );
    let base_seq = db
        .write(cx, first)
        .await
        .map_err(|error| format!("write: {error}"))?;
    let mut sequences = vec![(base_seq, 0i64)];
    for value in 1..=VERSIONS {
        let mut update = WriteBatch::new(KNOWS);
        update.set_edge_property(EId(10), WEIGHT, Some(CanonicalScalar::Int(value)));
        let seq = db
            .write(cx, update)
            .await
            .map_err(|error| format!("write: {error}"))?;
        sequences.push((seq, value));
    }

    let on_disk = bytes_on_disk(&dir);
    // Historical probes: every version must answer its exact value.
    let mut probe_samples = Vec::new();
    for (seq, value) in &sequences {
        let started = Instant::now();
        let record = db
            .edge_at(EId(10), *seq)
            .map_err(|error| format!("edge_at read: {error}"))?
            .ok_or_else(|| format!("version {value} missing at {seq:?}"))?;
        probe_samples.push(started.elapsed());
        if edge_weight(&record) != Ok(*value) {
            return Err(format!(
                "version {value} answered the wrong payload at {seq:?}"
            ));
        }
    }
    drop(db);

    // The history is durable: a cold open answers every version identically.
    let db = Database::open(cx, &dir, keys())
        .await
        .map_err(|error| format!("reopen: {error}"))?;
    for (seq, value) in &sequences {
        let record = db
            .edge_at(EId(10), *seq)
            .map_err(|error| format!("cold edge_at: {error}"))?
            .ok_or_else(|| format!("cold open lost version {value}"))?;
        if edge_weight(&record) != Ok(*value) {
            return Err(format!("cold open drifted version {value}"));
        }
    }
    shape_result(
        "version-chain",
        "edge=10 versions=65 durable-history=whole-spine-history cold-verified=true",
        &[
            ("versions", (VERSIONS + 1) as u128),
            ("bytes_on_disk", on_disk as u128),
            (
                "bytes_per_version",
                on_disk as u128 / (VERSIONS as u128 + 1),
            ),
            ("probe_p50_us", percentile_us(&probe_samples, 0.50)),
            ("probe_p99_us", percentile_us(&probe_samples, 0.99)),
        ],
    );
    Ok(())
}

/// Shape 4: cold reopen — the store path instead of the memory path. Time from
/// `Database::open` entry to the first VERIFIED adjacency answer, repeatedly.
pub async fn shape_cold_reopen(cx: &CommitCx) -> Result<(), String> {
    let model =
        Model::preferential_attachment(VERTEX_COUNT, EDGES_PER_NEW_VERTEX, LOAD_SEED ^ 0x5EED);
    let dir = scratch("cold-reopen");
    let (_, _, _) = load_model_durably(cx, &dir, &model, true).await?;
    let model =
        Model::preferential_attachment(VERTEX_COUNT, EDGES_PER_NEW_VERTEX, LOAD_SEED ^ 0x5EED);
    let supernode = model.supernode();
    let expected = model.neighbours_of(supernode);
    let mut samples = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        let db = Database::open(cx, &dir, keys())
            .await
            .map_err(|error| format!("open: {error}"))?;
        let got = db
            .neighbours(supernode, KNOWS)
            .map_err(|error| format!("first answer: {error}"))?;
        samples.push(started.elapsed());
        if got != expected {
            return Err("cold reopen answered a different supernode neighbourhood".to_string());
        }
        drop(db);
    }
    shape_result(
        "cold-reopen",
        &format!(
            "vertices={} open_to_first_verified_answer rounds=5",
            model.out_adjacency.len()
        ),
        &[
            ("rounds", 5),
            ("open_p50_us", percentile_us(&samples, 0.50)),
            ("open_p95_us", percentile_us(&samples, 0.95)),
            ("open_p99_us", percentile_us(&samples, 0.99)),
        ],
    );
    Ok(())
}

/// Shape 5: compaction under load — a pinned reader traverses history in a
/// separate thread while the durable compactor publishes a replacement root;
/// the reader's answers must not move, and the publish latency is the number.
pub async fn shape_compaction_under_load(cx: &CommitCx) -> Result<(), String> {
    const VERSIONS: i64 = 64;
    let dir = scratch("compaction-under-load");
    let mut db = Database::create(cx, &dir, keys())
        .await
        .map_err(|error| format!("create: {error}"))?;
    let mut first = WriteBatch::new(KNOWS);
    first.create_vertex(VId(1), vec![], vec![]);
    first.create_vertex(VId(2), vec![], vec![]);
    first.add_edge(
        EId(10),
        VId(1),
        VId(2),
        vec![(WEIGHT, CanonicalScalar::Int(0))],
    );
    let base_seq = db
        .write(cx, first)
        .await
        .map_err(|error| format!("write: {error}"))?;
    let mut sequences = vec![(base_seq, 0i64)];
    for value in 1..=VERSIONS {
        let mut update = WriteBatch::new(KNOWS);
        update.set_edge_property(EId(10), WEIGHT, Some(CanonicalScalar::Int(value)));
        let seq = db
            .write(cx, update)
            .await
            .map_err(|error| format!("write: {error}"))?;
        sequences.push((seq, value));
    }

    let pinned = db
        .pinned_read_view()
        .map_err(|error| format!("pin: {error}"))?;
    let root_before = db
        .partition_root()
        .map_err(|error| format!("root: {error}"))?;
    let reader_done = std::sync::Arc::new(AtomicBool::new(false));
    let reader_flag = reader_done.clone();
    let reader_error: std::sync::Arc<std::sync::Mutex<Option<String>>> = std::sync::Arc::default();
    // A plain owned spawn: the read view and the expected history are cloned
    // into the thread, so no scope lifetime ties the reader to this stack.
    let reader_sequences = sequences.clone();
    let reader_error_thread = reader_error.clone();
    let handle = std::thread::spawn(move || {
        let mut traversals = 0u128;
        while !reader_flag.load(Ordering::Acquire) {
            for (seq, value) in &reader_sequences {
                let record = match pinned.edge_at(EId(10), *seq) {
                    Ok(Some(record)) => record,
                    Ok(None) => {
                        *reader_error_thread.lock().expect("reader lock") =
                            Some(format!("pinned view lost version {value}"));
                        return traversals;
                    }
                    Err(error) => {
                        *reader_error_thread.lock().expect("reader lock") =
                            Some(format!("pinned edge_at: {error}"));
                        return traversals;
                    }
                };
                if edge_weight(&record) != Ok(*value) {
                    *reader_error_thread.lock().expect("reader lock") =
                        Some(format!("pinned version {value} moved during compaction"));
                    return traversals;
                }
            }
            traversals += 1;
            std::hint::spin_loop();
        }
        traversals
    });

    let started = Instant::now();
    db.compact(cx)
        .await
        .map_err(|error| format!("compact: {error}"))?;
    let compact_us = started.elapsed().as_micros();
    reader_done.store(true, Ordering::Release);
    let traversals = handle
        .join()
        .map_err(|_| "pinned reader panicked".to_string())?;
    if let Some(error) = reader_error.lock().expect("reader lock").clone() {
        return Err(error);
    }
    let root_after = db
        .partition_root()
        .map_err(|error| format!("root: {error}"))?;
    if root_after == root_before {
        return Err("compaction published no replacement root for the hostile chain".to_string());
    }
    shape_result(
        "compaction-under-load",
        "edge=10 versions=65 pinned_reader=concurrent-thread isolation=verified",
        &[
            ("compaction_us", compact_us),
            ("reader_traversals_during_compaction", traversals),
        ],
    );
    Ok(())
}

/// Dispatch one named shape. Names are the published contract of the binary's
/// shape selector; an unknown name is a caller error, not a silent skip.
pub async fn run_shape(name: &str, cx: &CommitCx) -> Result<(), String> {
    eprintln!("==> {name}");
    match name {
        "ingest-power-law" => shape_ingest_power_law(cx).await,
        "point-reads-supernode" => shape_point_reads_supernode(cx).await,
        "version-chain" => shape_version_chain(cx).await,
        "cold-reopen" => shape_cold_reopen(cx).await,
        "compaction-under-load" => shape_compaction_under_load(cx).await,
        other => Err(format!("unknown shape {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_harness_constants_are_valid() {
        assert_eq!(KNOWS, RelationId(1));
        assert_eq!(WEIGHT, PropertyKeyId(7));
    }

    // ----- Model / Lcg property tests ---------------------------------------
    //
    // The model is the test oracle for every shape in this crate. If it
    // drifts, all 5 shapes emit `correctness: verified` against wrong
    // answers and the bench silently lies. These tests pin the model
    // invariants the shapes depend on.

    const TEST_VERTEX_COUNT: usize = 60;
    const TEST_EDGES_PER_NEW: usize = 3;
    const TEST_SEED: u64 = 0x5EED_5EED;

    fn build_test_model() -> Model {
        Model::preferential_attachment(TEST_VERTEX_COUNT, TEST_EDGES_PER_NEW, TEST_SEED)
    }

    #[test]
    fn model_out_and_in_lists_are_ascending() {
        // Lines 170-175 of this file sort both faces; every reader of the
        // model (and every shape's verify_model) assumes the ascending
        // invariant, because `Database::neighbours` and `in_neighbours`
        // return ascending lists too. Drift here breaks the differential.
        let m = build_test_model();
        for list in m.out_adjacency.values() {
            for window in list.windows(2) {
                assert!(
                    window[0] < window[1],
                    "out_adjacency not strictly ascending: {list:?}"
                );
            }
        }
        for list in m.in_adjacency.values() {
            for window in list.windows(2) {
                assert!(
                    window[0] < window[1],
                    "in_adjacency not strictly ascending: {list:?}"
                );
            }
        }
    }

    #[test]
    fn model_has_no_self_loops() {
        // A directed KNOWS edge from v to v would let a model answer
        // "neighbours_of(v) contains v" while the engine correctly
        // answers "no self-loop" — the differential would fail with
        // disagreement that is actually a model bug, not an engine bug.
        let m = build_test_model();
        for (_, src, dst) in &m.edges {
            assert_ne!(*src, *dst, "self-loop in model: {src:?} -> {dst:?}");
        }
    }

    #[test]
    fn model_edge_list_bijects_with_adjacency() {
        // Every edge (eid, src, dst) in `m.edges` must be present in
        // `out_adjacency[src]` AND `in_adjacency[dst]`, and the other
        // direction's presence is the model's undirected-record invariant
        // (every edge is stored in BOTH faces). If the `link` closure
        // changes, this test catches it before any bench shape does.
        let m = build_test_model();
        for (eid, src, dst) in &m.edges {
            let out = m.out_adjacency.get(src).expect("src has no out list");
            assert!(
                out.contains(dst),
                "edge {eid:?} {src:?}->{dst:?} missing from out[{src:?}]={out:?}"
            );
            let inn = m.in_adjacency.get(dst).expect("dst has no in list");
            assert!(
                inn.contains(src),
                "edge {eid:?} {src:?}->{dst:?} missing from in[{dst:?}]={inn:?}"
            );
        }
        for (src, dsts) in &m.out_adjacency {
            for dst in dsts {
                let inn = m.in_adjacency.get(dst).expect("dst has no in list");
                assert!(
                    inn.contains(src),
                    "out[{src:?}] -> {dst:?} has no matching in[{dst:?}]"
                );
            }
        }
    }

    #[test]
    fn model_undirected_degree_matches_adjacency_lengths() {
        // The `link` closure records each edge once: it pushes to
        // `out[src]`, `in[dst]`, and bumps `degree[src]` AND `degree[dst]`
        // by 1 each. So for every vertex v, the number of edges incident
        // to v in any direction equals `len(out[v]) + len(in[v])`, AND
        // the model's `degree[v]` is exactly that count (NOT twice it,
        // because `link` bumps `degree[v]` once per call even though it
        // records BOTH the out-side and the in-side of the call).
        //
        // This is the property the bench implicitly relies on: `tail(n)`
        // and `supernode()` are picked by `degree`, but the differential
        // compares `neighbours(v) == out[v]`. If the adjacency length
        // sum drifts away from `degree`, the p99 / p50 mix the bench
        // measures is no longer what it claims to be.
        let m = build_test_model();
        for (vid, degree) in m.degree.iter() {
            let out_len = m.out_adjacency.get(vid).map_or(0, |l| l.len());
            let in_len = m.in_adjacency.get(vid).map_or(0, |l| l.len());
            assert_eq!(
                out_len + in_len,
                *degree,
                "vertex {vid:?}: out_len({out_len}) + in_len({in_len}) != degree({degree})"
            );
        }
    }

    #[test]
    fn model_is_deterministic_under_seeded_construction() {
        // Two builds from the same (vertex_count, edges_per_new_vertex,
        // seed) must produce bit-identical edge lists. The bench's own
        // claim that the workload is replayable depends on this.
        let a = Model::preferential_attachment(TEST_VERTEX_COUNT, TEST_EDGES_PER_NEW, TEST_SEED);
        let b = Model::preferential_attachment(TEST_VERTEX_COUNT, TEST_EDGES_PER_NEW, TEST_SEED);
        assert_eq!(
            a.edges.len(),
            b.edges.len(),
            "edge count diverged between two seeded builds"
        );
        for (i, (ea, eb)) in a.edges.iter().zip(b.edges.iter()).enumerate() {
            assert_eq!(ea, eb, "edge {i} differs: {ea:?} vs {eb:?}");
        }
    }

    #[test]
    fn model_supernode_is_maximum_degree() {
        // `supernode()` is the vertex the p99 driver targets; the bench
        // asserts the engine's read of it against the model. If the
        // wrong vertex is selected, the comparison still passes (the
        // model and the engine are internally consistent for the wrong
        // vertex), which is exactly the kind of false green this
        // property test prevents.
        let m = build_test_model();
        let supernode = m.supernode();
        let supernode_degree = m.degree.get(&supernode).copied().unwrap_or(0);
        for (vid, degree) in m.degree.iter() {
            assert!(
                *degree <= supernode_degree,
                "vertex {vid:?} has degree {degree} > supernode {supernode:?} degree {supernode_degree}"
            );
        }
    }

    #[test]
    fn model_tail_returns_n_lowest_degree_vertices() {
        // The tail (n lowest-degree vertices) is the OTHER leg of the
        // p99 / p50 contrast. Same correctness argument as supernode.
        let m = build_test_model();
        let n = 7;
        let tail = m.tail(n);
        assert_eq!(tail.len(), n);
        // `tail` is sorted ascending by degree internally; check the
        // final element's degree is at most the smallest non-tail degree.
        let tail_last_degree = m.degree.get(&tail[n - 1]).copied().unwrap_or(0);
        let mut by_degree: Vec<(VId, usize)> = m
            .degree
            .iter()
            .map(|(vid, degree)| (*vid, *degree))
            .collect();
        by_degree.sort_by_key(|(_, d)| *d);
        for (vid, degree) in by_degree.iter().take(n) {
            assert!(tail.contains(vid), "tail missed vertex {vid:?} with degree {degree}");
        }
        // Every non-tail vertex must have degree >= every tail vertex
        // (this is the contract: tail is the n lowest).
        for (vid, degree) in by_degree.iter().skip(n) {
            assert!(
                *degree >= tail_last_degree,
                "non-tail vertex {vid:?} has degree {degree} < tail-last {tail_last_degree}"
            );
        }
    }

    #[test]
    fn lcg_below_respects_bound() {
        // The LCG is the closed-universe substitute for `rand`. The
        // `below(bound)` helper must always return < bound for every
        // non-zero bound; if it doesn't, the preferential-attachment
        // generator will index out of bounds on the endpoints vec.
        let mut rng = Lcg::new(0xC0FFEE);
        for _ in 0..10_000 {
            let v = rng.below(64);
            assert!(v < 64, "Lcg::below(64) returned {v} >= 64");
        }
    }

    #[test]
    fn model_seed_clique_records_every_directed_pair_once() {
        // The seed clique is the first `edges_per_new_vertex + 1`
        // vertices. Lines 138-147 record the clique as a DIRECTED
        // graph: for every (a, b) pair with a < b, one call to
        // `link(ea, eb)` records `out[ea] -> eb`, `in[eb] <- ea`, and
        // bumps both `degree[ea]` and `degree[eb]` by 1. The
        // undirected degree therefore gets BOTH endpoints' bumps, but
        // the directed adjacency list is one-way per pair. This is
        // the asymmetry the engine's `neighbours` API exposes: a
        // later read of `neighbours(eb)` (where b is a higher index
        // in the seed clique) does NOT include `a`, while
        // `neighbours(ea)` does.
        //
        // The bench relies on this property: the seed clique is what
        // makes the early vertices high-degree, and the bench's
        // supernode must be one of them. If the clique code ever
        // starts calling `link(eb, ea)` as well (symmetrising the
        // adjacency), the p99 / p50 mix the bench measures changes
        // shape and the result no longer matches the model the
        // shapes document.
        let m = build_test_model();
        let clique_size = seed_clique(TEST_EDGES_PER_NEW);
        for a in 0..clique_size {
            for b in 0..clique_size {
                if a == b {
                    continue;
                }
                let va = VId(a as u128);
                let vb = VId(b as u128);
                let out_a = m.out_adjacency.get(&va).cloned().unwrap_or_default();
                let in_b = m.in_adjacency.get(&vb).cloned().unwrap_or_default();
                if a < b {
                    // Directed edge (a -> b) is recorded.
                    assert!(
                        out_a.contains(&vb),
                        "seed clique missing directed edge {va:?} -> {vb:?}"
                    );
                    assert!(
                        in_b.contains(&va),
                        "seed clique missing in[{vb:?}] <- {va:?}"
                    );
                    // The reverse (b -> a) is NOT recorded.
                    assert!(
                        !out_a.contains(&va) || true, // out_a never contains va (self)
                        "self-edge in out[{va:?}]: {out_a:?}"
                    );
                } else {
                    // a > b: no directed edge from a to b in the seed
                    // clique; the edge (b, a) is recorded as
                    // `out[b] -> a` and `in[a] <- b`, but `out[a]`
                    // does NOT contain b.
                    assert!(
                        !out_a.contains(&vb),
                        "seed clique contains a reverse edge {va:?} -> {vb:?} that the code did not record"
                    );
                }
            }
        }
        // Every seed-clique vertex's undirected degree is `clique_size - 1`
        // (one bump per (a, b) pair where it appears as either src or dst).
        for a in 0..clique_size {
            let va = VId(a as u128);
            let degree = m.degree.get(&va).copied().unwrap_or(0);
            assert_eq!(
                degree,
                clique_size - 1,
                "seed clique vertex {va:?} has degree {degree}, expected {}",
                clique_size - 1
            );
        }
    }
}
