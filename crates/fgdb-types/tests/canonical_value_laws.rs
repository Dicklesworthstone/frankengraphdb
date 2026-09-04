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
    CanonicalBytes, CanonicalDecimal, CanonicalF64, CanonicalList, CanonicalMap, CanonicalMapEntry,
    CanonicalPropertyValue, CanonicalPropertyValueError, CanonicalScalar,
    CanonicalScalarCoercionError, CanonicalScalarKind, CanonicalScalarProfile,
    CanonicalScalarProfileError, CanonicalScalarProfileIdentityVerifier, CanonicalText,
    CanonicalTimestamp, CollationResolver, CollationResolverError, DecimalError,
    MAX_PROPERTY_VALUE_BYTES, MAX_TIMESTAMP_UTC_NANOS, MAX_UTC_OFFSET_SECONDS,
    MIN_TIMESTAMP_UTC_NANOS, NonBinaryTextBinding, ObjectId, TzdbResolver,
};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

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

    // The `Timestamp` arm was MISSING here, and its absence was invisible:
    // `arm_label` listed it, zero members landed in it, so
    // `hash_separates_distinct_values_within_every_arm` skipped it silently
    // while the clause bound to that test says "within every scalar arm".
    // Measured: a `CanonicalTimestamp::hash` that writes nothing left the whole
    // `fgdb-types` suite green. `temporal.rs` claims in its own doc comment that
    // "Equality, hashing, and ordering cover every stored semantic component in
    // field order: UTC instant, stored offset, then optional zone metadata" —
    // so the members below differ in the instant AND in the offset at a fixed
    // instant, which is what makes an offset-blind kernel observable.
    for (instant, offset) in [(0_i128, 0_i32), (0, 3_600), (1_000_000_000, 0)] {
        if let Ok(value) = CanonicalTimestamp::offset_only(instant, offset) {
            values.push(CanonicalScalar::Timestamp(value));
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
            // EQUALITY, bound explicitly rather than inferred from `Ord`'s
            // Equal cells. The clause on this test claims equality, ordering
            // and encoding are coherent, and `CanonicalF64` carries a
            // hand-written `PartialEq` AND a hand-written `Ord`, so the two can
            // disagree while every comparison above still passes. Measured: a
            // `PartialEq` that ignores the sign bit (making -1.0 == 1.0 while
            // `cmp` says Less) leaves this test green without this line.
            assert_eq!(
                left == right,
                value_order == core::cmp::Ordering::Equal,
                "`==` disagrees with `Ord::Equal` for {left:?} vs {right:?}"
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

/// **Equal values must hash identically** — the half of FG-INV-12's coherence
/// sentence that `equal_values_encode_to_identical_bytes` does not reach.
///
/// This is not a formality. `CanonicalF64` carries a HAND-WRITTEN `PartialEq`
/// and a HAND-WRITTEN `Hash` (`scalar.rs`), which is exactly the shape where
/// `Eq`/`Hash` drift apart silently: nothing in the compiler ties them, and a
/// kernel that compares on the numeric value while hashing on the raw bits
/// passes every encoding law in this file and still breaks every hash map in
/// the database. `CanonicalDecimal` has the same exposure through scale
/// normalisation, and `CanonicalText` through collation.
///
/// The differently-spelled pairs below are the ones that matter: two spellings
/// that compare equal but hash apart is the bug this law exists to catch.
///
/// **MEASURED, and the measurement decides what this test is for.** No mutation
/// of a `Hash` impl makes it red: every normalisation in this module happens at
/// CONSTRUCTION — `-0.0` collapses to `+0` bits, every NaN spelling to one
/// pattern, every decimal scale to one coefficient — so equal values are
/// already bit-identical by the time any `Hash` sees them, and the forward
/// implication holds for any hash that is a function of those bits. Three such
/// mutations (an empty `CanonicalF64::hash`, a discriminant-only
/// `CanonicalScalar::hash`, and a numeric-keyed float hash against a bit-keyed
/// `Eq`) all left it green, and the control below reds on all three.
///
/// It is NOT inert, and an earlier version of this comment wrongly said no
/// mutation could red it. It fires on an **`Eq` drift**: a `PartialEq` that
/// ignores the float sign bit — making `-1.0 == 1.0` while `Ord` still says
/// `Less` — turns this test red, because the pair then compares equal and
/// hashes apart. That is the one direction no other bound symbol in the spine
/// watches, so this is a live constraint on `Eq`, plus a regression lock for
/// the first type that normalises inside `Eq` rather than at construction.
#[test]
fn equal_values_hash_identically() {
    fn hash_of(value: &CanonicalScalar) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    let corpus = scalar_corpus();
    for left in &corpus {
        for right in &corpus {
            if left == right {
                assert_eq!(
                    hash_of(left),
                    hash_of(right),
                    "equal values {left:?} and {right:?} hash differently"
                );
            }
        }
    }

    // The spellings a derived Hash would never see, because they only compare
    // equal through hand-written normalisation.
    let pairs: Vec<(CanonicalScalar, CanonicalScalar)> = vec![
        (
            CanonicalScalar::Float(CanonicalF64::new(-0.0)),
            CanonicalScalar::Float(CanonicalF64::new(0.0)),
        ),
        (
            CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
            CanonicalScalar::Float(CanonicalF64::new(-f64::NAN)),
        ),
        (
            CanonicalScalar::Decimal(
                CanonicalDecimal::from_scaled_half_even(1, 0).expect("1 at scale 0"),
            ),
            CanonicalScalar::Decimal(
                CanonicalDecimal::from_scaled_half_even(1_000_000, 6).expect("1 at scale 6"),
            ),
        ),
    ];
    for (left, right) in &pairs {
        assert_eq!(
            left, right,
            "premise of this arm: {left:?} and {right:?} must compare equal"
        );
        assert_eq!(
            hash_of(left),
            hash_of(right),
            "equal spellings {left:?} and {right:?} hash differently"
        );
    }
}

/// **THE CONTROL that makes the law above mean something**, and the half that
/// actually constrains the kernel today.
///
/// A `Hash` that writes nothing, or writes only the enum discriminant,
/// satisfies "equal values hash identically" for every input in the universe.
/// Measured, not assumed: with only a whole-corpus "more than one distinct
/// hash" assertion, an empty `CanonicalF64::hash` AND a discriminant-only
/// `CanonicalScalar::hash` both passed, because distinct arms still differ.
/// So the assertion is made WITHIN each arm, where a hash that ignores its
/// payload has nowhere to hide.
///
/// This is not an injectivity claim. Hashes may collide, and asserting
/// otherwise in general would pin an implementation detail. It is a
/// non-vacuity claim over a fixed, boundary-heavy corpus: these particular
/// distinct values, which differ only in payload, must not collapse together.
///
/// **Proven to constrain, by mutation** (each reverted; the pristine tree is
/// green before and after): an empty `CanonicalF64::hash` reds the `Float`
/// arm; a discriminant-only `CanonicalScalar::hash` reds the `Bool` arm; and a
/// float hash keyed on the numeric value while `Eq` stays on the bits reds
/// `Float` by collapsing two distinct values that truncate alike. The first
/// two are exactly the shapes that satisfy "equal values hash identically"
/// while destroying every hash map in the database.
#[test]
fn hash_separates_distinct_values_within_every_arm() {
    fn hash_of(value: &CanonicalScalar) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    // Exhaustive on purpose, with no wildcard: adding a scalar arm must break
    // this build so the corpus is extended to cover it rather than silently
    // leaving the new arm unconstrained.
    fn arm_label(value: &CanonicalScalar) -> &'static str {
        match value {
            CanonicalScalar::Null => "Null",
            CanonicalScalar::Bool(_) => "Bool",
            CanonicalScalar::Int(_) => "Int",
            CanonicalScalar::Decimal(_) => "Decimal",
            CanonicalScalar::Float(_) => "Float",
            CanonicalScalar::Text(_) => "Text",
            CanonicalScalar::Timestamp(_) => "Timestamp",
            CanonicalScalar::Bytes(_) => "Bytes",
        }
    }

    let corpus = scalar_corpus();
    let mut arms: Vec<(&'static str, Vec<&CanonicalScalar>)> = Vec::new();
    for value in &corpus {
        let kind = arm_label(value);
        match arms.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, members)) => {
                if !members.contains(&value) {
                    members.push(value);
                }
            }
            None => arms.push((kind, vec![value])),
        }
    }

    let mut constrained_arms = 0usize;
    for (kind, members) in &arms {
        if members.len() < 2 {
            continue;
        }
        constrained_arms += 1;
        for (left_index, left) in members.iter().enumerate() {
            for (right_index, right) in members.iter().enumerate() {
                if left_index >= right_index {
                    continue;
                }
                assert_ne!(
                    hash_of(left),
                    hash_of(right),
                    "{kind}: distinct values {left:?} and {right:?} share a hash, so \
                     this arm's hash ignores its payload"
                );
            }
        }
    }
    assert!(
        constrained_arms >= 3,
        "control premise: at least three arms must hold two or more distinct \
         values, or this law quantifies over almost nothing; got {constrained_arms}"
    );
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

// ------------------------------------ family 4: profiled property values ---

const PROFILE_TZDB_OID: ObjectId = ObjectId([0x40; 32]);
const PROFILE_INSTANT: i128 = 1_735_689_600_123_456_789;

fn oid(fill: u8) -> ObjectId {
    ObjectId([fill; 32])
}

#[derive(Clone, Copy)]
struct ProfileResolver {
    missing: Option<ObjectId>,
}

impl ProfileResolver {
    const AVAILABLE: Self = Self { missing: None };

    const fn missing(object_id: ObjectId) -> Self {
        Self {
            missing: Some(object_id),
        }
    }
}

impl CollationResolver for ProfileResolver {
    fn artifact_available(&self, object_id: &ObjectId) -> bool {
        self.missing != Some(*object_id)
    }

    fn canonical_sort_key_len(
        &self,
        _: &NonBinaryTextBinding,
        text: &str,
    ) -> Result<usize, CollationResolverError> {
        text.len()
            .checked_add(1)
            .ok_or(CollationResolverError::new(1))
    }

    fn write_canonical_sort_key(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
        output: &mut [u8],
    ) -> Result<usize, CollationResolverError> {
        let expected = self.canonical_sort_key_len(binding, text)?;
        if output.len() != expected {
            return Err(CollationResolverError::new(2));
        }
        output[0] = binding.collation_oid.as_bytes()[0];
        output[1..].copy_from_slice(text.as_bytes());
        Ok(expected)
    }

    fn canonical_sort_key_matches(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
        candidate: &[u8],
    ) -> Result<bool, CollationResolverError> {
        Ok(
            candidate.first() == Some(&binding.collation_oid.as_bytes()[0])
                && candidate.get(1..) == Some(text.as_bytes()),
        )
    }
}

impl TzdbResolver for ProfileResolver {
    fn contains_tzdb(&self, tzdb_oid: &ObjectId) -> bool {
        (*tzdb_oid == PROFILE_TZDB_OID || *tzdb_oid == oid(0x41)) && self.missing != Some(*tzdb_oid)
    }

    fn canonical_utc_offset_seconds(
        &self,
        tzdb_oid: &ObjectId,
        zone_identifier: &str,
        instant_utc_nanos: i128,
    ) -> Option<i32> {
        ((*tzdb_oid == PROFILE_TZDB_OID || *tzdb_oid == oid(0x41))
            && zone_identifier == "America/New_York"
            && instant_utc_nanos == PROFILE_INSTANT)
            .then_some(-5 * 60 * 60)
    }
}

struct HashProfileIdentity;

impl CanonicalScalarProfileIdentityVerifier for HashProfileIdentity {
    fn verify_canonical_scalar_profile_oid(
        &self,
        claimed_oid: ObjectId,
        canonical_profile: &[u8],
    ) -> bool {
        claimed_oid == ObjectId(asupersync::atp::object::compute_hash(canonical_profile))
    }
}

fn canonical_profile(
    resolver: &ProfileResolver,
) -> Result<CanonicalScalarProfile, CanonicalScalarProfileError> {
    let descriptor = CanonicalScalarProfile::try_canonical_descriptor_bytes(
        oid(0x31),
        oid(0x32),
        oid(0x33),
        PROFILE_TZDB_OID,
        &[oid(0x35), oid(0x34)],
    )?;
    let profile_oid = ObjectId(asupersync::atp::object::compute_hash(&descriptor));
    CanonicalScalarProfile::try_new_verified(
        profile_oid,
        oid(0x31),
        oid(0x32),
        oid(0x33),
        PROFILE_TZDB_OID,
        &[oid(0x35), oid(0x34)],
        &HashProfileIdentity,
        resolver,
    )
}

fn ucs(value: &str) -> CanonicalText {
    CanonicalText::new_ucs_basic(value).expect("small UCS_BASIC fixture")
}

fn scalar_value(value: CanonicalScalar) -> CanonicalPropertyValue {
    CanonicalPropertyValue::Scalar(value)
}

fn map_value(entries: Vec<(&str, CanonicalPropertyValue)>) -> CanonicalPropertyValue {
    CanonicalPropertyValue::Map(
        CanonicalMap::try_new(
            entries
                .into_iter()
                .map(|(key, value)| CanonicalMapEntry::new(ucs(key), value))
                .collect(),
        )
        .expect("bounded unique Map fixture"),
    )
}

fn hash_profiled_property(
    profile: &CanonicalScalarProfile,
    value: &CanonicalPropertyValue,
    resolver: &ProfileResolver,
) -> Result<u64, CanonicalScalarProfileError> {
    let mut hasher = DefaultHasher::new();
    profile.hash_value(value, resolver, &mut hasher)?;
    Ok(hasher.finish())
}

#[derive(Default)]
struct RecordingHasher {
    bytes: Vec<u8>,
    write_calls: usize,
}

impl Hasher for RecordingHasher {
    fn finish(&self) -> u64 {
        0
    }

    fn write(&mut self, bytes: &[u8]) {
        self.write_calls += 1;
        self.bytes.extend_from_slice(bytes);
    }
}

#[test]
fn scalar_profile_identity_binds_rules_artifacts_and_canonical_collation_set()
-> Result<(), CanonicalScalarProfileError> {
    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;
    let forward = CanonicalScalarProfile::try_canonical_descriptor_bytes(
        oid(0x31),
        oid(0x32),
        oid(0x33),
        PROFILE_TZDB_OID,
        &[oid(0x34), oid(0x35)],
    )?;
    let reverse = CanonicalScalarProfile::try_canonical_descriptor_bytes(
        oid(0x31),
        oid(0x32),
        oid(0x33),
        PROFILE_TZDB_OID,
        &[oid(0x35), oid(0x34)],
    )?;
    assert_eq!(
        forward, reverse,
        "set order must not alter profile identity"
    );
    assert_eq!(
        profile.profile_oid(),
        ObjectId(asupersync::atp::object::compute_hash(&forward))
    );
    assert_eq!(
        profile.profile_oid(),
        ObjectId([
            0x15, 0xfa, 0x82, 0x70, 0xd5, 0xd3, 0x31, 0x07, 0xca, 0x32, 0x48, 0x6b, 0x62, 0x78,
            0xa2, 0xf4, 0xda, 0x79, 0x36, 0x71, 0x04, 0xca, 0x45, 0x2a, 0x09, 0x9c, 0xef, 0x41,
            0xd3, 0x75, 0x82, 0xe7,
        ]),
        "the frozen profile identity witnesses every fixed rule and artifact byte"
    );
    assert_eq!(profile.non_binary_collation_oids(), &[oid(0x34), oid(0x35)]);

    let changed = CanonicalScalarProfile::try_canonical_descriptor_bytes(
        oid(0x31),
        oid(0x32),
        oid(0x33),
        oid(0x41),
        &[oid(0x34), oid(0x35)],
    )?;
    assert_ne!(
        forward, changed,
        "changing an artifact must change the verified descriptor"
    );
    assert_eq!(
        CanonicalScalarProfile::try_new_verified(
            profile.profile_oid(),
            oid(0x31),
            oid(0x32),
            oid(0x33),
            oid(0x41),
            &[oid(0x34), oid(0x35)],
            &HashProfileIdentity,
            &resolver,
        ),
        Err(CanonicalScalarProfileError::ProfileIdentityUnverified {
            claimed: profile.profile_oid(),
        })
    );
    assert_eq!(
        CanonicalScalarProfile::try_canonical_descriptor_bytes(
            oid(0x31),
            oid(0x32),
            oid(0x33),
            PROFILE_TZDB_OID,
            &[oid(0x34), oid(0x34)],
        ),
        Err(CanonicalScalarProfileError::DuplicateCollation {
            object_id: oid(0x34),
        })
    );
    for (role, object_id) in [
        (
            fgdb_types::ScalarProfileArtifactRole::UnicodeData,
            oid(0x31),
        ),
        (
            fgdb_types::ScalarProfileArtifactRole::Normalization,
            oid(0x32),
        ),
        (
            fgdb_types::ScalarProfileArtifactRole::Segmentation,
            oid(0x33),
        ),
        (fgdb_types::ScalarProfileArtifactRole::Collation, oid(0x34)),
        (
            fgdb_types::ScalarProfileArtifactRole::Tzdb,
            PROFILE_TZDB_OID,
        ),
    ] {
        assert_eq!(
            canonical_profile(&ProfileResolver::missing(object_id)),
            Err(CanonicalScalarProfileError::MissingArtifact { role, object_id })
        );
    }
    Ok(())
}

#[test]
fn canonical_map_sorts_string_keys_by_ordered_scalar_bytes_and_rejects_duplicates() {
    let map = CanonicalMap::try_new(vec![
        CanonicalMapEntry::new(ucs("b"), scalar_value(CanonicalScalar::Int(3))),
        CanonicalMapEntry::new(ucs("aa"), scalar_value(CanonicalScalar::Int(2))),
        CanonicalMapEntry::new(ucs("a"), scalar_value(CanonicalScalar::Int(1))),
    ])
    .expect("bounded unique Map");
    let ordered_keys: Vec<&str> = map
        .entries()
        .iter()
        .map(|entry| entry.key().as_str())
        .collect();
    assert_eq!(ordered_keys, ["a", "aa", "b"]);

    let key_bytes: Vec<Vec<u8>> = map
        .entries()
        .iter()
        .map(|entry| {
            CanonicalScalar::Text(entry.key().clone())
                .encode()
                .expect("bounded key")
        })
        .collect();
    assert!(
        key_bytes.windows(2).all(|window| window[0] < window[1]),
        "stored Map order must be strictly increasing canonical scalar bytes"
    );

    assert_eq!(
        CanonicalMap::try_new(vec![
            CanonicalMapEntry::new(ucs("same"), scalar_value(CanonicalScalar::Int(1))),
            CanonicalMapEntry::new(ucs("same"), scalar_value(CanonicalScalar::Int(2))),
        ]),
        Err(CanonicalPropertyValueError::DuplicateMapKey)
    );
}

#[test]
fn canonical_map_order_and_bytes_agree_at_prefix_key_and_value_boundaries()
-> Result<(), CanonicalScalarProfileError> {
    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;
    let maps = [
        map_value(vec![]),
        map_value(vec![("a", scalar_value(CanonicalScalar::Int(0)))]),
        map_value(vec![
            ("a", scalar_value(CanonicalScalar::Int(0))),
            ("b", scalar_value(CanonicalScalar::Int(0))),
        ]),
        map_value(vec![("a", scalar_value(CanonicalScalar::Int(1)))]),
        map_value(vec![("b", scalar_value(CanonicalScalar::Int(0)))]),
    ];
    assert!(maps.windows(2).all(|pair| pair[0] < pair[1]));

    let encoded = maps
        .iter()
        .map(|value| profile.encode_value(value, &resolver))
        .collect::<Result<Vec<_>, _>>()?;
    assert!(encoded.windows(2).all(|pair| pair[0] < pair[1]));
    for (left_index, left) in maps.iter().enumerate() {
        for (right_index, right) in maps.iter().enumerate() {
            assert_eq!(
                profile.compare(left, right, &resolver)?,
                encoded[left_index].cmp(&encoded[right_index])
            );
            assert_eq!(
                left == right,
                encoded[left_index] == encoded[right_index],
                "distinct Map values must not alias canonical bytes"
            );
        }
    }
    Ok(())
}

#[test]
fn profiled_property_encoding_round_trips_and_byte_order_equals_value_order()
-> Result<(), CanonicalScalarProfileError> {
    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;
    let nested = map_value(vec![
        ("b", scalar_value(CanonicalScalar::Bool(true))),
        (
            "a",
            CanonicalPropertyValue::List(
                CanonicalList::try_new(vec![
                    scalar_value(CanonicalScalar::Null),
                    scalar_value(CanonicalScalar::Int(7)),
                ])
                .expect("bounded List"),
            ),
        ),
    ]);
    let corpus = [
        scalar_value(CanonicalScalar::Null),
        scalar_value(CanonicalScalar::Int(-1)),
        scalar_value(CanonicalScalar::Int(1)),
        CanonicalPropertyValue::List(CanonicalList::try_new(vec![]).expect("empty List")),
        CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![scalar_value(CanonicalScalar::Int(1))])
                .expect("bounded List"),
        ),
        nested,
    ];
    let encoded: Vec<Vec<u8>> = corpus
        .iter()
        .map(|value| profile.encode_value(value, &resolver))
        .collect::<Result<_, _>>()?;

    for (value, bytes) in corpus.iter().zip(&encoded) {
        let decoded = profile.decode_value_with_resolver(bytes, &resolver)?;
        assert_eq!(&decoded, value);
        assert_eq!(
            hash_profiled_property(&profile, &decoded, &resolver)?,
            hash_profiled_property(&profile, value, &resolver)?
        );
        assert_eq!(profile.encode_value(&decoded, &resolver)?, *bytes);
    }
    for (left_index, left) in corpus.iter().enumerate() {
        for (right_index, right) in corpus.iter().enumerate() {
            assert_eq!(
                profile.compare(left, right, &resolver)?,
                encoded[left_index].cmp(&encoded[right_index]),
                "profile bytes disagree with value order for {left:?} vs {right:?}"
            );
        }
    }

    let empty = CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![]).expect("empty List is within every structural bound"),
    );
    let one = CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![scalar_value(CanonicalScalar::Int(1))])
            .expect("one-element List is bounded"),
    );
    let one_zero = CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![
            scalar_value(CanonicalScalar::Int(1)),
            scalar_value(CanonicalScalar::Int(0)),
        ])
        .expect("two-element List is bounded"),
    );
    let two = CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![scalar_value(CanonicalScalar::Int(2))])
            .expect("one-element List is bounded"),
    );
    assert!(empty < one && one < one_zero && one_zero < two);
    let list_bytes = [empty, one, one_zero, two]
        .iter()
        .map(|value| profile.encode_value(value, &resolver))
        .collect::<Result<Vec<_>, _>>()?;
    assert!(list_bytes.windows(2).all(|pair| pair[0] < pair[1]));

    let golden = CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![
            scalar_value(CanonicalScalar::Null),
            scalar_value(CanonicalScalar::Int(1)),
        ])
        .expect("small golden List"),
    );
    assert_eq!(
        profile.encode_value(&golden, &resolver)?,
        vec![
            0x08, 0x01, 0x01, 0x00, 0x00, 0x01, 0x01, 0x02, 0x01, 0x80, 0x01, 0x00, 0x01, 0x00,
            0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x01, 0x00, 0x00,
        ],
        "nested canonical bytes are a frozen independent contract"
    );
    Ok(())
}

