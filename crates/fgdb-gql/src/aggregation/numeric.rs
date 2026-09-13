//! Exact integer averages and DISTINCT numeric aggregation.
//!
//! Averages retain a checked i128 sum and u64 nonnull count. Result values are
//! reduced fractions, never rounded floats or truncated integer quotients.
//! Comparisons split quotient/remainder before cross multiplication: only two
//! u64-bounded residuals multiply, so even i128::MIN is safe. Borrowed result
//! cells need no gcd or allocation; normalization happens at final output.

use super::*;
use core::cmp::Ordering;

/// A canonical exact rational returned by AVG_INT (and the bounded AVG alias).
/// Denominators are positive; numerator/denominator are reduced, including 0/1.
/// Construction with a zero denominator returns None. This is an application
/// result value, not a new durable CanonicalScalar arm or floating SQL domain.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GraphExactAverage {
    numerator: i128,
    denominator: u64,
}

impl GraphExactAverage {
    #[must_use]
    pub fn new(numerator: i128, denominator: u64) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        let mut a = denominator;
        let mut b = (numerator.unsigned_abs() % u128::from(denominator)) as u64;
        while b != 0 {
            (a, b) = (b, a % b);
        }
        Some(Self {
            numerator: numerator / i128::from(a),
            denominator: denominator / a,
        })
    }

    #[must_use]
    pub const fn numerator(self) -> i128 {
        self.numerator
    }

    #[must_use]
    pub const fn denominator(self) -> u64 {
        self.denominator
    }

    #[must_use]
    pub fn compare_integer(self, other: i128) -> Ordering {
        compare_ratios((self.numerator, self.denominator), (other, 1))
    }
}

impl Ord for GraphExactAverage {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_ratios(
            (self.numerator, self.denominator),
            (other.numerator, other.denominator),
        )
    }
}
impl PartialOrd for GraphExactAverage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl core::fmt::Debug for GraphExactAverage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphExactAverage([REDACTED])")
    }
}
impl core::fmt::Display for GraphExactAverage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.denominator == 1 {
            core::fmt::Display::fmt(&self.numerator, f)
        } else {
            write!(f, "{}/{}", self.numerator, self.denominator)
        }
    }
}

/// Both denominators are private positive counts (or the integer denominator
/// one). Integer quotient comparisons avoid the overflowing n1*d2/n2*d1 path.
/// Each residual is < its u64 denominator, so residual*other_denominator fits
/// u128. unsigned_abs also represents the magnitude of i128::MIN exactly.
pub(super) fn compare_ratios(left: (i128, u64), right: (i128, u64)) -> Ordering {
    let (a, da) = left;
    let (b, db) = right;
    debug_assert!(da != 0 && db != 0);
    if da == 1 && db == 1 {
        return a.cmp(&b);
    }
    match (a.is_negative(), b.is_negative()) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let (a_abs, b_abs) = (a.unsigned_abs(), b.unsigned_abs());
    let (da, db) = (u128::from(da), u128::from(db));
    let result = (a_abs / da)
        .cmp(&(b_abs / db))
        .then_with(|| ((a_abs % da) * db).cmp(&((b_abs % db) * da)));
    if a.is_negative() {
        result.reverse()
    } else {
        result
    }
}

pub(super) enum NumericResult {
    Empty,
    Sum(i128),
    Average { sum: i128, count: u64 },
}

pub(super) struct NumericAccumulator {
    sum: i128,
    count: u64,
    average: bool,
    seen: Option<BTreeSet<i64>>,
}

impl NumericAccumulator {
    pub(super) fn new(average: bool, distinct: bool) -> Self {
        Self {
            sum: 0,
            count: 0,
            average,
            seen: distinct.then(BTreeSet::new),
        }
    }

    pub(super) fn result(&self) -> NumericResult {
        if self.count == 0 {
            NumericResult::Empty
        } else if self.average {
            NumericResult::Average { sum: self.sum, count: self.count }
        } else {
            NumericResult::Sum(self.sum)
        }
    }

    /// Retire membership storage after the owning group has consumed its last
    /// input. The completed sum/count remain available to exact comparisons
    /// and output. Only the root-group selector calls this; retired heap groups
    /// are never admitted as mutable input accumulators again.
    pub(super) fn release_distinct_set(&mut self) {
        self.seen = None;
    }

    #[cfg(test)]
    pub(super) fn retained_distinct_values(&self) -> usize {
        self.seen.as_ref().map_or(0, BTreeSet::len)
    }

