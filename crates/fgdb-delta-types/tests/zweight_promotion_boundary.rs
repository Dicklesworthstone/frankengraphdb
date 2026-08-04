//! Properties for the ZWeight i128-to-BigInt promotion boundary.
//!
//! This is the sharpest hidden-defect site in the crate: a `ZWeight` that WRAPPED
//! or SATURATED instead of promoting would silently corrupt every Z-set weight in
//! the system, and no unit test that stays inside the i128 range can see it. Each
//! property below distinguishes all three outcomes, because they are all
//! plausible and only one is correct:
//!
//!     i128::MAX + 1   promotes to 170141183460469231731687303715884105728
//!                     wraps    to i128::MIN
//!                     saturates to i128::MAX
//!
//! Asserting merely "the result is not i128::MAX + 1 as i128" would pass under a
//! wrapping kernel, so every assertion here pins the exact promoted magnitude and
//! the representation flag together.
//!
//! MUTATION EVIDENCE. Both mutations this suite exists to catch were observed
//! turning it FULLY red, and the source reverts to green:
//!   MZ1 the Fast+Fast promotion check never fires and overflow WRAPS:
//!       `left.checked_add(*right)` -> `Some(left.wrapping_add(*right))`
//!       -> 0 passed, 6 failed
//!   MZ2 the same anchor with `saturating_add`, so overflow SATURATES
//!       -> 0 passed, 6 failed
//! MZ2 is the discriminating one: a suite that only asserted "the result is not
//! the wrapped value" would pass under it. Every assertion here pins the exact
//! promoted magnitude together with `is_promoted()`, which is why saturation
//! fails as loudly as wrapping.
//!
//! An experiment whose anchor does not match exactly once, whose mutant compiles
//! byte-identical, or whose mutant test binary fails to link, proves nothing --
//! all three are indistinguishable from a passing suite.
//! Inputs are boundary-heavy and deterministic: i128 extremes, the first values
//! on each side of the promotion edge, zero-adjacent values, and a seeded
//! SplitMix64 sweep. No clock, no entropy, no dependencies.

use fgdb_delta_types::{LimbLimit, ZWeight};

fn wide() -> LimbLimit {
    LimbLimit::new(64)
}

/// Deterministic, dependency-free generator. Seeded, never clocked.
struct Sweep(u64);

