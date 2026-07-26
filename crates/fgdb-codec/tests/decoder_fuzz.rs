//! Deterministic structure-aware fuzzing for every byte-input decoder in
//! `fgdb-codec`.
//!
//! The dependency universe is closed, so this is not cargo-fuzz, libFuzzer or
//! AFL — it is the same shape built in-house: a seeded generator that produces
//! structurally VALID inputs through each encoder and then corrupts them
//! through a fixed catalogue of mutations, plus a checked-in corpus of
//! hand-picked boundary inputs, plus unstructured random bytes as the control.
//!
//! The contract asserted for every decoder, on every input, is exactly three
//! things:
//!
//!   1. it never panics — a decoder reachable from durable bytes that panics
//!      on malformed input is a denial-of-service defect, not a bug report;
//!   2. it terminates in bounded work — enforced structurally, by asserting
//!      that an accepted decode produced exactly the number of values its
//!      caller asked for, so a decoder that keeps emitting cannot pass;
//!   3. it either ACCEPTS or returns a clean typed error — never a partial
//!      result, and never a different answer for the same bytes.
//!
//! Determinism is asserted directly (same input decoded twice must give the
//! same answer) because a decoder that reads uninitialised or
//! allocation-dependent state would otherwise pass every other check.
//!
//! Seeds are pinned in source. There is no clock, no entropy source, and no
//! dependency beyond the crate under test.
//!
//! WHAT IS NOT COVERED, stated so the gap is not mistaken for coverage: the
//! plan's verification ladder also names the GQL/Cypher grammars, FGP frames
//! and the SymbolRecord decoder. None of those exist in the tree yet —
//! `SymbolRecord` is an Appendix A physical-kind row with no decoder — so
//! there is nothing to fuzz. This file covers the decoders that exist today.

use fgdb_codec::bitpack;
use fgdb_codec::delta_varint;
use fgdb_codec::identity::OriginBirthOrder;
use fgdb_codec::varint;
use fgdb_types::VId;

// ---------------------------------------------------------------------------
// deterministic generator
// ---------------------------------------------------------------------------

/// SplitMix64 over pinned seeds. Every failure message carries the seed, the
/// case index and the exact input bytes, so a red run is replayable from the
/// message alone without re-running the generator.
struct Fuzzer {
    state: u64,
}

impl Fuzzer {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next_u64() % bound as u64) as usize
        }
    }

    fn byte(&mut self) -> u8 {
        // Bias toward the bytes that matter to a varint/bitpack reader:
        // continuation bits set, all-ones, and zero.
        match self.next_u64() % 8 {
            0 => 0x00,
            1 => 0xff,
            2 => 0x80,
            3 => 0x7f,
            _ => (self.next_u64() & 0xff) as u8,
        }
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.byte()).collect()
    }
}

/// Pinned seeds. Changing this list changes the corpus, so it is a reviewed
/// edit, not a tuning knob.
const SEEDS: [u64; 6] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_002a,
    0xdead_beef_dead_beef,
    0x5eed_5eed_5eed_5eed,
    0xffff_ffff_ffff_ffff,
    0x0123_4567_89ab_cdef,
];

/// The fixed corruption catalogue. Each variant is a distinct way a durable
/// byte string goes wrong on disk or on the wire.
#[derive(Clone, Copy, Debug)]
enum Corruption {
    None,
    TruncateOne,
    TruncateHalf,
    TruncateAll,
    FlipBit,
    SetByteMax,
    SetByteZero,
    AppendJunk,
    DuplicateByte,
    SwapAdjacent,
}

impl Corruption {
    const ALL: [Corruption; 10] = [
        Corruption::None,
        Corruption::TruncateOne,
        Corruption::TruncateHalf,
        Corruption::TruncateAll,
        Corruption::FlipBit,
        Corruption::SetByteMax,
        Corruption::SetByteZero,
        Corruption::AppendJunk,
        Corruption::DuplicateByte,
        Corruption::SwapAdjacent,
    ];

