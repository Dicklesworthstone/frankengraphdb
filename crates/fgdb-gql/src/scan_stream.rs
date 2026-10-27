//! A fixed choice of an existing vertex or identified-edge pull operator.
//!
//! Native text preparation can choose a source shape without exposing two
//! unrelated return types. This sum does not prepare, retry, meter, prefetch or
//! collect anything: the chosen cursor keeps its source and cumulative policy.

use crate::algebra::GraphValueRow;
use crate::edge_stream::{EdgeScanCursor, EdgeScanError, EdgeScanSource, EdgeScanState};
use crate::stream::{VertexScanCursor, VertexScanError, VertexScanSource};
use crate::{GlaExecutionStats, GqlExecutionStats, GqlQueryError};
use fgdb_types::CommitSeq;

/// A conservative numeric aggregate specialization for an ordinary query.
///
/// Unlike the explicit streaming APIs, a native host may keep its current
/// physical evaluator when this compiler returns `None`. This decision uses
/// only the complete bound definition and precedes source access. Once a
/// cursor has opened, its errors are final; execution is never retried.
/// Metadata counts retain their host's exact-count path. Collections retain
/// their existing source-order contract. An edge input must be one unbound
/// fixed-edge scan: keyed endpoint and multi-hop plans can use substantially
/// cheaper bound-closure or factorized evaluation in the ordinary host.
#[derive(Clone, Debug)]
pub enum AggregateScanPlan {
    Vertex(crate::stream::aggregate::VertexAggregatePlan),
    Edge(crate::edge_stream::aggregate::EdgeAggregatePlan),
}

impl AggregateScanPlan {
    /// Admit only complete, numeric reductions whose existing source cursor
    /// avoids retaining the matched input relation. Grouping, argument
    /// DISTINCT, HAVING, output expressions, ordering and pagination remain
    /// the existing aggregate compiler's responsibility.
    #[must_use]
    pub fn compile_numeric(aggregate: &crate::PreparedGraphAggregate) -> Option<Self> {
        use crate::GraphAggregateFunction::{Collect, CollectDistinct};
        use crate::GraphSetValue;
        use crate::algebra::GlaOperator;

        if aggregate.count_metadata_source().is_some()
            || aggregate.count_summary_source().is_some()
            || aggregate
                .aggregates()
                .iter()
                .any(|spec| matches!(spec.function(), Collect | CollectDistinct))
        {
            return None;
        }
        // These cursors retire their source before completed-group output and
        // evaluate each input in physical scan order. Keep graph-reading,
        // dynamically typed and ordinal-dependent expressions on the host
        // with an element reader and the complete canonical input relation.
        // A dead branch is still part of the prepared expression contract.
        let row_local = |value: &GraphSetValue| {
            matches!(
                value,
                GraphSetValue::Column(_)
                    | GraphSetValue::Literal(_)
                    | GraphSetValue::TypedNull(_)
                    | GraphSetValue::Value(_)
                    | GraphSetValue::Integer(_)
            )
        };
        if aggregate
            .input_projection()
            .is_some_and(|columns| columns.iter().any(|column| !row_local(column.value())))
            || aggregate
                .output_projection()
                .is_some_and(|columns| columns.iter().any(|column| !row_local(column.value())))
            || aggregate
                .computed_ordering()
                .iter()
                .any(|value| !row_local(value))
        {
            return None;
        }
        let operators = aggregate.input_pattern().plan().operators();
        match operators.first()? {
            GlaOperator::ScanVertices => {
                crate::stream::aggregate::VertexAggregatePlan::compile(aggregate)
                    .ok()
                    .map(Self::Vertex)
            }
            GlaOperator::ScanEdges { .. }
                if operators.iter().all(|operator| match operator {
                    GlaOperator::ScanEdges { .. }
                    | GlaOperator::ProjectValues { .. }
                    | GlaOperator::OrderByValues
                    | GlaOperator::Limit {
                        offset: 0,
                        count: None,
                    } => true,
                    GlaOperator::Select { predicates, .. } => predicates.is_empty(),
                    _ => false,
                }) =>
            {
                crate::edge_stream::aggregate::EdgeAggregatePlan::compile(aggregate)
                    .ok()
                    .map(Self::Edge)
            }
            _ => None,
        }
    }
}

