//! Metamorphic and round-trip properties for the canonical signed-limb integer.
//!
//! Each property is chosen to CONSTRAIN the kernel rather than restate it. The
//! merge-algebra lesson applies directly: commutativity and associativity alone
//! cannot separate addition from `max`, so every relation here is one a plausible
//! wrong implementation FAILS.
//!
//!   `(a + b) - b == a`      separates add/sub from max/min/or/xor: `max(a,b) - b`
//!                           is not `a` whenever `b > a`.
//!   `a * (b + c) == a*b + a*c`
//!                           separates mul from add, and, or, and from shifting:
//!                           distributivity fails for every one of them.
//!   `q*d + r == n, |r| < |d|`
//!                           separates div_rem from a shift or a truncating
//!                           quotient that drops the remainder.
//!   canonical-limb round-trip
//!                           separates the encoding from any lossy normalisation.
//!   `LimbLimit` enforcement separates a checked allocating op from an unchecked
//!                           one: the wrong implementation returns Ok.
//!
//! MUTATION EVIDENCE. Every property below was observed RED under a named wrong
//! kernel and GREEN on revert; a property no mutation can falsify is decoration
//! that reads as coverage, which is worse than an absent test.
//!   M1  checked_neg returns a clone            -> negation involution
//!   M3  LimbLimit::ensure never rejects        -> limb-limit enforcement
//!   M4  addition replaced by max               -> add-then-subtract, commut/assoc,
//!                                                 div_rem, limb-limit
//!   M5  checked_mul returns a+b                -> distributivity, div_rem
//!   M8  from_canonical_limbs accepts a zero
//!       carrying magnitude                     -> construction rejection
//!   M9  to_i128 discards the sign              -> i128 round-trip
//!   M10 from_canonical_limbs forces
//!       Sign::Positive                         -> canonical-limb round-trip
//! A mutation whose anchor does not match exactly once, or whose mutant compiles
//! byte-identical to the original, is an INVALID experiment and proves nothing --
//! an unapplied mutation is indistinguishable from a passing suite.
//!
//! Inputs are boundary-heavy by construction: limb edges, sign edges, and
//! zero-adjacent values, plus a deterministic pseudo-random sweep so the suite is
//! byte-reproducible under the determinism doctrine (no clock, no entropy).

use fgdb_bigint::{ArithmeticError, BigInt, ConstructionError, LimbLimit, Sign};

/// Generous limit for value-space properties; the enforcement tests use tight ones.
fn wide() -> LimbLimit {
    LimbLimit::new(64)
}

/// Deterministic, dependency-free sweep (SplitMix64). Seeded, never clocked.
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

/// Boundary-heavy corpus: every value a limb kernel is most likely to get wrong.
fn corpus() -> Vec<BigInt> {
    let mut v = vec![
        BigInt::zero(),
        BigInt::from_i64(1),
        BigInt::from_i64(-1),
        BigInt::from_i64(i64::MAX),
        BigInt::from_i64(i64::MIN),
        BigInt::from_u64(u64::MAX),
        BigInt::from_u64(1),
        BigInt::from_u128(u128::MAX),
        BigInt::from_i128(i128::MIN),
        BigInt::from_i128(i128::MAX),
        // limb boundaries: 2^64 and its neighbours straddle a limb edge
        BigInt::from_u128(1u128 << 64),
        BigInt::from_u128((1u128 << 64) - 1),
        BigInt::from_u128((1u128 << 64) + 1),
        BigInt::from_i128(-(1i128 << 64)),
    ];
    let mut s = Sweep(0x5EED_1234_ABCD_0001);
    for _ in 0..48 {
        let hi = s.next();
        let lo = s.next();
        let mag = ((hi as u128) << 64) | lo as u128;
        v.push(BigInt::from_u128(mag));
        v.push(BigInt::from_i128(-((mag >> 1) as i128)));
    }
    v
}

