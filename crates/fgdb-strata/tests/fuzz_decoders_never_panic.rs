//! In-house, deterministic, structure-aware fuzzing of the Strata
//! durable-format decoders (bead `fgdb-fuzz-durable-decoders-qd9l`).
//!
//! A torn or bit-rotted object is the recovery boundary: every decoder must
//! answer `Ok` or a typed `Err` — never a panic, hang, or unbounded
//! allocation. No `catch_unwind` anywhere; a panic fails the harness, gets
//! minimized, and becomes a named regression test next to its root-cause fix.
//!
//! Seeds are REAL encoder output (each seed's round trip is proven
//! byte-identical, which proves the corpus is real). Mutations: bit flips,
//! byte replacement, truncation, count/length inflation toward `u32::MAX`,
//! duplicated sections, and trailing extensions.
//!
//! ALLOCATION-BOUND METHOD (documented per the acceptance criterion): every
//! decoder here derives its allocations from header-declared counts that are
//! validated against the ACTUAL remaining byte length before any allocation
//! (the truncation/implausible-count refusals the format suites pin). A
//! declared `u32::MAX` count therefore cannot become an allocation: the
//! refusal fires while the input is still tiny. The enforced per-input bound
//! (50 ms of the decoding thread's CPU time, see `bounded`) is the indirect
//! allocation instrument — a huge allocation would have to zero/fill its
//! memory and would blow the bound first. The
//! `header_inflation_is_refused_or_bounded` test plants maximal counts
//! explicitly and asserts the bound holds.
//!
//! knob: `CAMPAIGNS` scales the mutation campaign (mutants = CAMPAIGNS * 6
//! seeds * 6 ops). 1_400 campaigns => 50_400 mutated inputs, each fanned
//! across all six decoders; the whole file stays well under ~90s debug on
//! one core. Lower it for a quick local smoke.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_strata::edge_props::{EdgePropertyRow, decode_property_patch, encode_property_patch};
use fgdb_strata::manifest::{ManifestError, ManifestRecord, decode_manifest, encode_manifest};
use fgdb_strata::root::{BlockRef, PartitionRoot, RootError, decode_root, encode_root, span_of};
use fgdb_strata::vertex::{VertexRow, decode_patch, encode_patch};
use fgdb_strata::{
    AdjacencyEntry, PartitionRootVersion, block_id, decode_block, decode_block_with_properties,
    encode_block, encode_block_with_properties,
};
use fgdb_types::ids::{BranchId, DatabaseSecurityNamespaceId, GraphId, ObjectId};
use fgdb_types::{CanonicalScalar, CommitSeq, EId};
use std::time::{Duration, Instant};

const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const REL: RelationId = RelationId(1);
const HOSTED_PATCH_ID: ObjectId = ObjectId([0xab; 32]);

/// Per-input CPU-time bound; also the documented allocation guard (module doc).
const PER_INPUT_BOUND: Duration = Duration::from_millis(50);

/// Charge one decode against [`PER_INPUT_BOUND`] by the CPU time its thread
/// spent, not wall time: on a shared host wall time also charges the decoder
/// for every slice another process took, which reddened the sibling Chronicle
/// campaign with an unchanged binary (fgdb-g79t4). A hanging, super-linear or
/// allocation-heavy decoder burns CPU, page faults included, so it is still
/// caught. Where the kernel has no per-thread CPU accounting, wall time.
fn bounded<T>(decode: impl FnOnce() -> T) -> (T, Duration) {
    let (cpu, wall) = (thread_cpu_time(), Instant::now());
    let value = decode();
    let spent = match (cpu, thread_cpu_time()) {
        (Some(before), Some(after)) => after.saturating_sub(before),
        _ => wall.elapsed(),
    };
    (value, spent)
}

/// This thread's on-CPU time from the scheduler's own accounting (first field
/// of `/proc/thread-self/schedstat`, nanoseconds; current to within a tick).
fn thread_cpu_time() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let nanos = stat.split_whitespace().next()?.parse().ok()?;
    Some(Duration::from_nanos(nanos))
}
const CAMPAIGNS: usize = 1_400;
const DECODERS: usize = 6;

// ---------------------------------------------------------------------------
// Deterministic RNG + the six mutation operators
// ---------------------------------------------------------------------------

mod fuzz {
    /// splitmix64 — tiny, deterministic, dependency-free.
    pub struct Rng {
        state: u64,
    }