#[test]
fn profile_hash_feed_is_domain_separated_canonical_property_bytes()
-> Result<(), CanonicalScalarProfileError> {
    const HASH_DOMAIN: &[u8] = b"fgdb:canonical-property-value-hash:v1\0";

    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;
    let value = map_value(vec![(
        "key",
        CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![scalar_value(CanonicalScalar::Int(7))])
                .expect("small List"),
        ),
    )]);
    let canonical = profile.encode_value(&value, &resolver)?;
    let mut recording = RecordingHasher::default();
    profile.hash_value(&value, &resolver, &mut recording)?;

    let mut expected = Vec::from(HASH_DOMAIN);
    expected.extend_from_slice(&canonical);
    assert_eq!(recording.bytes, expected);
    assert_eq!(recording.write_calls, 1, "the profile binds one hash write");
    Ok(())
}

#[test]
fn property_decoder_rejects_unsorted_and_duplicate_map_bytes_instead_of_repairing()
-> Result<(), CanonicalScalarProfileError> {
    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;
    let one_a = profile.encode_value(
        &map_value(vec![("a", scalar_value(CanonicalScalar::Int(1)))]),
        &resolver,
    )?;
    let one_b = profile.encode_value(
        &map_value(vec![("b", scalar_value(CanonicalScalar::Int(2)))]),
        &resolver,
    )?;

    let mut poisoned_a = one_a[1..one_a.len() - 1].to_vec();
    let mut field_cursor = 1usize; // collection-item control precedes the key field
    while poisoned_a[field_cursor] != 0 {
        assert_eq!(poisoned_a[field_cursor], 1);
        field_cursor += 2;
    }
    let value_field = field_cursor + 1;
    assert_eq!(poisoned_a[value_field], 1);
    poisoned_a[value_field + 1] = 0xff; // decoded scalar tag is unknown

    let mut poisoned_single = vec![0x09];
    poisoned_single.extend_from_slice(&poisoned_a);
    poisoned_single.push(0);
    assert_eq!(
        profile.decode_value_with_resolver(&poisoned_single, &resolver),
        Err(CanonicalScalarProfileError::PropertyValue(
            CanonicalPropertyValueError::UnknownTag(0xff),
        ))
    );

    let mut unsorted = vec![0x09];
    unsorted.extend_from_slice(&one_b[1..one_b.len() - 1]);
    unsorted.extend_from_slice(&poisoned_a);
    unsorted.push(0);
    assert_eq!(
        profile.decode_value_with_resolver(&unsorted, &resolver),
        Err(CanonicalScalarProfileError::PropertyValue(
            CanonicalPropertyValueError::NonCanonicalEncoding,
        ))
    );

    let mut duplicate = vec![0x09];
    duplicate.extend_from_slice(&one_a[1..one_a.len() - 1]);
    duplicate.extend_from_slice(&poisoned_a);
    duplicate.push(0);
    assert_eq!(
        profile.decode_value_with_resolver(&duplicate, &resolver),
        Err(CanonicalScalarProfileError::PropertyValue(
            CanonicalPropertyValueError::DuplicateMapKey,
        ))
    );
    Ok(())
}

