//! In-house, deterministic, structure-aware fuzzing of the Chronicle
//! durable-format decoders (bead `fgdb-fuzz-durable-decoders-qd9l`).
//!
//! Targets: `marker::decode_canonical`, `capsule::decode_container`,
//! `symbolize::decode_object`. A torn or bit-rotted object is the recovery
//! boundary: every decoder must answer `Ok`/`Some` or a typed refusal —
//! never a panic, hang, or unbounded allocation. No `catch_unwind` anywhere.
//!
//! Seeds are REAL encoder output (proven by round trip): a canonical marker
//! from `CommitMarker::canonical_bytes`, a capsule container from
//! `encode_container(seal(...))`, and a symbol-record set from
//! `encode_object`. The symbolize decoder's corpus mutates the serialized
//! symbol records (the durable bytes), never the protected object.
//!
//! ALLOCATION-BOUND METHOD (documented per the acceptance criterion): the
//! capsule decoder enforces `MAX_CAPSULE_CONTAINER_BYTES_V1` against the
//! actual input length before allocating, and every symbol-count/length
//! field is validated against the remaining byte length before use (the
//! framing laws the capsule suite pins). The enforced per-input bound (50 ms
//! of the decoding thread's CPU time, see `bounded`) is the indirect
//! allocation instrument: any huge allocation must zero/fill its memory and
//! would blow the bound first.
//! `header_inflation_is_refused_or_bounded` plants maximal counts and
//! asserts the bound.
// knob: `CAMPAIGNS` scales the mutation campaign (mutants = CAMPAIGNS * 3
// seeds * 6 ops). 4_200 campaigns => 50_400 mutated inputs; each is fanned
// across the three decoders (>=50k decode calls per run counting the
// fan-out), well under ~90s debug on one core.
#![allow(dead_code, unused_imports, clippy::cloned_ref_to_slice_refs)]

const CAMPAIGNS: usize = 4_200;

use fgdb_chronicle::capsule::{
    CAPSULE_HEADER_BYTES_V1, CapsuleError, decode_container, encode_container, seal,
};
use fgdb_chronicle::identity::{CipherDescriptor, EncodingDescriptor, IdentifiedObject};
use fgdb_chronicle::marker::{CommitMarker, EffectSource, decode_canonical};
use fgdb_chronicle::symbolize::{RecoveryTarget, SymbolizeError, decode_object, encode_object};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use std::time::{Duration, Instant};

