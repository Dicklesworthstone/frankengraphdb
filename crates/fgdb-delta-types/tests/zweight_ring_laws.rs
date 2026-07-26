//! The commutative-ring laws `ZWeight` must obey to be a Z-set weight.
//!
//! `zweight_promotion_boundary.rs` already covers the inline/promoted
//! representation edge — that a value promotes rather than wraps, demotes
//! exactly, and compares consistently across the boundary. This file covers
//! the other half: the ALGEBRA. A Z-set delta is only sound if its weights
//! form a commutative ring, because DBSP-style incremental evaluation
//! reassociates and reorders weight arithmetic freely — consolidation sums a
//! key's weights in whatever order it encounters them, and a retraction is
//! addition of a negation. If commutativity or distributivity is even subtly
//! wrong, an incremental result silently diverges from its batch equivalent
//! and no representation test notices.
//!
//! Every law here is proven RED under a named mutation of the kernel it
//! constrains; the mutation is recorded on the property it kills so a reader
//! can re-run the proof instead of trusting it.
//!
//! Inputs are deterministic and boundary-heavy: i128 extremes, both sides of
//! the promotion edge, zero and one, and a seeded SplitMix64 sweep. No clock,
//! no entropy, no dependency beyond the crate under test.

use fgdb_delta_types::{LimbLimit, ZWeight};

/// Wide enough that no law here is ever limb-bound; the limit's own
/// enforcement is a separate concern with its own test below.
fn wide() -> LimbLimit {
    LimbLimit::new(64)
}

/// Deterministic, dependency-free. Seeded, never clocked.
struct Sweep {
    state: u64,
    seed: u64,
}