    fn apply(self, bytes: &[u8], f: &mut Fuzzer) -> Vec<u8> {
        let mut out = bytes.to_vec();
        match self {
            Corruption::None => {}
            Corruption::TruncateOne => {
                out.pop();
            }
            Corruption::TruncateHalf => {
                let keep = out.len() / 2;
                out.truncate(keep);
            }
            Corruption::TruncateAll => out.clear(),
            Corruption::FlipBit => {
                if !out.is_empty() {
                    let index = f.below(out.len());
                    let bit = f.below(8);
                    out[index] ^= 1 << bit;
                }
            }
            Corruption::SetByteMax => {
                if !out.is_empty() {
                    let index = f.below(out.len());
                    out[index] = 0xff;
                }
            }
            Corruption::SetByteZero => {
                if !out.is_empty() {
                    let index = f.below(out.len());
                    out[index] = 0x00;
                }
            }
            Corruption::AppendJunk => {
                let extra = 1 + f.below(8);
                for _ in 0..extra {
                    out.push(f.byte());
                }
            }
            Corruption::DuplicateByte => {
                if !out.is_empty() {
                    let index = f.below(out.len());
                    let value = out[index];
                    out.insert(index, value);
                }
            }
            Corruption::SwapAdjacent => {
                if out.len() >= 2 {
                    let index = f.below(out.len() - 1);
                    out.swap(index, index + 1);
                }
            }
        }
        out
    }
}

/// The checked-in corpus: inputs chosen by hand because they sit exactly on a
/// decoder boundary, not because a generator happened to find them. These run
/// against every byte-input decoder regardless of which one they were written
/// for — a boundary for one reader is often a boundary for another.
const CORPUS: [&[u8]; 24] = [
    &[],
    &[0x00],
    &[0x7f],
    &[0x80],
    &[0xff],
    &[0x80, 0x00],
    &[0xff, 0xff],
    &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00],
    // ten continuation bytes: one past the maximum u64 LEB128 length
    &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80],
    // u64::MAX as canonical LEB128
    &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
    // the same value with a non-canonical trailing zero group
    &[
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x81, 0x00,
    ],
    &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    &[0xaa, 0x55, 0xaa, 0x55, 0xaa, 0x55, 0xaa, 0x55],
    &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
    &[0xff; 16],
    &[0x00; 16],
    &[0x80; 16],
    &[0xff; 31],
    &[0xff; 32],
    &[0x00; 32],
    &[0x00; 33],
    &[0x00; 39],
    &[0x00; 40],
];

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ===========================================================================
// varint — canonical unsigned LEB128
// ===========================================================================

/// Every byte string is either a valid canonical varint prefix or a clean
/// rejection, the decode is deterministic, and an accepted prefix reports a
/// consumed length that is inside the input.
#[test]
fn varint_decoder_accepts_or_rejects_every_input_without_panicking() {
    let mut cases = 0usize;
    let mut accepted = 0usize;

    let check = |input: &[u8], origin: &str| {
        let first = varint::decode_u64_prefix(input);
        let second = varint::decode_u64_prefix(input);
        assert_eq!(
            first,
            second,
            "{origin}: varint prefix decode is not deterministic for {}",
            hex(input)
        );
        if let Ok((_, consumed)) = first {
            assert!(
                consumed <= input.len(),
                "{origin}: consumed {consumed} of {} bytes for {}",
                input.len(),
                hex(input)
            );
            assert!(
                consumed > 0,
                "{origin}: accepted a zero-length read for {}",
                hex(input)
            );
        }
        // The whole-input decoder must agree with the prefix decoder about
        // acceptance whenever the prefix consumed the entire input.
        let whole = varint::decode_u64(input);
        if let Ok((value, consumed)) = first
            && consumed == input.len()
        {
            assert_eq!(
                whole,
                Ok(value),
                "{origin}: prefix and whole-input decoders disagree on {}",
                hex(input)
            );
        }
    };

    for (index, case) in CORPUS.iter().enumerate() {
        check(case, &format!("corpus[{index}]"));
        cases += 1;
    }

    for &seed in &SEEDS {
        let mut f = Fuzzer::new(seed);
        // structurally valid, then corrupted
        for step in 0..600 {
            let value = match step % 4 {
                0 => 0,
                1 => u64::MAX,
                2 => f.next_u64() & 0x7f,
                _ => f.next_u64(),
            };
            let encoded = varint::encode_u64(value);
            let valid = encoded.as_bytes();
            // an uncorrupted encode must round-trip exactly
            let (decoded, consumed) = varint::decode_u64_prefix(valid).unwrap_or_else(|e| {
                panic!("seed {seed} step {step}: valid encoding rejected: {e:?}")
            });
            assert_eq!(
                decoded, value,
                "seed {seed} step {step}: round-trip drifted"
            );
            assert_eq!(
                consumed,
                valid.len(),
                "seed {seed} step {step}: round-trip consumed the wrong length"
            );
            accepted += 1;

            for corruption in Corruption::ALL {
                let mutated = corruption.apply(valid, &mut f);
                check(&mutated, &format!("seed {seed} step {step} {corruption:?}"));
                cases += 1;
            }
        }
        // unstructured control
        for _ in 0..400 {
            let len = f.below(24);
            let input = f.bytes(len);
            check(&input, &format!("seed {seed} unstructured"));
            cases += 1;
        }
    }

    assert!(
        cases > 20_000,
        "the sweep must actually cover something; ran {cases}"
    );
    assert!(accepted > 0, "no valid encoding was exercised");
}

