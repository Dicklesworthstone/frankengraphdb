//! HAVING, deterministic group ranking and late result materialization.
//!
//! Source and aggregation are already complete. This stage never filters child
//! bindings or pushes LIMIT into the aggregate. Selected groups borrow their
//! keys/state, and a finite page retains at most offset + count references.

use super::*;
use crate::algebra::IntegerComparison;
use std::cmp::Ordering;

pub const MAX_AGGREGATE_FILTERS: usize = 64;

/// Column indices are in key_columns() or aggregate_columns(), not the child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphAggregateColumn {
    GroupKey(usize),
    Aggregate(usize),
}

/// Integer comparisons use i128 without subtraction, truncation or float
/// coercion. Null yields UNKNOWN (and is not retained by HAVING), including NE.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphAggregateTest {
    Integer {
        comparison: IntegerComparison,
        value: i128,
    },
    IsNull,
    IsNotNull,
}
impl core::fmt::Debug for GraphAggregateTest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Integer { comparison, .. } => f
                .debug_struct("Integer")
                .field("comparison", comparison)
                .field("value", &"[REDACTED]")
                .finish(),
            Self::IsNull => f.write_str("IsNull"),
            Self::IsNotNull => f.write_str("IsNotNull"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphAggregateFilter {
    pub column: GraphAggregateColumn,
    pub test: GraphAggregateTest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphNullPlacement {
    First,
    Last,
}

/// Null placement is independent of ascending/descending direction. Canonical
/// ascending group keys break all explicit sort ties, regardless of direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphAggregateOrder {
    pub column: GraphAggregateColumn,
    pub descending: bool,
    pub nulls: GraphNullPlacement,
}
impl GraphAggregateOrder {
    #[must_use]
    pub const fn ascending(column: GraphAggregateColumn) -> Self {
        Self {
            column,
            descending: false,
            nulls: GraphNullPlacement::Last,
        }
    }
    #[must_use]
    pub const fn descending(column: GraphAggregateColumn) -> Self {
        Self {
            column,
            descending: true,
            nulls: GraphNullPlacement::Last,
        }
    }
}

#[derive(Clone, Copy)]
struct Group<'g, 'a> {
    key: &'g [ValueRef<'a>],
    state: &'g [Accumulator<'a>],
}

#[derive(Clone, Copy)]
enum Cell<'a> {
    Count(u64),
    Integer(i128),
    Value(ValueRef<'a>),
}
impl<'a> Cell<'a> {
    fn from_state(state: &Accumulator<'a>) -> Self {
        match state {
            Accumulator::Count(count) => Self::Count(*count),
            Accumulator::Distinct(seen) => Self::Count(seen.len() as u64),
            Accumulator::Sum {
                value,
                present: true,
            } => Self::Integer(*value),
            Accumulator::Sum { present: false, .. } | Accumulator::Extreme(None) => {
                Self::Value(ValueRef::Scalar(&NULL))
            }
            Accumulator::Extreme(Some(value)) => Self::Value(*value),
        }
    }
    fn is_null(self) -> bool {
        matches!(self, Self::Value(value) if value.is_null())
    }
    fn integer(self) -> Option<i128> {
        match self {
            Self::Count(value) => Some(i128::from(value)),
            Self::Integer(value) => Some(value),
            Self::Value(ValueRef::Scalar(CanonicalScalar::Int(value))) => Some(i128::from(*value)),
            _ => None,
        }
    }
    fn payload_units(self) -> usize {
        match self {
            Self::Value(value) => value.payload_units(),
            _ => 0,
        }
    }
    fn compare(self, other: Self) -> Ordering {
        match (self, other) {
            (Self::Count(left), Self::Count(right)) => left.cmp(&right),
            (Self::Integer(left), Self::Integer(right)) => left.cmp(&right),
            (Self::Value(left), Self::Value(right)) => left.cmp(&right),
            // Comparisons select the same prepared column from two groups.
            // A SUM's only differing variant is null, handled before this call.
            _ => unreachable!("the prepared aggregate column has one nonnull result domain"),
        }
    }
}
impl<'a> Group<'_, 'a> {
    fn cell(self, column: GraphAggregateColumn) -> Cell<'a> {
        match column {
            GraphAggregateColumn::GroupKey(at) => Cell::Value(self.key[at]),
            GraphAggregateColumn::Aggregate(at) => Cell::from_state(&self.state[at]),
        }
    }
    fn copy_owned<E>(
        self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphAggregateRow, E> {
        control(GlaExecutionEvent::ResultRow)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut keys = Vec::new();
        for cell in self.key {
            keys.push(cell.copy_owned(control)?);
        }
        let mut values = Vec::new();
        for state in self.state {
            control(GlaExecutionEvent::ScratchEntry)?;
            let cell = match Cell::from_state(state) {
                Cell::Count(value) => GraphAggregateValue::Count(value),
                Cell::Integer(value) => GraphAggregateValue::Integer(value),
                Cell::Value(value) if value.is_null() => {
                    GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
                }
                Cell::Value(value) => GraphAggregateValue::Value(value.copy_owned(control)?),
            };
            values.push(cell);
        }
        Ok(GraphAggregateRow {
            keys: keys.into_boxed_slice(),
            values: values.into_boxed_slice(),
        })
    }
}