const K_OID: [u8; 32] = [0x5a; 32];
const DEK: [u8; 32] = [0x3c; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const KIND: u16 = 0x0274;
const HEADER: &[u8] = b"canonical-header";

/// Per-input CPU-time bound; also the documented allocation guard (module doc).
const PER_INPUT_BOUND: Duration = Duration::from_millis(50);

/// Charge one decode against [`PER_INPUT_BOUND`] by the CPU time its thread
/// spent, not wall time. On a shared host, wall time also charges the decoder
/// for every slice another process took: an unchanged binary failed one run in
/// three at load average 50, and the fastest of three back-to-back attempts
/// still exceeded 50 ms at load 102 (fgdb-g79t4). A hanging, super-linear or
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

const DECODERS: usize = 3;

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
// Seeds: REAL encoder output for each decoder
// ---------------------------------------------------------------------------

fn digest(seed: u8) -> fgdb_crypto::Digest {
    fgdb_crypto::Digest([seed; 32])
}

fn marker() -> CommitMarker {
    CommitMarker {
        logical_command_seq: 11,
        commit_seq: 7,
        effect_source: EffectSource::Local {
            capsule_ref: fgdb_types::ObjectId([0x21; 32]),
            logical_delta_template_digest: digest(1),
        },
        prev_global: None,
        head_updates: Vec::new(),
        merge_record_oid: None,
        coordinate_schema_transition_digest: digest(3),
        topology_epoch: 1,
        policy_epoch: 2,
        revocation_index: 3,
        txn_token: [7u8; 16],
        commit_hlc: 1_007,
        final_effect_digest: digest(4),
        authorization_decision_digest: digest(5),
        resource_effect_digest: digest(6),
        payload_availability_certificate_oid: None,
        flags: 0,
    }
}

struct SymbolSeed {
    /// The authenticated encoding the symbols belong to.
    encoding: fgdb_chronicle::EncodedObject,
    /// Serialized symbol records: the decoder's actual input corpus.
    symbols: Vec<Vec<u8>>,
    /// Recovery identity inputs the decoder checks the result against.
    object_id: fgdb_types::ids::ObjectId,
    protected_len: usize,
}

fn cipher_descriptor() -> CipherDescriptor {
    CipherDescriptor {
        object_kind: KIND,
        canonical_plaintext_len: 4096,
        codec_profile: 1,
        compressed_len: 4096,
        data_crypto_profile: 1,
        dek_id: [9u8; 16],
        object_nonce: core::array::from_fn(|i| (i as u8).wrapping_mul(5).wrapping_add(3)),
        object_tag_len: 16,
    }
}

fn encoding_descriptor(protected_len: usize) -> EncodingDescriptor {
    EncodingDescriptor {
        fec_profile: 1,
        transfer_length: protected_len as u64,
        oti_common: 0x0001_0002_0003_0004,
        oti_scheme: 0x0005_0006,
        symbol_size: 256,
        source_block_count: 1,
        symbol_auth_profile: 1,
    }
}

/// Seal, encode, and symbolize one object: the real producer of every byte
/// the symbolize decoder consumes.
fn symbol_seed() -> SymbolSeed {
    let plaintext: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let object = IdentifiedObject::new(&K_OID, NAMESPACE, KIND, HEADER, &plaintext);
    let object_id = object.object_id();
    let protected = object
        .protect(&DEK, cipher_descriptor(), &plaintext)
        .expect("registered AEAD profile");
    let protected_len = protected.protected_bytes().len();
    let encoding = protected.encode(encoding_descriptor(protected_len));
    let symbols = encode_object(&encoding, protected.protected_bytes(), KIND, 0, 8, &DEK)
        .expect("symbolization must succeed");
    SymbolSeed {
        encoding,
        symbols,
        object_id,
        protected_len,
    }
}

struct Seeds {
    marker: Vec<u8>,
    capsule: Vec<u8>,
    symbolize: Vec<Vec<u8>>,
}

fn seeds() -> Seeds {
    let m = marker();
    let capsule = seal(
        &K_OID,
        NAMESPACE,
        &DEK,
        KIND,
        &(0..2000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
        fgdb_chronicle::capsule::CapsuleProfile::balanced(),
    )
    .expect("seals");
    Seeds {
        marker: m.canonical_bytes().expect("marker encodes"),
        capsule: encode_container(&capsule),
        symbolize: symbol_seed().symbols,
    }
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

    /// The marker decoder answers `Option`: `None` IS its typed refusal.
    fn record_option<T>(&mut self, result: &Option<T>) {
        self.calls += 1;
        match result {
            Some(_) => self.ok += 1,
            None => self.err += 1,
        }
    }
}

/// All three decoders over the byte strings their seeds produce, inside the
/// per-input bound. The marker decoder returns `Option` — `None` is its
/// typed refusal shape and is recorded as `err`.
fn fan_out(
    marker_bytes: &[u8],
    container_bytes: &[u8],
    symbols: &[Vec<u8>],
    outcomes: &mut [Outcomes; DECODERS],
) {
    let ((marker, capsule, symbolized), elapsed) = bounded(|| {
        let marker = decode_canonical(marker_bytes).map(|m| m.commit_seq);
        let capsule = decode_container(container_bytes);
        let target = RecoveryTarget {
            k_oid: &K_OID,
            namespace: NAMESPACE,
            object_id: seeds_object_id(),
            canonical_header: HEADER,
            protected_len: seeds_protected_len(),
        };
        let symbolized = decode_object(&seeds_encoding(), symbols, target, &DEK, &mut Vec::new());
        (marker, capsule, symbolized)
    });

    outcomes[0].record_option(&marker);
    outcomes[1].record(&capsule);
    outcomes[2].record(&symbolized);

    assert!(
        elapsed <= PER_INPUT_BOUND,
        "decode fan-out exceeded the per-input bound: {elapsed:?}; marker len={} container len={} symbols={}",
        marker_bytes.len(),
        container_bytes.len(),
        symbols.len()
    );
}

// The symbolize decoder needs the encoding + identity of the seed; rebuild
// the (deterministic) seed pieces once per call site via these helpers.
fn seeds_object_id() -> fgdb_types::ids::ObjectId {
    symbol_seed().object_id
}

fn seeds_protected_len() -> usize {
    symbol_seed().protected_len
}

fn seeds_encoding() -> fgdb_chronicle::EncodedObject {
    symbol_seed().encoding
}

// ---------------------------------------------------------------------------
// Round-trip sanity: the corpus is REAL
// ---------------------------------------------------------------------------

#[test]
fn every_seed_decodes_and_reencodes_byte_identically() {
    // Marker: canonical bytes decode to the marker; re-encoding them is
    // byte-identical.
    let encoded = marker().canonical_bytes().expect("marker encodes");
    let decoded = decode_canonical(&encoded).expect("marker decodes");
    assert_eq!(
        decoded.canonical_bytes().expect("re-encodes"),
        encoded,
        "marker round trip must be byte-identical"
    );

    // Capsule: container decodes to the exact descriptor and symbols.
    let capsule = seal(
        &K_OID,
        NAMESPACE,
        &DEK,
        KIND,
        &(0..2000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
        fgdb_chronicle::capsule::CapsuleProfile::balanced(),
    )
    .expect("seals");
    let container = encode_container(&capsule);
    let (descriptor, symbols) = decode_container(&container).expect("container decodes");
    assert_eq!(descriptor, capsule.descriptor);
    assert_eq!(symbols, capsule.symbols);
    assert_eq!(
        encode_container(&fgdb_chronicle::capsule::SealedCapsule {
            object_id: capsule.object_id,
            descriptor: capsule.descriptor.clone(),
            symbols: symbols.clone(),
        }),
        container,
        "capsule container round trip must be byte-identical"
    );

    // Symbolize: the full symbol set recovers the exact protected object.
    let seed = symbol_seed();
    let target = RecoveryTarget {
        k_oid: &K_OID,
        namespace: NAMESPACE,
        object_id: seed.object_id,
        canonical_header: HEADER,
        protected_len: seed.protected_len,
    };
    let recovered = decode_object(&seed.encoding, &seed.symbols, target, &DEK, &mut Vec::new())
        .expect("the full symbol set recovers the object");
    // decode_object returns the recovered COMPRESSED PLAINTEXT — the bytes
    // that went into the AEAD — whose length is the descriptor's
    // canonical plaintext length, not the sealed ciphertext length.
    assert_eq!(
        recovered.len(),
        seed.encoding.cipher_descriptor().canonical_plaintext_len as usize
    );
}

// ---------------------------------------------------------------------------
// Tiny inputs: the shortest torn objects are typed refusals, seen by all
// ---------------------------------------------------------------------------

#[test]
fn tiny_inputs_are_typed_refusals_seen_by_every_decoder() {
    let mut outcomes = [Outcomes::default(); DECODERS];
    let encoding = seeds_encoding();
    let object_id = seeds_object_id();
    let protected_len = seeds_protected_len();
    for len in 0..=8usize {
        for fill in [0u8, 0xFF] {
            let marker_bytes = vec![fill; len];
            let container_bytes = vec![fill; len];
            let symbol_bytes = vec![fill; len];
            let ((marker, capsule, symbolized), elapsed) = bounded(|| {
                let marker = decode_canonical(&marker_bytes).map(|m| m.commit_seq);
                let capsule = decode_container(&container_bytes);
                let symbolized = decode_object(
                    &encoding,
                    std::slice::from_ref(&symbol_bytes),
                    RecoveryTarget {
                        k_oid: &K_OID,
                        namespace: NAMESPACE,
                        object_id,
                        canonical_header: HEADER,
                        protected_len,
                    },
                    &DEK,
                    &mut Vec::new(),
                );
                (marker, capsule, symbolized)
            });

            outcomes[0].record_option(&marker);
            outcomes[1].record(&capsule);
            outcomes[2].record(&symbolized);

            assert!(
                elapsed <= PER_INPUT_BOUND,
                "tiny-input fan-out exceeded the bound at len {len}"
            );
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

/// Structural = the refusal is NOT a magic/format refusal: proof a mutation
/// reached past the header checks.
fn is_header_refusal(debug: &str) -> bool {
    debug.contains("MalformedContainer") || debug.contains("UnsupportedFormat")
}

#[test]
fn every_strict_prefix_is_a_typed_refusal_and_one_reaches_the_structure() {
    let encoded = marker().canonical_bytes().expect("marker encodes");
    let capsule = seal(
        &K_OID,
        NAMESPACE,
        &DEK,
        KIND,
        &(0..2000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
        fgdb_chronicle::capsule::CapsuleProfile::balanced(),
    )
    .expect("seals");
    let container = encode_container(&capsule);
    let seed = symbol_seed();

    // Marker prefixes: every strict prefix must decode to None.
    for cut in 0..encoded.len() {
        let prefix = &encoded[..cut];
        let (refused, elapsed) = bounded(|| decode_canonical(prefix).is_none());
        assert!(refused, "a strict marker prefix decoded at cut {cut}");
        assert!(
            elapsed <= PER_INPUT_BOUND,
            "marker prefix decode exceeded the bound at cut {cut}"
        );
    }

    // Capsule prefixes INSIDE the header must refuse: a partial descriptor
    // cannot be checked. At or past CAPSULE_HEADER_BYTES_V1 the decoder may
    // legitimately return Ok with a partial symbol inventory — a short
    // symbol is DROPPED, not refused, because that damage is exactly what
    // the erasure code exists to absorb (decode_container's contract). The
    // structural-reach proof for capsules is the partial inventory: a prefix
    // past the header must return Ok with FEWER symbols than the full
    // container, proving the parser walked past the header into the body.
    for cut in 0..CAPSULE_HEADER_BYTES_V1.min(container.len()) {
        let (outcome, elapsed) = bounded(|| decode_container(&container[..cut]));
        assert!(
            elapsed <= PER_INPUT_BOUND,
            "container prefix decode exceeded the bound at cut {cut}"
        );
        assert!(
            outcome.is_err(),
            "a {cut}-byte capsule prefix decoded; the header is load-bearing"
        );
    }
    let (descriptor, partial) = decode_container(&container[..CAPSULE_HEADER_BYTES_V1 + 4])
        .expect("a header-complete prefix parses with partial inventory");
    assert_eq!(descriptor, capsule.descriptor, "the descriptor survives");
    assert!(
        partial.len() < capsule.symbols.len(),
        "a cut inside the first symbol must drop symbols, proving post-header reach"
    );

    // Symbol record prefixes: a torn record must refuse authentication, and
    // the decoder must still answer typed rather than panicking.
    for symbol in &seed.symbols {
        for cut in 0..symbol.len().min(64) {
            let torn = symbol[..cut].to_vec();
            let (outcome, elapsed) = bounded(|| {
                decode_object(
                    &seed.encoding,
                    std::slice::from_ref(&torn),
                    RecoveryTarget {
                        k_oid: &K_OID,
                        namespace: NAMESPACE,
                        object_id: seed.object_id,
                        canonical_header: HEADER,
                        protected_len: seed.protected_len,
                    },
                    &DEK,
                    &mut Vec::new(),
                )
            });
            assert!(
                elapsed <= PER_INPUT_BOUND,
                "torn symbol decode exceeded the bound at cut {cut}"
            );
            assert!(outcome.is_err(), "a torn symbol decoded");
        }
    }
}

// ---------------------------------------------------------------------------
// Header inflation: u32::MAX-style counts must refuse or stay bounded
// ---------------------------------------------------------------------------

#[test]
fn header_inflation_is_refused_or_bounded() {
    let encoded = marker().canonical_bytes().expect("marker encodes");
    let capsule = seal(
        &K_OID,
        NAMESPACE,
        &DEK,
        KIND,
        &(0..2000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
        fgdb_chronicle::capsule::CapsuleProfile::balanced(),
    )
    .expect("seals");
    let container = encode_container(&capsule);
    let mut patterns: Vec<Vec<u8>> = Vec::new();
    for seed in [&encoded, &container] {
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
    let target = RecoveryTarget {
        k_oid: &K_OID,
        namespace: NAMESPACE,
        object_id: seeds_object_id(),
        canonical_header: HEADER,
        protected_len: seeds_protected_len(),
    };
    let encoding = seeds_encoding();
    for pattern in &patterns {
        let ((marker, capsule, symbolized), elapsed) = bounded(|| {
            let marker = decode_canonical(pattern).map(|m| m.commit_seq);
            let capsule = decode_container(pattern);
            let symbolized = decode_object(
                &encoding,
                std::slice::from_ref(pattern),
                target,
                &DEK,
                &mut Vec::new(),
            );
            (marker, capsule, symbolized)
        });
        outcomes[0].record_option(&marker);
        outcomes[1].record(&capsule);
        outcomes[2].record(&symbolized);
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
// The main campaign: >=25k mutated inputs, every decoder on every input,
// plus the per-crate 50k decode-call floor (three decoders per mutant).
// ---------------------------------------------------------------------------

#[test]
fn mutated_seeds_never_panic_any_decoder() {
    let encoded = marker().canonical_bytes().expect("marker encodes");
    let capsule = seal(
        &K_OID,
        NAMESPACE,
        &DEK,
        KIND,
        &(0..2000u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
        fgdb_chronicle::capsule::CapsuleProfile::balanced(),
    )
    .expect("seals");
    let container = encode_container(&capsule);
    let symbol = symbol_seed();
    let target = RecoveryTarget {
        k_oid: &K_OID,
        namespace: NAMESPACE,
        object_id: symbol.object_id,
        canonical_header: HEADER,
        protected_len: symbol.protected_len,
    };

    let mut outcomes = [Outcomes::default(); DECODERS];
    let mut mutants = 0usize;
    for campaign in 0..CAMPAIGNS {
        let mut rng = fuzz::Rng::new(0x6B0B + campaign as u64);
        for (name, seed) in [("marker", &encoded), ("capsule", &container)] {
            // Pristine keepalive: real-encoder bytes must still decode.
            let (decoded, elapsed) = bounded(|| {
                if name == "marker" {
                    decode_canonical(seed).is_some()
                } else {
                    decode_container(seed).is_ok()
                }
            });
            assert!(decoded, "the real-encoder {name} seed must decode");
            assert!(
                elapsed <= PER_INPUT_BOUND,
                "{name}: seed decode exceeded the bound"
            );
            for op in 0..6 {
                let Some(mutant) = fuzz::mutate(op, &mut rng, seed) else {
                    continue;
                };
                let ((marker, capsule, symbolized), elapsed) = bounded(|| {
                    let marker = decode_canonical(&mutant).map(|m| m.commit_seq);
                    let capsule = decode_container(&mutant);
                    let symbolized = decode_object(
                        &symbol.encoding,
                        std::slice::from_ref(&mutant),
                        target,
                        &DEK,
                        &mut Vec::new(),
                    );
                    (marker, capsule, symbolized)
                });
                outcomes[0].record_option(&marker);
                outcomes[1].record(&capsule);
                outcomes[2].record(&symbolized);
                assert!(
                    elapsed <= PER_INPUT_BOUND,
                    "{name} op {op}: fan-out exceeded the bound; len={}",
                    mutant.len()
                );
                mutants += 1;
            }
        }
        // Serialized symbol-record mutations: drop/dup/flip whole records and
        // bytes inside records.
        if campaign % 2 == 0 {
            let mut records = symbol.symbols.clone();
            let index = rng.below(records.len());
            match campaign % 4 {
                0 => {
                    let keep = rng.below(records[index].len());
                    records[index].truncate(keep);
                }
                1 => {
                    let bit = rng.below(8) as u8;
                    let at = rng.below(records[index].len());
                    records[index][at] ^= 1u8 << bit;
                }
                2 => {
                    let clone = records[index].clone();
                    records.insert(index, clone);
                }
                3 => {
                    records.remove(index);
                }
                _ => records[index].push(rng.next_u64() as u8),
            }
            let (symbolized, elapsed) = bounded(|| {
                decode_object(&symbol.encoding, &records, target, &DEK, &mut Vec::new())
            });
            assert!(
                elapsed <= PER_INPUT_BOUND,
                "symbol mutation decode exceeded the bound: {elapsed:?}"
            );
            outcomes[2].record(&symbolized);
            mutants += 1;
        }
    }
    assert!(
        mutants >= 4 * CAMPAIGNS + CAMPAIGNS / 2,
        "campaign produced only {mutants} mutated inputs (floor = 4.5 * CAMPAIGNS)"
    );
    let total: usize = outcomes.iter().map(|o| o.calls).sum();
    assert!(
        total >= 50_000,
        "total mutated decode calls {total} below the 50k acceptance floor"
    );
    for (index, outcome) in outcomes.iter().enumerate() {
        assert!(
            outcome.ok > 0,
            "decoder {index} never decoded a valid object: {outcome:?}"
        );
        assert!(
            outcome.err >= outcome.calls / 2,
            "decoder {index} almost never refused; the fuzzer is not reaching it: {outcome:?}"
        );
    }
}

#[allow(dead_code)]
fn unused_capsule_error_shape(err: &CapsuleError) -> String {
    format!("{err:?}")
}

#[allow(dead_code)]
fn unused_symbolize_error_shape(err: &SymbolizeError) -> String {
    format!("{err:?}")
}
