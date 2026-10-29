//! Lazy unions of scalar-membership postings in canonical vertex order.
//!
//! The bound Boolean program owns its literals. Opening clones that immutable
//! definition, without collecting keys, histories, or candidate identities.
//! First demand proves scalar conversion cannot fail, then retains at most the
//! same 64 posting heads as range scans. Wider unions and collection-bearing
//! property domains use the complete history scan before any row is emitted.

use super::{IndexMap, index_successor, numeric, range};
use crate::gql_exec::source::{
    History, PropertyEqualityIndex, SourceEvent, VertexHistoryScanCursor,
};
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::BoundBooleanExpression;
use fgdb_gql::scan_stream::SourceStep;
use fgdb_gql::stream::VertexScanEvent;
use fgdb_types::{CanonicalScalar, VId};
use std::borrow::Cow;
use std::sync::Arc;

enum Frontier {
    Pending,
    Buckets(Vec<range::Head>),
    Histories(VertexHistoryScanCursor),
}

pub(super) struct MembershipCandidates {
    key: PropertyKeyId,
    expression: BoundBooleanExpression,
    frontier: Frontier,
}

impl MembershipCandidates {
    pub(super) fn new(expression: &BoundBooleanExpression) -> Option<Self> {
        let (key, _) = expression.literal_membership_conjunct()?;
        Some(Self {
            key,
            expression: expression.clone(),
            frontier: Frontier::Pending,
        })
    }

    pub(super) fn key(&self) -> PropertyKeyId {
        self.key
    }