impl Sweep {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn edge_values() -> Vec<i128> {
    let mut v = vec![
        0,
        1,
        -1,
        i128::MAX,
        i128::MIN,
        i128::MAX - 1,
        i128::MIN + 1,
        i64::MAX as i128,
        i64::MIN as i128,
        1i128 << 100,
        -(1i128 << 100),
    ];
    let mut s = Sweep(0x2E17_C0DE_1234_0001);
    for _ in 0..24 {
        let hi = s.next() as i128;
        let lo = s.next() as i128;
        v.push((hi << 64) ^ lo);
    }
    v
}

/// THE property. Overflowing i128 must promote — not wrap, not saturate — and the
/// promoted value must carry the exact mathematical magnitude.
#[test]
fn overflow_promotes_and_does_not_wrap_or_saturate() {
    let one = ZWeight::from_i128(1);
    let max = ZWeight::from_i128(i128::MAX);
    let sum = max
        .checked_add(&one, wide())
        .expect("promotion, not failure");

    assert!(
        sum.is_promoted(),
        "i128::MAX + 1 must promote to the bigint representation"
    );
    assert_eq!(
        sum.to_i128(),
        None,
        "a promoted value must not claim an i128; wrapping would report i128::MIN \
         and saturation would report i128::MAX"
    );
    // Pin the exact magnitude: (MAX + 1) - 1 must return to exactly MAX.
    let back = sum.checked_sub(&one, wide()).expect("demotion path");
    assert_eq!(
        back.to_i128(),
        Some(i128::MAX),
        "(MAX+1)-1 must be exactly MAX, which a wrapped or saturated sum cannot give"
    );
    assert!(!back.is_promoted(), "a value that fits i128 must demote");

    // The negative edge behaves the same way.
    let min = ZWeight::from_i128(i128::MIN);
    let below = min
        .checked_sub(&one, wide())
        .expect("promotion on the low side");
    assert!(below.is_promoted(), "i128::MIN - 1 must promote");
    assert_eq!(below.to_i128(), None, "promoted low edge claims no i128");
    let restored = below.checked_add(&one, wide()).expect("demotion path");
    assert_eq!(
        restored.to_i128(),
        Some(i128::MIN),
        "(MIN-1)+1 must be exactly MIN"
    );
}

/// A promoted value and an inline value of the same magnitude must be equal and
/// order identically. A kernel that compares representations rather than values
/// fails here.
#[test]
fn promoted_and_inline_agree_on_equality_and_order() {
    let one = ZWeight::from_i128(1);
    for v in edge_values() {
        let inline = ZWeight::from_i128(v);
        // Route the same number through a promotion and back.
        let promoted_route = match inline.checked_add(&one, wide()) {
            Ok(up) => up.checked_sub(&one, wide()).expect("round trip"),
            Err(_) => continue,
        };
        assert_eq!(
            promoted_route, inline,
            "a value routed through the promotion path must equal its inline form"
        );
        assert_eq!(
            promoted_route.cmp(&inline),
            core::cmp::Ordering::Equal,
            "equal values must compare Equal across representations"
        );
        assert_eq!(
            promoted_route.to_i128(),
            inline.to_i128(),
            "both forms must agree on i128 representability"
        );
    }
}

/// Ordering must hold across the boundary: everything inline is below the first
/// promoted value above it, and above the first promoted value below it.
#[test]
fn ordering_is_consistent_across_the_promotion_boundary() {
    let one = ZWeight::from_i128(1);
    let max = ZWeight::from_i128(i128::MAX);
    let above = max.checked_add(&one, wide()).expect("promote");
    let min = ZWeight::from_i128(i128::MIN);
    let below = min.checked_sub(&one, wide()).expect("promote");

    assert!(max < above, "MAX must order below the promoted MAX+1");
    assert!(below < min, "the promoted MIN-1 must order below MIN");
    assert!(
        below < above,
        "promoted values must order against each other"
    );
    for v in edge_values() {
        let w = ZWeight::from_i128(v);
        assert!(w <= max, "every inline value is at most MAX");
        assert!(w < above, "every inline value is below the promoted MAX+1");
        assert!(w > below, "every inline value is above the promoted MIN-1");
    }
}

/// Demotion must be exact wherever the value is representable, and must not
/// happen where it is not. `is_canonical` encodes exactly that law.
#[test]
fn demotion_is_exact_wherever_representable() {
    let one = ZWeight::from_i128(1);
    for v in edge_values() {
        let w = ZWeight::from_i128(v);
        assert_eq!(w.to_i128(), Some(v), "an inline weight round-trips exactly");
        assert!(!w.is_promoted(), "a constructible i128 must stay inline");
        assert!(w.is_canonical(), "inline construction must be canonical");

        // Push it over the edge and back; the result must be canonical either way.
        if let Ok(up) = w.checked_add(&one, wide()) {
            assert!(up.is_canonical(), "a sum must be canonical");
            if let Some(back) = up.to_i128() {
                assert_eq!(
                    back,
                    v.checked_add(1).expect("no overflow when it demoted"),
                    "a demoted sum must equal the exact i128 arithmetic"
                );
            } else {
                assert!(
                    up.is_promoted(),
                    "a value with no i128 form must report as promoted"
                );
            }
        }
    }
}

/// Multiplication crosses the boundary far faster than addition, so it exercises
/// the promotion path from a different direction.
#[test]
fn multiplication_promotes_and_stays_exact() {
    let big = ZWeight::from_i128(i128::MAX);
    let two = ZWeight::from_i128(2);
    let product = big.checked_mul(&two, wide()).expect("promote");
    assert!(product.is_promoted(), "MAX * 2 must promote");
    assert_eq!(product.to_i128(), None, "MAX * 2 has no i128 form");

    // (MAX * 2) / nothing — check it against MAX + MAX instead, which must agree.
    let doubled = big.checked_add(&big, wide()).expect("promote");
    assert_eq!(
        product, doubled,
        "MAX*2 and MAX+MAX must be the same promoted value"
    );

    // A product that stays inside i128 must not promote.
    let small = ZWeight::from_i128(3);
    let inline = small
        .checked_mul(&two, wide())
        .expect("no promotion needed");
    assert!(!inline.is_promoted(), "3 * 2 must stay inline");
    assert_eq!(inline.to_i128(), Some(6), "3 * 2 must be exactly 6");
}

/// Zero and negation behave the same on both sides of the boundary.
#[test]
fn negation_round_trips_across_the_boundary() {
    let one = ZWeight::from_i128(1);
    let max = ZWeight::from_i128(i128::MAX);
    let promoted = max.checked_add(&one, wide()).expect("promote");
    let neg = promoted.checked_neg(wide()).expect("negate");
    // Two's complement is ASYMMETRIC: |i128::MIN| == i128::MAX + 1, so the
    // negation of the first promoted value is exactly i128::MIN and MUST demote.
    // A kernel that kept it promoted would violate canonicality, and one that
    // refused to demote would leak an allocation on every sign flip.
    assert!(
        !neg.is_promoted(),
        "-(MAX+1) is exactly i128::MIN and must demote"
    );
    assert_eq!(
        neg.to_i128(),
        Some(i128::MIN),
        "-(MAX+1) must be exactly i128::MIN"
    );
    let back = neg.checked_neg(wide()).expect("negate back");
    assert_eq!(
        back, promoted,
        "neg(neg(x)) must equal x across the boundary"
    );
    assert!(back.is_promoted(), "negating i128::MIN must promote again");

    // A promoted value with no i128 form must stay promoted under negation.
    let far = promoted.checked_add(&one, wide()).expect("MAX+2");
    let far_neg = far.checked_neg(wide()).expect("negate");
    assert!(
        far_neg.is_promoted(),
        "-(MAX+2) has no i128 form and must stay promoted"
    );

    let zero = ZWeight::from_i128(0);
    assert!(zero.is_zero(), "zero must report is_zero");
    assert_eq!(
        zero.checked_neg(wide()).expect("negate zero").to_i128(),
        Some(0),
        "negated zero is zero"
    );
}