/// One resumable source examination. A scoped source can skip a private
/// identity without charging or exposing it as a matching candidate. The
/// identity is retained only to advance an independently owned successor.
/// This is source progress, never a query row or a public cardinality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceStep<T> {
    Item(T),
    Skipped(T),
    Exhausted,
}

/// Compiler-owned terminal clauses deferred by a blocking scan consumer.
/// A sort-input cursor is NOT a completed query: the consumer must order all
/// admitted occurrences, apply DISTINCT when requested, then SKIP/LIMIT and
/// retain only the visible prefix. Hidden sort cells remain through comparison
/// and pagination; DISTINCT is admitted only when every evaluated cell is visible.
/// This contains no source, authority, mutable plan or caller-asserted order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanSortTail {
    evaluation_width: usize,
    visible_width: usize,
    order: Vec<crate::algebra::GraphValueOrder>,
    explicit_order: bool,
    distinct: bool,
    offset: u64,
    count: Option<u64>,
}

impl ScanSortTail {
    /// Complete row width emitted by the private sort-input cursor.
    /// The trailing cells, if any, are compiler-owned hidden ORDER BY keys.
    pub fn evaluation_width(&self) -> usize {
        self.evaluation_width
    }
    /// Public result width after ordering, DISTINCT and pagination.
    pub fn visible_width(&self) -> usize {
        self.visible_width
    }
    pub fn order(&self) -> &[crate::algebra::GraphValueOrder] {
        &self.order
    }
    /// Whether these keys came from explicit language ORDER BY. Otherwise
    /// they encode implicit canonical whole-row order. Equal key vectors do
    /// not imply equal comparison laws: ASC NULLS FIRST over every column may
    /// still require numeric equivalence and language collection ordering.
    /// Consumers must preserve this compiler-owned distinction through sorting
    /// and defer the canonical tie-break until ALL explicit keys have tied.
    pub fn has_explicit_order(&self) -> bool {
        self.explicit_order
    }
    pub fn distinct(&self) -> bool {
        self.distinct
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn count(&self) -> Option<u64> {
        self.count
    }

    /// The tail of a relation whose complete projected rows the caller sorts
    /// (a pulled set, fgdb-7xqxb): `width` visible cells, the relation's own
    /// ORDER BY keys, quantifier and page, with no hidden cells. An implicit
    /// order is canonical whole-row order (NULL first), not explicit ASC's
    /// default NULLS LAST. The consumer must drain and project the complete
    /// input before applying DISTINCT and then the page, even for LIMIT 0.
    /// None for an invalid width, repeated key or out-of-range column.
    pub(crate) fn for_relation(
        width: usize,
        mut order: Vec<crate::algebra::GraphValueOrder>,
        quantifier: crate::GraphSetQuantifier,
        offset: u64,
        count: Option<u64>,
    ) -> Option<Self> {
        if width == 0
            || width > crate::algebra::MAX_PATTERN_VERTICES
            || order.len() > width
            || order.iter().enumerate().any(|(at, key)| {
                key.column >= width || order[..at].iter().any(|prior| prior.column == key.column)
            })
        {
            return None;
        }
        let explicit_order = !order.is_empty();
        if !explicit_order {
            order = (0..width)
                .map(|column| {
                    crate::algebra::GraphValueOrder::ascending(column).with_nulls_first(true)
                })
                .collect();
        }
        Some(Self {
            evaluation_width: width,
            visible_width: width,
            order,
            explicit_order,
            distinct: quantifier == crate::GraphSetQuantifier::Distinct,
            offset,
            count,
        })
    }

    // Only physical compilers call this. They must still audit EVERY source,
    // predicate and projection instruction, not just recognize the final tail.
    pub(crate) fn compile(plan: &crate::algebra::GlaPlan<GraphValueRow>) -> Result<Self, usize> {
        use crate::algebra::{GlaOperator, GraphValueOrder, MAX_PATTERN_VERTICES};
        let ops = plan.operators();
        let at = ops.len().checked_sub(2).ok_or(0_usize)?;
        let Some(GlaOperator::Limit { offset, count }) = ops.last() else {
            return Err(at + 1);
        };
        let preceding = at.checked_sub(1).ok_or(at)?;
        let distinct = matches!(ops.get(preceding), Some(GlaOperator::Distinct));
        let projection = if distinct {
            preceding.checked_sub(1).ok_or(preceding)?
        } else {
            preceding
        };
        let Some(GlaOperator::ProjectValues { columns }) = ops.get(projection) else {
            return Err(projection);
        };
        if columns.is_empty() || columns.len() > MAX_PATTERN_VERTICES {
            return Err(projection);
        }
        let evaluation_width = columns.len();
        let visible_width = plan.visible_columns.unwrap_or(evaluation_width);
        if visible_width == 0
            || visible_width > evaluation_width
            || (distinct && plan.visible_columns.is_some())
        {
            // Hidden cells must never change DISTINCT's equivalence classes.
            // The text compiler already refuses this combination; audit the
            // logical metadata here too, before any source or LIMIT is driven.
            return Err(projection);
        }
        let (order, explicit_order) = match &ops[at] {
            // Implicit whole-row order is canonical GraphValue order, whose
            // Null scalar is least. Explicit ASC's default NULLS LAST differs.
            GlaOperator::OrderByValues => (
                (0..columns.len())
                    .map(|column| GraphValueOrder::ascending(column).with_nulls_first(true))
                    .collect(),
                false,
            ),
            GlaOperator::OrderByValueColumns { columns: keys } => {
                if keys.is_empty() || keys.len() > MAX_PATTERN_VERTICES {
                    return Err(at);
                }
                for (index, key) in keys.iter().enumerate() {
                    if key.column >= columns.len()
                        || keys[..index].iter().any(|other| other.column == key.column)
                    {
                        return Err(at);
                    }
                }
                (keys.to_vec(), true)
            }
            _ => return Err(at),
        };
        Ok(Self {
            evaluation_width,
            visible_width,
            order,
            explicit_order,
            distinct,
            offset: *offset,
            count: *count,
        })
    }
}

// Both physical cursors implement the same lifecycle. Reuse its existing type
// so consumers of the native vertex stream retain their state comparisons.
pub use crate::stream::VertexScanState as ScanState;

/// Actual source provenance carried by native execution and completed spools.
/// A Values source creates the relational singleton without opening a graph
/// cursor; its snapshot-record count is zero. Set identifies a completed binary
/// set or Cartesian relation whose graph records, if any, come from all of its
/// actual operands, including when an operand's selected rows are empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanKind {
    Values,
    Vertex,
    Edge,
    Set,
}

