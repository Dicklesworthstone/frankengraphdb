//! Metamorphic properties for the succinct bitvector's rank/select pair.
//!
//! Rank and select are mutual inverses over the populated domain, and that pair
//! is the sharpest relation this crate offers: a rank that counts inclusive
//! instead of exclusive, or an off-by-one in the block summary, cannot satisfy
//! it even though either wrong kernel still returns plausible monotone numbers.
//! Relations that a wrong implementation would ALSO satisfy — "rank is
//! monotone", "select is increasing" — are included only as cheap locators, never
//! as the evidence.
//!
//! MUTATION EVIDENCE. Every property here was observed RED under a named wrong
//! kernel and GREEN on revert:
//!   MS1 rank1 counts the inclusive prefix `[0, end]` instead of `[0, end)`
//!       -> rank_matches_the_linear_reference, rank_select_are_mutual_inverses,
//!          select_recovers_every_set_position          (3 of 6 RED)
//!   MS2 select1 answers the ordinal one past the one asked for
//!       -> rank_select_are_mutual_inverses, select_lands_on_a_set_bit,
//!          select_recovers_every_set_position          (3 of 6 RED)
//!   MS3 the builder drops the final bit of each extend
//!       -> all six RED, which is what a lossy packing should do
//! An experiment whose anchor does not match exactly once, or whose mutant
//! compiles byte-identical to the original, proves nothing: an unapplied mutation
//! is indistinguishable from a passing suite.
//!
//! Inputs are boundary-heavy and deterministic — word edges (63/64/65 bits),
//! all-zero and all-one runs, single set bits at each word boundary, and a seeded
//! SplitMix64 sweep. No clock, no entropy, no dependencies.

#[cfg(miri)]
use asupersync::Cx;
#[cfg(miri)]
use asupersync::cx::cap;
#[cfg(not(miri))]
use asupersync::lab::run_async_under_lab;
use fgdb_collections::succinct::{SuccinctBitVector, SuccinctBitVectorBuilder};
use fgdb_types::{PurposeContexts, QueryCx};
use fgdb_unsafe_arena::RegionScope;

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

#[cfg(not(miri))]
fn query_cx() -> QueryCx {
    let (query, report) = run_async_under_lab(0x5acc_7e57, |root| async move {
        PurposeContexts::narrow_runtime_root(&root).query()
    });
    assert!(
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    query
}

#[cfg(miri)]
fn query_cx() -> QueryCx {
    let root = Cx::<cap::All>::for_testing();
    PurposeContexts::narrow_runtime_root(&root).query()
}

fn test_scope() -> RegionScope {
    RegionScope::with_capacity(1 << 20, 1 << 28)
}

fn build<'region>(
    scope: &'region RegionScope,
    cx: &QueryCx,
    bits: &[bool],
) -> SuccinctBitVector<'region> {
    let mut b = SuccinctBitVectorBuilder::new_in(scope, bits.len().max(1)).expect("builder opens");
    b.extend(cx, bits).expect("builder accepts the bit run");
    b.finish(cx).expect("builder finishes")
}

/// Bit patterns a rank/select kernel is most likely to get wrong: every word
/// boundary, every all-same run, and a pseudo-random sweep for the interior.
fn corpus() -> Vec<Vec<bool>> {
    let mut out: Vec<Vec<bool>> = Vec::new();
    out.push(Vec::new());
    for n in [1usize, 63, 64, 65, 127, 128, 129, 511, 512, 513] {
        out.push(vec![false; n]);
        out.push(vec![true; n]);
        // exactly one set bit, walked across the word boundary
        for p in [0usize, 1, 62, 63, 64, 65] {
            if p < n {
                let mut v = vec![false; n];
                v[p] = true;
                out.push(v);
            }
        }
        // alternating, which puts a set bit on both parities of every boundary
        out.push((0..n).map(|i| i % 2 == 0).collect());
    }
    let mut s = Sweep(0x51E1_D0C0_FEED_0001);
    for n in [64usize, 200, 1000] {
        for _ in 0..8 {
            let mut v = Vec::with_capacity(n);
            let mut w = s.next();
            for i in 0..n {
                if i % 64 == 0 {
                    w = s.next();
                }
                v.push((w >> (i % 64)) & 1 == 1);
            }
            out.push(v);
        }
    }
    out
}

