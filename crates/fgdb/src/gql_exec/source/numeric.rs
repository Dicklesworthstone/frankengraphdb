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

pub(super) struct NumericBounds {
    key: PropertyKeyId,
    ranges: [PropertyRange; 2],
    // Exact query-domain endpoints also recheck a visible winner without
    // allocating its canonical encoding on every streamed history.
    lower: Option<(CanonicalScalar, bool)>,
    upper: Option<(CanonicalScalar, bool)>,
}

fn numeric_peers(value: &CanonicalScalar) -> [CanonicalScalar; 2] {
    match value {
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
    }
}

/// Equality needs at most one canonical key in each numeric domain. The
/// shared exact comparator rejects a rounded/saturated peer instead of
/// treating the cast as evidence of equality.
pub(super) fn equality_peers(value: &CanonicalScalar) -> [Option<CanonicalScalar>; 2] {
    numeric_peers(value).map(|peer| {
        C::Equal
            .accepts_scalar_pair(Some(&peer), Some(value))
            .then_some(peer)
    })
}

impl NumericBounds {
    pub(super) fn new(key: PropertyKeyId) -> Self {
        Self {
            key,
            lower: None,
            upper: None,
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
    pub(super) fn constrain(&mut self, value: &CanonicalScalar, comparison: C) {
        let inclusive = matches!(comparison, C::Equal | C::LessOrEqual | C::GreaterOrEqual);
        for (endpoint, tighter) in [(&mut self.lower, C::Less), (&mut self.upper, C::Greater)] {
            let applies = match tighter {
                C::Less => matches!(comparison, C::Equal | C::Greater | C::GreaterOrEqual),
                _ => matches!(comparison, C::Equal | C::Less | C::LessOrEqual),
            };
            if !applies {
                continue;
            }
            match endpoint {
                Some((old, old_inclusive))
                    if C::Equal.accepts_scalar_pair(Some(old), Some(value)) =>
                {
                    *old_inclusive &= inclusive;
                }
                Some((old, _)) if !tighter.accepts_scalar_pair(Some(old), Some(value)) => {}
                _ => *endpoint = Some((value.clone(), inclusive)),
            }
        }
        let peers = numeric_peers(value);
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

    pub(super) fn key(&self) -> PropertyKeyId {
        self.key
    }

    pub(super) fn ranges(&self) -> &[PropertyRange; 2] {
        &self.ranges
    }

    /// Exact native comparisons, independent of the encoded seek boundaries.
    /// Scoped streams call this for the visible winner only after admitting
    /// its labels; private candidate discovery may check historical statements.
    pub(super) fn accepts(&self, value: &CanonicalScalar) -> bool {
        matches!(value, CanonicalScalar::Int(_) | CanonicalScalar::Float(_))
            && self.lower.as_ref().is_none_or(|(bound, inclusive)| {
                let comparison = if *inclusive {
                    C::GreaterOrEqual
                } else {
                    C::Greater
                };
                comparison.accepts_scalar_pair(Some(value), Some(bound))
            })
            && self.upper.as_ref().is_none_or(|(bound, inclusive)| {
                let comparison = if *inclusive { C::LessOrEqual } else { C::Less };
                comparison.accepts_scalar_pair(Some(value), Some(bound))
            })
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

pub(super) fn numeric_operand(
    predicate: &VertexPredicate,
) -> Option<(PropertyKeyId, C, CanonicalScalar)> {
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

/// A comparison can probe at most two exact storage keys. Keep Int/Float
/// peers together; all other non-null scalar equalities have one posting.
fn equality_operand(
    predicate: &VertexPredicate,
) -> Option<(PropertyKeyId, [Option<CanonicalScalar>; 2])> {
    let (key, value) = match predicate {
        VertexPredicate::IntegerProperty {
            key,
            comparison: C::Equal,
            value,
        } => (*key, CanonicalScalar::Int(*value)),
        VertexPredicate::ScalarProperty { key, predicate }
            if predicate.comparison() == C::Equal
                && !matches!(predicate.value(), CanonicalScalar::Null) =>
        {
            (*key, predicate.value().clone())
        }
        _ => return None,
    };
    let peers = match &value {
        CanonicalScalar::Int(_) | CanonicalScalar::Float(_) => equality_peers(&value),
        _ => [Some(value), None],
    };
    Some((key, peers))
}

enum BoundCandidates<'a> {
    Posting(super::Candidates<'a>),
    Union(BTreeSet<VId>),
}

impl BoundCandidates<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Posting(posting) => posting.len(),
            Self::Union(candidates) => candidates.len(),
        }
    }
}

/// Rank conjunction keys using cached historical posting lengths, without
/// walking any posting merely to estimate it. Summing numeric peers is a
/// conservative estimate: their historical IDs may overlap. Only the chosen
/// postings are enumerated, and their union still precedes winner selection.
fn equality_candidates<'a, 'p, E>(
    snapshot: &'a Snapshot,
    predicates: impl Iterator<Item = &'p VertexPredicate>,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<BoundCandidates<'a>>, E> {
    let mut selected: Option<([super::Candidates<'_>; 2], usize)> = None;
    for predicate in predicates {
        control(SourceEvent::Work)?;
        let Some((key, peers)) = equality_operand(predicate) else {
            continue;
        };
        let mut postings = [super::Candidates(None); 2];
        let mut count = 0_usize;
        for (posting, value) in postings.iter_mut().zip(peers) {
            if let Some(value) = value {
                control(SourceEvent::Work)?;
                // Do not force the lazy index for a predicate-only scan.
                *posting = snapshot.property_index().lookup(key, &value);
                count = count.saturating_add(posting.len());
            }
        }
        if selected.as_ref().is_none_or(|(_, best)| count < *best) {
            selected = Some((postings, count));
        }
    }
    let Some((postings, _)) = selected else {
        return Ok(None);
    };
    // A single posting is already unique and ordered. Keep its borrowed
    // cursor, rather than building a second identity set for a broad scalar
    // predicate or an equality whose other numeric domain has no entries.
    if postings[0].len() == 0 {
        return Ok(Some(BoundCandidates::Posting(postings[1])));
    }
    if postings[1].len() == 0 {
        return Ok(Some(BoundCandidates::Posting(postings[0])));
    }
    let mut candidates = BTreeSet::new();
    for posting in postings {
        control(SourceEvent::Work)?;
        for vid in posting {
            control(SourceEvent::Work)?;
            if !candidates.contains(vid) {
                control(SourceEvent::ScratchEntry)?;
                candidates.insert(*vid);
            }
        }
    }
    Ok(Some(BoundCandidates::Union(candidates)))
}

/// The caller admits only a single-domain root selection. This must not be
/// used to prune the shared vertex table of joins, OPTIONAL or nested scans.
pub(super) fn bound_vertices<'a, E>(
    snapshot: &'a Snapshot,
    operators: &[GlaOperator],
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<(Vec<&'a VertexRow>, u64)>, E> {
    // Match the existing scalar-equality extraction barrier in source.rs.
    // A later filter must not move before LIMIT or a richer projection, even
    // as a residual check after a safely chosen earlier equality posting.
    let mut equality_end = 0;
    for operator in operators {
        control(SourceEvent::Work)?;
        if !matches!(
            operator,
            GlaOperator::Select { .. }
                | GlaOperator::Project { .. }
                | GlaOperator::Distinct
                | GlaOperator::OrderByVertexId
        ) {
            break;
        }
        equality_end += 1;
    }
    bound_rows_with_equalities(
        snapshot,
        || predicates(operators),
        || predicates(&operators[..equality_end]),
        as_of,
        control,
    )
}

/// The visible rows at as_of satisfying every predicate predicates() yields.
/// Prefer the smallest estimated equality posting union, otherwise seek both
/// numeric domains of the first numeric predicate's key. None when no
/// non-null equality or numeric comparison can serve the conjunction.
pub(super) fn bound_rows<'a, 'p, E, I>(
    snapshot: &'a Snapshot,
    predicates: impl Fn() -> I,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<(Vec<&'a VertexRow>, u64)>, E>
where
    I: Iterator<Item = &'p VertexPredicate>,
{
    bound_rows_with_equalities(snapshot, &predicates, &predicates, as_of, control)
}

fn bound_rows_with_equalities<'a, 'p, E, I>(
    snapshot: &'a Snapshot,
    predicates: impl Fn() -> I,
    equality_predicates: impl Fn() -> I,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<(Vec<&'a VertexRow>, u64)>, E>
where
    I: Iterator<Item = &'p VertexPredicate>,
{
    let equality = equality_candidates(snapshot, equality_predicates(), control)?;
    let used_equality = equality.is_some();
    let candidates = if let Some(candidates) = equality {
        candidates
    } else {
        let mut bounds: Option<NumericBounds> = None;
        for predicate in predicates() {
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
        BoundCandidates::Union(bounds.candidates(snapshot.property_index(), control)?)
    };
    let count = candidates.len() as u64;
    let mut rows = Vec::new();
    let mut visit = |vid| -> Result<(), E> {
        control(SourceEvent::Work)?;
        control(SourceEvent::SnapshotRecord)?;
        if let Some(row) =
            snapshot
                .property_index()
                .visible_row(&snapshot.patches, vid, as_of, control)?
        {
            let mut matches = true;
            let residuals = if used_equality {
                equality_predicates()
            } else {
                predicates()
            };
            for predicate in residuals {
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
        Ok(())
    };
    match candidates {
        BoundCandidates::Posting(posting) => {
            for vid in posting {
                visit(*vid)?;
            }
        }
        BoundCandidates::Union(candidates) => {
            for vid in candidates {
                visit(vid)?;
            }
        }
    }
    Ok(Some((rows, count)))
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
                props: vec![(KEY, value.clone().into())],
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
                            C::Equal.accepts_scalar_pair(
                                row.props[0].1.as_scalar(),
                                Some(&float(100.0)),
                            )
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

    const ID_KEY: PropertyKeyId = PropertyKeyId(2);

    fn text(value: &str) -> CanonicalScalar {
        CanonicalScalar::ucs_basic_text(value).unwrap()
    }

    fn compound_row(
        id: u128,
        created: u64,
        retired: Option<u64>,
        tenant: CanonicalScalar,
        value: CanonicalScalar,
    ) -> VertexRow {
        VertexRow {
            vid: VId(id),
            birth_ordinal: id as u64,
            created_at: CommitSeq(created),
            retired_at: retired.map(CommitSeq),
            labels: vec![],
            props: vec![(KEY, tenant.into()), (ID_KEY, value.into())],
        }
    }

    fn compound_snapshot(patches: Vec<Vec<VertexRow>>) -> Snapshot {
        Snapshot {
            read_caches: Default::default(),
            blocks: vec![],
            adjacency: Default::default(),
            property_index: Default::default(),
            exact_counts: Default::default(),
            block_props: vec![],
            patches: patches
                .iter()
                // Preserve the large logical fixture without asking one
                // physical patch to violate its normative row ceiling.
                .flat_map(|rows| rows.chunks(fgdb_strata::vertex::MAX_PATCH_ROWS as usize))
                .map(|rows| decode_patch(&encode_patch(rows).unwrap()).unwrap())
                .collect(),
            frontier: CommitSeq(3),
            root: fgdb_strata::PartitionRootVersion(fgdb_types::ObjectId([0; 32])),
            manifest: fgdb_strata::manifest::ManifestVersion(fgdb_types::ObjectId([0; 32])),
            delta_index: Default::default(),
        }
    }

    fn equal(key: PropertyKeyId, value: CanonicalScalar) -> VertexPredicate {
        match value {
            CanonicalScalar::Int(value) => VertexPredicate::IntegerProperty {
                key,
                comparison: C::Equal,
                value,
            },
            value => VertexPredicate::ScalarProperty {
                key,
                predicate: fgdb_gql::algebra::ScalarPredicate::new(value, C::Equal).unwrap(),
            },
        }
    }

    #[test]
    fn single_posting_and_numeric_range_attribute_part_of_the_owner_closure_cost() {
        // The six owner-closure fixtures all start at the sole p=0 vertex.
        // Compare BOTH live production routes in this invocation: the range
        // fallback used before 9e85981d, and its borrowed equality posting.
        // Only the candidate selector differs; visibility, residual checks
        // and admitted rows run through the same implementation and meter.
        // This comparison explains THREE work events and one scratch entry.
        // The public closure pins differ by four work events: their remaining
        // event is not attributed here, and those pins remain unchanged.
        let snapshot = compound_snapshot(vec![
            (0..6)
                .map(|id| VertexRow {
                    vid: VId(id),
                    birth_ordinal: id as u64,
                    created_at: CommitSeq(1),
                    retired_at: None,
                    labels: vec![],
                    props: vec![(KEY, CanonicalScalar::Int(id as i64).into())],
                })
                .collect(),
        ]);
        let predicates = [equal(KEY, CanonicalScalar::Int(0))];
        let no_equality_keys: [VertexPredicate; 0] = [];
        let run = |select_equality| {
            let mut events = [0_u64; 3];
            let mut control = |event| {
                events[match event {
                    SourceEvent::Work => 0,
                    SourceEvent::ScratchEntry => 1,
                    SourceEvent::SnapshotRecord => 2,
                }] += 1;
                Ok::<_, ()>(())
            };
            let (rows, candidates) = bound_rows_with_equalities(
                &snapshot,
                || predicates.iter(),
                || {
                    if select_equality {
                        predicates.iter()
                    } else {
                        no_equality_keys.iter()
                    }
                },
                CommitSeq(3),
                &mut control,
            )
            .unwrap()
            .unwrap();
            (
                rows.iter().map(|row| row.vid).collect::<Vec<_>>(),
                candidates,
                events,
            )
        };
        let (range_ids, range_candidates, range) = run(false);
        let (posting_ids, posting_candidates, posting) = run(true);
        assert_eq!(range_ids, vec![VId(0)]);
        assert_eq!(posting_ids, range_ids);
        assert_eq!((range_candidates, posting_candidates), (1, 1));
        assert_eq!(range, [posting[0] + 3, posting[1] + 1, posting[2]]);
        assert_eq!(posting[2], 1);
    }

    #[test]
    fn compound_equalities_visit_the_selective_key_in_either_order_and_scalar_domain() {
        for count in [32_u128, 1024] {
            for text_tenant in [false, true] {
                for text_id in [false, true] {
                    let tenant = if text_tenant {
                        text("tenant")
                    } else {
                        CanonicalScalar::Int(1)
                    };
                    let key = |id| {
                        if text_id {
                            text(&format!("record-{id}"))
                        } else {
                            CanonicalScalar::Int(id as i64)
                        }
                    };
                    let snapshot = compound_snapshot(vec![
                        (1..=count)
                            .map(|id| compound_row(id, 1, None, tenant.clone(), key(id)))
                            .collect(),
                    ]);
                    assert_eq!(
                        snapshot
                            .patches
                            .iter()
                            .map(|patch| patch.len())
                            .sum::<usize>(),
                        count as usize,
                        "physical chunking must retain the whole logical fixture"
                    );
                    let mut predicates = [equal(KEY, tenant), equal(ID_KEY, key(17))];
                    for reverse in [false, true] {
                        if reverse {
                            predicates.reverse();
                        }
                        let (mut work, mut records, mut scratch) = (0, 0, 0);
                        let (rows, candidates) = bound_rows(
                            &snapshot,
                            || predicates.iter(),
                            CommitSeq(1),
                            &mut |event| {
                                match event {
                                    SourceEvent::Work => work += 1,
                                    SourceEvent::SnapshotRecord => records += 1,
                                    SourceEvent::ScratchEntry => scratch += 1,
                                }
                                Ok::<_, ()>(())
                            },
                        )
                        .unwrap()
                        .unwrap();
                        assert_eq!(
                            rows.iter().map(|row| row.vid).collect::<Vec<_>>(),
                            vec![VId(17)]
                        );
                        assert_eq!(candidates, 1, "tenant rows must not become candidates");
                        assert_eq!(records, 1);
                        assert_eq!(scratch, 1, "a borrowed posting needs only its admitted row");
                        assert!(
                            work < 64,
                            "ranking must not enumerate the {count}-row tenant"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn selected_numeric_peers_recheck_history_retirement_and_residual_keys() {
        let mut rows = vec![
            compound_row(1, 1, None, text("blue"), CanonicalScalar::Int(7)),
            compound_row(2, 1, None, text("blue"), float(7.0)),
            compound_row(3, 1, None, text("blue"), CanonicalScalar::Int(7)),
            compound_row(4, 1, None, text("blue"), float(7.0)),
        ];
        rows.extend(
            (10..100)
                .map(|id| compound_row(id, 1, None, text("blue"), CanonicalScalar::Int(id as i64))),
        );
        let snapshot = compound_snapshot(vec![
            rows,
            vec![
                compound_row(1, 2, None, text("blue"), float(7.0)),
                compound_row(2, 2, None, text("blue"), CanonicalScalar::Int(9)),
                compound_row(3, 2, None, text("red"), CanonicalScalar::Int(7)),
            ],
            vec![compound_row(1, 2, Some(3), text("blue"), float(7.0))],
        ]);
        for operand in [CanonicalScalar::Int(7), float(7.0)] {
            let predicates = [equal(KEY, text("blue")), equal(ID_KEY, operand)];
            for (at, expected) in [
                (1, vec![VId(1), VId(2), VId(3), VId(4)]),
                (2, vec![VId(1), VId(4)]),
                (3, vec![VId(4)]),
            ] {
                let (rows, candidates) =
                    bound_rows(&snapshot, || predicates.iter(), CommitSeq(at), &mut |_| {
                        Ok::<_, ()>(())
                    })
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    rows.iter().map(|row| row.vid).collect::<Vec<_>>(),
                    expected,
                    "at={at}"
                );
                assert_eq!(
                    candidates, 4,
                    "type changes must not duplicate a historical ID"
                );
            }
        }
    }

    #[test]
    fn selective_equalities_do_not_round_large_integer_keys() {
        for (integer, rounded) in [
            (9_007_199_254_740_993, 9_007_199_254_740_992.0),
            (i64::MAX, 9_223_372_036_854_775_808.0),
        ] {
            let mut rows = vec![
                compound_row(
                    1,
                    1,
                    None,
                    CanonicalScalar::Int(1),
                    CanonicalScalar::Int(integer),
                ),
                compound_row(2, 1, None, CanonicalScalar::Int(1), float(rounded)),
            ];
            rows.extend((3..32).map(|id| {
                compound_row(
                    id,
                    1,
                    None,
                    CanonicalScalar::Int(1),
                    CanonicalScalar::Int(id as i64),
                )
            }));
            let snapshot = compound_snapshot(vec![rows]);
            for (operand, expected) in [
                (CanonicalScalar::Int(integer), VId(1)),
                (float(rounded), VId(2)),
            ] {
                let predicates = [equal(KEY, CanonicalScalar::Int(1)), equal(ID_KEY, operand)];
                let (rows, candidates) =
                    bound_rows(&snapshot, || predicates.iter(), CommitSeq(1), &mut |_| {
                        Ok::<_, ()>(())
                    })
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    rows.iter().map(|row| row.vid).collect::<Vec<_>>(),
                    vec![expected]
                );
                assert_eq!(candidates, 1);
            }
        }
    }

    #[test]
    fn equality_residuals_stay_inside_the_admitted_operator_prefix() {
        let snapshot = compound_snapshot(vec![vec![
            compound_row(1, 1, None, text("blue"), text("first")),
            compound_row(2, 1, None, text("blue"), text("second")),
        ]]);
        let prepared = fgdb_gql::PreparedGqlQuery::prepare(
            "MATCH (n:N) RETURN n",
            &fgdb_gql::RelationBind::new().with_label("N", fgdb_delta_types::LabelId(1)),
        )
        .unwrap();
        let logical = fgdb_gql::algebra::GlaPlan::lower(prepared.plan());
        let slot = logical
            .operators()
            .iter()
            .find_map(|operator| match operator {
                GlaOperator::Select { slot, .. } => Some(*slot),
                _ => None,
            })
            .unwrap();
        for barrier in [
            GlaOperator::Limit {
                offset: 0,
                count: Some(1),
            },
            GlaOperator::ProjectValues { columns: vec![] },
        ] {
            // Neither selecting the later, narrower posting nor prechecking
            // its predicate before this window/projection boundary is legal.
            let operators = [
                GlaOperator::Select {
                    slot,
                    predicates: vec![equal(KEY, text("blue"))],
                },
                barrier,
                GlaOperator::Select {
                    slot,
                    predicates: vec![equal(ID_KEY, text("second"))],
                },
            ];
            let (rows, candidates) = bound_vertices(
                &snapshot,
                &operators,
                CommitSeq(1),
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                rows.iter().map(|row| row.vid).collect::<Vec<_>>(),
                vec![VId(1), VId(2)]
            );
            assert_eq!(candidates, 2);
        }
    }

    #[test]
    fn missing_equalities_are_empty_and_unindexed_predicates_keep_the_index_lazy() {
        let snapshot = compound_snapshot(vec![vec![compound_row(
            1,
            1,
            None,
            CanonicalScalar::Int(1),
            text("present"),
        )]]);
        let predicates = [equal(KEY, CanonicalScalar::Null)];
        assert!(
            bound_rows(
                &snapshot,
                || predicates.iter(),
                CommitSeq(1),
                &mut |_| Ok::<_, ()>(())
            )
            .unwrap()
            .is_none()
        );
        assert!(snapshot.property_index.get().is_none());
        let predicates = [
            equal(KEY, CanonicalScalar::Int(1)),
            equal(ID_KEY, text("missing")),
        ];
        let (rows, candidates) = bound_rows(
            &snapshot,
            || predicates.iter(),
            CommitSeq(1),
            &mut |event| {
                assert_eq!(
                    event,
                    SourceEvent::Work,
                    "an empty posting must not materialize candidates"
                );
                Ok::<_, ()>(())
            },
        )
        .unwrap()
        .unwrap();
        assert!(rows.is_empty());
        assert_eq!(candidates, 0);
    }

    #[test]
    fn every_compound_lookup_checkpoint_propagates_refusal() {
        let snapshot = compound_snapshot(vec![
            vec![
                compound_row(1, 1, None, text("blue"), CanonicalScalar::Int(7)),
                compound_row(2, 1, None, text("blue"), float(7.0)),
                compound_row(3, 1, None, text("blue"), CanonicalScalar::Int(3)),
                compound_row(4, 1, None, text("blue"), CanonicalScalar::Int(4)),
            ],
            vec![compound_row(1, 2, None, text("blue"), float(7.0))],
        ]);
        let predicates = [equal(KEY, text("blue")), equal(ID_KEY, float(7.0))];
        let run = |stop| {
            let mut events = 0;
            let result = bound_rows(&snapshot, || predicates.iter(), CommitSeq(2), &mut |_| {
                events += 1;
                if events == stop { Err(stop) } else { Ok(()) }
            })
            .map(|selected| {
                selected.map(|(rows, count)| {
                    (rows.iter().map(|row| row.vid).collect::<Vec<_>>(), count)
                })
            });
            (result, events)
        };
        let (result, total) = run(usize::MAX);
        assert_eq!(result, Ok(Some((vec![VId(1), VId(2)], 2))));
        for stop in 1..=total {
            assert_eq!(run(stop), (Err(stop), stop));
        }
    }
}
