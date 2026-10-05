//! Exact numeric index bounds without changing canonical storage identity.
//!
//! Int and Float have separate canonical tags. Query comparisons, unlike
//! storage ordering, admit both domains. Seek each domain independently and
//! union historical candidates BEFORE resolving visibility and native filters.

use super::{PropertyEqualityIndex, PropertyRange, Snapshot, SourceEvent, VertexRow};
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{GlaOperator, IntegerComparison as C, VertexPredicate};
use fgdb_types::{CanonicalF64, CanonicalScalar, CommitSeq, VId};
use std::collections::BTreeSet;

struct NumericBounds {
    key: PropertyKeyId,
    ranges: [PropertyRange; 2],
}

impl NumericBounds {
    fn new(key: PropertyKeyId) -> Self {
        Self {
            key,
            ranges: [
                PropertyRange::new(
                    key,
                    &CanonicalScalar::Int(0).encode().expect("integer encoding"),
                ),
                PropertyRange::new(
                    key,
                    &CanonicalScalar::Float(CanonicalF64::new(0.0))
                        .encode()
                        .expect("float encoding"),
                ),
            ],
        }
    }

    /// A cast only supplies a neighboring boundary, never the comparison's
    /// truth. The exact shared comparator decides which side includes it.
    /// This handles fractions, i64 saturation and binary64 rounding above 2^53.
    /// STRICT_PORTABLE orders NaN after +infinity, hence its integer boundary
    /// is MAX, not Rust's NaN-to-integer cast result of zero.
    fn constrain(&mut self, value: &CanonicalScalar, comparison: C) {
        let peers = match value {
            CanonicalScalar::Int(value) => [
                CanonicalScalar::Int(*value),
                CanonicalScalar::Float(CanonicalF64::new(*value as f64)),
            ],
            CanonicalScalar::Float(value) => [
                CanonicalScalar::Int(if value.get().is_nan() {
                    i64::MAX
                } else {
                    value.get() as i64
                }),
                CanonicalScalar::Float(*value),
            ],
            _ => unreachable!("only numeric operands enter numeric bounds"),
        };
        for (range, peer) in self.ranges.iter_mut().zip(peers) {
            let equal = C::Equal.accepts_scalar_pair(Some(&peer), Some(value));
            let below = C::Less.accepts_scalar_pair(Some(&peer), Some(value));
            let comparison = match comparison {
                C::Equal if !equal => {
                    range.empty = true;
                    continue;
                }
                C::Equal => C::Equal,
                // x < literal includes the rounded boundary only when that
                // boundary is itself below the literal. The other inequalities
                // are the corresponding exact open/closed interval choices.
                C::Less if below => C::LessOrEqual,
                C::Less => C::Less,
                C::LessOrEqual if below || equal => C::LessOrEqual,
                C::LessOrEqual => C::Less,
                C::Greater if !below && !equal => C::GreaterOrEqual,
                C::Greater => C::Greater,
                C::GreaterOrEqual if !below => C::GreaterOrEqual,
                C::GreaterOrEqual => C::Greater,
                // A != constraint needs two intervals. Leave it to the native
                // residual predicate, rather than pruning either valid side.
                C::NotEqual => continue,
            };
            range.constrain(peer.encode().expect("numeric encoding"), comparison);
        }
    }

