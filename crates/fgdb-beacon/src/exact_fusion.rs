//! Exact two-lane RRF over explicitly bounded candidate populations.
//!
//! This does not make ANN, truncated candidates, BM25 arithmetic, or an
//! unauthorized input corpus exact/authorized. It replaces only the fusion
//! comparison and score-rendering stage. There is no durable format here.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::num::NonZeroU32;

use fgdb_types::{CanonicalDecimal, VId};

use crate::{BeaconError, IndexSnapshot, TextMatch, VectorSearch, WorkControl};

/// Exact, nonnegative integer weights for two ranked sources. Scores are
/// `vector_weight / (k0 + vector_rank) + text_weight / (k0 + text_rank)`;
/// absent sources contribute zero. Weights are NOT implicitly normalized.
///
/// The domain is deliberately explicit: u32 rank constant/ranks and u16
/// weights. A denominator is < 2^33, a combined numerator < 2^50, and a
/// combined denominator < 2^66. Comparing ANY two such scores takes < 116
/// bits, so u128 arithmetic is exact throughout this entire supported domain.
/// Arbitrary Decimal128 weights and unbounded/many-source fusion require a
/// different, bigint-backed profile; they must not be narrowed into this one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExactRrfProfile {
    rank_constant: NonZeroU32,
    vector_weight: u16,
    text_weight: u16,
}

impl ExactRrfProfile {
    pub fn new(
        rank_constant: u32,
        vector_weight: u16,
        text_weight: u16,
    ) -> Result<Self, BeaconError> {
        let rank_constant = NonZeroU32::new(rank_constant)
            .ok_or(BeaconError::InvalidQuery("RRF rank constant must be positive"))?;
        if vector_weight == 0 && text_weight == 0 {
            return Err(BeaconError::InvalidQuery("at least one RRF weight must be positive"));
        }
        Ok(Self { rank_constant, vector_weight, text_weight })
    }

    #[must_use]
    pub const fn rank_constant(self) -> NonZeroU32 {
        self.rank_constant
    }

    #[must_use]
    pub const fn vector_weight(self) -> u16 {
        self.vector_weight
    }

    #[must_use]
    pub const fn text_weight(self) -> u16 {
        self.text_weight
    }
}

impl Default for ExactRrfProfile {
    fn default() -> Self {
        Self {
            rank_constant: NonZeroU32::new(60).expect("60 is nonzero"),
            vector_weight: 1,
            text_weight: 1,
        }
    }
}

/// A reduced nonnegative rational. Private fields preserve the arithmetic
/// bounds of ExactRrfProfile; equality/hash and total order agree numerically.
/// Decimal rendering can collapse distinct scores and is never used to rank.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExactRrfScore {
    numerator: u128,
    denominator: u128,
}

impl ExactRrfScore {
    #[must_use]
    pub fn from_ranks(
        profile: ExactRrfProfile,
        vector_rank: Option<NonZeroU32>,
        text_rank: Option<NonZeroU32>,
    ) -> Self {
        let term = |rank: Option<NonZeroU32>, weight: u16| match rank {
            Some(rank) if weight != 0 => (
                u128::from(weight),
                u128::from(profile.rank_constant.get()) + u128::from(rank.get()),
            ),
            _ => (0, 1),
        };
        let (vn, vd) = term(vector_rank, profile.vector_weight);
        let (tn, td) = term(text_rank, profile.text_weight);
        let numerator = vn * td + tn * vd;
        let denominator = vd * td;
        let mut a = numerator;
        let mut b = denominator;
        while b != 0 {
            (a, b) = (b, a % b);
        }
        Self { numerator: numerator / a, denominator: denominator / a }
    }

    #[must_use]
    pub const fn numerator(self) -> u128 {
        self.numerator
    }

    #[must_use]
    pub const fn denominator(self) -> u128 {
        self.denominator
    }

    /// Canonical scale-18 Decimal128, rounded once, nearest/ties-to-even.
    /// The scaled numerator needs < 110 bits and the score is <= 65535,
    /// well inside both u128 arithmetic and the canonical decimal profile.
    pub fn decimal(self) -> Result<CanonicalDecimal, BeaconError> {
        let scaled = self.numerator * 1_000_000_000_000_000_000_u128;
        let quotient = scaled / self.denominator;
        let remainder = scaled % self.denominator;
        let twice = remainder * 2;
        let round_up = twice > self.denominator
            || (twice == self.denominator && quotient % 2 != 0);
        let coefficient = i128::try_from(quotient + u128::from(round_up))
            .map_err(|_| BeaconError::Invariant("bounded RRF decimal coefficient"))?;
        CanonicalDecimal::from_coefficient(coefficient)
            .map_err(|_| BeaconError::Invariant("bounded RRF decimal profile"))
    }
}

impl Ord for ExactRrfScore {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.numerator * other.denominator).cmp(&(other.numerator * self.denominator))
    }
}

impl PartialOrd for ExactRrfScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Candidate depths are semantic inputs, not adaptive tuning. A zero weight
/// disables its lane entirely: that lane is neither validated nor searched.
/// The requested output may exceed either depth, but not their active sum.
#[derive(Clone, Copy, Debug)]
pub struct ExactHybridQuery<'a> {
    pub vector: &'a [f32],
    pub text: &'a str,
    pub k: usize,
    pub vector_candidates: u32,
    pub text_candidates: u32,
    pub vector_mode: VectorSearch,
    pub text_mode: TextMatch,
    pub profile: ExactRrfProfile,
}

