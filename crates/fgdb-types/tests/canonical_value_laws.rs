//! Canonical-value laws for the core value spine.
//!
//! Three relation families, each chosen so an obvious wrong kernel fails:
//!
//! 1. **Canonical encoding** — `CanonicalScalar::encode` is documented as
//!    order-preserving: "ordinary bytewise lexicographic comparison is exactly
//!    `Ord` for every scalar arm". So the law is not merely that decode inverts
//!    encode, but that the byte order *equals* the value order. A truncating or
//!    endian-swapped encode round-trips perfectly and still breaks this, which
//!    is why round-trip alone would not constrain the kernel.
//! 2. **Collation** — `CanonicalText` ordering must be a strict total order:
//!    irreflexive, antisymmetric, and transitive. Transitivity over triples is
//!    what catches a comparator that is inconsistent on ties.
//! 3. **Canonical normalisation** — equal values must encode to *identical*
//!    bytes. The determinism doctrine rests on this: `-0.0` and `0.0`, or two
//!    spellings of one decimal, must not survive as distinct byte strings.
//!
//! Every relation below was proven to constrain by mutating the kernel,
//! observing red, and reverting. The mutations are named in the commit.
//!
//! Inputs are fixed and boundary-heavy — domain minima and maxima rather than
//! interior samples. No clock, no entropy, no new dependencies.

use fgdb_types::{
    CanonicalBytes, CanonicalDecimal, CanonicalF64, CanonicalScalar, CanonicalText,
    CanonicalTimestamp, MAX_TIMESTAMP_UTC_NANOS, MAX_UTC_OFFSET_SECONDS, MIN_TIMESTAMP_UTC_NANOS,
};

/// A boundary-heavy scalar set: every arm of the union, and for each numeric
/// arm the domain edges where sign handling and width handling break.
fn scalar_corpus() -> Vec<CanonicalScalar> {
    let mut values = vec![
        CanonicalScalar::Null,
        CanonicalScalar::Bool(false),
        CanonicalScalar::Bool(true),
    ];

    for int in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
        values.push(CanonicalScalar::Int(int));
    }

    for coefficient in [-1_i128, 0, 1, 1_000_000] {
        if let Ok(decimal) = CanonicalDecimal::from_coefficient(coefficient) {
            values.push(CanonicalScalar::Decimal(decimal));
        }
    }

    for float in [
        f64::NEG_INFINITY,
        -1.5,
        -1.0,
        -0.0,
        0.0,
        1.0,
        1.5,
        f64::INFINITY,
    ] {
        values.push(CanonicalScalar::Float(CanonicalF64::new(float)));
    }

    // "aa" vs "b" is the boundary that separates a memcomparable encoding from
    // a length-prefixed one: they differ in length, so a leading length field
    // would order them backwards.
    for text in ["", "a", "aa", "ab", "b", "\u{0}", "\u{10FFFF}"] {
        if let Ok(scalar) = CanonicalScalar::ucs_basic_text(text) {
            values.push(scalar);
        }
    }

    for bytes in [
        Vec::new(),
        vec![0x00],
        vec![0x00, 0x00],
        vec![0xFF],
        vec![0xFF, 0xFF],
    ] {
        if let Ok(scalar) = CanonicalBytes::new(bytes).map(CanonicalScalar::Bytes) {
            values.push(scalar);
        }
    }

    values
}

// ------------------------------------------------- family 1: encoding ------

#[test]
fn canonical_scalar_encoding_round_trips_every_arm_and_boundary() {
    for value in scalar_corpus() {
        let encoded = value
            .encode()
            .unwrap_or_else(|error| panic!("encode failed for {value:?}: {error:?}"));
        let decoded = CanonicalScalar::decode(&encoded)
            .unwrap_or_else(|error| panic!("decode failed for {value:?}: {error:?}"));
        assert_eq!(decoded, value, "round trip lost {value:?}");

        // Determinism: re-encoding is byte-identical.
        assert_eq!(
            value.encode().expect("re-encode"),
            encoded,
            "encoding {value:?} is not deterministic"
        );
    }
}