impl PreparedGraphAggregate {
    /// Replace post-aggregate clauses after validating every referenced output.
    /// All filters are AND-conjoined; ORDER BY is lexicographic. The child,
    /// grouping, output schema and existing pagination remain unchanged.
    pub fn with_result_clauses(
        mut self,
        having: &[GraphAggregateFilter],
        ordering: &[GraphAggregateOrder],
    ) -> Result<Self, GraphAggregateBuildError> {
        if having.len() > MAX_AGGREGATE_FILTERS {
            return Err(GraphAggregateBuildError::TooManyFilters {
                limit: MAX_AGGREGATE_FILTERS,
                observed: having.len(),
            });
        }
        if ordering.len() > MAX_PATTERN_VERTICES {
            return Err(GraphAggregateBuildError::TooManyColumns {
                limit: MAX_PATTERN_VERTICES,
                observed: ordering.len(),
            });
        }
        let validate = |column| {
            let valid = match column {
                GraphAggregateColumn::GroupKey(at) => at < self.keys.len(),
                GraphAggregateColumn::Aggregate(at) => at < self.aggregates.len(),
            };
            if valid {
                Ok(())
            } else {
                Err(GraphAggregateBuildError::UnknownOutputColumn { column })
            }
        };
        for filter in having {
            validate(filter.column)?;
        }
        for (at, order) in ordering.iter().enumerate() {
            validate(order.column)?;
            if ordering[..at]
                .iter()
                .any(|previous| previous.column == order.column)
            {
                return Err(GraphAggregateBuildError::DuplicateOrder {
                    column: order.column,
                });
            }
        }
        self.having = having.to_vec();
        self.ordering = ordering.to_vec();
        Ok(self)
    }
    #[must_use]
    pub fn having(&self) -> &[GraphAggregateFilter] {
        &self.having
    }
    #[must_use]
    pub fn ordering(&self) -> &[GraphAggregateOrder] {
        &self.ordering
    }