    /// Recheck only the necessary bound, never the whole Boolean expression.
    /// Scoped callers have already admitted the current vertex and this key.
    /// The same check applies to postings and full-history fallbacks, so a
    /// hidden collection or extra posting cannot change visible accounting.
    /// A stored collection must reach the ordinary evaluator: scalar conversion
    /// may fail there before IN returns false, including for an empty list.
    pub(super) fn accepts<C>(
        &self,
        value: Option<&CanonicalScalar>,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<bool, C> {
        let Some(value) = value else {
            return Ok(true);
        };
        let (_, members) = self
            .expression
            .literal_membership_conjunct()
            .expect("the immutable membership definition was checked at opening");
        for member in members {
            control(VertexScanEvent::Work)?;
            if fgdb_gql::algebra::IntegerComparison::Equal
                .accepts_scalar_pair(Some(value), Some(member.as_ref()))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The complete-scan fallback retains the history it just enumerated,
    /// exactly as an ordinary root scan does. Correlated probes still seek.
    pub(super) fn history(&self, vid: VId) -> Option<&History> {
        match &self.frontier {
            Frontier::Histories(scan) => scan.history(vid),
            Frontier::Pending | Frontier::Buckets(_) => None,
        }
    }

    pub(super) fn next<C>(
        &mut self,
        index: &PropertyEqualityIndex,
        after: Option<VId>,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<SourceStep<VId>, C> {
        control(VertexScanEvent::Work)?;
        if matches!(self.frontier, Frontier::Pending) {
            // Publish no partial setup: a refused proof, lookup, or allocation
            // leaves the source pending and safe to retry from the same cut.
            self.frontier = self.prepare(index, after, control)?;
        }
        match &mut self.frontier {
            Frontier::Buckets(heads) => {
                while let Some(head) = heads.first() {
                    if after.is_none_or(|after| head.vid > after) {
                        return Ok(SourceStep::Item(head.vid));
                    }
                    let next = index_successor(&head.root, after, control)?;
                    range::advance_head(heads, next, control)?;
                }
                Ok(SourceStep::Exhausted)
            }
            Frontier::Histories(scan) => scan
                .next(&index.histories, &mut |event| control(stream_event(event)))
                .map(|next| next.map_or(SourceStep::Exhausted, SourceStep::Item)),
            Frontier::Pending => unreachable!("frontier selected above"),
        }
    }

    fn prepare<C>(
        &self,
        index: &PropertyEqualityIndex,
        after: Option<VId>,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Frontier, C> {
        if !index.scalar_only(self.key, &mut |event| control(stream_event(event)))? {
            // Scalar IN eagerly loads its property value. A collection that
            // cannot enter the scalar evaluator must still raise on the scan.
            return Ok(Frontier::Histories(VertexHistoryScanCursor::default()));
        }
        let (_, members) = self
            .expression
            .literal_membership_conjunct()
            .expect("the immutable membership definition was checked at opening");
        let mut heads: Vec<range::Head> = Vec::new();
        for member in members {
            control(VertexScanEvent::Work)?;
            let peers = match member.as_ref() {
                CanonicalScalar::Null => continue,
                value @ (CanonicalScalar::Int(_) | CanonicalScalar::Float(_)) => {
                    numeric::equality_peers(value).map(|peer| peer.map(Cow::Owned))
                }
                value => [Some(Cow::Borrowed(value)), None],
            };
            for peer in peers.into_iter().flatten() {
                control(VertexScanEvent::Work)?;
                let Ok(encoded) = peer.encode() else {
                    return Ok(Frontier::Histories(VertexHistoryScanCursor::default()));
                };
                let Some(root) = lookup(index, self.key, &encoded, control)? else {
                    continue;
                };
                let Some(root_node) = root.0.as_ref() else {
                    continue;
                };
                // Repeated literals and exact Int/Float peers may name the
                // same posting. Shared immutable roots prove identical sets;
                // duplicates consume work but no frontier space.
                let mut duplicate = false;
                for head in &heads {
                    control(VertexScanEvent::Work)?;
                    if head
                        .root
                        .0
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, root_node))
                    {
                        duplicate = true;
                        break;
                    }
                }
                if duplicate {
                    continue;
                }
                let Some(vid) = index_successor(root, after, control)? else {
                    continue;
                };
                if heads.len() == range::MAX_RANGE_HEADS {
                    // The cap bounds live frontier state, independent of list
                    // length and graph size. Work/scratch already spent is
                    // retained by the caller's meter, never refunded.
                    return Ok(Frontier::Histories(VertexHistoryScanCursor::default()));
                }
                range::push_head(
                    &mut heads,
                    range::Head {
                        vid,
                        root: root.clone(),
                    },
                    control,
                )?;
            }
        }
        Ok(Frontier::Buckets(heads))
    }
}

fn stream_event(event: SourceEvent) -> VertexScanEvent {
    match event {
        SourceEvent::Work | SourceEvent::SnapshotRecord => VertexScanEvent::Work,
        SourceEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
    }
}

fn lookup<'a, C>(
    index: &'a PropertyEqualityIndex,
    key: PropertyKeyId,
    encoded: &[u8],
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<Option<&'a IndexMap<VId, ()>>, C> {
    let mut node = index.candidates.0.as_deref();
    while let Some(current) = node {
        control(VertexScanEvent::Work)?;
        let order = key
            .cmp(&current.key.0)
            .then_with(|| encoded.cmp(current.key.1.as_ref()));
        match order {
            core::cmp::Ordering::Less => node = current.left.0.as_deref(),
            core::cmp::Ordering::Greater => node = current.right.0.as_deref(),
            core::cmp::Ordering::Equal => return Ok(Some(&current.value)),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_gql::algebra::{
        GraphBooleanExpression, GraphBooleanOp as BooleanOp, GraphBooleanOperand as Operand,
        GraphColumn, GraphPatternBuilder, IntegerComparison,
    };
    use fgdb_gql::stream::VertexScanPlan;
    use fgdb_strata::vertex::{VertexPatchRows, VertexRow, decode_patch, encode_patch};
    use fgdb_types::{CanonicalList, CanonicalPropertyValue, CommitSeq};
    use std::collections::BTreeSet;

    const KEY: PropertyKeyId = PropertyKeyId(7);

    fn definition(values: &[CanonicalScalar]) -> BoundBooleanExpression {
        let property = Operand::Property {
            variable: "n",
            key: KEY,
        };
        let mut program = Vec::new();
        for value in values {
            program.push(BooleanOp::Compare {
                left: property,
                comparison: IntegerComparison::Equal,
                right: Operand::Literal(value),
            });
            if program.len() > 1 {
                program.push(BooleanOp::Or);
            }
        }
        if values.is_empty() {
            program.extend([
                BooleanOp::IsNull {
                    operand: property,
                    is_null: true,
                },
                BooleanOp::Truth(Some(false)),
                BooleanOp::And,
            ]);
        }
        let expression = GraphBooleanExpression::prepare(&program).unwrap();
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        builder.filter_boolean(&expression).unwrap();
        // A property-reading filter needs a value projection: the identity
        // projection refuses RequiresValueProjection (fgdb-gql
        // vertex_stream_filters property_sensitive_filters_keep_the_legacy_projection_refusal).
        let pattern = builder
            .prepare_values(&[GraphColumn::vertex("node", "n")], 0, None)
            .unwrap();
        VertexScanPlan::compile(pattern.plan())
            .unwrap()
            .leading_root_membership()
            .unwrap()
            .clone()
    }

    fn row(vid: VId, at: u64, value: CanonicalScalar) -> VertexRow {
        VertexRow {
            vid,
            birth_ordinal: vid.0 as u64,
            created_at: CommitSeq(at),
            retired_at: None,
            labels: vec![],
            props: vec![(KEY, value.into())],
        }
    }

    fn patch(mut rows: Vec<VertexRow>) -> VertexPatchRows {
        rows.sort_by_key(|row| (row.vid, row.created_at));
        decode_patch(&encode_patch(&rows).unwrap()).unwrap()
    }

    fn drain<C>(
        source: &mut MembershipCandidates,
        index: &PropertyEqualityIndex,
        after: &mut Option<VId>,
        output: &mut Vec<VId>,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<(), C> {
        loop {
            match source.next(index, *after, control)? {
                SourceStep::Item(vid) => {
                    assert!(after.is_none_or(|previous| previous < vid));
                    assert_eq!(
                        source.history(vid).is_some(),
                        matches!(source.frontier, Frontier::Histories(_)),
                        "fallback reuses only the currently delivered history"
                    );
                    assert!(source.history(VId(vid.0 ^ 1)).is_none());
                    *after = Some(vid);
                    output.push(vid);
                }
                SourceStep::Skipped(_) => unreachable!("membership fallback keeps all histories"),
                SourceStep::Exhausted => return Ok(()),
            }
        }
    }

    #[test]
    fn every_refusal_preserves_membership_retry_at_the_frontier_capacity_boundary() {
        for distinct in [1, 4, range::MAX_RANGE_HEADS, range::MAX_RANGE_HEADS + 1] {
            let mut first = Vec::new();
            let mut changed = Vec::new();
            let mut members = Vec::new();
            for value in 0..distinct {
                let scalar = CanonicalScalar::Int(value as i64);
                members.extend([scalar.clone(), scalar.clone()]);
                for copy in 0..2 {
                    let vid = VId((value * 2 + copy) as u128);
                    first.push(row(vid, 1, scalar.clone()));
                    if copy == 0 {
                        changed.push(row(
                            vid,
                            2,
                            CanonicalScalar::Int(((value + 1) % distinct) as i64),
                        ));
                    }
                }
            }
            first.push(row(VId(1_u128 << 100), 1, CanonicalScalar::Int(0)));
            first.push(row(VId(u128::MAX), 1, CanonicalScalar::Int(0)));
            first.push(row(VId(50_000), 1, CanonicalScalar::Int(-1)));
            let patches = vec![patch(first), patch(changed)];
            let index = PropertyEqualityIndex::build(&patches);
            let indexed = distinct <= range::MAX_RANGE_HEADS;
            let expected: Vec<_> = patches
                .iter()
                .flat_map(|patch| patch.iter())
                .filter(|row| !indexed || row.vid != VId(50_000))
                .map(|row| row.vid)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let expression = definition(&members);
            let mut source = MembershipCandidates::new(&expression).unwrap();
            assert!(matches!(source.frontier, Frontier::Pending));
            let mut total = 0;
            let mut scratch = 0;
            let mut full = Vec::new();
            drain(&mut source, &index, &mut None, &mut full, &mut |event| {
                total += 1;
                scratch += usize::from(matches!(event, VertexScanEvent::ScratchEntry));
                Ok::<_, usize>(())
            })
            .unwrap();
            assert_eq!(full, expected);
            assert_eq!(matches!(source.frontier, Frontier::Buckets(_)), indexed);
            assert_eq!(
                scratch,
                if indexed {
                    distinct
                } else {
                    range::MAX_RANGE_HEADS + usize::from(index.histories.height())
                },
                "duplicate list members do not allocate duplicate posting heads"
            );
            for stop in 1..=total {
                let mut source = MembershipCandidates::new(&expression).unwrap();
                let mut after = None;
                let mut output = Vec::new();
                let mut seen = 0;
                let result = drain(&mut source, &index, &mut after, &mut output, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                });
                assert_eq!(result, Err(stop), "distinct={distinct}, stop={stop}");
                assert_eq!(seen, stop);
                assert_eq!(output, expected[..output.len()]);
                drain(&mut source, &index, &mut after, &mut output, &mut |_| {
                    Ok::<_, usize>(())
                })
                .unwrap();
                assert_eq!(output, expected, "distinct={distinct}, stop={stop}");
            }
        }
    }

    #[test]
    fn membership_candidates_match_independent_scalar_comparison_across_numeric_boundaries() {
        use fgdb_types::{CanonicalDecimal, CanonicalF64};
        let float = |value| CanonicalScalar::Float(CanonicalF64::new(value));
        let values = vec![
            CanonicalScalar::Int(1),
            float(1.0),
            CanonicalScalar::Null,
            float(f64::NAN),
            CanonicalScalar::Int(9_007_199_254_740_993),
            float(9_007_199_254_740_992.0),
            CanonicalScalar::Int(9_007_199_254_740_992),
            CanonicalScalar::Int(i64::MAX),
            float(i64::MAX as f64),
            CanonicalScalar::Decimal(CanonicalDecimal::from_integer(1).unwrap()),
            CanonicalScalar::Bool(true),
            CanonicalScalar::ucs_basic_text("match").unwrap(),
            CanonicalScalar::Int(0),
        ];
        let patches = vec![patch(
            values
                .iter()
                .enumerate()
                .map(|(at, value)| row(VId(at as u128), 1, value.clone()))
                .collect(),
        )];
        let index = PropertyEqualityIndex::build(&patches);
        let mut batches: Vec<Vec<CanonicalScalar>> = values
            .iter()
            .map(|value| vec![value.clone(), value.clone(), CanonicalScalar::Null])
            .collect();
        batches.extend([
            vec![],
            vec![CanonicalScalar::Null],
            vec![CanonicalScalar::Int(1), float(1.0), CanonicalScalar::Int(1)],
            values.clone(),
        ]);
        for members in batches {
            let expected: Vec<_> = values
                .iter()
                .enumerate()
                .filter(|(_, value)| {
                    members.iter().any(|member| {
                        IntegerComparison::Equal.accepts_scalar_pair(Some(value), Some(member))
                    })
                })
                .map(|(at, _)| VId(at as u128))
                .collect();
            let expression = definition(&members);
            let mut source = MembershipCandidates::new(&expression).unwrap();
            let mut output = Vec::new();
            drain(&mut source, &index, &mut None, &mut output, &mut |_| {
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(output, expected, "members={members:?}");
        }
    }

    #[test]
    fn historical_collections_force_the_complete_scan_even_after_a_scalar_update() {
        let mut collection = row(VId(0), 1, CanonicalScalar::Int(0));
        collection.props[0].1 = CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![CanonicalScalar::Int(1).into()]).unwrap(),
        );
        let patches = vec![
            patch(vec![
                collection,
                row(VId(1), 1, CanonicalScalar::Int(1)),
                row(VId(u128::MAX), 1, CanonicalScalar::Int(9)),
            ]),
            patch(vec![row(VId(0), 2, CanonicalScalar::Int(9))]),
        ];
        let index = PropertyEqualityIndex::build(&patches);
        for members in [vec![], vec![CanonicalScalar::Int(1)]] {
            let expression = definition(&members);
            let mut source = MembershipCandidates::new(&expression).unwrap();
            let mut output = Vec::new();
            drain(&mut source, &index, &mut None, &mut output, &mut |_| {
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(output, [VId(0), VId(1), VId(u128::MAX)]);
            assert!(matches!(source.frontier, Frontier::Histories(_)));
        }
    }
}