#[test]
fn canonical_scalar_encoding_is_injective() {
    // Two DISTINCT values must never share an encoding. A truncating encode
    // collapses domain edges together, which this catches directly.
    let corpus = scalar_corpus();
    let mut seen: Vec<(Vec<u8>, CanonicalScalar)> = Vec::new();
    for value in corpus {
        let encoded = value.encode().expect("encode");
        if let Some((_, other)) = seen.iter().find(|(bytes, _)| *bytes == encoded) {
            assert_eq!(
                &value, other,
                "distinct values {value:?} and {other:?} share an encoding"
            );
        }
        seen.push((encoded, value));
    }
}

#[test]
fn canonical_scalar_byte_order_equals_value_order() {
    // THE law for this encoding: bytewise lexicographic comparison must be
    // exactly `Ord`. Round-trip cannot see a violation here — an endian swap or
    // a missing sign-bit flip decodes perfectly and still orders wrongly.
    let corpus = scalar_corpus();
    let encoded: Vec<Vec<u8>> = corpus.iter().map(|v| v.encode().expect("encode")).collect();

    for (left_index, left) in corpus.iter().enumerate() {
        for (right_index, right) in corpus.iter().enumerate() {
            let value_order = left.cmp(right);
            let byte_order = encoded[left_index].cmp(&encoded[right_index]);
            assert_eq!(
                byte_order, value_order,
                "byte order disagrees with value order for {left:?} vs {right:?}"
            );
        }
    }
}

#[test]
fn signed_integer_encoding_orders_negatives_below_positives() {
    // The specific boundary a sign-bit flip exists to handle: without it, a
    // negative's two's-complement high bit sorts ABOVE every positive.
    let negative = CanonicalScalar::Int(i64::MIN).encode().expect("encode");
    let minus_one = CanonicalScalar::Int(-1).encode().expect("encode");
    let zero = CanonicalScalar::Int(0).encode().expect("encode");
    let positive = CanonicalScalar::Int(i64::MAX).encode().expect("encode");

    assert!(negative < minus_one, "i64::MIN must encode below -1");
    assert!(minus_one < zero, "-1 must encode below 0");
    assert!(zero < positive, "0 must encode below i64::MAX");
}

// ------------------------------------------------ family 2: collation ------

fn text_corpus() -> Vec<CanonicalText> {
    ["", "a", "aa", "ab", "b", "ba", "\u{0}", "z"]
        .into_iter()
        .filter_map(|value| CanonicalText::new_ucs_basic(value).ok())
        .collect()
}