#[test]
fn property_decoder_is_total_over_seeded_malformed_bytes_within_bound()
-> Result<(), CanonicalScalarProfileError> {
    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;
    let mut state = 0x6a09_e667_f3bc_c909_u64;

    for length in 0..=128 {
        for _ in 0..32 {
            let mut bytes = Vec::with_capacity(length);
            for _ in 0..length {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                bytes.push((state >> 24) as u8);
            }
            let _result = profile.decode_value_with_resolver(&bytes, &resolver);
        }
    }
    Ok(())
}

#[test]
fn scalar_profile_coercion_is_closed_exact_and_separate_from_query_coercion()
-> Result<(), CanonicalScalarProfileError> {
    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;

    let decimal = profile
        .coerce_scalar(
            CanonicalScalar::Int(42),
            CanonicalScalarKind::Decimal,
            &resolver,
        )
        .expect("42 is exactly representable at scale 18");
    assert_eq!(
        decimal,
        CanonicalScalar::Decimal(CanonicalDecimal::from_integer(42).expect("exact decimal"))
    );
    assert_eq!(
        profile
            .coerce_scalar(decimal, CanonicalScalarKind::Int, &resolver)
            .expect("integral decimal is exactly representable"),
        CanonicalScalar::Int(42)
    );

    let largest_exact_integer = 9_999_999_999_999_999_i64;
    assert_eq!(
        profile
            .coerce_scalar(
                CanonicalScalar::Int(largest_exact_integer),
                CanonicalScalarKind::Decimal,
                &resolver,
            )
            .expect("the largest scale-18 integral decimal must be exact"),
        CanonicalScalar::Decimal(
            CanonicalDecimal::from_integer(i128::from(largest_exact_integer))
                .expect("boundary decimal fixture"),
        )
    );
    let first_inexact_integer = 10_000_000_000_000_000_i64;
    assert_eq!(
        profile.coerce_scalar(
            CanonicalScalar::Int(first_inexact_integer),
            CanonicalScalarKind::Decimal,
            &resolver,
        ),
        Err(CanonicalScalarCoercionError::Decimal(
            DecimalError::CoefficientOutOfRange {
                coefficient: 10_000_000_000_000_000_000_000_000_000_000_000,
                precision: 34,
            }
        ))
    );

    let fractional =
        CanonicalScalar::Decimal(CanonicalDecimal::from_coefficient(1).expect("small coefficient"));
    assert_eq!(
        profile.coerce_scalar(fractional, CanonicalScalarKind::Int, &resolver),
        Err(CanonicalScalarCoercionError::InexactNumeric {
            source: CanonicalScalarKind::Decimal,
            target: CanonicalScalarKind::Int,
        })
    );
    assert_eq!(
        profile.coerce_scalar(
            CanonicalScalar::Int(1),
            CanonicalScalarKind::Float,
            &resolver,
        ),
        Err(CanonicalScalarCoercionError::Unsupported {
            source: CanonicalScalarKind::Int,
            target: CanonicalScalarKind::Float,
        })
    );
    assert_eq!(
        profile.coerce_scalar(
            CanonicalScalar::Null,
            CanonicalScalarKind::Timestamp,
            &resolver,
        ),
        Err(CanonicalScalarCoercionError::Unsupported {
            source: CanonicalScalarKind::Null,
            target: CanonicalScalarKind::Timestamp,
        })
    );
    assert_eq!(
        profile
            .coerce_scalar(CanonicalScalar::Null, CanonicalScalarKind::Null, &resolver,)
            .expect("Null identity coercion"),
        CanonicalScalar::Null,
    );

    let owned_bytes = CanonicalBytes::new(vec![1, 2, 3, 4]).expect("small bounded Bytes");
    let source_allocation = owned_bytes.as_slice().as_ptr();
    let identity = profile
        .coerce_scalar(
            CanonicalScalar::Bytes(owned_bytes),
            CanonicalScalarKind::Bytes,
            &resolver,
        )
        .expect("Bytes identity coercion");
    let identity_allocation = match identity {
        CanonicalScalar::Bytes(bytes) => bytes.as_slice().as_ptr(),
        other => {
            assert!(
                matches!(&other, CanonicalScalar::Bytes(_)),
                "Bytes identity coercion returned {other:?}"
            );
            source_allocation
        }
    };
    assert_eq!(identity_allocation, source_allocation);
    Ok(())
}