/// Preserve the original operator/source error rather than stringifying it.
/// Resource and interruption errors stay outside this sum in GqlQueryError.
#[derive(Debug)]
pub enum ScanError<E> {
    Vertex(VertexScanError<E>),
    Edge(EdgeScanError<E>),
    Projection {
        row: u64,
        column: usize,
        error: crate::GraphIntegerError,
    },
}
impl<E: core::fmt::Display> core::fmt::Display for ScanError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vertex(error) => error.fmt(f),
            Self::Edge(error) => error.fmt(f),
            Self::Projection { row, column, error } => {
                write!(f, "stream row {row} column {column}: {error}")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for ScanError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Vertex(error) => Some(error),
            Self::Edge(error) => Some(error),
            Self::Projection { error, .. } => Some(error),
        }
    }
}

/// One already-open physical scan, not an iterator over an eager result bag.
/// Both variants emit GraphValueRow and preserve their native order, counters,
/// late-error behavior and source lifetime. Switching physical strategies is
/// never an error-recovery mechanism; the host chooses before opening a source.
/// Source errors have one shared domain; checkpoint closures may differ while
/// retaining the same interruption type. A projected vertex cursor retains its
/// checked row projection beside the source in one box; it adds no row bag or
/// second meter.
pub enum ScanCursor<VS, VF, ES, EF> {
    Vertex(VertexScanCursor<VS, VF, GraphValueRow>),
    /// The cursor must be compiled from this projection's `pattern()`. The
    /// checked leading identity preserves canonical output order and uniqueness.
    ProjectedVertex(
        Box<(
            VertexScanCursor<VS, VF, GraphValueRow>,
            crate::GraphSetRowPull,
        )>,
    ),
    /// Boxed: the edge cursor's join and resumable-quantum state is twice
    /// the vertex cursor's size (clippy::large_enum_variant).
    Edge(Box<EdgeScanCursor<ES, EF>>),
}
impl<VS, VF, ES, EF> ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource,
    ES: EdgeScanSource<Error = VS::Error>,
{
    #[must_use]
    pub fn kind(&self) -> ScanKind {
        match self {
            Self::Vertex(_) => ScanKind::Vertex,
            Self::ProjectedVertex(_) => ScanKind::Vertex,
            Self::Edge(_) => ScanKind::Edge,
        }
    }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        match self {
            Self::Vertex(cursor) => cursor.snapshot_seq(),
            Self::ProjectedVertex(projected) => projected.0.snapshot_seq(),
            Self::Edge(cursor) => cursor.snapshot_seq(),
        }
    }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats {
        match self {
            Self::Vertex(cursor) => cursor.row_stats(),
            Self::ProjectedVertex(projected) => projected.0.row_stats(),
            Self::Edge(cursor) => cursor.row_stats(),
        }
    }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        match self {
            Self::Vertex(cursor) => cursor.evaluator_stats(),
            Self::ProjectedVertex(projected) => projected.0.evaluator_stats(),
            Self::Edge(cursor) => cursor.evaluator_stats(),
        }
    }
    #[must_use]
    pub fn state(&self) -> ScanState {
        match self {
            Self::Vertex(cursor) => cursor.state(),
            Self::ProjectedVertex(projected) => projected.0.state(),
            Self::Edge(cursor) => match cursor.state() {
                EdgeScanState::Open => ScanState::Open,
                EdgeScanState::Exhausted => ScanState::Exhausted,
                EdgeScanState::Closed => ScanState::Closed,
                EdgeScanState::Failed => ScanState::Failed,
            },
        }
    }
    /// Idempotent; the selected cursor releases its pin without another pull.
    pub fn close(&mut self) {
        match self {
            Self::Vertex(cursor) => cursor.close(),
            Self::ProjectedVertex(projected) => projected.0.close(),
            Self::Edge(cursor) => cursor.close(),
        }
    }
}
impl<VS, VF, ES, EF, C> Iterator for ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource,
    ES: EdgeScanSource<Error = VS::Error>,
    VF: FnMut() -> Result<(), C>,
    EF: FnMut() -> Result<(), C>,
{
    type Item = Result<GraphValueRow, GqlQueryError<ScanError<VS::Error>, C>>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Vertex(cursor) => cursor
                .next()
                .map(|row| row.map_err(|error| error.map_source(ScanError::Vertex))),
            Self::ProjectedVertex(projected) => {
                let (cursor, pull) = projected.as_mut();
                cursor.next_projected(
                    pull,
                    |error| error.map_source(ScanError::Vertex),
                    |row, column, error| {
                        GqlQueryError::Source(ScanError::Projection { row, column, error })
                    },
                )
            }
            Self::Edge(cursor) => cursor
                .next()
                .map(|row| row.map_err(|error| error.map_source(ScanError::Edge))),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Vertex(cursor) => cursor.size_hint(),
            Self::ProjectedVertex(projected) => projected.0.size_hint(),
            Self::Edge(cursor) => cursor.size_hint(),
        }
    }
}
impl<VS, VF, ES, EF, C> std::iter::FusedIterator for ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource,
    ES: EdgeScanSource<Error = VS::Error>,
    VF: FnMut() -> Result<(), C>,
    EF: FnMut() -> Result<(), C>,
{
}
impl<VS, VF, ES, EF> core::fmt::Debug for ScanCursor<VS, VF, ES, EF> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vertex(cursor) => f.debug_tuple("ScanCursor::Vertex").field(cursor).finish(),
            Self::ProjectedVertex(projected) => f
                .debug_tuple("ScanCursor::ProjectedVertex")
                .field(&projected.0)
                .field(&projected.1)
                .finish(),
            Self::Edge(cursor) => f.debug_tuple("ScanCursor::Edge").field(cursor).finish(),
        }
    }
}