impl ExactHybridQuery<'_> {
    fn depths(self) -> Result<(usize, usize), BeaconError> {
        let depth = |weight: u16, count: u32| {
            if weight == 0 {
                Ok(0)
            } else {
                usize::try_from(count)
                    .map_err(|_| BeaconError::InvalidQuery("RRF candidate depth exceeds usize"))
            }
        };
        let vector = depth(self.profile.vector_weight, self.vector_candidates)?;
        let text = depth(self.profile.text_weight, self.text_candidates)?;
        let total = vector.checked_add(text)
            .ok_or(BeaconError::InvalidQuery("RRF candidate sum exceeds usize"))?;
        if self.k > total {
            return Err(BeaconError::InvalidQuery("RRF k exceeds active candidate depths"));
        }
        Ok((vector, text))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExactHybridHit {
    pub id: VId,
    pub score: ExactRrfScore,
    pub decimal_score: CanonicalDecimal,
    pub vector_rank: Option<NonZeroU32>,
    pub text_rank: Option<NonZeroU32>,
    pub vector_distance: Option<f64>,
    pub text_score: Option<f64>,
}

#[derive(Default)]
struct Evidence {
    vector_rank: Option<NonZeroU32>,
    text_rank: Option<NonZeroU32>,
    vector_distance: Option<f64>,
    text_score: Option<f64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Ranked {
    id: VId,
    score: ExactRrfScore,
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        // Smaller means better. BinaryHeap keeps the worst retained hit on top.
        other.score.cmp(&self.score).then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn rank(offset: usize) -> Result<NonZeroU32, BeaconError> {
    u32::try_from(offset).ok().and_then(|n| n.checked_add(1))
        .and_then(NonZeroU32::new)
        .ok_or(BeaconError::Invariant("source exceeded admitted RRF rank domain"))
}

impl IndexSnapshot {
    /// Fuse text/vector candidates from THIS immutable generation with exact
    /// rational ordering and VId-ascending ties. Result predicates are not ACLs;
    /// the caller must have built the corpus in the proper authority domain.
    ///
    /// This is exact fusion of the selected candidates, not an exhaustive
    /// hybrid top-k or an AnswerContract::Exact certificate. In particular,
    /// VectorSearch::Approximate remains approximate. No fallback is hidden.
    /// Additional fusion memory is bounded by the candidate depths plus k. Errors expose no
    /// partial result, and the final publication has its own work checkpoint.
    pub fn hybrid_search_exact_fusion(
        &self,
        query: ExactHybridQuery<'_>,
        eligible: impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<ExactHybridHit>, BeaconError> {
        work.charge(1)?;
        let (vector_depth, text_depth) = query.depths()?;
        if query.k == 0 {
            return Ok(Vec::new());
        }
        let mut fused = BTreeMap::<VId, Evidence>::new();
        if vector_depth != 0 {
            let hits = self.knn(query.vector, vector_depth, query.vector_mode, &eligible, work)?;
            for (offset, hit) in hits.into_iter().enumerate() {
                work.charge(1)?;
                let row = fused.entry(hit.id).or_default();
                row.vector_rank = Some(rank(offset)?);
                row.vector_distance = Some(hit.distance);
            }
        }
        if text_depth != 0 {
            let hits = self.text_search(query.text, text_depth, query.text_mode, &eligible, work)?;
            for (offset, hit) in hits.into_iter().enumerate() {
                work.charge(1)?;
                let row = fused.entry(hit.id).or_default();
                row.text_rank = Some(rank(offset)?);
                row.text_score = Some(hit.score);
            }
        }
        let mut best = BinaryHeap::<Ranked>::new();
        for (&id, evidence) in &fused {
            // Heap maintenance has at most logarithmic work between checks;
            // charge a conservative height bound rather than hiding a sort.
            work.charge(2 * (1 + best.len().checked_ilog2().unwrap_or(0) as usize))?;
            let candidate = Ranked {
                id,
                score: ExactRrfScore::from_ranks(
                    query.profile, evidence.vector_rank, evidence.text_rank,
                ),
            };
            if best.len() < query.k {
                best.push(candidate);
            } else if best.peek().is_some_and(|worst| candidate < *worst) {
                best.pop();
                best.push(candidate);
            }
        }
        let mut rows = Vec::new();
        while !best.is_empty() {
            work.charge(1 + best.len().checked_ilog2().unwrap_or(0) as usize)?;
            let hit = best.pop().ok_or(BeaconError::Invariant("RRF heap disappeared"))?;
            let evidence = fused.remove(&hit.id)
                .ok_or(BeaconError::Invariant("RRF evidence disappeared"))?;
            rows.push(ExactHybridHit {
                id: hit.id,
                score: hit.score,
                decimal_score: hit.score.decimal()?,
                vector_rank: evidence.vector_rank,
                text_rank: evidence.text_rank,
                vector_distance: evidence.vector_distance,
                text_score: evidence.text_score,
            });
        }
        // Heap removal is worst-first. Reverse under checkpoints, not a
        // potentially corpus-sized uninterruptible sort/reverse operation.
        let len = rows.len();
        for i in 0..len / 2 {
            work.charge(1)?;
            rows.swap(i, len - i - 1);
        }
        work.charge(1)?;
        Ok(rows)
    }
}