    fn candidates<E>(
        &self,
        index: &PropertyEqualityIndex,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<BTreeSet<VId>, E> {
        let mut candidates = BTreeSet::new();
        for range in &self.ranges {
            index.extend_range_candidates(range, &mut candidates, control)?;
        }
        Ok(candidates)
    }
}

fn numeric_operand(predicate: &VertexPredicate) -> Option<(PropertyKeyId, C, CanonicalScalar)> {
    let (key, comparison, value) = match predicate {
        VertexPredicate::IntegerProperty {
            key,
            comparison,
            value,
        } => (*key, *comparison, CanonicalScalar::Int(*value)),
        VertexPredicate::ScalarProperty { key, predicate } => match predicate.value() {
            value @ (CanonicalScalar::Int(_) | CanonicalScalar::Float(_)) => {
                (*key, predicate.comparison(), value.clone())
            }
            _ => return None,
        },
        _ => return None,
    };
    (comparison != C::NotEqual).then_some((key, comparison, value))
}

fn predicates(operators: &[GlaOperator]) -> impl Iterator<Item = &VertexPredicate> {
    operators
        .iter()
        .filter_map(|operator| match operator {
            GlaOperator::Select { predicates, .. } => Some(predicates.as_slice()),
            _ => None,
        })
        .flatten()
}

/// The caller admits only a single-domain root selection. This must not be
/// used to prune the shared vertex table of joins, OPTIONAL or nested scans.
pub(super) fn bound_vertices<'a, E>(
    snapshot: &'a Snapshot,
    operators: &[GlaOperator],
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<(Vec<&'a VertexRow>, u64)>, E> {
    let mut bounds: Option<NumericBounds> = None;
    for predicate in predicates(operators) {
        control(SourceEvent::Work)?;
        if let Some((key, comparison, value)) = numeric_operand(predicate) {
            let selected = bounds.get_or_insert_with(|| NumericBounds::new(key));
            if selected.key == key {
                selected.constrain(&value, comparison);
            }
        }
    }
    let Some(bounds) = bounds else {
        return Ok(None);
    };
    let candidates = bounds.candidates(&snapshot.property_index, control)?;
    let mut rows = Vec::new();
    for vid in &candidates {
        control(SourceEvent::Work)?;
        control(SourceEvent::SnapshotRecord)?;
        if let Some(row) =
            snapshot
                .property_index
                .visible_row(&snapshot.patches, *vid, as_of, control)?
        {
            let mut matches = true;
            for predicate in predicates(operators) {
                control(SourceEvent::Work)?;
                if !predicate.matches(&row.labels, &row.props) {
                    matches = false;
                    break;
                }
            }
            if matches {
                control(SourceEvent::ScratchEntry)?;
                rows.push(row);
            }
        }
    }
    Ok(Some((rows, candidates.len() as u64)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_strata::vertex::{VertexPatchRows, decode_patch, encode_patch};

    const KEY: PropertyKeyId = PropertyKeyId(1);
    const COMPARISONS: [C; 5] = [
        C::Equal,
        C::Less,
        C::LessOrEqual,
        C::Greater,
        C::GreaterOrEqual,
    ];

    fn float(value: f64) -> CanonicalScalar {
        CanonicalScalar::Float(CanonicalF64::new(value))
    }

    fn values() -> Vec<CanonicalScalar> {
        let mut values = vec![
            CanonicalScalar::Int(i64::MIN),
            CanonicalScalar::Int(i64::MIN + 1),
            CanonicalScalar::Int(-9_007_199_254_740_993),
            CanonicalScalar::Int(9_007_199_254_740_993),
            CanonicalScalar::Int(i64::MAX - 1023),
            CanonicalScalar::Int(i64::MAX),
            float(f64::NEG_INFINITY),
            float(f64::INFINITY),
            float(f64::NAN),
            float(-9_223_372_036_854_775_808.0),
            float(9_223_372_036_854_775_808.0),
            float(-9_007_199_254_740_992.0),
            float(9_007_199_254_740_992.0),
            float(9_007_199_254_740_994.0),
            float(9_223_372_036_854_774_784.0),
            float(-f64::from_bits(1)),
            float(f64::from_bits(1)),
            float(-0.0),
        ];
        for n in -16..=16 {
            values.push(CanonicalScalar::Int(n));
            values.push(float(n as f64 / 4.0));
        }
        values
    }

    fn contains(range: &PropertyRange, value: &CanonicalScalar) -> bool {
        let encoded = value.encode().unwrap();
        !range.empty
            && (encoded > range.lower || (range.lower_inclusive && encoded == range.lower))
            && (encoded < range.upper || (range.upper_inclusive && encoded == range.upper))
    }

    #[test]
    fn every_numeric_domain_bound_matches_the_exact_query_comparator() {
        let values = values();
        for literal in &values {
            for comparison in COMPARISONS {
                let mut bounds = NumericBounds::new(KEY);
                bounds.constrain(literal, comparison);
                for actual in &values {
                    let accepted = bounds.ranges.iter().any(|range| contains(range, actual));
                    assert_eq!(
                        accepted,
                        comparison.accepts_scalar_pair(Some(actual), Some(literal)),
                        "literal={literal:?} actual={actual:?} comparison={comparison:?}"
                    );
                }
                for unrelated in [
                    CanonicalScalar::Null,
                    CanonicalScalar::Bool(true),
                    CanonicalScalar::ucs_basic_text("100").unwrap(),
                    CanonicalScalar::bytes(vec![100]).unwrap(),
                ] {
                    assert!(
                        !bounds
                            .ranges
                            .iter()
                            .any(|range| contains(range, &unrelated))
                    );
                }
            }
        }
    }

    fn patch(rows: &[(u128, u64, Option<u64>, CanonicalScalar)]) -> VertexPatchRows {
        let rows: Vec<_> = rows
            .iter()
            .map(|(id, created, retired, value)| VertexRow {
                vid: VId(*id),
                birth_ordinal: *id as u64,
                created_at: CommitSeq(*created),
                retired_at: retired.map(CommitSeq),
                labels: vec![],
                props: vec![(KEY, value.clone())],
            })
            .collect();
        decode_patch(&encode_patch(&rows).unwrap()).unwrap()
    }

    #[test]
    fn mixed_numeric_conjunctions_keep_both_tags_without_precision_loss() {
        let patches = vec![patch(&[
            (1, 1, None, CanonicalScalar::Int(100)),
            (2, 1, None, float(100.0)),
            (3, 1, None, float(100.5)),
            (4, 1, None, CanonicalScalar::Int(101)),
            (5, 1, None, CanonicalScalar::Int(9_007_199_254_740_993)),
            (6, 1, None, float(9_007_199_254_740_992.0)),
        ])];
        let index = PropertyEqualityIndex::build(&patches);
        let mut bounds = NumericBounds::new(KEY);
        bounds.constrain(&CanonicalScalar::Int(100), C::GreaterOrEqual);
        bounds.constrain(&float(101.0), C::Less);
        assert_eq!(
            bounds.candidates(&index, &mut |_| Ok::<_, ()>(())).unwrap(),
            BTreeSet::from([VId(1), VId(2), VId(3)])
        );
        let mut exact = NumericBounds::new(KEY);
        exact.constrain(&CanonicalScalar::Int(9_007_199_254_740_993), C::Equal);
        assert_eq!(
            exact.candidates(&index, &mut |_| Ok::<_, ()>(())).unwrap(),
            BTreeSet::from([VId(5)])
        );
        let mut interval = NumericBounds::new(KEY);
        interval.constrain(&float(9_007_199_254_740_992.0), C::GreaterOrEqual);
        interval.constrain(&CanonicalScalar::Int(9_007_199_254_740_993), C::Less);
        assert_eq!(
            interval
                .candidates(&index, &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            BTreeSet::from([VId(6)])
        );
    }

    #[test]
    fn candidate_union_deduplicates_type_changes_and_visibility_stays_authoritative() {
        let patches = vec![
            patch(&[
                (1, 1, None, CanonicalScalar::Int(100)),
                (2, 1, None, float(100.0)),
            ]),
            patch(&[(1, 2, None, float(100.0)), (2, 2, None, float(99.0))]),
            patch(&[(1, 2, Some(3), float(100.0))]),
        ];
        let index = PropertyEqualityIndex::build(&patches);
        let mut bounds = NumericBounds::new(KEY);
        bounds.constrain(&float(100.0), C::Equal);
        let mut allocations = 0;
        let candidates = bounds
            .candidates(&index, &mut |event| {
                allocations += usize::from(event == SourceEvent::ScratchEntry);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(candidates, BTreeSet::from([VId(1), VId(2)]));
        assert_eq!(
            allocations, 2,
            "one set entry per ID, not per type or version"
        );
        for (at, expected) in [(1, vec![VId(1), VId(2)]), (2, vec![VId(1)]), (3, vec![])] {
            let actual: Vec<_> = candidates
                .iter()
                .filter_map(|vid| {
                    index
                        .visible_row(&patches, *vid, CommitSeq(at), &mut |_| Ok::<_, ()>(()))
                        .unwrap()
                        .filter(|row| {
                            C::Equal.accepts_scalar_pair(Some(&row.props[0].1), Some(&float(100.0)))
                        })
                        .map(|row| row.vid)
                })
                .collect();
            assert_eq!(actual, expected, "at={at}");
        }
    }

    #[test]
    fn every_numeric_index_checkpoint_propagates_refusal_without_partial_candidates() {
        // The same ID is in multiple historical values and BOTH type domains.
        // Even duplicate candidate visits must cross the work/cancel seam.
        let patches = vec![
            patch(&[
                (1, 1, None, CanonicalScalar::Int(100)),
                (2, 1, None, float(100.0)),
            ]),
            patch(&[
                (1, 2, None, float(101.0)),
                (2, 2, None, CanonicalScalar::Int(101)),
            ]),
        ];
        let index = PropertyEqualityIndex::build(&patches);
        let mut bounds = NumericBounds::new(KEY);
        bounds.constrain(&float(100.0), C::GreaterOrEqual);
        let run = |stop| {
            let mut events = 0;
            let result = bounds.candidates(&index, &mut |_| {
                events += 1;
                if events == stop { Err(stop) } else { Ok(()) }
            });
            (result, events)
        };
        let (result, total) = run(usize::MAX);
        assert_eq!(result.unwrap(), BTreeSet::from([VId(1), VId(2)]));
        assert!(total > 2);
        for stop in 1..=total {
            assert_eq!(run(stop), (Err(stop), stop));
        }
    }
}