// ===========================================================================
// bitpack — checked fixed-width packing, and the one decoder in the crate
// that indexes its input directly
// ===========================================================================

/// `bitpack::decode` reads `input[byte_index]` with an unchecked index after a
/// separate `validate_input` guard. That shape is exactly where a length
/// guard and a read loop drift apart, so this drives hostile `(count, width)`
/// against short inputs.
#[test]
fn bitpack_decoder_never_indexes_past_a_short_input() {
    let mut cases = 0usize;
    for &seed in &SEEDS {
        let mut f = Fuzzer::new(seed);
        for _ in 0..2_000 {
            // widths include the 0 and 64 boundaries and one past 64
            let width = match f.next_u64() % 6 {
                0 => 0,
                1 => 1,
                2 => 63,
                3 => 64,
                4 => 65,
                _ => (f.next_u64() % 66) as u8,
            };
            let count = match f.next_u64() % 5 {
                0 => 0,
                1 => 1,
                2 => f.below(64),
                3 => usize::MAX, // hostile: must be rejected, not allocated
                _ => f.below(1_024),
            };
            let len = f.below(48);
            let input = f.bytes(len);

            let first = bitpack::decode(&input, count, width);
            let second = bitpack::decode(&input, count, width);
            assert_eq!(
                first.is_ok(),
                second.is_ok(),
                "seed {seed}: bitpack decode not deterministic for count={count} width={width} \
                 input={}",
                hex(&input)
            );
            if let Ok(values) = first {
                // bounded work: an accepted decode yields EXACTLY count values
                assert_eq!(
                    values.len(),
                    count,
                    "seed {seed}: accepted decode produced {} values for count={count} width={width}",
                    values.len()
                );
                if width < 64 && width > 0 {
                    let ceiling = 1u64 << width;
                    for (index, value) in values.iter().enumerate() {
                        assert!(
                            *value < ceiling,
                            "seed {seed}: value {value} at {index} exceeds width {width}"
                        );
                    }
                }
            }
            // decode_for shares the same reader and adds a checked base
            let base = f.next_u64();
            let with_base = bitpack::decode_for(&input, count, base, width);
            if let Ok(values) = with_base {
                assert_eq!(
                    values.len(),
                    count,
                    "seed {seed}: decode_for produced the wrong length"
                );
            }
            cases += 1;
        }
    }
    assert!(cases >= 12_000, "ran only {cases} bitpack cases");
}

/// Structurally valid packings, then corrupted: the decoder must round-trip
/// the clean bytes exactly and stay total on every mutation of them.
#[test]
fn bitpack_round_trips_clean_packings_and_survives_corruption() {
    for &seed in &SEEDS {
        let mut f = Fuzzer::new(seed);
        for step in 0..400 {
            let width = (1 + f.next_u64() % 64) as u8;
            let count = f.below(33);
            let ceiling = if width >= 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            let values: Vec<u64> = (0..count)
                .map(|_| match f.next_u64() % 4 {
                    0 => 0,
                    1 => ceiling,
                    2 => ceiling / 2,
                    _ => f.next_u64() & ceiling,
                })
                .collect();
            let Ok(encoded) = bitpack::encode(&values, width) else {
                continue;
            };
            let decoded = bitpack::decode(&encoded, count, width).unwrap_or_else(|e| {
                panic!("seed {seed} step {step}: valid packing rejected: {e:?}")
            });
            assert_eq!(
                decoded, values,
                "seed {seed} step {step}: bitpack round-trip drifted at width {width}"
            );

            for corruption in Corruption::ALL {
                let mutated = corruption.apply(&encoded, &mut f);
                // total on every corruption; the only contract is no panic and
                // a length-exact accept
                if let Ok(out) = bitpack::decode(&mutated, count, width) {
                    assert_eq!(
                        out.len(),
                        count,
                        "seed {seed} step {step} {corruption:?}: accepted a wrong-length decode"
                    );
                }
            }
        }
    }
}