    pub(super) fn append_result_transcript(&self, bytes: &mut Vec<u8>) {
        if self.having.is_empty() && self.ordering.is_empty() {
            return;
        }
        // Preserve the original no-clause transcript exactly. The old prefix is
        // self-delimiting; this versioned suffix has an unambiguous structure.
        bytes.extend_from_slice(b"fgdb:aggregate-result-clauses:v1\0");
        let column = |bytes: &mut Vec<u8>, column| {
            let (tag, at) = match column {
                GraphAggregateColumn::GroupKey(at) => (0, at),
                GraphAggregateColumn::Aggregate(at) => (1, at),
            };
            bytes.push(tag);
            bytes.extend_from_slice(&(at as u64).to_be_bytes());
        };
        bytes.extend_from_slice(&(self.having.len() as u64).to_be_bytes());
        for filter in &self.having {
            column(bytes, filter.column);
            match filter.test {
                GraphAggregateTest::IsNull => bytes.push(0),
                GraphAggregateTest::IsNotNull => bytes.push(1),
                GraphAggregateTest::Integer { comparison, value } => {
                    bytes.push(2);
                    bytes.push(match comparison {
                        IntegerComparison::Equal => 0,
                        IntegerComparison::NotEqual => 1,
                        IntegerComparison::Greater => 2,
                        IntegerComparison::Less => 3,
                        IntegerComparison::GreaterOrEqual => 4,
                        IntegerComparison::LessOrEqual => 5,
                    });
                    bytes.extend_from_slice(&value.to_be_bytes());
                }
            }
        }
        bytes.extend_from_slice(&(self.ordering.len() as u64).to_be_bytes());
        for order in &self.ordering {
            column(bytes, order.column);
            bytes.push(u8::from(order.descending));
            bytes.push(u8::from(order.nulls == GraphNullPlacement::Last));
        }
    }

    fn keep<E, C>(
        &self,
        group: Group<'_, '_>,
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<bool, GqlQueryError<GraphAggregateError<E>, C>> {
        let mut keep = true;
        // Evaluate every declared filter even after false/unknown, so an
        // earlier test cannot silently mask an invalid argument domain.
        for (predicate, filter) in self.having.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            let cell = group.cell(filter.column);
            let accepted = match filter.test {
                GraphAggregateTest::IsNull => cell.is_null(),
                GraphAggregateTest::IsNotNull => !cell.is_null(),
                GraphAggregateTest::Integer { .. } if cell.is_null() => false,
                GraphAggregateTest::Integer { comparison, value } => {
                    let actual = cell.integer().ok_or(GqlQueryError::Source(
                        GraphAggregateError::NonIntegerHaving { predicate },
                    ))?;
                    match comparison {
                        IntegerComparison::Equal => actual == value,
                        IntegerComparison::NotEqual => actual != value,
                        IntegerComparison::Greater => actual > value,
                        IntegerComparison::Less => actual < value,
                        IntegerComparison::GreaterOrEqual => actual >= value,
                        IntegerComparison::LessOrEqual => actual <= value,
                    }
                }
            };
            keep &= accepted;
        }
        Ok(keep)
    }