#[cfg(test)]
mod sort_tail_tests {
    use super::*;
    use crate::algebra::{
        BindingSlot, EdgeRelation, GlaDirection, GlaOperator, GlaPlan, GraphColumn,
        GraphPatternBuilder, GraphValueOrder, ValueProjection,
    };
    use crate::edge_stream::EdgeScanPlan;
    use crate::stream::VertexScanPlan;
    use fgdb_delta_types::{PropertyKeyId, RelationId};

    fn plan(
        edge: bool,
        distinct: bool,
        visible: Option<usize>,
        count: Option<u64>,
    ) -> GlaPlan<GraphValueRow> {
        let mut operators = vec![
            if edge {
                GlaOperator::ScanEdges {
                    relation: EdgeRelation::One(RelationId(1)),
                    direction: GlaDirection::Forward,
                }
            } else {
                GlaOperator::ScanVertices
            },
            GlaOperator::ProjectValues {
                columns: vec![
                    ValueProjection::Vertex {
                        slot: BindingSlot(0),
                    },
                    ValueProjection::Property {
                        slot: BindingSlot(u32::from(edge)),
                        key: PropertyKeyId(1),
                    },
                    ValueProjection::Property {
                        slot: BindingSlot(0),
                        key: PropertyKeyId(2),
                    },
                ],
            },
        ];
        if distinct {
            operators.push(GlaOperator::Distinct);
        }
        operators.push(GlaOperator::OrderByValueColumns {
            columns: vec![
                GraphValueOrder::descending(1).with_nulls_first(true),
                GraphValueOrder::ascending(2),
            ]
            .into(),
        });
        operators.push(GlaOperator::Limit { offset: 7, count });
        let mut plan = GlaPlan::from_operators(operators);
        plan.visible_columns = visible;
        plan
    }