// ===========================================================================
// delta-varint
// ===========================================================================

/// The delta-varint reader walks the input twice (once to validate, once to
/// build). Both walks must agree, and a hostile `count` must be rejected by
/// the entry limit before any allocation.
#[test]
fn delta_varint_decoder_is_total_over_corrupted_and_random_input() {
    let limit = delta_varint::EntryLimit::new(4_096);
    for &seed in &SEEDS {
        let mut f = Fuzzer::new(seed);
        for step in 0..400 {
            let count = f.below(24);
            let mut values: Vec<u64> = (0..count).map(|_| f.next_u64() >> 8).collect();
            values.sort_unstable();
            values.dedup();
            let actual = values.len();
            let Ok(encoded) = delta_varint::encode(&values) else {
                continue;
            };
            let decoded = delta_varint::decode(&encoded, actual, limit).unwrap_or_else(|e| {
                panic!("seed {seed} step {step}: valid delta encoding rejected: {e:?}")
            });
            assert_eq!(
                decoded, values,
                "seed {seed} step {step}: delta-varint round-trip drifted"
            );

            for corruption in Corruption::ALL {
                let mutated = corruption.apply(&encoded, &mut f);
                for hostile_count in [actual, 0, actual + 1, usize::MAX] {
                    let first = delta_varint::decode(&mutated, hostile_count, limit);
                    let second = delta_varint::decode(&mutated, hostile_count, limit);
                    assert_eq!(
                        first.is_ok(),
                        second.is_ok(),
                        "seed {seed} step {step} {corruption:?}: nondeterministic for \
                         count={hostile_count} input={}",
                        hex(&mutated)
                    );
                    if let Ok(out) = first {
                        assert_eq!(
                            out.len(),
                            hostile_count,
                            "seed {seed} step {step} {corruption:?}: accepted decode of \
                             length {} for count={hostile_count}",
                            out.len()
                        );
                    }
                }
            }
        }
        for _ in 0..600 {
            let len = f.below(40);
            let input = f.bytes(len);
            let count = f.below(16);
            if let Ok(out) = delta_varint::decode(&input, count, limit) {
                assert_eq!(
                    out.len(),
                    count,
                    "seed {seed}: random accept had wrong length"
                );
            }
        }
    }
}

/// The entry ceiling is only observable in a narrow window: `count` must
/// exceed the limit AND the input must actually carry that many entries. A
/// sweep whose counts are all small or all absurd never enters it, so the
/// ceiling is driven here explicitly — this test exists because removing the
/// ceiling SURVIVED the rest of the sweep.
///
/// PROVEN RED BY: deleting the `count > limit.max_entries()` guard from
/// `delta_varint::decode`, which then decodes past the declared ceiling.
#[test]
fn delta_varint_entry_ceiling_rejects_inside_its_observable_window() {
    let mut f = Fuzzer::new(SEEDS[0]);
    let entries = 512usize;
    let mut values: Vec<u64> = (0..entries).map(|_| f.next_u64() >> 16).collect();
    values.sort_unstable();
    values.dedup();
    let actual = values.len();
    let encoded = delta_varint::encode(&values).expect("valid encoding");

    // Above the ceiling: the limit must reject even though the bytes are
    // perfectly well formed and carry exactly `actual` entries.
    for ceiling in [0usize, 1, actual / 2, actual - 1] {
        let limit = delta_varint::EntryLimit::new(ceiling);
        let outcome = delta_varint::decode(&encoded, actual, limit);
        assert!(
            outcome.is_err(),
            "a well-formed {actual}-entry payload must be rejected under a ceiling of {ceiling}"
        );
    }
    // At and above the true size: accepted, and exact.
    for ceiling in [actual, actual + 1, 4_096] {
        let limit = delta_varint::EntryLimit::new(ceiling);
        let decoded = delta_varint::decode(&encoded, actual, limit)
            .unwrap_or_else(|e| panic!("ceiling {ceiling} must admit {actual} entries: {e:?}"));
        assert_eq!(
            decoded, values,
            "ceiling {ceiling}: admitted decode drifted from the encoded values"
        );
    }
}