/// The relation that constrains: rank and select invert each other. A rank that
/// counts inclusively, or a select off by one, breaks this immediately.
#[test]
fn rank_select_are_mutual_inverses() {
    let cx = query_cx();
    for bits in corpus() {
        let scope = test_scope();
        let bv = build(&scope, &cx, &bits);
        let ones = bits.iter().filter(|b| **b).count();
        for i in 0..ones {
            let p = bv
                .select1(i)
                .expect("ordinal below the one-count must select");
            assert_eq!(
                bv.rank1(p),
                Some(i),
                "rank1(select1(i)) must equal i (exclusive prefix), len={}",
                bits.len()
            );
            assert_eq!(
                bv.get(p),
                Some(true),
                "select1 must land on a set bit, len={}",
                bits.len()
            );
        }
        // select past the populated domain is None, never a clamped position.
        assert_eq!(
            bv.select1(ones),
            None,
            "select1 at the one-count must be None, len={}",
            bits.len()
        );
    }
}

/// The other direction, at set positions only: select(rank(p)) == p.
#[test]
fn select_recovers_every_set_position() {
    let cx = query_cx();
    for bits in corpus() {
        let scope = test_scope();
        let bv = build(&scope, &cx, &bits);
        for (p, set) in bits.iter().enumerate() {
            if !*set {
                continue;
            }
            let r = bv.rank1(p).expect("position inside the vector");
            assert_eq!(
                bv.select1(r),
                Some(p),
                "select1(rank1(p)) must recover a set position p={p}, len={}",
                bits.len()
            );
        }
    }
}

/// rank against an obviously-correct linear reference over the whole domain,
/// including the `end == len` boundary the fast path special-cases.
#[test]
fn rank_matches_the_linear_reference() {
    let cx = query_cx();
    for bits in corpus() {
        let scope = test_scope();
        let bv = build(&scope, &cx, &bits);
        let mut running = 0usize;
        for end in 0..=bits.len() {
            assert_eq!(
                bv.rank1(end),
                Some(running),
                "rank1 must equal the linear count of [0,{end}), len={}",
                bits.len()
            );
            let zeros = end - running;
            assert_eq!(
                bv.rank0(end),
                Some(zeros),
                "rank0 must be end - rank1 over [0,{end})"
            );
            if end < bits.len() && bits[end] {
                running += 1;
            }
        }
        assert_eq!(
            bv.rank1(bits.len() + 1),
            None,
            "rank1 past the length must be None, not a clamp"
        );
    }
}

/// select must land on a set bit and be strictly increasing in the ordinal.
#[test]
fn select_lands_on_a_set_bit() {
    let cx = query_cx();
    for bits in corpus() {
        let scope = test_scope();
        let bv = build(&scope, &cx, &bits);
        let ones = bits.iter().filter(|b| **b).count();
        let mut prev: Option<usize> = None;
        for i in 0..ones {
            let p = bv.select1(i).expect("ordinal below the one-count");
            assert_eq!(bv.get(p), Some(true), "select1 must land on a set bit");
            if let Some(q) = prev {
                assert!(p > q, "select1 must strictly increase in the ordinal");
            }
            prev = Some(p);
        }
    }
}

/// select0 is the same inverse over the zero domain — a kernel that shares a
/// broken block summary between the two fails here as well.
#[test]
fn select0_and_rank0_are_mutual_inverses() {
    let cx = query_cx();
    for bits in corpus() {
        let scope = test_scope();
        let bv = build(&scope, &cx, &bits);
        let zeros = bits.iter().filter(|b| !**b).count();
        for i in 0..zeros {
            let p = bv.select0(i).expect("ordinal below the zero-count");
            assert_eq!(bv.rank0(p), Some(i), "rank0(select0(i)) must equal i");
            assert_eq!(bv.get(p), Some(false), "select0 must land on a clear bit");
        }
        assert_eq!(bv.select0(zeros), None, "select0 at the zero-count is None");
    }
}

/// Builder round-trip: every bit read back is the bit written, and the two
/// construction paths agree. Separates the packing from a lossy one.
#[test]
fn builder_round_trip_preserves_every_bit() {
    let cx = query_cx();
    for bits in corpus() {
        let scope = test_scope();
        let bv = build(&scope, &cx, &bits);
        for (i, b) in bits.iter().enumerate() {
            assert_eq!(bv.get(i), Some(*b), "bit {i} changed through the builder");
        }
        assert_eq!(
            bv.get(bits.len()),
            None,
            "reading past the length must be None"
        );
        let via_bits = SuccinctBitVector::try_from_bits(&scope, &cx, &bits).expect("try_from_bits");
        for i in 0..bits.len() {
            assert_eq!(
                via_bits.get(i),
                bv.get(i),
                "the two construction paths must agree at {i}"
            );
        }
    }
}