    impl Rng {
        pub fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        pub fn next_u64(&mut self) -> u64 {
            self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        pub fn below(&mut self, bound: usize) -> usize {
            if bound == 0 {
                0
            } else {
                self.next_u64() as usize % bound
            }
        }
    }

    /// op 0 bit flip; 1 byte replacement; 2 truncation; 3 count/length
    /// inflation; 4 section duplication; 5 trailing extension.
    pub fn mutate(op: usize, rng: &mut Rng, bytes: &[u8]) -> Option<Vec<u8>> {
        if bytes.is_empty() {
            return None;
        }
        let mut out = bytes.to_vec();
        match op {
            0 => {
                let at = rng.below(out.len());
                out[at] ^= 1u8 << rng.below(8);
                Some(out)
            }
            1 => {
                let at = rng.below(out.len());
                out[at] = rng.next_u64() as u8;
                Some(out)
            }
            2 => {
                out.truncate(rng.below(out.len()));
                Some(out)
            }
            3 => {
                // Count/length/version fields live in the first bytes of
                // every one of these formats: plant an implausible maximum
                // at a header offset.
                let word: [u8; 4] = match rng.below(3) {
                    0 => u32::MAX.to_le_bytes(),
                    1 => u32::MAX.to_be_bytes(),
                    _ => [0x7F; 4],
                };
                let at = 4 + 2 * rng.below(8);
                for (offset, byte) in word.into_iter().enumerate() {
                    if at + offset < out.len() {
                        out[at + offset] = byte;
                    }
                }
                Some(out)
            }
            4 => {
                if out.len() < 2 {
                    return None;
                }
                let start = rng.below(out.len());
                let end = start + 1 + rng.below(out.len() - start);
                let section = out[start..end].to_vec();
                let insert_at = rng.below(out.len());
                out.splice(insert_at..insert_at, section.iter().copied());
                Some(out)
            }
            _ => {
                for _ in 0..1 + rng.below(8) {
                    out.push(rng.next_u64() as u8);
                }
                Some(out)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Seeds: one per decoder, all REAL encoder output
// ---------------------------------------------------------------------------

fn edge(eid: u128, src: u128, dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: fgdb_types::VId(src),
        relation: REL,
        dst: fgdb_types::VId(dst),
        eid: EId(eid),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn entries() -> Vec<AdjacencyEntry> {
    vec![
        edge(10, 1, 2, 3, None),
        edge(11, 1, 3, 4, None),
        edge(12, 1, 4, 5, Some(7)),
    ]
}

fn vertex_rows() -> Vec<VertexRow> {
    vec![
        VertexRow {
            vid: fgdb_types::VId(101),
            birth_ordinal: 7,
            created_at: CommitSeq(3),
            retired_at: None,
            labels: vec![LabelId(11), LabelId(23)],
            props: vec![
                (
                    PropertyKeyId(41),
                    CanonicalScalar::ucs_basic_text("ada").expect("admissible text"),
                ),
                (PropertyKeyId(59), CanonicalScalar::Int(-1815)),
            ],
        },
        VertexRow {
            vid: fgdb_types::VId(202),
            birth_ordinal: 13,
            created_at: CommitSeq(5),
            retired_at: Some(CommitSeq(9)),
            labels: vec![LabelId(31)],
            props: vec![(PropertyKeyId(67), CanonicalScalar::Bool(true))],
        },
        VertexRow {
            vid: fgdb_types::VId(303),
            birth_ordinal: 17,
            created_at: CommitSeq(6),
            retired_at: None,
            labels: vec![],
            props: vec![],
        },
    ]
}

fn property_rows() -> Vec<EdgePropertyRow> {
    vec![
        vec![
            (
                PropertyKeyId(41),
                CanonicalScalar::ucs_basic_text("ada").expect("admissible"),
            ),
            (PropertyKeyId(59), CanonicalScalar::Int(-1815)),
        ],
        vec![(PropertyKeyId(67), CanonicalScalar::Bool(true))],
    ]
}

fn manifest_records() -> Vec<ManifestRecord> {
    vec![
        ManifestRecord {
            graph: GraphId(3),
            branch: BranchId(5),
            partition: 7,
            root: PartitionRootVersion(ObjectId([0x11; 32])),
            published_chain_hash: fgdb_crypto::Digest([0xa1; 32]),
        },
        ManifestRecord {
            graph: GraphId(3),
            branch: BranchId(5),
            partition: 9,
            root: PartitionRootVersion(ObjectId([0x22; 32])),
            published_chain_hash: fgdb_crypto::Digest([0xa2; 32]),
        },
        ManifestRecord {
            graph: GraphId(4),
            branch: BranchId(2),
            partition: 1,
            root: PartitionRootVersion(ObjectId([0x33; 32])),
            published_chain_hash: fgdb_crypto::Digest([0xa3; 32]),
        },
    ]
}

fn sample_root() -> PartitionRoot {
    let first = &entries()[..2];
    let second = &entries()[2..];
    let bytes_a = encode_block(0, None, first).expect("block a encodes");
    let bytes_b = encode_block(0, None, second).expect("block b encodes");
    let reference = |bytes: &[u8], slice: &[AdjacencyEntry]| {
        let span = span_of(slice).expect("non-empty");
        BlockRef {
            block_id: block_id(&K_OID, NAMESPACE, bytes),
            first_seq: span.0,
            last_seq: span.1,
        }
    };
    PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(1),
        partition: 0,
        published_at: CommitSeq(9),
        blocks: vec![
            reference(&bytes_a.clone(), first),
            reference(&bytes_b.clone(), second),
        ],
        vertex_patches: vec![],
    }
}

struct Seeds {
    block_plain: Vec<u8>,
    block_hosted: Vec<u8>,
    vertex_patch: Vec<u8>,
    property_patch: Vec<u8>,
    root: Vec<u8>,
    manifest: Vec<u8>,
}

fn seeds() -> Seeds {
    let locators = [1u8, 0, 2];
    Seeds {
        block_plain: encode_block(0, None, &entries()).expect("plain block encodes"),
        block_hosted: encode_block_with_properties(
            0,
            None,
            &entries(),
            HOSTED_PATCH_ID,
            &locators,
            &property_rows(),
        )
        .expect("hosted block encodes"),
        vertex_patch: encode_patch(&vertex_rows()).expect("vertex patch encodes"),
        property_patch: encode_property_patch(&property_rows()).expect("property patch encodes"),
        root: encode_root(&sample_root()).expect("root encodes"),
        manifest: encode_manifest(&manifest_records()).expect("manifest encodes"),
    }
}

/// Decoder families in a fixed order; `(usize, &str, &Vec<u8>)` per entry.
/// Family index is the decoder slot in `Outcomes`.
fn families(s: &Seeds) -> [(usize, &'static str, &Vec<u8>); 6] {
    [
        (0, "block_hosted", &s.block_hosted),
        (1, "vertex_patch", &s.vertex_patch),
        (2, "property_patch", &s.property_patch),
        (3, "root", &s.root),
        (4, "manifest", &s.manifest),
        (5, "block_plain", &s.block_plain),
    ]
}

/// The decoder that owns `family`, returning the raw typed outcome. A
/// structural refusal is anything except a magic/version refusal: this is
/// the proof a mutation reached past the header checks.
fn decode_own(family: usize, bytes: &[u8]) -> Result<usize, String> {
    match family {
        0 => decode_block_with_properties(bytes)
            .map(|(entries, _)| entries.len())
            .map_err(|err| format!("{err:?}")),
        1 => decode_patch(bytes)
            .map(|rows| rows.len())
            .map_err(|err| format!("{err:?}")),
        2 => decode_property_patch(bytes)
            .map(|rows| rows.len())
            .map_err(|err| format!("{err:?}")),
        3 => decode_root(bytes)
            .map(|root| root.blocks.len())
            .map_err(|err| format!("{err:?}")),
        4 => decode_manifest(bytes)
            .map(|records| records.len())
            .map_err(|err| format!("{err:?}")),
        _ => decode_block(bytes)
            .map(|entries| entries.len())
            .map_err(|err| format!("{err:?}")),
    }
}

fn is_header_refusal(debug: &str) -> bool {
    debug.contains("NotABlock")
        || debug.contains("NotAManifest")
        || debug.contains("NotARoot")
        || debug.contains("NotAVertexPatch")
        || debug.contains("NotAPropertyPatch")
        || debug.contains("UnsupportedFormat")
}

// ---------------------------------------------------------------------------
// Uniform decode fan-out with per-decoder outcome accounting
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy, core::fmt::Debug)]
struct Outcomes {
    calls: usize,
    ok: usize,
    err: usize,
}

impl Outcomes {
    fn record<R>(&mut self, result: &Result<R, impl core::fmt::Debug>) {
        self.calls += 1;
        match result {
            Ok(_) => self.ok += 1,
            Err(err) => {
                self.err += 1;
                let _ = format!("{err:?}");
            }
        }
    }
}

/// All six decoders over `bytes`, the whole fan-out inside the bound.
fn fan_out(bytes: &[u8], outcomes: &mut [Outcomes; DECODERS]) {
    let ((hosted, vertex, property, root, manifest, plain), elapsed) = bounded(|| {
        (
            decode_block_with_properties(bytes),
            decode_patch(bytes),
            decode_property_patch(bytes),
            decode_root(bytes),
            decode_manifest(bytes),
            decode_block(bytes),
        )
    });

    outcomes[0].record(&hosted);
    outcomes[1].record(&vertex);
    outcomes[2].record(&property);
    outcomes[3].record(&root);
    outcomes[4].record(&manifest);
    outcomes[5].record(&plain);

    assert!(
        elapsed <= PER_INPUT_BOUND,
        "decode fan-out exceeded the per-input bound: {elapsed:?}; len={}",
        bytes.len()
    );
}

// ---------------------------------------------------------------------------
// Round-trip sanity: the corpus is REAL
// ---------------------------------------------------------------------------

#[test]
fn every_seed_decodes_and_reencodes_byte_identically() {
    let s = seeds();

    let decoded = decode_block(&s.block_plain).expect("plain block decodes");
    assert_eq!(
        encode_block(0, None, &decoded).expect("re-encodes"),
        s.block_plain,
        "plain block round trip must be byte-identical"
    );

    let (decoded, patch) = decode_block_with_properties(&s.block_hosted).expect("decodes");
    assert_eq!(decoded, entries());
    let (found_id, found_locators) = patch.expect("the hosted patch survives");
    assert_eq!(found_id, HOSTED_PATCH_ID);
    assert_eq!(found_locators, [1u8, 0, 2]);
    assert_eq!(
        encode_block_with_properties(
            0,
            None,
            &decoded,
            found_id,
            &found_locators,
            &property_rows()
        )
        .expect("re-encodes"),
        s.block_hosted,
        "hosted block round trip must be byte-identical"
    );

    let rows = decode_patch(&s.vertex_patch).expect("vertex patch decodes");
    assert_eq!(
        encode_patch(&rows).expect("re-encodes"),
        s.vertex_patch,
        "vertex patch round trip must be byte-identical"
    );

    let rows = decode_property_patch(&s.property_patch).expect("property patch decodes");
    assert_eq!(
        encode_property_patch(&rows).expect("re-encodes"),
        s.property_patch,
        "property patch round trip must be byte-identical"
    );

    let root = decode_root(&s.root).expect("root decodes");
    assert_eq!(
        encode_root(&root).expect("re-encodes"),
        s.root,
        "root round trip must be byte-identical"
    );

    let records = decode_manifest(&s.manifest).expect("manifest decodes");
    assert_eq!(
        encode_manifest(&records).expect("re-encodes"),
        s.manifest,
        "manifest round trip must be byte-identical"
    );
}

// ---------------------------------------------------------------------------
// Tiny inputs: the shortest torn objects are typed refusals, seen by all
// ---------------------------------------------------------------------------

#[test]
fn tiny_inputs_are_typed_refusals_seen_by_every_decoder() {
    let mut outcomes = [Outcomes::default(); DECODERS];
    for len in 0..=8usize {
        for fill in [0u8, 0xFF] {
            let bytes = vec![fill; len];
            fan_out(&bytes, &mut outcomes);
        }
    }
    for (index, outcome) in outcomes.iter().enumerate() {
        assert!(
            outcome.calls >= 18,
            "decoder {index} under-fed: {outcome:?}"
        );
        assert!(
            outcome.err >= 18,
            "decoder {index} admitted a tiny input; header checks are load-bearing: {outcome:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Truncation at EVERY prefix length for small objects
// ---------------------------------------------------------------------------

#[test]
fn every_strict_prefix_is_a_typed_refusal_and_one_reaches_the_structure() {
    let s = seeds();
    for (family, name, seed) in families(&s) {
        let mut structural = 0usize;
        for cut in 0..seed.len() {
            let mut prefix = seed.clone();
            prefix.truncate(cut);
            let (outcome, elapsed) = bounded(|| decode_own(family, &prefix));
            assert!(
                elapsed <= PER_INPUT_BOUND,
                "{name}: prefix decode exceeded the bound at cut {cut}"
            );
            assert!(
                outcome.is_err(),
                "{name}: a strict prefix decoded Ok at cut {cut}"
            );
            if cut > 8 && !is_header_refusal(outcome.as_ref().expect_err("checked err")) {
                structural += 1;
            }
        }
        assert!(
            structural > 0,
            "{name}: no truncation reached past the magic/version header checks"
        );
    }
}

// ---------------------------------------------------------------------------
// Header inflation: u32::MAX-style counts must refuse or stay bounded
// ---------------------------------------------------------------------------

#[test]
fn header_inflation_is_refused_or_bounded() {
    let s = seeds();
    let mut patterns: Vec<Vec<u8>> = Vec::new();
    for (_, _, seed) in families(&s) {
        let mut filled = seed.clone();
        for byte in filled.iter_mut().take(24) {
            *byte = 0xFF;
        }
        patterns.push(filled);
        for at in (4..20).step_by(2) {
            let mut planted = seed.clone();
            if at + 4 <= planted.len() {
                planted[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            }
            patterns.push(planted);
        }
    }
    let mut outcomes = [Outcomes::default(); DECODERS];
    for pattern in &patterns {
        let ((), elapsed) = bounded(|| fan_out(pattern, &mut outcomes));
        assert!(
            elapsed <= PER_INPUT_BOUND,
            "an inflated header burst the per-input bound; len={}",
            pattern.len()
        );
    }
    for (index, outcome) in outcomes.iter().enumerate() {
        assert!(
            outcome.calls >= patterns.len(),
            "decoder {index} under-fed: {outcome:?}"
        );
        assert!(
            outcome.err > 0,
            "decoder {index} never refused an inflated header: {outcome:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The main campaign: >=50k mutated inputs, every decoder on every input
// ---------------------------------------------------------------------------

#[test]
fn mutated_seeds_never_panic_any_decoder() {
    let s = seeds();
    let mut outcomes = [Outcomes::default(); DECODERS];
    let mut mutants = 0usize;
    for campaign in 0..CAMPAIGNS {
        let mut rng = fuzz::Rng::new(0x00DE_C0DE + campaign as u64);
        for (family, name, seed) in families(&s) {
            // Pristine keepalive: the owning decoder must still accept its
            // own real-encoder seed (Ok evidence for the anti-vacuity check).
            let (pristine, elapsed) = bounded(|| decode_own(family, seed));
            assert!(
                pristine.is_ok(),
                "{name}: the real-encoder seed must decode"
            );
            assert!(
                elapsed <= PER_INPUT_BOUND,
                "{name}: seed decode exceeded the bound"
            );
            outcomes[family].calls += 1;
            outcomes[family].ok += 1;

            for op in 0..6 {
                let Some(mutant) = fuzz::mutate(op, &mut rng, seed) else {
                    continue;
                };
                let ((), elapsed) = bounded(|| fan_out(&mutant, &mut outcomes));
                assert!(
                    elapsed <= PER_INPUT_BOUND,
                    "{name} op {op}: fan-out exceeded the bound; len={}",
                    mutant.len()
                );
                mutants += 1;
            }
        }
    }
    assert!(
        mutants >= 50_000,
        "campaign produced only {mutants} mutated inputs"
    );
    for (index, outcome) in outcomes.iter().enumerate() {
        assert!(
            outcome.calls >= mutants,
            "decoder {index} saw fewer inputs than mutants ({outcome:?})"
        );
        assert!(
            outcome.ok >= CAMPAIGNS,
            "decoder {index} never decoded a valid object: {outcome:?}"
        );
        assert!(
            outcome.err >= mutants / 2,
            "decoder {index} almost never refused; the fuzzer is not reaching it: {outcome:?}"
        );
    }
}

/// Silence the unused warning for the typed-shape helper kept for the
/// regression tests a discovered crash will add beside it.
#[allow(dead_code)]
fn unused_shape_helper(_: fn(&[u8]) -> Result<usize, String>) {}

#[allow(dead_code)]
fn unused_manifest_error_shape(err: &ManifestError) -> String {
    format!("{err:?}")
}

#[allow(dead_code)]
fn unused_root_error_shape(err: &RootError) -> String {
    format!("{err:?}")
}
