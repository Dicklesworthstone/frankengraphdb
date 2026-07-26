//! The island kernel, adapted into the safe seam that owns its semantics
//! (bead `fgdb-w1-unsafe-islands-eqrq`).
//!
//! `fgdb-collections::probe` defines what a control-group classifier means:
//! sixteen lanes, lane `n` to bit `n`, and `ControlGroupDispatch` as the
//! copyable handle a probe loop takes. This island implements that function
//! over a raw `[u8; 16]` and depends on nothing at runtime — so "compatible
//! with `ControlGroupDispatch`" would otherwise be a claim made in prose.
//!
//! Here it is made in code: the adapter below is the entire glue a consumer
//! needs, it type-checks against the real `ControlGroupClassifyFn`, and the
//! resulting dispatch is differenced against `SCALAR_CONTROL_GROUP_DISPATCH` —
//! the oracle read from its owner rather than restated.

use fgdb_collections::probe::{
    CONTROL_GROUP_WIDTH, ControlGroup, ControlGroupDispatch, ControlGroupMasks, ControlTag,
    DELETED_CONTROL, EMPTY_CONTROL, LaneMask, SCALAR_CONTROL_GROUP_DISPATCH,
    SWAR_CONTROL_GROUP_DISPATCH,
};

/// The whole adapter. Safe, total, and free of raw pointers — which is the
/// island's API contract: a `forbid(unsafe_code)` crate can consume the vector
/// kernel without writing a single `unsafe`.
fn island_classify(group: &ControlGroup, tag: ControlTag) -> ControlGroupMasks {
    let masks = fgdb_unsafe_simd::classify(group.lanes(), tag.get());
    ControlGroupMasks {
        matching: LaneMask::from_bits(masks.matching),
        empty: LaneMask::from_bits(masks.empty),
        deleted: LaneMask::from_bits(masks.deleted),
    }
}

/// Const-constructed exactly as `SCALAR_CONTROL_GROUP_DISPATCH` is, which is
/// what makes the compatibility claim structural rather than incidental.
const ISLAND_CONTROL_GROUP_DISPATCH: ControlGroupDispatch =
    ControlGroupDispatch::new(island_classify);

#[test]
fn the_islands_control_bytes_are_the_collections_control_bytes() {
    // A silent disagreement here would make every mask below agree about the
    // wrong thing.
    assert_eq!(fgdb_unsafe_simd::CONTROL_GROUP_WIDTH, CONTROL_GROUP_WIDTH);
    assert_eq!(fgdb_unsafe_simd::EMPTY_CONTROL, EMPTY_CONTROL);
    assert_eq!(fgdb_unsafe_simd::DELETED_CONTROL, DELETED_CONTROL);
}

#[test]
fn the_island_dispatch_equals_the_scalar_dispatch_on_every_uniform_group() {
    for control in u8::MIN..=u8::MAX {
        let group = ControlGroup::new([control; CONTROL_GROUP_WIDTH]);
        for raw in u8::MIN..EMPTY_CONTROL {
            let tag = ControlTag::new(raw).expect("tags below EMPTY_CONTROL are occupied");
            let expected = SCALAR_CONTROL_GROUP_DISPATCH.classify(&group, tag);
            assert_eq!(
                ISLAND_CONTROL_GROUP_DISPATCH.classify(&group, tag),
                expected,
                "control {control:#04x} tag {raw:#04x}"
            );
        }
    }
}

#[test]
fn the_island_dispatch_preserves_probe_lane_order_through_a_real_gather() {
    // Wraparound gathers are how a probe loop actually reaches a group, so the
    // agreement is checked through `gather_wrapping` rather than only on
    // hand-built lane arrays.
    let controls: Vec<u8> = (0..64_u8)
        .map(|byte| match byte % 7 {
            0 => EMPTY_CONTROL,
            1 => DELETED_CONTROL,
            other => other * 9,
        })
        .collect();
    let tag = ControlTag::new(0x12).expect("occupied tag");
    for start in 0..controls.len() {
        let group = ControlGroup::gather_wrapping(&controls, start).expect("power-of-two controls");
        let expected = SCALAR_CONTROL_GROUP_DISPATCH.classify(&group, tag);
        let island = ISLAND_CONTROL_GROUP_DISPATCH.classify(&group, tag);
        assert_eq!(island, expected, "start {start}");
        // First lane in probe order is the property a caller actually depends
        // on, so it is asserted directly rather than inferred from mask equality.
        assert_eq!(island.matching.first(), expected.matching.first());
        assert_eq!(island.empty.first(), expected.empty.first());
        assert_eq!(island.occupied().bits(), expected.occupied().bits());
    }
}

#[test]
fn the_island_agrees_with_both_safe_backends_on_a_seeded_stream() {
    // Two independent oracles: the scalar reference and the portable SWAR
    // implementation. Agreeing with both is a stronger statement than agreeing
    // with either, and it is free.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..4_096 {
        let lanes = core::array::from_fn(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 32) as u8
        });
        let group = ControlGroup::new(lanes);
        let tag = ControlTag::from_hash(state);
        let expected = SCALAR_CONTROL_GROUP_DISPATCH.classify(&group, tag);
        assert_eq!(SWAR_CONTROL_GROUP_DISPATCH.classify(&group, tag), expected);
        assert_eq!(ISLAND_CONTROL_GROUP_DISPATCH.classify(&group, tag), expected);
    }
}

#[test]
fn prefetching_a_control_array_changes_no_classification() {
    // §8.7: prefetch policy is physical only. The observable consequence is
    // that a probe sequence run with hints must equal the same sequence run
    // without them, mask for mask.
    let controls: Vec<u8> = (0..128_u8).map(|byte| byte.wrapping_mul(11)).collect();
    let tag = ControlTag::new(0x33).expect("occupied tag");
    let mut without = Vec::new();
    let mut with = Vec::new();
    for start in 0..controls.len() {
        let group = ControlGroup::gather_wrapping(&controls, start).expect("power-of-two controls");
        without.push(ISLAND_CONTROL_GROUP_DISPATCH.classify(&group, tag));
    }
    for start in 0..controls.len() {
        fgdb_unsafe_simd::prefetch_controls(&controls, (start + CONTROL_GROUP_WIDTH) % controls.len());
        let group = ControlGroup::gather_wrapping(&controls, start).expect("power-of-two controls");
        with.push(ISLAND_CONTROL_GROUP_DISPATCH.classify(&group, tag));
    }
    assert_eq!(with, without);
}