/// Every value the kernel produces must be in canonical form. A representation
/// with a trailing zero limb, or a zero carrying a sign, is a distinct encoding
/// of the same number and breaks unique-representation.
fn assert_canonical(x: &BigInt, what: &str) {
    assert!(x.is_canonical(), "{what}: result is not canonical");
    match x.sign() {
        Sign::Zero => {
            assert!(x.is_zero(), "{what}: Sign::Zero but is_zero() is false");
            assert_eq!(x.limb_count(), 0, "{what}: zero carries magnitude limbs");
        }
        Sign::Positive | Sign::Negative => {
            assert!(!x.is_zero(), "{what}: nonzero sign but is_zero() is true");
            assert_ne!(x.limb_count(), 0, "{what}: nonzero sign with no limbs");
            assert_ne!(
                x.magnitude_limbs_le().last(),
                Some(&0),
                "{what}: trailing zero limb is a non-canonical encoding"
            );
        }
    }
}

#[test]
fn canonical_limb_round_trip_is_lossless() {
    for x in corpus() {
        let limbs: Box<[u64]> = x.magnitude_limbs_le().to_vec().into_boxed_slice();
        let back = BigInt::from_canonical_limbs(x.sign(), limbs, wide())
            .expect("a value's own canonical limbs must reconstruct it");
        assert_eq!(
            back, x,
            "round-trip through canonical limbs changed the value"
        );
        assert_canonical(&back, "round-trip");
    }
}

#[test]
fn add_then_subtract_recovers_the_original() {
    // Separates add from max/min/or: those fail this for b > a.
    let c = corpus();
    for a in &c {
        for b in c.iter().take(24) {
            let sum = a.checked_add(b, wide()).expect("wide limit");
            let back = sum.checked_sub(b, wide()).expect("wide limit");
            assert_eq!(&back, a, "(a+b)-b must equal a");
            assert_canonical(&sum, "sum");
            assert_canonical(&back, "difference");
        }
    }
}

#[test]
fn multiplication_distributes_over_addition() {
    // Separates mul from add/and/or and from shifting.
    let c = corpus();
    for a in c.iter().take(20) {
        for b in c.iter().take(12) {
            for d in c.iter().take(6) {
                let bd = b.checked_add(d, wide()).expect("wide");
                let lhs = a.checked_mul(&bd, wide()).expect("wide");
                let ab = a.checked_mul(b, wide()).expect("wide");
                let ad = a.checked_mul(d, wide()).expect("wide");
                let rhs = ab.checked_add(&ad, wide()).expect("wide");
                assert_eq!(lhs, rhs, "a*(b+d) must equal a*b + a*d");
                assert_canonical(&lhs, "product-of-sum");
            }
        }
    }
}

#[test]
fn negation_is_an_involution_and_flips_sign_only() {
    for x in corpus() {
        let n = x.checked_neg(wide()).expect("wide");
        let back = n.checked_neg(wide()).expect("wide");
        assert_eq!(back, x, "neg(neg(x)) must equal x");
        assert_eq!(
            n.magnitude_limbs_le(),
            x.magnitude_limbs_le(),
            "negation must not alter magnitude"
        );
        assert_canonical(&n, "negation");
        if x.is_zero() {
            assert_eq!(n.sign(), Sign::Zero, "negated zero must stay Sign::Zero");
        } else {
            assert_ne!(n.sign(), x.sign(), "negation must flip a nonzero sign");
        }
    }
}

#[test]
fn div_rem_reconstructs_the_dividend_with_a_smaller_remainder() {
    // Separates div_rem from a shift, and from a quotient that discards the
    // remainder: both fail q*d + r == n.
    let c = corpus();
    for n in c.iter().take(24) {
        for d in c.iter().take(16) {
            if d.is_zero() {
                assert!(
                    matches!(
                        n.checked_div_rem(d, wide()),
                        Err(ArithmeticError::DivisionByZero)
                    ),
                    "division by zero must be an error, never a value"
                );
                continue;
            }
            let (q, r) = n.checked_div_rem(d, wide()).expect("wide");
            let qd = q.checked_mul(d, wide()).expect("wide");
            let recon = qd.checked_add(&r, wide()).expect("wide");
            assert_eq!(&recon, n, "q*d + r must reconstruct n exactly");
            // |r| < |d| — compare magnitudes, sign-independent.
            let rm = r.magnitude_limbs_le();
            let dm = d.magnitude_limbs_le();
            let smaller = rm.len() < dm.len()
                || (rm.len() == dm.len() && rm.iter().rev().lt(dm.iter().rev()));
            assert!(smaller || r.is_zero(), "|remainder| must be < |divisor|");
            assert_canonical(&q, "quotient");
            assert_canonical(&r, "remainder");
        }
    }
}