    #[test]
    fn hidden_key_widths_and_windows_come_from_the_complete_vertex_or_edge_plan() {
        for edge in [false, true] {
            for count in [None, Some(0), Some(3)] {
                let logical = plan(edge, false, Some(1), count);
                let tail = if edge {
                    assert!(EdgeScanPlan::compile(&logical).is_err());
                    EdgeScanPlan::compile_sort_input(&logical).unwrap().1
                } else {
                    assert!(VertexScanPlan::<GraphValueRow>::compile(&logical).is_err());
                    VertexScanPlan::compile_sort_input(&logical).unwrap().1
                };
                assert_eq!(tail.evaluation_width(), 3);
                assert_eq!(tail.visible_width(), 1);
                assert_eq!(tail.offset(), 7);
                assert_eq!(tail.count(), count);
                assert!(!tail.distinct());
                assert!(tail.has_explicit_order());
                assert_eq!(
                    tail.order(),
                    &[
                        GraphValueOrder::descending(1).with_nulls_first(true),
                        GraphValueOrder::ascending(2),
                    ],
                );
                assert_eq!(tail, ScanSortTail::compile(&logical).unwrap());
            }
        }
    }

    #[test]
    fn hidden_distinct_and_invalid_visible_widths_refuse_even_at_limit_zero() {
        for edge in [false, true] {
            for (distinct, visible) in [(false, 0), (false, 4), (true, 1), (true, 2), (true, 3)] {
                let logical = plan(edge, distinct, Some(visible), Some(0));
                assert!(ScanSortTail::compile(&logical).is_err());
                if edge {
                    assert!(EdgeScanPlan::compile_sort_input(&logical).is_err());
                } else {
                    assert!(VertexScanPlan::compile_sort_input(&logical).is_err());
                }
            }
            let logical = plan(edge, true, None, Some(0));
            let tail = ScanSortTail::compile(&logical).unwrap();
            assert_eq!(tail.evaluation_width(), 3);
            assert_eq!(tail.visible_width(), 3);
            assert!(tail.distinct());
        }
    }

