//! Round-trip conformance for the generated durable encoder/decoder pairs.
//!
//! Registered checker symbol: `idr_generated_encoder_decoder_roundtrip`
//! (`registries/checker_index.toml`). Every pair asserts two properties:
//!
//! 1. **Round trip.** `decode(encode(x)) == x` across the value space, including
//!    both domain boundaries, not only interior samples.
//! 2. **Determinism.** The same input encodes to byte-identical output every
//!    time. Determinism is a stated project invariant (plan doctrine #4), so an
//!    encoder that round-trips but varies its bytes is still a defect here.
//!
//! Error paths are covered as first-class cases rather than as an afterthought:
//! truncated input, trailing bytes, non-minimal encodings and out-of-domain
//! discriminants must fail cleanly. A codec that silently accepts malformed
//! bytes is the failure this harness exists to catch, because such a decoder
//! loads without complaint and is wrong forever.
//!
//! The value space is generated deterministically — a fixed LCG, never an RNG —
//! so a failure reproduces exactly from the test name alone.

use fgdb_codec::{bitpack, delta_varint, identity, varint};
use fgdb_types::{CommitSeq, EId, VId};

/// Deterministic spread over the u64 domain. Fixed multiplier and increment so
/// every run visits the identical sequence; there is no seeding and no clock.
fn deterministic_u64s(count: usize) -> Vec<u64> {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.push(state);
    }
    out
}

/// Boundary values every unsigned codec must handle, plus every power-of-two
/// edge, which is where width- and continuation-byte logic changes behaviour.
fn u64_value_space() -> Vec<u64> {
    let mut values = vec![0, 1, 2, 127, 128, 129, 255, 256, u64::MAX - 1, u64::MAX];
    for shift in 0..64 {
        let bit = 1_u64 << shift;
        values.push(bit);
        values.push(bit.wrapping_sub(1));
        values.push(bit.wrapping_add(1));
    }
    values.extend(deterministic_u64s(256));
    values.sort_unstable();
    values.dedup();
    values
}

// ---------------------------------------------------------------- varint ----

#[test]
fn varint_round_trips_and_encodes_deterministically() {
    for value in u64_value_space() {
        let encoded = varint::encode_u64(value);
        let bytes = encoded.as_bytes().to_vec();

        let decoded = varint::decode_u64(&bytes)
            .unwrap_or_else(|error| panic!("decode failed for {value}: {error:?}"));
        assert_eq!(decoded, value, "round trip lost the value {value}");

        // Determinism: the same input must produce byte-identical output.
        let again = varint::encode_u64(value);
        assert_eq!(
            again.as_bytes(),
            bytes.as_slice(),
            "encoding {value} is not deterministic"
        );

        // The prefix decoder must agree with the whole-input decoder and must
        // report exactly the bytes it consumed.
        let (prefix_value, consumed) = varint::decode_u64_prefix(&bytes)
            .unwrap_or_else(|error| panic!("prefix decode failed for {value}: {error:?}"));
        assert_eq!(prefix_value, value);
        assert_eq!(
            consumed,
            bytes.len(),
            "consumed length disagrees for {value}"
        );
    }
}

#[test]
fn varint_rejects_empty_truncated_and_trailing_input() {
    assert!(
        varint::decode_u64(&[]).is_err(),
        "empty input must not decode"
    );

    // Truncation: drop the final byte of a multi-byte encoding. The remaining
    // prefix still has its continuation bit set, so the decoder must refuse
    // rather than return a partial value.
    let multi = varint::encode_u64(u64::MAX).as_bytes().to_vec();
    assert!(multi.len() > 1, "u64::MAX must need more than one byte");
    assert!(
        varint::decode_u64(&multi[..multi.len() - 1]).is_err(),
        "truncated input must not decode"
    );

    // Trailing bytes: a complete encoding followed by unrelated bytes is not a
    // valid whole-input encoding, even though its prefix decodes fine.
    let mut trailing = varint::encode_u64(1).as_bytes().to_vec();
    trailing.push(0x00);
    assert!(
        varint::decode_u64(&trailing).is_err(),
        "trailing bytes must not decode"
    );
    // ...and the prefix decoder must still succeed on exactly the same bytes,
    // consuming only the encoding. That contrast is the point of the pair.
    let (value, consumed) = varint::decode_u64_prefix(&trailing).expect("prefix must decode");
    assert_eq!(value, 1);
    assert!(consumed < trailing.len());
}

#[test]
fn varint_rejects_non_minimal_and_overlong_encodings() {
    // Non-minimal: 0 padded with a redundant continuation group. Canonical
    // encoding is a durability property, so accepting this would admit two
    // byte strings for one value.
    assert!(
        varint::decode_u64(&[0x80, 0x00]).is_err(),
        "non-minimal encoding of 0 must not decode"
    );

    // Overlong: more continuation bytes than u64 can hold must overflow rather
    // than wrap.
    let overlong = [0xFF_u8; 11];
    assert!(
        varint::decode_u64(&overlong).is_err(),
        "overlong input must not decode"
    );
}