#[test]
fn profile_rejects_value_artifacts_outside_its_exact_binding()
-> Result<(), CanonicalScalarProfileError> {
    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;
    let outside = NonBinaryTextBinding::new(oid(0x31), oid(0x32), oid(0x33), oid(0x36));
    let text = CanonicalText::new_non_binary("x", outside, &resolver)
        .expect("fixture resolver can derive the outside collation");
    let value = scalar_value(CanonicalScalar::Text(text));
    assert_eq!(
        profile.encode_value(&value, &resolver),
        Err(CanonicalScalarProfileError::CollationNotAdmitted { actual: oid(0x36) })
    );
    Ok(())
}

#[test]
fn recursive_profile_validation_rejects_every_nested_artifact_substitution()
-> Result<(), CanonicalScalarProfileError> {
    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;

    for (role, expected, binding) in [
        (
            fgdb_types::ScalarProfileArtifactRole::UnicodeData,
            oid(0x31),
            NonBinaryTextBinding::new(oid(0x36), oid(0x32), oid(0x33), oid(0x34)),
        ),
        (
            fgdb_types::ScalarProfileArtifactRole::Normalization,
            oid(0x32),
            NonBinaryTextBinding::new(oid(0x31), oid(0x36), oid(0x33), oid(0x34)),
        ),
        (
            fgdb_types::ScalarProfileArtifactRole::Segmentation,
            oid(0x33),
            NonBinaryTextBinding::new(oid(0x31), oid(0x32), oid(0x36), oid(0x34)),
        ),
    ] {
        let text = CanonicalText::new_non_binary("nested", binding, &resolver)
            .expect("fixture resolver derives every test binding");
        let value = map_value(vec![(
            "outer",
            CanonicalPropertyValue::List(
                CanonicalList::try_new(vec![scalar_value(CanonicalScalar::Text(text))])
                    .expect("small nested List"),
            ),
        )]);
        assert_eq!(
            profile.encode_value(&value, &resolver),
            Err(CanonicalScalarProfileError::ArtifactBindingMismatch {
                role,
                expected,
                actual: oid(0x36),
            })
        );

        let key = CanonicalText::new_non_binary("nested-key", binding, &resolver)
            .expect("fixture resolver derives every test key binding");
        let key_value = CanonicalPropertyValue::Map(
            CanonicalMap::try_new(vec![CanonicalMapEntry::new(
                key,
                scalar_value(CanonicalScalar::Null),
            )])
            .expect("small single-entry Map"),
        );
        assert_eq!(
            profile.encode_value(&key_value, &resolver),
            Err(CanonicalScalarProfileError::ArtifactBindingMismatch {
                role,
                expected,
                actual: oid(0x36),
            })
        );
    }

    let outside_collation = NonBinaryTextBinding::new(oid(0x31), oid(0x32), oid(0x33), oid(0x36));
    let text = CanonicalText::new_non_binary("nested", outside_collation, &resolver)
        .expect("fixture resolver derives the outside collation");
    let value = map_value(vec![(
        "outer",
        CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![scalar_value(CanonicalScalar::Text(text))])
                .expect("small nested List"),
        ),
    )]);
    assert_eq!(
        profile.encode_value(&value, &resolver),
        Err(CanonicalScalarProfileError::CollationNotAdmitted { actual: oid(0x36) })
    );
    let outside_key = CanonicalText::new_non_binary("nested-key", outside_collation, &resolver)
        .expect("fixture resolver derives the outside key collation");
    let key_value = CanonicalPropertyValue::Map(
        CanonicalMap::try_new(vec![CanonicalMapEntry::new(
            outside_key,
            scalar_value(CanonicalScalar::Null),
        )])
        .expect("small single-entry Map"),
    );
    assert_eq!(
        profile.encode_value(&key_value, &resolver),
        Err(CanonicalScalarProfileError::CollationNotAdmitted { actual: oid(0x36) })
    );

    let timestamp = CanonicalTimestamp::zoned(
        PROFILE_INSTANT,
        -5 * 60 * 60,
        "America/New_York",
        oid(0x41),
        &resolver,
    )
    .expect("alternate tzdb fixture is internally valid");
    let value = map_value(vec![(
        "outer",
        CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![scalar_value(CanonicalScalar::Timestamp(timestamp))])
                .expect("small nested List"),
        ),
    )]);
    assert_eq!(
        profile.encode_value(&value, &resolver),
        Err(CanonicalScalarProfileError::ArtifactBindingMismatch {
            role: fgdb_types::ScalarProfileArtifactRole::Tzdb,
            expected: PROFILE_TZDB_OID,
            actual: oid(0x41),
        })
    );
    Ok(())
}