    #[test]
    fn ordinary_vertex_compilation_keeps_the_visible_row_contract() {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let complete = builder
            .prepare_values(
                &[
                    GraphColumn::vertex("id", "n"),
                    GraphColumn::property("score", "n", PropertyKeyId(1)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        assert!(VertexScanPlan::<GraphValueRow>::compile(complete.plan()).is_ok());
        let hidden = complete.with_visible_columns(1);
        assert_eq!(hidden.columns(), &["id"]);
        assert!(VertexScanPlan::<GraphValueRow>::compile(hidden.plan()).is_err());
        let (_, tail) = VertexScanPlan::compile_sort_input(hidden.plan()).unwrap();
        assert_eq!(tail.evaluation_width(), 2);
        assert_eq!(tail.visible_width(), 1);
        assert!(!tail.has_explicit_order());
    }

    #[test]
    fn equal_scan_key_vectors_retain_different_comparison_laws() {
        for edge in [false, true] {
            for distinct in [false, true] {
                let logical = plan(edge, distinct, None, Some(0));
                let mut operators = logical.operators().to_vec();
                let at = operators.len() - 2;
                operators[at] = GlaOperator::OrderByValues;
                let implicit = GlaPlan::<GraphValueRow>::from_operators(operators.clone());
                let canonical = ScanSortTail::compile(&implicit).unwrap();
                operators[at] = GlaOperator::OrderByValueColumns {
                    columns: canonical.order().to_vec().into(),
                };
                let explicit = GlaPlan::<GraphValueRow>::from_operators(operators);
                let language = ScanSortTail::compile(&explicit).unwrap();
                assert_eq!(canonical.order(), language.order());
                assert!(!canonical.has_explicit_order());
                assert!(language.has_explicit_order());
                assert_ne!(canonical, language);
                for tail in [&canonical, &language] {
                    assert_eq!(tail.evaluation_width(), 3);
                    assert_eq!(tail.visible_width(), 3);
                    assert_eq!(tail.distinct(), distinct);
                    assert_eq!(tail.offset(), 7);
                    assert_eq!(tail.count(), Some(0));
                    assert_eq!((*tail).clone(), *tail);
                }
                if edge {
                    assert_eq!(
                        EdgeScanPlan::compile_sort_input(&implicit).unwrap().1,
                        canonical
                    );
                    assert_eq!(
                        EdgeScanPlan::compile_sort_input(&explicit).unwrap().1,
                        language
                    );
                } else {
                    assert_eq!(
                        VertexScanPlan::compile_sort_input(&implicit).unwrap().1,
                        canonical
                    );
                    assert_eq!(
                        VertexScanPlan::compile_sort_input(&explicit).unwrap().1,
                        language
                    );
                }
            }
        }
    }

    #[test]
    fn relation_tail_remembers_explicitness_before_default_keys_are_synthesized() {
        use crate::GraphSetQuantifier;
        for width in [1, 2, crate::algebra::MAX_PATTERN_VERTICES] {
            for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
                let implicit =
                    ScanSortTail::for_relation(width, vec![], quantifier, 2, Some(0)).unwrap();
                let explicit = ScanSortTail::for_relation(
                    width,
                    implicit.order().to_vec(),
                    quantifier,
                    2,
                    Some(0),
                )
                .unwrap();
                assert_eq!(implicit.order(), explicit.order());
                assert!(!implicit.has_explicit_order());
                assert!(explicit.has_explicit_order());
                assert_ne!(implicit, explicit);
                assert_eq!(implicit.distinct(), explicit.distinct());
                assert_eq!(implicit.count(), explicit.count());
            }
        }
        for keys in [
            vec![GraphValueOrder::ascending(2)],
            vec![
                GraphValueOrder::ascending(0),
                GraphValueOrder::descending(0),
            ],
        ] {
            assert!(
                ScanSortTail::for_relation(2, keys, GraphSetQuantifier::All, 0, Some(0)).is_none()
            );
        }
        assert!(ScanSortTail::for_relation(0, vec![], GraphSetQuantifier::All, 0, None).is_none());
    }
}
