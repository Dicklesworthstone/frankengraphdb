//! Merge-algebra properties for the sketch families whose kernels were never
//! repaired after the original merge defect.
//!
//! Context that sets the standard for this file: a kernel with addition
//! replaced by `max` once passed thirty tests in this crate. Coverage that only
//! exercises a kernel does not constrain it. Every relation below is chosen to
//! FAIL under at least one plausible wrong kernel — `max`, `min`, or
//! `saturating_add` — and each was proven to fail by actually performing that
//! substitution, observing red, and reverting. The mutations are named in the
//! commit that introduced this file.
//!
//! The three families here have deliberately different algebras, so no single
//! uniform substitution can satisfy all of them:
//!
//! | family      | merge kernel                              |
//! |-------------|-------------------------------------------|
//! | `count_min` | element-wise checked **addition**          |
//! | `distinct`  | element-wise **max** of registers          |
//! | `zone_map`  | **min**-of-minima / **max**-of-maxima, and an additive count |
//!
//! `zone_map` is the sharpest case: its bounds and its count disagree about
//! which kernel is correct, so replacing "the kernel" uniformly must break one
//! side or the other.
//!
//! All inputs are fixed byte strings. No clock, no entropy, no new
//! dependencies, and boundary values are preferred over interior samples.

use fgdb_sketch::count_min::{
    CountMinError, CountMinHashAlgorithm, CountMinProfile, CountMinSketch,
};
use fgdb_sketch::distinct::{DistinctHashAlgorithm, DistinctProfile, DistinctSketch};
use fgdb_sketch::zone_map::{ByteZoneMap, ZoneMapProfile};

/// Deterministic keys chosen to collide across rows in a narrow sketch and to
/// include empty and maximal-byte edges.
fn keys() -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0x00],
        vec![0xFF],
        vec![0x00, 0x00],
        vec![0xFF, 0xFF],
    ];
    for index in 0_u16..64 {
        out.push(index.to_be_bytes().to_vec());
    }
    out
}

// -------------------------------------------------------------- count_min ---

fn count_min_profile() -> CountMinProfile {
    CountMinProfile {
        width: 64,
        depth: 4,
        hash_algorithm: CountMinHashAlgorithm::SeededFnvMix64V1,
        seed: 0x5350_4152_5345_0001,
        max_total_weight: 1_000_000,
        max_cells: 2_000,
    }
}

fn count_min_of(stream: &[(&[u8], u64)]) -> CountMinSketch {
    let mut sketch = CountMinSketch::try_new(count_min_profile()).expect("bounded profile");
    for &(key, weight) in stream {
        sketch
            .try_observe(key, weight)
            .expect("bounded observation");
    }
    sketch
}

#[test]
fn count_min_merge_is_a_homomorphism_over_stream_concatenation() {
    // merge(sketch(A), sketch(B)) must equal sketch(A ++ B), counter for
    // counter. Under `max` the merged counters would be the element-wise
    // maximum instead of the sum, which differs the moment both streams touch
    // one bucket.
    let left_stream: Vec<(&[u8], u64)> = vec![(b"alpha", 3), (b"beta", 5), (b"alpha", 7)];
    let right_stream: Vec<(&[u8], u64)> = vec![(b"beta", 11), (b"gamma", 13)];

    let mut merged = count_min_of(&left_stream);
    merged
        .try_merge(&count_min_of(&right_stream))
        .expect("compatible profiles");

    let mut concatenated = left_stream.clone();
    concatenated.extend_from_slice(&right_stream);
    let direct = count_min_of(&concatenated);

    assert_eq!(
        merged.canonical_state().counters,
        direct.canonical_state().counters,
        "merge must equal observing both streams into one sketch"
    );
    assert_eq!(
        merged.canonical_state().total_weight,
        direct.canonical_state().total_weight
    );
    assert_eq!(merged.canonical_state().total_weight, 3 + 5 + 7 + 11 + 13);
}