// ---------------------------------------------------------- delta varint ----

#[test]
fn delta_varint_round_trips_and_encodes_deterministically() {
    let limit = delta_varint::EntryLimit::new(4096);
    let mut sequences: Vec<Vec<u64>> = vec![
        vec![],
        vec![0],
        vec![u64::MAX],
        vec![0, 0, 0],
        vec![0, 1, 2, 3],
        vec![0, u64::MAX],
    ];
    // Monotone non-decreasing runs, which is what this codec accepts.
    let mut running: u64 = 0;
    let mut spread = Vec::new();
    for step in deterministic_u64s(64) {
        running = running.saturating_add(step % 1_000_003);
        spread.push(running);
    }
    sequences.push(spread);

    for values in &mut sequences {
        let encoded = delta_varint::encode(values)
            .unwrap_or_else(|error| panic!("encode failed for {values:?}: {error:?}"));
        let decoded = delta_varint::decode(&encoded, values.len(), limit)
            .unwrap_or_else(|error| panic!("decode failed for {values:?}: {error:?}"));
        assert_eq!(&decoded, values, "round trip lost a delta-varint sequence");

        let again = delta_varint::encode(values).expect("re-encode must succeed");
        assert_eq!(again, encoded, "delta-varint encoding is not deterministic");
    }
}

#[test]
fn delta_varint_rejects_non_monotone_input_and_limit_violations() {
    // The codec stores successive differences, so a decreasing run has no
    // representation and must be refused at encode time.
    assert!(
        delta_varint::encode(&[5, 4]).is_err(),
        "non-monotone input must not encode"
    );

    let limit = delta_varint::EntryLimit::new(2);
    let encoded = delta_varint::encode(&[1, 2, 3]).expect("encode must succeed");
    assert!(
        delta_varint::decode(&encoded, 3, limit).is_err(),
        "a count above the entry limit must not decode"
    );

    // Truncation must fail rather than yield a short sequence.
    let generous = delta_varint::EntryLimit::new(4096);
    assert!(
        !encoded.is_empty(),
        "a three-entry encoding must not be empty"
    );
    assert!(
        delta_varint::decode(&encoded[..encoded.len() - 1], 3, generous).is_err(),
        "truncated delta-varint input must not decode"
    );
}

// --------------------------------------------------------------- bitpack ----

/// The largest value representable in `width` bits.
fn width_max(width: u8) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1_u64 << width) - 1
    }
}

#[test]
fn bitpack_round_trips_every_width_and_encodes_deterministically() {
    for width in 1_u8..=64 {
        let max = width_max(width);
        let mut values = vec![0, max];
        if max > 1 {
            values.push(1);
            values.push(max - 1);
        }
        for sample in deterministic_u64s(16) {
            values.push(if max == u64::MAX {
                sample
            } else {
                sample % (max + 1)
            });
        }

        let encoded = bitpack::encode(&values, width)
            .unwrap_or_else(|error| panic!("encode failed at width {width}: {error:?}"));
        let decoded = bitpack::decode(&encoded, values.len(), width)
            .unwrap_or_else(|error| panic!("decode failed at width {width}: {error:?}"));
        assert_eq!(decoded, values, "round trip lost values at width {width}");

        let again = bitpack::encode(&values, width).expect("re-encode must succeed");
        assert_eq!(
            again, encoded,
            "bitpack encoding is not deterministic at width {width}"
        );

        // The declared byte length must match what encoding actually produced.
        let expected = bitpack::expected_byte_len(values.len(), width)
            .expect("expected_byte_len must resolve");
        assert_eq!(
            encoded.len(),
            expected,
            "encoded length disagrees with expected_byte_len at width {width}"
        );
    }
}

#[test]
fn bitpack_frame_of_reference_round_trips_with_a_base() {
    for width in 1_u8..=32 {
        let span = width_max(width);
        let base = 1_000_000_u64;
        let values: Vec<u64> = [0, 1, span / 2, span]
            .into_iter()
            .map(|delta| base + delta)
            .collect();

        let encoded = bitpack::encode_for(&values, base, width)
            .unwrap_or_else(|error| panic!("encode_for failed at width {width}: {error:?}"));
        let decoded = bitpack::decode_for(&encoded, values.len(), base, width)
            .unwrap_or_else(|error| panic!("decode_for failed at width {width}: {error:?}"));
        assert_eq!(decoded, values, "frame-of-reference round trip failed");

        let again = bitpack::encode_for(&values, base, width).expect("re-encode must succeed");
        assert_eq!(again, encoded, "encode_for is not deterministic");
    }
}