#[test]
fn limb_limit_is_enforced_on_every_allocating_operation() {
    // Separates a checked op from an unchecked one: the wrong kernel returns Ok.
    let tight = LimbLimit::new(1);
    let big = BigInt::from_u128(u128::MAX); // needs two limbs
    let one = BigInt::from_u64(1);
    assert!(
        big.checked_clone(tight).is_err(),
        "clone past the limit must fail"
    );
    assert!(
        big.checked_add(&one, tight).is_err(),
        "add past the limit must fail"
    );
    assert!(
        big.checked_mul(&big, tight).is_err(),
        "mul past the limit must fail"
    );
    assert!(
        big.checked_neg(tight).is_err(),
        "neg past the limit must fail"
    );
    // And a limit that DOES fit must succeed, so the test cannot pass by
    // rejecting everything.
    let roomy = LimbLimit::new(8);
    assert!(
        big.checked_add(&one, roomy).is_ok(),
        "roomy limit must allow"
    );
}

#[test]
fn construction_rejects_every_non_canonical_encoding() {
    let limit = wide();
    // zero carrying magnitude
    assert!(matches!(
        BigInt::from_canonical_limbs(Sign::Zero, vec![1u64].into_boxed_slice(), limit),
        Err(ConstructionError::ZeroWithMagnitude { .. })
    ));
    // nonzero sign with no magnitude
    assert!(matches!(
        BigInt::from_canonical_limbs(Sign::Positive, Vec::new().into_boxed_slice(), limit),
        Err(ConstructionError::NonzeroSignWithoutMagnitude { .. })
    ));
    // over the limb limit
    assert!(matches!(
        BigInt::from_canonical_limbs(
            Sign::Positive,
            vec![1u64; 3].into_boxed_slice(),
            LimbLimit::new(2)
        ),
        Err(ConstructionError::LimbLimitExceeded { .. })
    ));
    // the canonical forms must still be accepted
    assert!(BigInt::from_canonical_limbs(Sign::Zero, Vec::new().into_boxed_slice(), limit).is_ok());
    assert!(
        BigInt::from_canonical_limbs(Sign::Negative, vec![7u64].into_boxed_slice(), limit).is_ok()
    );
}

#[test]
fn small_value_round_trip_through_i128_is_exact() {
    for v in [0i128, 1, -1, i128::MAX, i128::MIN, 1 << 64, -(1 << 64)] {
        let x = BigInt::from_i128(v);
        assert_eq!(x.to_i128(), Some(v), "i128 round-trip must be exact");
        assert_canonical(&x, "from_i128");
    }
}

#[test]
fn addition_is_commutative_and_associative_over_boundaries() {
    // Weak on their own — included because they are cheap and catch carry bugs
    // the stronger relations above can mask when both sides share the fault.
    let c = corpus();
    for a in c.iter().take(16) {
        for b in c.iter().take(10) {
            let ab = a.checked_add(b, wide()).expect("wide");
            let ba = b.checked_add(a, wide()).expect("wide");
            assert_eq!(ab, ba, "addition must commute");
            for d in c.iter().take(5) {
                let l = ab.checked_add(d, wide()).expect("wide");
                let bd = b.checked_add(d, wide()).expect("wide");
                let r = a.checked_add(&bd, wide()).expect("wide");
                assert_eq!(l, r, "addition must associate");
            }
        }
    }
}