impl Sweep {
    fn new(seed: u64) -> Self {
        Self { state: seed, seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Boundary-heavy: two thirds of draws land on or beside a representation
    /// edge, because that is where a wrong kernel stops agreeing with the
    /// arithmetic it is meant to implement.
    fn weight(&mut self) -> ZWeight {
        const EDGES: [i128; 12] = [
            0,
            1,
            -1,
            2,
            -2,
            i128::MAX,
            i128::MIN,
            i128::MAX - 1,
            i128::MIN + 1,
            i128::MAX / 2,
            i128::MIN / 2,
            1 << 100,
        ];
        let r = self.next_u64();
        let inline = if r.is_multiple_of(3) {
            // a mid-range value, signed
            let magnitude = i128::from(self.next_u64() >> 8);
            ZWeight::from_i128(if r.is_multiple_of(2) {
                magnitude
            } else {
                -magnitude
            })
        } else {
            ZWeight::from_i128(EDGES[(r >> 8) as usize % EDGES.len()])
        };
        // One draw in four is PROMOTED. Without this the sweep only ever
        // builds inline weights, so every promoted-representation arm of every
        // kernel goes unexercised — measured: a mutation turning the
        // promoted+promoted subtraction into an addition SURVIVED the whole
        // suite until this line existed.
        if r % 4 == 1 {
            let limit = LimbLimit::new(64);
            let big = ZWeight::from_i128(i128::MAX);
            let squared = big.checked_mul(&big, limit).expect("promote");
            squared.checked_add(&inline, limit).expect("offset")
        } else {
            inline
        }
    }

    fn context(&self) -> String {
        format!("seed {}", self.seed)
    }
}

const SEEDS: [u64; 5] = [1, 42, 0xDEAD_BEEF, 0x5EED_5EED_5EED_5EED, u64::MAX];

/// A weight big enough to force the promoted representation, built only
/// through the public API so the test never depends on internals.
fn promoted(seed: i128, limit: LimbLimit) -> ZWeight {
    let big = ZWeight::from_i128(i128::MAX);
    let mut acc = big.checked_mul(&big, limit).expect("promote");
    acc = acc
        .checked_add(&ZWeight::from_i128(seed), limit)
        .expect("offset");
    assert!(acc.is_promoted(), "the fixture must actually be promoted");
    acc
}

// ===========================================================================
// additive group
// ===========================================================================

/// a + b == b + a, including across the promotion boundary.
///
/// PROVEN RED BY: `ZWeight::checked_add`'s promoted arm computing
/// `lhs - rhs` instead of `lhs + rhs`. Commutativity is the cheapest law that
/// catches a swapped or negated operand, and it survives any amount of
/// representation testing because both operands are the same TYPE.
#[test]
fn addition_is_commutative_including_across_the_promotion_boundary() {
    let limit = wide();
    for &seed in &SEEDS {
        let mut g = Sweep::new(seed);
        for _ in 0..1_500 {
            let a = g.weight();
            let b = g.weight();
            let ab = a.checked_add(&b, limit);
            let ba = b.checked_add(&a, limit);
            match (ab, ba) {
                (Ok(x), Ok(y)) => {
                    assert_eq!(x, y, "{}: a+b != b+a for {a:?} and {b:?}", g.context())
                }
                (Err(_), Err(_)) => {}
                (x, y) => panic!(
                    "{}: addition disagreed on admissibility: {x:?} vs {y:?}",
                    g.context()
                ),
            }
        }
        // explicitly mix promoted and inline
        let p = promoted(0, limit);
        let q = ZWeight::from_i128(-7);
        assert_eq!(
            p.checked_add(&q, limit).unwrap(),
            q.checked_add(&p, limit).unwrap(),
            "{}: promoted + inline is not commutative",
            g.context()
        );
    }
}

/// (a + b) + c == a + (b + c). Consolidation sums a key's weights in
/// encounter order, so associativity is what makes that order irrelevant.
///
/// PROVEN RED BY: the same `lhs - rhs` mutation in the promoted add arm, and
/// independently by an add kernel that demotes without re-canonicalising.
#[test]
fn addition_is_associative() {
    let limit = wide();
    for &seed in &SEEDS {
        let mut g = Sweep::new(seed);
        for _ in 0..1_500 {
            let (a, b, c) = (g.weight(), g.weight(), g.weight());
            let left = a
                .checked_add(&b, limit)
                .and_then(|ab| ab.checked_add(&c, limit));
            let right = b
                .checked_add(&c, limit)
                .and_then(|bc| a.checked_add(&bc, limit));
            if let (Ok(x), Ok(y)) = (&left, &right) {
                assert_eq!(x, y, "{}: (a+b)+c != a+(b+c)", g.context());
            }
        }
    }
}

/// ZERO is the additive identity and does not perturb the representation.
///
/// PROVEN RED BY: an add kernel that promotes unconditionally instead of
/// demoting an in-range result — `a + 0` then compares equal but reports
/// `is_promoted()`, which breaks the canonical-form guarantee the module
/// documents.
#[test]
fn zero_is_the_additive_identity_and_preserves_canonical_form() {
    let limit = wide();
    for &seed in &SEEDS {
        let mut g = Sweep::new(seed);
        for _ in 0..1_000 {
            let a = g.weight();
            let sum = a.checked_add(&ZWeight::ZERO, limit).expect("a + 0");
            assert_eq!(sum, a, "{}: a + 0 != a", g.context());
            assert_eq!(
                sum.is_promoted(),
                a.is_promoted(),
                "{}: a + 0 changed the representation of {a:?}",
                g.context()
            );
            assert!(
                sum.is_canonical(),
                "{}: a + 0 left a non-canonical value",
                g.context()
            );
        }
    }
}

/// a + (-a) == 0, which is what makes a retraction exact. Includes i128::MIN,
/// whose negation is not representable inline and must promote.
///
/// PROVEN RED BY: `checked_neg` saturating at `i128::MIN` instead of
/// promoting. That mutation is invisible to any test that only negates
/// mid-range values, and it makes a retraction of the most negative weight
/// silently fail to cancel.
#[test]
fn every_weight_has_an_exact_additive_inverse() {
    let limit = wide();
    for &seed in &SEEDS {
        let mut g = Sweep::new(seed);
        for _ in 0..1_000 {
            let a = g.weight();
            let neg = a.checked_neg(limit).expect("negation is always exact");
            let sum = a.checked_add(&neg, limit).expect("a + (-a)");
            assert!(
                sum.is_zero(),
                "{}: a + (-a) = {sum:?}, not zero, for {a:?}",
                g.context()
            );
            assert_eq!(
                sum,
                ZWeight::ZERO,
                "{}: cancellation is not ZERO",
                g.context()
            );
        }
    }
    // the boundary case that a mid-range sweep cannot reach
    let min = ZWeight::from_i128(i128::MIN);
    let neg = min
        .checked_neg(limit)
        .expect("negating i128::MIN must promote");
    assert!(
        neg.is_promoted(),
        "negating i128::MIN must promote, not saturate"
    );
    assert!(
        min.checked_add(&neg, limit).expect("cancel").is_zero(),
        "i128::MIN + (-i128::MIN) must be exactly zero"
    );
}

/// Subtraction is exactly addition of the negation. Two kernels that are
/// supposed to agree are a standing invitation for one to drift.
///
/// PROVEN RED BY: `checked_sub`'s promoted arm computing `lhs + rhs`.
#[test]
fn subtraction_equals_addition_of_the_negation() {
    let limit = wide();
    for &seed in &SEEDS {
        let mut g = Sweep::new(seed);
        for _ in 0..1_500 {
            let a = g.weight();
            let b = g.weight();
            let direct = a.checked_sub(&b, limit);
            let vianeg = b
                .checked_neg(limit)
                .and_then(|nb| a.checked_add(&nb, limit));
            match (direct, vianeg) {
                (Ok(x), Ok(y)) => {
                    assert_eq!(x, y, "{}: a-b != a+(-b) for {a:?} and {b:?}", g.context())
                }
                (Err(_), Err(_)) => {}
                (x, y) => panic!(
                    "{}: sub and add-of-neg disagreed: {x:?} vs {y:?}",
                    g.context()
                ),
            }
        }
    }
}

// ===========================================================================
// multiplicative structure and distributivity
// ===========================================================================

/// a * b == b * a and ONE is the multiplicative identity.
///
/// PROVEN RED BY: `checked_mul` losing the sign of the right operand.
#[test]
fn multiplication_is_commutative_with_one_as_identity() {
    let limit = wide();
    for &seed in &SEEDS {
        let mut g = Sweep::new(seed);
        for _ in 0..1_000 {
            let a = g.weight();
            let b = g.weight();
            if let (Ok(ab), Ok(ba)) = (a.checked_mul(&b, limit), b.checked_mul(&a, limit)) {
                assert_eq!(ab, ba, "{}: a*b != b*a", g.context());
            }
            let one = a.checked_mul(&ZWeight::ONE, limit).expect("a * 1");
            assert_eq!(one, a, "{}: a * 1 != a for {a:?}", g.context());
            let zero = a.checked_mul(&ZWeight::ZERO, limit).expect("a * 0");
            assert!(zero.is_zero(), "{}: a * 0 != 0 for {a:?}", g.context());
        }
    }
}

/// a * (b + c) == a*b + a*c. This is the law that ties the two operations
/// together, and the one a sign or promotion error in either kernel breaks
/// while both remain individually plausible.
///
/// PROVEN RED BY: the `checked_add` promoted-arm subtraction mutation (which
/// breaks the left side only) and independently by `checked_mul` dropping a
/// sign.
#[test]
fn multiplication_distributes_over_addition() {
    let limit = wide();
    for &seed in &SEEDS {
        let mut g = Sweep::new(seed);
        let mut exercised = 0usize;
        for _ in 0..1_500 {
            let (a, b, c) = (g.weight(), g.weight(), g.weight());
            let left = b
                .checked_add(&c, limit)
                .and_then(|bc| a.checked_mul(&bc, limit));
            let right = match (a.checked_mul(&b, limit), a.checked_mul(&c, limit)) {
                (Ok(ab), Ok(ac)) => ab.checked_add(&ac, limit),
                _ => continue,
            };
            if let (Ok(x), Ok(y)) = (&left, &right) {
                assert_eq!(x, y, "{}: a*(b+c) != a*b + a*c", g.context());
                exercised += 1;
            }
        }
        assert!(
            exercised > 100,
            "{}: distributivity ran only {exercised} times — the sweep is not \
             reaching admissible triples and proves little",
            g.context()
        );
    }
}

/// `checked_mul_i128` must agree with the full-weight multiply on the same
/// value. A scalar fast path is exactly where a kernel silently diverges.
///
/// PROVEN RED BY: `checked_mul_i128` ignoring the sign of its scalar factor.
#[test]
fn the_i128_scalar_fast_path_agrees_with_the_general_multiply() {
    let limit = wide();
    for &seed in &SEEDS {
        let mut g = Sweep::new(seed);
        for _ in 0..1_000 {
            let a = g.weight();
            let factor = match g.next_u64() % 6 {
                0 => 0,
                1 => 1,
                2 => -1,
                3 => i128::MAX,
                4 => i128::MIN,
                _ => i128::from(g.next_u64() as i64),
            };
            let fast = a.checked_mul_i128(factor, limit);
            let general = a.checked_mul(&ZWeight::from_i128(factor), limit);
            match (fast, general) {
                (Ok(x), Ok(y)) => assert_eq!(
                    x,
                    y,
                    "{}: scalar path disagrees for {a:?} * {factor}",
                    g.context()
                ),
                (Err(_), Err(_)) => {}
                (x, y) => panic!(
                    "{}: scalar and general path disagreed on admissibility for factor \
                     {factor}: {x:?} vs {y:?}",
                    g.context()
                ),
            }
        }
    }
}

// ===========================================================================
// the limb budget is a rejection, never a truncation
// ===========================================================================

/// A limit too small to hold an exact result must REJECT. The module states it
/// has "no wrapping, saturating, or approximate fallback", and that guarantee
/// is only worth anything if the narrow-limit path is exercised.
///
/// PROVEN RED BY: any `checked_*` that clamps to the limit instead of
/// returning the error — the operation then reports success with a value that
/// is not the arithmetic result.
#[test]
fn a_limb_budget_too_small_rejects_rather_than_approximating() {
    let wide_limit = wide();
    let narrow = LimbLimit::new(1);
    let big = promoted(0, wide_limit);

    // Under a wide budget this squares fine; under one limb it cannot.
    let squared_wide = big.checked_mul(&big, wide_limit);
    assert!(
        squared_wide.is_ok(),
        "the wide budget must admit the square"
    );
    let squared_narrow = big.checked_mul(&big, narrow);
    assert!(
        squared_narrow.is_err(),
        "a one-limb budget must reject a promoted square, not approximate it"
    );

    // And the rejection must not have mutated the operand.
    assert_eq!(
        big,
        promoted(0, wide_limit),
        "a rejected operation left its operand changed"
    );
}

/// Every value produced through the public API is canonical: demoted whenever
/// it fits inline. Equality and hashing both depend on it, so a single
/// non-canonical result poisons set membership downstream.
///
/// PROVEN RED BY: an add or mul kernel that skips demotion on the promoted
/// path. Equality still passes (the type compares by value), which is exactly
/// why `is_canonical` has to be asserted directly.
#[test]
fn every_result_of_the_public_api_is_canonical() {
    let limit = wide();
    for &seed in &SEEDS {
        let mut g = Sweep::new(seed);
        for _ in 0..1_000 {
            let a = g.weight();
            let b = g.weight();
            for result in [
                a.checked_add(&b, limit),
                a.checked_sub(&b, limit),
                a.checked_mul(&b, limit),
                a.checked_neg(limit),
                a.checked_clone(limit),
            ]
            .into_iter()
            .flatten()
            {
                assert!(
                    result.is_canonical(),
                    "{}: a public-API result is not canonical: {result:?}",
                    g.context()
                );
                // canonical means: promoted only when genuinely out of i128 range
                if result.is_promoted() {
                    assert!(
                        result.to_i128().is_none(),
                        "{}: {result:?} is promoted but fits in i128",
                        g.context()
                    );
                } else {
                    assert!(
                        result.to_i128().is_some(),
                        "{}: {result:?} is inline but does not convert to i128",
                        g.context()
                    );
                }
            }
        }
    }
}