#[test]
fn count_min_self_merge_doubles_every_counter() {
    // The single sharpest separator from BOTH `max` and `min`: those kernels
    // are idempotent, so self-merge would leave the sketch unchanged. Addition
    // must double it exactly.
    let stream: Vec<(&[u8], u64)> = vec![(b"a", 1), (b"b", 2), (b"c", 4), (b"a", 8)];
    let original = count_min_of(&stream);
    let mut doubled = original.clone();
    doubled.try_merge(&original).expect("self merge");

    let before = original.canonical_state();
    let after = doubled.canonical_state();
    assert_eq!(
        after.total_weight,
        before.total_weight * 2,
        "self-merge must double the total weight, not leave it unchanged"
    );
    for (index, (&b, &a)) in before.counters.iter().zip(after.counters).enumerate() {
        assert_eq!(a, b * 2, "counter {index} must double under self-merge");
    }
    // At least one counter must be nonzero, or the relation above is vacuous
    // and would hold under an idempotent kernel too.
    assert!(
        before.counters.iter().any(|&c| c > 0),
        "fixture must populate counters for the doubling relation to constrain"
    );
}

#[test]
fn count_min_merge_overflows_rather_than_saturating() {
    // Separates checked addition from `saturating_add`: at the profile ceiling
    // the merge must REFUSE, not clamp. A saturating kernel would silently
    // succeed and report a wrong total.
    let profile = CountMinProfile {
        max_total_weight: 10,
        ..count_min_profile()
    };
    let mut left = CountMinSketch::try_new(profile).expect("bounded profile");
    left.try_observe(b"x", 6).expect("within bound");
    let mut right = CountMinSketch::try_new(profile).expect("bounded profile");
    right.try_observe(b"x", 6).expect("within bound");

    let before = left.canonical_state().counters.to_vec();
    let before_total = left.canonical_state().total_weight;

    assert_eq!(
        left.try_merge(&right),
        Err(CountMinError::WeightOverflow),
        "merging past the weight ceiling must error, not saturate"
    );
    assert_eq!(
        left.canonical_state().counters,
        before.as_slice(),
        "a refused merge must leave counters untouched"
    );
    assert_eq!(left.canonical_state().total_weight, before_total);
}

#[test]
fn count_min_merge_is_commutative_and_estimate_is_an_upper_bound() {
    let left_stream: Vec<(&[u8], u64)> = vec![(b"k1", 2), (b"k2", 3)];
    let right_stream: Vec<(&[u8], u64)> = vec![(b"k2", 5), (b"k3", 7)];

    let mut forward = count_min_of(&left_stream);
    forward
        .try_merge(&count_min_of(&right_stream))
        .expect("merge");
    let mut backward = count_min_of(&right_stream);
    backward
        .try_merge(&count_min_of(&left_stream))
        .expect("merge");
    assert_eq!(
        forward.canonical_state().counters,
        backward.canonical_state().counters,
        "merge must be commutative"
    );

    // Count-Min is a one-sided estimator: never below the true count.
    assert!(forward.estimate(b"k2") >= 3 + 5);
    assert!(forward.estimate(b"k1") >= 2);
    assert!(forward.estimate(b"k3") >= 7);
}

// --------------------------------------------------------------- distinct ---

fn distinct_profile() -> DistinctProfile {
    DistinctProfile {
        precision: 10,
        hash_algorithm: DistinctHashAlgorithm::SeededHasherV1,
        seed: 0x5350_4152_5345_0002,
        max_registers: 1 << 10,
    }
}

fn distinct_of(stream: &[Vec<u8>]) -> DistinctSketch {
    let mut sketch = DistinctSketch::try_new(distinct_profile()).expect("bounded profile");
    for key in stream {
        sketch.observe(key);
    }
    sketch
}

#[test]
fn distinct_self_merge_is_idempotent() {
    // The register kernel is `max`, so self-merge must be a no-op. Under
    // addition or saturating addition the registers would grow, which is what
    // this relation exists to catch.
    let stream = keys();
    let original = distinct_of(&stream);
    let mut merged = original.clone();
    merged.try_merge(&original).expect("self merge");

    assert_eq!(
        merged.canonical_state().registers,
        original.canonical_state().registers,
        "max-merge must be idempotent under self-merge"
    );
    assert!(
        original.canonical_state().registers.iter().any(|&r| r > 0),
        "fixture must set registers for idempotence to constrain"
    );
}

#[test]
fn distinct_merge_equals_observing_the_union_and_never_decreases_a_register() {
    let left: Vec<Vec<u8>> = keys().into_iter().take(40).collect();
    let right: Vec<Vec<u8>> = keys().into_iter().skip(20).collect();

    let left_sketch = distinct_of(&left);
    let mut merged = left_sketch.clone();
    merged.try_merge(&distinct_of(&right)).expect("merge");

    let mut union = left.clone();
    union.extend(right.iter().cloned());
    let direct = distinct_of(&union);
    assert_eq!(
        merged.canonical_state().registers,
        direct.canonical_state().registers,
        "merge must equal observing the union of both streams"
    );

    // Monotonicity separates `max` from `min`: no register may fall.
    for (index, (&before, &after)) in left_sketch
        .canonical_state()
        .registers
        .iter()
        .zip(merged.canonical_state().registers)
        .enumerate()
    {
        assert!(
            after >= before,
            "register {index} decreased under merge: {before} -> {after}"
        );
    }
}