    fn compare<E>(
        &self,
        left: Group<'_, '_>,
        right: Group<'_, '_>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Ordering, E> {
        for order in &self.ordering {
            control(GlaExecutionEvent::Work)?;
            let a = left.cell(order.column);
            let b = right.cell(order.column);
            let result = match (a.is_null(), b.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => {
                    if order.nulls == GraphNullPlacement::First {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if order.nulls == GraphNullPlacement::First {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, false) => {
                    for _ in 0..a.payload_units().max(b.payload_units()) {
                        control(GlaExecutionEvent::Work)?;
                    }
                    let result = a.compare(b);
                    if order.descending {
                        result.reverse()
                    } else {
                        result
                    }
                }
            };
            if result != Ordering::Equal {
                return Ok(result);
            }
        }
        // Unique canonical group keys form the deterministic final tie break.
        for (a, b) in left.key.iter().zip(right.key) {
            control(GlaExecutionEvent::Work)?;
            for _ in 0..a.payload_units().max(b.payload_units()) {
                control(GlaExecutionEvent::Work)?;
            }
            let result = a.cmp(b);
            if result != Ordering::Equal {
                return Ok(result);
            }
        }
        Ok(left.key.len().cmp(&right.key.len()))
    }

    fn sift_down<E>(
        &self,
        heap: &mut [Group<'_, '_>],
        mut root: usize,
        end: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while root < end / 2 {
            let mut child = 2 * root + 1;
            if child + 1 < end
                && self.compare(heap[child], heap[child + 1], control)? == Ordering::Less
            {
                child += 1;
            }
            if self.compare(heap[root], heap[child], control)? != Ordering::Less {
                break;
            }
            control(GlaExecutionEvent::Work)?;
            heap.swap(root, child);
            root = child;
        }
        Ok(())
    }

    pub(super) fn finish_groups<'a, E, C>(
        &self,
        groups: &BTreeMap<Vec<ValueRef<'a>>, Vec<Accumulator<'a>>>,
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<Vec<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>> {
        let offset = usize::try_from(self.offset).unwrap_or(usize::MAX);
        let count = self
            .count
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(usize::MAX);
        let mut output = Vec::new();
        if self.ordering.is_empty() {
            // Already in canonical group order; no selection buffer or payload
            // copy for rejected/skipped groups. Preserve the old no-filter path.
            if self.having.is_empty() {
                for (key, state) in groups.iter().skip(offset).take(count) {
                    output.push(Group { key, state }.copy_owned(control)?);
                }
            } else {
                let mut skipped = 0;
                for (key, state) in groups {
                    control(GlaExecutionEvent::Work)?;
                    let group = Group { key, state };
                    if !self.keep(group, control)? {
                        continue;
                    }
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    if output.len() < count {
                        output.push(group.copy_owned(control)?);
                    }
                }
            }
            return Ok(output);
        }
        let prefix = if count == 0 || offset >= groups.len() {
            0
        } else {
            offset.saturating_add(count).min(groups.len())
        };
        let mut heap = Vec::new();
        for (key, state) in groups {
            control(GlaExecutionEvent::Work)?;
            let candidate = Group { key, state };
            if !self.keep(candidate, control)? || prefix == 0 {
                continue;
            }
            if heap.len() < prefix {
                control(GlaExecutionEvent::ScratchEntry)?;
                heap.push(candidate);
                let mut child = heap.len() - 1;
                while child > 0 {
                    let parent = (child - 1) / 2;
                    if self.compare(heap[parent], heap[child], control)? != Ordering::Less {
                        break;
                    }
                    control(GlaExecutionEvent::Work)?;
                    heap.swap(parent, child);
                    child = parent;
                }
            } else if self.compare(candidate, heap[0], control)? == Ordering::Less {
                control(GlaExecutionEvent::Work)?;
                heap[0] = candidate;
                let end = heap.len();
                self.sift_down(&mut heap, 0, end, control)?;
            }
        }
        // The worst-first selected heap becomes ascending output in place.
        for end in (1..heap.len()).rev() {
            control(GlaExecutionEvent::Work)?;
            heap.swap(0, end);
            self.sift_down(&mut heap, 0, end, control)?;
        }
        for group in heap.into_iter().skip(offset).take(count) {
            output.push(group.copy_owned(control)?);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphColumn, GraphPatternBuilder};

    fn definition(offset: u64, count: Option<u64>) -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder
            .prepare_values(
                &[
                    GraphColumn::vertex("key", "n"),
                    GraphColumn::property("p", "n", PropertyKeyId(1)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        PreparedGraphAggregate::prepare(
            input,
            &[0],
            &[
                GraphAggregate::count_rows("n"),
                GraphAggregate::sum_int("sum", 1),
            ],
            offset,
            count,
        )
        .unwrap()
    }
    fn numeric(
        column: GraphAggregateColumn,
        comparison: IntegerComparison,
        value: i128,
    ) -> GraphAggregateFilter {
        GraphAggregateFilter {
            column,
            test: GraphAggregateTest::Integer { comparison, value },
        }
    }
    fn ids(rows: &[GraphAggregateRow]) -> Vec<u128> {
        rows.iter()
            .map(|row| row.keys()[0].as_vertex().unwrap().0)
            .collect()
    }
    type Groups<'a> = BTreeMap<Vec<ValueRef<'a>>, Vec<Accumulator<'a>>>;
    fn groups(values: &[Option<i128>]) -> Groups<'static> {
        values
            .iter()
            .enumerate()
            .map(|(at, value)| {
                (
                    vec![ValueRef::Vertex(VId(at as u128))],
                    vec![
                        Accumulator::Count((at % 3 + 1) as u64),
                        Accumulator::Sum {
                            value: value.unwrap_or(0),
                            present: value.is_some(),
                        },
                    ],
                )
            })
            .collect()
    }
    fn run(query: &PreparedGraphAggregate, groups: &Groups<'_>) -> Vec<GraphAggregateRow> {
        query
            .finish_groups(groups, &mut |_| {
                Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(())
            })
            .unwrap()
    }

    #[test]
    fn clauses_validate_all_columns_and_do_not_mutate_existing_definitions() {
        let base = definition(1, Some(2));
        let bytes = base.canonical_bytes();
        assert_eq!(
            base.clone()
                .with_result_clauses(&[], &[])
                .unwrap()
                .canonical_bytes(),
            bytes
        );
        let filter = numeric(
            GraphAggregateColumn::Aggregate(0),
            IntegerComparison::GreaterOrEqual,
            i128::MAX,
        );
        let order = GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(1));
        let changed = base
            .clone()
            .with_result_clauses(&[filter], &[order])
            .unwrap();
        assert_ne!(changed.canonical_bytes(), bytes);
        assert_eq!(base.canonical_bytes(), bytes);
        assert_eq!(changed.having(), &[filter]);
        assert_eq!(changed.ordering(), &[order]);
        assert!(!format!("{filter:?}").contains(&i128::MAX.to_string()));
        assert!(matches!(
            base.clone().with_result_clauses(
                &[numeric(
                    GraphAggregateColumn::Aggregate(2),
                    IntegerComparison::Equal,
                    0
                )],
                &[]
            ),
            Err(GraphAggregateBuildError::UnknownOutputColumn { .. })
        ));
        assert!(matches!(
            base.clone().with_result_clauses(
                &[],
                &[GraphAggregateOrder::ascending(
                    GraphAggregateColumn::GroupKey(1)
                )]
            ),
            Err(GraphAggregateBuildError::UnknownOutputColumn { .. })
        ));
        assert!(matches!(
            base.clone()
                .with_result_clauses(&[filter; MAX_AGGREGATE_FILTERS + 1], &[]),
            Err(GraphAggregateBuildError::TooManyFilters { .. })
        ));
        assert!(matches!(
            base.clone().with_result_clauses(&[], &[order, order]),
            Err(GraphAggregateBuildError::DuplicateOrder { .. })
        ));
        let mut null_first = order;
        null_first.nulls = GraphNullPlacement::First;
        assert_ne!(
            base.clone()
                .with_result_clauses(&[], &[order])
                .unwrap()
                .canonical_bytes(),
            base.clone()
                .with_result_clauses(&[], &[null_first])
                .unwrap()
                .canonical_bytes()
        );
        assert_ne!(
            base.clone()
                .with_result_clauses(&[filter], &[])
                .unwrap()
                .canonical_bytes(),
            base.with_result_clauses(
                &[numeric(filter.column, IntegerComparison::Less, i128::MAX)],
                &[]
            )
            .unwrap()
            .canonical_bytes()
        );
    }

    #[test]
    fn bounded_selection_matches_independent_full_sort_filter_and_slice() {
        let choices = [None, Some(i128::MIN), Some(0), Some(i128::MAX)];
        for mut encoded in 0..4_usize.pow(4) {
            let mut values = Vec::new();
            for _ in 0..4 {
                values.push(choices[encoded % 4]);
                encoded /= 4;
            }
            let input = groups(&values);
            for descending in [false, true] {
                for nulls in [GraphNullPlacement::First, GraphNullPlacement::Last] {
                    let mut expected: Vec<_> =
                        (0..values.len()).filter(|at| at % 3 + 1 >= 2).collect();
                    expected.sort_by(|a, b| {
                        let order = match (values[*a], values[*b]) {
                            (None, None) => Ordering::Equal,
                            (None, Some(_)) => {
                                if nulls == GraphNullPlacement::First {
                                    Ordering::Less
                                } else {
                                    Ordering::Greater
                                }
                            }
                            (Some(_), None) => {
                                if nulls == GraphNullPlacement::First {
                                    Ordering::Greater
                                } else {
                                    Ordering::Less
                                }
                            }
                            (Some(a), Some(b)) => {
                                if descending {
                                    b.cmp(&a)
                                } else {
                                    a.cmp(&b)
                                }
                            }
                        };
                        order
                            .then_with(|| (a % 3 + 1).cmp(&(b % 3 + 1)))
                            .then_with(|| a.cmp(b))
                    });
                    for offset in 0..=6 {
                        for count in [None, Some(0), Some(1), Some(2), Some(6), Some(u64::MAX)] {
                            let query = definition(offset, count)
                                .with_result_clauses(
                                    &[numeric(
                                        GraphAggregateColumn::Aggregate(0),
                                        IntegerComparison::GreaterOrEqual,
                                        2,
                                    )],
                                    &[
                                        GraphAggregateOrder {
                                            column: GraphAggregateColumn::Aggregate(1),
                                            descending,
                                            nulls,
                                        },
                                        GraphAggregateOrder::ascending(
                                            GraphAggregateColumn::Aggregate(0),
                                        ),
                                    ],
                                )
                                .unwrap();
                            let expected: Vec<_> = expected
                                .iter()
                                .skip(offset as usize)
                                .take(
                                    count
                                        .and_then(|value| usize::try_from(value).ok())
                                        .unwrap_or(usize::MAX),
                                )
                                .map(|at| *at as u128)
                                .collect();
                            assert_eq!(ids(&run(&query, &input)), expected);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn having_runs_before_offset_and_result_limits_without_requiring_ordering() {
        let input = groups(&[Some(1), Some(2), Some(3), None]);
        let filter = numeric(
            GraphAggregateColumn::Aggregate(1),
            IntegerComparison::GreaterOrEqual,
            2,
        );
        assert_eq!(
            ids(&run(
                &definition(1, Some(1))
                    .with_result_clauses(&[filter], &[])
                    .unwrap(),
                &input
            )),
            vec![2]
        );
        let is_null = GraphAggregateFilter {
            column: GraphAggregateColumn::Aggregate(1),
            test: GraphAggregateTest::IsNull,
        };
        assert_eq!(
            ids(&run(
                &definition(0, None)
                    .with_result_clauses(&[is_null], &[])
                    .unwrap(),
                &input
            )),
            vec![3]
        );
        let nonnull = GraphAggregateFilter {
            test: GraphAggregateTest::IsNotNull,
            ..is_null
        };
        assert_eq!(
            ids(&run(
                &definition(0, None)
                    .with_result_clauses(&[nonnull], &[])
                    .unwrap(),
                &input
            )),
            vec![0, 1, 2]
        );
        let ne = numeric(is_null.column, IntegerComparison::NotEqual, 2);
        assert_eq!(
            ids(&run(
                &definition(0, None).with_result_clauses(&[ne], &[]).unwrap(),
                &input
            )),
            vec![0, 2]
        );
    }

    #[test]
    fn numeric_predicates_preserve_full_width_counts_and_sums_and_refuse_bad_types() {
        let key = [ValueRef::Vertex(VId(1))];
        let states = [
            Accumulator::Count(u64::MAX),
            Accumulator::Sum {
                value: i128::MIN,
                present: true,
            },
        ];
        let group = Group {
            key: &key,
            state: &states,
        };
        let query = definition(0, None)
            .with_result_clauses(
                &[
                    numeric(
                        GraphAggregateColumn::Aggregate(0),
                        IntegerComparison::Equal,
                        i128::from(u64::MAX),
                    ),
                    numeric(
                        GraphAggregateColumn::Aggregate(1),
                        IntegerComparison::Less,
                        i128::MAX,
                    ),
                ],
                &[],
            )
            .unwrap();
        assert!(
            query
                .keep(group, &mut |_| Ok::<
                    _,
                    GqlQueryError<GraphAggregateError<()>, ()>,
                >(()))
                .unwrap()
        );
        // The bad-domain test must not be masked by an earlier false filter,
        // LIMIT 0, or an offset larger than the group count.
        let boolean = CanonicalScalar::Bool(true);
        let mut input = Groups::new();
        input.insert(
            vec![ValueRef::Scalar(&boolean)],
            vec![
                Accumulator::Count(0),
                Accumulator::Sum {
                    value: 0,
                    present: false,
                },
            ],
        );
        for count in [None, Some(0)] {
            let query = definition(u64::MAX, count)
                .with_result_clauses(
                    &[
                        numeric(
                            GraphAggregateColumn::Aggregate(0),
                            IntegerComparison::Greater,
                            0,
                        ),
                        numeric(
                            GraphAggregateColumn::GroupKey(0),
                            IntegerComparison::Equal,
                            1,
                        ),
                    ],
                    &[GraphAggregateOrder::ascending(
                        GraphAggregateColumn::Aggregate(0),
                    )],
                )
                .unwrap();
            assert!(matches!(
                query.finish_groups(&input, &mut |_| Ok::<
                    _,
                    GqlQueryError<GraphAggregateError<()>, ()>,
                >(())),
                Err(GqlQueryError::Source(
                    GraphAggregateError::NonIntegerHaving { predicate: 1 }
                ))
            ));
        }
    }

    #[test]
    fn every_comparison_swap_filter_and_copy_can_be_interrupted() {
        let input = groups(&[Some(3), None, Some(-1), Some(3), Some(8)]);
        let query = definition(1, Some(3))
            .with_result_clauses(
                &[GraphAggregateFilter {
                    column: GraphAggregateColumn::Aggregate(1),
                    test: GraphAggregateTest::IsNotNull,
                }],
                &[GraphAggregateOrder::descending(
                    GraphAggregateColumn::Aggregate(1),
                )],
            )
            .unwrap();
        let mut events = 0;
        let complete = query
            .finish_groups(&input, &mut |_| {
                events += 1;
                Ok::<_, GqlQueryError<GraphAggregateError<()>, usize>>(())
            })
            .unwrap();
        assert_eq!(ids(&complete), vec![0, 3, 2]);
        for stop in 1..=events {
            let mut at = 0;
            let refused = query.finish_groups(&input, &mut |_| {
                at += 1;
                if at == stop {
                    Err(GqlQueryError::<GraphAggregateError<()>, _>::Interrupted(
                        stop,
                    ))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(refused, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
        assert_eq!(ids(&run(&query, &input)), vec![0, 3, 2]);
    }

    #[test]
    fn ranking_reserves_only_the_page_prefix_and_copies_only_returned_payloads() {
        let payload = CanonicalScalar::bytes(vec![7; 1024]).unwrap();
        let mut input = Groups::new();
        for at in 0..1000 {
            input.insert(
                vec![ValueRef::Vertex(VId(at))],
                vec![
                    Accumulator::Count(at as u64),
                    Accumulator::Extreme(Some(ValueRef::Scalar(&payload))),
                ],
            );
        }
        let query = definition(3, Some(2))
            .with_result_clauses(
                &[],
                &[GraphAggregateOrder::descending(
                    GraphAggregateColumn::Aggregate(0),
                )],
            )
            .unwrap();
        let mut scratch = 0;
        let result = query
            .finish_groups(&input, &mut |event| {
                scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(())
            })
            .unwrap();
        assert_eq!(ids(&result), vec![996, 995]);
        // Five borrowed heap references; each returned row owns one key, one
        // row entry, two aggregate cells, one extreme and sixteen payload units.
        assert_eq!(scratch, 5 + 2 * (1 + 1 + 2 + 1 + 16));
        for row in &result {
            assert_eq!(
                row.get(1).unwrap().as_value().unwrap().as_scalar(),
                Some(&payload)
            );
        }
    }
}