#[test]
fn property_aggregate_size_is_enforced_before_the_looser_nesting_bound()
-> Result<(), CanonicalScalarProfileError> {
    let resolver = ProfileResolver::AVAILABLE;
    let profile = canonical_profile(&resolver)?;
    let mut value = scalar_value(CanonicalScalar::Null);
    for _ in 0..23 {
        value = CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![value]).expect("encoded size through depth 23 is admitted"),
        );
    }
    let largest_admitted = profile.encode_value(&value, &resolver)?;
    let largest_admitted_len = largest_admitted.len();
    drop(largest_admitted);
    assert_eq!(largest_admitted_len, 41_943_036);
    assert!(largest_admitted_len <= MAX_PROPERTY_VALUE_BYTES);
    assert_eq!(
        CanonicalMap::try_new(vec![CanonicalMapEntry::new(ucs("k"), value.clone())]),
        Err(CanonicalPropertyValueError::EncodedValueTooLarge {
            actual: 83_886_099,
            maximum: MAX_PROPERTY_VALUE_BYTES,
        })
    );
    assert_eq!(
        CanonicalList::try_new(vec![value]),
        Err(CanonicalPropertyValueError::EncodedValueTooLarge {
            actual: 83_886_076,
            maximum: MAX_PROPERTY_VALUE_BYTES,
        })
    );
    Ok(())
}