#[test]
fn bitpack_width_zero_is_the_all_zero_encoding() {
    // The declared domain is the CLOSED range 0..=64, so width 0 is valid, not
    // an error: the only representable value is 0 and it costs no bytes. This
    // test pins that contract, because assuming width 0 was invalid is exactly
    // the kind of plausible-but-wrong expectation a conformance harness exists
    // to settle against the implementation rather than against intuition.
    let zeros = vec![0_u64; 8];
    let encoded = bitpack::encode(&zeros, 0).expect("width 0 must encode all-zero values");
    assert!(encoded.is_empty(), "width 0 must occupy no bytes");
    assert_eq!(
        bitpack::decode(&encoded, zeros.len(), 0).expect("width 0 must decode"),
        zeros,
        "width 0 must round trip the all-zero column"
    );
    // A nonzero value has no representation at width 0 and must be refused.
    assert!(
        bitpack::encode(&[1], 0).is_err(),
        "a nonzero value must not encode at width 0"
    );
}

#[test]
fn bitpack_rejects_invalid_widths_over_width_values_and_truncation() {
    assert!(
        bitpack::encode(&[0], 65).is_err(),
        "width above the closed 0..=64 domain must be rejected"
    );

    // A value that does not fit the declared width must be refused, not
    // silently truncated to the low bits.
    assert!(
        bitpack::encode(&[4], 2).is_err(),
        "a value wider than its declared width must not encode"
    );

    // Truncated payload must fail rather than decode a short run.
    let encoded = bitpack::encode(&[1, 2, 3], 8).expect("encode must succeed");
    assert!(
        bitpack::decode(&encoded[..encoded.len() - 1], 3, 8).is_err(),
        "truncated bitpack input must not decode"
    );
}

// -------------------------------------------------------------- identity ----

fn vid(bits: u128) -> VId {
    VId(bits)
}

fn eid(bits: u128) -> EId {
    EId(bits)
}

#[test]
fn origin_birth_order_key_round_trips_and_encodes_deterministically() {
    let scalars = [0_u64, 1, 2, u64::MAX - 1, u64::MAX];
    let identity_bits = [0_u128, 1, u128::MAX / 2, u128::MAX];

    for &commit in &scalars {
        for &intent in &scalars {
            for &merge in &scalars {
                for &bits in &identity_bits {
                    let vertex = identity::OriginBirthOrder::new(
                        CommitSeq(commit),
                        intent,
                        merge,
                        vid(bits),
                    );
                    let key = vertex.canonical_be_key();
                    assert_eq!(key.len(), identity::ORIGIN_BIRTH_ORDER_KEY_BYTES);

                    let decoded =
                        identity::OriginBirthOrder::<VId>::try_from_canonical_be_key(&key)
                            .expect("vertex key must decode");
                    assert_eq!(decoded, vertex, "vertex origin key round trip failed");
                    assert_eq!(
                        decoded.canonical_be_key(),
                        key,
                        "vertex origin key encoding is not deterministic"
                    );

                    let edge = identity::OriginBirthOrder::new(
                        CommitSeq(commit),
                        intent,
                        merge,
                        eid(bits),
                    );
                    let edge_key = edge.canonical_be_key();
                    let edge_decoded =
                        identity::OriginBirthOrder::<EId>::try_from_canonical_be_key(&edge_key)
                            .expect("edge key must decode");
                    assert_eq!(edge_decoded, edge, "edge origin key round trip failed");
                    assert_eq!(
                        edge_decoded.canonical_be_key(),
                        edge_key,
                        "edge origin key encoding is not deterministic"
                    );
                }
            }
        }
    }
}

#[test]
fn origin_birth_order_key_rejects_wrong_length_input() {
    let key = identity::OriginBirthOrder::new(CommitSeq(1), 2, 3, vid(4)).canonical_be_key();

    assert!(
        identity::OriginBirthOrder::<VId>::try_from_canonical_be_key(&key[..key.len() - 1])
            .is_err(),
        "a short key must not decode"
    );

    let mut long = key.to_vec();
    long.push(0);
    assert!(
        identity::OriginBirthOrder::<VId>::try_from_canonical_be_key(&long).is_err(),
        "a key with trailing bytes must not decode"
    );

    assert!(
        identity::OriginBirthOrder::<VId>::try_from_canonical_be_key(&[]).is_err(),
        "an empty key must not decode"
    );
}

#[test]
fn origin_birth_order_key_is_order_preserving_across_the_tuple() {
    // The key exists so that byte order equals tuple order; a round trip that
    // preserved values but broke ordering would still be a durable defect.
    let mut ordered = Vec::new();
    for commit in [0_u64, 1, u64::MAX] {
        for intent in [0_u64, 1, u64::MAX] {
            for merge in [0_u64, 1, u64::MAX] {
                for bits in [0_u128, 1, u128::MAX] {
                    ordered.push(identity::OriginBirthOrder::new(
                        CommitSeq(commit),
                        intent,
                        merge,
                        vid(bits),
                    ));
                }
            }
        }
    }
    ordered.sort();

    let keys: Vec<_> = ordered.iter().map(|o| o.canonical_be_key()).collect();
    for window in keys.windows(2) {
        assert!(
            window[0] <= window[1],
            "byte order must follow tuple order for the origin birth key"
        );
    }
}