#[test]
fn canonical_text_ordering_is_a_strict_total_order() {
    let corpus = text_corpus();

    // Irreflexive under strict comparison, and reflexive under equality.
    for value in &corpus {
        assert_eq!(value.cmp(value), core::cmp::Ordering::Equal);
    }

    // Antisymmetry: a < b implies b > a, with no pair claiming both.
    for left in &corpus {
        for right in &corpus {
            let forward = left.cmp(right);
            let backward = right.cmp(left);
            assert_eq!(
                forward,
                backward.reverse(),
                "comparator is not antisymmetric for {left:?} vs {right:?}"
            );
        }
    }

    // Transitivity over every triple. This is what catches a comparator that
    // is inconsistent on ties: such a comparator can be antisymmetric pairwise
    // and still admit a < b, b < c, c < a.
    for a in &corpus {
        for b in &corpus {
            for c in &corpus {
                if a <= b && b <= c {
                    assert!(
                        a <= c,
                        "transitivity broken: {a:?} <= {b:?} <= {c:?} but not {a:?} <= {c:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn text_orders_through_the_scalar_encoding_but_not_the_durable_one() {
    // Order preservation is a property of `CanonicalScalar::encode`, which uses
    // memcomparable groups. `CanonicalText::encode` is a different artefact: a
    // length-prefixed durable form (version, binding tag, 8-byte length, text),
    // and a leading length field CANNOT order "aa" below "b". Asserting order
    // on the durable form would be asserting a law the type never claimed, so
    // this test pins the real distinction instead.
    let corpus = text_corpus();
    for left in &corpus {
        for right in &corpus {
            let value_order = left.cmp(right);
            let scalar_order = CanonicalScalar::Text(left.clone())
                .encode()
                .expect("encode")
                .cmp(
                    &CanonicalScalar::Text(right.clone())
                        .encode()
                        .expect("encode"),
                );
            assert_eq!(
                scalar_order, value_order,
                "scalar text byte order must equal value order for {left:?} vs {right:?}"
            );
        }
    }

    // And the durable form genuinely does not order, which is why the scalar
    // encoding exists separately. Guard the claim with a concrete witness.
    let short = CanonicalText::new_ucs_basic("b").expect("text");
    let long = CanonicalText::new_ucs_basic("aa").expect("text");
    assert!(long < short, "value order puts \"aa\" below \"b\"");
    assert!(
        long.encode().expect("encode") > short.encode().expect("encode"),
        "the length-prefixed durable form is expected NOT to preserve that order"
    );
}

#[test]
fn canonical_text_round_trips_and_collation_equivalence_is_reflexive() {
    for text in text_corpus() {
        let encoded = text.encode().expect("encode");
        let decoded = CanonicalText::decode(&encoded).expect("decode");
        assert_eq!(
            decoded.as_str(),
            text.as_str(),
            "text round trip lost value"
        );
        assert!(
            text.is_collation_equivalent_to(&text),
            "collation equivalence must be reflexive"
        );
    }
}

// -------------------------------------------- family 3: normalisation ------

#[test]
fn equal_values_encode_to_identical_bytes() {
    // The determinism doctrine in one relation: if two values compare equal,
    // their canonical encodings must be byte-identical. A kernel that leaves
    // -0.0 or a non-normalised spelling distinct fails here even though it
    // round-trips each spelling faithfully.
    let corpus = scalar_corpus();
    for left in &corpus {
        for right in &corpus {
            if left == right {
                assert_eq!(
                    left.encode().expect("encode"),
                    right.encode().expect("encode"),
                    "equal values {left:?} and {right:?} encode differently"
                );
            }
        }
    }
}

#[test]
fn negative_zero_is_normalised_to_positive_zero() {
    let negative = CanonicalF64::new(-0.0);
    let positive = CanonicalF64::new(0.0);

    assert_eq!(
        negative.to_bits(),
        positive.to_bits(),
        "-0.0 must collapse to the +0 bit pattern"
    );
    assert_eq!(negative, positive, "-0.0 and 0.0 must compare equal");
    assert_eq!(
        CanonicalScalar::Float(negative).encode().expect("encode"),
        CanonicalScalar::Float(positive).encode().expect("encode"),
        "-0.0 and 0.0 must encode identically"
    );
}

#[test]
fn nan_is_canonicalised_to_a_single_bit_pattern() {
    // Two differently-spelled NaNs must not survive as distinct durable bytes.
    let quiet = CanonicalF64::new(f64::NAN);
    let negated = CanonicalF64::new(-f64::NAN);
    assert_eq!(
        quiet.to_bits(),
        negated.to_bits(),
        "every NaN must canonicalise to one pattern"
    );
    assert_eq!(
        CanonicalScalar::Float(quiet).encode().expect("encode"),
        CanonicalScalar::Float(negated).encode().expect("encode"),
    );
}

#[test]
fn non_canonical_float_bits_are_rejected_rather_than_repaired() {
    // Fail closed: a durable input that is already non-canonical must be
    // refused, not silently re-canonicalised, or two encodings of one value
    // could both be accepted.
    let minus_zero_bits = (-0.0_f64).to_bits();
    assert!(
        CanonicalF64::from_bits_canonical(minus_zero_bits).is_none(),
        "the -0.0 bit pattern must be rejected as non-canonical"
    );
    assert!(
        CanonicalF64::from_bits_canonical(0.0_f64.to_bits()).is_some(),
        "the +0 bit pattern must be accepted"
    );
}

#[test]
fn one_decimal_spelled_at_many_scales_normalises_to_one_encoding() {
    // STRICT_PORTABLE pins a single scale (18), so a decimal has exactly one
    // canonical coefficient no matter which scale it arrives at. This is the
    // law that makes "equal values have identical bytes" true for decimals:
    // 1.5 authored at scale 1, 3 or 6 must be ONE durable byte string.
    for (spellings, label) in [
        ([(15_i128, 1_u32), (1_500, 3), (1_500_000, 6)], "1.5"),
        ([(-15, 1), (-1_500, 3), (-1_500_000, 6)], "-1.5"),
        ([(0, 0), (0, 1), (0, 18)], "0"),
        ([(1, 0), (10, 1), (1_000_000, 6)], "1"),
    ] {
        let mut canonical: Option<(CanonicalDecimal, Vec<u8>)> = None;
        for (coefficient, scale) in spellings {
            let value = CanonicalDecimal::from_scaled_half_even(coefficient, scale)
                .unwrap_or_else(|e| panic!("{label} at scale {scale} rejected: {e:?}"));
            let bytes = CanonicalScalar::Decimal(value).encode().expect("encode");
            match &canonical {
                None => canonical = Some((value, bytes)),
                Some((first_value, first_bytes)) => {
                    assert_eq!(
                        &value, first_value,
                        "{label} at scale {scale} is not the same canonical value"
                    );
                    assert_eq!(
                        &bytes, first_bytes,
                        "{label} at scale {scale} encodes to different bytes"
                    );
                }
            }
        }
    }
}

#[test]
fn decimal_rescale_ties_go_to_even_not_up_and_not_truncated() {
    // The exact-half cases are the only ones where half-even, half-up and
    // truncation disagree, so they are the whole discriminating power of the
    // rounding kernel. At scale 19 the discarded digit is a bare 5:
    //   2.5 -> 2 (even, stays)      half-up would give 3, truncation gives 2
    //   3.5 -> 4 (odd, rounds up)   half-up gives 4,     truncation gives 3
    // Asserting BOTH separates half-even from half-up AND from truncation.
    for (source, expected, why) in [
        (25_i128, 2_i128, "2.5 ties down to even"),
        (35, 4, "3.5 ties up to even"),
        (15, 2, "1.5 ties up to even"),
        (45, 4, "4.5 ties down to even"),
        (26, 3, "above the tie always rounds up"),
        (24, 2, "below the tie always rounds down"),
        (-25, -2, "ties to even is sign-symmetric"),
        (-35, -4, "ties to even is sign-symmetric"),
    ] {
        let value = CanonicalDecimal::from_scaled_half_even(source, 19).expect("rescale");
        assert_eq!(
            value.coefficient(),
            expected,
            "{why}: {source} at scale 19 should normalise to {expected}"
        );
    }
}

#[test]
fn decimal_equality_implies_identical_encoding() {
    let corpus: Vec<CanonicalDecimal> = [-1_i128, 0, 1, 1_000_000]
        .into_iter()
        .filter_map(|c| CanonicalDecimal::from_coefficient(c).ok())
        .collect();

    for left in &corpus {
        for right in &corpus {
            if left == right {
                assert_eq!(
                    CanonicalScalar::Decimal(*left).encode().expect("encode"),
                    CanonicalScalar::Decimal(*right).encode().expect("encode"),
                    "equal decimals must encode identically"
                );
            }
        }
    }

    // Ordering must survive the encoding for decimals too, including across
    // the sign boundary.
    let mut sorted = corpus.clone();
    sorted.sort();
    let encoded: Vec<Vec<u8>> = sorted
        .iter()
        .map(|d| CanonicalScalar::Decimal(*d).encode().expect("encode"))
        .collect();
    for window in encoded.windows(2) {
        assert!(
            window[0] <= window[1],
            "decimal byte order must follow value order"
        );
    }
}

/// Offset-only timestamps at the domain edges, plus the offset extremes. No
/// clock is read: every instant here is a literal.
fn timestamp_corpus() -> Vec<CanonicalTimestamp> {
    let mut values = Vec::new();
    for instant in [
        MIN_TIMESTAMP_UTC_NANOS,
        MIN_TIMESTAMP_UTC_NANOS + 1,
        -1_000_000_000,
        -1,
        0,
        1,
        1_000_000_000,
        MAX_TIMESTAMP_UTC_NANOS - 1,
        MAX_TIMESTAMP_UTC_NANOS,
    ] {
        for offset in [
            -MAX_UTC_OFFSET_SECONDS,
            -3_600,
            0,
            3_600,
            MAX_UTC_OFFSET_SECONDS,
        ] {
            if let Ok(value) = CanonicalTimestamp::offset_only(instant, offset) {
                values.push(value);
            }
        }
    }
    values
}

#[test]
fn timestamp_round_trips_and_equal_values_encode_identically() {
    let corpus = timestamp_corpus();
    assert!(
        corpus.len() >= 40,
        "corpus must actually populate, got {}",
        corpus.len()
    );

    for value in &corpus {
        let encoded = value.encode().expect("encode");
        let decoded = CanonicalTimestamp::decode(&encoded).expect("decode");
        assert_eq!(&decoded, value, "timestamp round trip lost {value:?}");
        assert_eq!(
            value.encode().expect("re-encode"),
            encoded,
            "timestamp encoding is not deterministic"
        );
    }

    for left in &corpus {
        for right in &corpus {
            let left_bytes = left.encode().expect("encode");
            let right_bytes = right.encode().expect("encode");
            // Equal values MUST share bytes, and distinct values must not:
            // otherwise the instant/offset pair is not fully covered.
            assert_eq!(
                left == right,
                left_bytes == right_bytes,
                "byte identity must track value equality for {left:?} vs {right:?}"
            );
        }
    }
}

#[test]
fn timestamp_ordering_is_by_utc_instant_not_by_wall_clock() {
    // The documented contract orders "UTC instant, stored offset, then zone".
    // The obvious wrong kernel orders by local wall time, which is what a
    // human reading a formatted timestamp would do. These two disagree exactly
    // when a smaller instant carries a larger offset, so build that case.
    let earlier_instant_later_wall =
        CanonicalTimestamp::offset_only(0, MAX_UTC_OFFSET_SECONDS).expect("timestamp");
    let later_instant_earlier_wall =
        CanonicalTimestamp::offset_only(1_000_000_000, -MAX_UTC_OFFSET_SECONDS).expect("timestamp");

    assert!(
        earlier_instant_later_wall.local_wall_nanos()
            > later_instant_earlier_wall.local_wall_nanos(),
        "fixture must actually invert wall clock against instant, or it proves nothing"
    );
    assert!(
        earlier_instant_later_wall < later_instant_earlier_wall,
        "ordering must follow the UTC instant, never the local wall clock"
    );
    assert!(
        earlier_instant_later_wall.encode().expect("encode")
            < later_instant_earlier_wall.encode().expect("encode"),
        "the encoding must order by UTC instant too"
    );

    // And the same instant orders by stored offset, which keeps the order
    // total across values that denote one moment in different offsets.
    let west = CanonicalTimestamp::offset_only(0, -3_600).expect("timestamp");
    let utc = CanonicalTimestamp::offset_only(0, 0).expect("timestamp");
    assert!(west < utc, "equal instants must order by stored offset");
    assert_ne!(
        west.encode().expect("encode"),
        utc.encode().expect("encode"),
        "offset is a semantic component and must reach the bytes"
    );
}

#[test]
fn timestamp_local_wall_is_exactly_instant_plus_offset() {
    // Total and exact by construction — no saturation, no drift at the edges.
    for value in timestamp_corpus() {
        let expected =
            value.instant_utc_nanos() + i128::from(value.utc_offset_seconds()) * 1_000_000_000_i128;
        assert_eq!(
            value.local_wall_nanos(),
            expected,
            "local wall drifted for {value:?}"
        );
    }
}