    /// Called only for a nonnull argument after the shared source read. A
    /// repeated integer is one value, even when distinct vertices/paths carry
    /// it. Charge before growing the set and mutate arithmetic only afterward:
    /// refusal cannot leave a newly counted value missing from its set.
    pub(super) fn update<E, C>(
        &mut self,
        value: ValueRef<'_>,
        aggregate: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        let ValueRef::Scalar(CanonicalScalar::Int(value)) = value else {
            return Err(GqlQueryError::Source(if self.average {
                GraphAggregateError::NonIntegerAverage { aggregate }
            } else {
                GraphAggregateError::NonIntegerSum { aggregate }
            }));
        };
        if let Some(seen) = &self.seen {
            control(GlaExecutionEvent::Work)?;
            if seen.contains(value) {
                return Ok(());
            }
        }
        let overflow = || GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate });
        let count = self.count.checked_add(1).ok_or_else(overflow)?;
        let sum = self.sum.checked_add(i128::from(*value)).ok_or_else(overflow)?;
        if let Some(seen) = &mut self.seen {
            control(GlaExecutionEvent::ScratchEntry)?;
            seen.insert(*value);
        }
        self.sum = sum;
        self.count = count;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_average_normalizes_without_narrowing_or_exposing_debug_values() {
        assert!(GraphExactAverage::new(1, 0).is_none());
        let zero = GraphExactAverage::new(0, u64::MAX).unwrap();
        assert_eq!((zero.numerator(), zero.denominator()), (0, 1));
        assert_eq!(GraphExactAverage::new(6, 4), GraphExactAverage::new(3, 2));
        assert_eq!(GraphExactAverage::new(-6, 4).unwrap().to_string(), "-3/2");
        assert_eq!(GraphExactAverage::new(i128::MIN, 2).unwrap().numerator(), i128::MIN / 2);
        let value = GraphExactAverage::new(123456789, 17).unwrap();
        assert!(!format!("{value:?}").contains("123456789"));
        assert_eq!(GraphExactAverage::new(i128::MIN, 1).unwrap().compare_integer(i128::MIN), Ordering::Equal);
    }

    #[test]
    fn ratio_order_matches_independent_safe_cross_products_on_small_domain() {
        for a in -12_i128..=12 {
            for b in -12_i128..=12 {
                for da in 1..=12_u64 {
                    for db in 1..=12_u64 {
                        let expected = (a * i128::from(db)).cmp(&(b * i128::from(da)));
                        assert_eq!(compare_ratios((a, da), (b, db)), expected);
                        let left = GraphExactAverage::new(a, da).unwrap();
                        let right = GraphExactAverage::new(b, db).unwrap();
                        assert_eq!(left.cmp(&right), expected);
                        assert_eq!(left == right, expected == Ordering::Equal);
                    }
                }
            }
        }
    }

    #[test]
    fn ratio_order_keeps_sub_float_differences_and_full_signed_width() {
        let d = u64::MAX;
        let high = i128::MAX;
        assert_eq!(compare_ratios((high, d), (high - 1, d)), Ordering::Greater);
        assert_eq!(compare_ratios((i128::MIN, d), (i128::MIN + 1, d)), Ordering::Less);
        assert_eq!(compare_ratios((high, d), (high, d - 1)), Ordering::Less);
        assert_eq!(compare_ratios((i128::MIN, d), (i128::MIN, d - 1)), Ordering::Greater);
        assert_eq!(compare_ratios((-1, d), (0, 1)), Ordering::Less);
        let n = i128::from(i64::MAX);
        assert_eq!(compare_ratios((2 * n - 1, 2), (n, 1)), Ordering::Less);
        assert_eq!(compare_ratios((2 * n - 1, 2), (n - 1, 1)), Ordering::Greater);
    }

    #[test]
    fn distinct_numeric_state_admits_values_not_paths_and_is_atomic_on_refusal() {
        let mut state = NumericAccumulator::new(true, true);
        let mut scratch = 0;
        for number in [1, 1, 5, 5, -3] {
            state.update(ValueRef::Scalar(&CanonicalScalar::Int(number)), 0, &mut |event| {
                scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(())
            }).unwrap();
        }
        assert_eq!((state.sum, state.count, scratch), (3, 3, 3));
        let original = (state.sum, state.count, state.seen.as_ref().unwrap().clone());
        let result = state.update(ValueRef::Scalar(&CanonicalScalar::Int(9)), 0, &mut |event| {
            if event == GlaExecutionEvent::ScratchEntry {
                Err(GqlQueryError::<GraphAggregateError<()>, _>::Interrupted("stop"))
            } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted("stop"))));
        assert_eq!((state.sum, state.count, state.seen.as_ref().unwrap().clone()), original);
        state.count = u64::MAX;
        let before = state.sum;
        let result = state.update(ValueRef::Scalar(&CanonicalScalar::Int(9)), 2,
            &mut |_| Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(()));
        assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate: 2 }))));
        assert_eq!(state.sum, before);
        assert!(!state.seen.as_ref().unwrap().contains(&9));
    }
}