// ===========================================================================
// origin-order key — a fixed-length durable key decoder
// ===========================================================================

/// A fixed-length key decoder must reject every length but its own, and must
/// round-trip its own encoding. Lengths are swept exhaustively around the
/// boundary rather than sampled, because that is where a `!=` written as `<`
/// hides.
#[test]
fn origin_birth_order_key_rejects_every_wrong_length_and_never_panics() {
    for &seed in &SEEDS {
        let mut f = Fuzzer::new(seed);
        let mut accepted_lengths = Vec::new();
        for len in 0..80usize {
            for _ in 0..8 {
                let input = f.bytes(len);
                let first = OriginBirthOrder::<VId>::try_from_canonical_be_key(&input);
                let second = OriginBirthOrder::<VId>::try_from_canonical_be_key(&input);
                assert_eq!(
                    first.is_ok(),
                    second.is_ok(),
                    "seed {seed}: key decode not deterministic at length {len}"
                );
                if first.is_ok() && !accepted_lengths.contains(&len) {
                    accepted_lengths.push(len);
                }
            }
        }
        assert!(
            accepted_lengths.len() <= 1,
            "seed {seed}: a fixed-length key decoder accepted {} distinct lengths: {accepted_lengths:?}",
            accepted_lengths.len()
        );
        for case in CORPUS {
            let _ = OriginBirthOrder::<VId>::try_from_canonical_be_key(case);
        }
    }
}

/// Every accepted key must re-encode to the exact bytes it was decoded from —
/// the round-trip that makes an order-preserving durable key trustworthy.
#[test]
fn origin_birth_order_accepted_keys_re_encode_to_their_input() {
    for &seed in &SEEDS {
        let mut f = Fuzzer::new(seed);
        let mut round_trips = 0usize;
        for len in 30..46usize {
            for _ in 0..64 {
                let input = f.bytes(len);
                if let Ok(order) = OriginBirthOrder::<VId>::try_from_canonical_be_key(&input) {
                    let re_encoded = order.canonical_be_key();
                    assert_eq!(
                        re_encoded.as_slice(),
                        input.as_slice(),
                        "seed {seed}: accepted key did not re-encode to its input: {}",
                        hex(&input)
                    );
                    round_trips += 1;
                }
            }
        }
        assert!(
            round_trips > 0,
            "seed {seed}: no key was ever accepted, so the round-trip proves nothing"
        );
    }
}

// ===========================================================================
// cross-decoder sweep — every corpus entry through every decoder
// ===========================================================================

/// The corpus is deliberately shared: a byte string that is a boundary for one
/// reader is frequently a boundary for another, and a decoder should not care
/// which reader an input was written for. This is the cheapest way to catch a
/// decoder that panics on an input shape nobody thought to hand it.
#[test]
fn every_corpus_entry_survives_every_decoder() {
    let limit = delta_varint::EntryLimit::new(1_024);
    for (index, case) in CORPUS.iter().enumerate() {
        let _ = varint::decode_u64_prefix(case);
        let _ = varint::decode_u64(case);
        let _ = OriginBirthOrder::<VId>::try_from_canonical_be_key(case);
        for width in [0u8, 1, 7, 8, 63, 64, 65] {
            for count in [0usize, 1, 8, 1_024, usize::MAX] {
                if let Ok(values) = bitpack::decode(case, count, width) {
                    assert_eq!(
                        values.len(),
                        count,
                        "corpus[{index}]: bitpack accepted a wrong-length decode"
                    );
                }
                let _ = bitpack::decode_for(case, count, u64::MAX, width);
                if let Ok(values) = delta_varint::decode(case, count, limit) {
                    assert_eq!(
                        values.len(),
                        count,
                        "corpus[{index}]: delta-varint accepted a wrong-length decode"
                    );
                }
            }
        }
    }
}
