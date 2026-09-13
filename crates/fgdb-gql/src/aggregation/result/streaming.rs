//! Retire completed source-identity groups from the ordinary GLA visitor.
//!
//! The physical-access owner proves contiguous, ascending root identities.
//! Exactly one evaluation key must be that identity. A finite result page then
//! needs one active accumulator group and at most SKIP + LIMIT compact group
//! summaries, not the accumulator maps/sets for every matched source vertex.
//! This is not a streaming source, byte-memory governor, or spill mechanism.

use super::*;

type QueryError<E, C> = GqlQueryError<GraphAggregateError<E>, C>;

struct OwnedGroup<'a> {
    key: [ValueRef<'a>; 1],
    state: Vec<Accumulator<'a>>,
}
impl<'a> OwnedGroup<'a> {
    fn view(&self) -> Group<'_, 'a> {
        Group { key: &self.key, state: &self.state }
    }

    /// Retained groups are never updated again. COUNT DISTINCT only needs its
    /// count; numeric DISTINCT only needs the finished sum and nonnull count.
    /// Extrema borrow immutable source payloads, so no property clone is needed.
    fn compact<E>(&mut self, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>)
        -> Result<(), E>
    {
        for state in &mut self.state {
            control(GlaExecutionEvent::Work)?;
            match state {
                Accumulator::Distinct(seen) => *state = Accumulator::Count(seen.len() as u64),
                Accumulator::Numeric(numeric) => numeric.release_distinct_set(),
                _ => {}
            }
        }
        Ok(())
    }
}

/// A private execution specialization, not a different prepared definition.
/// The active group is the only state receiving inputs; the heap holds retired
/// groups. A resource/cancellation refusal releases no partially ranked output.
pub(in crate::aggregation) struct RootGroups<'q, 'a> {
    query: &'q PreparedGraphAggregate,
    prefix: usize,
    active: Option<OwnedGroup<'a>>,
    heap: Vec<OwnedGroup<'a>>,
    deferred_having: Option<usize>,
}

impl<'q, 'a> RootGroups<'q, 'a> {
    pub(in crate::aggregation) fn new(query: &'q PreparedGraphAggregate) -> Option<Self> {
        if query.keys.len() != 1 || query.needs_output_distinct()
            || !weighted::root_groups_are_contiguous(query)
        {
            return None;
        }
        let columns = query.input.plan().operators().iter().rev().find_map(|op| match op {
            GlaOperator::ProjectValues { columns } => Some(columns),
            _ => None,
        })?;
        if !matches!(columns.get(query.keys[0]), Some(ValueProjection::Vertex { slot })
            if slot.ordinal() == 0)
        {
            return None;
        }
        let count = query.count?;
        // A zero page needs no retained groups even with an enormous offset.
        // An unrepresentable positive prefix keeps the general implementation.
        let prefix = if count == 0 { 0 }
            else { usize::try_from(query.offset.checked_add(count)?).ok()? };
        Some(Self { query, prefix, active: None, heap: Vec::new(), deferred_having: None })
    }