// --------------------------------------------------------------- zone_map ---

fn zone_map_profile() -> ZoneMapProfile {
    ZoneMapProfile {
        max_value_bytes: 16,
        max_observations: 1_000,
    }
}

fn zone_map_of(values: &[&[u8]]) -> ByteZoneMap {
    let mut map = ByteZoneMap::new(zone_map_profile());
    for &value in values {
        map.try_observe(value).expect("bounded observation");
    }
    map
}

#[test]
fn zone_map_merge_keeps_extremal_bounds_while_counting_additively() {
    // The mixed algebra: bounds take min/max, the count adds. No single uniform
    // kernel satisfies both halves, which is why this one relation is the
    // strongest constraint in the file.
    let left = zone_map_of(&[b"m", b"q"]);
    let right = zone_map_of(&[b"a", b"z", b"a"]);

    let mut merged = left.clone();
    merged.try_merge(&right).expect("compatible profiles");
    let state = merged.canonical_state();

    assert_eq!(state.minimum, Some(b"a".as_slice()), "min-of-minima");
    assert_eq!(state.maximum, Some(b"z".as_slice()), "max-of-maxima");
    assert_eq!(
        state.count,
        2 + 3,
        "observation count must ADD across merge even though bounds do not"
    );
}

#[test]
fn zone_map_self_merge_doubles_the_count_but_leaves_bounds_fixed() {
    // One assertion pins both halves at once: an additive-bounds kernel would
    // move the endpoints, and a max/min count kernel would leave the count at 3.
    let original = zone_map_of(&[b"b", b"d", b"f"]);
    let mut merged = original.clone();
    merged.try_merge(&original).expect("self merge");
    let state = merged.canonical_state();

    assert_eq!(state.count, 6, "count must double under self-merge");
    assert_eq!(
        state.minimum,
        Some(b"b".as_slice()),
        "self-merge must not move the minimum"
    );
    assert_eq!(
        state.maximum,
        Some(b"f".as_slice()),
        "self-merge must not move the maximum"
    );
}

#[test]
fn zone_map_merge_is_commutative_and_bounds_contain_every_observation() {
    let values_left: Vec<&[u8]> = vec![b"\x00", b"hh"];
    let values_right: Vec<&[u8]> = vec![b"\xff\xff", b"cc"];

    let mut forward = zone_map_of(&values_left);
    forward
        .try_merge(&zone_map_of(&values_right))
        .expect("merge");
    let mut backward = zone_map_of(&values_right);
    backward
        .try_merge(&zone_map_of(&values_left))
        .expect("merge");

    assert_eq!(
        forward.canonical_state().minimum,
        backward.canonical_state().minimum
    );
    assert_eq!(
        forward.canonical_state().maximum,
        backward.canonical_state().maximum
    );
    assert_eq!(
        forward.canonical_state().count,
        backward.canonical_state().count
    );

    // Every observed value must fall inside the merged envelope.
    for value in values_left.iter().chain(&values_right) {
        assert!(
            forward.may_contain(value),
            "merged bounds must contain every observed value"
        );
    }
}

#[test]
fn zone_map_merging_an_empty_map_is_an_identity() {
    let populated = zone_map_of(&[b"k", b"p"]);
    let empty = zone_map_of(&[]);

    let mut left = populated.clone();
    left.try_merge(&empty).expect("merge with empty");
    assert_eq!(
        left.canonical_state().count,
        populated.canonical_state().count
    );
    assert_eq!(
        left.canonical_state().minimum,
        populated.canonical_state().minimum
    );
    assert_eq!(
        left.canonical_state().maximum,
        populated.canonical_state().maximum
    );

    let mut right = empty.clone();
    right.try_merge(&populated).expect("merge into empty");
    assert_eq!(
        right.canonical_state().count,
        populated.canonical_state().count
    );
    assert_eq!(
        right.canonical_state().minimum,
        populated.canonical_state().minimum
    );
    assert_eq!(
        right.canonical_state().maximum,
        populated.canonical_state().maximum
    );
}