    pub(in crate::aggregation) fn push<E, C>(
        &mut self,
        values: &[ValueRef<'a>],
        multiplicity: weighted::Multiplicity,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<(), QueryError<E, C>> {
        control(GlaExecutionEvent::Work)?;
        let key = values[self.query.keys[0]];
        if !matches!(key, ValueRef::Vertex(_)) {
            return Err(GqlQueryError::Source(GraphAggregateError::NonMonotonicGroups));
        }
        if let Some(active) = &self.active {
            // This guard is load-bearing if a future physical access path
            // violates its contiguity claim. Never emit a silently split group.
            match active.key[0].cmp(&key) {
                Ordering::Greater => return Err(GqlQueryError::Source(GraphAggregateError::NonMonotonicGroups)),
                Ordering::Less => self.retire(control)?,
                Ordering::Equal => {}
            }
        }
        if self.active.is_none() {
            control(GlaExecutionEvent::ScratchEntry)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let state = new_group(&self.query.aggregates, control)?;
            self.active = Some(OwnedGroup { key: [key], state });
        }
        let group = self.active.as_mut().expect("active root group was admitted");
        update_group(&self.query.aggregates, &mut group.state, values, multiplicity, control)
    }

    fn retire<E, C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<(), QueryError<E, C>> {
        let Some(mut candidate) = self.active.take() else { return Ok(()); };
        if self.deferred_having.is_some() {
            return Ok(());
        }
        control(GlaExecutionEvent::Work)?;
        match self.query.keep(candidate.view(), control) {
            Ok(false) => return Ok(()),
            // Ordinary aggregation consumes all inputs before running HAVING.
            // Preserve that data-error precedence: a later property/source or
            // aggregate arithmetic error must not be hidden by early retirement.
            // Source/root order is also canonical group order for this profile,
            // so the first deferred HAVING predicate is the same one as before.
            Err(GqlQueryError::Source(GraphAggregateError::NonIntegerHaving { predicate })) => {
                self.deferred_having = Some(predicate);
                self.heap.clear();
                return Ok(());
            }
            Err(error) => return Err(error),
            Ok(true) => {}
        }
        if self.prefix == 0 {
            return Ok(());
        }
        if self.heap.len() < self.prefix {
            candidate.compact(control)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            self.heap.push(candidate);
            let mut child = self.heap.len() - 1;
            while child > 0 {
                let parent = (child - 1) / 2;
                if self.query.compare(self.heap[parent].view(), self.heap[child].view(), control)?
                    != Ordering::Less
                {
                    break;
                }
                control(GlaExecutionEvent::Work)?;
                self.heap.swap(parent, child);
                child = parent;
            }
        } else if self.query.compare(candidate.view(), self.heap[0].view(), control)? == Ordering::Less {
            // Do not destroy the old complete candidate until all fallible
            // compaction work for its replacement has succeeded.
            candidate.compact(control)?;
            control(GlaExecutionEvent::Work)?;
            self.heap[0] = candidate;
            self.sift_down(0, self.heap.len(), control)?;
        }
        Ok(())
    }

    fn sift_down<E>(
        &mut self,
        mut root: usize,
        end: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while root < end / 2 {
            let mut child = 2 * root + 1;
            if child + 1 < end && self.query.compare(
                self.heap[child].view(), self.heap[child + 1].view(), control,
            )? == Ordering::Less {
                child += 1;
            }
            if self.query.compare(self.heap[root].view(), self.heap[child].view(), control)?
                != Ordering::Less
            {
                break;
            }
            control(GlaExecutionEvent::Work)?;
            self.heap.swap(root, child);
            root = child;
        }
        Ok(())
    }

    pub(in crate::aggregation) fn finish<E, C>(
        mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<Vec<GraphAggregateRow>, QueryError<E, C>> {
        self.retire(control)?;
        if let Some(predicate) = self.deferred_having {
            return Err(GqlQueryError::Source(GraphAggregateError::NonIntegerHaving { predicate }));
        }
        for end in (1..self.heap.len()).rev() {
            control(GlaExecutionEvent::Work)?;
            self.heap.swap(0, end);
            self.sift_down(0, end, control)?;
        }
        let offset = usize::try_from(self.query.offset).unwrap_or(usize::MAX);
        let count = self.query.count.and_then(|count| usize::try_from(count).ok()).unwrap_or(usize::MAX);
        let mut output = Vec::new();
        for group in self.heap.iter().skip(offset).take(count) {
            output.push(group.view().copy_owned(self.query.key_output.as_ref(), self.query.output_aggregates, control)?);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText};
    use fgdb_delta_types::LabelId;

    fn prepare(text: &str) -> PreparedGraphAggregate {
        PreparedGraphAggregateText::prepare(text, |kind, name| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
    }
    fn allow(_: GlaExecutionEvent) -> Result<(), QueryError<(), usize>> { Ok(()) }
    fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000) }
    const HEAD: &str = "MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS n,COUNT(DISTINCT b.p) AS d,\
        SUM(b.p) AS total,AVG(DISTINCT b.p) AS mean,MIN(b.p) AS least,MAX(b.p) AS most GROUP BY a";

    #[test]
    fn streaming_requires_a_proved_root_group_and_representable_finite_prefix() {
        for text in [
            "MATCH (a)-[:R]->(b) RETURN a,AVG(b.p) AS n GROUP BY a LIMIT 2",
            "MATCH (a)<-[:R]-(b) RETURN a,AVG(b.p) AS n GROUP BY a LIMIT 2",
            "MATCH (a)-[:R]-(b) RETURN a,AVG(b.p) AS n GROUP BY a LIMIT 2",
            "MATCH (a:L)-[:R]->(b) RETURN a,COUNT(*) AS n GROUP BY a LIMIT 2",
            "MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)-[:R]->(c) RETURN a,COUNT(c) AS n GROUP BY a LIMIT 2",
            "MATCH (a)-[:R]->(b) RETURN AVG(b.p) AS n GROUP BY a LIMIT 2",
            "MATCH (a)-[:R]->(b) RETURN a,AVG(b.p) AS n GROUP BY a SKIP 18446744073709551615 LIMIT 0",
        ] {
            let query = prepare(text);
            let bytes = query.canonical_bytes();
            assert!(RootGroups::new(&query).is_some(), "{text}");
            assert_eq!(query.canonical_bytes(), bytes);
        }
        for text in [
            "MATCH (a) RETURN a,AVG(a.p) AS n GROUP BY a LIMIT 2",
            "MATCH (a)-[:R]->(b) RETURN b,AVG(b.p) AS n GROUP BY b LIMIT 2",
            "MATCH (a)-[:R]->(b) RETURN a,b,AVG(b.p) AS n GROUP BY a,b LIMIT 2",
            "MATCH (a)-[:R]->(b) RETURN a.p,AVG(b.p) AS n GROUP BY a.p LIMIT 2",
            "MATCH (a)-[:R]->(b) RETURN a,AVG(b.p) AS n GROUP BY a",
            "MATCH (a)-[:R]->(b) RETURN DISTINCT AVG(b.p) AS n GROUP BY a LIMIT 2",
            "MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS n GROUP BY a LIMIT 2",
            "MATCH (a)-[:R]->(b) RETURN a,AVG(b.p) AS n GROUP BY a SKIP 1 LIMIT 18446744073709551615",
        ] { assert!(RootGroups::new(&prepare(text)).is_none(), "{text}"); }
    }

    #[test]
    fn retired_groups_match_full_group_filter_ranking_and_projection_before_pages() {
        let scalars = [CanonicalScalar::Null, CanonicalScalar::Int(-7),
            CanonicalScalar::Int(0), CanonicalScalar::Int(7), CanonicalScalar::Int(i64::MAX)];
        let bags: [&[usize]; 5] = [&[], &[0], &[1, 1, 2], &[2, 3, 3], &[3, 4]];
        let base = prepare(&format!("{HEAD} LIMIT 2"));
        for mut encoded in 0..5_usize.pow(3) {
            let choices: Vec<_> = (0..3).map(|_| { let at = encoded % 5; encoded /= 5; at }).collect();
            for ordered in [false, true] { for filtered in [false, true] {
                for offset in 0..=4 { for count in [0, 1, 3] {
                    let mut query = base.clone(); query.offset = offset; query.count = Some(count);
                    query.ordering = if ordered { vec![GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(3))] } else { Vec::new() };
                    query.having = if filtered { vec![GraphAggregateFilter {
                        column: GraphAggregateColumn::Aggregate(2), test: GraphAggregateTest::IsNotNull,
                    }] } else { Vec::new() };
                    for hide_key in [false, true] { for prefix in [0, 3, 6] {
                        let query = query.clone().with_key_output_columns(if hide_key { &[] } else { &[0] }).unwrap()
                            .with_aggregate_output_prefix(prefix).unwrap();
                        let mut streaming = RootGroups::new(&query).unwrap();
                        let mut materialized = BTreeMap::new();
                        for (id, choice) in choices.iter().enumerate() {
                            for at in bags[*choice] {
                                let values = [ValueRef::Vertex(VId(id as u128)), ValueRef::Scalar(&scalars[*at])];
                                streaming.push(&values, weighted::Multiplicity::ONE, &mut allow).unwrap();
                                assert!(streaming.heap.len() <= (offset + count) as usize);
                                let states = materialized.entry(vec![values[0]])
                                    .or_insert_with(|| new_group(&query.aggregates, &mut allow).unwrap());
                                update_group(&query.aggregates, states, &values, weighted::Multiplicity::ONE, &mut allow).unwrap();
                            }
                        }
                        assert_eq!(streaming.finish(&mut allow).unwrap(), query.finish_groups(&materialized, &mut allow).unwrap());
                    }}
                }}
            }}
        }
    }

    #[test]
    fn thousands_of_groups_retain_only_the_page_prefix_without_distinct_sets() {
        let query = prepare(&format!("{HEAD} ORDER BY mean DESC SKIP 2 LIMIT 3"));
        let count = 4096;
        let values: Vec<_> = (0..=count).map(CanonicalScalar::Int).collect();
        let mut streaming = RootGroups::new(&query).unwrap();
        for root in 0..count as usize {
            for at in [root, root, root + 1] {
                streaming.push(&[ValueRef::Vertex(VId(root as u128)), ValueRef::Scalar(&values[at])],
                    weighted::Multiplicity::ONE, &mut allow).unwrap();
                assert!(streaming.heap.len() <= 5);
                for retired in &streaming.heap {
                    assert!(matches!(retired.state[1], Accumulator::Count(2)));
                    let Accumulator::Numeric(mean) = &retired.state[3] else { panic!("average accumulator"); };
                    assert_eq!(mean.retained_distinct_values(), 0);
                }
            }
        }
        let rows = streaming.finish(&mut allow).unwrap();
        assert_eq!(rows.iter().map(|row| row.keys()[0].as_vertex().unwrap()).collect::<Vec<_>>(),
            vec![VId(4093), VId(4092), VId(4091)]);
        assert!(rows.iter().all(|row| row.get(0).unwrap().as_count() == Some(3)
            && row.get(1).unwrap().as_count() == Some(2)));
        assert_eq!(rows[0].get(3).unwrap().as_average().unwrap().numerator(), 8187);
        assert_eq!(rows[0].get(3).unwrap().as_average().unwrap().denominator(), 2);
    }

    #[test]
    fn a_root_order_contract_violation_refuses_instead_of_splitting_a_group() {
        let query = prepare(&format!("{HEAD} LIMIT 2"));
        let scalar = CanonicalScalar::Int(1);
        let mut streaming = RootGroups::new(&query).unwrap();
        for root in [1, 1, 3] {
            streaming.push(&[ValueRef::Vertex(VId(root)), ValueRef::Scalar(&scalar)],
                weighted::Multiplicity::ONE, &mut allow).unwrap();
        }
        let result = streaming.push(&[ValueRef::Vertex(VId(1)), ValueRef::Scalar(&scalar)],
            weighted::Multiplicity::ONE, &mut allow);
        assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::NonMonotonicGroups))));
        let result = RootGroups::new(&query).unwrap().push(&[ValueRef::Scalar(&NULL), ValueRef::Scalar(&scalar)],
            weighted::Multiplicity::ONE, &mut allow);
        assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::NonMonotonicGroups))));
    }

    #[test]
    fn deferred_having_errors_do_not_hide_later_source_or_aggregate_input_errors() {
        let edges = [(VId(1), RelationId(1), VId(10)), (VId(2), RelationId(1), VId(11)),
            (VId(3), RelationId(1), VId(12))];
        let boolean = CanonicalScalar::Bool(true); let integer = CanonicalScalar::Int(1);
        for tail in ["HAVING least > 0", "HAVING TRUE OR least > 0", "HAVING FALSE AND least > 0"] {
            let query = prepare(&format!("MATCH (a)-[:R]->(b) RETURN a,MIN(b.p) AS least GROUP BY a {tail} LIMIT 0"));
            assert!(RootGroups::new(&query).is_some());
            let mut reads = 0;
            let result = query.execute_governed(3, [], edges, |_, _| Ok::<_, &str>(true), |vid, _| {
                reads += 1;
                if vid == VId(12) { Err("last root unreadable") }
                else { Ok(Some(if vid == VId(10) { &boolean } else { &integer })) }
            }, policy(), || Ok::<_, ()>(()));
            assert_eq!(reads, 3);
            assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source("last root unreadable")))));
            let mut reads = 0;
            let result = query.execute_governed(3, [], edges, |_, _| Ok::<_, ()>(true), |vid, _| {
                reads += 1; Ok(Some(if vid == VId(10) { &boolean } else { &integer }))
            }, policy(), || Ok::<_, ()>(()));
            assert_eq!(reads, 3);
            assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::NonIntegerHaving { .. }))));
        }
        let query = prepare("MATCH (a)-[:R]->(b) RETURN a,MIN(b.p) AS least,SUM(b.q) AS total \
            GROUP BY a HAVING least > 0 LIMIT 0");
        let result = query.execute_governed(3, [], edges, |_, _| Ok::<_, ()>(true), |vid, key| {
            Ok(Some(if (vid == VId(10) && key == PropertyKeyId(1))
                || (vid == VId(12) && key == PropertyKeyId(2)) { &boolean } else { &integer }))
        }, policy(), || Ok::<_, ()>(()));
        assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 1 }))));
    }

    #[test]
    fn every_streaming_update_retirement_and_output_checkpoint_is_cancellable() {
        let query = prepare(&format!("{HEAD} HAVING total IS NOT NULL ORDER BY mean DESC SKIP 1 LIMIT 2"));
        let values: Vec<_> = (0..=4).map(CanonicalScalar::Int).collect();
        let run = |stop: Option<usize>| {
            let mut at = 0;
            let mut control = |_| { at += 1; if stop == Some(at) { Err(GqlQueryError::Interrupted(at)) }
                else { Ok::<_, QueryError<(), usize>>(()) } };
            let mut streaming = RootGroups::new(&query).unwrap();
            let result = (|| {
                for root in 0..4 {
                    for item in [root, root + 1, root + 1] {
                        streaming.push(&[ValueRef::Vertex(VId(root as u128)), ValueRef::Scalar(&values[item])],
                            weighted::Multiplicity::ONE, &mut control)?;
                    }
                }
                streaming.finish(&mut control)
            })();
            (result, at)
        };
        let (expected, events) = run(None); let expected = expected.unwrap();
        assert_eq!(expected.len(), 2);
        for stop in 1..=events {
            let (result, at) = run(Some(stop));
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
        assert_eq!(run(None).0.unwrap(), expected);
    }
}
